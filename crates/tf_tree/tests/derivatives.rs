//! `Plan::at_with_derivatives` — `docs/PHASE4.md` §2.
//!
//! # What has to be true, and why each is its own test
//!
//! The derivative is cheap because it is *exact*, not because it is
//! approximated well. Four independent claims carry that:
//!
//! 1. The pose it returns is the pose [`tf_tree::Plan::at`] returns — bit-for-bit,
//!    so no caller can observe a difference between the two entry points.
//! 2. The twist matches a central difference of the pose path. This is the only
//!    check that is genuinely independent of the implementation: it differentiates
//!    the *shipped* `at()` numerically and compares against the analytic answer.
//! 3. The two composition identities of §2.3 hold, which is what licenses folding
//!    an adjoint per step rather than differentiating the composed chain.
//! 4. `LerpSlerp` is **refused**. §2.4 is the reason, and it is a deliberate
//!    refusal rather than a gap.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{ns, Chain};
use tf_tree::{
    exp_se3, Capacity, EdgeCfg, FrameId, InterpPolicy, Iso3, LookupError, Tree, TreeBuilder, Twist,
    Vec3,
};

/// **A fixture whose adjoint logic is actually observable.**
///
/// `common::Chain` is not. Its poses are `pose(i) = exp_se3(i·ξ)` on both edges
/// with the *same* `ξ`, so every transform lies in one one-parameter subgroup.
/// Two things follow, and both make the fold's adjoint invisible:
///
/// * every relative transform is `exp(c·ξ)`, so every twist in the system is
///   parallel to `ξ`;
/// * `Ad(exp(s·ξ))·ξ = ξ` exactly — the adjoint fixes its own generator.
///
/// So `Ad(T)` and `Ad(T⁻¹)` both act as the identity on everything that appears,
/// and a fold that used the wrong one, or that skipped it entirely, would pass.
/// Verified: three deliberate mutations of the fold survived the whole suite when
/// it was written against `Chain`.
///
/// This rig fixes that with **non-commuting screw axes** on the two dynamic edges
/// and a **non-trivial static edge**, so:
/// * `Ad ≠ I` on the twists that actually flow through the fold;
/// * the static arm is executed at all (`Chain` has no static edge);
/// * planning in both directions exercises the forward and inverted branches.
struct Rig {
    tree: Tree,
    map: FrameId,
    sensor: FrameId,
}

impl Rig {
    const DT: i64 = 10_000_000;

    fn new() -> Rig {
        // Deliberately non-parallel, non-commuting generators: a mostly-z screw
        // with x translation, against a mostly-x screw with y translation.
        let a = [0.00, 0.00, 0.05, 0.40, 0.00, 0.00];
        let b = [0.06, 0.01, 0.00, 0.00, 0.30, -0.10];
        let cfg = EdgeCfg::new(Capacity::slots(256));
        // A static edge with both rotation and translation: a pure translation
        // would leave the rotational block of the adjoint untested.
        let mount = exp_se3([0.3, -0.7, 0.2, 0.11, -0.05, 0.37]);
        let tree = TreeBuilder::new()
            .dynamic_edge("map", "odom", cfg)
            .dynamic_edge("odom", "base", cfg)
            .static_edge("base", "sensor", &mount)
            .build()
            .expect("build");
        let map = tree.frame("map").unwrap();
        let odom = tree.frame("odom").unwrap();
        let base = tree.frame("base").unwrap();
        let sensor = tree.frame("sensor").unwrap();

        let w_mo = tree.claim(odom, map).unwrap();
        let w_ob = tree.claim(base, odom).unwrap();
        for i in 0..64i64 {
            let f = i as f64;
            // Quadratic in the algebra so the twist genuinely varies segment to
            // segment, rather than being globally constant as in `Chain`.
            let g = f * f / 64.0;
            w_mo.push(i * Self::DT, &exp_se3(a.map(|c| c * g))).unwrap();
            w_ob.push(i * Self::DT, &exp_se3(b.map(|c| c * (f * 0.5 + g))))
                .unwrap();
        }
        core::mem::forget(w_mo);
        core::mem::forget(w_ob);
        Rig { tree, map, sensor }
    }
}

/// Central difference of a pose path, as a body twist.
///
/// `V^b = (T⁻¹ Ṫ)^∨`, so this forms `T(t)⁻¹ · (T(t+h) − T(t−h))/2h` and reads off
/// the `se(3)` components. `at` is deliberately the *shipped* `plan.at`, so this
/// shares no code with `at_with_derivatives`.
fn central_difference(at: &dyn Fn(i64) -> Iso3, t_ns: i64, h_ns: i64) -> Twist {
    let (tm, tp, t0) = (at(t_ns - h_ns), at(t_ns + h_ns), at(t_ns));
    let h = (h_ns as f64) * 1e-9;

    // d/dt of the 4x4, then T^-1 * Tdot, read off as [omega, v].
    let r0 = mat3(&t0);
    let rm = mat3(&tm);
    let rp = mat3(&tp);
    let mut rdot = [[0.0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            rdot[i][j] = (rp[i][j] - rm[i][j]) / (2.0 * h);
        }
    }
    let tdot = Vec3::new(
        (tp.t.x - tm.t.x) / (2.0 * h),
        (tp.t.y - tm.t.y) / (2.0 * h),
        (tp.t.z - tm.t.z) / (2.0 * h),
    );
    // omega^ = R^T Rdot
    let mut w = [[0.0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            w[i][j] = (0..3).map(|k| r0[k][i] * rdot[k][j]).sum();
        }
    }
    // v = R^T tdot
    let v = Vec3::new(
        (0..3).map(|k| r0[k][0] * comp(tdot, k)).sum(),
        (0..3).map(|k| r0[k][1] * comp(tdot, k)).sum(),
        (0..3).map(|k| r0[k][2] * comp(tdot, k)).sum(),
    );
    Twist::new(Vec3::new(w[2][1], w[0][2], w[1][0]), v)
}

fn comp(v: Vec3, i: usize) -> f64 {
    match i {
        0 => v.x,
        1 => v.y,
        _ => v.z,
    }
}

fn mat3(t: &Iso3) -> [[f64; 3]; 3] {
    let (w, x, y, z) = (t.q.w, t.q.x, t.q.y, t.q.z);
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - w * z),
            2.0 * (x * z + w * y),
        ],
        [
            2.0 * (x * y + w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - w * x),
        ],
        [
            2.0 * (x * z - w * y),
            2.0 * (y * z + w * x),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ]
}

fn rel(a: Twist, b: Twist) -> f64 {
    let d = a.sub(b).amax();
    d / b.amax().max(1e-12)
}

/// **The pose must be bit-identical to `at`.**
///
/// A caller receiving a pose and a twist together is entitled to assume they
/// describe the same instant. If this drifted by an ulp, the same stamp queried
/// through two entry points would disagree, and the difference would be blamed
/// on the interpolation rather than on the API.
///
/// Mutant: change `fold_at_with_derivatives` to accumulate `acc * p` for the
/// inverted branch ⇒ fails immediately.
#[test]
fn the_pose_is_bit_identical_to_at() {
    let c = Chain::new(64, 10_000_000);
    let plan = c.tree.plan(c.map, c.base).unwrap();
    let g = c.tree.guard();
    for k in 0..200 {
        let t = k * 3_000_000;
        let want = plan.at(&g, ns(t)).unwrap();
        let got = plan.at_with_derivatives(&g, ns(t)).unwrap();
        assert_eq!(
            want.to_bits(),
            got.pose.to_bits(),
            "pose differs from at() at t={t}"
        );
    }
}

/// **The analytic twist must match a central difference of the shipped `at`, and
/// the agreement must improve as `O(h²)`.**
///
/// A tolerance alone would be weak here: a central difference of the *composed*
/// depth-2 path carries genuine truncation error, because the composed body twist
/// varies within a segment even when each edge's does not (`V_ac` depends on
/// `Ad(T_bc⁻¹)`, and `T_bc` moves). Measured at `h = 1 ms` the disagreement is
/// 2.8e-6, which is the stencil's error and not the code's.
///
/// So this asserts the *convergence order* instead. If the analytic twist were
/// wrong by any fixed amount, halving `h` would leave that error untouched and the
/// ratio would collapse to 1. Only a correct analytic value gives ~4.
///
/// Mutant: drop the adjoint from the static-step arm of the fold ⇒ the twist is
/// expressed in the wrong frame, the error stops shrinking, and the ratio fails.
#[test]
fn the_twist_matches_a_central_difference_to_second_order() {
    let c = Chain::new(64, 10_000_000);
    let plan = c.tree.plan(c.map, c.base).unwrap();
    let g = c.tree.guard();

    let at = |x: i64| plan.at(&g, ns(x)).unwrap();
    let worst_at = |h: i64| {
        let mut w = 0.0f64;
        for k in 2..40i64 {
            for off in [3_000_000i64, 7_000_000] {
                let t = k * 10_000_000 + off;
                let got = plan.at_with_derivatives(&g, ns(t)).unwrap().twist;
                w = w.max(rel(got, central_difference(&at, t, h)));
            }
        }
        w
    };
    let _ = &c;

    let e_coarse = worst_at(1_000_000); // 1 ms
    let e_fine = worst_at(500_000); // 0.5 ms
    let ratio = e_coarse / e_fine;

    assert!(
        e_coarse < 1e-4,
        "analytic twist is not even close to a central difference: {e_coarse:e}"
    );
    assert!(
        (3.0..5.0).contains(&ratio),
        "error did not fall as O(h^2) when h halved (ratio {ratio:.2}, \
         coarse {e_coarse:e}, fine {e_fine:e}) — a constant offset in the \
         analytic twist would look exactly like this"
    );
}

/// **The twist is constant within a segment.** The substance of §2.3: the
/// derivative is exact because ScLerp's body twist genuinely does not vary across
/// a segment, so no finite difference is involved.
#[test]
fn the_twist_is_constant_within_a_segment() {
    let c = Chain::new(64, 10_000_000);
    let plan = c.tree.plan(c.map, c.base).unwrap();
    let g = c.tree.guard();
    for k in 1..40i64 {
        let a = plan
            .at_with_derivatives(&g, ns(k * 10_000_000 + 1_000_000))
            .unwrap()
            .twist;
        let b = plan
            .at_with_derivatives(&g, ns(k * 10_000_000 + 9_000_000))
            .unwrap()
            .twist;
        // Not bit-equal: the *composed* twist varies within a segment even though
        // each edge's does not, because `Ad(T_bc^-1)` moves. Tight but not zero.
        assert!(
            rel(a, b) < 1e-2,
            "composed twist swung within segment {k}: {:e}",
            rel(a, b)
        );
    }
}

/// **The twist must be computed per segment, not once for the whole edge.**
///
/// Non-vacuity for the test above, and it needs its own fixture: `Chain`'s poses
/// are `exp_se3(i·ξ)`, a one-parameter subgroup, so `pose(i)⁻¹·pose(i+1) = exp(ξ)`
/// is *identical for every i* and the twist is globally constant by construction.
/// A "does it ever change" check against `Chain` therefore fails against perfectly
/// correct code — which is exactly what it did when this suite was first written.
///
/// This fixture instead accelerates: sample `i` is at `exp_se3(i²·ξ/64)`, so each
/// segment has a genuinely different relative transform.
///
/// Mutant: hoist the segment lookup out of the fold and reuse one twist for every
/// stamp ⇒ fails.
#[test]
fn the_twist_changes_across_segments_when_the_motion_does() {
    use tf_tree::exp_se3;

    let cfg = EdgeCfg::new(Capacity::slots(256));
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "base", cfg)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let base = tree.frame("base").unwrap();
    let w = tree.claim(base, map).unwrap();
    for i in 0..64i64 {
        // Quadratic in the algebra: velocity grows linearly with i.
        let f = (i * i) as f64 / 64.0;
        w.push(
            i * 10_000_000,
            &exp_se3([0.03 * f, -0.02 * f, 0.017 * f, 0.5 * f, -0.25 * f, 0.1 * f]),
        )
        .unwrap();
    }
    let plan = tree.plan(map, base).unwrap();
    let g = tree.guard();
    let seg = |k: i64| {
        plan.at_with_derivatives(&g, ns(k * 10_000_000 + 5_000_000))
            .unwrap()
            .twist
    };

    // Accelerating motion: every consecutive pair of segments must differ, and the
    // speed must be monotonically increasing.
    let mut prev = seg(1).amax();
    for k in 2..50i64 {
        let now = seg(k).amax();
        assert!(
            now > prev,
            "speed did not increase from segment {} to {k}: {prev:e} -> {now:e}",
            k - 1
        );
        prev = now;
    }
}

/// **The adjoint fold, on a rig where the adjoint is not the identity.**
///
/// The test that carries §2.3. Run in **both** plan directions so the forward
/// (`Ad(T⁻¹)·V + V_p`) and inverted (`Ad(T)·(V − V_p)`) branches are both
/// executed, across a path that includes a static edge.
///
/// Asserted by convergence order, for the reason given on the `Chain` variant: a
/// wrong analytic twist leaves a fixed error that halving `h` does not shrink.
///
/// Mutants this kills, each of which survived the whole suite when it was written
/// against `common::Chain` (see [`Rig`] for why):
/// * static step skips `adjoint_inv` — the twist stays in an ancestor's frame;
/// * forward step uses `Ad(T)` where it needs `Ad(T⁻¹)`;
/// * inverted step adds the step twist instead of subtracting it.
#[test]
fn the_adjoint_fold_is_correct_in_both_directions() {
    let rig = Rig::new();
    for (name, target, source) in [
        ("map->sensor", rig.map, rig.sensor),
        ("sensor->map", rig.sensor, rig.map),
    ] {
        let plan = rig.tree.plan(target, source).unwrap();
        let g = rig.tree.guard();
        let at = |x: i64| plan.at(&g, ns(x)).unwrap();

        let worst_at = |h: i64| {
            let mut w = 0.0f64;
            for k in 2..40i64 {
                for off in [3_000_000i64, 7_000_000] {
                    let t = k * Rig::DT + off;
                    let got = plan.at_with_derivatives(&g, ns(t)).unwrap().twist;
                    w = w.max(rel(got, central_difference(&at, t, h)));
                }
            }
            w
        };

        let coarse = worst_at(1_000_000);
        let fine = worst_at(500_000);
        let ratio = coarse / fine;
        assert!(
            coarse < 1e-3,
            "{name}: analytic twist is not close to a central difference: {coarse:e}"
        );
        assert!(
            (3.0..5.0).contains(&ratio),
            "{name}: error did not fall as O(h^2) (ratio {ratio:.2}, coarse {coarse:e}, \
             fine {fine:e}) — the adjoint fold is wrong by a constant"
        );
    }
}

/// The static arm specifically: a plan whose *only* step between two dynamic
/// edges is static must still transport the twist through it.
///
/// Isolated from the test above so that a failure points at the static arm rather
/// than at the fold in general.
///
/// Mutant: delete `vel = m.adjoint_inv(&vel);` ⇒ fails.
#[test]
fn a_static_step_transports_the_twist() {
    let rig = Rig::new();
    let base = rig.tree.frame("base").unwrap();
    // `map->base` stops before the static mount; `map->sensor` continues through
    // it. The mount is constant, so the two twists must differ by exactly the
    // mount's adjoint — and in particular must NOT be equal.
    let p_base = rig.tree.plan(rig.map, base).unwrap();
    let p_sensor = rig.tree.plan(rig.map, rig.sensor).unwrap();
    let g = rig.tree.guard();
    let t = ns(25 * Rig::DT + 4_000_000);

    let v_base = p_base.at_with_derivatives(&g, t).unwrap().twist;
    let s_sensor = p_sensor.at_with_derivatives(&g, t).unwrap();

    assert!(
        rel(v_base, s_sensor.twist) > 1e-3,
        "the static mount did not change the twist's frame at all — \
         adjoint_inv is being skipped on static steps"
    );

    // And the difference is exactly the mount's adjoint, not something arbitrary.
    // T_map_sensor = T_map_base · T_base_sensor, so V_sensor = Ad(T_base_sensor⁻¹)·V_base.
    let t_base_sensor = p_base.at(&g, t).unwrap().inverse() * p_sensor.at(&g, t).unwrap();
    let predicted = t_base_sensor.adjoint_inv(&v_base);
    assert!(
        rel(predicted, s_sensor.twist) < 1e-9,
        "the static step's transport is not Ad(mount^-1): {:e}",
        rel(predicted, s_sensor.twist)
    );
}

/// **`LerpSlerp` must be refused, not answered** — §2.4, and NORMATIVE.
///
/// The refusal names the edge and the policy, because "derivatives unavailable"
/// on a twelve-edge plan without naming which edge is a diagnostic that costs an
/// hour.
///
/// Mutant: dispatch `LerpSlerp` to `sample_with_twist` anyway ⇒ this returns a
/// plausible twist and the test fails.
#[test]
fn lerpslerp_is_refused_and_names_the_edge() {
    let cfg = EdgeCfg::new(Capacity::slots(64));
    let tree = TreeBuilder::new()
        .default_interp(InterpPolicy::LerpSlerp)
        .dynamic_edge("map", "base", cfg)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let base = tree.frame("base").unwrap();
    let w = tree.claim(base, map).unwrap();
    for i in 0..8i64 {
        w.push(i * 10_000_000, &common::pose(i as u64 + 1)).unwrap();
    }
    let plan = tree.plan(map, base).unwrap();
    let g = tree.guard();

    // The plain lookup still works — this is a derivative-only refusal.
    plan.at(&g, ns(25_000_000)).expect("at() must still work");

    // The edge the plan actually walks, read from the plan rather than assumed —
    // edge ids are allocation order, not something a test should hardcode.
    let want_edge = plan
        .steps()
        .iter()
        .find_map(|s| match s {
            tf_tree::Step::Dyn { edge, .. } => Some(*edge),
            tf_tree::Step::Static(_) => None,
        })
        .expect("the plan has a dynamic edge");

    match plan.at_with_derivatives(&g, ns(25_000_000)) {
        Err(LookupError::DerivativesUnavailable { edge, interp }) => {
            assert_eq!(interp, InterpPolicy::LerpSlerp.as_u8());
            assert_eq!(edge, want_edge, "the offending edge must be named");
        }
        other => panic!("expected DerivativesUnavailable, got {other:?}"),
    }
}

/// A single-sample edge has a pose but no segment, and must say so *specifically*.
///
/// `NoData` would be wrong and actively misleading: there is data, and the caller
/// would go looking for a publisher that is in fact running.
///
/// Mutant: return `NoData` for the one-sample case ⇒ fails.
#[test]
fn one_sample_is_no_segment_not_no_data() {
    let cfg = EdgeCfg::new(Capacity::slots(64));
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "base", cfg)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let base = tree.frame("base").unwrap();
    let w = tree.claim(base, map).unwrap();
    w.push(1_000_000, &common::pose(1)).unwrap();

    let plan = tree.plan(map, base).unwrap();
    let g = tree.guard();
    // The pose is available...
    plan.at(&g, ns(1_000_000)).expect("pose is well defined");
    // ...the derivative is not, and the error says which of the two is missing.
    assert!(
        matches!(
            plan.at_with_derivatives(&g, ns(1_000_000)),
            Err(LookupError::NoSegment { .. })
        ),
        "a single sample must be NoSegment, not NoData"
    );
}

/// Equal stamps are legal (invariant 6) but span no time, so the velocity would
/// be infinite rather than merely unknown. Must be `NoSegment`, never `inf`.
#[test]
fn a_zero_length_segment_is_no_segment_not_infinity() {
    let cfg = EdgeCfg::new(Capacity::slots(64));
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "base", cfg)
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let base = tree.frame("base").unwrap();
    let w = tree.claim(base, map).unwrap();
    w.push(1_000_000, &common::pose(1)).unwrap();
    w.push(1_000_000, &common::pose(2)).unwrap(); // same stamp, legal

    let plan = tree.plan(map, base).unwrap();
    let g = tree.guard();
    match plan.at_with_derivatives(&g, ns(1_000_000)) {
        Err(LookupError::NoSegment { .. }) => {}
        Ok(s) => panic!(
            "a zero-length segment produced a twist: {:?} (finite={})",
            s.twist,
            s.twist.amax().is_finite()
        ),
        other => panic!("expected NoSegment, got {other:?}"),
    }
}

/// `accel` is `None` under ScLerp, and that is a statement about the interpolant.
///
/// `Some(ZERO)` would claim the path is twice differentiable at the knots. It is
/// not: the twist is piecewise-constant, so the acceleration is a train of deltas.
#[test]
fn accel_is_none_because_sclerp_has_no_second_derivative() {
    let c = Chain::new(16, 10_000_000);
    let plan = c.tree.plan(c.map, c.base).unwrap();
    let g = c.tree.guard();
    let s = plan.at_with_derivatives(&g, ns(35_000_000)).unwrap();
    assert!(s.accel.is_none());
}

/// **`to_spatial` round-trips through the returned pose.**
///
/// The pairing between a twist and the pose it was taken at is the thing most
/// easily got wrong by a caller, so the API-level round trip is worth pinning.
#[test]
fn the_spatial_twist_round_trips_through_the_pose() {
    let c = Chain::new(32, 10_000_000);
    let plan = c.tree.plan(c.map, c.base).unwrap();
    let g = c.tree.guard();
    for k in 1..20i64 {
        let s = plan
            .at_with_derivatives(&g, ns(k * 10_000_000 + 4_000_000))
            .unwrap();
        let back = s.pose.adjoint_inv(&s.twist.to_spatial(&s.pose));
        assert!(
            rel(back, s.twist) < 1e-12,
            "Ad(T^-1)Ad(T) did not round-trip at k={k}"
        );
    }
}

/// **§2.4's finding, asserted rather than merely documented.**
///
/// ScLerp's body-frame linear velocity is constant across a segment; LerpSlerp's
/// is not — and the trap is that its *magnitude* is, so a caller sanity-checking
/// `‖v‖` sees nothing wrong while the vector rotates.
///
/// This test computes both numerically from the shipped interpolators, so it is a
/// claim about this implementation and not a restatement of the spec's prose.
#[test]
fn lerpslerp_body_velocity_swings_while_its_magnitude_does_not() {
    use tf_tree::{Interp, LerpSlerp, ScLerp};

    let a = common::pose(3);
    let b = common::pose(9);
    let h = 1e-6;

    // Body-frame linear velocity of an interpolator at parameter s.
    let body_v = |eval: &dyn Fn(&Iso3, &Iso3, f64) -> Iso3, s: f64| {
        let t0 = eval(&a, &b, s);
        let tp = eval(&a, &b, s + h);
        let tm = eval(&a, &b, s - h);
        let d = Vec3::new(
            (tp.t.x - tm.t.x) / (2.0 * h),
            (tp.t.y - tm.t.y) / (2.0 * h),
            (tp.t.z - tm.t.z) / (2.0 * h),
        );
        t0.q.conjugate().rotate(d)
    };

    let ls = |x: &Iso3, y: &Iso3, s: f64| <LerpSlerp as Interp>::eval(x, y, s);
    let sc = |x: &Iso3, y: &Iso3, s: f64| <ScLerp as Interp>::eval(x, y, s);

    let (l0, l1) = (body_v(&ls, 0.05), body_v(&ls, 0.95));
    let (s0, s1) = (body_v(&sc, 0.05), body_v(&sc, 0.95));

    let l_vec = l1.sub(l0).norm();
    let l_mag = (l1.norm() - l0.norm()).abs();
    let s_vec = s1.sub(s0).norm();

    // ScLerp: the vector itself is constant.
    assert!(
        s_vec < 1e-6 * s0.norm().max(1.0),
        "ScLerp body velocity was not constant: {s_vec:e}"
    );
    // LerpSlerp: the vector swings...
    assert!(
        l_vec > 1e-3 * l0.norm(),
        "LerpSlerp body velocity did not swing; the fixture is degenerate"
    );
    // ...but the magnitude does not, which is precisely why a magnitude check
    // cannot catch it. Orders of magnitude apart, not merely different.
    assert!(
        l_mag < 1e-6 * l_vec,
        "the |v| trap did not reproduce: vector spread {l_vec:e}, magnitude spread {l_mag:e}"
    );
}
