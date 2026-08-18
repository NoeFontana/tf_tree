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

/// The four components as raw bits, for the exactness claims below. `f64`
/// equality would call `0.0` and `-0.0` the same, and one of the cases here is
/// precisely a sign of zero.
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

/// The root re-export and the module path are the same item, not two.
///
/// `PROJECT.md` §6 forbids a second spelling of an existing path. `interp::slerp`
/// and the root `slerp` are one function reached two ways — the same arrangement
/// `Quat`, `Iso3` and `exp_so3` already have — and this is what says so rather
/// than leaving a reader to assume it.
#[test]
fn the_root_re_export_and_the_module_path_are_one_function() {
    let qa = sample_rotation(3);
    let qb = step(qa, 0.4);
    for j in 0..=8 {
        let s = j as f64 / 8.0;
        assert_eq!(bits(slerp(qa, qb, s)), bits(interp::slerp(qa, qb, s)));
    }
}

/// Above the LERP fallback, both endpoints come back bit-for-bit — `qb`
/// negated when the sign fix fires, which is the same rotation.
///
/// Mutant, run: `slerp_weight`'s `let k = 1.0 - x` → `let k = 1.0`, dropping the
/// `(1 − a²)` factor that is *why* the series is exact at both ends ⇒ this test
/// fails at `s = 0, theta = 2e-6`. That factor is an algebraic property rather
/// than a truncation artifact, so nothing about term count could substitute.
#[test]
fn endpoints_are_bit_exact_above_the_lerp_fallback() {
    let mut saw_sign_fix = 0usize;
    let mut saw_series = 0usize;
    let mut saw_closed_form = 0usize;
    for k in 0..24 {
        let qa = sample_rotation(k);
        // Straddles the 0.15 rad series/closed-form crossover and reaches past
        // pi/2, where the shortest-arc sign fix starts firing.
        for &theta in &[2e-6, 1e-4, 0.02, 0.1499, 0.1501, 0.6, 1.4, 1.9, 2.8] {
            let qb = step(qa, theta);
            if theta <= 0.15 {
                saw_series += 1;
            } else {
                saw_closed_form += 1;
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
        saw_series > 0 && saw_closed_form > 0,
        "one branch never ran"
    );
    assert!(
        saw_sign_fix > 0,
        "no pair had a negative dot, so the documented `s = 1` sign was never checked"
    );
}

/// **The LERP fallback is the one branch where an endpoint is not bit-exact,
/// and this is what proves the branch runs at all.**
///
/// Below a quaternion angle of `1e-6` the result is a *renormalized* LERP, so
/// `s = 0` returns `qa/‖qa‖`. For a `qa` whose components happen to square to
/// exactly `1.0` that is `qa`; for one that does not — about a third of the
/// rotations sampled here — it differs by a couple of ulp. No value comparison
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

        let inside = slerp(qa, step(qa, 1e-7), 0.0);
        assert_ne!(bits(inside), bits(qa), "fallback not taken at 1e-7, k={k}");
        let d = inside.sub(qa).norm();
        assert!(d < 1e-15, "fallback perturbed s=0 by {d:e}, k={k}");
        worst = worst.max(d);

        let outside = slerp(qa, step(qa, 1e-5), 0.0);
        assert_eq!(
            bits(outside),
            bits(qa),
            "series branch not exact at 1e-5, k={k}"
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

/// The output stays unit across all three branches without being renormalized.
///
/// The bound is what the sweep measures, not a round number: `slerp` returns
/// `qa·wa + qb·wb`, so the departure is the rounding of six multiplies and
/// three adds, and a caller whose type enforces unit norm needs to know it is
/// an ulp rather than nothing.
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
    assert!(worst < 4e-16, "worst |‖slerp‖ − 1| = {worst:e}");
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
        for &theta in &[1e-7, 0.05, 0.3, 1.2, 2.0, 2.9] {
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
/// it is the same renormalization and nothing else. The **bits** differ at 160
/// of 7560 sample points, and every one of them is at `s = 0` or `s = 1`,
/// because those are the two parameters `LerpSlerp::eval` answers from a
/// shortcut before it ever calls this function:
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
        "sweep changed shape; the counts below are stale"
    );
    assert_eq!(differing_bits, 160, "the endpoint delta moved");
    assert!(
        worst < 4e-16,
        "the two stopped being the same rotation: {worst:e}"
    );
}
