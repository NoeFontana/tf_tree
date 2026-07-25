//! Public-surface lookup tests: compile direction, static folding, and the
//! `docs/PHASE1.md` §10.1 lookup proptests (#4, #5, #10, #11, #12).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{assert_close, max_err, ns, Chain, Robot};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

use tf_tree::{Capacity, EdgeCfg, Iso3, TreeBuilder};

const TOL: f64 = 1e-12;

fn runner(cases: u32, seed: u8) -> TestRunner {
    TestRunner::new_with_rng(
        Config {
            cases,
            failure_persistence: None,
            ..Config::default()
        },
        TestRng::from_seed(RngAlgorithm::ChaCha, &[seed; 32]),
    )
}

/// Hand-computed three-frame lookup (`map -> odom -> base`) equals the manual
/// chain, verifying the compile direction is not inverted.
#[test]
fn three_frame_lookup_matches_manual_chain() {
    let c = Chain::new(4, 1000);
    let stamp = 0; // exact sample -> no interpolation

    // Manual: T_map_base = T_map_odom · T_odom_base.
    let t_map_base = c.map_odom[0] * c.odom_base[0];

    let got = c.tree.lookup("base", "map", ns(stamp)).unwrap();
    // lookup(base, map) = T_base_map = (T_map_base)⁻¹.
    assert_close(got, t_map_base.inverse(), TOL, "lookup(base,map)");

    let fwd = c.tree.lookup("map", "base", ns(stamp)).unwrap();
    assert_close(fwd, t_map_base, TOL, "lookup(map,base)");
}

/// A depth-6 chain with four contiguous static edges folds from 6 steps to 3.
#[test]
fn urdf_like_chain_folds_six_to_three() {
    // root=f0 -> f1(S) -> f2(S) -> f3(S) -> f4(S) -> f5(D) -> f6(D)
    let cfg = EdgeCfg::new(Capacity::slots(16));
    let mut builder = TreeBuilder::new();
    for i in 1..=4 {
        builder = builder.static_edge(&format!("f{}", i - 1), &format!("f{i}"), &Iso3::IDENTITY);
    }
    let tree = builder
        .dynamic_edge("f4", "f5", cfg)
        .dynamic_edge("f5", "f6", cfg)
        .build()
        .unwrap();

    let f: Vec<_> = (0..=6)
        .map(|i| tree.frame(&format!("f{i}")).unwrap())
        .collect();
    // Publish so the dynamic edges have data (not needed for compilation).
    {
        let w5 = tree.claim(f[5], f[4]).unwrap();
        w5.push(0, &Iso3::IDENTITY).unwrap();
        let w6 = tree.claim(f[6], f[5]).unwrap();
        w6.push(0, &Iso3::IDENTITY).unwrap();
    }

    // lookup(f6, f0): steps emitted as [D(f6),D(f5),S(f4),S(f3),S(f2),S(f1)],
    // folding the four trailing statics into one -> [D, D, S] = 3 steps.
    let plan = tree.plan(f[6], f[0]).unwrap();
    assert_eq!(
        plan.len(),
        6 - 3,
        "expected fold 6 -> 3, got {}",
        plan.len()
    );
}

/// #5: `lookup(X, X, t) == IDENTITY` exactly (empty plan).
#[test]
fn lookup_self_is_identity_exactly() {
    let r = Robot::new(8, 1000);
    let ids = r.frame_ids();
    let names = ["map", "odom", "base", "imu", "lidar", "cam_opt"];
    for nm in names {
        let got = r.tree.lookup(nm, nm, ns(3000)).unwrap();
        assert_eq!(got, Iso3::IDENTITY, "lookup({nm},{nm})");
    }
    let _ = ids;
}

/// #4: `lookup(X, Y, t) ≈ lookup(Y, X, t).inverse()`.
#[test]
fn lookup_is_antisymmetric() {
    let r = Robot::new(16, 1000);
    let ids = r.frame_ids();
    let names: Vec<&str> = ["map", "odom", "base", "imu", "lidar", "cam", "cam_opt"].to_vec();
    let max_t = (r.n as i64 - 1) * r.dt;

    runner(10_000, 0x41)
        .run(&(0..names.len(), 0..names.len(), 0..=max_t), |(a, b, t)| {
            let (x, y) = (names[a], names[b]);
            let fwd = r.tree.lookup(x, y, ns(t)).unwrap();
            let rev = r.tree.lookup(y, x, ns(t)).unwrap();
            prop_assert!(
                max_err(fwd, rev.inverse()) <= TOL,
                "lookup({x},{y}) vs lookup({y},{x})^-1 at t={t}: err {:e}",
                max_err(fwd, rev.inverse())
            );
            Ok(())
        })
        .unwrap();
    let _ = ids;
}

/// #10: `lookup(A, C) ≈ lookup(A, B) · lookup(B, C)` for any B.
#[test]
fn lookup_composes_through_any_frame() {
    let r = Robot::new(16, 1000);
    let names: Vec<&str> = ["map", "odom", "base", "imu", "lidar", "cam", "cam_opt"].to_vec();
    let max_t = (r.n as i64 - 1) * r.dt;

    runner(10_000, 0x42)
        .run(
            &(0..names.len(), 0..names.len(), 0..names.len(), 0..=max_t),
            |(a, b, c, t)| {
                let (fa, fb, fc) = (names[a], names[b], names[c]);
                let s = ns(t);
                let ac = r.tree.lookup(fa, fc, s).unwrap();
                let ab = r.tree.lookup(fa, fb, s).unwrap();
                let bc = r.tree.lookup(fb, fc, s).unwrap();
                let composed = ab * bc;
                // Composing two independently-sampled interpolated chains through an
                // arbitrary intermediate frame accumulates a few ulps × scale (the
                // shared sub-path is sampled twice and cancels only to float
                // precision). 5e-12 stays far tighter than any real topology bug.
                const COMPOSE_TOL: f64 = 5e-12;
                prop_assert!(
                    max_err(ac, composed) <= COMPOSE_TOL,
                    "lookup({fa},{fc}) vs lookup({fa},{fb})*lookup({fb},{fc}) at t={t}: err {:e}",
                    max_err(ac, composed)
                );
                Ok(())
            },
        )
        .unwrap();
}

/// #11 + #12: plan evaluation (which always folds static edges) equals the naive
/// unfolded chain composition. Because the fixture mixes static and dynamic
/// edges, agreement proves folding does not change the result.
#[test]
fn plan_eval_matches_unfolded_chain() {
    let r = Robot::new(16, 1000);
    let ids = r.frame_ids();

    runner(10_000, 0x43)
        .run(&(0..ids.len(), 0..ids.len(), 0..r.n), |(a, b, i)| {
            let (target, source) = (ids[a], ids[b]);
            // Exact sample stamp so temporal sampling returns the pushed pose and
            // the ground truth uses the same known poses.
            let t = i as i64 * r.dt;
            let plan = r.tree.plan(target, source).unwrap();
            let g = r.tree.guard();
            let got = plan.at(&g, ns(t)).unwrap();
            let want = r.transform(target, source, i);
            prop_assert!(
                max_err(got, want) <= TOL,
                "plan vs manual, target={a} source={b} i={i}: err {:e}",
                max_err(got, want)
            );
            Ok(())
        })
        .unwrap();
}
