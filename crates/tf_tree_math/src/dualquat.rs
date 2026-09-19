//! Fast SE(3) screw interpolation via unit dual-quaternion powers.
//!
//! Equals `a · exp_se3(s · log_se3(a⁻¹·b))` ([`crate::reference::sclerp`]), by
//! raising the relative transform's unit dual quaternion
//! `q̂ = q_r + ε q_d` (`q_d = ½·(0,t)⊗q`) to the power `s`.
//!
//! # No transcendental on the small-angle path
//!
//! With `φ = θ/2`: `sin φ` never appears alone (every `1/sin φ` cancels in the
//! result), and `w = sin(sφ)/sin φ` is the slerp weight [`crate::interp`]
//! already evaluates as a polynomial, with `cos(sφ) = wa + w·cos φ`,
//! `wa = sin((1−s)φ)/sin φ`.

use crate::interp::{slerp_weight, theta_sq_from_chord};
use crate::iso3::{exp_se3, log_se3, Iso3, Vec3};
use crate::quat::Quat;
use crate::twist::Twist;

/// `sin²(THETA_SLERP_SMALL)`; pinned by `sin_half_theta_small_sq_matches_the_shared_threshold`.
const SIN_HALF_THETA_SMALL_SQ: f64 = 0.022_331_755_437_196_99;

/// Below this `sin²(θ/2)`, `q_d.w/‖q_v‖²` would overflow: fall back to
/// `exp_se3(s·log_se3(rel))`. See `screw_pow_is_accurate_down_to_the_degenerate_threshold`.
const SCREW_DEGENERATE_SQ: f64 = 1e-290;

// Pinned: `crates/tf_tree_bench/examples/bracket_mix.rs` carries a copy (`docs/decisions/0060` step 0a).
const _: () = assert!(SCREW_DEGENERATE_SQ == 1e-290);

/// The screw decomposition, grouped to stay finite as `θ → 0`.
struct ScrewParts {
    q_v: Vec3,
    sh2: f64,
    ch: f64,
    q_d: Quat,
    k: f64,
    m_sh: Vec3,
}

/// The grouped parts, or the degenerate case with the `w ≥ 0` canonicalized transform.
enum Screw {
    Degenerate(Iso3),
    Regular(ScrewParts),
}

/// The screw-power prologue; `#[inline(always)]` is load-bearing (`interp_cost`).
#[inline(always)]
fn screw_parts(rel: &Iso3) -> Screw {
    // w ≥ 0: principal branch, and `cos φ ≥ 0` keeps `wa + w·cos φ` cancellation free.
    let q = if rel.q.w < 0.0 { rel.q.neg() } else { rel.q };
    let t = rel.t;

    let q_v = q.vector();
    let sh2 = q_v.norm_squared(); // sin²(θ/2) — no sqrt taken
    let ch = q.w; // cos(θ/2) ≥ 0

    if sh2 < SCREW_DEGENERATE_SQ {
        return Screw::Degenerate(Iso3::new(q, t));
    }

    // q_d = ½·(0,t)⊗q, written out (12 multiplies, not 16). The association is
    // the general product's tree minus the `0.0 * b` terms and must not be
    // tidied: `Vec3::dot`/`cross` is not bit-identical.
    let q_d = Quat::new(
        -(t.x * q.x) - t.y * q.y - t.z * q.z,
        t.x * q.w + t.y * q.z - t.z * q.y,
        -(t.x * q.z) + t.y * q.w + t.z * q.x,
        t.x * q.y - t.y * q.x + t.z * q.w,
    )
    .scale(0.5);

    // k diverges alone but only multiplies `q_v`: finite down to SCREW_DEGENERATE_SQ.
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

        let q_r2 = Quat::new(cos_sh, w * q_v.x, w * q_v.y, w * q_v.z);
        let q_d2_v = m_sh.scale(w).add(q_v.scale(-s * k * cos_sh));
        let q_d2 = Quat::new(s * self.q_d.w * w, q_d2_v.x, q_d2_v.y, q_d2_v.z);

        // Recover translation: (0, t') = 2·q_d'⊗q_r'*.
        let t2q = q_d2 * q_r2.conjugate();
        Iso3::new(q_r2, Vec3::new(2.0 * t2q.x, 2.0 * t2q.y, 2.0 * t2q.z))
    }

    /// `ξ = log_se3(rel)` from the parts in hand (`docs/PHASE4.md` §2.3):
    /// `ω = (2φ/sin φ)·q_v`, `v = (2φ/sin φ)·m_sh − 2k·q_v`. The `φ` branch is a
    /// speed choice only (`deriv_cost`).
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

/// The segment's body twist `ξ = log_se3(rel)` without a power
/// (`docs/PHASE4.md` §2.3); used by [`crate::ScLerp::eval_with_twist`] at `s ∈ {0, 1}`.
#[inline]
#[must_use]
pub fn screw_twist(rel: &Iso3) -> Twist {
    match screw_parts(rel) {
        Screw::Degenerate(c) => Twist::from_se3(log_se3(c)),
        Screw::Regular(p) => p.twist(),
    }
}

/// [`screw_pow`], plus the body twist `ξ = log_se3(rel)` per unit `s`
/// (`docs/PHASE4.md` §2.3); constant across the segment, so velocity is `ξ/Δt`.
/// The pose equals [`screw_pow`] bit-for-bit.
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

    /// A non-axis-aligned unit screw axis.
    const AXIS: Vec3 = Vec3 {
        x: 0.267_261_241_912_424_4,
        y: 0.534_522_483_824_848_8,
        z: 0.801_783_725_737_273_2,
    };

    /// Rotation `theta` about [`AXIS`] with a non-perpendicular translation.
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

    /// The `sin²` predicate agrees with the shared angle threshold.
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
            let e1 = err(&screw_pow(&rel, 1.0), &rel);
            assert!(e1 < 1e-14, "theta={theta:e} s=1 err={e1:e}");
            theta *= 0.5;
        }
        assert!(worst < 1e-13, "worst={worst:e}");
    }

    /// No jump at the series/exact boundary: both branches track the reference.
    #[test]
    fn no_discontinuity_across_the_series_threshold() {
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

    /// [`screw_pow`] and [`screw_pow_with_twist`] agree bit-for-bit.
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

    /// The recovered twist equals `log_se3` across nine decades of `θ`.
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

    /// The twist does not depend on `s` (§2.3).
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

    /// `exp(ξ) == rel` for the recovered twist.
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

    /// The degenerate arm reports a pure translation as `ω = 0`, `v = t`.
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
        assert!((pose.t.x - 0.25).abs() < 1e-15);
    }
}
