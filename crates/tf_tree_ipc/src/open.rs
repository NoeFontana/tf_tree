//! `open()` — the NORMATIVE §3.4 decision algorithm.
//!
//! ```text
//! deadline = now + open_timeout (default 5 s)
//! loop {
//!     1. if connect(sock) succeeds { ...; return Joined }
//!     2. if F_OFD_SETLK(byte 0, exclusive) fails { backoff; continue }
//!     3. deleted (docs/decisions/0037); do not re-add
//!     4. SPLIT-BRAIN CHECK: any participant byte held ->
//!            release byte 0; backoff; continue
//!     5. serve; return Created
//! }
//! on timeout -> Err(ArenaHeldButUnreachable { holder_slots, identities })
//! ```
//!
//! **Step 4 is the whole design.** Without it, an owner dies, a fresh process
//! finds no socket, wins the ownership lock before any survivor notices the
//! `HUP`, and creates a *second* arena — two live arenas diverging, no error
//! anywhere. It is **deterministic, not a grace period**: a locked participant
//! byte means a live arena, full stop. The timeout is correct for the same
//! reason — a `SIGSTOP`ped participant that never takes over blocks every new
//! process, and that beats divergence.
//!
//! Steps 1 and 5 are injected, which keeps this module free of the socket
//! ([`crate::OwnerServer`], [`crate::attach`]) and of the arena (this crate
//! depends only on `rustix` and `libc`, so `memfd` creation cannot happen here).
//! Step 1 is a [`ServerProbe`] — [`crate::SocketProbe`] for the real §3.7
//! handshake, [`NoServer`] for tests — and "serve" means taking the locks and
//! returning an [`OpenOutcome`] naming what the caller owes. What is left is
//! where every §3.4 race lives, testable without a socket.

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
    /// Prefer it in a supervised deployment: a consumer that creates an empty
    /// arena because the estimator has not started looks healthy while
    /// publishing nothing.
    Never,
    /// Create over an arena that exists but cannot be reached, abandoning it.
    ///
    /// **`docs/PHASE2.md` §3.4 calls this `--force-new`, and the flag has never
    /// existed** — this policy is the entire escape hatch (§0.0's row, #189). It
    /// **skips the split-brain check**, so never take it automatically; it is for
    /// an operator staring at [`IpcError::ArenaHeldButUnreachable`].
    ///
    /// It skips step 4's participant scan and nothing else: step 1 still joins a
    /// server that answers, and steps 2 and 5 still take the ownership byte and
    /// participant byte 0. Byte 0 is the owner's for the owner's whole life
    /// (`CREATOR_SLOT`, `docs/decisions/0035`; joiners get `>= 1`), so either byte
    /// held refuses exactly like [`CreatePolicy::IfAbsent`] — including a live
    /// non-owner left on byte 0 by [`Session::release_ownership`]. Read the bytes,
    /// not the role: all it can abandon is §3.4's stranded participant, an owner
    /// gone with non-owner holders alive. Both ways measured by
    /// `a_live_byte_0_refuses_both_policies_and_says_no_force_can_pass` and
    /// `a_held_ownership_byte_refuses_the_hatch_and_freeing_it_lets_one_through`
    /// (`crates/tf_tree/tests/rendezvous.rs`).
    ///
    /// # What it leaves behind (§3.9, §11.3)
    ///
    /// - **The abandoned arena**: §3.9 frees the segment only when the last
    ///   survivor drops it, so two `instance_uuid`s diverge — chosen, not
    ///   suffered.
    /// - **Their lock bytes.** The new owner's assigner skips a byte the kernel
    ///   reports held, so those slots are gone until the survivors exit.
    /// - **Their claim leases**, at [`crate::CLAIM_BASE`]` + edge_id` in that same
    ///   file (§6.1). The replacement numbers edges from zero, so a writer can win
    ///   the arena CAS on a genuinely free record and still lose the lease — the
    ///   [`LockAttempt::Contended`] arm, surfaced as
    ///   `ClaimApiError::LeaseContended` and unclearable while the survivor runs.
    ///   `docs/decisions/0005` §5's byte aliasing.
    /// - **A crash mid-force is the original wedge again**: §11.3's
    ///   `open.after_create_before_bind` row assumes no participant byte is held,
    ///   the one state this policy is reached from, so only another `Always` gets
    ///   past it.
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
    /// A stale socket path is expected, not exceptional: §3.9 makes removing what
    /// a dead owner left the winner's job, and it holds no state.
    Created,
}

/// Whether a server is reachable at the socket path, and what it gave us.
///
/// Generic over what a probe yields: the real one completes the §3.7 handshake
/// and returns the segment fd, since splitting "is anyone there?" from "attach"
/// would connect twice and re-run the race.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach<T> {
    /// The handshake succeeded.
    ///
    /// The granted slot rides in the variant, not a separate accessor: reporting
    /// success without naming a slot would let the two disagree.
    Serving {
        /// What the probe obtained; for the real one, the attachment.
        attached: T,
        /// The participant slot the owner granted, which is also the lock-file
        /// byte this client must take.
        slot: u32,
    },
    /// `ECONNREFUSED`, no socket at all, or a server that died mid-handshake.
    ///
    /// **Not an error.** §3.9 makes a stale socket path expected and the ownership
    /// byte is the real discriminator, so all three collapse to one verdict.
    Absent,
    /// The owner answered, and refused.
    ///
    /// Terminal, unlike [`Reach::Absent`]: §3.4's loop has no exit for a rejection,
    /// so a `LayoutMismatch` would burn the deadline and surface as
    /// [`IpcError::ArenaHeldButUnreachable`] — the debugging session §3.7's
    /// message exists to prevent.
    Rejected(IpcError),
}

/// Step 1 of the algorithm, injected.
///
/// The real one connects a `SOCK_SEQPACKET` and performs the §3.7 handshake. A
/// trait keeps every §3.4 branch testable, including the split-brain race — a
/// *live* but unreachable arena — which otherwise needs a won timing window.
pub trait ServerProbe {
    /// What a successful probe yields — for the real one, the attachment.
    type Attached;

    /// Try to reach a server bound at `sock`.
    ///
    /// # Errors
    ///
    /// Only for what is neither "nobody is listening" nor "the owner refused":
    /// those are [`Reach::Absent`] and [`Reach::Rejected`].
    fn probe(&mut self, sock: &Path) -> Result<Reach<Self::Attached>, IpcError>;
}

/// A probe that always reports nothing listening.
///
/// The state of the world after an owner dies, and during the split-brain race.
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

/// First backoff interval for *this* crate's handshake retry. Doubles up to
/// [`MAX_BACKOFF`].
///
/// **Private, deliberately.** A `docs/decisions/0019` §2b branch made this pair
/// `pub` for the facade to share: permanent public API for two numbers, and no
/// guarantee — `Open::await_open` retries whole rendezvous *attempts*, each
/// running this loop, so the two are free to disagree.
const MIN_BACKOFF: Duration = Duration::from_micros(200);
/// Backoff ceiling — small enough that a 5 s timeout still gives hundreds of
/// attempts, so a takeover that completes in a millisecond is joined promptly.
const MAX_BACKOFF: Duration = Duration::from_millis(4);

/// The participant slot a creator takes.
///
/// Named, not a bare `0`: it is the *same integer* as the arena record
/// `TreeBuilder::build_shared` gives the creator, which the facade indexes with
/// it. Two spellings is how they drifted (#201).
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
    /// [`IpcError::ArenaHeldButUnreachable`] on timeout (a live arena nothing
    /// serves); [`IpcError::ArenaAbsent`] under [`CreatePolicy::Never`];
    /// [`IpcError::NoParticipantSlots`] when the table is full; lock or directory
    /// failures.
    pub fn open<P: ServerProbe>(&self, probe: &mut P) -> Result<Session<P::Attached>, IpcError> {
        self.rendezvous.ensure_dir()?;
        let lock = LockFile::open(self.rendezvous.lock_path())?;
        let identity = Identity::of_self_best_effort(self.mode);

        let start = Instant::now();
        let mut backoff = MIN_BACKOFF;
        loop {
            // 1. Someone is already serving. Join.
            match probe.probe(self.rendezvous.sock_path())? {
                // Terminal: retrying cannot change a version or layout
                // disagreement, and would trade a precise message for a timeout.
                Reach::Rejected(why) => return Err(why),
                Reach::Serving { attached, slot } => {
                    // `None`: the named byte is held by somebody the owner has
                    // not noticed leaving. Drop the attachment and loop — it
                    // re-probes and names another, and nothing reached the arena.
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
                // **Step 3 is gone, and its absence is the decision.** It
                // short-circuited past step 4 for a process declaring it already
                // held the arena, which no new file description can verify:
                // `F_OFD_GETLK` reports conflicts and cannot name a holder.
                // **Do not re-add it** — #201 / `docs/decisions/0037` list the
                // five unsound states it cost.
                // 4. SPLIT-BRAIN CHECK. A held participant byte means a live
                //    arena whose holder has not taken over yet, so yield.
                if self.create != CreatePolicy::Always && lock.any_participant_held()? {
                    lock.release_ownership()?;
                } else if self.create == CreatePolicy::Never {
                    // Nothing serving and nothing alive, so there is no arena to
                    // join. A timeout cannot change that answer.
                    lock.release_ownership()?;
                    return Err(IpcError::ArenaAbsent);
                } else if let Some(slot) = self.register_creator(&lock, &identity)? {
                    // 5. Serve. The caller owes: memfd create + seal (§3.6),
                    //    unlink stale sock, bind, listen.
                    return Ok(Session {
                        outcome: OpenOutcome::Created,
                        lock,
                        slot,
                        owner: true,
                        attached: None,
                    });
                } else {
                    // The creator's byte was taken between step 4's scan and step
                    // 5's acquire — step 4's condition arriving late, so take
                    // step 4's branch. Nothing was built to unwind.
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

    /// Take **the creator's slot** (byte 0), or report somebody else holds it.
    ///
    /// **Lock, then write** — deliberately not §3.3's "identity written with
    /// `pwrite` before taking the slot lock", which presumes a slot the owner
    /// named. A joiner picking its own loses that race: two see slot 3 free, both
    /// write, one locks, and the record names the *loser*, pointing
    /// [`IpcError::ArenaHeldButUnreachable`] at the wrong pid. Reversed, a slot is
    /// briefly held with a stale record, which is harmless: the record is advisory
    /// (§5.1), the lock is the liveness. Creator-only — joiners use
    /// [`Open::register_at`] and a taker-over registers nothing (`0028`
    /// question 3, `docs/decisions/0037`).
    ///
    /// **Not a scan for the first free byte.** A creator is the *first*
    /// participant (step 4 refuses to create while any participant byte is held),
    /// so its slot is `0`, matching the arena's first `FREE` record that the
    /// facade indexes with the same integer. A scan (#201's deleted
    /// `register_any`) is a *separate* pass from step 4's: `any_participant_held`
    /// probes byte 0 then 63 more, leaving 63 `F_OFD_GETLK` calls in which byte 0
    /// can be taken. Measured, a second description toggling byte 0 over 4000
    /// iterations of exactly those two calls: **2242 took a non-zero byte** while
    /// the arena would have registered record 0 — reachable from outside this
    /// workspace anyway, `LockFile::try_take_participant` being published API. One
    /// `F_OFD_SETLK` makes check and take kernel-atomic, so divergence is
    /// unrepresentable rather than detected (the facade's
    /// `ParticipantSlotDiverged` guard stays as an assertion this cannot trip).
    ///
    /// `Ok(None)` is step 4's condition arriving here, so the caller takes step
    /// 4's branch — under [`CreatePolicy::Always`] too, where it means a *live*
    /// holder on byte 0, owner or not
    /// (`defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`), and
    /// forcing past that is the split brain `--force-new` must resolve, not cause.
    /// Byte 0 is free in exactly the case the hatch is for (`0035`) — not, as once
    /// claimed here, because a wedge's participants are dead: a wedge *requires* a
    /// live holder, or step 4 would not fire and `IfAbsent` would already create.
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

    /// Take the slot the owner named, in §3.3's specified order.
    ///
    /// Identity first, then the byte — safe here and not in the deleted
    /// `register_any`, because the owner hands each client a different byte, so
    /// nobody races us and a reader never sees a held byte nameless.
    ///
    /// `None` if the byte is already held: the owner named a slot whose holder it
    /// has not seen leave. The caller retries the handshake rather than falling
    /// back to another slot, which would re-open the byte/record split.
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
    /// Every caller reaching here has released the ownership byte again, so the
    /// probe reports somebody *else* — the bit that says whether
    /// [`CreatePolicy::Always`] could help, since it must take that byte before
    /// the participant bytes it may skip. A reading at the deadline, not a claim
    /// about the whole timeout.
    fn held_but_unreachable(&self, lock: &LockFile) -> Result<IpcError, IpcError> {
        let holder_slots = lock.held_participants()?;
        // `trailing_zeros()` is 64 on an empty mask, which is no arena's slot.
        // `Display` special-cased that, but the field is public and a supervisor
        // reading it would log a fictional slot.
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
/// true.
///
/// Dropping it closes the lock file, releasing both bytes by the mechanism the
/// kernel uses on process death. No separate teardown to forget.
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
    /// [`OpenOutcome::Joined`], create-and-bind for [`OpenOutcome::Created`].
    #[must_use]
    pub fn outcome(&self) -> OpenOutcome {
        self.outcome
    }

    /// This process's participant slot. Its lock byte is held for the `Session`'s
    /// lifetime, on this session's own file description.
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
    /// another participant take over, and lookups are unaffected because
    /// ownership is control plane only.
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
    /// The other half of [`Self::release_ownership`] and the answer to [`0037`]'s
    /// question 5: without it the rendezvous stayed ownerless until every
    /// participant left.
    ///
    /// **A method, not another `open()`**, because a takeover rests on "I already
    /// hold the arena at slot *n*" and no new file description can verify that:
    /// from the fresh [`LockFile`] `Open::open` builds, `F_OFD_GETLK` answers
    /// *does anyone **else** hold this byte*, so this caller and a live **peer**
    /// holding byte *n* are indistinguishable, and believing the declaration hands
    /// out a session naming the peer's slot. Two rounds of repair against that
    /// shape executed five unsound states, four introduced while fixing the one
    /// before ([`0037`]). Locking **the description this session already holds**
    /// leaves nothing to verify — `0028` question 3's property, made structural.
    ///
    /// Owner is not *server*: the caller must still bind the socket and serve its
    /// existing segment (`tf_tree::Tree::inherit_ownership` has arena, fd and
    /// rendezvous in scope). With racing survivors ([`0037`]'s question 2) one
    /// gets the byte and the other `Ok(false)` **with its slot intact** — not an
    /// error, just somebody else mid-bind. Already owning is a no-op `Ok(true)`.
    ///
    /// [`0037`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0037-a-takeover-is-not-a-second-open.md
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
    /// The kernel's answer, from `F_OFD_GETLK` on this session's own description —
    /// the second half of what `tf_tree::Tree::owner_lost` asks, a hung-up attach
    /// socket saying only *this process's channel is dead* ([`0043`]).
    ///
    /// **A survivor never sees its own byte:** [`crate::LockProbe::held`] reports
    /// *conflicting* locks, so an owner asking gets `false` about a byte it holds.
    /// The facade therefore calls this only from the `Joined` arm; for "am I the
    /// owner" use [`Self::take_over_ownership`]'s return. Not the participant
    /// table, whose pids and start times need the `/proc` heuristic [`0033`] shows
    /// cannot name a namespace — the byte is the liveness authority everywhere
    /// else here (§5.1, §6.1, [`0029`]).
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

    /// Take what the §3.7 handshake yielded, if this session joined one.
    ///
    /// `None` for [`OpenOutcome::Created`], which never ran a handshake. Taken,
    /// not borrowed: the payload owns the segment fd and the connection whose
    /// closure tells the owner this participant is gone (D17), and that liveness
    /// signal must not be tied to a struct the caller has no reason to keep.
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
        // Tests that pre-seed a lock (a survivor, a process mid-bind) open the
        // lock file before `Open::open` would have created the directory.
        rv.ensure_dir().unwrap();
        (rv, dir)
    }

    struct EnvOverride(std::path::PathBuf);

    impl crate::runtime_dir::EnvLookup for EnvOverride {
        fn var(&self, key: &str) -> Option<std::ffi::OsString> {
            (key == "TF_TREE_RUNTIME_DIR").then(|| self.0.clone().into_os_string())
        }
    }

    /// Reports "serving" only after `n` calls, so a test can make the owner
    /// appear mid-loop. Grants slot `1`; the creator here holds slot `0`.
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
        // §3.4 step 4: a survivor holds a participant byte and nothing serves,
        // so `open()` must NOT create.
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

        // Ownership was yielded back, so the survivor can take over at once.
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
        // It must not have left byte 0 held on the way out.
        let taker = LockFile::open(rv.lock_path()).unwrap();
        assert_eq!(taker.try_take_ownership().unwrap(), LockAttempt::Acquired);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// **The escape hatch is this policy, not a flag** — `--force-new`, which this
    /// test was once named after, has never existed (#189). It covers the branch
    /// only; the loudness §3.4 asks for belongs to whoever sets the policy. The
    /// survivor keeps byte 1, so the created session must take another: one lock
    /// file serves both arenas, and the byte is the kernel's.
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

    /// **The escape hatch abandons an *unreachable* arena, never a served one.**
    ///
    /// Step 1 runs before the create decision, so a server that answers is joined
    /// whatever the policy says — [`CreatePolicy::Always`]'s claimed bound on the
    /// damage, tested rather than argued.
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
        // Mid-bind: byte 0 held, nothing serving yet. The opener must back off
        // and join once the socket appears.
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
        // The participant byte is still held: the role is not the attachment.
        assert!(heir.probe_participant(s.slot()).unwrap().held);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `Session::ownership_held` reports the *kernel's* answer, live
    /// ([`0043`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0043-owner-lost-is-a-question-about-the-owner.md)).
    ///
    /// **Mutant, run:** body → `Ok(!self.owner)` fails on the **first** assertion,
    /// not the later one the note first claimed — a session that released the role
    /// is already `!self.owner`. The third assertion matches no one-line mutant
    /// but guards a `Cell<bool>` latched on the first `true` (the change made to
    /// save the `fcntl`): `Tree::owner_lost` needs a loser to stop answering
    /// `true` after a takeover **and start again** if the new owner dies.
    #[test]
    fn ownership_held_tracks_the_byte_and_not_a_latch() {
        let (rv, dir) = rendezvous("ownprobe");
        let mut s = Open::new(rv.clone()).open(&mut NoServer).unwrap();
        // Give up the role but keep the session — this is the survivor's shape.
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

    /// An owner asking about its own byte is told `false` — the kernel's rule, not
    /// this method's, since `F_OFD_GETLK` reports only *conflicting* locks. This
    /// test stops a later reader deleting `Tree::owner_lost`'s scoping to the
    /// `Joined` arm as unnecessary.
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
