//! The CLI against a live arena — `docs/decisions/0005` step 11: a publisher
//! runs and the shipped binary describes it, through `clap` and `tf_tree::open()`.
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

/// The test process is the publisher (`CARGO_BIN_EXE_*` names only same-package
/// bins). The returned value must outlive the CLI invocations: dropping it
/// releases the ownership byte and stops the server.
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
    // A short run of history for `echo` and the rate check.
    for i in 0..16i64 {
        w.push(
            1_000_000_000 + i * 10_000_000,
            &tf_tree_math::exp_se3([0.0, 0.0, 0.01 * i as f64, i as f64, 0.0, 0.0]),
        )
        .expect("push");
    }
    // The writer is leaked so the claim stays held (an UNCLAIMED edge changes the output).
    core::mem::forget(w);

    // One successful lookup, so the arena is in service: the §5 counters are
    // incremented by lookups, and without one `TFT010`/`TFT011` refuse to report.
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

/// `--attach` reads the publisher's tree (`map -> base -> cam`), not the
/// in-process fixture (`odom`, `base_link`, a laser).
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

/// The `age(ms)` column is measured against `Clock::decide`'s clock (shared with
/// `doctor` and `top`), not `fixture::NOW_NS`, which is arbitrary for a live arena.
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

    // The newest sample is at 1.15 s and nothing has written since, so the age is small.
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

/// `doctor` must not claim a clean bill of health it did not earn. `TFT001` is
/// reported not run (a ring remembers only the current owner); `TFT011` runs with
/// a note (the lookup in `publish` is load-bearing for that); and `TFT019`'s skip
/// line must name `doctor --from-bag`, the source that can answer
/// (`checks::tests::tft019_inherits_tft018s_replayed_stream_skip` pins the string).
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
    let not_run_reasons = out.split("not run:").nth(1).unwrap_or("");
    assert!(
        not_run_reasons.contains("--from-bag"),
        "TFT019's skip must reach the operator naming the source that can answer:\n{out}"
    );
    // `TFT014` resolves a claim through the shared arena's participant table;
    // the publisher is alive, so it must be silent.
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

/// `participants` must work with no arena at all (§3.3): the lock file survives
/// a segment this build cannot map.
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

/// Nothing running is an answer, not a failure: "no publisher" must differ from
/// "the tool could not look".
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

/// A wrong `--domain` must report nothing there, not a stale snapshot of another domain.
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

/// `tf_tree top` against a live arena, including the observer's own row: a
/// read-only participant holds a lock-file byte and writes no arena record
/// (`Tree::participant_slot` returns `u32::MAX`), so the pane must merge the
/// lock file and mark the observer read-only.
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
    assert!(out.contains("map->base"), "no live edge:\n{out}");
    assert!(
        !out.contains("base_link"),
        "this is the fixture, not the live arena:\n{out}"
    );
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

/// The two participant censuses disagree by the read-only population: a
/// read-only attachment (D18) holds a lock-file byte and writes no arena record,
/// so the arena table under-reports and cannot be `TFT015`'s participants
/// numerator (`no_occupancy_row_is_permanently_zero`). Asserts `held > arena`,
/// printing both counts.
#[test]
fn the_two_participant_censuses_disagree_by_the_read_only_population() {
    let scratch = Scratch::new("censuses");
    let publisher = publish(&scratch);

    let consumer = tf_tree::Open::new()
        .mode(AttachMode::ReadOnly)
        .create(CreatePolicy::Never)
        .timeout(std::time::Duration::from_secs(2))
        .open()
        .expect("join the arena read-only");
    assert_eq!(
        consumer.participant_slot(),
        u32::MAX,
        "the consumer registered an arena record, so this test would prove nothing"
    );

    // Census A: the arena's participant table (`LIVE` slots only).
    let view = publisher.arena_view();
    let table = view.participants();
    let arena = (0..table.capacity() as u32)
        .filter(|slot| table.identity(*slot).is_some())
        .count();

    // Census B: the lock file's held bytes.
    let lock =
        tf_tree_ipc::LockFile::open(&scratch.0.join("0/default.lock")).expect("open the lock file");
    let held = (0..tf_tree_ipc::MAX_PARTICIPANTS)
        .filter(|slot| {
            lock.probe_participant(*slot)
                .map(|p| p.held)
                .unwrap_or(false)
        })
        .count();

    assert!(
        held > arena,
        "the lock file holds {held} participant byte(s) and the arena table has \
         {arena} record(s); equal counts would mean a read-only attachment now \
         writes a record, which is a change to D18"
    );
    assert!(
        arena > 0,
        "the publisher has no record, so nothing is compared"
    );
}

/// `top` refuses `--rw` rather than quietly downgrading it (D18: the diagnostic
/// tool maps `PROT_READ`).
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

/// The lock file of the arena [`publish`] created, as a second open file
/// description: `F_OFD_SETLK` conflicts are per description, so a byte taken
/// here is visible to the CLI subprocess but not this process's `Session`.
fn lock_of(scratch: &Scratch) -> tf_tree_ipc::LockFile {
    let rv = tf_tree_ipc::Rendezvous::from_env().expect("the scratch runtime dir is in the env");
    assert_eq!(
        rv.runtime_dir().path(),
        scratch.0.as_path(),
        "the rendezvous must resolve to this test's scratch dir, not a real one"
    );
    tf_tree_ipc::LockFile::open(rv.lock_path()).expect("the publisher created the lock file")
}

/// A 16-byte `comm` field (`docs/decisions/0033`). Fixtures must fit the
/// kernel's 15-byte cap and leave `48..56` (`pid_ns_inode`) zero.
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

/// (a) A `LIVE` record over a free lock byte, through `doctor --json`
/// (`docs/decisions/0028` plan step 6): pins the wiring from `cmd_doctor` to
/// `probe_lock_facts` to the finding. `register_at` stages a complete
/// registration whose process died; pid `u32::MAX` is deterministically gone and
/// `start_time` is non-zero so the record is comparable. The healthy publisher's
/// slot 0 must produce nothing.
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
                // A pre-`0033` record: zero is "unknown namespace".
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

/// (b) A held byte over a dead pid, the fork case (`docs/decisions/0028` plan
/// step 6; `0030` closes the fd inheritance): the kernel says alive about a gone
/// process and nothing may reclaim it. A second open file description holds the
/// byte exactly as an inherited one does, so no real `fork` is needed.
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
    // The byte the inheritor holds on the dead parent's behalf.
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

/// (c) The read-only participant's fork inheritor, with no arena record
/// (`docs/RUNBOOK.md`'s forked-child paragraph; D18): byte held, recorded pid
/// gone, record `FREE`. Nothing is written to the arena, since the absence of the
/// record is the state under test.
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
            mode: tf_tree_ipc::AccessMode::ReadOnly,
            name: comm("forked-consumer"),
            pid_ns_inode: 0,
        },
    )
    .expect("write the identity record");
    // The byte the inheritor holds on the dead parent's behalf.
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

/// The negative: a healthy joiner caught mid-attach is not a finding
/// (`docs/decisions/0028` plan step 6). The state is `register_at`'s, stopped
/// after the `FREE -> RESERVED` CAS, so the byte is what tells it from a leak.
/// Asserts `TFT014` passed rather than the absence of a string.
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

// `docs/decisions/0033` plan step 1: the four arms of the namespace false
// positive (`TFT014` calling a healthy participant in another PID namespace a
// fork inheritor).
//
//   A  a namespaced participant seen from the host      `Ok(_) => Gone`
//   B  a host participant seen from a container         `ENOENT => Gone`
//   C  a genuine surviving fork inheritor               `ENOENT => Gone`
//   D  participant and observer inside one bare `unshare --fork --pid`
//
// C is the true positive and must keep firing. A and C render identical
// findings, so assertions are on the rendered `--json` evidence, not the arm
// taken. A, B and C are staged through the lock file; only D needs a real
// namespace.

/// An nsfs inode that is not this process's (adjacent to our own, as a kernel would allot).
fn a_foreign_pid_ns() -> u64 {
    own_pid_ns() + 1
}

fn own_pid_ns() -> u64 {
    let ino = tf_tree_ipc::self_pid_ns_inode()
        .expect("/proc/self/ns/pid must be readable to stage a namespace arm");
    assert_ne!(ino, 0, "zero is the record's `unknown namespace` marker");
    ino
}

/// Stage the two slot shapes `TFT014` can accuse, both with a held byte and the
/// same recorded identity:
///
/// * `arena` — a non-`FREE` arena record (`checks::slot_leak`'s main match).
/// * `bare` — no arena record (its `SlotState::Free` early return).
///
/// The returned [`tf_tree_ipc::LockFile`] holds the byte and must be kept alive:
/// dropping it releases the OFD lock and every arm becomes the byte-free shape.
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
    // A second open file description holds both bytes, as an inherited one would (§6.2).
    lock
}

/// Both accusable shapes, from the `--json` document, as an operator reads them.
fn tft014_names_slot(check: &str, slot: u32) -> bool {
    check.contains(&format!("slot {slot} pid "))
}

/// Arm A: a live participant one PID namespace away, seen from the host. The
/// recorded pid exists here (pid 1 on the host) with a different start time, so
/// `Ok(_) => Gone` fires. The observer stays on the host (inside the namespace
/// this is arm D).
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
            // Namespace-local; here it names init.
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

/// Arm B: a host participant seen from another PID namespace. The pid is not in
/// this `/proc`, so it reaches `Gone` through `ENOENT` (`0033` Decision 3: the
/// guard must precede the whole `match probe`). The non-`FREE` shape is staged
/// beside the bare one because this process is the owner.
#[test]
fn tft014_namespace_arm_b_a_host_participant_seen_from_elsewhere_is_not_one_either() {
    const ARENA: u32 = 23;
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

/// Arm C: the true positive. The byte is held for a process gone in the
/// observer's own PID namespace; it must fire on both accusable shapes.
#[test]
fn tft014_namespace_arm_c_a_real_fork_inheritor_in_this_namespace_still_fires() {
    const ARENA: u32 = 25;
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
            // The observer's own namespace.
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

/// Arm D: participant and observer inside one bare `unshare --fork --pid`,
/// where the recorded-namespace guard is blind and `doctor` would accuse its own
/// slot; the `proc_is_ours` guard (`readlink("/proc/self")` against `getpid()`)
/// covers it. A container remounts `/proc` and is arm B. Skips loudly, with the
/// reason, where `unshare -U` is refused; `print_stderr` is allowed on the two
/// items that print.
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

/// Run `doctor --json --attach` inside a fresh PID namespace whose `/proc` is the
/// parent's, or `None` if this host will not make one. `-U` without `-r` keeps
/// the `0600` lock file openable; no `--mount-proc`, or this stages arm B.
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
    // A refused `unshare` or a `doctor` that could not attach is a skip, not a pass.
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
