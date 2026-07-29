//! End-to-end ingest over synthetic MCAP fixtures — `docs/PHASE5.md` §3 and
//! §11's "ingest anomalies" and "out-of-order ingest" rows.
//!
//! Every recording here is fabricated by [`tf_tree_ingest::fixture`]; none of it
//! came off a robot. That module's docs say so at length, and so does this one:
//! what is proved below is that *this crate* classifies a stream the way §3.2
//! specifies, not that any real bag looks like these.

#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::path::{Path, PathBuf};

use tf_tree::{Stamp, SystemDomain};
use tf_tree_ingest::cdr::TransformStamped;
use tf_tree_ingest::fixture::{
    small_recording, two_publishers_with_latency, write_mcap, write_mcap_as, FixtureMessage,
};
use tf_tree_ingest::{ClockResetPolicy, Frames, IngestError, IngestOptions};

/// A scratch directory that removes itself, so a failing test does not leave a
/// recording behind. Disk is tight on the development host and `just test` runs
/// these dozens of times a day.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "tf_tree_ingest_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(dir: &Scratch, name: &str, msgs: &[FixtureMessage]) -> PathBuf {
    let p = dir.path(name);
    write_mcap(&p, msgs).unwrap();
    p
}

/// A pose whose every component is distinct, so a transposition or a swapped
/// sample is visible.
fn pose(k: f64) -> [f64; 7] {
    let (s, c) = (k.sin(), k.cos());
    [c, s, 0.0, 0.0, k, k * 2.0, k * 3.0]
}

/// The ordinary path: a recording with statics and three dynamic edges at
/// different rates ingests into a tree whose lookups answer.
///
/// Mutant: in `ingest::fill`, size every ring with `Capacity::slots(1)` instead
/// of the surveyed count — applied, and this test failed on the `map -> odom`
/// rate, which came back as the ring's contents rather than the source's.
///
/// A second mutant was tried and **survived**, which is worth recording: dropping
/// the `if rec.is_static` early-return in the pass-two callback is inert, because
/// a static edge is never in any buffer group and the `buffers.get_mut` lookup
/// misses. The early return is a short-circuit, not a guard, and nothing here
/// depends on it.
#[test]
fn small_recording_ingests() {
    let dir = Scratch::new("small");
    let path = write(&dir, "run.mcap", &small_recording());

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));

    assert_eq!(out.report.static_edges, 2);
    assert_eq!(out.report.dynamic_edges, 3);
    // 100 + 50 + 10 dynamic samples, and nothing dropped.
    assert_eq!(out.report.samples_pushed, 160);
    assert_eq!(out.report.fill.passes, 1);
    assert_eq!(out.report.anomalies.zero_stamp_drops, 0);
    assert_eq!(out.report.anomalies.duplicate_stamps, 0);

    // The three rates are distinct, which is what makes the row worth printing.
    let mut rates: Vec<u64> = out
        .report
        .edges
        .iter()
        .filter(|e| !e.is_static)
        .map(|e| e.rate_hz.unwrap().round() as u64)
        .collect();
    rates.sort_unstable();
    assert_eq!(rates, vec![10, 50, 100]);

    // And the tree answers a query that crosses a static and two dynamic edges.
    let t = 1_500_000_000;
    let iso = out
        .tree
        .lookup("map", "laser", Stamp::<SystemDomain>::from_nanos(t))
        .unwrap();
    assert!(iso.t.x.is_finite() && (iso.q.norm() - 1.0).abs() < 1e-12);
}

/// §11: *"shuffle a recording's messages; the resulting `.tft` must be
/// byte-identical to one built from the ordered source."* Tested here at the
/// level this commit reaches — the arena, which is what a `.tft` is a copy of.
///
/// The shuffle is a deterministic reversal-plus-interleave rather than an RNG,
/// so a failure reproduces exactly.
///
/// Mutant: change `buf.sort_by_key(|(s, _)| *s)` in `ingest::fill` to a no-op —
/// applied, and this test failed with `Push(NonMonotonicStamp { .. })` before it
/// ever reached the comparison. Mutant 2: revert `fill` to first-seen
/// declaration order — applied, and this test failed on a `LookupError`
/// comparison whose *values* matched and whose `EdgeId`s did not. That mutant is
/// how the canonical-order requirement was found in the first place.
///
/// **The `sort_by_key` stability is not guarded by *this* test**, and is not
/// claimed to be: swapping it for `sort_unstable_by_key` leaves this one
/// passing, because this fixture has no duplicate stamps for stability to decide
/// between. It *is* guarded elsewhere in this file —
/// `an_oversized_edge_spills_and_matches_the_in_memory_path` and
/// `a_reduce_pass_keeps_the_last_occurrence` both fail under that swap, because
/// both carry duplicates and compare the in-memory path against the spill path.
/// An earlier revision of this paragraph said no test in the file caught it,
/// which was true when it was written and stopped being true when those two
/// landed.
#[test]
fn out_of_order_ingest_matches_ordered() {
    let dir = Scratch::new("shuffle");
    let ordered = small_recording();
    let mut shuffled = ordered.clone();
    // Interleave the halves back-to-front: every message still present, log
    // order badly wrong, statics no longer first.
    let (a, b) = shuffled.split_at(ordered.len() / 2);
    let mut mixed = Vec::with_capacity(ordered.len());
    for i in 0..a.len().max(b.len()) {
        if let Some(m) = b.get(b.len().saturating_sub(1).wrapping_sub(i)) {
            mixed.push(m.clone());
        }
        if let Some(m) = a.get(i) {
            mixed.push(m.clone());
        }
    }
    assert_eq!(mixed.len(), ordered.len());
    shuffled = mixed;

    let p1 = write(&dir, "ordered.mcap", &ordered);
    let p2 = write(&dir, "shuffled.mcap", &shuffled);

    // **The reset detector has to be told to stand down for this one, and that
    // is a real property of the check rather than a workaround.** `ClockGuard`
    // classifies a backward stamp by how far back it goes, fed in *log* order;
    // a recording is log-ordered, so its inversions are milliseconds. This
    // fixture's messages have been deliberately reordered inside the file, so
    // the inversions are the length of the recording and are indistinguishable
    // from a bag loop. A threshold that admits them is what §11's shuffle row
    // asks for; a default that admitted them would miss every real reset.
    let opts = IngestOptions {
        clock_reset_threshold_ns: i64::MAX,
        ..IngestOptions::default()
    };
    let mut f1 = Frames::default();
    let mut f2 = Frames::default();
    let a = tf_tree_ingest::run(&p1, &opts, &mut f1).unwrap();
    let b = tf_tree_ingest::run(&p2, &opts, &mut f2).unwrap();

    assert_eq!(a.report.samples_pushed, b.report.samples_pushed);

    // **Identity, not just value.** The amendment at `ingest::fill` has two
    // halves — canonical *edge* order and canonical *frame* order — and only the
    // edge half is observable through a `LookupError`. `FrameId`s are assigned in
    // declaration order, so if declaration followed first-seen order the two runs
    // would agree about every pose and disagree about every id, and every byte of
    // the topology block with them. That is precisely what §11's byte-identity
    // requirement is about, so it is asserted directly rather than inferred.
    //
    // The fixture is non-degenerate for this: the two files declare frames in
    // wholly different first-seen orders (`base_link, laser, imu_link, odom, map,
    // arm_link` against `odom, base_link, map, arm_link, imu_link, laser`), so an
    // ingest that used first-seen order could not pass by accident.
    let mut names: Vec<&str> = f1.all().iter().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names.len(), 6, "frames: {names:?}");
    let mut order_a: Vec<&str> = f1.all().iter().map(String::as_str).collect();
    let mut order_b: Vec<&str> = f2.all().iter().map(String::as_str).collect();
    assert_ne!(
        order_a, order_b,
        "the two files interned frames in the same first-seen order; \
         the shuffle is degenerate and this assertion proves nothing"
    );
    order_a.sort_unstable();
    order_b.sort_unstable();
    assert_eq!(order_a, order_b, "the two runs saw different frame names");
    for name in &names {
        let ia = a.tree.frame(name).unwrap();
        let ib = b.tree.frame(name).unwrap();
        assert_eq!(ia, ib, "frame {name:?} got a different FrameId");
    }
    // Out-of-order arrivals are *counted* in the shuffled run and absent in the
    // ordered one — if they were zero in both, the shuffle did nothing and this
    // test would be vacuous.
    assert_eq!(a.report.anomalies.out_of_order, 0);
    assert!(
        b.report.anomalies.out_of_order > 0,
        "the shuffle produced no out-of-order arrivals; the fixture is degenerate"
    );

    // The two trees answer identically, bit for bit, across the whole span.
    for i in 0..200 {
        let t = 1_000_000_000 + i * 5_000_000;
        let x = a
            .tree
            .lookup("map", "arm_link", Stamp::<SystemDomain>::from_nanos(t));
        let y = b
            .tree
            .lookup("map", "arm_link", Stamp::<SystemDomain>::from_nanos(t));
        match (x, y) {
            (Ok(x), Ok(y)) => assert_eq!(x, y, "at {t}"),
            (Err(x), Err(y)) => assert_eq!(x, y, "at {t}"),
            (x, y) => panic!("at {t}: {x:?} vs {y:?}"),
        }
    }
}

/// §3.2: a duplicate `(edge, stamp)` resolves to the **last** occurrence in the
/// recording, and is counted.
///
/// The two values differ in every component, so "last wins" and "first wins"
/// give different answers.
///
/// Mutant: change the last-wins skip to keep the first (`if i > 0 && buf[i - 1].0
/// == stamp { continue; }`) — applied, and the lookup assertion failed, reading
/// back the earlier duplicate.
///
/// **A run of three** is here on purpose, not for symmetry. The single lookahead
/// at the push site is the whole rule, and a run longer than two is what shows
/// it collapses the entire run rather than a pair at a time — a second check
/// against the previously-pushed stamp is unreachable, because every element of
/// a run except the last `continue`s without pushing. Mutant: restore that
/// second check (`if prev == Some(stamp) { duplicates += 1; continue; }`) —
/// applied, and every assertion here still passed, which is the evidence that
/// the branch was dead rather than a claim that it was.
#[test]
fn duplicates_resolve_last_wins() {
    let dir = Scratch::new("dupes");
    let msgs = vec![
        FixtureMessage::dynamic("odom", "base_link", 1_000_000_000, pose(0.5)),
        FixtureMessage::dynamic("odom", "base_link", 2_000_000_000, pose(1.0)),
        // Same edge, same stamp, different value, later in the recording —
        // three deep, so the run is longer than one lookahead.
        FixtureMessage::dynamic("odom", "base_link", 2_000_000_000, pose(1.5)),
        FixtureMessage::dynamic("odom", "base_link", 2_000_000_000, pose(2.0)),
        FixtureMessage::dynamic("odom", "base_link", 3_000_000_000, pose(3.0)),
    ];
    let path = write(&dir, "dupes.mcap", &msgs);
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();

    assert_eq!(out.report.anomalies.duplicate_stamps, 2);
    assert_eq!(out.report.samples_pushed, 3);
    let got = out
        .tree
        .lookup(
            "odom",
            "base_link",
            Stamp::<SystemDomain>::from_nanos(2_000_000_000),
        )
        .unwrap();
    let want = pose(2.0);
    assert!(
        (got.t.x - want[4]).abs() < 1e-12,
        "got {:?}, wanted the later duplicate {want:?}",
        got.t
    );
}

/// §3.2: zero stamps are dropped, counted, and reported loudly.
///
/// Mutant: delete the `if rec.stamp_ns == 0` arm in `survey` — applied, and the
/// `zero_stamp_drops` assertion failed at 0; the samples were also admitted to
/// the ring, which changed `samples_pushed` to 5.
#[test]
fn zero_stamps_are_dropped_and_counted() {
    let dir = Scratch::new("zero");
    let mut msgs = vec![
        FixtureMessage::dynamic("odom", "base_link", 0, pose(9.0)).logged_at(1_000_000_000),
        FixtureMessage::dynamic("odom", "base_link", 0, pose(9.5)).logged_at(1_100_000_000),
    ];
    for i in 1..4 {
        msgs.push(FixtureMessage::dynamic(
            "odom",
            "base_link",
            i * 1_000_000_000,
            pose(i as f64),
        ));
    }
    let path = write(&dir, "zero.mcap", &msgs);
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();

    assert_eq!(out.report.anomalies.zero_stamp_drops, 2);
    assert_eq!(out.report.samples_pushed, 3);
    assert!(
        out.report.summary().contains("header.stamp"),
        "the summary should say what a zero stamp means:\n{}",
        out.report.summary()
    );
}

/// §3.2: stamps far in the future are kept, counted, and reported with the
/// worst offset.
///
/// The horizon is measured against the message's own MCAP log time, which is
/// why the fixture has to set the two apart.
///
/// Mutant: compare against `opts.future_horizon_ns` in the wrong direction
/// (`ahead < ...`) — applied, and the `future_stamps` count came back 4 (every
/// ordinary message) instead of 1.
#[test]
fn future_stamps_are_kept_and_reported() {
    let dir = Scratch::new("future");
    let mut msgs: Vec<FixtureMessage> = (1..4)
        .map(|i| FixtureMessage::dynamic("odom", "base_link", i * 1_000_000_000, pose(i as f64)))
        .collect();
    // Stamped a minute ahead of when it was recorded.
    msgs.push(
        FixtureMessage::dynamic("odom", "base_link", 64_000_000_000, pose(4.0))
            .logged_at(4_000_000_000),
    );
    let path = write(&dir, "future.mcap", &msgs);
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();

    assert_eq!(out.report.anomalies.future_stamps, 1);
    assert_eq!(out.report.anomalies.worst_future_offset_ns, 60_000_000_000);
    // Kept, not dropped.
    assert_eq!(out.report.samples_pushed, 4);
}

/// §3.2: an edge that appears on both `/tf` and `/tf_static` is a hard error
/// naming the timestamp — and, through [`tf_tree_ingest::describe`], the edge.
///
/// Mutant: make the `observe_dynamic` conflict a counted anomaly instead of an
/// error — applied, and this test failed with `Ok`. (The *other* branch, the
/// `StaticVerdict::KindChanged` arm, is not what this fixture reaches: the
/// static arrives first, so the contradiction is found on the dynamic side.
/// Mutating that arm alone leaves the test passing, which is why the mutant
/// named here is the one that runs.)
#[test]
fn edge_kind_change_is_a_hard_error() {
    let dir = Scratch::new("kind");
    let msgs = vec![
        FixtureMessage::static_edge("base_link", "laser", pose(0.25)),
        FixtureMessage::dynamic("base_link", "laser", 5_000_000_000, pose(0.75)),
    ];
    let path = write(&dir, "kind.mcap", &msgs);
    let mut frames = Frames::default();
    let err = tf_tree_ingest::survey(&path, &IngestOptions::default(), &mut frames).unwrap_err();
    assert!(
        matches!(
            err,
            IngestError::EdgeKindChanged {
                stamp_ns: 5_000_000_000,
                ..
            }
        ),
        "got {err:?}"
    );
    let text = tf_tree_ingest::describe(err, &frames).to_string();
    assert!(
        text.contains("base_link") && text.contains("laser") && text.contains("5000000000"),
        "the message must name the edge and the stamp: {text}"
    );
}

/// §3.2: a backward clock jump past the threshold halts, naming the timestamp.
///
/// Ordinary interleaving must **not** trigger it, so the fixture contains both:
/// a 5 ms regression (well under the 100 ms default) that is tolerated, and a
/// 30-second one that is not.
///
/// Mutant: set `clock_reset_threshold_ns` to 0 in `IngestOptions::default()` —
/// applied, and the 5 ms regression halted the ingest, so the test failed on
/// the `by_ns` assertion (it saw 5 000 000, not 30 000 000 000).
#[test]
fn clock_reset_halts_but_jitter_does_not() {
    let dir = Scratch::new("clock");
    let msgs = vec![
        FixtureMessage::dynamic("odom", "base_link", 40_000_000_000, pose(1.0)),
        // 5 ms backwards: ordinary interleaving between two publishers.
        FixtureMessage::dynamic("odom", "base_link", 39_995_000_000, pose(2.0)),
        FixtureMessage::dynamic("odom", "base_link", 41_000_000_000, pose(3.0)),
        // 30 s backwards: a bag loop or a sim reset.
        FixtureMessage::dynamic("odom", "base_link", 11_000_000_000, pose(4.0)),
    ];
    let path = write(&dir, "clock.mcap", &msgs);
    let mut frames = Frames::default();
    let err = tf_tree_ingest::survey(&path, &IngestOptions::default(), &mut frames).unwrap_err();
    assert!(
        matches!(
            err,
            IngestError::ClockReset {
                at_ns: 11_000_000_000,
                by_ns: 30_000_000_000,
                ..
            }
        ),
        "got {err:?}"
    );
}

/// **Two publishers with different latencies are not a clock reset.** The most
/// common real `/tf` topology — a 100 Hz `odom -> base_link` stamped at publish
/// time interleaved with a 10 Hz `map -> odom` stamped 200 ms in the past by a
/// localization node — ingests at the defaults.
///
/// 200 ms is chosen to be *above* the 100 ms reset threshold: that is what makes
/// this test about the guard's scope rather than about its constant. Nothing in
/// this recording is anomalous, and the assertion that `clock_resets == 0` is the
/// one that matters.
///
/// Mutant: replace the per-edge `clocks[slot]` in `survey` with a single guard
/// over the whole stream — applied, and this test failed with
/// `ClockReset { at_ns: 9_800_000_000, by_ns: 200_000_000 }` on message 21 of
/// 110, exactly as the finding predicted.
#[test]
fn two_publishers_with_different_latencies_ingest_at_the_defaults() {
    let dir = Scratch::new("latency");
    let path = write(
        &dir,
        "latency.mcap",
        &two_publishers_with_latency(200_000_000),
    );

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));

    assert_eq!(out.report.anomalies.clock_resets, 0);
    // Each edge is internally monotone, so the skew is not "out of order"
    // either — it is simply not this check's business.
    assert_eq!(out.report.anomalies.out_of_order, 0);
    assert_eq!(out.report.samples_pushed, 110);
    assert_eq!(out.report.dynamic_edges, 2);
}

/// A **bag loop still halts** with the guard scoped per edge, because a loop
/// moves `/clock` itself and therefore regresses every edge at once.
///
/// This is the other half of the per-edge change: without it, narrowing the
/// guard's scope could have been a silent removal of the check.
///
/// Mutant: delete the `ClockVerdict::Reset` arm's `ClockResetPolicy::Halt`
/// return, leaving the reset merely counted — applied, and this test failed with
/// `Ok`.
#[test]
fn a_bag_loop_still_halts_with_a_per_edge_guard() {
    let dir = Scratch::new("loop");
    let mut msgs = two_publishers_with_latency(200_000_000);
    // The recording restarts: every edge's stamps go back to the beginning.
    msgs.extend(two_publishers_with_latency(200_000_000));
    let path = write(&dir, "loop.mcap", &msgs);

    let mut frames = Frames::default();
    let err = tf_tree_ingest::survey(&path, &IngestOptions::default(), &mut frames).unwrap_err();
    let (parent, child) = match err {
        IngestError::ClockReset { parent, child, .. } => (parent, child),
        other => panic!("expected a clock reset, got {other:?}"),
    };
    // **The message names the edge**, which is what an earlier revision could
    // not do: with one guard over the merged stream there was no edge to name.
    let text = tf_tree_ingest::describe(err, &frames).to_string();
    assert!(
        text.contains(frames.name(parent)) && text.contains(frames.name(child)),
        "the message must name the regressing edge: {text}"
    );
}

/// §3.2's `split` policy is refused with a reason rather than silently doing
/// something else. **That refusal is the decided behaviour**, argued in §3.2's
/// amendment — an ingest that produced N arenas would change the output type of
/// every consumer, and cutting the recording at the stamp `halt` reports does
/// the same job with tools the user already has. So this test pins a decision,
/// not a placeholder.
///
/// Mutant: map `ClockResetPolicy::Split` to the same `ClockReset` error as
/// `Halt` — applied, and this test failed on the variant.
#[test]
fn split_policy_says_it_is_unimplemented() {
    let dir = Scratch::new("split");
    let msgs = vec![
        FixtureMessage::dynamic("odom", "base_link", 40_000_000_000, pose(1.0)),
        FixtureMessage::dynamic("odom", "base_link", 1_000_000_000, pose(2.0)),
    ];
    let path = write(&dir, "split.mcap", &msgs);
    let opts = IngestOptions {
        on_clock_reset: ClockResetPolicy::Split,
        ..IngestOptions::default()
    };
    let mut frames = Frames::default();
    assert_eq!(
        tf_tree_ingest::survey(&path, &opts, &mut frames).unwrap_err(),
        IngestError::ClockResetSplitUnsupported
    );
}

/// §11's "spill path" row for the **grouping** half of §3.1's cap: a
/// `--max-memory` below the dataset size splits pass two into several re-reads,
/// and the result is identical to the single-pass one. The run-file half is
/// `an_oversized_edge_spills_and_matches_the_in_memory_path` below; grouping is
/// preferred wherever it applies, so this is the path most recordings take.
///
/// Mutant: in `plan_groups`, return one group containing everything (delete the
/// flush) — applied, and the `passes > 1` assertion failed, which is the point:
/// without it the "identical result" half would pass vacuously.
#[test]
fn capped_memory_matches_the_uncapped_path() {
    let dir = Scratch::new("cap");
    let path = write(&dir, "cap.mcap", &small_recording());

    let mut f1 = Frames::default();
    let uncapped = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut f1).unwrap();

    // 64 B per buffered sample. 120 samples' worth is above the largest single
    // edge (100) and below the total (160), so pass two splits into two groups
    // — which is the case worth testing. A cap below the largest edge is the
    // *other* case and is `one_oversized_edge_is_named`.
    let opts = IngestOptions {
        max_memory_bytes: 120 * 64,
        ..IngestOptions::default()
    };
    let mut f2 = Frames::default();
    let capped = tf_tree_ingest::run(&path, &opts, &mut f2).unwrap();

    assert!(
        capped.report.fill.passes > 1,
        "the cap did not split anything; the test would be vacuous"
    );
    assert!(capped.report.fill.peak_buffer_bytes <= 120 * 64);
    assert_eq!(uncapped.report.fill.passes, 1);
    assert_eq!(uncapped.report.samples_pushed, capped.report.samples_pushed);
    for i in 0..200 {
        let t = 1_000_000_000 + i * 5_000_000;
        let a = uncapped
            .tree
            .lookup("map", "laser", Stamp::<SystemDomain>::from_nanos(t));
        let b = capped
            .tree
            .lookup("map", "laser", Stamp::<SystemDomain>::from_nanos(t));
        assert_eq!(a, b, "at {t}");
    }
}

/// §3.3: discovery is by schema, so a remapped topic is ingested and its
/// `tf_static` counterpart is still recognised as static.
///
/// Mutant: key channel selection on `topic == "/tf"` instead of the schema —
/// applied, and this test failed with `NoTransforms`.
#[test]
fn remapped_topics_are_found_by_schema() {
    let dir = Scratch::new("remap");
    let mut msgs = vec![FixtureMessage {
        topic: "/robot1/tf_static".into(),
        log_time_ns: 0,
        transforms: vec![TransformStamped {
            stamp_ns: 0,
            frame_id: "base_link".into(),
            child_frame_id: "laser".into(),
            pose: pose(0.3),
        }],
    }];
    for i in 1..5 {
        msgs.push(FixtureMessage {
            topic: "/robot1/tf".into(),
            log_time_ns: i * 1_000_000_000,
            transforms: vec![TransformStamped {
                stamp_ns: i * 1_000_000_000,
                frame_id: "odom".into(),
                child_frame_id: "base_link".into(),
                pose: pose(i as f64),
            }],
        });
    }
    let path = write(&dir, "remap.mcap", &msgs);
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();
    assert_eq!(out.report.static_edges, 1);
    assert_eq!(out.report.dynamic_edges, 1);
    assert_eq!(out.report.samples_pushed, 4);
}

/// §3.3: the **ROS 1 schema spelling** `tf2_msgs/TFMessage` is read too.
///
/// `rosbags-convert` keeps it when it converts a ROS 1 bag, and the payload is
/// CDR either way once `rosbag2` has written it. `TF_SCHEMAS` carries both names
/// with a comment saying so, and until now nothing exercised the second entry.
///
/// Mutant: drop `"tf2_msgs/TFMessage"` from `source::TF_SCHEMAS` — applied, and
/// this test failed with `IngestError::NoTransforms`.
#[test]
fn the_ros1_schema_spelling_is_read() {
    let dir = Scratch::new("ros1");
    let p = dir.path("ros1.mcap");
    let msgs: Vec<FixtureMessage> = (1..5)
        .map(|i| FixtureMessage::dynamic("odom", "base_link", i * 1_000_000_000, pose(i as f64)))
        .collect();
    write_mcap_as(&p, &msgs, "tf2_msgs/TFMessage", &[]).unwrap();

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&p, &IngestOptions::default(), &mut frames).unwrap();
    assert_eq!(out.report.samples_pushed, 4);
    assert_eq!(out.report.dynamic_edges, 1);
}

/// A TF-schema channel whose encoding is not `cdr` is **counted and skipped**,
/// not fed to a decoder that would fail on it.
///
/// A `json` or `protobuf` channel carrying the TF schema name is possible and is
/// not this crate's to decode. The count reaching the report is the other half:
/// `filtered_channels` is `skips.filtered_channels + skips.non_cdr`, and the
/// `+ non_cdr` term had nothing exercising it.
///
/// Mutant: set `filtered_channels = skips.filtered_channels` in `survey`,
/// dropping the `+ non_cdr` — applied, and the count assertion failed at 0 and
/// the summary line vanished.
#[test]
fn a_non_cdr_tf_channel_is_counted_not_decoded() {
    let dir = Scratch::new("noncdr");
    let p = dir.path("mixed.mcap");

    // A **mixed** recording: one decodable topic and one JSON-encoded TF
    // channel. A file where nothing decodes could only show that the ingest
    // failed, not that the skip was counted.
    let mut msgs: Vec<FixtureMessage> = (1..5)
        .map(|i| FixtureMessage::dynamic("odom", "base_link", i * 1_000_000_000, pose(i as f64)))
        .collect();
    for i in 1..5 {
        msgs.push(FixtureMessage {
            topic: "/tf_json".into(),
            log_time_ns: i * 1_000_000_000,
            transforms: vec![TransformStamped {
                stamp_ns: i * 1_000_000_000,
                frame_id: "map".into(),
                child_frame_id: "odom".into(),
                pose: pose(i as f64),
            }],
        });
    }
    write_mcap_as(&p, &msgs, "tf2_msgs/msg/TFMessage", &[("/tf_json", "json")]).unwrap();

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&p, &IngestOptions::default(), &mut frames).unwrap();

    // The `cdr` half is ingested…
    assert_eq!(out.report.samples_pushed, 4);
    assert_eq!(out.report.dynamic_edges, 1);
    // …and the JSON channel is counted, not silently ignored and not decoded.
    assert_eq!(out.report.anomalies.filtered_channels, 1);
    assert!(
        out.report.summary().contains("TF channels were skipped"),
        "the skip must reach the report:\n{}",
        out.report.summary()
    );
}

/// §5.6: a leading `/` is stripped, once, and counted — so `/odom` and `odom`
/// are one frame rather than two.
///
/// Mutant: remove the normalization from `normalize_pair` and intern the raw
/// names — applied, and the frame count came back 4 instead of 2.
#[test]
fn leading_slashes_are_normalized_away() {
    let dir = Scratch::new("slash");
    let msgs: Vec<FixtureMessage> = (1..5)
        .map(|i| {
            let (p, c) = if i % 2 == 0 {
                ("/odom", "/base_link")
            } else {
                ("odom", "base_link")
            };
            FixtureMessage::dynamic(p, c, i * 1_000_000_000, pose(i as f64))
        })
        .collect();
    let path = write(&dir, "slash.mcap", &msgs);
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();
    assert_eq!(out.report.frames, 2, "frames: {:?}", frames.all());
    assert_eq!(out.report.dynamic_edges, 1);
    assert_eq!(out.report.anomalies.stripped_slash_names, 4);
}

/// §3.2: `/tf_static` messages that contradict an already-declared value are
/// counted and reported; the first value wins.
///
/// Mutant: relax `StaticStore`'s comparison by treating any two poses as equal
/// — not applied (it is another crate's code and is covered by its own tests);
/// instead, the mutant applied here was deleting the
/// `anomalies.static_conflicts += 1` increment, which failed this test at 0.
#[test]
fn static_conflicts_are_reported_and_first_wins() {
    let dir = Scratch::new("static");
    let msgs = vec![
        FixtureMessage::static_edge("base_link", "laser", pose(0.2)),
        FixtureMessage::static_edge("base_link", "laser", pose(0.9)),
        FixtureMessage::dynamic("odom", "base_link", 1_000_000_000, pose(1.0)),
        FixtureMessage::dynamic("odom", "base_link", 2_000_000_000, pose(2.0)),
    ];
    let path = write(&dir, "static.mcap", &msgs);
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();
    assert_eq!(out.report.anomalies.static_conflicts, 1);
    let got = out
        .tree
        .lookup(
            "base_link",
            "laser",
            Stamp::<SystemDomain>::from_nanos(1_500_000_000),
        )
        .unwrap();
    let want = pose(0.2);
    assert!(
        (got.t.x - want[4]).abs() < 1e-12,
        "the first declaration should win: {:?}",
        got.t
    );
}

/// A file that is not an MCAP at all fails with a named error, not a panic and
/// not an empty tree.
///
/// Mutant: map every `mcap::McapError` to `IngestError::NoTransforms` — applied,
/// and this test failed on the variant.
#[test]
fn a_non_mcap_file_is_refused() {
    let dir = Scratch::new("junk");
    let p = dir.path("junk.mcap");
    std::fs::write(&p, b"this is not an MCAP recording").unwrap();
    let mut frames = Frames::default();
    assert_eq!(
        tf_tree_ingest::survey(&p, &IngestOptions::default(), &mut frames).unwrap_err(),
        IngestError::Mcap
    );
}

/// **A truncated recording is read up to the truncation point**, not discarded.
///
/// A recorder that was SIGKILLed, a disk that filled, an interrupted copy: all
/// three produce a file with no end magic and otherwise-intact records. Refusing
/// it loses everything, and the message an earlier revision gave ("the file is
/// not a well-formed MCAP recording") was wrong as well as unhelpful — the file
/// is well-formed, it is incomplete.
///
/// The fixture is truncated at three fractions so the test is about the
/// *property* rather than about one lucky cut point. Every cut must ingest, must
/// recover something, and must be **reported as truncated** — silently returning
/// a prefix as though it were the whole recording would be worse than refusing
/// it, because every count in the report would then describe a run the user did
/// not make. The deepest cut is additionally required to lose samples, so a set
/// of cuts that all landed in the trailing summary section cannot make this pass
/// vacuously.
///
/// Mutant: propagate `McapError::UnexpectedEof` from `read_tf` instead of
/// breaking on it — applied, and all three cuts failed with `IngestError::Mcap`
/// and zero samples recovered. Mutant 2: set `truncated` to `false`
/// unconditionally — applied, and the `anomalies.truncated` assertion failed.
#[test]
fn a_truncated_recording_yields_what_it_contains() {
    let dir = Scratch::new("trunc");
    let path = write(&dir, "full.mcap", &small_recording());
    let whole = std::fs::read(&path).unwrap();

    let mut frames = Frames::default();
    let full = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();
    assert_eq!(full.report.samples_pushed, 160);
    assert!(
        !full.report.anomalies.truncated,
        "the intact recording must not be reported as truncated"
    );

    let mut recovered = Vec::new();
    for pct in [90usize, 70, 50] {
        let cut = whole.len() * pct / 100;
        let p = dir.path(&format!("cut{pct}.mcap"));
        std::fs::write(&p, &whole[..cut]).unwrap();

        let mut f = Frames::default();
        let out = tf_tree_ingest::run(&p, &IngestOptions::default(), &mut f).unwrap_or_else(|e| {
            panic!(
                "{pct}% of the recording should still ingest: {}",
                tf_tree_ingest::describe(e, &f)
            )
        });
        assert!(
            out.report.samples_pushed > 0,
            "{pct}% recovered nothing at all"
        );
        assert!(
            out.report.anomalies.truncated,
            "{pct}% was read as though the recording were whole"
        );
        assert!(
            out.report.summary().contains("ends mid-record"),
            "the summary must say the recording is incomplete:\n{}",
            out.report.summary()
        );
        recovered.push(out.report.samples_pushed);
    }
    assert!(
        recovered
            .last()
            .is_some_and(|&n| n < full.report.samples_pushed),
        "every cut recovered the whole recording ({recovered:?} of {}); the cuts \
         all landed past the last message and this test proves nothing",
        full.report.samples_pushed
    );
}

/// A recording cut before its first complete chunk says so, rather than claiming
/// the recording has no transforms in it.
///
/// This is the boundary of the previous test's guarantee, and it is worth pinning
/// because the two failures look identical from the outside and have completely
/// different remedies. Chunks are taken whole, so recovery is chunk-granular; a
/// file cut inside its first chunk yields nothing at all. "This recording
/// contains no transforms" would send the user hunting for a publisher that never
/// ran, when their file is simply incomplete.
///
/// The cut is 400 bytes: past the start magic and the header record, so the file
/// is recognisably an MCAP, and far short of the first 4 KiB chunk.
///
/// Mutant: return `NoTransforms` unconditionally in `survey`'s empty-edges branch
/// (i.e. drop the `out.anomalies.truncated` test) ⇒ this fails on the variant.
#[test]
fn a_recording_cut_before_its_first_chunk_says_so() {
    let dir = Scratch::new("trunc_first");
    let path = write(&dir, "full.mcap", &small_recording());
    let whole = std::fs::read(&path).unwrap();
    assert!(
        whole.len() > 4096,
        "the fixture must be larger than one chunk for this cut to be inside the first one"
    );

    let p = dir.path("cut_early.mcap");
    std::fs::write(&p, &whole[..400]).unwrap();

    let mut frames = Frames::default();
    let err = tf_tree_ingest::survey(&p, &IngestOptions::default(), &mut frames).unwrap_err();
    assert_eq!(
        err,
        tf_tree_ingest::IngestError::TruncatedBeforeAnyChunk,
        "a cut inside the first chunk must be reported as truncation, not as an \
         absence of transforms"
    );
    let text = tf_tree_ingest::describe(err, &frames).to_string();
    assert!(
        text.contains("truncated"),
        "the message must name truncation: {text}"
    );
}

/// A missing file is an `Io` error carrying the errno, not a panic.
#[test]
fn a_missing_file_reports_its_errno() {
    let mut frames = Frames::default();
    let err = tf_tree_ingest::survey(
        Path::new("/nonexistent/tf_tree/does-not-exist.mcap"),
        &IngestOptions::default(),
        &mut frames,
    )
    .unwrap_err();
    assert_eq!(err, IngestError::Io { raw_os_error: 2 });
}

/// The report's JSON carries every field the summary prints, and parses as
/// JSON.
///
/// There is no JSON parser in this workspace's dependency budget, so "parses"
/// is checked structurally: balanced braces and brackets outside of strings,
/// and no bare `NaN`/`Infinity`, which is what a hand-written encoder actually
/// gets wrong.
///
/// Mutant: delete the `s.push(']')` that closes the `edges` array — applied, and
/// the balance assertion failed at depth 1. The `NaN` half of this test is
/// guarded separately and directly by `report::tests::non_finite_rate_is_null`,
/// because no fixture here produces a non-finite rate.
#[test]
fn report_json_is_well_formed() {
    let dir = Scratch::new("json");
    let path = write(&dir, "json.mcap", &small_recording());
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();
    let json = out.report.to_json();

    assert!(json.contains("\"schema\":\"tf_tree.ingest/1\""));
    assert!(
        !json.contains("NaN") && !json.contains("Infinity"),
        "{json}"
    );

    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for c in json.chars() {
        match (in_str, esc, c) {
            (true, true, _) => esc = false,
            (true, false, '\\') => esc = true,
            (true, false, '"') => in_str = false,
            (true, false, _) => {}
            (false, _, '"') => in_str = true,
            (false, _, '{' | '[') => depth += 1,
            (false, _, '}' | ']') => depth -= 1,
            _ => {}
        }
        assert!(depth >= 0, "unbalanced: {json}");
    }
    assert_eq!(depth, 0, "unbalanced: {json}");
    assert!(!in_str, "unterminated string: {json}");
}

/// §3.1's spill-to-run-file: an edge that alone exceeds `--max-memory` is sorted
/// through a temporary run file and a k-way merge, and produces **the same
/// answers** as the in-memory path.
///
/// That equality is the whole assertion. The spill path has its own sort, its
/// own duplicate collapse and its own ordering rule across runs; the only way to
/// know all three agree with the path they replace is to run both over one
/// recording and compare the answers, not the counters.
///
/// The recording is permuted by a coprime stride rather than lightly perturbed,
/// and that is load-bearing: a locally-out-of-order edge cuts into runs whose
/// stamp ranges are disjoint, so the merge degenerates into concatenation and
/// never compares across runs. A stride permutation spreads the whole range
/// through every run, so the heap does the work. The same construction is why
/// the reset threshold has to stand down here, for the reason
/// `out_of_order_ingest_matches_ordered` states at length: a deliberately
/// reordered file's stamp inversions are the length of the recording and are
/// indistinguishable from a bag loop.
///
/// Mutant: in `fill_spilled`, drop the trailing `if !buf.is_empty()` flush —
/// applied, and this failed on `spilled_bytes` at 35 840 B against the 38 400 B
/// asserted: the recording's last partial run was never written, so 40 samples
/// vanished. Mutant 2: build the merger's readers from
/// `spans.iter().rev()`, so the run indices and therefore the cross-run tie
/// break run backwards — applied, and this failed inside the lookup comparison
/// loop, the two paths disagreeing about the pose at the duplicated stamp.
/// Mutant 3: swap the runs' `sort_by_key`
/// for `sort_unstable_by_key` — **survived**: the tie that matters here is
/// *between* runs, and this fixture never puts two equal stamps in one run.
/// `a_reduce_pass_keeps_the_last_occurrence` puts a pair in one run and it
/// survives there too, so the per-run sort's stability is a property no test in
/// this suite gates — see that test's docstring for why, and for what does hold
/// it. Mutant 4: swap **`fill`'s** in-memory `sort_by_key` for the unstable one
/// — applied, and this failed, because the in-memory path is this test's
/// reference and last-wins is what the two paths are being compared on.
///
/// The peak assertions are two-sided on purpose. `peak <= cap` alone is passed
/// by a spill path that reports nothing at all: deleting every
/// `peak_buffer_bytes` update in `fill_spilled` was applied, and it failed here
/// and in `a_tiny_cap_reduces_in_several_passes`.
#[test]
fn an_oversized_edge_spills_and_matches_the_in_memory_path() {
    const N: i64 = 600;
    // Coprime with N, so the stride visits every index exactly once.
    const STRIDE: i64 = 7;
    let dir = Scratch::new("spill");
    let spill = dir.0.join("spill");
    std::fs::create_dir_all(&spill).unwrap();

    let mut msgs = Vec::new();
    for i in 0..N {
        let k = (i * STRIDE) % N;
        let t = 1_000_000_000 + k * 1_000_000;
        msgs.push(FixtureMessage::dynamic("odom", "base_link", t, pose(k as f64)).logged_at(t));
        // A second, small edge so the run also exercises the ordinary grouped
        // path beside the spilled one — a recording with a single edge would not
        // show that the two coexist.
        if i % 30 == 0 {
            msgs.push(
                FixtureMessage::dynamic("base_link", "arm", t, pose(k as f64 * 0.5)).logged_at(t),
            );
        }
    }
    // Duplicates of an *early* stamp appended at the very end, so the original
    // and the duplicates land in different runs and the cross-run tie break is
    // what decides. Last wins (§3.2), so `pose(42.0)` must be the survivor.
    let dup_stamp = 1_000_000_000 + 3 * 1_000_000;
    msgs.push(FixtureMessage::dynamic("odom", "base_link", dup_stamp, pose(41.0)).logged_at(0));
    msgs.push(FixtureMessage::dynamic("odom", "base_link", dup_stamp, pose(42.0)).logged_at(0));

    let path = write(&dir, "spill.mcap", &msgs);
    let cap = 8192;
    let base = IngestOptions {
        clock_reset_threshold_ns: i64::MAX,
        ..IngestOptions::default()
    };
    let mut f_spill = Frames::default();
    let spilled = tf_tree_ingest::run(
        &path,
        &IngestOptions {
            max_memory_bytes: cap,
            spill_dir: Some(spill.clone()),
            ..base.clone()
        },
        &mut f_spill,
    )
    .unwrap();
    let mut f_mem = Frames::default();
    let in_memory = tf_tree_ingest::run(&path, &base, &mut f_mem).unwrap();

    // The fixture is non-degenerate for what it claims to test.
    assert!(
        spilled.report.fill.spilled_runs >= 3,
        "only {} run(s); the cap did not force a real merge",
        spilled.report.fill.spilled_runs
    );
    assert_eq!(
        in_memory.report.fill.spilled_runs, 0,
        "the uncapped run must not spill, or the two paths are the same path"
    );
    assert!(
        spilled.report.fill.spilled_bytes >= N as u64 * 64,
        "spilled {} B for {N} samples",
        spilled.report.fill.spilled_bytes
    );
    assert!(
        spilled.report.fill.peak_buffer_bytes <= cap,
        "peak {} B over the {cap} B cap",
        spilled.report.fill.peak_buffer_bytes
    );
    // And a *lower* bound, which is the half that makes the peak a measurement
    // rather than a ceiling: the spill phase alone holds `run_samples × 64`
    // plus staging, and `WINDOW_SHARE_*` puts that at three quarters of the cap
    // at the least. Without this, deleting every `peak_buffer_bytes` update in
    // `fill_spilled` — reporting zero for the whole spill path — passes.
    assert!(
        spilled.report.fill.peak_buffer_bytes >= cap * 3 / 4,
        "peak {} B is too small to be the spill path's; it was not measured",
        spilled.report.fill.peak_buffer_bytes
    );

    // The two paths agree about how much survived, and about the duplicate.
    assert_eq!(
        spilled.report.samples_pushed,
        in_memory.report.samples_pushed
    );
    // `N` distinct stamps on the spilled edge (the two appended duplicates
    // collapse onto one that is already there) plus `N / 30` on the small one.
    assert_eq!(spilled.report.samples_pushed, N as u64 + N as u64 / 30);
    assert_eq!(spilled.report.anomalies.duplicate_stamps, 2);
    assert_eq!(
        spilled.report.anomalies.duplicate_stamps,
        in_memory.report.anomalies.duplicate_stamps
    );

    // And they answer identically, across the whole span and past both ends.
    for i in -5..=(N + 5) {
        let t = 1_000_000_000 + i * 1_000_000 + 250_000;
        let a = spilled
            .tree
            .lookup("odom", "arm", Stamp::<SystemDomain>::from_nanos(t));
        let b = in_memory
            .tree
            .lookup("odom", "arm", Stamp::<SystemDomain>::from_nanos(t));
        match (a, b) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "at {t}"),
            (Err(a), Err(b)) => assert_eq!(a, b, "at {t}"),
            (a, b) => panic!("at {t}: {a:?} vs {b:?}"),
        }
    }
    let got = spilled
        .tree
        .lookup(
            "odom",
            "base_link",
            Stamp::<SystemDomain>::from_nanos(dup_stamp),
        )
        .unwrap();
    let want = pose(42.0);
    assert!(
        (got.t.x - want[4]).abs() < 1e-12,
        "cross-run duplicate resolved to {:?}, wanted the last occurrence {want:?}",
        got.t
    );

    // The run file does not outlive the ingest. On Linux it never had a name
    // after `create`; this catches the fallback path failing anywhere else.
    let left: Vec<_> = std::fs::read_dir(&spill)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(left.is_empty(), "spill directory still holds {left:?}");
}

/// A rosbag2 sqlite3 bag is **diagnosed**, not reported as a corrupt MCAP.
///
/// §3.3 lists `rosbag2` sqlite3 as a source and this build does not read it (see
/// that section's amendment for the dependency finding). The gap between "not
/// implemented" and "not implemented, and the tool says so" is the whole value
/// of the check: `.db3` is the most likely wrong file to be handed, and "the
/// file is not a well-formed MCAP recording" sends a user hunting for corruption
/// in a file that is perfectly intact.
///
/// The fixture is a **real** SQLite database with rosbag2's schema
/// (`testdata/rosbag2/`), not sixteen bytes of magic.
///
/// Mutant: delete the `is_sqlite` call in `read_tf` — applied, and this failed
/// with `Mcap` where it expects `Rosbag2Sqlite`, which is exactly the unhelpful
/// message the check exists to replace.
#[test]
fn a_rosbag2_sqlite3_bag_is_named_as_one() {
    let bag =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/rosbag2/synthetic_empty.db3");
    let mut frames = Frames::default();
    // `Ingested` holds a `Tree`, which is not `Debug`, so `unwrap_err` is not
    // available here.
    match tf_tree_ingest::run(&bag, &IngestOptions::default(), &mut frames) {
        Err(e) => assert_eq!(e, IngestError::Rosbag2Sqlite, "got {e:?}"),
        Ok(_) => panic!("a rosbag2 .db3 must not ingest as an MCAP"),
    }
}

/// A cap so small that the runs outnumber what one merge can hold: the spill
/// path **reduces in several passes** and still answers identically.
///
/// This is the case that makes `--max-memory` a bound rather than an
/// aspiration. A k-way merge keeps at least one sample of every run resident, so
/// a single-pass merge over enough runs exceeds the cap no matter how the read
/// windows are sized. The reduce loop is what removes that floor, and the test
/// above does not reach it: at its 8 KiB cap the spilled edge produces **six**
/// runs against a fan-in of **95**, so its `while runs.runs() > fan_in` loop
/// never executes.
///
/// The witness that reduction actually happened is `spilled_bytes`. Every pass
/// rewrites every sample, so three times the edge's own size means the initial
/// spill plus **two** reduce passes; one pass would be two times, and no
/// reduction at all would be one.
///
/// Mutant: delete the `while runs.runs() > fan_in` loop, leaving the
/// single-pass merge — applied, and this failed twice over. `spilled_bytes` came
/// back at 140 800 B against the 422 400 B asserted; with that assertion
/// relaxed so the run could continue, the peak assertion then failed at 10 176 B
/// resident against the 1 024 B cap, which is the tenfold overrun the loop
/// exists to prevent.
///
/// Mutant 2: neutralise the three `peak_run_index_bytes` updates in
/// `fill_spilled` — applied, and this failed with `run index 0 B`. Mutant 3:
/// neutralise the three `peak_buffer_bytes` updates instead, so the spill path
/// reports no peak at all — applied, and the *lower* bound failed. Both halves
/// of both numbers are asserted, because a one-sided bound on a measurement is
/// satisfied by not measuring.
#[test]
fn a_tiny_cap_reduces_in_several_passes() {
    // 2 200 samples at 14 per run is 158 runs against a fan-in of 11: one
    // reduce pass leaves 15, still over the fan-in, so a second runs and leaves
    // 2. `spilled_runs` is the sum over passes and comes back at 158 + 15 + 2 =
    // 175 — measured, not derived.
    const N: i64 = 2_200;
    const STRIDE: i64 = 7;
    const CAP: u64 = 1024;
    let dir = Scratch::new("reduce");
    let spill = dir.0.join("spill");
    std::fs::create_dir_all(&spill).unwrap();

    let mut msgs = Vec::new();
    for i in 0..N {
        let k = (i * STRIDE) % N;
        let t = 1_000_000_000 + k * 1_000_000;
        msgs.push(FixtureMessage::dynamic("odom", "base_link", t, pose(k as f64)).logged_at(t));
    }
    let path = write(&dir, "reduce.mcap", &msgs);
    let base = IngestOptions {
        clock_reset_threshold_ns: i64::MAX,
        ..IngestOptions::default()
    };
    let mut f1 = Frames::default();
    let reduced = tf_tree_ingest::run(
        &path,
        &IngestOptions {
            max_memory_bytes: CAP,
            spill_dir: Some(spill.clone()),
            ..base.clone()
        },
        &mut f1,
    )
    .unwrap();
    let mut f2 = Frames::default();
    let in_memory = tf_tree_ingest::run(&path, &base, &mut f2).unwrap();

    assert!(
        reduced.report.fill.spilled_bytes >= 3 * N as u64 * 64,
        "spilled {} B for {N} samples; fewer than two reduce passes ran",
        reduced.report.fill.spilled_bytes
    );
    assert_eq!(
        reduced.report.fill.spilled_runs, 175,
        "158 runs, then 15, then 2 — a different split means the reduce loop \
         changed shape and this test's arithmetic no longer describes it"
    );
    assert!(
        reduced.report.fill.peak_buffer_bytes <= CAP,
        "peak {} B over the {CAP} B cap",
        reduced.report.fill.peak_buffer_bytes
    );
    // The other side of the bound: a spill path that reported nothing would
    // pass the assertion above. See the sibling test for the same pair.
    assert!(
        reduced.report.fill.peak_buffer_bytes >= CAP * 3 / 4,
        "peak {} B is too small to be the spill path's; it was not measured",
        reduced.report.fill.peak_buffer_bytes
    );
    // **The one allocation the cap does not bound**, reported rather than
    // hidden: sixteen bytes per run, and at this cap there are enough runs for
    // it to exceed the cap several times over. Measured at 6 880 B — the two
    // files' `Vec<RunSpan>` plus the snapshot the reduce pass merges from.
    assert!(
        reduced.report.fill.peak_run_index_bytes > CAP,
        "run index {} B; this fixture is supposed to be in the regime where it \
         exceeds the cap, which is the regime worth reporting",
        reduced.report.fill.peak_run_index_bytes
    );
    assert_eq!(reduced.report.samples_pushed, N as u64);
    assert_eq!(
        reduced.report.samples_pushed,
        in_memory.report.samples_pushed
    );
    for i in 0..N {
        let t = 1_000_000_000 + i * 1_000_000;
        let a = reduced
            .tree
            .lookup("odom", "base_link", Stamp::<SystemDomain>::from_nanos(t))
            .unwrap();
        let b = in_memory
            .tree
            .lookup("odom", "base_link", Stamp::<SystemDomain>::from_nanos(t))
            .unwrap();
        assert_eq!(a, b, "at {t}");
    }
    let left: Vec<_> = std::fs::read_dir(&spill)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(left.is_empty(), "spill directory still holds {left:?}");
}

/// **A duplicate that is re-merged by a reduce pass still resolves to the last
/// occurrence in the recording.**
///
/// This is the half of §3.2's "last wins" that nothing else gates. The claim in
/// `spill`'s module docs is that the recording-order tie break survives *being
/// re-merged across passes*, and neither sibling test reaches that:
/// `an_oversized_edge_spills_and_matches_the_in_memory_path` is the only one
/// with duplicate stamps and its six runs never trip the
/// `while runs.runs() > fan_in` loop, while
/// `a_tiny_cap_reduces_in_several_passes` is the only one that reduces and its
/// fixture has no duplicates at all. The failure that fits in the gap is silent:
/// a `.tft` whose pose at one stamp is the second-to-last value the recording
/// carried, disagreeing with the in-memory path over the same bag.
///
/// The fixture is built for it. At a 1 KiB cap a run holds 14 samples and the
/// fan-in is 11, so recording indices 3 and 17 land in **runs 0 and 1** — two
/// runs inside the *same* reduce window, which is exactly where the ordering is
/// decided and then frozen: after pass one both live in one output run, and no
/// later merge can reorder them.
///
/// Mutant: `spans.reverse()` before `spans.chunks(fan_in)` in `fill_spilled`,
/// which keeps every window contiguous but inverts the run order inside it.
/// Mutant 2: `runs.merge_runs(&chunk.iter().rev().copied().collect::<Vec<_>>(),
/// window)`, which inverts the tie break inside a reduce pass only and leaves
/// the final merge correct, so `spill`'s own
/// `runs_merge_ascending_with_ties_in_run_order` cannot see it. Both were
/// applied, both failed here at `across runs: resolved to Vec3 { x: 41.0, .. },
/// wanted the last occurrence 42`, and **both left every other test in the
/// workspace passing** — which is the gap this test closes.
///
/// Mutant 3, for the second duplicate pair: swap `fill_spilled`'s two
/// `sort_by_key`s for `sort_unstable_by_key` — **survived**. A run here is 14
/// samples and `sort_unstable_by_key` insertion-sorts a slice that short, so it
/// is stable in fact and the pair cannot be reordered. The per-run sort's
/// stability is therefore gated by **no** test in this suite; what stands behind
/// it is the comment at the call site. The in-memory sort it mirrors *is* gated
/// — swapping `fill`'s `sort_by_key` for the unstable one fails this test and
/// `an_oversized_edge_spills_and_matches_the_in_memory_path` together — so the
/// within-run pair is kept for that, and for the day a larger run size makes the
/// per-run half reachable.
#[test]
fn a_reduce_pass_keeps_the_last_occurrence() {
    const N: i64 = 2_200;
    const STRIDE: i64 = 7;
    const CAP: u64 = 1024;
    // `spill::spill_budget(1024)`'s samples-per-run. Spelled as a literal
    // because the constant is crate-private, and a test that recomputed it from
    // the same expression would be checking nothing.
    const RUN: usize = 14;
    let dir = Scratch::new("reduce_ties");
    let spill = dir.0.join("spill");
    std::fs::create_dir_all(&spill).unwrap();

    let mut msgs = Vec::new();
    for i in 0..N {
        let k = (i * STRIDE) % N;
        let t = 1_000_000_000 + k * 1_000_000;
        msgs.push(FixtureMessage::dynamic("odom", "base_link", t, pose(k as f64)).logged_at(t));
    }
    // Off the 1 ms grid, so neither stamp collides with one the loop above
    // already produced and each duplicate is a clean pair of two.
    let across = 1_000_000_000 + 1_234_000_000 + 500_000;
    let within = 1_000_000_000 + 1_300_000_000 + 500_000;
    // Across two runs: index 3 is in run 0, index `3 + RUN` in run 1, and both
    // runs fall inside the first reduce window.
    msgs[3] = FixtureMessage::dynamic("odom", "base_link", across, pose(41.0)).logged_at(0);
    msgs[3 + RUN] = FixtureMessage::dynamic("odom", "base_link", across, pose(42.0)).logged_at(0);
    // Within one run, so the per-run sort decides rather than the heap.
    msgs[5] = FixtureMessage::dynamic("odom", "base_link", within, pose(51.0)).logged_at(0);
    msgs[9] = FixtureMessage::dynamic("odom", "base_link", within, pose(52.0)).logged_at(0);

    let path = write(&dir, "reduce_ties.mcap", &msgs);
    let base = IngestOptions {
        clock_reset_threshold_ns: i64::MAX,
        ..IngestOptions::default()
    };
    let mut f1 = Frames::default();
    let reduced = tf_tree_ingest::run(
        &path,
        &IngestOptions {
            max_memory_bytes: CAP,
            spill_dir: Some(spill),
            ..base.clone()
        },
        &mut f1,
    )
    .unwrap();
    let mut f2 = Frames::default();
    let in_memory = tf_tree_ingest::run(&path, &base, &mut f2).unwrap();

    // Non-degenerate: the reduce loop has to have run, or this is the sibling
    // test again under another name.
    assert!(
        reduced.report.fill.spilled_bytes >= 3 * N as u64 * 64,
        "spilled {} B for {N} samples; fewer than two reduce passes ran",
        reduced.report.fill.spilled_bytes
    );
    assert_eq!(reduced.report.anomalies.duplicate_stamps, 2);
    assert_eq!(
        reduced.report.anomalies.duplicate_stamps,
        in_memory.report.anomalies.duplicate_stamps
    );
    assert_eq!(reduced.report.samples_pushed, N as u64 - 2);

    for (stamp, want, which) in [
        (across, 42.0, "across runs"),
        (within, 52.0, "within one run"),
    ] {
        let got = reduced
            .tree
            .lookup(
                "odom",
                "base_link",
                Stamp::<SystemDomain>::from_nanos(stamp),
            )
            .unwrap();
        assert!(
            (got.t.x - want).abs() < 1e-12,
            "{which}: resolved to {:?}, wanted the last occurrence {want}",
            got.t
        );
        let same = in_memory
            .tree
            .lookup(
                "odom",
                "base_link",
                Stamp::<SystemDomain>::from_nanos(stamp),
            )
            .unwrap();
        assert_eq!(got, same, "{which}: the two paths disagree");
    }
}
