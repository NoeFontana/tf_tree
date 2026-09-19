//! The fixed-size arena header — the first bytes of every arena.
//!
//! [`ArenaHeader`] is a `#[repr(C, align(64))]` control block that a second
//! process reads to locate every region: its field order, offsets and
//! little-endian encoding are a normative wire contract (320 bytes).

use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64};

/// Magic identifying a `tf_tree` arena.
/// Magic identifying a `tf_tree` arena, a byte array so byte order is unambiguous.
pub const TF_TREE_MAGIC: [u8; 8] = *b"TF_TREE\0";

/// Arena format version, bumped on any incompatible layout change.
///
/// **2**: `docs/PHASE2.md` §1 A1, A2, A6, A7. **3**: `docs/PHASE5.md` §1, the
/// counter regions, Phase 6's `spline_region_off`/`_degree`, and a 320-byte
/// header. `MappedArena::attach` refuses versions 1 and 2.
pub const FORMAT_VERSION: u32 = 3;

/// Number of topology blocks the arena rotates through: four, not two
/// (`docs/PHASE2.md` §1 A1).
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

/// The in-arena topology mutation lock (`docs/PHASE2.md` §1 A2), reapable: an
/// abandoned mutation leaves no trace, so a stealer re-copies from the active block.
#[repr(C, align(64))]
pub struct TopoLock {
    /// `0` = free, else `participant_slot + 1`.
    pub owner: AtomicU64,
    /// When the holder acquired it; diagnostics only (`docs/PHASE2.md` §6.4).
    pub acquired_at_nanos: AtomicI64,
    _pad: [u8; 48],
}

/// Fixed-layout control block at the base of every arena. Field order and
/// offsets are normative; do not reorder. The four atomic fields carry the live
/// topology word and counts; every other field is read-only after construction.
#[repr(C, align(64))]
pub struct ArenaHeader {
    /// [`TF_TREE_MAGIC`] interpreted as a little-endian `u64`.
    pub magic: u64,
    /// [`FORMAT_VERSION`] at construction time.
    pub format_version: u32,
    /// Compile-time hash of header size/alignment and region strides
    /// ([`crate::layout::layout_hash`]).
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
    /// There is no odd state (A1): the writer mutates an inactive block.
    pub topo: AtomicU64,
    /// Number of frames interned so far.
    pub frame_count: AtomicU32,
    /// Number of edges declared so far.
    pub edge_count: AtomicU32,
    /// **Vestigial: nothing increments or reads it** (liveness is a lock byte,
    /// D17). Kept because removal moves every later offset (`docs/decisions/0056`,
    /// `0032` part 2).
    pub participant_count: AtomicU32,
    /// PID of the process that created the arena.
    pub creator_pid: u32,
    /// The creator's process start time (`/proc/<pid>/stat` field 22), which makes
    /// `creator_pid` PID-reuse-proof (A7).
    pub owner_start_time: u64,
    /// Linux boot id of the creating host (A7): detects a segment that outlived a reboot.
    pub boot_id: [u8; 16],
    /// Reserved padding to keep the layout stable across future additions.
    _reserved: [u8; 8],
    /// Identifies this arena instance (`docs/PHASE2.md` §3.7, `docs/decisions/0005`);
    /// all-zero means "not a shared instance" ([`crate::HeapArena`]). Offset 136.
    pub instance_uuid: [u8; 16],
    // FORMAT_VERSION 3 additions (`docs/PHASE5.md` §1.2).
    /// Byte offset of the per-edge counter region (§5.2). Never zero in a v3
    /// arena, so disabling `counters` does not fork the layout hash (D34).
    pub edge_counters_off: u32,
    /// Byte offset of the per-participant counter region (§5.2). Same contract.
    pub participant_counters_off: u32,
    /// Formerly `covariance_region_off` + `covariance_stride`; reserved in place
    /// after [`0009`] so `spline_region_off`/`spline_degree` (168, 172) do not move.
    ///
    /// [`0009`]: ../../../docs/decisions/0009-descoping-phase-6.md
    _reserved_covariance: [u8; 8],
    /// Byte offset of the cumulative-B-spline control region. **Phase 6.**
    /// `0` when absent.
    pub spline_region_off: u32,
    /// Spline degree. **Phase 6.** `0` when absent.
    pub spline_degree: u8,
    _pad_v3: [u8; 3],
    /// **≥ 64 bytes still reserved after everything above** (§1.2).
    _reserved_v3: [u8; 64],
    /// The topology mutation lock (A2), last so it has its own cache line. Offset 256.
    pub topo_lock: TopoLock,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn header_fits_within_region() {
        // The struct must fit the 320-byte header region (`docs/PHASE5.md`
        // §1.2); `crate::layout`'s region table holds the same number.
        assert!(size_of::<ArenaHeader>() <= 320);
        assert_eq!(align_of::<ArenaHeader>(), 64);
    }

    #[test]
    fn key_field_offsets_are_stable() {
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
        // FORMAT_VERSION 3 additions.
        assert_eq!(offset_of!(ArenaHeader, edge_counters_off), 152);
        assert_eq!(offset_of!(ArenaHeader, participant_counters_off), 156);
        // 160..168 was covariance's (`docs/decisions/0009`).
        assert_eq!(offset_of!(ArenaHeader, _reserved_covariance), 160);
        assert_eq!(offset_of!(ArenaHeader, spline_region_off), 168);
        assert_eq!(offset_of!(ArenaHeader, spline_degree), 172);
        assert_eq!(offset_of!(ArenaHeader, topo_lock), 256);
        assert_eq!(size_of::<ArenaHeader>(), 320);
    }

    /// **§1.2 requires ≥ 64 bytes still reserved after the v3 additions.**
    #[test]
    fn at_least_64_reserved_bytes_remain_after_the_v3_fields() {
        // Named reserved arrays plus the padding before the lock's 64-byte boundary.
        let named = 8usize /* _reserved */
            + 8 /* _reserved_covariance, freed by `0009` */
            + 3 /* _pad_v3 */
            + 64 /* _reserved_v3 */;
        let last_named_end = offset_of!(ArenaHeader, _reserved_v3) + 64;
        let implicit = offset_of!(ArenaHeader, topo_lock) - last_named_end;
        let free = named + implicit;
        assert!(
            free >= 64,
            "only {free} reserved bytes remain; §1.2 requires at least 64, and \
             spending them means the next phase pays for another format break"
        );
    }

    /// A later field must not silently push the lock off its cacheline.
    #[test]
    fn the_header_has_no_slack_left_between_its_last_field_and_the_lock() {
        let after_boot_id = offset_of!(ArenaHeader, boot_id) + 16;
        let uuid_at = offset_of!(ArenaHeader, instance_uuid);
        let lock_at = offset_of!(ArenaHeader, topo_lock);

        assert!(uuid_at >= after_boot_id, "{uuid_at} < {after_boot_id}");
        assert!(
            uuid_at + 16 <= lock_at,
            "uuid overruns the lock at {lock_at}"
        );

        // Adding a field without extending the header fails this.
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
