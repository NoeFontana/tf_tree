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
use crate::frame::{blake3_64, find_core, intern_core, FrameRecord, InternTable, CLAIM_UNRECORDED};
use crate::participant::{state_of, ParticipantRecord, ParticipantTable, LIVE};
use crate::sync::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use crate::topology::{Block, TopologyView};

/// A caller-supplied liveness predicate for A8's interning takeover, and for the
/// claim/topology-lock reaping that will use the same source
/// (`docs/PHASE2.md` §6.2).
///
/// It is injected rather than implemented here for two reasons. This crate is
/// `no_std` and cannot read `/proc`. And `docs/PHASE2.md` §5.1 makes the **OFD
/// lock file** — not `/proc`, not the participant `state` field — the
/// authoritative answer to "is that participant still running"; that machinery
/// lives outside `tf_tree_core` and is built independently. Handing the answer in
/// as a closure keeps the arena algorithms unchanged when it arrives.
///
/// **It must fail safe** (§6.2): return `true` whenever it cannot tell. A false
/// "dead" verdict steals an in-flight entry from a working process; a false
/// "alive" verdict only postpones recovery.
/// The first argument is the **participant slot**, which the `docs/PHASE2.md`
/// §5.1 implementation needs: it asks the kernel about that slot's lock byte
/// rather than parsing `/proc`. The record is passed too, because the `/proc`
/// fallback and `doctor` still read the identity out of it.
pub type LivenessFn = dyn Fn(u32, &ParticipantRecord) -> bool;

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
    /// A8: this caller's participant slot **+ 1**, or [`CLAIM_UNRECORDED`] if it
    /// is not a registered participant. Recorded in `claiming` when this view
    /// wins an interning hash slot, and required to *rescue* one.
    me: u32,
    /// A8's injected liveness predicate; `None` means "assume alive", which
    /// disables takeover of a claimant whose participant slot still reads `LIVE`.
    is_alive: Option<&'a LivenessFn>,
}

impl<'a> ArenaView<'a> {
    /// Build a view over `arena`, reading its header.
    ///
    /// The view is anonymous and has no liveness source: it can wait on another
    /// interner but never takes an entry over. Add both with
    /// [`Self::as_participant`] and [`Self::with_liveness`].
    #[must_use]
    pub fn new(arena: &'a dyn Arena) -> ArenaView<'a> {
        let base = arena.base();
        // SAFETY: module invariant — the arena base is a validly-initialized
        // `ArenaHeader` (written by `HeapArena::new`), 64-byte aligned, and lives
        // as long as the borrowed `arena`, hence as long as `'a`.
        let header = unsafe { &*base.cast::<ArenaHeader>() };
        ArenaView {
            base,
            header,
            me: CLAIM_UNRECORDED,
            is_alive: None,
        }
    }

    /// Identify this view as participant `slot` (`docs/PHASE2.md` §1 A6/A8).
    ///
    /// `slot` is what [`ParticipantTable::register`] returned. An out-of-range
    /// slot — including the `u32::MAX` a read-only attachment carries — leaves the
    /// view anonymous rather than recording a claim nobody can resolve.
    ///
    /// A view must be identified before it can rescue an interning slot whose
    /// claimant died: the rescuer publishes *itself* into `claiming`, so an
    /// anonymous one would erase the entry's owner.
    #[must_use]
    pub fn as_participant(mut self, slot: u32) -> ArenaView<'a> {
        self.me = if slot < self.header.max_participants {
            slot + 1
        } else {
            CLAIM_UNRECORDED
        };
        self
    }

    /// Attach the liveness predicate used to decide whether a claimant that has
    /// not published is dead (see [`LivenessFn`], `docs/PHASE2.md` §6.2).
    ///
    /// Without one, a participant whose slot still reads `LIVE` is always
    /// believed — the fail-safe default, and the reason a crashed process is only
    /// *detected* once the real (OFD-lock) predicate is wired in.
    #[must_use]
    pub fn with_liveness(mut self, is_alive: &'a LivenessFn) -> ArenaView<'a> {
        self.is_alive = Some(is_alive);
        self
    }

    /// The participant slot this view interns as, or `None` if it is anonymous.
    ///
    /// A rescuer publishes *itself* into `claiming` when it takes over a stalled
    /// entry, so an anonymous view can wait but never rescue (A8). Exposed
    /// because "can this handle recover a wedged intern?" is a real diagnostic
    /// question — `doctor` should be able to answer it — and because it is the
    /// only way to assert from outside this crate that a `Tree` wired itself up.
    #[must_use]
    pub fn interning_identity(&self) -> Option<u32> {
        if self.me == CLAIM_UNRECORDED {
            None
        } else {
            Some(self.me - 1)
        }
    }

    /// Whether this view can decide that a claimant died.
    ///
    /// Without a liveness source every claimant is believed alive — the correct
    /// fail-safe, and one that means A8's takeover never fires. A view with an
    /// identity but no predicate is *silently* inert, which is exactly the
    /// failure worth being able to test for.
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

    // ---- frame interning -------------------------------------------------

    /// The interning hash array (`next_pow2(2 * max_frames)` slots).
    pub(crate) fn frame_hashes(&self) -> &'a [AtomicU64] {
        let slots = next_pow2(2 * self.header.max_frames as usize);
        let off = self.header.frame_hash_off as usize;
        // SAFETY: module invariant — the frame-hash region begins at
        // `frame_hash_off`, is 64-byte aligned, and reserves
        // `slots * FRAME_HASH_STRIDE` bytes; the first `slots * 8` are the
        // `AtomicU64` hash array.
        unsafe { core::slice::from_raw_parts(self.base.add(off).cast::<AtomicU64>(), slots) }
    }

    /// The interning id array (parallel to the hashes; `0` = unpublished, see
    /// [`crate::frame::ID_UNPUBLISHED`]).
    pub(crate) fn frame_ids(&self) -> &'a [AtomicU32] {
        let slots = next_pow2(2 * self.header.max_frames as usize);
        let off = self.header.frame_hash_off as usize + slots * 8;
        // SAFETY: module invariant — the id array follows the hash array within
        // the same region; `off` is 8-byte aligned (hashes are `slots * 8` bytes)
        // and names `slots` `AtomicU32`.
        unsafe { core::slice::from_raw_parts(self.base.add(off).cast::<AtomicU32>(), slots) }
    }

    /// **A8**: the interning claim array (parallel to the hashes; participant
    /// slot + 1 of the in-flight interner, [`CLAIM_UNRECORDED`] if none).
    pub(crate) fn frame_claiming(&self) -> &'a [AtomicU32] {
        let slots = next_pow2(2 * self.header.max_frames as usize);
        // Third array in the region: hashes (8 B) then ids (4 B) then claiming.
        let off = self.header.frame_hash_off as usize + slots * (8 + 4);
        // SAFETY: module invariant — `ArenaLayout` sizes the frame-hash region at
        // `slots * FRAME_HASH_STRIDE` (16) bytes, of which this is the last
        // `slots * 4`; `off` is 4-byte aligned because `slots * 12` is.
        unsafe { core::slice::from_raw_parts(self.base.add(off).cast::<AtomicU32>(), slots) }
    }

    /// The three interning arrays plus the id allocator, as `frame`'s algorithms
    /// want them.
    fn intern_table(&self) -> InternTable<'a> {
        InternTable {
            hashes: self.frame_hashes(),
            ids: self.frame_ids(),
            claiming: self.frame_claiming(),
            frame_count: &self.header.frame_count,
            // Usable ids are 1..max_frames (slot 0 is the root sentinel), so the
            // interned-frame capacity is one less than the table slot count.
            capacity: self.header.max_frames.saturating_sub(1),
        }
    }

    /// A8's claimant-liveness test, as [`crate::frame`] wants it: given a
    /// `claiming` entry (participant slot + 1), may that interner still publish?
    ///
    /// Two conditions, both required, and **both fail safe** — anything this
    /// cannot resolve counts as alive (`docs/PHASE2.md` §6.2):
    ///
    /// 1. the participant slot still reads `LIVE` (a `FREE` slot detached, a
    ///    `RESERVED` one died mid-attach — neither is going to finish an intern);
    /// 2. the injected [`LivenessFn`] agrees, when one was supplied.
    ///
    /// Condition 1 alone cannot see a *crash*: a `SIGKILL`ed process leaves its
    /// slot `LIVE` forever, which is exactly what condition 2 exists to catch.
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
    /// If another interner won this name's hash slot and died before publishing
    /// the id, this call takes the entry over rather than spinning forever
    /// (`docs/PHASE2.md` §1 A8) — but only if the view was given an identity with
    /// [`Self::as_participant`] and enough information to declare the claimant
    /// dead (see [`Self::with_liveness`]).
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
            &table,
            hash,
            self.me,
            self.claimant_alive(),
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
    /// A lookup never writes, so it cannot rescue a slot whose interner died; it
    /// reports `Ok(None)` instead (`docs/PHASE2.md` §1 A8). That is truthful — no
    /// id exists for the name — and self-correcting, since the next *interner* of
    /// the name takes the entry over.
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
        // SAFETY: module invariant — the participant region reserves
        // `max_participants` 128-byte records at `participant_table_off`, sized
        // by `ArenaLayout` and validated on attach, and every field is atomic so
        // sharing the slice across processes is sound.
        let slots = unsafe {
            core::slice::from_raw_parts(self.base.add(off).cast::<ParticipantRecord>(), n)
        };
        ParticipantTable::new(slots)
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
