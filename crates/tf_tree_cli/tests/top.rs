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

/// CPU time the process has used so far, from `/proc/<pid>/schedstat` (ns
/// resolution; `/proc/<pid>/stat` ticks at 10 ms, too coarse for 0.5%).
fn cpu_ns(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/schedstat")).ok()?;
    s.split_whitespace().next()?.parse().ok()
}

/// `top` at 10 Hz costs under 0.5% of a core once running (`--interval 100`).
/// Startup is excluded: the window opens after the first frames. Measured 0.30%
/// at `--release`; a debug build reads ~0.8%, so it runs under `just top-cpu`.
#[test]
#[cfg_attr(debug_assertions, ignore = "debug build; `just top-cpu`")]
fn ten_hertz_costs_under_half_a_percent_of_a_core() {
    use std::time::{Duration, Instant};
    let mut child = Command::new(env!("CARGO_BIN_EXE_tf_tree"))
        .args(["top", "--iterations", "60", "--interval", "100"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn tf_tree top");
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(1000));
    let c0 = cpu_ns(pid);
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(4000));
    let c1 = cpu_ns(pid);
    let t1 = Instant::now();
    child.kill().ok();
    child.wait().ok();
    // No schedstat on this host: nothing to measure.
    let (Some(c0), Some(c1)) = (c0, c1) else {
        return;
    };
    let frac = (c1 - c0) as f64 / t1.duration_since(t0).as_nanos() as f64;
    assert!(
        frac < 0.005,
        "top at 10 Hz used {:.2}% of a core",
        frac * 100.0
    );
}
