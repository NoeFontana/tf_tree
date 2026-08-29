//! `docs/PHASE2.md` §11.3 — one test per crash point this crate owns.
//!
//! # What each test can and cannot observe
//!
//! §11.3 asks two things of a crash point: that the process **dies at the named
//! instruction** without unwinding, and that **the state it leaves behind is
//! repairable** in the way that row's right-hand column claims. Those are two
//! observations, and inside `tf_tree_core` they cannot be made against one
//! object:
//!
//! * The **death** is observed across a process boundary, because that is the
//!   only place an `abort` is observable at all. Each `<site>_aborts_*` test
//!   re-executes this test binary with `TF_TREE_CRASH_AT` armed, running one of
//!   the `#[ignore]`d `child_*` workloads below, and asserts `SIGABRT` plus the
//!   site name and hit number on the child's stderr. Arming `:2` against a
//!   workload that performs the operation twice pins the *counting*: the child
//!   must print its first completion marker and then die inside the second
//!   operation.
//!
//! * The **repair** is observed in the parent, against the state staged in
//!   place. It has to be: this crate is `no_std + alloc`, its structures are
//!   heap-backed here, and a child's heap dies with the child. Mapping the same
//!   arena into two processes is `MappedArena`, which the `tf_tree` facade
//!   builds over `tf_tree_ipc` — and `docs/decisions/0007`'s unsafe budget puts
//!   the OS boundary in `tf_tree_ipc`, not here, so the `fork`+`mmap` a genuine
//!   post-mortem read would need cannot live in this crate either.
//!
//! §11.2 already names this split for the two `attach.*` staging tests — "that
//! is coverage of the recovery, not of the crash" — and it is the same split
//! here, with the crash half now actually present. What is **not** covered by
//! either half is the join: that the real path, killed at the named
//! instruction, leaves exactly the words the repair half stages. That is an
//! argument from the placement (each site's comment names the two stores it sits
//! between), not a measurement, and it stays that way until a §11.3 harness runs
//! these sites against a shared segment.
//!
//! # Where each row's repair is asserted
//!
//! | Crash point | Repair asserted by |
//! |---|---|
//! | `push.after_seq_odd` | `tests::stale_odd_seq_from_a_dead_writer_is_healed_by_the_next_push` |
//! | `push.after_data_before_seq_even` | the same test (the row says "as above") |
//! | `push.after_seq_even_before_head` | [`an_unpublished_sample_is_invisible_and_then_overwritten`], below |
//! | `topo.after_copy_before_publish` | `tests::topology_depth_and_cycle_detection` (the aborted-mutation half) and `topology::tests::a_dead_holder_is_stolen_from_and_leaves_no_trace` |
//! | `claim.after_cas` | [`a_claim_left_by_a_dead_participant_resolves_and_is_reapable`], below |
//! | `intern.after_hash_cas_before_id_store` | `tests::intern_recovers_from_a_claimant_that_died_before_publishing` |
//!
//! The child workloads are `#[ignore]`d rather than hidden behind an env-var
//! dispatch so they are listed, runnable by hand, and harmless if some runner
//! decides to execute ignored tests: unarmed, each one just performs its
//! operation and exits 0.

// The child's stdout is its protocol — the parent parses it, exactly as
// `tf_tree_ipc`'s `ipc_child` argues for the same pattern. `panic`/`unwrap` are
// allowed for the same reason the rest of the suite allows them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]

use tf_tree_arena::{ArenaLayout, HeapArena};
use tf_tree_math::exp_se3;

use crate::arena_view::{ArenaBuilder, ArenaView};
use crate::edge::{claim, reap, slot_of, EdgeRecord};
use crate::error::{EdgeId, LookupError};
use crate::sync::Ordering;

/// PID of the participant that "died" holding something.
const DEAD_PID: u32 = 91_001;

/// Two dynamic edges, four frame slots, ring capacity 4 — enough for a workload
/// to perform its operation twice, which is what pins the hit counter.
fn two_edge_arena() -> HeapArena {
    let layout = ArenaLayout::new(4, 2, alloc::vec![4, 4]).unwrap();
    HeapArena::new(&layout, 4242, 0, [0u8; 16])
}

/// `two_edge_arena` with `a -> b` on edge 0 and `b -> c` on edge 1.
fn built_arena() -> HeapArena {
    let mut arena = two_edge_arena();
    {
        let mut builder = ArenaBuilder::new(&mut arena);
        let a = builder.view().intern("a").unwrap();
        let b = builder.view().intern("b").unwrap();
        let c = builder.view().intern("c").unwrap();
        builder
            .declare_edge(
                EdgeId(0),
                EdgeRecord::dynamic(a.get(), b.get(), 4, 0, 0, 0, 0),
            )
            .unwrap();
        builder
            .declare_edge(
                EdgeId(1),
                EdgeRecord::dynamic(b.get(), c.get(), 4, 0, 0, 0, 0),
            )
            .unwrap();
    }
    arena
}

// ---- the repair halves (feature-independent: the repair ships in every build) --

/// `push.after_seq_even_before_head`: "sample fully written but unpublished →
/// invisible, then overwritten".
///
/// The state is staged by *rewinding the publishing store* — the technique
/// §11.2 uses for the two `attach.*` rows — because the two stores that make a
/// sample visible are `seq -> even` and `head + 1`, and only the second of them
/// can be undone from outside `push`. A completed push followed by
/// `head = 0` (and `heartbeat = 0`, which `push`'s own `debug_assert` pins to
/// it) is byte-for-byte the state the crash point leaves: slot consistent, seq
/// even, payload present, `head` never moved.
///
/// Both halves of the row are asserted, and the second is the one with teeth: a
/// sample that is merely invisible but *not* overwritten would come back to life
/// the moment the ring lapped.
///
/// **Mutant:** `SampleRing::stored` returns `head.max(retained())` instead of
/// `head.min(retained())` — the reader stops consulting `head` for how much of
/// the ring is real. Applied: this test fails at the `stored()` assertion,
/// `left: 3  right: 0`, and it is the **only** test in the crate that fails
/// (88 passed, 1 failed). The "invisible" half of the row rests on this test
/// alone.
///
/// **Mutant:** `push` writes physical slot `(h + 1) & mask` instead of
/// `h & mask`, so a rewound `head` no longer sends the next push to the
/// orphaned slot. Applied: this test fails — but at the earlier "the payload
/// the dead writer wrote is present in the slot" assertion
/// (`left: [0, 0, 0, 0, 0, 0, 0]`), not at the overwrite assertion, because the
/// *first* push is displaced too. No mutation can separate the two: after the
/// rewind both pushes see `h == 0`, so any function of `h` sends them to the
/// same slot.
#[test]
fn an_unpublished_sample_is_invisible_and_then_overwritten() {
    let arena = built_arena();
    let view = ArenaView::new(&arena);
    let ring = view.ring(EdgeId(0)).unwrap();
    let orphan = exp_se3([0.1, 0.2, 0.3, 1.0, 2.0, 3.0]);
    let published = exp_se3([0.0, 0.0, 0.4, 9.0, 8.0, 7.0]);

    ring.push(1_000, &orphan).unwrap();
    assert_eq!(ring.head.load(Ordering::Relaxed), 1);
    ring.head.store(0, Ordering::Release);
    ring.heartbeat.store(0, Ordering::Relaxed);

    // The slot itself is intact — this is *not* the torn state
    // `after_data_before_seq_even` leaves, and asserting that is what stops this
    // test from silently becoming a duplicate of the A5 one.
    assert_eq!(ring.poses[0].seq_for_test() & 1, 0, "seq must be even");
    assert_eq!(
        ring.read_slot(0).unwrap().to_bits(),
        orphan.to_bits(),
        "the payload the dead writer wrote is present in the slot"
    );

    // Invisible: every reader entry point addresses samples below `head`.
    assert_eq!(ring.newest_stamp(), None);
    assert_eq!(ring.oldest_stamp(), None);
    assert_eq!(ring.stored(), 0);
    assert!(matches!(
        ring.sample::<tf_tree_math::LerpSlerp>(1_000, crate::sample::ExtrapPolicy::Error),
        Err(LookupError::NoData { .. })
    ));

    // Then overwritten: `head == 0` sends the next push to the same physical
    // slot, and the orphan is gone rather than merely unreachable.
    ring.push(2_000, &published).unwrap();
    assert_eq!(ring.newest_stamp(), Some(2_000));
    assert_eq!(
        ring.read_slot(0).unwrap().to_bits(),
        published.to_bits(),
        "the unpublished sample must be overwritten, not retained"
    );

    // And the stale stamp cannot re-enter through the monotonicity check: a
    // reader asking for the orphan's stamp is told the window starts later.
    assert!(matches!(
        ring.sample::<tf_tree_math::LerpSlerp>(1_000, crate::sample::ExtrapPolicy::Error),
        Err(LookupError::Extrapolation {
            requested: 1_000,
            ..
        })
    ));
}

/// `claim.after_cas`: "claim held by a dead participant → reapable via slot
/// indirection (A3)".
///
/// Staging needs no rewind at all, which is the point of A3: a `claim` that
/// returns and whose caller never builds a `Publisher` *is* the post-crash
/// state, because the only thing that would have freed the claim is a `Drop`
/// the abort skips.
///
/// The chain asserted here is the row's, in order: the owner word resolves to a
/// participant slot; that slot resolves to a record a liveness source can judge;
/// a competing claimer is refused and told which slot to judge; `reap` frees it;
/// and the freed edge is claimable again. A4's other half — that the reaped
/// writer refuses to publish — is `tests::a_reaped_writer_refuses_to_push`.
///
/// **Mutant:** `edge::slot_of` returns `word & 0xFFFF` instead of
/// `(word & 0xFFFF).saturating_sub(1)` — it forgets that the owner word holds
/// `slot + 1`. Applied: this test fails at `assert_eq!(slot_of(held),
/// dead_slot)` with `left: 1  right: 0`. The indirection the whole row rests on
/// then resolves to the wrong participant, which is a reaper judging the
/// liveness of a process that does not own the claim.
/// `tests::claim_is_exclusive_and_epoch_increments` fails alongside it.
#[test]
fn a_claim_left_by_a_dead_participant_resolves_and_is_reapable() {
    let arena = built_arena();
    let view = ArenaView::new(&arena);
    let (dead_slot, _) = view.participants().register(DEAD_PID, 7, 0).unwrap();

    let rec = view.claim(EdgeId(0)).unwrap();
    let (epoch, word) = claim(rec, dead_slot).unwrap();
    assert_eq!(epoch, 1);

    // The indirection: word -> slot -> a record with an identity to judge. The
    // killed process never cleared its participant record, which is exactly why
    // the reaper's verdict comes from a liveness source and not from `state`.
    let held = rec.owner.load(Ordering::Acquire);
    assert_eq!(held, word);
    assert_eq!(slot_of(held), dead_slot);
    assert!(
        !crate::edge::is_claiming(held),
        "the claim is held, not in flight"
    );
    assert_eq!(
        view.participants()
            .identity(dead_slot)
            .map(|(pid, _, _)| pid),
        Some(DEAD_PID),
        "the owner slot must still resolve to the dead participant"
    );

    // A competing claimer is refused, and is told which slot to go and judge
    // rather than being left to guess.
    assert_eq!(
        claim(rec, 3).unwrap_err(),
        crate::error::ClaimError::EdgeAlreadyClaimed {
            owner_slot: dead_slot
        }
    );

    // Reap, then re-claim: the leaked edge comes back.
    reap(rec);
    assert_eq!(rec.owner.load(Ordering::Acquire), 0);
    let (epoch2, _) = claim(rec, 3).unwrap();
    assert!(
        epoch2 > epoch,
        "the epoch must advance across a reap so the dead writer's is stale \
         (was {epoch}, now {epoch2})"
    );
}

// ---- the crash halves ---------------------------------------------------

/// Sites this crate compiles, checked against what the tests below arm.
///
/// `SITES` exists so a harness in another crate need not re-spell the literals;
/// this is what stops it drifting from the literals in the protocols, which the
/// `_aborts_at_` tests pin by actually firing.
#[cfg(feature = "crash-points")]
#[test]
fn the_published_site_list_is_the_one_the_tests_arm() {
    let mut sorted = crate::crash::SITES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        crate::crash::SITES.len(),
        "duplicate site name"
    );
    for site in ARMED_BY_TESTS {
        assert!(
            crate::crash::SITES.contains(site),
            "{site} is armed by a test but missing from crash::SITES"
        );
    }
    assert_eq!(crate::crash::SITES.len(), ARMED_BY_TESTS.len());
}

/// Every site a test below arms, so the two lists cannot drift apart silently.
#[cfg(feature = "crash-points")]
const ARMED_BY_TESTS: &[&str] = &[
    "push.after_seq_odd",
    "push.after_data_before_seq_even",
    "push.after_seq_even_before_head",
    "topo.after_copy_before_publish",
    "claim.after_cas",
    "intern.after_hash_cas_before_id_store",
    "attach.after_slot_assigned_before_publish",
];

/// A finished child run: how it died and what it managed to say first.
#[cfg(all(feature = "crash-points", unix))]
struct ChildRun {
    signal: Option<i32>,
    code: Option<i32>,
    stdout: std::string::String,
    stderr: std::string::String,
}

/// Re-execute this test binary, running one `#[ignore]`d workload, optionally
/// with a crash point armed.
///
/// `--nocapture` matters: libtest buffers a test's output and prints it when the
/// test *finishes*, and an aborting child never finishes. Without it the
/// workload's progress markers and the crash point's own diagnostic would both
/// be lost with the process.
#[cfg(all(feature = "crash-points", unix))]
fn run_child(workload: &str, armed: Option<&str>) -> ChildRun {
    use std::os::unix::process::ExitStatusExt as _;

    let exe = std::env::current_exe().expect("the test binary must be re-executable");
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(std::format!("crash_tests::{workload}"))
        .arg("--exact")
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1");
    match armed {
        Some(spec) => cmd.env(crate::crash::ENV_VAR, spec),
        None => cmd.env_remove(crate::crash::ENV_VAR),
    };
    let out = cmd.output().expect("spawning the child test binary");
    ChildRun {
        signal: out.status.signal(),
        code: out.status.code(),
        stdout: std::string::String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: std::string::String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// `SIGABRT`. Named rather than inlined because the *number* is the assertion:
/// a `panic!` would exit 101, and a panic with `panic = "abort"` would also
/// raise 6 — so the stderr check below carries the rest of the weight.
#[cfg(all(feature = "crash-points", unix))]
const SIGABRT: i32 = 6;

/// Assert a child died by `abort` at `site`, on hit `hit`, having got as far as
/// `progress` and no further.
#[cfg(all(feature = "crash-points", unix))]
fn assert_aborted_at(run: &ChildRun, site: &str, hit: u64, progress: &[&str], not: &[&str]) {
    assert_eq!(
        run.signal,
        Some(SIGABRT),
        "expected SIGABRT at {site}; signal={:?} code={:?}\nstderr:\n{}",
        run.signal,
        run.code,
        run.stderr
    );
    let expected = std::format!("crash point {site} hit {hit}, aborting");
    assert!(
        run.stderr.contains(&expected),
        "child died without announcing {site}; stderr:\n{}",
        run.stderr
    );
    // A panic on the way to the abort would leave libtest's own report behind
    // and, worse, would have unwound through the `Drop`s §11.3 forbids running.
    assert!(
        !run.stderr.contains("panicked at"),
        "the crash point must not panic; stderr:\n{}",
        run.stderr
    );
    for marker in progress {
        assert!(
            run.stdout.contains(marker),
            "child should have reached {marker:?}; stdout:\n{}",
            run.stdout
        );
    }
    for marker in not {
        assert!(
            !run.stdout.contains(marker),
            "child got past {marker:?}, so the site did not fire where it should; \
             stdout:\n{}",
            run.stdout
        );
    }
}

/// Assert the same workload, unarmed, runs to completion.
///
/// This is the control the whole file rests on. Without it "the child died" says
/// nothing: a workload broken in some unrelated way would die too, and every
/// abort assertion above would be measuring the breakage.
#[cfg(all(feature = "crash-points", unix))]
fn assert_clean_run(workload: &str, progress: &[&str]) {
    let run = run_child(workload, None);
    assert_eq!(
        run.code,
        Some(0),
        "unarmed {workload} must exit 0; signal={:?}\nstdout:\n{}\nstderr:\n{}",
        run.signal,
        run.stdout,
        run.stderr
    );
    for marker in progress {
        assert!(run.stdout.contains(marker), "unarmed run missed {marker:?}");
    }
}

/// `push.after_seq_odd` — the parity is flipped, no payload is written yet.
///
/// Armed on the **second** push, so the child must print `push 0 done` (a whole
/// push completed after the site was reached once and returned) and then die
/// inside push 1. That is what distinguishes a site that fires on a counter from
/// one that fires on the first thing it sees.
///
/// Repair: `tests::stale_odd_seq_from_a_dead_writer_is_healed_by_the_next_push`.
///
/// **Mutant:** `crash::maybe_abort`'s `if hit < *nth { return; }` becomes
/// `if hit < 1`, so an armed site fires on its first hit whatever `nth` says.
/// Applied: this test fails with "child died without announcing
/// push.after_seq_odd; stderr: tf_tree_core: crash point push.after_seq_odd hit
/// 1, aborting" — the child died one push early and never printed
/// `push 0 done`. Five of the six `_aborts_at_` tests fail with it; the one
/// armed at `:1` (`push.after_data_before_seq_even`) still passes, which is why
/// the rest are armed higher.
#[cfg(all(feature = "crash-points", unix))]
#[test]
fn push_after_seq_odd_aborts_at_the_named_point() {
    assert_clean_run("child_push", &["push 0 done", "push 1 done", "push 2 done"]);
    let run = run_child("child_push", Some("push.after_seq_odd:2"));
    assert_aborted_at(
        &run,
        "push.after_seq_odd",
        2,
        &["push 0 done"],
        &["push 1 done"],
    );
}

/// `push.after_data_before_seq_even` — payload written, seq still odd.
///
/// Repair: the same A5 test; §11.3's row for this site says "as above".
#[cfg(all(feature = "crash-points", unix))]
#[test]
fn push_after_data_before_seq_even_aborts_at_the_named_point() {
    let run = run_child("child_push", Some("push.after_data_before_seq_even:1"));
    assert_aborted_at(
        &run,
        "push.after_data_before_seq_even",
        1,
        &[],
        &["push 0 done"],
    );
}

/// `push.after_seq_even_before_head` — sample complete, `head` not yet moved.
///
/// Repair: [`an_unpublished_sample_is_invisible_and_then_overwritten`].
#[cfg(all(feature = "crash-points", unix))]
#[test]
fn push_after_seq_even_before_head_aborts_at_the_named_point() {
    let run = run_child("child_push", Some("push.after_seq_even_before_head:3"));
    assert_aborted_at(
        &run,
        "push.after_seq_even_before_head",
        3,
        &["push 0 done", "push 1 done"],
        &["push 2 done"],
    );
}

/// `topo.after_copy_before_publish` — inactive block written, word unchanged.
///
/// Repair: `tests::topology_depth_and_cycle_detection` asserts the published
/// topology and the generation are untouched by a mutation that did not reach
/// its publishing store, and `topology::tests::a_dead_holder_is_stolen_from_and_
/// leaves_no_trace` asserts the dirty scratch block is overwritten wholesale by
/// the next mutation.
#[cfg(all(feature = "crash-points", unix))]
#[test]
fn topo_after_copy_before_publish_aborts_at_the_named_point() {
    assert_clean_run("child_topo", &["set_parent 0 done", "set_parent 1 done"]);
    let run = run_child("child_topo", Some("topo.after_copy_before_publish:2"));
    assert_aborted_at(
        &run,
        "topo.after_copy_before_publish",
        2,
        &["set_parent 0 done"],
        &["set_parent 1 done"],
    );
}

/// `claim.after_cas` — the owner word is installed and no `Publisher` exists.
///
/// Repair: [`a_claim_left_by_a_dead_participant_resolves_and_is_reapable`].
///
/// **Mutant:** delete the `crash_point!("claim.after_cas")` line from
/// `edge::claim`. Applied: this test fails with "expected SIGABRT at
/// claim.after_cas; signal=None code=Some(0)" — the armed child ran both claims
/// and exited cleanly. It is the only failure (88 passed, 1 failed), which is
/// the whole reason this test exists.
#[cfg(all(feature = "crash-points", unix))]
#[test]
fn claim_after_cas_aborts_at_the_named_point() {
    assert_clean_run("child_claim", &["claim 0 done", "claim 1 done"]);
    let run = run_child("child_claim", Some("claim.after_cas:2"));
    assert_aborted_at(
        &run,
        "claim.after_cas",
        2,
        &["claim 0 done"],
        &["claim 1 done"],
    );
}

/// `attach.after_slot_assigned_before_publish` — slot `RESERVED`, nothing
/// published into it.
///
/// **This is the row whose state the repository could previously only stage.**
/// The window is the `FREE -> RESERVED` CAS to the `live_word` store, measured
/// at ~12 ns in [`0028`] open question 4, so nothing outside fault injection can
/// kill a process inside it — which is why §11.2's two
/// `..._collects_a_record_left_reserved_by_a_killed_registrant` tests build the
/// word by hand (`register_at`, then the publishing store rewound) and say in
/// their own comments that this is coverage of the recovery and not of the
/// crash. With the site placed, a real process really dies there.
///
/// Repair: those same two collectors, which accept any observed word including
/// `RESERVED` (`0028` plan step 1) — the owner's hangup callback and its slot
/// assigner. What this test adds is that the state they collect is now
/// *produced* rather than arranged.
///
/// [`0028`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0028-the-slot-a-killed-participant-keeps.md
///
/// **Mutant:** delete the `crash_point!` line from `participant::fill_slot`.
/// The armed child then registers twice and exits cleanly, and this fails with
/// "expected SIGABRT at attach.after_slot_assigned_before_publish".
#[cfg(all(feature = "crash-points", unix))]
#[test]
fn attach_after_slot_assigned_before_publish_aborts_at_the_named_point() {
    assert_clean_run("child_attach", &["attach 0 done", "attach 1 done"]);
    let run = run_child(
        "child_attach",
        Some("attach.after_slot_assigned_before_publish:2"),
    );
    assert_aborted_at(
        &run,
        "attach.after_slot_assigned_before_publish",
        2,
        &["attach 0 done"],
        &["attach 1 done"],
    );
}

/// `intern.after_hash_cas_before_id_store` — hash claimed, claimant recorded, id
/// unpublished.
///
/// Repair: `tests::intern_recovers_from_a_claimant_that_died_before_publishing`,
/// whose staging helper `wedge_intern_slot` documents itself as "exactly as a
/// process killed between the hash CAS and the id store would leave it".
#[cfg(all(feature = "crash-points", unix))]
#[test]
fn intern_after_hash_cas_before_id_store_aborts_at_the_named_point() {
    assert_clean_run("child_intern", &["intern 0 done", "intern 1 done"]);
    let run = run_child(
        "child_intern",
        Some("intern.after_hash_cas_before_id_store:2"),
    );
    assert_aborted_at(
        &run,
        "intern.after_hash_cas_before_id_store",
        2,
        &["intern 0 done"],
        &["intern 1 done"],
    );
}

/// An armed name that no site carries must not fire anywhere.
///
/// The failure this rules out is the one that would make every test above pass
/// for the wrong reason: a `maybe_abort` that aborted on any call once the
/// variable was set.
#[cfg(all(feature = "crash-points", unix))]
#[test]
fn an_unknown_site_name_arms_nothing() {
    let run = run_child("child_push", Some("push.no_such_site:1"));
    assert_eq!(
        run.code,
        Some(0),
        "an unknown site name must arm nothing; stderr:\n{}",
        run.stderr
    );
    let malformed = run_child("child_push", Some("push.after_seq_odd:not-a-number"));
    assert_eq!(
        malformed.code,
        Some(0),
        "a malformed nth_hit must disarm rather than fire; stderr:\n{}",
        malformed.stderr
    );
}

// ---- child workloads ----------------------------------------------------
//
// Each runs its operation more than once so an `:n` arming has something to
// count, and prints a completion marker after each. Unarmed they exit 0, which
// is what `assert_clean_run` checks.

/// Three pushes on one ring. Marker after each.
#[test]
#[ignore = "child workload for the §11.3 push crash points"]
fn child_push() {
    let arena = built_arena();
    let view = ArenaView::new(&arena);
    let ring = view.ring(EdgeId(0)).unwrap();
    for i in 0..3u64 {
        ring.push(
            1_000 * (i as i64 + 1),
            &exp_se3([0.0, 0.0, 0.1 * i as f64, 1.0, 2.0, 3.0]),
        )
        .unwrap();
        report(std::format!("push {i} done"));
    }
}

/// Two topology mutations. Marker after each.
#[test]
#[ignore = "child workload for the §11.3 topology crash point"]
fn child_topo() {
    let arena = built_arena();
    let view = ArenaView::new(&arena);
    let topo = view.topology();
    let a = view.find_frame("a").unwrap().unwrap();
    let b = view.find_frame("b").unwrap().unwrap();
    let c = view.find_frame("c").unwrap().unwrap();
    topo.set_parent(b, a.get(), 1).unwrap();
    report("set_parent 0 done");
    topo.set_parent(c, b.get(), 2).unwrap();
    report("set_parent 1 done");
}

/// Two claims, on the arena's two edges. Marker after each.
#[test]
#[ignore = "child workload for the §11.3 claim crash point"]
fn child_claim() {
    let arena = built_arena();
    let view = ArenaView::new(&arena);
    let (slot, _) = view.participants().register(DEAD_PID, 7, 0).unwrap();
    for (i, edge) in [EdgeId(0), EdgeId(1)].into_iter().enumerate() {
        // The returned handle is deliberately dropped on the floor rather than
        // wrapped in a `Publisher`: a `Publisher::drop` would release the claim,
        // and the state this workload exists to leave is a *held* one.
        let _ = claim(view.claim(edge).unwrap(), slot).unwrap();
        report(std::format!("claim {i} done"));
    }
}

/// Two registrations into the participant table. Marker after each.
///
/// `register` is `fill_slot`, which is where the §11.3 `attach.*` window is: the
/// `FREE -> RESERVED` CAS, then the identity fields, then the publishing store
/// of `live_word`.
#[test]
#[ignore = "child workload for the §11.3 attach crash point"]
fn child_attach() {
    let arena = two_edge_arena();
    let view = ArenaView::new(&arena);
    for i in 0..2u32 {
        // Distinct pids, so the two registrations cannot be confused for one
        // retried. The slot index is not asserted here — the parent asserts the
        // *markers*, and which slot a registrar wins is the assigner's business.
        view.participants().register(DEAD_PID + i, 7, 0).unwrap();
        report(std::format!("attach {i} done"));
    }
}

/// Two interns of fresh names, by a registered participant. Marker after each.
#[test]
#[ignore = "child workload for the §11.3 intern crash point"]
fn child_intern() {
    // A bare arena: `built_arena` interns three names already, and this workload
    // has to be the only thing hitting the site.
    let arena = two_edge_arena();
    let slot = {
        let view = ArenaView::new(&arena);
        view.participants().register(DEAD_PID, 7, 0).unwrap().0
    };
    let view = ArenaView::new(&arena).as_participant(slot);
    for (i, name) in ["victim", "second"].into_iter().enumerate() {
        view.intern(name).unwrap();
        report(std::format!("intern {i} done"));
    }
}

/// Print a progress marker the parent can look for, and flush it.
///
/// Rust's stdout is line-buffered rather than block-buffered even on a pipe, so
/// the flush is belt and braces — but a marker lost to buffering would turn
/// "the site fired too early" into a passing test, which is the one failure this
/// file cannot afford.
fn report(what: impl core::fmt::Display) {
    use std::io::Write as _;
    std::println!("{what}");
    let _ = std::io::stdout().flush();
}
