//! End-to-end ingest over synthetic MCAP fixtures — `docs/PHASE5.md` §3 and
//! §11's "ingest anomalies" and "out-of-order ingest" rows.
//!
//! Every recording here is fabricated by [`tf_tree_ingest::fixture`]; none of it
//! came off a robot. What is proved below is that *this crate* classifies a
//! stream the way §3.2 specifies, not that any real bag looks like these.

#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::path::{Path, PathBuf};

use tf_tree::{Stamp, SystemDomain};
use tf_tree_ingest::cdr::TransformStamped;
use tf_tree_ingest::fixture::{
    small_recording, two_publishers_with_latency, write_mcap, write_mcap_as, write_mcap_chunked,
    ChunkDamage, ChunkedSpec, FixtureMessage, DAMAGED_CHUNK_ORDINAL,
};
// Only the compressed tests use these, and they are `#[cfg(feature = "compression")]` — a
// plain `use` would be an unused import in the codec-free build, which `-D warnings`
// rejects.
#[cfg(feature = "compression")]
use tf_tree_ingest::fixture::{
    conformance_recording, FixtureCodec, CONFORMANCE_MESSAGES_PER_CHUNK,
};
use tf_tree_ingest::{
    BadChunkKind, ClockResetPolicy, Frames, IngestError, IngestOptions, OnBadChunk, TopicRoles,
};

/// A scratch directory that removes itself, so a failing test does not leave a recording
/// behind.
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

/// The ordinary path: a recording with statics and three dynamic edges at different rates
/// ingests into a tree whose lookups answer.
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

/// §11: *"shuffle a recording's messages; the resulting `.tft` must be byte-identical to
/// one built from the ordered source."* Tested here at the level this commit reaches — the
/// arena, which is what a `.tft` is a copy of.
#[test]
fn out_of_order_ingest_matches_ordered() {
    let dir = Scratch::new("shuffle");
    let ordered = small_recording();
    let mut shuffled = ordered.clone();
    // Interleave the halves back-to-front: every message still present, log order badly
    // wrong, statics no longer first.
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

    // **The reset detector has to be told to stand down for this one, and that is a real
    // property of the check rather than a workaround.** `ClockGuard` classifies a backward
    // stamp by how far back it goes, fed in *log* order; a recording is log-ordered, so its
    // inversions are milliseconds.
    let opts = IngestOptions {
        clock_reset_threshold_ns: i64::MAX,
        ..IngestOptions::default()
    };
    let mut f1 = Frames::default();
    let mut f2 = Frames::default();
    let a = tf_tree_ingest::run(&p1, &opts, &mut f1).unwrap();
    let b = tf_tree_ingest::run(&p2, &opts, &mut f2).unwrap();

    assert_eq!(a.report.samples_pushed, b.report.samples_pushed);

    // **Identity, not just value.** The amendment at `ingest::fill` has two halves —
    // canonical *edge* order and canonical *frame* order — and only the edge half is
    // observable through a `LookupError`.
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
    // Out-of-order arrivals are *counted* in the shuffled run and absent in the ordered one
    // — if they were zero in both, the shuffle did nothing and this test would be vacuous.
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

/// §3.2: a duplicate `(edge, stamp)` resolves to the **last** occurrence in the recording,
/// and is counted.
#[test]
fn duplicates_resolve_last_wins() {
    let dir = Scratch::new("dupes");
    let msgs = vec![
        FixtureMessage::dynamic("odom", "base_link", 1_000_000_000, pose(0.5)),
        FixtureMessage::dynamic("odom", "base_link", 2_000_000_000, pose(1.0)),
        // Same edge, same stamp, different value, later in the recording.
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

/// §3.2: stamps far in the future are kept, counted, and reported with the worst offset.
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

/// §3.2: an edge that appears on both `/tf` and `/tf_static` is a hard error naming the
/// timestamp — and, through [`tf_tree_ingest::describe`], the edge.
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

/// **Two publishers with different latencies are not a clock reset.** The most common real
/// `/tf` topology — a 100 Hz `odom -> base_link` stamped at publish time interleaved with a
/// 10 Hz `map -> odom` stamped 200 ms in the past by a localization node — ingests at the
/// defaults.
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
    // Each edge is internally monotone, so the skew is not "out of order" either — it is
    // simply not this check's business.
    assert_eq!(out.report.anomalies.out_of_order, 0);
    assert_eq!(out.report.samples_pushed, 110);
    assert_eq!(out.report.dynamic_edges, 2);
}

/// A **bag loop still halts** with the guard scoped per edge, because a loop moves `/clock`
/// itself and therefore regresses every edge at once.
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
    // **The message names the edge**, which is what an earlier revision could not do: with
    // one guard over the merged stream there was no edge to name.
    let text = tf_tree_ingest::describe(err, &frames).to_string();
    assert!(
        text.contains(frames.name(parent)) && text.contains(frames.name(child)),
        "the message must name the regressing edge: {text}"
    );
}

/// **The reset names the coordinate that survives the reset.**
#[test]
fn a_reset_reports_the_recorders_clock_and_not_only_the_stamp() {
    const LOOP_NS: i64 = 1_000_000_000;
    // Ten messages a loop, one edge, 100 ms apart: enough that the rewind is far past the
    // 100 ms threshold and small enough to read.
    let dir = Scratch::new("logtime");
    let mut msgs = Vec::new();
    for loop_index in 0..2i64 {
        for i in 0..10i64 {
            let stamp = 10_000_000_000 + i * 100_000_000;
            // The stamps repeat; the recorder's clock does not.
            let log_time = stamp + loop_index * (10 * LOOP_NS);
            msgs.push(
                FixtureMessage::dynamic("odom", "base_link", stamp, pose(i as f64))
                    .logged_at(log_time),
            );
        }
    }
    let path = write(&dir, "logtime.mcap", &msgs);

    let mut frames = Frames::default();
    let err = tf_tree_ingest::survey(&path, &IngestOptions::default(), &mut frames).unwrap_err();
    let IngestError::ClockReset {
        at_ns,
        at_log_time_ns,
        by_ns,
        ..
    } = err
    else {
        panic!("expected a clock reset, got {err:?}");
    };
    // The first message of the second loop: stamp back to the start, 900 ms behind the 10.9
    // s high-water mark the first loop left.
    assert_eq!(at_ns, 10_000_000_000);
    assert_eq!(by_ns, 900_000_000);
    assert_eq!(
        at_log_time_ns, 20_000_000_000,
        "the log time must be the recorder's clock, not a second copy of the stamp"
    );

    // And the operator can read it, not merely match on it.
    let text = tf_tree_ingest::describe(err, &frames).to_string();
    assert!(
        text.contains("20000000000"),
        "the message must name the log time to cut at: {text}"
    );
}

/// §3.2's `split` policy is refused with a reason rather than silently doing something
/// else.
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

/// §11's "spill path" row for the **grouping** half of §3.1's cap: a `--max-memory` below
/// the dataset size splits pass two into several re-reads, and the result is identical to
/// the single-pass one.
#[test]
fn capped_memory_matches_the_uncapped_path() {
    let dir = Scratch::new("cap");
    let path = write(&dir, "cap.mcap", &small_recording());

    let mut f1 = Frames::default();
    let uncapped = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut f1).unwrap();

    // 64 B per buffered sample. 120 samples' worth is above the largest single edge (100)
    // and below the total (160), so pass two splits into two groups — which is the case
    // worth testing.
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

/// §3.3: discovery is by schema, so a remapped topic is ingested and its `tf_static`
/// counterpart is still recognised as static.
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

/// A TF-schema channel whose encoding is not `cdr` is **counted and skipped**, not fed to a
/// decoder that would fail on it.
#[test]
fn a_non_cdr_tf_channel_is_counted_not_decoded() {
    let dir = Scratch::new("noncdr");
    let p = dir.path("mixed.mcap");

    // A **mixed** recording: one decodable topic and one JSON-encoded TF channel.
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
    assert_eq!(out.report.anomalies.non_cdr_channels, 1);
    assert_eq!(
        out.report.anomalies.filtered_channels, 0,
        "no topic filter was set, so nothing was excluded by one"
    );
    // The terminal row names the two numbers, and which number is which is the whole point
    // of the split — so the arm asserts the rendered text, not the substring both arms
    // share.
    assert!(
        out.report
            .summary()
            .contains("1 TF channels were skipped (1 not CDR, 0 excluded by --topic)"),
        "the skip must reach the report, on the right side:\n{}",
        out.report.summary()
    );
    assert!(
        out.report.to_json().contains("\"non_cdr_channels\":1"),
        "the machine-readable half must say which of the two reasons it was:\n{}",
        out.report.to_json()
    );
}

/// A channel the operator's own `--tf-topic` excluded is **not** a channel this build could
/// not decode.
#[test]
fn a_topic_filter_is_not_an_undecodable_channel() {
    let dir = Scratch::new("filtered");
    let p = dir.path("two_topics.mcap");

    let mut msgs: Vec<FixtureMessage> = (1..5)
        .map(|i| FixtureMessage::dynamic("odom", "base_link", i * 1_000_000_000, pose(i as f64)))
        .collect();
    for i in 1..5 {
        msgs.push(FixtureMessage {
            topic: "/robot2/tf".into(),
            log_time_ns: i * 1_000_000_000,
            transforms: vec![TransformStamped {
                stamp_ns: i * 1_000_000_000,
                frame_id: "map".into(),
                child_frame_id: "odom".into(),
                pose: pose(i as f64),
            }],
        });
    }
    write_mcap(&p, &msgs).unwrap();

    let opts = IngestOptions {
        roles: TopicRoles {
            dynamic_topics: vec!["/tf".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&p, &opts, &mut frames).unwrap();

    // The narrowing worked: one topic in, one out.
    assert_eq!(out.report.transforms_read, 4);
    assert_eq!(out.report.samples_pushed, 4);
    assert_eq!(out.report.anomalies.filtered_channels, 1);
    // And the excluded channel is **not** reported as one this build cannot decode.
    assert_eq!(out.report.anomalies.non_cdr_channels, 0);
    let json = out.report.to_json();
    assert!(
        json.contains("\"filtered_channels\":1") && json.contains("\"non_cdr_channels\":0"),
        "the JSON must say which of the two it was:\n{json}"
    );
    assert!(
        !json.contains("undecodable"),
        "the withdrawn key names one term of a sum:\n{json}"
    );
    // And the terminal row, which is the other rendering and was the ungated one: it prints
    // both numbers, so it can misdescribe this channel the same way the withdrawn JSON key
    // did.
    assert!(
        out.report
            .summary()
            .contains("1 TF channels were skipped (0 not CDR, 1 excluded by --topic)"),
        "the excluded channel must not read as an undecodable one:\n{}",
        out.report.summary()
    );
}

/// §5.6: a leading `/` is stripped, once, and counted — so `/odom` and `odom` are one frame
/// rather than two.
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

/// §3.2: `/tf_static` messages that contradict an already-declared value are counted and
/// reported; the first value wins.
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

/// §3.2's static-conflict row: **"report both values"**, not only a count.
#[test]
fn a_static_conflict_reports_both_values() {
    let dir = Scratch::new("static_both");
    let (first, second) = (pose(0.2), pose(0.9));
    let msgs = vec![
        FixtureMessage::static_edge("base_link", "laser", first),
        FixtureMessage::static_edge("base_link", "laser", second),
        // Latched `/tf_static` re-delivers to every late joiner, so the same contradiction
        // arrives again.
        FixtureMessage::static_edge("base_link", "laser", second),
        FixtureMessage::dynamic("odom", "base_link", 1_000_000_000, pose(1.0)),
    ];
    let path = write(&dir, "static_both.mcap", &msgs);
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();

    assert_eq!(
        out.report.anomalies.static_conflicts, 2,
        "both contradicting messages are counted"
    );
    assert_eq!(
        out.report.static_conflict_details.len(),
        1,
        "one row per contradicted edge, not one per latched repeat: {:?}",
        out.report.static_conflict_details
    );
    let c = &out.report.static_conflict_details[0];
    assert_eq!(
        (c.parent.as_str(), c.child.as_str()),
        ("base_link", "laser")
    );
    assert_eq!(
        (c.declared_by.as_str(), c.contradicted_by.as_str()),
        ("/tf_static", "/tf_static")
    );
    // Bit-identical: these poses went into the fixture as `f64` and came back through CDR,
    // which is a little-endian `f64` on the wire, so no rounding happens anywhere on the
    // path and any tolerance here would be slack that hides a swapped component.
    assert_eq!(c.existing, first, "the value on file, which wins");
    assert_eq!(c.offered, second, "the value that was refused");
    assert_ne!(
        c.existing, c.offered,
        "a conflict whose two values are equal is not one"
    );

    // The JSON carries both, spelled so a consumer can diff them against a URDF.
    let json = out.report.to_json();
    let want_existing = format!(
        "\"existing\":[{},{},{},{},{},{},{}]",
        first[0], first[1], first[2], first[3], first[4], first[5], first[6]
    );
    let want_offered = format!(
        "\"offered\":[{},{},{},{},{},{},{}]",
        second[0], second[1], second[2], second[3], second[4], second[5], second[6]
    );
    assert!(
        json.contains(&want_existing),
        "missing {want_existing} in {json}"
    );
    assert!(
        json.contains(&want_offered),
        "missing {want_offered} in {json}"
    );

    // And the terminal summary does too, at full precision: `StaticStore` calls two poses
    // the same within 1e-12, so a rounded rendering could print two identical numbers under
    // a line saying they differ.
    let text = out.report.summary();
    assert!(
        text.contains("base_link -> laser")
            && text.contains(&format!("{}", first[4]))
            && text.contains(&format!("{}", second[4])),
        "the summary must name the edge and both values: {text}"
    );
}

/// A file that is not an MCAP at all fails with a named error, not a panic and not an empty
/// tree.
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

/// Truncation recovery is **record-granular, including inside a chunk**: the more of a
/// recording survives, the more transforms come out of it.
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
        // **Truncation is not corruption, and the report must not conflate them.** The
        // final record in a cut chunk's prefix necessarily runs past the end; treating that
        // as a malformed chunk would tell an operator their recording is damaged when it is
        // merely incomplete, and would inflate a counter they are meant to act on.
        assert_eq!(
            s.anomalies.bad_chunks, 0,
            "a truncated recording reported {} corrupt chunk(s) at a {cut}-byte cut",
            s.anomalies.bad_chunks
        );
        recovered.push(s.transforms_read);
    }
    // Strictly increasing: a chunk-granular reader would give a run of zeros and then a
    // jump, so this is what distinguishes the two.
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

/// A recording cut before *any* complete record says it was truncated, rather than claiming
/// the recording has no transforms in it.
#[test]
fn a_recording_cut_before_any_record_says_it_was_truncated() {
    let dir = Scratch::new("trunc_floor");
    let path = write(&dir, "full.mcap", &small_recording());
    let whole = std::fs::read(&path).unwrap();

    // 200 bytes: past the 8-byte magic and the Header record, so the file is recognisably
    // an MCAP, and short of the first complete message.
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

/// A file whose start magic is missing is refused, **even though its records would parse
/// perfectly well**.
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

/// The report's JSON carries every field the summary prints, and parses as JSON.
#[test]
fn report_json_is_well_formed() {
    let dir = Scratch::new("json");
    let path = write(&dir, "json.mcap", &small_recording());
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();
    let json = out.report.to_json();

    assert!(json.contains("\"schema\":\"tf_tree.ingest/2\""));
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

/// §3.1's spill-to-run-file: an edge that alone exceeds `--max-memory` is sorted through a
/// temporary run file and a k-way merge, and produces **the same answers** as the in-memory
/// path.
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
        // A second, small edge so the run also exercises the ordinary grouped path beside
        // the spilled one — a recording with a single edge would not show that the two
        // coexist.
        if i % 30 == 0 {
            msgs.push(
                FixtureMessage::dynamic("base_link", "arm", t, pose(k as f64 * 0.5)).logged_at(t),
            );
        }
    }
    // Duplicates of an *early* stamp appended at the very end, so the original and the
    // duplicates land in different runs and the cross-run tie break is what decides.
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
    // And the **exact** value, which pins the reported bound from below, so a term dropped
    // from that bound is caught.
    assert_eq!(
        spilled.report.fill.peak_buffer_bytes, cap,
        "the spill phase's own peak is the cap by construction at this cap; \
         a smaller number means a term is missing from it"
    );

    // The two paths agree about how much survived, and about the duplicate.
    assert_eq!(
        spilled.report.samples_pushed,
        in_memory.report.samples_pushed
    );
    // `N` distinct stamps on the spilled edge (the two appended duplicates collapse onto
    // one that is already there) plus `N / 30` on the small one.
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

    // The run file does not outlive the ingest.
    let left: Vec<_> = std::fs::read_dir(&spill)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(left.is_empty(), "spill directory still holds {left:?}");
}

/// A rosbag2 sqlite3 bag is **diagnosed**, not reported as a corrupt MCAP.
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

/// A cap so small that the runs outnumber what one merge can hold: the spill path **reduces
/// in several passes** and still answers identically.
#[test]
fn a_tiny_cap_reduces_in_several_passes() {
    // 2 200 samples at 7 per run is 315 runs against a fan-in of 11: one reduce pass leaves
    // 29, still over the fan-in, so a second runs and leaves 3.
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
        reduced.report.fill.spilled_runs, 347,
        "315 runs, then 29, then 3 — a different split means the reduce loop \
         changed shape and this test's arithmetic no longer describes it"
    );
    assert!(
        reduced.report.fill.peak_buffer_bytes <= CAP,
        "peak {} B over the {CAP} B cap",
        reduced.report.fill.peak_buffer_bytes
    );
    // The other side of the bound: a spill path that reported nothing would pass the
    // assertion above.
    assert!(
        reduced.report.fill.peak_buffer_bytes >= CAP * 3 / 4,
        "peak {} B is too small to be the spill path's; it was not measured",
        reduced.report.fill.peak_buffer_bytes
    );
    // **The one allocation the cap does not bound**, reported rather than hidden: sixteen
    // bytes per run, and at this cap there are enough runs for it to exceed the cap several
    // times over.
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

/// **A duplicate that is re-merged by a reduce pass still resolves to the last occurrence
/// in the recording.**
#[test]
fn a_reduce_pass_keeps_the_last_occurrence() {
    const N: i64 = 2_200;
    const STRIDE: i64 = 7;
    const CAP: u64 = 1024;
    // `spill::spill_budget(1024)`'s samples-per-run.
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
    // Off the 1 ms grid, so neither stamp collides with one the loop above already produced
    // and each duplicate is a clean pair of two.
    let across = 1_000_000_000 + 1_234_000_000 + 500_000;
    let within = 1_000_000_000 + 1_300_000_000 + 500_000;
    // Across two runs: index 3 is in run 0, index `3 + RUN` in run 1, and both runs fall
    // inside the first reduce window.
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

    // Non-degenerate: the reduce loop has to have run, or this is the sibling test again
    // under another name.
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

/// §3.1's spill path: the **per-run** sort is stable, so "last wins" means the
/// last occurrence in the recording *inside* one run.
#[test]
fn the_per_run_sort_is_stable_so_last_wins_inside_a_run() {
    // `spill::spill_budget(65_536)`'s samples per run, spelled as a literal for the reason
    // `a_reduce_pass_keeps_the_last_occurrence`'s `RUN` gives.
    const RUN: usize = 896;
    const RUNS: usize = 4;
    const PAIRS: usize = RUN / 2;
    const CAP: u64 = 64 * 1024;
    // Coprime with `PAIRS` (448 = 2^6 · 7), so `j * STRIDE % PAIRS` visits every key once
    // and the run arrives scattered rather than sorted.
    const STRIDE: usize = 11;

    let dir = Scratch::new("run_stability");
    let spill = dir.0.join("spill");
    std::fs::create_dir_all(&spill).unwrap();

    // Position `j` and position `j + PAIRS` of each run carry the same stamp, because
    // `STRIDE * PAIRS % PAIRS == 0`.
    let key_ns = |r: usize, j: usize| {
        1_000_000_000
            + (r as i64) * 1_000_000_000
            + (((j % PAIRS) * STRIDE % PAIRS) as i64) * 1_000_000
    };
    let mut msgs = Vec::new();
    for r in 0..RUNS {
        for j in 0..RUN {
            let i = r * RUN + j;
            let t = key_ns(r, j);
            msgs.push(FixtureMessage::dynamic("odom", "base_link", t, pose(i as f64)).logged_at(0));
        }
    }
    assert_eq!(msgs.len(), RUN * RUNS);

    let path = write(&dir, "run_stability.mcap", &msgs);
    let base = IngestOptions {
        // The fixture is deliberately out of stamp order by seconds, which at the default
        // threshold is a clock reset — `out_of_order_ingest_matches_ordered` states the
        // same at length.
        clock_reset_threshold_ns: i64::MAX,
        ..IngestOptions::default()
    };
    let mut f1 = Frames::default();
    let spilled = tf_tree_ingest::run(
        &path,
        &IngestOptions {
            max_memory_bytes: CAP,
            spill_dir: Some(spill),
            ..base
        },
        &mut f1,
    )
    .unwrap();

    // Non-degenerate: the spill path has to have run, or this is an in-memory
    // test under another name.
    assert!(
        spilled.report.fill.spilled_runs >= RUNS as u32,
        "{} runs written; the edge did not spill in {RUNS} pieces",
        spilled.report.fill.spilled_runs
    );
    assert_eq!(
        spilled.report.anomalies.duplicate_stamps,
        (PAIRS * RUNS) as u64,
        "every pair is one collapsed duplicate"
    );

    for r in 0..RUNS {
        for j in 0..PAIRS {
            // The second occurrence in recording order, which is the one §3.2
            // says wins.
            let want = pose((r * RUN + j + PAIRS) as f64);
            let got = spilled
                .tree
                .lookup(
                    "odom",
                    "base_link",
                    Stamp::<SystemDomain>::from_nanos(key_ns(r, j)),
                )
                .unwrap();
            // Bit-identical: the pose is an `f64` written to CDR and read back, and the
            // arena stores it verbatim, so any tolerance here would be slack that admits
            // the *other* member of the pair.
            assert_eq!(
                (got.t.x, got.t.y, got.t.z),
                (want[4], want[5], want[6]),
                "run {r}, key {j}: the last occurrence in the run must win"
            );
        }
    }
}

// The hand-rolled, summary-free fixture and the corrupt-chunk skip policy.

/// Nine messages, three per child frame, so a chunk of three makes "the second chunk's
/// messages" exactly "the `sensor_b` edge".
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
    // Times as well as counts, for the reason `edge_time_rows` gives.
    assert_eq!(edge_time_rows(&b.report), edge_time_rows(&a.report));
    // Nothing about a summary-free file is anomalous, and the reader must not mistake the
    // absence of one for a truncation.
    assert!(!b.report.anomalies.truncated);
    assert_eq!(b.report.anomalies.bad_chunks, 0);
}

/// **THE CENTREPIECE: one corrupt chunk does not lose the recording.**
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

/// **A framing fault mid-chunk keeps every record it had already delivered, and the report
/// still blames the whole chunk.** Pinned because the two halves of that sentence disagree,
/// and nothing else in the crate notices.
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

    // Eight, not six: the damaged chunk's first two messages were delivered before the walk
    // reached the record whose length is a lie.
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
    // The contradiction in one assertion: a transform the report calls lost is queryable,
    // at a stamp inside the span the report calls lost.
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

/// A skipped chunk is **counted**, and the report says *when* the recording lost data.
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

/// Under `--on-bad-chunk halt` the ingest **fails, naming the chunk**, and the ordinal is
/// the damaged chunk's own.
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

/// **An unknown codec is a hard error, not a skip — even under [`OnBadChunk::Skip`].**
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

/// **A chunk that claims a codec it does not carry is damage, and one damaged chunk must
/// not cost the recording.**
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

/// **A skipped chunk that carried the only `Channel` record silently drops every later
/// message.** This test exists to document that, not to celebrate it.
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
    // **The finding, asserted so it cannot quietly change:** the diagnosis does not name
    // the chunk that was skipped, because a failed survey has no report to name it from.
    let text = tf_tree_ingest::describe(err, &f2).to_string();
    assert!(
        !text.contains(&format!("chunk {DAMAGED_CHUNK_ORDINAL}")),
        "if the diagnosis has improved to name the skip, this test must be rewritten \
         to match: {text}"
    );
}

/// A lying `uncompressed_size` on an **uncompressed** chunk is refused, with no decoder
/// involved.
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

    // Under `halt` the fault is named, and it is the *stored-size* disagreement rather than
    // a CRC one — the check is on the header's own two numbers, so it fires before anything
    // hashes — and rather than `LengthMismatch`, because no decoder ran and neither number
    // is a decoder's output.
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
                matches!(kind, BadChunkKind::StoredSizeMismatch { .. }),
                "got {kind:?}"
            );
        }
        other => panic!("expected BadChunk, got {other:?}"),
    }
    // The rendered message must not send the reader to a decompressor that this path never
    // reaches.
    let text = tf_tree_ingest::describe(err, &frames).to_string();
    assert!(
        !text.contains("produced"),
        "no decoder ran, so nothing was 'produced': {text}"
    );
}

/// **A recording every chunk of which exceeds a ceiling is not reported as a recording with
/// nothing in it.**
#[cfg(feature = "compression")]
#[test]
fn a_recording_over_every_ceiling_names_the_flag_not_an_empty_recording() {
    let dir = Scratch::new("chunk_over_limit");
    let path = dir.path("big.mcap");
    write_mcap_chunked(
        &path,
        &three_sensors_nine_messages(),
        ChunkedSpec::new(3).compressed(FixtureCodec::Zstd),
    )
    .unwrap();

    // A ceiling below every chunk in the file.
    let opts = IngestOptions {
        max_chunk_uncompressed_bytes: 0,
        ..IngestOptions::default()
    };
    let mut frames = Frames::default();
    match tf_tree_ingest::survey(&path, &opts, &mut frames).unwrap_err() {
        IngestError::AllChunksOverLimit { skipped } => assert_eq!(skipped, 3),
        other => panic!("expected AllChunksOverLimit, got {other:?}"),
    }

    // And under `halt` it is the ordinary named fault, because the policy did not change —
    // only the diagnosis when the skips take everything.
    let opts = IngestOptions {
        max_chunk_uncompressed_bytes: 0,
        on_bad_chunk: OnBadChunk::Halt,
        ..IngestOptions::default()
    };
    let mut frames = Frames::default();
    match tf_tree_ingest::survey(&path, &opts, &mut frames).unwrap_err() {
        IngestError::BadChunk { chunk, kind } => {
            assert_eq!(chunk, 0);
            assert!(
                matches!(kind, BadChunkKind::ImplausibleSize { .. }),
                "got {kind:?}"
            );
        }
        other => panic!("expected BadChunk, got {other:?}"),
    }
}

/// **A ceiling refusal is counted apart from damage, and only when it is one.**
#[test]
fn damage_is_not_counted_as_a_ceiling_refusal() {
    let dir = Scratch::new("chunk_damage_vs_limit");
    let path = dir.path("damaged.mcap");
    write_mcap_chunked(
        &path,
        &three_sensors_nine_messages(),
        ChunkedSpec::new(3).damaged(ChunkDamage::UncompressedCrc),
    )
    .unwrap();

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));
    assert_eq!(out.report.anomalies.bad_chunks, 1);
    assert_eq!(
        out.report.anomalies.chunks_over_limit, 0,
        "a failed CRC is damage; there is no flag to raise for it"
    );
    assert!(
        !out.report.summary().contains("--max-chunk-size"),
        "the report must not offer a remedy that cannot help: {}",
        out.report.summary()
    );
}

// Compressed recordings — the case rosbag2 and Foxglove actually write.

/// **A zstd recording ingests byte-for-byte identically to the uncompressed one.**
#[cfg(feature = "compression")]
#[test]
fn a_zstd_recording_ingests_identically() {
    a_compressed_recording_ingests_identically(FixtureCodec::Zstd, "zstd");
}

/// **An lz4 recording ingests byte-for-byte identically to the uncompressed one.**
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
        // **Cut four bytes into the last chunk's payload**, so two chunks are complete and
        // the third is a partial codec frame.
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

/// **A recording compressed by real libzstd ingests, with the same transforms as the
/// uncompressed equivalent.**
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

    // The control: the same corpus, uncompressed, written here and now.
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

/// **`compression_compiled_in` reports the truth about *this* build.**
#[test]
fn the_predicate_reports_a_build_with_codecs() {
    assert_eq!(
        tf_tree_ingest::compression_compiled_in(),
        cfg!(feature = "compression"),
        "the default build compiles both codecs in"
    );
}

/// §5.6's `tf_prefix` is applied by **both** passes, which is the only way the two can
/// agree about which edge a sample belongs to.
#[test]
fn a_tf_prefix_is_applied_by_both_passes() {
    let dir = Scratch::new("prefix");
    let path = write(&dir, "prefix.mcap", &small_recording());

    let opts = IngestOptions {
        tf_prefix: Some("robot1".into()),
        ..IngestOptions::default()
    };
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &opts, &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));

    let mut names: Vec<&str> = frames.all().iter().map(String::as_str).collect();
    names.sort_unstable();
    assert!(
        names.iter().all(|n| n.starts_with("robot1/")),
        "pass one must intern the prefixed names: {names:?}"
    );

    // **The half no existing test reaches.** The report's per-edge counts come from the
    // survey, so they are prefixed either way; `samples_pushed` is what pass two actually
    // managed to store, and it is zero if pass two normalized differently from pass one.
    assert!(
        out.report.samples_pushed > 0,
        "pass two normalized with a different prefix than pass one, so every \
         `frames.id` lookup missed and the samples were dropped in silence"
    );
    assert_eq!(
        out.report
            .edges
            .iter()
            .filter(|e| !e.is_static)
            .map(|e| e.samples)
            .sum::<u64>(),
        out.report.samples_pushed,
        "the report claims samples the arena does not hold"
    );
    assert!(
        out.report
            .remaps
            .iter()
            .any(|(raw, mapped)| raw == "odom" && mapped == "robot1/odom"),
        "§5.6's remap table must reach the report: {:?}",
        out.report.remaps
    );
    assert!(
        out.tree.frame("map").is_err(),
        "the unprefixed name must not also be in the tree"
    );
}

/// **A spill directory that cannot be written is `IngestError::Spill`, not a panic and not
/// a quietly short ingest.**
#[test]
fn an_unusable_spill_directory_is_reported_as_a_spill_failure() {
    let dir = Scratch::new("spill_enoent");
    // 100 samples on one edge against a 1 KiB cap: 6 400 B on its own, so `plan_groups`
    // routes it to `Group::Spilled` and `fill_spilled` runs.
    let msgs: Vec<FixtureMessage> = (0..100i64)
        .map(|i| {
            FixtureMessage::dynamic(
                "odom",
                "base_link",
                1_000_000_000 + i * 1_000_000,
                pose(i as f64),
            )
        })
        .collect();
    let path = write(&dir, "spill.mcap", &msgs);

    let opts = IngestOptions {
        max_memory_bytes: 1024,
        spill_dir: Some(dir.path("no/such/directory")),
        ..IngestOptions::default()
    };
    let mut frames = Frames::default();
    match tf_tree_ingest::run(&path, &opts, &mut frames) {
        Err(IngestError::Spill { .. }) => {}
        Err(e) => panic!("wanted a Spill failure, got {e:?}"),
        Ok(out) => panic!(
            "an ingest whose run file could not be created reported success, \
             with {} of 100 samples",
            out.report.samples_pushed
        ),
    }
}

/// §5.6: a transform whose parent or child name is empty is **dropped and counted**, and
/// the recording around it still ingests.
#[test]
fn an_empty_frame_name_is_dropped_and_counted() {
    let dir = Scratch::new("empty_names");
    let mut msgs: Vec<FixtureMessage> = (1..5)
        .map(|i| FixtureMessage::dynamic("odom", "base_link", i * 1_000_000_000, pose(i as f64)))
        .collect();
    msgs.push(FixtureMessage::dynamic(
        "",
        "base_link",
        5_000_000_000,
        pose(5.0),
    ));
    // A bare slash normalizes to empty too, and is the likelier one in the field.
    msgs.push(FixtureMessage::dynamic(
        "odom",
        "/",
        6_000_000_000,
        pose(6.0),
    ));
    let path = write(&dir, "empty_names.mcap", &msgs);

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));

    assert_eq!(out.report.anomalies.empty_names, 2);
    assert_eq!(
        out.report.samples_pushed, 4,
        "the rest of the recording survives"
    );
    assert_eq!(
        out.report.dynamic_edges, 1,
        "a dropped transform must not create an edge"
    );
    assert!(
        out.report.summary().contains("empty frame name"),
        "the drop must reach the report:\n{}",
        out.report.summary()
    );
}

/// **A whole recording with no TF in it is `NoTransforms`** — the arm the variant is named
/// for, and the one the truncation and ceiling tests are each defined *against*.
#[test]
fn a_recording_with_no_transforms_in_it_says_exactly_that() {
    let dir = Scratch::new("no_tf");

    let empty = dir.path("empty.mcap");
    write_mcap(&empty, &[]).unwrap();
    let mut f1 = Frames::default();
    assert_eq!(
        tf_tree_ingest::survey(&empty, &IngestOptions::default(), &mut f1).unwrap_err(),
        IngestError::NoTransforms,
        "a complete recording with nothing in it is not truncation"
    );

    // The same corpus every other test ingests, under a schema that is not TF.
    let other = dir.path("imu.mcap");
    let msgs: Vec<FixtureMessage> = (1..5)
        .map(|i| FixtureMessage::dynamic("odom", "base_link", i * 1_000_000_000, pose(i as f64)))
        .collect();
    write_mcap_as(&other, &msgs, "sensor_msgs/msg/Imu", &[]).unwrap();
    let mut f2 = Frames::default();
    assert_eq!(
        tf_tree_ingest::survey(&other, &IngestOptions::default(), &mut f2).unwrap_err(),
        IngestError::NoTransforms,
        "a channel whose schema is not TF must not be read for its topic name"
    );
    assert!(
        f2.is_empty(),
        "no frame may be interned from a non-TF channel: {:?}",
        f2.all()
    );
}

/// §3.2's *"frame declared, never published"*: an edge every sample of which was dropped
/// stays **in the tree and is flagged**, and the recording around it ingests.
#[test]
fn an_edge_whose_every_sample_was_dropped_is_declared_and_flagged() {
    let dir = Scratch::new("no_samples");
    let mut msgs: Vec<FixtureMessage> = (1..5)
        .map(|i| FixtureMessage::dynamic("odom", "base_link", i * 1_000_000_000, pose(i as f64)))
        .collect();
    msgs.push(FixtureMessage::dynamic("base_link", "laser", 0, pose(9.0)).logged_at(1_000_000_000));
    let path = write(&dir, "no_samples.mcap", &msgs);

    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames)
        .unwrap_or_else(|e| panic!("{}", tf_tree_ingest::describe(e, &frames)));

    assert_eq!(
        out.report.edges_without_samples,
        vec!["base_link -> laser".to_string()]
    );
    assert_eq!(
        out.report.dynamic_edges, 2,
        "the edge is kept in the topology, not dropped with its samples"
    );
    assert_eq!(out.report.samples_pushed, 4);
    assert!(
        out.report.summary().contains("no samples"),
        "the flag must reach the report:\n{}",
        out.report.summary()
    );
}

/// A rejected push prints the engine's sentence, which names the edge — not
/// `NonMonotonicStamp { last: 2, got: 1 }`.
#[test]
fn a_rejected_push_renders_the_engines_prose() {
    let push = tf_tree::PushError::NonMonotonicStamp {
        edge: tf_tree::EdgeId(4),
        last: 2,
        got: 1,
    };
    assert_eq!(
        IngestError::Push(push).to_string(),
        format!("push rejected: {push}")
    );
}
