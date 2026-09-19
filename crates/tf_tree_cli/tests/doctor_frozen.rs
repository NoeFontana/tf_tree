//! `tf_tree doctor --from-file` against a frozen `.tft` — `docs/PHASE5.md` §2, §6.
//!
//! Needs the frozen backend; `just shm-check` runs it. `TFT018`/`TFT019` skip on
//! a `.tft` (a ring holds only accepted pushes, so they would always pass);
//! `doctor_recording.rs` is where they fire.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

use tf_tree_ingest::fixture::{small_recording, write_mcap};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p =
            std::env::temp_dir().join(format!("tf_tree_cli_frozen-{}-{tag}", std::process::id()));
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

/// Write a small recording into `dir` and freeze it, returning the `.tft`.
/// The freeze runs in a subprocess, so the participant table names an exited process.
fn freeze_a_recording(dir: &Scratch) -> PathBuf {
    let bag = dir.0.join("run.mcap");
    let tft = dir.0.join("run.tft");
    write_mcap(&bag, &small_recording()).unwrap();

    let freeze = tf_tree()
        .arg("freeze")
        .arg("--from-bag")
        .arg(&bag)
        .arg("--out")
        .arg(&tft)
        .output()
        .unwrap();
    assert!(
        freeze.status.success(),
        "freeze failed: {}",
        String::from_utf8_lossy(&freeze.stderr)
    );
    tft
}

/// `doctor --from-file <tft> --json`, asserted to succeed.
fn doctor_json(tft: &PathBuf) -> String {
    let out = tf_tree()
        .arg("doctor")
        .arg("--from-file")
        .arg(tft)
        .arg("--json")
        .output()
        .unwrap();
    let json = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{json}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    json
}

/// The `--json` object for `id`, wide enough for its status, reason and first finding.
fn outcome_of<'a>(json: &'a str, id: &str) -> &'a str {
    let at = json
        .find(&format!("\"id\": \"{id}\""))
        .unwrap_or_else(|| panic!("{id} missing:\n{json}"));
    &json[at..(at + 1200).min(json.len())]
}

/// A frozen `.tft` is a `doctor` source read by the same code as a live arena
/// (§2.1), and `TFT018`/`TFT019` skip with the arena-specific reason
/// (`checks::PushStream` has four variants for this).
#[test]
fn a_frozen_index_is_a_doctor_source_and_the_two_stream_checks_skip_on_it() {
    let dir = Scratch::new("wiring");
    let json = doctor_json(&freeze_a_recording(&dir));

    assert!(
        json.contains("\"source\": \"frozen .tft index\""),
        "the report must name what it read:\n{json}"
    );
    assert!(
        json.contains("\"frames\": 6") && json.contains("\"edges\": 5"),
        "the arena's own shape must survive the round trip:\n{json}"
    );

    for id in ["TFT018", "TFT019"] {
        let window = outcome_of(&json, id);
        assert!(
            window.contains("\"status\": \"skipped\""),
            "{id} must not claim a verdict it cannot have: {window}"
        );
        assert!(
            window.contains("only the pushes the engine accepted"),
            "{id}'s reason must be the arena's, not the live ring's: {window}"
        );
        assert!(
            window.contains("--from-bag"),
            "{id} must point at the source that can answer: {window}"
        );
    }
}

/// `TFT014` skips on a `.tft`: the freeze copies a `LIVE` participant record for
/// the already-exited `tf_tree freeze` process, so [`SlotTable::Image`] must
/// skip the liveness probe (§2.3).
///
/// [`SlotTable::Image`]: tf_tree_cli::checks::SlotTable::Image
#[test]
fn a_frozen_index_is_not_asked_whether_its_participants_are_running() {
    let dir = Scratch::new("slots");
    let json = doctor_json(&freeze_a_recording(&dir));

    let window = outcome_of(&json, "TFT014");
    assert!(
        window.contains("\"status\": \"skipped\""),
        "TFT014 must not judge a copy of a participant table: {window}"
    );
    assert!(
        window.contains("byte copy of the whole arena"),
        "TFT014's reason must name why a file cannot answer: {window}"
    );
    assert!(
        window.contains("--attach"),
        "the skip must send the operator to the source that can answer: {window}"
    );
}

/// A recording handed to `--from-file` is told to use `--from-bag`.
#[test]
fn a_recording_handed_to_from_file_is_pointed_at_from_bag() {
    let dir = Scratch::new("wrongflag");
    let bag = dir.0.join("run.mcap");
    write_mcap(&bag, &small_recording()).unwrap();

    let out = tf_tree()
        .arg("doctor")
        .arg("--from-file")
        .arg(&bag)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("use --from-bag"),
        "the error must name the flag that reads this file: {stderr}"
    );
    assert!(
        stderr.contains(&bag.display().to_string()),
        "the error must name the file: {stderr}"
    );
}

/// `--attach` and a recording flag are refused together, on either side of the
/// subcommand. The check lives in `doctor_source` because `--attach` is
/// `global = true` and clap's `conflicts_with` misses the pre-subcommand form.
#[test]
fn attach_and_a_recording_source_are_mutually_exclusive_in_both_orders() {
    let dir = Scratch::new("conflict");
    let bag = dir.0.join("clean.mcap");
    write_mcap(&bag, &small_recording()).unwrap();

    for args in [
        vec!["--attach", "--name", "doesnotexist", "doctor", "--from-bag"],
        vec!["doctor", "--from-bag"],
    ] {
        let trailing = args[0] == "doctor";
        let mut cmd = tf_tree();
        cmd.args(&args).arg(&bag);
        if trailing {
            cmd.arg("--attach");
        }
        let out = cmd.output().unwrap();
        assert!(
            !out.status.success(),
            "{args:?} (trailing --attach: {trailing}) reported on the bag while naming a live \
             arena:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("--attach") && stderr.contains("--from-bag"),
            "the error must name both flags: {stderr}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("catalogue checks"),
            "no report may be printed alongside the refusal"
        );
    }
}

/// `freeze --from-live` refuses an ingest flag it cannot act on, before the
/// attach is attempted.
#[test]
fn freeze_from_live_refuses_an_ingest_flag_it_will_ignore() {
    let dir = Scratch::new("freeze-flags");
    let out = dir.0.join("x.tft");

    let res = tf_tree()
        .args(["freeze", "--from-live", "--out"])
        .arg(&out)
        .args(["--tf-prefix", "robot1"])
        .env("TF_TREE_RUNTIME_DIR", &dir.0)
        .output()
        .unwrap();
    assert!(!res.status.success());
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(
        stderr.contains("--tf-prefix") && stderr.contains("--from-bag"),
        "the flag must be refused before the attach is attempted, naming itself \
         and the source that would give it meaning: {stderr}"
    );
    assert!(
        !out.exists(),
        "a refused invocation must not have written an index"
    );
}
