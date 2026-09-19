//! `tf_tree top` end to end through the shipped binary (`docs/PHASE5.md` §7).
//!
//! Deliberately not `--attach`: this runs in the default build (no `shm`); the
//! live-arena half is `tests/attach.rs`.
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

/// `--iterations n` draws exactly `n` frames, and a pipe gets no escapes.
#[test]
fn iterations_are_exact_and_a_pipe_gets_plain_text() {
    let (ok, out, err) = cli(&["top", "--iterations", "2", "--interval", "50"]);
    assert!(ok, "top failed:\n{err}");
    let frames = out.matches("tf_tree top").count();
    assert_eq!(frames, 2, "expected two frames:\n{out}");
    assert!(!out.contains('\x1b'), "escape sequences in piped output");
    assert!(out.contains("read-only observer"), "{out}");
}

/// `--color always` overrides tty detection; a pipe defaults to `never`.
#[test]
fn colour_can_be_forced_through_a_pipe() {
    let (ok, out, err) = cli(&["top", "--iterations", "1", "--color", "always"]);
    assert!(ok, "{err}");
    assert!(out.contains('\x1b'), "--color always emitted no colour");

    let (ok, out, err) = cli(&["top", "--iterations", "1", "--color", "never"]);
    assert!(ok, "{err}");
    assert!(!out.contains('\x1b'), "--color never emitted colour");
}

/// A sub-50 ms interval is refused, not clamped.
#[test]
fn a_too_fast_interval_is_refused() {
    let (ok, _out, err) = cli(&["top", "--iterations", "1", "--interval", "10"]);
    assert!(!ok, "a 10 ms interval was accepted");
    assert!(err.contains("50 ms floor"), "unhelpful refusal: {err}");
}

/// The detail pane reports the ring's real inter-arrival distribution
/// (200 Hz into a 2048-slot ring: 5 ms period over many samples).
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
    // Thousands of samples prove the ring was walked.
    assert!(
        detail.contains("inter-arrival: n 1999"),
        "the ring was not fully walked:\n{detail}"
    );
    assert!(detail.contains('#'), "no histogram bars:\n{detail}");
}

/// An unmatched `--edge` says so instead of showing nothing.
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
