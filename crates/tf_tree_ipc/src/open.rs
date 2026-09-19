//! `open()` — the NORMATIVE §3.4 decision algorithm.
//!
//! ```text
//! deadline = now + open_timeout (default 5 s)
//! loop {
//!     // 1. Someone is already serving. Join.
//!     if connect(sock) succeeds { ...; return Joined }
//!
//!     // 2. Nobody is serving. Try to become the owner.
//!     if F_OFD_SETLK(byte 0, exclusive) fails { backoff; continue }
//!
//!     // 3. Deleted (docs/decisions/0037). Do not re-add it.
//!
//!     // 4. SPLIT-BRAIN CHECK. Is any participant byte locked?
//!     if any participant byte is held { release byte 0; backoff; continue }
//!
//!     // 5. Serve.
//!     ...
//!     return Created
//! }
//! on timeout -> Err(ArenaHeldButUnreachable { holder_slots, identities })
//! ```
//!
//! **Step 4 is the whole design.** Without it an owner dies, a fresh process
//! wins the ownership lock before survivors notice the `HUP`, and creates a
//! *second* arena while the survivors keep the first — silent divergence. The
//! check is deterministic, not a grace period: any held participant byte means a
//! live arena exists, so the timeout case (a `SIGSTOP`ped participant) is the
//! right answer, not a limitation.
//!
//! # What this module leaves to its caller
//!
//! Steps 1 and 5 are injected, keeping this module free of the socket and the
//! arena (this crate depends on `rustix` and `libc` only). "Is someone serving?"
//! is a [`ServerProbe`] — [`crate::SocketProbe`] runs the real §3.7 handshake,
//! [`NoServer`] is the test one — and "serve" returns an [`OpenOutcome`] saying
//! whether the caller owes bind/create. The lock-file half, where every §3.4
//! race lives, is testable without a socket.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::error::IpcError;
use crate::identity::{AccessMode, Identity};
use crate::lockfile::LockFile;
use crate::ofd::LockAttempt;
use crate::rendezvous::Rendezvous;

/// What `open()` should do when no arena exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CreatePolicy {
    /// Create one if nothing is there. The default.
    #[default]
    IfAbsent,
    /// Never create; fail with [`IpcError::ArenaAbsent`] instead.
    ///
    /// Recommended under supervision: a consumer that creates an empty arena
    /// before the estimator starts looks healthy and publishes nothing.
    Never,
    /// Create over an arena that exists but cannot be reached, abandoning it.
    ///
    /// The escape hatch is this policy alone (`docs/PHASE2.md` §3.4's
    /// `--force-new` never existed; §0.0, #189). It **skips the split-brain
    /// check**, deliberately doing what §3.4 exists to prevent. Never take this
    /// path automatically.
    ///
    /// Step 1 still runs, so a rendezvous with a server answering is *joined*.
    /// Steps 2 and 5 still take the ownership byte and participant byte 0
    /// (`CREATOR_SLOT`, `docs/decisions/0035`), so anything holding either
    /// refuses exactly like [`CreatePolicy::IfAbsent`]; this creates only when
    /// the owner is gone and non-owner holders survive (§3.4's stranded
    /// participant). Read the bytes, not the role: [`Session::release_ownership`]
    /// keeps byte 0. Pinned by `a_live_byte_0_refuses_both_policies` and
    /// `a_held_ownership_byte_refuses_the_hatch_and_freeing_it_lets_one_through`
    /// (`crates/tf_tree/tests/rendezvous.rs`).
    ///
    /// # What it leaves behind (§3.9, §11.3)
    ///
    /// - **The abandoned arena**: survivors keep publishing; two arenas, two
    ///   `instance_uuid`s, diverging.
    /// - **Their lock bytes**: the new owner's slot assigner skips held bytes, so
    ///   those slots are gone until the survivors exit.
    /// - **Their claim leases** at [`crate::CLAIM_BASE`]` + edge_id` (§6.1): the
    ///   replacement numbers edges from zero, so a claim on an id a survivor
    ///   holds loses the lease ([`LockAttempt::Contended`], surfaced as
    ///   `ClaimApiError::LeaseContended`); the aliasing `docs/decisions/0005` §5
    ///   names.
    /// - **A crash mid-force is the original wedge again**: §11.3's
    ///   `open.after_create_before_bind` row assumes no participant byte is held,
    ///   the one state this policy is reached from.
    Always,
}

/// How `open()` resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    /// A server was reachable and this process joined it.
    Joined,
    /// Nothing existed; this process won ownership and must now create the
    /// arena (§3.6), unlink any stale socket path (§3.9), bind and listen.
    Created,
}

/// Whether a server is reachable at the socket path, and what it gave us.
///
/// Generic over what a probe yields: the real one completes the §3.7 handshake
/// and returns holding the segment fd.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach<T> {
    /// The handshake succeeded.
    ///
    /// The slot is in the variant so a probe cannot report success without one.
    Serving {
        /// What the probe obtained; for the real one, the attachment.
        attached: T,
        /// The participant slot the owner granted, also the lock byte to take.
        slot: u32,
    },
    /// `ECONNREFUSED`, no socket at all, or a server that died mid-handshake.
    ///
    /// **Not an error**: a stale socket path is expected (§3.9) and the ownership
    /// byte is the real discriminator.
    Absent,
    /// The owner answered, and refused.
    ///
    /// Terminal, unlike [`Reach::Absent`]: retrying a `LayoutMismatch` to the
    /// deadline would report [`IpcError::ArenaHeldButUnreachable`], the debugging
    /// session §3.7 exists to prevent.
    Rejected(IpcError),
}

/// Step 1 of the algorithm, injected.
///
/// The real implementation connects a `SOCK_SEQPACKET` and performs the §3.7
/// handshake; a trait keeps every §3.4 branch reachable from a test.
pub trait ServerProbe {
    /// What a successful probe yields — for the real one, the attachment.
    type Attached;

    /// Try to reach a server bound at `sock`.
    ///
    /// # Errors
    ///
    /// Only for failures that are neither "nobody is listening" nor "the owner
    /// refused" — those are [`Reach::Absent`] and [`Reach::Rejected`].
    fn probe(&mut self, sock: &Path) -> Result<Reach<Self::Attached>, IpcError>;
}

/// A probe that always reports nothing listening.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoServer;

impl ServerProbe for NoServer {
    type Attached = ();

    fn probe(&mut self, _sock: &Path) -> Result<Reach<()>, IpcError> {
        Ok(Reach::Absent)
    }
}

/// The builder from `docs/PHASE2.md` §3.2, minus the parts that need an arena.
#[derive(Clone, Debug)]
pub struct Open {
    rendezvous: Rendezvous,
    mode: AccessMode,
    create: CreatePolicy,
    timeout: Duration,
}

/// Default `open_timeout` (§3.4).
pub const DEFAULT_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// First backoff interval for this crate's handshake retry; doubles up to
/// [`MAX_BACKOFF`]. Private: the facade keeps its own pair in `tf_tree::tree`.
const MIN_BACKOFF: Duration = Duration::from_micros(200);
/// Backoff ceiling, small enough that a 5 s timeout gives hundreds of attempts.
const MAX_BACKOFF: Duration = Duration::from_millis(4);

/// The participant slot a creator takes: the same integer as the arena record
/// `TreeBuilder::build_shared` gives it, which the facade's liveness predicates
/// use to index both (#201).
const CREATOR_SLOT: u32 = 0;

impl Open {
    /// Start from an already-resolved rendezvous.
    #[must_use]
    pub fn new(rendezvous: Rendezvous) -> Open {
        Open {
            rendezvous,
            mode: AccessMode::ReadOnly,
            create: CreatePolicy::IfAbsent,
            timeout: DEFAULT_OPEN_TIMEOUT,
        }
    }

    /// Resolve the rendezvous from the environment (§3.1, §3.2).
    ///
    /// # Errors
    ///
    /// Anything [`Rendezvous::from_env`] can fail with.
    pub fn from_env() -> Result<Open, IpcError> {
        Ok(Open::new(Rendezvous::from_env()?))
    }

    /// Attach mode. [`AccessMode::ReadOnly`] is the consumer default (§8).
    #[must_use]
    pub fn mode(mut self, mode: AccessMode) -> Open {
        self.mode = mode;
        self
    }

    /// What to do when no arena exists.
    #[must_use]
    pub fn create(mut self, create: CreatePolicy) -> Open {
        self.create = create;
        self
    }

    /// How long to keep retrying before giving up with
    /// [`IpcError::ArenaHeldButUnreachable`].
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Open {
        self.timeout = timeout;
        self
    }

    /// The resolved rendezvous.
    #[must_use]
    pub fn rendezvous(&self) -> &Rendezvous {
        &self.rendezvous
    }

    /// Run the §3.4 algorithm.
    ///
    /// # Errors
    ///
    /// [`IpcError::ArenaHeldButUnreachable`] on timeout — a live arena exists
    /// but nothing serves it; [`IpcError::ArenaAbsent`] under
    /// [`CreatePolicy::Never`]; [`IpcError::NoParticipantSlots`] when the
    /// participant table is full; and any lock or directory failure.
    pub fn open<P: ServerProbe>(&self, probe: &mut P) -> Result<Session<P::Attached>, IpcError> {
        self.rendezvous.ensure_dir()?;
        let lock = LockFile::open(self.rendezvous.lock_path())?;
        let identity = Identity::of_self_best_effort(self.mode);

        let start = Instant::now();
        let mut backoff = MIN_BACKOFF;
        loop {
            // 1. Someone is already serving. Join.
            match probe.probe(self.rendezvous.sock_path())? {
                Reach::Rejected(why) => return Err(why),
                Reach::Serving { attached, slot } => {
                    // `None`: the byte the owner named is still held by a departed
                    // peer the owner has not noticed. Retry; nothing was written.
                    if let Some(slot) = self.register_at(&lock, &identity, slot)? {
                        return Ok(Session {
                            outcome: OpenOutcome::Joined,
                            lock,
                            slot,
                            owner: false,
                            attached: Some(attached),
                        });
                    }
                }
                Reach::Absent => {}
            }

            // 2. Nobody is serving. Try to become the owner.
            if lock.try_take_ownership()? == LockAttempt::Acquired {
                // Step 3 is deleted: a takeover is not a second `open()`
                // (`docs/decisions/0037`). **Do not re-add it.**
                // 4. SPLIT-BRAIN CHECK: a held participant byte means a live arena;
                //    yield to it.
                if self.create != CreatePolicy::Always && lock.any_participant_held()? {
                    lock.release_ownership()?;
                } else if self.create == CreatePolicy::Never {
                    // No arena to join; fail fast rather than wait out the timeout.
                    lock.release_ownership()?;
                    return Err(IpcError::ArenaAbsent);
                } else if let Some(slot) = self.register_creator(&lock, &identity)? {
                    // 5. Serve. The caller owes memfd create + seal (§3.6).
                    return Ok(Session {
                        outcome: OpenOutcome::Created,
                        lock,
                        slot,
                        owner: true,
                        attached: None,
                    });
                } else {
                    // Somebody took the creator's byte after step 4's scan: take
                    // step 4's branch. Nothing was built.
                    lock.release_ownership()?;
                }
            }

            if start.elapsed() >= self.timeout {
                return Err(self.held_but_unreachable(&lock)?);
            }
            std::thread::sleep(backoff);
            backoff = core::cmp::min(backoff * 2, MAX_BACKOFF);
        }
    }

    /// Take the creator's participant slot (`0`), or report `None` if somebody
    /// else holds it.
    ///
    /// Lock, then write the identity record: with the slot not yet known to be
    /// ours, write-then-lock lets a loser's name land against a winner's byte and
    /// point [`IpcError::ArenaHeldButUnreachable`] at the wrong pid. The record is
    /// advisory (§5.1). A joiner uses [`Open::register_at`]; a taker-over
    /// registers nothing (`0028` question 3, `docs/decisions/0037`).
    ///
    /// # Why this is not a scan for the first free byte
    ///
    /// A creator is the first participant (§3.4 step 4), so its slot is `0`, and
    /// the lock byte and arena record must share that one integer. A scan is a
    /// pass separate from step 4's, and byte 0 can be taken between them (#201:
    /// 2242 of 4000 races took a non-zero byte). So the check and the take are one
    /// `F_OFD_SETLK` on byte 0, atomic in the kernel; `Ok(None)` is step 4's
    /// condition and the caller takes step 4's branch.
    ///
    /// [`CreatePolicy::Always`] gets the same treatment: `Ok(None)` there means a
    /// live participant holds the creator's byte, which no force can pass; the
    /// caller times out into [`IpcError::ArenaHeldButUnreachable`]. Byte 0 is the
    /// owner's for its whole life (`0035`, joiners get `>= 1`), so a free byte 0
    /// with a held byte `>= 1` is the stranded-participant case the hatch
    /// resolves. [`Session::release_ownership`] keeps byte 0, and
    /// `defect_201_release_ownership_strands_a_live_non_owner_on_byte_0` pins
    /// that state. Pinned by `a_live_byte_0_refuses_both_policies`
    /// (`crates/tf_tree/tests/rendezvous.rs`).
    fn register_creator(
        &self,
        lock: &LockFile,
        identity: &Identity,
    ) -> Result<Option<u32>, IpcError> {
        if lock.try_take_participant(CREATOR_SLOT)? == LockAttempt::Contended {
            return Ok(None);
        }
        lock.write_identity(CREATOR_SLOT, identity)?;
        Ok(Some(CREATOR_SLOT))
    }

    /// Take the slot the owner named, in §3.3's order: identity record first,
    /// then the lock byte. Safe here because the owner hands each client a
    /// different byte, so nobody races for it.
    ///
    /// `None` if the byte is still held: the caller retries the handshake rather
    /// than falling back to another slot, which would split byte from record.
    fn register_at(
        &self,
        lock: &LockFile,
        identity: &Identity,
        slot: u32,
    ) -> Result<Option<u32>, IpcError> {
        lock.write_identity(slot, identity)?;
        match lock.try_take_participant(slot)? {
            LockAttempt::Acquired => Ok(Some(slot)),
            LockAttempt::Contended => Ok(None),
        }
    }

    /// Build the timeout error, naming the slots an operator has to deal with.
    ///
    /// Every caller has released the ownership byte, so the ownership probe
    /// reports somebody *else* — the bit deciding whether
    /// [`CreatePolicy::Always`] could help. Advisory, read at the deadline.
    fn held_but_unreachable(&self, lock: &LockFile) -> Result<IpcError, IpcError> {
        let holder_slots = lock.held_participants()?;
        // `trailing_zeros()` is 64 on an empty mask, not a slot: make "no holder"
        // unrepresentable.
        let first = (holder_slots != 0).then(|| holder_slots.trailing_zeros());
        let first_pid = match first {
            Some(slot) => lock.read_identity(slot)?.map_or(0, |id| id.pid),
            None => 0,
        };
        Ok(IpcError::ArenaHeldButUnreachable {
            holder_slots,
            first_slot: first,
            first_pid,
            ownership_held: lock.probe_ownership()?.held,
        })
    }
}

/// The result of a successful `open()`: the outcome, and the locks that make it
/// true. Dropping a `Session` closes the lock file, releasing its bytes as
/// process death would.
#[derive(Debug)]
pub struct Session<A = ()> {
    outcome: OpenOutcome,
    lock: LockFile,
    slot: u32,
    owner: bool,
    attached: Option<A>,
}

impl<A> Session<A> {
    /// How `open()` resolved: nothing owed for [`OpenOutcome::Joined`],
    /// create-and-bind for [`OpenOutcome::Created`].
    #[must_use]
    pub fn outcome(&self) -> OpenOutcome {
        self.outcome
    }

    /// This process's participant slot; its byte is held for the `Session`.
    #[must_use]
    pub fn slot(&self) -> u32 {
        self.slot
    }

    /// Whether this process holds byte 0 and is therefore the owner.
    #[must_use]
    pub fn is_owner(&self) -> bool {
        self.owner
    }

    /// The lock file, for identity lookups and (later) claim locks.
    #[must_use]
    pub fn lock_file(&self) -> &LockFile {
        &self.lock
    }

    /// Give up the owner role while staying attached (§3.5).
    ///
    /// Ownership is a role: releasing byte 0 lets another participant take over.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`].
    pub fn release_ownership(&mut self) -> Result<(), IpcError> {
        if self.owner {
            self.lock.release_ownership()?;
            self.owner = false;
        }
        Ok(())
    }

    /// Inherit the owner role, keeping this session's slot, byte and arena
    /// (§3.5). Returns whether this process is now the owner.
    ///
    /// Answers [`0037`]'s question 5. The lock is taken on **the description this
    /// session already holds**, so slot, byte and arena record cannot disagree —
    /// nothing needs verifying, unlike a second `open()`, where a caller and a
    /// live peer on byte *n* are indistinguishable ([`0037`], `0028` question 3).
    ///
    /// # What the caller still owes
    ///
    /// Owning byte 0 does not make this process a *server*: bind the socket and
    /// serve the existing segment (`tf_tree::Tree::inherit_ownership`). Lookups
    /// are unaffected (§3.5).
    ///
    /// # Racing survivors
    ///
    /// The loser gets `Ok(false)` and stays a participant with its slot intact.
    /// It is not final: somebody else may hold byte 0 mid-bind, or a fresh
    /// `open()` in steps 2–4 may hand it back ([`0057`] Decision 3). Retry while
    /// the attach socket has hung up **and** [`Self::ownership_held`] reads byte 0
    /// free — the pair `tf_tree::Tree::owner_lost` checks; a hangup alone is not
    /// vacancy ([`0043`]).
    ///
    /// Already the owner: no-op returning `Ok(true)`.
    ///
    /// [`0037`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0037-a-takeover-is-not-a-second-open.md
    /// [`0043`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0043-owner-lost-is-a-question-about-the-owner.md
    /// [`0057`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`] for any `fcntl` failure that is not contention.
    pub fn take_over_ownership(&mut self) -> Result<bool, IpcError> {
        if self.owner {
            return Ok(true);
        }
        match self.lock.try_take_ownership()? {
            LockAttempt::Acquired => {
                self.owner = true;
                Ok(true)
            }
            LockAttempt::Contended => Ok(false),
        }
    }

    /// Does **anyone else** hold the ownership byte (§3.3 byte 0)?
    ///
    /// The kernel's answer, and the second half of `tf_tree::Tree::owner_lost`:
    /// a hung-up socket says this process's channel is dead, only this says the
    /// **role** is vacant ([`0043`]). The byte, not the participant table's
    /// `/proc` heuristic ([`0033`]), is the liveness authority (§5.1, §6.1,
    /// [`0029`]).
    ///
    /// An owner asking about its own byte gets `false` (nothing conflicts with
    /// itself); use [`Self::take_over_ownership`]'s return for "am I the owner".
    ///
    /// [`0029`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0029-the-topology-lock-is-a-kernel-lock.md
    /// [`0033`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0033-the-identity-record-cannot-name-a-namespace.md
    /// [`0043`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0043-owner-lost-is-a-question-about-the-owner.md
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`] for any `fcntl` failure.
    pub fn ownership_held(&self) -> Result<bool, IpcError> {
        Ok(self.lock.probe_ownership()?.held)
    }

    /// Take what the §3.7 handshake yielded; `None` for [`OpenOutcome::Created`].
    ///
    /// Taking, not borrowing: the payload owns the segment and the connection
    /// whose closure signals this participant's departure (D17).
    pub fn take_attached(&mut self) -> Option<A> {
        self.attached.take()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::error::EnvVar;
    use crate::rendezvous::ArenaName;
    use crate::runtime_dir::{current_uid, RuntimeDir};

    fn rendezvous(tag: &str) -> (Rendezvous, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("tf_tree_ipc_open-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rd = RuntimeDir::resolve_with(&EnvOverride(dir.clone()), current_uid()).unwrap();
        let rv = Rendezvous::new(rd, 0, ArenaName::new("default", EnvVar::Name).unwrap());
        // Pre-seeded locks need the directory before `Open::open` creates it.
        rv.ensure_dir().unwrap();
        (rv, dir)
    }

    struct EnvOverride(std::path::PathBuf);

    impl crate::runtime_dir::EnvLookup for EnvOverride {
        fn var(&self, key: &str) -> Option<std::ffi::OsString> {
            (key == "TF_TREE_RUNTIME_DIR").then(|| self.0.clone().into_os_string())
        }
    }

    /// A probe that reports "serving" only after `n` calls; grants slot `1`.
    struct ServingAfter(u32);

    impl ServerProbe for ServingAfter {
        type Attached = u32;

        fn probe(&mut self, _sock: &Path) -> Result<Reach<u32>, IpcError> {
            if self.0 == 0 {
                return Ok(Reach::Serving {
                    attached: 1,
                    slot: 1,
                });
            }
            self.0 -= 1;
            Ok(Reach::Absent)
        }
    }

    #[test]
    fn nothing_present_means_create() {
        let (rv, dir) = rendezvous("create");
        let s = Open::new(rv).open(&mut NoServer).unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Created);
        assert!(s.is_owner());
        assert_eq!(s.slot(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_reachable_server_means_join() {
        let (rv, dir) = rendezvous("join");
        let s = Open::new(rv).open(&mut ServingAfter(0)).unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Joined);
        assert!(!s.is_owner(), "a joiner must not hold byte 0");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_second_opener_joins_rather_than_creating() {
        let (rv, dir) = rendezvous("second");
        let first = Open::new(rv.clone()).open(&mut NoServer).unwrap();
        assert_eq!(first.outcome(), OpenOutcome::Created);
        // The first process is now serving, so the second one's probe succeeds.
        let second = Open::new(rv).open(&mut ServingAfter(0)).unwrap();
        assert_eq!(second.outcome(), OpenOutcome::Joined);
        assert_ne!(first.slot(), second.slot(), "slots must be distinct");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_split_brain_check_refuses_to_create() {
        // §3.4 step 4: a survivor holds a byte and nothing is serving; `open()`
        // must NOT create.
        let (rv, dir) = rendezvous("split-brain");
        let survivor = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(
            survivor.try_take_participant(2).unwrap(),
            LockAttempt::Acquired
        );

        let err = Open::new(rv.clone())
            .timeout(Duration::from_millis(50))
            .open(&mut NoServer)
            .unwrap_err();
        match err {
            IpcError::ArenaHeldButUnreachable {
                holder_slots,
                first_slot,
                ..
            } => {
                assert_eq!(holder_slots, 1 << 2);
                assert_eq!(first_slot, Some(2));
            }
            other => panic!("expected ArenaHeldButUnreachable, got {other}"),
        }

        // The yielded ownership byte was released.
        let taker = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(taker.try_take_ownership().unwrap(), LockAttempt::Acquired);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn create_never_fails_fast_when_nothing_exists() {
        let (rv, dir) = rendezvous("never");
        let err = Open::new(rv.clone())
            .create(CreatePolicy::Never)
            .timeout(Duration::from_secs(30))
            .open(&mut NoServer)
            .unwrap_err();
        assert_eq!(err, IpcError::ArenaAbsent);
        // Byte 0 was not left held.
        let taker = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(taker.try_take_ownership().unwrap(), LockAttempt::Acquired);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The escape hatch is this policy, not a flag (#189). The survivor keeps
    /// byte 1, so the created session must take a different one.
    #[test]
    fn create_always_overrides_the_split_brain_check() {
        let (rv, dir) = rendezvous("force");
        let survivor = LockFile::open(rv.lock_path()).unwrap();
        survivor.try_take_participant(1).unwrap();
        // IfAbsent refuses...
        assert!(Open::new(rv.clone())
            .timeout(Duration::from_millis(30))
            .open(&mut NoServer)
            .is_err());
        // ...and Always is the explicit override.
        let s = Open::new(rv)
            .create(CreatePolicy::Always)
            .open(&mut NoServer)
            .unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Created);
        assert_ne!(s.slot(), 1, "the escape hatch took the survivor's byte");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The escape hatch abandons an *unreachable* arena, never a served one.
    #[test]
    fn create_always_still_joins_a_reachable_server() {
        let (rv, dir) = rendezvous("force-reachable");
        let s = Open::new(rv)
            .create(CreatePolicy::Always)
            .open(&mut ServingAfter(0))
            .unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Joined);
        assert!(!s.is_owner(), "a joiner must not hold byte 0");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_contended_ownership_byte_is_retried_not_failed() {
        // Another process is mid-bind: back off, join once the socket appears.
        let (rv, dir) = rendezvous("mid-bind");
        let binder = LockFile::open(rv.lock_path()).unwrap();
        binder.try_take_ownership().unwrap();

        let s = Open::new(rv)
            .timeout(Duration::from_secs(2))
            .open(&mut ServingAfter(3))
            .unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Joined);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ownership_can_be_released_without_detaching() {
        let (rv, dir) = rendezvous("migrate");
        let mut s = Open::new(rv.clone()).open(&mut NoServer).unwrap();
        assert!(s.is_owner());
        s.release_ownership().unwrap();
        assert!(!s.is_owner());

        let heir = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(heir.try_take_ownership().unwrap(), LockAttempt::Acquired);
        // Releasing the role is not detaching.
        assert!(heir.probe_participant(s.slot()).unwrap().held);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `Session::ownership_held` reports the kernel's answer, live
    /// ([`0043`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0043-owner-lost-is-a-question-about-the-owner.md)).
    ///
    /// Mutant `Ok(!self.owner)` fails the first assertion. The third assertion
    /// guards against a latching cache, which no one-line mutant expresses: a
    /// loser must stop answering `true` after a takeover and start again if the
    /// new owner dies.
    #[test]
    fn ownership_held_tracks_the_byte_and_not_a_latch() {
        let (rv, dir) = rendezvous("ownprobe");
        let mut s = Open::new(rv.clone()).open(&mut NoServer).unwrap();
        // The survivor's shape: role given up, session kept.
        s.release_ownership().unwrap();
        assert!(
            !s.ownership_held().unwrap(),
            "nobody holds byte 0 once the creator released it"
        );

        let heir = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(heir.try_take_ownership().unwrap(), LockAttempt::Acquired);
        assert!(
            s.ownership_held().unwrap(),
            "another description holds byte 0 and this session must see it"
        );

        heir.release_ownership().unwrap();
        assert!(
            !s.ownership_held().unwrap(),
            "the byte is free again, so the answer must move back"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An owner asking about its own byte is told `false`; `Tree::owner_lost`
    /// reaches it only from the `Joined` arm, and this stops that scoping being
    /// deleted as unnecessary.
    #[test]
    fn an_owner_does_not_see_its_own_ownership_byte() {
        let (rv, dir) = rendezvous("ownself");
        let s = Open::new(rv).open(&mut NoServer).unwrap();
        assert!(s.is_owner());
        assert!(
            !s.ownership_held().unwrap(),
            "a description never conflicts with itself"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
