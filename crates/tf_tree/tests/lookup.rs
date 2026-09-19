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

/// A three-frame lookup equals the manual chain: the compile direction is right.
#[test]
fn three_frame_lookup_matches_manual_chain() {
    let c = Chain::new(4, 1000);
    let stamp = 0; // exact sample -> no interpolation

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
    {
        let w5 = tree.claim(f[5], f[4]).unwrap();
        w5.push(0, &Iso3::IDENTITY).unwrap();
        let w6 = tree.claim(f[6], f[5]).unwrap();
        w6.push(0, &Iso3::IDENTITY).unwrap();
    }

    // Steps [D,D,S,S,S,S]: the four trailing statics fold into one.
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
                // The shared sub-path is sampled twice and cancels only to float
                // precision.
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

/// #11 + #12: plan evaluation (which folds static edges) equals the unfolded
/// chain composition.
#[test]
fn plan_eval_matches_unfolded_chain() {
    let r = Robot::new(16, 1000);
    let ids = r.frame_ids();

    runner(10_000, 0x43)
        .run(&(0..ids.len(), 0..ids.len(), 0..r.n), |(a, b, i)| {
            let (target, source) = (ids[a], ids[b]);
            // Exact sample stamp: the ground truth uses the pushed poses.
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

/// The two `TreeTooDeep` sentences (`0034`) name the bound that refused.
///
/// The compiled-bound sentence names `TreeBuilder::static_edge` (`docs/API.md`
/// R5: the remedy lives in the binding's prose layer; the Python side is pinned
/// in `tests/python/test_errors.py`).
///
/// Mutant: collapse the arm to a single `path depth {depth} exceeds the maximum`
/// write ⇒ fails.
#[test]
fn the_two_too_deep_messages_name_the_bound_that_refused() {
    fn chain_of(links: usize) -> tf_tree::Tree {
        let names: Vec<String> = (0..=links).map(|i| format!("d{i}")).collect();
        let mut b = TreeBuilder::new();
        for w in names.windows(2) {
            b = b.dynamic_edge(&w[0], &w[1], EdgeCfg::new(Capacity::slots(4)));
        }
        b.build().unwrap()
    }

    // Past the compiled bound, short of the walk's.
    let tree = chain_of(tf_tree::MAX_DEPTH + 8);
    let err = tree
        .plan(
            tree.frame("d0").unwrap(),
            tree.frame(&format!("d{}", tf_tree::MAX_DEPTH + 8)).unwrap(),
        )
        .unwrap_err();
    let msg = tree.describe(err).to_string();
    assert!(
        msg.contains(&format!("compiles to {} steps", tf_tree::MAX_DEPTH + 8))
            && msg.contains(&format!("a plan holds {}", tf_tree::MAX_DEPTH)),
        "the compiled-bound message must name the true step count and the bound, \
         not the bound twice: {msg}"
    );
    assert!(
        msg.contains("TreeBuilder::static_edge"),
        "the Rust prose layer names the remedy a Rust caller can reach: {msg}"
    );

    // Past the walk's bound: no step count is quoted.
    let tree = chain_of(tf_tree::MAX_PATH_EDGES + 8);
    let err = tree
        .plan(
            tree.frame("d0").unwrap(),
            tree.frame(&format!("d{}", tf_tree::MAX_PATH_EDGES + 8))
                .unwrap(),
        )
        .unwrap_err();
    let msg = tree.describe(err).to_string();
    assert!(
        msg.contains(&format!(
            "longer than the {} edges a lookup walks",
            tf_tree::MAX_PATH_EDGES
        )),
        "the walk's message must say 'longer than', because the walk never \
         learns how much longer: {msg}"
    );
    assert!(
        !msg.contains("compiles to"),
        "nothing folded, so no step count may be quoted: {msg}"
    );

    // The seam: exactly `MAX_PATH_EDGES` links are walked and refused by the
    // compiled bound; pins the `>` (not `>=`) comparison in `describe`.
    let tree = chain_of(tf_tree::MAX_PATH_EDGES);
    let err = tree
        .plan(
            tree.frame("d0").unwrap(),
            tree.frame(&format!("d{}", tf_tree::MAX_PATH_EDGES))
                .unwrap(),
        )
        .unwrap_err();
    assert_eq!(
        err,
        tf_tree::LookupError::TreeTooDeep {
            depth: tf_tree::MAX_PATH_EDGES as u16
        },
        "a path of exactly the walk's bound is walked, and refused by the \
         compiled bound with its true folded length"
    );
    let msg = tree.describe(err).to_string();
    assert!(
        msg.contains(&format!("compiles to {} steps", tf_tree::MAX_PATH_EDGES)),
        "at the seam the compiled-bound sentence is the right one: {msg}"
    );
    assert!(
        !msg.contains("edges a lookup walks"),
        "the walk did not refuse this path and must not be blamed for it: {msg}"
    );
}

/// `Described` delegates to `core`'s `Display` rather than `Debug`-printing the
/// variants that carry no frame or edge (`docs/decisions/0040` decision 3).
///
/// Mutant: restore `write!(f, "{other:?}")` ⇒ `BufferTooSmall { need: 32, got: 4 }`
/// reaches the message.
#[test]
fn describe_delegates_rather_than_debug_printing_what_it_cannot_name() {
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(16)))
        .build()
        .unwrap();

    let g = tree.guard();
    let plan = tree
        .plan(tree.frame("map").unwrap(), tree.frame("base").unwrap())
        .unwrap();
    let mut too_small = [0.0f64; 4];
    let err = plan
        .at_many_into::<tf_tree::SystemDomain>(&g, &[0, 1], tf_tree::Layout::Mat4, &mut too_small)
        .unwrap_err();

    let msg = tree.describe(err).to_string();
    assert!(
        !msg.contains("BufferTooSmall"),
        "the wrapper is still Debug-printing what it cannot name: {msg}"
    );
    assert!(
        msg.contains("32") && msg.contains('4'),
        "the delegated message must keep the numbers the variant carries: {msg}"
    );
    // Same sentence `core` writes.
    assert_eq!(msg, format!("{err}"));
}
