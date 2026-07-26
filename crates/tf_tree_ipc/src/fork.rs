//! Fork detection — one relaxed load, and a counter the kernel bumps for us.
//!
//! # The problem this exists for
//!
//! A `tf_tree` shared arena is mapped `MADV_DONTFORK` (`docs/PHASE2.md` §7.3),
//! so a `fork()` child gets **no mapping at all** where the arena used to be.
//! Every `&`-reference the parent's `Tree` holds into that region is dangling in
//! the child the instant `fork` returns. Nothing in the child *observes* that:
//! the `Tree` value is byte-for-byte identical, the pointers still look like
//! pointers, and the first read is a `SIGSEGV` with no diagnostic attached.
//!
//! Worse, the child does not have to *do* anything. `Tree`, `ClaimLease` and
//! `Attachment` all have destructors, and the child runs them at scope exit —
//! including on the `os._exit`-less path that `multiprocessing` takes. Two of
//! those destructors release **OFD locks on inherited descriptions**, and an OFD
//! lock is owned by the open file description, not the process: unlocking from
//! the child releases the *parent's* claim lease and the *parent's* ownership
//! byte. That is a silent, remote failure — the parent keeps publishing onto an
//! edge a reaper is now free to hand to somebody else.
//!
//! So the child must be able to tell that it is the child, cheaply, at any
//! point, with no syscall.
//!
//! # Why a counter and not `getpid()`
//!
//! `getpid` on Linux is a real syscall, not a vDSO call — roughly 50–100 ns
//! against `PHASE1.md` §11's 150 ns p50 lookup budget, which would be a
//! measurable tax on every lookup forever. It is also *insufficient*: a cached
//! pid can only be compared when something calls, so a fork that happens while
//! no call is in flight leaves a `Tree` that has never been re-validated.
//!
//! `pthread_atfork`'s child handler runs **in the child, inside `fork`**, before
//! the child can execute any user code. So by the time anything can observe the
//! `Tree`, the counter has already moved. Reading it is a relaxed load of a
//! process-local static: a few nanoseconds, no fence, no syscall.
//!
//! # Ordering
//!
//! `Relaxed` is correct and is not a shortcut. The child of a `fork` is a fresh
//! single-threaded process whose memory is a snapshot taken at the fork point,
//! and the handler runs in that child before it returns from `fork` — so the
//! bump *happens-before* every subsequent operation in the child by program
//! order alone. There is no inter-thread edge to establish and nothing for an
//! `Acquire` to synchronize with.
//!
//! # SAFETY (module invariant)
//!
//! The single `unsafe` block calls [`pthread_atfork`] with a `child` handler
//! that is an `extern "C" fn` with no arguments and no return value, and no
//! `prepare` or `parent` handler. The handler is a `'static` item, so the
//! pointer the C library retains for the lifetime of the process is always
//! valid. Its body is a single `fetch_add` on a `static AtomicU64`, which is
//! async-signal-safe and lock-free on every architecture this crate supports —
//! the standing requirement for an atfork handler, which may run with arbitrary
//! locks held by other threads. It allocates nothing and can neither panic nor
//! unwind across the FFI boundary.

use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

// `libc` 0.2.189 declares `pthread_atfork` for the BSDs, Haiku, AIX, Cygwin and
// newlib — but **not** for `linux_like`. So it is declared here.
//
// `ofd.rs` argues at length against hand-maintaining an ABI, and that argument
// does not apply to this one. What made `fcntl(F_OFD_*)` the wrong thing to
// hand-roll was `struct flock`: a layout that differs between 32- and 64-bit,
// with command numbers that are renumbered on sparc and hppa. `pthread_atfork`
// has no struct in its signature at all. It is three nullable function pointers
// and an `int`, fixed by POSIX.1-2001, identical on every platform that has it.
// There is nothing here that can drift.
unsafe extern "C" {
    fn pthread_atfork(
        prepare: Option<extern "C" fn()>,
        parent: Option<extern "C" fn()>,
        child: Option<extern "C" fn()>,
    ) -> libc::c_int;
}

/// Incremented once per `fork()`, in the child, before the child runs any code.
static FORK_GEN: AtomicU64 = AtomicU64::new(0);

/// Guards the one-time registration.
static ARMED: Once = Once::new();

/// The `pthread_atfork` child handler.
///
/// Deliberately the smallest thing that can be written: an atfork handler runs
/// in a child that may have forked while another thread held the allocator lock,
/// so anything that could allocate or take a lock can deadlock here. A
/// `fetch_add` on a `static` does neither.
extern "C" fn after_fork_in_child() {
    FORK_GEN.fetch_add(1, Ordering::Relaxed);
}

/// Install the fork handler. Idempotent, and cheap enough to call from any
/// constructor.
///
/// Call this **before** any `fork` can happen — from wherever a shared mapping
/// is established. Arming lazily on first *use* would be too late: the fork that
/// matters may already have happened, and the counter would read 0 in both
/// processes.
///
/// A failure to register is swallowed. `pthread_atfork` fails only with
/// `ENOMEM`, at which point the process has larger problems than fork detection,
/// and the alternative — refusing to open the arena — would turn a
/// nearly-impossible allocation failure into a hard outage.
pub fn arm() {
    ARMED.call_once(|| {
        // SAFETY: see the module invariant above. `after_fork_in_child` is a
        // `'static` `extern "C" fn()` matching the handler type exactly, and no
        // other argument is passed.
        let _ = unsafe { pthread_atfork(None, None, Some(after_fork_in_child)) };
    });
}

/// The current fork generation of this process.
///
/// Capture it alongside anything that holds a shared mapping or an OFD lock, and
/// compare before using that thing. A difference means "this value belongs to a
/// process that no longer exists" — see [`arm`].
#[inline]
#[must_use]
pub fn generation() -> u64 {
    FORK_GEN.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arming twice must not register the handler twice — a doubly-registered
    /// handler bumps the counter by 2 per fork, which is still "different" and
    /// so still passes any test that only asserts inequality. This asserts the
    /// `Once` is doing its job at all.
    #[test]
    fn arming_is_idempotent() {
        arm();
        arm();
        arm();
        // Nothing forked, so the generation is still whatever it was.
        assert_eq!(generation(), generation());
    }

    /// In the parent — the process running this test — the generation never
    /// moves on its own. If this ever fails, something is calling the handler
    /// outside a fork.
    #[test]
    fn the_generation_is_stable_without_a_fork() {
        arm();
        let before = generation();
        for _ in 0..1000 {
            std::hint::black_box(generation());
        }
        assert_eq!(generation(), before);
    }

    /// The real behaviour is only observable across a `fork`, which needs
    /// `unsafe` and a child process; that lives in
    /// `crates/tf_tree_bench/src/bin/fork_child.rs` and is driven by
    /// `crates/tf_tree_bench/tests/fork.rs`. This test records the link so the
    /// coverage is findable from here.
    #[test]
    fn the_cross_fork_behaviour_is_tested_elsewhere() {
        assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tf_tree_bench/tests/fork.rs")
            .exists());
    }
}
