//! Open file description locks — `F_OFD_SETLK` / `F_OFD_GETLK`.
//!
//! `docs/PHASE2.md` §3.3 requires OFD locks. Classic POSIX locks are owned by the
//! process and dropped when *any* descriptor to the file closes, so an unrelated
//! crate opening the lock file would silently release ownership. OFD locks
//! (Linux ≥ 3.15) belong to the open file description, release when its last
//! descriptor closes (including on `SIGKILL`), and conflict even inside one
//! process.
//!
//! # Why `libc::fcntl`
//!
//! `rustix` 1.1 has no OFD locking, so this module calls `fcntl` through `libc`:
//! a deviation from §2's "no libc crate", which exists to avoid a C build step
//! that `libc` does not introduce. It is one of two `unsafe` sites in this crate;
//! the other is `fork`'s `pthread_atfork` shim.
//!
//! # SAFETY (module invariant)
//!
//! [`fcntl_flock`] passes `fcntl` a `&mut libc::flock` owned by the caller's
//! frame for the call. The kernel reads it (and writes it for `F_OFD_GETLK`),
//! never retains it, and touches no other user memory; the pointer is valid,
//! aligned and unaliased, and `libc::flock` is the kernel's `struct flock`.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use rustix::io::Errno;

// `libc` carries the correct `struct flock`, `F_OFD_*` numbers and `fcntl`
// syscall for every target (32-bit `flock64`, sparc/hppa renumbering).

/// `F_OFD_GETLK` — query without taking. Reports only *conflicting* locks, so a
/// lock held by the querying description itself always reads as free.
const F_OFD_GETLK: i32 = libc::F_OFD_GETLK;
/// `F_OFD_SETLK` — non-blocking acquire or release.
const F_OFD_SETLK: i32 = libc::F_OFD_SETLK;

/// `F_RDLCK`, `F_WRLCK`, `F_UNLCK` from `asm-generic/fcntl.h`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i16)]
pub(crate) enum LockKind {
    /// Shared. Unused by the rendezvous today; every §3.3 byte is exclusive.
    #[allow(dead_code)]
    Shared = 0,
    /// Exclusive. Requires the descriptor to be open for writing.
    Exclusive = 1,
    /// Release.
    Unlock = 2,
}

/// `SEEK_SET`: offsets in [`Range`] are absolute file offsets.
const SEEK_SET: i16 = 0;

/// A byte range of the lock file. Ranges are single bytes: the range names a
/// lock, it does not protect data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Range {
    /// First byte of the range.
    pub start: u64,
    /// Length in bytes. Never 0: `l_len == 0` means "to end of file".
    pub len: u64,
}

impl Range {
    /// A one-byte range at `offset`.
    pub(crate) const fn byte(offset: u64) -> Range {
        Range {
            start: offset,
            len: 1,
        }
    }
}

/// Outcome of a non-blocking acquire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockAttempt {
    /// The lock is now held by this open file description.
    Acquired,
    /// Someone else holds a conflicting lock. `EAGAIN`/`EACCES` from the kernel.
    Contended,
}

/// What `F_OFD_GETLK` reports about a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockProbe {
    /// Whether a *conflicting* lock is held; the querying description's own locks
    /// are invisible.
    pub held: bool,
    /// The `l_pid` the kernel filled in: `-1` for an OFD lock (nobody can be
    /// named, `docs/PHASE2.md` §3.3), `0` when nothing is held. Names come from
    /// the identity records.
    pub holder_pid: i32,
}

/// Take or release an OFD lock on `range`, without blocking.
pub(crate) fn try_lock(
    fd: BorrowedFd<'_>,
    range: Range,
    kind: LockKind,
) -> Result<LockAttempt, Errno> {
    let mut lock = libc::flock {
        l_type: kind as i16,
        l_whence: SEEK_SET,
        l_start: range.start as i64,
        l_len: range.len as i64,
        // NORMATIVE: `fcntl(2)` requires `l_pid == 0` on input for OFD commands.
        l_pid: 0,
    };
    match fcntl_flock(fd, F_OFD_SETLK, &mut lock) {
        Ok(()) => Ok(LockAttempt::Acquired),
        // Linux returns EAGAIN; POSIX permits EACCES.
        Err(e) if e == Errno::AGAIN || e == Errno::ACCESS => Ok(LockAttempt::Contended),
        Err(e) => Err(e),
    }
}

/// Ask whether anyone *else* holds a conflicting lock on `range`.
pub(crate) fn probe(fd: BorrowedFd<'_>, range: Range) -> Result<LockProbe, Errno> {
    let mut lock = libc::flock {
        // An exclusive query conflicts with any holder.
        l_type: LockKind::Exclusive as i16,
        l_whence: SEEK_SET,
        l_start: range.start as i64,
        l_len: range.len as i64,
        l_pid: 0,
    };
    fcntl_flock(fd, F_OFD_GETLK, &mut lock)?;
    // No conflict: the kernel overwrites `l_type` with F_UNLCK.
    let held = lock.l_type != LockKind::Unlock as i16;
    Ok(LockProbe {
        held,
        holder_pid: if held { lock.l_pid } else { 0 },
    })
}

/// `fcntl(fd, cmd, &mut flock)`, returning the kernel's errno on failure.
fn fcntl_flock(fd: BorrowedFd<'_>, cmd: i32, lock: &mut libc::flock) -> Result<(), Errno> {
    // SAFETY: `fcntl` with an `F_OFD_*` command reads (and, for `F_OFD_GETLK`,
    // writes) one `struct flock` through the pointer without retaining it; `lock`
    // is live, aligned and uniquely borrowed, and the fd is borrowed for the call.
    let ret = unsafe { libc::fcntl(fd.as_fd().as_raw_fd(), cmd, lock as *mut libc::flock) };
    if ret < 0 {
        return Err(Errno::from_raw_os_error(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn flock_matches_the_kernel_abi() {
        // OFD commands must exist and differ from the classic ones; an alias would
        // silently make locks process-owned.
        assert_ne!(libc::F_OFD_SETLK, libc::F_SETLK);
        assert_ne!(libc::F_OFD_GETLK, libc::F_GETLK);
        assert_ne!(libc::F_OFD_SETLK, libc::F_OFD_GETLK);
    }

    #[test]
    fn a_bad_descriptor_reports_ebadf_rather_than_succeeding() {
        // An unopened fd must decode to EBADF, not a spurious `Acquired`.
        // SAFETY: fd 4096 is not open here; the `BorrowedFd` only goes to a syscall
        // that rejects it.
        let bad = unsafe { BorrowedFd::borrow_raw(4096) };
        let err = try_lock(bad, Range::byte(0), LockKind::Exclusive).unwrap_err();
        assert_eq!(err, Errno::BADF);
    }
}
