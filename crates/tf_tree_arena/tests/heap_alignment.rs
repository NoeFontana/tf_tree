//! `docs/decisions/0021` — the heap arena is aligned by hand so that
//! `alloc_zeroed` reaches `calloc`, and the allocation is freed by the pointer
//! the allocator actually returned.
//!
//! # Why this file exists and why `just miri` runs it
//!
//! The fix over-allocates at 16-byte alignment and offsets the base up to the
//! next 64-byte boundary. That introduces exactly one way to be catastrophically
//! wrong: **freeing the offset pointer instead of the allocation's own.** It is
//! undefined behaviour, it is invisible to a passing test suite — glibc will
//! usually accept the free and corrupt its own bookkeeping silently — and it
//! would be found weeks later as an unrelated crash.
//!
//! So the whole lifecycle is exercised here, and `just miri` runs this crate.
//! Miri's borrow tracker and allocator model reject a mismatched `dealloc`
//! outright, which is the only cheap way to see it.
//!
//! The residency property itself — the 293x — is measured by
//! `just tf2-native-footprint` and `bench_report`'s `arena_memory_floor` row,
//! not here: a unit test cannot read `smaps_rollup` for a single allocation
//! without `unsafe` and without a whole-process counter that other tests would
//! perturb.

// The same allowance `layout.rs`'s own test module takes: a fixture that
// cannot construct its input has nothing to assert, and a panic there is the
// clearest possible failure.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use tf_tree_arena::{ArenaLayout, HeapArena};

/// The `docs/PHASE5.md` §9.3 geometry, which is what `0021` measured.
fn layout() -> ArenaLayout {
    ArenaLayout::from_totals(64, 64, 32 * 1024).expect("§9.3 geometry is valid")
}

/// A spread of geometries, so a single lucky alignment cannot carry the suite.
fn geometries() -> Vec<ArenaLayout> {
    [
        (1u32, 1u32, 1u32),
        (2, 1, 2),
        (7, 3, 64),
        (64, 64, 1024),
        (64, 64, 32 * 1024),
        (129, 130, 4096),
    ]
    .into_iter()
    .filter_map(|(f, e, s)| ArenaLayout::from_totals(f, e, s).ok())
    .collect()
}

/// The base is 64-byte aligned — the property `PoseSlot`'s cache-line
/// occupancy depends on, and the one the hand-rolled offset must preserve.
///
/// Mutant: drop the `+ ARENA_ALIGN - 1` slack and the `offset` computation in
/// `HeapArena::new`, returning the allocator's pointer directly. glibc returns
/// 16-byte-aligned blocks, so this fires on the first geometry whose allocation
/// does not happen to land on 64.
#[test]
fn every_geometry_gets_a_64_byte_aligned_base() {
    for l in geometries() {
        let a = HeapArena::new(&l, 0, 0, [0; 16]);
        let base = tf_tree_arena::Arena::base(&a);
        assert_eq!(
            base.addr() % 64,
            0,
            "arena base {base:p} is not 64-byte aligned"
        );
    }
}

/// The arena is `total_size()` bytes of *readable, zeroed* memory starting at
/// the aligned base — not at the allocation's base.
///
/// This is the test that would catch an offset applied to the length but not to
/// the pointer, or vice versa: the last byte of the arena must still be inside
/// the allocation, and Miri is what turns "must" into an error.
///
/// Mutant: change `len: size` to `len: size + ARENA_ALIGN - 1` in
/// `HeapArena::new` — the final read then runs off the end of the allocation and
/// Miri rejects it. (Native it passes, which is the point of running Miri.)
#[test]
fn the_whole_arena_is_readable_and_zeroed_from_the_aligned_base() {
    for l in geometries() {
        let a = HeapArena::new(&l, 0, 0, [0; 16]);
        let base = tf_tree_arena::Arena::base(&a);
        let len = tf_tree_arena::Arena::len(&a);
        assert_eq!(len, l.total_size());

        // Past the header, every byte must still be zero: `calloc` guarantees it
        // and nothing has written there. Reading the first and last byte of the
        // tail is what proves both ends are inside the allocation.
        let header = 320usize;
        // SAFETY: `base` is valid for `len` bytes by `Arena`'s contract, and
        // both offsets are inside `[header, len)`.
        unsafe {
            assert_eq!(*base.add(header), 0, "first tail byte is not zero");
            assert_eq!(*base.add(len - 1), 0, "last arena byte is not zero");
        }
    }
}

/// Build and drop many arenas in one process.
///
/// **This is the `dealloc` test.** Every iteration frees an allocation whose
/// base is, for most geometries, *not* the pointer `Arena::base` reports. Under
/// Miri a mismatched free is an immediate error; natively this is a smoke test
/// that the allocator's bookkeeping survives, which it would not if `dealloc`
/// were given the offset pointer.
///
/// Mutant, run rather than assumed: change `Drop` to
/// `dealloc(self.ptr.as_ptr(), self.alloc_layout)`. Miri aborts on the first
/// drop with *"Undefined Behavior: deallocating 0x… which does not point to the
/// beginning of an object"*. Natively the same mutant passes all four tests,
/// which is exactly why this runs under Miri.
#[test]
fn many_build_drop_cycles_free_the_allocations_base() {
    for _ in 0..8 {
        for l in geometries() {
            let a = HeapArena::new(&l, 0, 0, [0; 16]);
            std::hint::black_box(tf_tree_arena::Arena::base(&a));
            drop(a);
        }
    }
}

/// The header still lands at the aligned base and reads back.
///
/// A pointer/length mix-up that survived the two tests above would most likely
/// show up here, because `write_header_at` writes through the *arena* base while
/// `header()` reads through it too — so both would have to be wrong in the same
/// direction to stay consistent, and then the magic word would not match.
#[test]
fn the_header_is_written_at_the_aligned_base() {
    let l = layout();
    let a = HeapArena::new(&l, 4242, 99, [7; 16]);
    let h = a.header();
    assert_eq!(h.creator_pid, 4242, "header is not at the arena base");
    assert_eq!(h.layout_hash, tf_tree_arena::layout_hash());
}
