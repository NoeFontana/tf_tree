//! Fast SE(3) screw interpolation via unit dual-quaternion powers.
//!
//! The reference geodesic interpolation is `a · exp_se3(s · log_se3(a⁻¹·b))`
//! (see [`crate::reference::sclerp`]). The screw form here computes the same
//! result by raising the relative transform's unit dual quaternion
//! `q̂ = q_r + ε q_d` (`q_r = q`, `q_d = ½·(0,t)⊗q`) to the power `s`, which
//! scales the screw angle `θ` and pitch translation `d` by `s`.
//!
//! # The hot path spends no transcendental at all
//!
//! With `φ = θ/2` the power needs only `cos(sφ)` and `sin(sφ)`, and two
//! observations remove every `sqrt`, `atan2`, `sincos` and division by a
//! vanishing quantity:
//!
//! 1. **`sin φ` never appears alone.** `l`, `d` and `m` are singular as `φ → 0`,
//!    but every product reaching the result (`sin(sφ)·l`, `m·sin(sφ)`,
//!    `½sd·sin(sφ)`) is finite, because each `1/sin φ` cancels.
//! 2. **`sin(sφ)/sin(φ)` is the slerp weight** [`crate::interp`] already
//!    evaluates as a polynomial in `u = φ²`. With `w = sin(sφ)/sin φ` and
//!    `wa = sin((1−s)φ)/sin φ`, `cos(sφ) = wa + w·cos φ`, an all-positive sum
//!    for `s ∈ [0,1]` and `cos φ ≥ 0`.

use crate::interp::{slerp_weight, theta_sq_from_chord};
use crate::iso3::{exp_se3, log_se3, Iso3, Vec3};
use crate::quat::Quat;
use crate::twist::Twist;

/// `sin²(THETA_SLERP_SMALL)` — the fast-path predicate in `sin²(θ/2)` (`‖q_v‖²`,
/// already in hand). Pinned by `sin_half_theta_small_sq_matches_the_shared_threshold`.
const SIN_HALF_THETA_SMALL_SQ: f64 = 0.022_331_755_437_196_99;

/// Below this `sin²(θ/2)` the dual part underflows, so the degenerate case
/// routes through the exact `exp_se3(s·log_se3(rel))` fallback.
///
/// The regrouped algebra forms no divergent intermediate; the only remaining
/// hazard is `q_d.w/‖q_v‖²` overflowing as `‖q_v‖²` denormalizes. Validated by
/// `screw_pow_is_accurate_down_to_the_degenerate_threshold`.
const SCREW_DEGENERATE_SQ: f64 = 1e-290;

// Pinned because `crates/tf_tree_bench/examples/bracket_mix.rs` carries a copy
// (`docs/decisions/0060` step 0a) the compiler cannot check; `SIN_HALF_THETA_SMALL_SQ`
// is tied to `interp.rs`'s threshold by its own test.
const _: () = assert!(SCREW_DEGENERATE_SQ == 1e-290);

/// The screw decomposition of a transform, in the grouped form that stays finite
/// as `θ → 0`; shared by [`screw_pow`] and [`screw_pow_with_twist`].
struct ScrewParts {
    q_v: Vec3,
    sh2: f64,
    ch: f64,
    q_d: Quat,
    k: f64,
    m_sh: Vec3,
}

/// Either the grouped screw parts, or the degenerate case carrying the
/// **canonicalized** transform for `log_se3` (a `w < 0` quaternion would land on
/// the other branch, `θ − 2π`).
enum Screw {
    Degenerate(Iso3),
    Regular(ScrewParts),
}

/// The prologue of the screw power. `#[inline(always)]` is load-bearing: it must
/// fold into the hot path with no call and no materialized struct
/// (`interp_cost`).
#[inline(always)]
fn screw_parts(rel: &Iso3) -> Screw {
    // Canonicalize to w ≥ 0 so θ/2 ∈ [0, π/2] is the principal branch and
    // `cos φ ≥ 0` keeps `wa + w·cos φ` cancellation free.
    let q = if rel.q.w < 0.0 { rel.q.neg() } else { rel.q };
    let t = rel.t;

    let q_v = q.vector();
    let sh2 = q_v.norm_squared(); // sin²(θ/2) — no sqrt taken
    let ch = q.w; // cos(θ/2) ≥ 0

    if sh2 < SCREW_DEGENERATE_SQ {
        // Identity to far beyond f64 resolution; unreachable for physical input.
        return Screw::Degenerate(Iso3::new(q, t));
    }

    // Dual part q_d = ½·(0,t)⊗q. Written out rather than
    // `(Quat::from_pure(t) * q).scale(0.5)`: LLVM cannot fold the literal `0.0`
    // operands away without `nnan`/`nsz`, so that form costs a full Hamilton
    // product (16 multiplies against 12 here).
    //
    // **The association is load-bearing and must not be tidied**: it is the
    // general product's own tree with the `0.0 * b` terms deleted, so the natural
    // `Vec3::dot`/`cross` spelling is not bit-identical. Residual, intermediate
    // only: at `t = 0` this `q_d.w` is `-0.0` where the general product gave
    // `+0.0`; no output differs (compared bit-for-bit over 414 336 cases).
    let q_d = Quat::new(
        -(t.x * q.x) - t.y * q.y - t.z * q.z,
        t.x * q.w + t.y * q.z - t.z * q.y,
        -(t.x * q.z) + t.y * q.w + t.z * q.x,
        t.x * q.y - t.y * q.x + t.z * q.w,
    )
    .scale(0.5);

    // k = q_d.w / sin²(θ/2) diverges alone but only ever multiplies `q_v`, so
    // every product below is finite down to SCREW_DEGENERATE_SQ.
    let k = q_d.w / sh2;

    // m·sin(θ/2) = q_d_v + q_v·k·cos(θ/2).
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
            // h = 1 − cos(θ/2) via sin²/(1+cos): direct subtraction cancels.
            let phi_sq = theta_sq_from_chord(sh2 / (1.0 + ch));
            let wa = slerp_weight(1.0 - s, phi_sq);
            let wb = slerp_weight(s, phi_sq);
            // cos(sφ) = [sin((1−s)φ) + sin(sφ)·cos φ] / sin φ = wa + wb·cos φ.
            (wa + wb * ch, wb)
        } else {
            // Large arc; sin(φ) is exactly ‖q_v‖.
            let sh = libm::sqrt(sh2);
            let phi = libm::atan2(sh, ch);
            let (sin_sp, cos_sp) = libm::sincos(s * phi);
            (cos_sp, sin_sp / sh)
        };

        // Recompose; every `l` and `m` is folded into `w`, `k`, `m_sh`.
        let q_r2 = Quat::new(cos_sh, w * q_v.x, w * q_v.y, w * q_v.z);
        let q_d2_v = m_sh.scale(w).add(q_v.scale(-s * k * cos_sh));
        let q_d2 = Quat::new(s * self.q_d.w * w, q_d2_v.x, q_d2_v.y, q_d2_v.z);

        // Recover translation: (0, t') = 2·q_d'⊗q_r'*.
        let t2q = q_d2 * q_r2.conjugate();
        Iso3::new(q_r2, Vec3::new(2.0 * t2q.x, 2.0 * t2q.y, 2.0 * t2q.z))
    }

    /// `ξ = log_se3(rel)` recovered from the parts already in hand
    /// (`docs/PHASE4.md` §2.3; [`screw_pow`] never materializes `ξ`).
    ///
    /// * `ω = (2φ/sin φ)·q_v`.
    /// * `v = (2φ/sin φ)·m_sh − 2k·q_v`; both `1/sin φ` cancel, keeping it
    ///   accurate at small angles.
    ///
    /// `φ` is the `atan2` the power takes on the large-arc branch, and the
    /// transcendental-free `sqrt(theta_sq_from_chord(·))` on the small-angle one.
    /// The branch is a speed choice only; `deriv_cost` is the guard.
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
/// Equivalent to `exp_se3(s · log_se3(rel))`, via the unit dual-quaternion screw
/// decomposition. `s = 1` returns `rel`; `s = 0`, the identity.
#[inline]
#[must_use]
pub fn screw_pow(rel: &Iso3, s: f64) -> Iso3 {
    match screw_parts(rel) {
        Screw::Degenerate(c) => scaled_exp(log_se3(c), s),
        Screw::Regular(p) => p.pow(s),
    }
}

/// The segment's body twist `ξ = log_se3(rel)` **without** raising `rel` to any
/// power — `docs/PHASE4.md` §2.3. Taken by [`crate::ScLerp::eval_with_twist`] at
/// `s ∈ {0, 1}`, the most frequently queried stamps on any edge.
#[inline]
#[must_use]
pub fn screw_twist(rel: &Iso3) -> Twist {
    match screw_parts(rel) {
        Screw::Degenerate(c) => Twist::from_se3(log_se3(c)),
        Screw::Regular(p) => p.twist(),
    }
}

/// [`screw_pow`], plus the segment's body twist `ξ = log_se3(rel)` **per unit
/// `s`** — `docs/PHASE4.md` §2.3.
///
/// Under ScLerp the body twist is constant across the segment (unlike
/// LerpSlerp, §2.4), so a caller gets per-second velocity by scaling `ξ/Δt`.
/// The pose equals [`screw_pow`] bit-for-bit
/// (`screw_pow_with_twist_agrees_bit_for_bit`).
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

    /// Rotation `theta` about [`AXIS`] with a translation not perpendicular to
    /// it, exercising the pitch term.
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

    /// The `sin²` predicate must agree with the shared angle threshold.
    #[test]
    fn sin_half_theta_small_sq_matches_the_shared_threshold() {
        let expected = libm::sin(THETA_SLERP_SMALL) * libm::sin(THETA_SLERP_SMALL);
        assert_eq!(SIN_HALF_THETA_SMALL_SQ, expected);
    }

    /// Well conditioned all the way down to `SCREW_DEGENERATE_SQ`.
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
            // s = 1 must return `rel` to near machine precision.
            let e1 = err(&screw_pow(&rel, 1.0), &rel);
            assert!(e1 < 1e-14, "theta={theta:e} s=1 err={e1:e}");
            theta *= 0.5;
        }
        assert!(worst < 1e-13, "worst={worst:e}");
    }

    /// Nothing may jump at the series/exact boundary. Comparing just below with
    /// just above tests only the function's own slope, so the check is that
    /// *both* branches track the same smooth reference.
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

    /// Both branches reproduce the endpoints: `s = 0 → I`, `s = 1 → rel`.
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

    /// A pure translation *along* the screw axis cancels `m·sin(θ/2)`; the pitch
    /// must still be carried exactly.
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

    /// **The two public entry points must not drift apart**: [`screw_pow`] and
    /// [`screw_pow_with_twist`] must agree bit-for-bit. It cannot detect a change
    /// to the *shared* code, which moves both sides equally.
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

    /// **The recovered twist must equal a full `log_se3`**, swept across nine
    /// decades of `θ` (a test only at `θ ≈ 1` would pass a formula that loses
    /// every digit at `1e-8`).
    ///
    /// Mutant: drop the `− 2k·q_v` term ⇒ the linear part is wrong for any screw
    /// with pitch, which is every case here.
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

    /// **The twist does not depend on `s`** (§2.3), which makes it exact rather
    /// than a finite difference.
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

    /// The recovered `ξ` must generate the segment: `exp(ξ) == rel`, closing the
    /// loop through `exp_se3` so a matched pair of sign errors cannot pass.
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

    /// The degenerate arm must produce a twist, not a zero one: a pure
    /// translation has `ω = 0`, `v = t`.
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
        // The pose is the half-way point, so the two halves are consistent.
        assert!((pose.t.x - 0.25).abs() < 1e-15);
    }
}
