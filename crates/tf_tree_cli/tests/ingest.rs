//! `tf_tree ingest` through the shipped binary — `docs/PHASE5.md` §3.
//!
//! The library's own suite proves the two passes classify a stream correctly.
//! What it cannot prove is that any of that is *reachable*: the argument
//! parsing, the unit conversions on `--max-memory` and `--future-horizon`, and
//! the exit status all live in the CLI and have their own ways of being wrong.
//! This runs the real binary, through `clap`, on a real file.
//!
//! The recording is synthetic — see `tf_tree_ingest::fixture`, which says so at
//! length. Nothing here came off a robot.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

use tf_tree_ingest::fixture::{small_recording, write_mcap, FixtureMessage};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p =
            std::env::temp_dir().join(format!("tf_tree_cli_ingest-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tf_tree() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tf_tree"))
}

/// The wedge path: point the binary at a recording, get the report, get a zero
/// exit status, and get a JSON file holding the same numbers as the summary.
///
/// Mutant: make `IngestArgs::to_options` pass `self.max_memory` as bytes rather
/// than MiB — applied, and this failed on `"passes":1`, which came back as
/// `"passes":2` with `"spilled_runs":2`: a 4 096 B cap turns a 160-sample
/// recording into two re-reads and a spill file. That unit conversion is what
/// this test exists for, and the two counters are how it is still visible —
/// until §3.1's spill file landed the mutant produced a hard error and the
/// status assertion caught it, which it no longer would.
#[test]
fn ingest_prints_a_report_and_writes_json() {
    let dir = Scratch::new("report");
    let bag = dir.0.join("run.mcap");
    let json = dir.0.join("run.json");
    write_mcap(&bag, &small_recording()).unwrap();

    let out = tf_tree()
        .arg("ingest")
        .arg("--bag")
        .arg(&bag)
        .arg("--report")
        .arg(&json)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "status {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("2 static edges, 3 dynamic edges"),
        "summary:\n{stdout}"
    );
    assert!(stdout.contains("160 samples stored"), "summary:\n{stdout}");
    // A clean recording prints no anomaly lines at all — §3.2's report is worth
    // reading precisely because it does not print ten zeroes.
    assert!(!stdout.contains(" ! "), "summary:\n{stdout}");

    let text = std::fs::read_to_string(&json).unwrap();
    assert!(text.contains("\"schema\":\"tf_tree.ingest/2\""), "{text}");
    assert!(text.contains("\"samples_pushed\":160"), "{text}");
    // The default cap is 4 096 **MiB**, so a 160-sample recording is one
    // re-read with nothing spilled. These two are the observable consequence of
    // the unit conversion in `IngestArgs::to_options`.
    assert!(text.contains("\"passes\":1"), "{text}");
    assert!(text.contains("\"spilled_runs\":0"), "{text}");
}

/// A recording this build cannot read fails non-zero and says what the file is
/// not, rather than exiting zero with an empty report.
///
/// Mutant: change `cmd_ingest` to `let _ = tf_tree_ingest::run(...)` and return
/// `Ok(())` — applied, and this test failed on the exit-status assertion.
#[test]
fn an_unreadable_recording_exits_non_zero() {
    let dir = Scratch::new("junk");
    let bag = dir.0.join("junk.mcap");
    std::fs::write(&bag, b"not an MCAP").unwrap();

    let out = tf_tree()
        .arg("ingest")
        .arg("--bag")
        .arg(&bag)
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected a non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("MCAP"),
        "the message should name the format: {stderr}"
    );
}

/// **`--clock-reset-threshold` is milliseconds**, and the conversion to
/// nanoseconds is the whole test.
///
/// Both directions are pinned, because only one of them catches the mutant. The
/// fixture regresses one edge by 150 ms: above the 100 ms default, so it halts;
/// below an explicit `--clock-reset-threshold 200`, so it does not. A missing
/// `* 1_000_000` turns that 200 into 200 **nanoseconds**, under which the 150 ms
/// regression is a reset again — which is why the passing half is here.
///
/// This is the flag a user reaches for first when an ingest halts, so a silent
/// factor of a million in it is the difference between the tool being usable and
/// appearing broken.
///
/// Mutant: drop the `.saturating_mul(1_000_000)` from
/// `IngestArgs::to_options` — applied, and the `--clock-reset-threshold 200` run
/// exited non-zero with a clock-reset error, failing this test.
#[test]
fn clock_reset_threshold_is_milliseconds() {
    let dir = Scratch::new("threshold");
    let bag = dir.0.join("skew.mcap");
    let msgs = vec![
        FixtureMessage::dynamic(
            "odom",
            "base_link",
            10_000_000_000,
            [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0],
        ),
        // 150 ms backwards on the same edge.
        FixtureMessage::dynamic(
            "odom",
            "base_link",
            9_850_000_000,
            [1.0, 0.0, 0.0, 0.0, 4.0, 5.0, 6.0],
        ),
    ];
    write_mcap(&bag, &msgs).unwrap();

    // Default 100 ms: 150 ms is a reset.
    let out = tf_tree()
        .arg("ingest")
        .arg("--bag")
        .arg(&bag)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "150 ms past the 100 ms default must halt"
    );

    // 200 ms: it is not.
    let out = tf_tree()
        .arg("ingest")
        .arg("--bag")
        .arg(&bag)
        .arg("--clock-reset-threshold")
        .arg("200")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "a 200 ms threshold must admit a 150 ms regression; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// **`--future-horizon` is seconds**, and the conversion to nanoseconds is the
/// whole test — same shape as `--clock-reset-threshold`, same reason.
///
/// The fixture stamps one transform 30 s ahead of its own log time. Under the
/// 10 s default that is reported; under `--future-horizon 60` it is not. A
/// missing `* 1e9` makes 60 into 60 nanoseconds, under which it is reported
/// again, so the second half is what kills the mutant.
///
/// Mutant: replace `self.future_horizon * 1e9` with `self.future_horizon` in
/// `IngestArgs::to_options` — applied, and the `--future-horizon 60` run still
/// printed the "ahead of when they were recorded" line, failing this test.
#[test]
fn future_horizon_is_seconds() {
    let dir = Scratch::new("horizon");
    let bag = dir.0.join("future.mcap");
    let msgs = vec![
        FixtureMessage::dynamic(
            "odom",
            "base_link",
            1_000_000_000,
            [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0],
        ),
        // Stamped 30 s ahead of when it was recorded.
        FixtureMessage::dynamic(
            "odom",
            "base_link",
            32_000_000_000,
            [1.0, 0.0, 0.0, 0.0, 4.0, 5.0, 6.0],
        )
        .logged_at(2_000_000_000),
    ];
    write_mcap(&bag, &msgs).unwrap();

    const LINE: &str = "ahead of when they were recorded";

    let out = tf_tree()
        .arg("ingest")
        .arg("--bag")
        .arg(&bag)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains(LINE),
        "30 s past the 10 s default must be reported:\n{stdout}"
    );

    let out = tf_tree()
        .arg("ingest")
        .arg("--bag")
        .arg(&bag)
        .arg("--future-horizon")
        .arg("60")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        !stdout.contains(LINE),
        "a 60 s horizon must admit a 30 s lead:\n{stdout}"
    );
}

/// `--tf-prefix` reaches the normalizer, and `--static-topic` reaches the topic
/// roles. Both are plain field copies in `to_options` and both were previously
/// replaceable with a constant without any test noticing.
///
/// The static topic is called `/fixed_frames` on purpose: it does not end in
/// `tf_static`, so the suffix rule cannot classify it and only the flag can.
///
/// Mutant: `tf_prefix: None` in `to_options` — applied, and the frame-name
/// assertion failed (the JSON carried `odom`, not `robot1/odom`). Mutant 2:
/// `static_topics: Vec::new()` — applied, and the static-edge count came back 0
/// with two dynamic edges instead of 1 and 1.
#[test]
fn tf_prefix_and_static_topic_reach_the_library() {
    let dir = Scratch::new("prefix");
    let bag = dir.0.join("prefix.mcap");
    let json = dir.0.join("prefix.json");
    let mut msgs = vec![FixtureMessage {
        topic: "/fixed_frames".into(),
        log_time_ns: 0,
        transforms: vec![tf_tree_ingest::cdr::TransformStamped {
            stamp_ns: 0,
            frame_id: "base_link".into(),
            child_frame_id: "laser".into(),
            pose: [1.0, 0.0, 0.0, 0.0, 0.1, 0.2, 0.3],
        }],
    }];
    for i in 1..5i64 {
        msgs.push(FixtureMessage::dynamic(
            "odom",
            "base_link",
            i * 1_000_000_000,
            [1.0, 0.0, 0.0, 0.0, i as f64, 0.0, 0.0],
        ));
    }
    write_mcap(&bag, &msgs).unwrap();

    let out = tf_tree()
        .arg("ingest")
        .arg("--bag")
        .arg(&bag)
        .arg("--report")
        .arg(&json)
        .arg("--tf-prefix")
        .arg("robot1")
        .arg("--static-topic")
        .arg("/fixed_frames")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("1 static edges, 1 dynamic edges"),
        "--static-topic did not classify /fixed_frames:\n{stdout}"
    );

    let text = std::fs::read_to_string(&json).unwrap();
    assert!(
        text.contains("\"robot1/odom\"") && text.contains("\"robot1/base_link\""),
        "--tf-prefix did not reach the normalizer: {text}"
    );
    assert!(
        !text.contains("\"parent\":\"odom\""),
        "an unprefixed name survived: {text}"
    );
}

/// `--on-clock-reset=split` is accepted by the parser and refused by the tool,
/// with the reason — §3.2 lists the value, so rejecting the *spelling* would
/// leave a user unable to tell a typo from a missing feature.
///
/// Mutant: remove `Split` from `ClockResetArg` — applied, and this test failed
/// with clap's "invalid value 'split'" on stderr instead of the reason.
#[test]
fn split_is_a_known_value_that_is_refused_with_a_reason() {
    let dir = Scratch::new("split");
    let bag = dir.0.join("split.mcap");
    let msgs = vec![
        FixtureMessage::dynamic(
            "odom",
            "base_link",
            40_000_000_000,
            [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0],
        ),
        FixtureMessage::dynamic(
            "odom",
            "base_link",
            1_000_000_000,
            [1.0, 0.0, 0.0, 0.0, 4.0, 5.0, 6.0],
        ),
    ];
    write_mcap(&bag, &msgs).unwrap();

    let out = tf_tree()
        .arg("ingest")
        .arg("--bag")
        .arg(&bag)
        .arg("--on-clock-reset")
        .arg("split")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not implemented") && stderr.contains("PHASE5"),
        "the refusal should carry its reason: {stderr}"
    );
}

/// **The CLI's `compression` feature must actually control the reader's**, and a
/// compressed bag must survive the whole binary.
///
/// The manifest comment on `compression = ["tf_tree_ingest/compression"]` argues that
/// the line is load-bearing, and it is right: the workspace declares `tf_tree_ingest`
/// with `default-features = false`, so deleting it ships a `cargo install` that
/// refuses every zstd bag — the ordinary rosbag2/Foxglove case — while
/// `cargo build --workspace`, `just lint` and `cargo nextest run --workspace` all
/// stay green, because building `tf_tree_ingest` as a workspace member turns its own
/// defaults on and feature unification covers for the missing edge. Nothing asserted
/// it. `counters` has had exactly this test since the same defect bit it once.
///
/// Mutant: replace the `tf_tree_ingest` dev-dependency's `workspace = true` (which
/// carries the workspace's `default-features = false`) with a plain path dependency,
/// so its defaults come back — applied, and under
/// `cargo nextest run -p tf_tree_cli --no-default-features` this failed with
/// `left: false, right: true`: "the CLI asked for compression=false but the reader was
/// built with compression=true". It was the only failure in the crate. That is the
/// counters defect exactly, one feature over.
///
/// **What this test cannot catch, stated because the reviewer's suggested fix assumed
/// it could.** Deleting `compression` from `[features] default` *and* from
/// `[features]` — the failure the manifest comment warns about — leaves this
/// assertion passing: both sides go `false` together, and
/// `a_zstd_bag_ingests_through_the_binary` below compiles to nothing. Applied and
/// measured: 117 tests ran and 117 passed. Every `cfg!` in this crate is relative to
/// the configuration being deleted, so no in-crate test can see it. That invariant is
/// asserted against the dependency graph instead, in `just ingest-check`
/// (`cargo tree -p tf_tree_cli -e normal | grep -q ruzstd`), which does fire.
#[test]
fn the_cli_compression_feature_switches_the_reader() {
    assert_eq!(
        cfg!(feature = "compression"),
        tf_tree_ingest::compression_compiled_in(),
        "the CLI asked for compression={} but the reader was built with \
         compression={}; the tf_tree_ingest/compression feature edge is broken, or \
         some dependency is re-enabling it through its own defaults",
        cfg!(feature = "compression"),
        tf_tree_ingest::compression_compiled_in()
    );
}

/// A zstd-compressed bag ingests through the shipped binary.
///
/// Separate from the predicate above because it proves the other direction: that the
/// feature being on means the *binary* reads a compressed recording, not merely that
/// two `cfg!`s agree. No `tf_tree_cli` test did that before — `grep -rn FixtureCodec
/// crates/tf_tree_cli/` returned nothing — so the CLI's headline capability was
/// exercised only one crate down. Gated on `compression`, because the codec-free
/// build refuses this bag by design and `fixture::chunked_mcap_bytes` will not even
/// write it.
///
/// Mutant: `IngestArgs::to_options` hard-coding `max_chunk_expansion_ratio: 1` —
/// applied, and this failed with `status Some(1)` and
/// `the recording contains no tf2_msgs/msg/TFMessage transforms` on stderr: every
/// chunk expands by more than 1×, so every chunk is refused and an intact recording
/// reads as empty. It killed `tests::the_chunk_bounds_default_to_the_librarys_and_are_settable`
/// with it — but that unit test only compares two numbers, while this one shows what
/// the wrong number does to a real file through the real binary.
#[cfg(feature = "compression")]
#[test]
fn a_zstd_bag_ingests_through_the_binary() {
    use tf_tree_ingest::fixture::{write_mcap_chunked, ChunkedSpec, FixtureCodec};

    let dir = Scratch::new("zstd");
    let bag = dir.0.join("zstd.mcap");
    write_mcap_chunked(
        &bag,
        &small_recording(),
        ChunkedSpec::new(8).compressed(FixtureCodec::Zstd),
    )
    .unwrap();

    let out = tf_tree()
        .arg("ingest")
        .arg("--bag")
        .arg(&bag)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "status {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    // The same counts the uncompressed fixture of this corpus produces, so the
    // decompression is transparent rather than merely non-fatal.
    assert!(
        stdout.contains("2 static edges, 3 dynamic edges"),
        "summary:\n{stdout}"
    );
    assert!(stdout.contains("160 samples stored"), "summary:\n{stdout}");
    assert!(
        !stdout.contains(" ! "),
        "a clean bag reports no anomalies:\n{stdout}"
    );
}
