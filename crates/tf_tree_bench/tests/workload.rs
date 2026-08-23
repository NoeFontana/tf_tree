//! The workload catalogue's own gate.
//!
//! Every performance harness in this crate is driven by
//! [`tf_tree_bench::workload::CATALOGUE`], and every one of them reports
//! numbers. The failure mode that matters is therefore not a crash: it is a
//! sweep that runs green while measuring nothing — a query pair that does not
//! resolve, a stamp window that no edge covers, a fleet whose cross-robot path
//! exceeds `MAX_DEPTH` once folded (or `MAX_PATH_EDGES` before it is). Each of
//! those turns a benchmark row into a timing of
//! the error path, and none of them is visible in the output.
//!
//! So this file builds **every** catalogue entry and asserts, for each, that its
//! pairs compile, that a query at both ends of its window is answered, and that
//! the shape it reported is the shape it built. Adding a catalogue entry without
//! adding a test is the point: the test is written once, over the catalogue.
//!
//! `extreme_wide` is deliberately included despite costing seconds. It is the
//! entry most likely to break — it is the one near the arena's `u32` ceiling —
//! and a limit that is only found by the harness that trips over it in a
//! 30-minute run is not a limit anybody has documented.
// `panic!` is in this list deliberately: every assertion here is over the whole
// catalogue in a loop, so a failure that does not name the entry it came from
// costs a bisect through seven workloads to find out which one broke.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tf_tree::{InterpPolicy, Stamp};
use tf_tree_bench::workload::{self, Backing, Workload};

/// Build a workload on the heap, with the tf2-comparable interpolation policy.
fn build(w: &Workload) -> workload::Built {
    w.build(InterpPolicy::LerpSlerp, Backing::Heap)
        .unwrap_or_else(|e| panic!("building workload {}: {e:#}", w.name))
}

#[test]
fn every_catalogue_entry_estimates_before_it_is_built() {
    // `estimate` is what a harness calls to decide whether it can afford a
    // workload. If it disagrees with what `build` produces, that decision is
    // made on a wrong number — and the disagreement would only ever be noticed
    // as an unexplained OOM.
    for w in workload::CATALOGUE {
        let estimated = w
            .estimate()
            .unwrap_or_else(|e| panic!("estimating {}: {e:#}", w.name));
        let built = build(w);

        assert_eq!(
            estimated.frames, built.shape.frames,
            "{}: estimated frame count disagrees with the built tree",
            w.name
        );
        assert_eq!(
            estimated.edges, built.shape.edges,
            "{}: estimated edge count disagrees with the built tree",
            w.name
        );
        assert_eq!(
            estimated.samples, built.shape.samples,
            "{}: estimated sample count disagrees with the built tree",
            w.name
        );
        assert_eq!(
            estimated.arena_bytes, built.shape.arena_bytes,
            "{}: estimated arena size disagrees with the built tree",
            w.name
        );
        // `estimate` cannot know this one — folding is a property of the
        // compiled plan — and it must say so rather than guess.
        assert!(
            estimated.dyn_steps.is_none(),
            "{}: estimate claimed a dynamic-step count it cannot know",
            w.name
        );
    }
}

#[test]
fn every_catalogue_entry_answers_at_both_ends_of_its_window() {
    for w in workload::CATALOGUE {
        let built = build(w);
        let plans = built
            .plans()
            .unwrap_or_else(|e| panic!("compiling {}'s query pairs: {e:#}", w.name));
        assert!(!plans.is_empty(), "{}: no query pairs", w.name);

        let guard = built.tree.guard();
        // Both ends and the middle. The ends are where an off-by-one in ring
        // sizing shows up: `docs/PHASE1.md`'s ring retains `cap - 1` samples, so
        // a workload sized for exactly its sample count loses the oldest one —
        // which is precisely what the window's lower bound points at.
        for frac in [0.0, 0.5, 1.0] {
            let stamp: Stamp = Stamp::from_nanos(built.stamp_at(frac));
            let mut answered = 0usize;
            for plan in &plans {
                if plan.at(&guard, stamp).is_ok() {
                    answered += 1;
                }
            }
            assert_eq!(
                answered,
                plans.len(),
                "{}: only {answered}/{} pairs answered at frac {frac} (stamp {}); \
                 a benchmark over this workload would be timing the error path",
                w.name,
                plans.len(),
                built.stamp_at(frac),
            );
        }
    }
}

#[test]
fn every_catalogue_entry_states_a_dynamic_step_count() {
    // `docs/PHASE1.md` §11.3, normative: "Every reported latency row must state
    // its dynamic-step count, not just its nominal depth." A workload whose
    // deepest plan folds to zero dynamic steps is all-static — it would report a
    // beautiful latency for a path that never touches the sampling code the gate
    // exists to bound.
    for w in workload::CATALOGUE {
        let built = build(w);
        let steps = built
            .shape
            .dyn_steps
            .unwrap_or_else(|| panic!("{}: built shape has no dynamic-step count", w.name));
        assert!(
            steps > 0,
            "{}: deepest query folds to {steps} dynamic steps, so it does not \
             exercise the sampling path at all",
            w.name
        );
        assert!(
            steps <= tf_tree::MAX_DEPTH,
            "{}: {steps} dynamic steps exceeds MAX_DEPTH ({})",
            w.name,
            tf_tree::MAX_DEPTH
        );
    }
}

#[test]
fn the_fleet_query_actually_crosses_the_fleet() {
    // The whole reason `fleet_*` exists is that its query leaves one robot's
    // subtree and enters another's. If it did not, the row would be the `robot`
    // row with more frames in the arena — the same number, differently labelled,
    // which is worse than no row.
    let w = workload::by_name("fleet_16").expect("fleet_16 in the catalogue");
    let built = build(w);
    let (target, source) = &built.pairs[0];
    assert!(
        target.starts_with("r0/") && !source.starts_with("r0/"),
        "fleet_16 queries {target} <- {source}, which does not cross robots"
    );
    // Two robots' spines, each contributing `map->odom` and `odom->base_link`.
    assert_eq!(
        built.shape.dyn_steps,
        Some(4),
        "a cross-fleet query should compose two robots' two dynamic spine edges"
    );
}

#[test]
fn the_robot_workload_is_the_depth_three_chain_the_gate_is_pinned_to() {
    // `docs/PHASE1.md` §11.3 pins the go/no-go gate to three *dynamic* steps.
    // `robot` is the continuity anchor for every other row in the suite, so if
    // it ever stops being that chain, every comparison against a committed
    // number silently changes meaning.
    let w = workload::by_name("robot").expect("robot in the catalogue");
    let built = build(w);
    assert_eq!(built.pairs, vec![("imu_link".to_owned(), "map".to_owned())]);
    assert_eq!(built.shape.dyn_steps, Some(3));
    assert_eq!(built.shape.frames, 24, "the fixture is 24 frames");
    assert_eq!(built.shape.dynamic_edges, 4);
}

#[test]
fn every_publisher_edge_can_be_claimed_and_continued() {
    // The reader harnesses run a live publisher so the seqlock retry path is
    // exercised. Two ways that silently fails: an edge that cannot be claimed
    // (population left a writer alive), and a `next_stamp_ns` that is not after
    // the last populated sample — which `push` rejects as out of order, leaving
    // a "publisher" that publishes nothing and a reader benchmark back on a
    // quiescent tree.
    for w in workload::CATALOGUE {
        let built = build(w);
        assert!(
            !built.publishers.is_empty(),
            "{}: no publisher edges, so no harness can drive writers",
            w.name
        );
        for p in &built.publishers {
            let parent = built
                .tree
                .frame(&p.parent)
                .unwrap_or_else(|e| panic!("{}: frame {}: {e:?}", w.name, p.parent));
            let child = built
                .tree
                .frame(&p.child)
                .unwrap_or_else(|e| panic!("{}: frame {}: {e:?}", w.name, p.child));
            let writer = built
                .tree
                .claim(child, parent)
                .unwrap_or_else(|e| panic!("{}: claim {}->{}: {e}", w.name, p.parent, p.child));
            writer
                .push(p.next_stamp_ns, &tf_tree::exp_se3([0.0; 6]))
                .unwrap_or_else(|e| {
                    panic!(
                        "{}: continuing {}->{} at {} was rejected: {e:?}",
                        w.name, p.parent, p.child, p.next_stamp_ns
                    )
                });
        }
    }
}

#[test]
fn an_unknown_workload_names_the_known_ones() {
    let err = workload::by_name("no_such_workload").expect_err("should not resolve");
    let msg = err.to_string();
    for name in workload::names() {
        assert!(
            msg.contains(name),
            "the error for an unknown workload should list {name}: {msg}"
        );
    }
}
