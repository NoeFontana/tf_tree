//! The per-publisher clock offset (`docs/decisions/0036`).
//!
//! The writer stores `wall clock - stamp`, sampled once per second of published
//! data at an interval derived per claim from the edge's declared rate. The
//! whole file is gated on `unstable`: the field is reachable only through
//! `Tree::arena_view`. "When a sample happened" is detected by zeroing the
//! field, never by comparing two readings.
#![cfg(feature = "unstable")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::Ordering::Relaxed;

use tf_tree::{Capacity, EdgeCfg, EdgeId, PushError, Tree, TreeBuilder};

mod common;

use common::pose;

/// A publisher clock error of five seconds behind the host, so every recorded
/// offset is `+SKEW_NS`. Positive means the publisher's clock reads earlier;
/// `TFT004` reads that convention from these tests.
const SKEW_NS: i64 = 5_000_000_000;

/// How far a recorded offset may sit from the injected skew (bounds the harness).
const TOLERANCE_NS: i64 = 250_000_000;

/// A one-edge `map -> odom` tree; `rate_hz` of `None` leaves `nominal_rate_mhz == 0`.
fn tree_with_rate(rate_hz: Option<f64>) -> Tree {
    let mut cfg = EdgeCfg::new(Capacity::slots(64));
    if let Some(hz) = rate_hz {
        cfg = cfg.nominal_rate_hz(hz);
    }
    TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .build()
        .unwrap()
}

/// Read the recorded offset and clear it, so the next non-zero reading is a new store.
fn take_offset(tree: &Tree, edge: EdgeId) -> i64 {
    tree.arena_view()
        .claim(edge)
        .unwrap()
        .clock_offset_nanos
        .swap(0, Relaxed)
}

/// Read the recorded offset without disturbing it.
fn peek_offset(tree: &Tree, edge: EdgeId) -> i64 {
    tree.arena_view()
        .claim(edge)
        .unwrap()
        .clock_offset_nanos
        .load(Relaxed)
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

/// A declared rate sets the interval (one sample per second of data, `0036`
/// question 1): a 10 Hz edge samples the first push and every tenth after it.
/// The equality is on the list of sampling pushes, and every offset is `SKEW_NS`.
#[test]
fn a_declared_rate_samples_once_per_second_of_published_data() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let w = tree.claim(odom, map).unwrap();
    let edge = w.edge();

    const SAMPLE_EVERY: i64 = 10; // 10 Hz = 10_000 mHz, divided by 1000.

    // Publisher five seconds behind; stamps 1 µs apart stay monotone.
    let base = now_nanos() - SKEW_NS;
    let mut sampled: Vec<(i64, i64)> = Vec::new();
    for i in 1..=10 * SAMPLE_EVERY {
        w.push(base + i * 1_000, &pose(i as u64)).unwrap();
        let offset = take_offset(&tree, edge);
        if offset != 0 {
            sampled.push((i, offset));
        }
    }

    let pushes: Vec<i64> = sampled.iter().map(|&(i, _)| i).collect();
    assert_eq!(
        pushes,
        (0..10).map(|k| 1 + k * SAMPLE_EVERY).collect::<Vec<i64>>(),
        "a 10 Hz edge sampled on the wrong pushes: the claim's first push must \
         sample, and every sample_every-th one after it, where sample_every is \
         nominal_rate_mhz / 1000"
    );

    for &(i, offset) in &sampled {
        assert!(
            (offset - SKEW_NS).abs() < TOLERANCE_NS,
            "push {i} recorded {offset} ns against an injected skew of \
             {SKEW_NS} ns: the stored quantity is not (wall clock - stamp)"
        );
    }
}

/// The recorded offset does not move with the newest stamp (the writer does the
/// subtraction, so a reader need not pair a receipt time with a later stamp).
#[test]
fn the_recorded_offset_does_not_move_with_the_newest_stamp() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let w = tree.claim(odom, map).unwrap();
    let edge = w.edge();

    // 10 Hz: the first push samples; the next nine only advance the newest stamp.
    let base = now_nanos() - SKEW_NS;
    w.push(base, &pose(0)).unwrap();
    let at_sample = peek_offset(&tree, edge);
    assert_ne!(at_sample, 0, "the claim's first push did not sample");

    for i in 1..10i64 {
        w.push(base + i * 100_000_000, &pose(i as u64)).unwrap();
    }

    let after_900ms_of_data = peek_offset(&tree, edge);
    assert_eq!(
        after_900ms_of_data, at_sample,
        "the recorded value changed without a sample: something other than the \
         sampler is writing this field"
    );
    assert!(
        (after_900ms_of_data - SKEW_NS).abs() < TOLERANCE_NS,
        "after 900 ms of further data the edge reports {after_900ms_of_data} ns \
         against an injected skew of {SKEW_NS} ns. A reader differencing a \
         stored receipt time against the newest stamp would be out by exactly \
         that 900 ms, which is why the writer does the subtraction"
    );
}

/// A failed push records nothing and does not spend the interval (`0036`
/// question 4: the clock read stays outside the seqlock window).
#[test]
fn a_rejected_push_neither_records_an_offset_nor_spends_the_interval() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let w = tree.claim(odom, map).unwrap();
    let edge = w.edge();

    // Drain the first push, which samples by construction.
    w.push(1_000, &pose(0)).unwrap();
    assert_ne!(take_offset(&tree, edge), 0, "the first push did not sample");

    for i in 2..=10i64 {
        w.push(i * 1_000, &pose(i as u64)).unwrap();
        assert_eq!(
            take_offset(&tree, edge),
            0,
            "push {i} sampled early: sample_every is not 10"
        );
    }

    // Push eleven would sample; this one regresses the stamp and is rejected.
    let err = w.push(1, &pose(99)).unwrap_err();
    assert!(
        matches!(err, PushError::NonMonotonicStamp { .. }),
        "expected the stamp to be rejected, got {err:?}"
    );
    assert_eq!(
        take_offset(&tree, edge),
        0,
        "a rejected push recorded an offset: the clock read is on the wrong \
         side of the `?`, which is where it would sit inside the seqlock window"
    );

    // The interval was not spent: the next accepted push still samples.
    w.push(11_000, &pose(11)).unwrap();
    assert_ne!(
        take_offset(&tree, edge),
        0,
        "the eleventh accepted push did not sample: the rejected push consumed \
         an interval it never earned"
    );
}

/// An edge that declares no rate still samples, at the fixed default.
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
        if take_offset(&tree, edge) != 0 {
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

/// The interval is per claim: a re-claim restarts it, and writers share no counter.
#[test]
fn a_second_claim_of_the_same_edge_starts_a_fresh_interval() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();

    let edge = {
        let w = tree.claim(odom, map).unwrap();
        let edge = w.edge();
        w.push(1_000, &pose(0)).unwrap();
        assert_ne!(take_offset(&tree, edge), 0, "the first push did not sample");
        for i in 2..=9i64 {
            w.push(i * 1_000, &pose(i as u64)).unwrap();
            assert_eq!(take_offset(&tree, edge), 0, "push {i} sampled early");
        }
        edge
    };

    // Mid-interval by the edge's count, at the start of its own: must sample.
    let w = tree.claim(odom, map).unwrap();
    w.push(10_000, &pose(10)).unwrap();
    assert_ne!(
        take_offset(&tree, edge),
        0,
        "the second writer inherited the first's nine pushes: the countdown is \
         not per claim"
    );
}

/// A claim clears the offset it inherits, so `TFT004` never attributes a
/// departed publisher's skew to the live one (`0036` step 3).
#[test]
fn a_fresh_claim_does_not_inherit_the_previous_writers_offset() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();

    let edge = {
        let w = tree.claim(odom, map).unwrap();
        let edge = w.edge();
        w.push(now_nanos() - SKEW_NS, &pose(0)).unwrap();
        assert_ne!(
            peek_offset(&tree, edge),
            0,
            "the first writer never recorded an offset, so this test proves \
             nothing about the second"
        );
        edge
    };

    let _w = tree.claim(odom, map).unwrap();
    assert_eq!(
        peek_offset(&tree, edge),
        0,
        "a fresh claim inherited the previous writer's offset: TFT004 would \
         bill a departed publisher's skew to this one"
    );
}

/// A non-wall-clock edge records nothing, end to end: `Tree::claim` reads the
/// edge record's domain, not only its rate.
#[test]
fn an_edge_outside_the_wall_clock_domain_records_no_offset() {
    const SIM_TAG: u8 = 2; // `tf_tree_core::plan::SimDomain::TAG`.

    let tree = TreeBuilder::new()
        .dynamic_edge(
            "map",
            "odom",
            EdgeCfg::new(Capacity::slots(64))
                .nominal_rate_hz(10.0)
                .domain(SIM_TAG),
        )
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let w = tree.claim(odom, map).unwrap();
    let edge = w.edge();

    // Sim time makes the subtraction meaningless.
    for i in 1..=40i64 {
        w.push(i * 100_000_000, &pose(i as u64)).unwrap();
        let recorded = peek_offset(&tree, edge);
        assert_eq!(
            recorded, 0,
            "a SimDomain edge recorded {recorded} on push {i}: the wall clock \
             and sim time do not share an epoch, so this is not an offset"
        );
    }
}
