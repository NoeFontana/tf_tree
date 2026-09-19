//! The seqlock sample ring — the concurrency core.
//!
//! # SAFETY (module invariant)
//!
//! This module is one of the crate's two `unsafe` islands (the other is
//! [`crate::arena_view`]). Under a production (`not(loom)`) build it reinterprets
//! raw arena bytes as `&[PoseSlot]` / `&[AtomicI64]`. That reinterpretation is
//! sound because:
//!
//! * [`PoseSlot`] is `#[repr(C, align(64))]` and exactly 64 bytes (asserted at
//!   compile time), containing only atomics whose all-zero bit pattern is a valid
//!   value. The arena is zero-initialized and 64-byte aligned, so a zeroed region
//!   is already a valid array of `PoseSlot`.
//! * The stamp arena is a contiguous run of `i64`-sized, `i64`-aligned slots; an
//!   `AtomicI64` has identical layout, and any bit pattern is a valid `i64`.
//! * The caller (via [`crate::arena_view::ArenaView`]) guarantees the byte offset
//!   and length name a region that lies wholly inside the arena and is used by no
//!   other typed view for a different purpose.
//!
//! All *interior mutation* of the reinterpreted memory happens through the
//! atomics, so aliasing the region as `&PoseSlot` from multiple threads is sound.
//!
//! The `push`/`read_slot` orderings are **normative** (`docs/PHASE1.md` §6.2–§6.3)
//! and loom-checked (§10.2 mutation test); never weaken one to `Relaxed` because
//! an x86 test passes. The `head` store at the end of [`SampleRing::push`] dies
//! only against `head_publishes_every_stamp_below_it`.
#![allow(unsafe_code)]

use tf_tree_math::Iso3;

use crate::crash::crash_point;
use crate::error::{EdgeId, LookupError, PushError};
use crate::sync::{fence, spin, AtomicI64, AtomicU32, AtomicU64, Ordering};

/// Maximum consecutive odd-seqlock observations before a read gives up with
/// [`LookupError::SlotContended`]. A single writer holds a slot odd for only a
/// handful of stores, so 64 is astronomically generous.
pub const SEQ_RETRY_LIMIT: u32 = 64;

/// One cacheline of published pose, guarded by a per-slot seqlock.
///
/// `seq` is even when the slot is stable and odd while a write is in progress.
/// `data` holds the seven `f64` bit patterns of an [`Iso3`] (`qw qx qy qz tx ty
/// tz`, see [`Iso3::to_bits`]).
///
/// The pose is stored as `[AtomicU64; 7]` rather than `[f64; 7]` behind an
/// `UnsafeCell` on purpose: the classic seqlock reads the payload non-atomically
/// and discards it on a version mismatch, which is a data race and therefore UB
/// in the Rust memory model even though it works on every real CPU. Relaxed
/// atomic loads compile to the same instruction (`mov` / `ldr`) and make the
/// protocol sound. **Do not replace this with a `memcpy`.**
#[cfg(not(loom))]
#[repr(C, align(64))]
pub struct PoseSlot {
    seq: AtomicU32,
    _pad: u32,
    data: [AtomicU64; 7],
}

// `PoseSlot` is a wire record (peers read it; `write_frozen` copies it), so pin
// its layout, not just its size.
#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<PoseSlot>() == 64);
    assert!(core::mem::align_of::<PoseSlot>() == 64);
    assert!(core::mem::offset_of!(PoseSlot, seq) == 0);
    assert!(core::mem::offset_of!(PoseSlot, data) == 8);
};

/// Under `loom`, `PoseSlot` holds loom's atomics on the heap and carries no
/// `repr`/size guarantee; the algorithm is identical.
#[cfg(loom)]
pub struct PoseSlot {
    seq: AtomicU32,
    data: [AtomicU64; 7],
}

impl PoseSlot {
    /// Test-only: set `seq` to simulate a writer killed mid-publish (see
    /// `stale_odd_seq_from_a_dead_writer_is_healed_by_the_next_push`).
    #[cfg(test)]
    pub(crate) fn set_seq_for_test(&self, v: u32) {
        self.seq.store(v, Ordering::Relaxed);
    }

    /// Test-only: read `seq`.
    #[cfg(test)]
    pub(crate) fn seq_for_test(&self) -> u32 {
        self.seq.load(Ordering::Relaxed)
    }

    /// A fresh, stable (`seq == 0`) slot, for heap rings in tests.
    #[must_use]
    pub fn new() -> PoseSlot {
        #[cfg(not(loom))]
        {
            PoseSlot {
                seq: AtomicU32::new(0),
                _pad: 0,
                data: core::array::from_fn(|_| AtomicU64::new(0)),
            }
        }
        #[cfg(loom)]
        {
            PoseSlot {
                seq: AtomicU32::new(0),
                data: core::array::from_fn(|_| AtomicU64::new(0)),
            }
        }
    }
}

impl Default for PoseSlot {
    fn default() -> Self {
        PoseSlot::new()
    }
}

/// A borrowed view of one edge's sample ring: monotone head, writer heartbeat,
/// and the parallel stamp/pose arrays. Backed by arena bytes in production and
/// heap arrays in loom tests; every mutation goes through the atomics.
///
/// # INVARIANT
///
/// `stamps.len() == poses.len()`, a power of two equal to the ring capacity.
/// [`Self::mask`] is derived from `poses.len()` so the two cannot disagree.
pub struct SampleRing<'a> {
    /// Monotone count of samples ever published (invariant 5); masked only at access.
    pub head: &'a AtomicU64,
    /// Bumped by the writer on every successful push (Phase 2 liveness input).
    pub heartbeat: &'a AtomicU64,
    /// Per-slot stamps, parallel to `poses`.
    pub stamps: &'a [AtomicI64],
    /// Per-slot poses.
    pub poses: &'a [PoseSlot],
    /// The edge this ring belongs to (named by every error it can raise).
    pub edge: EdgeId,
}

impl SampleRing<'_> {
    /// Ring capacity (number of physical slots).
    #[inline]
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.poses.len() as u64
    }

    /// `capacity - 1`; AND a logical index with this to get a physical index.
    ///
    /// `wrapping_sub`: only `ArenaView::ring_bytes` guarantees a non-zero
    /// power-of-two capacity, and the fields are `pub`; a bad ring must fail at
    /// the slice bounds check, not at a second panic site.
    #[inline]
    #[must_use]
    pub fn mask(&self) -> u64 {
        self.capacity().wrapping_sub(1)
    }

    /// How many of the most recent logical indices a reader may safely touch.
    ///
    /// **Not** `capacity`: logical index `head - capacity` maps to the slot
    /// [`Self::push`] is overwriting, so the window is
    /// `[head - capacity + 1, head - 1]`, `capacity - 1` samples. A one-slot ring
    /// keeps a window of `1` and is guarded only by the seqlock; never configure
    /// `Capacity` that small.
    #[inline]
    #[must_use]
    pub fn retained(&self) -> u64 {
        let cap = self.capacity();
        // Branchless `max(cap - 1, 1)` for the power-of-two capacities this ring
        // is built with (`cap >= 1`).
        cap - u64::from(cap > 1)
    }

    /// The newest published stamp, or `None` if the ring is empty.
    ///
    /// Reads `head` with `Acquire` so the newest stamp is ordered into view.
    #[inline]
    #[must_use]
    pub fn newest_stamp(&self) -> Option<i64> {
        let h = self.head.load(Ordering::Acquire);
        if h == 0 {
            return None;
        }
        Some(self.stamps[((h - 1) & self.mask()) as usize].load(Ordering::Relaxed))
    }

    /// The oldest stamp a reader may still touch, or `None` if the ring is
    /// empty.
    ///
    /// The lower end of the readable window (`head - retained()`, clamped at
    /// zero): the oldest sample still *in the ring*, not the oldest ever pushed.
    #[inline]
    #[must_use]
    pub fn oldest_stamp(&self) -> Option<i64> {
        let h = self.head.load(Ordering::Acquire);
        if h == 0 {
            return None;
        }
        let oldest = h.saturating_sub(self.retained());
        Some(self.stamps[(oldest & self.mask()) as usize].load(Ordering::Relaxed))
    }

    /// How many samples this ring currently holds — `min(head, retained())`.
    ///
    /// **Not** the number ever pushed; that is `head`, which keeps counting after
    /// the ring laps.
    #[inline]
    #[must_use]
    pub fn stored(&self) -> u64 {
        self.head.load(Ordering::Acquire).min(self.retained())
    }

    /// Publish one sample. **Single writer only** — exclusivity is a type-level
    /// property of the `Publisher` that owns this ring, not a convention.
    ///
    /// # Errors
    ///
    /// [`PushError::NonMonotonicStamp`] if `stamp` is strictly older than the
    /// edge's newest stamp (invariant 6). Equal stamps are accepted and the newer
    /// value wins (idempotent replay).
    pub fn push(&self, stamp: i64, iso: &Iso3) -> Result<(), PushError> {
        // Single writer: a Relaxed load of our own monotone head is correct.
        let h = self.head.load(Ordering::Relaxed);
        if h > 0 {
            let last = self.stamps[((h - 1) & self.mask()) as usize].load(Ordering::Relaxed);
            if stamp < last {
                return Err(PushError::NonMonotonicStamp {
                    edge: self.edge,
                    last,
                    got: stamp,
                });
            }
        }
        let idx = (h & self.mask()) as usize;
        let slot = &self.poses[idx];

        // Force the seq odd; do not increment (`docs/PHASE2.md` §1, A5). A writer
        // killed mid-publish leaves the slot odd, and an incrementing writer would
        // then land on even and invert the protocol. `s | 1` is idempotent, so
        // this self-heals. The Release fence keeps the payload stores below from
        // hoisting above it.
        let odd = slot.seq.load(Ordering::Relaxed) | 1;
        slot.seq.store(odd, Ordering::Relaxed); // -> odd (idempotent if already)
        fence(Ordering::Release);

        // §11.3 `push.after_seq_odd`: slot odd, no payload written, `head` unbumped.
        crash_point!("push.after_seq_odd");

        self.stamps[idx].store(stamp, Ordering::Relaxed);
        let bits = iso.to_bits();
        for (i, w) in bits.iter().enumerate() {
            slot.data[i].store(*w, Ordering::Relaxed);
        }

        // §11.3 `push.after_data_before_seq_even`: payload written, seq odd, `head` unmoved.
        crash_point!("push.after_data_before_seq_even");

        // Back to even publishes the payload; the head store publishes the
        // sample to the bracket search.
        slot.seq.store(odd.wrapping_add(1), Ordering::Release); // -> even

        // §11.3 `push.after_seq_even_before_head`: slot consistent but below no `head`.
        crash_point!("push.after_seq_even_before_head");

        self.head.store(h + 1, Ordering::Release);

        // A store, not `fetch_add`: the ring is single-writer (invariant 4 / D7) and
        // `head` and the heartbeat are equal at every quiescent point (`0014`),
        // asserted below; the lock-prefixed RMW cost is measured in `0014`.
        debug_assert_eq!(
            self.heartbeat.load(Ordering::Relaxed),
            h,
            "heartbeat diverged from head before this push: something other \
             than `push` wrote one of them (see decision 0014)"
        );
        self.heartbeat.store(h + 1, Ordering::Relaxed);
        Ok(())
    }

    /// Read one physical slot under the seqlock. Returns the consistent pose, or
    /// [`LookupError::SlotContended`] if the slot stayed odd for
    /// [`SEQ_RETRY_LIMIT`] attempts.
    ///
    /// Does **not** check whether the ring has lapped the reader; the bracket
    /// search revalidates once after reading both endpoints.
    ///
    /// # Errors
    ///
    /// [`LookupError::SlotContended`] as above.
    pub fn read_slot(&self, idx: usize) -> Result<Iso3, LookupError> {
        let slot = &self.poses[idx];
        for _ in 0..SEQ_RETRY_LIMIT {
            let s1 = slot.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                spin();
                continue;
            }
            let mut bits = [0u64; 7];
            for (i, b) in bits.iter_mut().enumerate() {
                *b = slot.data[i].load(Ordering::Relaxed);
            }
            // Acquire fence: keeps the payload loads before the `seq` re-read.
            fence(Ordering::Acquire);
            if slot.seq.load(Ordering::Relaxed) == s1 {
                return Ok(Iso3::from_bits(&bits));
            }
        }
        Err(LookupError::SlotContended { edge: self.edge })
    }
}

/// Reinterpret a run of zeroed, 64-byte-aligned arena bytes as a `PoseSlot`
/// array.
///
/// # Safety
///
/// `base.add(byte_off)` must be 64-byte aligned and name `len * 64` valid,
/// zero-initialized, owned bytes that outlive `'a` and are never accessed as any
/// type other than `PoseSlot` for that lifetime.
#[cfg(not(loom))]
pub(crate) unsafe fn pose_slots<'a>(base: *mut u8, byte_off: usize, len: usize) -> &'a [PoseSlot] {
    // SAFETY: module invariant — PoseSlot is repr(C, align(64)), exactly 64
    // bytes, all-zero is a valid instance, and the caller guarantees the region
    // is in-bounds, aligned, and exclusively typed as PoseSlot.
    unsafe { core::slice::from_raw_parts(base.add(byte_off).cast::<PoseSlot>(), len) }
}

/// Reinterpret a run of the stamp arena as an `AtomicI64` array.
///
/// # Safety
///
/// `base.add(byte_off)` must be 8-byte aligned and name `len * 8` valid, owned
/// bytes that outlive `'a` and are never accessed as any type other than
/// `AtomicI64` for that lifetime.
#[cfg(not(loom))]
pub(crate) unsafe fn stamp_slots<'a>(
    base: *mut u8,
    byte_off: usize,
    len: usize,
) -> &'a [AtomicI64] {
    // SAFETY: module invariant — AtomicI64 has the same layout as i64, any bit
    // pattern is a valid i64, and the caller guarantees the region is in-bounds,
    // 8-byte aligned, and exclusively typed as AtomicI64.
    unsafe { core::slice::from_raw_parts(base.add(byte_off).cast::<AtomicI64>(), len) }
}
