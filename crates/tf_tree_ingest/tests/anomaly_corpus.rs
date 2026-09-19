//! `docs/PHASE5.md` §11's second bullet: *"a synthetic corpus containing every
//! row of §3.2, asserting the exact ingest-report output."*
//!
//! One corpus exercises every reportable row together, and the assertion is the
//! whole JSON document, so a count that moves for the wrong reason moves a
//! different field with it. The two refusal rows (an edge changing kind, a
//! backward jump past the threshold under `halt`) cannot appear in a report and
//! are the last two tests, driven from the same corpus. `split` stays refused
//! (§3.2's amendment).
//!
//! The corpus is written by `tf_tree_ingest::fixture`, so this proves the
//! reader's bookkeeping, not its conformance (`testdata/ATTRIBUTION.md`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use tf_tree_ingest::fixture::{write_mcap, FixtureMessage};
use tf_tree_ingest::{Frames, IngestError, IngestOptions};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("tf_tree_ingest_anom_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A pose whose every component is distinct, so a swapped sample is visible.
fn pose(k: f64) -> [f64; 7] {
    let (s, c) = (k.sin(), k.cos());
    [c, s, 0.0, 0.0, k, k * 2.0, k * 3.0]
}

const SECOND: i64 = 1_000_000_000;
/// The default reset threshold is 100 ms, so 50 ms back is jitter and is kept.
const JITTER_BACK: i64 = 50_000_000;
/// The default future horizon is 10 s, so a stamp 18.8 s ahead is reported.
const FUTURE_STAMP: i64 = 20 * SECOND;
const FUTURE_LOG: i64 = 1_200_000_000;

/// The corpus: every reportable §3.2 row in one recording, on three edges.
///
/// * `base_link -> laser` (static): differing values.
/// * `map -> odom`: a duplicate `(edge, stamp)`, a sub-threshold backward jump, a
///   zero stamp and a far-future stamp.
/// * `odom -> base_link`: every sample zero-stamped, so it is declared and never
///   published.
///
/// Poses are distinct per message so the duplicate's winner is checkable.
fn corpus() -> Vec<FixtureMessage> {
    vec![
        FixtureMessage::static_edge("base_link", "laser", pose(1.0)),
        FixtureMessage::static_edge("base_link", "laser", pose(2.0)),
        FixtureMessage::dynamic("map", "odom", SECOND, pose(3.0)),
        FixtureMessage::dynamic("map", "odom", SECOND + 100_000_000, pose(4.0)),
        FixtureMessage::dynamic("map", "odom", SECOND + 100_000_000, pose(5.0)),
        FixtureMessage::dynamic("map", "odom", SECOND + 100_000_000 - JITTER_BACK, pose(6.0)),
        FixtureMessage::dynamic("map", "odom", 0, pose(7.0)),
        FixtureMessage::dynamic("map", "odom", FUTURE_STAMP, pose(8.0)).logged_at(FUTURE_LOG),
        FixtureMessage::dynamic("odom", "base_link", 0, pose(9.0)),
        FixtureMessage::dynamic("odom", "base_link", 0, pose(10.0)),
    ]
}

fn write(dir: &Path, name: &str, msgs: &[FixtureMessage]) -> PathBuf {
    let p = dir.join(name);
    write_mcap(&p, msgs).unwrap();
    p
}

/// **The exact report**, as one JSON document (§11): a per-field assertion is
/// satisfied by the wrong anomaly being counted.
///
/// Only `source` and `tf_tree` are interpolated. Changing the report's shape
/// fails this test deliberately: the schema tag (`tf_tree.ingest/2`) is pinned.
#[test]
fn the_anomaly_corpus_report_is_exact() {
    let dir = Scratch::new("exact");
    let path = write(&dir.0, "anomalies.mcap", &corpus());
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();

    let p1 = pose(1.0);
    let p2 = pose(2.0);
    let want = format!(
        concat!(
            r#"{{"schema":"tf_tree.ingest/2","tf_tree":"{version}","source":"{source}","#,
            r#""frames":4,"static_edges":1,"dynamic_edges":2,"#,
            r#""transforms_read":10,"samples_pushed":4,"#,
            r#""passes":1,"peak_buffer_bytes":640,"peak_run_index_bytes":0,"#,
            r#""spilled_runs":0,"spilled_bytes":0,"#,
            r#""span_ns":[1000000000,20000000000],"#,
            r#""anomalies":{{"zero_stamp_drops":3,"future_stamps":1,"#,
            r#""worst_future_offset_ns":18800000000,"out_of_order":1,"#,
            r#""clock_resets":0,"static_conflicts":1,"duplicate_stamps":1,"#,
            r#""stripped_slash_names":0,"empty_names":0,"filtered_channels":0,"non_cdr_channels":0,"#,
            r#""truncated":false,"bad_chunks":0,"chunks_over_limit":0,"#,
            r#""oversized_records_skipped":0,"bad_chunk_span_ns":null,"#,
            r#""first_reset_at_ns":null}},"#,
            r#""edges":["#,
            r#"{{"parent":"base_link","child":"laser","topic":"/tf_static","static":true,"#,
            r#""samples":0,"source_oldest_ns":null,"source_newest_ns":null,"rate_hz":null}},"#,
            r#"{{"parent":"map","child":"odom","topic":"/tf","static":false,"#,
            r#""samples":5,"source_oldest_ns":1000000000,"source_newest_ns":20000000000,"#,
            r#""rate_hz":0.210526}},"#,
            r#"{{"parent":"odom","child":"base_link","topic":"/tf","static":false,"#,
            r#""samples":0,"source_oldest_ns":null,"source_newest_ns":null,"rate_hz":null}}"#,
            r#"],"remaps":[],"edges_without_samples":["odom -> base_link"],"#,
            r#""static_conflict_details":[{{"parent":"base_link","child":"laser","#,
            r#""declared_by":"/tf_static","contradicted_by":"/tf_static","#,
            r#""existing":[{e0},{e1},{e2},{e3},{e4},{e5},{e6}],"#,
            r#""offered":[{o0},{o1},{o2},{o3},{o4},{o5},{o6}]}}]}}"#,
        ),
        version = out.report.tool_version,
        source = path.display(),
        e0 = p1[0],
        e1 = p1[1],
        e2 = p1[2],
        e3 = p1[3],
        e4 = p1[4],
        e5 = p1[5],
        e6 = p1[6],
        o0 = p2[0],
        o1 = p2[1],
        o2 = p2[2],
        o3 = p2[3],
        o4 = p2[4],
        o5 = p2[5],
        o6 = p2[6],
    );
    assert_eq!(out.report.to_json(), want);

    // The duplicate resolved to the last occurrence, bit-identically.
    let got = out
        .tree
        .lookup(
            "map",
            "odom",
            tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(SECOND + 100_000_000),
        )
        .unwrap();
    let want_pose = pose(5.0);
    assert_eq!(
        (got.t.x, got.t.y, got.t.z),
        (want_pose[4], want_pose[5], want_pose[6]),
        "the duplicate must resolve to the last occurrence in the recording"
    );
}

/// Every reportable row reaches the terminal summary too, and says something
/// an operator can act on.
#[test]
fn every_corpus_row_reaches_the_summary() {
    let dir = Scratch::new("summary");
    let path = write(&dir.0, "anomalies.mcap", &corpus());
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();
    let text = out.report.summary();

    for (row, needle) in [
        ("zero stamps", "3 transforms had stamp 0"),
        (
            "stamps far in the future",
            "1 transforms are stamped up to 18.800 s ahead",
        ),
        ("duplicate (edge, stamp)", "1 duplicate (edge, stamp) pairs"),
        (
            "backward jump below the threshold",
            "1 transforms arrived out of stamp order",
        ),
        (
            "static edge with differing values",
            "1 /tf_static messages contradicted",
        ),
        (
            "static conflict, both values",
            "base_link -> laser: /tf_static declared",
        ),
        (
            "frame declared, never published",
            "1 dynamic edges are in the tree with no samples",
        ),
    ] {
        assert!(
            text.contains(needle),
            "§3.2 row '{row}' is not in the summary; looked for {needle:?} in:\n{text}"
        );
    }
    assert!(!text.contains("backward clock jumps"), "{text}");
    assert!(!text.contains("--max-record-size"), "{text}");
}

/// §3.2's edge kind changes mid-recording: a hard error naming the timestamp.
#[test]
fn the_corpus_plus_a_kind_change_is_a_hard_error() {
    let dir = Scratch::new("kind");
    let mut msgs = corpus();
    let stamp = 9 * SECOND;
    let mut static_one = FixtureMessage::static_edge("map", "odom", pose(11.0));
    static_one.log_time_ns = stamp;
    static_one.transforms[0].stamp_ns = stamp;
    msgs.push(static_one);

    let path = write(&dir.0, "kind.mcap", &msgs);
    let mut frames = Frames::default();
    let err = tf_tree_ingest::survey(&path, &IngestOptions::default(), &mut frames).unwrap_err();
    let IngestError::EdgeKindChanged { stamp_ns, .. } = err else {
        panic!("expected a kind change, got {err:?}");
    };
    assert_eq!(stamp_ns, stamp, "§3.2 requires the error to name the stamp");
}

/// §3.2's backward clock jump past the threshold, under the default `halt`.
#[test]
fn the_corpus_plus_a_real_reset_halts() {
    let dir = Scratch::new("reset");
    let mut msgs = corpus();
    msgs.push(FixtureMessage::dynamic("map", "odom", SECOND, pose(12.0)).logged_at(30 * SECOND));

    let path = write(&dir.0, "reset.mcap", &msgs);
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
    assert_eq!(at_ns, SECOND);
    assert_eq!(
        at_log_time_ns,
        30 * SECOND,
        "the recorder's clock is the one to cut on"
    );
    assert_eq!(by_ns, FUTURE_STAMP - SECOND);
}
