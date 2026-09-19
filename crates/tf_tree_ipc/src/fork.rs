//! Fork detection — one relaxed load of a counter `pthread_atfork` bumps in the child.
//!
//! A shared arena is mapped `MADV_DONTFORK` (`docs/PHASE2.md` §7.3), yet a fork
//! child inherits the parent's `Tree` and its destructors, which would release
//! OFD locks on inherited descriptions. The child must detect that with no
//! syscall. `Relaxed` suffices: the handler runs in the single-threaded child
//! before `fork` returns.
//! # SAFETY (module invariant)
//!
//! The single `unsafe` block registers a `'static` `extern "C" fn()` child
//! handler (no `prepare`/`parent`), so the pointer the C library retains stays
//! valid. Its body is one `fetch_add` on a `static AtomicU64`: async-signal-safe,
//! lock-free, allocation-free, cannot panic or unwind across the FFI boundary.

use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

// `libc` declares `pthread_atfork` for the BSDs but not `linux_like`.
unsafe extern "C" {
    fn pthread_atfork(
        prepare: Option<extern "C" fn()>,
        parent: Option<extern "C" fn()>,
        child: Option<extern "C" fn()>,
    ) -> libc::c_int;
}

/// Incremented once per `fork()`, in the child.
static FORK_GEN: AtomicU64 = AtomicU64::new(0);

/// Guards the one-time registration.
static ARMED: Once = Once::new();

/// The `pthread_atfork` child handler; must not allocate or take a lock.
extern "C" fn after_fork_in_child() {
    FORK_GEN.fetch_add(1, Ordering::Relaxed);
}

/// Install the fork handler. Idempotent.
///
/// Call **before** any `fork` can happen; arming on first use is too late. A
/// registration failure (`ENOMEM`) is swallowed.
pub fn arm() {
    ARMED.call_once(|| {
        // SAFETY: see the module invariant; the handler type matches exactly.
        let _ = unsafe { pthread_atfork(None, None, Some(after_fork_in_child)) };
    });
}

/// The current fork generation of this process.
///
/// Capture it beside anything holding a shared mapping or OFD lock; a
/// difference means the value belongs to another process — see [`arm`].
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

    /// The cross-fork behaviour is asserted by `tf_tree_bench`'s `fork_child`
    /// (`docs/decisions/0005`); this records the link.
    #[test]
    fn the_cross_fork_behaviour_is_tested_elsewhere() {
        assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tf_tree_bench/tests/fork.rs")
            .exists());
    }
}
