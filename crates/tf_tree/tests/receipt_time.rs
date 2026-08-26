//! The per-publisher arena receipt time — `docs/decisions/0036`.
//!
//! `ClaimRecord::last_push_nanos` has existed, zeroed, in every shipped arena
//! since it was declared; `docs/PHASE2.md` §6.4 said it was *"bumped on every
//! push"* and nothing bumped it. `TFT004` (clock skew) is blind for exactly that
//! reason, and `docs/PHASE5.md` §6 calls it the check most likely to find
//! something nobody knew.
//!
//! What is under test here is the **sampling rule** rather than the write: an
//! unconditional wall-clock read costs 38 ns against a 3 ns push, so the field
//! is stamped once per second of published data, at an interval derived per
//! claim from the edge's declared rate.
//!
//! **The whole file is gated**, on the same granularity and for the same reason
//! as `tests/counters.rs`: `last_push_nanos` is reachable only through
//! `Tree::arena_view`, there is no stable-tier spelling of "what does this
//! edge's receipt time say", and every test below would otherwise be reduced to
//! its setup. `cargo nextest run --workspace` unifies `unstable` in from the
//! four consumers that declare it, so these still run in `just test`.
//!
//! **Every assertion detects the store by zeroing the field**, never by
//! comparing two readings. Two samples ten pushes apart can land in the same
//! nanosecond on a fast enough build, and a test that counts distinct values
//! would then flake in the direction of "the sampler stopped working".
#![cfg(feature = "unstable")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::Ordering::Relaxed;

use tf_tree::{Capacity, EdgeCfg, EdgeId, PushError, Tree, TreeBuilder};

mod common;

use common::pose;

/// A one-edge tree, `map -> odom`, at whatever nominal rate the caller declares.
/// `rate_hz` of `None` leaves the edge with `nominal_rate_mhz == 0`.
fn tree_with_rate(rate_hz: Option<f64>) -> Tree {
    let mut cfg = EdgeCfg::new(Capacity::slots(8));
    if let Some(hz) = rate_hz {
        cfg = cfg.nominal_rate_hz(hz);
    }
    TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .build()
        .unwrap()
}

/// Read the receipt time and clear it, so the next non-zero reading is
/// unambiguously a *new* store and not the previous one still sitting there.
fn take_receipt(tree: &Tree, edge: EdgeId) -> i64 {
    tree.arena_view()
        .claim(edge)
        .unwrap()
        .last_push_nanos
        .swap(0, Relaxed)
}

fn now_nanos() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap()
}

/// **A declared rate sets the interval, and the interval is one sample per
/// second of published data** — `0036` question 1, which is the whole reason
/// that record exists rather than a patch that picks a constant.
///
/// A 10 Hz edge declares 10_000 mHz, so `sample_every` is 10 and 100 pushes
/// must stamp the record at pushes 10, 20, … 100 and at no other push. The
/// equality is on the *whole* list of sampling pushes, not on their count: a
/// sampler that fired on every push would also produce ten samples in the first
/// hundred and pass a count assertion.
///
/// Mutants, both run rather than predicted:
///
/// * `sample_interval`'s `mhz => (mhz / 1000).max(1)` to `mhz => 1` — *"a 10 Hz
///   edge sampled on the wrong pushes"*, `left: [1, 2, 3, … 100]`.
/// * `now_nanos()` to an `Instant` differenced from a process-start `OnceLock`,
///   which is the shape of "somebody took the 10 ns" — *"push 10 stamped 130,
///   outside the wall-clock window \[…\] this test ran in"*.
#[test]
fn a_declared_rate_stamps_the_receipt_once_per_second_of_published_data() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let w = tree.claim(odom, map).unwrap();
    let edge = w.edge();

    const SAMPLE_EVERY: i64 = 10; // 10 Hz = 10_000 mHz, divided by 1000.

    let before = now_nanos();
    let mut sampled: Vec<(i64, i64)> = Vec::new();
    for i in 1..=10 * SAMPLE_EVERY {
        w.push(i, &pose(i as u64)).unwrap();
        let receipt = take_receipt(&tree, edge);
        if receipt != 0 {
            sampled.push((i, receipt));
        }
    }
    let after = now_nanos();

    let pushes: Vec<i64> = sampled.iter().map(|&(i, _)| i).collect();
    assert_eq!(
        pushes,
        (0..10).map(|k| 1 + k * SAMPLE_EVERY).collect::<Vec<i64>>(),
        "a 10 Hz edge sampled on the wrong pushes: the claim's first push must \
         sample, and every sample_every-th one after it, where sample_every is \
         nominal_rate_mhz / 1000"
    );

    // **The clock, pinned.** `Instant::now()` is 10 ns cheaper and unusable:
    // `TFT004` differences this against a wall-clock header stamp (`API.md` R3),
    // and a monotonic reading — nanoseconds since boot — would fail this bound
    // by decades on any machine with an uptime shorter than its age.
    for &(i, receipt) in &sampled {
        assert!(
            (before..=after).contains(&receipt),
            "push {i} stamped {receipt}, outside the wall-clock window \
             [{before}, {after}] this test ran in: the receipt is not \
             nanoseconds since the Unix epoch"
        );
    }
}

/// **A push that fails leaves the receipt untouched**, which is the only part of
/// `0036` question 4's hard constraint a test can see.
///
/// The constraint is that the clock read stays *outside* the seqlock window: a
/// longer write window is not merely a slower push, it is more `SlotContended`
/// retries for every reader of that edge, which converts a writer's diagnostic
/// into a reader's latency. `EdgeWriter::push`'s `?` is what enforces it, and a
/// receipt stamped by a push that never wrote a sample is the observable form of
/// that `?` having been dropped.
///
/// Staged so the *failing* push is the one that would otherwise sample: nine
/// successes at 10 Hz leave the counter one short, so if the failure counts or
/// stamps, it does so visibly.
///
/// Mutant: in `EdgeWriter::push`, take the clock before the inner push —
///
/// ```text
/// self.sample_receipt_time();
/// self.publisher.push(stamp, iso)
/// ```
///
/// — and this fails with *"a rejected push stamped a receipt time"*, run rather
/// than predicted. The other three tests still pass under it: no push in any of
/// them is rejected, which is exactly why this one exists.
#[test]
fn a_rejected_push_neither_stamps_a_receipt_nor_spends_the_interval() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let w = tree.claim(odom, map).unwrap();
    let edge = w.edge();

    // The claim's first push samples by construction; drain it, so the interval
    // under test starts from a known point.
    w.push(1_000, &pose(0)).unwrap();
    assert_ne!(
        take_receipt(&tree, edge),
        0,
        "the first push did not sample"
    );

    for i in 2..=10i64 {
        w.push(i * 1_000, &pose(i as u64)).unwrap();
        assert_eq!(
            take_receipt(&tree, edge),
            0,
            "push {i} sampled early: sample_every is not 10"
        );
    }

    // Push eleven would sample. This one regresses the stamp and is rejected.
    let err = w.push(1, &pose(99)).unwrap_err();
    assert!(
        matches!(err, PushError::NonMonotonicStamp { .. }),
        "expected the stamp to be rejected, got {err:?}"
    );
    assert_eq!(
        take_receipt(&tree, edge),
        0,
        "a rejected push stamped a receipt time: the clock read is on the wrong \
         side of the `?`, which is where it would sit inside the seqlock window"
    );

    // …and the interval was not spent either, so the next *accepted* push is
    // still the eleventh and still samples.
    w.push(11_000, &pose(11)).unwrap();
    assert_ne!(
        take_receipt(&tree, edge),
        0,
        "the eleventh accepted push did not sample: the rejected push consumed \
         an interval it never earned"
    );
}

/// **An edge that declares no rate still samples**, at the fixed default.
///
/// `nominal_rate_mhz == 0` means *not declared* — the reading `TFT007` already
/// takes of that value — and a tree built without a topology file is the common
/// case, not an exotic one. Falling back to "never sample" would leave `TFT004`
/// blind for exactly the users who never wrote a rate down.
///
/// Mutant, run: `sample_interval`'s `0 => DEFAULT_SAMPLE_EVERY` to
/// `0 => u32::MAX` — *"an undeclared-rate edge did not sample at the documented
/// default of 1024 pushes"*, `left: []`.
#[test]
fn an_edge_with_no_declared_rate_samples_at_the_default_interval() {
    let tree = tree_with_rate(None);
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let w = tree.claim(odom, map).unwrap();
    let edge = w.edge();

    const DEFAULT: i64 = 1024;

    let mut sampled: Vec<i64> = Vec::new();
    for i in 1..=2 * DEFAULT {
        w.push(i, &pose(i as u64)).unwrap();
        if take_receipt(&tree, edge) != 0 {
            sampled.push(i);
        }
    }

    assert_eq!(
        sampled,
        vec![1, 1 + DEFAULT],
        "an undeclared-rate edge did not sample on its first push and then \
         every {DEFAULT} pushes, the documented default"
    );
}

/// **The interval is per claim, and a re-claim restarts it.**
///
/// `sample_every` is derived once, at claim time, from the edge record, and the
/// countdown that consumes it lives in the writer. Two writers of the same edge
/// therefore get the same interval and neither inherits the other's position in
/// it — the property a later reader would break by caching the counter anywhere
/// shared, and the one that makes a replacement writer's first push observable
/// rather than up to `sample_every` pushes away.
///
/// Mutant, run: give `sample_receipt_time` a `thread_local!` counter instead of
/// `self.until_sample` — any cell shared between writers does it — and this
/// fails with *"the second writer inherited the first's nine pushes"*.
#[test]
fn a_second_claim_of_the_same_edge_starts_a_fresh_interval() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();

    let edge = {
        let w = tree.claim(odom, map).unwrap();
        let edge = w.edge();
        w.push(1_000, &pose(0)).unwrap();
        assert_ne!(
            take_receipt(&tree, edge),
            0,
            "the first push did not sample"
        );
        for i in 2..=9i64 {
            w.push(i * 1_000, &pose(i as u64)).unwrap();
            assert_eq!(take_receipt(&tree, edge), 0, "push {i} sampled early");
        }
        edge
    };

    // The second writer is mid-interval by any count the *edge* keeps — push ten
    // overall — and at the start of its own. It must sample.
    let w = tree.claim(odom, map).unwrap();
    w.push(10_000, &pose(10)).unwrap();
    assert_ne!(
        take_receipt(&tree, edge),
        0,
        "the second writer inherited the first's nine pushes: the countdown is \
         not per claim"
    );
}

/// **A claim clears the receipt time it inherits.**
///
/// Nothing else in the system resets `last_push_nanos` — not `release`, not the
/// reaper, not `tf_tree_core::edge::claim` — so a writer that takes over an edge
/// whose previous owner is long gone would otherwise publish under *that*
/// writer's timestamp. `docs/decisions/0036` step 3's planned "nothing sampled
/// yet" skip is `last_push_nanos == 0`, which would not fire, so `TFT004` would
/// attribute the dead writer's age to the live one as clock skew.
///
/// The window is one push wide now that a claim's first push samples, and one
/// push is enough: this reads the field between `claim` and the first `push`,
/// which is exactly where the stale value would be.
///
/// Mutant, run: delete `claim_rec.last_push_nanos.store(0, Ordering::Relaxed)`
/// from `Tree::claim` — *"a fresh claim inherited the previous writer's receipt
/// time"*.
#[test]
fn a_fresh_claim_does_not_inherit_the_previous_writers_receipt_time() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();

    let edge = {
        let w = tree.claim(odom, map).unwrap();
        let edge = w.edge();
        w.push(1_000, &pose(0)).unwrap();
        // Deliberately *not* drained: the point is what the next claimant finds.
        assert_ne!(
            tree.arena_view()
                .claim(edge)
                .unwrap()
                .last_push_nanos
                .load(Relaxed),
            0,
            "the first writer never stamped a receipt, so this test proves \
             nothing about the second"
        );
        edge
    };

    let _w = tree.claim(odom, map).unwrap();
    assert_eq!(
        tree.arena_view()
            .claim(edge)
            .unwrap()
            .last_push_nanos
            .load(Relaxed),
        0,
        "a fresh claim inherited the previous writer's receipt time: TFT004 \
         would read the gap between them as this publisher's clock skew"
    );
}
