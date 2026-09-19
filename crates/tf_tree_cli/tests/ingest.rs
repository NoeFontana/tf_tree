//! `tf_tree ingest` through the shipped binary — `docs/PHASE5.md` §3. Pins the
//! CLI-only layer (argument parsing, unit conversions, exit status) on a
//! synthetic recording (`tf_tree_ingest::fixture`).

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

/// The wedge path: a recording yields the report, a zero exit status and a JSON
/// file holding the same numbers. `"passes":1` / `"spilled_runs":0` pin the
/// `--max-memory` MiB conversion in `IngestArgs::to_options`.
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
    // A clean recording prints no anomaly lines (§3.2).
    assert!(!stdout.contains(" ! "), "summary:\n{stdout}");

    let text = std::fs::read_to_string(&json).unwrap();
    assert!(text.contains("\"schema\":\"tf_tree.ingest/2\""), "{text}");
    assert!(text.contains("\"samples_pushed\":160"), "{text}");
    // The default cap is 4 096 MiB: one pass, nothing spilled.
    assert!(text.contains("\"passes\":1"), "{text}");
    assert!(text.contains("\"spilled_runs\":0"), "{text}");
}

/// A recording this build cannot read fails non-zero and says what the file is not.
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

/// `--clock-reset-threshold` is milliseconds. Both directions are pinned: a
/// 150 ms regression halts under the 100 ms default and not under an explicit
/// 200 (a missing `* 1_000_000` would make that 200 ns).
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

/// `--future-horizon` is seconds, pinned in both directions as above: a
/// transform 30 s ahead is reported under the 10 s default and not under 60.
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

/// `--tf-prefix` reaches the normalizer and `--static-topic` the topic roles.
/// The topic is `/fixed_frames` so the `tf_static` suffix rule cannot classify it.
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

/// `--on-clock-reset=split` is accepted by the parser and refused by the tool
/// with the reason (§3.2 lists the value).
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

/// The CLI's `compression` feature must control the reader's, and a compressed
/// bag must survive the binary. Cannot catch deleting the feature entirely; that
/// is asserted against the dependency graph by `just ingest-check`.
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

/// A zstd-compressed bag ingests through the shipped binary. Gated on
/// `compression`: the codec-free build refuses this bag by design.
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
    // Same counts as the uncompressed fixture: decompression is transparent.
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
