//! Construction tests for decision `0004`
//! (`docs/decisions/0004-builder-time-edge-declaration.md`, still authoritative
//! for the builder-time edge declaration API): the arena is sized from exactly the
//! declared edges, per-edge capacities are honored (including one far larger than
//! its siblings), edges of different capacities each sample their own ring (which
//! guards the cumulative `stamp_off`/`pose_off` math), and `Capacity::history`
//! rounds to a power of two.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{assert_close, ns, pose};

use tf_tree::{Capacity, EdgeCfg, Iso3, LookupError, TreeBuilder};

const TOL: f64 = 1e-12;

/// A tree that is mostly static (a long static chain) plus a couple of dynamic
/// edges has an arena whose size tracks only the two dynamic rings — far smaller
/// than a uniform per-edge reservation would give (the waste `0004` warns about).
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

    // Uniform reservation would size the pose arena for every edge slot at
    // `dyn_cap`: (STATIC + 2 dynamic + 1 sentinel) * dyn_cap * 64 B. Sparse
    // sizing reserves rings for only the two dynamic edges.
    let uniform_pose_bytes = (STATIC + 3) * dyn_cap as usize * 64;
    assert!(
        actual * 4 < uniform_pose_bytes,
        "sparse arena {actual} B is not far below the uniform {uniform_pose_bytes} B"
    );

    // But it does hold the two dynamic rings (pose+stamp per slot).
    let two_rings = 2 * dyn_cap as usize * (64 + 8);
    assert!(
        actual > two_rings,
        "arena {actual} B must cover two {dyn_cap}-slot rings ({two_rings} B)"
    );
}

/// A per-edge capacity far larger than its sibling's (8192 vs 16) is honored: the
/// big edge retains ~8192 samples and samples correctly across a wrapped ring,
/// which a 16-slot reservation could never do.
#[test]
fn large_capacity_edge_samples_across_a_wrapped_ring() {
    let big: u32 = 8192;
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(big)))
        // A small sibling so the big edge is not the only ring in the arena.
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

    // The newest sample reads back exactly (single-edge, exact stamp).
    let recent = ns((total - 1) as i64 * dt);
    assert_close(
        tree.lookup("map", "odom", recent).unwrap(),
        pose(total as u64),
        TOL,
        "newest sample after wrap",
    );

    // The retained window is the last `big` samples: indices [total-big, total-1]
    // = [500, 8691]. Index 550 is retained; index 450 was overwritten.
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

/// Two dynamic edges of different capacities each sample their own ring. A wrong
/// cumulative `stamp_off`/`pose_off` would make one edge read into the other's
/// ring; distinct pose streams make that visible.
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

    // Where both rings retain the index, each edge returns its OWN pose stream.
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

    // The big edge retains a sample the small edge has long evicted — the two
    // rings are independent, not aliased through a shared offset.
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

/// `Capacity::history(1000 Hz, 10 s)` -> `next_pow2(10_000)` == 16384, and
/// `Capacity::slots` rounds up to the next power of two.
#[test]
fn capacity_rounds_to_power_of_two() {
    assert_eq!(Capacity::history(1000.0, 10.0).get(), 16384);
    assert_eq!(Capacity::history(50.0, 10.0).get(), 512); // next_pow2(500)
    assert_eq!(Capacity::slots(1).get(), 1);
    assert_eq!(Capacity::slots(3).get(), 4);
    assert_eq!(Capacity::slots(4096).get(), 4096);
    assert_eq!(Capacity::slots(4097).get(), 8192);
}

/// **A declared nominal rate reaches `EdgeRecord::nominal_rate_mhz`, and an
/// undeclared edge leaves it 0.**
///
/// The field is the *only* evidence `tf_tree doctor`'s `TFT007` has that an
/// observed rate is wrong rather than merely what it is (`docs/PHASE5.md` §1.2,
/// §6). Declaration is also the only moment it can be written: after `build()`
/// the arena may be shared, and no API mutates a record afterwards.
///
/// The values are chosen so the units cannot pass by accident. 19.79 Hz is what
/// `tf_tree topology --discover` emits for a 20 Hz publisher (it measures and
/// rounds up to two decimals), so hertz-rounding would store 20 000 mHz and
/// milli-hertz truncation would store 19 790 either way; 0.1 Hz is a map update,
/// which an integer-hertz field could not express at all. **19.9999 Hz is the
/// value that separates `round` from `as`** — the conversion's own comment calls
/// the distinction load-bearing, and neither 19.79 nor 0.1 discriminates it,
/// because `19.79 * 1000.0` and `0.1 * 1000.0` are both exact in `f64`.
///
/// Mutant: delete `record.nominal_rate_mhz = cfg.nominal_rate_mhz;` from
/// `TreeBuilder::build_with`. Applied: the first assertion fails with
/// `left: 0, right: 19790`.
/// Mutant B: `mhz.round() as u32` -> `mhz as u32` in `EdgeCfg::nominal_rate_hz`.
/// Applied: the 19.9999 Hz assertion fails with `left: 19999, right: 20000` —
/// an edge the operator declared at 20 Hz, off by a milli-hertz forever.
#[test]
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
        // Declares nothing: sized by slots, so there is no rate to record.
        .dynamic_edge("base", "laser", EdgeCfg::new(Capacity::slots(64)))
        // A rate no robot publishes at is dropped back to "undeclared" rather
        // than clamped — a clamp would invent a nominal out of a typo and then
        // report every real sample as deviating from it.
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
        // A 20 Hz rate that came back from a text round-trip a whisker short.
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

    // A static edge never publishes, so it has no rate and the builder writes
    // none — checked because `EdgeRecord::static_edge` and `::dynamic` are
    // separate constructors and only one of them is on the path above.
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
