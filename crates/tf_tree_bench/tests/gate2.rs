//! `frozen_open`'s exit status and refusals — what makes PHASE5 §12 gate 2 a
//! gate. Each alternative is driven separately: the verdict is a conjunction of
//! two checks.
//!
//! | invocation | what it proves |
//! |---|---|
//! | `--gate --prefault` | size-proportional open work turns the gate red, no threshold edited |
//! | `--prefault` alone | exits 0 without `--gate` |
//! | `--gate --prefault --budget-ms 1000` | the scale check alone can fail |
//! | `--gate --budget-ms <tiny>` | the budget check alone can fail |
//! | `--gate --no-evict` | a gated run REFUSES when eviction did not take (ungated voids the arm) |
//! | a fixture under 233 MB, with `--gate` | the gate-scale floor REFUSES |
//! | two fixtures under 8x apart, with `--gate` | the span floor REFUSES |
//! | `--gate --budget-ms 1000` on an unmodified open | REFUSED: no gated PASS against a budget above the criterion's |
//!
//! All cases but the floor test share one fixture and one test: the gate-scale
//! freeze costs ~1.4 s and ~338 MiB, and nextest runs each `#[test]` in its own
//! process. `tests/gate4.rs` splits its cases because its fixture is 2 MiB.
//!
//! Requires `--features shm`. Run: `just shm-check`.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Output;

/// A scratch directory per test, under the cargo target directory (not
/// `$TMPDIR`, which is often tmpfs and cannot evict pages) — where `just gate2`
/// writes.
fn scratch(name: &str) -> PathBuf {
    // `CARGO_BIN_EXE_*` is `<target>/<profile>/frozen_open`.
    let dir = PathBuf::from(env!("CARGO_BIN_EXE_frozen_open"))
        .parent()
        .expect("the driver binary has a parent directory")
        .join(format!("gate2_scratch_{}_{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("creating the scratch directory");
    dir
}

/// Run the shipped `frozen_open` binary.
fn drive(args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_frozen_open"))
        .args(args)
        .output()
        .expect("spawning frozen_open")
}

/// A gate-scale index and a small one, in `dir`; `--robots 48` clears the 233 MB
/// floor.
fn fixtures(dir: &std::path::Path) -> Vec<String> {
    vec![
        "--tft".into(),
        dir.join("index.tft").display().to_string(),
        "--small-tft".into(),
        dir.join("small.tft").display().to_string(),
        "--robots".into(),
        "48".into(),
        "--history".into(),
        "40".into(),
        "--rounds".into(),
        "2".into(),
    ]
}

fn as_args(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn with(base: &[String], extra: &[&str]) -> Vec<String> {
    let mut v = base.to_vec();
    v.extend(extra.iter().map(|s| (*s).to_owned()));
    v
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_verdict_can_go_red_and_each_half_can_do_it_alone() {
    let dir = scratch("red");
    let base = fixtures(&dir);

    // The fixture is at the criterion's scale, so the PASS below means something.
    let clean = drive(&as_args(&with(&base, &["--gate"])));
    let out = stdout_of(&clean);
    assert!(
        clean.status.success(),
        "the unmodified gate must pass on a gate-scale fixture, else every FAIL below is \
         ambiguous; got:\n{out}{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(
        !out.contains("NOT AT GATE SCALE"),
        "the fixture must clear the 233 MB floor; got:\n{out}"
    );

    // The falsifier: reading the whole index inside the timed region fails the
    // gate's arithmetic with no threshold edited.
    let both = drive(&as_args(&with(&base, &["--gate", "--prefault"])));
    let out = stdout_of(&both);
    assert!(
        !both.status.success(),
        "--gate --prefault must exit non-zero: it is the only falsifier that moves no \
         threshold; got:\n{out}"
    );
    assert!(out.contains("§12 gate 2 — FAIL"), "got:\n{out}");

    // Ungated, the same measurement exits 0: `--gate` is the caller's statement.
    let ungated = drive(&as_args(&with(&base, &["--prefault"])));
    let out = stdout_of(&ungated);
    assert!(
        ungated.status.success(),
        "--prefault without --gate must exit 0 while printing FAIL; got:\n{out}"
    );
    assert!(out.contains("§12 gate 2 — FAIL"), "got:\n{out}");

    // The scale check alone, with the budget out of the way.
    let scale_only = drive(&as_args(&with(
        &base,
        &["--gate", "--prefault", "--budget-ms", "1000"],
    )));
    let out = stdout_of(&scale_only);
    assert!(
        out.contains("budget:")
            && out
                .lines()
                .any(|l| l.contains("budget:") && l.ends_with("PASS")),
        "the 1000 ms budget must pass, else this case does not isolate the scale check; \
         got:\n{out}"
    );
    assert!(
        out.lines()
            .any(|l| l.contains("scale invariance") && l.ends_with("FAIL")),
        "got:\n{out}"
    );
    assert!(!scale_only.status.success(), "got:\n{out}");

    // The budget check alone.
    let budget_only = drive(&as_args(&with(&base, &["--gate", "--budget-ms", "0.0001"])));
    let out = stdout_of(&budget_only);
    assert!(
        out.lines()
            .any(|l| l.contains("scale invariance") && l.ends_with("PASS")),
        "scale invariance must pass, else this case does not isolate the budget; got:\n{out}"
    );
    assert!(
        out.lines()
            .any(|l| l.contains("budget:") && l.ends_with("FAIL")),
        "got:\n{out}"
    );
    assert!(!budget_only.status.success(), "got:\n{out}");

    // A gated run whose eviction did not take must refuse, not report.
    let unevicted = drive(&as_args(&with(&base, &["--gate", "--no-evict"])));
    let err = String::from_utf8_lossy(&unevicted.stderr).into_owned();
    assert!(
        !unevicted.status.success(),
        "--no-evict must refuse; got:\n{}{err}",
        stdout_of(&unevicted)
    );
    assert!(
        err.contains("REFUSED — the evicted arm did not evict"),
        "the refusal must say which premise failed; got:\n{err}"
    );
    assert!(
        !stdout_of(&unevicted).contains("gate 2 — PASS"),
        "a refused run must publish no verdict; got:\n{}",
        stdout_of(&unevicted)
    );

    // The span floor: the same fixture on both sides compares an open with itself.
    let large = dir.join("index.tft").display().to_string();
    let narrow = drive(&as_args(&with(
        &base,
        &["--gate", "--small-tft", large.as_str()],
    )));
    let err = String::from_utf8_lossy(&narrow.stderr).into_owned();
    assert!(
        !narrow.status.success(),
        "two fixtures of the same size must refuse under --gate; got:\n{}{err}",
        stdout_of(&narrow)
    );
    assert!(
        err.contains("apart"),
        "the refusal must name the span it is refusing on; got:\n{err}"
    );
    assert!(
        !stdout_of(&narrow).contains("gate 2 — PASS"),
        "a refused run must publish no verdict; got:\n{}",
        stdout_of(&narrow)
    );

    // A loosened `--budget-ms` may not produce a gated PASS; the `--prefault`
    // case above passes the same budget because that run still FAILS.
    let loosened = drive(&as_args(&with(&base, &["--gate", "--budget-ms", "1000"])));
    let err = String::from_utf8_lossy(&loosened.stderr).into_owned();
    assert!(
        !loosened.status.success(),
        "a gated PASS against a budget above the criterion's own must refuse; got:\n{}{err}",
        stdout_of(&loosened)
    );
    assert!(
        err.contains("REFUSED — --budget-ms"),
        "the refusal must name the flag that caused it; got:\n{err}"
    );
    assert!(
        !stdout_of(&loosened).contains("GATED"),
        "a refusal must print neither the gated comparisons nor a verdict; got:\n{}",
        stdout_of(&loosened)
    );
    // Ungated, the loosened budget still reports PASS at exit 0.
    let reported = drive(&as_args(&with(&base, &["--budget-ms", "1000"])));
    let out = stdout_of(&reported);
    assert!(
        reported.status.success() && out.contains("§12 gate 2 — PASS (reported)"),
        "got:\n{out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A fixture below §12 gate 2's 233 MB scale is refused, not passed. The pair is
/// 1.05x apart, so ungated it also trips the span disclosure.
#[test]
fn a_fixture_below_the_criterions_own_scale_is_refused_rather_than_passed() {
    let dir = scratch("floor");
    let args = vec![
        "--tft".to_owned(),
        dir.join("tiny.tft").display().to_string(),
        "--small-tft".to_owned(),
        dir.join("small.tft").display().to_string(),
        "--robots".to_owned(),
        "4".to_owned(),
        "--history".to_owned(),
        "0.5".to_owned(),
        "--small-robots".to_owned(),
        "2".to_owned(),
        "--small-history".to_owned(),
        "0.5".to_owned(),
        "--rounds".to_owned(),
        "1".to_owned(),
    ];

    let gated = drive(&as_args(&with(&args, &["--gate"])));
    let err = String::from_utf8_lossy(&gated.stderr).into_owned();
    assert!(!gated.status.success(), "got:\n{}{err}", stdout_of(&gated));
    assert!(
        err.contains("233 MB index"),
        "the refusal must name the criterion's own scale; got:\n{err}"
    );

    // Ungated, the same fixture reports and says it is not at gate scale.
    let reported = drive(&as_args(&args));
    let out = stdout_of(&reported);
    assert!(reported.status.success(), "got:\n{out}");
    assert!(out.contains("NOT AT GATE SCALE"), "got:\n{out}");
    // Both ungated disclosures fire.
    assert!(
        out.contains("SPAN TOO NARROW"),
        "an ungated run over two fixtures under 8x apart must say the scale line cannot \
         fail; got:\n{out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
