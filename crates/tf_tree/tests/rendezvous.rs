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
        let exe = env!("CARGO_BIN_EXE_rendezvous_child");
        let child = Command::new(exe)
            .args(args)
            .env("TF_TREE_RUNTIME_DIR", dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn rendezvous_child");
        Kid(child, None)
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
    /// Nudge a child that is waiting on stdin.
    fn poke(&mut self) {
        use std::io::Write;
        if let Some(mut stdin) = self.0.stdin.take() {
            let _ = writeln!(stdin, "go");
        }
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
/// **Mutant: build the predicate on `Tree::frame`** ⇒ the read-only consumer
/// gets `FrameError::ReadOnly` immediately and never resolves.
/// **Mutant: drop the memoization or the deadline** ⇒ caught by the elapsed
/// bounds.
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

    REAPER.set(reaper).ok().expect("set reaper");
    tf_tree::CLAIM_WINDOW_HOOK
        .set(reap_from_inside_the_window as fn())
        .ok()
        .expect("install hook");

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
