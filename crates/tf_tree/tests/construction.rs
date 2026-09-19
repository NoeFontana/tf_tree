//! Construction tests for decision `0004`: the arena is sized from exactly the
//! declared edges, per-edge capacities are honored and independent, and
//! `Capacity::history` rounds to a power of two.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{assert_close, ns, pose};

use tf_tree::{Capacity, EdgeCfg, Iso3, LookupError, TreeBuilder};

const TOL: f64 = 1e-12;

/// A mostly-static tree has an arena sized by its two dynamic rings only (`0004`).
#[test]
fn sparse_tree_arena_tracks_only_dynamic_edges() {
    const STATIC: usize = 200;
    let dyn_cap: u32 = 4096;

    let mut builder = TreeBuilder::new();
    for i in 1..=STATIC {
        builder = builder.static_edge(&format!("s{}", i - 1), &format!("s{i}"), &Iso3::IDENTITY);
    }
    let tree = builder
        .dynamic_edge(
            &format!("s{STATIC}"),
            "d_a",
            EdgeCfg::new(Capacity::slots(dyn_cap)),
        )
        .dynamic_edge("d_a", "d_b", EdgeCfg::new(Capacity::slots(dyn_cap)))
        .build()
        .unwrap();

    let actual = tree.arena_size_bytes();

    // Uniform reservation would be (STATIC + 2 + 1) * dyn_cap * 64 B.
    let uniform_pose_bytes = (STATIC + 3) * dyn_cap as usize * 64;
    assert!(
        actual * 4 < uniform_pose_bytes,
        "sparse arena {actual} B is not far below the uniform {uniform_pose_bytes} B"
    );

    let two_rings = 2 * dyn_cap as usize * (64 + 8);
    assert!(
        actual > two_rings,
        "arena {actual} B must cover two {dyn_cap}-slot rings ({two_rings} B)"
    );
}

/// A per-edge capacity far larger than its sibling's (8192 vs 16) is honored
/// across a wrapped ring.
#[test]
fn large_capacity_edge_samples_across_a_wrapped_ring() {
    let big: u32 = 8192;
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(big)))
        .dynamic_edge("map", "aux", EdgeCfg::new(Capacity::slots(16)))
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();

    let dt = 1000i64;
    let total = big as usize + 500; // > capacity, so the ring wraps
    {
        let w = tree.claim(odom, map).unwrap();
        for i in 0..total {
            w.push(i as i64 * dt, &pose(i as u64 + 1)).unwrap();
        }
    }

    let recent = ns((total - 1) as i64 * dt);
    assert_close(
        tree.lookup("map", "odom", recent).unwrap(),
        pose(total as u64),
        TOL,
        "newest sample after wrap",
    );

    // Retained window: indices [500, 8691]; 550 is retained, 450 overwritten.
    let retained = ns(550 * dt);
    assert!(
        tree.lookup("map", "odom", retained).is_ok(),
        "index 550 must still be retained (proves ~8192-slot capacity)"
    );
    let evicted = ns(450 * dt);
    let err = tree.lookup("map", "odom", evicted).unwrap_err();
    assert!(
        matches!(err, LookupError::Extrapolation { .. }),
        "index 450 must be evicted, got {err:?}"
    );
}

/// Two edges of different capacities each sample their own ring (guards the
/// cumulative `stamp_off`/`pose_off` math).
#[test]
fn distinct_capacity_edges_sample_their_own_rings() {
    let cap_a: u32 = 8192; // map -> odom (no wrap)
    let cap_b: u32 = 64; //   odom -> base (wraps)
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(cap_a)))
        .dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(cap_b)))
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let base = tree.frame("base").unwrap();

    let dt = 1000i64;
    let n_a = 200usize; //           < cap_a: no wrap on A
    let n_b = cap_b as usize + 40; // > cap_b: B wraps, retaining [40, 103]
    let a_poses: Vec<Iso3> = (0..n_a).map(|i| pose(i as u64 + 1)).collect();
    let b_poses: Vec<Iso3> = (0..n_b).map(|i| pose(i as u64 + 500)).collect();
    {
        let wa = tree.claim(odom, map).unwrap();
        for (i, p) in a_poses.iter().enumerate() {
            wa.push(i as i64 * dt, p).unwrap();
        }
        let wb = tree.claim(base, odom).unwrap();
        for (i, p) in b_poses.iter().enumerate() {
            wb.push(i as i64 * dt, p).unwrap();
        }
    }

    for i in [50usize, 70, 103] {
        let s = ns(i as i64 * dt);
        assert_close(
            tree.lookup("map", "odom", s).unwrap(),
            a_poses[i],
            TOL,
            "edge A samples its own ring",
        );
        assert_close(
            tree.lookup("odom", "base", s).unwrap(),
            b_poses[i],
            TOL,
            "edge B samples its own ring",
        );
    }

    // The rings are independent, not aliased through a shared offset.
    let old = ns(10 * dt);
    assert_close(
        tree.lookup("map", "odom", old).unwrap(),
        a_poses[10],
        TOL,
        "A still retains index 10",
    );
    let err = tree.lookup("odom", "base", old).unwrap_err();
    assert!(
        matches!(err, LookupError::Extrapolation { .. }),
        "B evicted index 10, got {err:?}"
    );
}

/// `Capacity::history(1000 Hz, 10 s)` -> 16384; `Capacity::slots` rounds up to a power of two.
#[test]
fn capacity_rounds_to_power_of_two() {
    assert_eq!(Capacity::history(1000.0, 10.0).get(), 16384);
    assert_eq!(Capacity::history(50.0, 10.0).get(), 512); // next_pow2(500)
    assert_eq!(Capacity::slots(1).get(), 1);
    assert_eq!(Capacity::slots(3).get(), 4);
    assert_eq!(Capacity::slots(4096).get(), 4096);
    assert_eq!(Capacity::slots(4097).get(), 8192);
}

/// A declared nominal rate reaches `EdgeRecord::nominal_rate_mhz`; an undeclared
/// edge leaves it 0 (`docs/PHASE5.md` §1.2, §6). 19.9999 Hz separates `round`
/// from `as`. Gated on `unstable`: read back through `ArenaView`.
#[test]
#[cfg(feature = "unstable")]
fn a_declared_nominal_rate_reaches_the_edge_record() {
    let tree = TreeBuilder::new()
        .dynamic_edge(
            "map",
            "odom",
            EdgeCfg::new(Capacity::slots(64)).nominal_rate_hz(19.79),
        )
        .dynamic_edge(
            "odom",
            "base",
            EdgeCfg::new(Capacity::slots(64)).nominal_rate_hz(0.1),
        )
        .dynamic_edge("base", "laser", EdgeCfg::new(Capacity::slots(64)))
        // An implausible rate drops to "undeclared" rather than being clamped.
        .dynamic_edge(
            "base",
            "imu",
            EdgeCfg::new(Capacity::slots(64)).nominal_rate_hz(f64::INFINITY),
        )
        .dynamic_edge(
            "base",
            "gps",
            EdgeCfg::new(Capacity::slots(64)).nominal_rate_hz(-5.0),
        )
        .dynamic_edge(
            "base",
            "wheel",
            EdgeCfg::new(Capacity::slots(64)).nominal_rate_hz(19.9999),
        )
        .build()
        .unwrap();

    let view = tree.arena_view();
    let mhz = |id: u32| view.edge(tf_tree::EdgeId(id)).unwrap().nominal_rate_mhz;
    assert_eq!(mhz(1), 19_790, "19.79 Hz in milli-hertz");
    assert_eq!(
        mhz(2),
        100,
        "0.1 Hz is 100 mHz, and is a real map-update rate"
    );
    assert_eq!(mhz(3), 0, "an edge sized by slots declares no rate");
    assert_eq!(mhz(4), 0, "an infinite rate is not a declaration");
    assert_eq!(mhz(5), 0, "a negative rate is not a declaration");
    assert_eq!(
        mhz(6),
        20_000,
        "19.9999 Hz rounds to 20 000 mHz; truncating would store 19 999"
    );

    // A static edge has no rate; `EdgeRecord::static_edge` is a separate constructor.
    let tree = TreeBuilder::new()
        .static_edge("map", "odom", &Iso3::IDENTITY)
        .build()
        .unwrap();
    assert_eq!(
        tree.arena_view()
            .edge(tf_tree::EdgeId(1))
            .unwrap()
            .nominal_rate_mhz,
        0
    );
}
