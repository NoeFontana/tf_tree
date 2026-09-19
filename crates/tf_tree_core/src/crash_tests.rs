//! `docs/PHASE2.md` §11.3 — one test per crash point this crate owns.
//!
//! Each crash point has two halves. The **death** is observed across a process
//! boundary: each `<site>_aborts_*` test re-executes this binary with
//! `TF_TREE_CRASH_AT` armed on an `#[ignore]`d `child_*` workload and asserts
//! `SIGABRT` plus the site name and hit number on stderr (arming `:2` pins the
//! counting). The **repair** is observed in the parent against staged state,
//! because this `no_std` crate's structures are heap-backed and the `fork`+`mmap`
//! a real post-mortem read needs belongs in `tf_tree_ipc` (`docs/decisions/0007`).
//! §11.2 names the same split; the join (that the real path leaves exactly the
//! staged words) is argued from each site's placement, not measured.
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
//! The child workloads are `#[ignore]`d so they are listed and harmless if run
//! unarmed (each performs its operation and exits 0).

// The child's stdout is its protocol; `panic`/`unwrap` as in the rest of the suite.
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

/// Two dynamic edges, four frame slots, ring capacity 4.
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
/// Staged by rewinding the publishing store (§11.2's technique): a completed push
/// then `head = 0` and `heartbeat = 0` is the state the crash point leaves. The
/// second half (overwritten, not merely invisible) is the one with teeth.
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

    // Intact, not the torn state `after_data_before_seq_even` leaves.
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

    // Overwritten: `head == 0` sends the next push to the same slot.
    ring.push(2_000, &published).unwrap();
    assert_eq!(ring.newest_stamp(), Some(2_000));
    assert_eq!(
        ring.read_slot(0).unwrap().to_bits(),
        published.to_bits(),
        "the unpublished sample must be overwritten, not retained"
    );

    // The orphan's stamp is now outside the window.
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
/// No rewind needed (A3): a `claim` whose caller never builds a `Publisher` *is*
/// the post-crash state. Asserted in order: the owner word resolves to a slot;
/// the slot to a judgeable record; a competing claimer is refused and told which
/// slot; `reap` frees it; it is claimable again. A4's other half is
/// `tests::a_reaped_writer_refuses_to_push`.
#[test]
fn a_claim_left_by_a_dead_participant_resolves_and_is_reapable() {
    let arena = built_arena();
    let view = ArenaView::new(&arena);
    let (dead_slot, _) = view.participants().register(DEAD_PID, 7, 0).unwrap();

    let rec = view.claim(EdgeId(0)).unwrap();
    let (epoch, word) = claim(rec, dead_slot).unwrap();
    assert_eq!(epoch, 1);

    // word -> slot -> record; the verdict comes from a liveness source, not `state`.
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

    // A competing claimer is refused and told which slot to judge.
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

/// `SITES` matches what the tests below arm.
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

/// Every site a test below arms.
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

/// Re-execute this binary on one `#[ignore]`d workload, optionally armed.
/// `--nocapture` because an aborting child never flushes libtest's buffer.
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

/// `SIGABRT`; a `panic = "abort"` panic also raises 6, so stderr is checked too.
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
    // A panic would have unwound through the `Drop`s §11.3 forbids running.
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
/// The control: without it a workload broken for another reason would pass every
/// abort assertion.
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
/// Armed on the **second** push, so the child prints `push 0 done` and dies in
/// push 1: the site fires on a counter, not the first thing it sees.
///
/// Repair: `tests::stale_odd_seq_from_a_dead_writer_is_healed_by_the_next_push`.
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
/// **Mutant:** delete the `crash_point!("claim.after_cas")` line from `edge::claim`;
/// the armed child exits cleanly and only this test fails.
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
/// The window (`FREE -> RESERVED` CAS to the `live_word` store) is too narrow to
/// hit without fault injection ([`0028`] open question 4); §11.2's two
/// `..._collects_a_record_left_reserved_by_a_killed_registrant` tests stage it.
/// This site produces it for real.
///
/// Repair: those two collectors, which accept any observed word including
/// `RESERVED` (`0028` plan step 1).
///
/// [`0028`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0028-the-slot-a-killed-participant-keeps.md
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

/// An armed name no site carries fires nowhere (else every test above could pass
/// for the wrong reason).
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
// Each runs its operation more than once so `:n` has something to count, and
// prints a marker after each.

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
        // No `Publisher`: its `Drop` would release the claim we want held.
        let _ = claim(view.claim(edge).unwrap(), slot).unwrap();
        report(std::format!("claim {i} done"));
    }
}

/// Two registrations into the participant table. Marker after each.
///
/// `register` is `fill_slot`, where the §11.3 `attach.*` window is.
#[test]
#[ignore = "child workload for the §11.3 attach crash point"]
fn child_attach() {
    let arena = two_edge_arena();
    let view = ArenaView::new(&arena);
    for i in 0..2u32 {
        // Distinct pids, so the two registrations are distinguishable.
        view.participants().register(DEAD_PID + i, 7, 0).unwrap();
        report(std::format!("attach {i} done"));
    }
}

/// Two interns of fresh names, by a registered participant. Marker after each.
#[test]
#[ignore = "child workload for the §11.3 intern crash point"]
fn child_intern() {
    // Bare: `built_arena` already interns three names.
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
/// Flushed so a buffered-away marker cannot hide a site firing too early.
fn report(what: impl core::fmt::Display) {
    use std::io::Write as _;
    std::println!("{what}");
    let _ = std::io::stdout().flush();
}
