//! The lock file — `docs/PHASE2.md` §3.3.
//!
//! A small regular file that holds no state, only locks. **The design principle
//! is to not implement leader election but to borrow the kernel's.** A
//! rendezvous needs exactly three properties: mutual exclusion, automatic
//! release when the holder dies, and a way to ask whether anyone holds it. Linux
//! byte-range OFD locks provide all three, maintained by the kernel, with no
//! timeouts, no heartbeats, and no state that can survive a `SIGKILL`.
//!
//! | Offset | Meaning |
//! |---|---|
//! | byte 0 | **Ownership.** Exclusive. The holder serves the socket. |
//! | byte 1 | **Topology mutation** (A2). Exclusive, held for one `Tree::reparent`. |
//! | bytes 2–15 | reserved |
//! | bytes 16 + *i* | **Participant liveness** for slot *i*, held for the lifetime of the attachment. |
//! | 4096 + 64·*i* | **Identity record** for slot *i*, written with `pwrite`. Advisory. |
//!
//! Two properties of this arrangement are easy to get wrong and are asserted by
//! tests rather than assumed:
//!
//! * **`F_OFD_GETLK` cannot name a holder.** An OFD lock belongs to an open file
//!   description, not a process, so the kernel reports `l_pid = -1`. The lock
//!   file answers *"is anyone alive?"* — all the rendezvous needs — and *"who?"*
//!   comes from the identity records. That is exactly why those records exist as
//!   plain `pwrite` data instead of living only in the arena: a process that
//!   cannot reach the arena can still run `doctor` and get names and pids.
//! * **A description's own locks are invisible to its own `GETLK`.** The kernel
//!   reports *conflicts*, and nothing conflicts with itself. Every query here is
//!   therefore "does anyone **else** hold this", which is what the §3.4
//!   split-brain check wants, and a trap for any future code that tries to read
//!   back its own state.

use std::fs::{File, OpenOptions};
use std::os::fd::AsFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

use crate::error::{IpcError, LockRole};
use crate::identity::{Identity, IDENTITY_RECORD_LEN};
use crate::ofd::{self, LockAttempt, LockKind, Range};

pub use crate::ofd::LockProbe;

/// Participant slots, and therefore participant lock bytes.
///
/// Matches `tf_tree_arena::DEFAULT_MAX_PARTICIPANTS`, which is the arena-side
/// table this indexes. The two must agree; they are separate constants only
/// because `docs/PHASE2.md` §2 forbids this crate from depending on the arena.
pub const MAX_PARTICIPANTS: u32 = 64;

/// Byte 0: ownership.
const OWNERSHIP_OFFSET: u64 = 0;
/// Byte 1: A2's topology mutation lock
/// ([`docs/decisions/0029`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0029-the-topology-lock-is-a-kernel-lock.md)).
///
/// **One byte, not one per participant.** It is not asked *whether a participant
/// is alive* — the question a per-slot byte would answer — but *whether anyone
/// is inside the critical section*, which is the only question a mutator has to
/// settle before it may steal the arena's topology word. A per-slot byte would
/// re-create the slot indirection that put `/proc` on this path in the first
/// place, and buy nothing: `F_OFD_GETLK` cannot name a holder anyway (see the
/// module documentation), so the identity records are still what names one.
const TOPOLOGY_OFFSET: u64 = 1;
/// Participant liveness starts at byte 16, leaving 2–15 reserved.
const PARTICIPANT_BASE: u64 = 16;
/// Identity records start on the second page.
const IDENTITY_BASE: u64 = 4096;

/// Base offset reserved for §6.1 claim locks (`CLAIM_BASE + edge_id`).
///
/// Nothing takes these yet — claims land with the arena in a later pass — but
/// the region is reserved here so the offset arithmetic lives in one file. It
/// starts at 1 MiB, far past the identity records, because a collision between
/// a claim byte and a participant byte would hand one edge to two writers and
/// present as impossible numerical results rather than as an error.
pub const CLAIM_BASE: u64 = 1 << 20;

/// How many claim bytes the reserved region can address.
///
/// A whole mebibyte of byte-range locks, which is far more edges than
/// `ArenaLayout` will accept — the bound exists so a corrupt `max_edges` cannot
/// walk a lock request out of the region rather than because the space is
/// tight.
pub const MAX_CLAIM_BYTES: u64 = 1 << 20;

/// Handle on the lock file for one open file description.
///
/// **Ownership of the `File` is the lock's lifetime.** OFD locks are released
/// when the last descriptor referring to this description closes, which happens
/// on `Drop` and, identically, on process death by any means including
/// `SIGKILL`. There is no unlock path that can be missed, which is the entire
/// reason this is not a heartbeat protocol.
#[derive(Debug)]
pub struct LockFile {
    file: File,
}

impl LockFile {
    /// Open (creating if absent) the lock file at `path`.
    ///
    /// Mode `0600`: the rendezvous is same-user by construction (§3.10) and the
    /// containing directory is `0700`, so a wider mode would only be misleading.
    /// Opened read-write because `F_WRLCK` requires a descriptor open for
    /// writing.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFileOpen`] if the file cannot be opened or created.
    pub fn open(path: &Path) -> Result<LockFile, IpcError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|e| IpcError::LockFileOpen {
                raw_os_error: IpcError::os(&e),
            })?;
        Ok(LockFile { file })
    }

    /// Try to take byte 0 — become the owner.
    ///
    /// Returns [`LockAttempt::Contended`] when someone else holds it. That is
    /// not an error: in the §3.4 loop it means another process is mid-bind and
    /// will be serving shortly.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`] for any `fcntl` failure that is not contention.
    pub fn try_take_ownership(&self) -> Result<LockAttempt, IpcError> {
        self.set(
            Range::byte(OWNERSHIP_OFFSET),
            LockKind::Exclusive,
            LockRole::Ownership,
        )
    }

    /// Release byte 0 without closing the file, so a process can yield
    /// ownership (§3.4 step 4) and keep its participant slot.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`].
    pub fn release_ownership(&self) -> Result<(), IpcError> {
        self.set(
            Range::byte(OWNERSHIP_OFFSET),
            LockKind::Unlock,
            LockRole::Ownership,
        )
        .map(|_| ())
    }

    /// Try to take byte 1 — the right to mutate topology (`docs/PHASE2.md` §1,
    /// A2).
    ///
    /// **This is an acquire, and the difference from a probe is the whole
    /// point.** [`Self::probe_participant`] answers a question about somebody
    /// else's byte and races every subsequent take, which is why §5.1 constrains
    /// the order in which a probe may be composed with an arena word. Holding
    /// this byte *excludes* every subsequent take for as long as it is held, so
    /// what the holder reads afterwards cannot be invalidated by a taker: there
    /// cannot be one.
    ///
    /// What that buys the caller is stated as an invariant in `0029` and is the
    /// reason this exists: if a process holds this byte and then observes a
    /// non-zero topology word, the word's holder is either **dead** or **a
    /// writer with no lock file**. A live holder that `/proc` misreports —
    /// another PID namespace, a non-dumpable process under `hidepid` — is
    /// excluded by the kernel before any inference runs.
    ///
    /// Returns [`LockAttempt::Contended`] when another open file description
    /// holds it, which means a live peer is mid-mutation. Retry.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`] for any `fcntl` failure that is not contention.
    pub fn try_take_topology(&self) -> Result<LockAttempt, IpcError> {
        self.set(
            Range::byte(TOPOLOGY_OFFSET),
            LockKind::Exclusive,
            LockRole::Topology,
        )
    }

    /// Release byte 1.
    ///
    /// **Order matters, and it is the mirror of the acquire**: release the arena
    /// topology word *first*, then this byte. The reverse leaves a window in
    /// which the byte is free and the word still names this process, which is
    /// the exact signature `0029`'s T2 reads as "the holder is dead or has no
    /// lock file" — so a peer would spin out its budget and then consult
    /// `/proc` about a process that is merely finishing.
    ///
    /// # Errors
    ///
    /// As [`Self::try_take_topology`].
    pub fn release_topology(&self) -> Result<(), IpcError> {
        self.set(
            Range::byte(TOPOLOGY_OFFSET),
            LockKind::Unlock,
            LockRole::Topology,
        )
        .map(|_| ())
    }

    /// Try to take the liveness byte for `slot`.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`], or [`IpcError::NoParticipantSlots`] if `slot`
    /// is out of range.
    pub fn try_take_participant(&self, slot: u32) -> Result<LockAttempt, IpcError> {
        self.set(
            participant_range(slot)?,
            LockKind::Exclusive,
            LockRole::Participant(slot),
        )
    }

    /// Release the liveness byte for `slot`.
    ///
    /// # Errors
    ///
    /// As [`LockFile::try_take_participant`].
    pub fn release_participant(&self, slot: u32) -> Result<(), IpcError> {
        self.set(
            participant_range(slot)?,
            LockKind::Unlock,
            LockRole::Participant(slot),
        )
        .map(|_| ())
    }

    /// Take the lowest free participant slot.
    ///
    /// **Not the joiner path.** §3.7 landed: the *owner* assigns a joiner's slot
    /// in its accept loop and returns it in the `HelloResponse`, and
    /// `Open::register_at` takes exactly that byte. This scan is what a
    /// **creator or a taker-over** uses — neither has an owner to ask — which is
    /// also why `Open::register_any` locks before it writes the identity record.
    ///
    /// # Errors
    ///
    /// [`IpcError::NoParticipantSlots`] when every slot is live.
    pub fn take_any_participant(&self) -> Result<u32, IpcError> {
        for slot in 0..MAX_PARTICIPANTS {
            if self.try_take_participant(slot)? == LockAttempt::Acquired {
                return Ok(slot);
            }
        }
        Err(IpcError::NoParticipantSlots {
            limit: MAX_PARTICIPANTS,
        })
    }

    /// Query byte 0.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`].
    pub fn probe_ownership(&self) -> Result<LockProbe, IpcError> {
        self.probe(Range::byte(OWNERSHIP_OFFSET), LockRole::Ownership)
    }

    /// Query one participant byte.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`] or [`IpcError::NoParticipantSlots`] for an
    /// out-of-range slot.
    pub fn probe_participant(&self, slot: u32) -> Result<LockProbe, IpcError> {
        self.probe(participant_range(slot)?, LockRole::Participant(slot))
    }

    /// Take the lease on `edge`'s claim byte (`docs/PHASE2.md` §6.1).
    ///
    /// **The lease is not the claim.** `docs/decisions/0005` §5 makes the
    /// arena's `ClaimRecord` CAS the decision and this the thing that makes
    /// death *observable*: a process that dies for any reason has its byte
    /// released by the kernel, with no cooperation and no timeout, which is
    /// the predicate §6.3's reaper needs.
    ///
    /// §6.1's literal wording — "the lock file is authoritative … any code
    /// that makes a decision from `ClaimRecord` alone is a bug" — is not
    /// implementable: the lock file and the arena are two files with no atomic
    /// cross-update, so exactly one has to be the linearization point, and the
    /// record also has to keep working for a `HeapArena` that has no lock file
    /// at all.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`], or [`IpcError::ClaimOutOfRange`] if `edge`
    /// exceeds what the reserved byte range can address.
    pub fn try_take_claim(&self, edge: u32) -> Result<LockAttempt, IpcError> {
        self.set(
            claim_range(edge)?,
            LockKind::Exclusive,
            LockRole::Claim(edge),
        )
    }

    /// Drop the lease on `edge`'s claim byte.
    ///
    /// **Order matters**: clear the arena record *first*, then unlock. The
    /// reverse leaves a window in which the record says held and the byte says
    /// free — which is exactly the signature a reaper treats as "the holder is
    /// dead" (`0005` §5).
    ///
    /// # Errors
    ///
    /// As [`Self::try_take_claim`].
    pub fn release_claim(&self, edge: u32) -> Result<(), IpcError> {
        self.set(claim_range(edge)?, LockKind::Unlock, LockRole::Claim(edge))
            .map(|_| ())
    }

    /// Whether `edge`'s claim byte is held, and by whom.
    ///
    /// Subject to the same self-blindness as every other `F_OFD_GETLK` here: a
    /// description does not see its own locks, so a process asking about an
    /// edge *it* holds is told the byte is free. Callers must skip their own
    /// edges — see `a_holder_does_not_see_its_own_lock`.
    ///
    /// # Errors
    ///
    /// As [`Self::try_take_claim`].
    pub fn probe_claim(&self, edge: u32) -> Result<LockProbe, IpcError> {
        self.probe(claim_range(edge)?, LockRole::Claim(edge))
    }

    /// Bitmask of participant slots held by *other* open file descriptions.
    ///
    /// This is the §3.4 step 4 question. It is deliberately a full scan rather
    /// than an early exit: the caller that fails the check needs the whole set
    /// to name the stuck slots in [`IpcError::ArenaHeldButUnreachable`], and 64
    /// `fcntl` calls on a cold path are free.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`].
    pub fn held_participants(&self) -> Result<u64, IpcError> {
        let mut mask = 0u64;
        for slot in 0..MAX_PARTICIPANTS {
            if self.probe_participant(slot)?.held {
                mask |= 1u64 << slot;
            }
        }
        Ok(mask)
    }

    /// Whether any participant byte is held — the split-brain predicate.
    ///
    /// Early-exits, because the §3.4 loop asks this on every iteration and only
    /// needs a yes/no.
    ///
    /// # Errors
    ///
    /// [`IpcError::LockFailed`].
    pub fn any_participant_held(&self) -> Result<bool, IpcError> {
        for slot in 0..MAX_PARTICIPANTS {
            if self.probe_participant(slot)?.held {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Write the identity record for `slot`.
    ///
    /// Written *before* the slot's lock is taken (§3.3), so that any process
    /// which observes the lock can also read a fully-formed record for it. The
    /// record is advisory (§5.1) — the lock is the liveness — but "advisory"
    /// must not mean "sometimes absent when a lock is held".
    ///
    /// # Errors
    ///
    /// [`IpcError::IdentityIo`] on a short or failed `pwrite`.
    pub fn write_identity(&self, slot: u32, id: &Identity) -> Result<(), IpcError> {
        let offset = identity_offset(slot)?;
        let bytes = id.to_bytes();
        let n = self
            .file
            .write_at(&bytes, offset)
            .map_err(|e| IpcError::IdentityIo {
                slot,
                raw_os_error: IpcError::os(&e),
            })?;
        if n != bytes.len() {
            return Err(IpcError::IdentityIo {
                slot,
                raw_os_error: 0,
            });
        }
        Ok(())
    }

    /// Read the identity record for `slot`, or `None` if it was never written.
    ///
    /// # Errors
    ///
    /// [`IpcError::IdentityIo`] if the read fails for a reason other than the
    /// file being short.
    pub fn read_identity(&self, slot: u32) -> Result<Option<Identity>, IpcError> {
        let offset = identity_offset(slot)?;
        let mut buf = [0u8; IDENTITY_RECORD_LEN];
        let n = self
            .file
            .read_at(&mut buf, offset)
            .map_err(|e| IpcError::IdentityIo {
                slot,
                raw_os_error: IpcError::os(&e),
            })?;
        if n != buf.len() {
            // Short read means the file has never been grown to this record, so
            // nobody has ever taken the slot.
            return Ok(None);
        }
        Ok(Identity::from_bytes(&buf))
    }

    // `as_file` removed: it had no caller anywhere in the workspace, and the
    // consumer its doc named — "code that needs to prove two `LockFile`s are
    // distinct open file descriptions" — does not exist; the test for that
    // property opens two `LockFile`s and contends the lock bytes instead.
    // Handing out `&File` also widened this type's contract, because it let a
    // caller `set_len` or `try_clone` the rendezvous file from outside the
    // module that owns the lock lifetime.

    fn set(&self, range: Range, kind: LockKind, role: LockRole) -> Result<LockAttempt, IpcError> {
        ofd::try_lock(self.file.as_fd(), range, kind)
            .map_err(|errno| IpcError::LockFailed { role, errno })
    }

    fn probe(&self, range: Range, role: LockRole) -> Result<LockProbe, IpcError> {
        ofd::probe(self.file.as_fd(), range).map_err(|errno| IpcError::LockFailed { role, errno })
    }
}

/// The lock byte for `slot`.
fn participant_range(slot: u32) -> Result<Range, IpcError> {
    if slot >= MAX_PARTICIPANTS {
        return Err(IpcError::NoParticipantSlots {
            limit: MAX_PARTICIPANTS,
        });
    }
    Ok(Range::byte(PARTICIPANT_BASE + u64::from(slot)))
}

/// The claim-lease byte for `edge`.
///
/// Bounded so an edge id from a corrupt header cannot address a byte outside
/// the reserved region and collide with an identity record — which would hand
/// one edge to two writers and present as impossible numbers rather than as an
/// error.
fn claim_range(edge: u32) -> Result<Range, IpcError> {
    if u64::from(edge) >= MAX_CLAIM_BYTES {
        return Err(IpcError::ClaimOutOfRange {
            edge,
            limit: MAX_CLAIM_BYTES,
        });
    }
    Ok(Range::byte(CLAIM_BASE + u64::from(edge)))
}

/// The identity record offset for `slot`.
fn identity_offset(slot: u32) -> Result<u64, IpcError> {
    if slot >= MAX_PARTICIPANTS {
        return Err(IpcError::NoParticipantSlots {
            limit: MAX_PARTICIPANTS,
        });
    }
    Ok(IDENTITY_BASE + u64::from(slot) * IDENTITY_RECORD_LEN as u64)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::identity::AccessMode;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tf_tree_ipc_lock-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("test.lock")
    }

    #[test]
    fn the_byte_layout_is_the_one_the_spec_tabulates() {
        assert_eq!(OWNERSHIP_OFFSET, 0);
        assert_eq!(TOPOLOGY_OFFSET, 1);
        assert_eq!(participant_range(0).unwrap(), Range::byte(16));
        assert_eq!(participant_range(63).unwrap(), Range::byte(79));
        assert_eq!(identity_offset(0).unwrap(), 4096);
        assert_eq!(identity_offset(1).unwrap(), 4096 + 64);
        assert_eq!(identity_offset(63).unwrap(), 4096 + 64 * 63);
        // Participant bytes must not reach into the identity page, and claims
        // must start past every identity record.
        assert!(PARTICIPANT_BASE + u64::from(MAX_PARTICIPANTS) <= IDENTITY_BASE);
        assert!(CLAIM_BASE > identity_offset(MAX_PARTICIPANTS - 1).unwrap());
        assert!(participant_range(MAX_PARTICIPANTS).is_err());
        assert!(identity_offset(MAX_PARTICIPANTS).is_err());
        // **The topology byte is disjoint from every other role.** It is one
        // byte in a region the spec calls reserved, so the only thing standing
        // between it and a participant byte is arithmetic — and a collision here
        // would let one process hold the topology lock and another believe it
        // holds a participant slot, which is the "same integer, two meanings"
        // failure `0035` is about, one region over.
        assert_ne!(TOPOLOGY_OFFSET, OWNERSHIP_OFFSET);
        const { assert!(TOPOLOGY_OFFSET < PARTICIPANT_BASE) };
        for slot in [0, 1, MAX_PARTICIPANTS - 1] {
            assert_ne!(
                participant_range(slot).unwrap(),
                Range::byte(TOPOLOGY_OFFSET)
            );
        }
        assert_ne!(claim_range(0).unwrap(), Range::byte(TOPOLOGY_OFFSET));
    }

    #[test]
    fn two_descriptions_contend_for_the_topology_byte_and_a_release_hands_it_over() {
        // A2's exclusion, as a kernel fact rather than an inference
        // (`docs/decisions/0029`). Two descriptions stand in for two mutators;
        // `two_descriptions_in_one_process_still_conflict` is why that is a
        // faithful stand-in rather than a convenience.
        let path = scratch("topo-byte");
        let a = LockFile::open(&path).unwrap();
        let b = LockFile::open(&path).unwrap();

        assert_eq!(a.try_take_topology().unwrap(), LockAttempt::Acquired);
        assert_eq!(b.try_take_topology().unwrap(), LockAttempt::Contended);

        // Holding topology must not imply holding anything else. If the offsets
        // ever collided this is the assertion that catches it, because the
        // *symptom* would be a peer refused a slot it is entitled to rather than
        // anything that looks like a lock bug.
        assert_eq!(b.try_take_ownership().unwrap(), LockAttempt::Acquired);
        assert_eq!(b.try_take_participant(0).unwrap(), LockAttempt::Acquired);
        assert_eq!(b.try_take_claim(0).unwrap(), LockAttempt::Acquired);

        a.release_topology().unwrap();
        assert_eq!(b.try_take_topology().unwrap(), LockAttempt::Acquired);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn closing_a_description_releases_the_topology_byte() {
        // The property the whole design rests on, asserted for this byte
        // specifically rather than inherited from
        // `dropping_the_file_releases_every_lock`: a mutator killed inside A2's
        // critical section must not wedge the tree, and nothing in `tf_tree`
        // runs on its behalf to unlock.
        let path = scratch("topo-death");
        let survivor = LockFile::open(&path).unwrap();
        {
            let corpse = LockFile::open(&path).unwrap();
            assert_eq!(corpse.try_take_topology().unwrap(), LockAttempt::Acquired);
            assert_eq!(
                survivor.try_take_topology().unwrap(),
                LockAttempt::Contended
            );
        }
        assert_eq!(survivor.try_take_topology().unwrap(), LockAttempt::Acquired);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn two_descriptions_in_one_process_still_conflict() {
        // The property that makes OFD locks usable in a library: unlike classic
        // POSIX locks, which are per-process and would let this succeed twice,
        // two separate `open`s conflict even inside one process.
        let path = scratch("two-fds");
        let a = LockFile::open(&path).unwrap();
        let b = LockFile::open(&path).unwrap();
        assert_eq!(a.try_take_ownership().unwrap(), LockAttempt::Acquired);
        assert_eq!(b.try_take_ownership().unwrap(), LockAttempt::Contended);
        a.release_ownership().unwrap();
        assert_eq!(b.try_take_ownership().unwrap(), LockAttempt::Acquired);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_holder_does_not_see_its_own_lock() {
        // Documented trap: GETLK reports conflicts, and nothing conflicts with
        // itself. Any future "read back my own state" code is wrong.
        let path = scratch("self-blind");
        let a = LockFile::open(&path).unwrap();
        assert_eq!(a.try_take_participant(3).unwrap(), LockAttempt::Acquired);
        assert!(!a.probe_participant(3).unwrap().held);
        let b = LockFile::open(&path).unwrap();
        assert!(b.probe_participant(3).unwrap().held);
        assert_eq!(b.held_participants().unwrap(), 1 << 3);
        assert!(b.any_participant_held().unwrap());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn dropping_the_file_releases_every_lock() {
        let path = scratch("drop");
        let observer = LockFile::open(&path).unwrap();
        {
            let a = LockFile::open(&path).unwrap();
            a.try_take_ownership().unwrap();
            a.try_take_participant(0).unwrap();
            assert!(observer.probe_ownership().unwrap().held);
        }
        assert!(!observer.probe_ownership().unwrap().held);
        assert_eq!(observer.held_participants().unwrap(), 0);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn slots_are_handed_out_lowest_first() {
        let path = scratch("slots");
        let a = LockFile::open(&path).unwrap();
        let b = LockFile::open(&path).unwrap();
        let c = LockFile::open(&path).unwrap();
        assert_eq!(a.take_any_participant().unwrap(), 0);
        assert_eq!(b.take_any_participant().unwrap(), 1);
        drop(a);
        // Slot 0 is free again the instant its description closed.
        assert_eq!(c.take_any_participant().unwrap(), 0);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// **`docs/PHASE2.md` §11.2 scenario 6**: the 65th participant is refused,
    /// and the message says how to raise the limit.
    ///
    /// The exhaustion arm of [`LockFile::take_any_participant`] is otherwise
    /// unreachable from any test in the workspace — every other one takes at
    /// most three slots — so `Err(NoParticipantSlots)` could be replaced by
    /// `Ok(0)` and every suite would stay green while two participants shared
    /// slot 0, one arena record and one lock byte, with nothing reporting it.
    #[test]
    fn the_sixty_fifth_participant_is_refused_and_told_why() {
        let path = scratch("full");
        let mut holders: Vec<LockFile> = Vec::new();
        for expect in 0..MAX_PARTICIPANTS {
            let lf = LockFile::open(&path).unwrap();
            assert_eq!(lf.take_any_participant().unwrap(), expect);
            holders.push(lf);
        }

        let extra = LockFile::open(&path).unwrap();
        let err = extra.take_any_participant().unwrap_err();
        assert_eq!(
            err,
            IpcError::NoParticipantSlots {
                limit: MAX_PARTICIPANTS
            }
        );
        // §11.2 asks for the *message* too: "all slots are live" on its own
        // sends an operator hunting a leak that does not exist.
        let msg = err.to_string();
        assert!(msg.contains("64"), "{msg}");
        assert!(msg.contains("MAX_PARTICIPANTS"), "{msg}");

        // The limit is a concurrency bound, not a one-way quota: one departure
        // frees exactly one slot, and it is the slot that departed.
        holders.pop();
        assert_eq!(
            extra.take_any_participant().unwrap(),
            MAX_PARTICIPANTS - 1,
            "a released slot must become takeable again"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn identity_records_round_trip_at_the_specified_offsets() {
        let path = scratch("identity");
        let lf = LockFile::open(&path).unwrap();
        assert_eq!(lf.read_identity(5).unwrap(), None, "never written");
        let id = Identity {
            pid: 4242,
            start_time: 987_654,
            boot_id: [7u8; 16],
            mode: AccessMode::ReadWrite,
            name: {
                let mut n = [0u8; 16];
                n[..4].copy_from_slice(b"node");
                n
            },
            pid_ns_inode: 4_026_531_836,
        };
        lf.write_identity(5, &id).unwrap();
        assert_eq!(lf.read_identity(5).unwrap(), Some(id));
        // Neighbouring records are untouched: the stride is 64, not "whatever
        // the struct happens to be".
        assert_eq!(lf.read_identity(4).unwrap(), None);
        assert_eq!(lf.read_identity(6).unwrap(), None);
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, 4096 + 64 * 6);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
