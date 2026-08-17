//! The diagnostic counters, end to end — `docs/PHASE5.md` §5.
//!
//! These live in the facade rather than in `tf_tree_core` because they need a
//! *built tree*, and the builder is here. `tf_tree_core` owns the counter
//! structs and the `Guard` that fills them; only this crate can hand it an arena
//! with edges in it.
//!
//! **The whole file is gated, and it is the one place in this crate's suite
//! where that is the honest granularity.** A counter is only observable through
//! `Tree::arena_view`'s `edge_counters` — `doctor` reads it that way, and there
//! is no stable-tier spelling of "what does this edge's `lookups_ok` say". Both
//! of `Fixture`'s helpers go through the view, so every test below would be
//! reduced to its setup. `cargo nextest run --workspace` unifies `unstable` in
//! from `tf_tree_cli`/`tf_tree_c`/`tf_tree_bench`/`tf_tree_py`, so these five
//! still run in `just test`; what they no longer do is break the build of a
//! packager who runs `cargo test` on the published tarball.
#![cfg(feature = "unstable")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::Ordering::Relaxed;

use tf_tree::{Capacity, EdgeCfg, Stamp, SystemDomain, Tree, TreeBuilder};

const MS: i64 = 1_000_000;

/// `map -> odom -> base`, both dynamic, 64 samples each 10 ms apart.
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

    /// The edge attaching `child`, read out of the topology block — which is
    /// where `edge_of_child` lives (the 10 B/frame topo stride), rather than
    /// from any per-edge table.
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

/// **The denominator is flushed once per guard, not once per lookup.**
///
/// That is the whole §5.4 design: a relaxed `fetch_add` per lookup is both a
/// per-lookup cost and a *contended* one, since several readers on one edge
/// serialize on that cache line. Accumulating in the guard turns N atomics into
/// one.
///
/// The assertion **inside** the scope is the load-bearing half. Checking only
/// the final total would pass just as well against a write-through
/// implementation, which is the thing this design exists to avoid.
///
/// Mutant: make `Guard::note_ok` write straight to the arena ⇒ the mid-scope
/// assertion fails while the total stays right.
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

    // A second batch adds to it rather than replacing it.
    {
        let g = f.tree.guard();
        plan.at(&g, Stamp::<SystemDomain>::from_nanos(0)).unwrap();
    }
    assert_eq!(f.ok_count(e), 51);
}

/// **Extrapolation is split by direction, and the worst gap is a high-water
/// mark.**
///
/// Past the newest stamp and before the oldest mean opposite things — a
/// publisher that stopped, versus a consumer running behind or a ring that is
/// too short — and `TFT010` and `TFT011` key off exactly that distinction.
/// Collapsing them would leave both checks unable to say which fault they found.
///
/// Mutant: use `ExtrapAfter` for both branches ⇒ `err_extrap_before` stays zero.
#[test]
fn extrapolation_is_split_by_direction_and_keeps_the_worst_gap() {
    let f = Fixture::new();
    let e = f.edge("odom");
    let src = f.tree.frame("map").unwrap();
    let dst = f.tree.frame("odom").unwrap();
    let plan = f.tree.plan(src, dst).unwrap();

    {
        let g = f.tree.guard();
        // The ring holds 0..630 ms. Ten seconds past the newest:
        assert!(plan
            .at(&g, Stamp::<SystemDomain>::from_nanos(10_000 * MS))
            .is_err());
        // ...and five seconds before the oldest:
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
    // A *smaller* subsequent gap must not lower it.
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

/// **A query-level failure is not filed against an edge.**
///
/// A plan compiled against an old topology is the *caller's* problem. Counting
/// it would send an operator to inspect a publisher that is working correctly —
/// the same attribution argument §5.2 makes, in the other direction.
///
/// Mutant: **remove both guards together** — move `note_err` above
/// `check_generation` *and* give `counter_of` a catch-all arm — ⇒ this fails.
///
/// **Neither one alone dies, and that was checked rather than assumed.** Two
/// independent things stop a query-level failure reaching an edge counter:
///
/// * `check_generation`'s `?` returns before the counter calls are reached at
///   all, so `counter_of` never sees a `TopologyChanged`;
/// * `TopologyChanged` carries no edge field, so `counter_of` would return
///   `None` for it even if it did.
///
/// Each covers the other's absence, which is why the pair has to be removed
/// together to see a failure. The redundancy is not accidental: the first also
/// covers `TimeDomainMismatch`, and the second also covers every future error
/// variant that names no edge. This docstring says which mutant dies instead of
/// naming one that does not.
#[test]
fn a_stale_plan_is_not_charged_to_any_edge() {
    let f = Fixture::new();
    let e = f.edge("odom");
    let src = f.tree.frame("map").unwrap();
    let dst = f.tree.frame("odom").unwrap();
    let plan = f.tree.plan(src, dst).unwrap();

    // Mutate the topology so the plan's generation goes stale.
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

    // **Every** edge, not just the plan's. An earlier version checked only the
    // `map -> odom` edge and a catch-all in `counter_of` that filed against
    // `EdgeId(1)` slipped past it — the mutant landed on a different edge than
    // the one being inspected. Summing over the whole table is what makes
    // "names no edge" mean what it says.
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

/// **A multi-edge plan credits neither edge.**
///
/// Attributing its successes to one of them would put a number in `doctor`'s
/// table that means something different from every other number in the same
/// column: "lookups through this edge" would silently become "lookups through
/// some plan that happened to include this edge".
///
/// Mutant: return the first dynamic edge from `first_dynamic_edge` instead of
/// the sentinel ⇒ one of the two is credited and this fails.
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

    // ...but a single-edge plan on the same tree still is, so the test above is
    // not passing because counting is broken outright.
    let odom = f.tree.frame("odom").unwrap();
    let one = f.tree.plan(src, odom).unwrap();
    {
        let g = f.tree.guard();
        one.at(&g, Stamp::<SystemDomain>::from_nanos(100 * MS))
            .unwrap();
    }
    assert_eq!(f.ok_count(a), 1);
}

/// **A read-only participant keeps no counters, and does not die trying.**
///
/// This is the interaction `docs/PHASE5.md` §5 does not discuss, and it is not
/// theoretical: a consumer attaches read-only (D18) — the MMU is what stops it
/// corrupting a robot's transform tree — so *any* write from a read path faults
/// with `SIGSEGV`. The `Guard` flush is a write from a read path, and it killed
/// a read-only child in `tf_tree_bench`'s multiprocess suite with signal 11.
///
/// The resolution is that a read-only participant silently keeps no counters:
/// it cannot, and refusing to run would be far worse than losing a diagnostic.
/// `doctor` reports what the *writable* participants recorded.
///
/// This test uses a heap tree with a view whose `writable` flag is cleared by
/// hand, because the read-only case that matters needs a shared mapping and
/// `shm` is not on in this build. It exercises the guard, which is the code the
/// child faulted in.
///
/// Mutant: drop the `is_writable` check from `Guard`'s `Drop` ⇒ this still
/// passes here (a heap arena is writable memory whatever the flag says) but
/// `just shm-check`'s `reparent_on_a_shared_arena_is_visible_to_another_process`
/// dies with SIGSEGV. Both are needed; neither alone is the whole guard.
#[test]
fn a_read_only_view_records_nothing() {
    let f = Fixture::new();
    let e = f.edge("odom");
    let src = f.tree.frame("map").unwrap();
    let dst = f.tree.frame("odom").unwrap();
    let plan = f.tree.plan(src, dst).unwrap();

    {
        // The same arena, viewed as a read-only attacher would see it.
        let ro = f.tree.arena_view().writable(false);
        assert!(!ro.is_writable());
        let g = tf_tree::Guard::new(ro);
        for i in 0..20i64 {
            plan.at(&g, Stamp::<SystemDomain>::from_nanos(i * 10 * MS))
                .unwrap();
        }
        // ...and a failure, so the error path is exercised too.
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

    // Non-vacuity: the *same* lookups through a writable view do record, so
    // this is not passing because counting is broken outright.
    {
        let g = f.tree.guard();
        plan.at(&g, Stamp::<SystemDomain>::from_nanos(100 * MS))
            .unwrap();
    }
    assert_eq!(f.ok_count(e), 1);
}
