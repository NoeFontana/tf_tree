//! `slerp` through the crate's **public** path, as an external caller reaches it.
//!
//! # Why this file is not in `src/interp.rs`
//!
//! Every existing test for this function lives in `interp.rs`'s `mod tests`,
//! where `use super::*` sees private items — so the entire suite passed for the
//! life of the project while `slerp` was `fn slerp`, unreachable from outside
//! the crate. A downstream crate had to build two `Iso3` with throwaway zero
//! translations and call `LerpSlerp::eval` to get at it. The defect was
//! *visibility*, and visibility is the one property a test inside the module
//! structurally cannot observe. This file is an integration test for that
//! reason and no other: it names `tf_tree_math::` on every call.
//!
//! # Why deterministic sweeps rather than proptest
//!
//! Everything asserted here is a numeric bound or a bit pattern, and both are
//! only worth quoting if the next run produces the same one. `tests/proptests.rs`
//! reaches for the same property from the other side and pins a fixed seed to
//! get it. Each test below therefore also asserts that its sweep **reached** the
//! case it exists to cover — a bound nothing exercised is the failure mode this
//! repository has already shipped once.

use tf_tree_math::{exp_so3, interp, slerp, Quat, Vec3};

/// The series/closed-form crossover, **mirrored** from `interp.rs`'s private
/// `THETA_SLERP_SMALL`, in quaternion angle.
///
/// This is the file's only copy of that number and every sweep below is derived
/// from it, so a re-measurement that moves the constant needs one edit here
/// rather than nine. Nothing checks the mirror: `interp.rs` carries
/// `const _: () = assert!(THETA_SLERP_SMALL == 0.15)`, which refuses to compile
/// when the constant moves and lists this file among the places to follow it to,
/// and that is the whole mechanism — a `const` assertion cannot see another
/// crate's integration test.
const THETA_CROSSOVER: f64 = 0.15;

/// The LERP-fallback threshold, mirrored from `SLERP_LERP_FALLBACK` on the same
/// terms as [`THETA_CROSSOVER`].
const THETA_FALLBACK: f64 = 1e-6;

/// The four components as raw bits, for the exactness claims below. `f64`
/// equality would call `0.0` and `-0.0` the same, and
/// `signed_zero_components_are_the_endpoint_exception` is precisely a sign of
/// zero — the only test here that is, because `sample_rotation` never produces
/// a zero component (measured: none in 4000 components over `k` in `0..1000`).
fn bits(q: Quat) -> [u64; 4] {
    [q.w.to_bits(), q.x.to_bits(), q.y.to_bits(), q.z.to_bits()]
}

/// A deterministic unit quaternion, varied enough across `k` to cover the
/// sphere without a random number generator. Built by `exp_so3`, so it is unit
/// by construction rather than by normalization.
fn sample_rotation(k: usize) -> Quat {
    let f = k as f64;
    exp_so3(Vec3::new(
        0.31 * libm::sin(f * 0.7) + 0.4,
        -0.83 * libm::cos(f * 0.37),
        1.1 * libm::sin(f * 0.19) - 0.2,
    ))
}

/// A unit axis, fixed, for stepping a known angle away from a base rotation.
fn axis() -> Vec3 {
    let a = Vec3::new(1.0, 2.0, -3.0);
    a.scale(1.0 / a.norm())
}

/// `qa` rotated by `theta` *quaternion* angle — half the rotation angle, which
/// is the quantity both of this function's thresholds are stated in.
fn step(qa: Quat, theta: f64) -> Quat {
    qa * exp_so3(axis().scale(2.0 * theta))
}

/// Compiles only if both arguments are the *same* item, not merely two items of
/// the same signature: a function item's type is unique to its definition, so
/// one generic parameter cannot take two of them.
///
/// Verified in both directions while this was written — `same_item` on two
/// distinct `fn(i32) -> i32` fails with *"expected fn item, found a different fn
/// item"* (`E0308`), and a path that does not resolve fails with `E0425`.
fn same_item<T>(_: T, _: T) {}

/// The root re-export and the module path are the same item, not two.
///
/// `PROJECT.md` §6 forbids a second spelling of an existing path. `interp::slerp`
/// and the root `slerp` are one function reached two ways — the same arrangement
/// `Quat`, `Iso3` and `exp_so3` already have — and this is what says so rather
/// than leaving a reader to assume it.
///
/// **This used to compare the bits of two calls, and could not fail.** `pub use`
/// names a function, it does not copy one, so both calls went to the same
/// address and any output agreed with itself. The check that can fail is the
/// one above: a wrapper introduced under either name — the second spelling this
/// test exists to forbid — is a type error even if its signature is identical.
///
/// The **third** path, `tf_tree::slerp`, is checked the same way one crate over
/// in `tf_tree/tests/math_reexports.rs`; it cannot be checked from here, because
/// `tf_tree` depends on this crate and not the other way round.
#[test]
fn the_root_re_export_and_the_module_path_are_one_function() {
    same_item(slerp, interp::slerp);
    same_item(tf_tree_math::slerp, tf_tree_math::interp::slerp);
}

/// Above the LERP fallback, both endpoints come back bit-for-bit — `qb`
/// negated when the sign fix fires, which is the same rotation.
///
/// Mutant, run: `slerp_weight`'s `let k = 1.0 - x` → `let k = 1.0`, dropping the
/// `(1 − a²)` factor that is *why* the series is exact at both ends ⇒ this test
/// fails at `s = 0, theta = 2e-6`. That factor is an algebraic property rather
/// than a truncation artifact, so nothing about term count could substitute.
///
/// **The two counters below say the sweep straddles the *documented* crossover,
/// which is not the same thing as observing the branch `slerp` took** — nothing
/// outside the crate can see that, since the series and the closed form agree
/// far below `f64` where they meet. They used to be spelled against a `0.15`
/// literal of this test's own, which would have gone on reporting coverage of a
/// branch it no longer reached the moment the constant moved. They are now
/// derived from [`THETA_CROSSOVER`], and so is the sweep, so the two cannot
/// disagree.
#[test]
fn endpoints_are_bit_exact_above_the_lerp_fallback() {
    let mut saw_sign_fix = 0usize;
    let mut below_crossover = 0usize;
    let mut above_crossover = 0usize;
    for k in 0..24 {
        let qa = sample_rotation(k);
        // Straddles the series/closed-form crossover — the two `THETA_CROSSOVER`
        // terms are one part in a thousand either side of it — and reaches past
        // pi/2, where the shortest-arc sign fix starts firing.
        for &theta in &[
            2.0 * THETA_FALLBACK,
            1e-4,
            0.02,
            THETA_CROSSOVER * 0.999,
            THETA_CROSSOVER * 1.001,
            0.6,
            1.4,
            1.9,
            2.8,
        ] {
            let qb = step(qa, theta);
            if theta <= THETA_CROSSOVER {
                below_crossover += 1;
            } else {
                above_crossover += 1;
            }

            assert_eq!(
                bits(slerp(qa, qb, 0.0)),
                bits(qa),
                "s=0 k={k} theta={theta}"
            );

            let one = slerp(qa, qb, 1.0);
            if qa.dot(qb) < 0.0 {
                saw_sign_fix += 1;
                assert_eq!(bits(one), bits(qb.neg()), "s=1 k={k} theta={theta}");
            } else {
                assert_eq!(bits(one), bits(qb), "s=1 k={k} theta={theta}");
            }
        }
    }
    assert!(
        below_crossover > 0 && above_crossover > 0,
        "the sweep no longer straddles THETA_CROSSOVER, so only one of the two \
         weight formulas was ever asked for an endpoint"
    );
    assert!(
        saw_sign_fix > 0,
        "no pair had a negative dot, so the documented `s = 1` sign was never checked"
    );
}

/// **The LERP fallback is the one branch where an endpoint is not bit-exact,
/// and this is what proves the branch runs at all.**
///
/// Below a quaternion angle of [`THETA_FALLBACK`] (`1e-6`) the result is a
/// *renormalized* LERP, so
/// `s = 0` returns `qa/‖qa‖`. For a `qa` whose components happen to square to
/// exactly `1.0` that is `qa`; for one that does not — 14 of the 64 rotations
/// swept here, counted, where this line said "about a third" until it was —
/// it differs by a couple of ulp. No value comparison
/// can tell the fallback from the series (they agree far below `f64`), so that
/// ulp *is* the observable, and the test asserts it appears below the threshold
/// and disappears above it with the same `qa`.
#[test]
fn endpoints_lose_bit_exactness_only_in_the_lerp_fallback() {
    let mut discriminating = 0usize;
    let mut worst = 0.0f64;
    for k in 0..64 {
        let qa = sample_rotation(k);
        // The whole discriminator: `normalize` is only a no-op when the norm
        // rounds to exactly 1.0. A `qa` for which it is a no-op cannot tell the
        // two branches apart and is not evidence either way.
        if bits(qa.normalize()) == bits(qa) {
            continue;
        }
        discriminating += 1;

        let inside = slerp(qa, step(qa, THETA_FALLBACK * 0.1), 0.0);
        assert_ne!(
            bits(inside),
            bits(qa),
            "fallback not taken a decade below THETA_FALLBACK, k={k}"
        );
        let d = inside.sub(qa).norm();
        assert!(d < 1e-15, "fallback perturbed s=0 by {d:e}, k={k}");
        worst = worst.max(d);

        let outside = slerp(qa, step(qa, THETA_FALLBACK * 10.0), 0.0);
        assert_eq!(
            bits(outside),
            bits(qa),
            "series branch not exact a decade above THETA_FALLBACK, k={k}"
        );
    }
    assert!(
        discriminating >= 8,
        "only {discriminating} sampled rotations could distinguish the branches; \
         the assertions above proved nothing"
    );
    // Measured 2.7336071744532853e-16 on this sweep, quoted in `slerp`'s doc
    // comment as ~2.7e-16. The assertion is a band rather than that literal
    // because the fact worth defending is "a couple of ulp, and not zero" — a
    // bit-pattern equality here would be a gate that fails on an unrelated
    // libm change and teaches people to widen it.
    assert!(
        (2.0e-16..4.0e-16).contains(&worst),
        "documented fallback endpoint departure moved: {worst:e}"
    );
}

/// **A `-0.0` component is the one input for which "both endpoints come back
/// bit-for-bit" is false, and it is false on every branch.**
///
/// The weights at `s = 0` are exactly `(1, 0)`, so the answer is
/// `qa·1.0 + qb·0.0` and not `qa` — and `-0.0 + (+0.0)` is `+0.0`. Nothing in
/// this crate manufactures the input: `exp_so3` carries a sign of zero only
/// from an axis component that already has one, and `sample_rotation`'s
/// rotations contain no zero component at all (measured over `k` in `0..1000`),
/// which is why every other sweep in this file misses this.
///
/// It is pinned rather than fixed. The fix is an `s == 0.0` shortcut, and
/// `slerp`'s doc rules out its `s == 1.0` twin on purpose — the limit from
/// below at `s = 1` goes to `-qb` under the sign fix, so a shortcut returning
/// `qb` would put a jump in the components at exactly the endpoint. Fixing one
/// end and not the other buys a sign of zero at the price of an asymmetry
/// between the two endpoints.
#[test]
fn signed_zero_components_are_the_endpoint_exception() {
    let id = Quat::IDENTITY;
    // A `-0.0` in `qa`, and a `qb` whose matching component is positive so the
    // sum is `-0.0 + (+0.0)`.
    let qa = Quat::new(1.0, -0.0, 0.0, 0.0);
    assert!(qa.x.is_sign_negative(), "the input lost its sign of zero");

    // One pair per branch, by the *pair's* angle: fallback, series, closed form.
    for &theta in &[THETA_FALLBACK * 0.1, 0.02, THETA_CROSSOVER * 2.0] {
        let qb = step(id, theta);
        assert!(
            qb.x > 0.0,
            "theta={theta} does not exercise `-0.0 + (+0.0)`"
        );
        let out = slerp(qa, qb, 0.0);
        assert_ne!(
            bits(out),
            bits(qa),
            "s=0 became bit-exact for a -0.0 component at theta={theta}; if that \
             is intended, `slerp`'s third endpoint bullet and this test are stale"
        );
        assert!(
            out.x == 0.0 && out.x.is_sign_positive(),
            "expected +0.0, got {} at theta={theta}",
            out.x
        );
        // The departure is a sign and nothing else: as a rotation, identical.
        assert_eq!(out.sub(qa).norm(), 0.0, "theta={theta}");
    }
}

/// The output stays unit across all three branches without being renormalized.
///
/// `slerp` returns `qa·wa + qb·wb`, so the departure is the rounding of six
/// multiplies and three adds, and a caller whose type enforces unit norm needs
/// to know it is an ulp rather than nothing.
///
/// **The bound is stated in ulp, and it is deliberately not the measured worst
/// case.** This sweep measures `3.3306690738754696e-16` on x86-64, which is
/// `1.5·ε` — and `|‖q‖ − 1|` is *quantized* near 1.0: the representable norms
/// step by `ε/2` below 1.0 and by `ε` above it, so the departure can only be
/// `0`, `0.5·ε`, `1.0·ε`, `1.5·ε`, `2.0·ε`, … The measured value is the fourth
/// of those, and the next one is `4.440892098500626e-16`. An
/// `assert!(worst < 4e-16)` — which is what this line was — therefore had
/// **exactly one representable step** of headroom on a quantity produced by
/// `libm::sqrt` and by whatever the target does with `a*b + c`. aarch64 CI
/// became real on 2026-08-16 and an FMA-contracting target is a live
/// possibility; a one-step margin is the same kind of gate this file already
/// deleted an `assert_eq!(differing_bits, 160)` for being. `4·ε` keeps the
/// claim worth defending — a few ulp, not a renormalization and not `1e-8` —
/// with room for a target that rounds this sum differently.
#[test]
fn unit_norm_survives_every_branch() {
    let mut worst = 0.0f64;
    let mut samples = 0usize;
    for k in 0..16 {
        let qa = sample_rotation(k);
        for i in 0..=120 {
            // 1e-9 rad to just past pi, log-spaced: fallback, series and closed
            // form in one sweep.
            let theta = 1e-9 * libm::pow(10.0, i as f64 * 9.5 / 120.0);
            let qb = step(qa, theta);
            for j in 0..=10 {
                let s = j as f64 / 10.0;
                worst = worst.max((slerp(qa, qb, s).norm() - 1.0).abs());
                samples += 1;
            }
        }
    }
    assert!(samples > 20_000, "sweep collapsed to {samples} samples");
    let bound = 4.0 * f64::EPSILON;
    assert!(
        worst < bound,
        "worst |‖slerp‖ − 1| = {worst:e} ({:.1} ulp of 1.0), over the {:.0}-ulp \
         bound; the output stopped being unit to within the rounding of \
         `qa·wa + qb·wb`",
        worst / f64::EPSILON,
        bound / f64::EPSILON
    );
    assert!(worst > 0.0, "nothing was actually measured");
}

/// Shortest arc: the sign of `qb` cannot change the answer, and the path never
/// leaves the hemisphere nearest `qa`.
///
/// Sign invariance is the sign fix stated as a property a caller can rely on —
/// `qb` and `-qb` are the same rotation, so an input pipeline that normalizes
/// the double cover differently must not change the output. It holds
/// bit-for-bit, because the fix and the `acos` argument both go through `|dot|`.
///
/// Mutant, run: `if dot < 0.0` → `if dot < -0.5` ⇒ this test fails at
/// `k=0, theta=1.2, s=0.1` and `endpoints_are_bit_exact_above_the_lerp_fallback`
/// fails at `s = 1, theta = 1.9`.
#[test]
fn the_arc_is_the_short_one_and_the_sign_of_qb_is_irrelevant() {
    let mut saw_obtuse = 0usize;
    for k in 0..24 {
        let qa = sample_rotation(k);
        for &theta in &[THETA_FALLBACK * 0.1, 0.05, 0.3, 1.2, 2.0, 2.9] {
            let qb = step(qa, theta);
            let dot = qa.dot(qb);
            if dot < 0.0 {
                saw_obtuse += 1;
            }
            for j in 0..=10 {
                let s = j as f64 / 10.0;
                let out = slerp(qa, qb, s);
                assert!(
                    out.dot(qa) >= 0.0,
                    "left the near hemisphere: k={k} theta={theta} s={s}"
                );
                // `dot == 0.0` is the one place this is not required to hold:
                // the branch is `dot < 0.0`, so at exactly zero the two calls
                // take opposite arms of a tie between two equally short arcs.
                if dot != 0.0 {
                    assert_eq!(
                        bits(out),
                        bits(slerp(qa, qb.neg(), s)),
                        "negating qb changed the result: k={k} theta={theta} s={s}"
                    );
                }
            }
        }
    }
    assert!(
        saw_obtuse > 0,
        "no pair needed the sign fix, so nothing above tested it"
    );
}

/// Identical inputs short-circuit, and `s` cannot perturb the answer.
///
/// A stationary body publishes this pair on every tick, so it is the most
/// common input this function sees in production and the one where the
/// arithmetic below it has no direction to work with. Coverage says the arm is
/// reached nowhere else in this file: `cargo llvm-cov` counted the `return qa`
/// line 0 times before this test existed.
#[test]
fn numerically_identical_inputs_return_qa_at_every_s() {
    for k in 0..16 {
        let qa = sample_rotation(k);
        for j in 0..=10 {
            let s = j as f64 / 10.0;
            assert_eq!(bits(slerp(qa, qa, s)), bits(qa), "k={k} s={s}");
        }
    }
}

/// The exact great-circle point at parameter `s`, valid **off** the segment as
/// well as on it: stepping `qa` by `s` times the pair's quaternion angle is the
/// definition an extrapolation is judged against.
fn geodesic(qa: Quat, theta: f64, s: f64) -> Quat {
    step(qa, s * theta)
}

/// The double cover: `q` and `-q` are one rotation, so the distance that counts
/// is the smaller of the two.
fn rot_dist(a: Quat, b: Quat) -> f64 {
    a.sub(b).norm().min(a.add(b).norm())
}

/// **`s` outside `[0, 1]` is documented as unsupported, and this is the
/// measurement that sentence rests on.**
///
/// The three branches do not degrade alike, and which one a caller gets is
/// chosen by the *pair* — so the accuracy of an extrapolation is set by the
/// publish rate, which is the argument for refusing to promise anything about
/// it. The bands here are two to four orders wide on purpose: what is being
/// pinned is the *shape* (closed form holds, series collapses, the fallback's
/// chord is fine until the arc it cuts stops being small), not a libm revision.
#[test]
fn out_of_range_s_extrapolates_and_only_the_closed_form_holds() {
    // Closed form: `sin((1-s)*theta)/sin theta` is as true off the segment as
    // on it, and the arithmetic does not care.
    let mut worst_closed = 0.0f64;
    for k in 0..40 {
        let qa = sample_rotation(k);
        let qb = step(qa, 0.5);
        for &s in &[-20.0, -5.0, 2.0, 5.0, 20.0] {
            worst_closed = worst_closed.max(rot_dist(slerp(qa, qb, s), geodesic(qa, 0.5, s)));
        }
    }
    assert!(worst_closed > 0.0, "nothing was actually measured");
    assert!(
        worst_closed < 1e-13,
        "the closed form stopped extrapolating: {worst_closed:e}"
    );

    // Series: `slerp_weight` is calibrated for |a| <= 1. Same pair, in range and
    // far out of it — the contrast is the finding, not either number alone.
    let mut worst_in = 0.0f64;
    let mut worst_out = f64::MAX;
    for k in 0..40 {
        let qa = sample_rotation(k);
        let qb = step(qa, 0.1);
        for j in 0..=10 {
            let s = j as f64 / 10.0;
            worst_in = worst_in.max(rot_dist(slerp(qa, qb, s), geodesic(qa, 0.1, s)));
        }
        worst_out = worst_out.min(rot_dist(slerp(qa, qb, 20.0), geodesic(qa, 0.1, 20.0)));
    }
    assert!(
        worst_in < 1e-14,
        "the series stopped being exact in range: {worst_in:e}"
    );
    assert!(
        worst_out > 1e-8,
        "the series' out-of-range collapse is gone; the doc's bullet is stale: {worst_out:e}"
    );

    // Where it leaves 1e-15 — the number the doc quotes as "between |s| ~ 2.3
    // and ~ 5 depending on the angle".
    for &theta in &[0.02, 0.1, 0.1499] {
        for k in 0..40 {
            let qa = sample_rotation(k);
            let qb = step(qa, theta);
            let mut s = 1.0;
            while s < 8.0 {
                if rot_dist(slerp(qa, qb, s), geodesic(qa, theta, s)) > 1e-15 {
                    break;
                }
                s += 0.01;
            }
            assert!(
                (2.0..6.0).contains(&s),
                "series left 1e-15 at |s| = {s} (theta={theta}, k={k}); the doc says 2.3 to 5"
            );
        }
    }

    // LERP fallback: a chord, extrapolated and renormalized — the crudest of the
    // three mechanically and the most forgiving numerically, because the arc it
    // cuts is tiny until |s| is enormous.
    let mut worst_near = 0.0f64;
    let mut worst_far = f64::MAX;
    for k in 0..40 {
        let qa = sample_rotation(k);
        let theta = THETA_FALLBACK * 0.1;
        let qb = step(qa, theta);
        worst_near = worst_near.max(rot_dist(slerp(qa, qb, 100.0), geodesic(qa, theta, 100.0)));
        worst_far = worst_far.min(rot_dist(slerp(qa, qb, 1e6), geodesic(qa, theta, 1e6)));
    }
    assert!(
        worst_near < 1e-12,
        "the fallback's chord showed up far earlier than documented: {worst_near:e}"
    );
    assert!(
        worst_far > 1e-6,
        "the fallback stopped being a chord: {worst_far:e}"
    );
}

/// `NaN` propagates through every branch — except the one early return that
/// answers before `s` is ever read.
///
/// That exception is the interesting half: a stationary body publishes
/// numerically identical samples, so `slerp(qa, qa, s)` is a *common* call, and
/// it swallows a `NaN` (or an infinite) `s` and returns `qa`. A caller who
/// expects a poisoned stamp to show up as a `NaN` pose gets a plausible one
/// instead, and only from the pairs that carry no motion.
///
/// # **Do not "harden" the closed form to make this pass differently**
///
/// A `NaN` *component* survives by a mechanism that reads like a bug. `h` is
/// `NaN`, and `NaN <= x` is false for every `x`, so the component clears the
/// identical-input return **and both branch tests** — the closed form is the
/// only arm it can reach, at any angle. There `dot` is `NaN`,
/// `NaN.min(1.0)` is `1.0`, so `angle` is `acos(1.0) = 0.0` and `sin_angle` is
/// `0.0`: the `NaN` is destroyed, and the output is `NaN` only because both
/// weights come back as `0.0/0.0`.
///
/// So a guard of the form `if sin_angle == 0.0 { return qa; }` — which looks
/// like a defence against dividing by zero and is the edit a reader reaches for
/// on seeing that line — turns a `NaN` input into a **plausible pose**. If one
/// of the `qnan` assertions below fails on your branch, the answer is not to
/// relax it.
///
/// Mutant, run: that exact guard inserted after `let sin_angle = …` ⇒ this test
/// fails at `k=0` on *"NaN in qb was swallowed"*, and the other nine tests in
/// this file all still pass.
///
/// (`dot.clamp(-1.0, 1.0)` in place of `.min(1.0)` is safe — `clamp` returns
/// `NaN` for a `NaN` receiver — and would make this test pass for the reason it
/// claims to. It is not the current line; `slerp`'s docs record why.)
#[test]
fn nan_propagates_except_through_the_identical_input_return() {
    let all_nan = |q: Quat| q.w.is_nan() && q.x.is_nan() && q.y.is_nan() && q.z.is_nan();
    for k in 0..16 {
        let qa = sample_rotation(k);
        // One pair per branch *for a NaN `s`*: fallback, series, closed form.
        // A NaN *component* is not swept across branches by this list and
        // cannot be: it makes `h` NaN, so it lands in the closed form whatever
        // the angle. The three thetas are the same three either way, so the
        // `qnan` assertions below repeat one branch three times, on purpose —
        // they are the ones the `sin_angle == 0.0` guard would break.
        for &theta in &[THETA_FALLBACK * 0.1, 0.1, 0.5] {
            let qb = step(qa, theta);
            assert!(
                all_nan(slerp(qa, qb, f64::NAN)),
                "s=NaN survived: k={k} theta={theta}"
            );
            let qnan = Quat::new(f64::NAN, qb.x, qb.y, qb.z);
            assert!(
                all_nan(slerp(qa, qnan, 0.5)),
                "NaN in qb was swallowed: k={k}. The closed form recreates it as \
                 0.0/0.0; a guard on `sin_angle == 0.0` returns a plausible pose \
                 instead. See this test's doc comment."
            );
            assert!(
                all_nan(slerp(qnan, qb, 0.5)),
                "NaN in qa was swallowed: k={k}. See this test's doc comment \
                 before relaxing this."
            );
        }
        // The exception, both ways it fires. `h` is `0`, so nothing downstream
        // ever looks at `s`.
        assert_eq!(bits(slerp(qa, qa, f64::NAN)), bits(qa), "k={k}");
        assert_eq!(bits(slerp(qa, qa, f64::INFINITY)), bits(qa), "k={k}");
        // And a NaN *component* cannot reach it: `h` is NaN, not `<= 0.0`.
        let qnan = Quat::new(f64::NAN, qa.x, qa.y, qa.z);
        assert!(
            all_nan(slerp(qnan, qnan, 0.5)),
            "the early return ate a NaN component: k={k}"
        );
    }
}

/// **What a caller who drops the `Iso3` round trip is signing up for.**
///
/// The reason this function became public is that a downstream crate was
/// reaching it by building two [`tf_tree_math::Iso3`] with zero translations
/// and calling `LerpSlerp::eval`. That swap has to be behaviour-preserving to
/// be worth making, and "obviously it is, it is the same code" is exactly the
/// claim that turns out to have two exceptions — so this enumerates them
/// instead of asserting the absence.
///
/// As *rotations* the two never disagree: worst `2.7336071744532853e-16` over
/// the sweep below — the same number
/// `endpoints_lose_bit_exactness_only_in_the_lerp_fallback` measures, because
/// it is the same renormalization and nothing else. The **bits** differ at some
/// of the 7560 sample points (160 of them on this host), and every difference is
/// at `s = 0` or `s = 1`, because those are the two parameters
/// `LerpSlerp::eval` answers from a shortcut before it ever calls this function:
///
/// * `s = 1` with `qa·qb < 0` — `eval` returns `qb`, this returns `-qb`. Same
///   rotation, opposite components. A consumer that compares quaternion
///   components rather than rotations sees this one.
/// * `s = 0` or `s = 1` under the LERP fallback — `eval`'s shortcut is exact,
///   this renormalizes.
#[test]
fn the_iso3_round_trip_it_replaces_agrees_as_a_rotation() {
    let mut differing_bits = 0usize;
    let mut samples = 0usize;
    let mut worst = 0.0f64;
    for k in 0..40 {
        let qa = sample_rotation(k);
        for &theta in &[1e-9, 1e-7, 1e-3, 0.1, 0.2, 1.0, 1.6, 2.5, 3.0] {
            let qb = step(qa, theta);
            let a = tf_tree_math::Iso3::new(qa, Vec3::ZERO);
            let b = tf_tree_math::Iso3::new(qb, Vec3::ZERO);
            for j in 0..=20 {
                let s = j as f64 / 20.0;
                let direct = slerp(qa, qb, s);
                let via_iso3 = <tf_tree_math::LerpSlerp as tf_tree_math::Interp>::eval(&a, &b, s).q;
                samples += 1;

                // The double cover is the point: `q` and `-q` are one rotation,
                // so the distance that matters is the smaller of the two.
                let d = direct.sub(via_iso3).norm().min(direct.add(via_iso3).norm());
                worst = worst.max(d);

                if bits(direct) != bits(via_iso3) {
                    differing_bits += 1;
                    assert!(
                        s == 0.0 || s == 1.0,
                        "the two disagree away from an endpoint: k={k} theta={theta} s={s}"
                    );
                }
            }
        }
    }
    assert_eq!(
        samples, 7560,
        "sweep changed shape; the 7560 in this test's doc comment is stale"
    );
    // **Not `assert_eq!(differing_bits, 160)`, which is what this line used to
    // be.** 160 is a joint property of `sample_rotation`'s arbitrary rotations,
    // the theta list, `libm`'s `sin`/`cos`/`acos`, and whether each `qa`'s
    // components happen to square to exactly 1.0 — the same objection this file
    // already makes to a bit-pattern gate 150 lines up, and none of it is the
    // behaviour under test. What is under test is asserted twice already: every
    // difference is at an endpoint (in the loop, where it names the case that
    // broke it) and the two stay one rotation to 4e-16 (below). All that is left
    // for this line is that the enumeration had something to enumerate.
    assert!(
        differing_bits > 0,
        "no sample distinguished the direct call from the Iso3 round trip, so \
         the endpoint exceptions this test documents were never exercised"
    );
    assert!(
        worst < 4e-16,
        "the two stopped being the same rotation: {worst:e}"
    );
}
