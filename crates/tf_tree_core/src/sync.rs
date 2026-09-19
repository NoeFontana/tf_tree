//! Atomics abstraction: every concurrency primitive imports its atomics from
//! `crate::sync`, never `core::sync::atomic`, so the same code compiles against
//! real atomics or `loom`'s (`--cfg loom`).

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{fence, AtomicI64, AtomicU32, AtomicU64, Ordering};

// `AtomicU16` backs the topology `depth` field; production arena view only, not modelled by loom.
#[cfg(not(loom))]
pub(crate) use core::sync::atomic::AtomicU16;

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{fence, AtomicI64, AtomicU32, AtomicU64, Ordering};

/// Spin hint that yields to the model checker under `loom`. Every wait on
/// another thread must call it so loom schedules the awaited thread.
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
