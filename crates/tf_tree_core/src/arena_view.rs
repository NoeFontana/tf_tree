//! Typed, `unsafe` access to the raw arena regions.
//!
//! # SAFETY (module invariant)
//!
//! The crate's second `unsafe` island (the other is [`crate::buffer`]): it
//! reinterprets the zeroed, 64-byte aligned arena bytes as typed records at the
//! offsets [`ArenaHeader`] records. Sound because:
//!
//! * Every record type and atomic array element is valid all-zero.
//! * Offsets and strides come from the header, 64-aligned by
//!   [`crate::layout`](tf_tree_arena::layout), so pointers are in-bounds and aligned.
//! * Interior mutation is atomic (or ordered before the atomic publish).
//! * **Every record index is bounds-checked against `max_frames` / `max_edges`
//!   before a pointer is formed**: [`EdgeId`] and [`FrameId::new`] accept
//!   out-of-range values from safe code.
//!
//! Every `unsafe` block names the invariant it relies on.
#![allow(unsafe_code)]

use tf_tree_arena::{Arena, ArenaHeader};

use crate::buffer::{pose_slots, stamp_slots, SampleRing};
use crate::counters::{EdgeCounters, ParticipantCounters};
use crate::edge::{ClaimRecord, EdgeRecord};
use crate::error::{EdgeId, FrameError, FrameId, TopologyError};
use crate::frame::{blake3_64, find_core, intern_core, FrameRecord, InternTable, CLAIM_UNRECORDED};
use crate::participant::{state_of, ParticipantRecord, ParticipantTable, LIVE};
use crate::sync::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use crate::topology::{Block, TopologyView};

/// Caller-supplied liveness predicate for A8's takeover and claim reaping
/// (`docs/PHASE2.md` §6.2); injected because this crate is `no_std`.
///
/// **Must fail safe:** return `true` when it cannot tell. Arguments: the
/// participant slot and its record.
pub type LivenessFn = dyn Fn(u32, &ParticipantRecord) -> bool;

/// Smallest power of two `>= n`.
const fn next_pow2(n: usize) -> usize {
    let mut p: usize = 1;
    while p < n {
        p <<= 1;
    }
    p
}

/// A borrowed, typed view over an [`Arena`]'s regions.
pub struct ArenaView<'a> {
    base: *mut u8,
    header: &'a ArenaHeader,
    /// A8: this caller's participant slot **+ 1**, or [`CLAIM_UNRECORDED`].
    me: u32,
    /// A8's liveness predicate; `None` means "assume alive" (no takeover).
    is_alive: Option<&'a LivenessFn>,
    /// Whether the mapping is writable. Default `false` is load-bearing: a
    /// read-only consumer (D18) faults on any write.
    writable: bool,
}

impl<'a> ArenaView<'a> {
    /// Build a view over `arena`, reading its header. Anonymous, no liveness
    /// source; add both with [`Self::as_participant`] and [`Self::with_liveness`].
    #[must_use]
    pub fn new(arena: &'a dyn Arena) -> ArenaView<'a> {
        let base = arena.base();
        // SAFETY: module invariant — the base is a valid, 64-aligned
        // `ArenaHeader` living as long as the borrowed `arena`.
        let header = unsafe { &*base.cast::<ArenaHeader>() };
        ArenaView {
            base,
            header,
            me: CLAIM_UNRECORDED,
            is_alive: None,
            writable: false,
        }
    }

    /// Declare the mapping writable; only a `PROT_WRITE` caller may (a false claim faults).
    #[must_use]
    pub fn writable(mut self, yes: bool) -> ArenaView<'a> {
        self.writable = yes;
        self
    }

    /// Whether the mapping permits writes.
    #[must_use]
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Identify this view as participant `slot` (`docs/PHASE2.md` §1 A6/A8).
    /// Out of range (including `u32::MAX`) leaves it anonymous; a rescuer must
    /// be identified.
    #[must_use]
    pub fn as_participant(mut self, slot: u32) -> ArenaView<'a> {
        self.me = if slot < self.header.max_participants {
            slot + 1
        } else {
            CLAIM_UNRECORDED
        };
        self
    }

    /// Attach the predicate deciding whether an unpublished claimant is dead
    /// ([`LivenessFn`]). Without one, a `LIVE` slot is always believed.
    #[must_use]
    pub fn with_liveness(mut self, is_alive: &'a LivenessFn) -> ArenaView<'a> {
        self.is_alive = Some(is_alive);
        self
    }

    /// The participant slot this view interns as, or `None` if anonymous.
    #[must_use]
    pub fn interning_identity(&self) -> Option<u32> {
        if self.me == CLAIM_UNRECORDED {
            None
        } else {
            Some(self.me - 1)
        }
    }

    /// Whether this view can decide a claimant died (an identity alone is inert).
    #[must_use]
    pub fn has_liveness_source(&self) -> bool {
        self.is_alive.is_some()
    }

    /// The arena header.
    #[inline]
    #[must_use]
    pub fn header(&self) -> &'a ArenaHeader {
        self.header
    }

    /// The interning hashes (`next_pow2(2 * max_frames)` slots).
    pub(crate) fn frame_hashes(&self) -> &'a [AtomicU64] {
        let slots = next_pow2(2 * self.header.max_frames as usize);
        let off = self.header.frame_hash_off as usize;
        // SAFETY: module invariant; the first `slots * 8` bytes of the region are the hashes.
        unsafe { core::slice::from_raw_parts(self.base.add(off).cast::<AtomicU64>(), slots) }
    }

    /// The interning ids (`0` = [`crate::frame::ID_UNPUBLISHED`]).
    pub(crate) fn frame_ids(&self) -> &'a [AtomicU32] {
        let slots = next_pow2(2 * self.header.max_frames as usize);
        let off = self.header.frame_hash_off as usize + slots * 8;
        // SAFETY: module invariant; the ids follow the hashes, 8-aligned.
        unsafe { core::slice::from_raw_parts(self.base.add(off).cast::<AtomicU32>(), slots) }
    }

    /// **A8**: the claiming interner per slot (slot + 1, or [`CLAIM_UNRECORDED`]).
    pub(crate) fn frame_claiming(&self) -> &'a [AtomicU32] {
        let slots = next_pow2(2 * self.header.max_frames as usize);
        let off = self.header.frame_hash_off as usize + slots * (8 + 4);
        // SAFETY: module invariant; the last `slots * 4` of `slots * 16`, 4-aligned.
        unsafe { core::slice::from_raw_parts(self.base.add(off).cast::<AtomicU32>(), slots) }
    }

    fn intern_table(&self) -> InternTable<'a> {
        InternTable {
            hashes: self.frame_hashes(),
            ids: self.frame_ids(),
            claiming: self.frame_claiming(),
            frame_count: &self.header.frame_count,
            // Ids are 1..max_frames (0 is the root sentinel).
            capacity: self.header.max_frames.saturating_sub(1),
        }
    }

    /// A8's claimant-liveness test for a `claiming` entry (slot + 1): the slot
    /// reads `LIVE` and the [`LivenessFn`] agrees; unresolvable counts as alive
    /// (`docs/PHASE2.md` §6.2).
    fn claimant_alive(&self) -> impl Fn(u32) -> bool + '_ {
        move |owner: u32| {
            if owner == CLAIM_UNRECORDED {
                return true; // nobody named: not ours to judge
            }
            match self.participants().get(owner - 1) {
                None => true, // out of range for this arena: cannot judge
                Some(rec) => {
                    state_of(rec.state.load(Ordering::Acquire)) == LIVE
                        && self.is_alive.is_none_or(|f| f(owner - 1, rec))
                }
            }
        }
    }

    /// Pointer to frame record `id` (slot 0 is the root sentinel), or `None` if out of range.
    fn frame_record_ptr(&self, id: u32) -> Option<*mut FrameRecord> {
        if id >= self.header.max_frames {
            return None;
        }
        let off = self.header.frame_table_off as usize + id as usize * 64;
        // SAFETY: module invariant; `id < max_frames` was just checked.
        Some(unsafe { self.base.add(off).cast::<FrameRecord>() })
    }

    /// Byte offset of edge record `id` in a `stride`-byte table, or `None` if out of range.
    #[inline]
    fn edge_slot_off(&self, id: EdgeId, table_off: u32, stride: usize) -> Option<usize> {
        if id.get() >= self.header.max_edges {
            return None;
        }
        Some(table_off as usize + id.get() as usize * stride)
    }

    /// Intern `name`, returning its stable [`FrameId`]; idempotent, even across
    /// concurrent interners. Takes over a dead interner's entry only with
    /// [`Self::as_participant`] and [`Self::with_liveness`] (A8).
    /// # Errors
    ///
    /// [`FrameError::FrameHashCollision`] on a 64-bit hash collision with a
    /// different name; [`FrameError::CapacityExceeded`] when the table is full.
    pub fn intern(&self, name: &str) -> Result<FrameId, FrameError> {
        let hash = blake3_64(name);
        let table = self.intern_table();

        let name_matches = |id: u32| -> bool {
            match self.frame_record_ptr(id) {
                // SAFETY: `id` is in bounds and published (Acquired `ids` store); append-only.
                Some(ptr) => unsafe { &*ptr }.name_matches(name),
                None => false,
            }
        };
        let write_record = |id: u32| {
            if let Some(ptr) = self.frame_record_ptr(id) {
                let rec = FrameRecord::for_name(name, hash);
                // SAFETY: `id < max_frames`; only the unique CAS winner, before the `ids` store.
                unsafe { core::ptr::write(ptr, rec) };
            }
        };

        let id = intern_core(
            &table,
            hash,
            self.me,
            self.claimant_alive(),
            name_matches,
            write_record,
        )?;
        FrameId::new(id).ok_or(FrameError::CapacityExceeded)
    }

    /// Look up an already-interned frame without creating one: `Ok(None)` if
    /// never interned, or if its interner died before publishing (A8).
    ///
    /// # Errors
    ///
    /// [`FrameError::FrameHashCollision`] if a different name occupies this hash.
    /// [`FrameError::InternContended`] if an anonymous claimant is mid-publish
    /// past the reader's wait.
    pub fn find_frame(&self, name: &str) -> Result<Option<FrameId>, FrameError> {
        let hash = blake3_64(name);
        let table = self.intern_table();
        let name_matches = |id: u32| -> bool {
            match FrameId::new(id).and_then(|f| self.frame_record(f)) {
                Some(rec) => rec.name_matches(name),
                None => false,
            }
        };
        let id = find_core(&table, hash, self.claimant_alive(), name_matches)?;
        Ok(id.and_then(FrameId::new))
    }

    /// An interned frame record, or `None` if `id` is out of range.
    #[must_use]
    pub fn frame_record(&self, id: FrameId) -> Option<&'a FrameRecord> {
        let ptr = self.frame_record_ptr(id.get())?;
        // SAFETY: `id.get() < max_frames`; published, append-only record.
        Some(unsafe { &*ptr })
    }

    fn topo_block(&self, index: usize) -> Block<'a> {
        let mf = self.header.max_frames as usize;
        let block_off =
            self.header.topo_block_off as usize + index * self.header.topo_block_stride as usize;
        // Module invariant: `TOPO_BLOCKS` blocks of `align64(mf * 12)` bytes
        // (`docs/PHASE1.md` §4.3); `index < TOPO_BLOCKS` since only `topology()` calls this.
        //
        // SAFETY: `parent` is bytes `0..mf*4`, 4-aligned.
        let parent = unsafe {
            core::slice::from_raw_parts(self.base.add(block_off).cast::<AtomicU32>(), mf)
        };
        // SAFETY: `edge_of_child` is bytes `mf*4..mf*8`, 4-aligned.
        let edge_of_child = unsafe {
            core::slice::from_raw_parts(self.base.add(block_off + mf * 4).cast::<AtomicU32>(), mf)
        };
        // SAFETY: `depth` is bytes `mf*8..mf*10`, 2-aligned.
        let depth = unsafe {
            core::slice::from_raw_parts(self.base.add(block_off + mf * 8).cast::<AtomicU16>(), mf)
        };
        Block {
            parent,
            edge_of_child,
            depth,
        }
    }

    /// A view over the packed topology word and every block.
    #[must_use]
    pub fn topology(&self) -> TopologyView<'a> {
        TopologyView::new(
            &self.header.topo,
            core::array::from_fn(|i| self.topo_block(i)),
            self.header.max_frames,
        )
    }

    /// The participant table (`docs/PHASE2.md` §1, A6).
    #[must_use]
    pub fn participants(&self) -> ParticipantTable<'a> {
        let n = self.header.max_participants as usize;
        let off = self.header.participant_table_off as usize;
        // SAFETY: module invariant; `max_participants` records, all fields atomic.
        let slots = unsafe {
            core::slice::from_raw_parts(self.base.add(off).cast::<ParticipantRecord>(), n)
        };
        ParticipantTable::new(slots)
    }

    /// The claim record for edge `id`, or `None` if out of range.
    #[must_use]
    pub fn claim(&self, id: EdgeId) -> Option<&'a ClaimRecord> {
        let off = self.edge_slot_off(id, self.header.claim_table_off, 64)?;
        // SAFETY: module invariant; `id < max_edges` checked; atomic.
        Some(unsafe { &*self.base.add(off).cast::<ClaimRecord>() })
    }

    /// The per-edge diagnostic counters for edge `id` (`docs/PHASE5.md` §5.2),
    /// or `None` if out of range. The region exists with or without `counters` (D34).
    #[must_use]
    pub fn edge_counters(&self, id: EdgeId) -> Option<&'a EdgeCounters> {
        let off = self.edge_slot_off(id, self.header.edge_counters_off, 128)?;
        // SAFETY: module invariant; `id < max_edges` checked; atomic.
        Some(unsafe { &*self.base.add(off).cast::<EdgeCounters>() })
    }

    /// The per-participant diagnostic counters for `slot`, or `None` if out of range.
    #[must_use]
    pub fn participant_counters(&self, slot: u32) -> Option<&'a ParticipantCounters> {
        if slot >= self.header.max_participants {
            return None;
        }
        let off = self.header.participant_counters_off as usize + slot as usize * 128;
        // SAFETY: as above, against `max_participants`.
        Some(unsafe { &*self.base.add(off).cast::<ParticipantCounters>() })
    }

    /// The edge record for edge `id`, or `None` if out of range.
    #[must_use]
    pub fn edge(&self, id: EdgeId) -> Option<&'a EdgeRecord> {
        let off = self.edge_slot_off(id, self.header.edge_table_off, 128)?;
        // SAFETY: module invariant; `id < max_edges` checked.
        Some(unsafe { &*self.base.add(off).cast::<EdgeRecord>() })
    }

    /// The [`SampleRing`] for a dynamic edge, or `None` if out of range, static
    /// or tombstoned.
    #[must_use]
    pub fn ring(&self, id: EdgeId) -> Option<SampleRing<'a>> {
        let edge = self.edge(id)?;
        let claim = self.claim(id)?;
        self.ring_of(id, edge, claim)
    }

    /// An edge's interpolation discriminant and ring behind one bounds check;
    /// `None` exactly when [`Self::ring`] is.
    #[must_use]
    pub fn sampler(&self, id: EdgeId) -> Option<(u8, SampleRing<'a>)> {
        let edge = self.edge(id)?;
        let claim = self.claim(id)?;
        Some((edge.interp, self.ring_of(id, edge, claim)?))
    }

    /// The `(offset, len)` byte extents of a dynamic edge's rings, or `None`
    /// exactly when [`Self::ring`] is; bytes because a `&[T]` over a region
    /// another process writes would race (`docs/PHASE2.md` §7.1).
    #[must_use]
    pub fn ring_extents(&self, id: EdgeId) -> Option<[(usize, usize); 2]> {
        let edge = self.edge(id)?;
        let (stamp_byte_off, pose_byte_off, cap) = self.ring_bytes(edge)?;
        Some([(stamp_byte_off, cap * 8), (pose_byte_off, cap * 64)])
    }

    #[inline]
    fn ring_of(
        &self,
        id: EdgeId,
        edge: &'a EdgeRecord,
        claim: &'a ClaimRecord,
    ) -> Option<SampleRing<'a>> {
        let (stamp_byte_off, pose_byte_off, cap) = self.ring_bytes(edge)?;

        // SAFETY: `ring_bytes` proved the stamp and pose ranges lie inside the arenas.
        let stamps = unsafe { stamp_slots(self.base, stamp_byte_off, cap) };
        // SAFETY: as above; `pose_arena_off` and `pose_off * 64` are 64-aligned; sole typed view.
        let poses = unsafe { pose_slots(self.base, pose_byte_off, cap) };

        Some(SampleRing {
            head: &edge.head,
            heartbeat: &claim.heartbeat,
            stamps,
            poses,
            edge: id,
        })
    }

    /// The `(stamp_byte_off, pose_byte_off, capacity)` triple for a dynamic
    /// edge, proved inside the ring regions; `None` if `edge` is not a ring.
    #[inline]
    fn ring_bytes(&self, edge: &EdgeRecord) -> Option<(usize, usize, usize)> {
        let cap = edge.capacity as usize;
        if !cap.is_power_of_two() {
            return None;
        }

        // The record's offsets are foreign input; `validate_arena_header` bounds
        // only the regions, not sub-ranges.
        if (edge.stamp_off as usize).saturating_add(cap) > self.header.stamp_slots as usize
            || (edge.pose_off as usize).saturating_add(cap) > self.header.pose_slots as usize
        {
            return None;
        }

        let stamp_byte_off = self.header.stamp_arena_off as usize + edge.stamp_off as usize * 8;
        let pose_byte_off = self.header.pose_arena_off as usize + edge.pose_off as usize * 64;

        Some((stamp_byte_off, pose_byte_off, cap))
    }
}

/// Exclusive, construction-time access to an arena. Writing an [`EdgeRecord`]
/// is a non-atomic write; the `&mut` borrow proves no [`ArenaView`] exists.
pub struct ArenaBuilder<'a> {
    arena: &'a mut dyn Arena,
}

impl<'a> ArenaBuilder<'a> {
    /// Take exclusive access to `arena`.
    #[must_use]
    pub fn new(arena: &'a mut dyn Arena) -> ArenaBuilder<'a> {
        ArenaBuilder { arena }
    }

    /// A shared view; cannot coexist with [`Self::declare_edge`].
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
        // SAFETY: `id < max_edges`, so the slot is in bounds and 64-aligned;
        // `&mut self` proves no `ArenaView` aliases it.
        unsafe {
            core::ptr::write(base.add(off).cast::<EdgeRecord>(), record);
        }
        Ok(())
    }
}

#[cfg(all(test, not(loom)))]
mod padding_tests {
    use crate::edge::EdgeRecord;
    use core::mem::{align_of, offset_of, size_of};

    /// Every byte an `EdgeRecord` puts on disk is initialised: the `[28..32)`
    /// hole before `head` must not leak producer memory into a `.tft`. Lives
    /// here because a `tests/` directory breaks `cargo xtask loom`.
    #[test]
    fn an_edge_record_leaves_no_uninitialised_bytes() {
        /// The same `&[u8]` view `write_frozen` takes of the arena.
        fn wire_bytes(rec: &EdgeRecord) -> &[u8] {
            // SAFETY: `#[repr(C)]` with every hole named (the property under
            // test); the reference is live for `size_of` bytes.
            unsafe {
                core::slice::from_raw_parts(
                    core::ptr::from_ref(rec).cast::<u8>(),
                    size_of::<EdgeRecord>(),
                )
            }
        }

        // Geometry first: the guarded hole is at `[28..32)`.
        assert_eq!(size_of::<EdgeRecord>(), 128);
        assert_eq!(align_of::<EdgeRecord>(), 64);
        assert_eq!(offset_of!(EdgeRecord, nominal_rate_mhz), 24);
        assert_eq!(
            offset_of!(EdgeRecord, head),
            32,
            "head moved; the hole this test guards is no longer at [28..32)"
        );

        for fill in [0xAA_u8, 0x55, 0xEE, 0x00] {
            let mut dirt = [fill; 1024];
            core::hint::black_box(&mut dirt);

            let dynamic = EdgeRecord::dynamic(1, 2, 64, 0, 0, 0, 0);
            let statik = EdgeRecord::static_edge(1, 2, [0; 7], 0);
            for (what, rec) in [("dynamic", &dynamic), ("static_edge", &statik)] {
                assert_eq!(
                    &wire_bytes(rec)[28..32],
                    &[0_u8; 4],
                    "{what}: the hole before `head` carries producer memory into \
                     every .tft (stack was filled {fill:#04x})"
                );
            }
        }

        let mut dirt = [0xC3_u8; 1024];
        core::hint::black_box(&mut dirt);
        let a = EdgeRecord::dynamic(7, 9, 128, 16, 32, 1, 2);
        core::hint::black_box(&mut [0_u8; 1024]);
        let b = EdgeRecord::dynamic(7, 9, 128, 16, 32, 1, 2);
        assert_eq!(
            wire_bytes(&a),
            wire_bytes(&b),
            "two identically-built EdgeRecords differ byte for byte, so the \
             arena carries producer state and a .tft cannot be content-addressed"
        );
    }
}
