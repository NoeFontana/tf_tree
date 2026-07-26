//! Identity records — 64 bytes per slot at `4096 + 64·i` of the lock file.
//!
//! These exist because of a property of OFD locks that cannot be worked around:
//! `F_OFD_GETLK` reports `l_pid = -1`, since a lock belongs to an open file
//! description rather than a process. The lock file therefore answers *"is
//! anyone alive?"* exactly, and cannot answer *"who?"* at all.
//!
//! So *who* is written down separately, as plain `pwrite` data at a fixed
//! offset. Keeping it in the lock file rather than only in the arena is the
//! point: a process that cannot map the arena — wrong layout hash, no socket, an
//! operator on a wedged robot running `tf_tree doctor` — can still open a
//! 4 KiB file and print the pids holding it.
//!
//! `docs/PHASE2.md` §5.1: **this is advisory.** Liveness is the lock. Any code
//! that decides whether a participant is alive by reading these bytes is a bug;
//! the record may lag, and after a crash it is stale by definition (nothing runs
//! to clear it — that is the whole reason the lock is authoritative).

use crate::error::{IpcError, ProcError};
use crate::procstat::{boot_id, self_comm, self_start_time};

/// Size of one identity record. The 64-byte stride is NORMATIVE (§3.3) and is
/// one cache line, so a record never straddles two.
pub const IDENTITY_RECORD_LEN: usize = 64;

/// How a participant mapped the arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AccessMode {
    /// `PROT_READ`. The consumer default (§8): the MMU makes corruption
    /// impossible rather than merely impolite.
    ReadOnly = 0,
    /// `PROT_READ | PROT_WRITE`. Required to publish or claim.
    ReadWrite = 1,
}

impl AccessMode {
    fn from_byte(b: u8) -> AccessMode {
        // Anything unrecognised reads as the *less* privileged mode: this field
        // is diagnostics, and over-reporting privilege in a `doctor` listing is
        // the more misleading direction.
        if b == 1 {
            AccessMode::ReadWrite
        } else {
            AccessMode::ReadOnly
        }
    }

    /// Strict decode, for a byte a *decision* will be made from.
    ///
    /// [`AccessMode::from_byte`]'s leniency is right for a `doctor` listing and
    /// wrong for the §3.7 handshake: silently downgrading a mode the peer named
    /// and we did not understand hands back a read-only mapping the client
    /// never asked for, and the failure then surfaces at its first write with
    /// nothing to connect it to the handshake. The wire wants `Malformed`.
    pub(crate) fn try_from_byte(b: u8) -> Option<AccessMode> {
        match b {
            0 => Some(AccessMode::ReadOnly),
            1 => Some(AccessMode::ReadWrite),
            _ => None,
        }
    }
}

/// Who holds a participant slot.
///
/// `(pid, start_time, boot_id)` is the identity triple. A bare pid is not an
/// identity: pids are recycled, and on an embedded system with a low `pid_max`
/// they recycle fast, so `pid 1841` on its own can name two different processes
/// within a minute. `start_time` (ticks since boot) separates them within one
/// boot; `boot_id` separates the boots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// Process id.
    pub pid: u32,
    /// `/proc/<pid>/stat` field 22 — ticks since boot. See
    /// [`crate::parse_start_time`] for the parsing trap.
    pub start_time: u64,
    /// The kernel's boot id, so identities are comparable across a reboot.
    pub boot_id: [u8; 16],
    /// How this participant mapped the arena.
    pub mode: AccessMode,
    /// `comm`, NUL-padded. Diagnostics only.
    pub name: [u8; 32],
}

impl Identity {
    /// This process's identity.
    ///
    /// # Errors
    ///
    /// [`IpcError::Proc`] if `/proc/self/stat` or the boot id cannot be read.
    /// The caller may reasonably choose to proceed without one — the record is
    /// advisory — but that has to be an explicit decision, so this does not
    /// paper over the failure itself.
    pub fn of_self(mode: AccessMode) -> Result<Identity, IpcError> {
        Ok(Identity {
            pid: std::process::id(),
            start_time: self_start_time().map_err(IpcError::from)?,
            boot_id: boot_id().map_err(IpcError::from)?,
            mode,
            name: self_comm(),
        })
    }

    /// This process's identity, with unreadable fields left zero.
    ///
    /// For the rendezvous path, where failing to `open()` because `/proc` is not
    /// mounted would be absurd: the record is advisory and the lock carries the
    /// liveness regardless.
    #[must_use]
    pub fn of_self_best_effort(mode: AccessMode) -> Identity {
        Identity {
            pid: std::process::id(),
            start_time: self_start_time().unwrap_or(0),
            boot_id: boot_id().unwrap_or([0u8; 16]),
            mode,
            name: self_comm(),
        }
    }

    /// The name field as a string, trimmed at the first NUL.
    #[must_use]
    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|b| *b == 0).unwrap_or(32);
        core::str::from_utf8(&self.name[..end]).unwrap_or("<non-utf8>")
    }

    /// Encode to the on-disk layout: little-endian, fixed offsets, 64 bytes.
    ///
    /// Hand-rolled rather than `bytemuck`-cast because the dependency budget for
    /// this crate is `rustix` alone (§2), and because a record written by one
    /// build and read by another must not depend on either one's struct padding.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; IDENTITY_RECORD_LEN] {
        let mut out = [0u8; IDENTITY_RECORD_LEN];
        out[0..4].copy_from_slice(&self.pid.to_le_bytes());
        out[4..12].copy_from_slice(&self.start_time.to_le_bytes());
        out[12..28].copy_from_slice(&self.boot_id);
        out[28] = self.mode as u8;
        // 29..32 padding
        out[32..64].copy_from_slice(&self.name);
        out
    }

    /// Decode a record, or `None` if it was never written.
    ///
    /// A zero `pid` is the "never written" marker: the lock file is created by
    /// `open(O_CREAT)` and read sparsely, so an untouched record reads back as
    /// zeroes, and pid 0 is not a process any participant can be.
    #[must_use]
    pub fn from_bytes(raw: &[u8; IDENTITY_RECORD_LEN]) -> Option<Identity> {
        let pid = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        if pid == 0 {
            return None;
        }
        let mut start = [0u8; 8];
        start.copy_from_slice(&raw[4..12]);
        let mut boot = [0u8; 16];
        boot.copy_from_slice(&raw[12..28]);
        let mut name = [0u8; 32];
        name.copy_from_slice(&raw[32..64]);
        Some(Identity {
            pid,
            start_time: u64::from_le_bytes(start),
            boot_id: boot,
            mode: AccessMode::from_byte(raw[28]),
            name,
        })
    }

    /// Whether this record still describes a live process, for `doctor`-style
    /// reporting only.
    ///
    /// **Never use this to decide liveness for the protocol** (§5.1): the lock
    /// byte is authoritative, this is a `/proc` inference with a race in it. It
    /// exists to say "slot 3's record names pid 1841, which is gone" in a
    /// diagnostic, which is genuinely useful and not a decision.
    #[must_use]
    pub fn matches_running_process(&self) -> bool {
        match crate::procstat::start_time_of(self.pid) {
            Ok(start) => start == self.start_time,
            Err(ProcError::Unreadable { .. } | ProcError::Parse { .. } | ProcError::BootId) => {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn records_round_trip() {
        let id = Identity {
            pid: 0xDEAD_BEEF,
            start_time: 0x0102_0304_0506_0708,
            boot_id: [0xAB; 16],
            mode: AccessMode::ReadWrite,
            name: {
                let mut n = [0u8; 32];
                n[..5].copy_from_slice(b"hello");
                n
            },
        };
        let bytes = id.to_bytes();
        assert_eq!(bytes.len(), 64);
        assert_eq!(Identity::from_bytes(&bytes), Some(id));
        assert_eq!(id.name_str(), "hello");
    }

    #[test]
    fn an_all_zero_record_means_never_written() {
        assert_eq!(Identity::from_bytes(&[0u8; IDENTITY_RECORD_LEN]), None);
    }

    #[test]
    fn the_field_offsets_are_pinned() {
        // Two builds must agree byte for byte; this is the only place that is
        // checkable without a second binary.
        let id = Identity {
            pid: 1,
            start_time: 2,
            boot_id: [3; 16],
            mode: AccessMode::ReadOnly,
            name: [4; 32],
        };
        let b = id.to_bytes();
        assert_eq!(&b[0..4], &1u32.to_le_bytes());
        assert_eq!(&b[4..12], &2u64.to_le_bytes());
        assert_eq!(&b[12..28], &[3u8; 16]);
        assert_eq!(b[28], 0);
        assert_eq!(&b[29..32], &[0u8; 3], "padding must be zero");
        assert_eq!(&b[32..64], &[4u8; 32]);
    }

    #[test]
    fn self_identity_names_this_process() {
        let id = Identity::of_self(AccessMode::ReadOnly).unwrap();
        assert_eq!(id.pid, std::process::id());
        assert!(id.start_time > 0);
        assert!(id.matches_running_process());

        let mut dead = id;
        // Same pid, a start time no live process can have.
        dead.start_time = id.start_time.wrapping_add(1);
        assert!(
            !dead.matches_running_process(),
            "start_time is what defeats pid reuse"
        );
    }

    #[test]
    fn best_effort_never_fails() {
        let id = Identity::of_self_best_effort(AccessMode::ReadWrite);
        assert_eq!(id.pid, std::process::id());
        assert_eq!(id.mode, AccessMode::ReadWrite);
    }
}
