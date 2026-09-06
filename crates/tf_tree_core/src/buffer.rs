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
//! The `push`/`read_slot` protocol carries **normative** ordering annotations
//! (`docs/PHASE1.md` §6.2–§6.3). Every ordering below is load-bearing and is exercised by
//! the loom tests; do not weaken any to `Relaxed` because an x86 test passes.
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

// `PoseSlot` is a wire record too — it is what a peer process reads out of the
// ring and what `write_frozen` copies into a `.tft` — so `size_of` is not a
// layout here either (`edge.rs`'s `EdgeRecord` block carries the argument).
//
// **This is a strictly weaker improvement than the sibling pins and it is worth
// saying which**: unlike `ClaimRecord`'s two diagnostics-only fields, a swap of
// `seq` and `data` here already HAS a guard — the committed fixture test
// `tf_tree::frozen::the_committed_sensor_domain_fixture_reads_and_is_still_tag_one`
// fails with `SlotContended`, measured. That test runs only under
// `just shm-check` (its target carries `required-features = ["shm"]`), catches
// it by a property rather than by the layout, and its own doc says so. These two
// lines replace an accidental runtime guard two recipes away with a
// compile-time one; they do not close a hole.
#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<PoseSlot>() == 64);
    assert!(core::mem::align_of::<PoseSlot>() == 64);
    assert!(core::mem::offset_of!(PoseSlot, seq) == 0);
    assert!(core::mem::offset_of!(PoseSlot, data) == 8);
};

/// Under `loom`, `PoseSlot` holds loom's instrumented atomics and lives on the
/// heap (loom atomics are neither `repr(C)` nor constructible from zeroed bytes),
/// so it carries no `repr`/size guarantee. The `push`/`read_slot` algorithm is
/// byte-for-byte identical across the two definitions.
#[cfg(loom)]
pub struct PoseSlot {
    seq: AtomicU32,
    data: [AtomicU64; 7],
}

impl PoseSlot {
    /// Test-only access to the slot's sequence number.
    ///
    /// `seq` stays private because nothing outside the seqlock protocol may
    /// touch it; these exist so a test can *simulate a writer killed
    /// mid-publish*, which is the one state the protocol has to recover from and
    /// which no in-process API can otherwise produce (see
    /// `stale_odd_seq_from_a_dead_writer_is_healed_by_the_next_push`).
    #[cfg(test)]
    pub(crate) fn set_seq_for_test(&self, v: u32) {
        self.seq.store(v, Ordering::Relaxed);
    }

    /// Read the slot's sequence number. Test-only; see
    /// [`PoseSlot::set_seq_for_test`].
    #[cfg(test)]
    pub(crate) fn seq_for_test(&self) -> u32 {
        self.seq.load(Ordering::Relaxed)
    }

    /// A fresh, stable (`seq == 0`), identity-ish slot. Used to build heap rings
    /// for the loom tests and the wrapped-ring property test; the production
    /// arena views zeroed bytes instead of constructing.
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

/// A borrowed view of one edge's sample ring: its monotone head, its writer
/// heartbeat, and the parallel stamp/pose arrays.
///
/// The same struct backs both worlds. In production the slices are reinterpreted
/// arena bytes (via [`crate::arena_view`]); in the loom tests they borrow
/// heap-allocated arrays. All fields are shared references because every mutation
/// goes through the contained atomics.
///
/// # INVARIANT
///
/// `stamps.len() == poses.len()`, and that length is a power of two equal to the
/// edge's ring capacity.
///
/// **The capacity used to be spelled twice** — as `poses.len()` and as a `pub
/// mask: u64` field this comment then had to assert was `capacity - 1`. Nothing
/// enforced it, on a `pub` field of a `pub` struct in a published crate, and a
/// ring built with `mask = 3` over 8-slot arrays returned a **silently wrong
/// pose**: [`Self::capacity`] and [`Self::retained`] compute a 7-sample window
/// while `stamp_at` masks into 4 slots, so `oldest_stamp` reports a
/// sample it excludes and the sampler interpolates the wrong pair. No error, no
/// panic, and the `debug_assert` inside `push` does not fire. [`Self::mask`] is
/// derived from `poses.len()` now, so the two cannot disagree.
pub struct SampleRing<'a> {
    /// Monotone count of samples ever published (invariant 5). Never masked in
    /// storage, only at access.
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
    /// Derived rather than stored — see the struct's `INVARIANT` for what the
    /// stored version cost.
    ///
    /// **`wrapping_sub` and not `- 1`, and the reason is that the guarantee is
    /// narrower than it looks.** For a ring this crate builds the capacity is a
    /// power of two and therefore non-zero: `ArenaView::ring_bytes` is the one
    /// place that is established, and it returns `None` when
    /// `!cap.is_power_of_two()`, which also rejects `0`. That is a property of
    /// the *constructor*, not of the type — every remaining field of
    /// [`SampleRing`] is `pub`, `poses` included, so a caller outside this
    /// crate can build one over an empty or non-power-of-two slice and no
    /// guarantee here applies to it. `wrapping_sub` is what keeps such a ring
    /// failing the way it always has, at the slice bounds check, instead of
    /// adding a second debug-only panic site on the read path.
    #[inline]
    #[must_use]
    pub fn mask(&self) -> u64 {
        self.capacity().wrapping_sub(1)
    }

    /// How many of the most recent logical indices a reader may safely touch.
    ///
    /// **Not** `capacity`. [`Self::push`] writes logical index `head` into
    /// physical slot `head & mask`; logical index `head - capacity` maps to that
    /// same physical slot, so it is the slot currently being overwritten, not a
    /// retained sample. The readable window is therefore
    /// `[head - capacity + 1, head - 1]` — `capacity - 1` samples. Reading the
    /// lapped slot is what made an in-window query race a `push` and come back
    /// with a fabricated `Extrapolation`.
    ///
    /// A one-slot ring is degenerate (its readable window is empty). It keeps a
    /// window of `1` so a quiescent ring is still readable at all; concurrent
    /// reads of one are guarded only by the per-slot seqlock, which is why
    /// `Capacity` should never be configured that small.
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
    /// Reads `head` with `Acquire` (matching [`Self::sample`]) so the stamp of the
    /// most recently published sample is ordered into view. Used by the plan layer
    /// to resolve `Latest` and `LatestCommon` queries.
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
    /// The mirror of [`Self::newest_stamp`], and it lives here for the same
    /// reason [`Self::retained`] does: the readable window's lower end is
    /// `head - retained()` clamped at zero, and that arithmetic **changed once
    /// already** (reading the lapped slot is what made an in-window query race a
    /// `push`). A copy of it in another crate would not move when this one moves
    /// next.
    ///
    /// Note the asymmetry with `newest_stamp`: this is the oldest sample still
    /// *in the ring*, not the oldest ever pushed. A lapped ring dropped those.
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
    /// **Not** the number ever pushed: that is `head`, which keeps counting
    /// after the ring laps. The two answer different questions ("how big is this
    /// file" vs. "how many did the source produce") and a caller that wants a
    /// rate wants this one over the span [`Self::oldest_stamp`] describes.
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
                return Err(PushError::NonMonotonicStamp { last, got: stamp });
            }
        }
        let idx = (h & self.mask()) as usize;
        let slot = &self.poses[idx];

        // Flip the slot's seqlock to odd (write in progress). The Release fence
        // that follows keeps the payload stores below from being hoisted above
        // this point on a weakly-ordered target.
        //
        // **Force the parity; do not increment** (`docs/PHASE2.md` §1, A5). A
        // writer killed between the two stores below leaves the slot odd
        // forever. Within one process that was unobservable — the crash took the
        // readers with it — but across processes the readers survive, and when
        // the ring laps, an incrementing writer would read the stale odd `s` and
        // land its `s+1` on an *even* value, inverting the protocol for that slot
        // from then on: readers would accept mid-write payloads as published.
        //
        // `s | 1` is idempotent on a stale odd value, so this self-heals. Any
        // reader that saw the stale odd retried without reading, so none can be
        // mid-read holding it.
        let odd = slot.seq.load(Ordering::Relaxed) | 1;
        slot.seq.store(odd, Ordering::Relaxed); // -> odd (idempotent if already)
        fence(Ordering::Release);

        // §11.3 `push.after_seq_odd`: "slot odd, `head` unbumped -> A5 self-heals
        // on next claim". The parity is flipped and its Release fence has run;
        // not one byte of payload has been written. This is A5's own pseudo-code
        // boundary — its snippet has `// ...write stamp and pose data...` on the
        // line after the fence — and it is what separates this site from
        // `after_data_before_seq_even`, which leaves the same seq and head with
        // the payload written.
        crash_point!("push.after_seq_odd");

        self.stamps[idx].store(stamp, Ordering::Relaxed);
        let bits = iso.to_bits();
        for (i, w) in bits.iter().enumerate() {
            slot.data[i].store(*w, Ordering::Relaxed);
        }

        // §11.3 `push.after_data_before_seq_even`: "as above; sample invisible
        // because `head` never moved". Stamp and all seven payload words are in
        // the slot, the seq is still odd, and `head` is untouched — the last
        // instant at which the slot is *torn* as far as a reader is concerned.
        crash_point!("push.after_data_before_seq_even");

        // Back to even publishes the payload; the head store publishes the
        // sample to the bracket search.
        slot.seq.store(odd.wrapping_add(1), Ordering::Release); // -> even

        // §11.3 `push.after_seq_even_before_head`: "sample fully written but
        // unpublished -> invisible, then overwritten". Between the two publishing
        // stores: the slot is consistent and readable under its seqlock, and
        // `head` has not moved, so no correct reader addresses it (the bracket
        // search only ever looks below `head`) and the next push lands on the
        // same physical slot.
        crash_point!("push.after_seq_even_before_head");

        self.head.store(h + 1, Ordering::Release);

        // **A store, not a locked read-modify-write.** The heartbeat counts
        // pushes, and the post-push count is `h + 1`, already in a register:
        // `head` is written here and nowhere else in the workspace and is never
        // reset, and this line is the only writer of `ClaimRecord::heartbeat`,
        // so the two are equal at every quiescent point.
        //
        // The ordering is unchanged — `Relaxed` before and after. What goes away
        // is the atomicity, which bought nothing: the ring is single-writer by
        // construction (invariant 4 / D7), the same guarantee the plain `head`
        // store immediately above already rests on. On x86 `fetch_add` lowers to
        // a `lock`-prefixed instruction whose implicit full barrier drains the
        // store buffer right behind the eight relaxed payload stores above.
        // Measured on `push/single_writer`; the figure and the host are in
        // `0014`, which this comment already cites twice. *No number is written
        // here: this line carried an earlier run's figure, contradicting the
        // record it names as authoritative. `docs/decisions/README.md`'s `0014`
        // row carries the erratum, and it is the only place the superseded pair
        // appears.*
        //
        // This is the cost `counters.rs`'s module doc already rules out for
        // publish-side diagnostics — "a relaxed `fetch_add` on the push path
        // costs ~5-10 ns ... to store something the arena already holds" —
        // applied to the last such `fetch_add` left on the push path.
        //
        // The equality this rests on is asserted rather than left to the prose
        // (`0014` open question 1). Free in release, and it runs under `just
        // loom`, `just miri` and the whole debug test suite — which is where a
        // second writer to `head` or `heartbeat`, or a path that resets one
        // without the other, would first show up. `fetch_add` tolerated such a
        // divergence silently; a store cannot, so the invariant stops being a
        // comment somebody has to re-derive.
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
    /// This does **not** check whether the ring has since lapped the reader — the
    /// bracket search does that revalidation once, after reading both endpoints.
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
            // The Acquire fence stops the payload loads above from being reordered
            // after the re-read of `seq` on a weakly-ordered target. If `seq` is
            // unchanged and even, the payload we read is exactly this version.
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
