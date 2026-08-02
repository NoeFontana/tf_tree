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
/// including from the `#![forbid(unsafe_code)]` facade. They must report the
/// miss, not form an out-of-bounds pointer.
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

/// `docs/PHASE1.md` §7.1 pins `MAX_DEPTH` at 16 and makes a longer combined
/// path [`LookupError::TreeTooDeep`]. Nothing in the workspace asserted it, so
/// the four depth guards in `compile` were free to be off by one — and the
/// failure is not a clean panic but a *truncated plan*: a transform composed
/// from the bottom of the chain and missing its top, which looks entirely
/// plausible.
///
/// Mutant: `if nt >= MAX_DEPTH` -> `if nt > MAX_DEPTH` in the first
/// depth-equalisation loop ⇒ `t_edges[16]` panics out of bounds.
#[test]
fn max_depth_is_the_exact_boundary_between_a_plan_and_tree_too_deep() {
    use alloc::format;

    // 20 frame slots => ids 1..=19: a root plus an 18-link chain.
    let layout = ArenaLayout::new(20, 20, alloc::vec![0; 20]).unwrap();
    let arena = HeapArena::new(&layout, 4242, 0, [0u8; 16]);
    let view = ArenaView::new(&arena);

    let mut chain: Vec<FrameId> = Vec::new();
    for i in 0..19u32 {
        chain.push(view.intern(&format!("f{i}")).unwrap());
    }
    for w in chain.windows(2) {
        // Edge id == the child's frame id: non-zero, in range, and unique, so
        // no link trips the `MissingEdge` sentinel check instead.
        view.topology()
            .set_parent(w[1], w[0].get(), w[1].get())
            .unwrap();
    }

    let meta = |eid: EdgeId| {
        view.edge(eid).map(|e| crate::plan::EdgeMeta {
            kind: crate::edge::EdgeKind::from_u8(e.kind),
            domain: e.domain,
            static_pose: Iso3::from_bits(&e.static_pose),
        })
    };

    // Exactly MAX_DEPTH links: compiles, with every step retained.
    let root = chain[0];
    let at_limit = chain[crate::MAX_DEPTH];
    let plan = crate::plan::compile(&view.topology(), meta, at_limit, root).unwrap();
    assert_eq!(
        plan.len(),
        crate::MAX_DEPTH,
        "a depth-16 path must compile whole"
    );

    // One link further: refused, and the error names the depth that overflowed.
    let past_limit = chain[crate::MAX_DEPTH + 1];
    assert_eq!(
        crate::plan::compile(&view.topology(), meta, past_limit, root).unwrap_err(),
        LookupError::TreeTooDeep {
            depth: crate::MAX_DEPTH as u16
        }
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
/// Nothing is ever published into these rings: a *declared* rate is a property
/// of the topology, and reading it must not depend on a stream existing.
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

/// **The four built-in domain tags are `0`–`3`, distinct, and in that order.**
///
/// A tag is written into `EdgeRecord::domain` at declaration time and read by
/// every consumer, every recording and every diagnostic, so re-numbering one
/// silently re-interprets arenas already on disk — `docs/API.md` §5.2's
/// "unfixable after the fact" applied to the numbering rather than to the
/// choice. This test exists to make that re-numbering a red build.
///
/// The distinctness half is not redundant with the values half: `Domain::TAG`
/// is a per-impl constant with nothing structural preventing two domains from
/// sharing one, and two domains sharing a tag is exactly the collapse
/// `docs/API.md` §2.5 describes — `TimeDomainMismatch` stops firing between
/// them and nothing else changes.
///
/// Mutant: `SimDomain::TAG = 0` (the value it effectively had before it was a
/// type). Applied: the `SystemDomain`/`SimDomain` distinctness assertion fails,
/// and so does `a_sim_stamp_cannot_query_a_system_domain_plan` below.
#[test]
fn the_built_in_domain_tags_are_fixed_and_distinct() {
    use crate::plan::{Domain, SensorDomain, SimDomain, SteadyDomain, SystemDomain};

    assert_eq!(SystemDomain::TAG, 0, "the default domain must stay tag 0");
    assert_eq!(SensorDomain::TAG, 1);
    assert_eq!(SimDomain::TAG, 2);
    assert_eq!(SteadyDomain::TAG, 3);

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
/// The control below is the same query in the plan's own domain — otherwise
/// this would pass equally well against a plan that refused everything.
///
/// Mutant: `SimDomain::TAG = 0`. Applied: the first assertion fails with
/// `Ok(..)`, which is the silent wrong answer this whole mechanism exists to
/// prevent.
#[test]
fn a_sim_stamp_cannot_query_a_system_domain_plan() {
    use crate::plan::{SimDomain, SteadyDomain};

    let arena = rate_chain_arena([0, 0, 0, 0]);
    let view = ArenaView::new(&arena);
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

    // Control: the plan's own domain gets past the check and fails for a
    // reason about *data*, not about time domains.
    assert_eq!(
        plan.at(&g, Stamp::<SystemDomain>::from_nanos(1)),
        Err(LookupError::NoData { edge: EdgeId(1) })
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
