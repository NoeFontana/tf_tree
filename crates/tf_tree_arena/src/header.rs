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
/// A version-1 arena must not be attached: `MappedArena::attach` refuses it.
pub const FORMAT_VERSION: u32 = 2;

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
    /// The topology mutation lock (A2). Last so it lands on its own 64-byte
    /// line: it is contended only by mutators, and false-sharing it with the
    /// header fields every reader touches would be a needless cost.
    pub topo_lock: TopoLock,
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
        // The lock sits on its own cacheline, so its offset is a multiple of 64
        // and it is the last thing in the 256-byte header region.
        assert_eq!(offset_of!(ArenaHeader, topo_lock), 192);
        assert_eq!(size_of::<ArenaHeader>(), 256);
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
