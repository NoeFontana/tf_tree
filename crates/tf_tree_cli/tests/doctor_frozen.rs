//! `tf_tree doctor --from-file` against a frozen `.tft` — `docs/PHASE5.md` §2 + §6.
//!
//! `--from-file` is the source `docs/PHASE5.md` §6's `TFT019` amendment named,
//! and it needs the frozen backend, so `cargo nextest run --workspace` — which
//! builds without features — compiles this target out. `just shm-check` runs it.
//!
//! # The interesting assertion here is a *skip*
//!
//! §6's amendment expected a `.tft` source to unblock `TFT018` and `TFT019`.
//! It does not, and this file is where that is pinned rather than left as prose:
//! a `.tft` is an arena, `SampleRing::push` rejects an out-of-order stamp, and a
//! ring therefore holds only accepted pushes. Running the two checks here would
//! pass **every** `.tft` ever written, which is the fabricated all-clear the
//! catalogue refuses everywhere else. The recording source (`--from-bag`) is the
//! one that carries the evidence; `crates/tf_tree_cli/tests/doctor_recording.rs`
//! is where it fires.
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

/// **A frozen `.tft` is a `doctor` source, and the checks that read the arena
/// run on it unchanged.**
///
/// §2.1 is NORMATIVE that a frozen arena is read by the identical code as a live
/// one, so this is the smallest thing that shows the wiring is wiring: freeze a
/// recording, point `doctor` at the file, and get a report about the same
/// topology with no attach, no rendezvous and no publisher.
///
/// **And `TFT018`/`TFT019` skip, with the reason that is true of an arena** —
/// not the live-arena one about a torn ring, which does not apply to a file
/// nobody is writing. That distinction is the whole reason `checks::PushStream`
/// has four variants instead of being a `live: bool`; keyed on liveness, both
/// checks would have run here and passed unconditionally.
///
/// Mutant: give `Source::Frozen` the stream `PushStream::Observed` in
/// `lib.rs`'s `Source::stream`. Applied, and this failed with
/// `TFT018 must not claim a verdict it cannot have: ... "status": "pass"` —
/// which is precisely the vacuous pass a `.tft` source would have shipped.
#[test]
fn a_frozen_index_is_a_doctor_source_and_the_two_stream_checks_skip_on_it() {
    let dir = Scratch::new("wiring");
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

    let out = tf_tree()
        .arg("doctor")
        .arg("--from-file")
        .arg(&tft)
        .arg("--json")
        .output()
        .unwrap();
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{json}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        json.contains("\"source\": \"frozen .tft index\""),
        "the report must name what it read:\n{json}"
    );
    // The topology `small_recording` describes, read back out of the file.
    assert!(
        json.contains("\"frames\": 6") && json.contains("\"edges\": 5"),
        "the arena's own shape must survive the round trip:\n{json}"
    );

    for id in ["TFT018", "TFT019"] {
        let at = json
            .find(&format!("\"id\": \"{id}\""))
            .unwrap_or_else(|| panic!("{id} missing:\n{json}"));
        let window = &json[at..(at + 1200).min(json.len())];
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

/// **A recording handed to `--from-file` is told which flag it wanted.**
///
/// `.mcap` and `.tft` are one flag apart and the error a user gets from the
/// container's magic check is `BadMagic`, which is accurate and unhelpful. The
/// remedy costs a sentence and is the difference between a stranger's first run
/// succeeding on the second attempt and them concluding the tool is broken.
///
/// Mutant: drop the `--from-bag` sentence from `doctor_source`'s frozen error
/// context. Applied: the remedy assertion fails with only `BadMagic` printed.
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
