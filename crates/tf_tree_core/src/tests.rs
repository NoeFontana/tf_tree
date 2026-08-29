//! Non-`loom` unit + property tests for the concurrency core.
//!
//! Concurrency *interleavings* are checked by the loom suite (`src/loom_tests.rs`,
//! run under `--cfg loom`); this module covers single-threaded correctness, the
//! arena-view unsafe surface (for Miri), and the wrapped-ring property test (#15).
// `panic` is allowed for the same reason the loom suite allows it: a test that
// detects an *unbounded spin* (see `assert_completes_within`) has to report the
// timeout itself, and the message must name what timed out.
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

/// **`revalidated` is the lap check, and it fires exactly at the bound.**
///
/// `SampleRing::sample`'s `# Errors` promises `SlotRecycled` when the ring
/// lapped the reader mid-read. The interpolating tail enforced it; six arms that
/// *short-circuit* did not — `Hold` in `sample` and `sample_from`, the exact hit
/// on the newest stamp in both, `sample_with_twist_seeking`'s `Hold`, and
/// `constant_twist`'s single-sample case — so a reader descheduled long enough
/// for the ring to lap got a complete, valid pose belonging to a **different
/// stamp**. The seqlock catches a torn slot, not a recycled one. All six now go
/// through one helper, and this pins what that helper decides.
///
/// The bound is `retained`, not `capacity`: `head - i == capacity` already means
/// slot `i` is the one `push` is overwriting, so `retained == capacity - 1` is
/// the last index still safe to have read.
///
/// **This is deterministic, and two stress harnesses were deleted to get here.**
/// The first queried the exact newest stamp and demanded that stamp's pose back;
/// it fails, and not because of this fix — `bracket` binary-searches `stamp_at`
/// with `Relaxed` loads, one concurrent `push` overwrites the oldest retained
/// slot, and the search can then land on an index that is *still in the window*
/// (so the trailing check passes) whose stamps do not bracket the request. The
/// second used `newest_stamp()` as its baseline, which is the same unguarded
/// read: its head load and its stamp load race, so it can report a stamp from a
/// later lap and make an honest answer look stale. A concurrent test of this
/// needs a baseline that cannot over-report, and from outside the ring there is
/// none. Both hazards are written up at [`crate::sample`]'s module docs; neither
/// is smuggled into an assertion that would go red for the wrong reason.
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

    // And one more push moves the bound by exactly one, which is what makes this
    // a bound rather than a constant.
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

/// Regression: on a wrapped ring, logical index `head - capacity` shares a
/// physical slot with the sample `push` writes next, so retaining `capacity`
/// samples read the *newest* stamp as `t_old`. Every in-window query below that
/// stamp then failed immediately with an `Extrapolation` carrying a fabricated
/// `oldest` — no bracket search, no revalidation.
#[test]
fn a_wrapped_ring_does_not_retain_the_slot_push_overwrites() {
    let hr = HeapRing::new(4);
    let ring = hr.ring();
    // Five pushes into four slots: logical 4 (stamp 40) overwrote logical 0's
    // physical slot, so logical 1..=4 (stamps 10..40) look retained but only
    // 2..=4 actually are.
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
                // `capacity - 1`, not `capacity`: logical index `head - capacity`
                // shares a physical slot with the sample `push` writes next, so it
                // is not retained. See `SampleRing::retained`.
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

/// [`SampleRing::sample_from`]'s galloping search must return exactly what
/// [`SampleRing::sample`] returns, from **any** starting cursor.
///
/// The downward-gallop arm is unreachable from every in-tree caller —
/// `Plan::fold_at_cursors` seeds each cursor to 0 and only runs for monotone
/// stamps — so nothing exercised it, even though `sample_from` is public. Both
/// arms exist only to establish `bracket`'s precondition
/// (`stamp[lo] <= t < stamp[hi]`); break it and `bracket` returns an index
/// whose stamp is *above* `t`, `s` goes negative, and the caller gets a
/// confidently wrong pose with no error at all.
///
/// The ring is deliberately wrapped (20 pushes into 16 slots) so the logical
/// window does not start at 0 and the cursor clamp is exercised too.
///
/// Mutant: `hint.saturating_sub(step)` -> `hint.saturating_sub(step / 2)` in
/// the downward arm ⇒ fails.
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
/// [`SampleRing::sample_with_twist`] returns, from **any** starting cursor —
/// the derivative path's half of the property above.
///
/// The two share `bracket_from`, so this is really asking whether the twist
/// sampler still establishes that helper's precondition
/// (`stamp[lo] <= t < stamp[hi]`) on the arm that reaches it.
///
/// The twist is compared bit-for-bit and not within a tolerance: a cursor is a
/// hint and must not be able to move the last bit of an answer, because
/// `at_many_into(QuatTwist)` picks the cursor branch from the *stamps* and a
/// caller has no way to know which one ran.
///
/// The ring is deliberately wrapped (20 pushes into 16 slots), so the logical
/// window does not start at 0 and the cursor clamp is exercised.
///
/// **Mutants, run rather than asserted.** Drop the `.clamp(lo_logical, newest)`
/// in `bracket_from` ⇒ fails here, for the `start` values past the window.
/// Delete the `*cursor = i` write-back in `sample_with_twist_from` ⇒ the
/// equivalence loop still passes — a stale hint is still a hint, which is the
/// whole safety argument for cursors — so the final block asserts the cursor
/// *advances*, which is the only observable the write-back has.
///
/// One tempting mutant is **not** this test's: routing `t == t_new` through
/// `seek` instead of taking the left limit at `newest - 1` breaks both callers
/// identically, so an equivalence test cannot see it. It is killed by
/// `at_the_newest_stamp_the_twist_is_the_left_limit`, which checks the value.
#[test]
fn sample_with_twist_from_agrees_with_sample_with_twist_from_every_cursor() {
    let hr = HeapRing::new(16);
    let ring = hr.ring();
    for i in 0..20u64 {
        ring.push(i as i64 * 100, &pose(i + 1)).unwrap();
    }
    // Off-knot stamps that walk out of the retained window at both ends
    // (`retained` is 15, so the window is 500..=1900), plus the exact knots —
    // including `t_new`, which is the one index this sampler reaches without a
    // search.
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
    // Non-vacuity: the fixture must actually interpolate somewhere, or every
    // comparison above is between two copies of the same error.
    assert!(ring.sample_with_twist(1234, ExtrapPolicy::Error).is_ok());

    // The cursor must *move*, or the gallop restarts from `lo_logical` every
    // call and the batch layout is paying for a search it does not get. Nothing
    // above can see this: the answers are identical either way.
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
/// The sequence this guards: P1 claims, is `SIGSTOP`ped, is reaped, P2 claims,
/// P1 resumes and drops its `Publisher`. `push` already refuses (A4), but an
/// unconditional `owner.store(0)` in `release` would free *P2's* live claim and
/// let a third process claim the same edge — two writers on a single-writer
/// ring, which is the failure A4 exists to prevent, arriving through `Drop`.
/// The case the first version of this test missed, and the one that matters.
///
/// A revoked writer is *told* by `PushError::ClaimRevoked` to re-claim. Doing so
/// from the same participant slot produced an identical owner word under the
/// original `slot + 1` encoding, so dropping the stale `Publisher` freed the
/// brand-new claim while it was still publishing. Folding the epoch into the
/// word is what makes each acquisition distinguishable.
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

/// A4: a writer whose claim was reaped must refuse to publish.
///
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
    // `!Sync` is enforced structurally by the `PhantomData<Cell<()>>` marker on
    // `Publisher`; a positive `Sync` bound on it fails to compile. See the
    // `publisher_is_not_sync` compile-fail note in the doc tests.
}

// ---- end-to-end arena (exercises the unsafe arena_view surface) ---------

fn single_dyn_edge_arena() -> HeapArena {
    // 4 frame slots (root + 3), 1 dynamic edge, ring capacity 4.
    let layout = ArenaLayout::new(4, 1, alloc::vec![4]).unwrap();
    HeapArena::new(&layout, 4242, 0, [0u8; 16])
}

/// A poisoned [`Guard`] must fail **every** evaluation entry point, and must do
/// so without reading the view it was handed.
///
/// The second half is the whole reason the constructor exists: `Guard::new`
/// reads the topology generation immediately, so a facade whose arena has gone
/// away (a `fork` child, `docs/decisions/0005` §7) cannot build a guard even to
/// throw one away. If `poisoned` ever starts reading, that facade goes back to
/// faulting and nothing here would notice — so this asserts the *error*, which
/// only a non-reading construction can produce for an arena with no frames.
#[test]
fn a_poisoned_guard_refuses_every_evaluation() {
    // Two edges, because edge index `0` is the "no edge" sentinel in a
    // topology block (see `compile_rejects_the_no_edge_sentinel`), so a usable
    // link needs index 1 to exist.
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
    // `span` reads the same rings the sampler does, so a detached guard has to
    // silence it too. Mutant: drop `check_generation` from `Plan::span` and this
    // returns `Ok(None)` — a fork-poisoned child would be told the path is
    // answerable everywhere.
    assert_eq!(plan.span(&g), Err(LookupError::ChildDetached));
    // The same argument, one step further: `slowest_nominal_rate_mhz` is the
    // other half of `docs/decisions/0018`'s wait, and a waiter that missed the
    // poison would sleep on a period it read out of an arena it can no longer
    // reach, until its deadline, against a plan that can never be satisfied.
    // Mutant: drop `check_generation` from `Plan::slowest_nominal_rate_mhz` and
    // this returns `Ok(None)`.
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

    // An unpoisoned guard over the same arena still works — otherwise this test
    // would pass just as well against a plan that was broken to begin with.
    // Whatever it answers, it must not be the poison — that is the whole
    // control. An unpublished dynamic edge gives `NoData`; the point is only
    // that a live guard and a poisoned one are distinguishable.
    let live = Guard::new(ArenaView::new(&arena));
    assert_ne!(plan.at(&live, t), Err(LookupError::ChildDetached));
}

/// **The detached sentinel must not collide with a real generation.**
///
/// `Guard` encodes "detached" as a `generation` of `u64::MAX` rather than
/// carrying a 32-byte `Option<LookupError>` on a struct built once per `at()`
/// call. That is only sound while no real topology can reach the sentinel
/// value, and the cost of being wrong is a *live* guard reporting
/// `ChildDetached` — an unrecoverable error, on a tree that is perfectly fine.
///
/// This builds the exact collision: a plan compiled against a generation the
/// guard does not share, evaluated against an arena whose generation is `0`.
/// Any sentinel a real topology can produce turns that into `ChildDetached`
/// instead of the `TopologyChanged` it is. Mutant: `DETACHED = 0`.
#[test]
fn a_generation_mismatch_is_never_mistaken_for_a_detached_guard() {
    // There is exactly one safe value, so it is pinned. Every other `u64` is a
    // generation some tree can reach after enough mutations, and the collision
    // test below can only demonstrate the small ones — a sentinel of `2` is
    // just as wrong as `0` and no runnable test reaches it.
    assert_eq!(
        crate::plan::DETACHED_FOR_TEST,
        u64::MAX,
        "any sentinel below u64::MAX is a generation a real tree can reach"
    );

    let layout = ArenaLayout::new(4, 2, alloc::vec![4, 4]).unwrap();

    // Arena A, mutated so its generation is non-zero, is where the plan comes
    // from.
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

    // Arena B is untouched, so its generation is 0 — the value a careless
    // sentinel would pick.
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

/// Regression: the `ids` array lives in a zero-initialized arena, so the
/// "unpublished" sentinel has to be `0`. With `u32::MAX` nothing ever wrote the
/// sentinel, the publish-then-spin wait loop exited immediately with `id == 0`,
/// and a reader racing an interner got a bogus id.
#[test]
fn unpublished_sentinel_is_reachable_in_a_zeroed_arena() {
    assert_eq!(crate::frame::ID_UNPUBLISHED, 0, "sentinel must be zero");
    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    // A never-interned name reaches an empty hash slot and reports "not found"
    // rather than spinning on, or misreading, an unpublished id.
    assert_eq!(view.find_frame("never_interned").unwrap(), None);
    let a = view.intern("base_link").unwrap();
    assert_eq!(view.find_frame("base_link").unwrap(), Some(a));
}

// ---- A8: interning must not spin forever on a dead claimant --------------
//
// `docs/PHASE2.md` §1 A8, and its §11.3 crash point
// `intern.after_hash_cas_before_id_store`. The interleavings are checked by
// `loom_tests::intern_takes_over_from_a_claimant_that_died_before_publishing`;
// these tests cover the arena-view wiring — the `claiming` array's placement in
// the frame-hash region, the participant lookup, and the injected predicate.

/// PID of the participant these tests declare dead.
const DEAD_PID: u32 = 90_001;
/// PID of the participant that does the rescuing.
const LIVE_PID: u32 = 90_002;

/// Wedge `name`'s hash slot exactly as a process killed between the hash CAS and
/// the id store would leave it: hash claimed, `claiming` naming `owner_slot`, id
/// never published.
///
/// Returns the wedged slot index.
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
/// not finished within `secs`.
///
/// A regression in A8 does not produce a wrong answer, it produces an *infinite
/// spin*, and a test that simply calls `intern` would wedge CI instead of
/// reporting a failure. The worker is deliberately detached: if it is stuck it
/// stays stuck, and the process exits out from under it once the suite ends.
///
/// The deadline is a hang detector, not a performance assertion, so it scales
/// with the interpreter: a bounded spin that finishes in milliseconds natively
/// takes ~10 s under Miri, which put the tightest of these tests right on the
/// edge and made it fail only when the suite ran in parallel.
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
        // The dead participant's slot still reads LIVE — a `SIGKILL`ed process
        // never gets to clear it. That is exactly why the injected predicate,
        // and not the `state` field, is what detects the crash.
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

/// A8: a claimant that died in the two-instruction window *before* it could
/// record itself is recoverable too — `claiming` is still `CLAIM_UNRECORDED`, and
/// a registered interner may take that over.
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

    // No liveness predicate is needed or consulted: there is no claimant to
    // resolve. Being a registered participant is the whole requirement.
    let id = assert_completes_within(10, "intern of an unrecorded claimed slot", move || {
        ArenaView::new(&arena)
            .as_participant(live_slot)
            .intern("victim")
    })
    .unwrap();
    assert_eq!(id.get(), 1);
}

/// A8, the fail-safe direction (`docs/PHASE2.md` §6.2): a claimant that cannot be
/// proven dead is **never** stolen from. Waiting costs latency; stealing from a
/// working process costs correctness.
///
/// Asserting a negative about a spin needs a bounded wait, so the interner is
/// deliberately unblocked afterwards — that both keeps the test from leaking a
/// spinning thread and proves the waiter was still on the normal publish path.
#[test]
fn a_claimant_that_cannot_be_proven_dead_is_never_stolen_from() {
    use crate::sync::Ordering;

    let arena = single_dyn_edge_arena();
    let view = ArenaView::new(&arena);
    let (owner_slot, _) = view.participants().register(DEAD_PID, 7, 0).unwrap();
    let (live_slot, _) = view.participants().register(LIVE_PID, 8, 0).unwrap();
    let wedged = wedge_intern_slot(&view, "victim", owner_slot);
    // Pretend the claimant did allocate id 1 and write its record, so the
    // publish we perform below is a real one.
    let real = ArenaView::new(&arena)
        .as_participant(owner_slot)
        .intern("other")
        .unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            // No injected predicate: the claimant's slot reads LIVE, so it is
            // presumed alive however long it takes. This must block.
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

    // ...and it then resolves to whatever the claimant published, exactly as the
    // pre-A8 publish-then-spin handshake always did. ("victim" and "other" share
    // a record only because this test hand-published; the name check is what
    // reports the mismatch.)
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
/// non-zero `u32`, so out-of-range ids reach these accessors from safe code —
/// including from every path in the `tf_tree` facade, whose one `unsafe` is
/// `OwnedWriter`'s lifetime extension and is nowhere near these. They must
/// report the miss, not form an out-of-bounds pointer.
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

/// A root plus a chain of `links` frames, `f0 -> f1 -> … -> f{links}`, where the
/// edge on the link into `f{i}` is `EdgeId(i)`. Returns the arena and the frame
/// ids in chain order, so `chain[k]` is `k` edges below the root.
///
/// Edge id == the child's frame id: non-zero, in range, and unique, so no link
/// trips the `MissingEdge` sentinel check instead. `sentinel_at` names a link to
/// record with edge `0` on purpose — that is how the sentinel defect is placed
/// at a chosen distance along the path.
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

/// A **Y**: one root with a target branch of `p` links and a source branch of
/// `q`, so `lookup(target, source)` walks `p + q` edges through the lowest
/// common ancestor. Edge ids are the child frame's own id, as in
/// [`chain_arena`].
///
/// The straight chain is the shape every depth test in this file used, and it is
/// the shape that cannot see a **per-side** bound: one side is zero.
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

/// An `EdgeMeta` for a synthetic chain: every edge is `kind`, in domain `0`, and
/// carries a **non-identity** static pose that differs per edge.
///
/// The pose has to differ per edge, and it has to be a rotation as well as a
/// translation. An all-identity harness makes composition order invisible —
/// `I * I` is `I` in whatever association — and `docs/decisions/0034`'s
/// rationale (D) turns on exactly that being visible.
fn distinct_pose(edge: u32) -> Iso3 {
    // `pose` is the suite's own generator: an `exp_se3` of a twist with three
    // non-zero rotation components, so no two edges compose commutatively.
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

/// [`MAX_DEPTH`](crate::MAX_DEPTH) is the length of the **compiled** array, and
/// this is its boundary: `MAX_DEPTH` dynamic edges compile whole, one more is
/// refused.
///
/// The refusal reports the **exact folded length**, not the bound. That is what
/// `fold` running past the end of its output array buys, and it is the half of
/// `0034` that a test can see: before it, this assertion read
/// `depth: MAX_DEPTH`, which is the self-contradiction
/// `crates/tf_tree/src/tree.rs` rendered as "path depth 16 exceeds the maximum
/// of 16".
///
/// Mutant: `if n > MAX_DEPTH` -> `if n > MAX_PATH_EDGES` in `fold`, i.e. the
/// refusal keyed to the wrong bound. **Applied and run**: `881 tests run: 877
/// passed, 4 failed` — this test, the precedence table, the raw-bound test and
/// the corpus test, all four on
/// `plan.rs:536: range end index 33 out of range for slice of length 32`. The
/// prediction written here first was a *truncated plan*, and it is wrong:
/// `Plan::new` slices `steps[..len]` to derive `dyn_count`, so an out-of-range
/// `len` panics there rather than shipping a plausible-looking short plan. The
/// note is what the mutation produced, not what it was expected to.
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
/// this is the whole of `0034`: the same chain length that a dynamic path is
/// refused at compiles to a *single step* when the links are static.
///
/// The walk's refusal reports `MAX_PATH_EDGES + 1` rather than a length, and
/// that is deliberate — the walk stops because it has no buffer left, so it
/// never learns how much further the path went. `fold` can count past its array
/// because its input is already bounded; the walk cannot, because a corrupt
/// parent chain with a cycle in it would not terminate.
///
/// Mutant: `if nt + ns == MAX_PATH_EDGES` -> `if $n >= MAX_PATH_EDGES` in
/// `push_edge!` — the **per-side** spelling this replaced, and the one HEAD had.
/// **Applied and run twice.** Against the test as first written — a straight
/// chain and nothing else — the whole workspace passed, `881 passed, 0 failed`:
/// a one-sided walk puts every edge on one side, so a per-side bound and a
/// combined one are the same bound and no straight chain can tell them apart.
/// That is why the `y_arena` row below exists, and against it the same mutant
/// gives `880 passed, 1 failed` — the 40-up/40-down Y **compiles**, returning
/// `Ok(Plan { len: 1 })` for a path of 80 edges, because neither side reaches 64
/// on its own. `MAX_PATH_EDGES = 64` would have meant "up to 127 edges walked".
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

    // **The bound is on `nt + ns`, and only a Y can see that.** Per-side guards
    // — which is what this replaced, and what HEAD had at `MAX_DEPTH` — let a
    // path 40 edges up and 40 edges back down walk all 80 before anything
    // refuses, because neither side reaches the bound on its own. The whole
    // suite passes with the per-side spelling restored **except this
    // assertion**, and the way it fails is the point: the 80-edge path
    // *succeeds*, `Ok(Plan { len: 1 })`, because these links are static and 80
    // of them fold to one step. A bound of 64 would have meant 127 edges
    // walked.
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

/// **Error precedence, which nothing in the workspace pinned** — which is why
/// `0034` could move it invisibly. A table over (defect kind) x (position, before
/// or after the compiled bound fills) x (foldable or not).
///
/// The discriminating row is `("unknown edge", 40, all-dynamic)` and its
/// `MixedTimeDomains` twin: the path is 48 dynamic edges, so the compiled array
/// is full by step 32 and the defect sits 8 steps past it. `fold` skips the
/// write, keeps counting, and still resolves that edge — so the caller is told
/// what is *wrong* with their path rather than how long it is.
///
/// Mutant (variant A, the cheap alternative `0034` step 3 reads as): give `fold`
/// an early `if n >= MAX_DEPTH { return Err(TreeTooDeep { depth: n as u16 }) }`
/// at the top of the loop body. **Applied and run**: three of the nine rows move
/// and six do not —
///
/// ```text
/// clean, all-dynamic: got TreeTooDeep { depth: 32 }, want TreeTooDeep { depth: 48 }
/// unknown edge past the bound, all-dynamic: got TreeTooDeep { depth: 32 }, want UnknownEdge { edge: EdgeId(9) }
/// mixed domains past the bound, all-dynamic: got TreeTooDeep { depth: 32 }, want MixedTimeDomains { edge: EdgeId(9), expected: 0, got: 7 }
/// ```
///
/// — so the mutant loses both the precedence *and* the true folded length. The
/// six that stay green are the control: every all-static row, and
/// `missing edge past the bound, all-dynamic`, which is raised in the walk and
/// therefore wins by position under either variant. Those two defect rows are
/// what separates the two implementations, and they are the reason the dearer
/// one ships.
///
/// The last row is the other side of the same coin: a defect past the **raw**
/// bound is not reachable, because the walk really does stop there. Depth wins
/// that one, and it has to — `fold` never sees an edge the walk did not collect.
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

    /// Path length in edges, and the two positions, **counted in walk order**:
    /// the lookup runs `chain[LEN] -> chain[0]`, so step 0 is the link into the
    /// leaf and step `k` is `k` edges above it. On an all-dynamic path step `k`
    /// is also compiled step `k`, so `AFTER` sits 8 steps past a full array.
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
        // The control. All-dynamic overruns the compiled bound and reports its
        // true folded length.
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
        // All-static: the compiled array never fills, so these rows read the
        // same under either variant. They are the control that shows the two
        // rows above are about the bound and not about the position.
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
        // Past the *raw* bound the walk really does stop, so the defect is
        // unreachable and depth wins. It has to: `fold` never sees an edge the
        // walk did not collect.
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

    // Rows are collected rather than asserted one at a time: the table's value
    // is which rows move together, and a per-row `assert_eq!` reports only the
    // first.
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
///
/// ANYmal C is the one structure in the 91-robot URDF survey where folding
/// materially helps: its worst frame pair is **20 joints** apart and folds to
/// **13** steps, because the chain is `fixed fixed revolute` six times over. It
/// is the right fixture precisely because a straight static chain is not — an
/// alternation exercises the *adjacent*-only collapse rule, which a run of
/// statics cannot get wrong.
///
/// At `MAX_DEPTH = 16` this shape was refused outright (20 raw edges). It now
/// compiles, and the two numbers that matter are pinned: 20 edges walked, 13
/// steps kept.
///
/// The ceiling rows are the survey's other two numbers: the worst *diameter*
/// anyone measured is 30 joints (Unitree H2 Plus), which must compile with a
/// deployed `/tf` prefix on top of it; and a path past `MAX_PATH_EDGES` must
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
    // The failed mutation left the tree intact — `a` is still a root. The
    // generation is pinned exactly three lines below, so there is nothing a
    // parity check could add here.
    assert_eq!(read(a).0, 0);
    // And edge_of_child for the earlier attach survived the aborted mutation.
    assert_eq!(read(c).2, 20);
    // The published topology is byte-identical, so the generation must not have
    // advanced — advancing it would invalidate every compiled plan in the process
    // for a mutation that never happened.
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
/// next writer to reach it must recover.
///
/// This is the cross-process failure `docs/PHASE2.md` §1 A5 describes. Within one
/// process it was unobservable, because the crash that stranded the slot also
/// took every reader with it; across processes the readers survive. An
/// incrementing writer would read the stale odd `s` and store `s+1` — an *even*
/// value — while the payload was still being written, so readers would accept a
/// torn pose as published. Forcing the parity (`s | 1`) is idempotent on a stale
/// odd value and heals it.
#[test]
fn stale_odd_seq_from_a_dead_writer_is_healed_by_the_next_push() {
    let ring = HeapRing::new(4);
    let pose = exp_se3([0.1, 0.2, 0.3, 1.0, 2.0, 3.0]);

    // Simulate a writer killed after flipping slot 0 to odd but before
    // publishing: the slot is odd and `head` was never bumped.
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

    // The healed value must also still be *greater* than the stale odd one, so
    // the seqlock's monotonicity — what lets a reader detect a concurrent
    // overwrite — is preserved rather than reset.
    assert!(seq > 1, "seq went backwards: {seq}");
}

/// `register_at` takes the slot it is named, or nothing.
///
/// The exclusivity property under concurrency is
/// `loom_tests::two_joiners_handed_the_same_slot_cannot_both_take_it`; this
/// covers the two error paths, which are ordinary sequential behaviour and do
/// not need a 30-second model check to state.
#[test]
fn register_at_takes_the_named_slot_or_fails() {
    use crate::participant::{ParticipantError, ParticipantTable};

    let slots: Vec<ParticipantRecord> = (0..4).map(|_| ParticipantRecord::default()).collect();
    let table = ParticipantTable::new(&slots);

    let inc = table.register_at(2, 1234, 99, 0).expect("slot 2 is free");
    assert_eq!(inc, 1);
    assert_eq!(table.identity(2).map(|id| id.0), Some(1234));

    // A second joiner handed the same slot is refused, and — the part that
    // matters — is *not* quietly given a different one, which would break the
    // slot/lock-byte correspondence the method exists to establish.
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
///
/// This is the cheap half of the check, and it is honest about what it covers:
/// the *sequential* case, which the previous two-word guard (load `incarnation`,
/// then CAS `state`) also passed. The failure that guard actually had needs the
/// reap and the re-`register` to land between its two words, so the test that
/// exercises it is `loom_tests::a_late_release_racing_a_slot_handover_frees_nobody`.
/// Keep both: this one runs on every `just test`, the loom one on `just loom`.
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
///
/// The clean-detach path is `release`, which needs the incarnation as a
/// separate argument; a reaper has no incarnation to pass, only the word it
/// observed — and `live_word` packs the incarnation into that word, so the
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
///
/// `RESERVED` is what a process killed between `fill_slot`'s CAS and its
/// publishing store leaves behind — §11.3's
/// `attach.after_slot_assigned_before_publish` row. It carries no incarnation,
/// so `release` has no word to name it by; the observed word is the only handle
/// on it. That this is *safe* is not a property of this function — it holds
/// because the byte is the occupancy authority (`0028` steps 0b and 0c), and
/// the memory-model half is `loom_tests::reclaim_races_register`.
#[test]
fn reclaim_frees_a_slot_from_an_observed_reserved() {
    use crate::participant::{ParticipantTable, FREE, RESERVED};
    use core::sync::atomic::Ordering;

    let slots: Vec<ParticipantRecord> = (0..2).map(|_| ParticipantRecord::default()).collect();
    let table = ParticipantTable::new(&slots);

    // Staged rather than raced: a process that died inside `fill_slot` executes
    // no further instruction, so every store it will ever make has happened.
    table
        .get(1)
        .expect("slot")
        .state
        .store(RESERVED, Ordering::Release);
    // `register` cannot collect it — it only ever CASes from FREE — which is why
    // such a slot was lost to everybody for ever.
    assert_eq!(table.register(1, 1, 0).expect("register").0, 0);

    assert!(table.reclaim(1, RESERVED));
    assert_eq!(
        table.get(1).expect("slot").state.load(Ordering::Acquire),
        FREE
    );
    assert_eq!(table.register(5678, 100, 0).expect("register").0, 1);
}

/// `reclaim` fails when the observed word has changed under it.
///
/// That is the whole safety bound: the reclaimer's verdict was formed against
/// one occupancy, and a slot that has been freed, re-granted and re-occupied
/// since carries a different `live_word`. `docs/decisions/0028` names that row
/// `reclaim.probe_then_reoccupied`, in its own crash matrix — the rows §11.3 is
/// *missing* for this path. **§11.3 has no such row**, and citing one there
/// would be the mistake that record's question-6 evidence was itself corrected
/// for. It is also the case a caller must not mistake for "already free": the
/// return is *did this CAS fire*, not *is the slot free now*.
///
/// **This is what pins `reclaim`'s CAS guard.** The loom model
/// `loom_tests::reclaim_races_register` does not — measured, it never reaches
/// that CAS on a contended slot — so a `reclaim` that ignored `observed`
/// entirely would pass the model and fail here.
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
/// would-be interner and a reader — never waited on forever, never stolen from.
///
/// `CLAIM_ANONYMOUS` is the one owner value that is simultaneously (a) evidence
/// that somebody real is working and (b) unjudgeable, because there is no
/// participant slot to ask `/proc` about. Neither of A8's two normal answers
/// applies: takeover would allocate a second id for one name and permanently
/// inflate `frame_count`, and abandoning would report "no such frame" for a name
/// that is being published right now. So the bound has to terminate in a third
/// way, `InternContended` — a caller-visible "try again", which is exactly the
/// contract A8 asks for and the first version of this wiring did not have (it
/// `continue`d, reintroducing the unbounded spin A8 exists to remove).
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
// Found by review: the Hold arm, the ConstantTwist arm and the `t == t_new`
// left-limit branch were all reachable through `tf_tree_core`'s public surface
// and covered by no test at all. The plan fold always passes
// `ExtrapPolicy::Error`, so none of it was exercised through the facade either.

/// A ring with `n` samples at `i * 100` ns, moving along one screw.
fn twist_ring(hr: &HeapRing, n: u64) {
    for i in 0..n {
        hr.ring().push(i as i64 * 100, &pose(i)).unwrap();
    }
}

/// **`Hold` reports a zero twist, and that is the derivative of holding.**
///
/// Mutant: return the last segment's twist instead of `Twist::ZERO` ⇒ fails.
/// The pose is pinned past `t_new`, so its derivative is zero by definition;
/// reporting the incoming velocity would tell a controller the body is still
/// moving after the data stopped.
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

/// **`ConstantTwist` extends along the last segment, and reports that twist.**
///
/// Also pins that the pose agrees **bit-for-bit** with `sample`'s under the same
/// policy — the two used to compute it by different routes (`log_se3`/`exp_se3`
/// against the screw form), which is the sort of divergence nobody notices until
/// two call sites disagree in the field.
///
/// Mutant: drop the `NANOS_PER_SEC / dt` scaling ⇒ the twist is per-100-ns
/// rather than per-second and this fails by 1e7.
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

    // The samples are 100 ns apart on one screw, so the twist is the segment's
    // xi scaled to per-second. Cross-check against the in-window twist, which
    // must be the same screw.
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
    // A *non-identity* single sample. `twist_ring(&hr, 1)` pushes `pose(0)`,
    // which is exactly `Iso3::IDENTITY`, so an implementation that invented an
    // identity pose rather than holding the published one passed unnoticed —
    // and `is_ok()` would not have told them apart even if it had not.
    hr.ring().push(0, &pose(3)).unwrap();
    assert!(matches!(
        hr.ring()
            .sample_with_twist(10_000, ExtrapPolicy::ConstantTwist),
        Err(LookupError::NoSegment { .. })
    ));
    // The plain sample still answers, and it answers with the sample that was
    // published — held, not extrapolated and not invented.
    //
    // Mutant: return `Iso3::IDENTITY` from `constant_twist`'s
    // `newest == lo_logical` arm ⇒ fails.
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
///
/// Mutant: use `bracket` unconditionally instead of special-casing `t == t_new`
/// ⇒ `bracket`'s precondition (`stamp[lo] <= t < stamp[hi]`) is violated, it
/// returns `newest`, and `i + 1` reads a slot outside the published window.
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

/// **Below the oldest retained stamp is an error, not a clamp** — with or
/// without a derivative, and under every extrapolation policy, because the
/// policies govern the *newer* end only.
///
/// Mutant: delete the `t < t_old` guard from `sample_with_twist` ⇒ `bracket`'s
/// precondition is violated and it returns a bracket that does not contain `t`,
/// so the caller gets a confidently extrapolated pose from the wrong segment.
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

/// An empty ring has no pose and therefore no twist — `NoData`, not `NoSegment`.
/// The two are different questions and the distinction is the whole reason
/// `NoSegment` exists.
#[test]
fn an_empty_ring_is_no_data_not_no_segment() {
    let hr = HeapRing::new(8);
    assert!(matches!(
        hr.ring().sample_with_twist(0, ExtrapPolicy::Error),
        Err(LookupError::NoData { .. })
    ));
}

/// The two fields `Plan::new` derives must always equal what a fresh scan of the
/// plan's own steps produces.
///
/// `Plan::at` used to compute both on every call — once through `check_domain`
/// → `has_dynamic`, once for `note`'s attribution — each an O(`len`) scan over a
/// 1 KiB step array. Storing them at compile time removed both from the scalar
/// hot path, and bought in exchange a value that can go *stale* with respect to
/// the steps it describes. Nothing else in the type can catch that, so this
/// does, across the four shapes whose answers differ.
///
/// The `== 1` rule in `first_dynamic_edge` is the subtle one: a plan crossing
/// several dynamic edges must credit **no** edge (`EdgeId(0)`), because
/// attributing a multi-edge plan's success to one of them would put a number in
/// `doctor`'s table meaning something different from every other number in the
/// same column.
///
/// Mutants this kills, checked by making each edit and watching it fail:
/// `first_dynamic_edge` testing `>= 1` instead of `== 1`, and `dyn_count` never
/// incremented.
///
/// Assigning `first_dyn` on *every* dynamic step rather than only the first is
/// an **equivalent** mutant — the whole suite still passes with it applied —
/// because `first_dyn` is only ever read when `dyn_count == 1`, where the first
/// and the last dynamic step are the same one. Recorded so nobody reads the
/// field name as a guarantee the tests enforce; the `if dyn_count == 0` guard is
/// there for clarity, not for correctness.
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

    // The `edge_meta` closure is the caller's, so a plan's steps can be forced
    // dynamic or static without declaring records for either.
    // `&ArenaView` is `Copy`, so the inner `move` copies the borrow rather than
    // consuming the view — which is what lets `meta` be called more than once.
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

    // The shapes above are only a guard if they actually differ, so pin the
    // answers themselves — otherwise a `Plan::new` that derived nothing at all
    // would still agree with a scan of a plan that had no dynamic steps.
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

/// A five-frame chain `f0 -> f1 -> f2 -> f3 -> f4` over four dynamic edges,
/// where edge `i` declares `rates[i]` milli-hertz (`0` = undeclared).
///
/// Edge index `0` is the topology block's "no edge" sentinel (see
/// `compile_rejects_the_no_edge_sentinel`), so the chain occupies edges 1–4 and
/// slot 0 is left with capacity 0. `stamp_off`/`pose_off` are assigned
/// cumulatively, which is what `TreeBuilder` does in the facade and what
/// `ArenaView::ring_of` bounds-checks against.
///
/// **The rings come back empty**, and the rate tests rely on that: a *declared*
/// rate is a property of the topology, and reading it must not depend on a
/// stream existing. A test that needs the fold to actually answer calls
/// [`seed_rate_chain`] on the view afterwards.
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

/// [`rate_chain_arena`] with the edge table one slot shorter: the same five
/// frames and the same four parent links, but `max_edges = 4`, so `EdgeId(4)` is
/// **out of range** and `ArenaView::edge` answers `None` for it.
///
/// The point is the *generation*. `declare_edge` writes a table slot and does
/// not touch the topology word, so an arena built from the same five `intern`s
/// and the same four `set_parent`s carries the same generation as
/// `rate_chain_arena` regardless of how many edge records it holds. That is what
/// lets a plan compiled against the full arena get past `check_generation` here
/// and reach the `UnknownEdge` arm, which is otherwise unreachable.
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

/// Publish `pose(i)` into edge `i` of a [`rate_chain_arena`], twice: at stamp
/// `0` and at stamp `1000`. Returns the pose a full `f0 -> f4` lookup anywhere
/// in that window must produce.
///
/// Two samples carrying the *same* pose, so interpolation across the segment
/// reproduces it exactly and the composed answer is
/// `pose(1)·pose(2)·pose(3)·pose(4)` with no float slack to allow for. One
/// sample would leave the fold with no segment and `ExtrapPolicy::Error`.
///
/// The `Publisher` is dropped at the end of each iteration, which releases the
/// claim; the samples it wrote stay in the ring, which is all a reader needs.
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
///
/// A tag is written into `EdgeRecord::domain` at declaration time and read by
/// every consumer, every recording and every diagnostic, so re-numbering one
/// silently re-interprets arenas already on disk — `docs/API.md` §5.2's
/// "unfixable after the fact" applied to the numbering rather than to the
/// choice. This test exists to make that re-numbering a red build.
///
/// **Distinctness is a separate test on purpose.** These four `assert_eq!`s
/// imply it, so a distinctness loop placed after them could never be reached in
/// a failing state — it would be a dead assertion carrying a written argument
/// for why it is not dead, which is what this test used to hold. What
/// distinctness actually constrains is the *set*, and that survives a
/// deliberate re-numbering of these literals, so it now lives in
/// [`the_built_in_domain_tags_are_pairwise_distinct`], where it is the first
/// thing that runs.
///
/// Mutant: `SimDomain::TAG = 0` (the value it effectively had before it was a
/// type). Applied: `assert_eq!(SimDomain::TAG, 2)` fails, `left: 0, right: 2`.
#[test]
fn the_built_in_domain_tags_are_fixed() {
    use crate::plan::{Domain, SensorDomain, SimDomain, SteadyDomain, SystemDomain};

    assert_eq!(SystemDomain::TAG, 0, "the default domain must stay tag 0");
    assert_eq!(SensorDomain::TAG, 1);
    assert_eq!(SimDomain::TAG, 2);
    assert_eq!(SteadyDomain::TAG, 3);
}

/// **No two built-in domains share a tag.**
///
/// [`the_built_in_domain_tags_are_fixed`] pins today's four literals; this pins
/// the property those literals happen to have. `Domain::TAG` is a per-impl
/// constant with nothing structural preventing a collision, and two domains
/// sharing a tag is exactly the collapse `docs/API.md` §2.5 describes —
/// `TimeDomainMismatch` stops firing between them and nothing else changes.
/// Standing alone rather than after the value assertions is what makes it
/// reachable: it is the first assertion in its own test.
///
/// **What it does not cover:** a *fifth*, user-declared domain colliding with a
/// built-in. §2.5 keeps the trait open and reserves `0`–`3` by documentation,
/// so that collision is possible and is not checkable from inside this crate.
///
/// Mutant: `SimDomain::TAG = 0`. Applied: fails at the first pair —
/// ``assertion `left != right` failed: two built-in domains share a tag: 0 and
/// 0``.
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
///
/// The point of adding the two domains is not that they exist but that the
/// mismatch *fires*: with only two built-ins a sim deployment and a
/// steady-clock driver both take [`SystemDomain`] by default, so a node mixing
/// `/clock` time with a driver's clock gets a tree wrong by however long the
/// bag has been playing, and well-formed the whole time (`docs/API.md` §2.5,
/// §5.2).
///
/// **The fixture is seeded, and that is the whole point.** Against empty rings
/// this test passed for the wrong reason: `NoData` shadows the domain check, so
/// a tag collision died at the *data* arm and the silent wrong answer was never
/// demonstrated. With [`seed_rate_chain`] the same stamp in the plan's own
/// domain returns a pose, so the two refusals below are refusals of a query
/// that would otherwise have been answered — which is the failure being
/// described: a `/clock` stamp served, plausibly, out of a wall-clock stream.
///
/// The control at the end is that successful query. It is also what keeps this
/// from passing against a plan that refused everything.
///
/// Mutant: `SimDomain::TAG = 0`. Applied: the first assertion fails with
/// `left: Ok(Iso3 { q: Quat { w: 0.9909511798837932, .. }, t: Vec3 { x:
/// 0.8347429715979104, .. } }), right: Err(TimeDomainMismatch { expected: 0,
/// got: 2 })`. That `Ok` is bit-identical to the pose the control asserts
/// (checked by swapping the control in as the first assertion under the same
/// mutant, where it passes) — i.e. the sim stamp was answered out of the
/// wall-clock stream, which is the silent wrong answer this mechanism exists to
/// prevent.
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
/// epoch.**
///
/// `docs/API.md` §5.1 makes the converter normative *and* makes "no float
/// anywhere" the reason it exists: `sec * 10**9 + nanos`, written by hand in
/// every node, is the line users resent and also the line that wraps silently.
/// `1_700_000_000_123_456_789` needs 61 bits of significand and `f64` has 53,
/// so **the sum** cannot survive an `f64`; note carefully that the *product*
/// can — see Mutant B′.
///
/// The negative case is the one a normalising implementation gets wrong: a
/// `timespec` of `(-1, 250_000_000)` is 250 ms *after* one second before the
/// epoch, i.e. −750 ms, not −1250 ms.
///
/// Mutant: `sec * NANOS_PER_SEC + nanos` in `i64` with plain arithmetic.
/// Applied: a release build wraps silently and the `i64::MAX`/`i64::MIN`
/// assertions fail with `Some(..)`.
/// Mutant B: the whole expression in `f64` — `(sec as f64 * 1e9 + nanos as f64)
/// as i64`. Applied (verified by editing `plan.rs` and running this test): the
/// nanosecond-precision assertion fails with
/// `left: 1700000000123456768, right: 1700000000123456789` — 21 ns of error in
/// a stamp that prints as though it were exact.
/// Mutant B′: the *staged* cast, `(sec as f64 * 1e9) as i128 + nanos as i128`,
/// which is the form a reader will assume B covers, **and it does not**.
/// `1.7e18` is `12969970703125 × 2^17`, so the product alone is exact in `f64`
/// and the precision assertion above *passes*. It dies instead at the
/// `i64::MIN` edge — `left: -9223372036854775296, right: -9223372036854775808`,
/// where the `f64` ulp is 2048 ns — which is why the range assertions below are
/// not redundant with the precision one. Deleting either lets one f64
/// implementation through.
/// Mutant C: the staged `sec.checked_mul(1e9)?.checked_add(nanos)` this was
/// first written as. Applied: the `i64::MIN` assertion fails with
/// `called Option::unwrap() on a None value` — the product alone is below
/// `i64::MIN` while the sum is exactly `i64::MIN`, so one second of
/// representable stamps at the negative end is refused.
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

    // The exact edges of the representable range, both ends. One nanosecond
    // further in either direction has no answer at all.
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
///
/// Both source formats — `builtin_interfaces/Time` and `struct timespec` —
/// define the field as the remainder, so a value outside `[0, 1e9)` means the
/// pair is not a `Time`. Normalising it is arithmetically exact and still
/// wrong: it turns a malformed message into a plausible stamp, and a plausible
/// stamp is one nothing downstream will question.
///
/// `999_999_999` is the last accepted value and `1_000_000_000` the first
/// refused one, so an off-by-one in the bound cannot pass.
///
/// Mutant: drop the `nanos >= NANOS_PER_SEC` guard. Applied: the
/// `1_000_000_000` assertion fails with `Some(Stamp(1000000000))` — a message
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
///
/// `tf_tree_core`'s dependency budget is `libm` + `bytemuck` + `blake3`
/// (`docs/PROJECT.md` §5), so there is no `libc::timespec` to accept; the two
/// fields are `time_t` and `long`, both `i64` on a 64-bit target, so the call
/// site needs no cast.
///
/// POSIX allows a negative `tv_nsec` only in a *relative* `timespec` — the kind
/// passed to `nanosleep`. An absolute time from `clock_gettime` never has one,
/// so a negative value means an interval is being converted as if it were an
/// instant. Everything else is refused by `from_parts` already; this guard is
/// the only thing `from_timespec` adds.
///
/// Mutant: drop the `tv_nsec < 0` half of the guard, leaving
/// `tv_nsec >= NANOS_PER_SEC`. Applied: the `(0, -4_294_967_296)` assertion
/// fails with `Some(Stamp(0))` — an interval of −4.29 s answered as the epoch
/// itself. **Verified by editing `plan.rs` and running this test**, which is
/// the only way to know, because the obvious probes do not kill it: `-1 as u32`
/// is `4_294_967_295`, which `from_parts` refuses as a non-remainder with or
/// without the guard, so `(0, -1)` and `(-1, -1)` pass either way. Only a
/// `tv_nsec` whose **low 32 bits land back inside `[0, 1e9)`** reaches
/// `from_parts` with a value it will accept, and those are the two cases the
/// guard actually exists for.
#[test]
fn from_timespec_refuses_a_relative_interval() {
    type S = Stamp<SensorDomain>;

    assert_eq!(
        S::from_timespec(1_700_000_000, 123_456_789)
            .unwrap()
            .nanos(),
        1_700_000_000_123_456_789
    );
    // The negatives a caller actually produces. These are refused by the
    // `as u32` cast landing outside `[0, 1e9)`, not by the sign guard — see the
    // Mutant note; they document the API, they do not defend it.
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
    // A negative *second* with a valid remainder is a legitimate pre-epoch
    // stamp and must still be accepted.
    assert_eq!(
        S::from_timespec(-1, 250_000_000).unwrap().nanos(),
        -750_000_000
    );
}

// ---- the declared publish rate a waiter sleeps on ------------------------

/// **The slowest declared rate on the path is the answer, not the fastest.**
///
/// `docs/decisions/0018` puts the blocking wait in the caller and gives it two
/// engine-side inputs: `Plan::span` for the shortfall and this for the period
/// to sleep. A plan is answerable only once *every* dynamic edge has reached
/// the stamp, so the edge that decides when the wait ends is the one that
/// publishes least often. Sleeping a period of the fastest edge on a path that
/// also carries a 10 Hz map update wakes a hundred times per useful answer —
/// which is the poll the prediction exists to avoid, arrived at from inside it.
///
/// The rates are 1 kHz / 200 Hz / 50 Hz / 10 Hz, the real span of a robot
/// (an IMU down to a map update), and they are declared in *descending* order
/// so an implementation that returns the first, the last, or the fastest each
/// gives a different wrong answer.
///
/// Mutant: `current.max(mhz)` instead of `current.min(mhz)` — return the
/// fastest. Applied: fails with `left: Ok(Some(1000000)), right:
/// Ok(Some(10000))`; a waiter would then sleep 1 ms per re-check against an
/// edge that publishes every 100 ms.
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

    // Control: a plan over a sub-path that excludes the 10 Hz edge answers
    // differently, so the assertion above is about the fold and not about the
    // arena having only one declared rate in it.
    let mid = view.intern("f2").unwrap();
    let short = compile_chain(&view, root, mid);
    assert_eq!(short.slowest_nominal_rate_mhz(&g), Ok(Some(200_000)));
}

/// **`0` means undeclared and is skipped; a plan where nobody declares answers
/// `None`.**
///
/// `EdgeRecord::nominal_rate_mhz` uses `0` for "not declared" — an edge sized
/// by an explicit slot count states no rate — and `docs/PHASE5.md` §6's
/// `TFT007` amendment makes that distinction load-bearing rather than
/// cosmetic. Read as a rate, the sentinel is the minimum of every set it
/// appears in and yields an infinite period, so **one** undeclared edge would
/// silently disable the wait for the whole path.
///
/// `None` is a third answer, not a degenerate minimum: the caller falls back to
/// a conservative period and says so once at startup (`0018` *Consequences*).
/// Collapsing it to `Some(0)` would make "nobody declared" indistinguishable
/// from "somebody declared 0 Hz", which is the same conflation from the other
/// side.
///
/// Mutant: drop the `if mhz == 0 { continue; }` skip. Applied: the first
/// assertion fails with `Ok(Some(0))` — a waiter computing `1e9 / 0`.
/// Mutant B: seed the fold with `Some(u32::MAX)` instead of `None`. Applied:
/// the all-undeclared assertion fails with `Ok(Some(4294967295))`, a declared
/// 4.29 MHz nothing publishes at.
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
///
/// This is where the method parts company with `Plan::span`, deliberately. A
/// declaration is a property of the topology; a window is a property of the
/// stream. The caller asking how long to sleep is by definition asking *before*
/// the data exists — that startup case is the entire reason `0018`'s loop
/// exists — so returning `NoData` here would make the method unusable at the
/// only moment it is needed.
///
/// The `span` assertion beside it is the control: the same plan, the same
/// guard, and the empty rings *do* stop `span`, which is what makes the first
/// assertion a statement about this method rather than about the fixture.
///
/// Mutant: implement `nominal_rate_mhz` through `Guard::window` (or add a
/// `newest_stamp` probe) so it inherits the `NoData` refusal. Applied: the
/// first assertion fails with `Err(NoData { edge: EdgeId(1) })`.
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
///
/// This is the one documented arm of `slowest_nominal_rate_mhz` a *well-formed*
/// arena cannot reach, and the fixture is built to say why rather than to hide
/// it: `compile` already refuses an unknown edge, so a plan and the arena it was
/// compiled against never disagree, and evaluating against a *different* arena
/// is caught by `check_generation` whenever the two generations differ.
/// [`short_edge_table_arena`] is the residue — same frames, same parent links,
/// same generation, one fewer edge slot — and it is the only shape that reaches
/// the `?`.
///
/// Contrived, and deliberately so: the arm is on the path a shim's timeout loop
/// consults every iteration (`docs/decisions/0018`), and the failure a *timeout*
/// API hides best is a call that quietly answers "nobody declared a rate" and
/// sends the caller to a conservative sleep forever.
///
/// Mutant: swallow the error —
/// `let Ok(mhz) = g.nominal_rate_mhz(*edge) else { continue };`. Applied
/// (verified by editing `plan.rs` and running this test): fails with
/// `left: Ok(Some(200000)), right: Err(UnknownEdge { edge: EdgeId(4) })`.
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

    // Control: the two arenas really do agree on generation, so the assertion
    // below is about `UnknownEdge` and not about `TopologyChanged` arriving
    // first and making the test vacuous.
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
///
/// The constructor exists because `ErrBound` became `#[non_exhaustive]`
/// (`docs/API.md` §7's audit), so every caller now goes through it. Both
/// remaining call sites in this repository pass **equal** values —
/// `tests/batch.rs` uses `(1e-3, 1e-3)` and `(0.0, 0.0)` — so a swapped
/// mapping is invisible to them, and `tf_tree_py`'s `at_adaptive`, which does
/// pass `(ang, lin)` distinctly, is outside the workspace and outside
/// `just test`. Verified rather than assumed: with `ErrBound::new` mutated to
/// `ErrBound { rot_rad: trans, trans: rot_rad }`, `cargo nextest run
/// --workspace` reported *754 tests run: 754 passed* before this test existed.
///
/// Two unequal values, so the swap is observable.
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

/// [`Guard`](crate::plan::Guard) packs a per-step search cursor and its edge tag
/// into one `u64`, so the cursor it stores is the low 32 bits of a logical
/// index. `head` is monotone for the life of the arena and never masked, so past
/// 2^32 pushes every stored hint is smaller than `lo_logical` and a plain clamp
/// pins it to the *oldest* retained sample forever. `rebase_hint` lifts it back.
///
/// The window is strictly narrower than 2^32 (`retained = capacity - 1` with
/// `capacity: u32`), so a truncated index has exactly one preimage in it — and
/// the case that needs the correction is the window straddling a multiple of
/// 2^32, where that preimage lies in the block *below* `newest`'s.
///
/// **Mutant:** return `lifted` unconditionally, deleting the `lifted > newest`
/// arm. Applied — see the assertion message: the straddling case then answers
/// `3 * 2^32 - 4`, which is 2^32 past the newest sample the ring holds.
#[test]
fn a_truncated_cursor_is_lifted_back_onto_the_live_window() {
    use crate::sample::rebase_hint;

    // Below 2^32 nothing is truncated and nothing may change: a hint at or above
    // the window is returned as-is, and one below it stays below so the caller's
    // clamp still pins it to `lo_logical` exactly as before this existed.
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

    // A window wholly inside one block above 2^32: the true index is recovered
    // by restoring `newest`'s block base.
    let (lo, newest) = (CURSOR_BLOCK + 10, CURSOR_BLOCK + 20);
    let truth = CURSOR_BLOCK + 15;
    assert_eq!(
        rebase_hint(truth & (CURSOR_BLOCK - 1), lo, newest),
        truth,
        "a hint truncated inside one block is lifted back into it"
    );

    // A window straddling a multiple of 2^32. The true index is in the block
    // *below* `newest`'s, so restoring `newest`'s base overshoots by exactly one
    // block and the correction subtracts it.
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

/// The behavioural half: a ring whose monotone head has passed 2^32 must answer
/// a hinted sample exactly as the unhinted search does, from the truncated
/// cursor a `Guard` would actually have stored.
///
/// This is what `rebase_hint` protects — before it, the hint was pinned to the
/// oldest retained sample on every call and the resumed gallop walked the whole
/// window from the far end. The answer was right either way, which is why no
/// existing test could see it; what this pins is that making the hint *usable*
/// again did not make any answer wrong.
///
/// **Mutant:** in `rebase_hint`, return `lo_logical` instead of the lift. The
/// results still match (a hint can never change a result), which is the point of
/// the unit test above — this test guards the other direction.
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
/// [`Domain`](crate::plan::Domain) is an open trait and a foreign binding
/// therefore cannot dispatch to `at::<D>` at all
/// (`docs/decisions/0038-the-domain-a-binding-cannot-name.md`).
///
/// Every C, C++ and Python query site hardcoded `Stamp::<SystemDomain>` before
/// this existed, so an arena whose edges carry any other tag — which
/// `ros/tf_tree_ros/src/bridge_node.cpp` actively tells an operator to configure
/// under `use_sim_time` — was unreadable from those three languages by
/// construction.
///
/// **Mutant:** make `at_tagged` pass `self.domain` instead of its `domain`
/// argument, i.e. let the binding inherit the plan's own tag. Applied: the four
/// mismatch assertions below fail with `Ok(..)`, because every wrong tag is then
/// silently correct — which is the one-line "fix" `0038`'s Rationale rejects.
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

    // A user-declared tag — the case that motivates the whole surface, since no
    // binding can name a type it has never seen — is refused by tag, not by
    // failing to compile.
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

/// `ExtrapPolicy`'s three variants were all implemented, all tested at the
/// sampler, and reachable from no shipped surface: every fold site passed the
/// `Error` literal and the facade did not re-export the type
/// (`docs/decisions/0039-extrapolation-you-cannot-fail-to-notice.md`).
/// `Plan::at_extrapolating` is what reaches them.
///
/// **The third assertion is the one that earns the test.** Asserting that `Hold`
/// and `ConstantTwist` each return *something* would pass against a
/// `fold_at_policy` that ignored its argument and served `Hold` for both. What
/// distinguishes them is that a moving body's held pose and its
/// constant-twist extension are different poses at the same stamp, and only the
/// second keeps moving.
///
/// **Mutant:** `fold_at_policy` passes `ExtrapPolicy::Hold` instead of its
/// `policy` argument. Applied: the run stops at the *first* `at_extrapolating`
/// assertion — `ExtrapPolicy::Error` returns the held pose instead of refusing —
/// so the `ConstantTwist` assertion at the end is never reached under this
/// mutant. It is reached, and needed, under the narrower mutant of passing
/// **`by_ns == 0` must mean the fold bracketed, and the *order* of two walks is
/// the whole of that guarantee.**
///
/// `Extrapolated`'s doc says `0` means "every edge bracketed the query — the
/// answer is interpolated, not invented". `at_extrapolating_tagged` used to fold
/// the pose and *then* walk `newest_common`. A `push` landing between the two,
/// with a stamp at or past the query, lifts `common` to `>= nanos` — so the
/// distance reads `0` for a pose that was invented, which is the one claim this
/// type exists to make unmissable. Measuring first inverts the error into the
/// safe direction, because `SampleRing::newest_stamp` is non-decreasing.
///
/// **The pose is the witness, and it has to be**: `by_ns` alone cannot tell an
/// honest `0` (data really did arrive before the walk) from a dishonest one. One
/// dynamic edge, translation `x == stamp / STEP` exactly, and the query at a
/// half-step. A *held* answer is then integral in `x`; an *interpolated* one is
/// `k + 0.5`. So `by_ns == 0` with an integral `x` is the defect, caught
/// whichever thread won.
///
/// **Mutant, run:** move `newest_common` back below the fold — the shipped order
/// until this test existed. 3 runs of 3 fail, each in about 10 ms; with the walk
/// first, 6 runs of 6 pass the full 200 000 iterations. So it discriminates, and
/// quickly.
///
/// **The first version of this harness did not, and the note here claimed it
/// did.** The writer ran a fixed count, finished in microseconds, and the reader
/// then queried a frontier nothing was moving — every answer honestly
/// interpolated, mutant green. The writer now runs *until stopped* and the
/// reader owns the iteration count, and `t` is read from the frontier
/// immediately before the call rather than from a lagging value. A stress test
/// whose race cannot occur is indistinguishable from a passing one, which is why
/// the mutant is run rather than reasoned about.
///
/// Probabilistic even so: the window is one fold. The monotonicity argument at
/// the call site is what carries the guarantee; this is what would notice the
/// order being changed back.
/// **Ignored under Miri**, which is not a gap in coverage: the harness runs
/// 200 000 rounds against a writer thread that spins until told to stop, and
/// Miri interprets every atomic. It does not finish — measured at 28 minutes and
/// still running against a job that normally takes five, which is how it was
/// found. The property this guards is an *ordering* one and Miri is not the tool
/// for it; `just loom` is where interleavings are argued.
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
        // The `Publisher` is built **inside** the writer: D7 makes it `!Send`,
        // which is one writer per edge enforced in the type system and is why
        // this cannot be hoisted out of the closure.
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
            // first leaves the reader querying a frontier nothing is moving,
            // which is the state in which this race cannot happen at all.
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
            // Read the frontier **immediately** before the call, so `t` is half
            // a step past what exists right now and the writer's very next push
            // crosses it. A lagging `k` puts the writer thousands of steps ahead
            // and every answer is honestly interpolated.
            let k = published.load(O::Acquire);
            if k < 0 {
                continue; // the writer has not published its first sample yet
            }
            // Half a step past the newest sample this reader has seen: past it,
            // and short of the next one the writer will publish.
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

/// `Hold` only where `ConstantTwist` was asked for.
#[test]
fn extrapolation_is_selectable_and_reports_how_far_it_reached() {
    let arena = rate_chain_arena([0, 0, 0, 0]);
    let view = ArenaView::new(&arena);
    seed_rate_chain(&view);
    let (root, leaf) = (view.intern("f0").unwrap(), view.intern("f4").unwrap());
    let plan = compile_chain(&view, root, leaf);
    let g = Guard::new(ArenaView::new(&arena));

    // `latest_common` is the newest stamp every dynamic edge can answer for, so
    // it is the point past which any answer is invented.
    let common = plan.newest_common_for_test(&g).unwrap().unwrap().0;
    let past = common + 5_000_000; // 5 ms beyond it

    // The default refuses, and refusing stays the default: `Plan::at` is
    // untouched by this surface.
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

    // Inside the window, every policy agrees with `at` and reports zero
    // distance — the control that stops the assertions below passing against a
    // plan that extrapolated everything.
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

    // ...and `ConstantTwist` really extends. This is the assertion a
    // policy-ignoring fold cannot satisfy.
    assert_ne!(
        extended.pose, held.pose,
        "ConstantTwist returned the held pose, so the policy was ignored"
    );
}

// ---- errors that compose (decision 0040) --------------------------------

/// Every variant of every error enum renders as prose and names what it carries.
///
/// The point is coverage rather than wording: `docs/API.md` R5 is NORMATIVE that
/// message *text* is not a compatibility promise, so this asserts that a message
/// exists, that it is not the `Debug` spelling, and that the identifier the
/// variant carries appears in it — never that a particular sentence does.
///
/// **The match in `error.rs` is exhaustive, so a variant added later fails to
/// compile there rather than falling into a generic arm.** This test is the
/// other half: it pins that each arm actually *says* something, which a
/// compiling `write!(f, "")` would not.
///
/// **Mutant:** make `LookupError::WrongElementType`'s arm `write!(f, "")`.
/// Applied: `every LookupError variant renders: WrongElementType produced
/// nothing` — the case a catch-all `Debug` arm would have hidden, since `Debug`
/// is never empty.
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
///
/// `Tree::await_frames`' example was published as a ```text``` block, and its
/// comment gave the reason: *"the three calls yield `OpenError`, `AwaitError` and
/// `LookupError`, and `LookupError` implements neither `Display` nor `Error`, so
/// no single `?`-chain unifies them — not even into `Box<dyn Error>`."*
///
/// So this is that `?`-chain, compiled. It is the acceptance test for
/// `docs/decisions/0040`: not that a message is nice, but that an error can
/// *leave a function* the way every other Rust library's can.
///
/// **Mutant:** delete `impl core::error::Error for LookupError`. Applied: does
/// not compile — `the trait bound LookupError: core::error::Error is not
/// satisfied`, on the `?`. A compile failure is the strongest form this
/// assertion can take, which is why the test is shaped as a function that must
/// type-check rather than as an assertion about a value.
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
///
/// A plain `nanos - common` panics in a checked build — on the wait-free read
/// path — and wraps in a release one. Wrapped negative it clamps to `0`, which
/// reports *"not extrapolated"* for the most extrapolated answer the type can
/// hold: the exact confusion `Extrapolated` exists to prevent, arrived at from
/// the other side. `sample::span_ns` carries the same argument for the same
/// reason one layer down.
///
/// **Mutant:** restore `(nanos - common).max(0)`. Applied: the release build
/// asserts with `left: 0, right: 9223372036854775807`, and the debug build
/// panics with `attempt to subtract with overflow` inside `Plan::at_extrapolating`.
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
///
/// `Iso3` was `#[repr(C, align(64))]` with an 8-byte pad, on the stated grounds
/// that *"the Phase 2 shared-memory arena can store slots without re-deriving
/// layout"*. The arena re-derived it anyway — `buffer::PoseSlot` is its own
/// `align(64)` of atomics, because the seqlock payload has to be atomics to be
/// sound — and no arena structure has ever had an `Iso3` field. So the alignment
/// bought the arena nothing and cost every in-memory use eight bytes and a
/// 64-byte stride.
///
/// These are *figures*, not invariants: `MAX_DEPTH` moving would move `Plan`, and
/// that is expected. What this catches is the figures moving for a reason nobody
/// intended — a field added to `Iso3`, or an alignment attribute coming back —
/// which is silent otherwise and doubles a per-thread cache when it happens.
///
/// **Mutant:** add an 8-byte field to `Iso3` and fix its two constructors — the
/// realistic way this drifts back. Applied: fails here with `left: 64, right:
/// 56`, and `Plan` doubles behind it.
///
/// **Restoring `align(64)` is not the mutant to use, and finding that out is
/// worth recording**: it does not compile either way. With the `_pad` back, the
/// constructors no longer initialise every field; without it, `align(64)` over
/// 56 bytes of fields leaves trailing padding and the `Pod` derive refuses. So
/// the alignment cannot come back silently at all — which is a stronger
/// guarantee than this test, and not one this test provides.
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
    // The property the `Pod` derive rests on, which the pad used to guarantee
    // for free: seven f64 and nothing else.
    assert_eq!(size_of::<Iso3>(), 7 * size_of::<f64>());
}
