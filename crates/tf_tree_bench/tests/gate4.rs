//! `frozen_workers`'s exit status — what makes PHASE5 §12 gate 4 a gate.
//!
//! The same failing measurement exits non-zero with `--gate` (`just gate4`) and
//! zero without (`just gate4-python`, per §12 gate 4's amendment); the two
//! refused `--gate` combinations are each driven. The failure is real: a 2-robot
//! fixture (~2 MiB) is far below `S >= 74p`, so no threshold is edited.
//!
//! Requires `--features shm` (Linux). Run: `just shm-check`.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::{Command, Output};

/// A scratch `.tft` path per test; the driver reuses an existing `--tft`.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tf_tree_gate4_{}_{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("creating the scratch directory");
    dir.join("workers.tft")
}

/// Run the shipped `frozen_workers` binary.
fn drive(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_frozen_workers"))
        .args(args)
        .output()
        .expect("spawning frozen_workers")
}

/// A fixture far too small for `S >= 74p`.
fn tiny(tft: &std::path::Path) -> Vec<String> {
    vec![
        "--tft".into(),
        tft.display().to_string(),
        "--robots".into(),
        "2".into(),
        "--history".into(),
        "0.5".into(),
        "--workers".into(),
        "1,16".into(),
        "--stamps".into(),
        "8".into(),
    ]
}

fn as_args(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

/// `just gate4`: a FAIL under `--gate` is a non-zero exit.
#[test]
fn a_failing_ratio_exits_non_zero_under_gate() {
    let tft = scratch("gated");
    let mut args = tiny(&tft);
    args.push("--gate".into());
    let out = drive(&as_args(&args));

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("— FAIL"),
        "the fixture must be small enough to fail criterion 4, else this test \
         proves nothing about the exit status; got:\n{stdout}"
    );
    assert!(
        !out.status.success(),
        "a FAIL under --gate must fail the process, or `just gate4` is a report \
         wearing a job's name; status {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `just gate4-python`: the same FAIL exits 0 without `--gate`, per §12 gate 4's
/// amendment.
#[test]
fn the_same_failing_ratio_exits_zero_without_gate() {
    let tft = scratch("ungated");
    let args = tiny(&tft);
    let out = drive(&as_args(&args));

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("— FAIL"),
        "the same fixture must still print FAIL; got:\n{stdout}"
    );
    assert!(
        out.status.success(),
        "without --gate the driver reports and does not gate; status {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The second gated arm the amendment defers is refused at argument parse.
#[test]
fn gating_the_python_arm_is_refused() {
    let tft = scratch("gated_python");
    let mut args = tiny(&tft);
    args.push("--gate".into());
    args.push("--python".into());
    args.push("/usr/bin/python3".into());
    let out = drive(&as_args(&args));

    assert!(!out.status.success(), "--gate --python must refuse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("decision record"),
        "the refusal must say why, naming the decision it is not entitled to \
         make; got:\n{stderr}"
    );
}

/// Gating on the `--no-touch` control (documented FAIL at 5.32x) is refused; the
/// control itself stays runnable.
#[test]
fn gating_the_no_touch_control_is_refused() {
    let tft = scratch("gated_notouch");
    let mut args = tiny(&tft);
    args.push("--gate".into());
    args.push("--no-touch".into());
    let out = drive(&as_args(&args));

    assert!(!out.status.success(), "--gate --no-touch must refuse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("5.32x"),
        "the refusal must cite the control's documented reading; got:\n{stderr}"
    );

    // Refusing to gate on it is not refusing to run it.
    let plain = scratch("plain_notouch");
    let mut ok = tiny(&plain);
    ok.push("--no-touch".into());
    let out = drive(&as_args(&ok));
    assert!(
        out.status.success(),
        "the --no-touch control must still be runnable; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The same tiny fixture, driven at one worker count rather than two.
fn one_row(tft: &std::path::Path, workers: &str) -> Vec<String> {
    vec![
        "--tft".into(),
        tft.display().to_string(),
        "--robots".into(),
        "2".into(),
        "--history".into(),
        "0.5".into(),
        "--workers".into(),
        workers.into(),
        "--stamps".into(),
        "8".into(),
    ]
}

/// A run with no `N = 16` row refuses under `--gate`; the test asserts the row
/// named, since the sibling arm's refusal would also give a non-zero status.
#[test]
fn a_run_with_no_n_16_row_refuses_under_gate() {
    let tft = scratch("unevaluable_16");
    let mut args = one_row(&tft, "1");
    args.push("--gate".into());
    let out = drive(&as_args(&args));
    assert!(
        !out.status.success(),
        "--gate with no N = 16 row must refuse; stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no N = 16 row"),
        "the refusal must name the row that was missing, so this test cannot \
         pass on the other arm's refusal; got:\n{stderr}"
    );

    // Without --gate the same run is a report and says so on stdout.
    let mut report = args.clone();
    report.pop();
    let out = drive(&as_args(&report));
    assert!(out.status.success(), "without --gate it is a report");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cannot be evaluated"),
        "and it still says the criterion was not evaluated"
    );
}

/// The same refusal for the missing `N = 1` row, a separate `bail!`.
#[test]
fn a_run_with_no_n_1_row_refuses_under_gate() {
    let tft = scratch("unevaluable_1");
    let mut args = one_row(&tft, "16");
    args.push("--gate".into());
    let out = drive(&as_args(&args));
    assert!(
        !out.status.success(),
        "--gate with no N = 1 row must refuse; stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no N = 1 row"),
        "the refusal must name the row that was missing; got:\n{stderr}"
    );

    // Without `--gate` it is a report on this arm too.
    let mut report = args.clone();
    report.pop();
    let out = drive(&as_args(&report));
    assert!(out.status.success(), "without --gate it is a report");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cannot be evaluated"),
        "and it still says the criterion was not evaluated"
    );
}
