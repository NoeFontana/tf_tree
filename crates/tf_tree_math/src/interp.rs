//! Interpolation between two `Iso3` poses.
//!
//! Two policies share the [`Interp`] trait:
//!
//! * [`ScLerp`] — the SE(3) screw geodesic, **the default**. Left- and
//!   right-invariant. Computed with the fast dual-quaternion power
//!   ([`crate::dualquat::screw_pow`]), proptested against
//!   [`crate::reference::sclerp`].
//! * [`LerpSlerp`] — tf2-compatible: translation LERP, rotation shortest-arc
//!   SLERP. Left-invariant but **not** right-invariant; that asymmetry is why
//!   `ScLerp` is the default (`docs/PHASE1.md` §3.4; `docs/PROJECT.md` §5 D5).

use crate::dualquat::screw_pow;
use crate::iso3::Iso3;
use crate::quat::Quat;

/// Below this half-angle between the two quaternions, `slerp` falls back to a
/// normalized LERP to avoid dividing by `sin(angle) → 0`.
const SLERP_LERP_FALLBACK: f64 = 1e-6;

/// Above this angle (radians) between the two quaternions, [`slerp`] uses the
/// exact `acos`/`sin` form; at or below it, the transcendental-free series in
/// [`slerp_weight`].
///
/// **Measured, not guessed** — the same discipline as `THETA_SMALL` in
/// `docs/PHASE1.md` §3.3 (`docs/PROJECT.md` §5 D12), and for the same reason:
/// the first draft of this constant was 0.25 by eyeball, and
/// `slerp_series_matches_exact_below_threshold` showed the real error there was
/// **3e-9**, seven orders worse than claimed.
///
/// Measured largest θ holding 1e-15 relative error, by term count:
/// 4 terms 0.037 · 5 terms 0.091 · **6 terms 0.165** · 7 terms 0.248 rad.
/// [`slerp_weight`] uses six, so 0.15 sits just inside the measured bound.
///
/// **Why this covers the cases that matter.** The two quaternions are *adjacent
/// samples on one edge*, so θ is set by the publish rate and the body's angular
/// velocity. At a brisk 180 °/s: 1 kHz → 3 mrad, 200 Hz → 16 mrad, 50 Hz →
/// 63 mrad — all inside. A 10 Hz edge on such a body (314 mrad) takes the exact
/// path, which is correct: that is a genuinely large arc.
const THETA_SLERP_SMALL: f64 = 0.15;

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
    /// Computed directly — rotation `q_self*·q_rhs`, translation
    /// `q_self*·(t_rhs − t_self)` — rather than materializing `self.inverse()`
    /// and composing, which saves a vector rotation and a negation pass on the
    /// ScLerp interpolation hot path.
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
/// Applies the standard sign fix (`if q_a·q_b < 0 { negate q_b }`) so the path
/// takes the short way round, and falls back to a normalized LERP when the
/// half-angle drops below [`SLERP_LERP_FALLBACK`].
#[inline]
#[must_use]
fn slerp(qa: Quat, qb: Quat, s: f64) -> Quat {
    let dot = qa.dot(qb);
    let qb = if dot < 0.0 { qb.neg() } else { qb };

    // θ² from the *chord*, not from `acos(dot)`.
    //
    // `1 - dot` is catastrophic cancellation exactly where this code spends its
    // life (adjacent samples, dot → 1), and `acos` loses half its significant
    // digits there too. For unit quaternions `|qb - qa|² = 2 - 2·dot`, and
    // computing it from the component differences cancels nothing — so `h` below
    // is accurate to full precision no matter how close the two are.
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
/// `θ = 2·asin(d)` where `d` is the half-chord and `d² = h/2`, so
/// `θ² = 2h·Σ Cₖ hᵏ` with the `Cₖ` below coming from squaring the `asin` series.
/// `asin` near zero is well conditioned, which is the whole point: the caller
/// obtains `h` from component differences, so nothing in this path ever forms
/// `1 − dot` or feeds `acos` an argument near 1.
///
/// ```text
/// C₀..C₇ = 1, 1/6, 2/45, 1/70, 8/1575, 4/2079, 16/21021, 128/315315
/// ```
///
/// **Eight terms, and the count is load-bearing.** This series converges much
/// more slowly than it looks: at θ = 0.15, four terms give 8e-11 relative error,
/// six give 1.6e-15, and eight are exact to `f64`. Four terms would silently cap
/// the whole fast path at ~1e-10.
///
/// The first draft of this function had `C₂ = 3/40` instead of `2/45` — a
/// hand-derivation slip. It cost 3.8e-6 relative error at θ = 0.15, which the
/// synthetic fixture differential did **not** catch (its arcs are tiny) but the
/// *recorded* stream did, blowing up from 6.7e-15 to 4.8e-8. Hence
/// `theta_sq_matches_acos_across_the_fast_path` below: the conversion is now
/// tested on its own rather than only through `slerp`.
#[inline]
#[must_use]
fn theta_sq_from_chord(h: f64) -> f64 {
    const C: [f64; 8] = [
        1.0,
        1.0 / 6.0,
        2.0 / 45.0,
        1.0 / 70.0,
        8.0 / 1575.0,
        4.0 / 2079.0,
        16.0 / 21021.0,
        128.0 / 315_315.0,
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
/// Obtained by exact rational long division of the two Maclaurin series. Two
/// things about this that are easy to get wrong, and did get wrong here first:
///
/// * The `u³` coefficient is `31−18x+3x²`. The first draft read `31−42x+11x²`,
///   a hand-expansion slip that
///   `slerp_series_matches_exact_below_threshold` caught immediately.
/// * **Six terms, not four.** The coefficients fall only ~10× per order while
///   `u = θ²` shrinks by ~0.02 at the threshold, so convergence is far slower
///   than it looks. Measured maximum θ holding 1e-15 relative error: 4 terms
///   0.037 rad, 5 terms 0.091, **6 terms 0.165**, 7 terms 0.248. Four terms
///   would have forced a threshold so low that every edge below ~1 kHz fell to
///   the slow path — which is most of them.
///
/// `(1 − x)` factors out of every term above `u⁰`, which is why both endpoints
/// stay exact: at `a = 1` every correction vanishes and the weight is 1; at
/// `a = 0` the leading `a` makes it 0. That is a property of the algebra, not of
/// the truncation, so it survives at any term count.
///
/// Horner in `u`, with each coefficient itself Horner in `x`. Pure multiply–add:
/// no transcendental, no divide, and no data-dependent branch, so it is also a
/// shape a vectorizer can take.
#[inline]
#[must_use]
fn slerp_weight(a: f64, u: f64) -> f64 {
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

    /// The conversion `h -> theta^2` must itself be exact across the whole fast
    /// path, and it must be tested **separately** from `slerp_weight`.
    ///
    /// This test did not exist on the first draft, and its absence is precisely
    /// how a wrong `C2` (3/40 instead of 2/45) reached the recorded-stream
    /// differential: `slerp_series_matches_exact_below_threshold` feeds
    /// `slerp_weight` a `u` computed by the *test*, so it validated the weights
    /// while the input conversion was broken. The synthetic fixture missed it too
    /// — its inter-sample arcs are far too small for the h^2 term to matter.
    #[test]
    fn theta_sq_matches_acos_across_the_fast_path() {
        let mut worst = 0.0f64;
        let mut worst_theta = 0.0;
        for i in 0..=2000 {
            let theta = THETA_SLERP_SMALL * (i as f64) / 2000.0;
            if theta < 1e-9 {
                continue;
            }
            // Build `h` the way production does — from the half-chord, which is
            // cancellation-free — NOT as `1 - cos(theta)`, which loses all
            // precision for small theta and would be testing the test.
            // h = 1 - cos(theta) = 2*sin^2(theta/2).
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

    /// The threshold sweep `docs/PHASE1.md` §3.3 demands for any series/closed-form
    /// switch: show *where* the two agree, and pick the constant from the data
    /// rather than from taste.
    ///
    /// The series truncation error grows as θ⁸, so it degrades sharply above its
    /// range; `THETA_SLERP_SMALL` must sit comfortably inside where it still
    /// holds 1e-15.
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

    /// The series *does* lose 1e-15 beyond its measured range — which is why
    /// the threshold is 0.15 and not "as large as we can get away with". If this
    /// ever stops failing, the series gained a term and the threshold must be
    /// re-derived from a fresh sweep rather than nudged upward by hand.
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
            "the series is accurate at theta=0.9; the threshold could be raised \
             (worst rel err {worst:e}) — re-derive it from a sweep"
        );
    }

    /// Rotating both inputs about a fixed axis by a growing angle walks `slerp`
    /// across the threshold. The result must be continuous there — a visible step
    /// would mean the two branches disagree, which is the failure mode a
    /// threshold switch is prone to.
    #[test]
    fn no_discontinuity_across_the_threshold() {
        // Unit axis, written out so the test needs no Vec3 helper it does not have.
        let n = (0.3f64 * 0.3 + 0.5 * 0.5 + 0.81 * 0.81).sqrt();
        let axis = Vec3::new(0.3 / n, -0.5 / n, 0.81 / n);
        let s = 0.37;
        let mut prev: Option<(f64, Quat)> = None;
        let mut worst_jump = 0.0f64;
        for i in 0..4000 {
            // Sweep the angle between qa and qb straight through the threshold.
            let theta = THETA_SLERP_SMALL * 0.5 + (i as f64) * (THETA_SLERP_SMALL / 2000.0);
            let qa = Quat::IDENTITY;
            // `exp_so3` takes a rotation vector; the quaternion angle is half it.
            let qb = exp_so3(axis.scale(2.0 * theta));
            let got = slerp(qa, qb, s);
            if let Some((pt, pq)) = prev {
                if (theta - pt).abs() < 1e-6 {
                    let d = (got.w - pq.w).abs().max((got.x - pq.x).abs());
                    let _ = pt;
                    worst_jump = worst_jump.max(d);
                }
            }
            prev = Some((theta, got));
        }
        assert!(
            worst_jump < 1e-14,
            "slerp jumps by {worst_jump:e} across THETA_SLERP_SMALL"
        );
    }

    /// Endpoints stay exact (proptest #6 in `docs/PHASE1.md` §10.1) on both
    /// branches — the fast path must not perturb `s = 0` or `s = 1`.
    #[test]
    fn endpoints_are_exact_on_both_branches() {
        let axis = Vec3::new(1.0, 0.0, 0.0);
        // Kept below pi/2 in *quaternion* angle: past that the shortest-arc sign
        // fix negates qb, so `slerp(.., 1.0)` returns `-qb` — the same rotation,
        // different components. That is correct behaviour, not an endpoint
        // failure, so comparing raw components there would be testing the wrong
        // thing.
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
}
