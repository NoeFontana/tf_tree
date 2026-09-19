//! Fork detection — one relaxed load of a counter `pthread_atfork` bumps in the child.
//!
//! A shared arena is mapped `MADV_DONTFORK` (`docs/PHASE2.md` §7.3), so a fork
//! child has no mapping where the arena was, yet inherits the parent's `Tree`
//! and its destructors. Two of those release OFD locks on inherited
//! descriptions, which frees the *parent's* claim lease and ownership byte.
//! The child must therefore be able to tell it is the child with no syscall
//! (`getpid` is a real syscall, and a cached pid is only checked when called).
//!
//! `Relaxed` is correct: the handler runs in the fresh single-threaded child
//! before `fork` returns, so the bump happens-before everything the child does
//! by program order.
//!
//! # SAFETY (module invariant)
//!
//! The single `unsafe` block registers a `'static` `extern "C" fn()` child
//! handler (no `prepare`/`parent`), so the pointer the C library retains stays
//! valid. Its body is one `fetch_add` on a `static AtomicU64`: async-signal-safe,
//! lock-free, allocation-free, cannot panic or unwind across the FFI boundary.

use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

// `libc` declares `pthread_atfork` for the BSDs but not `linux_like`. Its
// signature has no struct (unlike `fcntl(F_OFD_*)`), so it cannot drift.
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
/// Must not allocate or take a lock: the child may have forked while another
/// thread held the allocator lock.
extern "C" fn after_fork_in_child() {
    FORK_GEN.fetch_add(1, Ordering::Relaxed);
}

/// Install the fork handler. Idempotent.
///
/// Call this **before** any `fork` can happen, from wherever a shared mapping
/// is established; arming on first use is too late. A registration failure
/// (`ENOMEM` only) is swallowed.
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
/// Capture it alongside anything holding a shared mapping or an OFD lock and
/// compare before use; a difference means the value belongs to another process
/// — see [`arm`].
#[inline]
#[must_use]
pub fn generation() -> u64 {
    FORK_GEN.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generation never moves in the parent.
    #[test]
    fn the_generation_is_stable_without_a_fork() {
        arm();
        let before = generation();
        for _ in 0..1000 {
            std::hint::black_box(generation());
        }
        assert_eq!(generation(), before);
    }

    /// The cross-fork behaviour (the counter moves by exactly one, pinning the
    /// `Once` in [`arm`]) is asserted by `crates/tf_tree_bench/src/bin/fork_child.rs`
    /// (`docs/decisions/0005`); this records the link.
    #[test]
    fn the_cross_fork_behaviour_is_tested_elsewhere() {
        assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tf_tree_bench/tests/fork.rs")
            .exists());
    }
}
