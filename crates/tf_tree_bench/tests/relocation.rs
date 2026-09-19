//! The relocation gate: a byte-copy of a populated arena at a different address
//! answers every query bit-for-bit identically, so nothing absolute is stored in
//! the arena (`docs/PHASE1.md` §13). Bit-for-bit, not `approx_eq`: a tolerance
//! would hide an offset resolving to a neighbouring slot.
// `panic`: the match arm reports which side declined, which `assert!` cannot.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// `docs/decisions/0007` rule 1, kind 6 (`docs/decisions/0048`): the
// `unsafe impl Send/Sync for RelocatedArena` in a target that never ships. A
// test is a separate crate root, so the posture is declared here (`0048` step 4).
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;

use tf_tree::unstable::EdgeKind;
use tf_tree::{Iso3, Stamp};
use tf_tree_arena::Arena;
use tf_tree_arena::ArenaHeader;
use tf_tree_bench::fixture;
use tf_tree_core::arena_view::ArenaView;
use tf_tree_core::plan::{compile, EdgeMeta, Guard};
use tf_tree_core::EdgeId;

/// Alignment the arena requires of its base (`tf_tree_arena`'s `ARENA_ALIGN`).
const ARENA_ALIGN: usize = 64;

/// An [`Arena`] over a heap block this test owns, at an address other than the
/// original's.
struct RelocatedArena {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

// SAFETY: `ptr` is non-null, 64-byte aligned and owns `len` initialized bytes
// for the lifetime of the value (allocated in `copy_of`, freed exactly once in
// `Drop` with the identical `Layout`). All typed access goes through
// `tf_tree_core`'s atomic protocols, which is what makes sharing sound — the
// same argument `HeapArena` makes.
unsafe impl Send for RelocatedArena {}
// SAFETY: as above.
unsafe impl Sync for RelocatedArena {}
// SAFETY: `base`/`len` describe one valid, aligned, owned region for `self`'s
// whole lifetime, which is the `Arena` contract.
unsafe impl Arena for RelocatedArena {
    fn base(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for RelocatedArena {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`layout` are exactly what `alloc_zeroed` returned in
        // `copy_of`, freed once.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// Byte-copy an arena from its header (at arena offset 0). `Tree` exposes no
/// `&[u8]` (it would race with atomic publishers); here the tree is quiescent.
fn copy_of(header: &ArenaHeader) -> RelocatedArena {
    let len = header.arena_size as usize;
    let src = std::ptr::from_ref(header).cast::<u8>();
    let layout = Layout::from_size_align(len, ARENA_ALIGN).expect("arena layout");
    // SAFETY: `layout` has non-zero size (an arena always holds at least its
    // 256-byte header), so `alloc_zeroed` is being called correctly.
    let raw = unsafe { alloc_zeroed(layout) };
    let ptr = NonNull::new(raw).expect("allocation failed");
    // SAFETY: `src` points to `len` initialized bytes (the live arena, quiescent
    // — every `Publisher` has been dropped, so no writer can be mid-publish).
    // `ptr` owns `len` freshly allocated bytes. The two regions are distinct
    // allocations, so they cannot overlap.
    unsafe { std::ptr::copy_nonoverlapping(src, ptr.as_ptr(), len) };
    RelocatedArena { ptr, len, layout }
}

/// `tf_tree::tree::edge_meta`, private to the facade; if it diverges the plans
/// stop matching and the test fails.
fn edge_meta(view: &ArenaView, eid: EdgeId) -> Option<EdgeMeta> {
    let e = view.edge(eid)?;
    Some(EdgeMeta {
        kind: EdgeKind::from_u8(e.kind),
        domain: e.domain,
        static_pose: Iso3::from_bits(&e.static_pose),
    })
}

/// Evaluate `target <- source` at `stamp` against an arbitrary arena via the
/// full public path.
fn lookup(view: ArenaView, target: &str, source: &str, ns: i64) -> Option<Iso3> {
    let t = view.find_frame(target).ok()??;
    let s = view.find_frame(source).ok()??;
    let plan = compile(&view.topology(), |e| edge_meta(&view, e), t, s).ok()?;
    let guard = Guard::new(view);
    let stamp: Stamp = Stamp::from_nanos(ns);
    plan.at(&guard, stamp).ok()
}

/// The gate: a relocated arena answers identically, bit for bit.
#[test]
fn relocated_arena_answers_bit_identically() {
    // `populated_tree` drops its publishers, so the arena is quiescent.
    let (tree, _samples) = fixture::populated_tree().expect("build fixture");

    let original = tree.arena_view();
    let relocated = copy_of(original.header());

    // The copy must land elsewhere, or the test is vacuous.
    let src_addr = std::ptr::from_ref(original.header()) as usize;
    assert_ne!(
        src_addr,
        relocated.base() as usize,
        "copy landed at the same address; the test would be vacuous"
    );

    // Every frame pair across the history window.
    let names = fixture::frame_names();
    let mut compared = 0usize;
    for (i, &target) in names.iter().enumerate() {
        for &source in names.iter().skip(i + 1) {
            for k in 0..8 {
                let ns = fixture::NOW_NS - k * 137_000_000;
                let a = lookup(tree.arena_view(), target, source, ns);
                let b = lookup(ArenaView::new(&relocated), target, source, ns);
                match (a, b) {
                    (None, None) => {}
                    (Some(a), Some(b)) => {
                        assert_eq!(
                            a.to_bits(),
                            b.to_bits(),
                            "{target} <- {source} @ {ns} differs after relocation"
                        );
                        compared += 1;
                    }
                    (a, b) => panic!(
                        "{target} <- {source} @ {ns}: one arena answered and the \
                         other declined ({}, {})",
                        a.is_some(),
                        b.is_some()
                    ),
                }
            }
        }
    }

    // Guard against a vacuous pass: all-declined proves nothing.
    assert!(
        compared > 1000,
        "only {compared} queries actually compared; the gate is vacuous"
    );
}

/// Frame interning resolves from the relocated bytes too; a cached resolved
/// address would still let plans work.
#[test]
fn frame_interning_survives_relocation() {
    let (tree, _samples) = fixture::populated_tree().expect("build fixture");
    let relocated = copy_of(tree.arena_view().header());
    let view = ArenaView::new(&relocated);

    for &name in &fixture::frame_names() {
        let want = tree
            .arena_view()
            .find_frame(name)
            .expect("original lookup")
            .expect("frame present in original");
        let got = view
            .find_frame(name)
            .expect("relocated lookup")
            .expect("frame present after relocation");
        assert_eq!(want, got, "frame {name} resolved to a different id");
    }

    // A relocated table that resolved everything would pass the loop above.
    assert_eq!(
        view.find_frame("no_such_frame").expect("lookup"),
        None,
        "relocated arena invented a frame"
    );
}

/// The header survives the move: magic, version and layout hash.
#[test]
fn header_identifies_the_relocated_arena() {
    let (tree, _samples) = fixture::populated_tree().expect("build fixture");
    let src = tree.arena_view();
    let relocated = copy_of(src.header());
    let dst = ArenaView::new(&relocated);

    assert_eq!(dst.header().magic, src.header().magic);
    assert_eq!(dst.header().format_version, src.header().format_version);
    assert_eq!(dst.header().layout_hash, src.header().layout_hash);
    assert_eq!(dst.header().arena_size, src.header().arena_size);
    assert_eq!(
        dst.header().pose_slots,
        src.header().pose_slots,
        "ring capacity did not survive relocation"
    );
}
