//! Non-`loom` unit + property tests for the concurrency core.
//!
//! Concurrency *interleavings* are checked by the loom suite (`src/loom_tests.rs`,
//! run under `--cfg loom`); this module covers single-threaded correctness, the
//! arena-view unsafe surface (for Miri), and the wrapped-ring property test (#15).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use alloc::vec::Vec;

use tf_tree_arena::{ArenaLayout, HeapArena};
use tf_tree_math::{exp_se3, Iso3, LerpSlerp};

use crate::arena_view::ArenaView;
use crate::buffer::{PoseSlot, SampleRing};
use crate::edge::{claim, EdgeRecord, Publisher};
use crate::error::{ClaimError, EdgeId, LookupError, PushError};
use crate::sample::ExtrapPolicy;
use crate::sync::{AtomicI64, AtomicU64};

// ---- heap ring harness (exercises buffer + sample directly) -------------

/// A heap-allocated sample ring, mirroring the arena's `SampleRing` pieces but
/// backed by `Vec`s so tests need no arena. Capacity is a power of two.
struct HeapRing {
    head: AtomicU64,
    heartbeat: AtomicU64,
    stamps: Vec<AtomicI64>,
    poses: Vec<PoseSlot>,
    mask: u64,
}

impl HeapRing {
    fn new(capacity: usize) -> HeapRing {
        assert!(capacity.is_power_of_two());
        let mut stamps = Vec::with_capacity(capacity);
        let mut poses = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            stamps.push(AtomicI64::new(0));
            poses.push(PoseSlot::new());
        }
        HeapRing {
            head: AtomicU64::new(0),
            heartbeat: AtomicU64::new(0),
            stamps,
            poses,
            mask: (capacity as u64) - 1,
        }
    }

    fn ring(&self) -> SampleRing<'_> {
        SampleRing {
            head: &self.head,
            heartbeat: &self.heartbeat,
            stamps: &self.stamps,
            poses: &self.poses,
            mask: self.mask,
            edge: EdgeId(0),
        }
    }
}

fn pose(seed: u64) -> Iso3 {
    let f = seed as f64;
    exp_se3([0.01 * f, -0.02 * f, 0.015 * f, 0.1 * f, -0.05 * f, 0.2 * f])
}

#[test]
fn push_then_sample_exact_and_interpolated() {
    let hr = HeapRing::new(8);
    let ring = hr.ring();
    for i in 0..5u64 {
        ring.push(i as i64 * 100, &pose(i)).unwrap();
    }
    // Exact hits round-trip bit-for-bit.
    for i in 0..5u64 {
        let got = ring
            .sample::<LerpSlerp>(i as i64 * 100, ExtrapPolicy::Error)
            .unwrap();
        assert_eq!(got.to_bits(), pose(i).to_bits(), "exact hit {i}");
    }
    // A bracketed query matches the interpolator applied to the two endpoints.
    let a = ring.sample::<LerpSlerp>(100, ExtrapPolicy::Error).unwrap();
    let b = ring.sample::<LerpSlerp>(200, ExtrapPolicy::Error).unwrap();
    let mid = ring.sample::<LerpSlerp>(150, ExtrapPolicy::Error).unwrap();
    let expect = <LerpSlerp as tf_tree_math::Interp>::eval(&a, &b, 0.5);
    assert_eq!(mid.to_bits(), expect.to_bits());
}

#[test]
fn empty_ring_is_no_data() {
    let hr = HeapRing::new(4);
    let err = hr
        .ring()
        .sample::<LerpSlerp>(0, ExtrapPolicy::Error)
        .unwrap_err();
    assert!(matches!(err, LookupError::NoData { .. }));
}

#[test]
fn extrapolation_before_and_after() {
    let hr = HeapRing::new(4);
    let ring = hr.ring();
    ring.push(100, &pose(1)).unwrap();
    ring.push(200, &pose(2)).unwrap();

    let before = ring
        .sample::<LerpSlerp>(50, ExtrapPolicy::Error)
        .unwrap_err();
    assert!(matches!(
        before,
        LookupError::Extrapolation {
            requested: 50,
            oldest: 100,
            newest: 200,
            ..
        }
    ));

    let after = ring
        .sample::<LerpSlerp>(300, ExtrapPolicy::Error)
        .unwrap_err();
    assert!(matches!(
        after,
        LookupError::Extrapolation { requested: 300, .. }
    ));

    // Hold returns the newest sample unchanged.
    let held = ring.sample::<LerpSlerp>(300, ExtrapPolicy::Hold).unwrap();
    assert_eq!(held.to_bits(), pose(2).to_bits());
}

#[test]
fn non_monotonic_push_rejected() {
    let hr = HeapRing::new(4);
    let ring = hr.ring();
    ring.push(100, &pose(1)).unwrap();
    let err = ring.push(50, &pose(2)).unwrap_err();
    assert_eq!(err, PushError::NonMonotonicStamp { last: 100, got: 50 });
    // Equal stamps are accepted (idempotent replay); the newer value wins.
    ring.push(100, &pose(9)).unwrap();
    let got = ring.sample::<LerpSlerp>(100, ExtrapPolicy::Error).unwrap();
    assert_eq!(got.to_bits(), pose(9).to_bits());
}

/// Property test #15: after `3.5 * capacity` pushes onto a wrapped ring, every
/// still-retained sample reads back exactly, and older ones extrapolate-before.
#[test]
#[cfg_attr(miri, ignore = "256-case proptest is too slow under Miri")]
fn wrapped_ring_retained_samples_read_back_exactly() {
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

    let mut runner = TestRunner::new_with_rng(
        Config {
            cases: 256,
            failure_persistence: None,
            ..Config::default()
        },
        TestRng::from_seed(RngAlgorithm::ChaCha, &[0x37; 32]),
    );

    runner
        .run(
            &(4usize..=6).prop_flat_map(|log2| {
                let cap = 1usize << log2;
                let n = cap * 7 / 2 + 3; // > 3.5 * capacity
                (Just(cap), proptest::collection::vec(any::<u64>(), n))
            }),
            |(cap, seeds)| {
                let hr = HeapRing::new(cap);
                let ring = hr.ring();
                let poses: Vec<Iso3> = seeds.iter().map(|&s| pose(s)).collect();
                for (i, p) in poses.iter().enumerate() {
                    ring.push(i as i64 * 10, p).unwrap();
                }
                let total = poses.len() as u64;
                let oldest_retained = total - cap as u64;

                // Retained samples read back bit-exactly at their stamps.
                for logical in oldest_retained..total {
                    let got = ring
                        .sample::<LerpSlerp>(logical as i64 * 10, ExtrapPolicy::Error)
                        .unwrap();
                    prop_assert_eq!(
                        got.to_bits(),
                        poses[logical as usize].to_bits(),
                        "retained logical {}",
                        logical
                    );
                }
                // The sample just older than the window extrapolates-before.
                if oldest_retained > 0 {
                    let too_old = (oldest_retained - 1) as i64 * 10;
                    let err = ring
                        .sample::<LerpSlerp>(too_old, ExtrapPolicy::Error)
                        .unwrap_err();
                    let is_extrap = matches!(err, LookupError::Extrapolation { .. });
                    prop_assert!(is_extrap);
                }
                Ok(())
            },
        )
        .unwrap();
}

// ---- claim / publisher --------------------------------------------------

#[test]
fn claim_is_exclusive_and_epoch_increments() {
    use crate::edge::{release, ClaimRecord};
    let rec = ClaimRecord::new();
    let e1 = claim(&rec, 111, 0xaa).unwrap();
    assert_eq!(e1, 1);
    // Second claim on a live edge fails, naming the owner.
    let err = claim(&rec, 222, 0xbb).unwrap_err();
    assert_eq!(err, ClaimError::EdgeAlreadyClaimed { owner_pid: 111 });
    release(&rec);
    // Re-claim after release bumps the epoch.
    let e2 = claim(&rec, 333, 0xcc).unwrap();
    assert_eq!(e2, 2);
}

#[test]
fn publisher_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Publisher<'static>>();
    // `!Sync` is enforced structurally by the `PhantomData<Cell<()>>` marker on
    // `Publisher`; a positive `Sync` bound on it fails to compile. See the
    // `publisher_is_not_sync` compile-fail note in the doc tests.
}

// ---- end-to-end arena (exercises the unsafe arena_view surface) ---------

fn single_dyn_edge_arena() -> HeapArena {
    // 4 frame slots (root + 3), 1 dynamic edge, ring capacity 4.
    let layout = ArenaLayout::new(4, 1, alloc::vec![4]).unwrap();
    HeapArena::new(&layout, 4242, 0xfeed)
}

#[test]
fn arena_intern_is_idempotent() {
    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    let a = view.intern("base_link").unwrap();
    let b = view.intern("camera").unwrap();
    let a2 = view.intern("base_link").unwrap();
    assert_eq!(a, a2);
    assert_ne!(a, b);
    assert_eq!(a.get(), 1);
    assert_eq!(b.get(), 2);
    // Record round-trips the stored name.
    assert!(view.frame_record(a).name_matches("base_link"));
}

#[test]
fn arena_capacity_exceeded() {
    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    // 4 frame slots => 3 usable frame ids.
    view.intern("a").unwrap();
    view.intern("b").unwrap();
    view.intern("c").unwrap();
    let err = view.intern("d").unwrap_err();
    assert_eq!(err, crate::error::FrameError::CapacityExceeded);
}

#[test]
fn arena_push_claim_sample_roundtrip() {
    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    let parent = view.intern("odom").unwrap();
    let child = view.intern("base_link").unwrap();
    let edge = EdgeId(0);
    view.declare_edge(
        edge,
        EdgeRecord::dynamic(parent.get(), child.get(), 4, 0, 0, 0, 0),
    );

    let epoch = claim(view.claim(edge), 4242, 0xfeed).unwrap();
    let pubr = Publisher::new(view.ring(edge), view.claim(edge), epoch);
    for i in 0..3u64 {
        pubr.push(i as i64 * 1000, &pose(i + 1)).unwrap();
    }
    // A fresh reader ring over the same arena sees the samples.
    let reader = view.ring(edge);
    let got = reader
        .sample::<LerpSlerp>(2000, ExtrapPolicy::Error)
        .unwrap();
    assert_eq!(got.to_bits(), pose(3).to_bits());
    // The claim heartbeat advanced once per push.
    assert_eq!(
        view.claim(edge)
            .heartbeat
            .load(crate::sync::Ordering::Relaxed),
        3
    );
    drop(pubr);
    // After the publisher drops, the edge can be re-claimed.
    assert!(claim(view.claim(edge), 1, 2).is_ok());
}

// ---- topology -----------------------------------------------------------

#[test]
fn topology_depth_and_cycle_detection() {
    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    let a = view.intern("a").unwrap(); // id 1
    let b = view.intern("b").unwrap(); // id 2
    let c = view.intern("c").unwrap(); // id 3
    let topo = view.topology();

    // a is a root; b under a via edge 10; c under b via edge 20 => depths 0,1,2.
    topo.set_parent(b, a.get(), 10).unwrap();
    topo.set_parent(c, b.get(), 20).unwrap();
    assert_eq!(topo.read_frame(a).1, 0);
    assert_eq!(topo.read_frame(b).1, 1);
    assert_eq!(topo.read_frame(c).1, 2);
    assert_eq!(topo.read_frame(b).0, a.get()); // parent
                                               // edge_of_child round-trips and stays consistent with parent across flips.
    assert_eq!(topo.read_frame(b).2, 10);
    assert_eq!(topo.read_frame(c).2, 20);
    assert_eq!(topo.read_frame(a).2, 0); // root has no edge

    // Each successful mutation advanced the generation by 2 (even, stable).
    assert_eq!(topo.generation() % 2, 0);

    // Attaching a under c would close a cycle a->b->c->a.
    let err = topo.set_parent(a, c.get(), 30).unwrap_err();
    assert_eq!(
        err,
        crate::error::TopologyError::WouldCreateCycle { child: a }
    );
    // The failed mutation left the tree intact and the generation stable/even.
    assert_eq!(topo.generation() % 2, 0);
    assert_eq!(topo.read_frame(a).0, 0);
    // And edge_of_child for the earlier attach survived the aborted mutation.
    assert_eq!(topo.read_frame(c).2, 20);
}

// ---- layout asserts -----------------------------------------------------

#[test]
fn record_sizes_are_pinned() {
    use core::mem::{align_of, size_of};
    assert_eq!(size_of::<PoseSlot>(), 64);
    assert_eq!(align_of::<PoseSlot>(), 64);
    assert_eq!(size_of::<EdgeRecord>(), 128);
    assert_eq!(size_of::<crate::edge::ClaimRecord>(), 64);
    assert_eq!(size_of::<crate::frame::FrameRecord>(), 64);
}
