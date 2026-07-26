//! The rendezvous against real processes.
//!
//! Everything this crate claims is a claim about what the **kernel** does across
//! a process boundary: that exactly one of two contenders gets a byte, that a
//! `SIGKILL`ed holder's lock is released immediately and without its
//! cooperation, and that a live participant's byte is visible to a process that
//! knows nothing about it. None of that is testable with threads — a thread
//! cannot be `SIGKILL`ed out from under its locks, and (unlike classic POSIX
//! locks) the interesting failure is not one a single process can stage.
//!
//! So these tests spawn `ipc_child`, which opens the lock file **by path** and
//! parks holding a lock until it is killed. Opening by path rather than
//! inheriting a descriptor is load-bearing: OFD locks belong to an open file
//! description, so a child holding the parent's inherited fd would conflict with
//! nobody and every assertion here would pass vacuously.
//!
//! `docs/PHASE2.md` §11.2 scenario 9 — split-brain — is the important one, and
//! per Appendix A it exists before the code it tests is finished.
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

/// An environment with only `TF_TREE_RUNTIME_DIR` set, so a test never depends
/// on the runner's own environment.
struct Fixed(PathBuf);

impl EnvLookup for Fixed {
    fn var(&self, key: &str) -> Option<std::ffi::OsString> {
        (key == "TF_TREE_RUNTIME_DIR").then(|| self.0.clone().into_os_string())
    }
}

/// A spawned `ipc_child`, killed on drop so a failing assertion cannot leave a
/// process holding a lock in `/tmp` forever.
struct Kid(Child, Option<BufReader<std::process::ChildStdout>>);

impl Kid {
    fn spawn(args: &[&str]) -> Kid {
        let exe = env!("CARGO_BIN_EXE_ipc_child");
        let child = Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn ipc_child");
        Kid(child, None)
    }

    /// The child's next line. The child flushes before it parks, so this
    /// returning is proof the lock has actually been taken — no sleeps, no
    /// polling, no "probably by now".
    ///
    /// The reader is kept across calls. Building a fresh `BufReader` each time
    /// discards whatever it buffered beyond the newline, which is invisible
    /// while a child emits exactly one line and silently drops the second when
    /// one emits two.
    fn line(&mut self) -> String {
        let reader = self
            .1
            .get_or_insert_with(|| BufReader::new(self.0.stdout.take().expect("piped stdout")));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read child line");
        line.trim_end().to_string()
    }

    /// `SIGKILL`, then reap. After `wait` returns, the kernel has torn down the
    /// process's descriptors, so its locks are gone — with no cooperation from
    /// the child, which is the entire point.
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
///
/// Each child retries for five seconds before reporting "lost", so the result
/// does not depend on scheduling order: the loser is the one that could not get
/// the byte while somebody held it, not the one that started second.
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
/// `docs/PHASE2.md` §3.3 states this as verified behaviour on Linux 6.18, and
/// the whole reason identity records exist as separate `pwrite` data is that it
/// is true. Verify it on *this* kernel rather than trusting the table: if some
/// kernel ever did name the holder, the identity records would be redundant, and
/// if it named the wrong one they would be actively misleading.
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
/// This is the property that replaces every heartbeat, timeout and reaping
/// heuristic in the previous draft of §6. The child is killed with a signal it
/// cannot handle, runs no destructor, and unlinks nothing — and the byte is free
/// the moment it is reaped.
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
    // The identity record names who it was, which GETLK cannot.
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
    // The record outlives the process: it is advisory (§5.1), and this is
    // exactly why it must never be consulted for liveness.
    assert_eq!(
        observer.read_identity(4).unwrap().map(|i| i.pid),
        Some(id.pid)
    );
}

/// **§11.2 scenario 9 — split-brain.** The single most important race in the
/// phase.
///
/// A participant is alive (its lock byte is held) and nothing is serving: the
/// state immediately after an owner dies, before any survivor has noticed the
/// `HUP`. A fresh `open()` must **not** create a second arena. It must fail,
/// naming the slot that is holding things up.
///
/// Run in a loop, because the failure this prevents is a race. The spec asks for
/// a thousand consecutive runs; that is what `$TF_TREE_SPLIT_BRAIN_ITERS` is
/// for, and the default of 128 keeps `just test` quick without letting the loop
/// disappear. Every iteration re-opens the lock file, so nothing carries over
/// except the child's held byte.
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
            } => {
                assert_eq!(holder_slots, 1 << 3, "iteration {i}");
                assert_eq!(first_slot, Some(3), "iteration {i}");
                assert_eq!(
                    first_pid,
                    survivor.0.id(),
                    "the error must name the process an operator has to kill"
                );
            }
            other => panic!("iteration {i}: expected ArenaHeldButUnreachable, got {other}"),
        }
        // Every refusal must have released byte 0 again, or the survivor could
        // never take over and the refusal would become permanent.
        let heir = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(
            heir.try_take_ownership().unwrap(),
            LockAttempt::Acquired,
            "iteration {i}: a yielding opener must not keep the ownership byte"
        );
    }

    // The positive control. Without it this test would still pass if `open()`
    // simply never created anything.
    survivor.kill();
    let session = Open::new(rv.clone())
        .timeout(Duration::from_millis(500))
        .open(&mut NoServer)
        .expect("nothing alive: open() must create");
    assert_eq!(session.outcome(), OpenOutcome::Created);
}

/// The same race from the child's side: the child runs the real `open()`, wins,
/// and holds both bytes; the parent's `open()` must refuse rather than create.
///
/// This is the shape of `docs/PHASE2.md` §11.3's `open.after_create_before_bind`
/// crash point — an arena exists with nothing serving it — and the recovery it
/// requires: once the creator is gone and no participant byte is held, a fresh
/// `open()` is free to create, and the orphan segment dies with its last
/// mapping.
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

/// `CreatePolicy::Never` fails fast rather than waiting out the timeout, and
/// leaves no lock behind for the next process.
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
        // Every one of them creates: none of them can see the others.
        assert_eq!(s.outcome(), OpenOutcome::Created);
        assert_eq!(s.slot(), 0);
        sessions.push(s);
    }
    assert_eq!(sessions.len(), 3);
}

/// The lock file path is the one §3.1 specifies, verified against the filesystem
/// rather than against the code that built it.
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

/// A request that matches what `ipc_child serve` publishes.
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

/// **The whole point of §3.7: a real descriptor crosses a process boundary.**
///
/// The assertion is on `fstat(received_fd).st_size`, not on the response — the
/// response is just bytes this process could have fabricated, whereas a size the
/// kernel reports for a descriptor is only obtainable if a descriptor actually
/// arrived. Omit the `ScmRights` push on the server and the client gets
/// `NoFdReceived` rather than a plausible-looking success.
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

    // The kernel's view of the received fd. This is the evidence.
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

/// A rejection names both sides and carries no fd.
///
/// §3.7 singles out `LayoutMismatch` because its raw symptom is "attach fails on
/// a machine where everything looks fine". The error must therefore carry the
/// *owner's* hash — the client already knows its own — or the message is useless.
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
        // Not merely "some error": a rejection that carried a segment is its
        // own named failure, and collapsing the two would let the server hand
        // over the arena to a peer it just refused while this test still passed.
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

    // Both wrong. A peer that lays its records out differently will also hash
    // differently, so reporting the layout first would send the operator after
    // the wrong problem.
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

/// **D17: the socket is the liveness signal.**
///
/// A participant that is `SIGKILL`ed — no unwinding, no destructor, no message —
/// must be visible to the owner immediately, because the kernel closes its fd.
/// This is what makes reaping prompt and timeout-free, and it is the property
/// `docs/PROJECT.md` §5 D17 forbids replacing with a heartbeat.
#[test]
fn the_owner_sees_a_hangup_when_a_participant_is_killed() {
    let scratch = Scratch::new("hangup");
    let sock = scratch.0.join("a.sock");
    let mut server = serve(&sock, 4096);

    let mut client = Kid::spawn(&["attach", sock.to_str().unwrap()]);
    let attached = client.line();
    assert!(attached.starts_with("attached 0 4096"), "got {attached}");

    // No cooperation from the client, and no timeout on either side.
    client.kill();

    assert_eq!(
        server.line(),
        "hangup 0",
        "the owner did not observe the dead participant's socket close"
    );
}

/// An absent server is distinguishable from a refusing one.
///
/// §3.9 makes a stale socket path an expected state, so "nothing is listening"
/// has to be its own error — `open()` reads it as "no server" and lets the
/// ownership byte decide, whereas a rejection is terminal.
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

/// A client that connects and never speaks must not wedge the owner.
///
/// The handshake `recvmsg` is blocking and the server loop is single-threaded,
/// so without a receive timeout one silent peer — hung, `SIGSTOP`ped, or simply
/// hostile — stalls every other participant's attach *and* the shutdown path,
/// for as long as it cares to hold the connection. §3.7 specifies no timeout on
/// either side.
///
/// The assertion is that a *second, well-behaved* client still gets through.
/// Removing the server's `SO_RCVTIMEO` makes this hang rather than fail, which
/// is exactly the production symptom: an arena that stops accepting nodes and
/// says nothing.
#[test]
fn a_silent_client_cannot_wedge_the_owner() {
    let scratch = Scratch::new("silent-client");
    let sock = scratch.0.join("a.sock");
    let _server = serve(&sock, 4096);

    // Connect, send nothing, and hold the connection open for the whole test.
    let addr = rustix::net::SocketAddrUnix::new(&sock).unwrap();
    let mute = rustix::net::socket_with(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::SEQPACKET,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::connect(&mute, &addr).unwrap();

    // The owner spends its per-client budget on the mute peer, then carries on.
    let attached = tf_tree_ipc::attach(&sock, &good_request(), Duration::from_secs(10))
        .expect("a well-behaved client must still be served");
    assert_eq!(attached.response.arena_size, 4096);

    drop(mute);
}

/// **`Joined` is reachable for the first time.**
///
/// Until §3.7 existed, `NoServer` was the only probe and every `open()` in this
/// file resolved to `Created` or timed out — `CreatePolicy::Never` could not
/// succeed at all, because nothing could hand a second process the segment.
/// This is the path the whole milestone was for.
#[test]
fn a_second_process_joins_a_served_arena() {
    let scratch = Scratch::new("real-join");
    let rd =
        RuntimeDir::resolve_with(&Fixed(scratch.0.clone()), tf_tree_ipc::current_uid()).unwrap();
    let rv = Rendezvous::new(rd, 0, ArenaName::new("default", EnvVar::Name).unwrap());

    // An owner takes the lock file, then serves the socket the rendezvous names.
    let mut creator = Open::new(rv.clone()).open(&mut NoServer).unwrap();
    assert_eq!(creator.outcome(), OpenOutcome::Created);
    let _server = serve(rv.sock_path(), 4096);

    // A joiner with `create = Never` — it must find the arena or fail. Before
    // this PR that combination could only ever fail.
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

    // It came back holding the segment, and the slot it locked is the one the
    // owner named.
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

/// A rejection is terminal and must not consume the open deadline.
///
/// §3.4's loop retries until `open_timeout`. A `LayoutMismatch` cannot be fixed
/// by waiting, so retrying it would replace the one message §3.7 says exists to
/// prevent a multi-hour debugging session with a generic timeout error — and
/// would take the full five seconds to do it.
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

/// **A killed claim holder's lease is released by the kernel, immediately.**
///
/// This is what the lease buys over the arena's `ClaimRecord` alone: the record
/// is a word in shared memory that a `SIGKILL`ed process leaves set forever,
/// while the byte is released with no cooperation and no timeout. That
/// distinction is the whole predicate §6.3's reaper runs on.
///
/// Asserted with **no sleep**: `wait()` returning means the kernel has torn the
/// process's descriptors down, so if the byte were not free by then, no amount
/// of waiting would help and the test would be measuring a race instead.
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
    // A different edge is independent — byte-range locks, not a whole-file one.
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

/// An edge id past the reserved region is refused, not silently wrapped.
///
/// Only reachable from a corrupt header — but a byte outside the region would
/// collide with an identity record, hand one edge to two writers, and present
/// as impossible numbers rather than as an error.
#[test]
fn an_edge_beyond_the_reserved_region_is_refused() {
    let scratch = Scratch::new("claim-range");
    let lock = LockFile::open(&scratch.0.join("claims.lock")).unwrap();
    match lock.probe_claim(u32::MAX) {
        Err(IpcError::ClaimOutOfRange { edge, .. }) => assert_eq!(edge, u32::MAX),
        other => panic!("expected ClaimOutOfRange, got {other:?}"),
    }
}

/// **A claim byte and a participant byte with the same index must not collide.**
///
/// This is what `CLAIM_BASE` is for, and the reason it is worth a test rather
/// than a comment: a collision would make edge *n*'s lease indistinguishable
/// from participant *n*'s registration, so one edge would be handed to two
/// writers — and the symptom is impossible numerical results, not an error.
///
/// Removing the offset leaves every other test in this file passing, which is
/// how the gap was found.
#[test]
fn claim_bytes_and_participant_bytes_do_not_overlap() {
    let scratch = Scratch::new("claim-vs-participant");
    let lock_path = scratch.0.join("both.lock");
    let observer = LockFile::open(&lock_path).unwrap();

    // A child holds *participant* slot 3.
    let mut kid = Kid::spawn(&["hold-participant", lock_path.to_str().unwrap(), "3"]);
    assert_eq!(kid.line(), "held 3");
    assert!(observer.probe_participant(3).unwrap().held);

    // *Edge* 3's lease must still be free, and takeable.
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
