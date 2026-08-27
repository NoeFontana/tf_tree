//! The per-publisher clock offset — `docs/decisions/0036`.
//!
//! `ClaimRecord::clock_offset_nanos` has existed, zeroed, in every shipped arena
//! since it was declared (as `last_push_nanos`); `docs/PHASE2.md` §6.4 said it
//! was *"bumped on every push"* and nothing bumped it. `TFT004` (clock skew) is
//! blind for exactly that reason, and `docs/PHASE5.md` §6 calls it the check
//! most likely to find something nobody knew.
//!
//! Two things are under test, and the second is the one that cost a redesign.
//!
//! **The sampling rule.** An unconditional wall-clock read costs 38.4 ns against
//! a ~4.9 ns push, so the field is written once per second of published data, at
//! an interval derived per claim from the edge's declared rate.
//!
//! **The quantity.** The writer stores `wall clock - stamp`, not the wall clock.
//! A receipt time cannot be paired with a stamp by anyone else: sampling means
//! the ring's newest stamp belongs to a *later* push than the receipt does, so
//! `receipt - newest_stamp` reads anywhere from +3 µs to -900 ms on a 10 Hz
//! publisher whose clock is exact — a ±1 s noise floor under a signal `TFT004`
//! must resolve at tens of milliseconds.
//! [`the_recorded_offset_does_not_move_with_the_newest_stamp`] is that property.
//!
//! **The whole file is gated**, on the same granularity and for the same reason
//! as `tests/counters.rs`: the field is reachable only through
//! `Tree::arena_view`, there is no stable-tier spelling of "what does this edge
//! report", and every test below would otherwise be reduced to its setup.
//! `cargo nextest run --workspace` unifies `unstable` in from the four consumers
//! that declare it, so these still run in `just test`.
//!
//! **Every assertion about *when* a sample happened detects the store by zeroing
//! the field**, never by comparing two readings. Two samples ten pushes apart
//! can land in the same nanosecond on a fast enough build, and a test that
//! counted distinct values would flake in the direction of "the sampler stopped
//! working".
#![cfg(feature = "unstable")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::Ordering::Relaxed;

use tf_tree::{Capacity, EdgeCfg, EdgeId, PushError, Tree, TreeBuilder};

mod common;

use common::pose;

/// A deliberate publisher clock error: five seconds **behind** the host, so
/// every stamp is `now - SKEW_NS` and every recorded offset is `+SKEW_NS`.
///
/// **The sign is the point, and these tests are the only executable statement of
/// it.** `clock_offset_nanos` is `wall clock - stamp`, so a *positive* offset
/// means the publisher's clock reads *earlier* than this host's — it is behind,
/// or its samples are old. `TFT004` will read this convention off these tests.
///
/// Large enough that no scheduling delay on any host could be mistaken for it,
/// small enough to be an error a real robot has.
const SKEW_NS: i64 = 5_000_000_000;

/// How far a recorded offset may sit from the skew injected into the stamps.
/// The gap is one `SystemTime::now()` call plus whatever the scheduler adds
/// between building a stamp and returning from `push`, so this bounds the *test
/// harness* and not the sampler.
const TOLERANCE_NS: i64 = 250_000_000;

/// A one-edge tree, `map -> odom`, at whatever nominal rate the caller declares.
/// `rate_hz` of `None` leaves the edge with `nominal_rate_mhz == 0`.
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

/// Read the recorded offset and clear it, so the next non-zero reading is
/// unambiguously a *new* store and not the previous one still sitting there.
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

/// **A declared rate sets the interval, and the interval is one sample per
/// second of published data** — `0036` question 1, which is the whole reason
/// that record exists rather than a patch that picks a constant. **And the
/// stored number is the offset**, which is the reason it needed a second one.
///
/// A 10 Hz edge declares 10_000 mHz, so `sample_every` is 10: the claim's first
/// push samples and every tenth after it. The equality is on the *whole* list of
/// sampling pushes, not on their count — a sampler that fired on every push
/// would also produce ten samples in the first hundred and pass a count
/// assertion.
///
/// Every stamp here is the wall clock minus [`SKEW_NS`], so every recorded
/// offset must come back at `SKEW_NS`.
///
/// Mutants, run rather than predicted:
///
/// * `sample_interval`'s `mhz => (mhz / 1000).max(1)` to `mhz => 1` — the push
///   list comes back `[1, 2, 3, …]`.
/// * store `now_nanos()` rather than `now_nanos() - stamp`, which is what this
///   field held before `0036`'s amendment — the offset assertion reads ~1.79e18
///   against a 5e9 skew.
#[test]
fn a_declared_rate_samples_once_per_second_of_published_data() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let w = tree.claim(odom, map).unwrap();
    let edge = w.edge();

    const SAMPLE_EVERY: i64 = 10; // 10 Hz = 10_000 mHz, divided by 1000.

    // A publisher five seconds behind the host. Stamps advance 1 µs apart so
    // they stay monotone whatever the host does between iterations.
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

/// **The recorded offset does not move with the newest stamp**, which is the
/// property that made the writer do the subtraction.
///
/// A receipt time stored on its own is unpairable: the sample is taken at one
/// push and the ring's newest stamp belongs to a later one, so a reader
/// computing `receipt - newest_stamp` gets the offset minus however much data
/// has been published since. Measured on this exact fixture before the
/// amendment: **+3 µs on the sampling push, walking to -900 ms nine pushes
/// later, on a publisher whose clock is exact.** The interval is ~1 s for every
/// publisher by construction, so it does not cancel in a fleet comparison
/// either.
///
/// Here the newest stamp advances 900 ms past the sampled one and the recorded
/// value must not follow it.
///
/// Mutant, run: store `now_nanos()` instead of the difference — the reading is a
/// wall clock, ~1.79e18 against a 5e9 skew, and the second assertion names it.
#[test]
fn the_recorded_offset_does_not_move_with_the_newest_stamp() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let w = tree.claim(odom, map).unwrap();
    let edge = w.edge();

    // 10 Hz: stamps 100 ms apart, and the publisher is five seconds behind. The
    // first push samples; the next nine only advance the newest stamp.
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

/// **A push that fails records nothing and does not spend the interval**, which
/// is the only part of `0036` question 4's hard constraint a test can see.
///
/// The constraint is that the clock read stays *outside* the seqlock window: a
/// longer write window is not merely a slower push, it is more `SlotContended`
/// retries for every reader of that edge, which converts a writer's diagnostic
/// into a reader's latency. `EdgeWriter::push`'s `?` is what enforces it, and an
/// offset recorded by a push that never wrote a sample is the observable form of
/// that `?` having been dropped.
///
/// Staged so the *failing* push is the one that would otherwise sample.
///
/// Mutant, run: in `EdgeWriter::push`, take the sample before the inner push —
/// *"a rejected push recorded an offset"*. The other tests still pass under it:
/// no push in any of them is rejected, which is why this one exists.
#[test]
fn a_rejected_push_neither_records_an_offset_nor_spends_the_interval() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();
    let w = tree.claim(odom, map).unwrap();
    let edge = w.edge();

    // The claim's first push samples by construction; drain it, so the interval
    // under test starts from a known point.
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

    // Push eleven would sample. This one regresses the stamp and is rejected.
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

    // …and the interval was not spent either, so the next *accepted* push is
    // still the eleventh and still samples.
    w.push(11_000, &pose(11)).unwrap();
    assert_ne!(
        take_offset(&tree, edge),
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
/// `0 => u32::MAX` — the loop finishes having sampled nothing, `left: []`.
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

/// **The interval is per claim, and a re-claim restarts it.**
///
/// `sample_every` is derived once, at claim time, from the edge record, and the
/// countdown that consumes it lives in the writer. Two writers of the same edge
/// therefore get the same interval and neither inherits the other's position in
/// it — the property a later reader would break by caching the counter anywhere
/// shared, and the one that makes a replacement writer's first push observable
/// rather than up to `sample_every` pushes away.
///
/// Mutant, run: give `sample_clock_offset` a `thread_local!` counter instead of
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
        assert_ne!(take_offset(&tree, edge), 0, "the first push did not sample");
        for i in 2..=9i64 {
            w.push(i * 1_000, &pose(i as u64)).unwrap();
            assert_eq!(take_offset(&tree, edge), 0, "push {i} sampled early");
        }
        edge
    };

    // The second writer is mid-interval by any count the *edge* keeps — push ten
    // overall — and at the start of its own. It must sample.
    let w = tree.claim(odom, map).unwrap();
    w.push(10_000, &pose(10)).unwrap();
    assert_ne!(
        take_offset(&tree, edge),
        0,
        "the second writer inherited the first's nine pushes: the countdown is \
         not per claim"
    );
}

/// **A claim clears the offset it inherits.**
///
/// Nothing else in the system resets `clock_offset_nanos` — not `release`, not
/// the reaper, not `tf_tree_core::edge::claim` — so a writer that takes over an
/// edge whose previous owner is long gone would otherwise publish under *that*
/// writer's number. `docs/decisions/0036` step 3's "nothing sampled yet" skip is
/// `clock_offset_nanos == 0`, which would not fire, so `TFT004` would attribute
/// a departed publisher's skew to the live one.
///
/// The window is one push wide now that a claim's first push samples, and one
/// push is enough: this reads the field between `claim` and the first `push`,
/// which is exactly where the stale value would be.
///
/// Mutant, run: delete `claim_rec.clock_offset_nanos.store(0, Ordering::Relaxed)`
/// from `Tree::claim` — *"a fresh claim inherited the previous writer's
/// offset"*.
#[test]
fn a_fresh_claim_does_not_inherit_the_previous_writers_offset() {
    let tree = tree_with_rate(Some(10.0));
    let map = tree.frame("map").unwrap();
    let odom = tree.frame("odom").unwrap();

    let edge = {
        let w = tree.claim(odom, map).unwrap();
        let edge = w.edge();
        w.push(now_nanos() - SKEW_NS, &pose(0)).unwrap();
        // Deliberately *not* drained: the point is what the next claimant finds.
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

/// **A non-wall-clock edge records nothing at all**, end to end.
///
/// The unit test beside `sample_interval` pins the mapping; this pins that the
/// mapping is *wired* — that `Tree::claim` reads the edge record's domain and
/// not only its rate. `wall clock - stamp` is an offset only when both sides
/// share an epoch, and a [`SimDomain`]-tagged edge stamping nanoseconds since
/// the start of a simulation would record about 1.79e18 — a fifty-six-year skew
/// that is not a skew.
///
/// `TFT005` skips a whole *arena* for this reason and cannot express it per
/// edge, which is what one tree holding a `SystemDomain` IMU beside a
/// `SimDomain` replay needs.
///
/// Mutant, run: pass `<SystemDomain as Domain>::TAG` instead of
/// `edge_rec.domain` at the `Tree::claim` call site — *"a SimDomain edge
/// recorded 17878…"*.
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

    // Sim time: nanoseconds since the simulation started, which is what makes
    // the subtraction meaningless rather than merely skewed.
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
