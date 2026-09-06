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
/// **The angle is `acos(qa·qb)`, which is *half* the rotation angle** the pair
/// spans, because `qa·qb = cos(Δ/2)`. `0.15` here is a rotation of `0.30` rad,
/// and every figure below says which of the two it is — measured by bisecting
/// the branch boundary, which lands on quaternion `0.150000000` rad / rotation
/// `0.300000000` rad.
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
/// samples on one edge*, so the arc between them is set by the publish rate and
/// the body's angular velocity — and θ is half of that arc. At a brisk 180 °/s,
/// where the arc is Δ = π/f:
///
/// ```text
/// f         Δ (rotation)   θ = Δ/2      branch
/// 1 kHz        3.1 mrad     1.6 mrad    series
/// 200 Hz      15.7 mrad     7.9 mrad    series
/// 50 Hz       62.8 mrad    31.4 mrad    series
/// 10 Hz      314.2 mrad   157.1 mrad    exact
/// ```
///
/// **The Δ column is what this table used to give, alone, against a threshold
/// stated in θ** — a factor of two, in the direction that makes the fast path
/// look roomier than it is. No row changes side, so the paragraph's conclusion
/// survives, but the margin does not: the 10 Hz row clears the threshold by
/// **4.7%**, not by 2×, and the crossover for a 180 °/s body is at
/// `f = ω/(2·0.15)` = **10.47 Hz**. A 10 Hz edge on such a body takes the exact
/// path, which is correct — and it is a marginal case, not a comfortable one.
/// Shared with [`crate::dualquat::screw_pow`], which raises a unit dual
/// quaternion to a real power and needs the identical `sin(a·φ)/sin(φ)` series
/// over the identical range — in both cases `φ` is the *half* angle between two
/// adjacent samples on one edge.
pub(crate) const THETA_SLERP_SMALL: f64 = 0.15;

// **Both constants are quoted as literals outside this file**, and neither is
// `pub` — a re-measurement is allowed to move them, and that is exactly why
// these two lines are here: the prose is what a re-measurement would leave
// behind. What the assertions can do is refuse to compile until whoever moved
// the constant comes here; what they cannot do is find the prose. So the sites
// are listed, because "grep for `0.15`" over this repository is not a short
// list:
//
// * `slerp`'s own rustdoc, below — `# Angles`, `# Preconditions` and
//   `# Numerics`, which is the crates.io page an external caller reads and the
//   only place the fallback band and the crossover are stated in a form they
//   can act on.
// * `THETA_SLERP_SMALL`'s own doc comment above (the per-term-count table and
//   the 10.47 Hz crossover), and `slerp_weight`'s.
// * `crates/tf_tree_math/README.md`, *Numerics, and where the constants came
//   from* — the crates.io front page.
// * `docs/API.md` §6 row 16.
// * `crates/tf_tree_math/tests/slerp_public.rs`, which mirrors the crossover in
//   a single `THETA_CROSSOVER` const and derives its sweep from it, so that
//   file has one site rather than nine.
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
    /// The twist is `ξ = log_se3(a⁻¹b)` and is *constant across the segment*,
    /// which is the property that makes it exact rather than a finite
    /// difference. Divide by the segment's duration to get velocity: for stamps
    /// `t_i, t_j` in nanoseconds, `V^b = ξ · 1e9/(t_j − t_i)`.
    ///
    /// The pose is bit-identical to [`Interp::eval`], endpoint shortcuts
    /// included — pinned by `eval_with_twist_pose_matches_eval`. There is
    /// deliberately **no equivalent on [`LerpSlerp`]**: it has a body twist, but
    /// one that rotates through the segment as an artifact of the interpolant
    /// rather than of the motion (§2.4), so `tf_tree_core` refuses the query
    /// instead of returning it.
    #[inline]
    #[must_use]
    pub fn eval_with_twist(a: &Iso3, b: &Iso3, s: f64) -> (Iso3, Twist) {
        let rel = a.inv_mul(b);
        // The endpoints are exact by construction, exactly as in `eval` — and the
        // test is made *before* the power, not after. `ScrewParts::pow` is the
        // half of the decomposition that carries the transcendental on the
        // large-arc branch, and at `s ∈ {0, 1}` its result is discarded. LLVM
        // does not sink the call out of the untaken branch, so computing it first
        // and then throwing it away is a real cost on the two stamps most likely
        // to be queried: an exact hit on a published sample, and `t == t_new`.
        //
        // The twist is still needed at the endpoints — it is a property of the
        // segment, not of `s` — so only the power is skipped, never the screw
        // decomposition itself.
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
/// This is the rotation kernel [`LerpSlerp`] evaluates, and it is public for
/// the same reason [`screw_pow`] — [`ScLerp`]'s kernel — always was: a caller
/// who holds two rotations and no translation should not have to build a pair
/// of [`Iso3`] with throwaway zero translations to reach it. A caller whose
/// quaternion type is not [`Quat`] still converts, because the types differ;
/// what goes away is the pair of isometries.
///
/// **What the round trip costs is a shape, not a number.** Both arms end in the
/// same out-of-line `slerp`; what the `Iso3` arm puts in front of it is 256
/// bytes of stack, two 64-byte isometries written out field by field, and a
/// lerp of one zero translation into another that LLVM does not fold away.
/// Compiled as exported `extern "C"` arms at `opt-level = 3` on x86-64, that
/// prologue is 45 instructions bare (7 against 52) and 28 through the
/// consumer's own `nalgebra` adapter (41 against 69).
///
/// **Those two counts are a codegen artifact and are quoted as one.** Across
/// four release profiles they moved to 48 and 31, and no build produced the
/// same figure for both argument shapes — so there is no single number here to
/// carry into a gate, and an earlier revision of this paragraph quoting one
/// (`15` against `51`, then "36 either way") reproduced in no build measured.
/// What is stable is the sign: the wrapper survived every configuration tried,
/// and that is the whole of the benefit.
///
/// `docs/API.md` §2.7 is what authorises this being `pub` and carries the §7
/// walk, item 8 — what a caller *loses* by taking it — included.
///
/// **The `tf_tree` facade re-exports this**, so a consumer of the engine
/// reaches it as `tf_tree::slerp` and does not take a second direct dependency
/// on this crate to do it. `tf_tree`'s `tests/math_reexports.rs` is what says
/// the two names are one item.
///
/// # Angles
///
/// **Every angle here is a *quaternion* angle** — `acos(qa·qb)`, which is
/// **half** the rotation the pair spans, because `qa·qb = cos(Δ/2)`. Read as
/// rotations these numbers are out by a factor of two, in the direction that
/// makes the fast path look wider than it is; a caller sizing a publish rate
/// against them is who this section is for.
///
/// # Preconditions
///
/// Both inputs must be unit, and nothing here checks. A norm test per call
/// would be paid by every caller on the interpolation hot path to catch one
/// who has already broken the only invariant [`Quat`] has, and this crate has
/// no error type to report it through — see [`Quat::normalize`], which makes
/// the same trade in the other direction for the one input (a zero
/// quaternion) where the arithmetic would produce infinities rather than
/// merely a wrong answer.
///
/// **Nor a `debug_assert!`, which is the version of that question worth
/// answering**, since it would cost a release caller nothing and
/// `iso3.rs`'s `vinv_c3` domain assertion is right there as a precedent.
/// Three reasons it is not the same case:
///
/// * That precedent is a **private** function with exactly one caller in this
///   crate, whose argument [`crate::log_se3`] derives from [`crate::log_so3`]
///   and is therefore in range by construction — the assertion pins a contract
///   the crate owns both ends of. This is a public entry point whose inputs come
///   from outside, and in the engine from a pose another *process* wrote into
///   a shared arena.
/// * **Nothing upstream enforces the invariant**, so the assertion would fire
///   on real data rather than on a bug: no push path in `tf_tree_core` or the
///   facade normalizes a stored pose, and a quaternion that has drifted a few
///   ulp off unit gives a slightly wrong answer here, not a wrong *kind* of
///   answer. A `debug_assert` would make that a panic in every debug build —
///   this crate's proptests, `just miri`, and every downstream `cargo test` —
///   and not in release, which is a value that works or aborts depending on
///   `-C debug-assertions`.
/// * It would not catch the hazard this function actually has. The
///   `# Storage order` transposition produces a **unit** quaternion that is the
///   wrong rotation, and no norm test of any tolerance sees it.
///
/// `s` is a dimensionless fraction of the segment, **not** a stamp. A caller
/// interpolating between two samples divides in integer nanoseconds and passes
/// the ratio; nothing in this crate knows what time is.
///
/// **`s` belongs to `[0, 1]`, and nothing clamps or refuses.** Out of range the
/// function extrapolates, and *how well* is a property of the pair rather than
/// of `s`, because the pair alone picks the branch — so an extrapolation's
/// accuracy is set by the publish rate, which is the one thing the caller
/// asking for it is least likely to be thinking about. Measured against the
/// exact geodesic `qa·exp(s·Δ·axis)` over 40 rotations, worst case:
///
/// * **Closed form** (quaternion angle above `0.15` rad) — the formula is
///   `sin((1−s)·θ)/sin θ` and holds off the segment as well as on it:
///   `7.2e-15` at `|s| = 20`, part of which is the reference's own rounding.
/// * **Series** (between the two thresholds) — **two mechanisms, and which one
///   loses the bound depends on the angle, not on `s`.** Far out it is
///   truncation: the weight series is calibrated for `|a| ≤ 1`, and outside it
///   `k = 1 − a²` grows quadratically until six terms stop covering it — `6.0e-6`
///   at `|s| = 20` (quaternion angle `0.1` rad), and `1.6e3` by `|s| = 100`,
///   not a rotation at all. **Where the `1e-15` bound is first lost, though, the
///   cause at the small-angle end is cancellation and not truncation**: `wa` and
///   `wb` grow like `∓s` while their sum stays near 1, so the rounding floor is
///   `(|wa| + |wb|)·ε` and the result cancels back down onto the sphere from
///   there. Measured over 40 rotations, the bound goes at `|s| ≈ 2.3`–`2.9` for
///   a quaternion angle of `0.1499` rad and at `|s| ≈ 3.2`–`5.0` for `0.02` rad
///   — and at `0.02` rad the largest term the six-term series still *carries* is
///   `5.9e-18` at `|s| = 5`, three orders under the bound, so the remainder it
///   drops is smaller again, and the observed error there tracks the
///   cancellation floor instead (`1.2e-15` measured against a floor of
///   `2.0e-15`). At `0.1499` rad the two are the same size right where the
///   bound goes: at `|s| = 2.3` the series' weights still match
///   `sin(aθ)/sin θ` to within their own ulp while the floor is `7.9e-16`, and
///   by `|s| = 3` the weights are `2.4e-15` off against a floor of `1.1e-15`.
///   **So the range's two ends have different causes**, and an edit adding a
///   seventh term would move the `0.1499` end and leave the `0.02` end exactly
///   where it is.
/// * **LERP fallback** (below `1e-6` rad) — a chord, extrapolated and
///   renormalized. Mechanically the crudest of the three and numerically the
///   most forgiving, because the arc it cuts is tiny: `1.2e-14` at `|s| = 100`,
///   and it takes `|s| = 1e6` — a swept rotation of `0.2` rad — to reach
///   `3.3e-4`.
///
/// **So extrapolation is not supported here**, and that is a statement about
/// this entry point rather than about the habit: `tf_tree_core` answers a stamp
/// outside an edge's sample window with `ExtrapPolicy` — `Error`, `Hold` or
/// `ConstantTwist` — and never hands this function an `s` outside `(0, 1)`. A
/// caller who wants the tf2 behaviour wants one of those three, not an `s` of
/// `1.4`. `out_of_range_s_extrapolates_and_only_the_closed_form_holds` is what
/// keeps the three bullets above honest.
///
/// **`NaN` is not rejected either**, and it does not always survive. A `NaN`
/// `s` propagates through all three branches, with one exception: two
/// numerically identical inputs return `qa` before `s` is read at all, so
/// `slerp(qa, qa, f64::NAN)` and `slerp(qa, qa, f64::INFINITY)` are both `qa`.
///
/// **A `NaN` *component* takes one branch and one only, and the mechanism is
/// worth writing down because the obvious hardening breaks it.** It makes `h`
/// `NaN`, and `NaN <= x` is false for every `x` — so it clears the
/// identical-input return *and* both branch tests, and the closed form is the
/// only arm a `NaN` component can reach whatever the angle between the inputs.
/// There the `NaN` is **destroyed and then recreated**: `dot` is `NaN`,
/// `NaN.min(1.0)` is `1.0` (Rust's `f64::min` returns the non-`NaN` operand),
/// so `angle` is `acos(1.0) = 0.0`, `sin_angle` is `0.0`, and the output is
/// `NaN` only because both weights are `sin(a·0.0)/0.0`, which is `0.0/0.0`.
/// A guard returning `qa` when `sin_angle == 0.0` — a reasonable-looking
/// defence against that division — would therefore turn a `NaN` input into a
/// plausible pose, and it is `nan_propagates_except_through_the_identical_input_return`
/// that would fail. Replacing `.min(1.0)` with `dot.clamp(-1.0, 1.0)` is the
/// other obvious edit and is *safe* — `clamp` returns `NaN` for a `NaN`
/// receiver, so the `NaN` would then survive on its own terms rather than by
/// coincidence — but it is not made here: the observable behaviour is
/// identical on every input, so it would be a change to the hot path that no
/// test could distinguish, and the reason `.min` looks like a bug is that the
/// paragraph explaining it was missing, not that the line is wrong.
///
/// # Storage order
///
/// [`Quat`] is `[w, x, y, z]` — scalar **first**. Eigen and `nalgebra` store it
/// last, and a transposed conversion compiles, type-checks, and returns a
/// perfectly unit quaternion that is the wrong rotation. Convention 2 in the
/// crate docs is this hazard; a boundary that crosses it needs a tested adapter
/// rather than a careful reading.
///
/// # Endpoints and degenerate inputs
///
/// On the series and closed-form branches the weights at `s = 0` and `s = 1`
/// are exactly `(1, 0)` and `(0, 1)`, so both endpoints come back bit-for-bit.
/// Four qualifications, all measured rather than reasoned:
///
/// * **`s = 1` returns `-qb` whenever `qa·qb < 0`.** That is the sign fix
///   arriving at the endpoint, not an endpoint failure: `-qb` is the same
///   rotation, and returning `qb` there instead would put a jump in the
///   returned *components* at exactly `s = 1` while the limit from below goes
///   to `-qb` — which is worse than the asymmetry it would tidy up, and is why
///   there is no `s == 1.0` shortcut. Compare rotations, or fix the sign first.
/// * **Below the LERP fallback the endpoints hold to an ulp, not bit-for-bit.**
///   Under a quaternion angle of `SLERP_LERP_FALLBACK` (`1e-6` rad, a rotation
///   of `2e-6` rad) the two inputs carry no usable direction, so the result is
///   a *renormalized* LERP:
///   `slerp(qa, qb, 0.0)` is `qa/‖qa‖`, which differs from `qa` by ~2.7e-16
///   whenever `qa`'s components do not happen to square to exactly `1.0`.
///   `endpoints_lose_bit_exactness_only_in_the_lerp_fallback` pins both halves.
/// * **A `-0.0` component is the one thing "bit-for-bit" does not cover, on
///   every branch.** Exact weights mean the answer at `s = 0` is
///   `qa·1.0 + qb·0.0` rather than `qa`, and `-0.0 + (+0.0)` is `+0.0` — so
///   `slerp(Quat::new(1.0, -0.0, 0.0, 0.0), qb, 0.0)` returns `+0.0` in `x`
///   for any `qb` whose `x` is positive, and a `-0.0` in `qb` flips the same
///   way at `s = 1`. The fallback's `normalize` loses it too. **Stated
///   rather than fixed**, because the fix is an `s == 0.0` shortcut and the
///   first bullet rules out its `s == 1.0` twin on purpose: adding one end and
///   not the other trades a sign of zero for an asymmetry between the two
///   endpoints, which is the larger of the two surprises. Nothing in this crate
///   manufactures a `-0.0` component — [`crate::exp_so3`] produces one only
///   from an axis component that already carries the sign, and
///   `sample_rotation`, which every sweep in `tests/slerp_public.rs` is built
///   from, emits no zero component at all over `k` in `0..1000` — so this
///   reaches a caller who built a [`Quat`] by hand.
///   `signed_zero_components_are_the_endpoint_exception` pins it.
/// * **Numerically identical inputs return `qa` for every `s`**, `s` included
///   in neither weight. Two consecutive `/tf` samples from a stationary body
///   are exactly this case, and it is an early return rather than an accident:
///   `h` is `0`, so there is no direction to interpolate along and every later
///   branch would divide by it.
///
/// The output is otherwise **not** renormalized — `qa·wa + qb·wb` is unit to
/// within `f64`, not exactly. A caller whose type enforces unit norm should
/// normalize on the way in to its own type.
///
/// The first two bullets are also the *entire* difference between calling this
/// and going through `LerpSlerp::eval` on a pair of zero-translation [`Iso3`],
/// for any input a rotation can produce — `eval` answers `s = 0` and `s = 1`
/// from a shortcut that never reaches here.
/// `the_iso3_round_trip_it_replaces_agrees_as_a_rotation` sweeps both and
/// classifies every bit difference; as rotations the two agree to 2.7e-16. The
/// third bullet is a difference too, and is excluded from that sweep rather
/// than absent from it: `eval`'s shortcut returns `*a` and keeps a `-0.0`,
/// where this returns `+0.0`, and no rotation the sweep can build has a zero
/// component to notice it with.
///
/// # Numerics
///
/// The crossover to the closed `acos`/`sin` form is at a quaternion angle of
/// `THETA_SLERP_SMALL` (`0.15` rad — a rotation of `0.30` rad, which for a
/// body turning at 180 °/s is a 10.47 Hz edge); below it the weights are a
/// six-term series with no transcendental and no division, and θ² comes from
/// the *chord* rather than from `acos(dot)`. Both constants are calibrated,
/// with the measurement in their own doc comments, and both are deliberately
/// private: they are numbers a re-measurement is allowed to move, and a
/// `pub const` is a promise not to.
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
/// C₀..C₇ = 1, 1/6, 2/45, 1/70, 8/1575, 4/2079, 16/21021, 2/6435
/// ```
///
/// **Eight terms, and the count is load-bearing.** This series converges much
/// more slowly than it looks: at θ = 0.15, four terms give 8e-11 relative error,
/// six give 1.6e-15, and eight are exact to `f64`. Four terms would silently cap
/// the whole fast path at ~1e-10.
///
/// The closed form is `Cₙ = 2ⁿ⁺¹ / ((n+1)²·C(2n+2, n+1))`; deriving each term
/// from it rather than by hand is the only reliable way to get eight right. The
/// shipped `C₇` was `128/315315` until a review caught it — the correct value is
/// `2/6435`, exactly `49/64` of the shipped one (23% smaller). *This read
/// "31% smaller", which is the reciprocal comparison stated in the wrong
/// direction: `128/315315` is 30.6% larger than `2/6435`. A paragraph whose
/// subject is a coefficient error caught by review is a bad place for
/// arithmetic nobody checked.* Inside the θ ≤ 0.15 fast path the term contributes
/// ~1e-17 relative, so no test could see it and no result was ever wrong; at
/// θ = 0.3 it is the difference between 3.2e-14 and 1.2e-15. It mattered because
/// the "exact to `f64`" claim above is what any future threshold increase would
/// rest on.
///
/// The first draft of this function had `C₂ = 3/40` instead of `2/45` — a
/// hand-derivation slip. It cost 3.8e-6 relative error at θ = 0.15, which the
/// synthetic fixture differential did **not** catch (its arcs are tiny) but the
/// *recorded* stream did, blowing up from 6.7e-15 to 4.8e-8. Hence
/// `theta_sq_matches_acos_across_the_fast_path` below: the conversion is now
/// tested on its own rather than only through `slerp`.
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
            // The θ is interpolated, not spelled: the literal here read
            // `theta=0.9` — twice the value the probe above actually uses —
            // from the commit that introduced both lines, so the one thing a
            // maintainer sees when this fires named a θ the test never probes.
            "the series is accurate at theta={theta}; the threshold could be raised \
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
        let mut worst = 0.0f64;
        // Straddle the threshold from half of it to one and a half times it, so
        // roughly half these samples take the series branch and half the exact
        // one.
        for i in 0..4000 {
            let theta = THETA_SLERP_SMALL * 0.5 + (i as f64) * (THETA_SLERP_SMALL / 4000.0);
            let qa = Quat::IDENTITY;
            // `exp_so3` takes a rotation vector; the quaternion angle is half it.
            let qb = exp_so3(axis.scale(2.0 * theta));

            // Compare against the exact closed form directly. Comparing adjacent
            // *samples* to each other cannot work: consecutive angles differ by
            // the step, so the difference is the function's own slope, which
            // swamps any branch mismatch. Requiring both branches to track one
            // reference is the statement with content — and the previous version
            // of this test, which compared neighbours behind an unreachable
            // `if`, asserted nothing at all.
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

    /// **`eval_with_twist`'s pose must be bit-identical to `eval`'s.**
    ///
    /// A caller gets the pose and its derivative from one call and is entitled
    /// to assume they describe the same instant. If the pose drifted by even an
    /// ulp from the `at()` path, two lookups at the same stamp through different
    /// entry points would disagree — the kind of discrepancy that costs a day.
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
