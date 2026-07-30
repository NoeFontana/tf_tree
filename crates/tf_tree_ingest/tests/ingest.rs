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
    small_recording, two_publishers_with_latency, write_mcap, write_mcap_as, write_mcap_chunked,
    ChunkDamage, ChunkedSpec, FixtureMessage, DAMAGED_CHUNK_ORDINAL,
};
// Only the compressed tests use these, and they are `#[cfg(feature =
// "compression")]` — a plain `use` would be an unused import in the codec-free
// build, which `-D warnings` rejects.
#[cfg(feature = "compression")]
use tf_tree_ingest::fixture::{
    conformance_recording, FixtureCodec, CONFORMANCE_MESSAGES_PER_CHUNK,
};
use tf_tree_ingest::{
    BadChunkKind, ClockResetPolicy, Frames, IngestError, IngestOptions, OnBadChunk,
};

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

/// Truncation recovery is **record-granular, including inside a chunk**: the more
/// of a recording survives, the more transforms come out of it.
///
/// This is the property the hand-rolled framing exists for. Asking a reader for
/// whole records only means a cut mid-chunk loses that entire chunk — the final
/// one of a real recording, and *all* of a small one. Measured on this fixture
/// (24 960 bytes, 4 KiB chunks): 400 bytes yields 1 transform, 800 yields 4,
/// 1 600 yields 10, 3 000 yields 21. A chunk-granular reader yields zero for
/// every one of those.
///
/// Mutant: skip the chunk arm when `!complete` (i.e. only read whole chunks) ⇒
/// every cut below 4 KiB recovers nothing and the strict-increase assertion
/// fails. Mutant: pass `tolerate_tail = false` in `read_chunk` ⇒ the final partial
/// record in each prefix is reported as `InnerFraming` corruption and the ingest
/// errors instead of recovering.
#[test]
fn truncation_recovery_is_record_granular() {
    let dir = Scratch::new("trunc_gran");
    let path = write(&dir, "full.mcap", &small_recording());
    let whole = std::fs::read(&path).unwrap();

    let mut recovered = Vec::new();
    for cut in [400usize, 800, 1600, 3000, 6000] {
        assert!(cut < whole.len());
        let p = dir.path(&format!("c{cut}.mcap"));
        std::fs::write(&p, &whole[..cut]).unwrap();
        let mut f = Frames::default();
        let s = tf_tree_ingest::survey(&p, &IngestOptions::default(), &mut f).unwrap_or_else(|e| {
            panic!(
                "a {cut}-byte prefix must still yield its complete records: {}",
                tf_tree_ingest::describe(e, &f)
            )
        });
        assert!(
            s.anomalies.truncated,
            "{cut} bytes was read as though the recording were whole"
        );
        // **Truncation is not corruption, and the report must not conflate them.**
        // The final record in a cut chunk's prefix necessarily runs past the end;
        // treating that as a malformed chunk would tell an operator their
        // recording is damaged when it is merely incomplete, and would inflate a
        // counter they are meant to act on.
        assert_eq!(
            s.anomalies.bad_chunks, 0,
            "a truncated recording reported {} corrupt chunk(s) at a {cut}-byte cut",
            s.anomalies.bad_chunks
        );
        recovered.push(s.transforms_read);
    }
    // Strictly increasing: a chunk-granular reader would give a run of zeros and
    // then a jump, so this is what distinguishes the two.
    for w in recovered.windows(2) {
        assert!(
            w[1] > w[0],
            "more bytes must yield more transforms, got {recovered:?}"
        );
    }
    assert!(
        recovered[0] > 0,
        "a 400-byte prefix lands inside the first 4 KiB chunk and must still \
         recover the records before the cut, got {recovered:?}"
    );
}

/// A recording cut before *any* complete record says it was truncated, rather than
/// claiming the recording has no transforms in it.
///
/// The remaining floor of the guarantee above: a prefix can be too short to hold
/// one whole record, and then there is genuinely nothing to recover. The two
/// failures look identical from the outside and have opposite remedies — "no
/// transforms" sends the user hunting for a publisher that never ran, when their
/// file is simply incomplete.
///
/// Mutant: return `NoTransforms` unconditionally in `survey`'s empty-edges branch
/// (drop the `out.anomalies.truncated` test) ⇒ this fails on the variant.
#[test]
fn a_recording_cut_before_any_record_says_it_was_truncated() {
    let dir = Scratch::new("trunc_floor");
    let path = write(&dir, "full.mcap", &small_recording());
    let whole = std::fs::read(&path).unwrap();

    // 200 bytes: past the 8-byte magic and the Header record, so the file is
    // recognisably an MCAP, and short of the first complete message.
    let p = dir.path("cut_early.mcap");
    std::fs::write(&p, &whole[..200]).unwrap();

    let mut frames = Frames::default();
    let err = tf_tree_ingest::survey(&p, &IngestOptions::default(), &mut frames).unwrap_err();
    assert_eq!(
        err,
        tf_tree_ingest::IngestError::TruncatedBeforeAnyChunk,
        "a cut before any complete record must be reported as truncation, not as \
         an absence of transforms"
    );
    let text = tf_tree_ingest::describe(err, &frames).to_string();
    assert!(
        text.contains("truncated"),
        "the message must name truncation: {text}"
    );
}

/// A file whose start magic is missing is refused, **even though its records would
/// parse perfectly well**.
///
/// The junk-file test above cannot show this: arbitrary bytes produce an absurd
/// record length and are refused by the size bound, so a build with no magic check
/// returns the same error by accident. This one strips the eight-byte magic off a
/// real recording and leaves everything else intact, so the magic is the only
/// thing standing between "a recording" and "something else that happens to be
/// shaped like one".
///
/// It matters because the magic is now the *only* structural check on the file as
/// a whole — everything downstream tolerates a short or damaged tail by design.
///
/// **No mutant kills this, and the reason is worth stating rather than hiding.**
/// Dropping the `magic != *mcap::MAGIC` comparison was applied and survived: the
/// eight bytes are consumed either way, so a file that fails the comparison is
/// also misaligned by eight bytes and fails downstream on a nonsense record length
/// — the same `IngestError::Mcap`, reached by accident. The comparison's value is
/// that the refusal is *immediate and unambiguous* instead of incidental, which is
/// a diagnostic property this test cannot distinguish. What the test does pin is
/// that a headless file is refused at all, which is not free: everything after the
/// magic tolerates damage by design, so without a structural check somewhere this
/// would read as a recording containing nothing.
#[test]
fn a_recording_without_its_start_magic_is_refused() {
    let dir = Scratch::new("nomagic");
    let path = write(&dir, "full.mcap", &small_recording());
    let whole = std::fs::read(&path).unwrap();

    // Sanity: the intact file ingests, so the only difference below is the magic.
    let mut ok_frames = Frames::default();
    tf_tree_ingest::survey(&path, &IngestOptions::default(), &mut ok_frames).unwrap();

    let p = dir.path("headless.mcap");
    std::fs::write(&p, &whole[8..]).unwrap();
    let mut frames = Frames::default();
    assert_eq!(
        tf_tree_ingest::survey(&p, &IngestOptions::default(), &mut frames).unwrap_err(),
        IngestError::Mcap,
        "a file without MCAP's magic must be refused, not read as a recording"
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

// ---------------------------------------------------------------------------
// The hand-rolled, summary-free fixture and the corrupt-chunk skip policy.
//
// `fixture`'s module docs say why these cannot be written against a fixture from
// `mcap::Writer`: it cannot produce a damaged chunk, and its summary section
// repeats every Schema and Channel record, which makes a skipped definitions
// chunk harmless and the caveat in `OnBadChunk`'s docs unreachable.
// ---------------------------------------------------------------------------

/// Nine messages, three per child frame, so a chunk of three makes "the second
/// chunk's messages" exactly "the `sensor_b` edge".
///
/// A per-chunk *edge* rather than a per-chunk slice of one edge, because the
/// assertion then has a shape a reader can check by eye: after the second chunk is
/// skipped, `sensor_b` is not in the report at all and the other two are whole.
fn three_sensors_nine_messages() -> Vec<FixtureMessage> {
    let mut out = Vec::new();
    for (chunk, child) in ["sensor_a", "sensor_b", "sensor_c"].iter().enumerate() {
        for i in 0..3 {
            let k = chunk * 3 + i;
            out.push(FixtureMessage::dynamic(
                "base_link",
                child,
                1_000_000_000 + k as i64 * 10_000_000,
                pose(k as f64 + 1.0),
            ));
        }
    }
    out
}

/// `(parent, child, samples)` for every edge in a report, in a stable order.
fn edge_rows(report: &tf_tree_ingest::IngestReport) -> Vec<(String, String, u64)> {
    let mut rows: Vec<(String, String, u64)> = report
        .edges
        .iter()
        .map(|e| (e.parent.clone(), e.child.clone(), e.samples))
        .collect();
    rows.sort();
    rows
}

/// One row of [`edge_time_rows`]: parent, child, samples, and the source's oldest
/// and newest stamp for that edge.
type EdgeTimeRow = (String, String, u64, Option<i64>, Option<i64>);

/// `(parent, child, samples, source_oldest_ns, source_newest_ns)` for every edge.
///
/// The time bounds are here and not in [`edge_rows`] because comparing two writers
/// on counts alone leaves *when* unchecked: a chunk loop that dropped one group and
/// duplicated another, or a corpus slice taken with the wrong offset, can keep every
/// sample count equal. The bounds are the cheapest field on the report that moves
/// when a stamp does.
///
/// They do **not** cover MCAP's `log_time`, which the report never derives an edge
/// time from — `fixture::tests::a_clean_hand_rolled_file_is_accepted_by_the_mcap_crate`
/// compares every message's `log_time` against the corpus for that reason, and its
/// third mutant is the one that proves nothing here can.
fn edge_time_rows(report: &tf_tree_ingest::IngestReport) -> Vec<EdgeTimeRow> {
    let mut rows: Vec<EdgeTimeRow> = report
        .edges
        .iter()
        .map(|e| {
            (
                e.parent.clone(),
                e.child.clone(),
                e.samples,
                e.source_oldest_ns,
                e.source_newest_ns,
            )
        })
        .collect();
    rows.sort();
    rows
}

/// The message-time span of the chunk `write_mcap_chunked` damages, computed from
/// the corpus rather than written down as a constant.
fn damaged_chunk_span(messages: &[FixtureMessage], per_chunk: usize) -> (u64, u64) {
    let at = DAMAGED_CHUNK_ORDINAL as usize * per_chunk;
    let group = &messages[at..at + per_chunk];
    let times: Vec<u64> = group
        .iter()
        .map(|m| u64::try_from(m.log_time_ns).unwrap())
        .collect();
    (*times.iter().min().unwrap(), *times.iter().max().unwrap())
}

/// **The hand-rolled writer produces a recording, not an approximation of one.**
///
/// The same corpus written twice — once through `mcap::Writer`, once through the
/// hand-rolled chunk writer with no summary section — ingests to the same
/// transforms, the same edges and the same per-edge sample counts.
///
/// This is the regression guard for the writer itself, and everything below
/// depends on it: a writer whose clean bytes were subtly wrong would make every
/// damage test meaningless, because the fault the reader reported would be the
/// writer's rather than the damage's.
///
/// Mutant: emit the `Channel` record *before* the `Schema` record in
/// `chunked_mcap_bytes` — applied, and this failed with `NoTransforms`, because
/// `handle_record` classifies a channel against the schema ids it has seen *so
/// far* and had seen none. Mutant 2: number a message's `channel_id` one higher
/// than its channel's (`channel_id(slot + 1)`) — applied, and this failed on
/// `transforms_read`, at 2 against 162: `small_recording` declares its two static
/// edges first, so `/tf_static` is `topics[0]` and gets channel id 1 while `/tf`
/// gets 2. Under the mutant `/tf_static`'s messages carry id 2 — `/tf`'s channel —
/// and `/tf`'s carry id 3, which no `Channel` record declares, so the two surviving
/// transforms are the static ones and the 160 dynamic ones are the silent drop.
/// (An earlier revision of this note had the two topics the wrong way round, which
/// would send the next reader to assert on `/tf`'s sample count — the one number
/// the mutant zeroes.)
///
/// **This is also the regression the codec work most risks**, which is why it is
/// named rather than left as one of the crowd: the uncompressed path is now one
/// branch of a `match` on the codec, and it must still be returned by borrow with no
/// decoder anywhere near it. Mutant: `ChunkCodec::parse("")` → `Self::Other` —
/// applied, and this failed with `the recording uses an unrecognised
/// codec-compressed chunks, which this build cannot read`. **42 of the crate's 94
/// tests died with it**, which is the right shape for a one-line change to codec
/// classification: every uncompressed chunk in every recording refused.
///
/// A third mutant **survived** and is worth recording: dropping the `+ 1` from
/// `channel_id`, so channels are numbered from `0`. Nothing in this crate reserves
/// channel id `0` — only `schema_id` `0` is special, meaning "no schema" — and the
/// writer uses the same function for the `Channel` record and for every `Message`
/// that names it, so the numbering is internally consistent and no reader can
/// tell. The one-based scheme is a fidelity choice (it is what a recorder writes),
/// not a property any test holds.
#[test]
fn an_uncompressed_chunked_recording_still_ingests() {
    let dir = Scratch::new("chunked_clean");
    let messages = small_recording();

    let by_crate = write(&dir, "crate.mcap", &messages);
    let by_hand = dir.path("hand.mcap");
    write_mcap_chunked(&by_hand, &messages, ChunkedSpec::new(60)).unwrap();

    let mut f1 = Frames::default();
    let a = tf_tree_ingest::run(&by_crate, &IngestOptions::default(), &mut f1)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &f1)));
    let mut f2 = Frames::default();
    let b = tf_tree_ingest::run(&by_hand, &IngestOptions::default(), &mut f2)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &f2)));

    assert_eq!(b.survey.transforms_read, a.survey.transforms_read);
    assert_eq!(b.report.samples_pushed, a.report.samples_pushed);
    assert_eq!(b.report.samples_pushed, 160, "the fixture's own count");
    assert_eq!(b.report.static_edges, 2);
    assert_eq!(b.report.dynamic_edges, 3);
    // Times as well as counts: the two writers must agree on *when* each edge was
    // published, not merely on how much of it there was.
    assert_eq!(edge_time_rows(&b.report), edge_time_rows(&a.report));
    // Nothing about a summary-free file is anomalous, and the reader must not
    // mistake the absence of one for a truncation.
    assert!(!b.report.anomalies.truncated);
    assert_eq!(b.report.anomalies.bad_chunks, 0);
}

/// **THE CENTREPIECE: one corrupt chunk does not lose the recording.**
///
/// Under the default [`OnBadChunk::Skip`] the damaged second chunk is skipped and
/// the chunks either side of it are read in full — so the loss is bounded by the
/// chunk rather than by "everything after the first fault", which is what a reader
/// that stopped at the first bad chunk would give and would look identical on a
/// fixture whose damage was in the last chunk.
///
/// **"Bounded by the chunk" is a property of this damage class, not of the skip
/// policy.** `FlippedBitInRecords` is caught by `chunk_records` before a single
/// record is handed to the callback, so the whole chunk is lost and the whole chunk
/// is reported. A fault the *framing walk* raises arrives after earlier records have
/// already been delivered, and there the boundary is a record while the report still
/// blames the chunk — see
/// `a_framing_fault_mid_chunk_keeps_what_it_already_delivered`, which measures that
/// case rather than leaving this sentence to be read as covering it.
///
/// The counts are asserted exactly, not as "more than zero": three samples for
/// `sensor_a` and three for `sensor_c`, and `sensor_b` — whose three messages were
/// all in the damaged chunk — absent from the report and unqueryable in the tree.
///
/// Mutant: in `read_chunk`, return the fault instead of calling `note_or_fail`
/// (i.e. no skip policy at all) — applied, and this failed with "chunk 1 is
/// unreadable: its CRC32 is 0x91c0fdca but the data hashes to 0x77cb8a16": the
/// whole recording lost to one bad chunk, which is the behaviour the policy exists
/// to replace. Mutant 2: `break` out of `read_tf`'s record loop once
/// `skips.bad_chunks > 0` — applied, and the edge rows came back as
/// `[("base_link", "sensor_a", 3)]` alone: a skip that silently ends the read costs
/// everything after the fault while still reporting exactly one bad chunk, which is
/// the failure this test's *third* chunk exists to catch.
#[test]
fn one_corrupt_chunk_does_not_lose_the_recording() {
    let dir = Scratch::new("chunk_skip");
    let messages = three_sensors_nine_messages();
    let path = dir.path("damaged.mcap");
    write_mcap_chunked(
        &path,
        &messages,
        ChunkedSpec::new(3).damaged(ChunkDamage::FlippedBitInRecords),
    )
    .unwrap();

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));

    assert_eq!(
        edge_rows(&out.report),
        vec![
            ("base_link".to_string(), "sensor_a".to_string(), 3),
            ("base_link".to_string(), "sensor_c".to_string(), 3),
        ],
        "the first and third chunks must survive whole, and only the second is lost"
    );
    assert_eq!(out.report.samples_pushed, 6);
    assert_eq!(out.report.anomalies.bad_chunks, 1);

    // The survivors are usable, not merely counted.
    for (child, stamp) in [("sensor_a", 1_010_000_000i64), ("sensor_c", 1_070_000_000)] {
        let iso = out
            .tree
            .lookup("base_link", child, Stamp::<SystemDomain>::from_nanos(stamp))
            .unwrap_or_else(|e| panic!("{child} at {stamp} should resolve: {e:?}"));
        assert!(iso.t.x.is_finite() && (iso.q.norm() - 1.0).abs() < 1e-12);
    }
    // And the loss is real: `sensor_b` is not silently present with garbage.
    assert!(
        out.tree
            .lookup(
                "base_link",
                "sensor_b",
                Stamp::<SystemDomain>::from_nanos(1_040_000_000)
            )
            .is_err(),
        "the skipped chunk's edge must be absent, not empty-but-present"
    );
}

/// **A framing fault mid-chunk keeps every record it had already delivered, and the
/// report still blames the whole chunk.** Pinned because the two halves of that
/// sentence disagree, and nothing else in the crate notices.
///
/// `ChunkDamage::InnerRecordRunsPastTheEnd` is caught by `for_each_record`, not by
/// `chunk_records`: the walk reaches the last record's inflated length only after
/// `handle_record` has accepted the two before it. So `note_or_fail` counts one
/// skipped chunk and reports the chunk header's whole span as lost, while two of the
/// three transforms in that span are in the tree and `sensor_b` looks like a
/// complete edge at two samples. An operator reading "the transforms between
/// 1.03 s and 1.05 s are missing" is being told something false about two thirds of
/// that window.
///
/// **Reported as a `source.rs` finding, not fixed here** — this PR adds a fixture
/// and does not touch the read path. The candidate fix is for `read_chunk` to report
/// what a partly-walked chunk actually cost (a record count, or a span narrowed to
/// the records that did not arrive) rather than the header's span. This test exists
/// so that fix has an exact set of numbers to change, and so nobody reads
/// `one_corrupt_chunk_does_not_lose_the_recording`'s "bounded by the chunk" as
/// covering this class.
///
/// Mutant: in `read_chunk`, pass `tolerate_tail = true` unconditionally instead of
/// `!complete` — applied, and this failed on `bad_chunks`, `0` against `1`: an inner
/// record whose body runs past the end of the records field is then read as a
/// trailing fragment, so the same eight samples arrive and the recording is declared
/// whole. The partial ingest is then invisible instead of merely mis-reported, which
/// is the worse of the two.
#[test]
fn a_framing_fault_mid_chunk_keeps_what_it_already_delivered() {
    let dir = Scratch::new("chunk_inner_framing");
    let messages = three_sensors_nine_messages();
    let path = dir.path("damaged.mcap");
    write_mcap_chunked(
        &path,
        &messages,
        ChunkedSpec::new(3).damaged(ChunkDamage::InnerRecordRunsPastTheEnd),
    )
    .unwrap();

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));

    // Eight, not six: the damaged chunk's first two messages were delivered before
    // the walk reached the record whose length is a lie.
    assert_eq!(out.report.samples_pushed, 8);
    // Two of `sensor_b`'s three, and their stamps say *which* two: the report shows a
    // plausible-looking edge whose window ends one message early.
    assert_eq!(
        edge_time_rows(&out.report),
        vec![
            (
                "base_link".to_string(),
                "sensor_a".to_string(),
                3,
                Some(1_000_000_000),
                Some(1_020_000_000)
            ),
            (
                "base_link".to_string(),
                "sensor_b".to_string(),
                2,
                Some(1_030_000_000),
                Some(1_040_000_000)
            ),
            (
                "base_link".to_string(),
                "sensor_c".to_string(),
                3,
                Some(1_060_000_000),
                Some(1_080_000_000)
            ),
        ],
        "the damaged chunk's edge survives partially, which is the finding"
    );
    // And yet the report accounts for the whole chunk, span included.
    assert_eq!(out.report.anomalies.bad_chunks, 1);
    let (lo, hi) = damaged_chunk_span(&messages, 3);
    assert_eq!(out.report.anomalies.bad_chunk_span_ns, Some((lo, hi)));
    // The contradiction in one assertion: a transform the report calls lost is
    // queryable, at a stamp inside the span the report calls lost.
    let stamp = 1_040_000_000i64;
    assert!(
        i64::try_from(lo).unwrap() <= stamp && stamp <= i64::try_from(hi).unwrap(),
        "the retained stamp must fall inside the reported loss"
    );
    out.tree
        .lookup(
            "base_link",
            "sensor_b",
            Stamp::<SystemDomain>::from_nanos(stamp),
        )
        .unwrap_or_else(|e| panic!("sensor_b at {stamp} was delivered: {e:?}"));
}

/// A skipped chunk is **counted**, and the report says *when* the recording lost
/// data.
///
/// A count alone is not actionable — "one chunk was unreadable" tells an operator
/// nothing they can do. The span comes from the damaged chunk's own header and is
/// asserted against the corpus's real log times, which is why
/// `chunked_mcap_bytes` writes the true min/max rather than zero: `chunk_span`
/// reports both-zero as "this writer did not track message times", so a lazy
/// fixture would make this assertion pass while measuring nothing.
///
/// Mutant: delete `skips.bad_chunks += 1` in `source.rs`'s `note_or_fail` —
/// applied, and this failed on `bad_chunks` (`0` against the `1` expected). The
/// ingest still succeeded, and because the report's skip row is gated on
/// `bad_chunks > 0` the summary then said nothing at all about the chunk: the
/// mutant is precisely "silently lose part of the recording". Mutant 2: neutralise
/// the `if let Some((lo, hi)) = span` update so `bad_chunk_span_ns` stays `None` —
/// applied, and this failed with `None` against
/// `Some((1030000000, 1050000000))`, the summary degrading to "their headers named
/// no message times" for a recording whose headers name them.
#[test]
fn a_corrupt_chunk_is_counted_and_its_span_reported() {
    let dir = Scratch::new("chunk_span");
    let messages = three_sensors_nine_messages();
    let path = dir.path("damaged.mcap");
    write_mcap_chunked(
        &path,
        &messages,
        ChunkedSpec::new(3).damaged(ChunkDamage::UncompressedCrc),
    )
    .unwrap();

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));

    assert_eq!(out.report.anomalies.bad_chunks, 1);
    let (lo, hi) = damaged_chunk_span(&messages, 3);
    assert_eq!(
        out.report.anomalies.bad_chunk_span_ns,
        Some((lo, hi)),
        "the reported span must be the damaged chunk's real message times"
    );
    let summary = out.report.summary();
    assert!(
        summary.contains("chunk(s) were unreadable"),
        "the report must surface the skip:\n{summary}"
    );
    assert!(
        summary.contains(&lo.to_string()) && summary.contains(&hi.to_string()),
        "the report must place the loss in time:\n{summary}"
    );
}

/// Under `--on-bad-chunk halt` the ingest **fails, naming the chunk**, and the
/// ordinal is the damaged chunk's own.
///
/// The ordinal is asserted to be `1` rather than merely "some number", because an
/// off-by-one in `read_tf`'s `chunk_ordinal` is otherwise invisible: every other
/// assertion in this file passes either way, and the number's whole purpose is to
/// be compared against `mcap info`'s chunk numbering by a human looking at a
/// damaged recording.
///
/// Mutant: pass `chunk_ordinal` instead of `chunk_ordinal - 1` to `read_chunk`, so
/// the ordinal becomes one-based — applied, and this failed with `2` against the
/// `1` expected. Mutant 2: in `note_or_fail`, drop the
/// `|| on_bad_chunk == OnBadChunk::Halt` term so `Halt` skips like `Skip` —
/// applied, and this failed on `unwrap_err`, the survey having returned six
/// transforms and `bad_chunks: 1` to a user who asked to be *told* the recording is
/// not whole.
#[test]
fn a_corrupt_chunk_under_halt_names_the_chunk_ordinal() {
    let dir = Scratch::new("chunk_halt");
    let path = dir.path("damaged.mcap");
    write_mcap_chunked(
        &path,
        &three_sensors_nine_messages(),
        ChunkedSpec::new(3).damaged(ChunkDamage::UncompressedCrc),
    )
    .unwrap();

    let opts = IngestOptions {
        on_bad_chunk: OnBadChunk::Halt,
        ..IngestOptions::default()
    };
    let mut frames = Frames::default();
    let err = tf_tree_ingest::survey(&path, &opts, &mut frames).unwrap_err();
    match err {
        IngestError::BadChunk { chunk, kind } => {
            assert_eq!(
                chunk, DAMAGED_CHUNK_ORDINAL,
                "the second chunk is ordinal 1, zero-based"
            );
            assert!(matches!(kind, BadChunkKind::Crc { .. }), "got {kind:?}");
        }
        other => panic!("expected BadChunk, got {other:?}"),
    }
    let text = err.to_string();
    assert!(text.contains("chunk 1"), "the message must name it: {text}");
}

/// **An unknown codec is a hard error, not a skip — even under
/// [`OnBadChunk::Skip`].**
///
/// Every chunk in a recording uses the same codec, so skipping them all would
/// answer a question nobody asked: `NoTransforms`, or a partial count, about a
/// file that is perfectly intact and needs one `mcap compress` command. The
/// relabelled-codec fixture is the only way to reach this from a file whose bytes
/// are otherwise clean.
///
/// **`"brotli"` and not `"zstd"`, and that changed with this build.** zstd and lz4
/// are now decoded, so a chunk relabelled `"zstd"` is a chunk that lies about its
/// payload — damage, and skippable, which
/// [`a_mislabelled_codec_is_damage_not_an_unsupported_codec`] covers. What remains
/// unsupported is a name outside the MCAP specification entirely, and *that* must
/// not degrade into a damaged-chunk skip just because the name is unrecognised.
///
/// Mutant: in `note_or_fail`, make `ChunkFault::Unsupported` skippable
/// (`let skippable = !matches!(fault, ChunkFault::Callback(_))`) — applied, and
/// this failed with "a brotli chunk must not be skipped under the default policy;
/// got 6 of 9 transforms and 1 bad chunk(s)". The survey succeeded and blamed an
/// unreadable chunk, which explains nothing about the compression that is the
/// actual cause and hides the one-command remedy. On a real recording, where
/// *every* chunk is compressed, the same mutant yields `NoTransforms` about an
/// intact file.
#[test]
fn an_unknown_codec_in_a_chunk_is_a_hard_error_not_a_skip() {
    let dir = Scratch::new("chunk_codec");
    let name = "brotli";
    let path = dir.path("brotli.mcap");
    write_mcap_chunked(
        &path,
        &three_sensors_nine_messages(),
        ChunkedSpec::new(3).damaged(ChunkDamage::Relabelled(name)),
    )
    .unwrap();
    let mut frames = Frames::default();
    // The **default** policy, which is `Skip`: this must fail anyway.
    let err = match tf_tree_ingest::survey(&path, &IngestOptions::default(), &mut frames) {
        Err(e) => e,
        Ok(s) => panic!(
            "a {name} chunk must not be skipped under the default policy; \
             got {} of 9 transforms and {} bad chunk(s)",
            s.transforms_read, s.anomalies.bad_chunks
        ),
    };
    assert_eq!(
        err,
        IngestError::CompressedChunk {
            codec: tf_tree_ingest::ChunkCodec::Other
        }
    );
    assert!(
        err.to_string().contains("cannot read"),
        "the message must name the build's limitation: {err}"
    );
}

/// **A chunk that claims a codec it does not carry is damage, and one damaged
/// chunk must not cost the recording.**
///
/// The two failures are a step apart and want opposite policies. An *unsupported*
/// codec is never skippable, because every chunk in the file uses it. A chunk that
/// says `"zstd"` over bytes that are not zstd is one bad chunk, and the skip policy
/// is exactly what recovers the other two.
///
/// Mutant: in `decompress::decode_zstd`, map every `Err` — not only
/// `TargetTooSmall` — through `ChunkFault::Unsupported` instead of
/// `BadChunkKind::Decompress` — applied, and this failed on `unwrap_or_else` with
/// `CompressedChunk`: one mislabelled chunk in a 400 000-chunk recording would take
/// the whole file with it, and the diagnosis would blame a codec the build has.
#[cfg(feature = "compression")]
#[test]
fn a_mislabelled_codec_is_damage_not_an_unsupported_codec() {
    let dir = Scratch::new("chunk_mislabelled");
    for name in ["zstd", "lz4"] {
        let path = dir.path(&format!("{name}.mcap"));
        write_mcap_chunked(
            &path,
            &three_sensors_nine_messages(),
            ChunkedSpec::new(3).damaged(ChunkDamage::Relabelled(name)),
        )
        .unwrap();
        let mut frames = Frames::default();
        let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
            .unwrap_or_else(|e| panic!("{name}: {}", tf_tree_ingest::describe(e, &frames)));
        assert_eq!(
            out.report.anomalies.bad_chunks, 1,
            "{name}: the mislabelled chunk is one skippable bad chunk"
        );
        assert_eq!(
            out.report.samples_pushed, 6,
            "{name}: the chunks either side of it survive"
        );
    }
}

/// **A skipped chunk that carried the only `Channel` record silently drops every
/// later message.** This test exists to document that, not to celebrate it.
///
/// It is the caveat [`OnBadChunk`]'s doc comment promises the report will surface,
/// and it is only constructible because the hand-rolled writer emits **no summary
/// section** — with one, the `Channel` record is repeated at the end of the file
/// and the loss does not happen.
///
/// The control matters as much as the case: the same layout *undamaged* ingests the
/// six messages that follow the definitions, and drops the three that precede them
/// because MCAP is read in order. So the damaged run's loss of all nine is caused
/// by the skip and not by the layout.
///
/// **What actually happens is worse than the docs imply, and is reported as a
/// finding rather than fixed here**: the dropped messages have no counter, `survey`
/// returns `NoTransforms` when no edge survives, and an error carries no anomalies —
/// so in exactly the case the caveat is about, the report that was supposed to
/// surface the skip does not exist. The user is told "the recording contains no
/// tf2_msgs/msg/TFMessage transforms" about a file that contains nine, with no
/// mention of the chunk. `OnBadChunk`'s doc comment now carries that caveat and
/// names this test, so the promise and the behaviour no longer contradict each other
/// while the fix waits; the fix itself — a counter for unknown-channel drops and an
/// error variant beside `TruncatedBeforeAnyChunk` for "chunks were skipped and
/// nothing survived" — is a `source.rs` change and belongs to its own commit.
///
/// Mutant: none applied — this test pins observed behaviour rather than a guard,
/// and the property is **structurally guaranteed** by the absence of a summary
/// section plus `handle_record`'s unknown-channel early return, which has no
/// counter to break. What it does defend is the *diagnosis*: if a later commit
/// makes this case report anything at all, this test fails and has to be rewritten,
/// which is the review this behaviour deserves.
#[test]
fn a_skipped_chunk_that_carried_the_only_channel_drops_the_rest() {
    let dir = Scratch::new("chunk_defs");
    let messages = three_sensors_nine_messages();

    // The control: definitions in the second chunk, nothing damaged.
    let control = dir.path("control.mcap");
    write_mcap_chunked(
        &control,
        &messages,
        ChunkedSpec::new(3).definitions_in_damaged_chunk(),
    )
    .unwrap();
    let mut f1 = Frames::default();
    let ok = tf_tree_ingest::run(&control, &IngestOptions::default(), &mut f1)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &f1)));
    assert_eq!(
        edge_rows(&ok.report),
        vec![
            ("base_link".to_string(), "sensor_b".to_string(), 3),
            ("base_link".to_string(), "sensor_c".to_string(), 3),
        ],
        "messages before the Channel record belong to an unknown channel and are \
         dropped; that is MCAP's ordering, not the skip"
    );
    assert_eq!(ok.report.anomalies.bad_chunks, 0);

    // The case: the same layout, with that chunk damaged.
    let damaged = dir.path("damaged.mcap");
    write_mcap_chunked(
        &damaged,
        &messages,
        ChunkedSpec::new(3)
            .definitions_in_damaged_chunk()
            .damaged(ChunkDamage::FlippedBitInRecords),
    )
    .unwrap();
    let mut f2 = Frames::default();
    let err = tf_tree_ingest::survey(&damaged, &IngestOptions::default(), &mut f2).unwrap_err();
    assert_eq!(
        err,
        IngestError::NoTransforms,
        "losing the only Channel record loses every message in the file"
    );
    // **The finding, asserted so it cannot quietly change:** the diagnosis does not
    // name the chunk that was skipped, because a failed survey has no report to name
    // it from. Asserted on the ordinal rather than on the word "chunk", which a
    // reword aimed at some other user (a hint about compressed chunks, say) would
    // trip without telling anyone anything about this behaviour.
    let text = tf_tree_ingest::describe(err, &f2).to_string();
    assert!(
        !text.contains(&format!("chunk {DAMAGED_CHUNK_ORDINAL}")),
        "if the diagnosis has improved to name the skip, this test must be rewritten \
         to match: {text}"
    );
}

/// A lying `uncompressed_size` on an **uncompressed** chunk is refused, with no
/// decoder involved.
///
/// The records are stored verbatim under `compression == ""`, so
/// `uncompressed_size == compressed_size == records.len()` is an invariant
/// checkable from the header alone. A previous revision of this test pinned the
/// *opposite* — the field was not retained, so a partially rewritten chunk header
/// (the size scrambled, `compressed_size` and the CRC in another sector and intact)
/// ingested with `bad_chunks == 0` — and said in its own wording that whichever
/// commit closed the gap had to rewrite it. This is that rewrite.
///
/// The other two chunks survive, which is the half that says the fix did not
/// become a reason to refuse the file: the damage is one skippable bad chunk.
///
/// Mutant: neutralise the `head.uncompressed_size != head.compressed_size` arm in
/// `decompress::chunk_records` — applied, and this failed on `bad_chunks`, `0`
/// against `1`, with the nine transforms all present. That is exactly the behaviour
/// the test this one replaced used to *assert*.
#[test]
fn a_lying_uncompressed_size_is_refused() {
    let dir = Scratch::new("chunk_usize");
    let path = dir.path("lying.mcap");
    write_mcap_chunked(
        &path,
        &three_sensors_nine_messages(),
        ChunkedSpec::new(3).damaged(ChunkDamage::UncompressedSizeTooLarge),
    )
    .unwrap();

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));
    assert_eq!(
        out.report.anomalies.bad_chunks, 1,
        "the header lies, and the reader must say so"
    );
    assert_eq!(
        out.report.samples_pushed, 6,
        "and only that chunk is lost, not the recording"
    );

    // Under `halt` the fault is named, and it is a length disagreement rather than
    // a CRC one — the check is on the header's own two numbers, so it fires before
    // anything hashes.
    let opts = IngestOptions {
        on_bad_chunk: OnBadChunk::Halt,
        ..IngestOptions::default()
    };
    let mut frames = Frames::default();
    let err = tf_tree_ingest::survey(&path, &opts, &mut frames).unwrap_err();
    match err {
        IngestError::BadChunk { chunk, kind } => {
            assert_eq!(chunk, DAMAGED_CHUNK_ORDINAL);
            assert!(
                matches!(kind, BadChunkKind::LengthMismatch { .. }),
                "got {kind:?}"
            );
        }
        other => panic!("expected BadChunk, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Compressed recordings — the case rosbag2 and Foxglove actually write.
//
// Every fixture below is compressed by the same crates that read it back, which
// proves round-trip and **not** conformance. `a_real_libzstd_recording_ingests`
// closes that gap for zstd against the host's `zstd` CLI; there is no `lz4` CLI
// here, so lz4 has round-trip coverage only, and this comment is the honest
// statement of that asymmetry rather than a claim that both are equal.
// ---------------------------------------------------------------------------

/// **A zstd recording ingests byte-for-byte identically to the uncompressed one.**
///
/// The comparison is against the *same corpus* written uncompressed by the same
/// writer, so the only variable is the codec. Sample counts alone would not be
/// enough — a decoder that dropped a block and a writer that dropped a message look
/// the same in a count — so the per-edge source time bounds are compared too, and
/// the tree is queried at a stamp inside the compressed chunk.
///
/// Mutant: in `decompress::decode_zstd`, hand `decode_all` a slice of `want - 1`
/// bytes — applied, and this failed with `zstd packed: the recording contains no
/// tf2_msgs/msg/TFMessage transforms`: every chunk in the file over-runs its
/// one-byte-short buffer, all of them are skipped, and an intact recording reads as
/// empty. It killed six other tests with it, `a_real_libzstd_recording_ingests`
/// included.
#[cfg(feature = "compression")]
#[test]
fn a_zstd_recording_ingests_identically() {
    a_compressed_recording_ingests_identically(FixtureCodec::Zstd, "zstd");
}

/// **An lz4 recording ingests byte-for-byte identically to the uncompressed one.**
///
/// Mutant: use `lz4_flex::block::decompress` in place of the frame decoder —
/// applied, and this failed with `lz4 packed: the recording contains no
/// tf2_msgs/msg/TFMessage transforms`. Every chunk faults, because MCAP's `"lz4"` is
/// the LZ4 *frame* container and the block decoder does not understand its 4-byte
/// magic. That is the silent-correctness trap the crate `#[deprecated]`s its own
/// block re-exports to prevent, and it is why this test exists separately from the
/// zstd one rather than as a loop inside it.
#[cfg(feature = "compression")]
#[test]
fn an_lz4_recording_ingests_identically() {
    a_compressed_recording_ingests_identically(FixtureCodec::Lz4, "lz4");
}

/// The body of the two tests above: same corpus, same writer, one codec apart.
#[cfg(feature = "compression")]
fn a_compressed_recording_ingests_identically(codec: FixtureCodec, tag: &str) {
    let dir = Scratch::new(&format!("codec_{tag}"));
    let messages = small_recording();
    let spec = ChunkedSpec::new(60);

    let plain = dir.path("plain.mcap");
    write_mcap_chunked(&plain, &messages, spec).unwrap();
    let packed = dir.path("packed.mcap");
    write_mcap_chunked(&packed, &messages, spec.compressed(codec)).unwrap();

    // The fixture really is smaller, so "identical" below is not two readings of
    // the same bytes.
    let (plain_len, packed_len) = (
        std::fs::metadata(&plain).unwrap().len(),
        std::fs::metadata(&packed).unwrap().len(),
    );
    assert!(
        packed_len < plain_len,
        "{tag}: the compressed fixture ({packed_len} B) is not smaller than the \
         uncompressed one ({plain_len} B), so this test compares nothing"
    );

    let mut f1 = Frames::default();
    let a = tf_tree_ingest::run(&plain, &IngestOptions::default(), &mut f1)
        .unwrap_or_else(|e| panic!("{tag} plain: {}", tf_tree_ingest::describe(e, &f1)));
    let mut f2 = Frames::default();
    let b = tf_tree_ingest::run(&packed, &IngestOptions::default(), &mut f2)
        .unwrap_or_else(|e| panic!("{tag} packed: {}", tf_tree_ingest::describe(e, &f2)));

    assert_eq!(b.survey.transforms_read, a.survey.transforms_read, "{tag}");
    assert_eq!(b.report.samples_pushed, a.report.samples_pushed, "{tag}");
    assert_eq!(
        b.report.samples_pushed, 160,
        "{tag}: the fixture's own count"
    );
    assert_eq!(
        edge_time_rows(&b.report),
        edge_time_rows(&a.report),
        "{tag}"
    );
    assert_eq!(b.report.static_edges, 2, "{tag}");
    assert_eq!(b.report.dynamic_edges, 3, "{tag}");
    assert!(!b.report.anomalies.truncated, "{tag}");
    assert_eq!(b.report.anomalies.bad_chunks, 0, "{tag}");

    // And the tree answers, at a stamp that lives in a compressed chunk.
    let iso = b
        .tree
        .lookup(
            "map",
            "laser",
            Stamp::<SystemDomain>::from_nanos(1_500_000_000),
        )
        .unwrap_or_else(|e| panic!("{tag}: the compressed recording must resolve: {e:?}"));
    assert!(
        iso.t.x.is_finite() && (iso.q.norm() - 1.0).abs() < 1e-12,
        "{tag}"
    );
}

/// **A short decompression is not read as a short recording.**
///
/// The header declares 64 bytes more than the codec produces. Without the
/// produced-against-declared check the tail of the output buffer is zeros, which
/// frame as empty records and lose the chunk's real content *silently* — the
/// walk stops on a clean boundary with nothing to complain about, which is the
/// worst shape a data-loss bug can take.
///
/// Both codecs, because each detects it differently: zstd by `decode_all` returning
/// a short count, lz4 by the byte count `read_to_end` reached.
///
/// **Mutant: in `decode_zstd`, replace `Ok(written) if written == want` with `Ok(_)`
/// — applied, and this test still passed.** The reason is worth recording rather
/// than papering over: this fixture carries an honest `uncompressed_crc`, and a
/// short decode leaves zero padding that the CRC over the whole records field also
/// rejects. So end to end the property is guarded **twice**, and no single mutant of
/// either check kills this test.
///
/// The length check is isolated one level down, by
/// `decompress::tests::each_codec_round_trips_and_catches_both_length_disagreements`,
/// whose CRC-0 rows remove the backstop — `uncompressed_crc == 0` means "not
/// computed" per the specification and real writers emit it — and which that same
/// mutant does kill, with the zero padding visible in the returned records.
///
/// What this test does catch, observed: swapping lz4's frame decoder for the block
/// API (`samples_pushed` 9 against 6), and a fixture that writes the codec name
/// without compressing. It is the end-to-end statement of the property; the unit
/// test is the one that says *which* check enforces it.
#[cfg(feature = "compression")]
#[test]
fn a_short_decompression_is_not_read_as_a_short_recording() {
    let dir = Scratch::new("codec_short");
    let messages = three_sensors_nine_messages();
    for (codec, tag) in [(FixtureCodec::Zstd, "zstd"), (FixtureCodec::Lz4, "lz4")] {
        let path = dir.path(&format!("{tag}.mcap"));
        write_mcap_chunked(
            &path,
            &messages,
            ChunkedSpec::new(3)
                .compressed(codec)
                .damaged(ChunkDamage::UncompressedSizeTooLarge),
        )
        .unwrap();

        let mut frames = Frames::default();
        let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
            .unwrap_or_else(|e| panic!("{tag}: {}", tf_tree_ingest::describe(e, &frames)));
        assert_eq!(
            out.report.anomalies.bad_chunks, 1,
            "{tag}: the length disagreement must be reported"
        );
        assert_eq!(
            out.report.samples_pushed, 6,
            "{tag}: and the chunk's records must not be handed over half-formed"
        );
    }
}

/// **A truncated *compressed* recording is reported as truncated, not as corrupt.**
///
/// A partial codec frame is not decodable, so the cut chunk's records are lost —
/// that is the one thing compression costs, and `decompress`'s module docs have
/// always said so. What must not happen is `bad_chunks` counting it: nothing is
/// wrong with the file beyond where it stops, and telling an operator their
/// recording is damaged sends them looking for a bad disk.
///
/// The complete chunks before the cut are read in full, which is what separates
/// this from "the reader gave up".
///
/// Mutant: in `chunk_records`, neutralise the `if !complete { return Ok(&[]) }` arm
/// so the truncated payload flows on — applied, and this failed on `bad_chunks`,
/// `1` against `0`: the cut chunk faults (as a length disagreement, since
/// `compressed_size` names bytes that were never written) and `note_or_fail` counts
/// it as damage.
#[cfg(feature = "compression")]
#[test]
fn a_truncated_compressed_recording_is_truncated_not_corrupt() {
    let dir = Scratch::new("codec_truncated");
    let messages = three_sensors_nine_messages();
    for (codec, tag) in [(FixtureCodec::Zstd, "zstd"), (FixtureCodec::Lz4, "lz4")] {
        let whole = tf_tree_ingest::fixture::chunked_mcap_bytes(
            &messages,
            ChunkedSpec::new(3).compressed(codec),
        )
        .unwrap();
        // **Cut four bytes into the last chunk's payload**, so two chunks are
        // complete and the third is a partial codec frame.
        //
        // Derived from the writer's own tail rather than guessed: after the last
        // chunk record come a `DataEnd` (9 + 4), a `Footer` (9 + 20) and the
        // 8-byte end magic, so the last chunk record's body ends
        // `HAND_ROLLED_TAIL` bytes from the end. An earlier revision of this test
        // cut at a fixed offset from the end and landed in the `Footer` instead,
        // where every chunk is complete and the assertions below passed for the
        // wrong reason.
        const HAND_ROLLED_TAIL: usize = (9 + 4) + (9 + 20) + 8;
        let cut = whole.len() - HAND_ROLLED_TAIL - 4;
        let path = dir.path(&format!("{tag}.mcap"));
        std::fs::write(&path, &whole[..cut]).unwrap();

        let mut frames = Frames::default();
        let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
            .unwrap_or_else(|e| panic!("{tag}: {}", tf_tree_ingest::describe(e, &frames)));
        assert!(
            out.report.anomalies.truncated,
            "{tag}: the recording is a prefix and the report must say so"
        );
        assert_eq!(
            out.report.anomalies.bad_chunks, 0,
            "{tag}: an incomplete file is not a damaged one"
        );
        assert_eq!(
            out.report.samples_pushed, 6,
            "{tag}: the two complete chunks are read in full"
        );
    }
}

/// **A recording compressed by real libzstd ingests, with the same transforms as
/// the uncompressed equivalent.**
///
/// Every other compressed fixture in this repository is encoded by `ruzstd`, the
/// same crate that decodes it. That proves round-trip and **not** conformance: an
/// encoder and a decoder from one crate can agree with each other and both
/// disagree with the zstd that rosbag2 links. `testdata/zstd_conformance.mcap`'s
/// chunk payloads were produced by the `zstd` CLI, version 1.5.5, i.e. by libzstd
/// itself; `testdata/ATTRIBUTION.md` records the command line and
/// `examples/gen_zstd_conformance.rs` regenerates it.
///
/// **A missing file fails loudly.** A `#[ignore]` or an early `return` here would
/// leave the only conformance evidence in the repository silently unchecked, which
/// is worse than not having it.
///
/// Mutant: byte 300 of the committed file flipped, which the framing walk locates
/// inside the first chunk's libzstd payload (the first chunk record starts at 92 and
/// its payload at 145) — applied, and this failed with `the recording contains no
/// tf2_msgs/msg/TFMessage transforms`: the corrupted frame no longer decodes, every
/// chunk after it is fine but the definitions were in the first, so nothing
/// survives. Mutant 2: in `decode_zstd`, feed `decode_all` `&payload[1..]` —
/// applied, and this failed the same way, i.e. libzstd's frame magic is really being
/// parsed rather than skipped past.
///
/// A third mutant is worth recording because it **survived**: flipping byte 80,
/// which lands in the `Header` record's `library` string rather than in a chunk.
/// Nothing in this crate reads that field, so the file is still a valid recording of
/// the same transforms. It is the check that the byte offsets above were located
/// rather than guessed.
#[cfg(feature = "compression")]
#[test]
fn a_real_libzstd_recording_ingests() {
    let path = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/zstd_conformance.mcap"
    ));
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        panic!(
            "the committed libzstd conformance fixture is missing or unreadable \
             ({}): {e}. It is the only evidence in this repository that the \
             pure-Rust decoder agrees with libzstd; regenerate it with \
             `cargo run -p tf_tree_ingest --features fixture \
             --example gen_zstd_conformance` (needs the `zstd` CLI) rather than \
             skipping this test.",
            path.display()
        )
    });
    assert!(
        bytes.len() < 64 * 1024,
        "the fixture is meant to stay a few kilobytes; it is {} B",
        bytes.len()
    );

    let mut frames = Frames::default();
    let real = tf_tree_ingest::run(path, &IngestOptions::default(), &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));
    assert_eq!(real.report.anomalies.bad_chunks, 0);
    assert!(!real.report.anomalies.truncated);

    // The control: the same corpus, uncompressed, written here and now. Comparing
    // against a number written down would pass just as well against a fixture
    // generated from the wrong corpus.
    let dir = Scratch::new("conformance_control");
    let control = dir.path("control.mcap");
    write_mcap_chunked(
        &control,
        &conformance_recording(),
        ChunkedSpec::new(CONFORMANCE_MESSAGES_PER_CHUNK),
    )
    .unwrap();
    let mut f2 = Frames::default();
    let plain = tf_tree_ingest::run(&control, &IngestOptions::default(), &mut f2)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &f2)));

    assert_eq!(
        real.survey.transforms_read, plain.survey.transforms_read,
        "libzstd's chunks must decode to the same transforms as the uncompressed \
         corpus; if the corpus moved, regenerate the fixture"
    );
    assert_eq!(real.report.samples_pushed, plain.report.samples_pushed);
    assert_eq!(edge_time_rows(&real.report), edge_time_rows(&plain.report));
}
