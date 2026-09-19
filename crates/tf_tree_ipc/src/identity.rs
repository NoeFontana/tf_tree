//! Identity records — 64 bytes per slot at `4096 + 64·i` of the lock file.
//!
//! `F_OFD_GETLK` reports `l_pid = -1`, so who holds a slot is written here as
//! plain `pwrite` data. `docs/PHASE2.md` §5.1: advisory; liveness is the lock.

use crate::error::{IpcError, ProcError};
use crate::procstat::{boot_id, self_comm, self_pid_ns_inode, self_start_time};

/// Size of one identity record; the 64-byte stride is NORMATIVE (§3.3).
pub const IDENTITY_RECORD_LEN: usize = 64;

/// How a participant mapped the arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AccessMode {
    /// `PROT_READ`. The consumer default (§8).
    ReadOnly = 0,
    /// `PROT_READ | PROT_WRITE`. Required to publish or claim.
    ReadWrite = 1,
}

impl AccessMode {
    fn from_byte(b: u8) -> AccessMode {
        // Unrecognised reads as the less privileged mode (diagnostics only).
        if b == 1 {
            AccessMode::ReadWrite
        } else {
            AccessMode::ReadOnly
        }
    }

    /// Strict decode for the §3.7 handshake, where `from_byte`'s leniency would
    /// downgrade a mode.
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
/// `(pid, start_time, boot_id)`: pids recycle, `start_time` (ticks since boot)
/// separates them within a boot, `boot_id` separates boots.
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
    /// `comm`, NUL-padded, at `32..48` (`docs/decisions/0033`); diagnostics only.
    pub name: [u8; 16],
    /// The `nsfs` inode of the writer's PID namespace; `0` means "unknown", not
    /// "namespace 0" (`docs/decisions/0033`).
    pub pid_ns_inode: u64,
}

impl Identity {
    /// This process's identity.
    ///
    /// # Errors
    ///
    /// [`IpcError::Proc`] if `/proc/self/stat` or the boot id cannot be read.
    /// The record is advisory, so the caller may proceed without one.
    pub fn of_self(mode: AccessMode) -> Result<Identity, IpcError> {
        Ok(Identity {
            pid: std::process::id(),
            start_time: self_start_time().map_err(IpcError::from)?,
            boot_id: boot_id().map_err(IpcError::from)?,
            mode,
            name: self_comm(),
            // An unreadable namespace is `0`, not an error.
            pid_ns_inode: self_pid_ns_inode().unwrap_or(0),
        })
    }

    /// This process's identity, with unreadable fields left zero.
    ///
    /// For the rendezvous path: a missing `/proc` must not fail `open()`.
    #[must_use]
    pub fn of_self_best_effort(mode: AccessMode) -> Identity {
        Identity {
            pid: std::process::id(),
            start_time: self_start_time().unwrap_or(0),
            boot_id: boot_id().unwrap_or([0u8; 16]),
            mode,
            name: self_comm(),
            pid_ns_inode: self_pid_ns_inode().unwrap_or(0),
        }
    }

    /// The name field as a string, trimmed at the first NUL.
    #[must_use]
    pub fn name_str(&self) -> &str {
        // `self.name.len()`, never a literal: `from_bytes` decodes a file any
        // same-uid process can write.
        let end = self
            .name
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..end]).unwrap_or("<non-utf8>")
    }

    /// Encode to the on-disk layout: little-endian, fixed offsets, 64 bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; IDENTITY_RECORD_LEN] {
        let mut out = [0u8; IDENTITY_RECORD_LEN];
        out[0..4].copy_from_slice(&self.pid.to_le_bytes());
        out[4..12].copy_from_slice(&self.start_time.to_le_bytes());
        out[12..28].copy_from_slice(&self.boot_id);
        out[28] = self.mode as u8;
        // 29..32 padding
        // `self.name.len()`, not a literal.
        out[32..32 + self.name.len()].copy_from_slice(&self.name);
        out[48..56].copy_from_slice(&self.pid_ns_inode.to_le_bytes());
        // 56..64 spare
        out
    }

    /// Decode a record, or `None` if never written (a zero `pid`).
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
        let mut name = [0u8; 16];
        name.copy_from_slice(&raw[32..48]);
        let mut ns = [0u8; 8];
        ns.copy_from_slice(&raw[48..56]);
        Some(Identity {
            pid,
            start_time: u64::from_le_bytes(start),
            boot_id: boot,
            mode: AccessMode::from_byte(raw[28]),
            name,
            // A pre-`0033` record reads `0` here.
            pid_ns_inode: u64::from_le_bytes(ns),
        })
    }

    /// Whether this record still describes a live process — for reporting only;
    /// never decide liveness from it (§5.1).
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
                let mut n = [0u8; 16];
                n[..5].copy_from_slice(b"hello");
                n
            },
            pid_ns_inode: 4_026_531_836,
        };
        let bytes = id.to_bytes();
        assert_eq!(bytes.len(), 64);
        assert_eq!(Identity::from_bytes(&bytes), Some(id));
        assert_eq!(id.name_str(), "hello");
    }

    /// A pre-`pid_ns_inode` record decodes as unknown namespace
    /// (`docs/decisions/0033`).
    #[test]
    fn a_pre_0033_record_reads_as_unknown_namespace() {
        let mut old = [0u8; IDENTITY_RECORD_LEN];
        old[0..4].copy_from_slice(&4242u32.to_le_bytes());
        old[4..12].copy_from_slice(&987_654u64.to_le_bytes());
        old[12..28].copy_from_slice(&[7u8; 16]);
        old[28] = AccessMode::ReadWrite as u8;
        old[32..36].copy_from_slice(b"node");

        let id = Identity::from_bytes(&old).expect("a nonzero pid is a written record");
        assert_eq!(id.name_str(), "node", "an old name still decodes");
        assert_eq!(
            id.pid_ns_inode, 0,
            "an old record must read as unknown, never as namespace 0"
        );

        // An old decoder NUL-trims a new record's name.
        let new = Identity {
            pid: 4242,
            start_time: 987_654,
            boot_id: [7u8; 16],
            mode: AccessMode::ReadWrite,
            name: *b"node\0\0\0\0\0\0\0\0\0\0\0\0",
            pid_ns_inode: 4_026_532_488,
        }
        .to_bytes();
        assert_eq!(&new[32..36], b"node");
        assert_eq!(new[36], 0, "an old reader trims here and stops");

        // A 16-byte name with no NUL reaches `name_str`'s fallback.
        let mut long = [0u8; IDENTITY_RECORD_LEN];
        long[0..4].copy_from_slice(&4242u32.to_le_bytes());
        long[32..48].copy_from_slice(b"a-parent-that-fo");
        let id = Identity::from_bytes(&long).expect("a nonzero pid is a written record");
        assert_eq!(
            id.name_str(),
            "a-parent-that-fo",
            "a name that fills the field decodes to all of it and panics on none of it"
        );
        assert_eq!(id.pid_ns_inode, 0, "and it is still an unknown namespace");
    }

    #[test]
    fn an_all_zero_record_means_never_written() {
        assert_eq!(Identity::from_bytes(&[0u8; IDENTITY_RECORD_LEN]), None);
    }

    #[test]
    fn the_field_offsets_are_pinned() {
        let id = Identity {
            pid: 1,
            start_time: 2,
            boot_id: [3; 16],
            mode: AccessMode::ReadOnly,
            name: [4; 16],
            pid_ns_inode: 5,
        };
        let b = id.to_bytes();
        assert_eq!(&b[0..4], &1u32.to_le_bytes());
        assert_eq!(&b[4..12], &2u64.to_le_bytes());
        assert_eq!(&b[12..28], &[3u8; 16]);
        assert_eq!(b[28], 0);
        assert_eq!(&b[29..32], &[0u8; 3], "padding must be zero");
        assert_eq!(&b[32..48], &[4u8; 16]);
        assert_eq!(&b[48..56], &5u64.to_le_bytes());
        assert_eq!(&b[56..64], &[0u8; 8], "the spare tail must be zero");
    }

    #[test]
    fn self_identity_names_this_process() {
        let id = Identity::of_self(AccessMode::ReadOnly).unwrap();
        assert_eq!(id.pid, std::process::id());
        assert!(id.start_time > 0);
        assert!(id.matches_running_process());
        assert_eq!(
            Some(id.pid_ns_inode),
            crate::procstat::self_pid_ns_inode(),
            "the record names the namespace its pid is drawn from"
        );

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
        // The registration path's only constructor.
        assert_eq!(
            Some(id.pid_ns_inode),
            crate::procstat::self_pid_ns_inode(),
            "the registration path must record the namespace it actually read"
        );
    }
}
