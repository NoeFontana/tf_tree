//! Heap-backed arena and the [`Arena`] abstraction.
//!
//! # SAFETY (module invariant)
//!
//! A [`HeapArena`] owns exactly one allocation obtained from
//! [`alloc_zeroed`] with a 64-byte-aligned [`Layout`] of `len` bytes, where
//! `len == ArenaLayout::total_size()`. The following hold for the whole lifetime
//! of the value:
//!
//! * `ptr` is non-null, 64-byte aligned, and points to `len` valid, owned bytes.
//! * The allocation is freed exactly once, in [`Drop`], with the identical
//!   [`Layout`] recorded in `alloc_layout`.
//! * `HeapArena` exposes only the raw base pointer and length; all typed access
//!   to the bytes happens through the atomic protocols in `tf_tree_core`, which
//!   is why sharing the handle across threads (`Send + Sync`) is sound.
//!
//! Every `unsafe` block below cites which of these invariants it relies on.

use alloc::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use core::ptr::NonNull;

use crate::header::{ArenaHeader, FORMAT_VERSION, TF_TREE_MAGIC};
use crate::layout::{layout_hash, ArenaLayout};

/// Byte alignment of the arena base and of [`ArenaHeader`].
const ARENA_ALIGN: usize = 64;

/// A flat, pointer-free byte arena.
///
/// # Safety
///
/// Implementors must guarantee that [`Arena::base`] returns a pointer valid for
/// reads and writes of [`Arena::len`] bytes for as long as `self` is alive, that
/// the pointer is 64-byte aligned, and that the region may be shared across
/// threads (all interior mutation goes through atomics).
// An arena is never empty (it always holds at least the 256-byte header), so an
// `is_empty` companion would be dead weight.
#[allow(clippy::len_without_is_empty)]
pub unsafe trait Arena: Send + Sync {
    /// Base pointer of the arena's byte region.
    fn base(&self) -> *mut u8;
    /// Length of the arena's byte region, in bytes.
    fn len(&self) -> usize;
}

/// An [`Arena`] backed by a single zeroed, 64-byte-aligned heap allocation.
///
/// Phase 2 adds `MappedArena` (`memfd` + `mmap`) as the only new backend; the
/// rest of the stack is written against [`Arena`] and never learns which it has.
pub struct HeapArena {
    ptr: NonNull<u8>,
    len: usize,
    alloc_layout: Layout,
}

impl HeapArena {
    /// Allocate a zeroed, 64-byte-aligned arena sized for `layout`, then write
    /// the [`ArenaHeader`] into its first bytes.
    ///
    /// `creator_pid` and `creator_boot_id` are constructor parameters, not
    /// self-discovered: this is a `no_std` crate and cannot read `/proc` or call
    /// `getpid`. The `std` facade supplies real values (Phase 2 uses `boot_id`
    /// to detect a segment that outlived a reboot); tests may pass `0`.
    ///
    /// # Panics
    ///
    /// Aborts (via [`handle_alloc_error`]) if the allocation fails. Asserts the
    /// host is little-endian (load-bearing invariant 7).
    pub fn new(layout: &ArenaLayout, creator_pid: u32, creator_boot_id: u64) -> HeapArena {
        // Invariant 7: the arena is host-native little-endian. Refuse to even
        // compile for a big-endian host rather than silently producing garbage.
        const {
            assert!(
                cfg!(target_endian = "little"),
                "tf_tree arenas are little-endian only"
            );
        }

        let size = layout.total_size();

        // SAFETY: `ARENA_ALIGN` (64) is a non-zero power of two, and `size` is a
        // multiple of 64 and at least 256 (the header region), so rounding it up
        // to the alignment cannot overflow `isize::MAX` for any arena we build.
        let alloc_layout = unsafe { Layout::from_size_align_unchecked(size, ARENA_ALIGN) };

        // SAFETY: `alloc_layout` has non-zero size (>= 256), satisfying
        // `alloc_zeroed`'s precondition. Nullness is checked immediately below.
        let raw = unsafe { alloc_zeroed(alloc_layout) };
        let ptr = match NonNull::new(raw) {
            Some(p) => p,
            None => handle_alloc_error(alloc_layout),
        };

        let arena = HeapArena {
            ptr,
            len: size,
            alloc_layout,
        };
        arena.write_header(layout, creator_pid, creator_boot_id);
        arena
    }

    fn write_header(&self, layout: &ArenaLayout, creator_pid: u32, creator_boot_id: u64) {
        // Offsets and slot counts are stored as u32 in the header. This is
        // enforced (not merely assumed) by `ArenaLayout::new`, which rejects any
        // layout whose `total_size` exceeds `u32::MAX` with `ArenaTooLarge`. So
        // every `as u32` below is a truncation that provably cannot lose bits;
        // this assert restates that invariant at the point it is relied on.
        debug_assert!(self.len <= u32::MAX as usize);

        let hdr = self.ptr.as_ptr().cast::<ArenaHeader>();

        // SAFETY: `hdr` is the arena base, which is 64-byte aligned (matching
        // ArenaHeader's align(64)) and backed by at least size_of::<ArenaHeader>()
        // owned bytes (total_size >= 256 >= header size). The bytes were just
        // zero-initialized, and an all-zero bit pattern is a valid ArenaHeader
        // (integers and atomics accept any pattern), so forming `&mut *hdr` and
        // assigning scalar fields is sound. The atomic fields are deliberately
        // left at their zeroed value (topo_generation/topo_active/frame_count/
        // edge_count all start at 0). `self` uniquely owns the allocation, so no
        // other reference aliases it during this write.
        let h = unsafe { &mut *hdr };
        h.magic = u64::from_le_bytes(TF_TREE_MAGIC);
        h.format_version = FORMAT_VERSION;
        h.layout_hash = layout_hash();
        h.arena_size = self.len as u64;
        h.max_frames = layout.max_frames();
        h.max_edges = layout.max_edges();
        h.stamp_slots = layout.stamp_slots();
        h.pose_slots = layout.pose_slots();
        h.frame_table_off = layout.frame_table().offset as u32;
        h.frame_hash_off = layout.frame_hash().offset as u32;
        h.topo_block_off = layout.topo_blocks().offset as u32;
        h.topo_block_stride = layout.topo_block_stride() as u32;
        h.claim_table_off = layout.claim_table().offset as u32;
        h.edge_table_off = layout.edge_table().offset as u32;
        h.stamp_arena_off = layout.stamp_arena().offset as u32;
        h.pose_arena_off = layout.pose_arena().offset as u32;
        h.creator_pid = creator_pid;
        h.creator_boot_id = creator_boot_id;
    }

    /// Borrow the arena header living at the base of the allocation.
    ///
    /// Useful for readers that need the region offsets or the live atomic
    /// counters without recomputing the layout.
    pub fn header(&self) -> &ArenaHeader {
        // SAFETY: the base is a validly-initialized ArenaHeader (written in
        // `new`), 64-byte aligned, and borrowed for no longer than `self`.
        unsafe { &*self.ptr.as_ptr().cast::<ArenaHeader>() }
    }
}

impl Drop for HeapArena {
    fn drop(&mut self) {
        // SAFETY: `ptr` and `alloc_layout` are exactly the pointer and layout
        // returned/used by `alloc_zeroed` in `new`; the allocation is still owned
        // by `self` and is freed here exactly once.
        unsafe { dealloc(self.ptr.as_ptr(), self.alloc_layout) }
    }
}

// SAFETY: `HeapArena` owns a unique heap allocation and exposes only its base
// pointer and length. It hands out no interior references that would alias the
// bytes, and all concurrent access to those bytes is mediated by atomics in the
// layers above, so the handle may be sent and shared across threads.
unsafe impl Send for HeapArena {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for HeapArena {}

// SAFETY: `base()`/`len()` describe the single owned allocation, which stays at
// a fixed 64-byte-aligned address, valid for `len` bytes, until `Drop`.
unsafe impl Arena for HeapArena {
    fn base(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use alloc::vec;
    use core::sync::atomic::Ordering;

    fn fixture() -> ArenaLayout {
        ArenaLayout::new(8, 4, vec![16, 0, 4, 64]).unwrap()
    }

    #[test]
    fn allocation_is_sized_and_aligned() {
        let layout = fixture();
        let arena = HeapArena::new(&layout, 0, 0);
        assert_eq!(arena.len(), layout.total_size());
        assert!(!arena.base().is_null());
        assert_eq!(arena.base() as usize % 64, 0);
    }

    #[test]
    fn header_is_written_correctly() {
        let layout = fixture();
        let arena = HeapArena::new(&layout, 4321, 0xdead_beef);
        let h = arena.header();

        assert_eq!(h.magic, u64::from_le_bytes(TF_TREE_MAGIC));
        assert_eq!(h.format_version, FORMAT_VERSION);
        assert_eq!(h.layout_hash, layout_hash());
        assert_eq!(h.arena_size, layout.total_size() as u64);
        assert_eq!(h.max_frames, 8);
        assert_eq!(h.max_edges, 4);
        assert_eq!(h.stamp_slots, layout.stamp_slots());
        assert_eq!(h.pose_slots, layout.pose_slots());

        assert_eq!(h.frame_table_off as usize, layout.frame_table().offset);
        assert_eq!(h.frame_hash_off as usize, layout.frame_hash().offset);
        assert_eq!(h.topo_block_off as usize, layout.topo_blocks().offset);
        assert_eq!(h.topo_block_stride as usize, layout.topo_block_stride());
        assert_eq!(h.claim_table_off as usize, layout.claim_table().offset);
        assert_eq!(h.edge_table_off as usize, layout.edge_table().offset);
        assert_eq!(h.stamp_arena_off as usize, layout.stamp_arena().offset);
        assert_eq!(h.pose_arena_off as usize, layout.pose_arena().offset);

        assert_eq!(h.creator_pid, 4321);
        assert_eq!(h.creator_boot_id, 0xdead_beef);

        // Atomics start zeroed.
        assert_eq!(h.topo_generation.load(Ordering::Relaxed), 0);
        assert_eq!(h.topo_active.load(Ordering::Relaxed), 0);
        assert_eq!(h.frame_count.load(Ordering::Relaxed), 0);
        assert_eq!(h.edge_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn body_is_zeroed_past_the_header() {
        let layout = fixture();
        let arena = HeapArena::new(&layout, 0, 0);
        // Sample a byte well past the header region.
        let off = layout.pose_arena().offset;
        // SAFETY: `off` is within the arena (`< len`); reading one owned byte.
        let byte = unsafe { *arena.base().add(off) };
        assert_eq!(byte, 0);
    }

    #[test]
    fn arena_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<HeapArena>();
    }
}
