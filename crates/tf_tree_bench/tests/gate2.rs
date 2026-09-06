//! **`frozen_open`'s exit status and its refusals, which are the difference
//! between PHASE5 §12 gate 2 being a gate and being a report.**
//!
//! `frozen_open.rs`'s own unit tests drive the two comparisons, which is the
//! arithmetic. This file drives the **process**, which is the half a workflow
//! reads, and it drives every alternative separately — because a union of two
//! checks is exactly where one of them stops being read, and this gate's
//! verdict is a conjunction of two.
//!
//! | invocation | what it proves |
//! |---|---|
//! | `--gate --prefault` | the real falsifier: an open that does size-proportional work turns the gate red, and **no threshold is edited** |
//! | `--prefault` alone | the same measurement exits 0 without `--gate` — the caller says whether a run is a gate |
//! | `--gate --prefault --budget-ms 1000` | the **scale** check alone can fail: the budget passes and the verdict is still red |
//! | `--gate --budget-ms <tiny>` | the **budget** check alone can fail: scale invariance passes and the verdict is still red |
//! | `--gate --no-evict` | a **gated** run REFUSES when its eviction did not take, rather than publishing a resident number as an evicted one (an ungated one voids the arm and says so — the evicted arm gates nothing, and a RAM-backed filesystem is an environment rather than a defect) |
//! | a fixture under 233 MB, with `--gate` | the gate-scale floor REFUSES, rather than passing 10 ms trivially |
//! | two fixtures under 8x apart, with `--gate` | the **span** floor REFUSES: the scale-invariance arm would be comparing an open with itself |
//! | `--gate --budget-ms 1000` on an unmodified open | REFUSED: a gated PASS against a budget **above** the criterion's own is a verdict about the argument. The same flag on a run that still FAILS — the scale-only case above — is untouched |
//!
//! **Why every case above but the last shares one fixture and one test.** The freeze of a
//! gate-scale `.tft` is ~1.4 s and ~338 MiB of disk; the measurements
//! themselves are milliseconds. nextest runs each `#[test]` in its own process,
//! so a test apiece would be a freeze apiece. They are separate assertions with
//! separate messages against one fixture, driven in sequence — including the
//! two floors, which `bail!` before `measure` runs and so touch no file, no page
//! cache and no ordering. `tests/gate4.rs` splits its cases because its fixture
//! is 2 MiB.
//!
//! # The mutants, seeded and observed
//!
//! Each was applied to `src/bin/frozen_open.rs`, run, and reverted — a check
//! nobody has watched fail is a check nobody has tested:
//!
//! * `if false && d.gate && !verdict` around the exit — caught by the
//!   `--gate --prefault` assertion;
//! * `evicted_premise` forced to `true` — caught by `--no-evict`;
//! * `if false && d.gate && large_bytes < GATE_INDEX_FLOOR_BYTES` — caught by
//!   the floor test below;
//! * `scale_invariant` rewritten to `true` — caught by the
//!   `--prefault --budget-ms 1000` assertion, which is the case that exists so
//!   the budget cannot cover for it;
//! * `if false && d.gate && small_bytes.saturating_mul(SCALE_SPAN) > large_bytes`
//!   — caught by the same-fixture case, which until it was written was the one
//!   declared refusal in this binary that nothing had ever driven;
//! * `if false && d.gate && budget_ok && scale_ok && d.budget_ms > BUDGET_MS`
//!   — caught by the `--budget-ms 1000` case, which then observes the
//!   `PASS (gated)` at exit 0 the refusal exists to prevent.
//!
//! Requires `--features shm` (`Tree::open_frozen` is `shm`-gated, and the
//! evicted arm is Linux page cache). Run: `just shm-check`.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Output;

/// A scratch directory unique to the calling test, **on the filesystem
/// `just gate2` writes to** — the cargo target directory, reached from the
/// driver's own path rather than from `$TMPDIR`.
///
/// Each test gets its own: the driver reuses an existing `--tft` rather than
/// re-freezing (that reuse is what `just gate2`'s `rm -f` line exists to
/// defeat), and nextest runs these in parallel processes.
///
/// **Not `std::env::temp_dir()`, and the reason is the gate's own premise.**
/// The evicted arm needs a filesystem whose pages can be evicted, and `$TMPDIR`
/// is a tmpfs on a large share of hosts and containers, where nothing can be:
/// these tests went red for the environment and blamed `dd`. `just gate2`
/// writes under `target/`, so putting the fixtures there is what makes the
/// per-PR test and the recipe agree about what they are measuring. A host whose
/// *target directory* is RAM-backed now fails both together, which is one
/// disclosure instead of a contradiction.
fn scratch(name: &str) -> PathBuf {
    // `CARGO_BIN_EXE_*` is `<target>/<profile>/frozen_open`.
    let dir = PathBuf::from(env!("CARGO_BIN_EXE_frozen_open"))
        .parent()
        .expect("the driver binary has a parent directory")
        .join(format!("gate2_scratch_{}_{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("creating the scratch directory");
    dir
}

/// Run the shipped driver — `CARGO_BIN_EXE_frozen_open` is the binary the
/// recipe runs, not a re-implementation of it.
fn drive(args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_frozen_open"))
        .args(args)
        .output()
        .expect("spawning frozen_open")
}

/// The fixture arguments: a gate-scale index and a small one, in `dir`.
///
/// `--robots 48` rather than the driver's default 64: 254 MiB clears the 233 MB
/// floor with room, and keeps the freeze and the disk to what the floor
/// actually needs. Measured in the profile this test runs in (debug): 1.1 s for
/// the freeze, ~1.8 s for the whole first invocation.
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

    // Establishes the subject set is not empty: the fixture the rest of this
    // test measures really is at the criterion's scale, so a PASS below is a
    // pass of something.
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

    // **The falsifier, and it edits no threshold**: an open that reads the
    // whole index inside the timed region is the regression this gate exists
    // to catch, and it fails the gate's own arithmetic.
    let both = drive(&as_args(&with(&base, &["--gate", "--prefault"])));
    let out = stdout_of(&both);
    assert!(
        !both.status.success(),
        "--gate --prefault must exit non-zero: it is the only falsifier that moves no \
         threshold; got:\n{out}"
    );
    assert!(out.contains("§12 gate 2 — FAIL"), "got:\n{out}");

    // The same measurement, ungated, exits 0. `--gate` is the caller's
    // statement, not an inference from the arm — `frozen_workers`'s two recipes
    // rest on the same distinction.
    let ungated = drive(&as_args(&with(&base, &["--prefault"])));
    let out = stdout_of(&ungated);
    assert!(
        ungated.status.success(),
        "--prefault without --gate must exit 0 while printing FAIL; got:\n{out}"
    );
    assert!(out.contains("§12 gate 2 — FAIL"), "got:\n{out}");

    // **The scale check alone.** With the budget moved out of the way the
    // verdict is still red, so a scale check that had silently stopped
    // comparing would show here rather than hiding behind the budget.
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

    // **The budget check alone**, on an unmodified open: scale invariance holds
    // and the verdict is still red.
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

    // **The evicted arm's premise, under `--gate`.** A gated run whose eviction
    // did not take must refuse, not report — a "cold" number taken against a
    // resident page cache is off by more than an order of magnitude and looks
    // exactly like a fast open. Ungated, the same run voids the arm and prints
    // why, which is why this case passes `--gate`.
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

    // **The span floor, gate 2's second anti-vacuity refusal.** The
    // scale-invariance arm compares an open at two index sizes; driven with the
    // same fixture on both sides it compares an open with itself and cannot
    // fail. It needs no new fixture and no second freeze — the floor `bail!`s
    // before `measure` runs, so it touches no file and no page cache.
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

    // **A loosened `--budget-ms` may not produce a gated PASS.** `SCALE_BOUND`
    // is a constant and `--budget-ms` is not, so a raised budget is the one
    // argument a caller could use to green a run. Refused in that direction
    // only: the `--prefault --budget-ms 1000` case above passes the same
    // loosened budget and is untouched, because that run still FAILS.
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
    // The same loosened budget, ungated, still reports a PASS at exit 0 — what
    // the refusal closes is the gate, not the flag.
    let reported = drive(&as_args(&with(&base, &["--budget-ms", "1000"])));
    let out = stdout_of(&reported);
    assert!(
        reported.status.success() && out.contains("§12 gate 2 — PASS (reported)"),
        "got:\n{out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **The floor that stops a small fixture passing trivially.**
///
/// §12 gate 2 is stated over a 233 MB index; an `mmap` of a 2 MiB one fits
/// 10 ms for reasons that have nothing to do with this design. A gate whose
/// subject is too small to fail is green exactly when it checked nothing, so
/// the floor refuses rather than passing.
///
/// The same fixture pair is 1.05x apart, so ungated it also trips the **span**
/// disclosure — the two floors are independent and this pair is under both.
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

    // Ungated, the same fixture reports rather than refusing — and says so,
    // because a PASS printed against a 2 MiB file must not read as a pass of a
    // criterion about a 233 MB one.
    let reported = drive(&as_args(&args));
    let out = stdout_of(&reported);
    assert!(reported.status.success(), "got:\n{out}");
    assert!(out.contains("NOT AT GATE SCALE"), "got:\n{out}");
    // **Both ungated disclosures, and this fixture pair trips both.** 2.2 MiB
    // and 2.1 MiB are 1.05x apart, so the scale-invariance line is comparing an
    // open with itself; it printed a `GATED` prefix over that with nothing said
    // until the disclosure was added.
    assert!(
        out.contains("SPAN TOO NARROW"),
        "an ungated run over two fixtures under 8x apart must say the scale line cannot \
         fail; got:\n{out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
