//! `Plan::at_with_derivatives` (`docs/PHASE4.md` §2). Claims: the pose is
//! bit-identical to `Plan::at`; the twist matches a central difference of the
//! shipped `at()`; the §2.3 composition identities hold; `LerpSlerp` is
//! refused (§2.4).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{ns, Chain};
use tf_tree::{
    exp_se3, Capacity, EdgeCfg, FrameId, InterpPolicy, Iso3, LookupError, Tree, TreeBuilder, Twist,
    Vec3,
};

/// A rig whose adjoint is observable: non-commuting screw axes on two dynamic
/// edges plus a non-trivial static edge, unlike `common::Chain` (one
/// one-parameter subgroup, where `Ad` acts as the identity).
struct Rig {
    tree: Tree,
    map: FrameId,
    sensor: FrameId,
}

impl Rig {
    const DT: i64 = 10_000_000;

    fn new() -> Rig {
        // Non-parallel, non-commuting generators.
        let a = [0.00, 0.00, 0.05, 0.40, 0.00, 0.00];
        let b = [0.06, 0.01, 0.00, 0.00, 0.30, -0.10];
        let cfg = EdgeCfg::new(Capacity::slots(256));
        // A static edge with rotation and translation, to test the adjoint's rotational block.
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
            // Quadratic in the algebra, so the twist varies segment to segment.
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

/// Central difference of the shipped `plan.at`, as a body twist `(T⁻¹ Ṫ)^∨`.
fn central_difference(at: &dyn Fn(i64) -> Iso3, t_ns: i64, h_ns: i64) -> Twist {
    let (tm, tp, t0) = (at(t_ns - h_ns), at(t_ns + h_ns), at(t_ns));
    let h = (h_ns as f64) * 1e-9;

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
    let mut w = [[0.0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            w[i][j] = (0..3).map(|k| r0[k][i] * rdot[k][j]).sum();
        }
    }
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

/// The pose is bit-identical to `at`.
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

/// The analytic twist matches a central difference of `at`, asserted by
/// convergence order (~4 on halving `h`), since a composed path carries real
/// truncation error.
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

/// The twist is constant within a segment (§2.3).
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
        // Not bit-equal: the composed twist varies within a segment.
        assert!(
            rel(a, b) < 1e-2,
            "composed twist swung within segment {k}: {:e}",
            rel(a, b)
        );
    }
}

/// The twist is computed per segment (non-vacuity for the test above): this
/// fixture accelerates, sample `i` at `exp_se3(i²·ξ/64)`.
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

    // Every consecutive pair of segments differs; speed increases monotonically.
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

/// The adjoint fold, on a rig where the adjoint is not the identity (§2.3), in
/// both plan directions and across a static edge; asserted by convergence order.
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

/// A static step between two dynamic edges still transports the twist.
#[test]
fn a_static_step_transports_the_twist() {
    let rig = Rig::new();
    let base = rig.tree.frame("base").unwrap();
    // `map->sensor` continues through the constant mount; the twists differ by its adjoint.
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

    // V_sensor = Ad(T_base_sensor⁻¹)·V_base.
    let t_base_sensor = p_base.at(&g, t).unwrap().inverse() * p_sensor.at(&g, t).unwrap();
    let predicted = t_base_sensor.adjoint_inv(&v_base);
    assert!(
        rel(predicted, s_sensor.twist) < 1e-9,
        "the static step's transport is not Ad(mount^-1): {:e}",
        rel(predicted, s_sensor.twist)
    );
}

/// `LerpSlerp` is refused, not answered (§2.4, NORMATIVE); the error names the
/// edge and the policy.
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

    plan.at(&g, ns(25_000_000)).expect("at() must still work");

    // Read the edge from the plan; ids are allocation order.
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

/// `Layout::QuatTwist` inherits the refusal (`docs/API.md` §3.3,
/// `docs/PHASE5.md` §4.4, NORMATIVE), and the buffer is left untouched
/// (`PHASE3.md` §5.3).
#[test]
fn the_quat_twist_layout_refuses_lerpslerp_exactly_as_the_scalar_call_does() {
    use tf_tree::{Layout, Stamp, SystemDomain};

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

    let want_edge = plan
        .steps()
        .iter()
        .find_map(|s| match s {
            tf_tree::Step::Dyn { edge, .. } => Some(*edge),
            tf_tree::Step::Static(_) => None,
        })
        .expect("the plan has a dynamic edge");

    let stamps = [25_000_000i64, 35_000_000];

    // Pose-only layouts still work, so the refusal is specific to the derivative.
    let mut poses = vec![0.0f64; stamps.len() * Layout::Quat.elems()];
    plan.at_many_into::<SystemDomain>(&g, &stamps, Layout::Quat, &mut poses)
        .expect("the pose layout must still work over a LerpSlerp edge");

    const SENTINEL: f64 = -12345.5;
    let mut rows = vec![SENTINEL; stamps.len() * Layout::QuatTwist.elems()];
    match plan.at_many_into::<SystemDomain>(&g, &stamps, Layout::QuatTwist, &mut rows) {
        Err(LookupError::DerivativesUnavailable { edge, interp }) => {
            assert_eq!(interp, InterpPolicy::LerpSlerp.as_u8());
            assert_eq!(edge, want_edge, "the offending edge must be named");
        }
        other => panic!("expected DerivativesUnavailable, got {other:?}"),
    }
    assert!(
        rows.iter().all(|v| *v == SENTINEL),
        "a refused QuatTwist batch wrote into the caller's buffer"
    );

    // The scalar call refuses identically.
    assert_eq!(
        plan.at_with_derivatives(&g, Stamp::<SystemDomain>::from_nanos(stamps[0]))
            .unwrap_err(),
        LookupError::DerivativesUnavailable {
            edge: want_edge,
            interp: InterpPolicy::LerpSlerp.as_u8(),
        }
    );
}

/// A single-sample edge has a pose but no segment and says so (`NoSegment`, not `NoData`).
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
    plan.at(&g, ns(1_000_000)).expect("pose is well defined");
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

/// `accel` is `None` under ScLerp: the twist is piecewise-constant.
#[test]
fn accel_is_none_because_sclerp_has_no_second_derivative() {
    let c = Chain::new(16, 10_000_000);
    let plan = c.tree.plan(c.map, c.base).unwrap();
    let g = c.tree.guard();
    let s = plan.at_with_derivatives(&g, ns(35_000_000)).unwrap();
    assert!(s.accel.is_none());
}

/// `to_spatial` round-trips through the returned pose.
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

/// §2.4's finding: ScLerp's body-frame linear velocity is constant across a
/// segment; LerpSlerp's rotates while its magnitude does not (computed from the
/// shipped interpolators).
#[test]
fn lerpslerp_body_velocity_swings_while_its_magnitude_does_not() {
    use tf_tree::{Interp, LerpSlerp, ScLerp};

    let a = common::pose(3);
    let b = common::pose(9);
    let h = 1e-6;

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
    // ...but the magnitude does not, so a magnitude check cannot catch it.
    assert!(
        l_mag < 1e-6 * l_vec,
        "the |v| trap did not reproduce: vector spread {l_vec:e}, magnitude spread {l_mag:e}"
    );
}

/// A plan where only some edges are LerpSlerp is refused, naming that edge.
#[test]
fn a_single_lerpslerp_edge_mid_chain_refuses_and_names_that_edge() {
    let cfg = EdgeCfg::new(Capacity::slots(64));
    let tree = TreeBuilder::new()
        .default_interp(InterpPolicy::ScLerp)
        .dynamic_edge("map", "odom", cfg)
        .dynamic_edge("odom", "base", cfg.interp(InterpPolicy::LerpSlerp))
        .dynamic_edge("base", "tool", cfg)
        .build()
        .unwrap();
    let names = ["map", "odom", "base", "tool"];
    for i in 0..3 {
        let parent = tree.frame(names[i]).unwrap();
        let child = tree.frame(names[i + 1]).unwrap();
        let w = tree.claim(child, parent).unwrap();
        for j in 0..8i64 {
            w.push(
                j * 10_000_000,
                &common::pose((i * 10 + j as usize) as u64 + 1),
            )
            .unwrap();
        }
        core::mem::forget(w);
    }
    let map = tree.frame("map").unwrap();
    let tool = tree.frame("tool").unwrap();
    let plan = tree.plan(map, tool).unwrap();
    let g = tree.guard();
    let t = ns(35_000_000);

    plan.at(&g, t).expect("at() must still work");

    // The middle dynamic step is the middle edge.
    let dyn_edges: Vec<_> = plan
        .steps()
        .iter()
        .filter_map(|s| match s {
            tf_tree::Step::Dyn { edge, .. } => Some(*edge),
            tf_tree::Step::Static(_) => None,
        })
        .collect();
    assert_eq!(
        dyn_edges.len(),
        3,
        "the fixture should have three dynamic edges"
    );
    let want = dyn_edges[1];

    match plan.at_with_derivatives(&g, t) {
        Err(LookupError::DerivativesUnavailable { edge, interp }) => {
            assert_eq!(interp, InterpPolicy::LerpSlerp.as_u8());
            assert_eq!(
                edge, want,
                "the refusal named the wrong edge; only one of the three is LerpSlerp"
            );
        }
        other => panic!("expected DerivativesUnavailable, got {other:?}"),
    }
}

/// A stale plan is refused before any sampling.
#[test]
fn at_with_derivatives_refuses_a_stale_plan() {
    let rig = Rig::new();
    let plan = rig.tree.plan(rig.map, rig.sensor).unwrap();
    let base = rig.tree.frame("base").unwrap();
    let map = rig.tree.frame("map").unwrap();
    rig.tree.reparent(rig.sensor, map).expect("reparent");
    let g = rig.tree.guard();
    let _ = base;
    assert!(
        matches!(
            plan.at_with_derivatives(&g, ns(25 * Rig::DT)),
            Err(LookupError::TopologyChanged { .. })
        ),
        "a plan compiled against the old topology must be refused"
    );
}

/// A cross-domain query is refused, as `at` refuses it.
#[test]
fn at_with_derivatives_refuses_a_cross_domain_stamp() {
    use tf_tree::{SensorDomain, Stamp};
    let rig = Rig::new();
    let plan = rig.tree.plan(rig.map, rig.sensor).unwrap();
    let g = rig.tree.guard();
    let t = Stamp::<SensorDomain>::from_nanos(25 * Rig::DT);
    assert!(
        matches!(
            plan.at_with_derivatives(&g, t),
            Err(LookupError::TimeDomainMismatch { .. })
        ),
        "a sensor-domain stamp must not address system-domain edges"
    );
}

/// The twist is in the plan's SOURCE frame, not its target frame: `base` is
/// rotated +90° about z and moves along map's +x at 1 m/s, so the source-frame
/// twist is `(0, −1, 0)` and `to_spatial` gives `(1, 0, 0)`.
#[test]
fn the_twist_is_in_the_source_frame_not_the_target_frame() {
    use tf_tree::Quat;

    let tree = TreeBuilder::new()
        .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64)))
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let base = tree.frame("base").unwrap();
    let w = tree.claim(base, map).unwrap();
    let h = core::f64::consts::FRAC_PI_4; // half of 90°
    let q = Quat::new(h.cos(), 0.0, 0.0, h.sin());
    for i in 0..8i64 {
        // T_map_base(t) = (Rz(+90°), (t, 0, 0)) with t in seconds.
        w.push(
            i * 1_000_000_000,
            &Iso3::new(q, Vec3::new(i as f64, 0.0, 0.0)),
        )
        .unwrap();
    }
    core::mem::forget(w);

    let plan = tree.plan(map, base).unwrap();
    let g = tree.guard();
    let s = plan.at_with_derivatives(&g, ns(3_500_000_000)).unwrap();

    let body = s.twist.v;
    let spatial = s.twist.to_spatial(&s.pose).v;

    // Equal magnitudes, so a magnitude check cannot catch a frame mix-up.
    assert!(
        (body.norm() - 1.0).abs() < 1e-9,
        "body speed should be 1 m/s"
    );
    assert!(
        (spatial.norm() - 1.0).abs() < 1e-9,
        "spatial speed should be 1 m/s"
    );

    // The body twist is resolved in BASE (the source), so +x of map is −y of base.
    assert!(
        body.sub(Vec3::new(0.0, -1.0, 0.0)).norm() < 1e-9,
        "body twist is not in the source frame: {body:?}"
    );
    // to_spatial brings it to MAP (the target).
    assert!(
        spatial.sub(Vec3::new(1.0, 0.0, 0.0)).norm() < 1e-9,
        "to_spatial did not land in the target frame: {spatial:?}"
    );
}
