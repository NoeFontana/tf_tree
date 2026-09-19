//! The rendezvous against real processes.
//!
//! Tests spawn `src/bin/ipc_child.rs`, which opens the lock file **by path**
//! (OFD locks belong to an open file description, so an inherited fd would
//! conflict with nobody) and parks holding a lock until killed.
//!
//! `docs/PHASE2.md` §11.2 scenario 9 (split-brain) is the important one.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tf_tree_ipc::{
    ArenaName, CreatePolicy, EnvLookup, EnvVar, IpcError, LockAttempt, LockFile, NoServer, Open,
    OpenOutcome, Rendezvous, RuntimeDir,
};

/// A scratch runtime directory, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("tf_tree_ipc_mp-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }

    fn rendezvous(&self) -> Rendezvous {
        let rd =
            RuntimeDir::resolve_with(&Fixed(self.0.clone()), tf_tree_ipc::current_uid()).unwrap();
        let rv = Rendezvous::new(rd, 0, ArenaName::new("default", EnvVar::Name).unwrap());
        rv.ensure_dir().unwrap();
        rv
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An environment with only `TF_TREE_RUNTIME_DIR` set.
struct Fixed(PathBuf);

impl EnvLookup for Fixed {
    fn var(&self, key: &str) -> Option<std::ffi::OsString> {
        (key == "TF_TREE_RUNTIME_DIR").then(|| self.0.clone().into_os_string())
    }
}

/// A spawned helper, killed on drop.
struct Kid(Child, Option<BufReader<std::process::ChildStdout>>);

impl Kid {
    fn spawn(args: &[&str]) -> Kid {
        let exe = env!("CARGO_BIN_EXE_tf_tree_ipc_child");
        let child = Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the ipc child helper");
        Kid(child, None)
    }

    /// The child's next line; it flushes before parking, so this returning proves
    /// the lock is taken. The reader is kept across calls so buffered lines
    /// are not dropped.
    fn line(&mut self) -> String {
        let reader = self
            .1
            .get_or_insert_with(|| BufReader::new(self.0.stdout.take().expect("piped stdout")));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read child line");
        line.trim_end().to_string()
    }

    /// `SIGKILL`, then reap; after `wait` the kernel has released its locks.
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

/// Two (here: eight) processes contend for the ownership byte. Exactly one wins.

#[test]
fn exactly_one_process_wins_the_ownership_byte() {
    let scratch = Scratch::new("ownership");
    let rv = scratch.rendezvous();
    let lock_path = rv.lock_path().to_str().unwrap();

    let mut kids: Vec<Kid> = (0..8)
        .map(|_| Kid::spawn(&["hold-ownership", lock_path, "1500"]))
        .collect();
    let lines: Vec<String> = kids.iter_mut().map(Kid::line).collect();

    let won = lines.iter().filter(|l| *l == "won").count();
    let lost = lines.iter().filter(|l| *l == "lost").count();
    assert_eq!(won, 1, "exactly one process may own the arena: {lines:?}");
    assert_eq!(lost, 7, "everyone else must be told no: {lines:?}");
}

/// `F_OFD_GETLK` on a held byte reports `l_pid = -1`.
///
/// Pins `docs/PHASE2.md` §3.3 on this kernel; it is why identity records exist.
#[test]
fn getlk_on_a_held_byte_cannot_name_the_holder() {
    let scratch = Scratch::new("getlk-pid");
    let rv = scratch.rendezvous();
    let lock_path = rv.lock_path().to_str().unwrap();

    let mut holder = Kid::spawn(&["hold-ownership", lock_path]);
    assert_eq!(holder.line(), "won");

    let observer = LockFile::open(rv.lock_path()).unwrap();
    let probe = observer.probe_ownership().unwrap();
    assert!(probe.held, "the child holds byte 0");
    assert_eq!(
        probe.holder_pid, -1,
        "an OFD lock belongs to a file description, so GETLK cannot name a pid"
    );
    assert_ne!(
        probe.holder_pid,
        holder.0.id() as i32,
        "if this ever names the child, §3.3's table is wrong on this kernel"
    );
}

/// A `SIGKILL`ed holder's lock is released by the kernel, immediately.
///
/// The kernel frees the byte with no cooperation from the child.
#[test]
fn a_sigkilled_holder_releases_its_locks() {
    let scratch = Scratch::new("sigkill");
    let rv = scratch.rendezvous();
    let lock_path = rv.lock_path().to_str().unwrap();

    let mut holder = Kid::spawn(&["hold-participant", lock_path, "4"]);
    assert_eq!(holder.line(), "held 4");

    let observer = LockFile::open(rv.lock_path()).unwrap();
    assert!(observer.probe_participant(4).unwrap().held);
    assert_eq!(observer.held_participants().unwrap(), 1 << 4);
    let id = observer.read_identity(4).unwrap().expect("identity record");
    assert_eq!(id.pid, holder.0.id());
    assert!(id.start_time > 0);

    holder.kill();

    assert!(
        !observer.probe_participant(4).unwrap().held,
        "the kernel must release a dead process's lock without its cooperation"
    );
    assert_eq!(observer.held_participants().unwrap(), 0);
    assert_eq!(
        observer.try_take_participant(4).unwrap(),
        LockAttempt::Acquired
    );
    // The record outlives the process: advisory (§5.1), never a liveness signal.
    assert_eq!(
        observer.read_identity(4).unwrap().map(|i| i.pid),
        Some(id.pid)
    );
}

/// §11.2 scenario 9, split-brain: with a live participant and nothing serving,
/// `open()` must fail naming the holding slot, never create a second arena.
/// `$TF_TREE_SPLIT_BRAIN_ITERS` sets the iteration count (default 128).
#[test]
fn a_live_participant_prevents_a_second_arena() {
    let scratch = Scratch::new("split-brain");
    let rv = scratch.rendezvous();
    let lock_path = rv.lock_path().to_str().unwrap();

    let mut survivor = Kid::spawn(&["hold-participant", lock_path, "3"]);
    assert_eq!(survivor.line(), "held 3");

    let iterations: usize = std::env::var("TF_TREE_SPLIT_BRAIN_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128);
    for i in 0..iterations {
        let err = Open::new(rv.clone())
            .timeout(Duration::from_millis(15))
            .open(&mut NoServer)
            .expect_err("open() created a second arena while one was alive");
        match err {
            IpcError::ArenaHeldButUnreachable {
                holder_slots,
                first_slot,
                first_pid,
                ownership_held,
            } => {
                assert_eq!(holder_slots, 1 << 3, "iteration {i}");
                assert_eq!(first_slot, Some(3), "iteration {i}");
                assert_eq!(
                    first_pid,
                    survivor.0.id(),
                    "the error must name the process an operator has to kill"
                );
                // Only a participant byte is held: the error must not read as the byte-0 wedge.
                assert!(
                    !ownership_held,
                    "iteration {i}: nothing holds the ownership byte in this scenario"
                );
            }
            other => panic!("iteration {i}: expected ArenaHeldButUnreachable, got {other}"),
        }
        // Every refusal must release byte 0 again.
        let heir = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(
            heir.try_take_ownership().unwrap(),
            LockAttempt::Acquired,
            "iteration {i}: a yielding opener must not keep the ownership byte"
        );
    }

    // Positive control: open() does create once nothing is alive.
    survivor.kill();
    let session = Open::new(rv.clone())
        .timeout(Duration::from_millis(500))
        .open(&mut NoServer)
        .expect("nothing alive: open() must create");
    assert_eq!(session.outcome(), OpenOutcome::Created);
}

/// A child that created the arena blocks a second creator; once it dies a fresh
/// `open()` creates (`docs/PHASE2.md` §11.3 `open.after_create_before_bind`).
#[test]
fn a_child_that_created_the_arena_blocks_a_second_creator() {
    let scratch = Scratch::new("child-open");
    let rv = scratch.rendezvous();
    let dir = scratch.0.to_str().unwrap();

    let mut creator = Kid::spawn(&["open", dir, "500"]);
    let line = creator.line();
    assert_eq!(line, "created 0", "the first process must create: {line}");

    let err = Open::new(rv.clone())
        .timeout(Duration::from_millis(50))
        .open(&mut NoServer)
        .expect_err("a second process must not create a second arena");
    assert!(
        matches!(
            err,
            IpcError::ArenaHeldButUnreachable {
                first_slot: Some(0),
                ..
            }
        ),
        "unexpected error: {err}"
    );

    creator.kill();
    let session = Open::new(rv)
        .timeout(Duration::from_millis(500))
        .open(&mut NoServer)
        .expect("with the creator dead, nothing is alive");
    assert_eq!(session.outcome(), OpenOutcome::Created);
    assert_eq!(session.slot(), 0, "the dead creator's slot is reusable");
}

/// `CreatePolicy::Never` fails fast and leaves no lock behind.
#[test]
fn a_consumer_that_refuses_to_create_fails_fast() {
    let scratch = Scratch::new("never");
    let rv = scratch.rendezvous();

    let started = std::time::Instant::now();
    let err = Open::new(rv.clone())
        .create(CreatePolicy::Never)
        .timeout(Duration::from_secs(60))
        .open(&mut NoServer)
        .expect_err("nothing to join");
    assert_eq!(err, IpcError::ArenaAbsent);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "fail fast means fail fast, not wait out the timeout"
    );

    let after = LockFile::open(rv.lock_path()).unwrap();
    assert_eq!(after.try_take_ownership().unwrap(), LockAttempt::Acquired);
}

/// Domain and runtime-directory isolation (§11.2 scenario 11): two arenas that
/// differ in either dimension never observe each other.
#[test]
fn different_domains_and_directories_never_meet() {
    let a = Scratch::new("iso-a");
    let b = Scratch::new("iso-b");
    let rd_a = RuntimeDir::resolve_with(&Fixed(a.0.clone()), tf_tree_ipc::current_uid()).unwrap();
    let rd_b = RuntimeDir::resolve_with(&Fixed(b.0.clone()), tf_tree_ipc::current_uid()).unwrap();
    let name = ArenaName::new("default", EnvVar::Name).unwrap();

    let cases = [
        Rendezvous::new(rd_a.clone(), 0, name),
        Rendezvous::new(rd_a, 1, name),
        Rendezvous::new(rd_b, 0, name),
    ];
    let mut sessions = Vec::new();
    for rv in cases {
        rv.ensure_dir().unwrap();
        let s = Open::new(rv).open(&mut NoServer).unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Created);
        assert_eq!(s.slot(), 0);
        sessions.push(s);
    }
    assert_eq!(sessions.len(), 3);
}

/// The on-disk paths are the ones §3.1 specifies.
#[test]
fn the_paths_on_disk_are_the_specified_ones() {
    let scratch = Scratch::new("paths");
    let rv = scratch.rendezvous();
    let _s = Open::new(rv.clone()).open(&mut NoServer).unwrap();
    let expected: PathBuf = scratch.0.join("0").join("default.lock");
    assert_eq!(rv.lock_path(), expected.as_path());
    assert!(Path::new(&expected).is_file());
    assert_eq!(rv.sock_path(), scratch.0.join("0").join("default.sock"));
}

// ---------------------------------------------------------------------------
// §3.7 attach handshake
// ---------------------------------------------------------------------------

/// A request that matches what the helper's `serve` mode publishes.
fn good_request() -> tf_tree_ipc::HelloRequest {
    tf_tree_ipc::HelloRequest {
        format_version: 2,
        layout_hash: 0xDEAD_BEEF,
        mode: tf_tree_ipc::AccessMode::ReadOnly,
        client_pid: std::process::id(),
        client_start_time: tf_tree_ipc::self_start_time().unwrap_or(0),
        client_boot_id: tf_tree_ipc::boot_id().unwrap_or([0; 16]),
        client_name: [0; 32],
    }
}

/// Start a server child and wait until it is actually listening.
fn serve(sock: &Path, size: u64) -> Kid {
    let mut kid = Kid::spawn(&["serve", sock.to_str().unwrap(), &size.to_string()]);
    assert_eq!(kid.line(), "serving");
    kid
}

/// A real descriptor crosses the process boundary; asserted on
/// `fstat(received_fd)`, not on the response bytes.
#[test]
fn a_segment_fd_crosses_the_process_boundary() {
    let scratch = Scratch::new("scm-rights");
    let sock = scratch.0.join("a.sock");
    let _server = serve(&sock, 8192);

    let attached = tf_tree_ipc::attach(&sock, &good_request(), Duration::from_secs(5))
        .expect("attach should succeed");

    assert_eq!(attached.response.status, tf_tree_ipc::HelloStatus::Ok);
    assert_eq!(attached.response.arena_size, 8192);
    assert_eq!(attached.response.instance_uuid, [0x5A; 16]);

    let st = rustix::fs::fstat(&attached.segment).expect("fstat the received fd");
    assert_eq!(
        st.st_size, 8192,
        "the fd did not refer to the served segment"
    );
}

/// Two clients get two slots, and both fds are independently valid.
#[test]
fn each_client_gets_its_own_slot() {
    let scratch = Scratch::new("two-clients");
    let sock = scratch.0.join("a.sock");
    let _server = serve(&sock, 4096);

    let a = tf_tree_ipc::attach(&sock, &good_request(), Duration::from_secs(5)).unwrap();
    let b = tf_tree_ipc::attach(&sock, &good_request(), Duration::from_secs(5)).unwrap();

    assert_ne!(a.response.participant_slot, b.response.participant_slot);
    assert_eq!(rustix::fs::fstat(&a.segment).unwrap().st_size, 4096);
    assert_eq!(rustix::fs::fstat(&b.segment).unwrap().st_size, 4096);
}

/// A `LayoutMismatch` rejection names the owner's hash and carries no fd (§3.7).
#[test]
fn a_layout_mismatch_names_the_owners_hash_and_sends_no_fd() {
    let scratch = Scratch::new("layout-mismatch");
    let sock = scratch.0.join("a.sock");
    let _server = serve(&sock, 4096);

    let mut req = good_request();
    req.layout_hash = 0x0BAD_0BAD;

    match tf_tree_ipc::attach(&sock, &req, Duration::from_secs(5)) {
        Err(IpcError::HandshakeRejected {
            status,
            owner_layout_hash,
            ..
        }) => {
            assert_eq!(status, tf_tree_ipc::HelloStatus::LayoutMismatch);
            assert_eq!(owner_layout_hash, 0xDEAD_BEEF, "must name the owner's hash");
        }
        Err(IpcError::RejectionCarriedFd { .. }) => {
            panic!("the owner sent a segment fd with a rejection")
        }
        other => panic!("expected a LayoutMismatch rejection, got {other:?}"),
    }
}

/// A version disagreement is reported as such, not as a layout complaint.
#[test]
fn a_version_mismatch_outranks_a_layout_mismatch() {
    let scratch = Scratch::new("version-mismatch");
    let sock = scratch.0.join("a.sock");
    let _server = serve(&sock, 4096);

    // Both wrong: version is reported first.
    let mut req = good_request();
    req.format_version = 99;
    req.layout_hash = 0x0BAD_0BAD;

    match tf_tree_ipc::attach(&sock, &req, Duration::from_secs(5)) {
        Err(IpcError::HandshakeRejected {
            status,
            owner_format_version,
            ..
        }) => {
            assert_eq!(status, tf_tree_ipc::HelloStatus::VersionMismatch);
            assert_eq!(owner_format_version, 2);
        }
        other => panic!("expected a VersionMismatch rejection, got {other:?}"),
    }
}

/// D17: a `SIGKILL`ed participant's socket close is visible to the owner.
#[test]
fn the_owner_sees_a_hangup_when_a_participant_is_killed() {
    let scratch = Scratch::new("hangup");
    let sock = scratch.0.join("a.sock");
    let mut server = serve(&sock, 4096);

    let mut client = Kid::spawn(&["attach", sock.to_str().unwrap()]);
    let attached = client.line();
    assert!(attached.starts_with("attached 0 4096"), "got {attached}");

    client.kill();

    assert_eq!(
        server.line(),
        "hangup 0",
        "the owner did not observe the dead participant's socket close"
    );
}

/// An absent server is `ServerUnreachable`, not a rejection (§3.9).
#[test]
fn an_absent_server_is_not_a_rejection() {
    let scratch = Scratch::new("absent");
    let sock = scratch.0.join("nobody.sock");

    match tf_tree_ipc::attach(&sock, &good_request(), Duration::from_millis(200)) {
        Err(IpcError::ServerUnreachable { .. }) => {}
        other => panic!("expected ServerUnreachable, got {other:?}"),
    }
}

/// A socket path longer than `sun_path` fails at construction, naming the limit.
#[test]
fn an_overlong_socket_path_is_refused_with_its_length() {
    let long = PathBuf::from(format!("/tmp/{}", "x".repeat(200)));
    match tf_tree_ipc::attach(&long, &good_request(), Duration::from_millis(200)) {
        Err(IpcError::SocketPathTooLong { len, limit }) => {
            assert_eq!(limit, 108);
            assert!(len > limit);
        }
        other => panic!("expected SocketPathTooLong, got {other:?}"),
    }
}

/// A client that connects and never speaks must not wedge the owner: a second
/// client still attaches. Without the server's `SO_RCVTIMEO` this hangs until
/// nextest's `terminate-after`.
#[test]
fn a_silent_client_cannot_wedge_the_owner() {
    let scratch = Scratch::new("silent-client");
    let sock = scratch.0.join("a.sock");
    let _server = serve(&sock, 4096);

    // Connect, send nothing.
    let addr = rustix::net::SocketAddrUnix::new(&sock).unwrap();
    let mute = rustix::net::socket_with(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::SEQPACKET,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::connect(&mute, &addr).unwrap();

    let attached = tf_tree_ipc::attach(&sock, &good_request(), Duration::from_secs(10))
        .expect("a well-behaved client must still be served");
    assert_eq!(attached.response.arena_size, 4096);

    drop(mute);
}

/// `Joined` through a real `SocketProbe`.
#[test]
fn a_second_process_joins_a_served_arena() {
    let scratch = Scratch::new("real-join");
    let rd =
        RuntimeDir::resolve_with(&Fixed(scratch.0.clone()), tf_tree_ipc::current_uid()).unwrap();
    let rv = Rendezvous::new(rd, 0, ArenaName::new("default", EnvVar::Name).unwrap());

    let mut creator = Open::new(rv.clone()).open(&mut NoServer).unwrap();
    assert_eq!(creator.outcome(), OpenOutcome::Created);
    let _server = serve(rv.sock_path(), 4096);

    let mut probe = tf_tree_ipc::SocketProbe::new(good_request(), Duration::from_secs(5));
    let mut joiner = Open::new(rv)
        .create(CreatePolicy::Never)
        .open(&mut probe)
        .expect("a served arena must be joinable");

    assert_eq!(joiner.outcome(), OpenOutcome::Joined);
    assert!(
        !joiner.is_owner(),
        "a joiner must not hold the ownership byte"
    );

    let attached = joiner
        .take_attached()
        .expect("Joined carries an attachment");
    assert_eq!(attached.response.participant_slot, joiner.slot());
    assert_eq!(
        rustix::fs::fstat(&attached.segment).unwrap().st_size,
        4096,
        "the joiner did not receive the served segment"
    );

    let _ = creator.release_ownership();
}

/// A rejection is terminal: `open()` returns it rather than retrying to the deadline.
#[test]
fn a_rejection_is_terminal_and_does_not_burn_the_deadline() {
    let scratch = Scratch::new("terminal-reject");
    let rd =
        RuntimeDir::resolve_with(&Fixed(scratch.0.clone()), tf_tree_ipc::current_uid()).unwrap();
    let rv = Rendezvous::new(rd, 0, ArenaName::new("default", EnvVar::Name).unwrap());

    let _creator = Open::new(rv.clone()).open(&mut NoServer).unwrap();
    let _server = serve(rv.sock_path(), 4096);

    let mut bad = good_request();
    bad.layout_hash = 0x0BAD_0BAD;
    let mut probe = tf_tree_ipc::SocketProbe::new(bad, Duration::from_secs(1));

    let started = std::time::Instant::now();
    let err = Open::new(rv)
        .timeout(Duration::from_secs(30))
        .open(&mut probe)
        .expect_err("a layout mismatch must not be joinable");

    match err {
        IpcError::HandshakeRejected {
            status,
            owner_layout_hash,
            ..
        } => {
            assert_eq!(status, tf_tree_ipc::HelloStatus::LayoutMismatch);
            assert_eq!(owner_layout_hash, 0xDEAD_BEEF);
        }
        other => panic!("expected the rejection to surface intact, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the rejection was retried instead of returned: took {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// §6.1 claim leases
// ---------------------------------------------------------------------------

/// A killed claim holder's lease is released at once (the predicate §6.3's
/// reaper runs on); no sleep is needed after `wait()`.
#[test]
fn a_killed_holder_releases_its_claim_lease_at_once() {
    let scratch = Scratch::new("claim-lease");
    let lock_path = scratch.0.join("claims.lock");
    let observer = LockFile::open(&lock_path).unwrap();

    let mut kid = Kid::spawn(&["hold-claim", lock_path.to_str().unwrap(), "7"]);
    assert_eq!(kid.line(), "held 7");

    assert!(
        observer.probe_claim(7).unwrap().held,
        "the child's lease is not visible to another description"
    );
    // Byte-range locks: a different edge is independent.
    assert!(!observer.probe_claim(8).unwrap().held);

    kid.kill();
    assert!(
        !observer.probe_claim(7).unwrap().held,
        "the lease survived the holder, so a dead writer would leak its edge"
    );
}

/// Two processes cannot hold one edge's lease.
#[test]
fn only_one_process_holds_an_edge_lease() {
    let scratch = Scratch::new("claim-exclusive");
    let lock_path = scratch.0.join("claims.lock");

    let mut first = Kid::spawn(&["hold-claim", lock_path.to_str().unwrap(), "3"]);
    assert_eq!(first.line(), "held 3");

    let mut second = Kid::spawn(&["hold-claim", lock_path.to_str().unwrap(), "3"]);
    assert_eq!(second.line(), "lost", "two processes took one edge's lease");
}

/// An edge id past the reserved region is refused, not wrapped.
#[test]
fn an_edge_beyond_the_reserved_region_is_refused() {
    let scratch = Scratch::new("claim-range");
    let lock = LockFile::open(&scratch.0.join("claims.lock")).unwrap();
    match lock.probe_claim(u32::MAX) {
        Err(IpcError::ClaimOutOfRange { edge, .. }) => assert_eq!(edge, u32::MAX),
        other => panic!("expected ClaimOutOfRange, got {other:?}"),
    }
}

/// A claim byte and a participant byte with the same index must not collide
/// (`CLAIM_BASE`).
#[test]
fn claim_bytes_and_participant_bytes_do_not_overlap() {
    let scratch = Scratch::new("claim-vs-participant");
    let lock_path = scratch.0.join("both.lock");
    let observer = LockFile::open(&lock_path).unwrap();

    let mut kid = Kid::spawn(&["hold-participant", lock_path.to_str().unwrap(), "3"]);
    assert_eq!(kid.line(), "held 3");
    assert!(observer.probe_participant(3).unwrap().held);

    assert!(
        !observer.probe_claim(3).unwrap().held,
        "edge 3's claim byte aliases participant slot 3's byte"
    );
    assert_eq!(
        observer.try_take_claim(3).unwrap(),
        LockAttempt::Acquired,
        "edge 3's lease could not be taken while participant 3 was registered"
    );
    observer.release_claim(3).unwrap();
}

/// A creator takes participant slot 0 or does not create (#201). A racing lock
/// description toggles byte 0; every create must still be slot 0.
#[test]
fn a_creator_takes_slot_zero_or_does_not_create() {
    let scratch = Scratch::new("creator_slot_zero");
    let rv = scratch.rendezvous();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let racer_stop = std::sync::Arc::clone(&stop);
    let racer_path = rv.lock_path().to_path_buf();
    drop(LockFile::open(rv.lock_path()).unwrap());
    let racer = std::thread::spawn(move || {
        let lock = LockFile::open(&racer_path).unwrap();
        while !racer_stop.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = lock.try_take_participant(0);
            std::hint::spin_loop();
            let _ = lock.release_participant(0);
        }
    });

    let mut created = 0u32;
    let mut yielded = 0u32;
    for i in 0..400 {
        match Open::new(rv.clone())
            .create(CreatePolicy::IfAbsent)
            .timeout(Duration::from_millis(20))
            .open(&mut NoServer)
        {
            Ok(session) => {
                assert_eq!(
                    session.outcome(),
                    OpenOutcome::Created,
                    "iteration {i}: nothing serves, so every success is a create"
                );
                assert_eq!(
                    session.slot(),
                    0,
                    "iteration {i}: a creator took byte {} while the arena would \
                     register record 0 — #201's divergence",
                    session.slot()
                );
                created += 1;
            }
            // Losing the byte-0 race is a yield, not a failure.
            Err(IpcError::ArenaHeldButUnreachable { .. }) => yielded += 1,
            Err(other) => panic!("iteration {i}: unexpected {other:?}"),
        }
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    racer.join().unwrap();

    // Positive control: some create succeeded.
    assert!(
        created > 0,
        "no create succeeded in 400 attempts ({yielded} yields) — the test proved nothing"
    );
}
