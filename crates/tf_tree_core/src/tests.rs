//! Non-`loom` unit + property tests for the concurrency core.
//!
//! Concurrency *interleavings* are checked by the loom suite (`src/loom_tests.rs`,
//! run under `--cfg loom`); this module covers single-threaded correctness, the
//! arena-view unsafe surface (for Miri), and the wrapped-ring property test (#15).
// `panic` is allowed: `assert_completes_within` reports its own timeout.
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

// ---- heap ring harness (exercises buffer + sample directly) -------------

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

/// **`revalidated` is the lap check and fires exactly at the bound.**
///
/// The seqlock catches a torn slot, not a recycled one; every short-circuiting arm
/// (`Hold`, an exact hit on the newest stamp, `constant_twist`'s single sample) goes
/// through this helper. The bound is `retained`, not `capacity`.
///
/// Deterministic on purpose: a concurrent version needs a baseline that cannot
/// over-report, and none exists (hazards in [`crate::sample`]'s module docs).
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

    // The newest index is always safe; the oldest still-retained one is the last
    // safe one; one older than that is the slot `push` is overwriting.
    assert!(ring.revalidated_for_test(head - 1, retained).is_ok());
    assert!(ring.revalidated_for_test(head - retained, retained).is_ok());
    assert_eq!(
        ring.revalidated_for_test(head - retained - 1, retained),
        Err(LookupError::SlotRecycled { edge: EdgeId(0) }),
        "the index `push` is overwriting must be refused, not returned"
    );

    // One more push moves the bound by exactly one.
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

    // Hold returns the newest sample unchanged.
    let held = ring.sample::<LerpSlerp>(300, ExtrapPolicy::Hold).unwrap();
    assert_eq!(held.to_bits(), pose(2).to_bits());
}

/// The refusal names the ring's own edge (D11); the ring uses a non-zero edge id
/// so a hard-coded `EdgeId(0)` fails.
///
/// **Mutant:** in `SampleRing::push`, `edge: self.edge` -> `edge: EdgeId(0)`.
#[test]
fn non_monotonic_push_rejected() {
    let hr = HeapRing::new(4);
    let ring = SampleRing {
        edge: EdgeId(5),
        ..hr.ring()
    };
    ring.push(100, &pose(1)).unwrap();
    let err = ring.push(50, &pose(2)).unwrap_err();
    assert_eq!(
        err,
        PushError::NonMonotonicStamp {
            edge: EdgeId(5),
            last: 100,
            got: 50
        }
    );
    // Equal stamps are accepted (idempotent replay); the newer value wins.
    ring.push(100, &pose(9)).unwrap();
    let got = ring.sample::<LerpSlerp>(100, ExtrapPolicy::Error).unwrap();
    assert_eq!(got.to_bits(), pose(9).to_bits());
}

/// Regression: on a wrapped ring logical `head - capacity` shares a slot with the
/// next push, so retaining `capacity` samples read the newest stamp as `t_old`.
#[test]
fn a_wrapped_ring_does_not_retain_the_slot_push_overwrites() {
    let hr = HeapRing::new(4);
    let ring = hr.ring();
    // Five pushes into four slots: only logical 2..=4 (stamps 20..40) are retained.
    for i in 0..5i64 {
        ring.push(i * 10, &pose(i as u64 + 1)).unwrap();
    }

    // An in-window query must interpolate, not report extrapolation.
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
                // `capacity - 1`, not `capacity` (see `SampleRing::retained`).
                let oldest_retained = total - (cap as u64 - 1);

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

/// [`SampleRing::sample_from`] must return exactly what [`SampleRing::sample`]
/// returns from **any** starting cursor.
///
/// The downward-gallop arm is unreachable from in-tree callers (cursors seed to 0);
/// breaking `bracket`'s precondition yields a confidently wrong pose. The ring is
/// wrapped (20 pushes into 16 slots) so the cursor clamp is exercised.
///
/// Mutant: `hint.saturating_sub(step)` -> `hint.saturating_sub(step / 2)` in the
/// downward arm.
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

/// [`SampleRing::sample_with_twist_from`] must equal
/// [`SampleRing::sample_with_twist`] from any cursor, bit-for-bit: a cursor is a hint
/// and must not move the last bit. Wrapped ring, as above.
///
/// Mutants: dropping `.clamp(lo_logical, newest)` in `bracket_from` fails here;
/// deleting the `*cursor = i` write-back does not (a stale hint is still a hint), so
/// the final block asserts the cursor advances. Routing `t == t_new` through `seek`
/// is killed by `at_the_newest_stamp_the_twist_is_the_left_limit`.
#[test]
fn sample_with_twist_from_agrees_with_sample_with_twist_from_every_cursor() {
    let hr = HeapRing::new(16);
    let ring = hr.ring();
    for i in 0..20u64 {
        ring.push(i as i64 * 100, &pose(i + 1)).unwrap();
    }
    // Off-knot stamps past both window ends (window 500..=1900), plus the exact knots including `t_new`.
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
    // Non-vacuity: the fixture must interpolate somewhere.
    assert!(ring.sample_with_twist(1234, ExtrapPolicy::Error).is_ok());

    // The cursor must *move*; the answers above are identical either way.
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

// ---- claim / publisher --------------------------------------------------

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
    // Re-claim after release bumps the epoch.
    let (e2, _) = claim(&rec, 333).unwrap();
    assert_eq!(e2, 2);
}

/// A stale `release` must not free a claim that has passed to somebody else.
///
/// P1 claims, is `SIGSTOP`ped and reaped, P2 claims, P1 drops its `Publisher`: an
/// unconditional `owner.store(0)` would free P2's live claim (A4 arriving through
/// `Drop`). A re-claim from the same slot must also get a distinguishable owner
/// word, which is why the epoch is folded into it.
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

    // The stale Publisher drops.
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

    // P1 resumes and drops its stale Publisher.
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

    // And P2's own release still works.
    release(&rec, p2_owner);
    assert_eq!(rec.owner.load(Ordering::Acquire), 0);
}

/// A4: a writer whose claim was reaped must refuse to publish. `reap` bumps the epoch *before* clearing the owner.
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
    // `!Sync` is enforced by `PhantomData<Cell<()>>`; see the `publisher_is_not_sync` doc test.
}

// ---- end-to-end arena (exercises the unsafe arena_view surface) ---------

fn single_dyn_edge_arena() -> HeapArena {
    // 4 frame slots (root + 3), 1 dynamic edge, ring capacity 4.
    let layout = ArenaLayout::new(4, 1, alloc::vec![4]).unwrap();
    HeapArena::new(&layout, 4242, 0, [0u8; 16])
}

/// A poisoned [`Guard`] must fail **every** evaluation entry point without reading
/// the view it was handed (`docs/decisions/0005` §7), asserted through the error an
/// empty arena can only produce from a non-reading construction.
#[test]
fn a_poisoned_guard_refuses_every_evaluation() {
    // Two edges: index `0` is the no-edge sentinel (`compile_rejects_the_no_edge_sentinel`).
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
    // A detached guard must silence `span` too. Mutant: drop `check_generation` from `Plan::span`.
    assert_eq!(plan.span(&g), Err(LookupError::ChildDetached));
    // Likewise `slowest_nominal_rate_mhz` (`docs/decisions/0018`). Mutant: drop its `check_generation`.
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

    // Control: an unpoisoned guard over the same arena answers something other than the poison.
    let live = Guard::new(ArenaView::new(&arena));
    assert_ne!(plan.at(&live, t), Err(LookupError::ChildDetached));
}

/// **The detached sentinel must not collide with a real generation.**
///
/// `Guard` encodes "detached" as generation `u64::MAX`. A plan compiled against a
/// generation the guard does not share, evaluated against an arena at generation
/// `0`, must give `TopologyChanged`, not `ChildDetached`. Mutant: `DETACHED = 0`.
#[test]
fn a_generation_mismatch_is_never_mistaken_for_a_detached_guard() {
    // Exactly one safe value, so it is pinned; the collision test can only show small ones.
    assert_eq!(
        crate::plan::DETACHED_FOR_TEST,
        u64::MAX,
        "any sentinel below u64::MAX is a generation a real tree can reach"
    );

    let layout = ArenaLayout::new(4, 2, alloc::vec![4, 4]).unwrap();

    // Arena A is mutated so its generation is non-zero.
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

    // Arena B is untouched (generation 0).
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
    // Record round-trips the stored name.
    assert!(view.frame_record(a).unwrap().name_matches("base_link"));
}

/// Regression: `ids` lives in a zero-initialized arena, so the unpublished sentinel
/// must be `0` (with `u32::MAX` the wait loop exited at once with `id == 0`).
#[test]
fn unpublished_sentinel_is_reachable_in_a_zeroed_arena() {
    assert_eq!(crate::frame::ID_UNPUBLISHED, 0, "sentinel must be zero");
    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    // A never-interned name reports "not found".
    assert_eq!(view.find_frame("never_interned").unwrap(), None);
    let a = view.intern("base_link").unwrap();
    assert_eq!(view.find_frame("base_link").unwrap(), Some(a));
}

// ---- A8: interning must not spin forever on a dead claimant --------------
// `docs/PHASE2.md` §1 A8 and §11.3 `intern.after_hash_cas_before_id_store`;
// interleavings are in `loom_tests`, these cover the arena-view wiring.

/// PID of the participant these tests declare dead.
const DEAD_PID: u32 = 90_001;
/// PID of the participant that does the rescuing.
const LIVE_PID: u32 = 90_002;

/// Wedge `name`'s hash slot as a process killed between the hash CAS and the id
/// store would: hash claimed, `claiming` names `owner_slot`, id unpublished. Returns
/// the slot index.
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

/// Run `f` on its own thread and fail, not hang, if it has not finished within
/// `secs`. An A8 regression is an infinite spin; the worker is detached. The
/// deadline scales with the interpreter (Miri is ~10x slower) and is a hang
/// detector, not a performance assertion.
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
        // A `SIGKILL`ed process never clears its slot, so the injected predicate, not `state`, detects the crash.
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
                // Idempotent afterwards: the rescued entry behaves like any other.
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

/// A8: a claimant that died before recording itself (`CLAIM_UNRECORDED`) is recoverable by any registered interner.
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

    // No predicate is consulted: there is no claimant to resolve.
    let id = assert_completes_within(10, "intern of an unrecorded claimed slot", move || {
        ArenaView::new(&arena)
            .as_participant(live_slot)
            .intern("victim")
    })
    .unwrap();
    assert_eq!(id.get(), 1);
}

/// A8, fail-safe (`docs/PHASE2.md` §6.2): a claimant not proven dead is **never**
/// stolen from. The interner is unblocked afterwards to prove it stayed on the
/// publish path.
#[test]
fn a_claimant_that_cannot_be_proven_dead_is_never_stolen_from() {
    use crate::sync::Ordering;

    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    let (owner_slot, _) = view.participants().register(DEAD_PID, 7, 0).unwrap();
    let (live_slot, _) = view.participants().register(LIVE_PID, 8, 0).unwrap();
    let wedged = wedge_intern_slot(&view, "victim", owner_slot);
    // Pretend the claimant allocated id 1 and wrote its record.
    let real = ArenaView::new(&arena)
        .as_participant(owner_slot)
        .intern("other")
        .unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            // No predicate: the claimant's slot reads LIVE, so this must block.
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

    // ...and it resolves to what the claimant published ("victim" and "other" share a record only because this test hand-published).
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
    // The full table still resolves what it holds, and did not over-count.
    assert_eq!(view.find_frame("a").unwrap(), Some(a));
    assert_eq!(view.intern("a").unwrap(), a);
    assert_eq!(
        view.header()
            .frame_count
            .load(crate::sync::Ordering::Relaxed),
        3
    );
}

/// Regression: `EdgeId` is a public `u32` newtype and `FrameId::new` accepts any
/// non-zero `u32`, so out-of-range ids reach these accessors from safe code. They
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
    // A fresh reader ring over the same arena sees the samples.
    let reader = view.ring(edge).unwrap();
    let got = reader
        .sample::<LerpSlerp>(2000, ExtrapPolicy::Error)
        .unwrap();
    assert_eq!(got.to_bits(), pose(3).to_bits());
    // The claim heartbeat advanced once per push.
    assert_eq!(
        view.claim(edge)
            .unwrap()
            .heartbeat
            .load(crate::sync::Ordering::Relaxed),
        3
    );
    drop(pubr);
    // After the publisher drops, the edge can be re-claimed.
    assert!(claim(view.claim(edge).unwrap(), 1).is_ok());
}

// ---- plan compilation ---------------------------------------------------

/// `set_parent` accepts `edge == 0` for "only the parent link matters", but edge
/// slot `0` is a real record. Compiling across such a link must name the problem,
/// not sample an unrelated ring.
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

/// A root plus a chain `f0 -> f1 -> ... -> f{links}` where the edge into `f{i}` is
/// `EdgeId(i)`. Returns the arena and frame ids in chain order (`chain[k]` is `k`
/// edges below the root). Edge id == child frame id (non-zero, unique).
/// `sentinel_at` places an edge-`0` link at a chosen distance.
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

/// A **Y**: one root, a target branch of `p` links and a source branch of `q`, so
/// `lookup(target, source)` walks `p + q` edges through the LCA. Edge ids as in
/// [`chain_arena`]. A straight chain cannot see a **per-side** bound: one side is
/// zero.
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

/// An `EdgeMeta` for a synthetic chain: every edge is `kind`, domain `0`, with a
/// **non-identity** static pose differing per edge (rotation and translation), since
/// an all-identity harness makes composition order invisible
/// (`docs/decisions/0034` rationale D).
fn distinct_pose(edge: u32) -> Iso3 {
    // `pose` is the suite's generator: three non-zero rotation components, so no two edges commute.
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
/// `MAX_DEPTH` dynamic edges compile whole, one more is refused, and the refusal
/// reports the **exact folded length** (`0034`).
///
/// Mutant: `if n > MAX_DEPTH` -> `if n > MAX_PATH_EDGES` in `fold` fails this test,
/// the precedence table, the raw-bound test and the corpus test (`Plan::new` panics
/// on the out-of-range `len`).
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

/// [`MAX_PATH_EDGES`](crate::MAX_PATH_EDGES) is the length of the **walk**
/// (`0034`): the chain length a dynamic path is refused at compiles to a *single
/// step* when the links are static.
///
/// The walk's refusal reports `MAX_PATH_EDGES + 1`, not a length: it stops because
/// it has no buffer left, and a cyclic parent chain would not terminate.
///
/// Mutant: `if nt + ns == MAX_PATH_EDGES` -> `if $n >= MAX_PATH_EDGES` in
/// `push_edge!` (the per-side spelling). A straight chain cannot kill it, which is
/// why the `y_arena` row exists: the 40-up/40-down Y would compile to
/// `Ok(Plan { len: 1 })`.
#[test]
fn the_raw_bound_is_the_exact_boundary_the_walk_refuses_at() {
    let (arena, chain) = chain_arena(crate::MAX_PATH_EDGES + 1, None);
    let view = ArenaView::new(&arena);
    let meta = chain_meta(crate::edge::EdgeKind::Static);
    let root = chain[0];

    // The row `0034` exists for: 64 raw edges, far past `MAX_DEPTH`, folding to
    // one constant.
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

    // **The bound is on `nt + ns`, and only a Y can see that**: with per-side guards a
    // 40-up/40-down path walks all 80 and *succeeds* (`Ok(Plan { len: 1 })`, static
    // links fold to one step).
    let (y, y_target, y_source) = y_arena(40, 40);
    let y_view = ArenaView::new(&y);
    assert_eq!(
        crate::plan::compile(&y_view.topology(), &meta, y_target, y_source).unwrap_err(),
        LookupError::TreeTooDeep {
            depth: (crate::MAX_PATH_EDGES + 1) as u16
        },
        "MAX_PATH_EDGES must mean edges walked, not edges walked on one side"
    );
    // The control: the same Y one edge under the bound compiles, so the row
    // above is about the bound and not about Y shapes.
    let (ok_y, ok_target, ok_source) = y_arena(32, 32);
    let ok_view = ArenaView::new(&ok_y);
    assert_eq!(
        crate::plan::compile(&ok_view.topology(), &meta, ok_target, ok_source)
            .unwrap()
            .len(),
        1,
        "64 edges walked is exactly the bound and must be accepted"
    );

    // And the two bounds are genuinely separate numbers: a chain longer than
    // `MAX_DEPTH` but shorter than `MAX_PATH_EDGES` compiles when it folds and
    // is refused when it does not.
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

/// **Error precedence**, a table over (defect kind) x (position before or after the
/// compiled bound fills) x (foldable or not); nothing else pinned it, which is why
/// `0034` could move it invisibly.
///
/// The discriminating row is `("unknown edge", 40, all-dynamic)` and its
/// `MixedTimeDomains` twin: the compiled array is full by step 32 and the defect
/// sits 8 steps past it. `fold` skips the write, keeps counting, and still resolves
/// that edge, so the caller is told what is *wrong* rather than how long the path is.
///
/// Mutant (variant A: an early `if n >= MAX_DEPTH { return Err(TreeTooDeep { depth:
/// n as u16 }) }` in `fold`): three of nine rows move (the clean all-dynamic row and
/// the two defect rows past the bound, each reporting depth 32); the six that stay
/// green are the control (all-static rows and `missing edge past the bound`, raised
/// in the walk).
///
/// A defect past the **raw** bound is unreachable: the walk stops there and depth
/// wins, because `fold` never sees an edge the walk did not collect.
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

    /// Path length in edges and the two positions in walk order (step `k` is `k` edges
    /// above the leaf, and on an all-dynamic path also compiled step `k`), so `AFTER`
    /// sits 8 steps past a full array.
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
        // The control: all-dynamic overruns the compiled bound and reports its true folded length.
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
        // All-static: the compiled array never fills, so these read the same under either variant.
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
        // Past the *raw* bound the walk stops, so the defect is unreachable and depth wins.
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

    // Rows are collected, not asserted one by one: the value is which rows move together.
    let mut wrong: Vec<alloc::string::String> = Vec::new();
    for (label, defect, links, step, kind, want) in cases {
        // Walk step `step` is the link into `chain[links - step]`.
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

/// The corpus shape and ceiling. ANYmal C (91-robot URDF survey): worst frame pair
/// is **20 joints** apart and folds to **13** steps (`fixed fixed revolute` x 6),
/// exercising the *adjacent*-only collapse rule; both numbers are pinned.
///
/// Ceiling rows: the worst *diameter* surveyed is 30 joints (Unitree H2 Plus), which
/// must compile with a `/tf` prefix on top, and a path past `MAX_PATH_EDGES` must
/// still refuse.
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
    // `chain[k]`'s frame id is the edge into it; links carry ids 2..=21 and `PATTERN[k - 1]` is link `k`'s kind.
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

    // The corpus ceiling: 30 dynamic joints plus `map -> odom -> base_footprint`
    // is 33 edges, and every one of them is a step.
    let (arena, chain) = chain_arena(33, None);
    let view = ArenaView::new(&arena);
    let dynamic = chain_meta(crate::edge::EdgeKind::Dynamic);
    assert_eq!(
        crate::plan::compile(&view.topology(), &dynamic, chain[33], chain[0]).unwrap_err(),
        LookupError::TreeTooDeep { depth: 33 },
        "33 dynamic steps is past MAX_DEPTH and says so exactly"
    );
    // …and the same 33 edges with the arm's fixed offsets declared static fit
    // easily, which is the deployment this bound is sized for.
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
    let read = |f| topo.read_frame(f).unwrap();
    assert_eq!(read(a).1, 0);
    assert_eq!(read(b).1, 1);
    assert_eq!(read(c).1, 2);
    assert_eq!(read(b).0, a.get()); // parent
                                    // edge_of_child round-trips and stays consistent with parent across flips.
    assert_eq!(read(b).2, 10);
    assert_eq!(read(c).2, 20);
    assert_eq!(read(a).2, 0); // root has no edge

    // Each successful mutation is one publishing store: the generation advances by exactly 1.
    assert_eq!(topo.generation(), 2, "two mutations, two generations");
    let before = topo.generation();

    // Attaching a under c would close a cycle a->b->c->a.
    let err = topo.set_parent(a, c.get(), 30).unwrap_err();
    assert_eq!(
        err,
        crate::error::TopologyError::WouldCreateCycle { child: a }
    );
    // The failed mutation left the tree intact.
    assert_eq!(read(a).0, 0);
    // And edge_of_child for the earlier attach survived the aborted mutation.
    assert_eq!(read(c).2, 20);
    // The topology is unchanged, so the generation must not advance (it would invalidate every plan).
    assert_eq!(
        topo.generation(),
        before,
        "an aborted set_parent must not bump the generation"
    );
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

// ---- A5: a dead writer must not invert a slot's parity ------------------

/// A writer killed between the two `seq` stores leaves the slot **odd**, and the
/// next writer must recover (`docs/PHASE2.md` §1 A5). An incrementing writer would
/// store an *even* value mid-payload and readers would accept a torn pose; forcing
/// the parity (`s | 1`) is idempotent on a stale odd value.
#[test]
fn stale_odd_seq_from_a_dead_writer_is_healed_by_the_next_push() {
    let ring = HeapRing::new(4);
    let pose = exp_se3([0.1, 0.2, 0.3, 1.0, 2.0, 3.0]);

    // A writer killed after flipping slot 0 to odd, before publishing.
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

    // The healed value must exceed the stale odd one: seqlock monotonicity is preserved.
    assert!(seq > 1, "seq went backwards: {seq}");
}

/// `register_at` takes the slot it is named, or nothing. Exclusivity under
/// concurrency is `loom_tests::two_joiners_handed_the_same_slot_cannot_both_take_it`;
/// this covers the sequential error paths.
#[test]
fn register_at_takes_the_named_slot_or_fails() {
    use crate::participant::{ParticipantError, ParticipantTable};

    let slots: Vec<ParticipantRecord> = (0..4).map(|_| ParticipantRecord::default()).collect();
    let table = ParticipantTable::new(&slots);

    let inc = table.register_at(2, 1234, 99, 0).expect("slot 2 is free");
    assert_eq!(inc, 1);
    assert_eq!(table.identity(2).map(|id| id.0), Some(1234));

    // A second joiner handed the same slot is refused, not quietly given another (that breaks the slot/lock-byte correspondence).
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
/// This is the sequential half; the race between the guard's two words is
/// `loom_tests::a_late_release_racing_a_slot_handover_frees_nobody`.
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

/// `reclaim` frees a slot left `LIVE` by a process that never ran `Drop`. A reaper
/// has no incarnation to pass, only the observed word, and `live_word` packs the
/// incarnation into it, so the word is the guard (`docs/decisions/0028`).
#[test]
fn reclaim_frees_a_slot_from_the_observed_live_word() {
    use crate::participant::{live_word, ParticipantTable, FREE};
    use core::sync::atomic::Ordering;

    let slots: Vec<ParticipantRecord> = (0..2).map(|_| ParticipantRecord::default()).collect();
    let table = ParticipantTable::new(&slots);

    let (slot, inc) = table.register(1234, 99, 0).expect("register");
    // `SIGKILL`: no `release`, the record stays LIVE, the kernel frees the lock byte.
    let observed = table.get(slot).expect("slot").state.load(Ordering::Acquire);
    assert_eq!(observed, live_word(inc));
    assert!(table.reclaim(slot, observed), "the word was unchanged");

    assert_eq!(
        table.get(slot).expect("slot").state.load(Ordering::Acquire),
        FREE
    );
    assert_eq!(table.identity(slot), None);
    // And the slot is usable again (#184).
    let (again, inc2) = table.register(5678, 100, 0).expect("re-register");
    assert_eq!(again, slot);
    assert_eq!(inc2, inc + 1, "the incarnation counter is never recycled");
}

/// `reclaim` also frees a slot left `RESERVED` (a process killed between
/// `fill_slot`'s CAS and its publishing store, §11.3
/// `attach.after_slot_assigned_before_publish`), which carries no incarnation so
/// `release` cannot name it. Safe because the byte is the occupancy authority
/// (`0028` steps 0b, 0c); the memory-model half is
/// `loom_tests::reclaim_races_register`.
#[test]
fn reclaim_frees_a_slot_from_an_observed_reserved() {
    use crate::participant::{ParticipantTable, FREE, RESERVED};
    use core::sync::atomic::Ordering;

    let slots: Vec<ParticipantRecord> = (0..2).map(|_| ParticipantRecord::default()).collect();
    let table = ParticipantTable::new(&slots);

    // Staged, not raced: a process that died inside `fill_slot` has made every store it ever will.
    table
        .get(1)
        .expect("slot")
        .state
        .store(RESERVED, Ordering::Release);
    // `register` only CASes from FREE, so it cannot collect this slot.
    assert_eq!(table.register(1, 1, 0).expect("register").0, 0);

    assert!(table.reclaim(1, RESERVED));
    assert_eq!(
        table.get(1).expect("slot").state.load(Ordering::Acquire),
        FREE
    );
    assert_eq!(table.register(5678, 100, 0).expect("register").0, 1);
}

/// `reclaim` fails when the observed word has changed under it: the verdict was
/// formed against one occupancy, and a re-occupied slot carries a different
/// `live_word` (`docs/decisions/0028`'s `reclaim.probe_then_reoccupied`; §11.3 has
/// no such row). The return is *did this CAS fire*, not *is the slot free now*.
///
/// This pins the CAS guard: `loom_tests::reclaim_races_register` never reaches that
/// CAS on a contended slot, so a `reclaim` ignoring `observed` would pass the model
/// and fail here.
#[test]
fn reclaim_fails_when_the_observed_word_has_changed() {
    use crate::participant::{live_word, state_of, ParticipantTable, LIVE, RESERVED};
    use core::sync::atomic::Ordering;

    let slots: Vec<ParticipantRecord> = (0..2).map(|_| ParticipantRecord::default()).collect();
    let table = ParticipantTable::new(&slots);

    let (slot, inc1) = table.register(1234, 99, 0).expect("register");
    let stale = live_word(inc1);
    // Between the observation and the CAS the slot is reclaimed by somebody
    // else and handed to a new process.
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
/// would-be interner and a reader: never waited on forever, never stolen from.
/// `CLAIM_ANONYMOUS` is evidence someone is working yet unjudgeable (no participant
/// slot). Takeover would allocate a second id and inflate `frame_count`; abandoning
/// would report "no such frame". So the bound ends in `InternContended`, a
/// caller-visible "try again" (A8).
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

// ---- sample_with_twist: the extrapolation arms and the left limit ---------
//
// The plan fold always passes `ExtrapPolicy::Error`, so these arms are reachable
// only here.

/// A ring with `n` samples at `i * 100` ns, moving along one screw.
fn twist_ring(hr: &HeapRing, n: u64) {
    for i in 0..n {
        hr.ring().push(i as i64 * 100, &pose(i)).unwrap();
    }
}

/// **`Hold` reports a zero twist**: the pose is pinned past `t_new`, so its
/// derivative is zero by definition.
///
/// Mutant: return the last segment's twist instead of `Twist::ZERO`.
#[test]
fn hold_extrapolation_reports_a_zero_twist() {
    let hr = HeapRing::new(8);
    twist_ring(&hr, 5);
    let (pose_h, tw) = hr
        .ring()
        .sample_with_twist(10_000, ExtrapPolicy::Hold)
        .expect("hold must succeed past the newest stamp");
    assert_eq!(tw, Twist::ZERO, "a held pose is stationary");
    // ...and the pose is the newest sample, unchanged.
    assert_eq!(pose_h.to_bits(), pose(4).to_bits());
}

/// **`ConstantTwist` extends along the last segment, and reports that twist**, with
/// the pose bit-for-bit equal to `sample`'s under the same policy.
///
/// Mutant: drop the `NANOS_PER_SEC / dt` scaling (fails by 1e7).
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

    // 100 ns apart on one screw: the twist is the segment's xi per second, equal to the in-window twist.
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
    // A *non-identity* single sample: `pose(0)` is `Iso3::IDENTITY`, which would hide an invented identity pose.
    hr.ring().push(0, &pose(3)).unwrap();
    assert!(matches!(
        hr.ring()
            .sample_with_twist(10_000, ExtrapPolicy::ConstantTwist),
        Err(LookupError::NoSegment { .. })
    ));
    // The plain sample answers with the published sample: held, not extrapolated or invented.
    //
    // Mutant: return `Iso3::IDENTITY` from `constant_twist`'s `newest == lo_logical` arm.
    assert_eq!(
        hr.ring()
            .sample::<ScLerp>(10_000, ExtrapPolicy::ConstantTwist)
            .expect("the pose is available; only the derivative is not")
            .to_bits(),
        pose(3).to_bits(),
        "a single-sample ConstantTwist must hold the published sample"
    );
}

/// **At exactly the newest stamp the twist is the left limit**: the segment that
/// *ends* at that knot.
///
/// Mutant: use `bracket` unconditionally; its precondition breaks and `i + 1` reads
/// outside the published window.
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

/// **Below the oldest retained stamp is an error, not a clamp**, with or without a
/// derivative and under every policy (policies govern the *newer* end only).
///
/// Mutant: delete the `t < t_old` guard from `sample_with_twist`; `bracket` then
/// returns a bracket not containing `t`.
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

/// An empty ring has no pose and no twist: `NoData`, not `NoSegment`.
#[test]
fn an_empty_ring_is_no_data_not_no_segment() {
    let hr = HeapRing::new(8);
    assert!(matches!(
        hr.ring().sample_with_twist(0, ExtrapPolicy::Error),
        Err(LookupError::NoData { .. })
    ));
}

/// The two fields `Plan::new` derives (`dyn_count`, `first_dynamic_edge`) must
/// always equal a fresh scan of the plan's steps; nothing else catches them going
/// stale. Four shapes whose answers differ.
///
/// A plan crossing several dynamic edges must credit **no** edge (`EdgeId(0)`) so
/// `doctor`'s column stays consistent.
///
/// Mutants killed: `first_dynamic_edge` testing `>= 1` instead of `== 1`; `dyn_count`
/// never incremented. Assigning `first_dyn` on every dynamic step is an
/// **equivalent** mutant (only read when `dyn_count == 1`).
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

    // The `edge_meta` closure is the caller's, so steps can be forced dynamic or static; `&ArenaView` is `Copy`, so `meta` can be called repeatedly.
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

    // Pin the answers themselves, or a `Plan::new` that derived nothing would agree with a scan of a plan with no dynamic steps.
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

// ---- typed domains, exact stamps, and the declared publish rate ----------

/// A five-frame chain `f0 -> f1 -> f2 -> f3 -> f4` over four dynamic edges, where
/// edge `i` declares `rates[i]` milli-hertz (`0` = undeclared).
///
/// Edge index `0` is the "no edge" sentinel, so the chain uses edges 1-4. The rings
/// come back **empty**: a declared rate is a property of the topology, and reading
/// it must not depend on a stream. Call [`seed_rate_chain`] for a fold that answers.
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
/// **out of range**. `declare_edge` does not touch the topology word, so the
/// generation matches the full arena and a plan compiled against it reaches the
/// otherwise unreachable `UnknownEdge` arm.
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
/// `1000`. Returns the pose any full `f0 -> f4` lookup in that window must produce:
/// the same pose twice, so interpolation is exact
/// (`pose(1)·pose(2)·pose(3)·pose(4)`).
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

/// **The four built-in domain tags are `0`-`3`, in that order.** A tag is written
/// into `EdgeRecord::domain` and read by every recording, so re-numbering silently
/// re-interprets arenas on disk (`docs/API.md` §5.2). Distinctness is pinned
/// separately in [`the_built_in_domain_tags_are_pairwise_distinct`].
///
/// Mutant: `SimDomain::TAG = 0`.
#[test]
fn the_built_in_domain_tags_are_fixed() {
    use crate::plan::{Domain, SensorDomain, SimDomain, SteadyDomain, SystemDomain};

    assert_eq!(SystemDomain::TAG, 0, "the default domain must stay tag 0");
    assert_eq!(SensorDomain::TAG, 1);
    assert_eq!(SimDomain::TAG, 2);
    assert_eq!(SteadyDomain::TAG, 3);
}

/// **No two built-in domains share a tag.** `Domain::TAG` is a per-impl constant
/// with nothing structural preventing a collision, which collapses
/// `TimeDomainMismatch` between them (`docs/API.md` §2.5). Not covered: a
/// user-declared fifth domain colliding with a built-in (§2.5 reserves `0`-`3` by
/// documentation).
///
/// Mutant: `SimDomain::TAG = 0`.
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

/// **A sim-time stamp must be refused by a system-domain plan** (`docs/API.md`
/// §2.5, §5.2): otherwise `/clock` time is served, plausibly, out of a wall-clock
/// stream.
///
/// The fixture is seeded: against empty rings `NoData` shadows the domain check and
/// a tag collision is never demonstrated. The final control (the plan's own domain
/// answered) keeps this from passing against a plan that refuses everything.
///
/// Mutant: `SimDomain::TAG = 0` makes the first assertion return the wall-clock pose.
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

// ---- exact stamp converters ---------------------------------------------

/// **`from_parts` is exact across the whole `i64` range, including below the
/// epoch** (`docs/API.md` §5.1). `1_700_000_000_123_456_789` needs 61 bits and `f64`
/// has 53, so the **sum** cannot survive an `f64` (the *product* can, see B').
/// A `timespec` of `(-1, 250_000_000)` is -750 ms, not -1250 ms.
///
/// Mutants: (A) plain `i64` arithmetic wraps silently and the `i64::MAX`/`i64::MIN`
/// assertions fail. (B) the whole expression in `f64` fails the nanosecond-precision
/// assertion (21 ns error). (B') `(sec as f64 * 1e9) as i128 + nanos as i128` passes
/// that one (`1.7e18` is exact in `f64`) and dies only at the `i64::MIN` edge, so the
/// range assertions are not redundant. (C) `sec.checked_mul(1e9)?.checked_add(nanos)`
/// refuses the last second at the negative end.
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

    // The exact edges of the representable range; one nanosecond further has no answer.
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

    // The multiplication overflows before the addition can, so both are checked.
    assert!(S::from_parts(i64::MAX, 0).is_none());
    assert!(S::from_parts(i64::MIN, 0).is_none());
}

/// **A nanosecond field that is not a sub-second remainder is refused, not carried
/// into the seconds**: normalising a malformed `Time` yields a plausible stamp
/// nothing downstream will question. `999_999_999` is the last accepted value and
/// `1_000_000_000` the first refused.
///
/// Mutant: drop the `nanos >= NANOS_PER_SEC` guard.
#[test]
fn from_parts_refuses_a_nanosecond_field_that_is_not_a_remainder() {
    type S = Stamp<SystemDomain>;

    assert_eq!(S::from_parts(0, 999_999_999).unwrap().nanos(), 999_999_999);
    assert!(S::from_parts(0, 1_000_000_000).is_none());
    assert!(S::from_parts(0, u32::MAX).is_none());
    // ... and the refusal does not depend on the seconds being zero.
    assert!(S::from_parts(1_700_000_000, 2_000_000_000).is_none());
}

/// **`from_timespec` takes the two fields, and refuses a relative interval.** No
/// `libc::timespec` (dependency budget, `docs/PROJECT.md` §5); `time_t` and `long`
/// are `i64` on 64-bit. A negative `tv_nsec` occurs only in a *relative*
/// `timespec`, so it means an interval is being converted as an instant;
/// `from_parts` refuses everything else.
///
/// Mutant: drop the `tv_nsec < 0` half of the guard. Only a `tv_nsec` whose **low 32
/// bits land inside `[0, 1e9)`** kills it (`(0, -4_294_967_296)` answers the epoch);
/// `(0, -1)` is refused by `from_parts` either way.
#[test]
fn from_timespec_refuses_a_relative_interval() {
    type S = Stamp<SensorDomain>;

    assert_eq!(
        S::from_timespec(1_700_000_000, 123_456_789)
            .unwrap()
            .nanos(),
        1_700_000_000_123_456_789
    );
    // Refused by the `as u32` cast, not the sign guard: these document the API, not defend it.
    assert!(S::from_timespec(0, -1).is_none());
    assert!(S::from_timespec(-1, -1).is_none());
    // Only the sign guard refuses these: their low 32 bits are `0` and `5`.
    assert!(S::from_timespec(0, -4_294_967_296).is_none());
    assert!(S::from_timespec(7, -4_294_967_291).is_none());
    // Delegation: everything `from_parts` refuses, this refuses too.
    assert!(S::from_timespec(0, 1_000_000_000).is_none());
    assert!(S::from_timespec(i64::MAX, 0).is_none());
    // A negative *second* with a valid remainder is a legitimate pre-epoch
    // stamp and must still be accepted.
    assert_eq!(
        S::from_timespec(-1, 250_000_000).unwrap().nanos(),
        -750_000_000
    );
}

// ---- the declared publish rate a waiter sleeps on ------------------------

/// **The slowest declared rate on the path is the answer, not the fastest.**
/// `docs/decisions/0018` gives the caller `Plan::span` and this period; a plan is
/// answerable only once *every* dynamic edge reached the stamp, so sleeping at the
/// fastest edge's period polls a hundred times per answer. Rates 1 kHz / 200 Hz /
/// 50 Hz / 10 Hz, declared descending so first/last/fastest each give a different
/// wrong answer.
///
/// Mutants: `current.max(mhz)` (fastest) and returning the first declared rate both
/// fail the same assertion.
/// Mutant B: return the first declared rate rather than folding. Applied: the
/// same assertion fails with `Ok(Some(1000000))`.
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
/// `None`.** Read as a rate, `0` is the minimum of every set and yields an infinite
/// period, so one undeclared edge would disable the wait (`docs/PHASE5.md` §6
/// `TFT007`). `None` is a third answer, not `Some(0)` (`0018` *Consequences*).
///
/// Mutants: drop the `if mhz == 0 { continue; }` skip (`Ok(Some(0))`); seed the fold
/// with `Some(u32::MAX)` (all-undeclared gives a 4.29 MHz rate).
#[test]
fn an_undeclared_rate_is_skipped_and_an_undeclared_plan_is_none() {
    // The skip must not stop the walk: the 50 Hz edge behind the undeclared one must contribute.
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

/// **The rate is readable before anything has been published.** A declaration is a
/// property of the topology, a window of the stream. The caller asks before the data
/// exists (the reason `0018`'s loop exists), so `NoData` would make the method
/// unusable. The `span` assertion beside it is the control (empty rings *do* stop
/// `span`).
///
/// Mutant: implement `nominal_rate_mhz` through `Guard::window` so it inherits the
/// `NoData` refusal.
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
/// skipped.** The one arm of `slowest_nominal_rate_mhz` a well-formed arena cannot
/// reach: `compile` refuses unknown edges and `check_generation` catches other
/// arenas. [`short_edge_table_arena`] (same generation, one fewer edge slot) is the
/// only shape that reaches it. Contrived on purpose: a shim's timeout loop consults
/// this arm every iteration (`docs/decisions/0018`).
///
/// Mutant: `let Ok(mhz) = g.nominal_rate_mhz(*edge) else { continue };`.
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

    // Control: the generations agree, so the assertion is about `UnknownEdge`, not an earlier `TopologyChanged`.
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

/// `ErrBound::new` maps its first argument to `rot_rad` and its second to `trans`;
/// nothing else can tell, since the other in-workspace call sites pass equal values
/// and `tf_tree_py`'s `at_adaptive` is outside `just test`. Two unequal values make
/// the swap observable.
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

// ---- the packed search cursor past 2^32 ---------------------------------

/// One more than the largest value `Guard`'s packed 32-bit cursor represents.
const CURSOR_BLOCK: u64 = 1 << 32;

/// [`Guard`](crate::plan::Guard) stores the cursor as the low 32 bits of a logical
/// index; past 2^32 pushes a plain clamp pins every hint to the oldest sample, and
/// `rebase_hint` lifts it back. The window is narrower than 2^32, so a truncated
/// index has one preimage; the case needing the correction is a window straddling a
/// multiple of 2^32 (preimage in the block *below* `newest`'s).
///
/// **Mutant:** delete the `lifted > newest` arm; the straddling case answers
/// `3 * 2^32 - 4`, 2^32 past the newest sample.
#[test]
fn a_truncated_cursor_is_lifted_back_onto_the_live_window() {
    use crate::sample::rebase_hint;

    // Below 2^32 nothing is truncated: an in-window hint is untouched and one below it stays below for the caller's clamp.
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

    // Window wholly inside one block above 2^32: restore `newest`'s block base.
    let (lo, newest) = (CURSOR_BLOCK + 10, CURSOR_BLOCK + 20);
    let truth = CURSOR_BLOCK + 15;
    assert_eq!(
        rebase_hint(truth & (CURSOR_BLOCK - 1), lo, newest),
        truth,
        "a hint truncated inside one block is lifted back into it"
    );

    // Straddling a multiple of 2^32: restoring `newest`'s base overshoots by one block; the correction subtracts it.
    let (lo, newest) = (2 * CURSOR_BLOCK - 6, 2 * CURSOR_BLOCK + 3);
    let truth = 2 * CURSOR_BLOCK - 4;
    assert_eq!(
        rebase_hint(truth & (CURSOR_BLOCK - 1), lo, newest),
        truth,
        "a straddling window's lower block must not be lifted a block too far"
    );

    // Every index in that straddling window round-trips through truncation.
    for truth in lo..=newest {
        assert_eq!(
            rebase_hint(truth & (CURSOR_BLOCK - 1), lo, newest),
            truth,
            "index {truth} did not survive truncation and rebasing"
        );
    }
}

/// Behavioural half: a ring whose head has passed 2^32 must answer a hinted sample
/// exactly as the unhinted search does, from the truncated cursor a `Guard` would
/// store. Answers were right before `rebase_hint` too, so this pins that making the
/// hint usable made no answer wrong.
///
/// **Mutant:** return `lo_logical` instead of the lift; results still match (a hint
/// never changes a result), which is why the unit test above exists.
#[test]
fn a_ring_past_two_to_the_thirty_two_samples_from_a_truncated_cursor() {
    let hr = HeapRing::new(64);
    // Place the window across a multiple of 2^32. `push` asserts the heartbeat tracks the head (`0014`), so both move.
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

/// The tagged query surface must be the typed one with the domain arriving as data
/// (same condition, error and answer): [`Domain`](crate::plan::Domain) is an open
/// trait, so a foreign binding cannot dispatch to `at::<D>`
/// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`). Without it every C,
/// C++ and Python query hardcoded `Stamp::<SystemDomain>`, making arenas with other
/// tags (as `ros/tf_tree_ros/src/bridge_node.cpp` tells operators to configure under
/// `use_sim_time`) unreadable.
///
/// **Mutant:** `at_tagged` passes `self.domain` instead of its argument; the four
/// mismatch assertions then fail with `Ok(..)`.
#[test]
fn a_tagged_query_is_the_typed_query_with_the_domain_as_data() {
    use crate::plan::{Domain, SimDomain, SteadyDomain};

    let arena = rate_chain_arena([0, 0, 0, 0]);
    let view = ArenaView::new(&arena);
    let expected = seed_rate_chain(&view);
    let (root, leaf) = (view.intern("f0").unwrap(), view.intern("f4").unwrap());
    let plan = compile_chain(&view, root, leaf);
    let g = Guard::new(ArenaView::new(&arena));

    // The plan's own tag is answered with the typed form's pose (else the test passes by refusing everything).
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

    // A user-declared tag (the case no binding can name) is refused by tag, not by failing to compile.
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

/// **`by_ns == 0` must mean the fold bracketed, and the *order* of two walks is the
/// whole of that guarantee.**
///
/// `Extrapolated`'s `0` means every edge bracketed the query. Folding the pose
/// *then* walking `newest_common` lets a `push` land between them and lift `common`
/// to `>= nanos`, reading `0` for an invented pose; measuring first errs in the safe
/// direction (`SampleRing::newest_stamp` is non-decreasing).
///
/// The pose is the witness: one dynamic edge, `x == stamp / STEP`, query at a
/// half-step. A *held* answer has integral `x`, an *interpolated* one `k + 0.5`;
/// `by_ns == 0` with integral `x` is the defect.
///
/// **Mutant:** move `newest_common` back below the fold. The writer runs *until
/// stopped* and the reader owns the iteration count (a fixed-count writer finishes
/// first and the race cannot occur); probabilistic even so, the monotonicity
/// argument at the call site carries the guarantee.
///
/// **Ignored under Miri**: 200 000 rounds against a spinning writer do not finish
/// there; `just loom` is where interleavings are argued.
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

    // `x == k` at stamp `k * STEP`: a half-step blend is `k + 0.5`, a hold is `k`.
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
        // The `Publisher` is built **inside** the writer: D7 makes it `!Send`.
        s.spawn(|| {
            let view = ArenaView::new(&arena);
            let (epoch, owner) = claim(view.claim(EdgeId(1)).unwrap(), 7).unwrap();
            let pubr = Publisher::new(
                view.ring(EdgeId(1)).unwrap(),
                view.claim(EdgeId(1)).unwrap(),
                epoch,
                owner,
            );
            // Until told to stop, not a fixed count: a finished writer leaves a frontier nothing moves.
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
            // Read the frontier **immediately** before the call so the writer's next push crosses `t`; a lagging `k` makes every answer honestly interpolated.
            let k = published.load(O::Acquire);
            if k < 0 {
                continue; // the writer has not published its first sample yet
            }
            // Half a step past the newest sample seen.
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
            // Claimed bracketed: the fold must have had a sample at or past `t`, and a half-step blend is not integral.
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

/// `ExtrapPolicy`'s variants were reachable from no shipped surface;
/// `Plan::at_extrapolating` reaches them
/// (`docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md`).
///
/// **The third assertion earns the test**: a moving body's held pose and its
/// constant-twist extension differ at the same stamp, and only the second keeps
/// moving; a `fold_at_policy` serving `Hold` for both would pass the others.
///
/// **Mutant:** `fold_at_policy` passes `ExtrapPolicy::Hold` instead of `policy`
/// (stops at the first assertion); the `ConstantTwist` assertion is needed for the
/// narrower mutant passing `Hold` only where `ConstantTwist` was asked.
#[test]
fn extrapolation_is_selectable_and_reports_how_far_it_reached() {
    let arena = rate_chain_arena([0, 0, 0, 0]);
    let view = ArenaView::new(&arena);
    seed_rate_chain(&view);
    let (root, leaf) = (view.intern("f0").unwrap(), view.intern("f4").unwrap());
    let plan = compile_chain(&view, root, leaf);
    let g = Guard::new(ArenaView::new(&arena));

    // `latest_common` is the newest stamp every dynamic edge can answer; past it any answer is invented.
    let common = plan.newest_common_for_test(&g).unwrap().unwrap().0;
    let past = common + 5_000_000; // 5 ms beyond it

    // The default refuses; `Plan::at` is untouched.
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

    // Control: inside the window every policy agrees with `at` at zero distance.
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

    // The distance is a property of the data, not the policy.
    assert_eq!(held.by_ns, 5_000_000);
    assert_eq!(extended.by_ns, 5_000_000);

    // `Hold` really holds: its answer is the pose at `latest_common`.
    assert_eq!(
        held.pose, inside.pose,
        "Hold did not hold the newest sample"
    );

    // ...and `ConstantTwist` really extends. This is the assertion a
    // policy-ignoring fold cannot satisfy.
    assert_ne!(
        extended.pose, held.pose,
        "ConstantTwist returned the held pose, so the policy was ignored"
    );
}

// ---- errors that compose (decision 0040) --------------------------------

/// Every variant of every error enum renders as prose and names what it carries.
/// Coverage, not wording (`docs/API.md` R5: message text is no compatibility
/// promise): a message exists, is not the `Debug` spelling, and contains the carried
/// identifier. The `error.rs` match is exhaustive; this pins that each arm *says*
/// something.
///
/// **Mutant:** `LookupError::WrongElementType`'s arm `write!(f, "")`.
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

    // The carried identifier must reach the message (D11).
    assert!(format!("{}", LookupError::NoData { edge }).contains('3'));
    assert!(format!("{}", LookupError::FrameOutOfRange { frame }).contains('7'));
    assert!(format!("{}", LookupError::BufferTooSmall { need: 48, got: 16 }).contains("48"));

    for e in [
        PushError::NonMonotonicStamp {
            edge,
            last: 9,
            got: 4,
        },
        PushError::ClaimRevoked { edge },
        PushError::ChildDetached,
    ] {
        assert!(!format!("{e}").is_empty());
        assert_ne!(format!("{e}"), format!("{e:?}"));
    }
    // D11 in the prose layer: a stamp regression names its edge. Mutant: drop `edge {}: ` from `PushError`'s Display.
    assert!(format!(
        "{}",
        PushError::NonMonotonicStamp {
            edge,
            last: 9,
            got: 4
        }
    )
    .starts_with("edge 3:"));
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

/// The `?`-chain `Tree::await_frames`' docs once called impossible, compiled:
/// `OpenError`, `AwaitError` and `LookupError` through one `Box<dyn Error>`.
/// Acceptance test for `docs/decisions/0040`.
///
/// **Mutant:** delete `impl core::error::Error for LookupError`; the test no longer
/// compiles, the strongest failure form.
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

    // Two different error types through one `?`-chain.
    fn two_kinds(which: u8) -> Result<(), Box<dyn core::error::Error>> {
        match which {
            0 => Err(FrameError::CapacityExceeded)?,
            _ => Err(LookupError::ChildDetached)?,
        }
    }
    assert!(two_kinds(0).is_err());
    assert!(two_kinds(1).is_err());
}

/// `Extrapolated::by_ns` must not wrap when query and data are more than
/// `i64::MAX` ns apart: a plain `nanos - common` panics in a checked build on the
/// wait-free read path, and wrapped negative clamps to `0`, reporting "not
/// extrapolated" for the most extrapolated answer possible (as `sample::span_ns`).
///
/// **Mutant:** restore `(nanos - common).max(0)`.
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
/// Figures, not invariants (`MAX_DEPTH` moving moves `Plan`); this catches a field
/// added to `Iso3` or an alignment attribute returning, which doubles a per-thread
/// cache.
///
/// **Mutant:** add an 8-byte field to `Iso3` (fails with `left: 64, right: 56`).
/// Restoring `align(64)` does not compile either way (constructors or the `Pod`
/// derive refuse).
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
    // What the `Pod` derive rests on: seven f64 and nothing else.
    assert_eq!(size_of::<Iso3>(), 7 * size_of::<f64>());
}
