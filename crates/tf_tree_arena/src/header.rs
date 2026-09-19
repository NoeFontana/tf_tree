//! The fixed-size arena header — the first bytes of every arena.
//!
//! [`ArenaHeader`] is a `#[repr(C, align(64))]` control block; a second process
//! reads it to locate every region, so its field order, offsets and
//! little-endian encoding are a **normative** wire contract. It is exactly the
//! 320-byte header region since `FORMAT_VERSION` 3.

use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64};

/// Magic identifying a `tf_tree` arena.
///
/// Stored as a byte array rather than a `u64` literal so the on-disk/in-memory
/// byte order is unambiguous regardless of host endianness.
pub const TF_TREE_MAGIC: [u8; 8] = *b"TF_TREE\0";

/// Arena format version. Bumped on any incompatible layout change.
///
/// **2** — `docs/PHASE2.md` §1's amendments A1 (packed [`ArenaHeader::topo`],
/// four topology blocks), A2 ([`TopoLock`] in the arena), A6 (participant
/// table) and A7 (16-byte `boot_id`, `owner_start_time`).
///
/// **3** — `docs/PHASE5.md` §1: one deliberate break adding the counter regions
/// (§5.2), Phase 6's `spline_region_off`/`_degree` (`0` when absent) and
/// reserved bytes in `EdgeRecord`/`FrameRecord`. The header grew from 256 to 320
/// bytes with §1.2's 64 reserved bytes still free, moving `topo_lock` and
/// `layout_hash`; tests pin both.
///
/// A version-1 or version-2 arena must not be attached; `MappedArena::attach`
/// refuses both.
pub const FORMAT_VERSION: u32 = 3;

/// Number of topology blocks the arena rotates through. **Four, not two**
/// (`docs/PHASE2.md` §1 A1): a reader is hit after four flips mid-read, not two,
/// for ~5 KB at `max_frames = 256`.
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

/// The in-arena topology mutation lock (`docs/PHASE2.md` §1 A2). It is
/// **reapable**: A1 makes an abandoned mutation leave no trace, so a stealer
/// re-copies from the active block with no rollback.
#[repr(C, align(64))]
pub struct TopoLock {
    /// `0` = free, else `participant_slot + 1`.
    pub owner: AtomicU64,
    /// When the current holder acquired it, for staleness diagnostics. Never a
    /// reaping trigger on its own (`docs/PHASE2.md` §6.4).
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
    /// **There is no odd state** (A1): the writer mutates an *inactive* block, so
    /// publication is one store and a killed writer leaves no trace.
    pub topo: AtomicU64,
    /// Number of frames interned so far.
    pub frame_count: AtomicU32,
    /// Number of edges declared so far.
    pub edge_count: AtomicU32,
    /// **Vestigial: nothing increments it and nothing should read it** (liveness
    /// is a lock byte, D17). Kept because removing it moves every later offset
    /// (`docs/decisions/0056`, `docs/decisions/0032` part 2).
    pub participant_count: AtomicU32,
    /// PID of the process that created the arena.
    pub creator_pid: u32,
    /// The creator's process start time (jiffies since boot, `/proc/<pid>/stat`
    /// field 22), which makes `creator_pid` PID-reuse-proof (A7).
    pub owner_start_time: u64,
    /// Linux boot id of the creating host, all 16 bytes (A7): detects a segment
    /// that outlived a reboot.
    pub boot_id: [u8; 16],
    /// Reserved padding to keep the layout stable across future additions.
    _reserved: [u8; 8],
    /// Identifies *this* arena instance, not its name: the split-brain check
    /// (`docs/PHASE2.md` §3.7, `docs/decisions/0005`).
    ///
    /// **All-zero means "not a shared instance"** ([`crate::HeapArena`]). Offset 136.
    pub instance_uuid: [u8; 16],
    // FORMAT_VERSION 3 additions (`docs/PHASE5.md` §1.2), at 152 in the padding
    // before `topo_lock`.
    /// Byte offset of the per-edge counter region (§5.2). Never zero in a v3
    /// arena, so disabling `counters` does not fork the layout hash (D34).
    pub edge_counters_off: u32,
    /// Byte offset of the per-participant counter region (§5.2). Same contract.
    pub participant_counters_off: u32,
    /// Eight bytes that were `covariance_region_off` + `covariance_stride`
    /// until [`0009`] descoped covariance, reserved **in place**: closing the gap
    /// would move `spline_region_off`/`spline_degree` (168, 172), and
    /// `layout_hash` would not notice.
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
        // 160..168 was covariance's (`docs/decisions/0009`); see
        // `_reserved_covariance`.
        assert_eq!(offset_of!(ArenaHeader, _reserved_covariance), 160);
        assert_eq!(offset_of!(ArenaHeader, spline_region_off), 168);
        assert_eq!(offset_of!(ArenaHeader, spline_degree), 172);
        assert_eq!(offset_of!(ArenaHeader, topo_lock), 256);
        assert_eq!(size_of::<ArenaHeader>(), 320);
    }

    /// **§1.2 requires ≥ 64 bytes still reserved after the v3 additions.**
    #[test]
    fn at_least_64_reserved_bytes_remain_after_the_v3_fields() {
        // Named reserved arrays, plus the implicit padding between the last
        // named field and the lock's 64-byte boundary.
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

    /// `instance_uuid`'s slot at 136 was pre-existing padding; a later field must
    /// not silently push the lock off its cacheline.
    #[test]
    fn the_header_has_no_slack_left_between_its_last_field_and_the_lock() {
        let after_boot_id = offset_of!(ArenaHeader, boot_id) + 16;
        let uuid_at = offset_of!(ArenaHeader, instance_uuid);
        let lock_at = offset_of!(ArenaHeader, topo_lock);

        // `instance_uuid` sits after `boot_id` + `_reserved` and before the lock.
        assert!(uuid_at >= after_boot_id, "{uuid_at} < {after_boot_id}");
        assert!(
            uuid_at + 16 <= lock_at,
            "uuid overruns the lock at {lock_at}"
        );

        // The lock is where alignment puts it given everything in front of it:
        // add a field without extending the header and this fails.
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
