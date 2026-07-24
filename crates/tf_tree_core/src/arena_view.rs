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
//!
//! Every `unsafe` block below names which of these invariants it relies on.
#![allow(unsafe_code)]

use tf_tree_arena::{Arena, ArenaHeader};

use crate::buffer::{pose_slots, stamp_slots, SampleRing};
use crate::edge::{ClaimRecord, EdgeRecord};
use crate::error::{EdgeId, FrameError, FrameId};
use crate::frame::{blake3_64, intern_core, FrameRecord};
use crate::sync::{AtomicU16, AtomicU32, AtomicU64};
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

    /// The interning id array (parallel to the hashes; `u32::MAX` = unpublished).
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

    /// Pointer to frame record slot `id` (1-based; slot 0 is the root sentinel).
    fn frame_record_ptr(&self, id: u32) -> *mut FrameRecord {
        let off = self.header.frame_table_off as usize + id as usize * 64;
        // SAFETY: module invariant — the frame table reserves `max_frames` 64-byte
        // records at `frame_table_off`; `id < max_frames` is guaranteed by the
        // interning capacity check, so this pointer is in-bounds and 64-aligned.
        unsafe { self.base.add(off).cast::<FrameRecord>() }
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
            let ptr = self.frame_record_ptr(id);
            // SAFETY: `id` was published (its record was fully written before the
            // `ids` Release store this reader synchronized with via Acquire), and
            // records are append-only, so no writer aliases it. Reading a shared
            // `&FrameRecord` is sound.
            let rec = unsafe { &*ptr };
            rec.name_matches(name)
        };
        let write_record = |id: u32| {
            let ptr = self.frame_record_ptr(id);
            let rec = FrameRecord::for_name(name, hash);
            // SAFETY: this runs only for the unique CAS winner, before the `ids`
            // Release store publishes `id`; no other thread holds a reference to
            // this (append-only) slot yet, so the raw write does not race.
            unsafe { core::ptr::write(ptr, rec) };
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

    /// Read an interned frame record (for name display / diagnostics).
    #[must_use]
    pub fn frame_record(&self, id: FrameId) -> &'a FrameRecord {
        let ptr = self.frame_record_ptr(id.get());
        // SAFETY: a live `FrameId` names a published, append-only record slot in
        // bounds; a shared read does not race any writer.
        unsafe { &*ptr }
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

    /// The claim record for edge `id`.
    #[must_use]
    pub fn claim(&self, id: EdgeId) -> &'a ClaimRecord {
        let off = self.header.claim_table_off as usize + id.get() as usize * 64;
        // SAFETY: module invariant — the claim table reserves `max_edges` 64-byte
        // records at `claim_table_off`; a valid `EdgeId` is in bounds and the
        // record is 64-aligned. All its mutation is atomic, so sharing is sound.
        unsafe { &*self.base.add(off).cast::<ClaimRecord>() }
    }

    /// The edge record for edge `id`.
    #[must_use]
    pub fn edge(&self, id: EdgeId) -> &'a EdgeRecord {
        let off = self.header.edge_table_off as usize + id.get() as usize * 128;
        // SAFETY: module invariant — the edge table reserves `max_edges` 128-byte
        // records at `edge_table_off`; a valid `EdgeId` is in bounds and the
        // record is 64-aligned.
        unsafe { &*self.base.add(off).cast::<EdgeRecord>() }
    }

    /// Write a fresh edge record. Called at declaration time, single-threaded,
    /// before any reader or writer touches the edge.
    ///
    /// # Safety-relevant contract
    ///
    /// The caller must hold the builder mutex and declare each `id` at most once,
    /// before publishing it — no concurrent access to this slot may exist.
    pub fn declare_edge(&self, id: EdgeId, record: EdgeRecord) {
        let off = self.header.edge_table_off as usize + id.get() as usize * 128;
        // SAFETY: declaration is single-threaded and happens before the edge is
        // published; no other reference aliases this in-bounds, 64-aligned slot.
        unsafe {
            let ptr = self.base.add(off).cast::<EdgeRecord>();
            core::ptr::write(ptr, record);
        }
    }

    /// Assemble the [`SampleRing`] for a dynamic edge from its edge record (head,
    /// capacity, region offsets) and claim record (heartbeat).
    ///
    /// # Panics
    ///
    /// Debug-asserts the edge is dynamic (non-zero, power-of-two capacity). The
    /// caller must not request a ring for a static or tombstoned edge.
    #[must_use]
    pub fn ring(&self, id: EdgeId) -> SampleRing<'a> {
        let edge = self.edge(id);
        let claim = self.claim(id);
        let cap = edge.capacity as usize;
        debug_assert!(cap.is_power_of_two(), "ring() on a non-ring edge");

        let stamp_byte_off = self.header.stamp_arena_off as usize + edge.stamp_off as usize * 8;
        let pose_byte_off = self.header.pose_arena_off as usize + edge.pose_off as usize * 64;

        // SAFETY: `declare_edge` sized `stamp_off`/`pose_off`/`capacity` to name a
        // `cap`-slot sub-range wholly inside the stamp/pose arenas; the helpers'
        // own SAFETY contracts (in `crate::buffer`) cover alignment and validity.
        let stamps = unsafe { stamp_slots(self.base, stamp_byte_off, cap) };
        let poses = unsafe { pose_slots(self.base, pose_byte_off, cap) };

        SampleRing {
            head: &edge.head,
            heartbeat: &claim.heartbeat,
            stamps,
            poses,
            mask: (cap as u64) - 1,
            edge: id,
        }
    }
}
