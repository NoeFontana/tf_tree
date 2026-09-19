//! `tf_tree doctor --from-bag` through the shipped binary — `docs/PHASE5.md` §6.
//!
//! Pins that a recording reaches the checks (ingest, arrival-order replay, the
//! edge-id join); a join that resolved nothing would make `TFT018` pass on
//! everything. Recordings are synthetic (`tf_tree_ingest::fixture`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

use tf_tree_ingest::fixture::{small_recording, write_mcap, FixtureMessage};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p =
            std::env::temp_dir().join(format!("tf_tree_cli_doctor-{}-{tag}", std::process::id()));
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

/// `doctor --from-bag <path> --json`, returning `(stdout, stderr, exit ok)`.
fn doctor_json(bag: &Path, extra: &[&str]) -> (String, String, bool) {
    let out = tf_tree()
        .arg("doctor")
        .arg("--from-bag")
        .arg(bag)
        .arg("--json")
        .args(extra)
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// The `status` of one catalogue id in a `--json` report, parsed by position
/// (`catalogue::render_json` has a fixed field order).
fn status_of(json: &str, id: &str) -> String {
    let lines: Vec<&str> = json.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.trim() == format!("\"id\": \"{id}\","))
        .unwrap_or_else(|| panic!("{id} is not in the report:\n{json}"));
    let line = lines
        .get(at + 3)
        .unwrap_or_else(|| panic!("{id} has no status line:\n{json}"));
    assert!(
        line.trim_start().starts_with("\"status\""),
        "the field order render_json writes moved; this extractor reads {line:?}"
    );
    line.trim()
        .trim_start_matches("\"status\": \"")
        .trim_end_matches("\",")
        .to_owned()
}

/// The `reason` of one catalogue id, or `""` when it is `null`.
fn reason_of(json: &str, id: &str) -> String {
    let lines: Vec<&str> = json.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.trim() == format!("\"id\": \"{id}\","))
        .unwrap_or_else(|| panic!("{id} is not in the report:\n{json}"));
    let line = lines.get(at + 5).expect("no reason line");
    assert!(
        line.trim_start().starts_with("\"reason\""),
        "the field order render_json writes moved; this extractor reads {line:?}"
    );
    line.trim()
        .trim_start_matches("\"reason\": ")
        .trim_start_matches('"')
        .trim_end_matches(',')
        .trim_end_matches('"')
        .to_owned()
}

/// `doctor` on a bag reaches a verdict on `TFT018`/`TFT019`, which no live arena
/// can. `TFT001` still skips, with the recording's own reason (a `TFMessage` has
/// no sender field, `docs/PHASE4.md` §1.3). `pass` is also what an empty arrival
/// stream yields, so the two tests below are what forbid a vacuous join.
#[test]
fn a_recording_is_a_doctor_source_and_the_two_dead_checks_reach_a_verdict() {
    let dir = Scratch::new("wedge");
    let bag = dir.0.join("clean.mcap");
    write_mcap(&bag, &small_recording()).unwrap();

    let (json, stderr, ok) = doctor_json(&bag, &[]);
    assert!(ok, "a healthy recording must not gate:\n{json}\n{stderr}");
    assert!(
        json.contains("\"source\": \"MCAP recording\""),
        "the report must name what it read:\n{json}"
    );

    assert_eq!(
        status_of(&json, "TFT018"),
        "pass",
        "TFT018 must run on a recording, not skip:\n{json}"
    );
    assert_eq!(
        status_of(&json, "TFT019"),
        "pass",
        "TFT019 must run on a recording, not skip:\n{json}"
    );

    let why = reason_of(&json, "TFT001");
    assert_eq!(status_of(&json, "TFT001"), "skipped", "{json}");
    assert!(
        why.contains("no publisher identity") && why.contains("PHASE4.md §1.3"),
        "TFT001 must skip for the recording's own reason, not the ring's: {why}"
    );
}

/// An out-of-order arrival in the recording is reported, though the arena built
/// from it is monotone (`SampleRing::push` rejects it; `docs/PHASE5.md` §3.1
/// sorts). The replay therefore reads the recording's log order.
#[test]
fn an_out_of_order_arrival_in_a_recording_fires_tft018() {
    let dir = Scratch::new("inversion");
    let bag = dir.0.join("inverted.mcap");
    let pose = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let mut msgs = Vec::new();
    for i in 0..20i64 {
        msgs.push(FixtureMessage::dynamic(
            "odom",
            "base_link",
            1_000_000_000 + i * 10_000_000,
            pose,
        ));
    }
    // One arrival out of place by 30 ms, under the 100 ms clock-reset threshold.
    msgs.push(FixtureMessage::dynamic(
        "odom",
        "base_link",
        1_000_000_000 + 16 * 10_000_000,
        pose,
    ));
    msgs.push(FixtureMessage::dynamic(
        "odom",
        "base_link",
        1_000_000_000 + 20 * 10_000_000,
        pose,
    ));
    write_mcap(&bag, &msgs).unwrap();

    let (json, stderr, ok) = doctor_json(&bag, &["--exit-code"]);
    assert_eq!(
        status_of(&json, "TFT018"),
        "fired",
        "the recording holds an inversion the arena cannot:\n{json}\n{stderr}"
    );
    assert!(
        !ok,
        "TFT018 is an error severity, so --exit-code must gate on it"
    );
    assert!(
        json.contains("out-of-order stamp arrival"),
        "the finding must say what it saw:\n{json}"
    );
}

/// A wall clock stepping backwards inside a recording is attributed to the clock
/// (`TFT019`, `docs/API.md` §5.3). The 90 ms step is under the 100 ms
/// `--clock-reset-threshold` and rejects nine arrivals, above
/// `checks::CLOCK_STEP_MIN_REJECTED_RUN`.
#[test]
fn a_backwards_wall_clock_in_a_recording_is_attributed_by_tft019() {
    let dir = Scratch::new("clockstep");
    let bag = dir.0.join("stepped.mcap");
    let pose = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let base = 1_700_000_000_000_000_000i64;
    let mut msgs = Vec::new();
    for i in 0..10i64 {
        msgs.push(FixtureMessage::dynamic(
            "odom",
            "base_link",
            base + i * 10_000_000,
            pose,
        ));
    }
    // The clock steps back 90 ms and the publisher carries on at the same rate.
    for i in 0..10i64 {
        msgs.push(FixtureMessage::dynamic(
            "odom",
            "base_link",
            base + i * 10_000_000,
            pose,
        ));
    }
    write_mcap(&bag, &msgs).unwrap();

    let (json, stderr, _) = doctor_json(&bag, &[]);
    assert!(!json.is_empty(), "doctor produced nothing: {stderr}");
    assert_eq!(
        status_of(&json, "TFT018"),
        "fired",
        "the rejected arrivals themselves are still an error:\n{json}\n{stderr}"
    );
    assert_eq!(
        status_of(&json, "TFT019"),
        "fired",
        "a concentrated burst on a wall-clock edge is a clock step:\n{json}"
    );
    assert!(
        json.contains("consecutive rejected"),
        "the attribution must show its concentration evidence:\n{json}"
    );
}

/// The §3.2 ingest report goes to stderr, so `--json` stdout stays a document.
#[test]
fn the_ingest_report_lands_on_stderr_and_leaves_stdout_parseable() {
    let dir = Scratch::new("streams");
    let bag = dir.0.join("clean.mcap");
    write_mcap(&bag, &small_recording()).unwrap();

    let (json, stderr, ok) = doctor_json(&bag, &[]);
    assert!(ok, "{json}\n{stderr}");
    assert!(
        json.trim_start().starts_with('{') && json.trim_end().ends_with('}'),
        "stdout must be a JSON document and nothing else:\n{json}"
    );
    assert!(
        stderr.contains("2 static edges, 3 dynamic edges"),
        "the ingest report is the other half of what a stranger needs:\n{stderr}"
    );
}

/// A file that is not a recording is diagnosed, not reported as a healthy tree,
/// and the error names the file.
#[test]
fn a_file_that_is_not_a_recording_is_refused_rather_than_silently_replaced() {
    let dir = Scratch::new("garbage");
    let bag = dir.0.join("notes.txt");
    std::fs::write(&bag, b"this is not an MCAP recording\n").unwrap();

    let out = tf_tree()
        .arg("doctor")
        .arg("--from-bag")
        .arg(&bag)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("well-formed MCAP"),
        "the error must say what was wrong with the file: {stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("catalogue checks"),
        "a refused source must not fall back to the fixture and report on it"
    );
}

/// The counter checks skip on a bag-built arena (its counters are all zero, which
/// reads as healthy). The reason differs per feature configuration
/// (`just ingest-check` runs both), so it is read from the document, not a `cfg`.
#[test]
fn a_bag_built_arena_skips_the_counter_checks_rather_than_passing_them() {
    let dir = Scratch::new("counters");
    let bag = dir.0.join("clean.mcap");
    write_mcap(&bag, &small_recording()).unwrap();

    let (json, stderr, ok) = doctor_json(&bag, &[]);
    assert!(ok, "{json}\n{stderr}");

    // The counter half's expected sentence for this build.
    let counter_reason = if json.contains("\"counters_compiled_in\": true") {
        "served a lookup"
    } else {
        "`counters` feature"
    };

    assert_eq!(
        status_of(&json, "TFT010"),
        "skipped",
        "an arena nobody has looked anything up in cannot report an extrapolation hotspot:\n{json}"
    );
    let why = reason_of(&json, "TFT010");
    assert!(
        why.contains(counter_reason),
        "the skip has to name the reason, not just decline: {why}"
    );

    assert_eq!(
        status_of(&json, "TFT011"),
        "skipped",
        "neither of TFT011's halves has evidence on a recording:\n{json}"
    );
    let why = reason_of(&json, "TFT011");
    assert!(
        why.contains(counter_reason) && why.contains("recorder's clock"),
        "both halves must say why they are blind: {why}"
    );
}

/// `TFT017` fires on every dynamic edge of a recording (the ingest releases its
/// claims); it stays a warn, and `Meta.notes` discloses why.
#[test]
fn an_all_unclaimed_arena_is_warned_about_and_explained() {
    let dir = Scratch::new("unclaimed");
    let bag = dir.0.join("clean.mcap");
    write_mcap(&bag, &small_recording()).unwrap();

    let (json, stderr, ok) = doctor_json(&bag, &[]);
    assert!(ok, "{json}\n{stderr}");
    assert_eq!(status_of(&json, "TFT017"), "fired", "{json}");
    assert!(
        json.contains("dynamic edge(s), so it names this arena rather than any edge in it"),
        "no note explaining an all-unclaimed arena:\n{json}"
    );
}

/// An ingest flag `doctor` cannot act on is refused, not ignored.
#[test]
fn an_ingest_flag_without_a_recording_is_refused() {
    let out = tf_tree()
        .arg("doctor")
        .arg("--tf-prefix")
        .arg("robot1")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a flag that does nothing must not be accepted silently:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--tf-prefix") && stderr.contains("--from-bag"),
        "the error must name the flag and the flag that would make it mean something: {stderr}"
    );

    // The same flag with a recording is accepted.
    let dir = Scratch::new("prefixed");
    let bag = dir.0.join("clean.mcap");
    write_mcap(&bag, &small_recording()).unwrap();
    let (json, stderr, ok) = doctor_json(&bag, &["--tf-prefix", "robot1"]);
    assert!(ok, "{json}\n{stderr}");
    assert!(
        json.contains("robot1/"),
        "the prefix reached the arena:\n{json}"
    );
}

/// A recording too large for `--max-memory` is refused, not truncated, so
/// `TFT018`/`TFT019` cannot pass on a prefix.
#[test]
fn a_recording_that_will_not_fit_in_max_memory_is_refused() {
    let dir = Scratch::new("bounded");
    let bag = dir.0.join("clean.mcap");
    write_mcap(&bag, &small_recording()).unwrap();

    let out = tf_tree()
        .arg("doctor")
        .arg("--from-bag")
        .arg(&bag)
        .arg("--max-memory")
        .arg("0")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--max-memory") && stderr.contains("TFT018"),
        "the error must name the flag to raise and what a truncation would cost: {stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("catalogue checks"),
        "no partial report may be printed alongside the refusal"
    );

    // The same recording at the default bound is fine.
    let (json, stderr, ok) = doctor_json(&bag, &[]);
    assert!(ok, "{json}\n{stderr}");
}
