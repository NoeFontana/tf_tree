//! Atomics abstraction: one import surface for both the production build and the
//! `loom` model-checking build.
//!
//! Every concurrency primitive in this crate imports its atomics from
//! `crate::sync`, **never** from `core::sync::atomic` directly. Under a normal
//! build these are the real `core` atomics; under `--cfg loom` they are
//! `loom`'s instrumented atomics, which the model checker uses to explore
//! interleavings. The publish/read/claim/intern algorithms are written once
//! against this surface and compile unchanged in both modes.

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{fence, AtomicI64, AtomicU32, AtomicU64, Ordering};

// `AtomicU16` backs the topology `depth` field (2 bytes/frame within the
// 10-byte-per-frame topology block: parent u32 + edge_of_child u32 + depth u16).
// It is only used by the production arena view, which is itself
// `#[cfg(not(loom))]`, so it is not re-exported under loom (loom need not model
// it — the topology loom test uses a bespoke wider model).
#[cfg(not(loom))]
pub(crate) use core::sync::atomic::AtomicU16;

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{fence, AtomicI64, AtomicU32, AtomicU64, Ordering};

/// A spin hint that yields to the model checker under `loom` and emits a plain
/// CPU spin hint otherwise.
///
/// Unbounded waits (the interning publish-then-spin, the topology odd-generation
/// retry) must call this so `loom` schedules the thread they are waiting on
/// rather than spinning forever inside a single interleaving.
#[cfg(not(loom))]
#[inline]
pub(crate) fn spin() {
    core::hint::spin_loop();
}

/// See the `not(loom)` variant.
#[cfg(loom)]
#[inline]
pub(crate) fn spin() {
    loom::thread::yield_now();
}
