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
//!     // 3. We hold ownership. Do we already have an arena?
//!     if we hold an arena fd { goto 5 }          // reuse it -- never create a second one
//!
//!     // 4. SPLIT-BRAIN CHECK. Is any participant byte locked?
//!     if any participant byte is held { release byte 0; backoff; continue }
//!
//!     // 5. Serve.
//!     ...
//!     return Created | TookOver
//! }
//! on timeout -> Err(ArenaHeldButUnreachable { holder_slots, identities })
//! ```
//!
//! **Step 4 is the whole design.** Without it this sequence is possible: the
//! owner dies; a fresh process starts, finds no socket, wins the ownership lock
//! before any surviving participant has noticed the `HUP`, and creates a
//! *second* arena. The survivors keep using the first. Two arenas, both live,
//! silently diverging — worse than any failure to start, because nothing reports
//! an error and the robot's transform tree is quietly inconsistent between
//! nodes.
//!
//! The check is **deterministic, not a grace period.** If any participant byte
//! is locked, a live arena exists, and a fresh process must not create one, full
//! stop. There is no timing assumption and no window to tune. The timeout case
//! is likewise correct rather than a limitation: if a participant is
//! `SIGSTOP`ped and never takes over, no new process can join, and that is the
//! right answer, because the alternative is divergence.
//!
//! # What this module does not do yet
//!
//! Steps 1 and 5 involve the `SOCK_SEQPACKET` protocol (§3.7) and `memfd`
//! creation (§3.6), which land with `MappedArena`. Here, "is someone serving?"
//! is a [`ServerProbe`] the caller injects, and "serve" means taking the locks
//! and returning an [`OpenOutcome`] that tells the caller which of bind/create
//! it now owes. That split is not a stub: the lock-file half is where every race
//! in §3.4 lives, and it is testable to the last branch without a socket.

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
    /// Create one if nothing is there. The default, and what makes the
    /// zero-argument case work.
    #[default]
    IfAbsent,
    /// Never create; fail with [`IpcError::ArenaAbsent`] instead.
    ///
    /// Worth recommending for anything in a supervised deployment: a consumer
    /// that creates an empty arena because the estimator has not started yet
    /// looks healthy and publishes nothing.
    Never,
    /// Create unconditionally, abandoning any arena that already exists.
    ///
    /// This is `--force-new`. It **skips the split-brain check**, which is to
    /// say it deliberately does the thing §3.4 exists to prevent, and it exists
    /// only because an operator staring at [`IpcError::ArenaHeldButUnreachable`]
    /// on a wedged robot needs an escape hatch. Never take this path
    /// automatically.
    Always,
}

/// How `open()` resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    /// A server was reachable and this process joined it.
    Joined,
    /// Nothing existed; this process won ownership and must now create the
    /// arena (§3.6), unlink any stale socket path, bind and listen.
    ///
    /// A stale socket path is expected rather than exceptional: it is what an
    /// owner that died leaves behind, it holds no state, and §3.9 makes it the
    /// job of whoever wins ownership to remove it.
    Created,
    /// This process already had the arena mapped and has now inherited the
    /// owner role: unlink the stale socket, bind, and serve its *existing* fd.
    /// It must not create a second arena (§3.4 step 3, §3.5).
    TookOver,
}

/// Whether a server is reachable at the socket path, and what it gave us.
///
/// Generic over what a successful probe yields, because the real probe does not
/// merely *observe* a server — it completes the §3.7 handshake and comes back
/// holding the segment fd. Splitting "is anyone there?" from "attach to them"
/// would mean connecting twice and re-running the race in between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach<T> {
    /// The handshake succeeded. Carries whatever the probe obtained.
    Serving(T),
    /// `ECONNREFUSED`, no socket at all, or a server that died mid-handshake.
    ///
    /// **Not an error.** §3.9 makes a stale socket path an expected state and
    /// the ownership byte is the real discriminator, so all three collapse to
    /// the same verdict and the §3.4 loop carries on.
    Absent,
    /// The owner answered, and refused.
    ///
    /// Terminal, unlike [`Reach::Absent`]. §3.4's loop has no exit for a
    /// rejection, so a `LayoutMismatch` would otherwise be retried until the
    /// deadline and then reported as [`IpcError::ArenaHeldButUnreachable`] —
    /// the exact multi-hour debugging session §3.7 says the message exists to
    /// prevent.
    Rejected(IpcError),
}

/// Step 1 of the algorithm, injected.
///
/// The real implementation connects a `SOCK_SEQPACKET` and performs the §3.7
/// handshake. Modelling it as a trait keeps every §3.4 branch reachable from a
/// test — including the ones that only occur when a *live* arena is
/// simultaneously unreachable, which is precisely the split-brain race and is
/// otherwise a matter of winning a timing window on purpose.
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

    /// The participant slot the owner granted in this attachment.
    ///
    /// On the trait rather than read from a concrete type, because this crate
    /// must not know what `Attached` is — for the real probe it holds a segment
    /// fd, which is exactly the arena knowledge §2 keeps out of here.
    fn slot_of(&self, attached: &Self::Attached) -> u32;
}

/// A probe that always reports nothing listening.
///
/// The state of the world after an owner dies, and the state during the
/// split-brain race. Also the honest stand-in until §3.7 exists.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoServer;

impl ServerProbe for NoServer {
    type Attached = ();

    fn probe(&mut self, _sock: &Path) -> Result<Reach<()>, IpcError> {
        Ok(Reach::Absent)
    }

    fn slot_of(&self, _attached: &()) -> u32 {
        // Unreachable: this probe never returns `Serving`.
        u32::MAX
    }
}

/// The builder from `docs/PHASE2.md` §3.2, minus the parts that need an arena.
#[derive(Clone, Debug)]
pub struct Open {
    rendezvous: Rendezvous,
    mode: AccessMode,
    create: CreatePolicy,
    timeout: Duration,
    already_attached: bool,
}

/// Default `open_timeout` (§3.4).
pub const DEFAULT_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// First backoff interval. Doubles up to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_micros(200);
/// Backoff ceiling — small enough that a 5 s timeout still gives hundreds of
/// attempts, so a takeover that completes in a millisecond is joined promptly.
const MAX_BACKOFF: Duration = Duration::from_millis(4);

impl Open {
    /// Start from an already-resolved rendezvous.
    #[must_use]
    pub fn new(rendezvous: Rendezvous) -> Open {
        Open {
            rendezvous,
            mode: AccessMode::ReadOnly,
            create: CreatePolicy::IfAbsent,
            timeout: DEFAULT_OPEN_TIMEOUT,
            already_attached: false,
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

    /// Declare that this process **already has the arena mapped** — it is an
    /// existing participant, and this call is a takeover after the owner died
    /// (§3.4 step 3, §3.5).
    ///
    /// This is what makes step 3 short-circuit past the split-brain check: a
    /// participant that already holds the arena is not at risk of creating a
    /// second one, it is the one thing that *cannot* be. Setting this without
    /// actually holding an arena fd would defeat step 4, which is why it is an
    /// explicit, separately-named builder method rather than an inferred flag.
    #[must_use]
    pub fn already_attached(mut self, yes: bool) -> Open {
        self.already_attached = yes;
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
                // Terminal. Retrying cannot change a version or layout
                // disagreement, and burning the deadline on it would replace a
                // precise message with a timeout.
                Reach::Rejected(why) => return Err(why),
                Reach::Serving(attached) => {
                    // The owner named the slot, so §3.3's specified order —
                    // write the identity, *then* take the lock — is restorable
                    // here. It was not in the fallback below, which has to find
                    // a free slot itself and would race two writers onto one
                    // record. See `register_any`.
                    // `None` means the byte the owner named is held by somebody
                    // the owner has not noticed leaving yet. Drop this
                    // attachment and go round again: the owner re-probes its
                    // table and will name a different slot. Nothing was written
                    // to the arena, so nothing is left behind — which is the
                    // point of taking the byte before touching it.
                    if let Some(slot) =
                        self.register_at(&lock, &identity, probe.slot_of(&attached))?
                    {
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
                // 3. We hold ownership and already have the arena, so this is a
                //    takeover: reuse it, and skip step 4 entirely. A process
                //    that already holds the arena is the one thing that cannot
                //    create a second one.
                if self.already_attached {
                    let slot = self.register_any(&lock, &identity)?;
                    return Ok(Session {
                        outcome: OpenOutcome::TookOver,
                        lock,
                        slot,
                        owner: true,
                        attached: None,
                    });
                }

                // 4. SPLIT-BRAIN CHECK. A held participant byte means a live
                //    arena exists whose holder has not taken over yet, so yield
                //    to it. Deterministic: no grace period, no timing
                //    assumption, no window to tune.
                if self.create != CreatePolicy::Always && lock.any_participant_held()? {
                    lock.release_ownership()?;
                } else if self.create == CreatePolicy::Never {
                    // Nothing is serving and nothing is alive, so there is
                    // genuinely no arena to join. Fail fast rather than wait out
                    // a timeout that cannot change the answer.
                    lock.release_ownership()?;
                    return Err(IpcError::ArenaAbsent);
                } else {
                    // 5. Serve. The caller owes: memfd create + seal (§3.6),
                    //    unlink stale sock, bind, listen.
                    let slot = self.register_any(&lock, &identity)?;
                    return Ok(Session {
                        outcome: OpenOutcome::Created,
                        lock,
                        slot,
                        owner: true,
                        attached: None,
                    });
                }
            }

            if start.elapsed() >= self.timeout {
                return Err(self.held_but_unreachable(&lock)?);
            }
            std::thread::sleep(backoff);
            backoff = core::cmp::min(backoff * 2, MAX_BACKOFF);
        }
    }

    /// Take a participant slot and write its identity record.
    ///
    /// **Deviation from §3.3's "written with `pwrite` before taking the slot
    /// lock", and it is deliberate.** That ordering presumes the slot is already
    /// known, which it is in the finished protocol: §3.7 has the *owner* assign
    /// the slot in its accept loop and return it in the `HelloResponse`. Until
    /// the socket exists, a joiner has to find a free slot itself, and
    /// write-then-lock loses the race it is meant to win — two processes both
    /// see slot 3 free, both write, one takes the lock, and the record now names
    /// the process that *lost*. A lock byte held with somebody else's name
    /// against it is worse than one held with no name yet: it makes
    /// [`IpcError::ArenaHeldButUnreachable`] point an operator at the wrong pid.
    ///
    /// So: lock, then write. The window where a slot is held with a stale record
    /// is a few microseconds long, and the record is advisory (§5.1) — the lock
    /// is the liveness.
    ///
    /// **This path is now only for a creator or a taker-over**, neither of which
    /// has an owner to ask. A joiner uses [`Open::register_at`], where §3.3's
    /// order *is* restored because the slot is known before anything is written.
    fn register_any(&self, lock: &LockFile, identity: &Identity) -> Result<u32, IpcError> {
        let slot = lock.take_any_participant()?;
        lock.write_identity(slot, identity)?;
        Ok(slot)
    }

    /// Take the slot the owner named, in §3.3's specified order.
    ///
    /// Write the identity record first, then the lock byte. That ordering is
    /// safe here and unsafe in [`Open::register_any`], and the difference is
    /// who chose the slot: nobody else is racing us for *this* byte, because the
    /// owner hands each client a different one. So the record can be in place
    /// before the byte is held, and a reader that sees the byte held never sees
    /// it nameless.
    ///
    /// Returns `None` if the byte is already held — the owner named a slot whose
    /// previous holder it has not seen leave yet. The caller retries the
    /// handshake rather than falling back to another slot: falling back would
    /// re-open the split between the byte and the arena record that
    /// `register_at` exists to close.
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
    fn held_but_unreachable(&self, lock: &LockFile) -> Result<IpcError, IpcError> {
        let holder_slots = lock.held_participants()?;
        // `trailing_zeros()` is 64 on an empty mask, which is not a slot any
        // arena has. `Display` happened to special-case the empty mask, but the
        // field is public and a supervisor logging it would have recorded a
        // fictional slot — so make "no holder" unrepresentable instead.
        let first = (holder_slots != 0).then(|| holder_slots.trailing_zeros());
        let first_pid = match first {
            Some(slot) => lock.read_identity(slot)?.map_or(0, |id| id.pid),
            None => 0,
        };
        Ok(IpcError::ArenaHeldButUnreachable {
            holder_slots,
            first_slot: first,
            first_pid,
        })
    }
}

/// The result of a successful `open()`: the outcome, and the locks that make it
/// true.
///
/// Dropping a `Session` closes the lock file, which releases the ownership and
/// participant bytes — the same thing the kernel does when the process dies, by
/// the same mechanism. There is no separate teardown to forget.
#[derive(Debug)]
pub struct Session<A = ()> {
    outcome: OpenOutcome,
    lock: LockFile,
    slot: u32,
    owner: bool,
    attached: Option<A>,
}

impl<A> Session<A> {
    /// How `open()` resolved, and therefore what the caller owes: nothing for
    /// [`OpenOutcome::Joined`], create-and-bind for [`OpenOutcome::Created`],
    /// bind-only for [`OpenOutcome::TookOver`].
    #[must_use]
    pub fn outcome(&self) -> OpenOutcome {
        self.outcome
    }

    /// This process's participant slot. Its lock byte is held for the lifetime
    /// of the `Session`.
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
    /// Ownership is a role, not a property of the arena: releasing byte 0 lets
    /// another participant take over. **Lookups do not stop, slow down, or
    /// observe anything during a takeover** — the data plane touches only the
    /// mapping, and ownership lives entirely in the control plane.
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

    /// Take what the §3.7 handshake yielded, if this session joined one.
    ///
    /// `None` for [`OpenOutcome::Created`] and [`OpenOutcome::TookOver`]: both
    /// already have the arena and never ran a handshake.
    ///
    /// Taking rather than borrowing, because the payload owns file descriptors —
    /// the segment and the connection whose closure tells the owner this
    /// participant is gone (D17). Leaving it inside the `Session` would make the
    /// lifetime of the liveness signal the lifetime of a struct the caller has
    /// no reason to keep.
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
        // Tests that pre-seed a lock (a surviving participant, a process
        // mid-bind) open the lock file before `Open::open` would have created
        // the directory.
        rv.ensure_dir().unwrap();
        (rv, dir)
    }

    struct EnvOverride(std::path::PathBuf);

    impl crate::runtime_dir::EnvLookup for EnvOverride {
        fn var(&self, key: &str) -> Option<std::ffi::OsString> {
            (key == "TF_TREE_RUNTIME_DIR").then(|| self.0.clone().into_os_string())
        }
    }

    /// A probe that reports "serving" only after `n` calls, so a test can make
    /// the owner appear mid-loop. Grants slot `1`, since the creator in these
    /// tests already holds slot `0`.
    struct ServingAfter(u32);

    impl ServerProbe for ServingAfter {
        type Attached = u32;

        fn probe(&mut self, _sock: &Path) -> Result<Reach<u32>, IpcError> {
            if self.0 == 0 {
                return Ok(Reach::Serving(1));
            }
            self.0 -= 1;
            Ok(Reach::Absent)
        }

        fn slot_of(&self, attached: &u32) -> u32 {
            *attached
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
        // §3.4 step 4, the whole design. A participant byte is held (a surviving
        // participant of an arena whose owner died) and nothing is serving. A
        // fresh `open()` must NOT create.
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

        // And the yielded ownership byte was released, so the survivor can take
        // over the instant it notices.
        let taker = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(taker.try_take_ownership().unwrap(), LockAttempt::Acquired);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_survivor_that_holds_the_arena_takes_over_instead() {
        // Same world as the previous test, but from the survivor's side: step 3
        // short-circuits, so it takes over rather than yielding forever.
        let (rv, dir) = rendezvous("takeover");
        let other = LockFile::open(rv.lock_path()).unwrap();
        other.try_take_participant(5).unwrap();

        let s = Open::new(rv)
            .already_attached(true)
            .timeout(Duration::from_millis(50))
            .open(&mut NoServer)
            .unwrap();
        assert_eq!(s.outcome(), OpenOutcome::TookOver);
        assert!(s.is_owner());
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
        // It must not have left byte 0 held on the way out.
        let taker = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(taker.try_take_ownership().unwrap(), LockAttempt::Acquired);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn force_new_is_the_documented_escape_hatch() {
        let (rv, dir) = rendezvous("force");
        let survivor = LockFile::open(rv.lock_path()).unwrap();
        survivor.try_take_participant(1).unwrap();
        // IfAbsent refuses...
        assert!(Open::new(rv.clone())
            .timeout(Duration::from_millis(30))
            .open(&mut NoServer)
            .is_err());
        // ...and Always is the loud, explicit override.
        let s = Open::new(rv)
            .create(CreatePolicy::Always)
            .open(&mut NoServer)
            .unwrap();
        assert_eq!(s.outcome(), OpenOutcome::Created);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_contended_ownership_byte_is_retried_not_failed() {
        // Another process is mid-bind: it holds byte 0 but is not yet serving.
        // The opener must back off and retry, and join once the socket appears.
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
        // The participant byte is still held: releasing the role is not
        // detaching.
        assert!(heir.probe_participant(s.slot()).unwrap().held);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
