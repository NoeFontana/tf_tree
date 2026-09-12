//! Non-`loom` unit + property tests for the concurrency core.
//!
//! Interleavings are the loom suite's (`src/loom_tests.rs`, `--cfg loom`); this
//! covers single-threaded correctness, the arena-view unsafe surface (Miri) and
//! the wrapped-ring property test (#15).
// `panic` is allowed because a test detecting an *unbounded spin* (see
// `assert_completes_within`) must report the timeout itself, naming what hung.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use alloc::vec::Vec;

use tf_tree_arena::{ArenaLayout, HeapArena};
use tf_tree_math::{exp_se3, Iso3, LerpSlerp, ScLerp, Twist};

use crate::arena_view::{ArenaBuilder, ArenaView};
use crate::buffer::{PoseSlot, SampleRing};
use crate::edge::{claim, EdgeRecord, Publisher};
use crate::error::{ClaimError, EdgeId, FrameError, FrameId, LookupError, PushError};
use crate::layout::Layout;
use crate::participant::ParticipantRecord;
use crate::plan::{Guard, Query, SensorDomain, Stamp, SystemDomain};
use crate::sample::ExtrapPolicy;
use crate::sync::{AtomicI64, AtomicU64};

/// A heap-allocated sample ring, mirroring the arena's `SampleRing` pieces but
/// backed by `Vec`s so tests need no arena. Capacity is a power of two.
struct HeapRing {
    head: AtomicU64,
    heartbeat: AtomicU64,
    stamps: Vec<AtomicI64>,
    poses: Vec<PoseSlot>,
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
        }
    }

    fn ring(&self) -> SampleRing<'_> {
        SampleRing {
            head: &self.head,
            heartbeat: &self.heartbeat,
            stamps: &self.stamps,
            poses: &self.poses,
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
    for i in 0..5u64 {
        let got = ring
            .sample::<LerpSlerp>(i as i64 * 100, ExtrapPolicy::Error)
            .unwrap();
        assert_eq!(got.to_bits(), pose(i).to_bits(), "exact hit {i}");
    }
    let a = ring.sample::<LerpSlerp>(100, ExtrapPolicy::Error).unwrap();
    let b = ring.sample::<LerpSlerp>(200, ExtrapPolicy::Error).unwrap();
    let mid = ring.sample::<LerpSlerp>(150, ExtrapPolicy::Error).unwrap();
    let expect = <LerpSlerp as tf_tree_math::Interp>::eval(&a, &b, 0.5);
    assert_eq!(mid.to_bits(), expect.to_bits());
}

/// `revalidated` is the lap check, and it fires exactly at the bound.
/// The seqlock catches a torn slot, not a recycled one, so six short-circuiting
/// arms once returned a valid pose from a **different stamp**. The bound is
/// `retained`, not `capacity`: `head - i == capacity` is the slot `push`
/// overwrites. Deterministic on purpose — two concurrent harnesses were
/// deleted, no baseline outside the ring being able to avoid over-reporting
/// ([`crate::sample`]'s module docs).
#[test]
fn revalidated_fires_exactly_at_the_retained_bound() {
    const CAP: usize = 8;
    let hr = HeapRing::new(CAP);
    let ring = hr.ring();
    for k in 0..CAP as i64 {
        ring.push(k, &pose(k as u64)).unwrap();
    }
    let retained = CAP as u64 - 1;
    let head = CAP as u64;

    // Newest is safe; oldest-retained is the last safe one; older is overwritten.
    assert!(ring.revalidated_for_test(head - 1, retained).is_ok());
    assert!(ring.revalidated_for_test(head - retained, retained).is_ok());
    assert_eq!(
        ring.revalidated_for_test(head - retained - 1, retained),
        Err(LookupError::SlotRecycled { edge: EdgeId(0) }),
        "the index `push` is overwriting must be refused, not returned"
    );

    // One more push moves the bound by exactly one: a bound, not a constant.
    ring.push(CAP as i64, &pose(CAP as u64)).unwrap();
    assert_eq!(
        ring.revalidated_for_test(head - retained, retained),
        Err(LookupError::SlotRecycled { edge: EdgeId(0) }),
        "the slot that was the oldest safe one is now the one being overwritten"
    );
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

/// Regression: logical index `head - capacity` shares a slot with the next
/// `push`, so retaining `capacity` samples read the *newest* stamp as `t_old`
/// and fabricated an `Extrapolation.oldest` for every in-window query.
#[test]
fn a_wrapped_ring_does_not_retain_the_slot_push_overwrites() {
    let hr = HeapRing::new(4);
    let ring = hr.ring();
    // Five pushes into four slots: 1..=4 look retained, only 2..=4 are.
    for i in 0..5i64 {
        ring.push(i * 10, &pose(i as u64 + 1)).unwrap();
    }

    let got = ring.sample::<LerpSlerp>(25, ExtrapPolicy::Error).unwrap();
    let want = <LerpSlerp as tf_tree_math::Interp>::eval(&pose(3), &pose(4), 0.5);
    assert_eq!(got.to_bits(), want.to_bits(), "bracketed query at t=25");

    // The window's true edges: 20 is the oldest retained, 10 is not.
    assert!(ring.sample::<LerpSlerp>(20, ExtrapPolicy::Error).is_ok());
    let err = ring
        .sample::<LerpSlerp>(10, ExtrapPolicy::Error)
        .unwrap_err();
    assert!(
        matches!(
            err,
            LookupError::Extrapolation {
                requested: 10,
                oldest: 20,
                newest: 40,
                ..
            }
        ),
        "the reported window must be the real one: {err:?}"
    );
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
                // `capacity - 1`: `head - capacity` shares a slot with the
                // next `push`. See `SampleRing::retained`.
                let oldest_retained = total - (cap as u64 - 1);

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

/// [`SampleRing::sample_from`]'s galloping search must return exactly what
/// [`SampleRing::sample`] returns, from **any** starting cursor.
/// No in-tree caller reaches the downward-gallop arm. Both arms only establish
/// `bracket`'s precondition (`stamp[lo] <= t < stamp[hi]`); break it and the
/// caller gets a confidently wrong pose with no error. Wrapped ring (20 pushes
/// into 16 slots) so the cursor clamp runs too. Mutant:
/// `hint.saturating_sub(step)` -> `…(step / 2)` downward ⇒ fails.
#[test]
fn sample_from_agrees_with_sample_from_every_cursor() {
    let hr = HeapRing::new(16);
    let ring = hr.ring();
    for i in 0..20u64 {
        ring.push(i as i64 * 100, &pose(i + 1)).unwrap();
    }
    for t in (300..=2000).step_by(7) {
        let want = ring.sample::<LerpSlerp>(t, ExtrapPolicy::Error);
        for start in 0..21u64 {
            let mut cursor = start;
            let got = ring.sample_from::<LerpSlerp>(t, ExtrapPolicy::Error, &mut cursor);
            match (&got, &want) {
                (Ok(g), Ok(w)) => assert_eq!(g.to_bits(), w.to_bits(), "t={t} start={start}"),
                (Err(g), Err(w)) => assert_eq!(g, w, "t={t} start={start}"),
                _ => panic!("t={t} start={start}: {got:?} vs {want:?}"),
            }
        }
    }
}

/// [`SampleRing::sample_with_twist_from`] must return exactly what
/// [`SampleRing::sample_with_twist`] returns, from **any** starting cursor.
/// They share `bracket_from`, so this asks whether the twist sampler still
/// establishes its precondition (`stamp[lo] <= t < stamp[hi]`). Bit-for-bit,
/// not to a tolerance: a cursor is a hint and must not move the last bit.
/// Wrapped ring (20 pushes into 16 slots) to exercise the cursor clamp.
/// **Mutants, run.** Drop `.clamp(lo_logical, newest)` in `bracket_from` ⇒
/// fails for `start` past the window. Delete the `*cursor = i` write-back ⇒ the
/// loop still passes, so the final block asserts the cursor *advances*. Routing
/// `t == t_new` through `seek` is invisible here, killed instead by
/// `at_the_newest_stamp_the_twist_is_the_left_limit`.
#[test]
fn sample_with_twist_from_agrees_with_sample_with_twist_from_every_cursor() {
    let hr = HeapRing::new(16);
    let ring = hr.ring();
    for i in 0..20u64 {
        ring.push(i as i64 * 100, &pose(i + 1)).unwrap();
    }
    // Off-knot stamps leaving the window at both ends (`retained` 15, window
    // 500..=1900), plus the knots — `t_new` is reached without a search.
    let stamps = (300..=2000)
        .step_by(7)
        .chain([500, 900, 1000, 1300, 1900, 1899, 1901]);
    for t in stamps {
        let want = ring.sample_with_twist(t, ExtrapPolicy::Error);
        for start in 0..21u64 {
            let mut cursor = start;
            let got = ring.sample_with_twist_from(t, ExtrapPolicy::Error, &mut cursor);
            match (&got, &want) {
                (Ok((gp, gv)), Ok((wp, wv))) => {
                    assert_eq!(gp.to_bits(), wp.to_bits(), "pose t={t} start={start}");
                    for (a, b) in [
                        (gv.omega.x, wv.omega.x),
                        (gv.omega.y, wv.omega.y),
                        (gv.omega.z, wv.omega.z),
                        (gv.v.x, wv.v.x),
                        (gv.v.y, wv.v.y),
                        (gv.v.z, wv.v.z),
                    ] {
                        assert_eq!(a.to_bits(), b.to_bits(), "twist t={t} start={start}");
                    }
                }
                (Err(g), Err(w)) => assert_eq!(g, w, "t={t} start={start}"),
                _ => panic!("t={t} start={start}: {got:?} vs {want:?}"),
            }
        }
    }
    // Non-vacuity: the fixture must interpolate somewhere, or the comparisons
    // above are between two copies of one error.
    assert!(ring.sample_with_twist(1234, ExtrapPolicy::Error).is_ok());

    // The cursor must *move*, or the gallop restarts from `lo_logical` every
    // call. Nothing above sees this: the answers are identical either way.
    let mut cursor = 0u64;
    let mut seen = [0u64; 3];
    for (k, t) in [550i64, 1150, 1750].into_iter().enumerate() {
        ring.sample_with_twist_from(t, ExtrapPolicy::Error, &mut cursor)
            .unwrap();
        seen[k] = cursor;
    }
    assert!(
        seen[0] < seen[1] && seen[1] < seen[2],
        "the cursor did not advance across an ascending sweep: {seen:?}"
    );
}

#[test]
fn claim_is_exclusive_and_epoch_increments() {
    use crate::edge::{release, ClaimRecord};
    let rec = ClaimRecord::new();
    let (e1, owner1) = claim(&rec, 111).unwrap();
    assert_eq!(e1, 1);
    assert_eq!(owner1 & 0xFFFF, 112, "low bits are participant_slot + 1");
    assert_eq!(owner1 >> 16, e1, "high bits are the claim epoch");
    // Second claim on a live edge fails, naming the owning *slot* — not a pid.
    let err = claim(&rec, 222).unwrap_err();
    assert_eq!(err, ClaimError::EdgeAlreadyClaimed { owner_slot: 111 });
    release(&rec, owner1);
    let (e2, _) = claim(&rec, 333).unwrap();
    assert_eq!(e2, 2);
}

/// A stale `release` must not free a claim that has passed to somebody else.
/// P1 claims, is `SIGSTOP`ped, reaped; P2 claims; P1 resumes and drops its
/// `Publisher`. `push` already refuses (A4), but an unconditional
/// `owner.store(0)` in `release` frees P2's live claim — two writers on a
/// single-writer ring, through `Drop`. Re-claiming from the same slot (what
/// `ClaimRevoked` asks for) gave an identical owner word under the original
/// `slot + 1` encoding: hence the epoch folded into the word.
#[test]
fn a_stale_release_cannot_free_the_same_slots_new_claim() {
    use crate::edge::{reap, release, ClaimRecord};
    use crate::sync::Ordering;

    let rec = ClaimRecord::new();
    let (_old_epoch, old_owner) = claim(&rec, 7).unwrap();

    // Reaped, then the *same* participant re-claims — what ClaimRevoked says to do.
    reap(&rec);
    let (new_epoch, new_owner) = claim(&rec, 7).unwrap();
    assert_ne!(
        old_owner, new_owner,
        "two acquisitions by one slot produced the same owner word"
    );

    release(&rec, old_owner);

    assert_eq!(
        rec.owner.load(Ordering::Acquire),
        new_owner,
        "a stale release freed the same participant's new claim"
    );
    assert_eq!(rec.epoch.load(Ordering::Acquire), new_epoch);
}

#[test]
fn a_stale_release_cannot_free_someone_elses_claim() {
    use crate::edge::{reap, release, ClaimRecord};
    use crate::sync::Ordering;

    let rec = ClaimRecord::new();
    let (_p1_epoch, p1_owner) = claim(&rec, 1).unwrap();

    // P1 is judged dead and reaped; P2 takes the edge.
    reap(&rec);
    let (p2_epoch, p2_owner) = claim(&rec, 2).unwrap();
    assert_ne!(p1_owner, p2_owner);

    release(&rec, p1_owner);

    assert_eq!(
        rec.owner.load(Ordering::Acquire),
        p2_owner,
        "a stale release freed the new owner's claim"
    );
    assert_eq!(
        rec.epoch.load(Ordering::Acquire),
        p2_epoch,
        "a stale release must not disturb the epoch either"
    );

    release(&rec, p2_owner);
    assert_eq!(rec.owner.load(Ordering::Acquire), 0);
}

/// A4: a writer whose claim was reaped must refuse to publish.
/// `reap` bumps the epoch *before* clearing the owner, so the window is closed
/// from both ends. This was the only amendment with no test.
#[test]
fn a_reaped_writer_refuses_to_push() {
    use crate::edge::reap;

    let mut arena = single_dyn_edge_arena();
    let mut builder = ArenaBuilder::new(&mut arena);
    let parent = builder.view().intern("odom").unwrap();
    let child = builder.view().intern("base_link").unwrap();
    let edge = EdgeId(0);
    builder
        .declare_edge(
            edge,
            EdgeRecord::dynamic(parent.get(), child.get(), 4, 0, 0, 0, 0),
        )
        .unwrap();

    let view = builder.view();
    let (epoch, owner) = claim(view.claim(edge).unwrap(), 7).unwrap();
    let pubr = Publisher::new(
        view.ring(edge).unwrap(),
        view.claim(edge).unwrap(),
        epoch,
        owner,
    );
    let pose = exp_se3([0.0, 0.0, 0.0, 1.0, 2.0, 3.0]);
    pubr.push(10, &pose).expect("push before the reap");

    reap(view.claim(edge).unwrap());

    assert_eq!(
        pubr.push(20, &pose),
        Err(PushError::ClaimRevoked { edge }),
        "a reaped writer kept publishing — two writers can now share one ring"
    );
}

#[test]
fn publisher_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Publisher<'static>>();
    // `!Sync` comes from the `PhantomData<Cell<()>>` marker; see the
    // `publisher_is_not_sync` compile-fail note in the doc tests.
}

fn single_dyn_edge_arena() -> HeapArena {
    // 4 frame slots (root + 3), 1 dynamic edge, ring capacity 4.
    let layout = ArenaLayout::new(4, 1, alloc::vec![4]).unwrap();
    HeapArena::new(&layout, 4242, 0, [0u8; 16])
}

/// A poisoned [`Guard`] must fail **every** evaluation entry point, without
/// reading the view it was handed: `Guard::new` reads the topology generation
/// immediately, so a facade whose arena has gone away (a `fork` child,
/// `docs/decisions/0005` §7) cannot build one at all. Asserting the *error* is
/// what pins the non-reading construction; a reading `poisoned` would fault.
#[test]
fn a_poisoned_guard_refuses_every_evaluation() {
    // Edge index `0` is a topology block's "no edge" sentinel (see
    // `compile_rejects_the_no_edge_sentinel`), so a usable link needs index 1.
    let layout = ArenaLayout::new(4, 2, alloc::vec![4, 4]).unwrap();
    let arena = HeapArena::new(&layout, 4242, 0, [0u8; 16]);
    let view = ArenaView::new(&arena);
    let a = view.intern("base_link").unwrap();
    let b = view.intern("camera").unwrap();
    view.topology().set_parent(b, a.get(), 1).unwrap();

    let plan = crate::plan::compile(
        &view.topology(),
        |eid| {
            view.edge(eid).map(|e| crate::plan::EdgeMeta {
                kind: crate::edge::EdgeKind::from_u8(e.kind),
                domain: e.domain,
                static_pose: Iso3::from_bits(&e.static_pose),
            })
        },
        a,
        b,
    )
    .unwrap();

    let g = Guard::detached(ArenaView::new(&arena));
    assert_eq!(g.poison(), Some(LookupError::ChildDetached));
    let t = Stamp::<SystemDomain>::from_nanos(1);
    assert_eq!(plan.at(&g, t), Err(LookupError::ChildDetached));
    assert_eq!(plan.latest(&g), Err(LookupError::ChildDetached));
    assert_eq!(plan.latest_common(&g), Err(LookupError::ChildDetached));
    // `span` reads the same rings, so a detached guard must silence it too.
    // Mutant: drop `check_generation` from `Plan::span` ⇒ `Ok(None)`.
    assert_eq!(plan.span(&g), Err(LookupError::ChildDetached));
    // Same for `slowest_nominal_rate_mhz`, the other half of
    // `docs/decisions/0018`'s wait: a waiter that missed the poison sleeps on a
    // period read out of an arena it can no longer reach. Mutant: drop its
    // `check_generation` ⇒ `Ok(None)`.
    assert_eq!(
        plan.slowest_nominal_rate_mhz(&g),
        Err(LookupError::ChildDetached)
    );
    assert_eq!(
        plan.query(&g, Query::At(t)),
        Err(LookupError::ChildDetached)
    );
    let mut out = [Iso3::IDENTITY; 1];
    assert_eq!(
        plan.at_many(&g, &[t], &mut out),
        Err(LookupError::ChildDetached)
    );
    let mut raw = [0.0f64; 16];
    assert_eq!(
        plan.at_many_into::<SystemDomain>(&g, &[1], Layout::Mat4, &mut raw),
        Err(LookupError::ChildDetached)
    );

    // Control: a live guard must answer something *other* than the poison, or a
    // plan broken from the start would pass too (unpublished edge ⇒ `NoData`).
    let live = Guard::new(ArenaView::new(&arena));
    assert_ne!(plan.at(&live, t), Err(LookupError::ChildDetached));
}

/// **The detached sentinel must not collide with a real generation.**
/// `Guard` encodes "detached" as `generation == u64::MAX` rather than a 32-byte
/// `Option<LookupError>` per `at()` call. Sound only while no real topology
/// reaches the sentinel; the cost of being wrong is a live guard reporting the
/// unrecoverable `ChildDetached` on a healthy tree. Mutant: `DETACHED = 0`.
#[test]
fn a_generation_mismatch_is_never_mistaken_for_a_detached_guard() {
    // One safe value, pinned: the collision test only reaches small sentinels.
    assert_eq!(
        crate::plan::DETACHED_FOR_TEST,
        u64::MAX,
        "any sentinel below u64::MAX is a generation a real tree can reach"
    );

    let layout = ArenaLayout::new(4, 2, alloc::vec![4, 4]).unwrap();

    // Arena A, mutated so its generation is non-zero, supplies the plan.
    let a_arena = HeapArena::new(&layout, 1, 0, [0u8; 16]);
    let a = ArenaView::new(&a_arena);
    let x = a.intern("x").unwrap();
    let y = a.intern("y").unwrap();
    a.topology().set_parent(y, x.get(), 1).unwrap();
    assert_ne!(a.topology().stable_generation(), 0, "arena A never mutated");
    let plan = crate::plan::compile(
        &a.topology(),
        |eid| {
            a.edge(eid).map(|e| crate::plan::EdgeMeta {
                kind: crate::edge::EdgeKind::from_u8(e.kind),
                domain: e.domain,
                static_pose: Iso3::from_bits(&e.static_pose),
            })
        },
        x,
        y,
    )
    .unwrap();

    // Arena B is untouched, generation 0 — what a careless sentinel would pick.
    let b_arena = HeapArena::new(&layout, 2, 0, [0u8; 16]);
    let b = Guard::new(ArenaView::new(&b_arena));
    assert_eq!(b.generation(), 0);
    assert_eq!(b.poison(), None, "a live guard must never read as detached");

    let err = plan
        .at(&b, Stamp::<SystemDomain>::from_nanos(1))
        .expect_err("a plan from another arena's generation cannot evaluate here");
    assert_eq!(
        err,
        LookupError::TopologyChanged {
            plan: plan.generation(),
            current: 0
        },
        "a generation mismatch was reported as a detached guard"
    );
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
    assert!(view.frame_record(a).unwrap().name_matches("base_link"));
}

/// Regression: the `ids` array lives in a zero-initialized arena, so the
/// "unpublished" sentinel has to be `0`. With `u32::MAX` nothing ever wrote the
/// sentinel, the publish-then-spin wait loop exited immediately with `id == 0`,
/// and a reader racing an interner got a bogus id.
#[test]
fn unpublished_sentinel_is_reachable_in_a_zeroed_arena() {
    assert_eq!(crate::frame::ID_UNPUBLISHED, 0, "sentinel must be zero");
    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    // A never-interned name hits an empty slot: "not found", not a spin.
    assert_eq!(view.find_frame("never_interned").unwrap(), None);
    let a = view.intern("base_link").unwrap();
    assert_eq!(view.find_frame("base_link").unwrap(), Some(a));
}

// A8: interning must not spin forever on a dead claimant. `docs/PHASE2.md` §1
// A8, §11.3 crash point `intern.after_hash_cas_before_id_store`. Interleavings
// are checked by
// `loom_tests::intern_takes_over_from_a_claimant_that_died_before_publishing`;
// these cover the arena-view wiring.

/// PID of the participant these tests declare dead.
const DEAD_PID: u32 = 90_001;
/// PID of the participant that does the rescuing.
const LIVE_PID: u32 = 90_002;

/// Wedge `name`'s hash slot as a process killed between the hash CAS and the id
/// store would leave it. Returns the wedged slot index.
fn wedge_intern_slot(view: &ArenaView, name: &str, owner_slot: u32) -> usize {
    use crate::sync::Ordering;
    let hash = crate::frame::blake3_64(name);
    let hashes = view.frame_hashes();
    let i = (hash & (hashes.len() - 1) as u64) as usize;
    hashes[i].store(hash, Ordering::Release);
    view.frame_claiming()[i].store(owner_slot + 1, Ordering::Release);
    assert_eq!(
        view.frame_ids()[i].load(Ordering::Relaxed),
        crate::frame::ID_UNPUBLISHED,
        "the crash point is *before* the id store"
    );
    i
}

/// Run `f` on its own thread and fail — rather than hang the suite — if it has
/// not finished within `secs`. An A8 regression is an *infinite spin*, not a
/// wrong answer, so a plain `intern` call would wedge CI; the worker is
/// deliberately detached. The deadline is a hang detector, hence the Miri
/// scaling: a bounded spin of milliseconds natively takes ~10 s interpreted.
fn assert_completes_within<T: Send + 'static>(
    secs: u64,
    what: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let secs = if cfg!(miri) { secs * 30 } else { secs };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(v) => v,
        Err(_) => panic!("{what} did not finish within {secs}s — A8 regression (unbounded spin)"),
    }
}

/// A8: an interner whose hash-slot claimant is dead takes the entry over instead
/// of spinning on an id that will never be published.
#[test]
fn intern_recovers_from_a_claimant_that_died_before_publishing() {
    use crate::sync::Ordering;

    let arena = single_dyn_edge_arena();
    let (dead_slot, live_slot, wedged) = {
        let view = ArenaView::new(&arena);
        let (dead_slot, _) = view.participants().register(DEAD_PID, 7, 0).unwrap();
        let (live_slot, _) = view.participants().register(LIVE_PID, 8, 0).unwrap();
        // The dead slot still reads LIVE (`SIGKILL` never clears it), which is
        // why the injected predicate detects the crash and not `state`.
        let wedged = wedge_intern_slot(&view, "victim", dead_slot);
        (dead_slot, live_slot, wedged)
    };

    let (id, again, count, claimant, found) =
        assert_completes_within(10, "intern of a name whose claimant died", move || {
            let is_alive =
                |_slot: u32, rec: &ParticipantRecord| rec.pid.load(Ordering::Relaxed) != DEAD_PID;
            let view = ArenaView::new(&arena)
                .as_participant(live_slot)
                .with_liveness(&is_alive);
            let id = view.intern("victim").unwrap();
            (
                id,
                view.intern("victim").unwrap(),
                view.header().frame_count.load(Ordering::Relaxed),
                view.frame_claiming()[wedged].load(Ordering::Relaxed),
                view.find_frame("victim").unwrap(),
            )
        });

    assert_eq!(id.get(), 1, "the rescued name gets the first frame id");
    assert_eq!(again, id, "a rescued entry must still be idempotent");
    assert_eq!(found, Some(id));
    assert_eq!(count, 1, "the dead claimant never allocated an id");
    assert_eq!(
        claimant,
        live_slot + 1,
        "the rescuer must record itself as the entry's claimant"
    );
    assert_ne!(dead_slot, live_slot);
}

/// A8: a claimant that died before recording itself is recoverable too —
/// `claiming` is still `CLAIM_UNRECORDED`, and a registered interner takes over.
#[test]
fn intern_recovers_when_the_claimant_died_before_recording_itself() {
    use crate::sync::Ordering;

    let arena = single_dyn_edge_arena();
    let live_slot = {
        let view = ArenaView::new(&arena);
        let (slot, _) = view.participants().register(LIVE_PID, 8, 0).unwrap();
        // Hash claimed, nothing else: no claimant recorded, no id published.
        let hash = crate::frame::blake3_64("victim");
        let hashes = view.frame_hashes();
        let i = (hash & (hashes.len() - 1) as u64) as usize;
        hashes[i].store(hash, Ordering::Release);
        assert_eq!(
            view.frame_claiming()[i].load(Ordering::Relaxed),
            crate::frame::CLAIM_UNRECORDED
        );
        slot
    };

    // No liveness predicate is consulted: there is no claimant to resolve.
    let id = assert_completes_within(10, "intern of an unrecorded claimed slot", move || {
        ArenaView::new(&arena)
            .as_participant(live_slot)
            .intern("victim")
    })
    .unwrap();
    assert_eq!(id.get(), 1);
}

/// A8, the fail-safe direction (`docs/PHASE2.md` §6.2): a claimant that cannot
/// be proven dead is **never** stolen from. Waiting costs latency; stealing
/// from a working process costs correctness. The interner is unblocked
/// afterwards, so no spinning thread leaks and the waiter is shown to have been
/// on the normal publish path.
#[test]
fn a_claimant_that_cannot_be_proven_dead_is_never_stolen_from() {
    use crate::sync::Ordering;

    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    let (owner_slot, _) = view.participants().register(DEAD_PID, 7, 0).unwrap();
    let (live_slot, _) = view.participants().register(LIVE_PID, 8, 0).unwrap();
    let wedged = wedge_intern_slot(&view, "victim", owner_slot);
    // Pretend the claimant allocated id 1, so the publish below is a real one.
    let real = ArenaView::new(&arena)
        .as_participant(owner_slot)
        .intern("other")
        .unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            // No injected predicate: the slot reads LIVE, so this must block.
            let _ = tx.send(
                ArenaView::new(&arena)
                    .as_participant(live_slot)
                    .intern("victim"),
            );
        });

        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(250))
                .is_err(),
            "an unproven claimant was stolen from — A8 must fail safe"
        );
        assert_eq!(
            view.frame_claiming()[wedged].load(Ordering::Relaxed),
            owner_slot + 1,
            "the claim must still name the original claimant"
        );

        // Unblock the waiter by publishing on the claimant's behalf.
        view.frame_ids()[wedged].store(real.get(), Ordering::Release);
    });

    // ...and then resolves to whatever the claimant published, as the pre-A8
    // handshake did. ("victim"/"other" share a record only by hand-publishing.)
    assert_eq!(
        rx.recv().unwrap().unwrap_err(),
        FrameError::FrameHashCollision {
            hash: crate::frame::blake3_64("victim")
        }
    );
}

/// A rejected intern must not poison its hash slot: `frame_count` stays exact and
/// the table keeps answering for the names that did fit.
#[test]
fn capacity_rejection_leaves_the_table_usable() {
    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    let a = view.intern("a").unwrap();
    view.intern("b").unwrap();
    view.intern("c").unwrap();
    assert_eq!(view.intern("d").unwrap_err(), FrameError::CapacityExceeded);
    assert_eq!(view.find_frame("a").unwrap(), Some(a));
    assert_eq!(view.intern("a").unwrap(), a);
    assert_eq!(
        view.header()
            .frame_count
            .load(crate::sync::Ordering::Relaxed),
        3
    );
}

/// Regression: `EdgeId` is a public `u32` newtype and `FrameId::new` takes any
/// non-zero `u32`, so out-of-range ids reach these accessors from safe code and
/// must report the miss, not form an out-of-bounds pointer.
#[test]
fn out_of_range_ids_are_rejected_not_dereferenced() {
    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    // The arena has 1 edge slot and 4 frame slots.
    for bad in [EdgeId(1), EdgeId(50_000_000), EdgeId(u32::MAX)] {
        assert!(view.edge(bad).is_none(), "edge({bad:?})");
        assert!(view.claim(bad).is_none(), "claim({bad:?})");
        assert!(view.ring(bad).is_none(), "ring({bad:?})");
        assert!(view.sampler(bad).is_none(), "sampler({bad:?})");
    }
    let bad_frame = FrameId::new(4).unwrap();
    assert!(view.frame_record(bad_frame).is_none());
    assert!(view.topology().read_frame(bad_frame).is_none());
    assert!(view.frame_record(FrameId::new(u32::MAX).unwrap()).is_none());
}

/// A static (zero-capacity) edge has no ring: asking for one is a `None`, not a
/// debug assertion or a mask of `u64::MAX` over an empty slot slice.
#[test]
fn static_edge_has_no_ring() {
    let mut arena = single_dyn_edge_arena();
    let mut builder = ArenaBuilder::new(&mut arena);
    let parent = builder.view().intern("odom").unwrap();
    let child = builder.view().intern("base_link").unwrap();
    builder
        .declare_edge(
            EdgeId(0),
            EdgeRecord::static_edge(parent.get(), child.get(), Iso3::IDENTITY.to_bits(), 0),
        )
        .unwrap();
    let view = builder.view();
    assert!(view.edge(EdgeId(0)).is_some(), "the record itself exists");
    assert!(view.ring(EdgeId(0)).is_none());
    assert!(view.sampler(EdgeId(0)).is_none());
}

/// Declaring an edge outside the edge table is refused, not written past the end
/// of the arena.
#[test]
fn declare_edge_rejects_an_out_of_range_id() {
    let mut arena = single_dyn_edge_arena();
    let mut builder = ArenaBuilder::new(&mut arena);
    let rec = EdgeRecord::dynamic(1, 2, 4, 0, 0, 0, 0);
    assert_eq!(
        builder.declare_edge(EdgeId(u32::MAX), rec).unwrap_err(),
        crate::error::TopologyError::CapacityExceeded
    );
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
    let mut arena = single_dyn_edge_arena();
    let mut builder = ArenaBuilder::new(&mut arena);
    let parent = builder.view().intern("odom").unwrap();
    let child = builder.view().intern("base_link").unwrap();
    let edge = EdgeId(0);
    builder
        .declare_edge(
            edge,
            EdgeRecord::dynamic(parent.get(), child.get(), 4, 0, 0, 0, 0),
        )
        .unwrap();

    let view = builder.view();
    let (epoch, owner) = claim(view.claim(edge).unwrap(), 7).unwrap();
    let pubr = Publisher::new(
        view.ring(edge).unwrap(),
        view.claim(edge).unwrap(),
        epoch,
        owner,
    );
    for i in 0..3u64 {
        pubr.push(i as i64 * 1000, &pose(i + 1)).unwrap();
    }
    let reader = view.ring(edge).unwrap();
    let got = reader
        .sample::<LerpSlerp>(2000, ExtrapPolicy::Error)
        .unwrap();
    assert_eq!(got.to_bits(), pose(3).to_bits());
    assert_eq!(
        view.claim(edge)
            .unwrap()
            .heartbeat
            .load(crate::sync::Ordering::Relaxed),
        3
    );
    drop(pubr);
    assert!(claim(view.claim(edge).unwrap(), 1).is_ok());
}

/// `set_parent` accepts `edge == 0` to mean "only the parent link matters", but
/// edge slot `0` is a real edge record. Compiling a path across such a link must
/// name the problem, not emit `Step::Dyn { edge: EdgeId(0) }` and silently sample
/// an unrelated edge's ring.
#[test]
fn compile_rejects_the_no_edge_sentinel() {
    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    let a = view.intern("a").unwrap();
    let b = view.intern("b").unwrap();
    // `b` has a parent, but the link records no edge.
    view.topology().set_parent(b, a.get(), 0).unwrap();

    let err = crate::plan::compile(
        &view.topology(),
        |eid| {
            view.edge(eid).map(|e| crate::plan::EdgeMeta {
                kind: crate::edge::EdgeKind::from_u8(e.kind),
                domain: e.domain,
                static_pose: Iso3::from_bits(&e.static_pose),
            })
        },
        b,
        a,
    )
    .unwrap_err();
    assert_eq!(err, LookupError::MissingEdge { child: b });
}

/// A chain `f0 -> f1 -> … -> f{links}`; `chain[k]` is `k` edges below the root.
///
/// Edge id == the child's frame id: non-zero, in range and unique, so no link
/// trips the `MissingEdge` sentinel check by accident. `sentinel_at` places
/// that defect deliberately, at a chosen distance along the path.
fn chain_arena(links: usize, sentinel_at: Option<usize>) -> (HeapArena, Vec<FrameId>) {
    use alloc::format;

    let slots = links + 2;
    let layout = ArenaLayout::new(slots as u32, slots as u32, alloc::vec![0; slots]).unwrap();
    let arena = HeapArena::new(&layout, 4242, 0, [0u8; 16]);
    let chain: Vec<FrameId> = {
        let view = ArenaView::new(&arena);
        let mut chain = Vec::new();
        for i in 0..=links {
            chain.push(view.intern(&format!("f{i}")).unwrap());
        }
        for (k, w) in chain.windows(2).enumerate() {
            let edge = if sentinel_at == Some(k + 1) {
                0
            } else {
                w[1].get()
            };
            view.topology().set_parent(w[1], w[0].get(), edge).unwrap();
        }
        chain
    };
    (arena, chain)
}

/// A **Y**: a target branch of `p` links and a source branch of `q`, so
/// `lookup(target, source)` walks `p + q` edges through the lowest common
/// ancestor. Edge ids as in [`chain_arena`]. A straight chain — every other
/// depth test here — cannot see a **per-side** bound, because one side is zero.
fn y_arena(p: usize, q: usize) -> (HeapArena, FrameId, FrameId) {
    use alloc::format;

    let slots = p + q + 2;
    let layout = ArenaLayout::new(slots as u32, slots as u32, alloc::vec![0; slots]).unwrap();
    let arena = HeapArena::new(&layout, 4242, 0, [0u8; 16]);
    let (target, source) = {
        let view = ArenaView::new(&arena);
        let root = view.intern("root").unwrap();
        let mut cur = root;
        for i in 0..p {
            let f = view.intern(&format!("t{i}")).unwrap();
            view.topology().set_parent(f, cur.get(), f.get()).unwrap();
            cur = f;
        }
        let target = cur;
        let mut cur = root;
        for j in 0..q {
            let f = view.intern(&format!("s{j}")).unwrap();
            view.topology().set_parent(f, cur.get(), f.get()).unwrap();
            cur = f;
        }
        (target, cur)
    };
    (arena, target, source)
}

/// A **non-identity** static pose, different per edge, for a synthetic chain.
///
/// It must be a rotation as well as a translation: an all-identity harness
/// makes composition order invisible (`I * I` is `I` in any association) and
/// `docs/decisions/0034`'s rationale (D) turns on that being visible.
fn distinct_pose(edge: u32) -> Iso3 {
    // `pose` is an `exp_se3` of a three-axis twist: no two edges commute.
    pose(u64::from(edge) + 1)
}

fn chain_meta(kind: crate::edge::EdgeKind) -> impl Fn(EdgeId) -> Option<crate::plan::EdgeMeta> {
    move |eid: EdgeId| {
        Some(crate::plan::EdgeMeta {
            kind,
            domain: 0,
            static_pose: distinct_pose(eid.0),
        })
    }
}

/// [`MAX_DEPTH`](crate::MAX_DEPTH) is the length of the **compiled** array:
/// `MAX_DEPTH` dynamic edges compile whole, one more is refused, and the
/// refusal reports the **exact folded length** — what `fold` counting past its
/// output array buys (`0034`). Before it this read `depth: MAX_DEPTH`, rendered
/// as "path depth 16 exceeds the maximum of 16". Mutant: `if n > MAX_DEPTH` ->
/// `if n > MAX_PATH_EDGES` in `fold` ⇒ 4 of 881 fail on `plan.rs:536: range end
/// index 33 out of range for slice of length 32`; `Plan::new` slices
/// `steps[..len]`, so it panics rather than shipping the truncated plan first
/// predicted here.
#[test]
fn the_compiled_bound_is_the_exact_boundary_between_a_plan_and_tree_too_deep() {
    let (arena, chain) = chain_arena(crate::MAX_DEPTH + 1, None);
    let view = ArenaView::new(&arena);
    let meta = chain_meta(crate::edge::EdgeKind::Dynamic);
    let root = chain[0];

    let at_limit = chain[crate::MAX_DEPTH];
    let plan = crate::plan::compile(&view.topology(), &meta, at_limit, root).unwrap();
    assert_eq!(
        plan.len(),
        crate::MAX_DEPTH,
        "a MAX_DEPTH-step path must compile whole"
    );

    let past_limit = chain[crate::MAX_DEPTH + 1];
    assert_eq!(
        crate::plan::compile(&view.topology(), &meta, past_limit, root).unwrap_err(),
        LookupError::TreeTooDeep {
            depth: (crate::MAX_DEPTH + 1) as u16
        },
        "the compiled bound reports the true folded length, not the bound"
    );
}

/// [`MAX_PATH_EDGES`](crate::MAX_PATH_EDGES) is the length of the **walk**, and
/// this is the whole of `0034`: the chain length a dynamic path is refused at
/// compiles to a *single step* when the links are static.
/// The walk reports `MAX_PATH_EDGES + 1`, not a length: it stops for want of
/// buffer, never learning how much further the path went. `fold` can count past
/// its array because its input is bounded; the walk cannot, a corrupt parent
/// chain with a cycle not terminating.
///
/// Mutant: `if nt + ns == MAX_PATH_EDGES` -> `if $n >= MAX_PATH_EDGES` in
/// `push_edge!`, the **per-side** spelling this replaced. Applied twice: a
/// straight chain passes 881/881 (one side holds every edge); the `y_arena` row
/// fails 1 of 881 — 40-up/40-down compiles to `Ok(Plan { len: 1 })` for 80
/// edges, so 64 would have meant 127 walked.
#[test]
fn the_raw_bound_is_the_exact_boundary_the_walk_refuses_at() {
    let (arena, chain) = chain_arena(crate::MAX_PATH_EDGES + 1, None);
    let view = ArenaView::new(&arena);
    let meta = chain_meta(crate::edge::EdgeKind::Static);
    let root = chain[0];

    // The row `0034` exists for: 64 raw edges folding to one constant.
    let at_limit = chain[crate::MAX_PATH_EDGES];
    let plan = crate::plan::compile(&view.topology(), &meta, at_limit, root).unwrap();
    assert_eq!(
        plan.len(),
        1,
        "a static chain folds to one step at any length"
    );

    let past_limit = chain[crate::MAX_PATH_EDGES + 1];
    assert_eq!(
        crate::plan::compile(&view.topology(), &meta, past_limit, root).unwrap_err(),
        LookupError::TreeTooDeep {
            depth: (crate::MAX_PATH_EDGES + 1) as u16
        },
    );

    // The bound is on `nt + ns`, and only a Y can see it: a per-side guard walks
    // all 80 edges of a 40-up/40-down path first. See this test's mutant note.
    let (y, y_target, y_source) = y_arena(40, 40);
    let y_view = ArenaView::new(&y);
    assert_eq!(
        crate::plan::compile(&y_view.topology(), &meta, y_target, y_source).unwrap_err(),
        LookupError::TreeTooDeep {
            depth: (crate::MAX_PATH_EDGES + 1) as u16
        },
        "MAX_PATH_EDGES must mean edges walked, not edges walked on one side"
    );
    // Control: the same Y one edge under the bound compiles, so it is the bound.
    let (ok_y, ok_target, ok_source) = y_arena(32, 32);
    let ok_view = ArenaView::new(&ok_y);
    assert_eq!(
        crate::plan::compile(&ok_view.topology(), &meta, ok_target, ok_source)
            .unwrap()
            .len(),
        1,
        "64 edges walked is exactly the bound and must be accepted"
    );

    // The two bounds are separate numbers: a chain longer than `MAX_DEPTH` but
    // shorter than `MAX_PATH_EDGES` compiles when it folds, not when it does not.
    let mid = chain[crate::MAX_DEPTH + 8];
    assert_eq!(
        crate::plan::compile(&view.topology(), &meta, mid, root)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        crate::plan::compile(
            &view.topology(),
            chain_meta(crate::edge::EdgeKind::Dynamic),
            mid,
            root
        )
        .unwrap_err(),
        LookupError::TreeTooDeep {
            depth: (crate::MAX_DEPTH + 8) as u16
        },
    );
}

/// **Error precedence, which nothing in the workspace pinned** — which is why
/// `0034` could move it invisibly. A table over (defect kind) x (position,
/// before or after the compiled bound fills) x (foldable or not).
/// The discriminating rows are `("unknown edge", 40, all-dynamic)` and its
/// `MixedTimeDomains` twin: 48 dynamic edges, so the array is full by step 32
/// and the defect sits 8 past it, yet `fold` keeps counting and resolves that
/// edge — the caller hears what is *wrong*, not how long the path is. Past the
/// **raw** bound the walk really stops, so the last row's depth has to win.
///
/// Mutant (variant A, `0034` step 3's cheap alternative): an early
/// `if n >= MAX_DEPTH { return Err(TreeTooDeep { depth: n as u16 }) }` in
/// `fold`'s loop ⇒ those two rows plus `clean, all-dynamic` report
/// `TreeTooDeep { depth: 32 }`, losing the precedence *and* the folded length;
/// the other six are the control.
#[test]
fn error_precedence_over_defect_kind_position_and_foldability() {
    use crate::edge::EdgeKind::{Dynamic, Static};

    /// Which defect to place.
    enum Defect {
        /// None: the control row.
        Clean,
        /// `set_parent` recorded edge `0` on the defective link.
        Sentinel,
        /// `edge_meta` answers `None` for the defective edge.
        Unknown,
        /// `edge_meta` answers a second domain tag for it.
        OtherDomain,
    }

    /// Path length and the two positions in **walk order**: the lookup runs
    /// `chain[LEN] -> chain[0]`, so step `k` is `k` edges above the leaf and,
    /// on an all-dynamic path, also compiled step `k` — `AFTER` sits 8 past a
    /// full array.
    const LEN: usize = 48;
    const BEFORE: usize = 5;
    const AFTER: usize = 40;

    let cases: [(
        &str,
        Defect,
        usize,
        usize,
        crate::edge::EdgeKind,
        LookupError,
    ); 9] = [
        // Control: all-dynamic overruns the bound and reports its folded length.
        (
            "clean, all-dynamic",
            Defect::Clean,
            LEN,
            0,
            Dynamic,
            LookupError::TreeTooDeep { depth: LEN as u16 },
        ),
        (
            "missing edge before the bound, all-dynamic",
            Defect::Sentinel,
            LEN,
            BEFORE,
            Dynamic,
            LookupError::MissingEdge {
                child: FrameId::new((LEN - BEFORE + 1) as u32).unwrap(),
            },
        ),
        (
            "missing edge past the bound, all-dynamic",
            Defect::Sentinel,
            LEN,
            AFTER,
            Dynamic,
            LookupError::MissingEdge {
                child: FrameId::new((LEN - AFTER + 1) as u32).unwrap(),
            },
        ),
        (
            "unknown edge before the bound, all-dynamic",
            Defect::Unknown,
            LEN,
            BEFORE,
            Dynamic,
            LookupError::UnknownEdge {
                edge: EdgeId((LEN - BEFORE + 1) as u32),
            },
        ),
        // *** The row that separates variant C from variant A. ***
        (
            "unknown edge past the bound, all-dynamic",
            Defect::Unknown,
            LEN,
            AFTER,
            Dynamic,
            LookupError::UnknownEdge {
                edge: EdgeId((LEN - AFTER + 1) as u32),
            },
        ),
        (
            "mixed domains past the bound, all-dynamic",
            Defect::OtherDomain,
            LEN,
            AFTER,
            Dynamic,
            LookupError::MixedTimeDomains {
                edge: EdgeId((LEN - AFTER + 1) as u32),
                expected: 0,
                got: 7,
            },
        ),
        // All-static: the array never fills, so these are the control.
        (
            "missing edge past the bound, all-static",
            Defect::Sentinel,
            LEN,
            AFTER,
            Static,
            LookupError::MissingEdge {
                child: FrameId::new((LEN - AFTER + 1) as u32).unwrap(),
            },
        ),
        (
            "unknown edge past the bound, all-static",
            Defect::Unknown,
            LEN,
            AFTER,
            Static,
            LookupError::UnknownEdge {
                edge: EdgeId((LEN - AFTER + 1) as u32),
            },
        ),
        // Past the *raw* bound the walk stops, so the defect is unreachable.
        (
            "unknown edge past the raw bound, all-static",
            Defect::Unknown,
            crate::MAX_PATH_EDGES + 6,
            crate::MAX_PATH_EDGES + 4,
            Static,
            LookupError::TreeTooDeep {
                depth: (crate::MAX_PATH_EDGES + 1) as u16,
            },
        ),
    ];

    // Collected, not asserted one by one: which rows move *together* is the point.
    let mut wrong: Vec<alloc::string::String> = Vec::new();
    for (label, defect, links, step, kind, want) in cases {
        // Walk step `step` is the link into `chain[links - step]`, whose edge id
        // is that frame's own id.
        let link = links - step;
        let edge = (link + 1) as u32;
        let sentinel = matches!(defect, Defect::Sentinel).then_some(link);
        let (arena, chain) = chain_arena(links, sentinel);
        let view = ArenaView::new(&arena);

        let unknown = matches!(defect, Defect::Unknown).then_some(edge);
        let other_domain = matches!(defect, Defect::OtherDomain).then_some(edge);
        let meta = move |eid: EdgeId| {
            if unknown == Some(eid.0) {
                return None;
            }
            Some(crate::plan::EdgeMeta {
                kind,
                domain: u8::from(other_domain == Some(eid.0)) * 7,
                static_pose: distinct_pose(eid.0),
            })
        };

        let got = crate::plan::compile(&view.topology(), meta, chain[links], chain[0]).unwrap_err();
        if got != want {
            wrong.push(alloc::format!("{label}: got {got:?}, want {want:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "precedence table:\n  {}",
        wrong.join("\n  ")
    );
}

/// The corpus shape, and the corpus ceiling.
/// ANYmal C is the one structure in the 91-robot URDF survey where folding
/// materially helps: **20 joints** apart folding to **13** steps, the chain
/// being `fixed fixed revolute` six times over — the alternation is the point,
/// a run of statics cannot get the *adjacent*-only collapse rule wrong. At
/// `MAX_DEPTH = 16` it was refused outright. The ceiling rows are the survey's
/// other numbers: the worst diameter is 30 joints (Unitree H2 Plus), which must
/// compile under a deployed `/tf` prefix, and past `MAX_PATH_EDGES` refuses.
#[test]
fn the_anymal_c_shape_folds_and_the_corpus_ceiling_holds() {
    // `fixed fixed revolute` x 6, then two more fixed: 20 links.
    const PATTERN: [crate::edge::EdgeKind; 20] = {
        use crate::edge::EdgeKind::{Dynamic, Static};
        [
            Static, Static, Dynamic, Static, Static, Dynamic, Static, Static, Dynamic, Static,
            Static, Dynamic, Static, Static, Dynamic, Static, Static, Dynamic, Static, Static,
        ]
    };

    let (arena, chain) = chain_arena(PATTERN.len(), None);
    let view = ArenaView::new(&arena);
    // `chain[k]`'s own frame id is the edge into it, and `chain[0]` is id 1, so
    // the links carry edge ids 2..=21 and `PATTERN[k - 1]` is link `k`'s kind.
    let meta = |eid: EdgeId| {
        Some(crate::plan::EdgeMeta {
            kind: PATTERN[eid.0 as usize - 2],
            domain: 0,
            static_pose: distinct_pose(eid.0),
        })
    };
    let plan =
        crate::plan::compile(&view.topology(), meta, chain[PATTERN.len()], chain[0]).unwrap();
    assert_eq!(
        plan.len(),
        13,
        "ANYmal C: 20 raw edges, 6 revolute, adjacent fixed runs collapsed"
    );

    // The ceiling: 30 joints plus `map -> odom -> base_footprint` is 33 steps.
    let (arena, chain) = chain_arena(33, None);
    let view = ArenaView::new(&arena);
    let dynamic = chain_meta(crate::edge::EdgeKind::Dynamic);
    assert_eq!(
        crate::plan::compile(&view.topology(), &dynamic, chain[33], chain[0]).unwrap_err(),
        LookupError::TreeTooDeep { depth: 33 },
        "33 dynamic steps is past MAX_DEPTH and says so exactly"
    );
    // …and the same 33 with fixed offsets static fit — the sized-for deployment.
    let mostly_static = |eid: EdgeId| {
        Some(crate::plan::EdgeMeta {
            kind: if eid.0.is_multiple_of(3) {
                crate::edge::EdgeKind::Dynamic
            } else {
                crate::edge::EdgeKind::Static
            },
            domain: 0,
            static_pose: distinct_pose(eid.0),
        })
    };
    assert_eq!(
        crate::plan::compile(&view.topology(), mostly_static, chain[33], chain[0])
            .unwrap()
            .len(),
        23
    );
}

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
    let read = |f| topo.read_frame(f).unwrap();
    assert_eq!(read(a).1, 0);
    assert_eq!(read(b).1, 1);
    assert_eq!(read(c).1, 2);
    assert_eq!(read(b).0, a.get()); // parent
                                    // edge_of_child round-trips and stays consistent with parent across flips.
    assert_eq!(read(b).2, 10);
    assert_eq!(read(c).2, 20);
    assert_eq!(read(a).2, 0); // root has no edge

    // A1 removed the odd "write in progress" state: each successful mutation is
    // a single publishing store that advances the generation by exactly 1.
    assert_eq!(topo.generation(), 2, "two mutations, two generations");
    let before = topo.generation();

    // Attaching a under c would close a cycle a->b->c->a.
    let err = topo.set_parent(a, c.get(), 30).unwrap_err();
    assert_eq!(
        err,
        crate::error::TopologyError::WouldCreateCycle { child: a }
    );
    // The failed mutation left the tree intact — `a` is still a root.
    assert_eq!(read(a).0, 0);
    // And edge_of_child for the earlier attach survived the aborted mutation.
    assert_eq!(read(c).2, 20);
    // The published topology is byte-identical, so the generation must not
    // advance: that would invalidate every compiled plan in the process.
    assert_eq!(
        topo.generation(),
        before,
        "an aborted set_parent must not bump the generation"
    );
}

#[test]
fn record_sizes_are_pinned() {
    use core::mem::{align_of, size_of};
    assert_eq!(size_of::<PoseSlot>(), 64);
    assert_eq!(align_of::<PoseSlot>(), 64);
    assert_eq!(size_of::<EdgeRecord>(), 128);
    assert_eq!(size_of::<crate::edge::ClaimRecord>(), 64);
    assert_eq!(size_of::<crate::frame::FrameRecord>(), 64);
}

/// A5: a writer killed between the two `seq` stores leaves the slot **odd**,
/// and the next writer to reach it must recover.
/// `docs/PHASE2.md` §1 A5's cross-process failure; within one process the crash
/// took every reader with it. An incrementing writer would read the stale odd
/// `s` and store the *even* `s+1` mid-payload, so readers accept a torn pose.
/// Forcing the parity (`s | 1`) is idempotent and heals it.
#[test]
fn stale_odd_seq_from_a_dead_writer_is_healed_by_the_next_push() {
    let ring = HeapRing::new(4);
    let pose = exp_se3([0.1, 0.2, 0.3, 1.0, 2.0, 3.0]);

    // A writer killed after flipping slot 0 odd: `head` was never bumped.
    ring.poses[0].set_seq_for_test(1);
    assert_eq!(
        ring.ring().read_slot(0),
        Err(LookupError::SlotContended { edge: EdgeId(0) }),
        "a slot left odd must read as contended, not as published"
    );

    // The next writer takes that slot and must leave it even.
    ring.ring().push(1_000, &pose).unwrap();
    let seq = ring.poses[0].seq_for_test();
    assert_eq!(
        seq & 1,
        0,
        "slot still odd after a completed push (seq={seq})"
    );

    // And the payload must now be readable, not merely even.
    let got = ring.ring().read_slot(0).expect("healed slot is readable");
    assert_eq!(got.to_bits(), pose.to_bits());

    // Still *greater* than the stale odd value: the seqlock's monotonicity is
    // what lets a reader detect a concurrent overwrite.
    assert!(seq > 1, "seq went backwards: {seq}");
}

/// `register_at` takes the slot it is named, or nothing.
/// Exclusivity under concurrency is
/// `loom_tests::two_joiners_handed_the_same_slot_cannot_both_take_it`; this
/// covers the two sequential error paths.
#[test]
fn register_at_takes_the_named_slot_or_fails() {
    use crate::participant::{ParticipantError, ParticipantTable};

    let slots: Vec<ParticipantRecord> = (0..4).map(|_| ParticipantRecord::default()).collect();
    let table = ParticipantTable::new(&slots);

    let inc = table.register_at(2, 1234, 99, 0).expect("slot 2 is free");
    assert_eq!(inc, 1);
    assert_eq!(table.identity(2).map(|id| id.0), Some(1234));

    // A second joiner is refused and — the part that matters — not quietly
    // given a different slot, which would break the slot/lock-byte match.
    assert_eq!(
        table.register_at(2, 5678, 100, 0),
        Err(ParticipantError::SlotTaken { slot: 2 })
    );
    for other in [0, 1, 3] {
        assert_eq!(table.identity(other), None, "slot {other} was touched");
    }

    // A slot from a malformed peer response is an error, not a panic.
    assert_eq!(
        table.register_at(4, 1, 1, 0),
        Err(ParticipantError::SlotOutOfRange {
            slot: 4,
            capacity: 4
        })
    );
}

/// A stale `release` must not free a slot that has been reaped and re-registered.
/// The *sequential* half only — the previous two-word guard (load
/// `incarnation`, then CAS `state`) passed this too; its real failure needs the
/// reap and re-`register` to land between the two words, which is
/// `loom_tests::a_late_release_racing_a_slot_handover_frees_nobody`. Keep both.
#[test]
fn a_stale_release_cannot_free_a_reused_participant_slot() {
    use crate::participant::{state_of, ParticipantTable, LIVE};
    use core::sync::atomic::Ordering;

    let slots: Vec<ParticipantRecord> = (0..2).map(|_| ParticipantRecord::default()).collect();
    let table = ParticipantTable::new(&slots);

    let (slot, inc1) = table.register(1234, 99, 0).expect("register");
    // The process dies; a reaper frees the slot, and another process takes it.
    table.release(slot, inc1);
    let (slot2, inc2) = table.register(5678, 100, 0).expect("re-register");
    assert_eq!(slot2, slot, "the freed slot should be the one reused");
    assert_ne!(inc2, inc1);

    // The first process's `Drop` finally runs, one incarnation too late.
    table.release(slot, inc1);

    let rec = table.get(slot).expect("slot");
    assert_eq!(
        state_of(rec.state.load(Ordering::Acquire)),
        LIVE,
        "a stale release freed the slot's new occupant"
    );
    assert_eq!(
        table.identity(slot).map(|id| id.0),
        Some(5678),
        "the new occupant's identity must survive the stale release"
    );
}

/// `reclaim` frees a slot left `LIVE` by a process that never ran `Drop`.
/// `release` needs the incarnation as a separate argument; a reaper has only
/// the word it observed, and `live_word` packs the incarnation into it, so the
/// observed word *is* the guard. `docs/decisions/0028`.
#[test]
fn reclaim_frees_a_slot_from_the_observed_live_word() {
    use crate::participant::{live_word, ParticipantTable, FREE};
    use core::sync::atomic::Ordering;

    let slots: Vec<ParticipantRecord> = (0..2).map(|_| ParticipantRecord::default()).collect();
    let table = ParticipantTable::new(&slots);

    let (slot, inc) = table.register(1234, 99, 0).expect("register");
    // The process is `SIGKILL`ed: no `release`, so the record stays LIVE and the
    // kernel frees its lock byte. A reaper observes the word and acts on it.
    let observed = table.get(slot).expect("slot").state.load(Ordering::Acquire);
    assert_eq!(observed, live_word(inc));
    assert!(table.reclaim(slot, observed), "the word was unchanged");

    assert_eq!(
        table.get(slot).expect("slot").state.load(Ordering::Acquire),
        FREE
    );
    assert_eq!(table.identity(slot), None);
    // And the slot is usable again, which is the whole point of #184.
    let (again, inc2) = table.register(5678, 100, 0).expect("re-register");
    assert_eq!(again, slot);
    assert_eq!(inc2, inc + 1, "the incarnation counter is never recycled");
}

/// `reclaim` also frees a slot left `RESERVED`, which `release` cannot.
/// `RESERVED` is what a process killed between `fill_slot`'s CAS and its
/// publishing store leaves — §11.3's `attach.after_slot_assigned_before_publish`
/// — and carries no incarnation, so the observed word is the only handle on it.
/// Safe because the byte is the occupancy authority (`0028` steps 0b, 0c); the
/// memory-model half is `loom_tests::reclaim_races_register`.
#[test]
fn reclaim_frees_a_slot_from_an_observed_reserved() {
    use crate::participant::{ParticipantTable, FREE, RESERVED};
    use core::sync::atomic::Ordering;

    let slots: Vec<ParticipantRecord> = (0..2).map(|_| ParticipantRecord::default()).collect();
    let table = ParticipantTable::new(&slots);

    // Staged, not raced: a process dead inside `fill_slot` makes no more stores.
    table
        .get(1)
        .expect("slot")
        .state
        .store(RESERVED, Ordering::Release);
    // `register` only ever CASes from FREE, so such a slot was lost for ever.
    assert_eq!(table.register(1, 1, 0).expect("register").0, 0);

    assert!(table.reclaim(1, RESERVED));
    assert_eq!(
        table.get(1).expect("slot").state.load(Ordering::Acquire),
        FREE
    );
    assert_eq!(table.register(5678, 100, 0).expect("register").0, 1);
}

/// `reclaim` fails when the observed word has changed under it.
/// The verdict was formed against one occupancy; a slot freed, re-granted and
/// re-occupied since carries a different `live_word`. `docs/decisions/0028`'s
/// own crash matrix names the row `reclaim.probe_then_reoccupied` — **§11.3 has
/// no such row**, so do not cite one there. The return is *did this CAS fire*,
/// not *is the slot free now*. **This pins the CAS guard** where the loom model
/// `loom_tests::reclaim_races_register` does not: measured, it never reaches
/// that CAS on a contended slot.
#[test]
fn reclaim_fails_when_the_observed_word_has_changed() {
    use crate::participant::{live_word, state_of, ParticipantTable, LIVE, RESERVED};
    use core::sync::atomic::Ordering;

    let slots: Vec<ParticipantRecord> = (0..2).map(|_| ParticipantRecord::default()).collect();
    let table = ParticipantTable::new(&slots);

    let (slot, inc1) = table.register(1234, 99, 0).expect("register");
    let stale = live_word(inc1);
    // Between the observation and the CAS, somebody else reclaims and re-grants.
    assert!(table.reclaim(slot, stale));
    let (slot2, inc2) = table.register(5678, 100, 0).expect("re-register");
    assert_eq!(slot2, slot);
    assert_ne!(inc2, inc1);

    assert!(
        !table.reclaim(slot, stale),
        "a stale verdict freed the slot's new occupant"
    );
    assert_eq!(
        state_of(table.get(slot).expect("slot").state.load(Ordering::Acquire)),
        LIVE
    );
    assert_eq!(table.identity(slot).map(|id| id.0), Some(5678));

    // The same guard on the widened word: a `RESERVED` verdict formed against
    // one occupancy does not fire on a slot that is no longer `RESERVED`.
    assert!(!table.reclaim(slot, RESERVED));
    assert_eq!(table.identity(slot).map(|id| id.0), Some(5678));

    // A slot beyond the table is refused rather than panicking.
    assert!(!table.reclaim(9, live_word(1)));
}

/// A stalled *anonymous* claimant must be reported, in bounded time, to both a
/// would-be interner and a reader — never waited on forever, never stolen from.
/// `CLAIM_ANONYMOUS` is both evidence that somebody real is working and
/// unjudgeable, there being no participant slot to ask `/proc` about. Neither
/// A8 answer applies: takeover would allocate a second id for one name and
/// inflate `frame_count`, abandoning would report "no such frame" for a name
/// being published right now. Hence the third exit, `InternContended`; the
/// first version `continue`d, reintroducing the unbounded spin A8 removes.
#[test]
fn an_anonymous_claimant_is_reported_contended_rather_than_spun_on() {
    use crate::sync::Ordering;

    /// Wedge "victim" as claimed by an anonymous interner that never publishes.
    fn wedge_anonymous(arena: &HeapArena) -> u32 {
        let view = ArenaView::new(arena);
        let (slot, _) = view.participants().register(LIVE_PID, 8, 0).unwrap();
        let hash = crate::frame::blake3_64("victim");
        let hashes = view.frame_hashes();
        let i = (hash & (hashes.len() - 1) as u64) as usize;
        hashes[i].store(hash, Ordering::Release);
        view.frame_claiming()[i].store(crate::frame::CLAIM_ANONYMOUS, Ordering::Release);
        assert_eq!(
            view.frame_ids()[i].load(Ordering::Relaxed),
            crate::frame::ID_UNPUBLISHED,
            "the anonymous claimant must not have published"
        );
        slot
    }

    // The interner path: a registered participant that finds the name claimed.
    let interner = assert_completes_within(10, "intern behind an anonymous claim", || {
        let arena = single_dyn_edge_arena();
        let slot = wedge_anonymous(&arena);
        ArenaView::new(&arena).as_participant(slot).intern("victim")
    });
    assert_eq!(
        interner.unwrap_err(),
        FrameError::InternContended,
        "an interner must be told to retry, not steal and not spin"
    );

    // The reader path, which has no takeover option at all.
    let reader = assert_completes_within(10, "find behind an anonymous claim", || {
        let arena = single_dyn_edge_arena();
        wedge_anonymous(&arena);
        ArenaView::new(&arena).find_frame("victim")
    });
    assert_eq!(
        reader.unwrap_err(),
        FrameError::InternContended,
        "a reader must be told to retry, not told the frame does not exist"
    );
}

// `sample_with_twist`: the extrapolation arms and the left limit. Found by
// review — the `Hold`, `ConstantTwist` and `t == t_new` branches were public
// and covered by no test, and the plan fold always passes `ExtrapPolicy::Error`.

/// A ring with `n` samples at `i * 100` ns, moving along one screw.
fn twist_ring(hr: &HeapRing, n: u64) {
    for i in 0..n {
        hr.ring().push(i as i64 * 100, &pose(i)).unwrap();
    }
}

/// **`Hold` reports a zero twist**: the pose is pinned past `t_new`, so
/// reporting the incoming velocity would tell a controller the body is still
/// moving after the data stopped.
/// Mutant: return the last segment's twist instead of `Twist::ZERO` ⇒ fails.
#[test]
fn hold_extrapolation_reports_a_zero_twist() {
    let hr = HeapRing::new(8);
    twist_ring(&hr, 5);
    let (pose_h, tw) = hr
        .ring()
        .sample_with_twist(10_000, ExtrapPolicy::Hold)
        .expect("hold must succeed past the newest stamp");
    assert_eq!(tw, Twist::ZERO, "a held pose is stationary");
    assert_eq!(pose_h.to_bits(), pose(4).to_bits());
}

/// **`ConstantTwist` extends along the last segment, and reports that twist.**
/// The pose must agree **bit-for-bit** with `sample`'s under the same policy:
/// the two once computed it by different routes (`log_se3`/`exp_se3` against
/// the screw form). Mutant: drop the `NANOS_PER_SEC / dt` scaling ⇒ the twist
/// is per-100-ns and this fails by 1e7.
#[test]
fn constant_twist_extends_the_last_segment_and_agrees_with_sample() {
    let hr = HeapRing::new(8);
    twist_ring(&hr, 5);
    let ring = hr.ring();

    let (p_ct, tw) = ring
        .sample_with_twist(650, ExtrapPolicy::ConstantTwist)
        .expect("constant-twist extrapolation");
    let p_plain = ring
        .sample::<ScLerp>(650, ExtrapPolicy::ConstantTwist)
        .expect("sample must take the same route");
    assert_eq!(
        p_ct.to_bits(),
        p_plain.to_bits(),
        "sample and sample_with_twist disagree on the extrapolated pose"
    );

    // 100 ns apart on one screw, so the twist is the segment's xi per second.
    // Cross-check against the in-window twist: it must be the same screw.
    let (_, tw_inside) = ring
        .sample_with_twist(350, ExtrapPolicy::Error)
        .expect("in-window");
    assert!(
        tw.sub(tw_inside).amax() < 1e-9 * tw_inside.amax(),
        "extrapolated twist {tw:?} differs from the in-window twist {tw_inside:?}"
    );
}

/// **A single sample cannot be extended**, so `ConstantTwist` degrades to Hold
/// for the pose while the derivative is simply absent.
#[test]
fn constant_twist_with_one_sample_is_no_segment() {
    let hr = HeapRing::new(8);
    // A *non-identity* single sample: `twist_ring(&hr, 1)` pushes `pose(0)`,
    // which is `Iso3::IDENTITY`, so an invented identity pose passed unnoticed.
    hr.ring().push(0, &pose(3)).unwrap();
    assert!(matches!(
        hr.ring()
            .sample_with_twist(10_000, ExtrapPolicy::ConstantTwist),
        Err(LookupError::NoSegment { .. })
    ));
    // The plain sample holds the published pose. Mutant: `Iso3::IDENTITY` from
    // `constant_twist`'s `newest == lo_logical` arm ⇒ fails.
    assert_eq!(
        hr.ring()
            .sample::<ScLerp>(10_000, ExtrapPolicy::ConstantTwist)
            .expect("the pose is available; only the derivative is not")
            .to_bits(),
        pose(3).to_bits(),
        "a single-sample ConstantTwist must hold the published sample"
    );
}

/// **At exactly the newest stamp the twist is the left limit** — the segment
/// that *ends* at that knot, since no forward segment exists.
/// Mutant: use `bracket` unconditionally instead of special-casing `t == t_new`
/// ⇒ its precondition breaks and `i + 1` reads outside the published window.
#[test]
fn at_the_newest_stamp_the_twist_is_the_left_limit() {
    let hr = HeapRing::new(8);
    twist_ring(&hr, 5);
    let ring = hr.ring();
    let (p, tw) = ring
        .sample_with_twist(400, ExtrapPolicy::Error)
        .expect("t == t_new is inside the window");
    assert_eq!(
        p.to_bits(),
        pose(4).to_bits(),
        "the pose is the newest sample"
    );

    // The left limit is the [3, 4] segment. Sample just inside it and compare.
    let (_, tw_left) = ring.sample_with_twist(399, ExtrapPolicy::Error).unwrap();
    assert!(
        tw.sub(tw_left).amax() < 1e-9 * tw_left.amax().max(1e-12),
        "twist at t_new is not the left limit: {tw:?} vs {tw_left:?}"
    );
}

/// **Below the oldest retained stamp is an error, not a clamp**, under every
/// extrapolation policy: the policies govern the *newer* end only.
/// Mutant: delete the `t < t_old` guard from `sample_with_twist` ⇒ `bracket`
/// returns a bracket not containing `t`, i.e. a pose from the wrong segment.
#[test]
fn below_the_oldest_stamp_is_an_error_under_every_policy() {
    let hr = HeapRing::new(8);
    twist_ring(&hr, 5);
    for policy in [
        ExtrapPolicy::Error,
        ExtrapPolicy::Hold,
        ExtrapPolicy::ConstantTwist,
    ] {
        match hr.ring().sample_with_twist(-1, policy) {
            Err(LookupError::Extrapolation {
                requested, oldest, ..
            }) => {
                assert_eq!(requested, -1);
                assert_eq!(oldest, 0);
            }
            other => panic!("policy {policy:?} did not refuse an old stamp: {other:?}"),
        }
    }
}

/// **Past the newest stamp under `Error` is refused**, and the error carries the
/// window so the caller can see how far outside it was.
#[test]
fn past_the_newest_stamp_under_error_is_refused_with_the_window() {
    let hr = HeapRing::new(8);
    twist_ring(&hr, 5);
    match hr.ring().sample_with_twist(10_000, ExtrapPolicy::Error) {
        Err(LookupError::Extrapolation {
            requested,
            oldest,
            newest,
            ..
        }) => {
            assert_eq!((requested, oldest, newest), (10_000, 0, 400));
        }
        other => panic!("expected Extrapolation, got {other:?}"),
    }
}

/// An empty ring has no pose and therefore no twist — `NoData`, not
/// `NoSegment`. Different questions; the distinction is why `NoSegment` exists.
#[test]
fn an_empty_ring_is_no_data_not_no_segment() {
    let hr = HeapRing::new(8);
    assert!(matches!(
        hr.ring().sample_with_twist(0, ExtrapPolicy::Error),
        Err(LookupError::NoData { .. })
    ));
}

/// The two fields `Plan::new` derives must equal a fresh scan of the plan's own
/// steps.
/// `Plan::at` used to compute both per call, each an O(`len`) scan over a 1 KiB
/// step array; storing them at compile time bought a value that can go *stale*,
/// which nothing else in the type catches. `first_dynamic_edge`'s `== 1` rule
/// is the subtle one: a plan crossing several dynamic edges must credit **no**
/// edge (`EdgeId(0)`), or `doctor`'s table carries a number meaning something
/// different from its column. Mutants killed, each applied: `>= 1` for `== 1`,
/// and `dyn_count` never incremented. Assigning `first_dyn` on *every* dynamic
/// step is an **equivalent** mutant, being read only when `dyn_count == 1`; the
/// `if dyn_count == 0` guard is clarity, not correctness.
#[test]
fn plan_derived_fields_match_a_fresh_scan() {
    let layout = ArenaLayout::new(8, 4, alloc::vec![4, 4, 4, 4]).unwrap();
    let arena = HeapArena::new(&layout, 7, 0, [0u8; 16]);
    let view = ArenaView::new(&arena);
    let x = view.intern("x").unwrap();
    let y = view.intern("y").unwrap();
    let z = view.intern("z").unwrap();
    view.topology().set_parent(y, x.get(), 1).unwrap();
    view.topology().set_parent(z, y.get(), 2).unwrap();

    // `edge_meta` is the caller's, so steps can be forced without records; and
    // `&ArenaView` is `Copy`, so the inner `move` leaves `meta` re-callable.
    let vref = &view;
    let meta = move |kind: crate::edge::EdgeKind| {
        move |eid: EdgeId| {
            vref.edge(eid).map(|e| crate::plan::EdgeMeta {
                kind,
                domain: e.domain,
                static_pose: Iso3::from_bits(&e.static_pose),
            })
        }
    };
    let dynamic = crate::edge::EdgeKind::Dynamic;
    let stat = crate::edge::EdgeKind::Static;

    for (label, plan) in [
        (
            "identity",
            crate::plan::compile(&view.topology(), meta(dynamic), x, x).unwrap(),
        ),
        (
            "all static",
            crate::plan::compile(&view.topology(), meta(stat), x, z).unwrap(),
        ),
        (
            "one dynamic edge",
            crate::plan::compile(&view.topology(), meta(dynamic), x, y).unwrap(),
        ),
        (
            "two dynamic edges",
            crate::plan::compile(&view.topology(), meta(dynamic), x, z).unwrap(),
        ),
    ] {
        let (stored, scanned) = plan.derived_vs_scan_for_test();
        assert_eq!(
            stored, scanned,
            "{label}: stored fields disagree with a scan"
        );
    }

    // The shapes only guard if they differ, so pin the answers: a `Plan::new`
    // deriving nothing still agrees with a scan of an all-static plan.
    let one = crate::plan::compile(&view.topology(), meta(dynamic), x, y).unwrap();
    assert_eq!(one.derived_vs_scan_for_test().0, (true, EdgeId(1)));
    let two = crate::plan::compile(&view.topology(), meta(dynamic), x, z).unwrap();
    assert_eq!(
        two.derived_vs_scan_for_test().0,
        (true, EdgeId(0)),
        "a plan crossing two dynamic edges must credit no edge"
    );
    let none = crate::plan::compile(&view.topology(), meta(stat), x, z).unwrap();
    assert_eq!(none.derived_vs_scan_for_test().0, (false, EdgeId(0)));
}

/// A five-frame chain `f0 -> … -> f4` over four dynamic edges, edge `i`
/// declaring `rates[i]` milli-hertz (`0` = undeclared).
///
/// Edge index `0` is the topology block's "no edge" sentinel, so the chain
/// occupies edges 1–4; `stamp_off`/`pose_off` are cumulative, as `TreeBuilder`
/// assigns and `ArenaView::ring_of` bounds-checks them. **The rings come back
/// empty** — a declared rate is a property of the topology, not of a stream.
/// Call [`seed_rate_chain`] when the fold must answer.
fn rate_chain_arena(rates: [u32; 4]) -> HeapArena {
    let layout = ArenaLayout::new(8, 5, alloc::vec![0, 4, 4, 4, 4]).unwrap();
    let mut arena = HeapArena::new(&layout, 4242, 0, [0u8; 16]);
    {
        let mut builder = ArenaBuilder::new(&mut arena);
        let mut frames = Vec::new();
        for name in ["f0", "f1", "f2", "f3", "f4"] {
            frames.push(builder.view().intern(name).unwrap());
        }
        for (i, &mhz) in rates.iter().enumerate() {
            let edge = EdgeId(i as u32 + 1);
            let mut record = EdgeRecord::dynamic(
                frames[i].get(),
                frames[i + 1].get(),
                4,
                i as u32 * 4,
                i as u32 * 4,
                0,
                0,
            );
            record.nominal_rate_mhz = mhz;
            builder.declare_edge(edge, record).unwrap();
            builder
                .view()
                .topology()
                .set_parent(frames[i + 1], frames[i].get(), edge.0)
                .unwrap();
        }
    }
    arena
}

/// [`rate_chain_arena`] with the edge table one slot shorter, so `EdgeId(4)` is
/// **out of range** and `ArenaView::edge` answers `None`. The point is the
/// *generation*: `declare_edge` does not touch the topology word, so this
/// arena's generation matches `rate_chain_arena`'s and a plan compiled against
/// the full one gets past `check_generation` to the otherwise unreachable
/// `UnknownEdge` arm.
fn short_edge_table_arena(rates: [u32; 3]) -> HeapArena {
    let layout = ArenaLayout::new(8, 4, alloc::vec![0, 4, 4, 4]).unwrap();
    let mut arena = HeapArena::new(&layout, 4242, 0, [0u8; 16]);
    {
        let mut builder = ArenaBuilder::new(&mut arena);
        let mut frames = Vec::new();
        for name in ["f0", "f1", "f2", "f3", "f4"] {
            frames.push(builder.view().intern(name).unwrap());
        }
        for i in 0..4usize {
            let edge = EdgeId(i as u32 + 1);
            if let Some(&mhz) = rates.get(i) {
                let mut record = EdgeRecord::dynamic(
                    frames[i].get(),
                    frames[i + 1].get(),
                    4,
                    i as u32 * 4,
                    i as u32 * 4,
                    0,
                    0,
                );
                record.nominal_rate_mhz = mhz;
                builder.declare_edge(edge, record).unwrap();
            }
            builder
                .view()
                .topology()
                .set_parent(frames[i + 1], frames[i].get(), edge.0)
                .unwrap();
        }
    }
    arena
}

/// Publish `pose(i)` into edge `i` of a [`rate_chain_arena`] at stamps `0` and
/// `1000`. Returns the pose a full `f0 -> f4` lookup in that window must give.
///
/// The two samples carry the *same* pose, so the composed
/// `pose(1)·pose(2)·pose(3)·pose(4)` has no float slack; one sample would leave
/// the fold with no segment under `ExtrapPolicy::Error`. Each `Publisher` drops
/// per iteration, releasing the claim and leaving its samples.
fn seed_rate_chain(view: &ArenaView<'_>) -> Iso3 {
    let mut expected = Iso3::IDENTITY;
    for i in 1..=4u32 {
        let edge = EdgeId(i);
        let (epoch, owner) = claim(view.claim(edge).unwrap(), 7).unwrap();
        let pubr = Publisher::new(
            view.ring(edge).unwrap(),
            view.claim(edge).unwrap(),
            epoch,
            owner,
        );
        let p = pose(u64::from(i));
        pubr.push(0, &p).unwrap();
        pubr.push(1000, &p).unwrap();
        expected = expected * p;
    }
    expected
}

/// Compile `lookup(target, source)` over an arena built by [`rate_chain_arena`].
fn compile_chain(
    view: &ArenaView<'_>,
    target: crate::error::FrameId,
    source: crate::error::FrameId,
) -> crate::plan::Plan {
    crate::plan::compile(
        &view.topology(),
        |eid| {
            view.edge(eid).map(|e| crate::plan::EdgeMeta {
                kind: crate::edge::EdgeKind::from_u8(e.kind),
                domain: e.domain,
                static_pose: Iso3::from_bits(&e.static_pose),
            })
        },
        target,
        source,
    )
    .unwrap()
}

/// **The four built-in domain tags are `0`–`3`, in that order.**
/// A tag is written into `EdgeRecord::domain` at declaration time, so
/// re-numbering one silently re-interprets arenas already on disk
/// (`docs/API.md` §5.2, "unfixable after the fact"). Distinctness lives in
/// [`the_built_in_domain_tags_are_pairwise_distinct`] on purpose: after these
/// four `assert_eq!`s it could never fail, and it constrains the *set*, which
/// survives a re-numbering. Mutant: `SimDomain::TAG = 0` ⇒ `left: 0, right: 2`.
#[test]
fn the_built_in_domain_tags_are_fixed() {
    use crate::plan::{Domain, SensorDomain, SimDomain, SteadyDomain, SystemDomain};

    assert_eq!(SystemDomain::TAG, 0, "the default domain must stay tag 0");
    assert_eq!(SensorDomain::TAG, 1);
    assert_eq!(SimDomain::TAG, 2);
    assert_eq!(SteadyDomain::TAG, 3);
}

/// **No two built-in domains share a tag.**
/// [`the_built_in_domain_tags_are_fixed`] pins today's literals; this pins the
/// property. `Domain::TAG` is a per-impl constant with nothing structural
/// preventing a collision, and a shared tag is the collapse `docs/API.md` §2.5
/// describes: `TimeDomainMismatch` stops firing between them. Standing alone
/// keeps it reachable. **Not covered:** a *fifth*, user-declared domain
/// colliding with a built-in — §2.5 reserves `0`–`3` by documentation only.
/// Mutant: `SimDomain::TAG = 0` ⇒ fails at the first pair.
#[test]
fn the_built_in_domain_tags_are_pairwise_distinct() {
    use crate::plan::{Domain, SensorDomain, SimDomain, SteadyDomain, SystemDomain};

    let tags = [
        SystemDomain::TAG,
        SensorDomain::TAG,
        SimDomain::TAG,
        SteadyDomain::TAG,
    ];
    for (i, a) in tags.iter().enumerate() {
        for b in &tags[i + 1..] {
            assert_ne!(a, b, "two built-in domains share a tag: {a} and {b}");
        }
    }
}

/// **A sim-time stamp must be refused by a system-domain plan.**
/// With only two built-ins a sim deployment and a steady-clock driver both take
/// [`SystemDomain`] by default, so a node mixing `/clock` with a driver's clock
/// gets a tree wrong by however long the bag has played, well-formed throughout
/// (`docs/API.md` §2.5, §5.2). **The fixture must be seeded**: against empty
/// rings `NoData` shadows the domain check and the silent wrong answer is never
/// demonstrated. Seeded, the refusals below refuse a query that would otherwise
/// be answered, and the control at the end is that query. Mutant:
/// `SimDomain::TAG = 0` ⇒ the first assertion returns `Ok(..)`, bit-identical
/// to the control's pose.
#[test]
fn a_sim_stamp_cannot_query_a_system_domain_plan() {
    use crate::plan::{SimDomain, SteadyDomain};

    let arena = rate_chain_arena([0, 0, 0, 0]);
    let view = ArenaView::new(&arena);
    let expected = seed_rate_chain(&view);
    let (root, leaf) = (view.intern("f0").unwrap(), view.intern("f4").unwrap());
    let plan = compile_chain(&view, root, leaf);
    let g = Guard::new(ArenaView::new(&arena));

    assert_eq!(
        plan.at(&g, Stamp::<SimDomain>::from_nanos(1)),
        Err(LookupError::TimeDomainMismatch {
            expected: 0,
            got: 2
        }),
        "a `/clock` stamp reached a wall-clock plan"
    );
    assert_eq!(
        plan.at(&g, Stamp::<SteadyDomain>::from_nanos(1)),
        Err(LookupError::TimeDomainMismatch {
            expected: 0,
            got: 3
        }),
        "a CLOCK_MONOTONIC stamp reached a wall-clock plan"
    );

    // Control: the plan's own domain gets past the check and is *answered*.
    assert_eq!(
        plan.at(&g, Stamp::<SystemDomain>::from_nanos(1)),
        Ok(expected),
        "the fixture must answer in its own domain, or the refusals above prove nothing"
    );
}

/// **`from_parts` is exact across the whole `i64` range, including below the
/// epoch.**
/// `docs/API.md` §5.1 makes the converter normative and "no float anywhere" its
/// reason: `1_700_000_000_123_456_789` needs 61 bits of significand against
/// `f64`'s 53, so **the sum** cannot survive an `f64` — the *product* can, see
/// B′. `(-1, 250_000_000)` is −750 ms, not −1250 ms, which is what a
/// normalising implementation gets wrong.
///
/// Mutants: plain `i64` `sec * NANOS_PER_SEC + nanos` ⇒ release wraps, range
/// assertions get `Some(..)`. B, all in `f64` ⇒ `left: 1700000000123456768`,
/// 21 ns of error in a stamp that prints exact. B′, staged
/// `(sec as f64 * 1e9) as i128 + nanos as i128` ⇒ **passes** precision
/// (`1.7e18` is `12969970703125 × 2^17`, so the product alone is exact) and
/// dies only at `i64::MIN`, ulp 2048 ns — neither assertion is redundant. C,
/// staged `sec.checked_mul(1e9)?.checked_add(nanos)` ⇒ `i64::MIN` unwraps a
/// `None`, the product alone being below `i64::MIN` while the sum equals it.
#[test]
fn from_parts_is_exact_and_never_wraps() {
    type S = Stamp<SystemDomain>;

    assert_eq!(S::from_parts(0, 0).unwrap().nanos(), 0);
    assert_eq!(
        S::from_parts(1, 500_000_000).unwrap().nanos(),
        1_500_000_000
    );
    assert_eq!(
        S::from_parts(1_700_000_000, 123_456_789).unwrap().nanos(),
        1_700_000_000_123_456_789,
        "a nanosecond of a 2023 wall-clock stamp does not survive an f64"
    );
    assert_eq!(
        S::from_parts(-1, 250_000_000).unwrap().nanos(),
        -750_000_000,
        "a pre-epoch timespec's nanoseconds are a positive remainder"
    );

    // The exact edges of the range; one nanosecond further has no answer.
    assert_eq!(
        S::from_parts(9_223_372_036, 854_775_807).unwrap().nanos(),
        i64::MAX
    );
    assert!(S::from_parts(9_223_372_036, 854_775_808).is_none());
    assert_eq!(
        S::from_parts(-9_223_372_037, 145_224_192).unwrap().nanos(),
        i64::MIN
    );
    assert!(S::from_parts(-9_223_372_037, 145_224_191).is_none());

    // The multiplication overflows long before the addition can, so both are
    // checked — a `checked_add` alone leaves this pair wrapping.
    assert!(S::from_parts(i64::MAX, 0).is_none());
    assert!(S::from_parts(i64::MIN, 0).is_none());
}

/// **A nanosecond field that is not a sub-second remainder is refused, not
/// carried into the seconds.**
/// `builtin_interfaces/Time` and `struct timespec` both define the field as the
/// remainder, so a value outside `[0, 1e9)` means the pair is not a `Time`;
/// normalising is exact and still wrong, turning a malformed message into a
/// plausible stamp. `999_999_999`/`1_000_000_000` pin the bound. Mutant: drop
/// the `nanos >= NANOS_PER_SEC` guard ⇒ `Some(Stamp(1000000000))`, a message
/// claiming second 0 answered as second 1.
#[test]
fn from_parts_refuses_a_nanosecond_field_that_is_not_a_remainder() {
    type S = Stamp<SystemDomain>;

    assert_eq!(S::from_parts(0, 999_999_999).unwrap().nanos(), 999_999_999);
    assert!(S::from_parts(0, 1_000_000_000).is_none());
    assert!(S::from_parts(0, u32::MAX).is_none());
    // ... and the refusal does not depend on the seconds being zero.
    assert!(S::from_parts(1_700_000_000, 2_000_000_000).is_none());
}

/// **`from_timespec` takes the two fields, and refuses a relative interval.**
/// The dependency budget (`docs/PROJECT.md` §5) leaves no `libc::timespec` to
/// accept; both fields are `i64` on a 64-bit target, so the call site casts
/// nothing. POSIX allows a negative `tv_nsec` only in a *relative* `timespec`,
/// so a negative value means an interval converted as an instant — the only
/// thing `from_timespec` adds over `from_parts`. Mutant: drop the
/// `tv_nsec < 0` half ⇒ `(0, -4_294_967_296)` answers `Some(Stamp(0))`, −4.29 s
/// read as the epoch. Only a `tv_nsec` whose **low 32 bits land back inside
/// `[0, 1e9)`** kills it: `-1 as u32` is `4_294_967_295`, which `from_parts`
/// refuses either way, so `(0, -1)` proves nothing.
#[test]
fn from_timespec_refuses_a_relative_interval() {
    type S = Stamp<SensorDomain>;

    assert_eq!(
        S::from_timespec(1_700_000_000, 123_456_789)
            .unwrap()
            .nanos(),
        1_700_000_000_123_456_789
    );
    // Refused by the `as u32` cast, not the sign guard: API doc, not defence.
    assert!(S::from_timespec(0, -1).is_none());
    assert!(S::from_timespec(-1, -1).is_none());
    // The negatives only the sign guard refuses: `-4_294_967_296` is
    // `0xFFFF_FFFF_0000_0000`, so `as u32` is `0`, and `-4_294_967_291`'s low
    // word is `5`. Without the guard these become second 0 and second 7.
    assert!(S::from_timespec(0, -4_294_967_296).is_none());
    assert!(S::from_timespec(7, -4_294_967_291).is_none());
    // Delegation: everything `from_parts` refuses, this refuses too.
    assert!(S::from_timespec(0, 1_000_000_000).is_none());
    assert!(S::from_timespec(i64::MAX, 0).is_none());
    // A negative *second* with a valid remainder is a legitimate pre-epoch stamp.
    assert_eq!(
        S::from_timespec(-1, 250_000_000).unwrap().nanos(),
        -750_000_000
    );
}

/// **The slowest declared rate on the path is the answer, not the fastest.**
/// `docs/decisions/0018` puts the blocking wait in the caller, with
/// `Plan::span` for the shortfall and this for the sleep period. A plan is
/// answerable only once *every* dynamic edge has reached the stamp, so the
/// least frequent publisher decides; the fastest edge's period would wake a
/// hundred times per useful answer against a 10 Hz map update. Rates
/// 1 kHz / 200 Hz / 50 Hz / 10 Hz in *descending* order, so first, last and
/// fastest are three different wrong answers. Mutant: `current.max(mhz)` for
/// `current.min(mhz)` ⇒ `Ok(Some(1000000))`, a 1 ms re-check against an edge
/// publishing every 100 ms. B: return the first declared rate ⇒ the same.
#[test]
fn the_slowest_declared_rate_is_what_a_waiter_sleeps_on() {
    let arena = rate_chain_arena([1_000_000, 200_000, 50_000, 10_000]);
    let view = ArenaView::new(&arena);
    let (root, leaf) = (view.intern("f0").unwrap(), view.intern("f4").unwrap());
    let plan = compile_chain(&view, root, leaf);
    let g = Guard::new(ArenaView::new(&arena));

    assert_eq!(
        plan.slowest_nominal_rate_mhz(&g),
        Ok(Some(10_000)),
        "10 Hz is 10000 mHz, and is the edge that decides when the wait ends"
    );

    // Control: a sub-path excluding the 10 Hz edge answers differently.
    let mid = view.intern("f2").unwrap();
    let short = compile_chain(&view, root, mid);
    assert_eq!(short.slowest_nominal_rate_mhz(&g), Ok(Some(200_000)));
}

/// **`0` means undeclared and is skipped; a plan where nobody declares answers
/// `None`.**
/// `EdgeRecord::nominal_rate_mhz` uses `0` for "not declared", which
/// `docs/PHASE5.md` §6's `TFT007` amendment makes load-bearing: read as a rate
/// the sentinel is the minimum of every set, so **one** undeclared edge would
/// silently disable the wait for the whole path. `None` is a third answer, not
/// a degenerate minimum — the caller falls back to a conservative period and
/// says so at startup (`0018` *Consequences*), where `Some(0)` would conflate
/// "nobody declared" with "somebody declared 0 Hz". Mutant: drop the
/// `if mhz == 0 { continue; }` skip ⇒ `Ok(Some(0))`, a waiter computing
/// `1e9 / 0`. B: seed the fold with `Some(u32::MAX)` ⇒ `Ok(Some(4294967295))`.
#[test]
fn an_undeclared_rate_is_skipped_and_an_undeclared_plan_is_none() {
    // Two declared edges around an undeclared one: the skip must not stop the
    // walk, or the 50 Hz edge behind it never contributes.
    let arena = rate_chain_arena([200_000, 0, 50_000, 0]);
    let view = ArenaView::new(&arena);
    let (root, leaf) = (view.intern("f0").unwrap(), view.intern("f4").unwrap());
    let plan = compile_chain(&view, root, leaf);
    let g = Guard::new(ArenaView::new(&arena));
    assert_eq!(
        plan.slowest_nominal_rate_mhz(&g),
        Ok(Some(50_000)),
        "an undeclared edge is skipped, not treated as the slowest thing here"
    );

    let bare = rate_chain_arena([0, 0, 0, 0]);
    let bare_view = ArenaView::new(&bare);
    let (bare_root, bare_leaf) = (
        bare_view.intern("f0").unwrap(),
        bare_view.intern("f4").unwrap(),
    );
    let bare_plan = compile_chain(&bare_view, bare_root, bare_leaf);
    let bare_g = Guard::new(ArenaView::new(&bare));
    assert_eq!(
        bare_plan.slowest_nominal_rate_mhz(&bare_g),
        Ok(None),
        "no edge declared a rate, and that is not the same as declaring 0 Hz"
    );
}

/// **The rate is readable before anything has ever been published.**
/// Where the method deliberately parts company with `Plan::span`: a declaration
/// is a property of the topology, a window of the stream. The caller asking how
/// long to sleep is asking *before* the data exists — `0018`'s whole startup
/// case — so `NoData` here would make it unusable. The `span` assertion beside
/// it is the control: same plan, same guard, empty rings that *do* stop `span`.
/// Mutant: implement it through `Guard::window` so it inherits that refusal ⇒
/// `Err(NoData { edge: EdgeId(1) })`.
#[test]
fn a_declared_rate_does_not_wait_for_a_published_sample() {
    let arena = rate_chain_arena([10_000, 0, 0, 0]);
    let view = ArenaView::new(&arena);
    let (root, leaf) = (view.intern("f0").unwrap(), view.intern("f4").unwrap());
    let plan = compile_chain(&view, root, leaf);
    let g = Guard::new(ArenaView::new(&arena));

    assert_eq!(plan.slowest_nominal_rate_mhz(&g), Ok(Some(10_000)));
    assert_eq!(
        plan.span(&g),
        Err(LookupError::NoData { edge: EdgeId(1) }),
        "the rings really are empty, so the assertion above is not vacuous"
    );
}

/// **A step naming an edge the guard's arena has no record for is reported, not
/// skipped.**
/// The one documented arm a *well-formed* arena cannot reach: `compile` refuses
/// an unknown edge, and a different arena is caught by `check_generation`.
/// [`short_edge_table_arena`] is the residue — same frames, same links, same
/// generation, one fewer edge slot. Contrived deliberately: the arm sits on the
/// path a shim's timeout loop consults every iteration (`docs/decisions/0018`),
/// where a quiet "nobody declared a rate" means a conservative sleep for ever.
/// Mutant: `let Ok(mhz) = g.nominal_rate_mhz(*edge) else { continue };` ⇒
/// `Ok(Some(200000))` instead of `Err(UnknownEdge { edge: EdgeId(4) })`.
#[test]
fn a_step_past_the_end_of_the_edge_table_is_reported() {
    let full = rate_chain_arena([200_000, 200_000, 200_000, 10_000]);
    let full_view = ArenaView::new(&full);
    let (root, leaf) = (
        full_view.intern("f0").unwrap(),
        full_view.intern("f4").unwrap(),
    );
    let plan = compile_chain(&full_view, root, leaf);

    let short = short_edge_table_arena([200_000, 200_000, 200_000]);
    let g = Guard::new(ArenaView::new(&short));

    // Control: the arenas agree on generation, so this is not `TopologyChanged`.
    assert_eq!(
        ArenaView::new(&short).topology().stable_generation(),
        full_view.topology().stable_generation(),
        "the fixture stopped exercising the arm it was built for"
    );
    assert_eq!(
        plan.slowest_nominal_rate_mhz(&g),
        Err(LookupError::UnknownEdge { edge: EdgeId(4) }),
        "a step off the end of the edge table must not read as `undeclared`"
    );
}

/// `ErrBound::new` maps its first argument to `rot_rad` and its second to
/// `trans`, and nothing else in the workspace can tell.
/// Every caller goes through the constructor since `ErrBound` became
/// `#[non_exhaustive]` (`docs/API.md` §7's audit), but both in-repo call sites
/// pass **equal** values and `tf_tree_py`'s `at_adaptive`, which does not, is
/// outside `just test`. Measured: with the fields swapped, `cargo nextest run
/// --workspace` reported 754 of 754 passing before this existed.
#[test]
fn err_bound_new_assigns_rotation_first() {
    let tol = crate::plan::ErrBound::new(0.25, 4.0);
    assert_eq!(
        tol.rot_rad, 0.25,
        "the first argument is the rotation bound"
    );
    assert_eq!(
        tol.trans, 4.0,
        "the second argument is the translation bound"
    );
}

/// One more than the largest value `Guard`'s packed 32-bit cursor represents.
const CURSOR_BLOCK: u64 = 1 << 32;

/// [`Guard`](crate::plan::Guard) packs a per-step search cursor and its edge tag
/// into one `u64`, so the stored cursor is the low 32 bits of a logical index.
/// `head` is monotone and never masked, so past 2^32 pushes every hint is below
/// `lo_logical` and a plain clamp pins it to the *oldest* retained sample for
/// ever; `rebase_hint` lifts it back. The window is narrower than 2^32
/// (`retained = capacity - 1`), so a truncated index has exactly one preimage
/// in it — in the block *below* `newest`'s when the window straddles 2^32.
/// **Mutant:** return `lifted` unconditionally, deleting the `lifted > newest`
/// arm ⇒ the straddling case answers `3 * 2^32 - 4`, one block past the ring.
#[test]
fn a_truncated_cursor_is_lifted_back_onto_the_live_window() {
    use crate::sample::rebase_hint;

    // Below 2^32 nothing is truncated and nothing may change: the caller's
    // clamp must behave exactly as it did before this existed.
    assert_eq!(rebase_hint(15, 10, 20), 15, "in-window hint is untouched");
    assert_eq!(
        rebase_hint(3, 10, 20),
        3,
        "a low hint stays low, then clamps"
    );
    assert_eq!(
        rebase_hint(99, 10, 20),
        99,
        "the caller clamps the high end"
    );

    // Wholly inside one block above 2^32: restoring `newest`'s base recovers it.
    let (lo, newest) = (CURSOR_BLOCK + 10, CURSOR_BLOCK + 20);
    let truth = CURSOR_BLOCK + 15;
    assert_eq!(
        rebase_hint(truth & (CURSOR_BLOCK - 1), lo, newest),
        truth,
        "a hint truncated inside one block is lifted back into it"
    );

    // Straddling a multiple of 2^32: the true index is in the block *below*
    // `newest`'s, so restoring its base overshoots by exactly one block.
    let (lo, newest) = (2 * CURSOR_BLOCK - 6, 2 * CURSOR_BLOCK + 3);
    let truth = 2 * CURSOR_BLOCK - 4;
    assert_eq!(
        rebase_hint(truth & (CURSOR_BLOCK - 1), lo, newest),
        truth,
        "a straddling window's lower block must not be lifted a block too far"
    );

    for truth in lo..=newest {
        assert_eq!(
            rebase_hint(truth & (CURSOR_BLOCK - 1), lo, newest),
            truth,
            "index {truth} did not survive truncation and rebasing"
        );
    }
}

/// The behavioural half: a ring whose head has passed 2^32 must answer a hinted
/// sample exactly as the unhinted search does, from the truncated cursor a
/// `Guard` would have stored.
/// Before `rebase_hint` the hint pinned to the oldest retained sample and the
/// gallop walked the whole window; the answer was right either way, which is
/// why no test saw it. This pins that restoring the hint changed no answer.
/// **Mutant:** return `lo_logical` instead of the lift ⇒ results still match,
/// a hint never changing one; the unit test above covers that direction.
#[test]
fn a_ring_past_two_to_the_thirty_two_samples_from_a_truncated_cursor() {
    let hr = HeapRing::new(64);
    // Place the ring so its window straddles a multiple of 2^32, the case the
    // lift has to correct rather than merely restore.
    // `push` asserts the heartbeat tracks the head (`0014`), so both move.
    hr.head
        .store(2 * CURSOR_BLOCK - 32, crate::sync::Ordering::Release);
    hr.heartbeat
        .store(2 * CURSOR_BLOCK - 32, crate::sync::Ordering::Release);
    let ring = hr.ring();
    for i in 0..64u64 {
        ring.push(i as i64 * 100, &pose(i)).unwrap();
    }

    let (lo, newest) = ring.window_for_test();
    assert!(
        lo < 2 * CURSOR_BLOCK,
        "the window must straddle the boundary"
    );
    assert!(
        newest >= 2 * CURSOR_BLOCK,
        "the window must straddle the boundary"
    );

    for t in [50i64, 100, 1234, 4321, 6300] {
        let want = ring.sample::<ScLerp>(t, ExtrapPolicy::Error);
        for truth in [lo, lo + 7, (lo + newest) / 2, newest] {
            let mut cursor = truth & (CURSOR_BLOCK - 1);
            let got = ring.sample_from::<ScLerp>(t, ExtrapPolicy::Error, &mut cursor);
            assert_eq!(
                got, want,
                "t={t} from a truncated cursor (true index {truth})"
            );
        }
    }
}

/// The tagged query surface must be the typed one with the domain arriving as
/// data — same condition, same error, same answer — because
/// [`Domain`](crate::plan::Domain) is an open trait, so a foreign binding cannot
/// dispatch to `at::<D>` at all
/// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`). Every C, C++ and
/// Python query site hardcoded `Stamp::<SystemDomain>`, leaving any other tag —
/// which `ros/tf_tree_ros/src/bridge_node.cpp` tells operators to configure
/// under `use_sim_time` — unreadable from those languages. **Mutant:**
/// `at_tagged` passes `self.domain` instead of its argument ⇒ the four mismatch
/// assertions get `Ok(..)`, every wrong tag silently correct, which is the
/// one-line "fix" `0038`'s Rationale rejects.
#[test]
fn a_tagged_query_is_the_typed_query_with_the_domain_as_data() {
    use crate::plan::{Domain, SimDomain, SteadyDomain};

    let arena = rate_chain_arena([0, 0, 0, 0]);
    let view = ArenaView::new(&arena);
    let expected = seed_rate_chain(&view);
    let (root, leaf) = (view.intern("f0").unwrap(), view.intern("f4").unwrap());
    let plan = compile_chain(&view, root, leaf);
    let g = Guard::new(ArenaView::new(&arena));

    // The plan's own tag is answered, and answered with the same pose the typed
    // form returns. Without this the test could pass by refusing everything.
    assert_eq!(plan.domain(), SystemDomain::TAG);
    assert_eq!(
        plan.at_tagged(&g, 1, SystemDomain::TAG),
        plan.at(&g, Stamp::<SystemDomain>::from_nanos(1)),
        "the tagged form disagreed with the typed form on the plan's own domain"
    );
    assert_eq!(
        plan.at_tagged(&g, 1, SystemDomain::TAG),
        Ok(expected),
        "the control query was not answered"
    );

    // Every other built-in tag is refused, identically to the typed form.
    assert_eq!(
        plan.at_tagged(&g, 1, SensorDomain::TAG),
        plan.at(&g, Stamp::<SensorDomain>::from_nanos(1))
    );
    assert_eq!(
        plan.at_tagged(&g, 1, SimDomain::TAG),
        plan.at(&g, Stamp::<SimDomain>::from_nanos(1))
    );
    assert_eq!(
        plan.at_tagged(&g, 1, SteadyDomain::TAG),
        plan.at(&g, Stamp::<SteadyDomain>::from_nanos(1))
    );
    assert_eq!(
        plan.at_tagged(&g, 1, SensorDomain::TAG),
        Err(LookupError::TimeDomainMismatch {
            expected: 0,
            got: 1
        })
    );

    // A user-declared tag is refused by tag, not by failing to compile.
    assert_eq!(
        plan.at_tagged(&g, 1, 7),
        Err(LookupError::TimeDomainMismatch {
            expected: 0,
            got: 7
        }),
        "a user domain must be refused by the same check, not by a panic"
    );

    // The derivative and batch shapes carry the same check.
    assert_eq!(
        plan.at_with_derivatives_tagged(&g, 1, SensorDomain::TAG)
            .err(),
        Some(LookupError::TimeDomainMismatch {
            expected: 0,
            got: 1
        })
    );
    let mut out = [0.0f64; 16];
    assert_eq!(
        plan.at_many_into_tagged(&g, &[1], SensorDomain::TAG, Layout::Mat4, &mut out),
        Err(LookupError::TimeDomainMismatch {
            expected: 0,
            got: 1
        })
    );
    let mut out32 = [0.0f32; 12];
    assert_eq!(
        plan.at_many_into_f32_tagged(&g, &[1], SensorDomain::TAG, Layout::Affine32, &mut out32),
        Err(LookupError::TimeDomainMismatch {
            expected: 0,
            got: 1
        })
    );
}

/// **`by_ns == 0` must mean the fold bracketed, and the *order* of two walks is
/// the whole of that guarantee.**
/// `at_extrapolating_tagged` used to fold the pose and *then* walk
/// `newest_common`; a `push` between the two, at or past the query, lifts
/// `common` to `>= nanos`, so `0` gets reported for an invented pose. Measuring
/// first inverts the error into the safe direction, `newest_stamp` being
/// non-decreasing. **The pose is the witness**, `by_ns` alone being unable to
/// tell an honest `0` from a dishonest one: one edge with `x == stamp / STEP`
/// and a half-step query, so a held answer is integral in `x` and an
/// interpolated one is `k + 0.5`.
///
/// **Mutant, run:** move `newest_common` back below the fold ⇒ 3 runs of 3 fail
/// in ~10 ms each; with the walk first, 6 of 6 pass all 200 000 iterations. The
/// first harness did *not* discriminate though its note claimed so — a fixed
/// writer count left the reader on a frontier nothing was moving. Probabilistic
/// even so; the call site's monotonicity argument carries the guarantee.
/// **Ignored under Miri**: 200 000 rounds against a spinning writer, measured
/// at 28 minutes against a five-minute job; `just loom` argues orderings.
#[test]
#[cfg_attr(miri, ignore = "200k rounds against a spinning writer does not finish")]
fn by_ns_zero_is_never_claimed_for_a_pose_the_fold_invented() {
    use core::sync::atomic::{AtomicBool, AtomicI64, Ordering as O};

    const STEP: i64 = 1_000_000; // 1 ms, so a 1 kHz query on a 1 kHz edge
    const ROUNDS: i64 = 200_000;

    let layout = ArenaLayout::new(4, 2, alloc::vec![0, 64]).unwrap();
    let mut arena = HeapArena::new(&layout, 4242, 0, [0u8; 16]);
    {
        let mut builder = ArenaBuilder::new(&mut arena);
        let f0 = builder.view().intern("f0").unwrap();
        let f1 = builder.view().intern("f1").unwrap();
        builder
            .declare_edge(
                EdgeId(1),
                EdgeRecord::dynamic(f0.get(), f1.get(), 64, 0, 0, 0, 0),
            )
            .unwrap();
        builder
            .view()
            .topology()
            .set_parent(f1, f0.get(), 1)
            .unwrap();
    }

    // `x == k` at stamp `k * STEP`, and nothing else moves: a blend at the
    // half-step is `k + 0.5` and a hold is `k`, which is the whole discriminator.
    let at = |k: i64| Iso3 {
        q: tf_tree_math::Quat::IDENTITY,
        t: tf_tree_math::Vec3 {
            x: k as f64,
            y: 0.0,
            z: 0.0,
        },
    };

    let (root, leaf) = {
        let view = ArenaView::new(&arena);
        (view.intern("f0").unwrap(), view.intern("f1").unwrap())
    };

    let published = AtomicI64::new(-1);
    let stop = AtomicBool::new(false);
    let bad = AtomicI64::new(-1);

    std::thread::scope(|s| {
        // Built inside the writer: D7 makes `Publisher` `!Send`, so no hoisting.
        s.spawn(|| {
            let view = ArenaView::new(&arena);
            let (epoch, owner) = claim(view.claim(EdgeId(1)).unwrap(), 7).unwrap();
            let pubr = Publisher::new(
                view.ring(EdgeId(1)).unwrap(),
                view.claim(EdgeId(1)).unwrap(),
                epoch,
                owner,
            );
            // Until told to stop, **not** a fixed count: a writer that finishes
            // first leaves the reader on a frontier where the race cannot occur.
            let mut k = 0i64;
            while !stop.load(O::Relaxed) {
                pubr.push(k * STEP, &at(k)).unwrap();
                published.store(k, O::Release);
                k += 1;
            }
        });

        let view = ArenaView::new(&arena);
        let plan = compile_chain(&view, root, leaf);
        for _ in 0..ROUNDS {
            // Read the frontier **immediately** before the call, so the next
            // push crosses `t`; a lagging `k` makes every answer honest.
            let k = published.load(O::Acquire);
            if k < 0 {
                continue; // the writer has not published its first sample yet
            }
            // Half a step past the newest sample seen, short of the next one.
            let t = k * STEP + STEP / 2;
            let g = Guard::new(ArenaView::new(&arena));
            let Ok(e) =
                plan.at_extrapolating(&g, Stamp::<SystemDomain>::from_nanos(t), ExtrapPolicy::Hold)
            else {
                continue;
            };
            if e.by_ns != 0 {
                continue;
            }
            // Claimed bracketed. Then the fold must have had a sample at or past
            // `t`, and a blend at the half-step is not integral.
            let x = e.pose.t.x.abs();
            if (x - x.round()).abs() < 1e-9 {
                bad.store(k, O::Release);
                break;
            }
        }
        stop.store(true, O::Release);
    });

    let k = bad.load(O::Acquire);
    assert_eq!(
        k, -1,
        "at round {k} `by_ns == 0` was reported for a held pose: the distance \
         was measured from a walk that ran after the fold, so a push inside \
         that window relabelled an invented answer as interpolated"
    );
}

/// `ExtrapPolicy` is selectable, and the answer reports how far it reached.
/// The three variants were all implemented, all tested at the sampler, and
/// reachable from no shipped surface: every fold site passed the `Error`
/// literal and the facade did not re-export the type
/// (`docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md`).
/// **The third assertion is the one that earns the test**: asserting that
/// `Hold` and `ConstantTwist` each return *something* would pass against a
/// `fold_at_policy` serving `Hold` for both. **Mutant:** `fold_at_policy`
/// passes `ExtrapPolicy::Hold` instead of its `policy` argument ⇒ the run stops
/// at the *first* `at_extrapolating` assertion, so the `ConstantTwist` one is
/// reached and needed only under the narrower mutant of passing `Hold` where
/// `ConstantTwist` was asked for.
#[test]
fn extrapolation_is_selectable_and_reports_how_far_it_reached() {
    let arena = rate_chain_arena([0, 0, 0, 0]);
    let view = ArenaView::new(&arena);
    seed_rate_chain(&view);
    let (root, leaf) = (view.intern("f0").unwrap(), view.intern("f4").unwrap());
    let plan = compile_chain(&view, root, leaf);
    let g = Guard::new(ArenaView::new(&arena));

    // `latest_common` is the point past which any answer is invented.
    let common = plan.newest_common_for_test(&g).unwrap().unwrap().0;
    let past = common + 5_000_000; // 5 ms beyond it

    // Refusing stays the default: `Plan::at` is untouched by this surface.
    assert!(matches!(
        plan.at(&g, Stamp::<SystemDomain>::from_nanos(past)),
        Err(LookupError::Extrapolation { .. })
    ));
    assert!(matches!(
        plan.at_extrapolating(
            &g,
            Stamp::<SystemDomain>::from_nanos(past),
            ExtrapPolicy::Error
        ),
        Err(LookupError::Extrapolation { .. })
    ));

    // Inside the window every policy agrees with `at` and reports zero distance
    // — the control against a plan that extrapolated everything.
    let inside = plan
        .at_extrapolating(
            &g,
            Stamp::<SystemDomain>::from_nanos(common),
            ExtrapPolicy::Hold,
        )
        .unwrap();
    assert_eq!(
        inside.by_ns, 0,
        "a bracketed query was reported as extrapolated"
    );
    assert_eq!(
        Ok(inside.pose),
        plan.at(&g, Stamp::<SystemDomain>::from_nanos(common))
    );

    let held = plan
        .at_extrapolating(
            &g,
            Stamp::<SystemDomain>::from_nanos(past),
            ExtrapPolicy::Hold,
        )
        .unwrap();
    let extended = plan
        .at_extrapolating(
            &g,
            Stamp::<SystemDomain>::from_nanos(past),
            ExtrapPolicy::ConstantTwist,
        )
        .unwrap();

    // Both report the same distance: it is a property of the data, not of the
    // policy, and it is what makes a held pose impossible to mistake for fresh.
    assert_eq!(held.by_ns, 5_000_000);
    assert_eq!(extended.by_ns, 5_000_000);

    // `Hold` really holds: its answer is the pose at `latest_common`.
    assert_eq!(
        held.pose, inside.pose,
        "Hold did not hold the newest sample"
    );

    // ...and `ConstantTwist` really extends: a policy-ignoring fold cannot.
    assert_ne!(
        extended.pose, held.pose,
        "ConstantTwist returned the held pose, so the policy was ignored"
    );
}

/// Every variant of every error enum renders as prose and names what it carries.
/// Coverage, not wording: `docs/API.md` R5 is NORMATIVE that message *text* is
/// not a compatibility promise, so this asserts only that a message exists, is
/// not the `Debug` spelling, and carries the variant's identifier. `error.rs`'s
/// match is exhaustive, so a new variant fails to compile there; this is the
/// other half. **Mutant:** `LookupError::WrongElementType`'s arm becomes
/// `write!(f, "")` ⇒ "WrongElementType produced nothing", which a catch-all
/// `Debug` arm would hide.
#[test]
fn every_error_variant_renders_as_prose_naming_what_it_carries() {
    use alloc::format;

    use crate::error::TopologyError;

    let edge = EdgeId(3);
    let frame = FrameId::new(7).unwrap();

    let lookups = alloc::vec![
        LookupError::UnknownFrame { hash: 0xdead_beef },
        LookupError::Disconnected {
            target: frame,
            source: frame,
            cut_at: frame
        },
        LookupError::TreeTooDeep { depth: 99 },
        LookupError::NoData { edge },
        LookupError::Extrapolation {
            edge,
            requested: 5,
            oldest: 1,
            newest: 4
        },
        LookupError::SlotRecycled { edge },
        LookupError::SlotContended { edge },
        LookupError::TopologyChanged {
            plan: 1,
            current: 2
        },
        LookupError::TimeDomainMismatch {
            expected: 1,
            got: 0
        },
        LookupError::MixedTimeDomains {
            edge,
            expected: 1,
            got: 0
        },
        LookupError::UnknownEdge { edge },
        LookupError::FrameOutOfRange { frame },
        LookupError::BufferTooSmall { need: 48, got: 16 },
        LookupError::WrongElementType,
        LookupError::ChildDetached,
        LookupError::MissingEdge { child: frame },
        LookupError::DerivativesUnavailable { edge, interp: 1 },
        LookupError::NoSegment { edge },
    ];
    for e in &lookups {
        let shown = format!("{e}");
        assert!(
            !shown.is_empty(),
            "every LookupError variant renders: {e:?} produced nothing"
        );
        assert_ne!(
            shown,
            format!("{e:?}"),
            "a variant fell through to Debug instead of prose"
        );
    }

    // The identifier each variant carries has to survive into the message —
    // that is what makes the error actionable without an arena (D11).
    assert!(format!("{}", LookupError::NoData { edge }).contains('3'));
    assert!(format!("{}", LookupError::FrameOutOfRange { frame }).contains('7'));
    assert!(format!("{}", LookupError::BufferTooSmall { need: 48, got: 16 }).contains("48"));

    for e in [
        PushError::NonMonotonicStamp { last: 9, got: 4 },
        PushError::ClaimRevoked { edge },
        PushError::ChildDetached,
    ] {
        assert!(!format!("{e}").is_empty());
        assert_ne!(format!("{e}"), format!("{e:?}"));
    }
    for e in [
        FrameError::FrameHashCollision { hash: 1 },
        FrameError::CapacityExceeded,
        FrameError::InternContended,
        FrameError::ChildDetached,
        FrameError::ReadOnly,
    ] {
        assert!(!format!("{e}").is_empty());
        assert_ne!(format!("{e}"), format!("{e:?}"));
    }
    for e in [
        TopologyError::WouldCreateCycle { child: frame },
        TopologyError::CapacityExceeded,
        TopologyError::UnknownFrame { frame: 4 },
    ] {
        assert!(!format!("{e}").is_empty());
        assert_ne!(format!("{e}"), format!("{e:?}"));
    }
    let c = ClaimError::EdgeAlreadyClaimed { owner_slot: 5 };
    assert!(format!("{c}").contains('5'));
}

/// The thing this crate's own documentation said could not be done.
/// `Tree::await_frames`' example was published as a ```text``` block because
/// *"the three calls yield `OpenError`, `AwaitError` and `LookupError`, and
/// `LookupError` implements neither `Display` nor `Error`, so no single
/// `?`-chain unifies them — not even into `Box<dyn Error>`."* This is that
/// `?`-chain, compiled: the acceptance test for `docs/decisions/0040`.
///
/// **Mutant:** delete `impl core::error::Error for LookupError` ⇒ does not
/// compile, `the trait bound LookupError: core::error::Error is not satisfied`
/// on the `?`. Shaped as a function that must type-check, a compile failure
/// being the stronger assertion.
#[test]
fn an_error_can_leave_a_function_as_box_dyn_error() {
    use alloc::boxed::Box;

    fn fallible(fail: bool) -> Result<Iso3, Box<dyn core::error::Error>> {
        if fail {
            // The `?` is the whole test: it needs `Error`, which needs `Display`.
            Err(LookupError::NoData { edge: EdgeId(3) })?;
        }
        Ok(Iso3::IDENTITY)
    }

    assert!(fallible(false).is_ok());
    let boxed = fallible(true).unwrap_err();
    assert!(
        alloc::format!("{boxed}").contains('3'),
        "the boxed error lost the edge it names: {boxed}"
    );

    // Two *different* error types through one `?`-chain, which is the shape the
    // startup sequence needs and the one the doc comment called impossible.
    fn two_kinds(which: u8) -> Result<(), Box<dyn core::error::Error>> {
        match which {
            0 => Err(FrameError::CapacityExceeded)?,
            _ => Err(LookupError::ChildDetached)?,
        }
    }
    assert!(two_kinds(0).is_err());
    assert!(two_kinds(1).is_err());
}

/// `Extrapolated::by_ns` must not wrap when the query and the data are more
/// than `i64::MAX` nanoseconds apart.
/// A plain `nanos - common` panics in a checked build — on the wait-free read
/// path — and wraps in a release one, where the negative clamps to `0` and
/// reports *"not extrapolated"* for the most extrapolated answer the type can
/// hold. `sample::span_ns` carries the same argument one layer down. **Mutant:**
/// restore `(nanos - common).max(0)` ⇒ release asserts `left: 0, right:
/// 9223372036854775807`; debug panics with `attempt to subtract with overflow`.
#[test]
fn an_extrapolation_distance_saturates_instead_of_wrapping() {
    let arena = rate_chain_arena([0, 0, 0, 0]);
    let view = ArenaView::new(&arena);
    // Seed the chain near the bottom of the range, then query near the top.
    for i in 1..=4u32 {
        let edge = EdgeId(i);
        let (epoch, owner) = claim(view.claim(edge).unwrap(), 7).unwrap();
        let w = Publisher::new(
            view.ring(edge).unwrap(),
            view.claim(edge).unwrap(),
            epoch,
            owner,
        );
        w.push(i64::MIN + 1, &pose(u64::from(i))).unwrap();
        w.push(i64::MIN + 2, &pose(u64::from(i) + 1)).unwrap();
    }
    let (root, leaf) = (view.intern("f0").unwrap(), view.intern("f4").unwrap());
    let plan = compile_chain(&view, root, leaf);
    let g = Guard::new(ArenaView::new(&arena));

    let far = plan
        .at_extrapolating(
            &g,
            Stamp::<SystemDomain>::from_nanos(i64::MAX),
            ExtrapPolicy::Hold,
        )
        .expect("Hold answers past the newest sample");
    assert_eq!(
        far.by_ns,
        i64::MAX,
        "a distance wider than i64 must saturate, never wrap to look fresh"
    );
}

/// The three sizes `0042` moved, pinned so they cannot drift back silently.
/// `Iso3` was `#[repr(C, align(64))]` with an 8-byte pad, so that *"the Phase 2
/// shared-memory arena can store slots without re-deriving layout"*. The arena
/// re-derived it anyway (`buffer::PoseSlot` is its own `align(64)` of atomics)
/// and has never had an `Iso3` field, so the alignment cost every in-memory use
/// eight bytes and a 64-byte stride for nothing. These are *figures*, not
/// invariants — what this catches is a field added to `Iso3`, silent otherwise
/// and doubling a per-thread cache.
///
/// **Mutant:** add an 8-byte field and fix `Iso3`'s two constructors ⇒
/// `left: 64, right: 56`, `Plan` doubling behind it. **Restoring `align(64)` is
/// not a usable mutant**: with the `_pad` back the constructors no longer
/// initialise every field, without it the `Pod` derive refuses the trailing
/// padding — so it cannot come back silently at all.
#[test]
fn the_sizes_0042_halved_stay_halved() {
    use core::mem::{align_of, size_of};

    assert_eq!(size_of::<Iso3>(), 56, "Iso3 was 64 with an 8-byte pad");
    assert_eq!(align_of::<Iso3>(), 8, "Iso3 was align(64)");
    assert_eq!(
        size_of::<crate::plan::Step>(),
        64,
        "a Step was 128: Iso3's 64 plus a discriminant that forced a second cacheline"
    );
    assert_eq!(
        size_of::<crate::plan::Plan>(),
        2064,
        "a Plan was 4160, and the facade caches sixteen of them per thread"
    );
    // What the `Pod` derive rests on, once the pad is gone: seven f64, no more.
    assert_eq!(size_of::<Iso3>(), 7 * size_of::<f64>());
}
