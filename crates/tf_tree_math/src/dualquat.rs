//! Fast SE(3) screw interpolation via unit dual-quaternion powers.
//!
//! The reference geodesic interpolation is `a · exp_se3(s · log_se3(a⁻¹·b))`
//! (see [`crate::reference::sclerp`]). That route spends two transcendental
//! pairs (an `atan2`+more in `log`, a `sin_cos` in `exp`) and the full `V`/`V⁻¹`
//! series. The screw form here computes the identical result raising the
//! relative transform's unit dual quaternion to the power `s`.
//!
//! A unit dual quaternion `q̂ = q_r + ε q_d` of a rigid transform `(q, t)` has
//! `q_r = q` and `q_d = ½·(0,t)⊗q`. Its screw parameters are the angle `θ`,
//! pitch translation `d`, axis direction `l`, and moment `m`; the power `q̂ˢ`
//! scales `θ` and `d` by `s` and leaves `l`, `m` fixed.
//!
//! # The hot path spends no transcendental at all
//!
//! Writing `φ = θ/2`, the screw power needs exactly two scalars from the angle:
//! `cos(sφ)` and `sin(sφ)`. The naive route computes `φ = atan2(sin φ, cos φ)`
//! and then `sincos(sφ)` — and, separately, `1/sin φ` to normalize the axis,
//! which is why the old code also needed a `sqrt` and a near-identity fallback.
//!
//! Two observations remove all of it:
//!
//! 1. **`sin φ` never appears alone.** `l = q_v/sin φ`, `d = −2·q_d.w/sin φ`
//!    and `m` are each singular as `φ → 0`, but the *products* that actually
//!    reach the result — `sin(sφ)·l`, `m·sin(sφ)`, `½sd·sin(sφ)` — are all
//!    finite, because every `1/sin φ` cancels against a `sin(sφ)` or a
//!    `q_v ∝ sin φ`. Grouping the algebra that way deletes the `sqrt`, both
//!    divisions by `sin φ`, and the reason the `SCREW_SMALL` fallback existed.
//! 2. **`sin(sφ)/sin(φ)` is the slerp weight.** It is the same function
//!    [`crate::interp`] already evaluates as a transcendental-free polynomial in
//!    `u = φ²`, valid over the same range, for the same reason (the two samples
//!    are adjacent on one edge, so `φ` is milliradians). With
//!    `w = sin(sφ)/sin φ` and `wa = sin((1−s)φ)/sin φ` in hand,
//!    `cos(sφ) = wa + w·cos φ` — an all-positive sum for `s ∈ [0,1]` and
//!    `cos φ ≥ 0`, so it cancels nothing.
//!
//! What remains on the fast path is two Horner polynomials, one reciprocal and
//! multiply–adds: **no `sqrt`, no `atan2`, no `sincos`, no division by a
//! quantity that approaches zero.**

use crate::interp::{slerp_weight, theta_sq_from_chord};
use crate::iso3::{exp_se3, log_se3, Iso3, Vec3};
use crate::quat::Quat;
use crate::twist::Twist;

/// `sin²(THETA_SLERP_SMALL)` — the fast-path predicate expressed in `sin²(θ/2)`,
/// which is `‖q_v‖²` and therefore already in hand.
///
/// Branching on this instead of on `φ²` keeps `theta_sq_from_chord` off the slow
/// path entirely. Pinned against `sin(0.15)²` by
/// `sin_half_theta_small_sq_matches_the_shared_threshold`.
const SIN_HALF_THETA_SMALL_SQ: f64 = 0.022_331_755_437_196_99;

/// Below this `sin²(θ/2)` the dual part underflows, so the degenerate case
/// routes through the exact `exp_se3(s·log_se3(rel))` fallback.
///
/// **~280 orders of magnitude below the old `1e-8` bound on `sin(θ/2)`.** The
/// old formulation materialized the moment `m`, which genuinely diverges as
/// `θ → 0`, so it had to bail out while `sin(θ/2)` was still large. The
/// regrouped algebra never forms a divergent intermediate, so the only
/// remaining hazard is `q_d.w/‖q_v‖²` overflowing once `‖q_v‖²` denormalizes —
/// which is what this guards, and nothing else.
///
/// This is a **simplicity and robustness** change, not a speed one: a stationary
/// robot's near-identity relative transforms did take the `log`/`exp` fallback
/// on every lookup, but `interp_cost` measures that fallback at 50.9 ns against
/// 51.6 ns for the main path — i.e. it never cost anything. Widening the fast
/// path is worth doing because it deletes a whole second code path with its own
/// accuracy characteristics, not because the old one was slow.
///
/// Validated by `screw_pow_is_accurate_down_to_the_degenerate_threshold`, which
/// sweeps θ from 3 rad down past 1e-160 against the reference.
const SCREW_DEGENERATE_SQ: f64 = 1e-290;

/// The screw decomposition of a transform, in the grouped form that stays finite
/// as `θ → 0`.
///
/// Shared by [`screw_pow`] and [`screw_pow_with_twist`] so the two cannot drift.
/// Every field is one of the quantities the module docs justify holding in
/// product form rather than factored: `sh2 = sin²(θ/2)`, `ch = cos(θ/2)`,
/// `k = q_d.w/sin²(θ/2)` and `m_sh = m·sin(θ/2)`.
struct ScrewParts {
    q_v: Vec3,
    sh2: f64,
    ch: f64,
    q_d: Quat,
    k: f64,
    m_sh: Vec3,
}

/// Either the grouped screw parts, or the degenerate case carrying the
/// **canonicalized** transform ready for `log_se3`.
///
/// The canonicalization has to survive into the degenerate arm: `log_se3` of a
/// `w < 0` quaternion lands on the other branch and answers `θ − 2π`.
enum Screw {
    Degenerate(Iso3),
    Regular(ScrewParts),
}

/// The prologue of the screw power, factored out of [`screw_pow`].
///
/// `#[inline(always)]` is load-bearing, not decoration: [`screw_pow`] is the
/// interpolation hot path and this must fold into it with no call and no
/// materialized struct. `interp_cost` is the benchmark that proves it did.
#[inline(always)]
fn screw_parts(rel: &Iso3) -> Screw {
    // Canonicalize the real quaternion to the w ≥ 0 hemisphere so θ/2 ∈ [0, π/2]
    // lands in the principal branch, matching the reference log/exp route. It
    // also makes `cos φ ≥ 0`, which is what keeps `wa + w·cos φ` cancellation
    // free below.
    let q = if rel.q.w < 0.0 { rel.q.neg() } else { rel.q };
    let t = rel.t;

    let q_v = q.vector();
    let sh2 = q_v.norm_squared(); // sin²(θ/2) — no sqrt taken
    let ch = q.w; // cos(θ/2) ≥ 0

    if sh2 < SCREW_DEGENERATE_SQ {
        // `q_d.w/sh2` would overflow. The rotation is the identity to far
        // beyond f64 resolution, so the exact form costs nothing in accuracy,
        // and this branch is unreachable for any physically meaningful input.
        return Screw::Degenerate(Iso3::new(q, t));
    }

    // Dual part q_d = ½·(0,t)⊗q = ½·(−t·q_v, cos(θ/2)·t + t×q_v).
    let q_d = (Quat::from_pure(t) * q).scale(0.5);

    // k = q_d.w / sin²(θ/2). Divergent on its own (q_d.w ∝ sin(θ/2)), but it
    // only ever reaches the result multiplied by q_v ∝ sin(θ/2), so every
    // product below is finite and relatively accurate down to
    // SCREW_DEGENERATE_SQ.
    let k = q_d.w / sh2;

    // m·sin(θ/2) = q_d_v + q_v·k·cos(θ/2). The moment `m` itself diverges as
    // θ → 0; this product does not, and it is the only form the result needs.
    let m_sh = q_d.vector().add(q_v.scale(k * ch));

    Screw::Regular(ScrewParts {
        q_v,
        sh2,
        ch,
        q_d,
        k,
        m_sh,
    })
}

impl ScrewParts {
    /// The screw power at parameter `s`, from the grouped parts.
    #[inline(always)]
    fn pow(&self, s: f64) -> Iso3 {
        let (q_v, sh2, ch, k, m_sh) = (self.q_v, self.sh2, self.ch, self.k, self.m_sh);

        // cos(sφ), and w = sin(sφ)/sin(φ).
        let (cos_sh, w) = if sh2 <= SIN_HALF_THETA_SMALL_SQ {
            // h = 1 − cos(θ/2) via sin²/(1+cos): the direct subtraction cancels
            // catastrophically here, this form cannot (1+cos ∈ [1,2]).
            let phi_sq = theta_sq_from_chord(sh2 / (1.0 + ch));
            let wa = slerp_weight(1.0 - s, phi_sq);
            let wb = slerp_weight(s, phi_sq);
            // cos(sφ) = [sin((1−s)φ) + sin(sφ)·cos φ] / sin φ = wa + wb·cos φ.
            (wa + wb * ch, wb)
        } else {
            // Large arc: reached by a low-rate edge on a fast-rotating body, and
            // by `at_adaptive`'s wide bisection spans. sin(φ) is exactly ‖q_v‖.
            let sh = libm::sqrt(sh2);
            let phi = libm::atan2(sh, ch);
            let (sin_sp, cos_sp) = libm::sincos(s * phi);
            (cos_sp, sin_sp / sh)
        };

        // Recompose the scaled unit dual quaternion. Every `l` and `m` has been
        // folded into `w`, `k` and `m_sh`, so nothing here divides by sin(θ/2).
        let q_r2 = Quat::new(cos_sh, w * q_v.x, w * q_v.y, w * q_v.z);
        let q_d2_v = m_sh.scale(w).add(q_v.scale(-s * k * cos_sh));
        let q_d2 = Quat::new(s * self.q_d.w * w, q_d2_v.x, q_d2_v.y, q_d2_v.z);

        // Recover translation: (0, t') = 2·q_d'⊗q_r'*.
        let t2q = q_d2 * q_r2.conjugate();
        Iso3::new(q_r2, Vec3::new(2.0 * t2q.x, 2.0 * t2q.y, 2.0 * t2q.z))
    }

    /// `ξ = log_se3(rel)` recovered from the parts already in hand.
    ///
    /// # Why this is the cheap route, and what it actually costs
    ///
    /// `docs/PHASE4.md` §2.3 originally claimed `ξ` was already computed and the
    /// derivative was "one scalar multiply". It is not — [`screw_pow`] never
    /// materializes `ξ`, deliberately, because the factored screw quantities
    /// diverge as `θ → 0` while the grouped ones do not. The amendment in that
    /// section records the real number, which is this function:
    ///
    /// * `ω = θ·l = (2φ/sin φ)·q_v`, since `q_v = sin(φ)·l` and `θ = 2φ`.
    /// * `v = d·l + θ·m = (2φ/sin φ)·m_sh − 2k·q_v`, since `d = −2k·sin φ`
    ///   and `m = m_sh/sin φ` — and both `1/sin φ` cancel, which is the whole
    ///   reason this stays accurate at small angles.
    ///
    /// So the cost is `sh = sqrt(sh2)` plus `φ`. On the **large-arc** branch `φ`
    /// is the same `atan2` [`ScrewParts::pow`] already takes. On the
    /// **small-angle** branch — the common one, adjacent samples on one edge —
    /// `φ²` comes from the same transcendental-free `theta_sq_from_chord` the
    /// power uses, so `φ` is a second `sqrt` and **no transcendental is added at
    /// all**.
    ///
    /// The branch is a **speed** choice and nothing else. Measured against a
    /// 50-digit reference, `atan2(sh, ch)` is accurate to ~`1e-16` across the
    /// whole range, and `sqrt(theta_sq_from_chord(·))` matches it to `f64`
    /// inside `θ ≤ 0.15` where it is used — so replacing this branch with an
    /// unconditional `atan2` is *correct*, merely slower. No unit test can tell
    /// them apart, and none pretends to: `interp_deriv_cost` is the guard.
    #[inline]
    fn twist(&self) -> Twist {
        let sh = libm::sqrt(self.sh2);
        let phi = if self.sh2 <= SIN_HALF_THETA_SMALL_SQ {
            libm::sqrt(theta_sq_from_chord(self.sh2 / (1.0 + self.ch)))
        } else {
            libm::atan2(sh, self.ch)
        };
        let c = 2.0 * phi / sh;
        Twist::new(
            self.q_v.scale(c),
            self.m_sh.scale(c).sub(self.q_v.scale(2.0 * self.k)),
        )
    }
}

/// `exp_se3(s·ξ)` for the degenerate arm, where `ξ` is already in hand.
#[inline]
fn scaled_exp(xi: [f64; 6], s: f64) -> Iso3 {
    exp_se3([
        s * xi[0],
        s * xi[1],
        s * xi[2],
        s * xi[3],
        s * xi[4],
        s * xi[5],
    ])
}

/// Raise the rigid transform `rel` to the real power `s` along its screw axis.
///
/// Equivalent to `exp_se3(s · log_se3(rel))` but computed through the unit
/// dual-quaternion screw decomposition. For `s = 1` this returns `rel`; for
/// `s = 0`, the identity.
#[inline]
#[must_use]
pub fn screw_pow(rel: &Iso3, s: f64) -> Iso3 {
    match screw_parts(rel) {
        Screw::Degenerate(c) => scaled_exp(log_se3(c), s),
        Screw::Regular(p) => p.pow(s),
    }
}

/// [`screw_pow`], plus the segment's body twist `ξ = log_se3(rel)` **per unit
/// `s`** — `docs/PHASE4.md` §2.3.
///
/// The twist is a property of the segment, not of `s`: under ScLerp
/// `T(s) = a·exp(s·ξ^)` has the constant body twist `ξ`, so a caller converts to
/// per-second velocity by scaling once, `ξ/Δt`. It does **not** vary across the
/// segment, which is exactly what distinguishes ScLerp from LerpSlerp (§2.4).
///
/// Returns the same pose as [`screw_pow`] bit-for-bit — pinned by
/// `screw_pow_with_twist_agrees_bit_for_bit`, which is what stops the shared
/// prologue from silently diverging.
#[inline]
#[must_use]
pub fn screw_pow_with_twist(rel: &Iso3, s: f64) -> (Iso3, Twist) {
    match screw_parts(rel) {
        Screw::Degenerate(c) => {
            let xi = log_se3(c);
            (scaled_exp(xi, s), Twist::from_se3(xi))
        }
        Screw::Regular(p) => (p.pow(s), p.twist()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::THETA_SLERP_SMALL;
    use crate::reference;

    /// A fixed, deliberately non-axis-aligned unit screw axis.
    const AXIS: Vec3 = Vec3 {
        x: 0.267_261_241_912_424_4,
        y: 0.534_522_483_824_848_8,
        z: 0.801_783_725_737_273_2,
    };

    /// Rotation `theta` about [`AXIS`] with a translation deliberately *not*
    /// perpendicular to it, so the screw pitch term is exercised.
    fn rot(theta: f64) -> Iso3 {
        let half = theta * 0.5;
        let (s, c) = libm::sincos(half);
        Iso3::new(
            Quat::new(c, s * AXIS.x, s * AXIS.y, s * AXIS.z),
            Vec3::new(0.5, -0.3, 0.8),
        )
    }

    fn err(a: &Iso3, b: &Iso3) -> f64 {
        let dq = (a.q.w - b.q.w).abs().max(
            (a.q.x - b.q.x)
                .abs()
                .max((a.q.y - b.q.y).abs().max((a.q.z - b.q.z).abs())),
        );
        let dt = (a.t.x - b.t.x)
            .abs()
            .max((a.t.y - b.t.y).abs().max((a.t.z - b.t.z).abs()));
        dq.max(dt)
    }

    /// The `sin²` form of the fast-path predicate must agree with the shared
    /// angle threshold it stands in for; otherwise `slerp` and `screw_pow`
    /// silently disagree about where the series is valid.
    #[test]
    fn sin_half_theta_small_sq_matches_the_shared_threshold() {
        let expected = libm::sin(THETA_SLERP_SMALL) * libm::sin(THETA_SLERP_SMALL);
        assert_eq!(SIN_HALF_THETA_SMALL_SQ, expected);
    }

    /// The regrouped algebra claims to be well conditioned all the way down to
    /// `SCREW_DEGENERATE_SQ`, which is the justification for lowering the
    /// fallback bound by ~280 orders of magnitude. Sweep it and prove it.
    #[test]
    fn screw_pow_is_accurate_down_to_the_degenerate_threshold() {
        let mut theta = 3.0;
        let mut worst: f64 = 0.0;
        while theta > 1e-160 {
            let rel = rot(theta);
            for &s in &[0.0, 0.125, 0.5, 0.7314, 1.0] {
                let fast = screw_pow(&rel, s);
                let re = reference::sclerp(&Iso3::IDENTITY, &rel, s);
                let e = err(&fast, &re);
                assert!(e < 1e-13, "theta={theta:e} s={s} err={e:e}");
                worst = worst.max(e);
            }
            // s = 1 must return `rel` itself, to near machine precision — the
            // strongest check available at angles where the reference's own
            // log/exp starts to lose digits.
            let e1 = err(&screw_pow(&rel, 1.0), &rel);
            assert!(e1 < 1e-14, "theta={theta:e} s=1 err={e1:e}");
            theta *= 0.5;
        }
        assert!(worst < 1e-13, "worst={worst:e}");
    }

    /// Nothing may jump at the series/exact boundary: a discontinuity there
    /// would surface as a visible glitch on any edge whose rate straddles it.
    ///
    /// Comparing `screw_pow` just below the threshold against `screw_pow` just
    /// above it does **not** test this — the two inputs are different angles, so
    /// they differ by the function's own slope (~3e-13 across 2e-12 of θ), which
    /// swamps any branch mismatch. The continuity statement that actually has
    /// content is that *both* branches track the same smooth reference.
    #[test]
    fn no_discontinuity_across_the_series_threshold() {
        // sin²(θ/2) = SIN_HALF_THETA_SMALL_SQ  =>  θ = 2·THETA_SLERP_SMALL.
        let theta_c = 2.0 * THETA_SLERP_SMALL;
        for d in [1e-12, 1e-9, 1e-6, 1e-3] {
            for theta in [theta_c - d, theta_c + d] {
                let rel = rot(theta);
                for &s in &[0.1, 0.4, 0.9] {
                    let fast = screw_pow(&rel, s);
                    let re = reference::sclerp(&Iso3::IDENTITY, &rel, s);
                    let e = err(&fast, &re);
                    assert!(e < 1e-14, "theta={theta} s={s} err={e:e}");
                }
            }
        }
    }

    /// Both branches must reproduce the endpoints — the interpolation contract
    /// `s = 0 → I`, `s = 1 → rel` is relied on by `Plan::at`.
    #[test]
    fn endpoints_are_exact_on_both_branches() {
        for theta in [1e-9, 0.01, 0.29, 0.31, 1.0, 3.0] {
            let rel = rot(theta);
            assert!(
                err(&screw_pow(&rel, 0.0), &Iso3::IDENTITY) < 1e-15,
                "theta={theta}"
            );
            assert!(err(&screw_pow(&rel, 1.0), &rel) < 1e-14, "theta={theta}");
        }
    }

    /// A pure translation *along* the screw axis is the case where `m·sin(θ/2)`
    /// cancels down to near zero; check the pitch is still carried exactly.
    #[test]
    fn pure_axial_translation_keeps_its_pitch() {
        for theta in [1e-7, 1e-3, 0.1, 0.5] {
            let half = theta * 0.5;
            let (s, c) = libm::sincos(half);
            let rel = Iso3::new(
                Quat::new(c, s * AXIS.x, s * AXIS.y, s * AXIS.z),
                AXIS.scale(2.0),
            );
            for &u in &[0.25, 0.5, 0.75] {
                let fast = screw_pow(&rel, u);
                let re = reference::sclerp(&Iso3::IDENTITY, &rel, u);
                let e = err(&fast, &re);
                assert!(e < 1e-14, "theta={theta:e} s={u} err={e:e}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // screw_pow_with_twist — `docs/PHASE4.md` §2.3
    // -----------------------------------------------------------------------

    /// **The two public entry points must not drift apart.**
    ///
    /// [`screw_pow`] and [`screw_pow_with_twist`] share [`screw_parts`] and
    /// [`ScrewParts::pow`] today. This pins the *contract* that they agree
    /// bit-for-bit, so a future change that specializes one of them — an
    /// `s`-dependent shortcut in the twist path, say — fails here rather than
    /// silently making the derivative disagree with the pose it came with.
    ///
    /// **What this test cannot do**, stated because an earlier version of this
    /// comment claimed otherwise: it cannot detect a change to the *shared*
    /// code, because such a change moves both sides equally. That the extraction
    /// of `screw_parts` was itself invisible was verified separately and once,
    /// by running HEAD's pre-refactor `screw_pow` beside this one over 8500
    /// `(θ, s)` pairs across 33 decades of `θ` — all bit-identical. That probe
    /// is not kept: it would have to be updated on every legitimate change to
    /// [`ScrewParts::pow`], at which point it stops testing the refactor and
    /// starts testing nothing.
    ///
    /// Mutant: make [`screw_pow_with_twist`] return `p.pow(s * 0.999)` ⇒ fails.
    #[test]
    fn screw_pow_with_twist_agrees_bit_for_bit() {
        for k in 0..64 {
            let theta = 3.0 * 0.5f64.powi(k / 4) * ((k % 4) as f64 * 0.25 + 0.25);
            let rel = rot(theta);
            for &s in &[0.0, 0.125, 0.5, 0.75, 1.0] {
                let a = screw_pow(&rel, s);
                let (b, _) = screw_pow_with_twist(&rel, s);
                assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "pose differs at theta={theta:e} s={s}"
                );
            }
        }
    }

    /// **The recovered twist must equal a full `log_se3`** — the independent
    /// route, sharing no code with the screw decomposition.
    ///
    /// Swept across nine decades of `θ` because the whole argument for
    /// recovering `ξ` from the grouped parts is that it stays accurate where the
    /// factored form does not. A test only at `θ ≈ 1` would pass on a formula
    /// that loses every digit at `1e-8`.
    ///
    /// Mutant: drop the `− 2k·q_v` term ⇒ the linear part is wrong for any screw
    /// with pitch, which is every case here (`rot` translates off-axis).
    #[test]
    fn recovered_twist_matches_log_se3() {
        let mut worst = 0.0f64;
        let mut worst_theta = 0.0;
        for k in 0..90 {
            let theta = 3.0 * libm::pow(10.0, -(k as f64) / 10.0);
            let rel = rot(theta);
            let want = log_se3(rel);
            let (_, got) = screw_pow_with_twist(&rel, 0.5);
            let g = got.to_se3();
            let scale = want.iter().fold(0.0f64, |m, x| m.max(x.abs()));
            let e = (0..6).fold(0.0f64, |m, i| m.max((g[i] - want[i]).abs())) / scale;
            if e > worst {
                worst = e;
                worst_theta = theta;
            }
        }
        assert!(
            worst < 1e-12,
            "worst relative twist error {worst:e} at theta={worst_theta:e}"
        );
    }

    /// **The twist does not depend on `s`.** This is the substance of §2.3: under
    /// ScLerp the body twist is *constant across the whole segment*, which is
    /// what makes it exact rather than a finite difference.
    ///
    /// Mutant: make [`ScrewParts::twist`] use `s` anywhere ⇒ fails.
    #[test]
    fn the_twist_is_constant_across_the_segment() {
        for k in 0..20 {
            let rel = rot(0.3 + 0.1 * k as f64);
            let (_, base) = screw_pow_with_twist(&rel, 0.0);
            for &s in &[0.17, 0.5, 0.83, 1.0] {
                let (_, got) = screw_pow_with_twist(&rel, s);
                assert_eq!(got.to_se3(), base.to_se3(), "twist varied with s={s}");
            }
        }
    }

    /// The recovered `ξ` must actually generate the segment: `exp(ξ) == rel`.
    ///
    /// A different claim from `recovered_twist_matches_log_se3` — that one
    /// checks against `log_se3`, this one closes the loop through `exp_se3`, so
    /// a matched pair of sign errors in `log`/`twist` cannot pass both.
    #[test]
    fn exp_of_the_recovered_twist_reproduces_the_segment() {
        for k in 0..20 {
            let theta = 0.05 + 0.15 * k as f64;
            let rel = rot(theta);
            let (_, xi) = screw_pow_with_twist(&rel, 0.5);
            let back = exp_se3(xi.to_se3());
            assert!(
                err(&back, &rel) < 1e-13,
                "exp(xi) != rel at theta={theta}: {}",
                err(&back, &rel)
            );
        }
    }

    /// The degenerate arm must produce a twist too, not silently a zero one.
    ///
    /// `sh2 < 1e-290` means the rotation is identity to far beyond `f64`, but the
    /// *translation* is still whatever it is — a pure translation has `ω = 0` and
    /// `v = t`, and answering `Twist::ZERO` would be wrong by the entire motion.
    ///
    /// Mutant: return `Twist::ZERO` from the `Degenerate` arm ⇒ fails.
    #[test]
    fn the_degenerate_arm_still_reports_the_translation() {
        let pure = Iso3::new(Quat::IDENTITY, Vec3::new(0.5, -0.3, 0.8));
        let (pose, xi) = screw_pow_with_twist(&pure, 0.5);
        assert_eq!(xi.omega, Vec3::ZERO, "a pure translation has no omega");
        assert!(
            (xi.v.x - 0.5).abs() < 1e-15
                && (xi.v.y + 0.3).abs() < 1e-15
                && (xi.v.z - 0.8).abs() < 1e-15,
            "pure translation twist should be the translation itself, got {:?}",
            xi.v
        );
        // And the pose is the half-way point, so the two halves are consistent.
        assert!((pose.t.x - 0.25).abs() < 1e-15);
    }
}
