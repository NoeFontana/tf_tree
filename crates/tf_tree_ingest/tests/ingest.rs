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
use tf_tree_ingest::fixture::{small_recording, write_mcap, FixtureMessage};
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
/// Mutant: in `ingest::fill`, drop the `if rec.is_static` early-return in the
/// pass-two callback so static samples are buffered as dynamic ones — applied,
/// and this test failed with `Claim(NotDeclaredDynamic)` on `base_link ->
/// laser`. A second mutant, making `Capacity::slots(e.samples)` into
/// `Capacity::slots(1)`, also failed: the 100-sample edge lapped and the
/// `samples_pushed` assertion saw the ring, not the source.
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
/// applied, and this test failed with `Push(NonMonotonicStamp { .. })` before
/// it ever reached the comparison. Mutant 2: change the stable `sort_by_key` to
/// `sort_unstable_by_key` — applied, and the *duplicate* test below
/// (`duplicates_resolve_last_wins`) failed instead; this one still passed,
/// because it has no duplicates. Both are recorded because the second shows
/// this test alone does not guard stability.
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
/// Mutant: change the last-wins skip to keep the first (`if i > 0 && buf[i-1].0
/// == stamp { continue; }`) — applied, and the lookup assertion failed, reading
/// back the 1.0 pose instead of the 2.0 one.
#[test]
fn duplicates_resolve_last_wins() {
    let dir = Scratch::new("dupes");
    let msgs = vec![
        FixtureMessage::dynamic("odom", "base_link", 1_000_000_000, pose(0.5)),
        FixtureMessage::dynamic("odom", "base_link", 2_000_000_000, pose(1.0)),
        // Same edge, same stamp, different value, later in the recording.
        FixtureMessage::dynamic("odom", "base_link", 2_000_000_000, pose(2.0)),
        FixtureMessage::dynamic("odom", "base_link", 3_000_000_000, pose(3.0)),
    ];
    let path = write(&dir, "dupes.mcap", &msgs);
    let mut frames = Frames::default();
    let out = tf_tree_ingest::run(&path, &IngestOptions::default(), &mut frames).unwrap();

    assert_eq!(out.report.anomalies.duplicate_stamps, 1);
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
/// Mutant: make the `StaticVerdict::KindChanged` arm a counted anomaly instead
/// of an error — applied, and this test failed with `Ok`.
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
    assert_eq!(
        err,
        IngestError::ClockReset {
            at_ns: 11_000_000_000,
            by_ns: 30_000_000_000,
        }
    );
}

/// §3.2's `split` policy is refused with a reason rather than silently doing
/// something else.
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

/// §11's "spill path" row, in the form §3.1's cap is actually implemented:
/// a `--max-memory` below the dataset size splits pass two into several
/// re-reads, and the result is identical to the single-pass one.
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
/// Mutant: emit `rate_hz` as `{r}` without the finite check and feed it a
/// single-sample edge — applied by removing the `is_finite` guard and dividing
/// by a zero span, and the `NaN` assertion failed.
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
