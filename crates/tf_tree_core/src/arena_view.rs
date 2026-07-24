//! Typed, `unsafe` access to the raw arena regions.
//!
//! # SAFETY (module invariant)
//!
//! This module is the crate's second `unsafe` island (the other is
//! [`crate::buffer`]). It reinterprets the flat, zero-initialized, 64-byte
//! aligned arena bytes as the typed records that live in each region, using the
//! byte offsets the [`ArenaHeader`] records. The reinterpretation is sound
//! because:
//!
//! * Every record type ([`FrameRecord`], [`EdgeRecord`], [`ClaimRecord`],
//!   [`PoseSlot`](crate::buffer::PoseSlot)) and every atomic array element has an all-zero bit pattern as
//!   a valid value, and the arena starts fully zeroed.
//! * Region offsets and strides come from the header, which
//!   [`crate::layout`](tf_tree_arena::layout) laid out 64-byte aligned and in
//!   header order, so every typed pointer formed here is in-bounds and correctly
//!   aligned for its element type.
//! * All interior mutation goes through atomics (or, for [`FrameRecord`] /
//!   [`EdgeRecord`] scalar fields, through writes that are ordered *before* the
//!   atomic publish that exposes them), so aliasing a region as a shared
//!   reference from multiple threads is sound.
//! * **Every record index is bounds-checked against the header's `max_frames` /
//!   `max_edges` before a pointer is formed.** [`EdgeId`] is a plain public
//!   `u32` newtype and [`FrameId::new`] accepts any non-zero `u32`, so an
//!   out-of-range id is reachable from perfectly safe caller code (including
//!   from `tf_tree`, which is `#![forbid(unsafe_code)]`). The checked accessors
//!   return `None` rather than forming an out-of-bounds pointer.
//!
//! Every `unsafe` block below names which of these invariants it relies on.
#![allow(unsafe_code)]

use tf_tree_arena::{Arena, ArenaHeader};

use crate::buffer::{pose_slots, stamp_slots, SampleRing};
use crate::edge::{ClaimRecord, EdgeRecord};
use crate::error::{EdgeId, FrameError, FrameId, TopologyError};
use crate::frame::{blake3_64, intern_core, FrameRecord, ID_FAILED, ID_UNPUBLISHED};
use crate::sync::{spin, AtomicU16, AtomicU32, AtomicU64, Ordering};
use crate::topology::{Block, TopologyView};

/// Smallest power of two `>= n` (matching the arena layout's `next_pow2`).
const fn next_pow2(n: usize) -> usize {
    let mut p: usize = 1;
    while p < n {
        p <<= 1;
    }
    p
}

/// A borrowed, typed view over an [`Arena`]'s regions.
///
/// Cheap to construct (it just reads the header offsets) and freely copyable in
/// spirit; it borrows the arena for `'a`.
pub struct ArenaView<'a> {
    base: *mut u8,
    header: &'a ArenaHeader,
}

impl<'a> ArenaView<'a> {
    /// Build a view over `arena`, reading its header.
    #[must_use]
    pub fn new(arena: &'a dyn Arena) -> ArenaView<'a> {
        let base = arena.base();
        // SAFETY: module invariant — the arena base is a validly-initialized
        // `ArenaHeader` (written by `HeapArena::new`), 64-byte aligned, and lives
        // as long as the borrowed `arena`, hence as long as `'a`.
        let header = unsafe { &*base.cast::<ArenaHeader>() };
        ArenaView { base, header }
    }

    /// The arena header.
    #[inline]
    #[must_use]
    pub fn header(&self) -> &'a ArenaHeader {
        self.header
    }

    // ---- frame interning -------------------------------------------------

    /// The interning hash array (`next_pow2(2 * max_frames)` slots).
    fn frame_hashes(&self) -> &'a [AtomicU64] {
        let slots = next_pow2(2 * self.header.max_frames as usize);
        let off = self.header.frame_hash_off as usize;
        // SAFETY: module invariant — the frame-hash region begins at
        // `frame_hash_off`, is 64-byte aligned, and reserves `slots * (8 + 4)`
        // bytes; the first `slots * 8` are the `AtomicU64` hash array.
        unsafe { core::slice::from_raw_parts(self.base.add(off).cast::<AtomicU64>(), slots) }
    }

    /// The interning id array (parallel to the hashes; `0` = unpublished, see
    /// [`ID_UNPUBLISHED`]).
    fn frame_ids(&self) -> &'a [AtomicU32] {
        let slots = next_pow2(2 * self.header.max_frames as usize);
        let off = self.header.frame_hash_off as usize + slots * 8;
        // SAFETY: module invariant — the id array follows the hash array within
        // the same region; `off` is 8-byte aligned (hashes are `slots * 8` bytes)
        // and names `slots` `AtomicU32`.
        unsafe { core::slice::from_raw_parts(self.base.add(off).cast::<AtomicU32>(), slots) }
    }

    fn frame_count(&self) -> &'a AtomicU32 {
        &self.header.frame_count
    }

    /// Pointer to frame record slot `id` (1-based; slot 0 is the root sentinel),
    /// or `None` if `id` is out of range for this arena.
    fn frame_record_ptr(&self, id: u32) -> Option<*mut FrameRecord> {
        if id >= self.header.max_frames {
            return None;
        }
        let off = self.header.frame_table_off as usize + id as usize * 64;
        // SAFETY: module invariant — the frame table reserves `max_frames` 64-byte
        // records at `frame_table_off`, and `id < max_frames` was just checked, so
        // this pointer is in-bounds and 64-aligned.
        Some(unsafe { self.base.add(off).cast::<FrameRecord>() })
    }

    /// Byte offset of record `id` in a table of `stride`-byte records starting at
    /// `table_off`, or `None` if `id` is out of range for this arena's edge table.
    #[inline]
    fn edge_slot_off(&self, id: EdgeId, table_off: u32, stride: usize) -> Option<usize> {
        if id.get() >= self.header.max_edges {
            return None;
        }
        Some(table_off as usize + id.get() as usize * stride)
    }

    /// Intern `name`, returning its stable [`FrameId`]. Idempotent: the same name
    /// always maps to the same id, even across concurrent interners (loom-tested).
    ///
    /// # Errors
    ///
    /// [`FrameError::FrameHashCollision`] on a 64-bit hash collision with a
    /// different name; [`FrameError::CapacityExceeded`] when the table is full.
    pub fn intern(&self, name: &str) -> Result<FrameId, FrameError> {
        let hash = blake3_64(name);
        let hashes = self.frame_hashes();
        let ids = self.frame_ids();
        let count = self.frame_count();
        // Usable ids are 1..max_frames (slot 0 is the root sentinel), so the
        // interned-frame capacity is one less than the table slot count.
        let capacity = self.header.max_frames.saturating_sub(1);

        let name_matches = |id: u32| -> bool {
            match self.frame_record_ptr(id) {
                // SAFETY: `id` is in bounds (checked by `frame_record_ptr`) and was
                // published (its record was fully written before the `ids` Release
                // store this reader synchronized with via Acquire); records are
                // append-only, so no writer aliases it. Reading a shared
                // `&FrameRecord` is sound.
                Some(ptr) => unsafe { &*ptr }.name_matches(name),
                None => false,
            }
        };
        let write_record = |id: u32| {
            if let Some(ptr) = self.frame_record_ptr(id) {
                let rec = FrameRecord::for_name(name, hash);
                // SAFETY: `id < max_frames` (checked), and this runs only for the
                // unique CAS winner, before the `ids` Release store publishes `id`;
                // no other thread holds a reference to this (append-only) slot yet,
                // so the raw write does not race.
                unsafe { core::ptr::write(ptr, rec) };
            }
        };

        let id = intern_core(
            hashes,
            ids,
            count,
            capacity,
            hash,
            name_matches,
            write_record,
        )?;
        FrameId::new(id).ok_or(FrameError::CapacityExceeded)
    }

    /// Look up an already-interned frame by name **without** creating one.
    ///
    /// Returns `Ok(Some(id))` if `name` was previously interned, `Ok(None)` if it
    /// was never interned. Unlike [`Self::intern`] this never inserts, so the
    /// read-only lookup path (`tree.lookup`) can distinguish "unknown frame" from
    /// "known but disconnected".
    ///
    /// # Errors
    ///
    /// [`FrameError::FrameHashCollision`] if a different name occupies this hash.
    pub fn find_frame(&self, name: &str) -> Result<Option<FrameId>, FrameError> {
        let hash = blake3_64(name);
        let hashes = self.frame_hashes();
        let ids = self.frame_ids();
        let mask = (hashes.len() - 1) as u64;
        let mut i = (hash & mask) as usize;
        for _ in 0..hashes.len() {
            let cur = hashes[i].load(Ordering::Acquire);
            if cur == 0 {
                return Ok(None); // reached an empty slot: name was never interned
            }
            if cur == hash {
                // Wait for the winning interner to publish the id (Phase 2 may have
                // a concurrent writer mid-intern; costs nothing in Phase 1).
                let id = loop {
                    let id = ids[i].load(Ordering::Acquire);
                    if id != ID_UNPUBLISHED {
                        break id;
                    }
                    spin();
                };
                if id == ID_FAILED {
                    // An interner claimed this slot and then lost the capacity
                    // race: the name was never actually interned.
                    return Ok(None);
                }
                let matches = match FrameId::new(id).and_then(|f| self.frame_record(f)) {
                    Some(rec) => rec.name_matches(name),
                    None => false,
                };
                return if matches {
                    Ok(FrameId::new(id))
                } else {
                    Err(FrameError::FrameHashCollision { hash })
                };
            }
            i = (i + 1) & (mask as usize);
        }
        Ok(None)
    }

    /// Read an interned frame record (for name display / diagnostics), or `None`
    /// if `id` is out of range for this arena.
    #[must_use]
    pub fn frame_record(&self, id: FrameId) -> Option<&'a FrameRecord> {
        let ptr = self.frame_record_ptr(id.get())?;
        // SAFETY: `id.get() < max_frames` (checked by `frame_record_ptr`); a live
        // `FrameId` names a published, append-only record slot, and a shared read
        // does not race any writer.
        Some(unsafe { &*ptr })
    }

    // ---- topology --------------------------------------------------------

    fn topo_block(&self, index: usize) -> Block<'a> {
        let mf = self.header.max_frames as usize;
        let block_off =
            self.header.topo_block_off as usize + index * self.header.topo_block_stride as usize;
        // Within a block: [parent: u32; mf], [edge_of_child: u32; mf], [depth:
        // u16; mf]. The two u32 arrays come first so both stay 4-byte aligned for
        // any `mf`; depth (u16) trails at `+ mf * 8`.
        // SAFETY: module invariant — each block reserves `align64(mf * 10)` bytes
        // at `topo_block_off + index * stride`, block start is 64-aligned; parent
        // is `mf` u32 at offset 0, edge_of_child is `mf` u32 at `+ mf*4`, depth is
        // `mf` u16 at `+ mf*8` — all in-bounds and correctly aligned.
        let parent = unsafe {
            core::slice::from_raw_parts(self.base.add(block_off).cast::<AtomicU32>(), mf)
        };
        let edge_of_child = unsafe {
            core::slice::from_raw_parts(self.base.add(block_off + mf * 4).cast::<AtomicU32>(), mf)
        };
        let depth = unsafe {
            core::slice::from_raw_parts(self.base.add(block_off + mf * 8).cast::<AtomicU16>(), mf)
        };
        Block {
            parent,
            edge_of_child,
            depth,
        }
    }

    /// A view over the topology seqlock and both double-buffered blocks.
    #[must_use]
    pub fn topology(&self) -> TopologyView<'a> {
        TopologyView::new(
            &self.header.topo_generation,
            &self.header.topo_active,
            [self.topo_block(0), self.topo_block(1)],
            self.header.max_frames,
        )
    }

    // ---- edges & claims --------------------------------------------------

    /// The claim record for edge `id`, or `None` if `id` is out of range.
    #[must_use]
    pub fn claim(&self, id: EdgeId) -> Option<&'a ClaimRecord> {
        let off = self.edge_slot_off(id, self.header.claim_table_off, 64)?;
        // SAFETY: module invariant — the claim table reserves `max_edges` 64-byte
        // records at `claim_table_off`, and `id < max_edges` was just checked, so
        // the record is in bounds and 64-aligned. All its mutation is atomic, so
        // sharing it across threads is sound.
        Some(unsafe { &*self.base.add(off).cast::<ClaimRecord>() })
    }

    /// The edge record for edge `id`, or `None` if `id` is out of range.
    #[must_use]
    pub fn edge(&self, id: EdgeId) -> Option<&'a EdgeRecord> {
        let off = self.edge_slot_off(id, self.header.edge_table_off, 128)?;
        // SAFETY: module invariant — the edge table reserves `max_edges` 128-byte
        // records at `edge_table_off`, and `id < max_edges` was just checked, so
        // the record is in bounds and 64-aligned.
        Some(unsafe { &*self.base.add(off).cast::<EdgeRecord>() })
    }

    /// The [`SampleRing`] for a dynamic edge, assembled from its edge record
    /// (head, capacity, region offsets) and claim record (heartbeat).
    ///
    /// Returns `None` if `id` is out of range, or if the edge is not a ring —
    /// a static or tombstoned edge has `capacity == 0`, which is not a power of
    /// two, so there is no ring to build.
    #[must_use]
    pub fn ring(&self, id: EdgeId) -> Option<SampleRing<'a>> {
        let edge = self.edge(id)?;
        let claim = self.claim(id)?;
        self.ring_of(id, edge, claim)
    }

    /// Everything the sampling hot path needs for one edge — its interpolation
    /// discriminant and its ring — resolved behind a single bounds check.
    ///
    /// Returns `None` under exactly the conditions [`Self::ring`] does.
    #[must_use]
    pub fn sampler(&self, id: EdgeId) -> Option<(u8, SampleRing<'a>)> {
        let edge = self.edge(id)?;
        let claim = self.claim(id)?;
        Some((edge.interp, self.ring_of(id, edge, claim)?))
    }

    /// Assemble a ring from already-resolved records. `None` if `edge` is not a
    /// dynamic ring.
    #[inline]
    fn ring_of(
        &self,
        id: EdgeId,
        edge: &'a EdgeRecord,
        claim: &'a ClaimRecord,
    ) -> Option<SampleRing<'a>> {
        let cap = edge.capacity as usize;
        if !cap.is_power_of_two() {
            return None;
        }

        let stamp_byte_off = self.header.stamp_arena_off as usize + edge.stamp_off as usize * 8;
        let pose_byte_off = self.header.pose_arena_off as usize + edge.pose_off as usize * 64;

        // SAFETY: `ArenaBuilder::declare_edge` sized `stamp_off`/`pose_off`/
        // `capacity` to name a `cap`-slot sub-range wholly inside the stamp/pose
        // arenas; the helpers' own SAFETY contracts (in `crate::buffer`) cover
        // alignment and validity.
        let stamps = unsafe { stamp_slots(self.base, stamp_byte_off, cap) };
        let poses = unsafe { pose_slots(self.base, pose_byte_off, cap) };

        Some(SampleRing {
            head: &edge.head,
            heartbeat: &claim.heartbeat,
            stamps,
            poses,
            mask: (cap as u64) - 1,
            edge: id,
        })
    }
}

/// Exclusive, construction-time access to an arena.
///
/// Writing an [`EdgeRecord`] is a raw, non-atomic write of a whole 128-byte
/// record: it is sound only when nothing else can observe the slot. Rather than
/// leave that as a comment on a safe `pub fn` — where any caller, including the
/// `#![forbid(unsafe_code)]` facade, could violate it — the capability is gated
/// behind an `&mut` borrow of the arena. Holding one *proves* no other
/// [`ArenaView`] exists, because a shared view borrows the same arena.
///
/// Hand out shared views for the rest of construction with [`Self::view`].
pub struct ArenaBuilder<'a> {
    arena: &'a mut dyn Arena,
}

impl<'a> ArenaBuilder<'a> {
    /// Take exclusive access to `arena` for declaration-time writes.
    #[must_use]
    pub fn new(arena: &'a mut dyn Arena) -> ArenaBuilder<'a> {
        ArenaBuilder { arena }
    }

    /// A shared view over the same arena, borrowed from this builder (so it
    /// cannot coexist with a [`Self::declare_edge`] call).
    #[must_use]
    pub fn view(&self) -> ArenaView<'_> {
        ArenaView::new(self.arena)
    }

    /// Write a fresh edge record into slot `id`.
    ///
    /// # Errors
    ///
    /// [`TopologyError::CapacityExceeded`] if `id` is out of range for this
    /// arena's edge table.
    pub fn declare_edge(&mut self, id: EdgeId, record: EdgeRecord) -> Result<(), TopologyError> {
        let view = ArenaView::new(self.arena);
        let off = view
            .edge_slot_off(id, view.header.edge_table_off, 128)
            .ok_or(TopologyError::CapacityExceeded)?;
        let base = view.base;
        // SAFETY: `id < max_edges` (checked above), so the slot is in bounds and
        // 64-aligned. `&mut self` proves this builder holds the arena exclusively,
        // so no `ArenaView` — and therefore no `&EdgeRecord` — aliases the slot
        // while the raw write happens.
        unsafe {
            core::ptr::write(base.add(off).cast::<EdgeRecord>(), record);
        }
        Ok(())
    }
}
