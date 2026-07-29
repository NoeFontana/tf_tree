//! `shm_torture`'s own gate: the detector is tested, not trusted.
//!
//! A soak test is the easiest kind of test to get wrong, because the failure
//! mode looks identical to the success: a harness that never reads anything, or
//! that reads and never checks, prints "0 violations" and passes forever. This
//! file is what stops that. It runs the real binary twice — once with a child
//! that deliberately publishes a corrupt transform, once without — and asserts
//! the two runs disagree.
//!
//! The runs here are seconds long. The **nightly** run (`just shm-torture`) is
//! thirty minutes, per `docs/PHASE2.md` §13; this is the part that belongs in a
//! gate somebody runs on a branch.
//!
//! Requires `--features shm` (Linux). Run: `just shm-torture-self-test`, which
//! `just shm-check` calls.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Command, Output};

/// Run the shipped `shm_torture` binary.
///
/// `CARGO_BIN_EXE_shm_torture` is set for integration tests, so this is the
/// binary the recipes run, not a re-implementation of it.
fn torture(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shm_torture"))
        .args(args)
        .output()
        .expect("spawning shm_torture")
}

/// **The detector works across a process boundary.**
///
/// One child publishes a transform with a NaN translation. Some *other*
/// participant — a sibling child, or the driver's own observer, which never
/// writes anything — must read it and say so. If nothing does, every "0
/// violations" this harness has ever printed is worth nothing.
///
/// The corruption is a NaN and not a denormalized quaternion on purpose:
/// `LerpSlerp` renormalizes what it interpolates, so a non-unit quaternion
/// pushed into the ring comes back unit and this test would pass while proving
/// the opposite. `sample()` in the binary carries that note.
///
/// Mutant (applied, confirmed fatal): make `Invariant::check` return `Ok(())`
/// unconditionally — the injected run then exits 0 and the first assertion
/// fails. Also confirmed by the reverse: with the writer pacing in `work`
/// removed, the observer manages 0 checked reads and the injected run passes —
/// which is what that pacing is for.
#[test]
fn a_corrupt_transform_is_caught_by_a_process_that_did_not_write_it() {
    let out = torture(&["--duration", "8s", "--children", "4", "--inject-violation"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "the injected run passed — nothing detected a NaN transform.\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("invariant violation(s)"),
        "the run failed for some other reason.\n{stdout}\n{stderr}"
    );
    // The observer is the driver's own attachment; it publishes nothing, so a
    // violation it reports cannot have been self-inflicted. A run where only the
    // injecting child noticed would prove nothing about cross-process reads, so
    // this is the assertion that carries the test's name.
    assert!(
        stdout.contains("the observer read a bad transform"),
        "only the writer noticed; no reader in another process did.\n{stdout}"
    );
}

/// **A clean run passes, and passes having actually looked.**
///
/// The control for the test above: without it, a harness that failed
/// unconditionally would satisfy the injected half. The read count is asserted
/// for the same reason the binary prints it — "0 violations" and "0 reads" are
/// the same verdict from a harness that did nothing.
///
/// Mutant (applied, confirmed fatal): remove the `std::thread::sleep` pacing in
/// `work` — the rings then cover microseconds, the observer's reads all fall
/// outside them, and the `checked reads > 0` assertion fails at 0.
#[test]
fn a_clean_run_passes_and_validates_a_nontrivial_number_of_transforms() {
    let out = torture(&["--duration", "8s", "--children", "4", "--kill-hz", "6"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a clean torture run failed.\n{stdout}\n{stderr}"
    );

    // Every line of the driver's output starts `shm_torture: `, including the
    // header, so the suffix is what identifies the line — `find_map` on the
    // prefix alone picks up the banner and parses `4` out of "4 children".
    let reads: u64 = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("shm_torture: "))
        .find_map(|l| l.strip_suffix(" checked reads from the observer"))
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    // Thousands, not one. The observer reads in bursts of 256 between kills, so
    // even a heavily contended 8-second run clears this by two orders of
    // magnitude; a run that does not has stopped reading, not slowed down.
    assert!(
        reads > 1_000,
        "the observer validated only {reads} transforms, so `0 violations` says \
         almost nothing.\n{stdout}"
    );

    let kills: u64 = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("shm_torture: "))
        .find(|l| l.contains(" kills, "))
        .and_then(|l| l.split(' ').next())
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    assert!(
        kills > 5,
        "only {kills} children were killed, so no recovery path was exercised.\n{stdout}"
    );
}

/// **The arguments `just shm-torture` passes by default actually parse.**
///
/// This is not hypothetical. `docs/PHASE2.md` §13 spells the nightly as "30
/// minutes" and the recipe's default is `--duration 30m`, while every test above
/// passes a duration in *seconds*. A parser that handles only `s` and `ms`
/// therefore passes the entire suite while making the one command this binary
/// exists for exit instantly with `invalid float literal`.
///
/// `--help` trails the duration so the parse is all that runs: arguments are
/// processed in order, so a value that fails to parse still fails first.
///
/// Mutant (applied, confirmed fatal): drop the `("m", 60.0)` row from
/// `parse_duration`'s table — `--duration 30m` then exits non-zero and the
/// `30m` case fails.
#[test]
fn the_nightly_recipes_default_duration_parses() {
    // The spellings the justfile and the workflow actually use, plus the two
    // the help text advertises.
    for spelling in ["30m", "120s", "500ms", "1h", "45"] {
        let out = torture(&["--duration", spelling, "--help"]);
        assert!(
            out.status.success(),
            "`--duration {spelling}` was rejected: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // The control: a duration that is genuinely not a duration must still be
    // refused, or the assertions above would pass on a parser that accepted
    // everything.
    let bad = torture(&["--duration", "soon", "--help"]);
    assert!(!bad.status.success(), "`--duration soon` was accepted");
}

/// `--crash-points` is refused, naming the feature that does not exist.
///
/// `docs/PHASE2.md` §11.4 asks for crash-point injection in 10% of children and
/// §0.0 records the `crash-points` feature as not implemented. Accepting the
/// flag and running the SIGKILL test anyway would let a run report §11.3
/// coverage the repository does not have — which is the same class of quiet
/// dishonesty `bench_report`'s `--duration` refusal exists for.
///
/// Mutant (applied, confirmed fatal): replace the `--crash-points` arm with
/// `"--crash-points" => {}` — the run then reaches `--help`, exits 0, and the
/// first assertion fails.
#[test]
fn crash_point_injection_is_refused_rather_than_approximated() {
    let control = torture(&["--help"]);
    assert!(control.status.success(), "control: --help must exit 0");

    let out = torture(&["--crash-points", "--help"]);
    assert!(
        !out.status.success(),
        "--crash-points was accepted; there is no crash-points feature to arm"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("crash-points") && err.contains("not implemented"),
        "the refusal must name the missing feature, got: {err}"
    );
}
