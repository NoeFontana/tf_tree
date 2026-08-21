//! The CLI against a **live** arena — `docs/decisions/0005` step 11.
//!
//! This is the milestone's acceptance test in the plainest sense available: a
//! publisher runs, and the shipped binary is asked to describe it. Everything
//! upstream of here is tested by code that arranges its own processes and knows
//! where the seams are. This does not — it goes through `clap`, through
//! `tf_tree::open()`, and through whatever the arena actually says.
//!
//! `tf_tree participants` gets its own test because its contract is the
//! opposite of the others': §3.3 requires it to work **without the arena**, and
//! the only way to show that is to ask it about a lock file whose segment never
//! existed.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, Tree, TreeBuilder};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("tf_tree_cli-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        std::env::set_var("TF_TREE_RUNTIME_DIR", &p);
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// **The test process is the publisher.**
///
/// A helper binary would be more ceremony for less fidelity: `CARGO_BIN_EXE_*`
/// only names bins of the *same* package, and the thing under test is the CLI
/// joining somebody else's arena — which this is, exactly. The test owns the
/// arena and serves it from its owner thread; the CLI runs as a real subprocess
/// and comes in over the socket like any other consumer.
///
/// Returned by value and held by the caller: dropping it releases the ownership
/// byte and stops the server, so it has to outlive the CLI invocations.
fn publish(_scratch: &Scratch) -> Tree {
    let tree = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64)))
                .dynamic_edge("base", "cam", EdgeCfg::new(Capacity::slots(64))),
        )
        .open()
        .expect("create the arena");

    let child = tree.frame("base").unwrap();
    let parent = tree.frame("map").unwrap();
    let w = tree.claim(child, parent).expect("claim");
    // A short run of history, so `echo` has something to interpolate between
    // and the rate check has intervals to look at.
    for i in 0..16i64 {
        w.push(
            1_000_000_000 + i * 10_000_000,
            &tf_tree_math::exp_se3([0.0, 0.0, 0.01 * i as f64, i as f64, 0.0, 0.0]),
        )
        .expect("push");
    }
    // The writer is leaked so the claim stays held for the duration: an edge
    // that reports UNCLAIMED would change what `tree` and `doctor` print.
    core::mem::forget(w);

    // **One successful lookup, so this is an arena *in service* and not merely
    // one somebody has published into.** The `docs/PHASE5.md` §5 counters are
    // incremented by lookups, so without a consumer every counter reads zero —
    // and `checks::no_counter_evidence` then correctly refuses to let `TFT010`
    // and `TFT011`'s counter half report anything, which is not the state this
    // file's `doctor` test is about. A live arena with no consumer is a real
    // state and it is covered by the fixture and `--from-bag` paths; here the
    // point is a deployment, where somebody is reading.
    tree.lookup(
        "base",
        "map",
        tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(1_000_000_000 + 15 * 10_000_000),
    )
    .expect("a lookup inside the published window");
    tree
}

fn cli(dir: &PathBuf, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tf_tree"))
        .args(args)
        .env("TF_TREE_RUNTIME_DIR", dir)
        .output()
        .expect("run tf_tree");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// **`--attach` reads the publisher's tree, not a fixture.**
///
/// The frame names are the tell. The publisher's topology is `map -> base ->
/// cam`; the in-process fixture is a mobile-robot rig with `odom`, `base_link`
/// and a laser. If `--attach` were quietly falling back to the
/// fixture — the failure mode that matters, because it prints a perfectly
/// plausible tree — the fixture's frames would be here instead.
#[test]
fn attach_shows_the_live_publishers_topology() {
    let scratch = Scratch::new("tree");
    let _pubr = publish(&scratch);

    let (ok, out) = cli(&scratch.0, &["tree", "--attach"]);
    assert!(ok, "tf_tree tree --attach failed:\n{out}");
    assert!(
        out.contains("live arena"),
        "banner still says fixture:\n{out}"
    );
    assert!(out.contains("map"), "no `map` frame:\n{out}");
    assert!(out.contains("base"), "no `base` frame:\n{out}");
    assert!(
        !out.contains("base_link"),
        "this is the in-process fixture, not the live arena:\n{out}"
    );
}

/// A lookup through the shipped binary must return the publisher's transform.
#[test]
fn echo_attaches_and_resolves() {
    let scratch = Scratch::new("echo");
    let _pubr = publish(&scratch);

    let (ok, out) = cli(&scratch.0, &["echo", "map", "base", "--attach"]);
    assert!(ok, "tf_tree echo --attach failed:\n{out}");
    assert!(
        out.contains("q=[") && !out.contains("error:"),
        "echo did not resolve against the live arena:\n{out}"
    );
}

/// **`doctor` must not claim a clean bill of health it did not earn.**
///
/// A live arena has no recorded push stream, and the two checks that depend on
/// one degrade differently — which is why the report has two ways of saying so.
/// `TFT001` loses its only evidence (a ring remembers the current claim owner,
/// not the sequence of writers) and is reported *not run*. `TFT011` keeps its
/// counter evidence — `publish` performs a lookup, so this arena has been read
/// as well as written — and loses only the capacity-vs-latency half, so it runs
/// and carries a note. A bare `pass` on either would be the lie by omission this
/// asserts against.
///
/// **The lookup in `publish` is load-bearing for that second half.** Without it
/// the counters are all zero, `checks::no_counter_evidence` refuses both halves,
/// and `TFT011` skips rather than disclosing — a different, also-correct
/// outcome, and not the one this test is about.
///
/// **And `TFT019` needs a third thing said, because an attach is not a source
/// it can answer from.** A reader who meets a silent `TFT019` here must not read
/// it as "the clock did not step" — the skip line itself is the only place that
/// can be corrected, since `docs/` is not what an operator has open at 3 a.m.,
/// and what it has to carry is the source that *can* answer:
/// `doctor --from-bag`. This is the end-to-end half of the unit assertion in
/// `checks::tests::tft019_inherits_tft018s_replayed_stream_skip` — that one pins
/// the string, this one pins that it survives into the printed report.
///
/// Mutant: return `Vec::new()` from `evidence_notes`. Applied: the
/// `TFT011` note assertion fails while the rest still passes, which is the
/// half-blind case going unreported.
/// Mutant B: delete the `--from-bag` sentence from
/// `PushStream::RingsUnderWriter`'s `no_rejected_arrivals` reason. Applied: the
/// `--from-bag` assertion fails — the redirection is stated in the check and
/// lost on the way to the operator.
#[test]
fn doctor_names_the_checks_it_cannot_run_on_a_live_arena() {
    let scratch = Scratch::new("doctor");
    let _pubr = publish(&scratch);

    let (_ok, out) = cli(&scratch.0, &["doctor", "--attach"]);
    assert!(
        out.contains("live arena"),
        "banner still says fixture:\n{out}"
    );
    assert!(
        out.contains("not run:"),
        "doctor did not disclose its blind checks:\n{out}"
    );
    assert!(
        out.contains("TFT001"),
        "doctor did not name the check that lost all its evidence:\n{out}"
    );
    assert!(
        out.contains("note: TFT011 ran on its counter evidence only"),
        "doctor did not disclose the half-blind check:\n{out}"
    );
    assert!(
        out.contains("instance "),
        "doctor did not report which arena instance it looked at:\n{out}"
    );
    // TFT018/TFT019 have no verdict on an attach, so the skip has to send the
    // operator to the source that does rather than let its silence read as an
    // all-clear.
    let not_run_reasons = out.split("not run:").nth(1).unwrap_or("");
    assert!(
        not_run_reasons.contains("--from-bag"),
        "TFT019's skip must reach the operator naming the source that can answer:\n{out}"
    );
    // `TFT014` resolves a claim through the *shared* arena's participant
    // table, which is the case a single-process test cannot reach. A leaked
    // claim would be a finding; the publisher is alive, so it must be silent.
    let not_run = out.split("not run:").nth(1).unwrap_or("");
    assert!(
        !not_run.contains("TFT014"),
        "TFT014 must run against a real participant table:\n{out}"
    );
    assert!(
        !out.contains("TFT014  participant"),
        "a live publisher's claim was reported as leaked:\n{out}"
    );
}

/// **`participants` must work with no arena at all** (§3.3).
///
/// The lock file is the source of truth about who is attached, and it is a
/// separate file precisely so that it survives a segment this build cannot map:
/// a format-version mismatch, a layout-hash mismatch, a wedged owner. Those are
/// the moments somebody reaches for a diagnostic tool, so this is the command
/// that must not need the thing that is broken.
#[test]
fn participants_lists_a_live_publisher() {
    let scratch = Scratch::new("participants");
    let _pubr = publish(&scratch);

    let (ok, out) = cli(&scratch.0, &["participants"]);
    assert!(ok, "tf_tree participants failed:\n{out}");
    assert!(out.contains("live"), "no live participant listed:\n{out}");
    assert!(
        out.contains("rw"),
        "the publisher attached read-write; that is not shown:\n{out}"
    );
}

/// Nothing running is an *answer*, not a failure.
///
/// Exiting non-zero here would make "no publisher" indistinguishable from "the
/// tool could not look", which is the distinction an operator is running it to
/// find out.
#[test]
fn participants_on_an_empty_machine_says_so_and_succeeds() {
    let scratch = Scratch::new("empty");
    let (ok, out) = cli(&scratch.0, &["participants"]);
    assert!(ok, "an empty machine must not be an error:\n{out}");
    assert!(
        out.contains("no lock file"),
        "did not say the machine is empty:\n{out}"
    );
}

/// A wrong `--domain` must report *nothing there*, not a stale snapshot of
/// something else.
///
/// This is the mistake an operator actually makes, and the dangerous version of
/// it is silent: attaching to domain 7 and being shown domain 0's tree looks
/// exactly like a working system.
#[test]
fn a_different_domain_is_a_different_arena() {
    let scratch = Scratch::new("domain");
    let _pubr = publish(&scratch);

    let (ok, out) = cli(&scratch.0, &["participants", "--domain", "7"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("no lock file"),
        "domain 7 reported something; the domains are not isolated:\n{out}"
    );

    let (ok, out) = cli(&scratch.0, &["tree", "--attach", "--domain", "7"]);
    assert!(!ok, "attaching to an empty domain must fail:\n{out}");
}

/// `tf_tree top` against a live arena, including the observer's own row.
///
/// Two properties that only a real attach can show:
///
/// 1. **The read-only observer is visible and is marked as one.** A read-only
///    participant holds a lock-file byte and writes *no* arena participant
///    record (`Tree::participant_slot` returns `u32::MAX`), so a participant
///    pane built from the arena table alone would show only the publisher — and
///    `top` would be invisible in its own output while sitting in the table's
///    capacity.
/// 2. **It says it is a read-only observer**, which is the claim the rest of
///    the pane is asking to be believed.
///
/// Mutant: replace `cmd_top`'s lock-file `merge` closure with the no-op one
/// used on the non-attach path. Applied: no `ro` row and no `record=no` row
/// exist, so the `mode ro` assertion fails while the frame otherwise renders
/// perfectly — exactly the silent half-picture this asserts against.
#[test]
fn top_shows_the_live_arena_and_its_own_read_only_row() {
    let scratch = Scratch::new("top");
    let _pubr = publish(&scratch);

    let (ok, out) = cli(
        &scratch.0,
        &["top", "--attach", "--iterations", "2", "--interval", "50"],
    );
    assert!(ok, "tf_tree top --attach failed:\n{out}");
    assert_eq!(
        out.matches("tf_tree top").count(),
        2,
        "not two frames:\n{out}"
    );
    assert!(out.contains("live arena"), "banner says fixture:\n{out}");
    assert!(out.contains("read-only observer"), "{out}");
    assert!(
        out.contains("no arena participant record"),
        "the observer did not disclose how it is attached:\n{out}"
    );
    // The publisher's topology, not the in-process fixture's.
    assert!(out.contains("map->base"), "no live edge:\n{out}");
    assert!(
        !out.contains("base_link"),
        "this is the fixture, not the live arena:\n{out}"
    );
    // The observer's own lock-file row: read-only, and with no arena record.
    let pane = out
        .split("participants")
        .nth(1)
        .expect("no participants pane");
    let ro_rows: Vec<&str> = pane
        .lines()
        .filter(|l| l.split_whitespace().nth(2) == Some("ro"))
        .collect();
    assert!(!ro_rows.is_empty(), "no read-only participant row:\n{pane}");
    assert!(
        ro_rows.iter().any(|l| l.contains(" no ")),
        "the read-only row claims an arena record it cannot have:\n{ro_rows:?}"
    );
}

/// **`top` refuses `--rw` rather than quietly downgrading it.**
///
/// D18 is why a diagnostic tool maps `PROT_READ`: the MMU is what stops a bug
/// in this binary from corrupting a robot's transform tree. `--rw` is a global
/// flag, so `tf_tree --rw top` parses; accepting it would put the longest-lived
/// diagnostic process on the robot inside the blast radius the mapping exists
/// to define.
///
/// Mutant: delete the `anyhow::ensure!(!live.rw, ...)` in `cmd_top`. Applied:
/// the command exits 0 and both assertions fail.
#[test]
fn top_refuses_a_read_write_attach() {
    let scratch = Scratch::new("top-rw");
    let _pubr = publish(&scratch);

    let out = Command::new(env!("CARGO_BIN_EXE_tf_tree"))
        .args(["top", "--attach", "--rw", "--iterations", "1"])
        .env("TF_TREE_RUNTIME_DIR", &scratch.0)
        .output()
        .expect("run tf_tree");
    assert!(!out.status.success(), "--rw was accepted");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("read-only observer") && err.contains("--rw"),
        "unhelpful refusal: {err}"
    );
}

// ---------------------------------------------------------------------------
// `TFT014`'s participant half — `docs/decisions/0028` plan step 6
// ---------------------------------------------------------------------------

/// The lock file of the arena [`publish`] created, as a **second** open file
/// description.
///
/// Second, and that matters twice over: `F_OFD_SETLK` conflicts are per
/// description, so a byte taken here is invisible to this process's own
/// `Session` and visible to the CLI subprocess — which is exactly the shape a
/// separate participant has, without needing a separate process to have it.
fn lock_of(scratch: &Scratch) -> tf_tree_ipc::LockFile {
    let rv = tf_tree_ipc::Rendezvous::from_env().expect("the scratch runtime dir is in the env");
    assert_eq!(
        rv.runtime_dir().path(),
        scratch.0.as_path(),
        "the rendezvous must resolve to this test's scratch dir, not a real one"
    );
    tf_tree_ipc::LockFile::open(rv.lock_path()).expect("the publisher created the lock file")
}

/// A 32-byte `comm` field.
fn comm(name: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = name.len().min(out.len());
    out[..n].copy_from_slice(&name.as_bytes()[..n]);
    out
}

/// Run `doctor --json --attach` and return stdout.
fn doctor_json(scratch: &Scratch) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_tf_tree"))
        .args(["doctor", "--attach", "--json"])
        .env("TF_TREE_RUNTIME_DIR", &scratch.0)
        .output()
        .expect("run tf_tree");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The `checks[]` entry for `id`, as a slice long enough to hold its findings.
fn check_of<'a>(json: &'a str, id: &str) -> &'a str {
    let at = json
        .find(&format!("\"id\": \"{id}\""))
        .unwrap_or_else(|| panic!("{id} missing:\n{json}"));
    let end = json[at..]
        .find("\n    }")
        .map_or(json.len(), |rel| at + rel);
    &json[at..end]
}

/// **(a): a `LIVE` record over a free lock byte, through `doctor --json`.**
///
/// `docs/decisions/0028` plan step 6's first clause, on a real arena with a
/// real rendezvous. The unit tests in `checks.rs` hand [`checks::slot_leak`] a
/// `ParticipantInfo` and prove the truth table; this proves the *wiring* — that
/// `cmd_doctor` opens the lock file at all, that `probe_lock_facts` reaches the
/// check, and that the finding comes out of the shipped `--json` document.
/// Before step 6 `doctor` never opened a lock file, so no amount of unit
/// testing could have shown this.
///
/// **What is staged and what is real.** `register_at` publishes the record the
/// same way `register` does, so what is staged is a *complete* registration
/// whose process then died — and the byte is simply never taken, which is what
/// death does to it. The identity record beside it is written first, which is
/// §3.3's order.
///
/// **The pid is `u32::MAX` because that is the deterministic form of "gone"**:
/// it exceeds every `pid_max`, so `/proc/<pid>` cannot exist and the classifier
/// takes its no-entry branch without racing pid reuse to get there. The
/// `start_time` is non-zero so the record is comparable at all — a zero one is
/// `of_self_best_effort`'s "could not read", which is deliberately *not*
/// evidence of death.
///
/// The healthy half of the same arena is asserted too, or this test would pass
/// for a check that reports every slot: slot 0 is the running publisher, with a
/// held byte and a live pid, and it must produce nothing.
///
/// Mutant: return `None` from `slot_leak`'s
/// `(LockByte::Free, Gone | Unknown)` arm. Applied: this failed at
/// *"TFT014 did not fire on a LIVE record over a free byte"* with
/// `"status": "pass"` — `doctor` reporting an all-clear on the arena issue #184
/// is named for.
#[test]
fn doctor_json_reports_a_stale_live_record_as_an_abandoned_slot() {
    const GONE: u32 = u32::MAX;
    const SLOT: u32 = 5;

    let scratch = Scratch::new("tft014-abandoned");
    let pubr = publish(&scratch);

    lock_of(&scratch)
        .write_identity(
            SLOT,
            &tf_tree_ipc::Identity {
                pid: GONE,
                start_time: 4242,
                boot_id: [0u8; 16],
                mode: tf_tree_ipc::AccessMode::ReadWrite,
                name: comm("a-writer-that-died"),
            },
        )
        .expect("write the identity record");
    pubr.arena_view()
        .participants()
        .register_at(SLOT, GONE, 4242, 0)
        .expect("slot 5 of a 64-slot table is free");

    let json = doctor_json(&scratch);
    let check = check_of(&json, "TFT014");
    assert!(
        check.contains("\"status\": \"fired\""),
        "TFT014 did not fire on a LIVE record over a free byte:\n{check}"
    );
    assert!(
        check.contains(&format!(
            "\"subject\": \"slot {SLOT} pid {GONE}, byte free\""
        )),
        "the finding must name the slot, the pid an operator has to look for, and \
         which of the two TFT014 shapes this is:\n{check}"
    );
    assert!(
        check.contains("the lock byte is free, and /proc has no running process for it"),
        "the finding must say which two facts it rests on:\n{check}"
    );
    assert!(
        !check.contains("forked child"),
        "a free byte is not the fork case, and conflating them sends an operator \
         after the wrong fault:\n{check}"
    );
    assert!(
        !check.contains("slot 0 pid"),
        "the running publisher's own slot was reported as leaked:\n{check}"
    );
}

/// **(b): a held byte over a dead pid — the fork case, named as itself.**
///
/// `docs/decisions/0028` plan step 6's second clause. This is the one state
/// D17's socket cannot see: a forked child keeps the parent's open file
/// descriptions alive, so the owner's `epoll` never reports `HUP` **and** the
/// lock byte stays held, and the kernel's own answer for the slot is therefore
/// *alive* about a process that no longer exists. Nothing may reclaim it —
/// overruling the kernel with a `/proc` guess is the inversion `PHASE2.md` §5.1
/// forbids — so a distinct message is the entire remedy this build has, and
/// `0030` is where the fd inheritance itself gets closed.
///
/// **A real `fork` is not needed to stage it and would be worse if it were.**
/// The state under test is *byte held by somebody, recorded pid gone*, and a
/// second open file description in this process holds a byte exactly the way an
/// inherited one does — the kernel cannot tell them apart, which is the whole
/// premise of §6.2. Forking would add a child whose lifetime the test would
/// then have to manage, to observe the same two facts.
///
/// Mutant: drop `cmd_doctor`'s `snap.probe_lock_facts(..)` call, so `doctor`
/// opens the lock file and throws the answer away. Applied: 9 passed, 3 failed
/// — this test, `doctor_json_reports_a_stale_live_record_as_an_abandoned_slot`
/// and `doctor_json_reports_a_read_only_fork_inheritor_with_no_arena_record`,
/// while `doctor_is_silent_about_a_joiner_that_is_mid_attach` passes. Which is
/// why the wiring is pinned by the three positives and not by the negative.
/// Mutant B: make `recorded_given` answer `Unknown` for a `NotFound` read
/// rather than consulting `proc_answers` — i.e. never let `/proc` prove a
/// death. Applied: the same three fail, 9 passed — the fork arm loses its only
/// evidence and both fork tests go silent.
#[test]
fn doctor_json_reports_a_held_byte_over_a_dead_pid_as_a_fork_inheritor() {
    const GONE: u32 = u32::MAX;
    const SLOT: u32 = 6;

    let scratch = Scratch::new("tft014-fork");
    let pubr = publish(&scratch);

    let lock = lock_of(&scratch);
    lock.write_identity(
        SLOT,
        &tf_tree_ipc::Identity {
            pid: GONE,
            start_time: 4242,
            boot_id: [0u8; 16],
            mode: tf_tree_ipc::AccessMode::ReadWrite,
            name: comm("a-parent-that-forked"),
        },
    )
    .expect("write the identity record");
    // The byte the inheritor is holding on the dead parent's behalf.
    assert_eq!(
        lock.try_take_participant(SLOT).expect("take the byte"),
        tf_tree_ipc::LockAttempt::Acquired
    );
    pubr.arena_view()
        .participants()
        .register_at(SLOT, GONE, 4242, 0)
        .expect("slot 6 of a 64-slot table is free");

    let json = doctor_json(&scratch);
    let check = check_of(&json, "TFT014");
    assert!(
        check.contains("\"status\": \"fired\""),
        "TFT014 stayed silent about a slot held for a process that is gone:\n{check}"
    );
    assert!(
        check.contains("forked child inherited it"),
        "the fork case must be named as itself:\n{check}"
    );
    assert!(
        check.contains("`spawn`"),
        "the remedy is a start method and the message has to name it:\n{check}"
    );
    assert!(
        !check.contains("the lock byte is free"),
        "the byte is held; reporting it as free sends an operator hunting a \
         reaper that would be wrong to run:\n{check}"
    );
    assert!(
        check.contains(&format!(
            "\"subject\": \"slot {SLOT} pid {GONE}, byte still HELD\""
        )),
        "the two shapes must be separable from the subject alone:\n{check}"
    );
}

/// **(c): the read-only participant's fork inheritor — no arena record at
/// all.**
///
/// The shape `docs/RUNBOOK.md`'s *"the tree works in the parent and everything
/// fails in a forked child"* paragraph sends an operator here for, and the one
/// the first revision of this check reported as `"status": "pass"`. D18 makes
/// read-only the consumer default and a read-only participant writes **no**
/// arena participant record — it takes a lock byte, writes the lock-file
/// identity beside it, and leaves the arena table `FREE` (the same fact
/// `top_shows_the_live_arena_and_its_own_read_only_row` asserts from the other
/// side). `fork` such a process — Python's `multiprocessing` default on
/// Linux — and let the parent die, and the child's inherited open file
/// description holds the byte for a pid that no longer exists.
///
/// So all three of `TFT014`'s facts are present and one of them is missing:
/// byte held, recorded pid gone, and no arena record to hang either on. A
/// predicate that starts *"if the record is FREE, return None"* sees the
/// likeliest leak on a Python robot and says nothing.
///
/// **Nothing is written to the arena here, deliberately.** The two sibling
/// tests call `register_at` to stage a record; this one must not, because the
/// absence of the record is the state under test.
///
/// Mutant: restore `if p.state == SlotState::Free { return None }` in
/// `checks::slot_leak`. Applied: this failed at *"TFT014 stayed silent about a
/// read-only participant's fork inheritor"* with `"status": "pass"`, and every
/// other test in this file still passed.
#[test]
fn doctor_json_reports_a_read_only_fork_inheritor_with_no_arena_record() {
    const GONE: u32 = u32::MAX;
    const SLOT: u32 = 9;

    let scratch = Scratch::new("tft014-ro-fork");
    let _pubr = publish(&scratch);

    let lock = lock_of(&scratch);
    lock.write_identity(
        SLOT,
        &tf_tree_ipc::Identity {
            pid: GONE,
            start_time: 4242,
            boot_id: [0u8; 16],
            // Read-only: the mode that writes no arena record.
            mode: tf_tree_ipc::AccessMode::ReadOnly,
            name: comm("a-forked-consumer"),
        },
    )
    .expect("write the identity record");
    // The byte the inheritor holds on the dead parent's behalf. No
    // `register_at`: a read-only participant never wrote an arena record, so
    // slot 9's record is `FREE` and stays that way.
    assert_eq!(
        lock.try_take_participant(SLOT).expect("take the byte"),
        tf_tree_ipc::LockAttempt::Acquired
    );

    let json = doctor_json(&scratch);
    let check = check_of(&json, "TFT014");
    assert!(
        check.contains("\"status\": \"fired\""),
        "TFT014 stayed silent about a read-only participant's fork inheritor — \
         the shape RUNBOOK.md says it reports:\n{check}"
    );
    assert!(
        check.contains(&format!(
            "\"subject\": \"slot {SLOT} pid {GONE}, byte still HELD\""
        )),
        "the pid is the lock file's — there is no arena record to take one \
         from:\n{check}"
    );
    assert!(
        check.contains("forked child inherited it"),
        "the fork case must be named as itself:\n{check}"
    );
    assert!(
        check.contains("read-only participant"),
        "an operator told the record is FREE has to be told why a real leak has \
         no record:\n{check}"
    );
    assert!(
        !check.contains("slot 0 pid"),
        "the running publisher's own slot was reported:\n{check}"
    );
}

/// **The negative: a healthy joiner caught mid-attach is not a finding.**
///
/// `docs/decisions/0028` plan step 6 asks for this one by name, and it is the
/// test that stops the participant half becoming a check that always fires.
/// The state staged is `register_at`'s exactly, stopped one step short of the
/// end: §3.3's identity record, then the lock byte, then the arena record's
/// `FREE -> RESERVED` CAS — and then nothing, which is where a registrant
/// preempted inside `fill_slot` sits (`participant.rs`, between the CAS and the
/// release-store of `LIVE`). A page fault on the fresh mapping is the ordinary
/// way to get there.
///
/// This is the state the old predicate could not judge and therefore ignored:
/// `Tree::participant_alive` folds `state == LIVE` in ahead of the byte probe,
/// so it answers "not alive" here as readily as it does for a registrant that
/// died. Reporting `RESERVED` without the byte would have put a `warn` on every
/// arena a `doctor` run catches mid-attach. With the byte, both are answerable
/// and only one is a leak.
///
/// Non-vacuity: the assertion is on `TFT014` specifically having **passed**
/// rather than on the absence of a string, so a run in which the check silently
/// stopped running would fail it too.
///
/// Mutant: widen `slot_leak`'s fork arm to `(LockByte::Held, _)`. Applied: this
/// failed with `"status": "fired"` and a fork-inheritor finding about the
/// healthy registrant — the check having become one that fires on every arena
/// caught mid-attach, which is exactly what this test exists to stop. It takes
/// three others down with it (8 passed, 4 failed), including
/// `doctor_names_the_checks_it_cannot_run_on_a_live_arena`, because under that
/// arm the `doctor` process's own held byte is a finding about itself.
#[test]
fn doctor_is_silent_about_a_joiner_that_is_mid_attach() {
    const SLOT: u32 = 7;

    let scratch = Scratch::new("tft014-mid-attach");
    let pubr = publish(&scratch);

    let lock = lock_of(&scratch);
    lock.write_identity(
        SLOT,
        &tf_tree_ipc::Identity::of_self_best_effort(tf_tree_ipc::AccessMode::ReadWrite),
    )
    .expect("write the identity record");
    assert_eq!(
        lock.try_take_participant(SLOT).expect("take the byte"),
        tf_tree_ipc::LockAttempt::Acquired
    );
    let view = pubr.arena_view();
    let rec = view
        .participants()
        .get(SLOT)
        .expect("slot 7 of a 64-slot table exists");
    rec.state.store(
        tf_tree_core::participant::RESERVED,
        std::sync::atomic::Ordering::Release,
    );

    let json = doctor_json(&scratch);
    let check = check_of(&json, "TFT014");
    assert!(
        check.contains("\"status\": \"pass\""),
        "a healthy joiner mid-attach was reported as a leak — this is the check \
         becoming one that always fires:\n{check}"
    );
}
