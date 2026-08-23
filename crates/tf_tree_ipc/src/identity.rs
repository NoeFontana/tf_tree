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
use crate::procstat::{boot_id, self_comm, self_pid_ns_inode, self_start_time};

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
    ///
    /// **Sixteen bytes at `32..48` since `docs/decisions/0033`, where it was
    /// thirty-two.** The kernel caps `comm` at 15 bytes plus its NUL
    /// (`TASK_COMM_LEN`), so nothing that ever reached this field filled half of
    /// the old one — the narrowing is what makes room for `pid_ns_inode` without
    /// touching the 64-byte stride, and therefore without touching
    /// `FORMAT_VERSION` or `layout_hash`, neither of which this file is.
    pub name: [u8; 16],
    /// The `nsfs` inode of the writer's PID namespace, or **`0` for "unknown
    /// namespace"**.
    ///
    /// The discriminator `pid` cannot be: a pid is namespace-local, so a record
    /// written inside `unshare --fork --pid` or a container names a *different*
    /// process when it is resolved against an observer's `/proc`, and the
    /// identity triple has nothing that differs — `boot_id` is identical across
    /// every namespace on one host, and the kernel has no per-namespace boot id.
    /// `docs/decisions/0033` is the argument; `TFT014` calling a healthy
    /// containerised participant a fork inheritor, and telling the operator to
    /// stop it, is what it cost.
    ///
    /// **Zero must mean *keep the pre-`0033` behaviour*, never "namespace 0".**
    /// A record written before this field existed reads back as zero, because
    /// `comm` never reached byte 48; so does one whose writer could not read
    /// `/proc`. Lock files outlive the process that wrote them, so an observer
    /// meets both.
    pub pid_ns_inode: u64,
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
            // Not an error even here, where every other field is one: an
            // unreadable namespace is `0`, which every reader already has a
            // rule for. Failing `of_self` on it would make a `/proc` without
            // `ns/` — a kernel built with `CONFIG_PID_NS=n`, or a sandbox that
            // hides the directory — refuse an arena it can otherwise serve.
            pid_ns_inode: self_pid_ns_inode().unwrap_or(0),
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
            pid_ns_inode: self_pid_ns_inode().unwrap_or(0),
        }
    }

    /// The name field as a string, trimmed at the first NUL.
    #[must_use]
    pub fn name_str(&self) -> &str {
        // `self.name.len()`, never a literal: with the field at `[u8; 16]` a
        // hard-coded 32 is an out-of-bounds slice on a `pub` method, and the
        // type checker does not see it. Not reachable from data this workspace
        // writes — `self_comm` NUL-terminates every name the kernel can produce
        // — but `from_bytes` is `pub`, validates `pid != 0` and nothing else,
        // and decodes a file any same-uid process can `pwrite` into.
        let end = self
            .name
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(self.name.len());
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
        // `32 + self.name.len()`, and for the same reason `name_str` spells its
        // bound that way: both sides of `copy_from_slice` are slices, so a
        // literal 32 here type-checks against a `[u8; 16]` and then panics at
        // run time on *every* `to_bytes` — which is every `write_identity`,
        // which is every registering `open()`.
        out[32..32 + self.name.len()].copy_from_slice(&self.name);
        out[48..56].copy_from_slice(&self.pid_ns_inode.to_le_bytes());
        // 56..64 spare
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
            // A pre-`0033` record reaches here too, and reads `0`: its writer's
            // `comm` stopped at byte 47 or earlier, so these eight bytes are
            // the NUL padding of a name that was never this long.
            pid_ns_inode: u64::from_le_bytes(ns),
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

    /// **The compatibility direction that has to hold in the wild: a record
    /// written before `pid_ns_inode` existed decodes as `0`, "unknown
    /// namespace".** Lock files outlive the process that wrote them, and this
    /// crate carries no version field to make the change detectable, so this is
    /// the whole compatibility argument rather than one clause of it.
    ///
    /// The bytes below are the *pre-`0033`* encoding, built here rather than
    /// produced by the current encoder — which could not produce them, and that
    /// is the point. What makes it decode correctly is that `self_comm` could
    /// never write past byte 47: the kernel caps `comm` at 15 bytes plus its
    /// NUL, so `48..64` of every record the old code wrote is padding.
    #[test]
    fn a_pre_0033_record_reads_as_unknown_namespace() {
        let mut old = [0u8; IDENTITY_RECORD_LEN];
        old[0..4].copy_from_slice(&4242u32.to_le_bytes());
        old[4..12].copy_from_slice(&987_654u64.to_le_bytes());
        old[12..28].copy_from_slice(&[7u8; 16]);
        old[28] = AccessMode::ReadWrite as u8;
        // The old field was `32..64`; a real writer filled at most `32..47`.
        old[32..36].copy_from_slice(b"node");

        let id = Identity::from_bytes(&old).expect("a nonzero pid is a written record");
        assert_eq!(id.name_str(), "node", "an old name still decodes");
        assert_eq!(
            id.pid_ns_inode, 0,
            "an old record must read as unknown, never as namespace 0"
        );

        // And the other direction, which is what lets an unmodified decoder
        // read a new record: every reader NUL-trims, so the name is intact and
        // the inode is simply never looked at.
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

        // **A pre-`0033` name longer than the new field, which is the only
        // input that reaches `name_str`'s fallback.** The `"node"` record
        // above has a NUL at 36 and never gets there, so it does not pin
        // `unwrap_or(self.name.len())` at all: reverting that to the old
        // `unwrap_or(32)` left `-p tf_tree_ipc` 91/91, `--lib` 124/124 and
        // `--test attach` 16/16 green. Sixteen bytes with no NUL is what an
        // 18-to-20-byte name from before the narrowing leaves in `32..48`, and
        // on `unwrap_or(32)` it panics — *range end index 32 out of range for
        // slice of length 16* — on a `pub` method, from lock-file bytes any
        // process with the right uid can write.
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
        // Two builds must agree byte for byte; this is the only place that is
        // checkable without a second binary.
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
        // The tail `0033` left spare. Nothing else in the workspace pins this
        // layout, so without this line the eight bytes a future field would
        // take are pinned by nothing — and the pre-`0033` compatibility
        // argument is exactly that a record's unused tail reads zero.
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
        // **This is the only test of the production writer's namespace field,
        // and without it `0033` ships inert.** `of_self_best_effort` is the
        // sole constructor on the registration path (`open.rs`'s step 5 and
        // `ipc_child`); `of_self` has one caller in the workspace and it is a
        // test. Every `TFT014` namespace arm hand-writes `pid_ns_inode` into a
        // synthetic record, so replacing the read here with a literal `0` —
        // the fix recording nothing, in the field it exists to fill — left
        // `-p tf_tree_ipc` 91/91, `-p tf_tree_cli --features shm --lib`
        // 124/124, `--test attach` 16/16 and `--test rendezvous` 31/31 all
        // green. Measured, not supposed. Mutant: with this line, that same
        // change fails here.
        assert_eq!(
            Some(id.pid_ns_inode),
            crate::procstat::self_pid_ns_inode(),
            "the registration path must record the namespace it actually read"
        );
    }
}
