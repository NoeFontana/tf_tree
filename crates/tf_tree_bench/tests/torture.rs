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
///
/// **The seed is not the default.** The default was the only seed the shipped
/// harness detected anything on: with the driver joining instead of owning the
/// rendezvous, seeds 999, 7, 8, 1 and 123456789 all ran the injector for a full
/// 15 s, read nothing, and exited 0. A self-test bound to one lucky seed is the
/// thing this file exists to prevent, so it runs on a seed that used to fail.
#[test]
fn a_corrupt_transform_is_caught_by_a_process_that_did_not_write_it() {
    let out = torture(&[
        "--duration",
        "8s",
        "--children",
        "4",
        "--seed",
        "999",
        "--inject-violation",
    ]);
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

/// **A run that validates nothing fails, instead of printing `PASS`.**
///
/// `--readers-only` starts children that attach and read but never claim or
/// publish, so every ring stays empty and the observer validates exactly
/// nothing. That is the state the shipped harness reported as a pass — on most
/// seeds, for thirty minutes at a time, with a NaN in flight — and the whole
/// value of a soak test is that its silence means something.
///
/// The control is every other test in this file: they all run without the flag
/// and pass, so this cannot be satisfied by a binary that fails unconditionally.
///
/// Mutant (applied, confirmed fatal): delete the `if vacuous { bail!(...) }`
/// arm in `drive` — the run then prints `PASS` and exits 0, and the first
/// assertion fails.
#[test]
fn a_run_that_validates_nothing_fails_instead_of_passing() {
    let out = torture(&["--duration", "3s", "--children", "2", "--readers-only"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a run that validated 0 transforms passed.\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("proves nothing"),
        "the run failed for some other reason.\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("0 composed map->tool, 0 single-edge"),
        "the run must print what it validated, not just its verdict.\n{stdout}"
    );
}

/// **An injected run that detects nothing fails naming the detector.**
///
/// `--inject-violation` means "this run is expected to FAIL". Reaching the end
/// of it with zero violations is therefore a defect in the harness however
/// healthy the rest of the output looks, and combining it with `--readers-only`
/// is the only way to reach that state on purpose: the injector never gets to
/// publish, so nothing can be detected.
///
/// Mutant (applied, confirmed fatal): delete the `if a.inject { bail!(...) }`
/// arm — the run then fails on the read floor instead, and the second assertion
/// fails because the message no longer mentions the detector.
#[test]
fn an_injected_run_that_detects_nothing_says_so() {
    let out = torture(&[
        "--duration",
        "3s",
        "--children",
        "2",
        "--readers-only",
        "--inject-violation",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an injected run that detected nothing passed.\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("ran to completion with 0 violations"),
        "the failure must name the detector, not only the read counts.\n{stdout}\n{stderr}"
    );
}

/// **A clean run passes, and passes having actually looked.**
///
/// The control for the test above: without it, a harness that failed
/// unconditionally would satisfy the injected half. The read count is asserted
/// for the same reason the binary prints it — "0 violations" and "0 reads" are
/// the same verdict from a harness that did nothing.
///
/// Mutant (applied, **survived**): removing the `std::thread::sleep` pacing in
/// `work` leaves this green at 11 989 composed reads. It was fatal against the
/// reader that asked at `now` — the rings then covered microseconds and every
/// lookup fell outside them — and it is not against one that probes each ring
/// for its window immediately before reading it. The pacing is kept for the
/// other reason its comment gives (spreading the kills across the protocol
/// rather than into one hot loop), and that reason has no mutant.
///
/// **Seed 999 at six children, deliberately.** That is the configuration the
/// shipped harness read *nothing* in — 0 composed reads over 15 s, exit 0 — and
/// the default seed is the one it happened to survive, so a test bound to the
/// default is a test of one lucky draw.
///
/// Mutant (applied, confirmed fatal): make `common_window` return `None`
/// unconditionally — the composed count drops to 0 and the second assertion
/// fails while the first still passes on the single-edge reads alone, which is
/// exactly the gap that assertion is for.
///
/// Mutant (applied, **survived**, and named because the surprise is the point):
/// restoring `observe`'s old hill-climbing aim leaves this green at 12 967
/// composed reads. The aiming was never what made the harness vacuous; the
/// rendezvous was. See [`shm_torture`'s `attach_observer`].
///
/// # Why eight seconds used to be load-bearing, and is not any more
///
/// Until 2026-08-17 a `SIGKILL`ed participant left its record `LIVE` for ever
/// and the arena's owner would never grant that slot again, so a run at
/// `--kill-hz 6` with 6 children exhausted the 64-slot table after about 57
/// kills — `t ≈ 9.5 s`. **This case runs for 8.** The margin was measured, not
/// estimated: this exact invocation against the unfixed engine ends having made
/// 50 kills with 47 slots leaked and 4 alive, so 52 of the 64 are gone and the
/// wedge is **12 slots — two seconds — away**. And it is worth being precise
/// about what it would have done past that margin, because the answer is not
/// "fail on the read floor": four rings whose writers are
/// all gone still answer every lookup inside the window they froze with, so the
/// composed count stays at the full 256 a round and the run reports a perfect
/// score over a dead arena. Measured on this host at `--duration 60s`, before
/// the fix: `writers=0.0/4 freshest=25670ms composed=25600/25600`, with 8193
/// `NoParticipantSlots` refusals on stderr. The 30-minute nightly is what
/// finally showed it, and only because on that runner the four rings happened
/// to freeze *without* overlapping.
///
/// Two things changed, and neither is this test's duration. The owner now reaps
/// a dead participant's record on hangup, which is what `docs/PHASE2.md` §3.9
/// always said it did — measured after: 728 kills over 120 s with the
/// registered-slot count flat at 5 of 64. And `shm_torture` now checks on every
/// round that some chain edge has a *live* writer and that the freshest sample
/// is recent, failing a run that spends most of itself quiescent, so a
/// regression of the first cannot hide the way it hid before. `check_recovery`
/// reports leaked slots by name on top of that.
///
/// The two are independent, and this case is the place that shows it. Run the
/// current harness against the *unfixed* engine (measured, in a scratch copy of
/// the tree with `open.rs` reverted): it exits 1 on `1 recovery failure(s)` —
/// "47 of 64 participant slot(s) hold a LIVE record for a process the kernel
/// says is dead" — while reporting `composed=13056/13056` and `live=96%`. So the
/// harness half fails an engine that leaks even when the run was, for its whole
/// eight seconds, genuinely healthy. That is the intended relationship: this
/// test goes green again only when the engine stops leaking, not when the
/// duration is tuned back under the margin.
#[test]
fn a_clean_run_passes_and_validates_a_nontrivial_number_of_transforms() {
    let out = torture(&[
        "--duration",
        "8s",
        "--children",
        "6",
        "--kill-hz",
        "6",
        "--seed",
        "999",
    ]);
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

    // **Composed reads specifically.** The single-edge reads alone would satisfy
    // the total above, and they are the easy half: one ring, one window. §11.4's
    // property is about a `map -> tool` that composes all four edges at one
    // stamp, and that is the number that was zero.
    let composed: u64 = stdout
        .lines()
        .filter_map(|l| l.trim().strip_prefix("shm_torture:   "))
        .find_map(|l| l.split(" composed").next())
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0);
    assert!(
        composed > 1_000,
        "only {composed} of those reads composed the whole chain; a bad sample on \
         three of the four edges would have gone unseen.\n{stdout}"
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
