//! `shm_torture`'s own gate: the detector is tested, not trusted. The runs here
//! are seconds long; the nightly (`just shm-torture`) is thirty minutes per
//! `docs/PHASE2.md` §13.
//!
//! Requires `--features shm` (Linux). Run: `just shm-torture-self-test`, which
//! `just shm-check` calls.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Command, Output};

/// Run the shipped `shm_torture` binary.
fn torture(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shm_torture"))
        .args(args)
        .output()
        .expect("spawning shm_torture")
}

/// The detector works across a process boundary: a child publishes a NaN
/// translation and some other participant must read it and say so. NaN, not a
/// denormalized quaternion, because `LerpSlerp` renormalizes what it
/// interpolates. Seed 999, not the default, which was the only seed that ever
/// detected anything.
///
/// Mutants: `Invariant::check` returning `Ok(())` makes the injected run exit 0;
/// removing the writer pacing in `work` leaves the observer 0 checked reads.
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
    // The observer publishes nothing, so a violation it reports cannot be
    // self-inflicted; this carries the test's name.
    assert!(
        stdout.contains("the observer read a bad transform"),
        "only the writer noticed; no reader in another process did.\n{stdout}"
    );
}

/// A run that validates nothing fails instead of printing `PASS`.
/// `--readers-only` leaves every ring empty; every other test is the control.
///
/// Mutant: delete the `if vacuous { bail!(...) }` arm in `drive`.
#[test]
fn a_run_that_validates_nothing_fails_instead_of_passing() {
    // `--children 4`: below `MIN_ATTACHED_FOR_ORDINARY_KILL + 1` the parse-time
    // refusal of the owner-kill arm lands before the vacuity guard.
    let out = torture(&["--duration", "3s", "--children", "4", "--readers-only"]);
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

/// An injected run that detects nothing fails naming the detector;
/// `--readers-only` is the only way to reach that state on purpose.
///
/// Mutant: delete the `if a.inject { bail!(...) }` arm; the run then fails on the
/// read floor and the second assertion fails.
#[test]
fn an_injected_run_that_detects_nothing_says_so() {
    let out = torture(&[
        "--duration",
        "3s",
        // 4: below `MIN_ATTACHED_FOR_ORDINARY_KILL + 1` the parse-time refusal lands first.
        "--children",
        "4",
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

/// A clean run passes having actually looked; the control for the injected test.
/// Seed 999 at six children is the configuration the shipped harness once read
/// nothing in.
///
/// The 8 s duration is not load-bearing: the owner reaps a dead participant's
/// record on hangup (`docs/PHASE2.md` §3.9), `shm_torture` checks each round for
/// a live writer and a recent sample, and `check_recovery` names leaked slots.
///
/// Mutants: `common_window` returning `None` drops the composed count to 0 and
/// fails the second assertion; removing the `std::thread::sleep` pacing in `work`
/// survives, and is kept to spread the kills across the protocol.
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

    // Every output line starts `shm_torture: `, so the suffix identifies the line.
    let reads: u64 = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("shm_torture: "))
        .find_map(|l| l.strip_suffix(" checked reads from the observer"))
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    // Thousands, not one: a run that misses this has stopped reading.
    assert!(
        reads > 1_000,
        "the observer validated only {reads} transforms, so `0 violations` says \
         almost nothing.\n{stdout}"
    );

    // Composed reads specifically: §11.4 is about `map -> tool` composing all
    // four edges at one stamp.
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

    // §3.5: 8 s lands exactly one owner kill (first at `OWNER_KILL_FIRST`, 4 s).
    // Asserted on the printed line, not the exit status: a run with the arm
    // silently disabled exits 0 too. `a_run_that_never_inherits_the_owner_role_fails_naming_it`
    // is the red half.
    assert!(
        stdout.contains("§3.5 owner kill 1:"),
        "the run never killed the rendezvous owner, so PHASE2 §3.5 was not exercised \
         at all.\n{stdout}"
    );
    assert!(
        stdout.contains("a fresh process joined"),
        "the owner was killed and no fresh process could join afterwards — or the run \
         did not check. An ownerless arena is exactly what refuses a new joiner, so this \
         is the assertion that says the role was inherited.\n{stdout}"
    );
    // Parsed as a number: a substring match on `0 survivor(s) inherited` also
    // matches `10 survivor(s) inherited`.
    let inherited: u64 = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("shm_torture: §3.5: "))
        .find_map(|l| {
            l.split(" inheritance(s) recorded by survivors")
                .next()
                .and_then(|head| head.rsplit(", ").next())
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0);
    assert!(
        inherited > 0,
        "a migration recovered with no survivor recording an inheritance: something is \
         serving and §3.5's caller-driven trigger is not why.\n{stdout}"
    );
}

/// The strict half of the leak check. `check_recovery` sweeps and requires
/// collection of records `docs/decisions/0043` says no hangup callback can reach,
/// and fails the run on any other leaked `LIVE` record; this case exercises the
/// second. `--no-kill-owner` keeps the owner child parked, so every worker record
/// is reachable by its hangup callback. A migrating run downgrades that verdict
/// to a note, so the run prints which verdict applied and this test asserts it.
///
/// Mutant: delete the `table.reclaim(slot, observed)` in the owner's hangup
/// callback (`crates/tf_tree/src/open.rs`, `docs/decisions/0028` plan step 4);
/// the run exits 1 naming the slots.
#[test]
fn a_run_that_never_migrates_holds_every_worker_record_to_the_strict_path() {
    let out = torture(&[
        "--duration",
        "8s",
        "--children",
        "4",
        "--kill-hz",
        "4",
        "--seed",
        "999",
        "--no-kill-owner",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a run with one owner for its whole life failed; every worker record here is \
         reachable by that owner's hangup callback.\n{stdout}\n{stderr}"
    );
    // The run must not reach the swept partition for anything but the owner's
    // own record, or it passes on a hangup collector that does nothing.
    assert!(
        !stdout.contains("§3.5 owner kill 1:"),
        "`--no-kill-owner` killed the owner, so this case is not the no-migration \
         configuration it is named for.\n{stdout}"
    );
    assert!(
        stdout.contains("recovery:") && !stdout.contains("RECOVERY FAILURE"),
        "the recovery check did not run, so nothing here judged a participant \
         record.\n{stdout}"
    );
    // Positive control: a downgrade to the sweep-and-note verdict would leave
    // this test green and vacuous.
    assert!(
        stdout.contains("judged on the STRICT path"),
        "this run did not put the leak check on its strict path, so passing says \
         nothing about the owner's hangup callback.\n{stdout}"
    );
}

/// A run in which nothing inherits the owner role fails naming §3.5. `--no-inherit`
/// makes every child skip `Tree::owner_lost`, so the arena goes ownerless
/// (`docs/decisions/0037`); this is the red half of the §3.5 assertions above.
/// The run waits the full ten-second recovery deadline.
///
/// Mutant: delete the `if !failed.is_empty()` bail in `drive`.
#[test]
fn a_run_that_never_inherits_the_owner_role_fails_naming_it() {
    let out = torture(&[
        "--duration",
        "6s",
        // 4: below `MIN_ATTACHED_FOR_ORDINARY_KILL + 1` the parse-time refusal lands first.
        "--children",
        "4",
        "--kill-hz",
        "4",
        "--seed",
        "999",
        "--no-inherit",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a run in which no survivor ever called `owner_lost` passed, so this run proved \
         nothing about §3.5.\n{stdout}\n{stderr}"
    );
    // The message must not assert a kill that may not have happened: a run whose
    // every attempt deferred is named separately, not reported as a migration
    // that did not recover.
    assert!(
        !stderr.contains("produced 0 migration(s)"),
        "the owner-kill arm never fired on this host, so `--no-inherit` tested nothing: every \
         attempt was deferred for want of a second eligible heir. That is a statement about \
         this fleet's population, not about §3.5 — raise --children or lower \
         --kill-hz.\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("ownership migration did not happen"),
        "the run failed for some other reason; the failure must name §3.5.\n{stdout}\n{stderr}"
    );
    // The specific diagnosis, not just the verdict: the read floor also trips
    // here, but the migration check bails first.
    assert!(
        stdout.contains("NO fresh process joined"),
        "the failure must say that no fresh process could join, which is the property \
         §3.5 restores and the one an internal flag cannot observe.\n{stdout}"
    );
}

/// A run whose owner-kill arm fired and never landed fails, however short. The
/// floor once counted the schedule's second attempt, so a short run whose one
/// attempt deferred printed `PASS` over `§3.5: 0 owner kill(s)`.
/// `--defer-owner-kills` is the positive control; nothing else reaches the path.
///
/// Mutant: restore the condition to `a.duration >= OWNER_KILL_FIRST + every`.
#[test]
fn a_short_run_whose_owner_kills_all_defer_fails_instead_of_passing() {
    let out = torture(&[
        "--duration",
        "6s",
        "--children",
        "4",
        "--kill-hz",
        "4",
        "--seed",
        "999",
        // More than the run can attempt, so every one defers.
        "--defer-owner-kills",
        "999",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a run whose every owner kill deferred printed PASS. It killed no owner, so it \
         covers none of §3.5 and must not be quoted as if it did.\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("produced 0 migration(s)"),
        "the failure must say the arm produced no migration, and from how many attempts — \
         a run that failed on the read floor instead would satisfy the assertion above while \
         saying nothing about the owner.\n{stdout}\n{stderr}"
    );
    // The run's own tally must agree with the verdict.
    assert!(
        stdout.contains("shm_torture: §3.5: 0 owner kill(s)"),
        "the run reported a migration it was not supposed to be able to make.\n{stdout}"
    );
}

/// A deferral that clears costs the run nothing: it retries at
/// `OWNER_KILL_DEFERRAL_RETRY` rather than re-arming a whole
/// `--owner-kill-every` later. Two forced deferrals, then the run must migrate
/// and pass.
///
/// Mutant: re-arm with `every` on the deferred branch.
#[test]
fn a_deferral_that_clears_still_leaves_time_for_the_kill() {
    let out = torture(&[
        "--duration",
        "8s",
        "--children",
        "4",
        "--kill-hz",
        "4",
        "--seed",
        "7",
        "--defer-owner-kills",
        "2",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "two deferrals that cleared cost the run its owner kill.\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("deferred for want of a second eligible heir: 2"),
        "the control did not defer the two kills it was asked to, so this run does not \
         exercise the retry at all.\n{stdout}"
    );
    assert!(
        stdout.contains("shm_torture: §3.5: 1 owner kill(s)"),
        "the arm never recovered from the deferrals; the retry is what this pins.\n{stdout}"
    );
}

/// The deferral budget has a floor: it is `3 × --owner-kill-every`, which is
/// zero at `--owner-kill-every 0s` and would make the first deferral fatal.
/// With the floor the run reports a `1.0s budget`.
///
/// Mutant: drop the `.max(MIN_OWNER_KILL_DEFERRAL_BUDGET)`.
#[test]
fn the_deferral_budget_does_not_collapse_on_a_zero_interval() {
    let out = torture(&[
        "--duration",
        "6s",
        "--children",
        "4",
        "--kill-hz",
        "4",
        "--seed",
        "5",
        "--owner-kill-every",
        "0s",
        "--defer-owner-kills",
        "999",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Still fails, but on the budget, not on the first attempt.
    assert!(
        !out.status.success(),
        "a run whose every owner kill deferred printed PASS.\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("of 1.0s budget"),
        "the budget did not take its floor, so a zero interval derived a zero \
         budget.\n{stdout}"
    );
    // A single deferral must not be fatal.
    let deferrals: usize = stderr
        .split("Deferrals so far: ")
        .nth(1)
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|d| d.parse().ok())
        })
        .unwrap_or(0);
    assert!(
        deferrals >= 3,
        "the run wedged after {deferrals} deferral(s); the budget exists so that ordinary \
         churn — a replacement finishing its handshake — clears, and three has been the \
         tolerated number since the bound was a count.\n{stdout}\n{stderr}"
    );
}

/// `--defer-owner-kills` is refused when there is no arm to defer, as
/// `--crash-points` is for a flag whose sites are compiled out.
#[test]
fn deferring_owner_kills_without_the_arm_is_refused() {
    let out = torture(&[
        "--duration",
        "5s",
        "--children",
        "4",
        "--no-kill-owner",
        "--defer-owner-kills",
        "2",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "the run accepted a control over an arm it had turned off.\n{stderr}"
    );
    assert!(
        stderr.contains("there is no owner-kill arm to defer"),
        "the refusal must name what is missing.\n{stderr}"
    );
}

/// The kill-window class, held open on purpose and required to survive it. In
/// the `kill()`-to-`wait()` interval the owner is dead and undetectable, so
/// nothing can inherit and a leaving survivor cannot return (§3.4 step 4); if
/// all leave, the arena is ownerless with nobody attached.
///
/// `--stop-owner-ms` (`SIGSTOP`) widens the window on any host; the
/// `--victim-ballast-mb` alternative depends on 4 KiB pages and is vacuous under
/// `transparent_hugepage=always`, as on CI.
///
/// Mutant: remove the `kill_window_open` check from the detach arm; the run
/// wedges at owner kill 1 with `4 heir(s) attached before the kill, 0 after it`.
#[test]
fn a_kill_window_wide_enough_to_drain_the_pool_does_not_wedge_the_arena() {
    let out = torture(&[
        "--duration",
        "20s",
        // `MIN_ATTACHED_FOR_ORDINARY_KILL + 1`: the thinnest fleet the arm accepts.
        "--children",
        "4",
        "--kill-hz",
        "4",
        "--seed",
        "424242",
        // A 20 s run still contains several owner kills.
        "--owner-kill-every",
        "4s",
        // Far past a plain reap, so the detach arm fires several times per window.
        "--stop-owner-ms",
        "300",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a fleet at the owner-kill arm's own floor did not survive a widened kill window. \
         That is the 2026-09-11 nightly failure, reproduced deliberately.\n{stdout}\n{stderr}"
    );
    assert!(
        !stdout.contains("UNRECOVERABLE"),
        "the arena reached the absorbing state.\n{stdout}"
    );
    // The control must be seen to have fired.
    assert!(
        !stdout.contains("could not stop pid"),
        "the positive control did not fire, so this run says nothing about the kill \
         window.\n{stdout}"
    );
    // Anti-vacuity: a non-zero suppression count is the evidence that the window
    // was held open and the exemption carried the run.
    let suppressed = stdout
        .lines()
        .find_map(|l| {
            l.strip_prefix("shm_torture: detaches suppressed inside an owner-kill window: ")
        })
        .and_then(|n| n.trim().parse::<usize>().ok())
        .unwrap_or_else(|| panic!("the run printed no suppression tally at all.\n{stdout}"));
    assert!(
        suppressed > 0,
        "the run suppressed no detaches, so no survivor drew its detach arm inside a \
         300 ms window — which cannot happen at one draw per ~50 operations unless the \
         marker is not being read.\n{stdout}"
    );
}

/// `--crash-site` is refused without `--crash-points`: accepting it in a build
/// with the sites compiled out would report every site unreachable while arming
/// none.
///
/// Mutant: drop the `crash_site.is_some() && !crash_points` guard.
#[test]
fn a_forced_crash_site_is_refused_without_the_flag_that_arms_it() {
    let out = torture(&["--crash-site", "claim.after_cas", "--help"]);
    assert!(
        !out.status.success(),
        "--crash-site was accepted without --crash-points, so the probe would have armed \
         nothing while reporting a reachability result"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--crash-site needs --crash-points"),
        "the refusal must name what is missing, got: {err}"
    );
}

/// The arguments `just shm-torture` passes by default parse: the recipe's
/// `--duration 30m` (`docs/PHASE2.md` §13) is the one command every seconds-only
/// test would miss. `--help` trails the duration so the parse is all that runs.
///
/// Mutant: drop the `("m", 60.0)` row from `parse_duration`'s table.
#[test]
fn the_nightly_recipes_default_duration_parses() {
    // The spellings the justfile and workflow use, plus the help text's.
    for spelling in ["30m", "120s", "500ms", "1h", "45"] {
        let out = torture(&["--duration", spelling, "--help"]);
        assert!(
            out.status.success(),
            "`--duration {spelling}` was rejected: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // Control: a non-duration must still be refused.
    let bad = torture(&["--duration", "soon", "--help"]);
    assert!(!bad.status.success(), "`--duration soon` was accepted");
}

/// `--crash-points` is refused by a build that cannot honour it, and the refusal
/// names why: the children are this same executable, so a compiled-out site is
/// compiled out everywhere. This test binary is built without `crash-points`;
/// `just shm-torture-crash-points` is the accepting path.
///
/// Mutant: replace the `--crash-points` arm with `"--crash-points" => {}`.
#[test]
fn crash_point_injection_is_refused_by_a_build_that_cannot_arm_it() {
    let control = torture(&["--help"]);
    assert!(control.status.success(), "control: --help must exit 0");

    let out = torture(&["--crash-points", "--help"]);
    assert!(
        !out.status.success(),
        "--crash-points was accepted by a binary built without the feature, so it \
         would have armed nothing in any child"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("crash-points") && err.contains("children are this same executable"),
        "the refusal must say why this build cannot honour it, got: {err}"
    );
}
