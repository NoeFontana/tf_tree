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
/// than MiB — applied, and the run failed with `EdgeExceedsMemoryCap` (4 096 B
/// against a 6 400 B edge), so the process exited non-zero and this test failed
/// on the status assertion. That unit conversion is what this test exists for.
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
    assert!(text.contains("\"schema\":\"tf_tree.ingest/1\""), "{text}");
    assert!(text.contains("\"samples_pushed\":160"), "{text}");
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
