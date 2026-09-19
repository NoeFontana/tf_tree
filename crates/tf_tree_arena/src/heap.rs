//! Heap-backed arena and the [`Arena`] abstraction.
//!
//! # SAFETY (module invariant)
//!
//! A [`HeapArena`] owns one 16-byte-aligned [`alloc_zeroed`] allocation of
//! `len + 63` bytes (`len == ArenaLayout::total_size()`), from which a
//! 64-byte-aligned base is carved by hand. For its lifetime:
//!
//! * `ptr` is non-null, 64-byte aligned, and points to `len` owned bytes.
//! * `alloc_ptr` is the allocation's own base, at most 63 bytes below `ptr`.
//! * It is freed once, in [`Drop`], with `alloc_ptr` and `alloc_layout`, never
//!   `ptr` (UB; [`0021`]; `tests/heap_alignment.rs` runs this under Miri).
//! * Only the raw base and length are exposed; typed access goes through the
//!   atomic protocols in `tf_tree_core`, so `Send + Sync` is sound.
//!
//! [`0021`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md

use alloc::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use core::ptr::NonNull;

use crate::header::{ArenaHeader, FORMAT_VERSION, TF_TREE_MAGIC};
use crate::layout::{layout_hash, ArenaLayout};

/// Byte alignment of the arena base and of [`ArenaHeader`] (one cache line per
/// `PoseSlot`); obtained by hand, see [`CALLOC_ALIGN`].
const ARENA_ALIGN: usize = 64;

/// The alignment actually requested from the allocator.
///
/// Must stay at or below `MIN_ALIGN` (16) so `alloc_zeroed` uses `calloc` and
/// pages stay demand-faulted; raising it to 64 silently undoes that
/// (`docs/decisions/0021`, `tests/heap_alignment.rs`).
const CALLOC_ALIGN: usize = 16;

const _: () = assert!(CALLOC_ALIGN <= ARENA_ALIGN);

/// A flat, pointer-free byte arena.
///
/// # Safety
///
/// Implementors must guarantee that [`Arena::base`] returns a pointer valid for
/// reads and writes of [`Arena::len`] bytes for as long as `self` is alive, that
/// the pointer is 64-byte aligned, and that the region may be shared across
/// threads (all interior mutation goes through atomics).
// An arena is never empty (the header region is at least 256 B).
#[allow(clippy::len_without_is_empty)]
pub unsafe trait Arena: Send + Sync {
    /// Base pointer of the arena's byte region.
    fn base(&self) -> *mut u8;
    /// Length of the arena's byte region, in bytes.
    fn len(&self) -> usize;
}

/// An [`Arena`] backed by a single zeroed, 64-byte-aligned heap allocation.
pub struct HeapArena {
    /// The **arena base** [`Arena::base`] returns, up to 63 bytes above
    /// [`Self::alloc_ptr`].
    ptr: NonNull<u8>,
    len: usize,
    /// The **allocation's own base**, the only pointer [`dealloc`] may be given;
    /// **freeing `ptr` is undefined behaviour** ([`0021`]).
    ///
    /// [`0021`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md
    alloc_ptr: NonNull<u8>,
    alloc_layout: Layout,
}

impl HeapArena {
    /// Allocate a zeroed, 64-byte-aligned arena sized for `layout`, then write
    /// the [`ArenaHeader`] into its first bytes.
    ///
    /// The creator identity is passed in: this `no_std` crate cannot read `/proc`.
    /// # Panics
    ///
    /// Aborts (via [`handle_alloc_error`]) if the allocation fails. Asserts the
    /// host is little-endian (load-bearing invariant 7).
    pub fn new(
        layout: &ArenaLayout,
        creator_pid: u32,
        owner_start_time: u64,
        boot_id: [u8; 16],
    ) -> HeapArena {
        const {
            assert!(
                cfg!(target_endian = "little"),
                "tf_tree arenas are little-endian only"
            );
        }

        let size = layout.total_size();

        // Over-allocate at `CALLOC_ALIGN` and align by hand: requesting 64 costs
        // ~293x the resident memory (`docs/decisions/0021`).
        // SAFETY: `CALLOC_ALIGN` is a non-zero power of two and `size + 63`
        // cannot overflow `isize::MAX` (`ArenaLayout::new` caps `total_size`).
        let alloc_layout =
            unsafe { Layout::from_size_align_unchecked(size + ARENA_ALIGN - 1, CALLOC_ALIGN) };

        // SAFETY: `alloc_layout` has non-zero size; nullness is checked below.
        let raw = unsafe { alloc_zeroed(alloc_layout) };
        let alloc_ptr = match NonNull::new(raw) {
            Some(p) => p,
            None => handle_alloc_error(alloc_layout),
        };

        // `addr()`, not `align_offset` (Miri may return `usize::MAX`).
        let offset = alloc_ptr.as_ptr().addr().wrapping_neg() % ARENA_ALIGN;

        // SAFETY: `offset < 64` and the allocation is `size + 63` bytes, so
        // `base..base+size` lies inside it; provenance is inherited from `raw`.
        let base = unsafe { alloc_ptr.as_ptr().add(offset) };

        // SAFETY: `alloc_ptr` advanced within its allocation is non-null.
        let ptr = unsafe { NonNull::new_unchecked(base) };

        let arena = HeapArena {
            ptr,
            len: size,
            alloc_ptr,
            alloc_layout,
        };
        arena.write_header(layout, creator_pid, owner_start_time, boot_id);
        arena
    }

    fn write_header(
        &self,
        layout: &ArenaLayout,
        creator_pid: u32,
        owner_start_time: u64,
        boot_id: [u8; 16],
    ) {
        // SAFETY: `self.ptr` is a freshly zeroed, 64-byte-aligned, uniquely
        // owned allocation of `self.len >= 256` bytes: `write_header_at`'s contract.
        unsafe {
            write_header_at(
                self.ptr.as_ptr(),
                self.len,
                layout,
                creator_pid,
                owner_start_time,
                boot_id,
                [0; 16],
            )
        }
    }

    /// Borrow the arena header living at the base of the allocation.
    pub fn header(&self) -> &ArenaHeader {
        // SAFETY: the base is a validly-initialized ArenaHeader (written in
        // `new`), 64-byte aligned, and borrowed for no longer than `self`.
        unsafe { &*self.ptr.as_ptr().cast::<ArenaHeader>() }
    }
}

impl Drop for HeapArena {
    fn drop(&mut self) {
        // SAFETY: **`alloc_ptr`, not `ptr`**: `alloc_ptr` and `alloc_layout` are
        // what `alloc_zeroed` used in `new`; freeing `ptr` would be UB. Freed once.
        unsafe { dealloc(self.alloc_ptr.as_ptr(), self.alloc_layout) }
    }
}

// SAFETY: `HeapArena` owns a unique allocation and exposes only its base pointer
// and length; concurrent access is mediated by atomics in the layers above.
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

/// Write an [`ArenaHeader`] into the first bytes of a freshly zeroed arena
/// region; shared by [`HeapArena`] and `MappedArena`.
///
/// # Safety
///
/// `base` must point to `len >= size_of::<ArenaHeader>()` writable, zeroed bytes
/// that the caller uniquely owns for the duration of the call, aligned to
/// [`ARENA_ALIGN`], with `len == layout.total_size()`.
pub(crate) unsafe fn write_header_at(
    base: *mut u8,
    len: usize,
    layout: &ArenaLayout,
    creator_pid: u32,
    owner_start_time: u64,
    boot_id: [u8; 16],
    instance_uuid: [u8; 16],
) {
    // `ArenaLayout::new` caps `total_size` at `u32::MAX`: every `as u32` is lossless.
    debug_assert!(len <= u32::MAX as usize);

    // SAFETY: by contract `base` is 64-byte aligned and backed by at least
    // `size_of::<ArenaHeader>()` owned, zeroed bytes, and all-zero is a valid
    // `ArenaHeader`; the caller uniquely owns the region, so nothing aliases the
    // `&mut`. The atomic fields (`topo`, the counts) are left at zero.
    let h = unsafe { &mut *base.cast::<ArenaHeader>() };
    h.magic = u64::from_le_bytes(TF_TREE_MAGIC);
    h.format_version = FORMAT_VERSION;
    h.layout_hash = layout_hash();
    h.arena_size = len as u64;
    h.max_frames = layout.max_frames();
    h.max_edges = layout.max_edges();
    h.stamp_slots = layout.stamp_slots();
    h.pose_slots = layout.pose_slots();
    h.frame_table_off = layout.frame_table().offset as u32;
    h.frame_hash_off = layout.frame_hash().offset as u32;
    h.topo_block_off = layout.topo_blocks().offset as u32;
    h.topo_block_stride = layout.topo_block_stride() as u32;
    h.claim_table_off = layout.claim_table().offset as u32;
    h.participant_table_off = layout.participant_table().offset as u32;
    h.max_participants = layout.max_participants();
    h.edge_table_off = layout.edge_table().offset as u32;
    h.stamp_arena_off = layout.stamp_arena().offset as u32;
    h.pose_arena_off = layout.pose_arena().offset as u32;
    // v3 (`docs/PHASE5.md` §1.2): the spline region is absent (offset 0).
    h.edge_counters_off = layout.edge_counters().offset as u32;
    h.participant_counters_off = layout.participant_counters().offset as u32;
    h.spline_region_off = 0;
    h.spline_degree = 0;
    h.creator_pid = creator_pid;
    h.owner_start_time = owner_start_time;
    h.boot_id = boot_id;
    h.instance_uuid = instance_uuid;
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
        let arena = HeapArena::new(&layout, 0, 0, [0u8; 16]);
        assert_eq!(arena.len(), layout.total_size());
        assert!(!arena.base().is_null());
        assert_eq!(arena.base() as usize % 64, 0);
    }

    #[test]
    fn header_is_written_correctly() {
        let layout = fixture();
        let arena = HeapArena::new(&layout, 4321, 99, [7u8; 16]);
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
        assert_eq!(h.edge_counters_off as usize, layout.edge_counters().offset);
        assert_eq!(
            h.participant_counters_off as usize,
            layout.participant_counters().offset
        );
        assert_ne!(
            h.edge_counters_off, 0,
            "the region must exist in a v3 arena"
        );
        assert_eq!(h.spline_region_off, 0, "Phase 6, absent");

        assert_eq!(h.creator_pid, 4321);
        assert_eq!(h.boot_id, [7u8; 16]);
        assert_eq!(h.owner_start_time, 99);
        assert_eq!(
            h.participant_table_off as usize,
            layout.participant_table().offset
        );
        assert_eq!(h.max_participants, layout.max_participants());

        assert_eq!(h.topo.load(Ordering::Relaxed), 0);
        assert_eq!(h.participant_count.load(Ordering::Relaxed), 0);
        assert_eq!(h.topo_lock.owner.load(Ordering::Relaxed), 0);
        assert_eq!(h.frame_count.load(Ordering::Relaxed), 0);
        assert_eq!(h.edge_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn body_is_zeroed_past_the_header() {
        let layout = fixture();
        let arena = HeapArena::new(&layout, 0, 0, [0u8; 16]);
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
