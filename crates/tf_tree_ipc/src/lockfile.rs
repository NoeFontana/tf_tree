//! The lock file — `docs/PHASE2.md` §3.3.
//!
//! A regular file holding no state, only kernel-maintained OFD byte-range locks.
//!
//! | Offset | Meaning |
//! |---|---|
//! | byte 0 | **Ownership.** Exclusive. The holder serves the socket. |
//! | byte 1 | **Topology mutation** (A2). Exclusive, held for one `Tree::reparent`. |
//! | bytes 2–15 | reserved |
//! | bytes 16 + *i* | **Participant liveness** for slot *i*, held for the lifetime of the attachment. |
//! | 4096 + 64·*i* | **Identity record** for slot *i*, written with `pwrite`. Advisory. |
//!
//! `F_OFD_GETLK` cannot name a holder, and a description's own locks are
//! invisible to its own `GETLK`: every query is "does anyone **else** hold this".

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
/// Must equal `tf_tree_arena::DEFAULT_MAX_PARTICIPANTS`; this crate may not
/// depend on the arena (`docs/PHASE2.md` §2).
pub const MAX_PARTICIPANTS: u32 = 64;

/// Byte 0: ownership.
const OWNERSHIP_OFFSET: u64 = 0;
/// Byte 1: A2's topology mutation lock
/// ([`docs/decisions/0029`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0029-the-topology-lock-is-a-kernel-lock.md)).
///
/// One byte, not one per participant: it guards a critical section.
const TOPOLOGY_OFFSET: u64 = 1;
/// Participant liveness starts at byte 16, leaving 2–15 reserved.
const PARTICIPANT_BASE: u64 = 16;
/// Identity records start on the second page.
const IDENTITY_BASE: u64 = 4096;

/// Base offset for §6.1 claim locks (`CLAIM_BASE + edge_id`), 1 MiB past the
/// identity records.
pub const CLAIM_BASE: u64 = 1 << 20;

/// How many claim bytes the reserved region can address.
pub const MAX_CLAIM_BYTES: u64 = 1 << 20;

/// Handle on the lock file for one open file description.
///
/// **Ownership of the `File` is the lock's lifetime**: locks release when the
/// last descriptor closes, including on `SIGKILL`.
#[derive(Debug)]
pub struct LockFile {
    file: File,
}

impl LockFile {
    /// Open (creating if absent) the lock file at `path`, mode `0600` (§3.10),
    /// read-write because `F_WRLCK` needs a writable descriptor.
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
    /// [`LockAttempt::Contended`] is not an error: another process holds byte 0
    /// (an owner mid-bind, or another `open()` in steps 2–4,
    /// [`0057`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md))
    /// and the §3.4 loop backs off.
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
    /// **An acquire, not a probe**: a non-zero topology word observed afterwards
    /// names a dead holder or one with no lock file (`0029`).
    /// [`LockAttempt::Contended`] means a live peer is mid-mutation; retry.
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
    /// Release the arena topology word *first*, then this byte (`0029`'s T2).
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
    /// No production caller: a participant's byte and arena record share one
    /// index (`docs/PHASE2.md` §5.1). **Do not use it to assign a slot.**
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
    /// The arena's `ClaimRecord` CAS decides the claim (`docs/decisions/0005`
    /// §5); this lease makes death observable to §6.3's reaper.
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
    /// Clear the arena record *first*, then unlock (`0005` §5).
    ///
    /// # Errors
    ///
    /// As [`Self::try_take_claim`].
    pub fn release_claim(&self, edge: u32) -> Result<(), IpcError> {
        self.set(claim_range(edge)?, LockKind::Unlock, LockRole::Claim(edge))
            .map(|_| ())
    }

    /// Whether `edge`'s claim byte is held by someone else; a holder does not see
    /// its own lock.
    ///
    /// # Errors
    ///
    /// As [`Self::try_take_claim`].
    pub fn probe_claim(&self, edge: u32) -> Result<LockProbe, IpcError> {
        self.probe(claim_range(edge)?, LockRole::Claim(edge))
    }

    /// Bitmask of participant slots held by *other* descriptions (§3.4 step 4).
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

    /// Whether any participant byte is held — the split-brain predicate;
    /// early-exits.
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

    /// Write the identity record for `slot`, before the slot's lock is taken (§3.3).
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
            // Short read: the slot was never taken.
            return Ok(None);
        }
        Ok(Identity::from_bytes(&buf))
    }

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

/// The claim-lease byte for `edge`, bounded against a corrupt id.
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
        // Participant bytes stay out of the identity page; claims start past it.
        assert!(PARTICIPANT_BASE + u64::from(MAX_PARTICIPANTS) <= IDENTITY_BASE);
        assert!(CLAIM_BASE > identity_offset(MAX_PARTICIPANTS - 1).unwrap());
        assert!(participant_range(MAX_PARTICIPANTS).is_err());
        assert!(identity_offset(MAX_PARTICIPANTS).is_err());
        // The topology byte is disjoint from every other role.
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
        // A2's exclusion (`0029`); see `two_descriptions_in_one_process_still_conflict`.
        let path = scratch("topo-byte");
        let a = LockFile::open(&path).unwrap();
        let b = LockFile::open(&path).unwrap();

        assert_eq!(a.try_take_topology().unwrap(), LockAttempt::Acquired);
        assert_eq!(b.try_take_topology().unwrap(), LockAttempt::Contended);

        // Holding topology implies holding nothing else.
        assert_eq!(b.try_take_ownership().unwrap(), LockAttempt::Acquired);
        assert_eq!(b.try_take_participant(0).unwrap(), LockAttempt::Acquired);
        assert_eq!(b.try_take_claim(0).unwrap(), LockAttempt::Acquired);

        a.release_topology().unwrap();
        assert_eq!(b.try_take_topology().unwrap(), LockAttempt::Acquired);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn closing_a_description_releases_the_topology_byte() {
        // A mutator killed inside A2's critical section must not wedge the tree.
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
        // Unlike POSIX locks, two `open`s conflict even inside one process.
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
        // The module doc's self-blindness.
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
        assert_eq!(c.take_any_participant().unwrap(), 0);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// `docs/PHASE2.md` §11.2 scenario 6: the 65th participant is refused with
    /// a message saying how to raise the limit.
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
        // §11.2 asks for the message too.
        let msg = err.to_string();
        assert!(msg.contains("64"), "{msg}");
        assert!(msg.contains("MAX_PARTICIPANTS"), "{msg}");

        // One departure frees exactly the slot that departed.
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
        // Neighbouring records are untouched: the stride is 64.
        assert_eq!(lf.read_identity(4).unwrap(), None);
        assert_eq!(lf.read_identity(6).unwrap(), None);
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, 4096 + 64 * 6);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
