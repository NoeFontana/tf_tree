//! The fixed-size arena header — the first bytes of every arena.
//!
//! [`ArenaHeader`] is a `#[repr(C, align(64))]` control block written once at
//! construction and thereafter read (and, for its atomic fields, mutated) by
//! every reader and writer. Its layout is **normative**: Phase 2 maps the arena
//! into a second process and reads this header to locate every region, so the
//! field order, offsets, and endianness are a wire contract, not an
//! implementation detail. All multi-byte fields are little-endian (load-bearing
//! invariant 7); construction asserts a little-endian host.
//!
//! The header lives inside the 256-byte header *region* (see
//! [`crate::layout`]); the struct itself is smaller (it rounds up to its 64-byte
//! alignment) and the remainder of the region is reserved padding.

use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64};

/// Magic identifying a `tf_tree` arena.
///
/// Stored as a byte array rather than a `u64` literal so the on-disk/in-memory
/// byte order is unambiguous regardless of host endianness.
pub const TF_TREE_MAGIC: [u8; 8] = *b"TF_TREE\0";

/// Arena format version. Bumped on any incompatible layout change.
///
/// **2** — `docs/PHASE2.md` §1's crash-consistency amendments. Four of them
/// change this struct or the region table, so they were applied together in one
/// break rather than as a sequence of them:
///
/// * **A1** — `topo_generation` + `topo_active` collapse into one packed
///   [`ArenaHeader::topo`] word, and the topology block count goes 2 -> 4.
/// * **A2** — the topology mutation lock moves *into* the arena
///   ([`TopoLock`]), because a `Mutex` in one process serializes nothing
///   against another.
/// * **A6** — a participant table region, which is what gives claims and the
///   reaper a PID-reuse-proof identity to name.
/// * **A7** — `boot_id` becomes the full 16 bytes (truncating a 128-bit UUID to
///   64 loses the property that makes it useful) and `owner_start_time` joins
///   it.
///
/// # Version 3 — `docs/PHASE5.md` §1
///
/// A **deliberate, one-time break**. Phase 5 needs new arena regions, and so do
/// Phases 6 and 8; taking three breaks would cost three coordinated
/// fleet-wide restarts. §1 takes one, now, while the user count is small, and
/// reserves room for what is known to be coming:
///
/// * the two counter regions (§5.2), which exist whether or not the `counters`
///   feature is compiled in, so that disabling it does not fork the layout hash
///   (D34);
/// * `covariance_region_off`/`_stride` and `spline_region_off`/`_degree`, which
///   are **Phase 6's** and are `0` (absent) in every arena this build creates;
/// * `nominal_rate_mhz` and `declared_by_slot` in `EdgeRecord`'s reserved bytes,
///   and `frame_kind` in `FrameRecord`'s.
///
/// The header grew from 256 to 320 bytes to hold them with §1.2's required 64
/// reserved bytes still free. That moved `topo_lock` off its pinned offset and
/// changed `layout_hash` — both intended, both pinned by tests.
///
/// **A version-2 arena must not be attached**, and neither must a version-1
/// one: `MappedArena::attach` refuses both, and `tf_tree doctor
/// --explain-version` is what tells an operator why and what to do about it.
pub const FORMAT_VERSION: u32 = 3;

/// Number of topology blocks the arena rotates through.
///
/// **Four, not two** (`docs/PHASE2.md` §1 A1). With two blocks a reader is hit
/// whenever the writer flips twice mid-read; with four it takes four flips. At
/// `max_frames = 256` a block is ~2.5 KB, so the extra pair costs ~5 KB — free
/// against making `TopologyChurn` effectively unreachable outside a torture
/// test, given topology mutations happen a few hundred times per process
/// lifetime.
pub const TOPO_BLOCKS: usize = 4;

/// Pack a topology generation and active-block index into one word.
///
/// Bits 63..8 are the monotone generation; bits 7..0 the active block index.
#[inline]
#[must_use]
pub const fn pack_topo(generation: u64, active: u8) -> u64 {
    (generation << 8) | active as u64
}

/// Inverse of [`pack_topo`].
#[inline]
#[must_use]
pub const fn unpack_topo(word: u64) -> (u64, u8) {
    (word >> 8, (word & 0xff) as u8)
}

/// The in-arena topology mutation lock (`docs/PHASE2.md` §1 A2).
///
/// Phase 1 serialized topology mutation with a Rust `Mutex`, which is
/// per-process and therefore does nothing once a second process maps the arena.
/// This lives in the arena so every participant contends on the same word, and
/// it is **reapable**: because A1 makes an abandoned mutation leave no trace (the
/// writer only ever mutates an *inactive* block), a stealer needs no rollback —
/// it simply re-copies from the current active block.
#[repr(C, align(64))]
pub struct TopoLock {
    /// `0` = free, else `participant_slot + 1`.
    pub owner: AtomicU64,
    /// When the current holder acquired it, for staleness diagnostics. Never a
    /// reaping trigger on its own (`docs/PHASE2.md` §6.4).
    pub acquired_at_nanos: AtomicI64,
    _pad: [u8; 48],
}

/// Fixed-layout control block at the base of every arena.
///
/// Field order and offsets are normative; do not reorder. Multi-byte fields are
/// little-endian. The four atomic fields carry the live topology seqlock and the
/// frame/edge counts; every other field is written once at construction and then
/// read-only.
#[repr(C, align(64))]
pub struct ArenaHeader {
    /// [`TF_TREE_MAGIC`] interpreted as a little-endian `u64`.
    pub magic: u64,
    /// [`FORMAT_VERSION`] at construction time.
    pub format_version: u32,
    /// Compile-time hash of the header size/alignment and region strides
    /// (see [`crate::layout::layout_hash`]). Checked on attach in Phase 2.
    pub layout_hash: u32,
    /// Total arena size in bytes (equals [`crate::layout::ArenaLayout::total_size`]).
    pub arena_size: u64,
    /// Maximum number of frames (fixed at construction).
    pub max_frames: u32,
    /// Maximum number of edges (fixed at construction).
    pub max_edges: u32,
    /// Total stamp slots across all edges (sum of per-edge ring capacities).
    pub stamp_slots: u32,
    /// Total pose slots across all edges (equals `stamp_slots`).
    pub pose_slots: u32,
    /// Byte offset of the frame table region from the arena base.
    pub frame_table_off: u32,
    /// Byte offset of the frame interning hash region.
    pub frame_hash_off: u32,
    /// Byte offset of the first of the [`TOPO_BLOCKS`] contiguous topology blocks.
    pub topo_block_off: u32,
    /// Byte stride between consecutive topology blocks.
    pub topo_block_stride: u32,
    /// Byte offset of the claim table region.
    pub claim_table_off: u32,
    /// Byte offset of the participant table region (A6).
    pub participant_table_off: u32,
    /// Capacity of the participant table, in records (A6).
    pub max_participants: u32,
    /// Byte offset of the edge table region.
    pub edge_table_off: u32,
    /// Byte offset of the stamp arena region.
    pub stamp_arena_off: u32,
    /// Byte offset of the pose arena region.
    pub pose_arena_off: u32,
    /// Packed topology generation and active block index — see [`pack_topo`].
    ///
    /// **There is no odd state** (A1). The writer mutates an *inactive* block,
    /// which no reader is looking at, so publication is a single store and there
    /// is no window to make atomic. A writer killed mid-mutation therefore
    /// leaves the arena indistinguishable from one where no write happened,
    /// where the Phase 1 seqlock left the generation permanently odd and spun
    /// every reader forever.
    pub topo: AtomicU64,
    /// Number of frames interned so far.
    pub frame_count: AtomicU32,
    /// Number of edges declared so far.
    pub edge_count: AtomicU32,
    /// Number of participant slots in use (A6).
    pub participant_count: AtomicU32,
    /// PID of the process that created the arena.
    pub creator_pid: u32,
    /// The creator's process start time (jiffies since boot, `/proc/<pid>/stat`
    /// field 22), which is what makes `creator_pid` PID-reuse-proof (A7).
    pub owner_start_time: u64,
    /// Linux boot id of the creating host, all 16 bytes (A7). Detects a segment
    /// that outlived a reboot. Truncating a 128-bit UUID to 64 bits loses the
    /// property that makes it useful, which version 1 did.
    pub boot_id: [u8; 16],
    /// Reserved padding to keep the layout stable across future additions.
    _reserved: [u8; 8],
    /// Identifies *this* arena instance, as distinct from this arena *name*
    /// (`docs/PHASE2.md` §3.7, `docs/decisions/0005`).
    ///
    /// Two processes that both resolved `<runtime_dir>/<domain>/<name>` can
    /// still have attached to different segments — the owner may have died and
    /// been replaced between their two `open()` calls, which is the split-brain
    /// §11.2 scenario 9 exists to catch. Comparing names cannot detect that;
    /// comparing this can, which is why `HelloResponse` carries it.
    ///
    /// **All-zero means "not a shared instance".** A [`crate::HeapArena`] is
    /// single-process by construction, so there is no second attacher to agree
    /// with and no randomness is drawn — which also keeps the no-`shm` build
    /// free of an RNG dependency.
    ///
    /// Lands at offset 136, inside padding that already existed because
    /// [`TopoLock`] is `align(64)`: `boot_id` ends at 128, `_reserved` at 136,
    /// and the next 64-byte boundary is 192. So adding it moved no pinned
    /// offset, did not grow the header past 256, and — because
    /// [`crate::layout::layout_hash`] covers region sizes and strides rather
    /// than header fields — did not change the layout hash either. That is why
    /// [`FORMAT_VERSION`] is still 2.
    pub instance_uuid: [u8; 16],
    // -----------------------------------------------------------------------
    // FORMAT_VERSION 3 additions — `docs/PHASE5.md` §1.2
    // -----------------------------------------------------------------------
    //
    // These land at 152, in the 40 bytes of implicit padding that already
    // existed between `instance_uuid`'s end and `topo_lock`'s 64-byte boundary.
    // They do not fit *with* §1.2's required 64 reserved bytes, which is why the
    // header grows to 320 — see the amendment in that section, and the tests
    // below, which pin every offset it moved.
    /// Byte offset of the per-edge counter region (§5.2). Never zero in a v3
    /// arena: the region exists whether or not the `counters` feature is on, so
    /// that disabling the feature does not fork the layout hash (D34).
    pub edge_counters_off: u32,
    /// Byte offset of the per-participant counter region (§5.2). Same contract.
    pub participant_counters_off: u32,
    /// Byte offset of the per-sample covariance region. **Phase 6.**
    ///
    /// `0` means absent, and it is 0 in every arena this build creates. The
    /// field exists now so Phase 6 fills a region the header already accounts
    /// for, rather than breaking the format a second time (§1's whole argument).
    pub covariance_region_off: u32,
    /// Bytes per covariance entry. **Phase 6.** `0` when absent.
    pub covariance_stride: u32,
    /// Byte offset of the cumulative-B-spline control region. **Phase 6.**
    /// `0` when absent.
    pub spline_region_off: u32,
    /// Spline degree. **Phase 6.** `0` when absent.
    pub spline_degree: u8,
    _pad_v3: [u8; 3],
    /// **≥ 64 bytes still reserved after everything above**, which §1.2 requires
    /// explicitly.
    ///
    /// The point is not superstition. Phases 5, 6 and 8 each want header fields,
    /// and a format break costs every participant a coordinated restart. This is
    /// the room that makes the next two additions free — and it is the reason
    /// the break is being taken *once*, now, rather than three times.
    _reserved_v3: [u8; 64],
    /// The topology mutation lock (A2). Last so it lands on its own 64-byte
    /// line: it is contended only by mutators, and false-sharing it with the
    /// header fields every reader touches would be a needless cost.
    ///
    /// Moved from 192 to 256 by the v3 additions above. That offset was pinned
    /// by a test on purpose; the test moved with it rather than being deleted,
    /// because it is what stops the next person assuming there is still slack.
    pub topo_lock: TopoLock,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn header_fits_within_region() {
        // The struct must fit within the header region reserved for it, which
        // grew from 256 to 320 with FORMAT_VERSION 3 (`docs/PHASE5.md` §1.2).
        // `crate::layout`'s region table holds the same number; a mismatch there
        // would overlap the frame table with the header.
        assert!(size_of::<ArenaHeader>() <= 320);
        assert_eq!(align_of::<ArenaHeader>(), 64);
    }

    #[test]
    fn key_field_offsets_are_stable() {
        // These offsets are a cross-process wire contract; pin them.
        assert_eq!(offset_of!(ArenaHeader, magic), 0);
        assert_eq!(offset_of!(ArenaHeader, format_version), 8);
        assert_eq!(offset_of!(ArenaHeader, layout_hash), 12);
        assert_eq!(offset_of!(ArenaHeader, arena_size), 16);
        assert_eq!(offset_of!(ArenaHeader, max_frames), 24);
        assert_eq!(offset_of!(ArenaHeader, max_edges), 28);
        assert_eq!(offset_of!(ArenaHeader, stamp_slots), 32);
        assert_eq!(offset_of!(ArenaHeader, pose_slots), 36);
        assert_eq!(offset_of!(ArenaHeader, frame_table_off), 40);
        assert_eq!(offset_of!(ArenaHeader, frame_hash_off), 44);
        assert_eq!(offset_of!(ArenaHeader, topo_block_off), 48);
        assert_eq!(offset_of!(ArenaHeader, topo_block_stride), 52);
        assert_eq!(offset_of!(ArenaHeader, claim_table_off), 56);
        assert_eq!(offset_of!(ArenaHeader, participant_table_off), 60);
        assert_eq!(offset_of!(ArenaHeader, max_participants), 64);
        assert_eq!(offset_of!(ArenaHeader, edge_table_off), 68);
        assert_eq!(offset_of!(ArenaHeader, stamp_arena_off), 72);
        assert_eq!(offset_of!(ArenaHeader, pose_arena_off), 76);
        assert_eq!(offset_of!(ArenaHeader, topo), 80);
        assert_eq!(offset_of!(ArenaHeader, frame_count), 88);
        assert_eq!(offset_of!(ArenaHeader, edge_count), 92);
        assert_eq!(offset_of!(ArenaHeader, participant_count), 96);
        assert_eq!(offset_of!(ArenaHeader, creator_pid), 100);
        assert_eq!(offset_of!(ArenaHeader, owner_start_time), 104);
        assert_eq!(offset_of!(ArenaHeader, boot_id), 112);
        assert_eq!(offset_of!(ArenaHeader, instance_uuid), 136);
        // FORMAT_VERSION 3, in the padding that used to sit between
        // `instance_uuid` and the lock's 64-byte boundary.
        assert_eq!(offset_of!(ArenaHeader, edge_counters_off), 152);
        assert_eq!(offset_of!(ArenaHeader, participant_counters_off), 156);
        assert_eq!(offset_of!(ArenaHeader, covariance_region_off), 160);
        assert_eq!(offset_of!(ArenaHeader, covariance_stride), 164);
        assert_eq!(offset_of!(ArenaHeader, spline_region_off), 168);
        assert_eq!(offset_of!(ArenaHeader, spline_degree), 172);
        // The lock sits on its own cacheline, so its offset is a multiple of 64
        // and it is the last thing in the 320-byte header region.
        assert_eq!(offset_of!(ArenaHeader, topo_lock), 256);
        assert_eq!(size_of::<ArenaHeader>(), 320);
    }

    /// **§1.2 requires ≥ 64 bytes still reserved after the v3 additions**, and
    /// this is what makes that a fact rather than an intention.
    ///
    /// The whole argument for breaking the format once is that the next two
    /// phases' header fields land for free. That is only true while the room
    /// exists, and the way it stops existing is somebody spending it without
    /// noticing — so the check is here, next to the fields.
    #[test]
    fn at_least_64_reserved_bytes_remain_after_the_v3_fields() {
        // Named reserved arrays, plus the implicit padding between the last
        // named field and the lock's 64-byte boundary.
        let named = 8usize /* _reserved */ + 3 /* _pad_v3 */ + 64 /* _reserved_v3 */;
        let last_named_end = offset_of!(ArenaHeader, _reserved_v3) + 64;
        let implicit = offset_of!(ArenaHeader, topo_lock) - last_named_end;
        let free = named + implicit;
        assert!(
            free >= 64,
            "only {free} reserved bytes remain; §1.2 requires at least 64, and \
             spending them means the next phase pays for another format break"
        );
    }

    /// `instance_uuid` had to fit without disturbing anything already published.
    ///
    /// `key_field_offsets_are_stable` catches the field *moving*; this one
    /// records why 136 was available in the first place, so that a later field
    /// added in the same gap does not silently push the lock off its cacheline.
    #[test]
    fn the_header_has_no_slack_left_between_its_last_field_and_the_lock() {
        let after_boot_id = offset_of!(ArenaHeader, boot_id) + 16;
        let uuid_at = offset_of!(ArenaHeader, instance_uuid);
        let lock_at = offset_of!(ArenaHeader, topo_lock);

        // `instance_uuid` still sits after `boot_id` + `_reserved` and entirely
        // before the lock. That much is unchanged from version 2.
        assert!(uuid_at >= after_boot_id, "{uuid_at} < {after_boot_id}");
        assert!(
            uuid_at + 16 <= lock_at,
            "uuid overruns the lock at {lock_at}"
        );

        // **What changed, and why this test was rewritten rather than deleted.**
        //
        // In version 2 this asserted `lock_at == (uuid_at + 16).next_multiple_of(64)`
        // — the lock landed exactly where its own alignment put it, which was
        // the proof that `instance_uuid` had consumed *pre-existing* padding and
        // cost nothing. FORMAT_VERSION 3 spent that padding on the fields
        // `docs/PHASE5.md` §1.2 lists, so the lock moved from 192 to 256 and
        // that assertion is now false.
        //
        // Deleting it would remove the only thing standing between the next
        // person and the assumption that there is still slack there. So it
        // asserts the *current* truth instead: the lock is where alignment puts
        // it given everything now in front of it. Add a field without extending
        // the header and this fails, which is the whole point.
        assert_eq!(align_of::<TopoLock>(), 64);
        let last_named_end = offset_of!(ArenaHeader, _reserved_v3) + 64;
        assert_eq!(
            lock_at,
            last_named_end.next_multiple_of(64),
            "the lock must sit at the first 64-byte boundary after the last \
             named field; if this fails, a field was added without the header \
             growing to hold it"
        );
    }

    /// A1 packs the generation and the active-block index into one word so that
    /// publication is a single store. Round-trip the packing at the boundaries.
    #[test]
    fn topo_word_packs_and_unpacks() {
        for (g, a) in [(0u64, 0u8), (1, 3), (u64::MAX >> 8, 255), (12345, 2)] {
            assert_eq!(unpack_topo(pack_topo(g, a)), (g, a));
        }
        // The active index must not bleed into the generation.
        assert_eq!(unpack_topo(pack_topo(7, 255)).0, 7);
    }

    #[test]
    fn magic_round_trips_little_endian() {
        assert_eq!(
            u64::from_le_bytes(TF_TREE_MAGIC).to_le_bytes(),
            TF_TREE_MAGIC
        );
    }
}
