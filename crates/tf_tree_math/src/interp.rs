//! Interpolation between two `Iso3` poses.
//!
//! Two policies share the [`Interp`] trait:
//!
//! * [`ScLerp`] — the SE(3) screw geodesic. Left- and right-invariant. Computed
//!   with the fast dual-quaternion power ([`crate::dualquat::screw_pow`]),
//!   proptested against [`crate::reference::sclerp`].
//! * [`LerpSlerp`] — tf2-compatible: translation LERP, rotation shortest-arc
//!   SLERP. Left-invariant but **not** right-invariant; that asymmetry is why
//!   `ScLerp` is the default (`docs/PHASE1.md` §3.4; `docs/PROJECT.md` §5 D5).

use crate::dualquat::{screw_pow, screw_pow_with_twist, screw_twist};
use crate::iso3::Iso3;
use crate::quat::Quat;
use crate::twist::Twist;

/// Below this half-angle between the two quaternions, `slerp` falls back to a
/// normalized LERP to avoid dividing by `sin(angle) → 0`.
const SLERP_LERP_FALLBACK: f64 = 1e-6;

/// Quaternion angle above which [`slerp`] uses the exact `acos`/`sin` form; at
/// or below it, the series in [`slerp_weight`]. Measured by
/// `slerp_series_matches_exact_below_threshold`; shared with
/// [`crate::dualquat::screw_pow`].
pub(crate) const THETA_SLERP_SMALL: f64 = 0.15;

// Quoted as literals in `slerp`'s rustdoc, the README, `docs/API.md` §6 and `tests/slerp_public.rs`.
const _: () = assert!(SLERP_LERP_FALLBACK == 1e-6);
const _: () = assert!(THETA_SLERP_SMALL == 0.15);

/// Interpolate between two poses `a` (at `s = 0`) and `b` (at `s = 1`).
pub trait Interp {
    /// Interpolate at parameter `s`. `s = 0` returns `a` exactly and `s = 1`
    /// returns `b` exactly; intermediate `s` follows the policy's path.
    fn eval(a: &Iso3, b: &Iso3, s: f64) -> Iso3;
}

/// SE(3) screw-geodesic interpolation — the default policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScLerp;

/// tf2-compatible interpolation: translation LERP + rotation SLERP.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LerpSlerp;

impl Interp for ScLerp {
    #[inline]
    fn eval(a: &Iso3, b: &Iso3, s: f64) -> Iso3 {
        // Endpoints are exact by construction (proptest #6).
        if s == 0.0 {
            return *a;
        }
        if s == 1.0 {
            return *b;
        }
        // a · (a⁻¹·b)ˢ — the fast screw power of the relative transform.
        let rel = a.inv_mul(b);
        *a * screw_pow(&rel, s)
    }
}

impl ScLerp {
    /// [`Interp::eval`], plus the body twist `ξ = log_se3(a⁻¹b)` per unit `s`
    /// (`docs/PHASE4.md` §2.3); for ns stamps, `V^b = ξ · 1e9/(t_j − t_i)`. The
    /// pose is bit-identical to [`Interp::eval`]. No [`LerpSlerp`] equivalent (§2.4).
    #[inline]
    #[must_use]
    pub fn eval_with_twist(a: &Iso3, b: &Iso3, s: f64) -> (Iso3, Twist) {
        let rel = a.inv_mul(b);
        // Endpoints first: LLVM does not sink the power out of the untaken branch.
        if s == 0.0 {
            return (*a, screw_twist(&rel));
        }
        if s == 1.0 {
            return (*b, screw_twist(&rel));
        }
        let (rel_pow, xi) = screw_pow_with_twist(&rel, s);
        (*a * rel_pow, xi)
    }
}

impl Interp for LerpSlerp {
    #[inline]
    fn eval(a: &Iso3, b: &Iso3, s: f64) -> Iso3 {
        if s == 0.0 {
            return *a;
        }
        if s == 1.0 {
            return *b;
        }
        let t = a.t.scale(1.0 - s).add(b.t.scale(s));
        let q = slerp(a.q, b.q, s);
        Iso3::new(q, t)
    }
}

impl Iso3 {
    /// `self⁻¹ · rhs`, the relative transform from `self` to `rhs`.

    #[inline]
    #[must_use]
    fn inv_mul(&self, rhs: &Iso3) -> Iso3 {
        let qi = self.q.conjugate();
        let q = qi * rhs.q;
        let t = qi.rotate(rhs.t.sub(self.t));
        Iso3::new(q, t)
    }
}

/// Shortest-arc spherical linear interpolation of two unit quaternions.
///
/// The rotation kernel of [`LerpSlerp`] (`docs/API.md` §2.7). Every angle below
/// is a quaternion angle, half the rotation spanned.
///
/// # Preconditions
///
/// Both inputs must be unit; nothing checks. `s` is a fraction, not a stamp.
///
/// # Range of `s`
///
/// `s` belongs to `[0, 1]`; nothing clamps, and extrapolation is unsupported
/// (`out_of_range_s_extrapolates_and_only_the_closed_form_holds`).
///
/// # Storage order
///
/// [`Quat`] is `[w, x, y, z]`, scalar first.
///
/// # Endpoints and degenerate inputs
///
/// The weights at `s = 0` and `s = 1` are exact, with these exceptions:
///
/// * **`s = 1` returns `-qb` when `qa·qb < 0`** (the sign fix). Compare rotations.
/// * **Below the LERP fallback** (angle under `1e-6`) the result is a
///   renormalized LERP, exact to an ulp
///   (`endpoints_lose_bit_exactness_only_in_the_lerp_fallback`).
/// * **A `-0.0` component is lost** (`signed_zero_components_are_the_endpoint_exception`).
/// * **Numerically identical inputs return `qa` for every `s`**, unread.
///
/// A `NaN` `s` propagates except through the identical-input return; do not add
/// a `sin_angle == 0.0` guard (`nan_propagates_except_through_the_identical_input_return`).
///
/// # Numerics
///
/// Above `THETA_SLERP_SMALL` (`0.15` rad) the weights are the closed `acos`/`sin`
/// form; below it, a six-term series with θ² from the chord.
///
/// ```
/// use tf_tree_math::{exp_so3, slerp, Quat, Vec3};
///
/// let qa = Quat::IDENTITY;
/// let qb = exp_so3(Vec3::new(0.0, 0.0, core::f64::consts::FRAC_PI_2));
/// let mid = slerp(qa, qb, 0.5);
///
/// // Half of a 90° yaw is a 45° yaw, and the result is unit without a
/// // normalization step.
/// let quarter = exp_so3(Vec3::new(0.0, 0.0, core::f64::consts::FRAC_PI_4));
/// assert!(mid.sub(quarter).norm() < 1e-15);
/// assert!((mid.norm() - 1.0).abs() < 1e-15);
/// ```
#[inline]
#[must_use]
pub fn slerp(qa: Quat, qb: Quat, s: f64) -> Quat {
    let dot = qa.dot(qb);
    let qb = if dot < 0.0 { qb.neg() } else { qb };

    // `|qb - qa|² = 2 - 2·dot`, from component differences: no `1 - dot` cancellation.
    let h = 0.5 * qa.sub(qb).norm_squared(); // = 1 - |dot|
    if h <= 0.0 {
        return qa; // identical (or numerically identical) inputs
    }

    let theta_sq = theta_sq_from_chord(h);

    if theta_sq <= THETA_SLERP_SMALL * THETA_SLERP_SMALL {
        if theta_sq < SLERP_LERP_FALLBACK * SLERP_LERP_FALLBACK {
            return lerp_norm(qa, qb, s);
        }
        let wa = slerp_weight(1.0 - s, theta_sq);
        let wb = slerp_weight(s, theta_sq);
        return qa.scale(wa).add(qb.scale(wb));
    }

    // Large arc: exact form.
    let angle = libm::acos(if dot < 0.0 { -dot } else { dot }.min(1.0));
    let sin_angle = libm::sin(angle);
    let wa = libm::sin((1.0 - s) * angle) / sin_angle;
    let wb = libm::sin(s * angle) / sin_angle;
    qa.scale(wa).add(qb.scale(wb))
}

/// `θ²` from `h = 1 − |cos θ|`, without `acos`.
///
/// `θ² = 2h·Σ Cₖ hᵏ` from the squared `asin` series of the half-chord;
/// `Cₙ = 2ⁿ⁺¹ / ((n+1)²·C(2n+2, n+1))`.
///
/// ```text
/// C₀..C₇ = 1, 1/6, 2/45, 1/70, 8/1575, 4/2079, 16/21021, 2/6435
/// ```
///
/// Eight terms are needed; see `theta_sq_matches_acos_across_the_fast_path`.
#[inline]
#[must_use]
pub(crate) fn theta_sq_from_chord(h: f64) -> f64 {
    const C: [f64; 8] = [
        1.0,
        1.0 / 6.0,
        2.0 / 45.0,
        1.0 / 70.0,
        8.0 / 1575.0,
        4.0 / 2079.0,
        16.0 / 21021.0,
        2.0 / 6435.0,
    ];
    let mut acc = C[7];
    for &c in C[..7].iter().rev() {
        acc = c + h * acc;
    }
    2.0 * h * acc
}

/// `sin(a·θ)/sin(θ)` evaluated as a series in `u = θ²`, for `|θ| ≤`
/// [`THETA_SLERP_SMALL`].
///
/// With `x = a²`:
///
/// ```text
/// sin(aθ)/sin(θ) = a·[ 1 + (1−x)·u/6
///                        + (1−x)(7−3x)·u²/360
///                        + (1−x)(31−18x+3x²)·u³/15120
///                        + (1−x)(381−239x+55x²−5x³)·u⁴/1814400
///                        + (1−x)(2555−1636x+410x²−52x³+3x⁴)·u⁵/119750400 ]
/// ```
///
/// From the two Maclaurin series; the `(1 − x)` factor keeps both endpoints
/// exact at any term count.
#[inline]
#[must_use]
pub(crate) fn slerp_weight(a: f64, u: f64) -> f64 {
    let x = a * a;
    let k = 1.0 - x;
    let c1 = 1.0 / 6.0;
    let c2 = (7.0 - 3.0 * x) / 360.0;
    let c3 = (31.0 + x * (-18.0 + 3.0 * x)) / 15120.0;
    let c4 = (381.0 + x * (-239.0 + x * (55.0 - 5.0 * x))) / 1_814_400.0;
    let c5 = (2555.0 + x * (-1636.0 + x * (410.0 + x * (-52.0 + 3.0 * x)))) / 119_750_400.0;
    a * (1.0 + k * u * (c1 + u * (c2 + u * (c3 + u * (c4 + u * c5)))))
}

/// Normalized linear interpolation of two quaternions.
#[inline]
#[must_use]
fn lerp_norm(qa: Quat, qb: Quat, s: f64) -> Quat {
    qa.scale(1.0 - s).add(qb.scale(s)).normalize()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::iso3::Vec3;
    use crate::quat::exp_so3;

    /// `sin(aθ)/sin(θ)` directly.
    fn weight_exact(a: f64, theta: f64) -> f64 {
        libm::sin(a * theta) / libm::sin(theta)
    }

    /// `h -> theta^2` is exact across the fast path, tested apart from `slerp_weight`.
    #[test]
    fn theta_sq_matches_acos_across_the_fast_path() {
        let mut worst = 0.0f64;
        let mut worst_theta = 0.0;
        for i in 0..=2000 {
            let theta = THETA_SLERP_SMALL * (i as f64) / 2000.0;
            if theta < 1e-9 {
                continue;
            }
            let d = libm::sin(0.5 * theta);
            let h = 2.0 * d * d;
            let got = theta_sq_from_chord(h);
            let want = theta * theta;
            let rel = (got - want).abs() / want;
            if rel > worst {
                worst = rel;
                worst_theta = theta;
            }
        }
        assert!(
            worst < 1e-15,
            "theta^2 conversion is off by {worst:e} at theta={worst_theta:e} \
             — check the C[] coefficients and the term count"
        );
    }

    /// `docs/PHASE1.md` §3.3 sweep: series vs exact holds 1e-15 up to `THETA_SLERP_SMALL`.
    #[test]
    fn slerp_series_matches_exact_below_threshold() {
        let mut worst = 0.0f64;
        let mut worst_at = (0.0, 0.0);
        for i in 0..=240 {
            let theta = 1e-9 * libm::pow(10.0, i as f64 * 8.5 / 240.0);
            if theta > THETA_SLERP_SMALL {
                break;
            }
            let u = theta * theta;
            for k in 0..=20 {
                let a = k as f64 / 20.0;
                let series = slerp_weight(a, u);
                let exact = weight_exact(a, theta);
                // Guard the a = 0 root.
                let denom = if exact.abs() > 1e-300 {
                    exact.abs()
                } else {
                    1.0
                };
                let rel = (series - exact).abs() / denom;
                if rel > worst {
                    worst = rel;
                    worst_at = (theta, a);
                }
            }
        }
        assert!(
            worst < 1e-15,
            "series/exact disagree by {worst:e} at theta={:e}, a={} \
             — THETA_SLERP_SMALL ({THETA_SLERP_SMALL}) is too permissive",
            worst_at.0,
            worst_at.1
        );
    }

    /// The series loses 1e-15 beyond its range.
    #[test]
    fn series_degrades_beyond_its_range() {
        let theta = 0.45f64;
        let u = theta * theta;
        let worst = (0..=20)
            .map(|k| {
                let a = k as f64 / 20.0;
                let e = weight_exact(a, theta);
                (slerp_weight(a, u) - e).abs() / e.abs().max(1e-300)
            })
            .fold(0.0f64, f64::max);
        assert!(
            worst > 1e-15,
            "the series is accurate at theta={theta}; the threshold could be raised \
             (worst rel err {worst:e}) — re-derive it from a sweep"
        );
    }

    /// Both branches track the exact form across the threshold.
    #[test]
    fn no_discontinuity_across_the_threshold() {
        let n = (0.3f64 * 0.3 + 0.5 * 0.5 + 0.81 * 0.81).sqrt();
        let axis = Vec3::new(0.3 / n, -0.5 / n, 0.81 / n);
        let s = 0.37;
        let mut worst = 0.0f64;
        for i in 0..4000 {
            let theta = THETA_SLERP_SMALL * 0.5 + (i as f64) * (THETA_SLERP_SMALL / 4000.0);
            let qa = Quat::IDENTITY;
            let qb = exp_so3(axis.scale(2.0 * theta));

            // Compare against the closed form, not adjacent samples.
            let angle = libm::acos(qa.dot(qb).min(1.0));
            let sin_angle = libm::sin(angle);
            let want = qa
                .scale(libm::sin((1.0 - s) * angle) / sin_angle)
                .add(qb.scale(libm::sin(s * angle) / sin_angle));

            let got = slerp(qa, qb, s);
            let d = (got.w - want.w)
                .abs()
                .max((got.x - want.x).abs())
                .max((got.y - want.y).abs())
                .max((got.z - want.z).abs());
            assert!(d < 1e-15, "theta={theta} err={d:e}");
            worst = worst.max(d);
        }
        assert!(worst > 0.0, "no sample was actually compared");
    }

    /// Endpoints stay exact on both branches (proptest #6, `docs/PHASE1.md` §10.1).
    #[test]
    fn endpoints_are_exact_on_both_branches() {
        let axis = Vec3::new(1.0, 0.0, 0.0);
        // Below pi/2 in quaternion angle: past it the sign fix returns `-qb`.
        for &theta in &[1e-7, 1e-3, 0.1, 0.24, 0.26, 1.0, 1.5] {
            let qa = Quat::IDENTITY;
            let qb = exp_so3(axis.scale(2.0 * theta));
            let zero = slerp(qa, qb, 0.0);
            assert_eq!(
                (zero.w, zero.x, zero.y, zero.z),
                (qa.w, qa.x, qa.y, qa.z),
                "s=0 @ {theta}"
            );
            let one = slerp(qa, qb, 1.0);
            for (g, e) in [(one.w, qb.w), (one.x, qb.x), (one.y, qb.y), (one.z, qb.z)] {
                assert!((g - e).abs() < 1e-15, "s=1 @ {theta}: {g} vs {e}");
            }
        }
    }

    /// `eval_with_twist`'s pose is bit-identical to `eval`'s.
    #[test]
    fn eval_with_twist_pose_matches_eval() {
        for k in 0..50 {
            let f = k as f64;
            let a = crate::exp_se3([
                0.4 * (f * 0.31).sin(),
                -0.9 * (f * 0.17).cos(),
                0.6 * (f * 0.53).sin(),
                2.0 * (f * 0.11).cos(),
                -((f * 0.29).sin()),
                1.5 * (f * 0.43).cos(),
            ]);
            let b = crate::exp_se3([
                0.4 * (f * 0.37).cos(),
                -0.9 * (f * 0.19).sin(),
                0.6 * (f * 0.59).cos(),
                2.0 * (f * 0.13).sin(),
                -((f * 0.23).cos()),
                1.5 * (f * 0.47).sin(),
            ]);
            for j in 0..=8 {
                let s = j as f64 / 8.0;
                let want = <ScLerp as Interp>::eval(&a, &b, s);
                let (got, _) = ScLerp::eval_with_twist(&a, &b, s);
                assert_eq!(want.to_bits(), got.to_bits(), "pose differs at s={s}");
            }
        }
    }
}
