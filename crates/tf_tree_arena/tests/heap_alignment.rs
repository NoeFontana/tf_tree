//! `docs/decisions/0021` — the heap arena is aligned by hand so that
//! `alloc_zeroed` reaches `calloc`, and the allocation is freed by the pointer
//! the allocator returned. `just miri` runs this file: Miri rejects a `dealloc`
//! of the offset pointer, which a native run does not. Residency is measured by
//! `just tf2-native-footprint`, not here.

// Fixtures that cannot construct their input have nothing to assert.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// `docs/decisions/0007` rule 1, kind 1 (arena memory); a test is a separate
// crate root, so the posture is declared here (`0048` step 4).
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

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

/// The base is 64-byte aligned, as `PoseSlot`'s cache-line occupancy needs.
///
/// Mutant: drop the alignment slack and offset in `HeapArena::new`.
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

/// The arena is `total_size()` readable, zeroed bytes from the aligned base.
///
/// Mutant: `len: size + ARENA_ALIGN - 1` in `HeapArena::new` runs off the
/// allocation; only Miri rejects it.
#[test]
fn the_whole_arena_is_readable_and_zeroed_from_the_aligned_base() {
    for l in geometries() {
        let a = HeapArena::new(&l, 0, 0, [0; 16]);
        let base = tf_tree_arena::Arena::base(&a);
        let len = tf_tree_arena::Arena::len(&a);
        assert_eq!(len, l.total_size());

        // Past the header every byte is still zero; the first and last tail
        // bytes prove both ends are inside the allocation.
        let header = 320usize;
        // SAFETY: `base` is valid for `len` bytes by `Arena`'s contract, and
        // both offsets are inside `[header, len)`.
        unsafe {
            assert_eq!(*base.add(header), 0, "first tail byte is not zero");
            assert_eq!(*base.add(len - 1), 0, "last arena byte is not zero");
        }
    }
}

/// Build and drop many arenas: the `dealloc` test. Under Miri a free of the
/// offset pointer is an error.
///
/// Mutant: `dealloc(self.ptr.as_ptr(), self.alloc_layout)` in `Drop`.
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

/// The header lands at the aligned base and reads back.
#[test]
fn the_header_is_written_at_the_aligned_base() {
    let l = layout();
    let a = HeapArena::new(&l, 4242, 99, [7; 16]);
    let h = a.header();
    assert_eq!(h.creator_pid, 4242, "header is not at the arena base");
    assert_eq!(h.layout_hash, tf_tree_arena::layout_hash());
}
