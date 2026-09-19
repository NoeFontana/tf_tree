//! The diagnostic counters, end to end (`docs/PHASE5.md` §5). Gated on
//! `unstable`: counters are observable only through `Tree::arena_view`'s
//! `edge_counters`.
#![cfg(feature = "unstable")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::Ordering::Relaxed;

use tf_tree::{Capacity, EdgeCfg, Stamp, SystemDomain, Tree, TreeBuilder};

const MS: i64 = 1_000_000;

/// `map -> odom -> base`, both dynamic, 64 samples 10 ms apart.
struct Fixture {
    tree: Tree,
}

impl Fixture {
    fn new() -> Fixture {
        let cfg = EdgeCfg::new(Capacity::slots(128));
        let tree = TreeBuilder::new()
            .dynamic_edge("map", "odom", cfg)
            .dynamic_edge("odom", "base", cfg)
            .frame_headroom(4)
            .build()
            .unwrap();
        for (parent, child) in [("map", "odom"), ("odom", "base")] {
            let p = tree.frame(parent).unwrap();
            let c = tree.frame(child).unwrap();
            let w = tree.claim(c, p).unwrap();
            for i in 0..64i64 {
                w.push(
                    i * 10 * MS,
                    &tf_tree::exp_se3([0.0, 0.0, 0.01 * i as f64, 0.0, 0.0, 0.0]),
                )
                .unwrap();
            }
            core::mem::forget(w);
        }
        Fixture { tree }
    }

    /// The edge attaching `child`, read from the topology block (`edge_of_child`).
    fn edge(&self, child: &str) -> tf_tree::EdgeId {
        let c = self.tree.frame(child).unwrap();
        let view = self.tree.arena_view();
        let (_parent, _depth, edge, _gen) = view
            .topology()
            .read_frame(c)
            .expect("the child is in the topology");
        tf_tree::EdgeId(edge)
    }

    fn ok_count(&self, edge: tf_tree::EdgeId) -> u64 {
        self.tree
            .arena_view()
            .edge_counters(edge)
            .expect("edge in range")
            .lookups_ok
            .load(Relaxed)
    }
}

/// The denominator is flushed once per guard, not per lookup (§5.4); the
/// in-scope assertion is the one that rejects a write-through `Guard::note_ok`.
#[test]
fn the_success_count_accumulates_in_the_guard_and_flushes_on_drop() {
    let f = Fixture::new();
    let e = f.edge("odom");
    let src = f.tree.frame("map").unwrap();
    let dst = f.tree.frame("odom").unwrap();
    let plan = f.tree.plan(src, dst).unwrap();

    {
        let g = f.tree.guard();
        for i in 0..50i64 {
            plan.at(&g, Stamp::<SystemDomain>::from_nanos(i * 10 * MS))
                .unwrap();
        }
        assert_eq!(
            f.ok_count(e),
            0,
            "the count must live in the guard until it drops"
        );
    }
    assert_eq!(f.ok_count(e), 50, "and reach the arena exactly once");

    {
        let g = f.tree.guard();
        plan.at(&g, Stamp::<SystemDomain>::from_nanos(0)).unwrap();
    }
    assert_eq!(f.ok_count(e), 51);
}

/// Extrapolation is split by direction, and the worst gap is a high-water mark
/// (`TFT010`/`TFT011` key off the split).
#[test]
fn extrapolation_is_split_by_direction_and_keeps_the_worst_gap() {
    let f = Fixture::new();
    let e = f.edge("odom");
    let src = f.tree.frame("map").unwrap();
    let dst = f.tree.frame("odom").unwrap();
    let plan = f.tree.plan(src, dst).unwrap();

    {
        let g = f.tree.guard();
        assert!(plan
            .at(&g, Stamp::<SystemDomain>::from_nanos(10_000 * MS))
            .is_err());
        assert!(plan
            .at(&g, Stamp::<SystemDomain>::from_nanos(-5_000 * MS))
            .is_err());
    }

    let view = f.tree.arena_view();
    let c = view.edge_counters(e).unwrap();
    assert_eq!(c.err_extrap_after.load(Relaxed), 1, "past the newest");
    assert_eq!(c.err_extrap_before.load(Relaxed), 1, "before the oldest");
    assert!(
        c.worst_extrap_gap_ns.load(Relaxed) >= 5_000 * MS,
        "the worst gap is a high-water mark, got {}",
        c.worst_extrap_gap_ns.load(Relaxed)
    );
    {
        let g = f.tree.guard();
        assert!(plan
            .at(&g, Stamp::<SystemDomain>::from_nanos(700 * MS))
            .is_err());
    }
    assert!(
        view.edge_counters(e)
            .unwrap()
            .worst_extrap_gap_ns
            .load(Relaxed)
            >= 5_000 * MS
    );
}

/// A query-level failure is not filed against an edge (§5.2). Two guards stop
/// it, `check_generation`'s `?` and `counter_of`'s edge-less arm; each covers
/// the other, so only removing both fails.
#[test]
fn a_stale_plan_is_not_charged_to_any_edge() {
    let f = Fixture::new();
    let e = f.edge("odom");
    let src = f.tree.frame("map").unwrap();
    let dst = f.tree.frame("odom").unwrap();
    let plan = f.tree.plan(src, dst).unwrap();

    let base = f.tree.frame("base").unwrap();
    let map = f.tree.frame("map").unwrap();
    f.tree.reparent(base, map).unwrap();

    {
        let g = f.tree.guard();
        assert!(
            plan.at(&g, Stamp::<SystemDomain>::from_nanos(100 * MS))
                .is_err(),
            "the plan must be stale"
        );
    }

    // Every edge, not just the plan's, so "names no edge" holds.
    let view = f.tree.arena_view();
    let mut errs = 0u64;
    let mut oks = 0u64;
    for id in 0..8u32 {
        let Some(c) = view.edge_counters(tf_tree::EdgeId(id)) else {
            continue;
        };
        errs += c.err_extrap_after.load(Relaxed)
            + c.err_extrap_before.load(Relaxed)
            + c.err_no_data.load(Relaxed)
            + c.err_slot_recycled.load(Relaxed)
            + c.err_slot_contended.load(Relaxed);
        oks += c.lookups_ok.load(Relaxed);
    }
    let _ = e;
    assert_eq!(errs, 0, "a query-level failure names no edge, anywhere");
    assert_eq!(oks, 0, "and is not a success either");
}

/// A multi-edge plan credits neither edge, so a `doctor` column means one thing.
#[test]
fn a_two_edge_plan_credits_neither_edge() {
    let f = Fixture::new();
    let (a, b) = (f.edge("odom"), f.edge("base"));
    let src = f.tree.frame("map").unwrap();
    let dst = f.tree.frame("base").unwrap();
    let plan = f.tree.plan(src, dst).unwrap();

    {
        let g = f.tree.guard();
        for i in 0..10i64 {
            plan.at(&g, Stamp::<SystemDomain>::from_nanos(i * 10 * MS))
                .unwrap();
        }
    }
    assert_eq!(f.ok_count(a), 0, "neither edge may be credited");
    assert_eq!(f.ok_count(b), 0);

    // A single-edge plan on the same tree is counted.
    let odom = f.tree.frame("odom").unwrap();
    let one = f.tree.plan(src, odom).unwrap();
    {
        let g = f.tree.guard();
        one.at(&g, Stamp::<SystemDomain>::from_nanos(100 * MS))
            .unwrap();
    }
    assert_eq!(f.ok_count(a), 1);
}

/// A read-only participant keeps no counters and does not fault (D18): the
/// `Guard` flush is a write from a read path. Uses a heap tree with `writable`
/// cleared; the shared-mapping half is `reparent_on_a_shared_arena_is_visible_to_another_process`
/// under `just shm-check`.
#[test]
fn a_read_only_view_records_nothing() {
    let f = Fixture::new();
    let e = f.edge("odom");
    let src = f.tree.frame("map").unwrap();
    let dst = f.tree.frame("odom").unwrap();
    let plan = f.tree.plan(src, dst).unwrap();

    {
        let ro = f.tree.arena_view().writable(false);
        assert!(!ro.is_writable());
        let g = tf_tree::Guard::new(ro);
        for i in 0..20i64 {
            plan.at(&g, Stamp::<SystemDomain>::from_nanos(i * 10 * MS))
                .unwrap();
        }
        assert!(plan
            .at(&g, Stamp::<SystemDomain>::from_nanos(10_000 * MS))
            .is_err());
    }
    let view = f.tree.arena_view();
    let c = view.edge_counters(e).unwrap();
    assert_eq!(
        c.lookups_ok.load(Relaxed),
        0,
        "a read-only view records nothing"
    );
    assert_eq!(c.err_extrap_after.load(Relaxed), 0);

    // Non-vacuity: a writable view does record.
    {
        let g = f.tree.guard();
        plan.at(&g, Stamp::<SystemDomain>::from_nanos(100 * MS))
            .unwrap();
    }
    assert_eq!(f.ok_count(e), 1);
}

/// The convenience path credits its denominator on every call, visible before
/// any loop ends (§5.4's withdrawal): `Guard`'s `Drop` alone publishes
/// `lookups_ok`, so a held guard would read `TFT010` as 100 %. The assertion is
/// per iteration, and the pair is single-edge because a two-edge plan credits no edge.
#[test]
fn the_convenience_path_publishes_its_denominator_on_every_call() {
    let f = Fixture::new();
    let e = f.edge("odom");
    assert_eq!(f.ok_count(e), 0, "the fixture must start from zero");

    for i in 0..50i64 {
        f.tree
            .lookup(
                "odom",
                "map",
                Stamp::<SystemDomain>::from_nanos(i * 10 * MS),
            )
            .unwrap();
        assert_eq!(
            f.ok_count(e),
            (i + 1) as u64,
            "the convenience path must publish its denominator per call, not \
             batch it: after {} lookups the edge should read {}",
            i + 1,
            i + 1
        );
    }
}
