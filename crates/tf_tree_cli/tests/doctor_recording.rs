//! `tf_tree doctor --from-bag` through the shipped binary — `docs/PHASE5.md` §6.
//!
//! # What this file is for
//!
//! `doctor`'s checks are unit-tested in `src/checks.rs` against hand-built
//! inputs. What that cannot show is that a **recording** reaches them: the flag,
//! the ingest, the arrival-order replay, the edge-id join between the recording's
//! frame names and the arena's, and the two streams the report lands on. Every
//! one of those lives between `clap` and `checks::run` and has its own way of
//! being silently wrong — in particular, a join that resolved nothing would make
//! `TFT018` pass on every recording, which is exactly the fabricated all-clear
//! this source exists to remove.
//!
//! So this runs the real binary, through `clap`, on a real file.
//!
//! The recordings are synthetic — see `tf_tree_ingest::fixture`, which says so
//! at length. Nothing here came off a robot.

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

/// The `status` of one catalogue id in a `--json` report.
///
/// The document is written line by line by `catalogue::render_json`, in a fixed
/// field order, so the status is the third line after the id. Parsing it by
/// position rather than pulling in a JSON crate keeps this test's dependency
/// footprint at zero, which is the same argument `render_json` itself makes for
/// being hand-written.
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

/// **The wedge, in one command: point `doctor` at a bag and it reaches a
/// verdict on the two checks no live arena can.**
///
/// `TFT018` and `TFT019` were structurally dead outside the built-in fixture:
/// `doctor`'s only other source was a live `--attach`, where the push stream is
/// reconstructed from a ring being written while it is read, so both skipped and
/// no run against real data could ever produce an answer. This is that hole
/// closed — and the assertion is `pass`, not merely "not skipped", because a
/// recording of a healthy publisher is what a stranger's first run looks like.
///
/// `TFT001` is asserted to still skip, and to skip for the **recording's** own
/// reason. It is the check `docs/PHASE4.md` §1.3 predicts a real stack will
/// fail, and a bag cannot answer it: a `tf2_msgs/TFMessage` has no sender field.
/// Saying "a live arena's rings remember the current owner" there would be a
/// true sentence about a source this run did not use.
///
/// **This test cannot be its own non-vacuity check, and that is stated rather
/// than papered over.** `pass` is also what an *empty* arrival stream produces —
/// a frame-name join that resolved nothing would satisfy every assertion here.
/// The two tests below are what forbid that: applying the mutant
/// `arrival_observations` → `Ok(Observations::new())` leaves this test green and
/// fails both of them, because a stream with no samples in it cannot fire.
///
/// Mutant: give `Source::Bag` the stream `PushStream::RingsAtRest` in `lib.rs`'s
/// `Source::stream` — the state before this branch, expressed for a recording.
/// Applied, and this failed with
/// `TFT018 must run on a recording, not skip: ... "status": "skipped"`.
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

/// **An out-of-order arrival in the recording is reported, and the arena it was
/// ingested into could never have shown it.**
///
/// This is the whole argument for replaying the recording's log order rather
/// than the arena's rings. `SampleRing::push` rejects a stamp older than the
/// ring's last, so a ring holds only accepted pushes; `docs/PHASE5.md` §3.1
/// additionally *sorts* every edge before pushing. The arena built from this
/// exact file is therefore perfectly monotone, and a check reading it would
/// pass. The file is not, and the check reading the file fires.
///
/// Mutant: read the arrival stream from the arena instead — replace the
/// `Source::Bag(obs)` arm's observations with `Observations::from_arena(tree,
/// &snap)` in `lib.rs`'s `observations`. Applied, and this failed with
/// `left: "pass", right: "fired"`: the sorted arena shows no inversion, which is
/// the vacuous pass this test exists to forbid.
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
    // One arrival out of place, by 30 ms — well under the 100 ms clock-reset
    // threshold, so ingest keeps it and counts it rather than halting.
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

/// **A wall clock stepping backwards inside a recording is attributed to the
/// clock, not to the publisher.**
///
/// `docs/API.md` §5.3 is the argument: an NTP step surfaces as a burst of
/// `NonMonotonicStamp` rejections, which reads as a `tf_tree` defect at 3 a.m.
/// `TFT019` is the attribution, and until `doctor` could read a recording it had
/// no source to attribute on — the amendment at `docs/PHASE5.md` §6 called that
/// out as a limitation rather than a caveat.
///
/// The step is 90 ms: under the 100 ms `--clock-reset-threshold`, so ingest
/// keeps the samples and counts them as `out_of_order` instead of halting, which
/// is §3.2's stated rule and is what leaves the evidence in the file. At a 10 ms
/// period that rejects nine consecutive arrivals, above the eight
/// `checks::CLOCK_STEP_MIN_REJECTED_RUN` requires.
///
/// Mutant: raise `CLOCK_STEP_MIN_REJECTED_RUN` from 8 to 12. Applied, and this
/// failed with `left: "pass", right: "fired"` — the nine-arrival burst is no
/// longer concentrated enough, so `TFT019` reports it as a stray inversion. That
/// is the threshold this fixture is sized against, and it is why the burst is
/// nine and not one.
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
    // The clock steps back 90 ms — from `base + 90 ms`, the newest stamp so far,
    // to `base` — and the publisher carries on at the same rate. Every arrival
    // on the way back up is older than the newest accepted one, so the run of
    // rejections is nine long and the largest single jump is 90 ms, under the
    // 100 ms threshold that would have halted the ingest instead.
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

/// **The §3.2 ingest report goes to stderr, so `--json` stdout stays a
/// document.**
///
/// Both halves matter and they pull against each other. A stranger's first run
/// has to be told what reading their file found — dropped zero stamps, a
/// truncated recording, skipped chunks — or the catalogue's clean bill of health
/// is a report about a fraction of their data. A CI job piping `--json` into a
/// parser has to get JSON and nothing else. Two streams is what lets both be
/// true.
///
/// Mutant: change `doctor_source`'s `eprint!` to `print!`. Applied, and this
/// failed on `stdout must be a JSON document and nothing else`: the summary's
/// first line lands ahead of the opening brace.
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

/// **A file that is not a recording is diagnosed, not reported as a healthy
/// tree.**
///
/// The failure this forbids is a `doctor` that falls back to the built-in
/// fixture when `--from-bag` cannot be read: it would print a clean report about
/// a synthetic robot while the operator believed it was about their bag. The
/// error also has to name the file, because "not a well-formed MCAP recording"
/// with no path is unactionable when a script passes the wrong argument.
///
/// Mutant: make `doctor_source` fall through to `source(live)` when
/// `open_bag` returns an error (`if let Ok(ingested) = ...`). Applied, and this
/// failed on the exit status: the run succeeds and prints the fixture's report.
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
