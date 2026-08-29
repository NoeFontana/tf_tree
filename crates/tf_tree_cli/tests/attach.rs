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

/// **The `age(ms)` column is measured against a real clock, not a benchmark
/// constant.**
///
/// It was `(fixture::NOW_NS - newest).max(0)`, and `fixture::NOW_NS` is
/// `9_900_000_000` — the in-process benchmark rig's synthetic "now". Against a
/// live arena that number is arbitrary: this publisher stamps around 1.0–1.15 s,
/// so the column reported **~8 750 ms of age for a transform pushed
/// milliseconds ago**, and against a robot stamping Unix nanoseconds the
/// subtraction clamps and every edge reads `0` however long its publisher has
/// been dead. Either way the number was about the fixture rather than the arena,
/// which is worse than an empty column: it is a plausible one.
///
/// `Clock::decide` is the estimator `doctor` and `top` already share, and the
/// header now discloses which clock it picked — the same disclosure discipline
/// the rest of the report follows.
///
/// **Mutant:** put `fixture::NOW_NS` back. The `8_7` assertion fires, because
/// that is the shipped output.
#[test]
fn the_age_column_is_measured_against_a_real_clock() {
    let scratch = Scratch::new("age");
    let _pubr = publish(&scratch);

    let (ok, out) = cli(&scratch.0, &["tree", "--attach"]);
    assert!(ok, "tf_tree tree --attach failed:\n{out}");

    let row = out
        .lines()
        .find(|l| l.contains("base") && !l.contains("cam") && l.contains("dynamic"))
        .unwrap_or_else(|| panic!("no dynamic `base` row:\n{out}"));
    let age: i64 = row
        .split_whitespace()
        .rev()
        .nth(2)
        .and_then(|f| f.parse().ok())
        .unwrap_or_else(|| panic!("no parsable age in `{row}`:\n{out}"));

    // The publisher pushed its newest sample at 1.15 s and nothing else has
    // written since, so against any honest reference clock this edge is the
    // newest thing in the arena and its age is small. `fixture::NOW_NS` put it
    // at ~8 750.
    assert!(
        age < 1_000,
        "age {age} ms for the newest edge in the arena — the column is measured \
         against something other than this arena's own clock:\n{out}"
    );
    assert!(
        out.contains("age(ms) is measured against the"),
        "the column must say which clock it used, as every other derived number \
         in this tool does:\n{out}"
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

/// A 16-byte `comm` field.
///
/// **Sixteen since `docs/decisions/0033`, and the fixtures below had to shrink
/// their names for a reason the size change does not show.** Written at offset
/// 32, the old 17-to-20-byte literals were the only thing in this repository
/// that put nonzero bytes in `48..56` — precisely the range that is now
/// `pid_ns_inode`. Left alone they would have handed the zero-means-unknown
/// compatibility path a fabricated namespace, and every assertion below would
/// then hold or fail for a reason unrelated to what it is pinning. The kernel
/// caps a real `comm` at 15 bytes, so a fixture that does not fit here is a
/// fixture no process could have written.
fn comm(name: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    assert!(
        name.len() < out.len(),
        "a fixture name the kernel could not have produced: {name}"
    );
    out[..name.len()].copy_from_slice(name.as_bytes());
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
                name: comm("writer-died"),
                // A **pre-`0033`** record: zero is "unknown namespace", which
                // means keep the behaviour this test was written against. The
                // arms where the field is set are `tft014_namespace_*`, below.
                pid_ns_inode: 0,
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
            name: comm("parent-forked"),
            pid_ns_inode: 0,
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
            name: comm("forked-consumer"),
            pid_ns_inode: 0,
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

// ---------------------------------------------------------------------------
// `docs/decisions/0033` plan step 1 — the four arms of the namespace false
// positive.
//
// `TFT014` calls a healthy participant in another PID namespace a *fork
// inheritor* and tells the operator to stop it. The four arms below are the
// four ways an observer and a participant can disagree about what a pid number
// means, and they are lettered as `0033` letters them:
//
//   A  a namespaced participant seen from the host      `Ok(_) => Gone`
//   B  a host participant seen from a container         `ENOENT => Gone`
//   C  a genuine surviving fork inheritor               `ENOENT => Gone`
//   D  participant and observer inside one bare `unshare --fork --pid`
//
// **C is the true positive and must keep firing.** A and C render
// byte-identical findings — 1092 bytes each once the slot number and the
// interpolated pid are normalised — so *which arm the classifier took carries
// no information about which fault is present*, and nothing below may assert on
// it. Every assertion here is on the rendered evidence in the `--json`
// document, which is what an operator and a script both read.
//
// A, B and C are staged through the lock file, the way the three `TFT014` tests
// above are staged and for the reason
// `doctor_json_reports_a_held_byte_over_a_dead_pid_as_a_fork_inheritor` gives:
// the state under test is a set of bytes in a file plus a byte held by some
// open file description, and the kernel cannot tell a second description in
// this process from an inherited one. What a namespace adds to that is one
// `u64` in the record and one in the observer, and only the second of those
// needs a real namespace — which is arm D, and arm D is therefore the one
// staged with a real `unshare`.
// ---------------------------------------------------------------------------

/// An nsfs inode that is not this process's.
///
/// Adjacent to our own on purpose: nsfs inums come from one allocator, and the
/// two `0033` measured differ by 652. Picking `1` would test a comparison no
/// kernel can produce. It does not matter whether some *other* live namespace
/// happens to own this number — the only comparison the guard makes is against
/// the observer's own, and this differs from that by construction.
fn a_foreign_pid_ns() -> u64 {
    own_pid_ns() + 1
}

fn own_pid_ns() -> u64 {
    let ino = tf_tree_ipc::self_pid_ns_inode()
        .expect("/proc/self/ns/pid must be readable to stage a namespace arm");
    assert_ne!(ino, 0, "zero is the record's `unknown namespace` marker");
    ino
}

/// Stage the **two** slot shapes `TFT014` can accuse, both with a held byte and
/// the same recorded identity.
///
/// Both, and not one, because they reach the verdict down different lines and a
/// fix that handles only the second passes a test that stages only the second:
///
/// * `arena` — an arena participant record that is **not** `FREE`, which is the
///   main match's `(LockByte::Held, RecordedProcess::Gone)` arm in
///   `checks::slot_leak`. This is the shape `0033`'s arm B accused *first*, and
///   the slot it accused was the arena's read-write owner.
/// * `bare` — no arena record at all, which is `slot_leak`'s `SlotState::Free`
///   early return. D18's read-only consumer, and the likeliest fork leak on a
///   Python deployment.
///
/// **The returned [`tf_tree_ipc::LockFile`] is the held byte and has to be kept
/// alive by the caller**: an OFD lock belongs to the open file description, so
/// dropping it releases both bytes and turns every arm below into the *byte
/// free* shape — which fires `TFT014` for a different reason and would let arm
/// A and arm B pass with no guard in the binary at all.
fn stage_two_accusable_slots(
    scratch: &Scratch,
    pubr: &Tree,
    id: &tf_tree_ipc::Identity,
    arena: u32,
    bare: u32,
) -> tf_tree_ipc::LockFile {
    let lock = lock_of(scratch);
    for slot in [arena, bare] {
        lock.write_identity(slot, id)
            .expect("write the identity record");
        assert_eq!(
            lock.try_take_participant(slot).expect("take the byte"),
            tf_tree_ipc::LockAttempt::Acquired,
            "slot {slot} of a 64-slot table is free"
        );
    }
    pubr.arena_view()
        .participants()
        .register_at(arena, id.pid, id.start_time, 0)
        .expect("the arena half of the non-FREE shape");
    // This process's own second open file description holds both bytes, which
    // is what an inherited one looks like to the kernel — the premise §6.2
    // rests on, and the reason the three `TFT014` tests above stage a fork
    // without forking.
    lock
}

/// Both accusable shapes, from the `--json` document, as an operator reads them.
fn tft014_names_slot(check: &str, slot: u32) -> bool {
    check.contains(&format!("slot {slot} pid "))
}

/// **Arm A: a live participant one PID namespace away, seen from the host.**
///
/// The recorded pid *exists here* — inside `unshare -U --fork --pid` a
/// participant is pid 1, and pid 1 on the host is `systemd` — with a completely
/// different start time. So the probe succeeds, `Ok(_) => Gone` fires, and
/// `TFT014` reports a running process as a forked child's leftovers with the
/// remediation *stop the child*. `0033`'s *Context* stages exactly this and
/// quotes the finding.
///
/// The stored start time is read from pid 1 and moved by one rather than
/// invented, because the arm under test is *"the number is in use and the start
/// time differs"* and a fabricated constant could collide.
///
/// **The observer stands on the host, and that is a constraint rather than a
/// convenience** (`0033` plan step 1). Moving it inside the namespace turns
/// this into arm D, which the recorded-namespace guard alone does not silence —
/// so a version of this test written that way would report the fix as not
/// working, and then, once both guards landed, pass for a reason that has
/// nothing to do with what it pins.
///
/// Mutant: delete the recorded-namespace guard from `recorded_given`, leaving
/// only `0033`'s second one. Applied: this failed at the first assertion, on a
/// `"status": "fired"` document carrying **two** findings — *"slot 21 pid 1,
/// byte still HELD … The record is LIVE"* and *"slot 22 pid 1 … The record is
/// FREE"* — which is both of `checks::slot_leak`'s routes to `ForkInheritor`
/// firing on one healthy participant, with the *stop the child* remedy.
#[test]
fn tft014_namespace_arm_a_a_namespaced_participant_is_not_a_fork_inheritor() {
    /// The non-`FREE` shape: `checks::slot_leak`'s main match.
    const ARENA: u32 = 21;
    /// The `FREE`-record shape: its `SlotState::Free` early return.
    const BARE: u32 = 22;
    let scratch = Scratch::new("tft014-ns-arm-a");
    let pubr = publish(&scratch);

    let Ok(init_start) = tf_tree_ipc::start_time_of(1) else {
        panic!("/proc/1/stat is unreadable, so arm A's `Ok(_)` arm cannot be staged here");
    };
    let _held = stage_two_accusable_slots(
        &scratch,
        &pubr,
        &tf_tree_ipc::Identity {
            // Namespace-local. Here it names init.
            pid: 1,
            start_time: init_start + 1,
            boot_id: [0u8; 16],
            mode: tf_tree_ipc::AccessMode::ReadWrite,
            name: comm("namespaced"),
            pid_ns_inode: a_foreign_pid_ns(),
        },
        ARENA,
        BARE,
    );

    let json = doctor_json(&scratch);
    let check = check_of(&json, "TFT014");
    assert!(
        !tft014_names_slot(check, ARENA),
        "a live participant in another namespace was reported as a leak — and \
         the slot is the non-FREE shape, which is the one an arm-B fix that \
         only handles `FREE` records walks past:\n{check}"
    );
    assert!(
        !tft014_names_slot(check, BARE),
        "the same, for the read-only shape with no arena record:\n{check}"
    );
}

/// **Arm B: a host participant seen from another PID namespace — the mirror,
/// and it takes the other arm.**
///
/// The recorded pid is not in the observer's `/proc` at all, so this reaches
/// `Gone` through `ENOENT` while arm A reaches it through `Ok(_)`. That is the
/// measurement behind `0033` *Decision* 3's placement: a guard written as an
/// arm ahead of `Ok(_)` silences A and leaves B firing, and a guard written at
/// the `ENOENT` arm silences arm C, which is the one true positive this check
/// exists for. Only a guard before the whole `match probe` covers both and
/// neither.
///
/// `u32::MAX` is the deterministic form of "not in this `/proc`" — it exceeds
/// every `pid_max`, so the classifier reaches its no-entry branch without
/// racing pid reuse to get there.
///
/// **What is staged here and what `0033` staged.** The record ran a container
/// `doctor` over a bind-mounted runtime dir and the first slot it accused was
/// the arena's read-write **owner**, through the non-`FREE`
/// `(LockByte::Held, Gone)` arm. This process is the owner and cannot move
/// itself into a second namespace, so the non-`FREE` shape is staged beside the
/// bare one instead: same line in `checks::slot_leak`, same verdict, one slot
/// over. The real container run is arm B in `0033`'s own staging scripts.
///
/// Mutant: the one on arm C — write the guard as an arm ahead of
/// `Ok(_) => Gone`. Applied: **this** is the only one of the four that fails,
/// which is what makes A and B two tests rather than one.
#[test]
fn tft014_namespace_arm_b_a_host_participant_seen_from_elsewhere_is_not_one_either() {
    /// The non-`FREE` shape: `checks::slot_leak`'s main match.
    const ARENA: u32 = 23;
    /// The `FREE`-record shape: its `SlotState::Free` early return.
    const BARE: u32 = 24;
    const GONE: u32 = u32::MAX;

    let scratch = Scratch::new("tft014-ns-arm-b");
    let pubr = publish(&scratch);

    let _held = stage_two_accusable_slots(
        &scratch,
        &pubr,
        &tf_tree_ipc::Identity {
            pid: GONE,
            start_time: 4242,
            boot_id: [0u8; 16],
            mode: tf_tree_ipc::AccessMode::ReadWrite,
            name: comm("host-side"),
            pid_ns_inode: a_foreign_pid_ns(),
        },
        ARENA,
        BARE,
    );

    let json = doctor_json(&scratch);
    let check = check_of(&json, "TFT014");
    assert!(
        !tft014_names_slot(check, ARENA),
        "the non-FREE shape: a pid this /proc does not number is not a pid this \
         /proc can call gone:\n{check}"
    );
    assert!(
        !tft014_names_slot(check, BARE),
        "the same, for the FREE-record shape:\n{check}"
    );
}

/// **Arm C: the true positive, and the reason none of this may be written at an
/// arm.**
///
/// A genuine surviving fork inheritor: the byte is held for a process that
/// really is gone, in the observer's **own** PID namespace. Byte for byte this
/// is arm B with one `u64` changed, it takes the same `ENOENT` branch, and
/// `0033` measured the two findings as byte-identical text once the slot and
/// pid are normalised. So this is the row that decides whether the fix
/// discriminates or merely silences: it must fire after every step of `0033`,
/// and it fires here on both accusable shapes.
///
/// Mutant: express the recorded-namespace guard as an arm ahead of
/// `Ok(_) => Gone` instead of before the whole `match probe` — i.e. leave the
/// `ENOENT` branch alone. Applied: 3 passed, 1 failed — this test passes, arm A
/// passes, arm D passes, and **arm B fails**, because B reaches `Gone` through
/// `ENOENT` and an arm ahead of `Ok(_)` never sees it. That is `0033`
/// *Decision* 3's placement argument as a measurement rather than a sentence,
/// and it is why A, B and C are three tests and not one.
#[test]
fn tft014_namespace_arm_c_a_real_fork_inheritor_in_this_namespace_still_fires() {
    /// The non-`FREE` shape: `checks::slot_leak`'s main match.
    const ARENA: u32 = 25;
    /// The `FREE`-record shape: its `SlotState::Free` early return.
    const BARE: u32 = 26;
    const GONE: u32 = u32::MAX;

    let scratch = Scratch::new("tft014-ns-arm-c");
    let pubr = publish(&scratch);

    let _held = stage_two_accusable_slots(
        &scratch,
        &pubr,
        &tf_tree_ipc::Identity {
            pid: GONE,
            start_time: 4242,
            boot_id: [0u8; 16],
            mode: tf_tree_ipc::AccessMode::ReadWrite,
            name: comm("forked-here"),
            // The observer's own. Nothing else separates this from arm B.
            pid_ns_inode: own_pid_ns(),
        },
        ARENA,
        BARE,
    );

    let json = doctor_json(&scratch);
    let check = check_of(&json, "TFT014");
    assert!(
        check.contains("\"status\": \"fired\""),
        "the namespace guards silenced the fault TFT014 exists for:\n{check}"
    );
    assert!(
        tft014_names_slot(check, ARENA),
        "the non-FREE shape must still be reported:\n{check}"
    );
    assert!(
        tft014_names_slot(check, BARE),
        "so must the read-only shape, which is the likeliest one on a Python \
         deployment:\n{check}"
    );
    assert!(
        check.contains("forked child inherited it"),
        "and it must still be named as the fork case, with the `spawn` \
         remedy:\n{check}"
    );
}

/// **Arm D: participant and observer inside one bare `unshare --fork --pid`,
/// where `doctor` accuses its own slot.**
///
/// The arm the recorded-namespace guard is *structurally blind* to, and the
/// only one here that needs a real namespace. Every process in this staging is
/// in the same PID namespace, so every recorded inode equals the observer's own
/// and that guard never fires — while the pid each record carries came from
/// `std::process::id()` and is namespace-local, and the `/proc` that
/// `start_time_of` resolves it against is still the parent's. The two are drawn
/// from different numberings and the first guard compares neither of them.
///
/// So `doctor` reads the record **it wrote at attach**, decides the process
/// named in it is gone, and prints the *stop the child* remediation about
/// itself. `0033` measured that: slot 1 of that run is `doctor`'s own
/// participant slot, pid 8, `(the record is FREE — a read-only participant,
/// D18)`. The second guard is `readlink("/proc/self")` against `getpid()`,
/// which disagree exactly when `/proc` is not this namespace's.
///
/// **A container is not this shape.** A real runtime remounts `/proc`, so
/// `readlink /proc/self` there matches its pid 1 and this guard is silent —
/// which is why arm B is the container arm and this one is bare `unshare`.
///
/// Mutant: delete the `proc_is_ours` guard, leaving only the recorded-namespace
/// one. Applied: A, B and C pass and **this fails**, on
/// *"slot 1 pid 1, byte still HELD … The record is FREE (no arena record: a
/// read-only participant, D18)"* — `doctor` inside the namespace is pid 1, it
/// took slot 1 at attach, and slot 1 is the slot it accused. The converse
/// mutant, deleting the recorded-namespace guard and keeping this one, passes
/// this and fails A and B. Neither guard carries the other's arms, which is the
/// measurement `0033` plan step 4b asks for.
///
/// **Skips loudly**, in the shape `just lint`'s `py-compile` skip has: an
/// unprivileged `unshare -U --fork --pid` works on an ordinary Linux 6.8
/// desktop and is refused where `kernel.unprivileged_userns_clone` is off or a
/// seccomp profile blocks the syscall, neither of which a CI runner can be
/// assumed to allow. A skip prints why; it does not pass quietly.
/// `print_stderr` is a workspace `warn` and this arm is what it is for: a skip
/// nobody sees is a pass. Allowed on the two items that print, not on the file,
/// so a stray `eprintln!` in an ordinary test here still warns.
#[allow(clippy::print_stderr)]
#[test]
fn tft014_namespace_arm_d_doctor_does_not_accuse_its_own_slot() {
    let scratch = Scratch::new("tft014-ns-arm-d");
    let _pubr = publish(&scratch);

    let Some(json) = doctor_json_under_a_pid_namespace(&scratch) else {
        eprintln!(
            "SKIP tft014_namespace_arm_d: no usable `unshare -U --fork --pid` on this host, \
             so the one arm that needs a second PID namespace around the *observer* cannot \
             be staged. The guard it pins is `recorded_given`'s `proc_is_ours`; its unit \
             rows are `a_pid_from_another_namespace_is_not_a_pid_this_proc_can_answer_about`."
        );
        return;
    };

    let check = check_of(&json, "TFT014");
    assert!(
        !check.contains("forked child inherited it"),
        "`doctor`, run where /proc is not its own namespace's, reported a fork \
         inheritor — the accused slot is its own, and the operator is being \
         told to stop the process reading the report:\n{check}"
    );
    assert!(
        check.contains("\"status\": \"pass\"") || check.contains("\"status\": \"skip\""),
        "every pid in the file is drawn from a numbering this /proc does not \
         use, so there is no verdict left to give:\n{check}"
    );
}

/// Run the shipped binary's `doctor --json --attach` inside a fresh PID
/// namespace whose `/proc` is the parent's, or `None` if this host will not
/// make one.
///
/// `-U` is what makes it unprivileged: `unshare --fork --pid` alone is refused
/// without `CAP_SYS_ADMIN`, and `-Ur` fails on `uid_map` on an ordinary
/// desktop, so `-U` without `-r` is the variant that stages this. Without a
/// `uid_map` the process shows as `nobody` inside while its kernel uid is
/// unchanged — which is what lets it still open the `0600` lock file and take a
/// byte, and therefore be a *participant* rather than merely a process.
///
/// Deliberately **no** `--mount-proc`: remounting `/proc` is what a real
/// container runtime does and it is what makes this fault a bare-`unshare`
/// shape rather than a fleet shape. Mounting it here would stage arm B badly
/// instead of arm D at all.
#[allow(clippy::print_stderr)]
fn doctor_json_under_a_pid_namespace(scratch: &Scratch) -> Option<String> {
    let out = Command::new("unshare")
        .args(["-U", "--fork", "--pid"])
        .arg(env!("CARGO_BIN_EXE_tf_tree"))
        .args(["doctor", "--attach", "--json"])
        .env("TF_TREE_RUNTIME_DIR", &scratch.0)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    // A refused `unshare` and a `doctor` that could not attach are both "this
    // host will not stage arm D", and neither may read as a pass. The
    // discriminator is the document itself: `check_of` needs one.
    if !stdout.contains("\"TFT014\"") {
        eprintln!(
            "unshare/doctor produced no TFT014 document (status {:?}):\nstdout: {stdout}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    Some(stdout)
}
