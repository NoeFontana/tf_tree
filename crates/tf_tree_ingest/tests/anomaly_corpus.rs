//! `docs/PHASE5.md` §11's second bullet: *"a synthetic corpus containing every
//! row of §3.2, asserting the exact ingest-report output."*
//!
//! # Why this is not covered by the per-row tests already in `tests/ingest.rs`
//!
//! Those tests each build a recording with **one** anomaly in it and assert its
//! counter. What §11 asks for is different in two ways that matter.
//!
//! * **One corpus, so the rows are exercised together.** A counter that is right
//!   in isolation can still be wrong beside another: pass one's zero-stamp drop
//!   happens before the clock guard, the duplicate collapse happens in pass two
//!   and is joined to the survey's counts in [`IngestReport::new`], and the static
//!   conflict is on a third code path entirely. Nothing asserted that the three
//!   arithmetics agree about one file.
//! * **Exact output, not a count.** `assert_eq!(anomalies.zero_stamp_drops, 3)` is
//!   satisfied by a reader that dropped three of the *wrong* transforms. The
//!   assertion below is the whole JSON document, byte for byte, so a count that
//!   moves for the wrong reason moves a different field with it.
//!
//! # The rows that are refusals and cannot be in a report
//!
//! Two of §3.2's rows are hard errors: an edge whose kind changes mid-recording,
//! and a backward jump past the reset threshold under `--on-clock-reset=halt` (the
//! default; `split` stays refused — §3.2's own amendment is the decision, not a
//! backlog entry). Neither produces a report at all, so they cannot be assertions
//! *about* one. They are the last two tests in
//! this file, driven from the **same** corpus with one message appended, so
//! §3.2's table is covered by one fixture rather than by a claim.
//!
//! The backward-jump row's *other* half — a regression **below** the threshold,
//! which §3.2's amendment says is counted as `out_of_order` and kept, because
//! §3.1 sorts — is in the corpus and in the report.
//!
//! # What this corpus is not
//!
//! Synthetic, and written by `tf_tree_ingest::fixture` — the same crate that
//! reads it back. Nothing in `tf_tree_ingest`'s test suite reads a recording this
//! repository did not write; `testdata/ATTRIBUTION.md` says so in its first
//! paragraph and `docs/PHASE5.md` §0.0's §3 row now says so too. A writer on the
//! input side of a correctness gate cannot find a defect it shares with the
//! reader — a CDR field order both agree on, a chunk framing both get wrong the
//! same way — so this file proves the reader's *bookkeeping*, never its
//! conformance.

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
/// The default reset threshold is 100 ms (`tf_tree_bridge::clock`), so 50 ms back
/// is jitter and is kept.
const JITTER_BACK: i64 = 50_000_000;
/// The default future horizon is 10 s, so a stamp 18.8 s ahead of its log time is
/// reported.
const FUTURE_STAMP: i64 = 20 * SECOND;
const FUTURE_LOG: i64 = 1_200_000_000;

/// The corpus: every reportable §3.2 row in one recording, on three edges.
///
/// * `base_link -> laser` (static) — **a static edge with differing values**: one
///   declaration and one contradiction.
/// * `map -> odom` (dynamic) — **a duplicate `(edge, stamp)`**, **a backward jump
///   below the threshold**, **a zero stamp**, and **a stamp far in the future**.
/// * `odom -> base_link` (dynamic) — every sample stamped zero, so the edge is
///   declared and carries nothing: §3.2's **frame declared, never published**, in
///   the only form a recording can show it.
///
/// The poses are distinct per message so that "the duplicate resolved to the last
/// occurrence" is checkable rather than merely counted.
fn corpus() -> Vec<FixtureMessage> {
    vec![
        // Row: static edge with differing values.
        FixtureMessage::static_edge("base_link", "laser", pose(1.0)),
        FixtureMessage::static_edge("base_link", "laser", pose(2.0)),
        // Row: an ordinary sample, so the edge is not entirely anomalies.
        FixtureMessage::dynamic("map", "odom", SECOND, pose(3.0)),
        // Row: duplicate (edge, stamp) — the second one wins.
        FixtureMessage::dynamic("map", "odom", SECOND + 100_000_000, pose(4.0)),
        FixtureMessage::dynamic("map", "odom", SECOND + 100_000_000, pose(5.0)),
        // Row: backward clock jump, below the threshold — counted as out of
        // order and kept, because §3.1 sorts.
        FixtureMessage::dynamic("map", "odom", SECOND + 100_000_000 - JITTER_BACK, pose(6.0)),
        // Row: zero stamp — dropped.
        FixtureMessage::dynamic("map", "odom", 0, pose(7.0)),
        // Row: stamp far in the future — kept, with the worst offset reported.
        FixtureMessage::dynamic("map", "odom", FUTURE_STAMP, pose(8.0)).logged_at(FUTURE_LOG),
        // Row: frame declared, never published — every sample is a zero stamp.
        FixtureMessage::dynamic("odom", "base_link", 0, pose(9.0)),
        FixtureMessage::dynamic("odom", "base_link", 0, pose(10.0)),
    ]
}

fn write(dir: &Path, name: &str, msgs: &[FixtureMessage]) -> PathBuf {
    let p = dir.join(name);
    write_mcap(&p, msgs).unwrap();
    p
}

/// **The exact report**, as one JSON document.
///
/// # Why the whole document and not a list of fields
///
/// §11 says *"asserting the exact ingest-report output"*, and the reason is in
/// the failure it prevents: a per-field assertion is satisfied by the wrong
/// anomaly being counted, because a field it does not name is free to move. Every
/// number below was derived by hand from `corpus()` before it was run — ten
/// transforms read, three dropped for a zero stamp, one duplicate collapsed, four
/// pushed — and the document is what makes an unstated field a failure too.
///
/// Only `source` and `tf_tree` are interpolated: one is a temporary path and the
/// other is the workspace version, and neither is a fact about the reader.
///
/// **Changing the report's shape will fail this test, deliberately.** The JSON is
/// a published surface with a schema tag (`tf_tree.ingest/2`) a consumer pins; a
/// field added or renamed without a bump is the thing this catches.
///
/// The per-row mutants are in this file's other tests and in the commit that
/// added it — one seeded violation per §3.2 row, not one for the corpus.
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

    // The tree agrees with the report: the duplicate resolved to the **last**
    // occurrence, which no counter can show. Bit-identical, because the pose went
    // in as an `f64` and came back through a little-endian CDR field.
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

/// Every reportable row reaches the **terminal summary** too, and says something
/// an operator can act on.
///
/// The JSON and the summary are two renderings of one structure and `report`'s
/// module docs say neither may carry what the other does not. A field asserted
/// only in the document above would satisfy that rule and still never be printed.
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
    // The rows whose count is zero stay silent, which is what makes the ones
    // above readable.
    assert!(!text.contains("backward clock jumps"), "{text}");
    assert!(!text.contains("--max-record-size"), "{text}");
}

/// §3.2's **edge kind changes mid-recording**: a hard error naming the timestamp.
///
/// Driven from the same corpus with one message appended, so this is the corpus's
/// sixth row rather than a separate fixture that happens to be nearby.
#[test]
fn the_corpus_plus_a_kind_change_is_a_hard_error() {
    let dir = Scratch::new("kind");
    let mut msgs = corpus();
    let stamp = 9 * SECOND;
    // `map -> odom` is dynamic throughout the corpus; the same edge on
    // `/tf_static` is the contradiction.
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

/// §3.2's **backward clock jump** past the threshold, under the default `halt`.
///
/// The corpus's own backward jump is 50 ms, below the 100 ms threshold, and is
/// kept and counted. This appends one an order of magnitude past it, which is the
/// other half of the same row.
#[test]
fn the_corpus_plus_a_real_reset_halts() {
    let dir = Scratch::new("reset");
    let mut msgs = corpus();
    // The corpus leaves `map -> odom` at 20 s; 1 s is nineteen seconds back, far
    // past the 100 ms threshold.
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
