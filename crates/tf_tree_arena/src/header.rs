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

use core::sync::atomic::{AtomicU32, AtomicU64};

/// Magic identifying a `tf_tree` arena.
///
/// Stored as a byte array rather than a `u64` literal so the on-disk/in-memory
/// byte order is unambiguous regardless of host endianness.
pub const TF_TREE_MAGIC: [u8; 8] = *b"TF_TREE\0";

/// Arena format version. Bumped on any incompatible layout change.
pub const FORMAT_VERSION: u32 = 1;

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
    /// Byte offset of the first of the two contiguous topology blocks.
    pub topo_block_off: u32,
    /// Byte stride between the two topology blocks.
    pub topo_block_stride: u32,
    /// Byte offset of the claim table region.
    pub claim_table_off: u32,
    /// Byte offset of the edge table region.
    pub edge_table_off: u32,
    /// Byte offset of the stamp arena region.
    pub stamp_arena_off: u32,
    /// Byte offset of the pose arena region.
    pub pose_arena_off: u32,
    /// Topology seqlock generation: even = stable, odd = write in progress.
    pub topo_generation: AtomicU64,
    /// Index (0 or 1) of the currently active topology block.
    pub topo_active: AtomicU32,
    /// Number of frames interned so far.
    pub frame_count: AtomicU32,
    /// Number of edges declared so far.
    pub edge_count: AtomicU32,
    /// PID of the process that created the arena.
    pub creator_pid: u32,
    /// Linux boot id of the creating host (used in Phase 2 to detect a stale
    /// segment that survived a reboot).
    pub creator_boot_id: u64,
    /// Reserved padding to keep the layout stable across future additions.
    _reserved: [u8; 40],
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn header_fits_within_region() {
        // The struct must fit within the 256-byte header region reserved for it.
        assert!(size_of::<ArenaHeader>() <= 256);
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
        assert_eq!(offset_of!(ArenaHeader, edge_table_off), 60);
        assert_eq!(offset_of!(ArenaHeader, stamp_arena_off), 64);
        assert_eq!(offset_of!(ArenaHeader, pose_arena_off), 68);
        assert_eq!(offset_of!(ArenaHeader, topo_generation), 72);
        assert_eq!(offset_of!(ArenaHeader, topo_active), 80);
        assert_eq!(offset_of!(ArenaHeader, frame_count), 84);
        assert_eq!(offset_of!(ArenaHeader, edge_count), 88);
        assert_eq!(offset_of!(ArenaHeader, creator_pid), 92);
        assert_eq!(offset_of!(ArenaHeader, creator_boot_id), 96);
    }

    #[test]
    fn magic_round_trips_little_endian() {
        assert_eq!(
            u64::from_le_bytes(TF_TREE_MAGIC).to_le_bytes(),
            TF_TREE_MAGIC
        );
    }
}
