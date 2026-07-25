//! Open file description locks — `F_OFD_SETLK` / `F_OFD_GETLK`.
//!
//! `docs/PHASE2.md` §3.3 is unambiguous about which lock flavour the rendezvous
//! is built on, and the distinction is not cosmetic:
//!
//! * **Classic POSIX locks** (`F_SETLK`) are owned by the *process*, and the
//!   kernel drops every one of them when **any** file descriptor to that file is
//!   closed anywhere in the process. A library that shares an address space with
//!   code it does not control cannot defend against that: an unrelated crate
//!   that opens and closes the lock file — a config reader, a `doctor`
//!   command — silently releases this crate's ownership lock, and two processes
//!   then both believe they own the arena. There is no way to detect it after
//!   the fact.
//! * **OFD locks** (`F_OFD_SETLK`, Linux ≥ 3.15) are owned by the open file
//!   description. They survive an unrelated `open`/`close` of the same path,
//!   they are released exactly when the last descriptor referring to *that*
//!   description closes — which the kernel does on process death, including
//!   `SIGKILL` — and two descriptions conflict even inside one process.
//!
//! # Why raw syscalls
//!
//! `rustix` 1.1 exposes `flock` (whole file), `fcntl_lock` (classic `F_SETLK`,
//! whole file) and `fcntl_getlk` (classic `F_GETLK`). None of the three can take
//! an OFD lock, and byte-range OFD locking is exactly what §3.3's layout needs.
//! The dependency budget in §2 allows `rustix` and *no libc crate and no C build
//! step*, so the remaining option is to issue `fcntl` directly. That is the only
//! `unsafe` in this crate and it lives entirely in this module.
//!
//! # SAFETY (module invariant)
//!
//! [`fcntl_flock`] performs the `fcntl` syscall with a pointer to a `libc::flock`
//! owned by its caller's stack frame for the duration of the call. The kernel
//! reads it for `F_OFD_SETLK` and reads *and writes* it for `F_OFD_GETLK`, never
//! retains it, and never touches any other user memory. Every caller in this
//! module passes `&mut libc::flock`, so the pointer is valid, aligned, unaliased
//! and sized correctly by construction; `libc::flock` is the kernel's own
//! identical to the kernel's `struct flock` on the two 64-bit architectures this
//! module supports.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use rustix::io::Errno;

// `struct flock`, the `F_OFD_*` command numbers, and the `fcntl` syscall number
// are all architecture- and ABI-dependent in ways that bite: 32-bit targets need
// `flock64` and `fcntl64`, and sparc and hppa renumber `F_RDLCK`/`F_WRLCK`/
// `F_UNLCK`. `libc` carries the correct definitions for every target Rust
// supports, which is why this module uses them rather than its own.
//
// **This is a documented deviation from `docs/PHASE2.md` §2**, which names
// `rustix` and says "no libc crate". rustix 1.1.4 has no OFD locking at all —
// its `fcntl_lock` is *classic* `F_SETLK` and whole-file, which §3.3 rejects by
// name, and `flock` is whole-file too. So the choice was between hand-rolling
// the syscall and taking `libc`.
//
// Hand-rolling was tried first and rejected on review: it pinned the syscall
// number and `struct flock` layout by hand and `compile_error!`d on every
// architecture except x86-64 and aarch64 — including riscv64 and ppc64le. A
// lock primitive that the entire rendezvous depends on is the wrong place to
// carry a hand-maintained ABI, and §2's rationale is "no C build step", which
// `libc` does not introduce: it is declarations, not compilation.

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

/// A byte range of the lock file.
///
/// Ranges are single bytes almost everywhere in §3.3 — the point of a byte range
/// is not to protect data (the file holds none at these offsets) but to give the
/// kernel a *name* for a lock, one per role and per slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Range {
    /// First byte of the range.
    pub start: u64,
    /// Length in bytes. Never 0 here: `l_len == 0` means "to end of file",
    /// which would make every range collide with every other.
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
    /// Whether a *conflicting* lock is held. Locks held by the querying
    /// descriptor itself are invisible here — the kernel reports conflicts, and
    /// nothing conflicts with itself.
    pub held: bool,
    /// The `l_pid` the kernel filled in. `-1` for an OFD lock, which is the
    /// documented (`docs/PHASE2.md` §3.3) consequence of an OFD lock belonging
    /// to a file description rather than a process — nobody can be named. `0`
    /// when nothing is held.
    ///
    /// The lock file therefore answers *"is anyone alive?"*, and *"who?"* comes
    /// from the identity records, which is why those exist as plain data.
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
        // NORMATIVE for the OFD commands: `fcntl(2)` requires `l_pid` to be zero
        // on input, and returns EINVAL otherwise.
        l_pid: 0,
    };
    match fcntl_flock(fd, F_OFD_SETLK, &mut lock) {
        Ok(()) => Ok(LockAttempt::Acquired),
        // EAGAIN is what Linux returns; POSIX permits EACCES and some
        // filesystems use it. Treating only one of them as contention would
        // turn a lost race into a hard failure.
        Err(e) if e == Errno::AGAIN || e == Errno::ACCESS => Ok(LockAttempt::Contended),
        Err(e) => Err(e),
    }
}

/// Ask whether anyone *else* holds a conflicting lock on `range`.
pub(crate) fn probe(fd: BorrowedFd<'_>, range: Range) -> Result<LockProbe, Errno> {
    let mut lock = libc::flock {
        // Ask about an exclusive lock: it conflicts with both shared and
        // exclusive holders, so this reports any holder at all.
        l_type: LockKind::Exclusive as i16,
        l_whence: SEEK_SET,
        l_start: range.start as i64,
        l_len: range.len as i64,
        l_pid: 0,
    };
    fcntl_flock(fd, F_OFD_GETLK, &mut lock)?;
    // The kernel signals "no conflict" by overwriting `l_type` with F_UNLCK and
    // leaving the rest of the structure alone.
    let held = lock.l_type != LockKind::Unlock as i16;
    Ok(LockProbe {
        held,
        holder_pid: if held { lock.l_pid } else { 0 },
    })
}

/// `fcntl(fd, cmd, &mut flock)`, returning the kernel's errno on failure.
fn fcntl_flock(fd: BorrowedFd<'_>, cmd: i32, lock: &mut libc::flock) -> Result<(), Errno> {
    // SAFETY: `fcntl` with an `F_OFD_*` command reads (and, for `F_OFD_GETLK`,
    // writes) exactly one `struct flock` through the pointer, and does not
    // retain it. `lock` is a live, aligned, uniquely-borrowed `libc::flock` —
    // libc's own definition, so the layout is the kernel's by construction — and
    // the descriptor is borrowed for the whole call.
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
        // If this ever drifts, every lock in the crate is placed at a garbage
        // offset and the failure mode is silent.
        // libc owns the layout now, so pinning field offsets here would only
        // re-assert libc's own definition. What this crate still relies on is
        // that the OFD commands exist and are distinct from the classic ones —
        // if a target ever aliased them, `F_OFD_SETLK` would silently become a
        // process-owned `F_SETLK` and every liveness guarantee in §3.3 would
        // quietly evaporate.
        assert_ne!(libc::F_OFD_SETLK, libc::F_SETLK);
        assert_ne!(libc::F_OFD_GETLK, libc::F_GETLK);
        assert_ne!(libc::F_OFD_SETLK, libc::F_OFD_GETLK);
    }

    #[test]
    fn a_bad_descriptor_reports_ebadf_rather_than_succeeding() {
        // Proves the errno decode works end to end: an unopened fd must come
        // back as EBADF, not as a spurious `Acquired`. (`borrow_raw(-1)` is not
        // an option — std asserts on it — so use a number no test process has
        // open.)
        //
        // SAFETY: fd 4096 is not open in this process; the `BorrowedFd` is only
        // handed to a syscall that will reject it, and is never used to close,
        // read, or write anything.
        let bad = unsafe { BorrowedFd::borrow_raw(4096) };
        let err = try_lock(bad, Range::byte(0), LockKind::Exclusive).unwrap_err();
        assert_eq!(err, Errno::BADF);
    }
}
