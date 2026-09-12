//! Typed, `unsafe` access to the raw arena regions.
//!
//! # SAFETY (module invariant)
//!
//! The crate's second `unsafe` island (with [`crate::buffer`]): it reads the
//! zeroed, 64-byte aligned arena bytes as typed records at the offsets
//! [`ArenaHeader`] gives. Sound because [`FrameRecord`], [`EdgeRecord`],
//! [`ClaimRecord`], [`PoseSlot`](crate::buffer::PoseSlot) and every atomic
//! element are valid all-zero; because [`crate::layout`](tf_tree_arena::layout)
//! laid every region out 64-byte aligned in header order, so each typed
//! pointer is in-bounds and aligned; because mutation is atomic, or (for
//! [`FrameRecord`] / [`EdgeRecord`] scalars) ordered before the atomic publish
//! that exposes it; and because **every index is bounds-checked against
//! `max_frames` / `max_edges` before a pointer is formed** — [`EdgeId`] and
//! [`FrameId::new`] accept out-of-range ids from safe code, so the checked
//! accessors return `None`. `tf_tree` reaches these paths without writing
//! `unsafe` at all: its one block is `OwnedWriter`'s lifetime extension
//! (`docs/decisions/0017`), which touches nothing here. Each `unsafe` block
//! below names what it relies on.
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

/// A caller-supplied liveness predicate for A8's interning takeover and the
/// claim/topology-lock reaping that will share it (`docs/PHASE2.md` §6.2).
///
/// Injected because this crate is `no_std` (no `/proc`) and because §5.1 makes
/// the OFD lock file authoritative over `/proc` and the participant `state`
/// field. **It must fail safe** (§6.2): `true` whenever it cannot tell, since a
/// false "dead" steals an in-flight entry from a working process. It is passed
/// the participant **slot** (§5.1 asks the kernel about that slot's lock byte)
/// and its record (the `/proc` fallback and `doctor` need the identity).
pub type LivenessFn = dyn Fn(u32, &ParticipantRecord) -> bool;

/// Smallest power of two `>= n` (matching the arena layout's `next_pow2`).
const fn next_pow2(n: usize) -> usize {
    let mut p: usize = 1;
    while p < n {
        p <<= 1;
    }
    p
}

/// A borrowed, typed view over an [`Arena`]'s regions. Cheap to construct (it
/// just reads the header offsets) and borrows the arena for `'a`.
pub struct ArenaView<'a> {
    base: *mut u8,
    header: &'a ArenaHeader,
    /// A8: this caller's participant slot **+ 1**, or [`CLAIM_UNRECORDED`] if
    /// anonymous. Recorded in `claiming` on winning an interning hash slot, and
    /// required to *rescue* one.
    me: u32,
    /// A8's injected liveness predicate; `None` means "assume alive", disabling
    /// takeover of a claimant whose participant slot still reads `LIVE`.
    is_alive: Option<&'a LivenessFn>,
    /// Whether the mapping behind `base` is writable. **Default `false`, and
    /// load-bearing**: a consumer attaches read-only (D18), so any write faults
    /// with `SIGSEGV` — the counters' `Guard` flush (`docs/PHASE5.md` §5) killed
    /// a read-only child with signal 11. §5 does not discuss it; a read-only
    /// participant silently keeps no counters.
    writable: bool,
}

impl<'a> ArenaView<'a> {
    /// Build a view over `arena`, reading its header. Anonymous and with no
    /// liveness source: it can wait on another interner but never take an entry
    /// over — [`Self::as_participant`] and [`Self::with_liveness`] add both.
    #[must_use]
    pub fn new(arena: &'a dyn Arena) -> ArenaView<'a> {
        let base = arena.base();
        // SAFETY: module invariant — the base is a validly-initialized,
        // 64-byte-aligned `ArenaHeader` (`HeapArena::new`) that lives for `'a`.
        let header = unsafe { &*base.cast::<ArenaHeader>() };
        ArenaView {
            base,
            header,
            me: CLAIM_UNRECORDED,
            is_alive: None,
            // Opting *in* to writes is safe to forget; opting out is not.
            writable: false,
        }
    }

    /// Declare that the mapping behind this view is writable. Only a caller that
    /// mapped it `PROT_WRITE` may say so; a false claim only faults.
    #[must_use]
    pub fn writable(mut self, yes: bool) -> ArenaView<'a> {
        self.writable = yes;
        self
    }

    /// Whether writes through this view are permitted by the mapping.
    #[must_use]
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Identify this view as participant `slot` (`docs/PHASE2.md` §1 A6/A8). An
    /// out-of-range slot — including the `u32::MAX` a read-only attachment
    /// carries — leaves the view anonymous rather than recording a claim nobody
    /// can resolve. Only an identified view can rescue a stalled intern: the
    /// rescuer publishes *itself* into `claiming`.
    #[must_use]
    pub fn as_participant(mut self, slot: u32) -> ArenaView<'a> {
        self.me = if slot < self.header.max_participants {
            slot + 1
        } else {
            CLAIM_UNRECORDED
        };
        self
    }

    /// Attach the predicate for whether a claimant that has not published is
    /// dead ([`LivenessFn`], `docs/PHASE2.md` §6.2). Without one a slot reading
    /// `LIVE` is always believed — fail-safe, and why a crashed process is
    /// detected only once the real OFD-lock predicate is wired in.
    #[must_use]
    pub fn with_liveness(mut self, is_alive: &'a LivenessFn) -> ArenaView<'a> {
        self.is_alive = Some(is_alive);
        self
    }

    /// The participant slot this view interns as, or `None` if anonymous — an
    /// anonymous view can wait on an intern but never rescue one (A8).
    #[must_use]
    pub fn interning_identity(&self) -> Option<u32> {
        if self.me == CLAIM_UNRECORDED {
            None
        } else {
            Some(self.me - 1)
        }
    }

    /// Whether this view can decide that a claimant died. Without a liveness
    /// source every claimant is believed alive, so A8's takeover never fires and
    /// an identified view is *silently* inert.
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

    /// The interning hash array (`next_pow2(2 * max_frames)` slots).
    pub(crate) fn frame_hashes(&self) -> &'a [AtomicU64] {
        let slots = next_pow2(2 * self.header.max_frames as usize);
        let off = self.header.frame_hash_off as usize;
        // SAFETY: module invariant — the 64-aligned region at `frame_hash_off`
        // spans `slots * FRAME_HASH_STRIDE`; this is its first `slots * 8`.
        unsafe { core::slice::from_raw_parts(self.base.add(off).cast::<AtomicU64>(), slots) }
    }

    /// The interning id array (parallel to the hashes; `0` = unpublished, see
    /// [`crate::frame::ID_UNPUBLISHED`]).
    pub(crate) fn frame_ids(&self) -> &'a [AtomicU32] {
        let slots = next_pow2(2 * self.header.max_frames as usize);
        let off = self.header.frame_hash_off as usize + slots * 8;
        // SAFETY: module invariant — the id array follows the hashes in the same
        // region; `off` is 8-byte aligned and names `slots` `AtomicU32`.
        unsafe { core::slice::from_raw_parts(self.base.add(off).cast::<AtomicU32>(), slots) }
    }

    /// **A8**: the interning claim array (parallel to the hashes; participant
    /// slot + 1 of the in-flight interner, [`CLAIM_UNRECORDED`] if none).
    pub(crate) fn frame_claiming(&self) -> &'a [AtomicU32] {
        let slots = next_pow2(2 * self.header.max_frames as usize);
        let off = self.header.frame_hash_off as usize + slots * (8 + 4);
        // SAFETY: module invariant — hashes (8 B), then ids (4 B), then this:
        // the last `slots * 4` of the region `ArenaLayout` sized at
        // `slots * FRAME_HASH_STRIDE` (16); `slots * 12` is 4-byte aligned.
        unsafe { core::slice::from_raw_parts(self.base.add(off).cast::<AtomicU32>(), slots) }
    }

    /// The three interning arrays plus the id allocator, for [`crate::frame`].
    fn intern_table(&self) -> InternTable<'a> {
        InternTable {
            hashes: self.frame_hashes(),
            ids: self.frame_ids(),
            claiming: self.frame_claiming(),
            frame_count: &self.header.frame_count,
            // Ids run 1..max_frames — slot 0 is the root sentinel.
            capacity: self.header.max_frames.saturating_sub(1),
        }
    }

    /// A8's claimant-liveness test: given a `claiming` entry (participant slot +
    /// 1), may that interner still publish? Both conditions **fail safe** —
    /// anything unresolvable counts as alive (`docs/PHASE2.md` §6.2). The slot
    /// must read `LIVE` (a `FREE` one detached, a `RESERVED` one died
    /// mid-attach), which alone cannot see a crash — `SIGKILL` leaves it `LIVE`
    /// forever — so the injected [`LivenessFn`] must agree as well.
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

    /// Pointer to frame slot `id` (slot 0 is the root sentinel); `None` if out
    /// of range for this arena.
    fn frame_record_ptr(&self, id: u32) -> Option<*mut FrameRecord> {
        if id >= self.header.max_frames {
            return None;
        }
        let off = self.header.frame_table_off as usize + id as usize * 64;
        // SAFETY: module invariant — `max_frames` 64-byte records at
        // `frame_table_off`, and `id < max_frames` was just checked.
        Some(unsafe { self.base.add(off).cast::<FrameRecord>() })
    }

    /// Byte offset of record `id` in a `stride`-byte table at `table_off`.
    #[inline]
    fn edge_slot_off(&self, id: EdgeId, table_off: u32, stride: usize) -> Option<usize> {
        if id.get() >= self.header.max_edges {
            return None;
        }
        Some(table_off as usize + id.get() as usize * stride)
    }

    /// Intern `name`, returning its stable [`FrameId`]. Idempotent even across
    /// concurrent interners (loom-tested), and takes over a hash slot whose
    /// claimant won it and died before publishing rather than spinning forever
    /// (`docs/PHASE2.md` §1 A8) — but only given both [`Self::as_participant`]
    /// and [`Self::with_liveness`].
    ///
    /// # Errors
    ///
    /// [`FrameError::FrameHashCollision`] on a 64-bit hash collision with a
    /// different name; [`FrameError::CapacityExceeded`] when the table is full.
    pub fn intern(&self, name: &str) -> Result<FrameId, FrameError> {
        let hash = blake3_64(name);
        let table = self.intern_table();

        let name_matches = |id: u32| -> bool {
            match self.frame_record_ptr(id) {
                // SAFETY: `id` is in bounds (`frame_record_ptr`) and published:
                // written before the `ids` Release store this reader acquired,
                // and append-only, so no writer aliases it.
                Some(ptr) => unsafe { &*ptr }.name_matches(name),
                None => false,
            }
        };
        let write_record = |id: u32| {
            if let Some(ptr) = self.frame_record_ptr(id) {
                let rec = FrameRecord::for_name(name, hash);
                // SAFETY: `id < max_frames` (checked); this runs only for the
                // unique CAS winner, before the `ids` Release store publishes
                // `id`, so nothing else references the append-only slot yet.
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

    /// Look up an already-interned frame by name **without** creating one:
    /// `Ok(None)` means never interned, which is how the read-only lookup path
    /// tells "unknown frame" from "known but disconnected". A lookup never
    /// writes, so it cannot rescue a slot whose interner died and reports
    /// `Ok(None)` there too (`docs/PHASE2.md` §1 A8) — truthful, and
    /// self-correcting, since the next *interner* takes the entry over.
    ///
    /// # Errors
    ///
    /// [`FrameError::FrameHashCollision`] if a different name occupies this hash.
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

    /// Read an interned frame record (name display / diagnostics), or `None` if
    /// `id` is out of range for this arena.
    #[must_use]
    pub fn frame_record(&self, id: FrameId) -> Option<&'a FrameRecord> {
        let ptr = self.frame_record_ptr(id.get())?;
        // SAFETY: `id.get() < max_frames` (checked by `frame_record_ptr`); a
        // live `FrameId` names a published, append-only record slot.
        Some(unsafe { &*ptr })
    }

    fn topo_block(&self, index: usize) -> Block<'a> {
        let mf = self.header.max_frames as usize;
        let block_off =
            self.header.topo_block_off as usize + index * self.header.topo_block_stride as usize;
        // Blocks hold [parent u32; mf][edge_of_child u32; mf][depth u16; mf] —
        // u32s first so both stay 4-aligned for any `mf`.
        // SAFETY: module invariant — each block reserves `align64(mf * 10)` at a
        // 64-aligned `topo_block_off + index * stride`, so the sub-arrays at 0,
        // `+ mf*4` and `+ mf*8` are all in-bounds and aligned.
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
        // SAFETY: module invariant — `max_participants` 128-byte, all-atomic
        // records at `participant_table_off`, sized by `ArenaLayout`.
        let slots = unsafe {
            core::slice::from_raw_parts(self.base.add(off).cast::<ParticipantRecord>(), n)
        };
        ParticipantTable::new(slots)
    }

    /// The claim record for edge `id`, or `None` if `id` is out of range.
    #[must_use]
    pub fn claim(&self, id: EdgeId) -> Option<&'a ClaimRecord> {
        let off = self.edge_slot_off(id, self.header.claim_table_off, 64)?;
        // SAFETY: module invariant — `max_edges` 64-byte records at
        // `claim_table_off`, `id < max_edges` checked; all mutation is atomic.
        Some(unsafe { &*self.base.add(off).cast::<ClaimRecord>() })
    }

    /// The per-edge diagnostic counters for edge `id`, or `None` if out of range
    /// (`docs/PHASE5.md` §5.2). The region exists in every `FORMAT_VERSION` 3
    /// arena whether or not the `counters` feature is compiled in — D34, so
    /// disabling it cannot fork the layout hash: it removes writes, not space.
    #[must_use]
    pub fn edge_counters(&self, id: EdgeId) -> Option<&'a EdgeCounters> {
        let off = self.edge_slot_off(id, self.header.edge_counters_off, 128)?;
        // SAFETY: module invariant — `max_edges` 128-byte all-atomic records at
        // `edge_counters_off`, validated on attach by `MappedArena::attach`;
        // `id < max_edges` was just checked.
        Some(unsafe { &*self.base.add(off).cast::<EdgeCounters>() })
    }

    /// The per-participant diagnostic counters for `slot`, `None` if out of
    /// range. Edge counters say failures exist; these say *which consumer*.
    #[must_use]
    pub fn participant_counters(&self, slot: u32) -> Option<&'a ParticipantCounters> {
        if slot >= self.header.max_participants {
            return None;
        }
        let off = self.header.participant_counters_off as usize + slot as usize * 128;
        // SAFETY: as above, against `max_participants` and
        // `participant_counters_off`, both validated on attach.
        Some(unsafe { &*self.base.add(off).cast::<ParticipantCounters>() })
    }

    /// The edge record for edge `id`, or `None` if `id` is out of range.
    #[must_use]
    pub fn edge(&self, id: EdgeId) -> Option<&'a EdgeRecord> {
        let off = self.edge_slot_off(id, self.header.edge_table_off, 128)?;
        // SAFETY: module invariant — `max_edges` 128-byte records at
        // `edge_table_off`, and `id < max_edges` was just checked.
        Some(unsafe { &*self.base.add(off).cast::<EdgeRecord>() })
    }

    /// The [`SampleRing`] for a dynamic edge, from its edge record (head,
    /// capacity, offsets) and claim record (heartbeat). `None` if `id` is out of
    /// range, or if `capacity == 0` — a static or tombstoned edge, not a ring.
    #[must_use]
    pub fn ring(&self, id: EdgeId) -> Option<SampleRing<'a>> {
        let edge = self.edge(id)?;
        let claim = self.claim(id)?;
        self.ring_of(id, edge, claim)
    }

    /// Everything the sampling hot path needs for one edge — interpolation
    /// discriminant and ring — behind one bounds check. `None` under exactly
    /// [`Self::ring`]'s conditions.
    #[must_use]
    pub fn sampler(&self, id: EdgeId) -> Option<(u8, SampleRing<'a>)> {
        let edge = self.edge(id)?;
        let claim = self.claim(id)?;
        Some((edge.interp, self.ring_of(id, edge, claim)?))
    }

    /// The byte extents of one dynamic edge's two rings, as `(offset, len)`
    /// pairs from the arena base — stamps first, then poses. `None` under
    /// exactly [`Self::ring`]'s conditions, sharing its `ring_bytes` check.
    ///
    /// For page population (`docs/PHASE2.md` §7.1): `MappedArena::populate_hot`
    /// skips the ring arenas, because §7.1's granularity is per-edge and the
    /// arena crate cannot name [`EdgeRecord`], which is defined here.
    /// Bytes, not `&[T]`: the caller is a `madvise`, and a typed slice of a
    /// region another process writes would be a data race in disguise.
    #[must_use]
    pub fn ring_extents(&self, id: EdgeId) -> Option<[(usize, usize); 2]> {
        let edge = self.edge(id)?;
        let (stamp_byte_off, pose_byte_off, cap) = self.ring_bytes(edge)?;
        Some([(stamp_byte_off, cap * 8), (pose_byte_off, cap * 64)])
    }

    /// Assemble a ring from resolved records; `None` if `edge` is not a ring.
    #[inline]
    fn ring_of(
        &self,
        id: EdgeId,
        edge: &'a EdgeRecord,
        claim: &'a ClaimRecord,
    ) -> Option<SampleRing<'a>> {
        let (stamp_byte_off, pose_byte_off, cap) = self.ring_bytes(edge)?;

        // SAFETY: `ring_bytes` proved `stamp_off + cap <= stamp_slots` and
        // `pose_off + cap <= pose_slots`, so both sub-ranges lie inside the
        // arenas whose extents `validate_arena_header` confirmed — not assumed
        // of `ArenaBuilder::declare_edge`, which writes the record unvalidated.
        let stamps = unsafe { stamp_slots(self.base, stamp_byte_off, cap) };
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
    /// edge, with the record's offsets proved to lie inside the two ring
    /// regions; `None` if `edge` is not a dynamic ring. The single place that
    /// bound is established, so [`Self::ring_of`]'s `unsafe` slice and
    /// [`Self::ring_extents`]'s `madvise` range cannot drift apart.
    #[inline]
    fn ring_bytes(&self, edge: &EdgeRecord) -> Option<(usize, usize, usize)> {
        let cap = edge.capacity as usize;
        if !cap.is_power_of_two() {
            return None;
        }

        // The record's triple is foreign input wherever this process did not
        // write the bytes — a peer's `memfd`, a `.tft` from disk — and
        // `validate_arena_header` bounds the *regions* only. Without this check
        // a corrupt triple formed a typed slice past the mapping, reachable from
        // safe code through `declare_edge` / `EdgeRecord::dynamic`. No
        // legitimate edge trips it: the facade assigns `stamp_off` cumulatively.
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

/// Exclusive, construction-time access to an arena.
///
/// Writing an [`EdgeRecord`] is a raw, non-atomic write of a whole 128-byte
/// record, sound only when nothing else can observe the slot; an `&mut` borrow
/// *proves* that, since a shared [`ArenaView`] borrows the same arena.
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
    /// [`TopologyError::CapacityExceeded`] if `id` is out of range.
    pub fn declare_edge(&mut self, id: EdgeId, record: EdgeRecord) -> Result<(), TopologyError> {
        let view = ArenaView::new(self.arena);
        let off = view
            .edge_slot_off(id, view.header.edge_table_off, 128)
            .ok_or(TopologyError::CapacityExceeded)?;
        let base = view.base;
        // SAFETY: `id < max_edges` (checked above), so the slot is in bounds and
        // 64-aligned, and `&mut self` proves no `ArenaView` — hence no
        // `&EdgeRecord` — aliases it while the raw write happens.
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

    /// **Every byte an `EdgeRecord` puts on disk is initialised.**
    ///
    /// `nominal_rate_mhz` ends at 28 and `head: AtomicU64` must start 8-aligned,
    /// so `[28..32)` is padding, which a struct literal does not initialise.
    /// [`TopologyBuilder::declare_edge`]'s typed `core::ptr::write` copies the
    /// hole and `write_frozen` memcpys the arena to the file, so four bytes of
    /// producer memory reached every `.tft` once per declared edge: UB, a leak,
    /// and why a `.tft` could not be content-addressed. Naming the bytes moves
    /// no offset and leaves `layout_hash` (stride constants) untouched.
    ///
    /// Keep it here: `src/tests.rs` cannot take the `&[u8]` view under
    /// `#![deny(unsafe_code)]`, `tf_tree_arena` cannot name `EdgeRecord`, and a
    /// `tf_tree_core/tests/` build gets no `cfg(loom)` dev-dependency under
    /// `cargo xtask loom`.
    ///
    /// **Mutant, run.** With `_pad1` removed, `cargo +nightly miri test -p
    /// tf_tree_core` reports UB at `[0x1c..0x20]` (= 28..32) and this fails.
    #[test]
    fn an_edge_record_leaves_no_uninitialised_bytes() {
        /// The same `&[u8]` view `write_frozen` takes of the arena.
        fn wire_bytes(rec: &EdgeRecord) -> &[u8] {
            // SAFETY: `EdgeRecord` is `#[repr(C)]` with every hole named — the
            // property under test — and the reference is live for `size_of`.
            unsafe {
                core::slice::from_raw_parts(
                    core::ptr::from_ref(rec).cast::<u8>(),
                    size_of::<EdgeRecord>(),
                )
            }
        }

        assert_eq!(size_of::<EdgeRecord>(), 128);
        assert_eq!(align_of::<EdgeRecord>(), 64);
        assert_eq!(offset_of!(EdgeRecord, nominal_rate_mhz), 24);
        assert_eq!(
            offset_of!(EdgeRecord, head),
            32,
            "head moved; the hole this test guards is no longer at [28..32)"
        );

        for fill in [0xAA_u8, 0x55, 0xEE, 0x00] {
            // Dirty the stack the constructors are about to build on.
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

        // The general property, observed the way `write_frozen` observes it.
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
