//! Interpolation between two `Iso3` poses. Two policies share [`Interp`]:
//!
//! * [`ScLerp`] — the SE(3) screw geodesic, **the default**: left- and
//!   right-invariant, computed with the fast dual-quaternion power
//!   ([`crate::dualquat::screw_pow`]), proptested against
//!   [`crate::reference::sclerp`].
//! * [`LerpSlerp`] — tf2-compatible: translation LERP, rotation shortest-arc
//!   SLERP. Left-invariant but **not** right-invariant, which is why `ScLerp`
//!   is the default (`docs/PHASE1.md` §3.4; `docs/PROJECT.md` §5 D5).

use crate::dualquat::{screw_pow, screw_pow_with_twist, screw_twist};
use crate::iso3::Iso3;
use crate::quat::Quat;
use crate::twist::Twist;

/// Below this half-angle between the two quaternions, `slerp` falls back to a
/// normalized LERP to avoid dividing by `sin(angle) → 0`.
const SLERP_LERP_FALLBACK: f64 = 1e-6;

/// Above this angle (radians) between the two quaternions, [`slerp`] uses the
/// exact `acos`/`sin` form; at or below it, the transcendental-free series in
/// [`slerp_weight`].
///
/// **A *quaternion* angle — *half* the rotation** spanned (`qa·qb = cos(Δ/2)`),
/// so `0.15` is a rotation of `0.30` rad.
///
/// Measured, not guessed, like `THETA_SMALL` (`docs/PHASE1.md` §3.3;
/// `docs/PROJECT.md` §5 D12): an eyeballed 0.25 held only **3e-9**, seven
/// orders worse than claimed. Largest θ holding 1e-15 relative error by term
/// count: 4 terms 0.037 · 5 terms 0.091 · **6 terms 0.165** · 7 terms 0.248
/// rad, and [`slerp_weight`] uses six. Since the pair is two adjacent samples
/// on one edge, θ is half an arc set by the publish rate: a 180 °/s body
/// crosses over at `f = ω/(2·0.15)` = **10.47 Hz**, so a 10 Hz edge takes the
/// exact path, clearing the threshold by **4.7%** — marginal, not comfortable.
///
/// Shared with [`crate::dualquat::screw_pow`]: the identical `sin(a·φ)/sin(φ)`
/// series over the identical half-angle range.
pub(crate) const THETA_SLERP_SMALL: f64 = 0.15;

// Neither constant is `pub` — a re-measurement may move them — yet both are
// quoted as literals elsewhere, and these assertions can only refuse to compile
// until whoever moved one comes here; they cannot find the prose. The sites,
// because "grep for `0.15`" over this repository is not short:
//
// * `slerp`'s rustdoc below (`# Angles`, `# Preconditions`, `# Numerics`) — the
//   crates.io page, the only form an external caller can act on.
// * `THETA_SLERP_SMALL`'s doc comment above, and `slerp_weight`'s.
// * `crates/tf_tree_math/README.md`, *Numerics, and where the constants came
//   from* — the crates.io front page.
// * `docs/API.md` §6 row 16.
// * `crates/tf_tree_math/tests/slerp_public.rs`, one `THETA_CROSSOVER` const.
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
    /// [`Interp::eval`], plus the segment's body twist **per unit `s`** —
    /// `docs/PHASE4.md` §2.3.
    ///
    /// `ξ = log_se3(a⁻¹b)` is *constant across the segment*, which is what makes
    /// it exact rather than a finite difference; for stamps `t_i, t_j` in
    /// nanoseconds, `V^b = ξ · 1e9/(t_j − t_i)`. The pose is bit-identical to
    /// [`Interp::eval`], endpoint shortcuts included
    /// (`eval_with_twist_pose_matches_eval`). Deliberately **no equivalent on
    /// [`LerpSlerp`]**: its body twist rotates through the segment as an
    /// artifact of the interpolant rather than of the motion (§2.4), so
    /// `tf_tree_core` refuses the query instead of returning it.
    #[inline]
    #[must_use]
    pub fn eval_with_twist(a: &Iso3, b: &Iso3, s: f64) -> (Iso3, Twist) {
        let rel = a.inv_mul(b);
        // The endpoint test comes *before* the power: `ScrewParts::pow` carries
        // the large-arc transcendental, is discarded at `s ∈ {0, 1}`, and LLVM
        // does not sink it out of the untaken branch — a real cost on the two
        // most-queried stamps (an exact sample hit, and `t == t_new`). The twist
        // belongs to the segment, not to `s`, so only the power is skipped.
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
    ///
    /// Direct — `q_self*·q_rhs`, `q_self*·(t_rhs − t_self)` — rather than
    /// materializing `self.inverse()` and composing: saves a vector rotation
    /// and a negation pass on the ScLerp hot path.
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
/// [`LerpSlerp`]'s rotation kernel, `pub` for the reason [`screw_pow`]
/// ([`ScLerp`]'s) always was: a caller holding two rotations should not have to
/// build a pair of [`Iso3`] with throwaway zero translations. What that saves
/// is a shape, not a number — 256 bytes of stack, two isometries written field
/// by field, a zero-translation lerp LLVM will not fold — worth 45 instructions
/// bare and 28 through a `nalgebra` adapter, **but those counts are a codegen
/// artifact**: over four release profiles they became 48 and 31, no build
/// agreed for both argument shapes, and an earlier `15` against `51` reproduced
/// nowhere; only the sign is stable. `docs/API.md` §2.7 authorises the `pub`
/// (§7 walk item 8); the `tf_tree` facade re-exports it, pinned by
/// `tests/math_reexports.rs`.
///
/// # Angles
///
/// **Every angle here is a *quaternion* angle** — `acos(qa·qb)`, **half** the
/// rotation spanned. Read as rotations they are out by two, in the direction
/// that makes the fast path look wider than it is — which is what a caller
/// sizing a publish rate against them needs to know.
///
/// # Preconditions
///
/// Both inputs must be unit and nothing checks: the test would be paid by the
/// whole hot path to catch a caller who already broke [`Quat`]'s only
/// invariant, and this crate has no error type to report through (compare
/// [`Quat::normalize`], trading the other way for the one input that yields
/// infinities). **Nor a `debug_assert!`**, unlike `iso3.rs`'s private `vinv_c3`
/// whose one in-crate caller is in range by construction: this is public and
/// fed poses another *process* wrote into a shared arena; no push path
/// normalizes a stored pose, so it would fire on real data, where a few-ulp
/// drift is a slightly wrong answer and not a wrong *kind*; and it would miss
/// the hazard that matters, a `# Storage order` transposition being **unit**
/// and wrong.
///
/// `s` is a dimensionless fraction of the segment, **not** a stamp; nothing in
/// this crate knows what time is. **It belongs to `[0, 1]`, and nothing clamps
/// or refuses**: out of range it extrapolates, and how well is a property of
/// the pair — which alone picks the branch — not of `s`. Worst case against
/// `qa·exp(s·Δ·axis)` over 40 rotations: **closed form** (above `0.15` rad)
/// `7.2e-15` at `|s| = 20`, holding off the segment as on it; **LERP fallback**
/// (below `1e-6` rad) `1.2e-14` at `|s| = 100` and `3.3e-4` only at `|s| = 1e6`,
/// crudest but most forgiving because its arc is tiny; **series** between them
/// by **two mechanisms, and which one loses the bound depends on the angle, not
/// on `s`** — truncation far out, its weights assuming `|a| ≤ 1` (`6.0e-6` at
/// `|s| = 20` for `0.1` rad, `1.6e3` by `|s| = 100`), but cancellation where
/// `1e-15` is *first* lost, `wa` and `wb` growing like `∓s` with their sum near
/// 1 for a floor of `(|wa| + |wb|)·ε`: `|s| ≈ 2.3`–`2.9` at `0.1499` rad
/// (weights 2.4e-15 off against a 1.1e-15 floor by `|s| = 3`) and
/// `|s| ≈ 3.2`–`5.0` at `0.02` rad (largest carried term 5.9e-18, observed
/// 1.2e-15 tracking a 2.0e-15 floor), so a seventh term moves the first end
/// only.
/// **Extrapolation is therefore not supported here** — `tf_tree_core` answers a
/// stamp outside an edge's window with `ExtrapPolicy` (`Error`, `Hold`,
/// `ConstantTwist`) and never passes an `s` outside `(0, 1)`;
/// `out_of_range_s_extrapolates_and_only_the_closed_form_holds` keeps those
/// numbers honest.
///
/// **`NaN` is not rejected and does not always survive.** A `NaN` `s`
/// propagates, except that numerically identical inputs return `qa` before `s`
/// is read. A `NaN` *component* makes `h` `NaN`, and `NaN <= x` is false, so it
/// clears that return and both branch tests and reaches the closed form alone,
/// where it is destroyed and recreated: `NaN.min(1.0)` is `1.0` (`f64::min`
/// returns the non-`NaN` operand), `angle` is `0.0`, the weights `0.0/0.0`.
/// **So do not add a `sin_angle == 0.0` guard returning `qa`** — it would turn
/// a `NaN` into a plausible pose, failing
/// `nan_propagates_except_through_the_identical_input_return`.
/// `dot.clamp(-1.0, 1.0)` would be safe (`clamp` keeps a `NaN` receiver) but is
/// not made: no input can observe the difference.
///
/// # Storage order
///
/// [`Quat`] is `[w, x, y, z]` — scalar **first**. Eigen and `nalgebra` store it
/// last, and a transposed conversion compiles, type-checks, and returns a
/// perfectly unit quaternion that is the wrong rotation (convention 2 in the
/// crate docs). Such a boundary needs a tested adapter, not a careful reading.
///
/// # Endpoints and degenerate inputs
///
/// The weights at `s = 0` and `s = 1` are exactly `(1, 0)` and `(0, 1)`, so
/// both endpoints come back bit-for-bit. Four measured qualifications:
///
/// * **`s = 1` returns `-qb` whenever `qa·qb < 0`** — the sign fix at the
///   endpoint, and the same rotation. Deliberately no `s == 1.0` shortcut:
///   returning `qb` would jump the *components* exactly where the limit from
///   below goes to `-qb`. Compare rotations, or fix the sign first.
/// * **Below `SLERP_LERP_FALLBACK`, an ulp rather than bit-for-bit** — with no
///   usable direction the result is a renormalized LERP, so `slerp(qa, qb, 0.0)`
///   is `qa/‖qa‖`, ~2.7e-16 off
///   (`endpoints_lose_bit_exactness_only_in_the_lerp_fallback`).
/// * **A `-0.0` component is what "bit-for-bit" misses, on every branch**:
///   `qa·1.0 + qb·0.0` turns `-0.0 + (+0.0)` into `+0.0`, and `normalize` loses
///   it too. **Stated rather than fixed** — the fix is an `s == 0.0` shortcut
///   whose twin the first bullet rules out, and one end alone trades a sign of
///   zero for the larger surprise of an endpoint asymmetry; only a hand-built
///   [`Quat`] gets here (`signed_zero_components_are_the_endpoint_exception`).
/// * **Numerically identical inputs return `qa` for every `s`** — two `/tf`
///   samples from a stationary body. Deliberate: `h` is `0`, so there is no
///   direction and every later branch would divide by it.
///
/// The output is otherwise **not** renormalized — unit to within `f64`, not
/// exactly. The first two bullets are also the *entire* difference between this
/// and `LerpSlerp::eval` on zero-translation [`Iso3`], whose shortcut answers
/// both endpoints without reaching here; as rotations the two agree to 2.7e-16
/// (`the_iso3_round_trip_it_replaces_agrees_as_a_rotation`). So is the third,
/// excluded from that sweep because no rotation it builds has a zero
/// component.
///
/// # Numerics
///
/// The crossover to the closed `acos`/`sin` form is at a quaternion angle of
/// `THETA_SLERP_SMALL` (`0.15` rad — a rotation of `0.30` rad, a 10.47 Hz edge
/// for a 180 °/s body); below it the weights are a six-term series with no
/// transcendental or division, and θ² comes from the *chord*, not `acos(dot)`.
/// Both constants are calibrated in their own doc comments and deliberately
/// private: a `pub const` is a promise not to re-measure.
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

    // θ² from the *chord*, not from `acos(dot)`: `1 - dot` is catastrophic
    // cancellation exactly where this code spends its life (adjacent samples,
    // dot → 1), and `acos` loses half its digits there too. `|qb - qa|² =
    // 2 - 2·dot` for unit quaternions, and component differences cancel
    // nothing, so `h` is accurate however close the two are.
    let h = 0.5 * qa.sub(qb).norm_squared(); // = 1 - |dot|, cancellation-free
    if h <= 0.0 {
        return qa; // identical (or numerically identical) inputs
    }

    let theta_sq = theta_sq_from_chord(h);

    if theta_sq <= THETA_SLERP_SMALL * THETA_SLERP_SMALL {
        if theta_sq < SLERP_LERP_FALLBACK * SLERP_LERP_FALLBACK {
            // Near-parallel: LERP and renormalize. Kept because the weights
            // below are exact here but the inputs carry no usable direction.
            return lerp_norm(qa, qb, s);
        }
        // Transcendental-free: two Horner evaluations, no acos, no sin, no div.
        let wa = slerp_weight(1.0 - s, theta_sq);
        let wb = slerp_weight(s, theta_sq);
        return qa.scale(wa).add(qb.scale(wb));
    }

    // Large arc: the exact form. Reached only by low-rate edges on a fast-moving
    // body, and by `at_adaptive`'s wide bisection spans.
    let angle = libm::acos(if dot < 0.0 { -dot } else { dot }.min(1.0));
    let sin_angle = libm::sin(angle);
    let wa = libm::sin((1.0 - s) * angle) / sin_angle;
    let wb = libm::sin(s * angle) / sin_angle;
    qa.scale(wa).add(qb.scale(wb))
}

/// `θ²` from `h = 1 − |cos θ|`, without `acos`.
///
/// `θ = 2·asin(d)` for half-chord `d² = h/2`, so `θ² = 2h·Σ Cₖ hᵏ`, the `Cₖ`
/// from squaring the `asin` series. `asin` near zero is well conditioned, which
/// is the point: `h` comes from component differences, so nothing forms
/// `1 − dot` or feeds `acos` an argument near 1.
///
/// **Eight terms, and the count is load-bearing**: at θ = 0.15 four terms give
/// 8e-11 relative error, six 1.6e-15, eight exact to `f64`; four would silently
/// cap the fast path at ~1e-10. Derive each from `Cₙ = 2ⁿ⁺¹/((n+1)²·C(2n+2,
/// n+1))`, never by hand, which got two wrong. `C₇` shipped as `128/315315`
/// (30.6% larger than `2/6435`): ~1e-17 relative inside θ ≤ 0.15, invisible to
/// every test, but 3.2e-14 against 1.2e-15 at θ = 0.3, and the "exact to `f64`"
/// claim is what a threshold increase would rest on. A first-draft `C₂ = 3/40`
/// cost 3.8e-6 at θ = 0.15, missed by the synthetic fixture differential (arcs
/// too small) and caught by the *recorded* stream at 4.8e-8 against 6.7e-15 —
/// hence `theta_sq_matches_acos_across_the_fast_path`, testing the conversion
/// alone rather than only through `slerp`.
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
/// With `x = a²`, the coefficients below are the exact rational long division
/// of the two Maclaurin series. Two things easy to get wrong, and wrong first:
///
/// * The `u³` coefficient is `31−18x+3x²`; a hand-expanded `31−42x+11x²` was
///   caught at once by `slerp_series_matches_exact_below_threshold`.
/// * **Six terms, not four.** The coefficients fall only ~10× per order while
///   `u = θ²` shrinks by ~0.02 at the threshold. Max θ holding 1e-15 relative
///   error: 4 terms 0.037 rad, 5 terms 0.091, **6 terms 0.165**, 7 terms 0.248.
///   Four would push every edge below ~1 kHz — most of them — to the slow path.
///
/// `(1 − x)` factors out of every term above `u⁰`, so both endpoints stay exact
/// at any term count: at `a = 1` every correction vanishes, at `a = 0` the
/// leading `a` zeroes it. Horner in `u`, each coefficient Horner in `x`: no
/// transcendental, no divide, no data-dependent branch — a vectorizable shape.
#[inline]
#[must_use]
pub(crate) fn slerp_weight(a: f64, u: f64) -> f64 {
    let x = a * a;
    let k = 1.0 - x; // (1 − a²) factors out of every term
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

    /// `sin(aθ)/sin(θ)` the obvious way — the definition the series approximates.
    fn weight_exact(a: f64, theta: f64) -> f64 {
        libm::sin(a * theta) / libm::sin(theta)
    }

    /// The `h -> theta^2` conversion, tested **separately** from `slerp_weight`:
    /// its absence let a wrong `C2` (3/40 for 2/45) reach the recorded-stream
    /// differential, since `slerp_series_matches_exact_below_threshold` feeds
    /// `slerp_weight` a `u` computed by the *test*, and the fixture's arcs are
    /// too small.
    #[test]
    fn theta_sq_matches_acos_across_the_fast_path() {
        let mut worst = 0.0f64;
        let mut worst_theta = 0.0;
        for i in 0..=2000 {
            let theta = THETA_SLERP_SMALL * (i as f64) / 2000.0;
            if theta < 1e-9 {
                continue;
            }
            // Build `h` from the half-chord as production does — `1 - cos(theta)`
            // loses all precision for small theta and would be testing the test.
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

    /// The sweep `docs/PHASE1.md` §3.3 demands of any series/closed-form switch:
    /// show *where* the two agree and pick the constant from the data. Truncation
    /// error grows as θ⁸, so `THETA_SLERP_SMALL` must sit well inside 1e-15.
    #[test]
    fn slerp_series_matches_exact_below_threshold() {
        let mut worst = 0.0f64;
        let mut worst_at = (0.0, 0.0);
        // Log grid over the range the fast path claims, times a spread of `s`.
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
                // Relative error, guarding the a = 0 root where both are 0.
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

    /// The series *does* lose 1e-15 beyond its measured range — why the threshold
    /// is 0.15 and not "as large as we can get away with". If this stops failing,
    /// the series gained a term: re-derive the threshold from a fresh sweep.
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
            // Interpolated, not spelled: this literal read `theta=0.9`, twice
            // the probed value, so the message named a θ never probed.
            "the series is accurate at theta={theta}; the threshold could be raised \
             (worst rel err {worst:e}) — re-derive it from a sweep"
        );
    }

    /// Rotating both inputs by a growing angle walks `slerp` across the
    /// threshold; a visible step there would mean the branches disagree, the
    /// failure mode a threshold switch is prone to.
    #[test]
    fn no_discontinuity_across_the_threshold() {
        // Unit axis, written out so the test needs no Vec3 helper it does not have.
        let n = (0.3f64 * 0.3 + 0.5 * 0.5 + 0.81 * 0.81).sqrt();
        let axis = Vec3::new(0.3 / n, -0.5 / n, 0.81 / n);
        let s = 0.37;
        let mut worst = 0.0f64;
        // Straddle the threshold, roughly half the samples on each branch.
        for i in 0..4000 {
            let theta = THETA_SLERP_SMALL * 0.5 + (i as f64) * (THETA_SLERP_SMALL / 4000.0);
            let qa = Quat::IDENTITY;
            // `exp_so3` takes a rotation vector; the quaternion angle is half it.
            let qb = exp_so3(axis.scale(2.0 * theta));

            // Both branches must track one reference. Comparing adjacent
            // *samples* measures the function's own slope, which swamps any
            // branch mismatch — the previous version did that behind an
            // unreachable `if` and asserted nothing at all.
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
        // A tolerance nothing reached would be as vacuous as the old guard.
        assert!(worst > 0.0, "no sample was actually compared");
    }

    /// Endpoints stay exact (proptest #6 in `docs/PHASE1.md` §10.1) on both
    /// branches — the fast path must not perturb `s = 0` or `s = 1`.
    #[test]
    fn endpoints_are_exact_on_both_branches() {
        let axis = Vec3::new(1.0, 0.0, 0.0);
        // Kept below pi/2 in *quaternion* angle: past that the shortest-arc sign
        // fix negates qb, so `slerp(.., 1.0)` is `-qb` — the same rotation, other
        // components, and a raw comparison would test the wrong thing.
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
            // At s = 1 the weights are (0, 1) exactly on both branches.
            for (g, e) in [(one.w, qb.w), (one.x, qb.x), (one.y, qb.y), (one.z, qb.z)] {
                assert!((g - e).abs() < 1e-15, "s=1 @ {theta}: {g} vs {e}");
            }
        }
    }

    /// **`eval_with_twist`'s pose must be bit-identical to `eval`'s**, or two
    /// lookups at one stamp through different entry points disagree.
    ///
    /// Mutant: drop the `s == 1.0` shortcut from `eval_with_twist` ⇒ fails at
    /// `s = 1`, where `a · rel^1` is `b` only to rounding.
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
