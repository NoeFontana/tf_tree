//! Heap-backed arena and the [`Arena`] abstraction.
//!
//! # SAFETY (module invariant)
//!
//! A [`HeapArena`] owns exactly one allocation obtained from [`alloc_zeroed`]
//! with a **16-byte**-aligned [`Layout`] of `len + 63` bytes, where
//! `len == ArenaLayout::total_size()`, out of which a 64-byte-aligned arena base
//! is carved by hand. The following hold for the whole lifetime of the value:
//!
//! * `ptr` is non-null, 64-byte aligned, and points to `len` valid, owned bytes.
//! * `alloc_ptr` is the allocation's own base, at most 63 bytes below `ptr`.
//! * The allocation is freed exactly once, in [`Drop`], **with `alloc_ptr` and
//!   the identical [`Layout`] recorded in `alloc_layout`** — never with `ptr`,
//!   which is generally not the pointer the allocator handed out. This is the
//!   one way to get [`0021`] wrong and it is undefined behaviour, so
//!   `tests/heap_alignment.rs` exercises the whole lifecycle under Miri.
//! * `HeapArena` exposes only the raw base pointer and length; all typed access
//!   to the bytes happens through the atomic protocols in `tf_tree_core`, which
//!   is why sharing the handle across threads (`Send + Sync`) is sound.
//!
//! Every `unsafe` block below cites which of these invariants it relies on.
//!
//! [`0021`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md

use alloc::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use core::ptr::NonNull;

use crate::header::{ArenaHeader, FORMAT_VERSION, TF_TREE_MAGIC};
use crate::layout::{layout_hash, ArenaLayout};

/// Byte alignment of the arena base and of [`ArenaHeader`].
///
/// Load-bearing: `PoseSlot` is `#[repr(C, align(64))]` and exactly one cache
/// line, and every concurrency number in this repository rests on two slots
/// never sharing one. It is **obtained** by hand rather than requested from the
/// allocator — see [`CALLOC_ALIGN`] — but it is not negotiable.
const ARENA_ALIGN: usize = 64;

/// The alignment actually requested from the allocator.
///
/// Must stay **at or below `MIN_ALIGN`** for the target (16 on every 64-bit
/// platform this builds for), because that is the condition under which Rust's
/// `System` allocator routes [`alloc_zeroed`] to `calloc` rather than to
/// `posix_memalign` plus an explicit zero-fill. The fill is what touches every
/// page; `calloc` at this size returns fresh `mmap` pages that stay
/// demand-faulted. `docs/decisions/0021` carries the measurement — 4 KiB
/// resident against 2356 KiB for the same bytes, differing only in this
/// constant.
///
/// **Raising this to 64 would silently undo the whole fix** and nothing in the
/// type system would notice, which is why `tests/heap_alignment.rs` asserts the
/// residency property and not merely the alignment.
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
// An arena is never empty (it always holds at least the header region, 320 B
// since FORMAT_VERSION 3 and never smaller than 256), so an
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
    /// The **arena base**: 64-byte aligned, `len` bytes, what [`Arena::base`]
    /// returns. Up to `ARENA_ALIGN - 1` bytes *above* [`Self::alloc_ptr`].
    ptr: NonNull<u8>,
    len: usize,
    /// The **allocation's own base**, and the only pointer [`dealloc`] may be
    /// given.
    ///
    /// Distinct from `ptr` since [`0021`]: the arena is over-allocated at an
    /// alignment `alloc_zeroed` will route to `calloc` and then aligned by hand,
    /// so these two differ whenever the allocator did not already return a
    /// 64-byte-aligned block. **Freeing `ptr` would be undefined behaviour**;
    /// `tests/heap_alignment.rs` runs the whole lifecycle under Miri because a
    /// passing suite alone cannot see that mistake.
    ///
    /// [`0021`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md
    alloc_ptr: NonNull<u8>,
    alloc_layout: Layout,
}

impl HeapArena {
    /// Allocate a zeroed, 64-byte-aligned arena sized for `layout`, then write
    /// the [`ArenaHeader`] into its first bytes.
    ///
    /// `creator_pid`, `owner_start_time` and `boot_id` are constructor
    /// parameters, not self-discovered: this is a `no_std` crate and cannot read
    /// `/proc` or call `getpid`. The `std` facade supplies real values —
    /// `boot_id` detects a segment that outlived a reboot, and
    /// `owner_start_time` is what makes the PID reuse-proof — and tests may pass
    /// zeros.
    ///
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
        // Invariant 7: the arena is host-native little-endian. Refuse to even
        // compile for a big-endian host rather than silently producing garbage.
        const {
            assert!(
                cfg!(target_endian = "little"),
                "tf_tree arenas are little-endian only"
            );
        }

        let size = layout.total_size();

        // **Over-allocate at `CALLOC_ALIGN` and align to 64 by hand.** Asking
        // the allocator for 64-byte alignment directly costs ~293x the resident
        // memory, and `docs/decisions/0021` is the measurement: Rust's
        // `alloc_zeroed` reaches `calloc` only when the requested alignment is
        // at most `MIN_ALIGN`, and above that falls back to `posix_memalign`
        // followed by an explicit zero-fill — which *touches every page*.
        // `calloc` for an allocation this size hands back fresh `mmap` pages the
        // kernel already guarantees to be zero and never materialises until
        // touched. Measured at the §9.3 geometry: **4 KiB resident at align 16,
        // 2356 KiB at align 64.**
        //
        // SAFETY: `CALLOC_ALIGN` (16) is a non-zero power of two; `size` is a
        // multiple of 64 and at least 256, and `size + ARENA_ALIGN - 1` cannot
        // overflow `isize::MAX` because `ArenaLayout::new` already refuses any
        // `total_size` above `u32::MAX`.
        let alloc_layout =
            unsafe { Layout::from_size_align_unchecked(size + ARENA_ALIGN - 1, CALLOC_ALIGN) };

        // SAFETY: `alloc_layout` has non-zero size (>= 256), satisfying
        // `alloc_zeroed`'s precondition. Nullness is checked immediately below.
        let raw = unsafe { alloc_zeroed(alloc_layout) };
        let alloc_ptr = match NonNull::new(raw) {
            Some(p) => p,
            None => handle_alloc_error(alloc_layout),
        };

        // The offset to the next 64-byte boundary, which is at most
        // `ARENA_ALIGN - 1` and is exactly the slack requested above.
        //
        // `addr()` arithmetic rather than `<*mut u8>::align_offset`: Miri is
        // permitted to return `usize::MAX` from `align_offset` on any call, on
        // purpose, so that code cannot come to depend on it succeeding — and
        // this crate is one `just miri` runs. Deriving the pointer with `add`
        // keeps provenance from `raw`, which the strict-provenance model
        // requires and which casting through an integer would lose.
        let offset = alloc_ptr.as_ptr().addr().wrapping_neg() % ARENA_ALIGN;

        // SAFETY: `offset < ARENA_ALIGN` and the allocation is
        // `size + ARENA_ALIGN - 1` bytes, so `base` and the `size` bytes after
        // it lie inside the same allocation; provenance is inherited from `raw`.
        let base = unsafe { alloc_ptr.as_ptr().add(offset) };

        // SAFETY: `base` is `alloc_ptr` (non-null) advanced within its own
        // allocation, so it is non-null.
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
        // SAFETY: `self.ptr` is the base of a freshly zeroed, 64-byte-aligned
        // allocation of `self.len >= 256` bytes that `self` uniquely owns, which
        // is exactly `write_header_at`'s contract.
        unsafe {
            write_header_at(
                self.ptr.as_ptr(),
                self.len,
                layout,
                creator_pid,
                owner_start_time,
                boot_id,
                // All-zero: a heap arena is single-process by construction, so
                // there is no second attacher for an instance id to disambiguate
                // against. Drawing randomness here would also put an RNG in the
                // no-`shm` dependency budget for no reader.
                [0; 16],
            )
        }
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
        // SAFETY: **`alloc_ptr`, not `ptr`.** `alloc_ptr` and `alloc_layout` are
        // exactly the pointer and layout returned/used by `alloc_zeroed` in
        // `new`; `ptr` is that pointer advanced by up to 63 bytes to reach a
        // 64-byte boundary, and freeing it would be undefined behaviour. The
        // allocation is still owned by `self` and is freed here exactly once.
        unsafe { dealloc(self.alloc_ptr.as_ptr(), self.alloc_layout) }
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

/// Write an [`ArenaHeader`] into the first bytes of a freshly zeroed arena
/// region.
///
/// Shared by [`HeapArena`] and (behind the `shm` feature) `MappedArena`, because
/// a mapped arena that disagreed with a heap arena about its own header would
/// break the one property Phase 2 rests on: that the two are the same bytes,
/// read by the same code.
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
    // Offsets and slot counts are stored as u32 in the header. This is enforced
    // (not merely assumed) by `ArenaLayout::new`, which rejects any layout whose
    // `total_size` exceeds `u32::MAX` with `ArenaTooLarge`. So every `as u32`
    // below is a truncation that provably cannot lose bits; this assert restates
    // that invariant at the point it is relied on.
    debug_assert!(len <= u32::MAX as usize);

    // SAFETY: by contract `base` is 64-byte aligned (matching ArenaHeader's
    // align(64)) and backed by at least size_of::<ArenaHeader>() owned, zeroed
    // bytes. An all-zero bit pattern is a valid ArenaHeader (integers and
    // atomics accept any pattern), so forming `&mut *base.cast::<ArenaHeader>()`
    // and assigning scalar fields is sound. The atomic fields are deliberately
    // left at their zeroed value: `topo` (one packed word since A1), plus
    // frame_count, edge_count and participant_count, all starting at 0.
    // The caller uniquely owns the region, so nothing aliases it.
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
    // v3 (`docs/PHASE5.md` §1.2). The counter regions always exist; the spline
    // region is declared absent with offset 0, which is what lets Phase 6 fill
    // it without another format break. Covariance had two fields here until
    // `docs/decisions/0009` descoped it; the header keeps their eight bytes
    // reserved in place so the spline offsets do not move.
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
        // v3: the counter regions are recorded, and the Phase 6 ones are
        // explicitly absent rather than uninitialised.
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

        // Atomics start zeroed.
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
