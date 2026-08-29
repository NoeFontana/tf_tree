//! `tf_tree::open()` against real processes.
//!
//! This is the test the whole Phase 2 completion milestone exists for: **a
//! process that is not a child obtains the arena and reads the same transforms
//! as its creator.** Until `docs/decisions/0005` steps 1-5 landed, the only
//! transport was fd inheritance, so this could not be written at all.
//!
//! Real processes, not threads. The kernel is what makes the claims true —
//! that a `SIGKILL`ed participant's socket closes, that its lock byte is
//! released without its cooperation — and none of that is stageable in one
//! process. Children open the lock file and the socket **by path**: an
//! inherited descriptor shares the open file description, so every contention
//! assertion would pass vacuously.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// A scratch runtime directory, removed when the test ends.
///
/// **`set_var` is process-wide, and that is safe here only because `nextest`
/// gives every test its own process.** Under plain `cargo test` these tests
/// share one process and one environment, so two `Scratch`es would race and the
/// loser would resolve the winner's rendezvous. Every recipe that runs this
/// target uses `cargo nextest run`; a `cargo test` invocation of it is not
/// supported and would fail intermittently rather than loudly.
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

    /// [`Self::spawn`] with extra environment — `TF_TREE_CRASH_AT`, which is how
    /// `docs/PHASE2.md` §11.3 arms a named crash point in a child.
    fn spawn_with_env(dir: &PathBuf, args: &[&str], env: &[(&str, &str)]) -> Kid {
        // The bin target carries the crate's name, not the file's: this crate is
        // published, and `--features shm` installs whatever is here into the
        // user's `bin/`. The manifest argues it; the source stays
        // `src/bin/rendezvous_child.rs`.
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
    ///
    /// Only `a_killed_heir_leaves_the_role_for_the_next_survivor` calls this,
    /// and it is the only test that has a child which dies by itself — every
    /// other one either parks until [`Kid::kill`] or answers on stdout. Gated on
    /// the same feature as that test, because without it the crash point is
    /// compiled out, no child dies, and this becomes dead code that fails
    /// `just shm-check`'s `--features shm` clippy row.
    #[cfg(feature = "crash-points")]
    fn wait(&mut self) -> std::process::ExitStatus {
        self.0.wait().expect("wait for the child")
    }

    /// The child's next line. It flushes before it parks, so this returning is
    /// proof the step actually happened — no sleeps, no polling.
    fn line(&mut self) -> String {
        let reader = self
            .1
            .get_or_insert_with(|| BufReader::new(self.0.stdout.take().expect("piped stdout")));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read child line");
        line.trim_end().to_string()
    }

    /// `SIGKILL`, then reap. After `wait` returns the kernel has torn down the
    /// process's descriptors, so its locks are gone with no cooperation from
    /// it — which is the entire point.
    /// Nudge a child that is waiting on stdin, **keeping the pipe**.
    ///
    /// It used to `take` the pipe and drop it, which closed the child's stdin as
    /// a side effect of nudging it. That was invisible while every helper read
    /// one line and parked; it is a trap now that `join-heir` answers one line
    /// per poke, because the second `poke` silently does nothing, the child sees
    /// EOF, and the caller blocks in [`Self::line`] forever. Keeping the pipe is
    /// what [`Self::ask`] already does for the same reason.
    fn poke(&mut self) {
        use std::io::Write;
        if let Some(stdin) = self.0.stdin.as_mut() {
            let _ = writeln!(stdin, "go");
            let _ = stdin.flush();
        }
    }

    /// Ask a `join-rw-report` child about one slot and read its answer.
    ///
    /// Unlike [`Self::poke`] this keeps the pipe: the child answers one line
    /// per line it reads, so a caller can put the same question either side of
    /// a `SIGKILL` and compare, which is the only way to observe a transition
    /// rather than a state.
    ///
    /// `unstable`, with both its callers: the helper mode this drives reads a
    /// participant record's raw `state` word through `Tree::arena_view`, and
    /// that feature is what gates it (`docs/API.md` §2.6).
    #[cfg(feature = "unstable")]
    fn ask(&mut self, slot: u32) -> String {
        use std::io::Write;
        let stdin = self.0.stdin.as_mut().expect("piped stdin");
        writeln!(stdin, "{slot}").expect("write a slot query");
        stdin.flush().expect("flush the slot query");
        self.line()
    }

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
///
/// Not merely "attach succeeded" — the joiner performs a real lookup and must
/// get the transform the creator published, **bit for bit**. Attaching to the
/// wrong segment, or to a segment whose header this build misreads, would still
/// let `open()` return `Ok`; only comparing the numbers rules that out.
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
///
/// The intended behaviour for a supervised consumer: a notebook or a
/// visualiser started before the robot must say so, not create an empty arena
/// that the real publisher then refuses to join (§3.2, D18).
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
///
/// `tf_tree::open()` is the call a README reader types and the only consumer of
/// [`tf_tree::Open::new`]'s *defaults* anywhere in the workspace — every other
/// caller names `mode` and `create` explicitly. `docs/decisions/0019` moved that
/// `create` default from `IfAbsent` to `Never`, and nothing but this test would
/// notice if the move had broken the join path.
///
/// Bit-for-bit, like `a_foreign_process_joins_and_reads_the_same_transform`:
/// attaching to the wrong segment would still return `Ok`.
///
/// **Mutant: make `open()` pass `CreatePolicy::IfAbsent`** ⇒ still passes here
/// (an arena is serving, so the join path is taken either way), which is why
/// `a_read_only_attach_refuses_to_create` exists as well. **Mutant: break the
/// free function's body** ⇒ the child reports `error ...` and this fails.
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
///
/// An owner is exactly two: the main thread, parked, and `tf_tree-owner`
/// running `OwnerServer::serve`. So "the serving thread died and the process
/// did not" — the shape of the defect below — is one integer, visible from
/// outside without asking the process anything.
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
///
/// The mirror of [`await_stopped`], and load-bearing for the same reason: it is
/// the **continue** that delivers the `EINTR`, and `kill(2)` returning says only
/// that `SIGCONT` was queued. Without this wait, everything below could be a
/// measurement of a process that is still stopped — which is a different
/// experiment with a different expected answer.
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
///
/// The budget is what buys the failure a chance to *happen* before it is
/// asserted on — the serving thread dies microseconds after the `SIGCONT`, and
/// spawning the joiner below takes longer than that, but neither is a
/// guarantee. Returning early when the count moves under us keeps it from
/// being a flat sleep in the case where the change lands late.
///
/// Measured either way, both arms cost the budget end to end: 0.512 s with the
/// defect and 0.513 s without it, because by the time [`await_continued`]
/// returns the thread is usually already gone and there is no *change* left to
/// observe — only a count to read. A bare `sleep` would give the same coverage
/// and a worse message; this way the count that failed is the count printed.
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
///
/// `epoll_wait` is one of the interfaces `signal(7)` lists as failing with
/// `EINTR` after a stop signal followed by `SIGCONT`, with no signal handler
/// installed anywhere — Ctrl-Z then `fg`, a container freeze/thaw, a debugger
/// attaching and detaching. `OwnerServer::serve` propagated that errno like any
/// other, and the consequences were not confined to the loop: `serve` returned
/// `Err`, the server's `Drop` unlinked the published socket, and the process
/// **lived on** holding participant byte 0 and the ownership byte. §3.4 then
/// has no exit for anybody — nothing serves, and step 4 refuses to create a
/// second arena because a participant byte is held by a process that really is
/// alive — so an operator's only remedy was killing a healthy publisher.
///
/// # What each part of this test is for
///
/// The join *before* the signals is the control: it establishes that this
/// rendezvous served, so a failure afterwards is caused by the pair and not by
/// a fixture that never worked. [`await_stopped`] makes the `SIGSTOP` a fact
/// rather than a queued signal and [`await_continued`] does the same for the
/// `SIGCONT`, which is the one that delivers the `EINTR`. The thread count is
/// the mechanism stated directly — a dead serving thread inside a live process
/// — and the second join is the consequence an operator actually meets.
///
/// # Measured, both ways
///
/// On the parent commit this fails at the thread-count assertion with
/// `left: Some(1), right: Some(2)`, `default.sock` gone; with the retry it
/// passes. Three controls, run standalone against the parent commit, say the
/// stop/continue *pair* is what does it and not the act of signalling: no
/// signal at all, three `SIGWINCH`es (default-ignored), and a bare `SIGCONT` to
/// a never-stopped owner each left two threads, the socket in place and the
/// join succeeding.
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

    // The control. Everything after this is a comparison against a rendezvous
    // that demonstrably worked.
    let mut before = Kid::spawn(&scratch.0, &["join"]);
    let joined = before.line();
    assert_eq!(
        joined.strip_prefix("joined "),
        Some(owner_value.as_str()),
        "the fixture never served in the first place: {joined}"
    );

    for signal in ["-STOP", "-CONT"] {
        if signal == "-CONT" {
            // Between the two: the target is genuinely stopped, so the SIGCONT
            // below is a *continue* rather than a no-op on a running process.
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
///
/// **The `layout_if_creating` is load-bearing and the test is vacuous without
/// it.** Without a layout, `ro` + a creating policy already failed, with
/// `NoLayoutToCreate`, so an assertion on the *new* variant would pass against
/// a build that never learned the rule. Supplying the builder is what makes the
/// old code reach `OpenOutcome::Created`.
///
/// Both creating policies, because `Always` is a different branch of the
/// rendezvous (it skips the split-brain yield) and only `IfAbsent` would be
/// covered otherwise.
///
/// **Mutant: allow the combination** ⇒ the first open returns `Ok` and the
/// re-open below finds a freshly created empty arena instead of `ArenaAbsent`.
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
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaAbsent)
        ),
        "the refused open left an arena behind: {err:?}"
    );
}

/// **`RUNBOOK.md`'s escape hatch out of `ArenaHeldButUnreachable`, run as written.**
///
/// `docs/PHASE2.md` §3.4 asks for it as `--force-new` and no binary ever grew
/// the flag (#189, and §0.0's row), so the procedure an operator follows is
/// `CreatePolicy::Always` on the process that creates the arena — and until this
/// test nothing above `tf_tree_ipc` ran it. That crate's
/// `create_always_overrides_the_split_brain_check` stops at the lock file; the
/// half a stranded operator needs is that the policy carries through
/// `build_shared` and comes back holding a `Tree`.
///
/// The stranded participant is a bare lock byte rather than a process, because a
/// held byte is the entirety of what §3.4 step 4 consults. A second process
/// would add a socket, a mapping and a race without changing the input.
///
/// The last two assertions are what separates the escape hatch from slot
/// reclamation (#184): it abandons the arena and **leaves the byte held**, so
/// the replacement's table is one slot smaller for as long as the survivor runs.
///
/// **Mutant: drop `self.create != CreatePolicy::Always &&` from step 4's
/// condition** ⇒ measured — `Always` yields the ownership byte like every other
/// policy, and the second half fails with `ArenaHeldButUnreachable`.
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
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaHeldButUnreachable { .. })
        ),
        "expected ArenaHeldButUnreachable, got {err:?}"
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
///
/// `the_escape_hatch_creates_over_a_stranded_participant` above shows
/// `CreatePolicy::Always` working; this shows where it stops, and the two
/// together are what §3.4's prose is now written against. The rule the code
/// implements is not "force always creates" and not "force never creates": a
/// forced create skips step 4's participant scan and **nothing else**, so it
/// still has to take the ownership byte (step 2) and still has to take
/// participant byte 0 (step 5, `0035`'s `CREATOR_SLOT`). Hold either one live
/// and it refuses exactly like an ordinary open.
///
/// That is why the operator-facing bit is *which* byte is held, not whether one
/// is: byte 0 is the creator's own slot and the owner keeps it for its whole
/// life, so a live holder of it is an arena whose owner is still there — and no
/// force can help. Bytes `>= 1` are joiners, so a live holder of one with byte 0
/// free is the owner-is-gone wedge §3.4 offers the hatch for.
///
/// Measured at `09efc9b`, before the `ownership_held` field existed, the two
/// states printed the *same* sentence: "participant slots 0x1 still hold their
/// lock bytes (slot 0, pid 0) and none took over ownership before the deadline;
/// refusing to create a second arena" against the identical line with `0x8` and
/// `slot 3` in it — differing only in a slot number whose meaning appears
/// nowhere in the message. The `assert_ne!` below is that they no longer do.
///
/// The holders are bare lock bytes rather than processes because a held byte is
/// the entirety of what steps 2, 4 and 5 consult; a second process would add a
/// socket, a mapping and a race without changing the input. Each is its own
/// open file description, since `F_OFD_GETLK` reports only *conflicting* locks
/// and a description's own byte does not conflict with itself.
///
/// **Mutant: `ownership_held: false` hardcoded at the construction site**
/// (`Open::held_but_unreachable`). Applied ⇒ measured: the sibling test below
/// fails, and it fails one assertion *earlier* than the message check — at
/// `expected ArenaHeldButUnreachable with ownership_held, got …
/// ownership_held: false`. **Mutant: `(Some(0), _) if false`, i.e. the slot-0
/// arm folded back into the generic one.** Applied ⇒ measured, this test fails
/// on `the message must not send an operator to the escape hatch here`, and the
/// message it printed was the stranded-joiner one — "CreatePolicy::Always will
/// create a fresh arena and abandon this one", recommended for the one state
/// where it cannot work.
#[test]
fn a_live_byte_0_refuses_both_policies_and_says_no_force_can_pass() {
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

    // The creator's own byte, held live. Nothing holds ownership: the point is
    // that byte 0 alone is enough to refuse.
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
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaHeldButUnreachable {
                holder_slots: 0b1,
                first_slot: Some(0),
                ownership_held: false,
                ..
            })
        ),
        "expected ArenaHeldButUnreachable naming slot 0 with ownership free, got {forced:?}"
    );
    let byte_0_message = forced.to_string();
    assert!(
        byte_0_message.contains("no forced create can pass this"),
        "the message must not send an operator to the escape hatch here: {byte_0_message}"
    );
    assert!(
        byte_0_message.contains("it is the only holder, so an ordinary open will then create"),
        "byte 0 alone: stopping it really is sufficient, and the message may say so: \
         {byte_0_message}"
    );

    // **The remedy branches on the rest of the mask, and getting that wrong is
    // how this message tells an operator something false.** Add one ordinary
    // joiner's byte and nothing else changes about slot 0 — but "stop it and an
    // ordinary open will create" stops being true, because step 4 still sees
    // byte 2. Measured: stopping only the byte-0 holder in this state leaves
    // `IfAbsent` refusing and makes `Always` the *only* thing that works, which
    // is the opposite of what the un-branched sentence advised.
    let alongside = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        alongside.try_take_participant(2).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );
    let crowded = refusal(CreatePolicy::Always);
    assert!(
        matches!(
            crowded,
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaHeldButUnreachable {
                holder_slots: 0b101,
                first_slot: Some(0),
                ..
            })
        ),
        "expected slot 0 plus slot 2 in the mask, got {crowded:?}"
    );
    let crowded_message = crowded.to_string();
    assert!(
        crowded_message.contains("the other slots in the mask above are still held"),
        "with a joiner still attached the remedy is not 'an ordinary open will create': \
         {crowded_message}"
    );
    assert!(
        !crowded_message.contains("it is the only holder"),
        "the two remedies must not both appear: {crowded_message}"
    );
    drop(alongside);

    // **The positive control.** One variable changes — which byte is held — and
    // the same forced create that just refused now succeeds. Without this the
    // test would pass just as well against an `open()` that never creates.
    drop(on_byte_0);
    let stranded = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        stranded.try_take_participant(3).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );
    let still_refused = refusal(CreatePolicy::IfAbsent);
    let stranded_message = still_refused.to_string();
    assert!(
        stranded_message.contains("escape hatch is for"),
        "a stranded joiner is the case the hatch is for: {stranded_message}"
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
///
/// A forced create takes the ownership byte before it reaches the participant
/// bytes it is allowed to skip, so a live holder of *that* byte refuses it even
/// with byte 0 free and only a joiner stranded — the shape that otherwise looks
/// exactly like §3.4's escape-hatch case. Reachable in the field as §11.3's
/// `open.after_create_before_bind` window and as any process that took ownership
/// and has not begun serving.
///
/// It matters because the remedy differs: here there is a second process to
/// stop, and an error that recommended `CreatePolicy::Always` on the strength of
/// `first_slot: Some(1)` alone would send an operator down a path that fails.
///
/// The control is the isolating one: **only** the ownership byte goes away —
/// same stranded joiner on byte 1, same free byte 0, same policy — and the
/// create succeeds.
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
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaHeldButUnreachable {
                first_slot: Some(1),
                ownership_held: true,
                ..
            })
        ),
        "expected ArenaHeldButUnreachable with ownership_held, got {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains("CreatePolicy::Always will not pass this"),
        "the message must not recommend the hatch here: {message}"
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

/// **A pinned defect (#201), not desired behaviour. Invert or delete this test
/// when the divergence is fixed** — every assertion below states what the code
/// does today, and two of them state something wrong: that one process's lock
/// byte and its arena record are different integers, and that a live,
/// publishing arena owner reads dead.
///
/// `docs/PHASE2.md` makes the participant slot *one* number: its §0.0 table
/// says "the arena slot and the lock byte are the same integer", and §3.7's
/// `HelloResponse` repeats it field by field — `participant_slot` "matches the
/// lock-file byte the client must take". That is why
/// `Tree::participant_alive` can take an arena record index and hand it
/// straight to a probe of the lock byte at that index
/// (`crates/tf_tree/src/open.rs`'s `LivenessProbe::is_held`).
///
/// [`CreatePolicy::Always`] is the path on which it can break. §3.4 step 4 reads
/// `if self.create != CreatePolicy::Always && lock.any_participant_held()?`, so
/// the escape hatch skips the split-brain check *by design*. **The rest of this
/// paragraph described what happened before `0035`**, and is kept because the
/// test below is named for it: the create fell through to
/// `take_any_participant`, the first **free** byte, so with byte 0 already held
/// the creator got byte 1 while `build_shared` handed it arena record **0** on a
/// fresh arena, and nothing reconciled the two afterwards — `hold_ownership`
/// parks the session and never compares `Session::slot` with
/// `Tree::participant`. Step 5 is now `register_creator` →
/// `try_take_participant(0)`, which on `Contended` releases ownership and loops,
/// so the forced creator is **refused** rather than diverged. That is what this
/// test's own body asserts, in `first_slot: Some(0)`.
///
/// **How that precondition arises is not known, and #201's answer to it is
/// measurably wrong.** The issue has the divergence biting "precisely in the
/// scenario the flag exists for: survivors are holding bytes" — an operator
/// force-creating past a wedge, with survivors on `0..k`. Run, that scenario
/// does not diverge. One `own` child plus two `join-rw` children hold bytes
/// `[0, 1, 2]`; `SIGKILL` the owner and the held set is `[1, 2]`; a
/// `CreatePolicy::Always` creator then takes byte **0** *and* arena record
/// **0**, one integer again. That is structural, not luck: an ordinary
/// creator's participant byte *is* 0, because step 4 refused to create while
/// any byte was held, and one `LockFile` description carries both it and the
/// ownership byte — so the death that frees the ownership byte frees byte 0 in
/// the same kernel action. §3.4 step 2 gates the hatch on that byte, so
/// against an owner that is wedged rather than dead (`SIGSTOP`, still holding
/// it) the hatch never reaches step 4 at all and `open()` fails
/// `ArenaHeldButUnreachable` naming the stuck slot. While the owner lives it
/// keeps arena record 0, and `open.rs`'s `assign` closure skips a record that
/// has an identity, so no joiner is granted slot 0 either: every byte a
/// survivor can hold is >= 1.
/// `the_escape_hatch_creates_over_a_stranded_participant` strands its byte at 3
/// and its creator takes byte 0 with record 0 — harmless for the same reason.
///
/// Those three readings were taken out of tree, driving
/// `tf_tree_rendezvous_child` and `tf_tree::Open` from a scratch binary, and
/// **nothing in this file pins them**. What they establish is negative: the
/// route #201 gives for reaching the divergence does not reach it.
///
/// So the divergence needs a **live holder of byte 0 that is not the arena's
/// owner**, and this test stages one with
/// `tf_tree_ipc::LockFile::try_take_participant(0)`, which nothing in the
/// workspace does outside tests. Two independent attempts to construct that
/// holder through `tf_tree::Open` alone failed. **A failed construction is not
/// an unreachability argument**, and settling which of the two this is — a
/// defect no public API can reach, or one nobody has found the route to — is
/// the question `0028` has to answer before #189's `--force-new` flag would
/// turn this policy into a documented operator procedure.
///
/// **#201's second path — the takeover arm — is closed by deletion.** It took
/// the first free byte through `register_any`; both are gone, and so is
/// `OpenOutcome::TookOver`, which `0037` question 3 answers `no` to: a takeover
/// is not an outcome of `open()`. `0028` question 3 is why the arm could not be
/// repaired — the heir keeps its existing slot, byte and arena record, and §3.5
/// cannot be wired as a second `Open::open` call. `0037` records the five
/// unsound states two rounds of repair produced before that landed, and §3.5
/// now ships as `Session::take_over_ownership`. What is pinned below is therefore the
/// *consequence* of the remaining divergence, on a staged instance of it, and
/// not its reachability.
///
/// Both signs below are read from a process that is not the creator, because
/// the creator's own `participant_alive(0)` hits `use_ofd_liveness`'s "never
/// report ourselves dead" guard — which compares the queried slot against the
/// *arena record* — and would show neither:
///
/// - **The right verdict, off the wrong byte.** While the survivor holds byte
///   0, record 0 reads alive on the strength of a byte belonging to somebody
///   else — a byte carrying no identity at all, since the survivor never wrote
///   one. The answer is nonetheless *correct* here: the process in record 0 is
///   alive. The genuinely wrong `alive` — the forced creator dies while the
///   survivor keeps byte 0, so record 0 reads alive about a dead process — is
///   **not measured, here or anywhere**. It would need a helper mode that
///   creates with `CreatePolicy::Always` inside a child, so the parent can
///   `SIGKILL` the creator and keep questioning a watcher that joined before
///   it died; `rendezvous_child.rs` has no such mode and this test does not add
///   one.
/// - **False dead, which is the corrupting one.** The survivor releases byte 0
///   and the verdict about record 0 flips to dead, though the process in that
///   record has not moved: it still holds byte 1, still owns the arena, still
///   publishes. **Nothing acts on that verdict destructively today**, which is
///   why this is a pinned defect rather than an incident: `Tree::reap_inner`
///   decides claim reaping from `lock.probe_claim(edge)` — the claim byte, held
///   by the live publisher — and not from `participant_alive`; the owner's
///   arena-record reap is driven by socket `HUP`; and `tf_tree_cli`'s
///   participant checks say "Detection only. Nothing here reclaims anything" in
///   as many words. The harm is conditional on a rescuer that reclaims from
///   this predicate — which `docs/PHASE2.md` §5.1 makes the authority on
///   liveness, and which `0028`'s piece 2 is a design for.
///
/// That same piece 2 would make the lock byte the *whole* predicate, with no
/// `record_is_alive` fallback — which does not soften this today (the fallback
/// is reached only on a probe *error*, and a free byte probes fine as
/// `Some(false)`) but would make the divergence unconditional rather than a
/// property of the probe a tree happens to carry.
///
/// Mutant: drop the `tree.use_ofd_liveness(...)` on `open`'s `Joined` arm, so
/// the watcher answers from the `/proc` inference instead of from the lock
/// byte. Applied: both asks report `slot 0 state live word 0x6 pid <creator>
/// alive true` and the final assertion fails. That is the reading of this test
/// — it measures the byte-index path and nothing else. Record 0's own pid was
/// alive the whole time, so every predicate that asked about *the record* got
/// the right answer; only the one that asked about the byte at its index did
/// not.
///
/// ---
///
/// **2026-08-20 — the consequence above is no longer constructible, and the
/// name is history** (`docs/decisions/0028` plan step 0c). The name is kept
/// because `CHANGELOG.md`'s `0.0.3` *Known issues*, `docs/PHASE2.md` §0.0's
/// participant-registry row and `0028` itself all cite it; renaming would leave
/// four dangling references for a cosmetic gain, and the transcript above is
/// the evidence those documents point at.
///
/// **Step 0c asked whether this test survives, on the reading that a facade
/// assertion cannot reach a state staged below the facade. Run, it does not
/// survive as written.** The state is staged below the facade, but the byte and
/// the record are *paired* at it — `Open::attempt` is what builds the arena and
/// registers the record — so the guard sits directly on this path. Measured, at
/// the guard's first green build: `CreatePolicy::Always must create over a
/// stranded participant: ParticipantSlotDiverged`, this test's own `expect`
/// panicking. So the staging is not a way around the assertion, which is the
/// answer the step wanted.
///
/// **What is lost, stated because it is the only thing this file loses.** The
/// measurement that gives the block above its force — a joined watcher
/// reporting `alive false` about a live, publishing process holding record 0 —
/// can no longer be staged *through this route*, because `Open::attempt` now
/// refuses a diverged pair.
///
/// **It is not unreachable, and an earlier revision of this comment said it
/// was.** That revision claimed nothing would now produce a `Tree` whose byte
/// and record disagree, and it conflated the judged tree with the judging one:
/// the probe belongs to the *observer*, and the subject needs no byte at all. A
/// directly-called `TreeBuilder::build_shared` registers a `LIVE` record with no
/// lock byte, and a facade joiner reads it dead —
/// `a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`
/// below pins exactly that, with no divergence anywhere in it. The transcript
/// survives here, in `docs/PHASE2.md` §0.0 and in `0028`.
///
/// **What it pins now**, and neither is covered by its sibling
/// `defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`: that the
/// guard is *identity-blind* — this byte 0 carries no identity record at all,
/// where the sibling's is a real registration — and that a caller can act on
/// the refusal, which is the argument for returning an error rather than
/// asserting. Release the byte and the ordinary create succeeds with byte and
/// record at one index.
///
/// Mutant: delete the `session.slot() != tree.participant_slot()` guard from
/// `Open::attempt`. Applied: `.err().expect("a create that would diverge must
/// be refused")` panics on an `Ok(Tree)`.
///
/// Mutant: compare against `session.slot()` twice, i.e. a guard that can never
/// fire. Applied: the same `expect` panics — the recovery half of the test is
/// never reached, so a tautological guard cannot pass this file.
///
/// Mutant: `std::mem::forget` the session instead of dropping it on the refusal
/// path. Applied: `the refused creator kept a participant byte` fails,
/// `left: 3, right: 1`.
#[cfg(feature = "unstable")]
#[test]
fn defect_201_a_forced_creators_record_reads_dead_while_it_is_publishing() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, Stamp, TreeBuilder};

    let scratch = Scratch::new("slot-divergence");
    let lock_path = scratch.0.join("0/default.lock");
    std::fs::create_dir_all(scratch.0.join("0")).unwrap();

    // The stranded survivor, on **byte 0** — the index a fresh arena also gives
    // its first participant record. A separate open file description, so its
    // byte conflicts with everything that asks about it.
    //
    // **Staged through `tf_tree_ipc`, because no sequence of `tf_tree::Open`
    // calls is known to produce it** — see the doc comment. An owner death
    // frees byte 0 along with the ownership byte, so the operator scenario
    // #201 names arrives here with byte 0 free and diverges from nothing.
    let survivor = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        survivor.try_take_participant(0).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );

    // **Refused — and since `0035` it is refused one layer lower, with a better
    // error.** Three revisions of this line, each measured:
    //
    // 1. `.expect("CreatePolicy::Always must create over a stranded
    //    participant")` — it did, with byte 1 against record 0. That is #201.
    // 2. `0028` step 0c added the facade guard, and this became
    //    `ParticipantSlotDiverged`: no divergence reached a caller, but the
    //    error named nothing an operator could act on.
    // 3. `0035` makes step 5 take byte 0 atomically, so the create never gets
    //    that far. Byte 0 is held, the acquire is contended, and that is step
    //    4's condition — yield, back off, and time out into
    //    `ArenaHeldButUnreachable`, which **names the holder**: `first_slot:
    //    Some(0)`. That is the difference worth pinning. `ParticipantSlotDiverged`
    //    tells an operator that two integers disagreed; this tells them which
    //    slot to look at.
    //
    // Everything else this test asserts is unchanged, and that is the point of
    // keeping it: the refusal still disturbs nothing.
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
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaHeldButUnreachable {
                first_slot: Some(0),
                ..
            })
        ),
        "expected ArenaHeldButUnreachable naming slot 0, got {err:?}"
    );

    // **What this pins that its sibling cannot.** The guard compares two
    // integers and consults nothing else. This byte 0 carries *no identity
    // record at all* — `try_take_participant` writes none — so a guard that
    // tried to decide from the lock file's identity rows, or to excuse a
    // divergence whose byte names nobody, would have nothing to read here.
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
    // what it does *not* catch is the guard's **placement**, because
    // `impl Drop for OwnerServer` unlinks either way. Both measured, in the
    // sibling test, which carries the note.
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
///
/// The test above pins the *consequence* on a state built with
/// `LockFile::try_take_participant(0)`, and says in as many words that no
/// sequence of `tf_tree::Open` calls was known to produce it. One does, and it
/// is neither of the two paths #201 was filed on — both of those were measured
/// not to diverge (#214, #215): an owner death frees byte 0 along with the
/// ownership byte, and the takeover arm now returns the caller's own slot
/// rather than taking a free byte.
///
/// The producer is [`tf_tree_ipc::Session::release_ownership`], which gives up
/// the **ownership** byte and keeps **participant byte 0**. That is exactly
/// what §3.5 asks of it — "give up the owner role while staying attached" —
/// and what it leaves behind is a live **non-owner** on byte 0, the state
/// `docs/PHASE2.md` §0.0 said nothing outside a test produces. A forced create
/// then takes byte 1 against arena record 0, and every liveness predicate reads
/// record 0's liveness from byte 0, which belongs to somebody else.
///
/// **What this adds over the test above is the route, so it stops where the
/// route is established.** The corrupting consequence is measured there and is
/// not measured twice here. One thing does differ and is worth an assertion:
/// the staged survivor writes no identity, so its byte names nobody, while this
/// one is a real registration — byte 0 carries a plausible `tf_tree
/// participants` row for a process that is not the one in record 0.
///
/// Both sessions are opened by path, so each holds its own open file
/// description and the bytes genuinely conflict; that is the same reason the
/// staged survivor above works inside one process. Identity is *not* read to
/// tell the two apart here, because in one process they share a pid — the
/// holder mask is what separates them.
///
/// **2026-08-20 — converted from a defect pin into a regression test**
/// (`docs/decisions/0028` plan step 0c). Everything above is kept because the
/// *route* is the finding: two derivations of how this state arises were wrong
/// before a reproduction found it, and this project keeps its refutations on
/// the page. What changed is the verdict at the end. `Open::attempt` now
/// compares `tf_tree_ipc::Session::slot` with `Tree::participant_slot` at the
/// single `hold_ownership` call site — the one place in the workspace where
/// both numbers are in scope, since `tf_tree_ipc` has no arena dependency and
/// cannot see a record index — and returns
/// [`tf_tree::OpenError::ParticipantSlotDiverged`] instead of a `Tree` whose
/// every liveness answer would be about the stranded session. The assertions
/// that used to read `tree.participant_slot() == 0` with `0b11` bytes held are
/// now the refusal, plus the proof that it leaves the rendezvous exactly as it
/// found it.
///
/// Mutant: delete the `session.slot() != tree.participant_slot()` guard from
/// `Open::attempt`. Applied: `.err().expect("the divergence must be refused,
/// not handed back as a Tree")` panics — the open returns `Ok` and the whole
/// test is downstream of that line.
///
/// Mutant: `std::mem::forget` the session instead of dropping it on the refusal
/// path. Applied: `the refused creator kept a participant byte` fails,
/// `left: 3, right: 1` — bytes 0 and 1 both held. That is the assertion that
/// makes this a refusal which costs a caller nothing rather than one that burns
/// a slot per attempt.
///
/// Mutant: drop `stranded` before opening the forced creator, so byte 0 is free
/// when it runs. Applied: the same `expect` panics, because the creator then
/// takes byte 0 itself, the two indices agree and the open legitimately
/// succeeds. That is the reading of this test — the stranded byte 0 is the
/// whole cause, and `CreatePolicy::Always` on its own diverges from nothing, so
/// the guard refuses no healthy create. (Before the conversion this mutant
/// failed the **holder-mask** assertion first, `left: 1, right: 3`.)
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
    // role. Every call here is published API; none of it is test-only.
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
    // which this test has arranged to be held by a **real registration** with an
    // identity record behind it. Contended, so the create yields.
    //
    // **`--force-new` cannot abandon an arena whose byte 0 a live process
    // holds, and it could not before `0035` either.** §3.4 calls it "an
    // explicit, loud escape hatch that abandons the existing arena", and for the
    // `SIGSTOP`ped-participant case it describes, that is not what happens: it
    // was `ParticipantSlotDiverged` after `0028` and it is
    // `ArenaHeldButUnreachable` now. The gap is pre-existing and is not this
    // record's to close — what changed is that the error now names the slot.
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
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaHeldButUnreachable {
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

    // **No bound rendezvous socket survives the refusal.** That is less than an
    // earlier revision of this comment claimed — it said the line below made the
    // guard's placement load-bearing — and it is worth keeping for what it does
    // catch: a refusal that returns while the owner server it spawned is still
    // alive. Measured: with the guard moved below `spawn_owner_server` *and*
    // the server `std::mem::forget`-ed instead of dropped, both `defect_201`
    // tests fail at this assertion — `a refused create must not leave a bound
    // rendezvous socket`.
    //
    // **It does not pin the placement, and nothing in this file does.** Measured
    // by moving the guard below `spawn_owner_server` on its own: the socket is
    // bound and published — an `eprintln!` from inside the refusal reports `sock
    // exists now = true` — and then `impl Drop for OwnerServer`
    // (`crates/tf_tree_ipc/src/server.rs:475`) unlinks the path it published, so
    // both tests stay green and this assertion never notices. The placement
    // rests on an argument instead, stated at the guard in
    // `crates/tf_tree/src/open.rs`: between the bind and the guard a joiner can
    // complete a §3.7 handshake and hold the segment, and refusing after that
    // would tear an arena out from under a process that did nothing wrong. That
    // needs a second process scheduled inside those two statements, which no
    // test here can arrange, so the ordering is defensible on its own merits and
    // untested.
    assert!(
        !scratch.0.join("0/default.sock").exists(),
        "a refused create must not leave a bound rendezvous socket"
    );

    // **And it left nothing behind.** Byte 1 — which the forced creator would
    // have been handed before `0035`, and now never reaches — is free, and so is the ownership byte it took
    // on the way in; only the stranded session's byte 0 is still held, by the
    // process entitled to it. A refusal that returned while its `Session` lived
    // would burn a slot per attempt, which is the failure
    // `require_create_refuses_a_live_arena_and_releases_its_slot` pins for the
    // other refusal on this path.
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

    // **`0035` removed the residue this used to pin.** Until then the refused
    // creator had *taken* byte 1 and written an identity there before the
    // facade compared the two integers and refused, so byte 1 kept an advisory
    // row it no longer owned — harmless under §5.1, which makes the byte the
    // liveness and the record advisory, but a row naming a process that never
    // served. The assertion here read `is_some()` and explained why that was
    // acceptable.
    //
    // Now the create never reaches a second byte: step 5 asks for byte 0, is
    // refused, and yields. Nothing is taken and nothing is written, so the
    // refusal leaves the lock file **exactly** as it found it. That is a
    // stronger property than the one it replaces and is worth pinning in its
    // place.
    assert!(
        witness.read_identity(1).unwrap().is_none(),
        "a refused create must take no byte, and so write no identity behind it"
    );
}

/// **`Open::require_create` refuses to join, and leaves nothing behind.**
///
/// `CreatePolicy` has no "create, or refuse if one is already live" setting, so
/// a second arena owner — `docs/decisions/0015`'s ROS bridge is the caller this
/// exists for — would otherwise take the *join* path and start claiming edges
/// in an arena somebody else sized (`docs/decisions/0019` §3, question 1).
///
/// The second half is the part worth a test: a refusal that returned while its
/// `Session` lived would leave this process's participant lock byte taken and a
/// socket the owner still counts, so a bridge that retried a few times would
/// exhaust the slot table.
///
/// **Mutant: return `ArenaAlreadyLive` without dropping the session** ⇒ slot 1
/// reads held after the refusal and the second assertion fails.
/// **Mutant: ignore `require_create`** ⇒ the open returns `Ok` and the first
/// `expect` fails.
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

    // The owner holds slot 0. The refused attach was granted slot 1 and must
    // have given it back.
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
///
/// `docs/decisions/0019` §2b's first wait. The owner is spawned ~200 ms after
/// the wait begins, so `Ok` alone proves nothing — the elapsed lower bound is
/// what says the call really waited rather than racing to a lucky attach.
///
/// **Mutant: classify `ArenaAbsent` as terminal** ⇒ `await_open` returns
/// `Err(ArenaAbsent)` in microseconds and the `expect` fails.
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
///
/// **Run on a worker thread with a `recv_timeout` on the main one, and that is
/// deliberate.** This repository has **no `.config/nextest.toml`**, so there is
/// no `slow-timeout` / `terminate-after` to bound a test that never returns: an
/// `await_open` that ignored its deadline would hang the whole suite instead of
/// failing one test. The channel is this test supplying its own bound.
///
/// **Mutant: ignore the deadline** ⇒ the `recv_timeout` expires and this fails
/// with the message below, rather than the run hanging.
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
        "await_open never returned: it ignored its deadline. There is no \
         .config/nextest.toml in this repository, so nothing else would have \
         bounded this",
    );
    let err = err.expect("an empty machine has no arena to open");

    // **The last retryable error, verbatim — there is no `Timeout` variant.**
    // `ArenaAbsent` already says exactly what was true for the whole budget,
    // and a second spelling would carry strictly less.
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaAbsent)
                | tf_tree::OpenError::Rendezvous(
                    tf_tree_ipc::IpcError::ArenaHeldButUnreachable { .. }
                )
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
///
/// `await_open` derives each attempt's timeout by subtracting elapsed time from
/// the caller's budget, and that subtraction is the only thing in the crate that
/// manufactures a `Duration` nobody wrote. `await_open(Duration::from_secs(1))`
/// reaches the handshake as `999.999_9xx ms` on the first iteration; the
/// conversion to `SO_RCVTIMEO_NEW`'s `(tv_sec, tv_usec)` rounds the
/// sub-microsecond tail up without carrying, yielding `tv_usec == 1_000_000`,
/// which the kernel rejects with `EDOM`. That arrives as
/// `IpcError::ClientSocketSetup`, which `is_retryable` calls **terminal** — so
/// the call returned in microseconds with a local-resource error instead of
/// waiting out its budget, and every whole-second budget was affected.
///
/// The budget is `1 s` and not `300 ms` **on purpose**:
/// `a_wait_for_an_arena_that_never_starts_gives_up` uses 300 ms, whose remainder
/// lands at `tv_usec == 300_000`, and that is why fifteen passing rendezvous
/// tests were blind to this.
///
/// Mutant: delete the `Duration::new(secs, subsec_micros * 1_000)` truncation in
/// `Open::await_open` ⇒ this fails on the error assertion with
/// `ClientSocketSetup { raw_os_error: 33 }`, and on the elapsed one, having
/// returned almost immediately.
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

    // Same shape as the test above, and for the same reason: there is no
    // `.config/nextest.toml`, so nothing else bounds a call that hangs.
    let (err, elapsed) = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("await_open never returned");
    let err = err.expect("an empty machine has no arena to open");

    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaAbsent)
                | tf_tree::OpenError::Rendezvous(
                    tf_tree_ipc::IpcError::ArenaHeldButUnreachable { .. }
                )
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
///
/// The owner declares `frame_headroom`, because the shared fixture declares
/// none and a late intern into it fails `CapacityExceeded` — which would time
/// the waiter out for a reason that has nothing to do with the wait.
///
/// The consumer's id must equal the *owner's*, which is what rules out the
/// failure this method is shaped to avoid: a wait that interned the name itself
/// and handed back an id for a frame nobody declared.
///
/// **This is the only test in the repository that pins the predicate.**
/// `await_frames`'s `is_writable` guard fires before the predicate on every tree
/// a default build can construct, so `tests/await_frames.rs` is blind to it; a
/// read-only handle needs a live shared arena, and this is where one exists.
/// `just shm-rendezvous` is therefore the predicate's entire gate.
///
/// **Mutant: build the predicate on `Tree::frame`** (`match self.frame(name)`,
/// `is_writable` guard untouched) ⇒ verified. *"FAIL [0.009s] … the frame was
/// interned well inside the budget: Frame(ReadOnly)"* — the read-only consumer
/// is refused on the first probe and never resolves.
/// `a_frames_wait_for_a_name_nobody_will_intern_gives_up` fails with it
/// (*"left: Frame(ReadOnly), right: Timeout { … }"*), so the predicate has two
/// killers, both in this file. The same mutant leaves
/// `cargo nextest run -p tf_tree --test await_frames` at *"5 tests run: 5
/// passed"*, which is the measurement that moved this note here.
///
/// # What this test does not cover, stated rather than implied
///
/// An earlier revision claimed *"drop the memoization or the deadline ⇒ caught
/// by the elapsed bounds"*. **Both mutants pass**, measured:
///
/// - `if false && slot.is_some()` (memoization off) ⇒ *"15 tests run: 15
///   passed"*, this test at 0.210 s against 0.217 s unmutated, and the plain
///   `await_frames` target at *"5 tests run: 5 passed"*. It cannot be caught
///   here, and it cannot be caught anywhere: `find_frame` is idempotent and
///   frames are append-only, so re-probing a found name returns the same id for
///   a few hundred nanoseconds of hashing. The memoization is a **cost**
///   property with no observable behaviour, and this note says so instead of
///   inventing a guard for it. If that ever needs pinning it wants a benchmark,
///   not an assertion.
/// - `if false && start.elapsed() >= timeout` (deadline off) ⇒ **this test still
///   passes**. The frame arrives at 200 ms against a 20 s budget, so it never
///   reaches the deadline at all. That gap is real and is now closed by
///   `a_frames_wait_for_a_name_nobody_will_intern_gives_up` below — the only
///   test in the run that mutant fails — which is where the deadline claim
///   belongs.
///
/// What the elapsed bounds here *do* pin is that the call waited rather than
/// racing to a lucky read — the same thing they pin in
/// `a_consumer_waits_for_an_arena_that_starts_late`.
///
/// It also pins the **live** half of `ArenaBacking::is_frozen`, which
/// `tests/frozen.rs` cannot: mutating `Mapped(_) => true` ⇒ verified, *"the
/// frame was interned well inside the budget: FrozenTree"*.
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
    // for. Taking the pipe rather than calling `Kid::poke` leaves `owner`
    // borrowable for `line()` afterwards.
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
///
/// Before this test the whole `if start.elapsed() >= timeout { … }` block in
/// `Tree::await_frames` could be deleted and every suite stayed green —
/// measured, `if false && start.elapsed() >= timeout` leaves `just test`'s
/// `await_frames` target at *"5 tests run: 5 passed"*, and in
/// `just shm-rendezvous` it fails **only** this test — *"15 tests run: 14
/// passed, 1 failed"*, so the fourteen that predate it are all blind to the
/// deadline. `docs/decisions/0019` §2b's second wait could hang a consumer
/// forever with nothing to say so. Untested along with it: the
/// first-missing-name hash, the `saturating_sub` clamp, and `all_interned::<0>`.
///
/// **The wait runs on a worker thread and the main thread bounds it with
/// `recv_timeout`.** This repository has **no `.config/` directory at all** —
/// verified, the root dotfiles are `.cargo`, `.claude`, `.git`, `.github`,
/// `.gitignore`, and `find` reports no `nextest.toml` anywhere — so there is no
/// `slow-timeout` or `terminate-after` profile setting, and a call that
/// ignored its deadline would wedge the whole run instead of failing one test.
/// The channel is this test supplying the bound nextest does not. It is the same
/// shape `a_wait_for_an_arena_that_never_starts_gives_up` uses one wait over.
///
/// The `Tree` is built *inside* the thread rather than moved into it: `Scratch`
/// has already put `TF_TREE_RUNTIME_DIR` in this process's environment, so the
/// thread resolves the same rendezvous, and nothing has to be `Send`.
///
/// **`["map", …]` and not just the missing name.** `map` is in the fixture
/// layout, so it resolves on the first probe and is memoized; the reported hash
/// must then be the *second* name's. A `Timeout` that hashed `names[0]` would
/// name a frame that is present, which is the most confusing answer available.
///
/// **Mutants, each applied, run, observed and reverted:**
///
/// - `if false && start.elapsed() >= timeout` (the deadline) ⇒ *"FAIL
///   [30.008s] … await_frames never returned: it ignored its deadline. There is
///   no .config/nextest.toml in this repository, so nothing else would have
///   bounded this: Timeout"*. Thirty seconds and a named failure, not a hang.
/// - `.next()` in place of `.find(|(_, slot)| slot.is_none())` (report
///   `names[0]` rather than the first *missing* name) ⇒ *"the timeout named the
///   wrong frame, or was not a timeout at all — left: Timeout { hash:
///   10663285463286226064 }, right: Timeout { hash: 15926179251682185921 }"*.
///   The left value is `map`'s hash, a frame that is present.
/// - `if N == 0 { return None; }` at the head of `all_interned` ⇒ *"a
///   zero-length request on a waitable tree must be answered without touching
///   the arena — left: Err(Timeout { hash: 0 }), right: Ok([])"*. That `hash: 0`
///   is the `map_or` fallback in the deadline branch, reached because no slot is
///   missing — so this mutant also exercises the arm whose comment calls itself
///   unreachable.
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
        "await_frames never returned: it ignored its deadline. There is no \
         .config/nextest.toml in this repository, so nothing else would have \
         bounded this",
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
///
/// `docs/PROJECT.md` §5 D17 forbids treating staleness as death, because a
/// legitimately slow publisher is indistinguishable from a hung one. This is
/// that rule as an executable assertion.
///
/// **What this does *not* prove, stated because the obvious reading is wrong.**
/// It does not discriminate between `F_OFD_GETLK` and the `/proc` heuristic it
/// replaced: swapping the implementation back leaves this test passing, which
/// was verified rather than assumed. `/proc` gets both cases right here — a
/// stopped process still has a `/proc` entry, and a killed-and-reaped one does
/// not.
///
/// The case where `/proc` is genuinely wrong is **pid reuse**: a dead
/// participant's number handed to an unrelated process, which `record_is_alive`
/// only survives because it also compares the start time. Staging that requires
/// exhausting the pid space, so it is not tested here. What OFD actually buys
/// is that the answer is the kernel's rather than an inference — no parsing, no
/// permission dependence, and no window between reading a pid and acting on it.
/// That is an argument from construction, and this test is not evidence for it.
#[test]
fn a_stopped_peer_is_alive_and_a_killed_one_is_not() {
    let scratch = Scratch::new("liveness");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "));

    // A joiner that will be stopped, then killed. The owner holds slot 0, so
    // the first joiner takes slot 1.
    // Read-write, deliberately. A **read-only** joiner takes a lock byte but
    // writes no arena record — `attach_shared` skips registration when the
    // mapping is not writable — so `participant_alive` reports it dead before
    // the OFD probe is ever consulted. That asymmetry is real and is written up
    // in `docs/decisions/0005`; here it would only make the test measure the
    // wrong thing.
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
///
/// `TFT014`'s participant arm fires on `state == LIVE` with a free lock byte.
/// This is the one shape of that state a test can stage out of real processes,
/// and it is `docs/decisions/0028`'s candidate-B hole 3, *"the owner's own slot
/// leaks unconditionally"* — the owner registers itself and nothing hangs up on
/// it, so no `LIVE -> FREE` transition exists for its record.
///
/// **The control matters more than the positive here.** #184 was written about
/// a `SIGKILL`ed rendezvous joiner under a running owner, and #191 closed that:
/// the owner's socket-hangup callback calls `ParticipantTable::release`, so the
/// killed peer's slot returns to `FREE` and `TFT014` correctly says nothing.
/// A test that only showed the leak would leave the reader unable to tell which
/// of the two shapes the check is for. Both are asserted, on one arena, from
/// one observer.
///
/// **The observer has to be a process that joined before the owner died.** With
/// the owner gone the rendezvous socket is gone, so a fresh attach is refused
/// `ArenaHeldButUnreachable` — asserted below, because it is also the reason
/// `tf_tree doctor --attach` cannot reach this arena: the check's predicate
/// covers the shape, the CLI's only live source cannot get to it.
///
/// Mutant: delete the `table.release(slot, incarnation)` from the hangup
/// callback in `crates/tf_tree/src/open.rs` (i.e. revert #191). Applied: the
/// `#191` assertion fails after the full two seconds of polling with *"it is
/// still: slot 1 state live word 0x6 pid … alive false"* — #184 reproduced,
/// and the exact predicate `TFT014` fires on.
/// Mutant B: have the child answer `alive` from its own `state` word instead
/// of from `Tree::participant_alive`. Applied: the dead owner's slot reports
/// *"slot 0 state live word 0x6 pid … alive true"* and the lock-byte assertion
/// fails. That mutant is `docs/PHASE2.md` §5.1's forbidden inference written
/// out, and it is the defect `0028` is about.
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
    // socket close and the callback releases the record. That is asynchronous
    // — a different process on a different thread — so this polls rather than
    // asserting once; a bounded wait that fails by timing out is the only
    // shape available for "something else will do this shortly".
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
    // lock byte free — and it stays that way for as long as nobody *asks*. The
    // watcher here only reads. `Tree::reap_participants` (`0028` plan step 5)
    // is the code path that clears it, and it runs when a survivor calls it and
    // at no other time; `a_survivor_reaps_the_killed_owners_slot_which_no_hangup_can`
    // is that half.
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
    // CLI check cannot be pointed at — `docs/PHASE5.md` §6's `TFT014`
    // amendment says which shapes it can.
    let mut late = Kid::spawn(&scratch.0, &["join-rw"]);
    let refused = late.line();
    assert!(
        refused.starts_with("error") && refused.contains("unreachable"),
        "a fresh attach to an owner-less arena must be refused, or this leak \
         would be reachable from `doctor --attach`: {refused}"
    );
}

/// **`docs/PHASE2.md` §11.2 scenario 2b — slot recycling under abnormal exit.**
///
/// Attach and `SIGKILL` a read-write participant 128 times against a 64-slot
/// arena, one at a time; every attach must succeed. This is the end-to-end
/// property `docs/decisions/0028` was opened about — #184 measured
/// `slots=63reg/0alive` and every subsequent attach refused
/// `NoParticipantSlots` — and it is the shape a crash-looping node produces in
/// minutes.
///
/// **What it does and does not falsify, measured rather than asserted.** The
/// record's plan names this as step 3's falsifier and says it "fails at HEAD on
/// the 65th". That was written against `f058f4f`, before `528eddd`/#191 landed
/// the owner's hangup callback, and it is **stale**: on `main` this test passes,
/// because a `SIGKILL`ed joiner's socket closes, the owner's `epoll` reports
/// `HUP`, and the callback already frees the record. Disabling only that
/// callback and re-running gives
///
/// ```text
/// attach 64 of 128 was refused: error the arena owner refused this attach:
/// NoParticipantSlots ... every participant slot is taken.
/// ```
///
/// — the 64th and not the 65th, because the owner holds slot 0 and only 63 are
/// ever available to joiners. So this scenario pins the *property*, and after
/// step 3 it holds through two independent mechanisms rather than one; what
/// pins step 3 itself is
/// [`the_assigner_reclaims_a_stale_record_no_hangup_will_ever_clear`], which
/// stages the state the hangup cannot reach.
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
        // `kill` waits, so the kernel has torn down the child's descriptors by
        // the time the next attach starts: its lock byte is released and its
        // socket is closed with no cooperation from it.
        joiner.kill();
    }
}

/// **`docs/decisions/0028` plan step 3 — the assigner decides from the byte.**
///
/// The step-3 falsifier, and it exists because §11.2's scenario 2b above stopped
/// being one when #191 landed. The state under test is the one the hangup
/// callback structurally cannot reach: **a participant record that is not
/// `FREE`, whose lock byte the kernel reports free, in a slot this owner never
/// granted.** 0028's *"Five places candidate B does not reach"* lists its real
/// producers — a §3.5 heir serving an arena whose participants connected to the
/// dead owner, the `epoll::add` failure path (`tf_tree_ipc/src/server.rs`,
/// which deliberately declines to call `on_hangup` after a successful
/// handshake), and the owner's own record — and every one of them is
/// unreachable from public API at `main`: §3.5 is unwired, `epoll::add` fails
/// only under `ENOSPC`/`ENOMEM`, and an owner's death takes its rendezvous with
/// it.
///
/// So the state is **staged** rather than produced, through `unstable`'s
/// `Tree::arena_view` and `tf_tree_core`'s `ParticipantTable::register_at` —
/// the same API a joiner registers itself with, called by a joined read-write
/// participant for slots that are not its own. That is a fixture and this
/// comment says so; what makes it the right fixture is that it is
/// *indistinguishable at the assigner* from all three real producers, which is
/// the only property the assigner can act on.
///
/// The recorded pid is `u32::MAX`, which no process can have — and it is
/// **decorative**: the predicate this test drives consults the OFD lock byte
/// and nothing else (`reclamation_verdict`, 0028 open question 1). A test that
/// depended on the pid would be testing a `/proc` conjunct that was deliberately
/// removed.
///
/// Before step 3 the owner skips every one of these slots for ever
/// (`table.identity(slot).is_some()`) and the attach below is refused
/// `NoParticipantSlots`. After it, the first stale record whose byte reads free
/// is reclaimed and the slot granted.
#[cfg(feature = "unstable")]
#[test]
fn the_assigner_reclaims_a_stale_record_no_hangup_will_ever_clear() {
    use tf_tree_core::participant::{state_of, FREE};

    let scratch = Scratch::new("assigner-reclaims");
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    // A joined read-write participant, so the staging writes go through a
    // writable mapping. It also holds a slot of its own, which the loop below
    // leaves alone because `register_at` refuses a slot that is not `FREE`.
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
///
/// The behaviour change steps 3 and 4 are *for* is not that a dead `LIVE`
/// record is collected — #191's hangup callback already did that, which is why
/// §11.2 scenario 2b passes at `HEAD` — but that a **`RESERVED`** one is.
/// `identity()` returns `None` for a `RESERVED` word, so `release` could never
/// name such a slot and no code in this workspace collected one: a process
/// killed inside `fill_slot`'s two-phase publication lost its slot to
/// everybody, for the life of the segment. §11.3's
/// `attach.after_slot_assigned_before_publish` row promised it "cleared by any
/// reaper" with no reaper behind it.
///
/// **This is a staged fixture and this sentence is the disclosure.** The window
/// is `fill_slot`'s `FREE -> RESERVED` CAS to its publishing `Release` store —
/// four `Relaxed` stores and one `fetch_add` — so a test that tried to
/// `SIGKILL` a registrant inside it would be a flake with an arbitrarily small
/// hit rate, not a test. The record is therefore built the way
/// [`the_assigner_reclaims_a_stale_record_no_hangup_will_ever_clear`] builds
/// its stale `LIVE` one: through `ParticipantTable::register_at`, which **is**
/// `fill_slot`, and then the state word driven back to `RESERVED`. That rewinds
/// exactly one instruction of the protocol — the publishing store — and leaves
/// the bytes a registrant killed after its last identity store leaves, its
/// incarnation bump included.
///
/// What makes it the right fixture is the same property the sibling test rests
/// on: it is **indistinguishable at the assigner** from the real producer. The
/// assigner reads a `state` word and a lock byte, and both are what a killed
/// registrant leaves.
///
/// **What this fails with at `HEAD` is candidate A's argument, performed.**
/// `identity()` reports `None` for a `RESERVED` word, so `HEAD`'s assigner does
/// not skip these slots — it *grants* one, and then `fill_slot`'s
/// `FREE -> RESERVED` CAS refuses the joiner the slot it was just given:
///
/// ```text
/// the owner must reclaim a RESERVED record and grant its slot; ...: error
/// ParticipantTableFull
/// ```
///
/// That is `0028`'s *"decides correctly and cannot act"* — deciding without
/// reclaiming leaves the grant useless — with a different error than the
/// `LIVE` case's `NoParticipantSlots`, because the refusal comes from the
/// joiner's own CAS rather than from the owner's scan.
///
/// Mutant (the narrowing this test exists to fail, and the one a reviewer
/// reaches for): give the assigner's `Reclamation::Reclaimable` arm an
/// `if state_of(observed) != LIVE { continue; }`, i.e. reclaim only the shape
/// #191 already handled. Applied and run: this test fails at the assertion
/// below with `NoParticipantSlots`, and it is the **only** test in the
/// rendezvous target that fails — 28 of 29 pass, including
/// [`the_assigner_reclaims_a_stale_record_no_hangup_will_ever_clear`], which is
/// why that test alone did not pin step 3's widening.
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
    // `LIVE` would pass under the mutant above and prove nothing about the
    // word this test is named for.
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
///
/// The same behaviour change as
/// [`the_assigner_collects_a_record_left_reserved_by_a_killed_registrant`],
/// on the other of the two callers. Step 4 replaced
/// `identity(slot)` + `release(slot, incarnation)` with one `state` load handed
/// to `ParticipantTable::reclaim`, and the widening is the whole of what that
/// buys: `identity` returns `None` for a `RESERVED` word, so the old callback
/// walked past this record no matter how long its process had been dead.
///
/// **Staged, for the reason the sibling test states**, and staged on a
/// participant that is *really there*: the peer joins through the real
/// rendezvous, takes its real lock byte, and its record is then rewound to the
/// word `fill_slot` leaves between its CAS and its publishing store. The
/// `SIGKILL` after that is not staged — the kernel closes the socket and
/// releases the byte with no cooperation from the peer — so what the owner's
/// `epoll` sees, and what its callback then reads, is exactly what a registrant
/// killed in that window produces.
///
/// **This isolates the callback from the assigner**, which is the point of
/// asserting on the word rather than on a later attach: no process joins after
/// the kill, so `assign` never runs, and the only thing that can drive this
/// word to `FREE` is the hangup callback. (`Tree`'s `Drop` cannot: it is
/// `release`, whose CAS names `live_word(inc)` and cannot match the bare
/// constant `RESERVED` — and the peer was `SIGKILL`ed, so it ran no `Drop` at
/// all.)
///
/// Mutant (the narrowing this test exists to fail): restore the callback's
/// pre-step-4 narrowness by testing `state_of(observed) == LIVE` instead of
/// `!= FREE`. Applied and run: this test fails after the full two seconds of
/// polling with *"slot 2 is still 0x1"*, and it is the only failure in the
/// target — 28 of 29 pass. `HEAD` itself fails it the same way, for the same
/// reason by a different route: `identity()` returns `None` for `0x1`, so the
/// `if let Some(..)` never enters.
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

    // Rewind the peer's record to the word `fill_slot` leaves between its CAS
    // and its publishing store, then kill it. The peer never reads its own
    // record, so this changes nothing about the process — only about what the
    // owner finds when the socket closes.
    table
        .get(peer_slot)
        .expect("slot in range")
        .state
        .store(RESERVED, Ordering::Release);
    peer.kill();

    // Asynchronous by construction — a different process, on a different
    // thread — so this is a bounded wait that fails by timing out, the same
    // shape `the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live` uses.
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
///
/// **What this verifies, and what it cannot.** `mode="ro"` is the consumer
/// default (D18) and the Python default, and such a peer takes a lock byte but
/// writes *no* arena record — `attach_shared` cannot register a `PROT_READ`
/// mapping. So the owner's slot table and the lock file disagree about that
/// slot, which this test does observe: slot 1 holds a byte and no record, slot
/// 2 holds both.
///
/// It does **not** verify that the owner consults the lock file when assigning.
/// Removing that check leaves this test passing — verified, not assumed —
/// because the granted-slot bitmask already prevents a re-grant for the life of
/// one owner. The check matters only once §3.5 takeover exists and a *new*
/// owner inherits an arena whose read-only peers it never granted; then the
/// bitmask is empty, the arena table reports those slots free, and the owner
/// names one forever while the joiner loops. That scenario is unreachable
/// today, so the guard is forward-looking and this test does not cover it.
///
/// Kept anyway: the byte/record asymmetry is a real invariant, it is the thing
/// a future reader will be surprised by, and pinning it means a change to
/// read-only registration cannot pass unnoticed.
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

    // A second joiner must land somewhere else. `join-rw` registers, so its
    // slot is observable.
    let mut rw = Kid::spawn(&scratch.0, &["join-rw"]);
    assert!(rw.line().starts_with("joined "), "second peer did not join");

    // The owner holds slot 0. If the assigner ignored the lock file it would
    // hand slot 1 to both peers; the read-write one would then be registered at
    // a slot whose byte belongs to the read-only one.
    let mut probe = Kid::spawn(&scratch.0, &["peer-alive", "1"]);
    let slot1 = probe.line();
    probe.kill();
    let mut probe2 = Kid::spawn(&scratch.0, &["peer-alive", "2"]);
    let slot2 = probe2.line();
    probe2.kill();

    // Exactly one of slots 1 and 2 carries a registered participant: the
    // read-write joiner. The read-only one has a byte and no record, which is
    // the asymmetry under test — what must not happen is *both* peers being
    // assigned the same slot.
    assert_eq!(
        (slot1.as_str(), slot2.as_str()),
        ("alive false", "alive true"),
        "the byte/record asymmetry changed: slot 1 should hold a read-only \
         peer's lock byte with no arena record, slot 2 a registered one"
    );
}

/// **A claim taken through `open()` also holds a kernel lease.**
///
/// The arena `ClaimRecord` alone cannot tell a live holder from a dead one — a
/// `SIGKILL`ed process leaves it set forever. The lease can, because the kernel
/// releases the byte with no cooperation. This checks both halves: that the
/// lease is taken at all, and that it is released when the holder dies.
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
///
/// `F_OFD_GETLK` reports only *conflicting* locks, so a description never sees
/// its own — every edge this process holds reads lease-free. A literal §6.3
/// loop therefore revokes its own live writers, and A4 then *correctly* reports
/// `ClaimRevoked` on the next push. The operator sees a publisher that stopped
/// working and a reaper that says it reaped something; nothing points at the
/// reaper.
///
/// So the assertion is not "reaped 0" alone — it is that **the claim still
/// works afterwards**, which is the property that actually matters.
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

/// A killed writer's edge is reclaimed, and then reclaimable.
///
/// Reaping that clears the record but leaves the edge unclaimable would be
/// worse than not reaping: the operator sees a freed record and still cannot
/// publish.
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

    // The owner sweeps. Its own claim survives; the dead peer's is reclaimed.
    owner.poke();
    let line = owner.line();
    assert!(
        line.starts_with("reaped 1 ") && line.ends_with("still_ours true"),
        "expected exactly the dead peer's edge to be reaped, got {line}"
    );
}

// ---------------------------------------------------------------------------
// The CAS-to-lease window (`docs/decisions/0005` §5)
// ---------------------------------------------------------------------------

/// The tree the in-window hook reaps from. A second, independent participant —
/// the reaper must not be the claimer, or the self-skip would fire.
#[cfg(feature = "test-hooks")]
static REAPER: std::sync::OnceLock<tf_tree::Tree> = std::sync::OnceLock::new();

/// Fires once. A `OnceLock` cannot be unset, so the hook disarms itself instead
/// — the point of the test is that the *retry* succeeds, which it cannot do if
/// a reaper keeps running inside every window.
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
///
/// Between `edge::claim`'s CAS and the lease `SETLK` there is one syscall of
/// exposure. A reaper that runs inside it sees `record held ∧ lease free` —
/// which is precisely its "the holder is dead" signature — and clears a claim
/// that is in the middle of being taken. The claimer would then hold a lease on
/// an edge the arena reports free, and a third process could claim it: two
/// writers on one ring, which is what D7 and A4 exist to prevent.
///
/// `edge::reap` bumps the epoch *before* clearing the owner, and that ordering
/// is the whole reason the window is recoverable: re-reading the epoch after
/// taking the lease detects the reap.
///
/// The window is a syscall wide, so it cannot be hit by racing — hence
/// `CLAIM_WINDOW_HOOK`, which is what `--features test-hooks` exists for. Two
/// participants in one process, because the reaper must not be the claimer: a
/// process skips its own slots, so a self-reap would prove nothing.
///
/// **Mutant: delete the epoch re-check in `take_claim_lease`** ⇒ `claim`
/// returns `Ok`, and the writer it hands back publishes onto a record that was
/// reaped out from under it — visible immediately as `ClaimRevoked` from A4,
/// and as a second process being free to claim the same edge.
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
    // older spelling was working around. `clippy::ok_expect` flags it, and no
    // recipe compiled this configuration until the line below in `shm-check`.
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

// ---------------------------------------------------------------------------
// `docs/decisions/0028` plan step 2 — the reclamation predicate, once.
//
// The predicate is `crate::open::reclamation_verdict`, and it is private. Its
// two production callers are step 3's slot assigner and step 5's
// `Tree::reap_participants`, and both *act* on a verdict rather than report
// one: the assigner stops at the first grantable slot and the sweep reports a
// count. So these reach the verdict itself through
// `reclamation_verdict_for_test`, the `--features test-hooks` seam, for the
// same reason `CLAIM_WINDOW_HOOK` exists — `Live` and `Unknown` are two
// different reasons to collect nothing, and neither a count nor a grant can
// tell them apart.
//
// Real processes, as everywhere else in this file: the two facts the predicate
// is made of — that a `SIGSTOP`ped process keeps its lock byte and a `SIGKILL`ed
// one loses it without cooperating — are the kernel's, and neither is stageable
// in one process.

/// The rendezvous lock file the predicate probes, for a scratch directory.
///
/// The same path `defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`
/// opens: domain 0, the default arena name.
#[cfg(feature = "test-hooks")]
fn lock_path(scratch: &std::path::Path) -> PathBuf {
    scratch.join("0/default.lock")
}

/// Join the arena read-write, as the process that will do the sweeping.
///
/// The sweeper is **this** process rather than a helper, because the predicate
/// takes a `Tree` and a slot and there is no line protocol to invent: the seam
/// returns the verdict directly, so a failing assertion prints the verdict
/// rather than a parse of one. `Tree::reap_participants` is the same shape for
/// the same reason, one feature over.
///
/// `unstable` is on the gate as well as `test-hooks` because four of `0028`'s
/// tests need the same participant — a joined read-write process that is not
/// the owner — to reach the participant table through `Tree::arena_view`:
/// steps 3 and 4's
/// [`the_assigner_reclaims_a_stale_record_no_hangup_will_ever_clear`],
/// [`the_assigner_collects_a_record_left_reserved_by_a_killed_registrant`] and
/// [`the_hangup_collects_a_record_left_reserved_by_a_killed_registrant`], and
/// step 5's
/// [`a_survivor_reaps_the_killed_owners_slot_which_no_hangup_can`]. A second
/// copy of these six lines is the kind of duplication `CLAUDE.md` names.
///
/// Hence the `any`, which is not an aesthetic choice either: `just shm-check`
/// clippies this target at `shm,unstable`, without `test-hooks`, and a
/// `test-hooks`-only helper used by an `unstable` test is a compile error in
/// that pass and in no other.
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
///
/// Parsed from the **last** `)` rather than by splitting the line, because
/// field 2 is the executable name in parentheses and may itself contain spaces
/// and parentheses. `None` if the process is gone.
///
/// This is a read of `/proc`, in a file whose predicate deliberately does not
/// read `/proc`. It is not a second liveness source: it is how the test
/// establishes its own **precondition** — that the target really is stopped —
/// and if `/proc` were unreadable this test would fail loudly rather than
/// quietly assert nothing, which is the direction that matters.
///
/// **Ungated, unlike the rest of this section.** It grew a second caller that
/// carries no feature gate —
/// [`a_stopped_and_continued_owner_still_serves_the_rendezvous`] — and
/// `just shm-check` clippies this target at `shm` alone, where a
/// `test-hooks`-only helper used by an ungated test is a compile error.
fn proc_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.get(stat.rfind(')')? + 1..)?;
    after_comm.split_whitespace().next()?.chars().next()
}

/// Block until `pid` is genuinely stopped by a job-control signal, or panic.
///
/// **This is what makes `kill -STOP` load-bearing.** `kill(2)` returning says
/// the signal was *queued*, not that the target has stopped running, so without
/// a wait the assertion that follows could be measuring a still-scheduled
/// process. And with the wait, deleting the `SIGSTOP` does not leave the test
/// passing — it leaves it panicking here, which is the property this helper
/// exists for.
///
/// Ungated for the same reason as [`proc_state`]: the EINTR regression above
/// stops a process too, and it is not a `test-hooks` test.
fn await_stopped(pid: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // `T` is "stopped (on a signal)". Linux reports a `ptrace`-stopped
        // process as `t`; nothing here traces, but accepting it costs nothing
        // and a spurious failure under a debugger costs a reader an hour.
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
///
/// `docs/PROJECT.md` §5 D17: liveness is the socket and the byte, never a
/// timeout. A `SIGSTOP`ped process is not scheduled — it publishes nothing,
/// advances nothing, answers nothing — and holds every OFD lock it took,
/// because the kernel releases those on *death* and on nothing else. A
/// predicate that called this participant dead would reclaim the slot of a
/// writer that is merely descheduled, which is `docs/decisions/0028`'s eviction
/// hazard exactly.
///
/// # What it pins, precisely
///
/// [`await_stopped`] is the assertion that carries the claim: the test blocks
/// until `/proc/<pid>/stat` reads `T` and **panics if it never does**, so a
/// revision that drops the `kill -STOP` fails here rather than passing on a
/// running process. That was verified by removing the `SIGSTOP` block and
/// re-running, not assumed. The `probes=1` half is a second statement: the
/// kernel *was* asked about this slot, so the `Live` did not come out of the
/// `state` word (`docs/PHASE2.md` §5.1's bug).
///
/// # What it does not pin, so nobody reads it as more
///
/// It does **not** separate the byte from `/proc`. A stopped process is still
/// in `/proc` with an unchanged `start_time`, so `record_is_alive` — the
/// composition `docs/decisions/0028`'s open question 1 rejects — would also
/// answer `true` here. The reason that function is off this path is a *host
/// configuration* (`hidepid`, or `/proc` unmounted, which it classifies as
/// proof of death), and no test in this workspace can stage a host. That
/// argument lives in the predicate's first constraint and is not tested.
/// What this test does separate the byte from is any scheme that requires the
/// target to keep running — a heartbeat or a timeout — because it establishes
/// that the target is not running at all.
///
/// Mutant: `Some(true) => Reclamation::Reclaimable { observed }` — the
/// held-byte arm alone, so the free-byte arm still answers correctly. Applied:
/// this test fails with *left: `"reclaimable word 0x6 probes=1"`, right:
/// `"live probes=1"`* and the message below.
/// `a_killed_participants_slot_is_reclaimable_…` fails with it too, on its
/// pre-kill control, and so does `a_free_word_is_decided_…` on its `LIVE`-word
/// control — three of the five. `the_sweepers_own_slot_…` and
/// `a_live_read_only_joiner_…` pass: neither reaches the held-byte arm, one
/// because the guard answers first and the other because the word does.
#[test]
#[cfg(feature = "test-hooks")]
fn a_stopped_participant_is_live_to_the_reclamation_predicate() {
    let scratch = Scratch::new("verdict-stopped");

    // Slots are handed out in join order and each join is awaited before the
    // next begins, so these numbers are determined rather than hoped for:
    // owner 0, this process 1, the target 2.
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    let sweeper = join_as_sweeper();
    assert_eq!(sweeper.participant_slot(), 1, "the sweeper took slot 1");

    // Read-write, deliberately. A read-only joiner takes a byte and writes no
    // arena record at all — that participant is
    // `a_live_read_only_joiner_is_unknown_not_reclaimable`'s subject, and its
    // verdict is `unknown` for a reason that has nothing to do with this one.
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
    // Load-bearing: `kill` returning means the signal was queued, and this is
    // what makes the rest of the test a measurement of a *stopped* process.
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
///
/// #191's socket-hangup callback releases a killed **joiner's** record within
/// milliseconds, so asserting on one would be racing the owner's own reap for a
/// state that is supposed to disappear. Nothing hangs up on the owner
/// (`docs/decisions/0028`, candidate B's hole 3: *"the owner's own slot leaks
/// unconditionally"* — it registers itself and no socket of its own can close),
/// so its record stays `LIVE` over a byte the kernel has released, for the life
/// of the segment. That is #184's wedge, and it is the state the predicate
/// exists to name.
///
/// The observer has to have joined **before** the owner died: the rendezvous
/// socket dies with it, so a fresh attach is refused `ArenaHeldButUnreachable`.
///
/// `Kid::kill` waits, and after `wait` returns the kernel has torn the owner's
/// descriptors down — so this needs no polling and no sleep.
///
/// Mutant: `Some(false) => Reclamation::Unknown` — the free-byte arm alone, i.e.
/// a predicate that never collects anything. Applied: this test fails with
/// *left: `"unknown probes=1"`, right: `"reclaimable word 0x6 probes=1"`*, and
/// so does `the_sweepers_own_slot_…`'s control;
/// `a_stopped_participant_is_live_…` passes, which is the point of running the
/// two arms as separate mutants.
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
    // incarnation is 1. The word is asserted rather than the variant because
    // `ParticipantTable::reclaim` CASes against *this* word, and a verdict that
    // carried a different one would free somebody else's occupancy.
    assert_eq!(
        ask(0),
        "reclaimable word 0x6 probes=1",
        "the killed owner's slot was not collectable: its record is still LIVE \
         and the kernel has released its byte, which is #184's wedge"
    );
}

/// **The sweeper's own slot is `Live`, unconditionally — including when its
/// byte reads free.**
///
/// `F_OFD_GETLK` reports only *conflicting* locks, so an open file description
/// does not see its own. This probe holds a second description, which happens
/// to make our own byte visible again — so on the ordinary shape the guard and
/// the byte agree and the guard is unobservable. `own_slot` is therefore a
/// parameter of the seam, and this test points it at a slot whose byte the
/// kernel has *released*: the same record, the same free byte, once as
/// somebody else's and once as our own. Only the guard can separate them.
///
/// `docs/decisions/0028` piece 2's second constraint is written about exactly
/// this: *"a sweep that omits the guard is one refactor away from reclaiming
/// its own live slot"*.
///
/// Mutant: delete the `if slot == own_slot { return Reclamation::Live; }` guard
/// from `reclamation_verdict`. Applied: this test fails with *left:
/// `"reclaimable word 0x6 probes=1"`, right: `"live probes=0"`* and the message
/// below. `a_free_word_is_decided_…` fails with it too, on its own-slot line
/// (`"live probes=1"` against `"live probes=0"`); the other three pass, because
/// the byte answers for them, which is why this test has to ask about a byte
/// that reads free.
///
/// `probes=0` on the two guard answers is the second thing this test pins, and
/// it is about **order**, not about cost: the guard runs with nothing in front
/// of it, so a revision that moved it below the byte probe would report
/// `probes=1` here and fail even where the verdict happened to agree.
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
///
/// This is the `FREE`-word branch, and it is not a formality. `Open`'s joiner
/// takes its participant lock byte inside the handshake (`register_at`), and
/// then `attach_joined_at` registers **nothing** when the mode is read-only:
/// the participant table lives in the arena and a `PROT_READ` mapping cannot be
/// written. Read-only is the consumer default (D18) *and* the Python default,
/// so a slot whose byte is held and whose record reads `FREE` is the common
/// shape on a real system — `spawn_owner_server`'s `assign` closure already has
/// to special-case it (*"the table alone reports its slot empty"*).
///
/// A wrong disposition here is a **false-death verdict about a live process**,
/// which is the corrupting direction `docs/decisions/0028` exists to prevent:
/// `reclaim(slot, FREE)` is a CAS `FREE -> FREE` that *succeeds*, so a sweep
/// would report the slot collected and steps 3-5 would hand it to somebody else
/// while this joiner is still sitting in it, still holding the byte.
///
/// Two slots are asked about, because "empty" and "held by a reader" must give
/// the same answer for *different* reasons and a test that asked only about an
/// untouched slot would let a reader believe the branch is dead-slot handling.
///
/// Not a second spelling of `a_read_only_peer_holds_a_byte_without_an_arena_record`,
/// which stages the same participant to pin something else: that the *owner*
/// hands a read-only and a read-write peer different slots. It asserts nothing
/// about a reclamation verdict, and there is no verdict in it to assert.
///
/// The record is shown to be non-`LIVE` without `unstable`: the byte is probed
/// directly, and `Tree::participant_alive(2)` folds `state == LIVE` into its
/// answer, so *byte held* together with *not alive* leaves only "the word is
/// not `LIVE`".
///
/// Mutants, both applied, both reported at the **slot 2** assertion because
/// that is the one that runs first — slot 3 is never reached under either:
/// - **Delete the branch.** Slot 2 becomes `"live probes=1"`: the byte is held,
///   so the predicate answers about the byte of a participant that has no
///   record at all.
/// - **Invert it to `Reclaimable { observed }`.** Slot 2 becomes
///   `"reclaimable word 0x0 probes=0"` — a collect verdict about a running
///   process, which is the direction that corrupts.
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
///
/// `docs/decisions/0028` piece 2's third constraint says the `state` word is
/// observed **before** the byte is probed. No sequence of stable slot states
/// tells the two orders apart — on a `FREE` word both answer `unknown`, on a
/// `LIVE` word both consult the byte — so the verdict alone cannot pin it, and
/// the interleaving that *does* separate them is two adjacent statements wide,
/// which nothing here can schedule a registrant into. That interleaving is
/// `reclaim_races_register`, plan step 1's `loom` case, and it is where the
/// argument lives.
///
/// What survives into a multiprocess test is **whether the byte was read at
/// all**, which the seam reports as `probes=`. Only a predicate that reached
/// the word first can answer a `FREE` slot with no syscall, so:
///
/// - `probes=1` on a `LIVE` word — the kernel is what was asked, not `state`
///   (`docs/PHASE2.md` §5.1).
/// - `probes=0` on a `FREE` word — the word was reached first and short-circuited.
/// - `probes=0` on our own slot — the guard is in front of both reads.
///
/// Mutants, both applied:
/// - **Probe the byte before loading the word.** The middle assertion becomes
///   `"unknown probes=1"` and this test fails. It is the reversal the verifier
///   ran, and before this test existed all three earlier tests stayed green
///   under it.
/// - **Move the own-slot guard below the probe.** The last assertion becomes
///   `"live probes=1"` and this test fails. `the_sweepers_own_slot_…` fails
///   with it as well, on the same `probes=0` — measured, after an earlier
///   revision of this note predicted it would pass because the *verdict* is
///   unchanged. It is unchanged; the syscall count is what both tests catch it
///   by, and that is the point of carrying one.
///
/// **What it still does not pin:** re-reading `observed` after the probe. A
/// reload yields the same word in every state stageable here, so nothing in
/// this workspace fails when one is introduced; that half is also
/// `reclaim_races_register`'s, and step 1 has not landed.
#[test]
#[cfg(feature = "test-hooks")]
fn a_free_word_is_decided_without_asking_the_kernel() {
    let scratch = Scratch::new("verdict-order");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    let sweeper = join_as_sweeper();
    assert_eq!(sweeper.participant_slot(), 1, "the sweeper took slot 1");

    // A live read-only joiner: byte held, record FREE. The one shape that puts
    // a *held* byte behind a `FREE` word, so the `probes=0` below cannot be
    // explained by there being nothing to probe.
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

// ---------------------------------------------------------------------------
// `docs/decisions/0028` plan step 5 — `Tree::reap_participants`, and it is not
// owner-only.
//
// Public API, so these need no seam; what they need is `unstable`, because the
// thing being asserted is a participant record's raw `state` word and
// `Tree::arena_view` is the only route to it (`docs/API.md` §2.6).
// `Tree::participant_alive` cannot stand in: it folds `state == LIVE` into its
// answer, so it reads `false` both for a slot this sweep collected and for the
// stale-`LIVE` record it was supposed to collect and did not — which is the
// whole distinction.

/// The raw `state` word of `slot`, read through the unstable view.
///
/// `Acquire`, to pair with `fill_slot`'s publishing `Release` store: this reads
/// the same word `ParticipantTable::reclaim` CASes, and reading it weakly here
/// would make the assertion about a value the sweep never saw.
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
///
/// This is `docs/decisions/0028` plan step 5's stated verification, and the
/// target is the owner deliberately. #191's socket-hangup callback collects a
/// killed **joiner's** record within milliseconds, so a test that killed one
/// would be racing the owner's own fast path. Nothing hangs up on the owner:
/// it registers itself, no socket of its own closes, and `epoll` has nothing to
/// report — candidate B's hole 3, *"the owner's own slot leaks
/// unconditionally"*. Its record stays `LIVE` over a byte the kernel released,
/// for the life of the segment, and that is #184's wedge. A sweep by a
/// *survivor* is the only thing that collects it, which is what makes
/// `PHASE2.md` §6.3's "reaping must not be owner-only" a property of the code
/// rather than a sentence in a spec.
///
/// **The sweep is asserted to do nothing first.** A reaper that collected on
/// some other ground — or on none — would pass the post-kill assertion for the
/// wrong reason, so the same call is made either side of the `SIGKILL` and the
/// death is what has to change the answer.
///
/// **It asserts nothing about a new joiner, on purpose.** §3.5's takeover is
/// unwired, so once the owner is dead every fresh `open()` is correctly refused
/// `ArenaHeldButUnreachable` (`docs/PHASE2.md` §0.0 spells out the sequence:
/// the joiner wins the ownership byte, meets §3.4's split-brain check against
/// this process's participant byte, and times out). This covers survivor
/// reclamation, not rejoin. A harness that conflated the two would report a
/// §3.5 gap as a reclamation failure.
///
/// `Kid::kill` waits, and after `wait` returns the kernel has torn the owner's
/// descriptors down — so no polling and no sleep: unlike #191's asynchronous
/// callback, this sweep runs in *this* process when it is called.
///
/// Mutants, all applied to `Tree::reap_participants` and all measured:
///
/// - **Count the verdict and never call `reclaim`.** Fails at the word
///   assertion, *left: 6, right: 0* — the sweep reported a collection it did
///   not make.
/// - **Act on every verdict that is not `Unknown`**, i.e. collect `Live` too.
///   Fails at the *pre-kill* assertion, *left: 2, right: 0*: with every
///   participant running it reclaims the dead-to-nobody owner **and this
///   process's own live record**. That assertion exists for this mutant.
/// - **Do not keep the probe** (`use_ofd_liveness` stores `None`). Fails at the
///   count, *left: 0, right: 1* — a sweep with no kernel fact to consult
///   collects nothing, which is the right answer to the wrong question.
///
/// And **two that pass**, recorded because they are the constraints this test
/// cannot reach:
///
/// - **Re-read the state word after the probe** and CAS against that instead of
///   the word the verdict carried. Green. A reload yields the same word in every
///   state stageable here; the interleaving where it does not is
///   `reclaim_races_register`, `tf_tree_core`'s `loom` case, which is where that
///   half is pinned.
/// - **Delete `reclamation_verdict`'s own-slot guard**
///   (`if slot == own_slot { return Reclamation::Live; }`). Green — and green
///   for the whole `--features shm,unstable` target, 21 of 21, which is why the
///   assertion on the sweeper's own record below says what it catches rather
///   than claiming this. On `shm,test-hooks,unstable` that mutant is 27 run, 25
///   passed, 2 failed, and neither failure is here: it is
///   `the_sweepers_own_slot_is_live_even_when_the_byte_reads_free`, which points
///   the seam's `own_slot` at a byte the kernel has *released* — the only shape
///   where the guard and the byte can disagree — plus
///   `a_free_word_is_decided_without_asking_the_kernel` on the probe count.
///   Those two pin the guard; a sweep run by a live process cannot.
#[test]
#[cfg(feature = "unstable")]
fn a_survivor_reaps_the_killed_owners_slot_which_no_hangup_can() {
    let scratch = Scratch::new("reap-participants-owner");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    let sweeper = join_as_sweeper();
    assert_eq!(sweeper.participant_slot(), 1, "the sweeper took slot 1");

    // `0x6` is `live_word(1)`: the owner registered into a fresh record, so its
    // incarnation is 1. Asserted as a word rather than as a state, because it
    // is the word `reclaim` CASes against.
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
    // is held exactly as it found it. **That is what this catches — not the
    // own-slot guard, which it cannot reach.** The sweeper's byte really is
    // held and the probe's separate description reports it held, so with
    // `reclamation_verdict`'s `if slot == own_slot { return Live; }` deleted
    // the *byte* returns the same `Live` the guard would have and this test
    // stays green (measured below). What it does fail for is a sweep that
    // clears a record over a held byte — the "act on every verdict that is not
    // `Unknown`" mutant, here on the one participant still running after the
    // kill, which the pre-kill assertion catches on two slots and this one
    // catches after the table has stopped being uniform.
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
///
/// `docs/API.md` R6 and D18: read-only is the consumer default and the Python
/// default, and reclaiming is a `compare_exchange`. A `PROT_READ` mapping does
/// not fault politely on one — it delivers `SIGSEGV` — so the refusal is what
/// makes read-only a safety boundary rather than a loaded gun. It is the same
/// refusal `Tree::reap_dead` makes for claims, in the same shape: `0`, with
/// `Tree::is_writable` as the way to tell "refused" from "nothing to collect".
///
/// **The staged state is the wedge itself**, not an empty table: the owner is
/// killed first, so slot 0 holds precisely the stale-`LIVE` record the previous
/// test collects. A refusal over an arena with nothing in it would pass with
/// the guard deleted.
///
/// Mutants, both applied:
///
/// - **Delete the `!self.arena.is_writable()` refusal.** This test aborts with
///   **signal 11: SIGSEGV**, inside `ParticipantTable::reclaim`'s
///   `compare_exchange` on a `PROT_READ` page. Not an assertion failure — the
///   refusal is a safety boundary, and the mutant demonstrates which kind.
/// - **Keep a `self.participant == u32::MAX` check instead**, the shape
///   `reap_inner` carries. Green: a read-only attachment registers no arena
///   record, so the sentinel catches the same trees by a different fact. It is
///   a proxy for unwritability rather than a second guard, which is why
///   `reap_participants` carries one check and not two, and why the one it
///   carries is the one that names the hazard.
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
///
/// This is what is left of #201 after `docs/decisions/0028` plan step 0c, and it
/// is **not** what #201 was filed about. Step 0c closed the byte/record
/// *divergence* through `tf_tree::Open`, and `docs/PHASE2.md` §0.0 concluded
/// from that that "no `Tree` whose byte and record disagree can be constructed".
/// That sentence conflated the tree being **judged** with the tree doing the
/// **judging**: the probe belongs to the observer, which joins normally, and the
/// subject needs no probe and no byte at all.
///
/// **There is no divergence anywhere below.** `TreeBuilder::build_shared`
/// registers a `LIVE` participant record and takes no lock byte, because such an
/// arena has no lock file — the fd is the capability. Published through
/// `tf_tree_ipc::OwnerServer` so facade peers can join, that record is a `LIVE`
/// word over a permanently free byte: precisely the shape step 0b refused for
/// `attach_shared(ReadWrite)`, reached by a call step 0b does not cover.
///
/// **It pins the defect, not the fix.** When
/// `docs/decisions/0031-the-participant-record-with-no-byte.md` is answered this
/// test flips, and each `PIN:` message says which way. It is written as a pin
/// rather than left for later because the claim it replaces sat in `PHASE2.md`
/// §0.0 — the document that outranks every other in this project — for three
/// days, and what let it survive is that nobody executed it.
///
/// Every call here is shipped public API: `TreeBuilder::build_shared`,
/// `Tree::shared_fd`, `tf_tree_ipc::OwnerServer::bind_at`, `Tree::open`,
/// `Tree::reap_participants`. **Nothing in this workspace composes them this
/// way**, which is why running the suite never found it — the bench binaries
/// that call `build_shared` pass the fd directly and stand up no rendezvous, so
/// no probe-carrying observer exists in them to hold the wrong opinion.
///
/// **Mutant, run rather than asserted.** `Tree::reap_participants` counting the
/// verdict without calling `ParticipantTable::reclaim`:
///
/// ```text
/// assertion `left == right` failed: PIN: the sweep currently FREEs the record
/// of a process that is running
///   left: 6
///  right: 0
/// ```
///
/// So the post-sweep word is what carries the claim, not the count beside it.
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

    // Publish it with the two published `tf_tree_ipc` calls. The assign closure
    // is this test's own and hands out slots from 1, so the facade's slot
    // assigner — which *would* reclaim on the way past — never runs. What frees
    // the record below is `reap_participants` and nothing else.
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
        "PIN: a joined peer currently reads the byte-less creator as DEAD. If \
         this assertion fails, 0031 has been answered — invert the test, do not \
         delete it"
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
        "PIN: the sweep currently collects the live creator; it collected {reaped}"
    );
    assert_eq!(
        state_word(&creator, record),
        0x0,
        "PIN: the sweep currently FREEs the record of a process that is running"
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
///
/// `docs/decisions/0031` open question 1, which asked whether the false-dead
/// verdict leads on to a *claim*-level loss. Measured answer: **the claim goes,
/// but D7 does not** — the eviction is an availability failure, not corruption,
/// and that is what keeps `0031` a documented limitation rather than a
/// fix-before-release.
///
/// The mechanism is one line below the one the sibling test above pins.
/// `take_claim_lease` opens `let Some(lock) = self.lock_file.as_ref() else {
/// return Ok(None) }`, so a `build_shared` publisher holds **no lease byte
/// either**; `reap_inner` declines only on a byte it can see *held*, and an
/// unheld byte is indistinguishable from a dead holder's.
///
/// **What this asserts, and what it deliberately does not.** It pins that the
/// live publisher is evicted (the defect) *and* that `push` refuses afterwards
/// rather than interleaving (the property that bounds the severity). If `0031`
/// is answered by giving the record a byte, the first assertion flips and the
/// second must not.
///
/// Its control is `a_leased_publisher_keeps_its_edge_against_a_sweeper` below:
/// same harness, publisher joined through the rendezvous, `reap_dead` takes
/// nothing. Without it this test would also pass against a reaper that simply
/// reaps everything.
///
/// **Mutant, run rather than asserted.** `reap_inner` made to decline every
/// claim — roughly the shape of one answer to `0031` — fails this test and
/// leaves the control passing, which is the right pair:
///
/// ```text
/// assertion `left == right` failed: PIN: an ordinary peer's sweep currently
/// takes the claim of a publisher that is running.
/// ```
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
        "PIN: an ordinary peer's sweep currently takes the claim of a publisher \
         that is running. If this is now 0, 0031 has been answered — invert it"
    );

    let thief = sweeper
        .claim(
            sweeper.frame("base").unwrap(),
            sweeper.frame("map").unwrap(),
        )
        .expect("PIN: and the edge is then claimable by somebody else");

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
/// so facade peers can join it. Shared by the two tests above.
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
///
/// A live process holds A2's topology lock. `/proc` says it is dead. Before this
/// change `Tree::reparent` believed `/proc` and **stole the topology lock from a
/// live mutator**, which is the direction `docs/PHASE2.md` §6.2 forbids and the
/// direction the whole liveness bias exists to avoid. After it, the kernel's
/// answer arrives first and the inference is never consulted.
///
/// # What is staged, and what is not
///
/// The discriminating input is `alive_given`'s `Known(st) != stored` — a `/proc`
/// entry that resolves and describes **the wrong process**. This test produces
/// that input directly, by storing a stale `start_time` into the holder's
/// participant record. It does **not** stage a PID namespace: `0029`'s appendix
/// measured `unshare -U --fork --pid` producing exactly this input against the
/// shipped binaries (a namespaced participant records `pid = 1`, a host reader
/// resolves that to `systemd`, and the start times differ), and
/// `docs/decisions/0033` is where that staging lives. Nothing here should be
/// cited as namespace coverage.
///
/// Two other things are staged rather than performed, both faithfully:
///
/// * **The word.** `reparent` holds the topology word between two `fcntl`s and
///   no peer can catch it there, so the word is written directly — the same
///   technique §11.2's two `..._collects_a_record_left_reserved_by_a_killed_registrant`
///   tests use, and for the same reason.
/// * **The byte**, from a second open file description. That is precisely what a
///   mutator inside A2's critical section presents to the kernel;
///   `two_descriptions_in_one_process_still_conflict` (`tf_tree_ipc`) is the pin
///   that a second description is a faithful stand-in rather than a convenience.
///
/// # The control is the same test with one variable moved
///
/// Release the byte and nothing else changes: same live process, same stale
/// `start_time`, same word. The steal then happens, which is correct — that is
/// the state a *crashed* holder leaves — and it is what proves the refusal above
/// is the byte's doing and not an accident of the fixture.
///
/// **Mutant, run rather than asserted.** Replacing `Tree::reparent`'s
/// `take_topology_lease` call with `None`:
///
/// ```text
/// thread 'a_live_holder_that_proc_calls_dead_keeps_the_topology_lock' panicked:
/// a live holder was stolen from: Ok(())
/// ```
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

    // Make `/proc` describe the wrong process for this slot. The record still
    // names a live pid — ours — so `identity()` still resolves and the predicate
    // still runs; what changes is that the start times no longer agree.
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
    // nothing about the triple. What proves the triple now reads dead is the
    // control arm at the end — the steal that happens once the byte is gone.

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
///
/// This is the property that makes the byte an improvement rather than a second
/// thing to get stuck on: A2's lock became a kernel lock, so a holder that dies
/// for any reason has it released by the kernel with no cooperation, no timeout
/// and nothing running on its behalf. A thread cannot stage it — `SIGKILL`
/// applies to a process, and an inherited descriptor would share the parent's
/// open file description and make the contention vacuous.
///
/// **The `owner_slot: None` is the point of the first assertion, not an
/// artefact.** The child holds the byte and has published no slot into the arena
/// word, which is the state every mutator passes through between its two
/// acquires. `l_pid` is `-1` for an OFD lock (§3.3), so nothing can name the
/// holder — and the honest answer is the absence rather than `slot_of(0)`'s
/// plausible-looking `0`, which would name whichever process happens to hold
/// slot 0. That is the same wrong answer `doctor` was fixed for.
///
/// **Mutant, run rather than asserted.** Replacing `Tree::reparent`'s
/// `take_topology_lease` call with `None` — the arena word is untouched here, so
/// there is nothing else for the mutation to contend on:
///
/// ```text
/// thread 'a_killed_topology_lock_holder_releases_its_byte_to_the_kernel' panicked:
/// a live holder of the topology byte did not refuse this mutation: Ok(())
/// ```
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
///
/// `tf_tree_core::topology::TOPO_LOCK_SPIN_LIMIT`'s own documentation calls that
/// budget "a patience knob, not a timeout", sized so "a *live* holder finishing
/// an ordinary mutation is not mistaken for a dead one". Taking the byte once
/// and giving up would have handed every brief overlap back to the caller as an
/// error — a behaviour change `docs/decisions/0029` did not set out to make, and
/// one that would have surfaced as `reparent(..).unwrap()` panicking under
/// contention that is not a fault.
///
/// # Why this is a floor against a measured baseline, and not a window
///
/// The budget is spent in `fcntl` round trips, whose cost is a property of the
/// machine — pinning microseconds would make this a CPU-speed test. So the test
/// measures one contended round trip *here*, then asserts the contended
/// `reparent` cost at least eight of them. A build that attempts the byte once
/// spends one; this build spends `TOPO_BYTE_ATTEMPTS`.
///
/// A floor is also the only safe direction: noise, preemption and a loaded
/// machine can only make the observed time *longer*, so this cannot fail
/// spuriously — it can only fail if the retry loop is gone.
///
/// **Mutant, run rather than asserted.** `TOPO_BYTE_ATTEMPTS = 1`:
///
/// ```text
/// thread 'the_topology_byte_is_retried_before_contention_is_reported' panicked:
/// reparent gave up after 2.974µs, under 8 contended fcntl round trips (6.36µs):
/// the retry budget is gone
/// ```
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
    // is the same syscall on the same byte in the same state that the loop under
    // test makes.
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
///
/// This is the failure `docs/PHASE2.md` §0.0 records, measured with
/// `shm_torture` and true for the whole life of the project: kill the arena's
/// owner and lookups keep working for everyone already attached, exactly as
/// §3.5 promises — but **no new process can ever join**. A joiner wins the
/// ownership byte, meets §3.4's split-brain check against the surviving
/// participants' held bytes, backs off, and times out with
/// `ArenaHeldButUnreachable` for as long as any survivor lives. That is the
/// shape a supervised robot has every time it restarts one node.
///
/// The takeover half was deleted by #275
/// ([`0037`](../../docs/decisions/0037-a-takeover-is-not-a-second-open.md)),
/// because the declaration it rested on — *"I already hold the arena at slot
/// n"* — cannot be verified from a new file description: `F_OFD_GETLK` answers
/// *does anyone else hold this byte*, so a caller holding it on another
/// description and a live peer holding it are indistinguishable. The
/// replacement is a method on the `Session` that already holds the byte, where
/// the invariant is structural rather than checked.
///
/// **The test asserts the failure before it asserts the repair**, because a
/// test that only shows the joiner succeeding at the end would pass just as
/// well against a build where the owner never really died.
///
/// **Mutant:** make `Tree::inherit_ownership` return `Ok(Inheritance::OwnerAlive)`
/// unconditionally. Applied: `left: "true OwnerAlive", right: "true Inherited"`.
/// Note *which* half of that line moves — `owner_lost` still answers `true`,
/// because the hangup is a kernel fact and the mutation is downstream of it. The
/// test therefore fails at the inheritance and not at the trigger, which is the
/// discrimination it is for: a build that sees the death and does nothing is the
/// pre-#275 world, and it is the one this test has to reject.
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

    // A survivor, attached before the owner dies. It holds a participant byte,
    // which is precisely what turns later joiners away.
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

    // The documented failure, reproduced. Nothing is serving, and the heir's
    // held byte makes the arena look occupied to everyone outside it.
    let err = joiner()
        .open()
        .err()
        .expect("with the owner dead and nothing serving, a joiner must be turned away");
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaHeldButUnreachable { .. })
        ),
        "expected the pre-§3.5 wedge, got {err:?}"
    );

    // The repair: the survivor notices and inherits. `owner_lost` is the trigger
    // that never existed — nothing watched the client socket, so no participant
    // ever reached the takeover path even while one was implemented.
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

/// **Two survivors race for the vacant owner role, and the loser keeps its slot.**
///
/// [`0037`](../../docs/decisions/0037-a-takeover-is-not-a-second-open.md)
/// question 2. There is no arbitration protocol here and there is deliberately
/// none: both survivors call the same method, the kernel grants byte 0 to
/// exactly one uncontended `F_OFD_SETLK`, and the other is told so. What makes
/// that safe is the thing the deleted arm could not do — the lock is taken on
/// **the file description the session already holds**, so a loser's participant
/// slot cannot move, because nothing went looking for a byte in the first place.
///
/// The old arm's five unsound states were all versions of that going wrong: it
/// handed back the first *free* byte, or a byte over a free slot, or an
/// out-of-range one. This test is the assertion those failures would have
/// tripped: **both survivors report the same slot before and after**, whichever
/// one won.
///
/// **Mutant:** in `Session::take_over_ownership`, return `Ok(true)` on
/// `LockAttempt::Contended` as well — i.e. let both survivors believe they are
/// the owner. Applied: the outcome pair becomes `Inherited`/`Inherited` and the
/// `exactly one` assertion fails, which is the split-brain §3.4 exists to
/// prevent, reached from the other direction.
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

    // Exactly one inherited. Two would be split brain, which is the failure
    // this whole shape exists to make impossible.
    let inherited = [&oa, &ob].iter().filter(|o| ***o == *"Inherited").count();
    assert_eq!(
        inherited, 1,
        "expected exactly one heir, got A={oa} B={ob} (two would be split brain)"
    );

    // **The loser has two correct answers, and which one it gives is the
    // scheduler's business.** `0043` made `owner_lost` ask the kernel whether
    // byte 0 is held before answering, so a survivor that polls *after* the
    // winner took the byte reports `false OwnerAlive` and never attempts the
    // lock at all; one that polls inside the window still sees the byte free,
    // attempts, and is told `true Contended`. Both are the survivor correctly
    // observing the state it found. Before `0043` only the second was reachable
    // — and it was also permanent, which is that record's subject and
    // `a_survivor_that_did_not_inherit_stops_being_told_the_owner_is_gone`'s.
    //
    // What must hold either way: the loser is *told*, rather than handed an
    // error, and it saw a hangup exactly when it acted on one.
    let loser = if oa == "Inherited" {
        (&ob, &rb)
    } else {
        (&oa, &ra)
    };
    assert!(
        (loser.0 == "Contended" && loser.1.starts_with("true "))
            || (loser.0 == "OwnerAlive" && loser.1.starts_with("false ")),
        "the survivor that did not inherit must be told so, and its hangup \
         verdict must match what it did: A={ra} B={rb}"
    );

    // The winner did see a hangup — the trigger is a kernel fact.
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

    // And the arena is joinable again, which is the point of any of it.
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
/// happens to the other survivor **next**, and until `0043` the answer was:
/// forever. `owner_lost` polled only this process's attach socket, which points
/// at the dead owner and stays hung up for the life of the process — so the loop
/// §3.5 recommends
///
/// ```ignore
/// if tree.owner_lost() { let _ = tree.inherit_ownership()?; }
/// ```
///
/// re-attempted an `F_OFD_SETLK` on byte 0 every control cycle, to be told each
/// time that somebody else owned it. On a fleet of *N* read-write survivors that
/// is *N−1* processes doing a syscall per cycle in the loop this library exists
/// to keep quiet.
///
/// **The race is taken out of this test on purpose.** A is poked and *read*
/// before B is poked at all, so A has certainly inherited by the time B looks —
/// which makes B's answer a fact about the code rather than about the scheduler.
/// The genuine tie is `two_survivors_race_and_exactly_one_inherits`'s subject.
///
/// **Mutant, run:** revert `Tree::owner_lost` to the socket poll alone
/// (`peer_hung_up(...).unwrap_or(false)` as the whole body). The last poke still
/// passes — the socket is hung up there too, which is exactly the problem — and
/// the middle one fails on `still being told the owner is gone`. That asymmetry
/// is the point: the old code was right about the state anybody tested and wrong
/// about the one a deployment sits in.
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
        "the first survivor should have inherited from the dead owner: {taken}"
    );

    // **The defect.** The other survivor's socket is hung up and always will be.
    // The role is not vacant, and it must say so — otherwise it spins on the
    // ownership byte for the life of the process.
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

    // The chain. Kill the heir; the kernel frees byte 0 with no cooperation, and
    // the survivor must notice **by itself** — which a latched flag could not do,
    // and is why this is a live probe.
    heir.kill();
    other.poke();
    let third = other.line();
    assert!(
        third.starts_with("true "),
        "the second owner died and the survivor did not notice: {third}"
    );
    assert_eq!(
        outcome(&third),
        "Inherited",
        "the survivor should have taken the role the dead heir left: {third}"
    );
}

/// **A fleet of read-only consumers cannot rescue itself, and that is D18
/// working rather than failing.**
///
/// An owner writes the participant table on every grant, so serving needs a
/// writable mapping. A `PROT_READ` attachment — the consumer default, and the
/// only real security boundary this design has — therefore cannot be an heir.
/// `Tree::inherit_ownership` says so as data rather than as an error, because a
/// read-only consumer meeting a dead owner has not done anything wrong: it keeps
/// reading the arena it has, exactly as §3.5 promises, and some read-write
/// participant must be the one to inherit.
///
/// This is the operational shape behind `docs/RUNBOOK.md`'s owner-death remedy:
/// the recovery needs a survivor that is *both* attached and writable, and a
/// deployment of nothing but `tf_tree top --attach` and read-only nodes has none.
///
/// **Mutant:** drop the `is_writable` guard from `inherit_ownership`. Applied:
/// the read-only tree reports `Inherited` instead of `ReadOnly` — it takes the
/// ownership byte it cannot serve behind, which is the state that makes an arena
/// unjoinable and the exact failure the guard exists to prevent.
#[test]
fn a_read_only_survivor_reports_that_it_cannot_inherit() {
    use tf_tree::{AttachMode, Inheritance};
    use tf_tree_ipc::CreatePolicy;

    let dir = Scratch::new("inherit-read-only");

    let mut owner = Kid::spawn(&dir.0, &["own"]);
    assert!(owner.line().starts_with("owning"));

    // This process is the read-only survivor.
    let mut ro = tf_tree::Open::new()
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
    assert!(ro.owner_lost(), "a read-only attachment missed the hangup");
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
///
/// D15 is that no mutation protocol lands without a named crash point and a walk
/// through the matrix, and §3.5's acquisition is a mutation protocol: between
/// taking byte 0 and binding the socket, a process is the owner and is not
/// serving — which is exactly the state that makes an arena unjoinable, and the
/// state a survivor's inheritance exists to end.
///
/// §11.3's repair claim for this row is *"ownership released; another
/// participant takes over; joiners retry"*, and what makes it true rather than
/// hopeful is that the byte is an OFD lock: the kernel releases it at process
/// death with no cooperation from the corpse, and `inherit_ownership` never
/// registers anything a dead heir would have to clean up.
///
/// So: two survivors, the first killed *inside* the window by
/// `TF_TREE_CRASH_AT`, and the assertion that the second still inherits and the
/// arena is joinable afterwards. The crash is real — `abort()`, not a `panic!`,
/// so no destructor runs and nothing gives the byte back politely.
///
/// **Mutant:** none is offered, because the honest one is the crash point's
/// *placement*, and moving it after `spawn_owner_server` makes this test pass
/// for a different reason (the heir dies owning *and* serving, and the socket's
/// closure is what the survivor then sees). What this pins is the row's repair,
/// not the placement; the placement is argued at the call site.
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
