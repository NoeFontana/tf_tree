//! **`frozen_workers`'s exit status, which is the difference between PHASE5
//! §12 gate 4 being a gate and being a report.**
//!
//! Until `--gate` existed, every path out of the driver was `Ok(())`: the
//! verdict was a string in a `println!`, so criterion 4 could regress from
//! 1.024x to anything at all and `nightly.yml`'s `gate4` job — whose only step
//! is `just gate4` — would still be green. That is the shape
//! `docs/benchmarks/EVIDENCE.md` was created to prevent, reappearing in a job
//! written to close it.
//!
//! `frozen_workers.rs`'s own unit tests drive `gate_4_holds`, which is the
//! arithmetic. This file drives the **process**, which is the half the job
//! actually reads, and it drives it in both directions:
//!
//! * the same failing measurement exits non-zero **with** `--gate` and zero
//!   **without** it, which is `just gate4` and `just gate4-python` respectively
//!   — §12 gate 4's amendment is NORMATIVE that the Python arm "exits 0 on the
//!   FAIL it prints", so a change that made the FAIL global would break a
//!   deliberate distinction rather than tighten a loose one;
//! * the two `--gate` combinations the binary refuses are each refused,
//!   separately, because a single seeded violation caught by one arm says
//!   nothing about the other.
//!
//! **The failing measurement is real, not injected.** A 2-robot fixture is
//! ~2 MiB, so `S` is a fraction of the 74·p the criterion needs and the ratio
//! collapses towards `16p/p` for the reason the binary's header gives. No
//! threshold is edited and no environment variable is read: this is the gate's
//! own arithmetic on a fixture chosen to be outside it, which is also what
//! makes it cheap enough to sit in `just shm-check` (~0.1 s of measurement on
//! top of one small freeze).
//!
//! Requires `--features shm` (Linux: the workers read
//! `/proc/self/smaps_rollup`). Run: `just shm-check`.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::{Command, Output};

/// A scratch `.tft` path unique to the calling test.
///
/// Each test gets its own file: the driver reuses an existing `--tft` rather
/// than re-freezing (that reuse is what `just gate4`'s `rm -f` line exists to
/// defeat), and nextest runs these in parallel processes, so a shared name
/// would have one test measuring another's fixture.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tf_tree_gate4_{}_{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("creating the scratch directory");
    dir.join("workers.tft")
}

/// Run the shipped driver — `CARGO_BIN_EXE_frozen_workers` is the binary the
/// recipes run, not a re-implementation of it.
fn drive(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_frozen_workers"))
        .args(args)
        .output()
        .expect("spawning frozen_workers")
}

/// A fixture far too small for `S >= 74p`, so the run genuinely fails
/// criterion 4.
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

/// **`just gate4`'s shape: a FAIL is a non-zero exit.**
///
/// This is the assertion `nightly.yml`'s `gate4` job rests on, and the one
/// that did not hold.
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

/// **`just gate4-python`'s shape, and it is deliberate rather than an
/// oversight.**
///
/// PHASE5 §12 gate 4's amendment: "`gate4-python` exits 0 on the FAIL it
/// prints", because `S >= 74p` makes the verdict a function of the worker's
/// private cost and giving criterion 4 a second *gated* arm is a decision that
/// needs a record. The measurement here is the same one the test above fails
/// on, so this asserts the distinction is carried by the flag and by nothing
/// else.
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

/// The second gated arm §12 gate 4's amendment defers is refused at argument
/// parse, so it cannot arrive as a flag pair in a recipe.
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

/// The `--no-touch` control is documented to FAIL at 5.32x, so gating on it
/// asserts a number nobody claims. It stays runnable — the control is evidence
/// that a passing run is not passing vacuously — and it is refused only when
/// somebody asks for its verdict to gate.
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

    // And the control itself still runs, which is the half that must not
    // regress: refusing to *gate* on it is not refusing to run it.
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

/// **A run that cannot evaluate the criterion refuses instead of exiting 0** —
/// the missing-`N = 16` half.
///
/// `just gate4` pins `--workers 1,16`, so reaching this state at all means the
/// criterion was not evaluated — and a job that goes green on that is the
/// vacuity `--gate` exists to remove.
///
/// **The refusal is asserted by the row it names, not only by the exit code.**
/// The driver has two of these arms, one per missing row, and an assertion on
/// the status alone would be satisfied by either — so a dead arm would hide
/// behind its live sibling, which is the defect this file exists to prevent one
/// level up.
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

/// The same refusal, missing the **other** row.
///
/// This arm was live and had no automated witness: `--workers 16` alone reaches
/// `no N = 1 row`, which is a different `bail!` from the one above and could
/// have been deleted without any test noticing. The gate is stated against one
/// worker (`total(16)/total(1)`), so this is the row the criterion is divided
/// by — an unevaluable run here is the one most likely to look like a small
/// configuration mistake in a recipe.
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

    // And without `--gate` it is a report, on this arm too.
    let mut report = args.clone();
    report.pop();
    let out = drive(&as_args(&report));
    assert!(out.status.success(), "without --gate it is a report");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cannot be evaluated"),
        "and it still says the criterion was not evaluated"
    );
}
