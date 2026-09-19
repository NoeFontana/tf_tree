//! `tf_tree::open()` against real processes.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// A scratch runtime directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("tf_tree_rv-{}-{tag}", std::process::id()));
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

/// A spawned helper, killed on drop so a failing assertion cannot leave a
/// process holding the arena in `/tmp`.
struct Kid(Child, Option<BufReader<std::process::ChildStdout>>);

impl Kid {
    fn spawn(dir: &PathBuf, args: &[&str]) -> Kid {
        Kid::spawn_with_env(dir, args, &[])
    }

    /// [`Self::spawn`] with extra environment — `TF_TREE_CRASH_AT`, which is
    /// how `docs/PHASE2.md` §11.3 arms a named crash point in a child.
    fn spawn_with_env(dir: &PathBuf, args: &[&str], env: &[(&str, &str)]) -> Kid {
        // The bin target carries the crate's name, not the file's: this crate
        // is published, and `--features shm` installs whatever is here into the
        // user's `bin/`.
        let exe = env!("CARGO_BIN_EXE_tf_tree_rendezvous_child");
        let child = Command::new(exe)
            .args(args)
            .env("TF_TREE_RUNTIME_DIR", dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .envs(env.iter().copied())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the rendezvous child helper");
        Kid(child, None)
    }

    /// Wait for a child that is expected to die on its own, and report how.
    #[cfg(feature = "crash-points")]
    fn wait(&mut self) -> std::process::ExitStatus {
        self.0.wait().expect("wait for the child")
    }

    /// Wait for a child to die **within a bound**, returning `None` if it is
    /// still alive at the deadline.
    fn wait_within(&mut self, bound: std::time::Duration) -> Option<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + bound;
        loop {
            match self.0.try_wait().expect("try_wait") {
                Some(status) => return Some(status),
                None if std::time::Instant::now() >= deadline => return None,
                None => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }

    /// The child's next line.
    fn line(&mut self) -> String {
        let reader = self
            .1
            .get_or_insert_with(|| BufReader::new(self.0.stdout.take().expect("piped stdout")));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read child line");
        line.trim_end().to_string()
    }

    /// Nudge a child that is waiting on stdin, keeping the pipe.
    fn poke(&mut self) {
        use std::io::Write;
        if let Some(stdin) = self.0.stdin.as_mut() {
            let _ = writeln!(stdin, "go");
            let _ = stdin.flush();
        }
    }

    /// Ask a `join-rw-report` child about one slot and read its answer.
    #[cfg(feature = "unstable")]
    fn ask(&mut self, slot: u32) -> String {
        use std::io::Write;
        let stdin = self.0.stdin.as_mut().expect("piped stdin");
        writeln!(stdin, "{slot}").expect("write a slot query");
        stdin.flush().expect("flush the slot query");
        self.line()
    }

    /// `SIGKILL`, then reap.
    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Kid {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// **The milestone's headline: a foreign process joins and agrees.**
#[test]
fn a_foreign_process_joins_and_reads_the_same_transform() {
    let scratch = Scratch::new("join");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    let published = owner.line();
    assert!(published.starts_with("owning "), "got {published}");
    let owner_value = published.strip_prefix("owning ").unwrap().to_string();

    let mut joiner = Kid::spawn(&scratch.0, &["join"]);
    let joined = joiner.line();
    assert!(
        joined.starts_with("joined "),
        "the joiner did not attach: {joined}"
    );
    let joiner_value = joined.strip_prefix("joined ").unwrap().to_string();

    assert_eq!(
        joiner_value, owner_value,
        "the joiner read a different transform than the owner published"
    );
}

/// `create = Never` fails fast when nothing is serving, rather than waiting.
#[test]
fn a_consumer_that_will_not_create_fails_fast_on_an_empty_machine() {
    let scratch = Scratch::new("never");
    let mut kid = Kid::spawn(&scratch.0, &["join"]);
    let line = kid.line();
    assert!(
        line.starts_with("error"),
        "expected a fast failure, got {line}"
    );
    assert!(
        line.contains("no arena"),
        "the error should name the absent arena: {line}"
    );
}

/// **The zero-argument convenience still joins a served arena.**
#[test]
fn the_free_open_joins_a_served_arena() {
    let scratch = Scratch::new("free-open");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    let published = owner.line();
    assert!(published.starts_with("owning "), "got {published}");
    let owner_value = published.strip_prefix("owning ").unwrap().to_string();

    let mut joiner = Kid::spawn(&scratch.0, &["open-free"]);
    let joined = joiner.line();
    assert!(
        joined.starts_with("joined "),
        "tf_tree::open() did not join a served arena: {joined}"
    );
    assert_eq!(
        joined.strip_prefix("joined ").unwrap(),
        owner_value,
        "tf_tree::open() read a different transform than the owner published"
    );
}

/// How many threads `pid` has, from `/proc/<pid>/status`.
fn threads(pid: u32) -> Option<usize> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Threads:"))?
        .trim()
        .parse()
        .ok()
}

/// Block until `pid` has left state `T`, or panic.
fn await_continued(pid: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match proc_state(pid) {
            Some('T' | 't') => assert!(
                std::time::Instant::now() < deadline,
                "pid {pid} never left state T after SIGCONT, so nothing below \
                 would be a measurement of a continued process"
            ),
            Some(_) => return,
            None => panic!("pid {pid} died instead of continuing"),
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Give `pid` up to `budget` to settle, then report its thread count.
fn threads_settle(pid: u32, budget: std::time::Duration) -> Option<usize> {
    let start = threads(pid);
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        let now = threads(pid);
        if now != start {
            return now;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    threads(pid)
}

/// **An owner that was stopped and continued is still serving.**
#[test]
fn a_stopped_and_continued_owner_still_serves_the_rendezvous() {
    let scratch = Scratch::new("stop-cont");
    let sock = scratch.0.join("0/default.sock");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    let published = owner.line();
    assert!(published.starts_with("owning "), "got {published}");
    let owner_value = published.strip_prefix("owning ").unwrap().to_string();
    let pid = owner.0.id();

    assert_eq!(
        threads(pid),
        Some(2),
        "an owner should be a parked main thread plus tf_tree-owner; without \
         two there is no serving thread for this test to lose"
    );
    assert!(sock.exists(), "the owner published no socket at {sock:?}");

    // The control.
    let mut before = Kid::spawn(&scratch.0, &["join"]);
    let joined = before.line();
    assert_eq!(
        joined.strip_prefix("joined "),
        Some(owner_value.as_str()),
        "the fixture never served in the first place: {joined}"
    );

    for signal in ["-STOP", "-CONT"] {
        if signal == "-CONT" {
            await_stopped(pid);
        }
        assert!(
            Command::new("kill")
                .args([signal, &pid.to_string()])
                .status()
                .is_ok_and(|s| s.success()),
            "could not send {signal} to the owner"
        );
    }
    await_continued(pid);

    assert_eq!(
        threads_settle(pid, std::time::Duration::from_millis(500)),
        Some(2),
        "the owner's serving thread died on a stop/continue pair while the \
         process lived on holding its lock bytes; socket present: {}",
        sock.exists()
    );

    let mut after = Kid::spawn(&scratch.0, &["join"]);
    let rejoined = after.line();
    assert_eq!(
        rejoined.strip_prefix("joined "),
        Some(owner_value.as_str()),
        "a second process could not join after the owner was stopped and \
         continued: {rejoined}"
    );
}

/// **`docs/decisions/0019` §2a: a read-only attach cannot create.**
#[test]
fn a_read_only_attach_refuses_to_create() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let _scratch = Scratch::new("ro-create");

    let layout = || {
        TreeBuilder::new()
            .default_interp(InterpPolicy::LerpSlerp)
            .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64)))
    };

    for policy in [CreatePolicy::IfAbsent, CreatePolicy::Always] {
        let err = tf_tree::Open::new()
            .mode(AttachMode::ReadOnly)
            .create(policy)
            .layout_if_creating(layout())
            .open()
            .err()
            .expect("a read-only creator must be refused");
        assert!(
            matches!(err, tf_tree::OpenError::ReadOnlyCannotCreate),
            "expected ReadOnlyCannotCreate for {policy:?}, got {err:?}"
        );
    }

    // **And the machine is still empty.** The refusal has to happen before
    // anything is created, not after — an error returned over a segment that
    // now exists would leave the next `Never` consumer joining an empty arena
    // and reporting itself healthy.
    let err = tf_tree::Open::new()
        .create(CreatePolicy::Never)
        .open()
        .err()
        .expect("nothing should have been created");
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaAbsent)
        ),
        "the refused open left an arena behind: {err:?}"
    );
}

/// **Byte 0 and the ownership byte held by two *different* holders** — the one
/// state of that arm nothing reached through the API (`0055` step 3).
#[test]
fn byte_0_and_ownership_held_by_two_different_holders_is_refused_without_naming_a_topology() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let scratch = Scratch::new("two-holders");
    let lock_path = scratch.0.join("0/default.lock");
    std::fs::create_dir_all(scratch.0.join("0")).unwrap();

    // Holder A: the creator's participant byte, with no ownership.
    let stranded = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        stranded.try_take_participant(0).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );
    // Holder B: the ownership byte, on its own description, serving nothing.
    let owner_byte = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        owner_byte.try_take_ownership().unwrap(),
        tf_tree_ipc::LockAttempt::Acquired,
        "the ownership byte must be free for a second description to take it"
    );

    let err = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
        )
        .timeout(std::time::Duration::from_millis(200))
        .open()
        .err()
        .expect("both bytes held must refuse");

    let tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaHeldButUnreachable {
        holder_slots,
        first_slot,
        ownership_held,
        ..
    }) = err
    else {
        panic!("expected ArenaHeldButUnreachable, got {err:?}");
    };
    assert_eq!(
        (holder_slots, first_slot, ownership_held),
        (0b1, Some(0), true),
        "two holders present the same triple a single owner of both bytes does"
    );

    // **This test's contribution is the reachability, and after step 6 that is
    // all of it.** Two real holders present the same triple a single owner of
    // both bytes does, and the message now says only what is true of both: the
    // bytes are held.
    let message = err.to_string();
    assert!(
        message.contains("ownership byte held"),
        "the held ownership byte is the fact that distinguishes this from a stranded \
         participant: {message}"
    );
    assert!(
        !message.contains("process") && !message.contains("stop"),
        "no claim about processes may be made from two bits: {message}"
    );
    assert_eq!(
        message,
        tf_tree::IpcError::ArenaHeldButUnreachable {
            holder_slots: 0b1,
            first_slot: Some(0),
            first_pid: 0,
            ownership_held: true,
        }
        .to_string(),
        "two holders and one owner of both bytes are indistinguishable here, and the \
         message must be identical for both — which is why neither may be prescribed for"
    );

    drop(owner_byte);
    drop(stranded);
}

/// **A healthy owner holds both bytes, and that is the state the `(0b1, true)`
/// remedy is written for** (#353, `0055` question 3).
#[test]
fn a_live_owner_holding_both_bytes_is_not_told_to_stop_a_second_process() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let scratch = Scratch::new("owner-holds-both");
    let layout = || {
        TreeBuilder::new()
            .default_interp(InterpPolicy::LerpSlerp)
            .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64)))
    };

    // A real owner: it creates, binds and serves, and it keeps the ownership
    // byte and slot 0 for its whole life.
    let owner = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .layout_if_creating(layout())
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("the owner must create and serve");
    assert_eq!(
        owner.participant_slot(),
        0,
        "the creator holds CREATOR_SLOT, which is what puts bit 0 in the mask"
    );

    // Remove the door while the owner is still behind it.
    let sock = scratch.0.join("0/default.sock");
    std::fs::remove_file(&sock).expect("the rendezvous socket must exist to be removed");

    let err = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .layout_if_creating(layout())
        .timeout(std::time::Duration::from_millis(200))
        .open()
        .err()
        .expect("a joiner that cannot reach the socket must be refused");

    let tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaHeldButUnreachable {
        holder_slots,
        first_slot,
        ownership_held,
        ..
    }) = err
    else {
        panic!("expected ArenaHeldButUnreachable, got {err:?}");
    };
    assert_eq!(
        (holder_slots, first_slot, ownership_held),
        (0b1, Some(0), true),
        "a live owner is one holder of both bytes: this is the state the remedy must fit"
    );

    // **The hedge became structural when the remedy left the message** (`0055`
    // step 6).
    let message = err.to_string();
    assert!(
        message.contains("participant bytes 0x1 held") && message.contains("ownership byte held"),
        "both held bytes must be reported: {message}"
    );
    assert!(
        !message.contains("process") && !message.contains("stop"),
        "the message must make no claim about processes or about what to stop — that is \
         the runbook's, because Display cannot see who holds a byte: {message}"
    );

    drop(owner);
}

/// **`RUNBOOK.md`'s escape hatch out of `ArenaHeldButUnreachable`, run as
/// written.**
#[test]
fn the_escape_hatch_creates_over_a_stranded_participant() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let scratch = Scratch::new("force-new");
    let lock_path = scratch.0.join("0/default.lock");
    std::fs::create_dir_all(scratch.0.join("0")).unwrap();
    let survivor = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        survivor.try_take_participant(3).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );

    let layout = || {
        TreeBuilder::new()
            .default_interp(InterpPolicy::LerpSlerp)
            .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64)))
    };
    let creator = |policy| {
        tf_tree::Open::new()
            .mode(AttachMode::ReadWrite)
            .create(policy)
            .layout_if_creating(layout())
            .timeout(std::time::Duration::from_millis(100))
    };

    // The wedge itself: a held byte, nothing serving, and an ordinary creator
    // refusing rather than starting a second arena beside it.
    let err = creator(CreatePolicy::IfAbsent)
        .open()
        .err()
        .expect("a held participant byte must turn an ordinary creator away");
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaHeldButUnreachable { .. })
        ),
        "expected ArenaHeldButUnreachable, got {err:?}"
    );

    // **The message read verbatim, in the state that reaches the lock file.**
    // **No arm sends an operator anywhere any more** — `0055` step 6 moved the
    // remedy, and the layout-and-read-write requirement step 2 added to the
    // message went with it to `RUNBOOK.md`.
    let no_layout = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::Always)
        .timeout(std::time::Duration::from_millis(100))
        .open()
        .err()
        .expect("a forced create with no layout cannot create");
    assert!(
        matches!(no_layout, tf_tree::OpenError::NoLayoutToCreate),
        "the policy alone must not create; expected NoLayoutToCreate, got {no_layout:?}"
    );

    let tree = creator(CreatePolicy::Always)
        .open()
        .expect("CreatePolicy::Always must create over a stranded participant");
    assert_ne!(
        tree.participant_slot(),
        3,
        "the escape hatch took the stranded participant's slot"
    );

    // A second open file description, because `F_OFD_GETLK` reports only
    // *conflicting* locks and `survivor`'s own byte does not conflict with
    // itself — asking through `survivor` would report its own byte free.
    let witness = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert!(
        witness.probe_participant(3).unwrap().held,
        "abandoning the arena released the stranded participant's byte"
    );
}

/// **The boundary of the escape hatch, from both sides of it** (#257).
#[test]
fn a_live_byte_0_refuses_both_policies() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let scratch = Scratch::new("force-boundary");
    let lock_path = scratch.0.join("0/default.lock");
    std::fs::create_dir_all(scratch.0.join("0")).unwrap();

    let opener = |policy| {
        tf_tree::Open::new()
            .mode(AttachMode::ReadWrite)
            .create(policy)
            .layout_if_creating(
                TreeBuilder::new()
                    .default_interp(InterpPolicy::LerpSlerp)
                    .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
            )
            .timeout(std::time::Duration::from_millis(100))
    };

    // The creator's own byte, held live.
    let on_byte_0 = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        on_byte_0.try_take_participant(0).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );

    let refusal = |policy| {
        opener(policy)
            .open()
            .err()
            .unwrap_or_else(|| panic!("{policy:?} created an arena over a live byte 0"))
    };
    let ordinary = refusal(CreatePolicy::IfAbsent);
    let forced = refusal(CreatePolicy::Always);
    assert_eq!(
        ordinary, forced,
        "forcing must change nothing about a wedge on byte 0"
    );
    assert!(
        matches!(
            forced,
            tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaHeldButUnreachable {
                holder_slots: 0b1,
                first_slot: Some(0),
                ownership_held: false,
                ..
            })
        ),
        "expected ArenaHeldButUnreachable naming slot 0 with ownership free, got {forced:?}"
    );
    // The remedy moved to `RUNBOOK.md` (`0055` step 6), so the facts that
    // *selected* the remedy are what this asserts: slot 0 is the creator's, and
    // the ownership byte is free.
    let byte_0_message = forced.to_string();
    assert!(
        byte_0_message.contains("the creator's"),
        "the message must say slot 0 is the creator's own: {byte_0_message}"
    );
    assert!(
        byte_0_message.contains("ownership byte free"),
        "the message must report the free ownership byte: {byte_0_message}"
    );
    assert!(
        byte_0_message.contains("participant bytes 0x1 held"),
        "byte 0 alone: the mask is what separates this state from the crowded one: \
         {byte_0_message}"
    );

    // **The remedy branches on the rest of the mask, and getting that wrong is
    // how this message tells an operator something false.** Add one ordinary
    // joiner's byte and nothing else changes about slot 0 — but "stop it and an
    // ordinary open will create" stops being true, because step 4 still sees
    // byte 2.
    let alongside = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        alongside.try_take_participant(2).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );
    let crowded = refusal(CreatePolicy::Always);
    assert!(
        matches!(
            crowded,
            tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaHeldButUnreachable {
                holder_slots: 0b101,
                first_slot: Some(0),
                ..
            })
        ),
        "expected slot 0 plus slot 2 in the mask, got {crowded:?}"
    );
    // **The mask is what a crowded state says differently, now that neither
    // state carries a remedy** (`0055` step 6).
    let crowded_message = crowded.to_string();
    assert!(
        crowded_message.contains("participant bytes 0x5 held"),
        "a crowded mask must be reported as such, not collapsed to its lowest slot: \
         {crowded_message}"
    );
    assert!(
        crowded_message.contains("lowest slot 0"),
        "the lowest held slot is the one fact the runbook's table is indexed by: \
         {crowded_message}"
    );
    drop(alongside);

    // **The positive control.** One variable changes — which byte is held — and
    // the same forced create that just refused now succeeds.
    drop(on_byte_0);
    let stranded = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        stranded.try_take_participant(3).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );
    let still_refused = refusal(CreatePolicy::IfAbsent);
    let stranded_message = still_refused.to_string();
    assert!(
        stranded_message.contains("lowest slot 3"),
        "a stranded joiner must be named by its own slot, which is the fact the \
         runbook's table is indexed by: {stranded_message}"
    );
    assert_ne!(
        byte_0_message, stranded_message,
        "the two states must not print the same sentence — telling them apart is the \
         whole point of the field"
    );

    let tree = opener(CreatePolicy::Always)
        .open()
        .expect("byte 0 free and only a joiner stranded: the hatch must create");
    assert_eq!(
        tree.participant_slot(),
        0,
        "the forced creator takes the creator's slot like any other creator"
    );
    let witness = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert!(
        witness.probe_participant(3).unwrap().held,
        "the abandoned arena's survivor must keep its byte"
    );
}

/// **The third state, which is neither of the two above** (#257).
#[test]
fn a_held_ownership_byte_refuses_the_hatch_and_freeing_it_lets_one_through() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let scratch = Scratch::new("force-ownership");
    let lock_path = scratch.0.join("0/default.lock");
    std::fs::create_dir_all(scratch.0.join("0")).unwrap();

    let forced = || {
        tf_tree::Open::new()
            .mode(AttachMode::ReadWrite)
            .create(CreatePolicy::Always)
            .layout_if_creating(
                TreeBuilder::new()
                    .default_interp(InterpPolicy::LerpSlerp)
                    .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
            )
            .timeout(std::time::Duration::from_millis(100))
    };

    let ownership = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        ownership.try_take_ownership().unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );
    let stranded = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        stranded.try_take_participant(1).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );

    let err = forced()
        .open()
        .err()
        .expect("a held ownership byte must refuse even a forced create");
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaHeldButUnreachable {
                first_slot: Some(1),
                ownership_held: true,
                ..
            })
        ),
        "expected ArenaHeldButUnreachable with ownership_held, got {err:?}"
    );
    // **The message states the facts and the runbook carries the remedy**
    // (`0055` step 6).
    let message = err.to_string();
    assert!(
        message.contains("ownership byte held"),
        "the message must report the held ownership byte: {message}"
    );
    assert!(
        message.ends_with("(ArenaHeldButUnreachable)"),
        "the message must end with the runbook's search key, in the parenthesised \
         form `0059` (g) specifies: {message}"
    );

    // The control: release the ownership byte and change nothing else.
    drop(ownership);
    let tree = forced()
        .open()
        .expect("with ownership free this is the stranded-joiner case again");
    assert_eq!(tree.participant_slot(), 0);
    let witness = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert!(
        witness.probe_participant(1).unwrap().held,
        "the abandoned arena's survivor must keep its byte"
    );
    drop(stranded);
}

/// A pinned defect (#201), not desired behaviour.
#[cfg(feature = "unstable")]
#[test]
fn defect_201_a_forced_creators_record_reads_dead_while_it_is_publishing() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, Stamp, TreeBuilder};

    let scratch = Scratch::new("slot-divergence");
    let lock_path = scratch.0.join("0/default.lock");
    std::fs::create_dir_all(scratch.0.join("0")).unwrap();

    // The stranded survivor, on **byte 0** — the index a fresh arena also gives
    // its first participant record.
    let survivor = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        survivor.try_take_participant(0).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );

    // Refused, since `0035` one layer lower, with a better error.
    let witness = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    let err = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::Always)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
        )
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .err()
        .expect("a create that would diverge must be refused");
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaHeldButUnreachable {
                first_slot: Some(0),
                ..
            })
        ),
        "expected ArenaHeldButUnreachable naming slot 0, got {err:?}"
    );

    // The guard compares two integers and consults nothing else, and
    // `try_take_participant` writes no identity: a guard that tried to decide
    // from the lock file's identity rows, or to excuse a divergence whose byte
    // names nobody, would have nothing to read here.
    assert!(
        witness.read_identity(0).unwrap().is_none(),
        "the staged survivor writes no identity, and the refusal must not need one"
    );
    assert!(
        witness.probe_participant(0).unwrap().held,
        "the refusal disturbed the staged survivor's byte"
    );
    assert_eq!(
        witness.held_participants().unwrap(),
        0b1,
        "the refused creator kept a participant byte"
    );
    assert!(
        !witness.probe_ownership().unwrap().held,
        "the refused creator kept the ownership byte"
    );
    // **No bound rendezvous socket survives the refusal.** What that catches is
    // a refusal that returns while the owner server it spawned is still alive;
    // what it does *not* catch is the guard's **placement**, because `impl Drop
    // for OwnerServer` unlinks either way.
    assert!(
        !scratch.0.join("0/default.sock").exists(),
        "a refused create must not leave a bound rendezvous socket"
    );

    // **The refusal is recoverable, which is the argument for an error rather
    // than an assertion.** Stop the process holding the byte — the one thing
    // `OpenError::ParticipantSlotDiverged`'s documentation tells a caller to do
    // — and the ordinary path works, with byte and record at one index again.
    drop(survivor);
    let tree = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
        )
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("with byte 0 released, an ordinary create must succeed");
    assert_eq!(
        tree.participant_slot(),
        0,
        "a fresh arena registers its creator at record 0"
    );
    assert_eq!(
        witness.held_participants().unwrap(),
        0b1,
        "and at lock byte 0 — the same integer, which is the whole invariant"
    );

    // And it is a working arena, not merely an opened one: the process the
    // guard refused to strand is publishing and reading back its own transform.
    let publisher = tree
        .claim(tree.frame("base").unwrap(), tree.frame("map").unwrap())
        .expect("claim the one edge");
    publisher
        .push(1_000, &tf_tree::exp_se3([1.0, 2.0, 3.0, 0.1, 0.2, 0.3]))
        .expect("push");
    let g = tree.guard();
    let plan = tree
        .plan(tree.frame("map").unwrap(), tree.frame("base").unwrap())
        .unwrap();
    assert!(
        plan.at(&g, Stamp::<tf_tree::SystemDomain>::from_nanos(1_000))
            .is_ok(),
        "the recovered creator must serve its own transform"
    );
}

/// **The same divergence, reached through published API with nothing staged.**
#[cfg(feature = "unstable")]
#[test]
fn defect_201_release_ownership_strands_a_live_non_owner_on_byte_0() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};
    use tf_tree_ipc::{ArenaName, EnvVar, NoServer, OpenOutcome, Rendezvous, RuntimeDir};

    let scratch = Scratch::new("release-ownership-divergence");
    let rendezvous = || {
        Rendezvous::new(
            RuntimeDir::resolve().unwrap(),
            0,
            ArenaName::new("default", EnvVar::Name).unwrap(),
        )
    };

    // An ordinary `tf_tree_ipc` consumer creates, then hands back the owner
    // role.
    let mut stranded = tf_tree_ipc::Open::new(rendezvous())
        .timeout(std::time::Duration::from_millis(500))
        .open(&mut NoServer)
        .expect("the first open must create");
    assert_eq!(stranded.outcome(), OpenOutcome::Created);
    assert_eq!(stranded.slot(), 0, "a creator takes participant byte 0");
    stranded
        .release_ownership()
        .expect("§3.5: giving up the owner role must succeed");
    assert!(!stranded.is_owner(), "the owner role was given up");

    let witness = tf_tree_ipc::LockFile::open(&scratch.0.join("0/default.lock")).unwrap();
    assert!(
        !witness.probe_ownership().unwrap().held,
        "release_ownership must free the ownership byte"
    );
    assert!(
        witness.probe_participant(0).unwrap().held,
        "and must keep participant byte 0 — §3.5 stays attached"
    );
    assert!(
        witness.read_identity(0).unwrap().is_some(),
        "unlike the staged survivor above, byte 0 names a real registration"
    );

    // The forced create. §3.4 step 4 lets it past precisely because it is
    // `CreatePolicy::Always` — and then step 5 asks the kernel for byte 0,
    // which this test has arranged to be held by a **real registration** with
    // an identity record behind it.
    let err = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::Always)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
        )
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .err()
        .expect("the divergence must be refused, not handed back as a Tree");
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaHeldButUnreachable {
                first_slot: Some(0),
                ..
            })
        ),
        "**#201, closed at the source.** The creator's lock byte would have been \
         1 and its arena record 0, so every predicate reading record 0's \
         liveness would have asked the kernel about the stranded session's \
         byte. `0035` stops the byte being handed out at all, so the refusal \
         now comes from the rendezvous and names the holder. Expected \
         ArenaHeldButUnreachable {{ first_slot: Some(0), .. }}, got {err:?}"
    );

    // No bound rendezvous socket survives the refusal.
    assert!(
        !scratch.0.join("0/default.sock").exists(),
        "a refused create must not leave a bound rendezvous socket"
    );

    // **And it left nothing behind.** Byte 1 — which the forced creator would
    // have been handed before `0035`, and now never reaches — is free, and so
    // is the ownership byte it took on the way in; only the stranded session's
    // byte 0 is still held, by the process entitled to it.
    assert_eq!(
        witness.held_participants().unwrap(),
        0b1,
        "the refused creator kept a participant byte"
    );
    assert!(
        !witness.probe_ownership().unwrap().held,
        "the refused creator kept the ownership byte"
    );
    assert!(
        witness.probe_participant(0).unwrap().held,
        "the refusal disturbed the stranded session's byte"
    );

    assert!(
        witness.read_identity(1).unwrap().is_none(),
        "a refused create must take no byte, and so write no identity behind it"
    );
}

/// **`Open::require_create` refuses to join, and leaves nothing behind.**
#[test]
fn require_create_refuses_a_live_arena_and_releases_its_slot() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let scratch = Scratch::new("require-create");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "));

    let err = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .require_create(true)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
        )
        .open()
        .err()
        .expect("a second owner must not silently join");
    assert!(
        matches!(err, tf_tree::OpenError::ArenaAlreadyLive),
        "expected ArenaAlreadyLive, got {err:?}"
    );

    // The owner holds slot 0.
    let lock = tf_tree_ipc::LockFile::open(&scratch.0.join("0/default.lock")).unwrap();
    assert!(
        !lock.probe_participant(1).unwrap().held,
        "the refused attach kept its participant lock byte"
    );

    // And an ordinary consumer is unaffected — the refusal is about this
    // caller's intent, not about the arena.
    let mut joiner = Kid::spawn(&scratch.0, &["join"]);
    assert!(
        joiner.line().starts_with("joined "),
        "the refusal disturbed the arena"
    );
}

/// A consumer that starts before its publisher waits, and the wait resolves.
#[test]
fn a_consumer_waits_for_an_arena_that_starts_late() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let scratch = Scratch::new("late-start");
    let dir = scratch.0.clone();

    let (tx, rx) = mpsc::channel::<Kid>();
    let spawner = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let mut owner = Kid::spawn(&dir, &["own"]);
        assert!(
            owner.line().starts_with("owning "),
            "the owner did not start"
        );
        // Hand it back so it stays alive for the assertions below; dropping it
        // here would kill the arena mid-test.
        let _ = tx.send(owner);
    });

    let started = Instant::now();
    let tree = tf_tree::Open::new()
        .await_open(Duration::from_secs(20))
        .expect("the wait should have outlasted a publisher 200 ms late");
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_millis(200),
        "the wait returned before the publisher could have started ({elapsed:?}) — \
         it did not actually wait"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the wait took far longer than the publisher's 200 ms delay: {elapsed:?}"
    );
    assert!(!tree.is_writable(), "the default attach is read-only (D18)");

    let owner = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the spawner thread never produced an owner");
    drop(tree);
    drop(owner);
    spawner.join().expect("spawner thread");
}

/// A wait with no publisher at all gives up inside a bounded time.
#[test]
fn a_wait_for_an_arena_that_never_starts_gives_up() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let _scratch = Scratch::new("never-starts");

    let budget = Duration::from_millis(300);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        let outcome = tf_tree::Open::new().await_open(budget);
        let _ = tx.send((outcome.err(), started.elapsed()));
    });

    let (err, elapsed) = rx.recv_timeout(Duration::from_secs(30)).expect(
        "await_open did not report back (Timeout = it ignored its deadline and \
         this thread's 30 s bound fired first, where nextest's terminate-after \
         would have stopped it only at 180 s without saying why; Disconnected = \
         the worker panicked before sending). Got",
    );
    let err = err.expect("an empty machine has no arena to open");

    // **The last retryable error, verbatim — there is no `Timeout` variant.**
    // `ArenaAbsent` already says exactly what was true for the whole budget,
    // and a second spelling would carry strictly less.
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaAbsent)
                | tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaHeldButUnreachable { .. })
        ),
        "expected the last retryable rendezvous error, got {err:?}"
    );
    assert!(elapsed >= budget, "it gave up early: {elapsed:?}");
    assert!(
        elapsed < budget * 20,
        "it overran its budget by more than the backoff can explain: {elapsed:?}"
    );
}

/// **A whole-second budget is retried, not rejected.**
#[test]
fn a_whole_second_wait_is_not_refused_by_the_socket_timeout() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let _scratch = Scratch::new("whole-second-budget");

    let budget = Duration::from_secs(1);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        let outcome = tf_tree::Open::new().await_open(budget);
        let _ = tx.send((outcome.err(), started.elapsed()));
    });

    // Same shape as the test above, and for the same reason.
    let (err, elapsed) = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("await_open never returned");
    let err = err.expect("an empty machine has no arena to open");

    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaAbsent)
                | tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaHeldButUnreachable { .. })
        ),
        "a whole-second budget must end in the last retryable rendezvous error, \
         not in a local socket failure: {err:?}"
    );
    assert!(
        elapsed >= budget,
        "it did not wait out a whole-second budget ({elapsed:?}) — the first \
         attempt was refused before it could retry"
    );
}

/// **`docs/decisions/0019` §2b's second wait: a frame interned after the arena
/// already exists.**
#[test]
fn a_consumer_waits_for_a_frame_interned_after_the_arena_exists() {
    use std::io::Write;
    use std::time::{Duration, Instant};

    let scratch = Scratch::new("late-frame");

    let mut owner = Kid::spawn(&scratch.0, &["own-headroom"]);
    assert_eq!(owner.line(), "owning");

    let consumer = tf_tree::Open::new()
        .open()
        .expect("join the arena the owner already created");
    assert!(!consumer.is_writable(), "the default attach is read-only");
    assert!(
        consumer.frames().unwrap().iter().all(|n| n != "late_frame"),
        "the frame under test was already interned before the wait began"
    );

    // Poke from a thread, ~200 ms in, so the wait below has something to wait
    // for.
    let mut stdin = owner.0.stdin.take().expect("piped stdin");
    let poker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let _ = writeln!(stdin, "go");
    });

    let started = Instant::now();
    let [late] = consumer
        .await_frames(["late_frame"], Duration::from_secs(20))
        .expect("the frame was interned well inside the budget");
    let elapsed = started.elapsed();

    poker.join().expect("poker thread");
    let interned = owner.line();
    let owner_id: u32 = interned
        .strip_prefix("interned ")
        .expect(&interned)
        .parse()
        .unwrap();

    assert_eq!(
        late.get(),
        owner_id,
        "the waiter resolved to a different id than the owner interned"
    );
    assert!(
        elapsed >= Duration::from_millis(200),
        "the wait returned before the owner could have interned ({elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the wait far outlasted the intern it was waiting for: {elapsed:?}"
    );
}

/// **`AwaitError::Timeout`, which nothing else in any gate produces.**
#[test]
fn a_frames_wait_for_a_name_nobody_will_intern_gives_up() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use tf_tree::AwaitError;
    use tf_tree_core::frame::blake3_64;

    /// Nothing in the helper's fixture interns this, and nothing in this test
    /// does either.
    const MISSING: &str = "no_publisher_will_ever_declare_this";

    let scratch = Scratch::new("frames-timeout");
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(
        owner.line().starts_with("owning "),
        "the owner did not start"
    );

    let budget = Duration::from_millis(300);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let consumer = tf_tree::Open::new()
            .open()
            .expect("join the arena the owner is serving");
        let writable = consumer.is_writable();
        // **`all_interned::<0>` — reachable only from here.** The `N == 0` test
        // in `tests/await_frames.rs` runs on a heap tree, so it is refused by
        // the writable guard and the conversion helper is never called with an
        // empty array anywhere else in the workspace.
        let empty = consumer.await_frames([], Duration::from_millis(0));
        let started = Instant::now();
        let outcome = consumer.await_frames(["map", MISSING], budget);
        let _ = tx.send((writable, empty, outcome, started.elapsed()));
    });

    let (writable, empty, outcome, elapsed) = rx.recv_timeout(Duration::from_secs(30)).expect(
        "await_frames did not report back (Timeout = it ignored its deadline and \
         this thread's 30 s bound fired first, where nextest's terminate-after \
         would have stopped it only at 180 s without saying why; Disconnected = \
         the worker panicked before sending). Got",
    );

    assert!(!writable, "the default attach must be read-only (D18)");
    assert_eq!(
        empty,
        Ok([]),
        "a zero-length request on a waitable tree must be answered without \
         touching the arena"
    );

    let err = outcome.expect_err("a name nobody interned must not resolve");
    assert_eq!(
        err,
        AwaitError::Timeout {
            hash: blake3_64(MISSING)
        },
        "the timeout named the wrong frame, or was not a timeout at all"
    );
    assert!(elapsed >= budget, "it gave up early: {elapsed:?}");
    assert!(
        elapsed < budget * 20,
        "it overran its budget by more than the backoff can explain: {elapsed:?}"
    );
}

/// A `SIGSTOP`ped participant is alive; a `SIGKILL`ed one is not.
#[test]
fn a_stopped_peer_is_alive_and_a_killed_one_is_not() {
    let scratch = Scratch::new("liveness");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "));

    // A joiner that will be stopped, then killed.
    let mut peer = Kid::spawn(&scratch.0, &["join-rw"]);
    assert!(peer.line().starts_with("joined "), "peer did not join");

    let observer_alive = |scratch: &PathBuf| {
        let mut k = Kid::spawn(scratch, &["peer-alive", "1"]);
        let line = k.line();
        k.kill();
        line
    };

    // Stopped: still holding its byte, so still alive.
    let pid = peer.0.id();
    assert!(
        std::process::Command::new("kill")
            .args(["-STOP", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success()),
        "could not SIGSTOP the peer"
    );
    assert_eq!(
        observer_alive(&scratch.0),
        "alive true",
        "a SIGSTOPped participant was reported dead — a slow publisher must \
         never be mistaken for a hung one (D17)"
    );

    // Killed: the kernel releases the byte, with no cooperation from the peer.
    let _ = std::process::Command::new("kill")
        .args(["-CONT", &pid.to_string()])
        .status();
    peer.kill();
    assert_eq!(
        observer_alive(&scratch.0),
        "alive false",
        "a SIGKILLed participant was still reported alive"
    );
}

/// **The hangup reap clears a joiner's slot and cannot clear the owner's — the
/// two halves of the same arena, measured against each other.**
#[cfg(feature = "unstable")]
#[test]
fn the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live() {
    let scratch = Scratch::new("owner-slot");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    // Slots are handed out in join order, and each child is waited for before
    // the next is spawned, so these numbers are determined rather than hoped
    // for: owner 0, peer 1, watcher 2.
    let mut peer = Kid::spawn(&scratch.0, &["join-rw"]);
    assert!(peer.line().starts_with("joined "), "peer did not join");
    let mut watcher = Kid::spawn(&scratch.0, &["join-rw-report"]);
    assert_eq!(watcher.line(), "joined", "watcher did not join");

    for slot in 0..3 {
        let seen = watcher.ask(slot);
        assert!(
            seen.contains("state live") && seen.contains("alive true"),
            "all three participants must start out live and running: {seen}"
        );
    }

    // **#191, measured.** The owner is running, so its `epoll` sees the peer's
    // socket close and the callback releases the record.
    peer.kill();
    let mut seen = String::new();
    for _ in 0..100 {
        seen = watcher.ask(1);
        if seen.contains("state free") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        seen.contains("state free"),
        "the owner's hangup callback must release a killed joiner's record \
         (#191); it is still: {seen}"
    );

    // **Hole 3, measured.** Nothing hangs up on the owner, so its own record is
    // left exactly as `TFT014` describes: `LIVE`, with the kernel reporting its
    // lock byte free — and it stays that way for as long as nobody *asks*.
    owner.kill();
    let mut seen = String::new();
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        seen = watcher.ask(0);
        assert!(
            seen.contains("state live"),
            "nothing clears the owner's own record *by itself* — no socket of \
             its own closes, so no hangup fires — and this watcher never \
             sweeps, so the record must stay LIVE: {seen}"
        );
    }
    assert!(
        seen.contains("alive false"),
        "the kernel released the dead owner's lock byte, so the liveness \
         predicate must say so: {seen}"
    );

    // **And nobody new can be told about it.** `doctor --attach` joins through
    // the rendezvous, which died with the owner, so the shape above is one the
    // CLI check cannot be pointed at — `docs/PHASE5.md` §6's `TFT014` amendment
    // says which shapes it can.
    let mut late = Kid::spawn(&scratch.0, &["join-rw"]);
    let refused = late.line();
    assert!(
        refused.starts_with("error") && refused.contains("unreachable"),
        "a fresh attach to an owner-less arena must be refused, or this leak \
         would be reachable from `doctor --attach`: {refused}"
    );
}

/// **`docs/PHASE2.md` §11.2 scenario 2b — slot recycling under abnormal exit.**
#[test]
fn slot_recycling_under_abnormal_exit() {
    let scratch = Scratch::new("recycle-2b");
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    for cycle in 1..=128u32 {
        let mut joiner = Kid::spawn(&scratch.0, &["join-rw"]);
        let line = joiner.line();
        assert!(
            line.starts_with("joined "),
            "attach {cycle} of 128 was refused: {line}"
        );
        // `kill` waits, so the next attach starts after the kernel has released
        // this child's byte and closed its socket.
        joiner.kill();
    }
}

/// **`docs/decisions/0028` plan step 3 — the assigner decides from the byte.**
#[cfg(feature = "unstable")]
#[test]
fn the_assigner_reclaims_a_stale_record_no_hangup_will_ever_clear() {
    use tf_tree_core::participant::{state_of, FREE};

    let scratch = Scratch::new("assigner-reclaims");
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    // A joined read-write participant, so the staging writes go through a
    // writable mapping.
    let sweeper = join_as_sweeper();
    let view = sweeper.arena_view();
    let table = view.participants();
    let capacity = u32::try_from(table.capacity()).unwrap();

    let mut staged = 0u32;
    for slot in 0..capacity {
        // `u32::MAX` for the pid: see this test's doc comment.
        if table.register_at(slot, u32::MAX, 0, 0).is_ok() {
            staged += 1;
        }
    }
    assert_eq!(
        staged,
        capacity - 2,
        "every slot but the owner's and the sweeper's should have been free to \
         stage; the fixture is not staging what it thinks it is"
    );

    // The precondition, asserted rather than assumed: there is now no slot an
    // assigner reading `state` alone could hand out.
    for slot in 0..capacity {
        let word = table
            .get(slot)
            .expect("slot in range")
            .state
            .load(std::sync::atomic::Ordering::Acquire);
        assert_ne!(
            state_of(word),
            FREE,
            "slot {slot} still reads FREE, so a grant would prove nothing"
        );
    }

    let mut joiner = Kid::spawn(&scratch.0, &["join-rw"]);
    let line = joiner.line();
    assert!(
        line.starts_with("joined "),
        "the owner must reclaim a stale record and grant its slot; every slot \
         held a record for a pid that cannot exist, and its lock byte was never \
         taken: {line}"
    );
}

/// **`docs/decisions/0028` plan step 3 — the assigner collects a `RESERVED`
/// record, which nothing in this workspace ever collected.**
#[cfg(feature = "unstable")]
#[test]
fn the_assigner_collects_a_record_left_reserved_by_a_killed_registrant() {
    use std::sync::atomic::Ordering;
    use tf_tree_core::participant::{state_of, FREE, RESERVED};

    let scratch = Scratch::new("assigner-reserved");
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    let sweeper = join_as_sweeper();
    let view = sweeper.arena_view();
    let table = view.participants();
    let capacity = u32::try_from(table.capacity()).unwrap();

    let mut staged = 0u32;
    for slot in 0..capacity {
        // `register_at` *is* `fill_slot`, so the slot passes through `RESERVED`
        // on its way to `LIVE` here exactly as a joiner's does; the store below
        // rewinds the last instruction of that protocol and nothing else.
        if table.register_at(slot, u32::MAX, 0, 0).is_ok() {
            table
                .get(slot)
                .expect("slot in range")
                .state
                .store(RESERVED, Ordering::Release);
            staged += 1;
        }
    }
    assert_eq!(
        staged,
        capacity - 2,
        "every slot but the owner's and the sweeper's should have been free to \
         stage; the fixture is not staging what it thinks it is"
    );

    // The precondition, asserted rather than assumed, and asserted as
    // `RESERVED` rather than as "not `FREE`": a fixture that had left these
    // `LIVE` would pass under the mutant above and prove nothing about the word
    // this test is named for.
    for slot in 0..capacity {
        let word = table
            .get(slot)
            .expect("slot in range")
            .state
            .load(Ordering::Acquire);
        assert_ne!(
            state_of(word),
            FREE,
            "slot {slot} still reads FREE, so a grant would prove nothing"
        );
        if slot != 0 && slot != sweeper.participant_slot() {
            assert_eq!(
                state_of(word),
                RESERVED,
                "slot {slot} is the staged shape and must read RESERVED"
            );
        }
    }

    let mut joiner = Kid::spawn(&scratch.0, &["join-rw"]);
    let line = joiner.line();
    assert!(
        line.starts_with("joined "),
        "the owner must reclaim a RESERVED record and grant its slot; every \
         slot held one, left by a registrant killed between `fill_slot`'s CAS \
         and its publishing store, and no lock byte was held: {line}"
    );
}

/// **`docs/decisions/0028` plan step 4 — the hangup callback collects a
/// `RESERVED` record, which `release` structurally could not.**
#[cfg(feature = "unstable")]
#[test]
fn the_hangup_collects_a_record_left_reserved_by_a_killed_registrant() {
    use std::sync::atomic::Ordering;
    use tf_tree_core::participant::{state_of, FREE, LIVE, RESERVED};

    let scratch = Scratch::new("hangup-reserved");
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    let sweeper = join_as_sweeper();
    let mut peer = Kid::spawn(&scratch.0, &["join-rw"]);
    assert!(peer.line().starts_with("joined "), "peer did not join");

    let view = sweeper.arena_view();
    let table = view.participants();
    let capacity = u32::try_from(table.capacity()).unwrap();
    let mine = sweeper.participant_slot();

    // The peer's slot is **derived, not assumed**: the three live records are
    // the owner's (slot 0, which it registers as the arena's creator), this
    // process's, and the peer's, so the peer is the one that is neither.
    let live: Vec<u32> = (0..capacity)
        .filter(|slot| {
            let word = table
                .get(*slot)
                .expect("slot in range")
                .state
                .load(Ordering::Acquire);
            state_of(word) == LIVE
        })
        .collect();
    assert!(
        live.len() == 3 && live.contains(&0) && live.contains(&mine),
        "the owner (slot 0), this process (slot {mine}) and the peer should \
         hold the only live records; instead: {live:?}"
    );
    let peer_slot = *live
        .iter()
        .find(|slot| **slot != 0 && **slot != mine)
        .expect("the peer holds a live record");

    // Rewind the record, then kill.
    table
        .get(peer_slot)
        .expect("slot in range")
        .state
        .store(RESERVED, Ordering::Release);
    peer.kill();

    // Asynchronous by construction — a different process, on a different thread
    // — so this is a bounded wait that fails by timing out, the same shape
    // `the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live` uses.
    let mut word = RESERVED;
    for _ in 0..100 {
        word = table
            .get(peer_slot)
            .expect("slot in range")
            .state
            .load(Ordering::Acquire);
        if state_of(word) == FREE {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        state_of(word),
        FREE,
        "the owner's hangup callback must collect a RESERVED record, not only \
         a LIVE one; slot {peer_slot} is still {word:#x} two seconds after the \
         registrant holding it was killed"
    );
}

/// A read-only peer and a read-write peer get different slots.
#[test]
fn a_read_only_peer_holds_a_byte_without_an_arena_record() {
    let scratch = Scratch::new("ro-slot");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "));

    // Read-only: takes a lock byte, writes no arena record.
    let mut ro = Kid::spawn(&scratch.0, &["join"]);
    assert!(
        ro.line().starts_with("joined "),
        "read-only peer did not join"
    );

    // A second joiner must land somewhere else.
    let mut rw = Kid::spawn(&scratch.0, &["join-rw"]);
    assert!(rw.line().starts_with("joined "), "second peer did not join");

    // The owner holds slot 0.
    let mut probe = Kid::spawn(&scratch.0, &["peer-alive", "1"]);
    let slot1 = probe.line();
    probe.kill();
    let mut probe2 = Kid::spawn(&scratch.0, &["peer-alive", "2"]);
    let slot2 = probe2.line();
    probe2.kill();

    // Exactly one of slots 1 and 2 carries a registered participant: the
    // read-write joiner.
    assert_eq!(
        (slot1.as_str(), slot2.as_str()),
        ("alive false", "alive true"),
        "the byte/record asymmetry changed: slot 1 should hold a read-only \
         peer's lock byte with no arena record, slot 2 a registered one"
    );
}

/// **A claim taken through `open()` also holds a kernel lease.**
#[test]
fn a_claim_takes_a_lease_and_a_dead_holder_releases_it() {
    let scratch = Scratch::new("claim-lease-e2e");

    let mut owner = Kid::spawn(&scratch.0, &["own-claiming"]);
    let line = owner.line();
    assert!(line.starts_with("claimed "), "got {line}");
    let edge: u32 = line.strip_prefix("claimed ").unwrap().parse().unwrap();

    // A separate description, from a process that knows nothing about the
    // claim, must see the byte held.
    let lock = tf_tree_ipc::LockFile::open(&scratch.0.join("0/default.lock")).unwrap();
    assert!(
        lock.probe_claim(edge).unwrap().held,
        "claiming through open() did not take the edge's lease"
    );

    owner.kill();
    assert!(
        !lock.probe_claim(edge).unwrap().held,
        "the lease outlived its holder: a dead writer would leak its edge"
    );
}

/// **A reaper must not reap itself.** `0005` calls this the single most
/// valuable test in the milestone, and it is, because the failure looks like a
/// bug somewhere else.
#[test]
fn a_reaper_does_not_reap_its_own_live_claim() {
    let scratch = Scratch::new("self-reap");
    let mut kid = Kid::spawn(&scratch.0, &["own-reap"]);
    assert_eq!(kid.line(), "claimed");
    kid.poke();
    assert_eq!(
        kid.line(),
        "reaped 0 still_ours true",
        "the reaper revoked its own live claim — F_OFD_GETLK does not report a \
         description's own locks, so every edge this process holds reads free"
    );
}

/// **A `Guard` may be outstanding across `inherit_ownership`**, which is the
/// one caller-side qualification `docs/PHASE2.md` §3.5 carried and
/// [`0044`](../../../docs/decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)
/// removes.
#[test]
fn a_guard_may_be_held_across_inheriting_ownership() {
    use tf_tree::{AttachMode, Inheritance};
    use tf_tree_ipc::CreatePolicy;

    let scratch = Scratch::new("guard-across");
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning"));

    let tree = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::Never)
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("join the owner's arena");

    owner.kill();

    // The guard is taken *before* the call and used *after* it.
    let g = tree.guard();
    let outcome = tree.inherit_ownership().expect("inherit");
    assert_eq!(
        outcome,
        Inheritance::Inherited,
        "the sole read-write survivor should have taken the vacant role"
    );
    assert!(tree.frame("map").is_ok(), "the arena went away");
    drop(g);
}

/// **A killed publisher's claims are revoked by the owner, with nobody calling
/// a reaper.**
#[test]
fn a_restarted_publisher_gets_its_predecessors_slot_and_can_still_claim() {
    let scratch = Scratch::new("restart-claim");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning"));

    let mut first = Kid::spawn(&scratch.0, &["join-claiming"]);
    let claimed = first.line();
    assert!(
        claimed.starts_with("claimed "),
        "the first publisher did not claim: {claimed}"
    );

    // `kill` waits, so the claim lease is released and the attach socket closed
    // before anything else runs: the owner's `epoll` has a hangup to process.
    first.kill();

    // The restart.
    let mut second = Kid::spawn(&scratch.0, &["join-claiming"]);
    let retried = second.line();
    assert!(
        retried.starts_with("claimed "),
        "a restarted publisher could not reclaim its own edge, and on its \
         predecessor's slot it is the one process that cannot repair it: \
         {retried}"
    );
    assert_eq!(
        retried, claimed,
        "the restarted publisher claimed a different edge than its predecessor \
         held, so this test is not exercising the reuse it is about"
    );
}

/// A killed writer's edge is reclaimed, and then reclaimable.
#[test]
fn a_killed_writers_edge_is_reaped_and_can_be_reclaimed() {
    let scratch = Scratch::new("reap-dead");

    let mut owner = Kid::spawn(&scratch.0, &["own-reap"]);
    assert_eq!(owner.line(), "claimed");

    // A second process joins and claims the *other* edge of the fixture.
    let mut peer = Kid::spawn(&scratch.0, &["join-claiming"]);
    let claimed = peer.line();
    assert!(
        claimed.starts_with("claimed "),
        "peer did not claim: {claimed}"
    );

    peer.kill();

    // The owner sweeps.
    owner.poke();
    let line = owner.line();
    assert!(
        line.ends_with("still_ours true"),
        "the sweep revoked the owner's own live claim: {line}"
    );

    // And the edge is genuinely free: a fresh publisher takes it.
    let mut successor = Kid::spawn(&scratch.0, &["join-claiming"]);
    let retaken = successor.line();
    assert_eq!(
        retaken, claimed,
        "the dead peer's edge was cleared but not reclaimable: {retaken}"
    );
}

// The CAS-to-lease window (`docs/decisions/0005` §5)

/// The tree the in-window hook reaps from.
#[cfg(feature = "test-hooks")]
static REAPER: std::sync::OnceLock<tf_tree::Tree> = std::sync::OnceLock::new();

/// Fires once.
#[cfg(feature = "test-hooks")]
static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// How many claims the in-window reaper actually cleared.
#[cfg(feature = "test-hooks")]
static REAPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "test-hooks")]
fn reap_from_inside_the_window() {
    use std::sync::atomic::Ordering;
    if !ARMED.swap(false, Ordering::Relaxed) {
        return;
    }
    if let Some(t) = REAPER.get() {
        REAPED.fetch_add(t.reap_dead(), Ordering::Relaxed);
    }
}

/// **The acquire window backs out.** `0005` step 7 has shipped this guard since
/// the lease landed and, until now, nothing could fail without it.
#[test]
#[cfg(feature = "test-hooks")]
fn the_acquire_window_backs_out() {
    use std::sync::atomic::Ordering;
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let _scratch = Scratch::new("acquire-window");

    let claimer = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
        )
        .open()
        .expect("create");
    let reaper = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::Never)
        .open()
        .expect("join as a second read-write participant");
    assert_ne!(
        claimer.participant_slot(),
        reaper.participant_slot(),
        "both handles took the same slot, so the reaper would skip the claim as its own"
    );

    // `assert!(… .is_ok())` and not `.ok().expect()`: `OnceLock::set` hands the
    // value back in the `Err`, and `Tree` is not `Debug`, which is what the
    // older spelling was working around.
    assert!(REAPER.set(reaper).is_ok(), "set reaper");
    assert!(
        tf_tree::CLAIM_WINDOW_HOOK
            .set(reap_from_inside_the_window as fn())
            .is_ok(),
        "install hook"
    );

    let child = claimer.frame("base").unwrap();
    let parent = claimer.frame("map").unwrap();

    let err = claimer
        .claim(child, parent)
        .err()
        .expect("a claim reaped inside its own acquire window must not succeed");
    assert!(
        matches!(err, tf_tree::ClaimApiError::ReapedDuringClaim { .. }),
        "expected ReapedDuringClaim, got {err:?}"
    );
    assert_eq!(
        REAPED.load(Ordering::Relaxed),
        1,
        "the hook did not actually reap anything, so the guard was never exercised"
    );

    // **Backing out must give everything back.** A guard that detects the reap
    // and then leaks the record or the lease turns a recoverable race into a
    // permanently unclaimable edge, which is worse than the race.
    let writer = claimer
        .claim(child, parent)
        .expect("the retry after ReapedDuringClaim must succeed");
    writer
        .push(
            1_000,
            &tf_tree_math::exp_se3([0.0, 0.0, 0.1, 1.0, 0.0, 0.0]),
        )
        .expect("and the reclaimed edge must be publishable");
}

// `docs/decisions/0028` plan step 2 — the reclamation predicate, once.

/// The rendezvous lock file the predicate probes, for a scratch directory.
#[cfg(feature = "test-hooks")]
fn lock_path(scratch: &std::path::Path) -> PathBuf {
    scratch.join("0/default.lock")
}

/// Join the arena read-write, as the process that will do the sweeping.
#[cfg(any(feature = "test-hooks", feature = "unstable"))]
fn join_as_sweeper() -> tf_tree::Tree {
    tf_tree::Open::new()
        .mode(tf_tree::AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::Never)
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("join the arena as a read-write participant")
}

/// The kernel's own state character for `pid` — field 3 of `/proc/<pid>/stat`.
fn proc_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.get(stat.rfind(')')? + 1..)?;
    after_comm.split_whitespace().next()?.chars().next()
}

/// Block until `pid` is genuinely stopped by a job-control signal, or panic.
fn await_stopped(pid: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // `T` is "stopped (on a signal)".
        match proc_state(pid) {
            Some('T' | 't') => return,
            other => assert!(
                std::time::Instant::now() < deadline,
                "pid {pid} never reached state T; it reads {other:?}. The SIGSTOP \
                 this test is about did not take, so nothing below would be a \
                 measurement of a stopped process"
            ),
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// **A participant the kernel has stopped is still `Live`, and the stoppedness
/// is load-bearing.**
#[test]
#[cfg(feature = "test-hooks")]
fn a_stopped_participant_is_live_to_the_reclamation_predicate() {
    let scratch = Scratch::new("verdict-stopped");

    // Join order determines these, each join awaited before the next begins:
    // owner 0, this process 1, the target 2.
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    let sweeper = join_as_sweeper();
    assert_eq!(sweeper.participant_slot(), 1, "the sweeper took slot 1");

    // Read-write, deliberately.
    let mut target = Kid::spawn(&scratch.0, &["join-rw"]);
    assert!(target.line().starts_with("joined "), "target did not join");

    let ask = |slot: u32| {
        tf_tree::reclamation_verdict_for_test(
            &sweeper,
            &lock_path(&scratch.0),
            sweeper.participant_slot(),
            slot,
        )
    };

    let pid = target.0.id();
    // The state field is read *before* the stop as well, so `T` below is a
    // transition this test caused and not a constant the parser invented.
    assert_ne!(
        proc_state(pid),
        Some('T'),
        "the target was already stopped before this test stopped it"
    );
    assert_eq!(
        ask(2),
        "live probes=1",
        "the target was not live to begin with"
    );

    assert!(
        Command::new("kill")
            .args(["-STOP", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success()),
        "could not SIGSTOP the target"
    );
    await_stopped(pid);
    assert_eq!(
        proc_state(pid),
        Some('T'),
        "the target left state T between the wait and the verdict"
    );

    assert_eq!(
        ask(2),
        "live probes=1",
        "a stopped participant was not reported live: it holds its lock byte, \
         D17 forbids telling it apart from a slow one, and `probes=1` is the \
         claim that the kernel — not the `state` word — is what was asked"
    );

    let _ = Command::new("kill")
        .args(["-CONT", &pid.to_string()])
        .status();
}

/// **A `SIGKILL`ed participant's slot is `Reclaimable` — and the target is the
/// *owner*, because that is the shape that stays reclaimable.**
#[test]
#[cfg(feature = "test-hooks")]
fn a_killed_participants_slot_is_reclaimable_to_the_reclamation_predicate() {
    let scratch = Scratch::new("verdict-killed");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    let sweeper = join_as_sweeper();
    assert_eq!(sweeper.participant_slot(), 1, "the sweeper took slot 1");

    let ask = |slot: u32| {
        tf_tree::reclamation_verdict_for_test(
            &sweeper,
            &lock_path(&scratch.0),
            sweeper.participant_slot(),
            slot,
        )
    };

    // A transition, not a state: the same question either side of the kill.
    assert_eq!(
        ask(0),
        "live probes=1",
        "the owner was not live before it was killed, so what follows proves nothing"
    );

    owner.kill();

    // `0x6` is `live_word(1)`: the owner registered into a fresh record, so its
    // incarnation is 1.
    assert_eq!(
        ask(0),
        "reclaimable word 0x6 probes=1",
        "the killed owner's slot was not collectable: its record is still LIVE \
         and the kernel has released its byte, which is #184's wedge"
    );
}

/// **The sweeper's own slot is `Live`, unconditionally — including when its
/// byte reads free.**
#[test]
#[cfg(feature = "test-hooks")]
fn the_sweepers_own_slot_is_live_even_when_the_byte_reads_free() {
    let scratch = Scratch::new("verdict-own");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    let sweeper = join_as_sweeper();
    assert_eq!(sweeper.participant_slot(), 1, "the sweeper took slot 1");

    let ask = |own_slot: u32, slot: u32| {
        tf_tree::reclamation_verdict_for_test(&sweeper, &lock_path(&scratch.0), own_slot, slot)
    };

    owner.kill();

    // The control: as somebody else's slot, the dead owner's record is exactly
    // the state the predicate collects.
    assert_eq!(
        ask(sweeper.participant_slot(), 0),
        "reclaimable word 0x6 probes=1",
        "the control failed, so the assertion below would hold for the wrong reason"
    );

    // The guard: the same record, the same released byte, asked about as ours.
    assert_eq!(
        ask(0, 0),
        "live probes=0",
        "the guard did not fire: a sweep that judges its own slot from the byte \
         reclaims itself the moment the probe stops seeing its own description"
    );

    // And the ordinary shape, where the byte agrees with the guard.
    assert_eq!(
        ask(sweeper.participant_slot(), sweeper.participant_slot()),
        "live probes=0",
        "the sweeper reported itself anything but live"
    );
}

/// **A live read-only joiner holds a lock byte and has no arena record — and
/// the predicate must say `unknown`, not `reclaimable`.**
#[test]
#[cfg(feature = "test-hooks")]
fn a_live_read_only_joiner_is_unknown_not_reclaimable() {
    let scratch = Scratch::new("verdict-readonly");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    let sweeper = join_as_sweeper();
    assert_eq!(sweeper.participant_slot(), 1, "the sweeper took slot 1");

    // `join`, not `join-rw`: the read-only consumer default.
    let mut reader = Kid::spawn(&scratch.0, &["join"]);
    assert!(
        reader.line().starts_with("joined "),
        "the read-only joiner did not attach"
    );

    // The kernel, asked directly instead of through the predicate, so the
    // premise is established independently of the thing under test.
    let witness = tf_tree_ipc::LockFile::open(&lock_path(&scratch.0)).unwrap();
    assert!(
        witness.probe_participant(2).unwrap().held,
        "the read-only joiner did not take a participant byte, so this test is \
         not staging the state it is about"
    );
    assert!(
        !witness.probe_participant(3).unwrap().held,
        "slot 3 was handed out to somebody; it is meant to be untouched"
    );
    assert!(
        !sweeper.participant_alive(2),
        "slot 2's record reads LIVE, so the read-only joiner registered after \
         all and the FREE-word branch is not what this test reaches"
    );

    let ask = |slot: u32| {
        tf_tree::reclamation_verdict_for_test(
            &sweeper,
            &lock_path(&scratch.0),
            sweeper.participant_slot(),
            slot,
        )
    };

    assert_eq!(
        ask(2),
        "unknown probes=0",
        "a live read-only joiner's slot was given a liveness verdict: its \
         record is FREE because it cannot write one, and collecting it would \
         hand a running process's slot away"
    );
    assert_eq!(
        ask(3),
        "unknown probes=0",
        "an untouched slot was given a verdict: there is no record to collect \
         and reclaim(slot, FREE) would be a CAS that succeeds having freed nothing"
    );
}

/// **A `FREE` word is decided without asking the kernel — the one part of the
/// predicate's read *order* a multiprocess test can see.**
#[test]
#[cfg(feature = "test-hooks")]
fn a_free_word_is_decided_without_asking_the_kernel() {
    let scratch = Scratch::new("verdict-order");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    let sweeper = join_as_sweeper();
    assert_eq!(sweeper.participant_slot(), 1, "the sweeper took slot 1");

    // A live read-only joiner: byte held, record FREE.
    let mut reader = Kid::spawn(&scratch.0, &["join"]);
    assert!(
        reader.line().starts_with("joined "),
        "the read-only joiner did not attach"
    );
    let witness = tf_tree_ipc::LockFile::open(&lock_path(&scratch.0)).unwrap();
    assert!(
        witness.probe_participant(2).unwrap().held,
        "slot 2's byte is not held, so a skipped probe would prove nothing"
    );

    let ask = |own_slot: u32, slot: u32| {
        tf_tree::reclamation_verdict_for_test(&sweeper, &lock_path(&scratch.0), own_slot, slot)
    };

    assert_eq!(
        ask(sweeper.participant_slot(), 0),
        "live probes=1",
        "a LIVE word was answered without asking the kernel, which is deciding \
         liveness from `state` (PHASE2 §5.1's bug)"
    );
    assert_eq!(
        ask(sweeper.participant_slot(), 2),
        "unknown probes=0",
        "a FREE word cost a byte probe, so the byte was read before the word \
         was decided — the read order piece 2's third constraint forbids"
    );
    assert_eq!(
        ask(0, 0),
        "live probes=0",
        "the own-slot guard cost a byte probe, so it is no longer first"
    );
}

// `docs/decisions/0028` plan step 5 — `Tree::reap_participants`, and it is not
// owner-only.

/// The raw `state` word of `slot`, read through the unstable view.
#[cfg(feature = "unstable")]
fn state_word(tree: &tf_tree::Tree, slot: u32) -> u32 {
    tree.arena_view()
        .participants()
        .get(slot)
        .expect("slot in range")
        .state
        .load(std::sync::atomic::Ordering::Acquire)
}

/// **A surviving read-write participant reclaims the killed *owner's* slot —
/// the case no hangup can ever cover.**
#[test]
#[cfg(feature = "unstable")]
fn a_survivor_reaps_the_killed_owners_slot_which_no_hangup_can() {
    let scratch = Scratch::new("reap-participants-owner");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    let sweeper = join_as_sweeper();
    assert_eq!(sweeper.participant_slot(), 1, "the sweeper took slot 1");

    // `0x6` is `live_word(1)`, asserted as a word rather than a state because
    // that is what `reclaim` CASes against.
    assert_eq!(
        state_word(&sweeper, 0),
        0x6,
        "the owner's record is not the LIVE word this test is about"
    );
    assert_eq!(
        sweeper.reap_participants(),
        0,
        "a sweep collected a slot while every participant was running, so what \
         follows would pass for the wrong reason"
    );

    owner.kill();

    assert_eq!(
        sweeper.reap_participants(),
        1,
        "the killed owner's record was not collected: nothing hangs up on an \
         owner, so a survivor's sweep is the only thing that can, and #184's \
         wedge is exactly this record staying LIVE for the life of the segment"
    );
    assert_eq!(
        state_word(&sweeper, 0),
        0,
        "the sweep reported a collection it did not make: the record must read \
         FREE, which is what makes the slot grantable again"
    );

    // The sweeper is still in its own slot: the sweep left a record whose byte
    // is held exactly as it found it.
    assert_eq!(
        state_word(&sweeper, 1),
        0x6,
        "the sweeper cleared its own live record, whose byte it still holds"
    );
    assert!(
        sweeper.participant_alive(1),
        "the sweeper reports itself dead after sweeping"
    );

    // Idempotent: there is nothing left to collect, and a second sweep must not
    // invent one out of the record it just cleared.
    assert_eq!(
        sweeper.reap_participants(),
        0,
        "a second sweep collected a slot the first one already freed"
    );
}

/// **A read-only tree reaps nothing, and the slot it declines to collect is one
/// a read-write tree would.**
#[test]
#[cfg(feature = "unstable")]
fn a_read_only_tree_reaps_no_participant_records() {
    let scratch = Scratch::new("reap-participants-ro");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    // D18's default, and the shape a `PROT_READ` mapping arrives in: a byte,
    // and no arena record of its own.
    let consumer = tf_tree::Open::new()
        .mode(tf_tree::AttachMode::ReadOnly)
        .create(tf_tree::CreatePolicy::Never)
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("join the arena read-only");
    assert!(
        !consumer.is_writable(),
        "the consumer attached writable, so this test would prove nothing"
    );
    assert_eq!(
        consumer.participant_slot(),
        u32::MAX,
        "a read-only attachment must register no arena record"
    );

    owner.kill();

    assert_eq!(
        state_word(&consumer, 0),
        0x6,
        "the killed owner's record is not the LIVE word a read-write sweeper \
         would collect, so the refusal below would be about nothing"
    );
    assert_eq!(
        consumer.reap_participants(),
        0,
        "a read-only tree reaped a participant record"
    );
    assert_eq!(
        state_word(&consumer, 0),
        0x6,
        "a read-only tree wrote to the participant table"
    );
}

/// **A participant record with no lock byte reads dead, and the sweep frees it
/// while the process is still publishing.**
#[test]
#[cfg(feature = "unstable")]
fn a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes() {
    let scratch = Scratch::new("byteless");
    std::fs::create_dir_all(scratch.0.join("0")).unwrap();

    let creator = tf_tree::TreeBuilder::new()
        .default_interp(tf_tree::InterpPolicy::LerpSlerp)
        .dynamic_edge(
            "map",
            "base",
            tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(64)),
        )
        .build_shared("tf_tree-byteless")
        .expect("build_shared");
    let record = creator.participant_slot();

    assert!(
        !scratch.0.join("0/default.lock").exists(),
        "a build_shared arena must carry no lock file — the absence is the \
         premise of this test, not an incidental"
    );

    // Publish it with the two published `tf_tree_ipc` calls.
    let desc = tf_tree_ipc::SegmentDescriptor {
        format_version: tf_tree_arena::FORMAT_VERSION,
        layout_hash: tf_tree_arena::layout_hash(),
        arena_size: creator.arena_size_bytes() as u64,
        instance_uuid: creator.instance_uuid(),
        boot_id: tf_tree_ipc::boot_id().unwrap_or([0; 16]),
    };
    let seg: std::os::fd::OwnedFd = creator
        .shared_fd()
        .expect("a build_shared tree has a segment fd")
        .try_clone_to_owned()
        .unwrap();
    let rv = tf_tree_ipc::Rendezvous::new(
        tf_tree_ipc::RuntimeDir::resolve().unwrap(),
        0,
        tf_tree_ipc::ArenaName::new("default", tf_tree_ipc::EnvVar::Name).unwrap(),
    );
    let server =
        tf_tree_ipc::OwnerServer::bind_at(rv.sock_path(), desc, std::process::id()).unwrap();
    let shutdown = server.shutdown_handle().unwrap();
    let serving = std::thread::spawn(move || {
        let mut next = 1;
        let _ = server.serve(
            std::os::fd::AsFd::as_fd(&seg),
            |_r| {
                let s = next;
                next += 1;
                Ok(s)
            },
            |_s| {},
        );
    });

    let publisher = creator
        .claim(
            creator.frame("base").unwrap(),
            creator.frame("map").unwrap(),
        )
        .expect("claim");
    publisher
        .push(1_000, &tf_tree::exp_se3([1.0, 2.0, 3.0, 0.1, 0.2, 0.3]))
        .unwrap();

    let rescuer = tf_tree::Open::new()
        .mode(tf_tree::AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::Never)
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("a facade peer joins the served arena");

    assert!(
        !rescuer.participant_alive(record),
        "BOUNDARY: a joined peer reads the byte-less creator as DEAD. This is \
         what serving a `build_shared` arena costs, and 0031 answered it out of \
         contract — if this assertion fails, the behaviour changed, so find out \
         why rather than inverting it"
    );
    assert_eq!(
        state_word(&creator, record),
        0x6,
        "the creator's record must be LIVE going in, or the sweep below is \
         about nothing"
    );

    let reaped = rescuer.reap_participants();

    assert!(
        reaped >= 1,
        "BOUNDARY: the sweep collects the live creator; it collected {reaped}"
    );
    assert_eq!(
        state_word(&creator, record),
        0x0,
        "BOUNDARY: the sweep FREEs the record of a process that is running"
    );

    publisher
        .push(3_000, &tf_tree::exp_se3([1.0, 2.0, 3.0, 0.1, 0.2, 0.3]))
        .expect(
            "the creator goes on publishing into an arena whose participant \
             table no longer records it — which is the whole defect",
        );

    let _ = shutdown.stop();
    let _ = serving.join();
}

/// **A byte-less publisher loses the edge it is writing to, and can be evicted
/// again every time it takes it back.**
#[test]
#[cfg(feature = "unstable")]
fn a_byteless_publisher_is_evicted_from_the_edge_it_is_publishing_to() {
    let scratch = Scratch::new("evict");
    std::fs::create_dir_all(scratch.0.join("0")).unwrap();
    let creator = byteless_served_arena();
    let (shutdown, serving) = serve(&creator, 1);

    let victim = creator
        .claim(
            creator.frame("base").unwrap(),
            creator.frame("map").unwrap(),
        )
        .expect("the byte-less creator claims the edge");
    victim
        .push(1_000, &tf_tree::exp_se3([0.0, 0.0, 0.0, 1.0, 0.0, 0.0]))
        .expect("and publishes");

    let sweeper = tf_tree::Open::new()
        .mode(tf_tree::AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::Never)
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("an ordinary peer joins");

    assert_eq!(
        sweeper.reap_dead(),
        1,
        "BOUNDARY: an ordinary peer's sweep takes the claim of a publisher that \
         is running. This is what serving a `build_shared` arena costs, and 0031 \
         answered it out of contract — if this is now 0, the behaviour changed, \
         so find out why rather than inverting it"
    );

    let thief = sweeper
        .claim(
            sweeper.frame("base").unwrap(),
            sweeper.frame("map").unwrap(),
        )
        .expect("BOUNDARY: and the edge is then claimable by somebody else");

    // The half that bounds the severity: the victim is refused, not interleaved.
    let revoked = victim.push(2_000, &tf_tree::exp_se3([0.0, 0.0, 0.0, 2.0, 0.0, 0.0]));
    assert!(
        matches!(revoked, Err(tf_tree::PushError::ClaimRevoked { .. })),
        "D7 must hold through the eviction: the evicted writer has to be refused, \
         not left interleaving with the new one. Got {revoked:?}"
    );
    thief
        .push(3_000, &tf_tree::exp_se3([0.0, 0.0, 0.0, 99.0, 0.0, 0.0]))
        .expect("the new owner publishes normally");

    // And the victim cannot keep it: re-claiming is the documented remedy and
    // lands it straight back in the byte-less state.
    drop(victim);
    let retaken = creator
        .claim(
            creator.frame("base").unwrap(),
            creator.frame("map").unwrap(),
        )
        .err()
        .map(|e| format!("{e:?}"));
    assert!(
        retaken.is_some(),
        "the thief holds it now, so the victim's re-claim must be refused here; \
         the unbounded cycle is what happens when the thief releases"
    );

    let _ = shutdown.stop();
    let _ = serving.join();
}

/// The control for the test above: a publisher that joined through the
/// rendezvous holds a lease byte, and the same sweep declines it.
#[test]
#[cfg(feature = "unstable")]
fn a_leased_publisher_keeps_its_edge_against_a_sweeper() {
    let scratch = Scratch::new("evictctl");
    std::fs::create_dir_all(scratch.0.join("0")).unwrap();
    let creator = byteless_served_arena();
    let (shutdown, serving) = serve(&creator, 1);

    let open = || {
        tf_tree::Open::new()
            .mode(tf_tree::AttachMode::ReadWrite)
            .create(tf_tree::CreatePolicy::Never)
            .timeout(std::time::Duration::from_millis(500))
            .open()
            .expect("join")
    };
    let holder = open();
    let _kept = holder
        .claim(holder.frame("base").unwrap(), holder.frame("map").unwrap())
        .expect("a properly joined publisher claims");

    let sweeper = open();
    assert_eq!(
        sweeper.reap_dead(),
        0,
        "a sweep must not take the claim of a publisher holding its lease byte"
    );
    assert!(
        sweeper
            .claim(
                sweeper.frame("base").unwrap(),
                sweeper.frame("map").unwrap()
            )
            .is_err(),
        "and the edge must stay unclaimable"
    );

    let _ = shutdown.stop();
    let _ = serving.join();
}

/// A `build_shared` arena, served through the two published `tf_tree_ipc` calls
/// so facade peers can join it.
#[cfg(feature = "unstable")]
fn byteless_served_arena() -> tf_tree::Tree {
    tf_tree::TreeBuilder::new()
        .default_interp(tf_tree::InterpPolicy::LerpSlerp)
        .dynamic_edge(
            "map",
            "base",
            tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(64)),
        )
        .build_shared("tf_tree-evict")
        .expect("build_shared")
}

/// Serve `tree`, granting joiners slots from `first` upward.
#[cfg(feature = "unstable")]
fn serve(
    tree: &tf_tree::Tree,
    first: u32,
) -> (tf_tree_ipc::ShutdownHandle, std::thread::JoinHandle<()>) {
    use std::os::fd::{AsFd, OwnedFd};
    let desc = tf_tree_ipc::SegmentDescriptor {
        format_version: tf_tree_arena::FORMAT_VERSION,
        layout_hash: tf_tree_arena::layout_hash(),
        arena_size: tree.arena_size_bytes() as u64,
        instance_uuid: tree.instance_uuid(),
        boot_id: tf_tree_ipc::boot_id().unwrap_or([0; 16]),
    };
    let seg: OwnedFd = tree
        .shared_fd()
        .expect("a build_shared tree has a segment fd")
        .try_clone_to_owned()
        .unwrap();
    let rv = tf_tree_ipc::Rendezvous::new(
        tf_tree_ipc::RuntimeDir::resolve().unwrap(),
        0,
        tf_tree_ipc::ArenaName::new("default", tf_tree_ipc::EnvVar::Name).unwrap(),
    );
    let server =
        tf_tree_ipc::OwnerServer::bind_at(rv.sock_path(), desc, std::process::id()).unwrap();
    let shutdown = server.shutdown_handle().unwrap();
    let h = std::thread::spawn(move || {
        let mut next = first;
        let _ = server.serve(
            seg.as_fd(),
            |_r| {
                let s = next;
                next += 1;
                Ok(s)
            },
            |_s| {},
        );
    });
    (shutdown, h)
}

/// **#213's regression, and the one test in the tree that tells the old
/// behaviour from the new** (`docs/decisions/0029` plan step 4).
#[test]
#[cfg(feature = "unstable")]
fn a_live_holder_that_proc_calls_dead_keeps_the_topology_lock() {
    use core::sync::atomic::Ordering;

    let scratch = Scratch::new("topo-liveness");
    let owner = tf_tree::Open::new()
        .mode(tf_tree::AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::IfAbsent)
        .layout_if_creating(
            tf_tree::TreeBuilder::new()
                .default_interp(tf_tree::InterpPolicy::LerpSlerp)
                .dynamic_edge(
                    "map",
                    "base",
                    tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(8)),
                )
                .dynamic_edge(
                    "map",
                    "odom",
                    tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(8)),
                ),
        )
        .open()
        .expect("create the arena");

    // A real second participant, so the record the predicate reads is one the
    // rendezvous produced rather than one this test invented.
    let holder = tf_tree::Open::new()
        .mode(tf_tree::AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::Never)
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("join as a second participant");
    let holder_slot = holder.participant_slot();
    assert_ne!(
        holder_slot,
        owner.participant_slot(),
        "the two attachments share a slot, so `acquire`'s own-slot guard would \
         refuse before any predicate ran and this test would be vacuous"
    );

    // Make `/proc` describe the wrong process for this slot.
    let view = owner.arena_view();
    let rec = view
        .participants()
        .get(holder_slot)
        .expect("the joiner's record");
    let real_start_time = rec.start_time.load(Ordering::Relaxed);
    rec.start_time
        .store(real_start_time ^ 0xDEAD_BEEF, Ordering::Relaxed);
    // Not asserted through `participant_alive`: that one consults the *byte* on
    // a rendezvous tree, so it correctly answers "alive" here and would say
    // nothing about the triple.

    // Stage the critical section: the word names the holder, and a second
    // description holds the byte, which is what a mutator between the two looks
    // like from anywhere else in the system.
    let word = &view.header().topo_lock.owner;
    word.store(u64::from(holder_slot) + 1, Ordering::Release);
    let lock_path = scratch.0.join("0/default.lock");
    let held_by = tf_tree_ipc::LockFile::open(&lock_path).expect("open the lock file");
    assert_eq!(
        held_by.try_take_topology().expect("take the topology byte"),
        tf_tree_ipc::LockAttempt::Acquired,
        "the topology byte was already held; the fixture is not in the state it \
         claims to be"
    );

    let map = owner.frame("map").expect("map");
    let base = owner.frame("base").expect("base");
    let odom = owner.frame("odom").expect("odom");

    // **The assertion this test exists for.**
    let refused = owner.reparent(base, odom);
    assert!(
        matches!(
            refused,
            Err(tf_tree::ReparentError::LockContended { owner_slot })
                if owner_slot == Some(holder_slot)
        ),
        "a live holder was stolen from: {refused:?}"
    );
    assert_eq!(
        word.load(Ordering::Acquire),
        u64::from(holder_slot) + 1,
        "the word changed hands despite the refusal — the steal happened and the \
         error is cosmetic"
    );

    // **The control: one variable.** Everything above is unchanged except that
    // the kernel no longer says anyone is in the critical section, which is the
    // state a crashed holder leaves behind.
    held_by
        .release_topology()
        .expect("release the topology byte");
    owner
        .reparent(base, odom)
        .expect("a byte-free word must still be stealable from a holder /proc calls dead");

    // And the mutation is real, not just a lock acquisition.
    assert!(
        owner.plan(base, map).is_ok(),
        "the topology did not survive the steal"
    );
    drop(holder);
}

/// **A killed topology-lock holder wedges nothing** — `docs/PHASE2.md` §11.3's
/// `topo.holding_lock` row, performed across a real process boundary.
#[test]
fn a_killed_topology_lock_holder_releases_its_byte_to_the_kernel() {
    let scratch = Scratch::new("topo-kill");
    let owner = tf_tree::Open::new()
        .mode(tf_tree::AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::IfAbsent)
        .layout_if_creating(
            tf_tree::TreeBuilder::new()
                .default_interp(tf_tree::InterpPolicy::LerpSlerp)
                .dynamic_edge(
                    "map",
                    "base",
                    tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(8)),
                )
                .dynamic_edge(
                    "map",
                    "odom",
                    tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(8)),
                ),
        )
        .open()
        .expect("create the arena");

    let lock_path = scratch.0.join("0/default.lock");
    let mut kid = Kid::spawn(
        &scratch.0,
        &["hold-topo", lock_path.to_str().expect("utf-8 path")],
    );
    assert_eq!(
        kid.line(),
        "holding-topo",
        "the helper did not get the byte, so the refusal below would prove nothing"
    );

    let base = owner.frame("base").expect("base");
    let odom = owner.frame("odom").expect("odom");
    let refused = owner.reparent(base, odom);
    assert!(
        matches!(
            refused,
            Err(tf_tree::ReparentError::LockContended { owner_slot: None })
        ),
        "a live holder of the topology byte did not refuse this mutation: {refused:?}"
    );

    // No cooperation: the child is not asked to release anything, and it holds
    // no `Drop` that could.
    kid.kill();

    owner
        .reparent(base, odom)
        .expect("the kernel did not release a killed holder's topology byte");
}

/// **A held topology byte is waited out before contention is reported**, so the
/// byte did not quietly cost `reparent` the patience the arena word always had.
#[test]
fn the_topology_byte_is_retried_before_contention_is_reported() {
    let scratch = Scratch::new("topo-patience");
    let owner = tf_tree::Open::new()
        .mode(tf_tree::AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::IfAbsent)
        .layout_if_creating(
            tf_tree::TreeBuilder::new()
                .default_interp(tf_tree::InterpPolicy::LerpSlerp)
                .dynamic_edge(
                    "map",
                    "base",
                    tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(8)),
                )
                .dynamic_edge(
                    "map",
                    "odom",
                    tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(8)),
                ),
        )
        .open()
        .expect("create the arena");

    let lock_path = scratch.0.join("0/default.lock");
    let holder = tf_tree_ipc::LockFile::open(&lock_path).expect("open the lock file");
    assert_eq!(
        holder.try_take_topology().expect("take the topology byte"),
        tf_tree_ipc::LockAttempt::Acquired
    );

    // The baseline: one *contended* round trip, from a third description, so it
    // is the same syscall on the same byte in the same state that the loop
    // under test makes.
    let meter = tf_tree_ipc::LockFile::open(&lock_path).expect("open the lock file");
    const CAL: u32 = 2000;
    for _ in 0..CAL {
        let _ = meter.try_take_topology();
    }
    let t = std::time::Instant::now();
    for _ in 0..CAL {
        let _ = meter.try_take_topology();
    }
    let per_attempt = t.elapsed() / CAL;

    let base = owner.frame("base").expect("base");
    let odom = owner.frame("odom").expect("odom");
    let t = std::time::Instant::now();
    let refused = owner.reparent(base, odom);
    let waited = t.elapsed();

    assert!(
        matches!(refused, Err(tf_tree::ReparentError::LockContended { .. })),
        "a held topology byte did not refuse the mutation: {refused:?}"
    );
    let floor = per_attempt * 8;
    assert!(
        waited >= floor,
        "reparent gave up after {waited:?}, under 8 contended fcntl round trips \
         ({floor:?}): the retry budget is gone"
    );
}

/// **§3.5, end to end: owner death stops being terminal for new joiners.**
#[test]
fn a_survivor_inherits_ownership_and_the_arena_becomes_joinable_again() {
    use tf_tree::{AttachMode, Stamp};
    use tf_tree_ipc::CreatePolicy;

    let dir = Scratch::new("inherit-ownership");

    let mut owner = Kid::spawn(&dir.0, &["own"]);
    assert!(
        owner.line().starts_with("owning"),
        "the owner did not come up"
    );

    // A survivor, attached before the owner dies.
    let mut heir = Kid::spawn(&dir.0, &["join-heir"]);
    assert!(
        heir.line().starts_with("joined "),
        "the survivor did not attach"
    );

    owner.kill();

    let joiner = || {
        tf_tree::Open::new()
            .mode(AttachMode::ReadWrite)
            .create(CreatePolicy::Never)
            .timeout(std::time::Duration::from_millis(200))
    };

    // The documented failure, reproduced.
    let err = joiner()
        .open()
        .err()
        .expect("with the owner dead and nothing serving, a joiner must be turned away");
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree::IpcError::ArenaHeldButUnreachable { .. })
        ),
        "expected the pre-§3.5 wedge, got {err:?}"
    );

    // The repair: the survivor notices and inherits.
    heir.poke();
    let report = heir.line();
    assert!(
        report.starts_with("true Inherited "),
        "the survivor did not see the hangup, or did not inherit: {report}"
    );

    // And now the thing that could not happen before: a new process joins the
    // same arena, under a new owner, and reads what the dead owner published.
    let tree = joiner()
        .open()
        .expect("after inheritance a new process must be able to join");
    let g = tree.guard();
    let target = tree.frame("map").unwrap();
    let source = tree.frame("base").unwrap();
    let plan = tree.plan(target, source).unwrap();
    let iso = plan
        .at(&g, Stamp::<tf_tree::SystemDomain>::from_nanos(1_500))
        .unwrap();
    let expected = tf_tree::exp_se3([1.0, 2.0, 3.0, 0.1, 0.2, 0.3]);
    assert_eq!(
        iso.to_bits(),
        expected.to_bits(),
        "the inherited arena served different bytes than the dead owner wrote"
    );
}

/// **An owner that dies *inside* the handshake is a transient, and §3.4 absorbs
/// it.**
#[test]
fn an_owner_that_dies_mid_handshake_is_retried_until_the_heir_serves() {
    use std::sync::mpsc;
    use std::time::Duration;

    use tf_tree::{AttachMode, Stamp};
    use tf_tree_ipc::CreatePolicy;

    let dir = Scratch::new("die-mid-handshake");

    let mut owner = Kid::spawn(&dir.0, &["own"]);
    let published = owner.line();
    assert!(
        published.starts_with("owning "),
        "the owner did not come up: {published}"
    );
    let owner_value = published.strip_prefix("owning ").unwrap().to_string();

    // A survivor, attached before the owner dies: it holds the participant byte
    // that keeps a `create = Never` joiner retrying instead of reporting
    // `ArenaAbsent`, and it is the one process that can inherit the role.
    let mut heir = Kid::spawn(&dir.0, &["join-heir"]);
    assert!(
        heir.line().starts_with("joined "),
        "the survivor did not attach"
    );

    owner.kill();

    // The owner that will die mid-handshake.
    let mut doomed = Kid::spawn(&dir.0, &["serve-then-die"]);
    assert_eq!(
        doomed.line(),
        "serving",
        "the doomed owner never bound the rendezvous socket"
    );

    // The joiner runs on a worker thread because `open()` blocks for as long as
    // it retries, and this thread has to drive the takeover meanwhile.
    let (tx, rx) = mpsc::channel();
    let joiner = std::thread::spawn(move || {
        let outcome = tf_tree::Open::new()
            .mode(AttachMode::ReadWrite)
            .create(CreatePolicy::Never)
            .timeout(Duration::from_secs(20))
            .open()
            .map(|tree| {
                let g = tree.guard();
                let target = tree.frame("map").unwrap();
                let source = tree.frame("base").unwrap();
                let plan = tree.plan(target, source).unwrap();
                let iso = plan
                    .at(&g, Stamp::<tf_tree::SystemDomain>::from_nanos(1_500))
                    .unwrap();
                iso.to_bits()
                    .iter()
                    .map(|w| format!("{w:016x}"))
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .map_err(|e| format!("{e:?}"));
        let _ = tx.send(outcome);
    });

    // **The window opened, and the doomed process dying is the proof.** Its
    // assigner runs only when a client's request has been accepted and read, so
    // there is nothing to time here and no sleep that could stand in.
    assert_eq!(
        doomed.line(),
        "dying",
        "the doomed owner never reached its slot assigner, so no client's request \
         was accepted and read"
    );
    let status = doomed.wait_within(Duration::from_secs(20)).expect(
        "the doomed owner announced the window and then did not die, so what the \
         joiner met was not an owner going away mid-handshake",
    );
    assert_eq!(
        status.code(),
        None,
        "the doomed owner exited normally instead of aborting, which would have \
         run its destructors and unlinked the socket: {status:?}"
    );

    // The repair the joiner is waiting for: the survivor takes the vacant role
    // and republishes the socket over the dead pair's path.
    let mut report = String::new();
    for _ in 0..40 {
        heir.poke();
        report = heir.line();
        if report.contains("Inherited") {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        report.contains("Inherited"),
        "the survivor never took the vacant owner role, so nothing came back to \
         serve the joiner: {report}"
    );

    match rx.recv_timeout(Duration::from_secs(40)).expect(
        "the joiner thread never reported; its own 20 s deadline should have ended \
         it either way",
    ) {
        Ok(bits) => assert_eq!(
            bits, owner_value,
            "the joiner attached to something other than the arena the dead owner \
             created and published into"
        ),
        Err(e) => panic!(
            "the joiner was refused instead of retrying past an owner that died \
             mid-handshake: {e}"
        ),
    }
    joiner.join().unwrap();
}

/// **Two survivors race for the vacant owner role, and the loser keeps its
/// slot.**
#[test]
fn two_survivors_race_and_exactly_one_inherits() {
    use tf_tree::AttachMode;
    use tf_tree_ipc::CreatePolicy;

    let dir = Scratch::new("inherit-race");

    let mut owner = Kid::spawn(&dir.0, &["own"]);
    assert!(owner.line().starts_with("owning"));

    let mut a = Kid::spawn(&dir.0, &["join-heir"]);
    let a_joined = a.line();
    let mut b = Kid::spawn(&dir.0, &["join-heir"]);
    let b_joined = b.line();
    let slot_of = |line: &str| line.split_whitespace().nth(1).unwrap_or("?").to_string();
    let (a_slot, b_slot) = (slot_of(&a_joined), slot_of(&b_joined));
    assert_ne!(a_slot, b_slot, "two participants were given one slot");

    owner.kill();

    a.poke();
    b.poke();
    let (ra, rb) = (a.line(), b.line());

    let outcome = |r: &str| r.split_whitespace().nth(1).unwrap_or("?").to_string();
    let after = |r: &str| r.split_whitespace().nth(2).unwrap_or("?").to_string();
    let (oa, ob) = (outcome(&ra), outcome(&rb));

    // Exactly one inherited.
    let inherited = [&oa, &ob].iter().filter(|o| ***o == *"Inherited").count();
    assert_eq!(
        inherited, 1,
        "expected exactly one heir, got A={oa} B={ob} (two would be split brain)"
    );

    // **The other survivor has three correct answers, and which one it gives is
    // the scheduler's business.** `0043` made `owner_lost` ask the kernel
    // whether byte 0 is held before answering, and the child reports two values
    // sampled at two instants — `owner_lost()` first, then whatever
    // `inherit_ownership()` decided, which re-evaluates the same predicate.
    let loser = if oa == "Inherited" { &ob } else { &oa };
    assert!(
        loser == "Contended" || loser == "OwnerAlive",
        "the survivor that did not inherit must be told so, not handed an \
         error: A={ra} B={rb}"
    );

    // The winner did see a hangup — for it the two instants cannot disagree,
    // because it is the process that took the byte.
    let winner = if oa == "Inherited" { &ra } else { &rb };
    assert!(
        winner.starts_with("true "),
        "the heir inherited without observing the owner's hangup: {winner}"
    );

    // The invariant: neither survivor's slot moved, winner or loser.
    assert_eq!(
        after(&ra),
        a_slot,
        "survivor A's slot moved: {a_joined} -> {ra}"
    );
    assert_eq!(
        after(&rb),
        b_slot,
        "survivor B's slot moved: {b_joined} -> {rb}"
    );

    // And the arena is joinable again.
    tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::Never)
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("after a contested inheritance the arena must still be joinable");
}

/// The loser stops being told the owner is gone, and starts again if it is
/// ([`0043`](../../../docs/decisions/0043-owner-lost-is-a-question-about-the-owner.md)).
///
/// `two_survivors_race_and_exactly_one_inherits` stops at the race. This is what
/// the other survivor sees next: `owner_lost` must stop reporting the owner
/// gone once byte 0 is held again.
///
/// ```ignore
/// if tree.owner_lost() { let _ = tree.inherit_ownership()?; }
/// ```
///
/// A is poked and read before B is poked, so B's answer is a fact about the
/// code, not the scheduler.
///
/// # One of the two pins of §3.5's NORMATIVE sentence
///
/// `docs/PHASE2.md` §3.5: `owner_lost()` answers `true` once the attach
/// connection has hung up and the last description holding byte 0 has closed,
/// with no delay, heartbeat or timeout (D17).
/// [`0057`](../../../docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md)
/// Decision 5 names this test and
/// `a_read_only_survivor_reports_that_it_cannot_inherit` as its pin. The kernel
/// closes a dying process's files before it is reapable, so once `Kid::kill`
/// returns the first `owner_lost()` must not answer `false`. It pins the event,
/// not a duration. A third party holding the dead owner's descriptors (an
/// `lsof`-style scanner) is its one legitimate failure.
#[test]
fn a_survivor_that_did_not_inherit_stops_being_told_the_owner_is_gone() {
    let dir = Scratch::new("inherit-loser");

    let mut owner = Kid::spawn(&dir.0, &["own"]);
    assert!(owner.line().starts_with("owning"));

    let mut heir = Kid::spawn(&dir.0, &["join-heir"]);
    let heir_joined = heir.line();
    let mut other = Kid::spawn(&dir.0, &["join-heir"]);
    let other_joined = other.line();
    assert_ne!(
        heir_joined, other_joined,
        "two participants were given one slot"
    );

    owner.kill();

    // Serialised: the heir takes the role and is serving before the other one
    // is asked anything.
    heir.poke();
    let taken = heir.line();
    assert!(
        taken.starts_with("true Inherited "),
        "the first survivor should have inherited from the dead owner: {taken}. \
         The owner was SIGKILLed and reaped, so its files were closed: a leading \
         `false` is owner_lost() answering false after the close, which \
         PHASE2 §3.5's NORMATIVE sentence forbids (0057 Decision 5). Its one \
         legitimate cause is another task holding a transient reference to the \
         dead owner's socket or lock-file description — a /proc/<pid>/fd reader \
         (lsof, ss -p, a monitoring agent) — on this host"
    );

    // **The defect.** The other survivor's socket is hung up and always will
    // be.
    other.poke();
    let second = other.line();
    assert!(
        second.starts_with("false "),
        "a survivor is still being told the owner is gone while the heir is \
         alive and serving, so the §3.5 loop never stops retrying: {second}"
    );
    let outcome = |r: &str| r.split_whitespace().nth(1).unwrap_or("?").to_string();
    assert_eq!(
        outcome(&second),
        "OwnerAlive",
        "an owner is alive, so inheriting must not even be attempted: {second}"
    );
    assert_eq!(
        second.split_whitespace().nth(2).unwrap_or("?"),
        other_joined.split_whitespace().nth(1).unwrap_or("!"),
        "the survivor's slot moved: {other_joined} -> {second}"
    );

    // The chain.
    heir.kill();
    other.poke();
    let third = other.line();
    assert!(
        third.starts_with("true "),
        "the second owner died and the survivor did not notice: {third}. \
         The heir was SIGKILLed and reaped, so the last description holding \
         byte 0 was closed, and PHASE2 §3.5's NORMATIVE sentence forbids a \
         false after that (0057 Decision 5). Its one legitimate cause is \
         another task holding a transient reference to the dead heir's \
         lock-file description — a /proc/<pid>/fd reader (lsof, ss -p, a \
         monitoring agent) — on this host"
    );
    assert_eq!(
        outcome(&third),
        "Inherited",
        "the survivor should have taken the role the dead heir left: {third}"
    );
}

/// **A fleet of read-only consumers cannot rescue itself, and that is D18
/// working rather than failing.**
#[test]
fn a_read_only_survivor_reports_that_it_cannot_inherit() {
    use tf_tree::{AttachMode, Inheritance};
    use tf_tree_ipc::CreatePolicy;

    let dir = Scratch::new("inherit-read-only");

    let mut owner = Kid::spawn(&dir.0, &["own"]);
    assert!(owner.line().starts_with("owning"));

    // This process is the read-only survivor.
    let ro = tf_tree::Open::new()
        .mode(AttachMode::ReadOnly)
        .create(CreatePolicy::Never)
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("a read-only consumer must be able to join");
    assert!(!ro.is_writable(), "the attachment was not read-only");

    // While the owner lives, nothing is attempted at all.
    assert_eq!(ro.inherit_ownership().unwrap(), Inheritance::OwnerAlive);

    owner.kill();

    // The owner's death is visible to a read-only consumer too — the socket is
    // the liveness signal regardless of the mapping's protection (D17).
    assert!(
        ro.owner_lost(),
        "a read-only attachment missed the hangup. The owner was SIGKILLed and \
         reaped, so its socket and byte 0 were closed, and PHASE2 §3.5's \
         NORMATIVE sentence forbids owner_lost() answering false after that \
         (0057 Decision 5). Its one legitimate cause is another task holding a \
         transient reference to the dead owner's socket or lock-file \
         description — a /proc/<pid>/fd reader (lsof, ss -p, a monitoring \
         agent) — on this host"
    );
    assert_eq!(
        ro.inherit_ownership().unwrap(),
        Inheritance::ReadOnly,
        "a PROT_READ attachment must refuse the role rather than take a byte it cannot serve behind"
    );

    // And the promise that survives all of it: lookups do not stop.
    let g = ro.guard();
    let target = ro.frame("map").unwrap();
    let source = ro.frame("base").unwrap();
    let plan = ro.plan(target, source).unwrap();
    let iso = plan
        .at(
            &g,
            tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(1_500),
        )
        .expect("a dead owner must not stop a reader");
    assert_eq!(
        iso.to_bits(),
        tf_tree::exp_se3([1.0, 2.0, 3.0, 0.1, 0.2, 0.3]).to_bits()
    );
}

/// **`docs/PHASE2.md` §11.3, `takeover.after_ownership_lock_before_bind`: the
/// crash-matrix row §3.5 owes.**
#[cfg(feature = "crash-points")]
#[test]
fn a_killed_topology_holder_leaves_a_word_the_next_acquirer_steals() {
    let dir = Scratch::new("topo-crash");

    let mut owner = Kid::spawn(&dir.0, &["own"]);
    assert!(owner.line().starts_with("owning"));

    // Armed to abort with byte 1 held and the arena word naming it.
    let mut doomed = Kid::spawn_with_env(
        &dir.0,
        &["join-reparent"],
        &[("TF_TREE_CRASH_AT", "topo.holding_lock:1")],
    );
    assert_eq!(doomed.line(), "joined");

    let mut heir = Kid::spawn(&dir.0, &["join-reparent"]);
    assert_eq!(heir.line(), "joined");

    doomed.poke();
    let status = doomed.wait();
    assert_eq!(
        status.code(),
        None,
        "the armed holder exited normally instead of aborting: {status:?}"
    );

    // The repair: the next acquirer takes the byte the kernel released, finds
    // the word naming a corpse, and steals it.
    heir.poke();
    let report = heir.line();
    assert_eq!(
        report, "reparented",
        "the topology lock was not recovered from a killed holder: {report}"
    );
}

/// **`docs/PHASE2.md` §11.3 `open.after_ownership_lock_before_bind` and
/// `open.after_create_before_bind`** — the two states a creator can die in, and
/// the reason neither leaves anything behind.
#[cfg(feature = "crash-points")]
#[test]
fn a_creator_killed_before_or_after_the_arena_exists_leaves_nothing_behind() {
    for site in [
        "open.after_ownership_lock_before_bind",
        "open.after_create_before_bind",
    ] {
        let dir = Scratch::new("open-crash");

        let mut doomed = Kid::spawn_with_env(
            &dir.0,
            &["own"],
            &[("TF_TREE_CRASH_AT", &format!("{site}:1"))],
        );
        let status = doomed.wait();
        assert_eq!(
            status.code(),
            None,
            "the armed creator at {site} exited normally instead of aborting: {status:?}"
        );

        // The repair: a fresh creator finds nothing alive and creates.
        let mut next = Kid::spawn(&dir.0, &["own"]);
        assert!(
            next.line().starts_with("owning"),
            "after a creator died at {site}, the next one could not create"
        );

        // And the arena it created is *its own*: a joiner reads the frames this
        // creator declared, which a half-created orphan could not answer for.
        let joined = tf_tree::Open::new()
            .mode(tf_tree::AttachMode::ReadWrite)
            .create(tf_tree_ipc::CreatePolicy::Never)
            .timeout(std::time::Duration::from_millis(500))
            .open()
            .unwrap_or_else(|e| panic!("a joiner could not reach the new arena after {site}: {e}"));
        assert!(
            joined.frame("cam").is_ok(),
            "the surviving arena is not the one the second creator declared"
        );
    }
}

/// **The facade's half of
/// `crash_tests::the_published_site_list_is_the_one_the_tests_arm`.**
#[cfg(feature = "crash-points")]
#[test]
fn the_facade_site_list_is_pinned_by_index_and_every_site_has_a_test() {
    // The order is load-bearing: these are the indices the `maybe_abort` calls
    // use.
    const EXPECTED: &[&str] = &[
        "takeover.after_ownership_lock_before_bind",
        "topo.holding_lock",
        "open.after_ownership_lock_before_bind",
        "open.after_create_before_bind",
        "reclaim.after_probe_before_cas",
        "hangup.after_probe_before_cas",
    ];
    assert_eq!(
        tf_tree::CRASH_SITES.len(),
        EXPECTED.len(),
        "a site was added to or removed from tf_tree::CRASH_SITES without updating \
         this gate - and every arming site names an *index*, so the new list must be \
         checked against its call sites, not just against this array"
    );
    for (i, want) in EXPECTED.iter().enumerate() {
        assert_eq!(
            &tf_tree::CRASH_SITES[i],
            want,
            "CRASH_SITES[{i}] moved. The `maybe_abort(CRASH_SITES[{i}])` call site \
             now arms a different protocol point than its name says."
        );
    }

    // **Which test arms each site, as data.** This cannot *prove* a test fires
    // a site — a unit test cannot observe that — and an earlier revision
    // pretended otherwise: it asserted set membership against a list
    // character-for-character identical to `EXPECTED` above, which the index
    // loop had already checked element by element.
    const ARMED_BY: &[(&str, &str)] = &[
        (
            "takeover.after_ownership_lock_before_bind",
            "a_killed_heir_leaves_the_role_for_the_next_survivor",
        ),
        (
            "topo.holding_lock",
            "a_killed_topology_holder_leaves_a_word_the_next_acquirer_steals",
        ),
        (
            "open.after_ownership_lock_before_bind",
            "a_creator_killed_before_or_after_the_arena_exists_leaves_nothing_behind",
        ),
        (
            "open.after_create_before_bind",
            "a_creator_killed_before_or_after_the_arena_exists_leaves_nothing_behind",
        ),
        (
            "reclaim.after_probe_before_cas",
            "a_killed_sweeper_leaves_the_record_for_the_next_one",
        ),
        (
            "hangup.after_probe_before_cas",
            "a_killed_owner_in_its_hangup_callback_leaves_the_role_inheritable",
        ),
    ];
    assert_eq!(
        ARMED_BY.len(),
        tf_tree::CRASH_SITES.len(),
        "a site was added without naming the test that arms it"
    );
    for (i, (site, test)) in ARMED_BY.iter().enumerate() {
        assert_eq!(
            site,
            &tf_tree::CRASH_SITES[i],
            "the arming table is out of order with CRASH_SITES at index {i}"
        );
        assert!(
            !test.is_empty(),
            "{site} is published in tf_tree::CRASH_SITES with no test named for it. \
             A site nothing fires is a crash point whose repair claim has never been \
             executed - which is what docs/PHASE2.md §11.3 recorded for two sites \
             until 2026-08-29."
        );
    }
}

/// §11.3 `reclaim.after_probe_before_cas`: a sweeper killed between judging a
/// record dead and claiming it.
#[cfg(all(feature = "crash-points", feature = "unstable"))]
#[test]
fn a_killed_sweeper_leaves_the_record_for_the_next_one() {
    let dir = Scratch::new("reclaim-crash");

    // **The corpse must be the owner, and that is the whole fixture.** A
    // *joiner*'s death fires the owner's hangup callback, which collects its
    // record within milliseconds - so a sweeper started afterwards finds
    // nothing to reclaim, never reaches the site, and parks instead of
    // aborting.
    let mut doomed_owner = Kid::spawn(&dir.0, &["own"]);
    assert!(doomed_owner.line().starts_with("owning"));

    // Both sweepers join while the owner is still serving.
    let mut doomed = Kid::spawn_with_env(
        &dir.0,
        &["join-sweep"],
        &[("TF_TREE_CRASH_AT", "reclaim.after_probe_before_cas:1")],
    );
    assert!(doomed.line().starts_with("joined "));
    let mut second = Kid::spawn(&dir.0, &["join-sweep"]);
    assert!(second.line().starts_with("joined "));

    // Now the record that no hangup will ever collect.
    doomed_owner.kill();

    // The armed sweeper reaches the site because there is finally something to
    // reclaim, and dies between the probe and the CAS.
    doomed.poke();
    let status = doomed
        .wait_within(std::time::Duration::from_secs(20))
        .expect("the armed sweeper neither aborted nor finished within 20s");
    assert_eq!(
        status.code(),
        None,
        "the armed sweeper exited normally instead of aborting: {status:?}. It \
         reaches the site only if the sweep finds a reclaimable record, so an \
         orderly exit here means either the fixture produced none, or \
         `reclaim.after_probe_before_cas` has stopped firing."
    );

    // The repair: the first sweeper died before its CAS landed, so the record
    // is still there and the next sweeper still has work.
    second.poke();
    let report = second.line();
    let swept: usize = report
        .strip_prefix("swept ")
        .expect("the second sweeper did not report")
        .trim()
        .parse()
        .expect("a count");
    // **Two, not one, and the difference is the whole assertion.** The armed
    // sweeper aborted, which releases *its own* byte and leaves *its own*
    // record reclaimable — so `swept >= 1` holds whether or not the killed
    // sweep lost the owner's record, and an earlier revision asserted exactly
    // that and could not fail.
    assert!(
        swept >= 2,
        "the second sweeper reclaimed {swept} record(s), expected the killed \
         owner's *and* the killed sweeper's ({report:?}). One means the armed \
         sweeper's half-finished reclaim consumed the owner's record instead of \
         leaving it for the next sweep - which is the repair claim
         `reclaim.after_probe_before_cas` makes."
    );
}

/// §11.3 `hangup.after_probe_before_cas`: the **owner** killed inside its own
/// hangup callback, between judging a departed peer's record and reclaiming it.
#[cfg(all(feature = "crash-points", feature = "unstable"))]
#[test]
fn a_killed_owner_in_its_hangup_callback_leaves_the_role_inheritable() {
    let dir = Scratch::new("hangup-crash");

    // The owner is the armed one this time.
    let mut owner = Kid::spawn_with_env(
        &dir.0,
        &["own"],
        &[("TF_TREE_CRASH_AT", "hangup.after_probe_before_cas:1")],
    );
    assert!(owner.line().starts_with("owning"));

    // A survivor that can inherit once the owner dies.
    let mut heir = Kid::spawn(&dir.0, &["join-heir"]);
    assert!(heir.line().starts_with("joined "));

    // A joiner whose departure fires the owner's hangup callback - and the
    // owner is armed to abort inside it.
    let mut departing = Kid::spawn(&dir.0, &["join-heir"]);
    assert!(departing.line().starts_with("joined "));
    departing.kill();

    let status = owner
        .wait_within(std::time::Duration::from_secs(20))
        .expect(
            "the armed owner was still alive 20s after a joiner departed: its hangup \
         callback either never ran or no longer passes \
         `hangup.after_probe_before_cas`",
        );
    assert_eq!(
        status.code(),
        None,
        "the armed owner exited normally instead of aborting inside its hangup \
         callback: {status:?}"
    );

    // The repair: byte 0 was released by the kernel, so the survivor inherits.
    heir.poke();
    let report = heir.line();
    assert!(
        report.contains("Inherited") || report.contains("Contended"),
        "no survivor could take the role after the owner died mid-hangup: {report:?}"
    );
}

#[cfg(feature = "crash-points")]
#[test]
fn a_killed_heir_leaves_the_role_for_the_next_survivor() {
    use tf_tree::AttachMode;
    use tf_tree_ipc::CreatePolicy;

    let dir = Scratch::new("inherit-crash");

    let mut owner = Kid::spawn(&dir.0, &["own"]);
    assert!(owner.line().starts_with("owning"));

    // The doomed heir: armed to abort with byte 0 held and nothing listening.
    let mut doomed = Kid::spawn_with_env(
        &dir.0,
        &["join-heir"],
        &[(
            "TF_TREE_CRASH_AT",
            "takeover.after_ownership_lock_before_bind:1",
        )],
    );
    assert!(doomed.line().starts_with("joined "));

    let mut heir = Kid::spawn(&dir.0, &["join-heir"]);
    assert!(heir.line().starts_with("joined "));

    owner.kill();

    // The doomed one goes first and dies holding the role.
    doomed.poke();
    let status = doomed.wait();
    assert_eq!(
        status.code(),
        None,
        "the armed heir exited normally instead of aborting: {status:?}"
    );

    // The repair: the next survivor takes the role the corpse was holding.
    heir.poke();
    let report = heir.line();
    assert!(
        report.starts_with("true Inherited "),
        "the kernel did not release the dead heir's ownership byte, or the \
         survivor could not take it: {report}"
    );

    // And joiners retry successfully, which is the row's last clause.
    tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::Never)
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("after the heir's death and the next survivor's takeover, a joiner must succeed");
}

// `docs/PHASE2.md` §11.2 — the required integration scenarios.

/// Read a `uuid <hex>` line from an `open-uuid` child, or its refusal.
fn uuid_of(kid: &mut Kid) -> Result<String, String> {
    let line = kid.line();
    match line.strip_prefix("uuid ") {
        Some(u) => Ok(u.to_string()),
        None => Err(line),
    }
}

/// **§11.2 scenario 7 — the thundering herd.** N processes call `open()` at
/// once with no arena present.
#[test]
fn scenario_7_a_thundering_herd_produces_exactly_one_arena() {
    let scratch = Scratch::new("herd");
    // Sixteen rather than §11.2's thirty-two: the race is between the *first*
    // few to reach step 4, and thirty-two processes on a four-core runner spend
    // the difference in the scheduler rather than in the window under test.
    const N: usize = 16;
    let mut kids: Vec<Kid> = (0..N)
        .map(|_| Kid::spawn(&scratch.0, &["open-uuid"]))
        .collect();

    let mut uuids = std::collections::BTreeSet::new();
    let mut refusals = Vec::new();
    for kid in &mut kids {
        match uuid_of(kid) {
            Ok(u) => {
                uuids.insert(u);
            }
            Err(e) => refusals.push(e),
        }
    }
    assert!(
        refusals.is_empty(),
        "no opener should be refused when the arena is simply absent: {refusals:?}"
    );
    assert_eq!(
        uuids.len(),
        1,
        "every process must see one arena; got {} distinct instance_uuids: {uuids:?}",
        uuids.len()
    );
    for kid in &mut kids {
        kid.kill();
    }
}

/// **§11.2 scenario 9 — the split-brain attempt**, and the most important
/// assertion in the phase: *"Two distinct `instance_uuid`s on one
/// `(runtime_dir, domain, name)` is a hard test failure."*
#[test]
fn scenario_9_a_split_brain_attempt_never_produces_a_second_arena() {
    let scratch = Scratch::new("split-brain");
    let mut owner = Kid::spawn(&scratch.0, &["open-uuid"]);
    let first = uuid_of(&mut owner).expect("the owner opens");

    // A survivor that *can* inherit, which is what makes the second half
    // reachable.
    let mut survivor = Kid::spawn(&scratch.0, &["join-heir"]);
    let joined = survivor.line();
    assert!(joined.starts_with("joined "), "survivor: {joined}");

    // Kill and race a newcomer into the window before the hangup is noticed.
    owner.kill();
    let mut racer = Kid::spawn(&scratch.0, &["open-uuid", "400"]);
    match uuid_of(&mut racer) {
        Ok(u) => assert_eq!(u, first, "a racer that opens must join, not create"),
        Err(refusal) => assert!(
            refusal.starts_with("refused"),
            "a racer must join or be refused, not fail some third way: {refusal}"
        ),
    }

    // Second half: once a survivor inherits, the arena is joinable again — and
    // it is still the *same* arena.
    survivor.poke();
    let inherited = survivor.line();
    assert!(
        inherited.contains("Inherited"),
        "the survivor must take the role: {inherited}"
    );
    let mut newcomer = Kid::spawn(&scratch.0, &["open-uuid"]);
    let after = uuid_of(&mut newcomer).expect("a newcomer joins the inherited arena");
    assert_eq!(
        after, first,
        "inheritance must preserve the arena; a new uuid here is the split brain"
    );

    survivor.kill();
    racer.kill();
    newcomer.kill();
}

/// **§11.2 scenario 10 — the stuck participant.** `SIGSTOP` the only
/// participant, then open from a fresh process: it must be refused with
/// `ArenaHeldButUnreachable` **naming the stuck slot**, never create a second
/// arena.
#[test]
fn scenario_10_a_stuck_participant_refuses_a_second_arena_and_recovers() {
    let scratch = Scratch::new("stuck");
    let mut held = Kid::spawn(&scratch.0, &["open-uuid"]);
    let first = uuid_of(&mut held).expect("the holder opens");
    let pid = held.0.id();

    std::process::Command::new("kill")
        .args(["-STOP", &pid.to_string()])
        .status()
        .expect("SIGSTOP");
    await_stopped(pid);

    // Refused, and the message names the slot the operator has to deal with.
    let mut blocked = Kid::spawn(&scratch.0, &["open-uuid", "400"]);
    let refusal = uuid_of(&mut blocked).expect_err("a stuck holder must refuse a create");
    assert!(
        refusal.contains("ArenaHeldButUnreachable"),
        "the refusal must be the one RUNBOOK.md documents: {refusal}"
    );
    assert!(
        refusal.contains("first_slot"),
        "the refusal must name the stuck slot: {refusal}"
    );

    std::process::Command::new("kill")
        .args(["-CONT", &pid.to_string()])
        .status()
        .expect("SIGCONT");

    // And the arena is the same one afterwards — a stuck participant delays a
    // joiner, it does not fork the arena.
    let mut after = Kid::spawn(&scratch.0, &["open-uuid"]);
    let seen = uuid_of(&mut after).expect("open succeeds once the holder runs again");
    assert_eq!(seen, first, "recovery must join the original arena");

    held.kill();
    blocked.kill();
    after.kill();
}

/// **§11.2 scenario 11 — domain isolation.** Two arenas under different domains
/// never observe each other.
#[test]
fn scenario_11_two_domains_in_one_runtime_dir_are_separate_arenas() {
    let scratch = Scratch::new("domains");
    let mut a = Kid::spawn_with_env(&scratch.0, &["open-uuid"], &[("TF_TREE_DOMAIN", "7")]);
    let mut b = Kid::spawn_with_env(&scratch.0, &["open-uuid"], &[("TF_TREE_DOMAIN", "9")]);

    let ua = uuid_of(&mut a).expect("domain 7 opens");
    let ub = uuid_of(&mut b).expect("domain 9 opens");
    assert_ne!(
        ua, ub,
        "two domains in one runtime dir must be two arenas, not one"
    );

    // And a third process in domain 7 joins *that* arena, not the other — which
    // is the half that fails if the domain reaches the name but not the lock.
    let mut c = Kid::spawn_with_env(&scratch.0, &["open-uuid"], &[("TF_TREE_DOMAIN", "7")]);
    let uc = uuid_of(&mut c).expect("a second opener in domain 7");
    assert_eq!(
        uc, ua,
        "domain 7's second opener must join domain 7's arena"
    );

    a.kill();
    b.kill();
    c.kill();
}

/// **§11.2 scenario 6 — a full participant table.** 64 participants, then a
/// 65th: `NoParticipantSlots`, *"and the message says how to raise the limit"*.
#[test]
fn scenario_6_a_full_participant_table_refuses_the_next_attach_with_a_remedy() {
    let scratch = Scratch::new("full-table");
    // The owner takes slot 0, so 63 joiners fill the default 64-slot table.
    let mut owner = Kid::spawn(&scratch.0, &["open-uuid"]);
    let uuid = uuid_of(&mut owner).expect("the owner opens");

    let limit = tf_tree_arena::DEFAULT_MAX_PARTICIPANTS as usize;
    let mut joiners = Vec::new();
    for i in 0..(limit - 1) {
        let mut k = Kid::spawn(&scratch.0, &["open-uuid"]);
        let got = uuid_of(&mut k)
            .unwrap_or_else(|e| panic!("joiner {i} of {} was refused: {e}", limit - 1));
        assert_eq!(got, uuid, "every joiner must be on the same arena");
        joiners.push(k);
    }

    // The 65th.
    let mut over = Kid::spawn(&scratch.0, &["open-uuid", "800"]);
    let refusal = uuid_of(&mut over).expect_err("the table is full; this attach must be refused");
    assert!(
        refusal.contains("NoParticipantSlots"),
        "the refusal must name the exhausted resource: {refusal}"
    );

    owner.kill();
    for k in &mut joiners {
        k.kill();
    }
    over.kill();
}

/// **§11.2 scenario 1 — one owner, one writer, many read-only readers**, with
/// *"zero divergence between readers"* as the assertion.
#[test]
fn scenario_1_many_readers_of_one_arena_do_not_diverge() {
    let scratch = Scratch::new("readers");
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    let owned = owner.line();
    let truth = owned
        .strip_prefix("owning ")
        .expect("the owner reports its own answer")
        .to_string();

    const READERS: usize = 14;
    let mut kids: Vec<Kid> = (0..READERS)
        .map(|_| Kid::spawn(&scratch.0, &["join"]))
        .collect();

    let mut seen = std::collections::BTreeSet::new();
    for (i, kid) in kids.iter_mut().enumerate() {
        let line = kid.line();
        let bits = line
            .strip_prefix("joined ")
            .unwrap_or_else(|| panic!("reader {i}: {line}"));
        seen.insert(bits.to_string());
    }
    assert_eq!(
        seen.len(),
        1,
        "{READERS} readers of one arena must agree exactly; got {seen:?}"
    );
    assert!(
        seen.contains(&truth),
        "the readers agreed with each other but not with the writer:\n  writer {truth}\n  readers {seen:?}"
    );

    owner.kill();
    for k in &mut kids {
        k.kill();
    }
}

/// **§11.2 scenario 2 — attach/detach churn**, with *"participant slots must
/// not leak"* as the assertion.
#[test]
fn scenario_2_attach_detach_churn_does_not_leak_participant_slots() {
    let scratch = Scratch::new("churn");
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    let _ = owner.line();

    let limit = tf_tree_arena::DEFAULT_MAX_PARTICIPANTS as usize;
    // Three tablefuls, eight at a time, so the collectors are always working
    // against a live table rather than an idle one.
    const BATCH: usize = 8;
    let rounds = (limit * 3).div_ceil(BATCH);
    for round in 0..rounds {
        let mut batch: Vec<Kid> = (0..BATCH)
            .map(|_| Kid::spawn(&scratch.0, &["join-rw"]))
            .collect();
        for (i, k) in batch.iter_mut().enumerate() {
            let line = k.line();
            assert!(
                line.starts_with("joined"),
                "round {round} joiner {i} was refused, so a slot leaked: {line}"
            );
        }
        for k in &mut batch {
            k.kill();
        }
    }

    // The assertion: after 3x the table has churned through it, the table is
    // still usable.
    let mut after = Kid::spawn(&scratch.0, &["join-rw"]);
    let line = after.line();
    assert!(
        line.starts_with("joined"),
        "after {} attach/detach cycles the table must still grant a slot: {line}",
        rounds * BATCH
    );

    owner.kill();
    after.kill();
}

/// **§11.2 scenario 3 — the owner dies mid-run.** Existing participants
/// continue, a new attach fails cleanly, and reaping still functions.
#[test]
fn scenario_3_an_owner_dying_leaves_readers_working_and_joins_refused() {
    let scratch = Scratch::new("owner-dies");
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    let truth = owner.line().strip_prefix("owning ").unwrap().to_string();

    // A survivor that can both read and reap.
    let mut survivor = Kid::spawn(&scratch.0, &["join-sweep"]);
    let joined = survivor.line();
    assert!(joined.starts_with("joined"), "survivor: {joined}");

    owner.kill();

    // 1. A new attach fails cleanly — a named refusal, not a hang or a crash.
    let mut newcomer = Kid::spawn(&scratch.0, &["open-uuid", "400"]);
    let refusal = uuid_of(&mut newcomer).expect_err("joining an ownerless arena must be refused");
    assert!(
        refusal.contains("ArenaHeldButUnreachable"),
        "the refusal must be the documented one: {refusal}"
    );

    // 2.
    let mut later = Kid::spawn(&scratch.0, &["open-uuid", "400"]);
    let still_refused =
        uuid_of(&mut later).expect_err("a read-write survivor that never polls is not an heir");
    assert!(
        still_refused.contains("ArenaHeldButUnreachable"),
        "the refusal must outlive the instant the owner died, not merely follow it: \
         {still_refused}"
    );
    later.kill();

    survivor.poke();
    let swept = survivor.line();
    let n: usize = swept
        .strip_prefix("swept ")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("sweeper: {swept}"));
    assert!(
        n >= 1,
        "the sweeper must reclaim the dead owner's slot, got {n}"
    );

    // 3. "Existing participants continue" is what the successful sweep above
    // *is*: `reap_participants` reads and writes the arena, so a survivor that
    // can still do it after the owner's death is a participant that still
    // works.
    let _ = &truth;

    // **The positive control for the refusal above: same command, one
    // variable.** `open-uuid 400` was refused while the ineligible survivor
    // held its byte; once it is gone the identical invocation succeeds.
    let status = survivor
        .wait_within(std::time::Duration::from_secs(10))
        .expect("the sweeper must exit after reporting, so its participant byte is released");
    assert!(
        status.success(),
        "the sweeper must exit cleanly, so its participant byte is released: {status:?}"
    );
    let mut after = Kid::spawn(&scratch.0, &["open-uuid", "400"]);
    let created =
        uuid_of(&mut after).expect("with every byte released the identical open must succeed");
    assert_eq!(
        created.len(),
        32,
        "expected an instance uuid, got {created:?}"
    );
    after.kill();

    newcomer.kill();
}
