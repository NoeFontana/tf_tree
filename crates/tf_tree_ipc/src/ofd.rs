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
//! [`fcntl_flock`] performs the `fcntl` syscall with a pointer to a `Flock`
//! owned by its caller's stack frame for the duration of the call. The kernel
//! reads it for `F_OFD_SETLK` and reads *and writes* it for `F_OFD_GETLK`, never
//! retains it, and never touches any other user memory. Every caller in this
//! module passes `&mut Flock`, so the pointer is valid, aligned, unaliased and
//! sized correctly by construction; `Flock` is `#[repr(C)]` and field-for-field
//! identical to the kernel's `struct flock` on the two 64-bit architectures this
//! module supports.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use rustix::io::Errno;

// The kernel's `struct flock` is architecture-dependent in ways that matter
// (32-bit needs `flock64` and the `fcntl64` syscall; sparc and hppa renumber
// `F_RDLCK`/`F_WRLCK`/`F_UNLCK`). Rather than guess, refuse to build anywhere
// the constants below have not been checked. `docs/PHASE2.md` §2 scopes Phase 2
// to Linux, and CI runs x86-64 and aarch64.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "tf_tree_ipc issues fcntl(F_OFD_SETLK) directly and has only been verified \
     for the x86_64 and aarch64 syscall ABIs (docs/PHASE2.md §2 scopes Phase 2 \
     to Linux on those two architectures)"
);

/// `fcntl` syscall number. `asm-generic/unistd.h` numbering on aarch64, the
/// x86-64 table otherwise. Both take a 64-bit `struct flock` directly, so no
/// `fcntl64` variant is involved.
#[cfg(target_arch = "x86_64")]
const SYS_FCNTL: usize = 72;
#[cfg(target_arch = "aarch64")]
const SYS_FCNTL: usize = 25;

/// `F_OFD_GETLK` — query without taking. Reports only *conflicting* locks, so a
/// lock held by the querying description itself always reads as free.
const F_OFD_GETLK: usize = 36;
/// `F_OFD_SETLK` — non-blocking acquire or release.
const F_OFD_SETLK: usize = 37;

/// The kernel's `struct flock` for x86-64 and aarch64: two 16-bit fields, two
/// 64-bit offsets, one 32-bit pid, `align(8)`, 32 bytes total.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Flock {
    l_type: i16,
    l_whence: i16,
    l_start: i64,
    l_len: i64,
    l_pid: i32,
}

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
    let mut lock = Flock {
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
    let mut lock = Flock {
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
fn fcntl_flock(fd: BorrowedFd<'_>, cmd: usize, lock: &mut Flock) -> Result<(), Errno> {
    let raw = fd.as_fd().as_raw_fd() as usize;
    let ptr: *mut Flock = lock;

    // SAFETY (both arms): the syscall convention is the platform's, `raw` is a
    // descriptor borrowed for the whole call, and `ptr` points at the caller's
    // live, aligned, uniquely-borrowed `Flock` — the only memory the kernel
    // touches for `F_OFD_SETLK`/`F_OFD_GETLK`, and it does not retain it.
    // `nostack` holds because a syscall does not use the user stack.
    #[cfg(target_arch = "x86_64")]
    let ret: isize = unsafe {
        let ret: isize;
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_FCNTL => ret,
            in("rdi") raw,
            in("rsi") cmd,
            in("rdx") ptr,
            // `syscall` clobbers rcx (return address) and r11 (saved rflags).
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
        ret
    };
    #[cfg(target_arch = "aarch64")]
    let ret: isize = unsafe {
        let ret: isize;
        core::arch::asm!(
            "svc 0",
            in("x8") SYS_FCNTL,
            inlateout("x0") raw => ret,
            in("x1") cmd,
            in("x2") ptr,
            options(nostack),
        );
        ret
    };

    // Linux returns errors as -errno in the return register, so the range
    // (-4095, -1] is the error window and everything else is a success value.
    if (-4095..0).contains(&ret) {
        // `Errno::from_raw_os_error` wants the positive C value.
        return Err(Errno::from_raw_os_error(-ret as i32));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn flock_matches_the_kernel_abi() {
        // If this ever drifts, every lock in the crate is placed at a garbage
        // offset and the failure mode is silent.
        assert_eq!(size_of::<Flock>(), 32);
        assert_eq!(align_of::<Flock>(), 8);
        assert_eq!(offset_of!(Flock, l_type), 0);
        assert_eq!(offset_of!(Flock, l_whence), 2);
        assert_eq!(offset_of!(Flock, l_start), 8);
        assert_eq!(offset_of!(Flock, l_len), 16);
        assert_eq!(offset_of!(Flock, l_pid), 24);
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
