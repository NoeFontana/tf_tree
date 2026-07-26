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
