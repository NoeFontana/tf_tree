//! `slerp` through the crate's public path, as an external caller reaches it (an
//! in-module test cannot observe visibility). Deterministic sweeps, each
//! asserting that it reached the case it covers.

use tf_tree_math::{exp_so3, interp, slerp, Quat, Vec3};

/// The series/closed-form crossover, mirrored from `interp.rs`'s private
/// `THETA_SLERP_SMALL` (quaternion angle); a `const` assertion there guards it.
const THETA_CROSSOVER: f64 = 0.15;

/// The LERP-fallback threshold, mirrored from `SLERP_LERP_FALLBACK` on the same
/// terms as [`THETA_CROSSOVER`].
const THETA_FALLBACK: f64 = 1e-6;

/// The four components as raw bits: `f64` equality conflates `0.0` and `-0.0`.
fn bits(q: Quat) -> [u64; 4] {
    [q.w.to_bits(), q.x.to_bits(), q.y.to_bits(), q.z.to_bits()]
}

/// A deterministic unit quaternion, varied across `k`, built by `exp_so3`.
fn sample_rotation(k: usize) -> Quat {
    let f = k as f64;
    exp_so3(Vec3::new(
        0.31 * libm::sin(f * 0.7) + 0.4,
        -0.83 * libm::cos(f * 0.37),
        1.1 * libm::sin(f * 0.19) - 0.2,
    ))
}

/// A fixed unit axis.
fn axis() -> Vec3 {
    let a = Vec3::new(1.0, 2.0, -3.0);
    a.scale(1.0 / a.norm())
}

/// `qa` rotated by `theta` quaternion angle (half the rotation angle).
fn step(qa: Quat, theta: f64) -> Quat {
    qa * exp_so3(axis().scale(2.0 * theta))
}

/// Compiles only if both arguments are the same item: a function item's type is
/// unique to its definition.
fn same_item<T>(_: T, _: T) {}

/// The root re-export and the module path are the same item, not two
/// (`PROJECT.md` §6). `tf_tree::slerp` is checked in
/// `tf_tree/tests/math_reexports.rs`.
#[test]
fn the_root_re_export_and_the_module_path_are_one_function() {
    same_item(slerp, interp::slerp);
    same_item(tf_tree_math::slerp, tf_tree_math::interp::slerp);
}

/// Above the LERP fallback, both endpoints come back bit-for-bit (`-qb` when the
/// sign fix fires). The counters check that the sweep straddles the documented
/// [`THETA_CROSSOVER`], not which branch ran.
#[test]
fn endpoints_are_bit_exact_above_the_lerp_fallback() {
    let mut saw_sign_fix = 0usize;
    let mut below_crossover = 0usize;
    let mut above_crossover = 0usize;
    for k in 0..24 {
        let qa = sample_rotation(k);
        // Straddles the crossover and reaches past pi/2, where the sign fix fires.
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

/// The LERP fallback is the one branch where an endpoint is not bit-exact:
/// below [`THETA_FALLBACK`] (`1e-6`) the result is a renormalized LERP, so
/// `s = 0` differs from `qa` by a couple of ulp unless `qa` is already unit to
/// the bit. That ulp is the observable: it appears below the threshold and
/// disappears above it with the same `qa`.
#[test]
fn endpoints_lose_bit_exactness_only_in_the_lerp_fallback() {
    let mut discriminating = 0usize;
    let mut worst = 0.0f64;
    for k in 0..64 {
        let qa = sample_rotation(k);
        // `normalize` is a no-op when the norm rounds to exactly 1.0: such a `qa` is no evidence.
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
    // Asserted as a band ("a couple of ulp, not zero"), not a bit pattern, so an
    // unrelated libm change does not fail it.
    assert!(
        (2.0e-16..4.0e-16).contains(&worst),
        "documented fallback endpoint departure moved: {worst:e}"
    );
}

/// A `-0.0` component is the one input for which "both endpoints come back
/// bit-for-bit" is false, on every branch: the weights are `(1, 0)`, so the
/// answer is `qa·1.0 + qb·0.0`, and `-0.0 + 0.0` is `+0.0`. Pinned, not fixed:
/// an `s == 0.0` shortcut without its `s == 1.0` twin (ruled out by `slerp`'s
/// doc) would make the endpoints asymmetric.
#[test]
fn signed_zero_components_are_the_endpoint_exception() {
    let id = Quat::IDENTITY;
    // A `-0.0` in `qa`, and a positive `qb` component.
    let qa = Quat::new(1.0, -0.0, 0.0, 0.0);
    assert!(qa.x.is_sign_negative(), "the input lost its sign of zero");

    // One pair per branch: fallback, series, closed form.
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
        // A sign and nothing else: as a rotation, identical.
        assert_eq!(out.sub(qa).norm(), 0.0, "theta={theta}");
    }
}

/// The output stays unit across all three branches without renormalization.
///
/// The bound is 4·ε, deliberately not the measured worst case (1.5·ε on x86-64):
/// the departure is quantized in steps of ε/2..ε, so a tighter bound leaves one
/// representable step of headroom on other targets (FMA, aarch64).
#[test]
fn unit_norm_survives_every_branch() {
    let mut worst = 0.0f64;
    let mut samples = 0usize;
    for k in 0..16 {
        let qa = sample_rotation(k);
        for i in 0..=120 {
            // Log-spaced from 1e-9 rad to just past pi: all three branches.
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

/// Shortest arc: the sign of `qb` cannot change the answer, bit-for-bit, because
/// the sign fix and the `acos` argument both go through `|dot|`.
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
                // At `dot == 0.0` the two calls take opposite arms of a tie.
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

/// The exact great-circle point at parameter `s`, valid off the segment too.
fn geodesic(qa: Quat, theta: f64, s: f64) -> Quat {
    step(qa, s * theta)
}

/// The double cover: the distance that counts is the smaller of the two.
fn rot_dist(a: Quat, b: Quat) -> f64 {
    a.sub(b).norm().min(a.add(b).norm())
}

/// `s` outside `[0, 1]` is documented as unsupported; this measures why. The
/// branches degrade differently (closed form holds, series collapses, the
/// fallback's chord is fine until the arc grows), so the bands are wide and pin
/// the shape, not a libm revision.
#[test]
fn out_of_range_s_extrapolates_and_only_the_closed_form_holds() {
    // Closed form: as true off the segment as on it.
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

    // Series: calibrated for |a| <= 1; same pair, in range and far out.
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

    // Where the series leaves 1e-15 (the doc quotes |s| ~ 2.3..5).
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

    // LERP fallback: a chord, extrapolated and renormalized; forgiving until |s| is enormous.
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

/// `NaN` propagates through every branch except the early return that answers
/// before `s` is read: `slerp(qa, qa, NaN)` returns `qa`.
///
/// Do not "harden" the closed form. A `NaN` component reaches the closed form,
/// where `acos(NaN.min(1.0)) = 0` makes `sin_angle` zero and the output `NaN` only
/// via `0.0/0.0`; a `sin_angle == 0.0` guard would turn a `NaN` input into a
/// plausible pose. `dot.clamp(-1.0, 1.0)` would be safe; see `slerp`'s docs.
#[test]
fn nan_propagates_except_through_the_identical_input_return() {
    let all_nan = |q: Quat| q.w.is_nan() && q.x.is_nan() && q.y.is_nan() && q.z.is_nan();
    for k in 0..16 {
        let qa = sample_rotation(k);
        // One pair per branch for a NaN `s`. A NaN component always lands in the
        // closed form, so the `qnan` assertions repeat that branch on purpose.
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
        // `h` is `0`, so nothing downstream reads `s`.
        assert_eq!(bits(slerp(qa, qa, f64::NAN)), bits(qa), "k={k}");
        assert_eq!(bits(slerp(qa, qa, f64::INFINITY)), bits(qa), "k={k}");
        // A NaN component cannot reach it: `h` is NaN, not `<= 0.0`.
        let qnan = Quat::new(f64::NAN, qa.x, qa.y, qa.z);
        assert!(
            all_nan(slerp(qnan, qnan, 0.5)),
            "the early return ate a NaN component: k={k}"
        );
    }
}

/// What dropping the `Iso3` round trip costs: as rotations `slerp` and
/// `LerpSlerp::eval` never disagree (worst ~2.7e-16, the renormalization); the
/// bits differ only at `s = 0` or `s = 1`, where `eval` answers from a shortcut:
///
/// * `s = 1` with `qa·qb < 0`: `eval` returns `qb`, this returns `-qb`.
/// * `s = 0` or `s = 1` under the LERP fallback: `eval` is exact, this renormalizes.
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

                // The double cover: the smaller distance counts.
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
    // The enumeration must have had something to enumerate; the exact count is
    // not asserted (it depends on the sample set and libm).
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
