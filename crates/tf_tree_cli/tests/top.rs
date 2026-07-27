//! `tf_tree top` end to end, through `clap` and the shipped binary
//! (`docs/PHASE5.md` §7).
//!
//! The unit tests in `top.rs` cover the model — rates, histogram, feed — with
//! hand-built captures. What they cannot cover is the wiring: whether the
//! iteration bound is off by one, whether a pipe gets escape sequences, and
//! whether the interval floor is enforced at all. Those are properties of the
//! process, so this runs the process.
//!
//! **Deliberately not `--attach`**: this file runs in the default build, which
//! has no `shm`, so it is in `cargo nextest run --workspace` — the gate that
//! runs on every commit. The live-arena half is in `tests/attach.rs`, which
//! `just shm-check` runs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;

fn cli(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tf_tree"))
        .args(args)
        .output()
        .expect("run tf_tree");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// **`--iterations n` draws exactly `n` frames, and a pipe gets no escapes.**
///
/// Both halves are about the same thing: `top` piped into a file is how an
/// operator attaches a live arena to a bug report, and a file full of `ESC[H`
/// with an unpredictable number of frames in it is not that.
///
/// Mutant: change the loop bound in `top::run` from `n >= iterations` to
/// `n > iterations`. Applied: three banners instead of two, and the
/// `assert_eq!(frames, 2)` fails.
///
/// Second mutant: change `colour.unwrap_or(tty)` to `colour.unwrap_or(true)`.
/// Applied: the captured stdout contains `\x1b[` and the escape assertion
/// fails.
#[test]
fn iterations_are_exact_and_a_pipe_gets_plain_text() {
    let (ok, out, err) = cli(&["top", "--iterations", "2", "--interval", "50"]);
    assert!(ok, "top failed:\n{err}");
    let frames = out.matches("tf_tree top").count();
    assert_eq!(frames, 2, "expected two frames:\n{out}");
    assert!(!out.contains('\x1b'), "escape sequences in piped output");
    assert!(out.contains("read-only observer"), "{out}");
}

/// **`--color always` overrides the tty detection, `never` is the default's
/// answer for a pipe.**
///
/// The override exists because a pipe is not always a file: `tf_tree top |
/// less -R` is a terminal at the far end, and auto-detection cannot know that.
///
/// Mutant: make `ColorChoice::forced` return `None` for `Always`. Applied: the
/// piped output has no escape sequence and the first assertion fails.
#[test]
fn colour_can_be_forced_through_a_pipe() {
    let (ok, out, err) = cli(&["top", "--iterations", "1", "--color", "always"]);
    assert!(ok, "{err}");
    assert!(out.contains('\x1b'), "--color always emitted no colour");

    let (ok, out, err) = cli(&["top", "--iterations", "1", "--color", "never"]);
    assert!(ok, "{err}");
    assert!(!out.contains('\x1b'), "--color never emitted colour");
}

/// **A sub-50 ms interval is refused, not clamped.**
///
/// The one way this tool can perturb what it observes is by spinning on the
/// arena's cache lines, so the floor is a rule rather than a preference, and a
/// silent clamp would leave the operator believing they had asked for something
/// and got it.
///
/// Mutant: delete the `anyhow::ensure!(interval_ms >= 50, ...)` in `cmd_top`.
/// Applied: the command succeeds, `ok` is true, and the assertion fails.
#[test]
fn a_too_fast_interval_is_refused() {
    let (ok, _out, err) = cli(&["top", "--iterations", "1", "--interval", "10"]);
    assert!(!ok, "a 10 ms interval was accepted");
    assert!(err.contains("50 ms floor"), "unhelpful refusal: {err}");
}

/// **The detail pane reports the ring's real inter-arrival distribution.**
///
/// The fixture publishes `odom->base_link` at 200 Hz into a 2048-slot ring, so
/// the histogram must show a 5 ms period over a non-trivial number of samples —
/// a fixture whose ring held one sample would make every distribution assertion
/// vacuously true.
///
/// Mutant: in `Capture::from_tree`, iterate `(head - retained)..head` as
/// `head..head` (i.e. collect no stamps). Applied: the pane prints "fewer than
/// two retained samples" and both assertions fail.
#[test]
fn the_detail_pane_shows_a_real_distribution() {
    let (ok, out, err) = cli(&[
        "top",
        "--iterations",
        "1",
        "--interval",
        "50",
        "--edge",
        "odom->base_link",
    ]);
    assert!(ok, "top --edge failed:\n{err}");
    let detail = out
        .split("edge detail")
        .nth(1)
        .expect("no detail pane:\n{out}");
    assert!(
        detail.contains("median 5.0 ms"),
        "not the fixture's 200 Hz edge:\n{detail}"
    );
    // `n` is one fewer than the retained samples; anything in the thousands
    // proves the ring was actually walked.
    assert!(
        detail.contains("inter-arrival: n 1999"),
        "the ring was not fully walked:\n{detail}"
    );
    assert!(detail.contains('#'), "no histogram bars:\n{detail}");
}

/// **An unmatched `--edge` says so instead of silently showing nothing.**
///
/// Mutant: make `select_edge` fall back to `edges.first()`. Applied: the pane
/// renders edge#1 and the "no edge matches" assertion fails — which is the bug
/// of an operator reading one edge's histogram while believing it is another's.
#[test]
fn an_unmatched_edge_needle_is_reported() {
    let (ok, out, err) = cli(&[
        "top",
        "--iterations",
        "1",
        "--interval",
        "50",
        "--edge",
        "no_such_frame",
    ]);
    assert!(ok, "top failed:\n{err}");
    assert!(out.contains("no edge matches"), "{out}");
}
