//! The seqlock sample ring — the concurrency core.
//!
//! # SAFETY (module invariant)
//!
//! One of the crate's two `unsafe` islands (the other is [`crate::arena_view`]).
//! Under a `not(loom)` build it reinterprets raw arena bytes as `&[PoseSlot]` /
//! `&[AtomicI64]`. Sound because:
//!
//! * [`PoseSlot`] is `#[repr(C, align(64))]`, exactly 64 bytes (asserted at
//!   compile time), all atomics whose all-zero pattern is valid; the arena is
//!   zeroed and 64-byte aligned.
//! * The stamp arena is a run of `i64`-aligned slots; `AtomicI64` has `i64`'s layout.
//! * The caller ([`crate::arena_view::ArenaView`]) guarantees the region lies
//!   inside the arena and is used by no other typed view.
//!
//! All interior mutation goes through atomics.
//!
//! The `push`/`read_slot` orderings are **normative** (`docs/PHASE1.md`
//! §6.2–§6.3), loom-checked (§10.2); never weaken one because an x86 test
//! passes. The final `head` store dies only against
//! `head_publishes_every_stamp_below_it`.
#![allow(unsafe_code)]

use tf_tree_math::Iso3;

use crate::crash::crash_point;
use crate::error::{EdgeId, LookupError, PushError};
use crate::sync::{fence, spin, AtomicI64, AtomicU32, AtomicU64, Ordering};

/// Maximum consecutive odd-seqlock observations before a read gives up with
/// [`LookupError::SlotContended`].
pub const SEQ_RETRY_LIMIT: u32 = 64;

/// One cacheline of published pose, guarded by a per-slot seqlock.
///
/// `seq` is even when stable, odd mid-write; `data` holds the seven `f64` bit
/// patterns of an [`Iso3`] ([`Iso3::to_bits`]). Stored as `[AtomicU64; 7]`, not
/// `[f64; 7]`: a non-atomic payload read discarded on mismatch is a data race
/// (UB). **Do not replace this with a `memcpy`.**
#[cfg(not(loom))]
#[repr(C, align(64))]
pub struct PoseSlot {
    seq: AtomicU32,
    _pad: u32,
    data: [AtomicU64; 7],
}

// A wire record (`write_frozen` copies it): pin its layout, not just its size.
#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<PoseSlot>() == 64);
    assert!(core::mem::align_of::<PoseSlot>() == 64);
    assert!(core::mem::offset_of!(PoseSlot, seq) == 0);
    assert!(core::mem::offset_of!(PoseSlot, data) == 8);
};

/// Under `loom`, `PoseSlot` holds loom's atomics and carries no `repr`/size guarantee.
#[cfg(loom)]
pub struct PoseSlot {
    seq: AtomicU32,
    data: [AtomicU64; 7],
}

impl PoseSlot {
    /// Test-only: set `seq` to simulate a writer killed mid-publish.
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

/// A borrowed view of one edge's sample ring: head, heartbeat, and the parallel
/// stamp/pose arrays (arena bytes in production, heap arrays under loom).
///
/// # INVARIANT
///
/// `stamps.len() == poses.len()`, a power of two equal to the capacity;
/// [`Self::mask`] derives from `poses.len()`.
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
    /// `wrapping_sub`: a bad ring must fail at the slice bounds check, not at a
    /// second panic site (the fields are `pub`).
    #[inline]
    #[must_use]
    pub fn mask(&self) -> u64 {
        self.capacity().wrapping_sub(1)
    }

    /// How many of the most recent logical indices a reader may safely touch:
    /// `capacity - 1`, not `capacity`, since `head - capacity` is the slot
    /// [`Self::push`] is overwriting. Never configure a one-slot `Capacity`.
    #[inline]
    #[must_use]
    pub fn retained(&self) -> u64 {
        let cap = self.capacity();
        // Branchless `max(cap - 1, 1)` for power-of-two `cap >= 1`.
        cap - u64::from(cap > 1)
    }

    /// The newest published stamp, or `None` if the ring is empty.
    #[inline]
    #[must_use]
    pub fn newest_stamp(&self) -> Option<i64> {
        let h = self.head.load(Ordering::Acquire);
        if h == 0 {
            return None;
        }
        Some(self.stamps[((h - 1) & self.mask()) as usize].load(Ordering::Relaxed))
    }

    /// The oldest stamp still in the ring, or `None` if empty.
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

    /// How many samples the ring holds, `min(head, retained())`; `head` keeps
    /// counting after the ring laps.
    #[inline]
    #[must_use]
    pub fn stored(&self) -> u64 {
        self.head.load(Ordering::Acquire).min(self.retained())
    }

    /// Publish one sample. **Single writer only**, a type-level property of the
    /// owning `Publisher`.
    ///
    /// # Errors
    ///
    /// [`PushError::NonMonotonicStamp`] if `stamp` is older than the edge's
    /// newest (invariant 6); equal stamps are accepted (idempotent replay).
    pub fn push(&self, stamp: i64, iso: &Iso3) -> Result<(), PushError> {
        // Single writer: a Relaxed load of our own head is correct.
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

        // Force the seq odd, never increment (`docs/PHASE2.md` §1 A5): a writer
        // killed mid-publish leaves it odd, and `s | 1` self-heals. The Release
        // fence keeps the payload stores below it.
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

        // Even seq publishes the payload; the head store publishes the sample.
        slot.seq.store(odd.wrapping_add(1), Ordering::Release); // -> even

        // §11.3 `push.after_seq_even_before_head`: slot consistent but below no `head`.
        crash_point!("push.after_seq_even_before_head");

        self.head.store(h + 1, Ordering::Release);

        // A store, not `fetch_add`: single writer (D7); `head` equals the
        // heartbeat at every quiescent point (`0014`), asserted below.
        debug_assert_eq!(
            self.heartbeat.load(Ordering::Relaxed),
            h,
            "heartbeat diverged from head before this push: something other \
             than `push` wrote one of them (see decision 0014)"
        );
        self.heartbeat.store(h + 1, Ordering::Relaxed);
        Ok(())
    }

    /// Read one physical slot under the seqlock, or
    /// [`LookupError::SlotContended`] after [`SEQ_RETRY_LIMIT`] odd observations.
    /// Does not check whether the ring lapped the reader; the bracket search
    /// revalidates.
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

/// Reinterpret zeroed, 64-byte-aligned arena bytes as a `PoseSlot` array.
///
/// # Safety
///
/// `base.add(byte_off)` must be 64-byte aligned and name `len * 64` valid,
/// zeroed, owned bytes that outlive `'a` and are accessed only as `PoseSlot`.
#[cfg(not(loom))]
pub(crate) unsafe fn pose_slots<'a>(base: *mut u8, byte_off: usize, len: usize) -> &'a [PoseSlot] {
    // SAFETY: module invariant; the caller guarantees the region is in-bounds,
    // aligned and typed only as PoseSlot.
    unsafe { core::slice::from_raw_parts(base.add(byte_off).cast::<PoseSlot>(), len) }
}

/// Reinterpret a run of the stamp arena as an `AtomicI64` array.
///
/// # Safety
///
/// `base.add(byte_off)` must be 8-byte aligned and name `len * 8` valid, owned
/// bytes that outlive `'a` and are accessed only as `AtomicI64`.
#[cfg(not(loom))]
pub(crate) unsafe fn stamp_slots<'a>(
    base: *mut u8,
    byte_off: usize,
    len: usize,
) -> &'a [AtomicI64] {
    // SAFETY: module invariant; the caller guarantees the region is in-bounds,
    // aligned and typed only as AtomicI64.
    unsafe { core::slice::from_raw_parts(base.add(byte_off).cast::<AtomicI64>(), len) }
}
