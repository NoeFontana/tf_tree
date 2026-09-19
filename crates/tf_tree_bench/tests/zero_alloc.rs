//! The zero-allocation gate (`docs/PHASE1.md` §10.4 *Allocation*, invariant 8 in §2).
//!
//! A `CountingAllocator` tallies every `alloc`/`realloc`; after construction the
//! tally must not move across the measured windows. Host-independent: a hard gate.
//!
//! Later tests cover the rest of `docs/API.md` §8.1's list: the batch forms in every
//! layout, `at_with_derivatives`, `at_extrapolating` under each policy, and a stale
//! plan's `TopologyChanged` refusal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]
// `docs/decisions/0007` rule 1, kind 6 (`0048`): `unsafe impl GlobalAlloc for CountingAllocator`.
// Declared here because a test is a separate crate root.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use tf_tree::Stamp;
use tf_tree_bench::fixture;

thread_local! {
    /// Allocating calls (`alloc` + `realloc`) made **by this thread**.
    ///
    /// Thread-local because the other tests' construction phases run on other threads
    /// and would land inside a process-global window. `const { Cell::new(0) }` is
    /// required: lazy init inside the global allocator would recurse.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

/// This thread's allocating-call count; `try_with` because the local may be destroyed at teardown.
fn allocations() -> usize {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

/// Record one allocating call on this thread, if the local is still live.
fn note_allocation() {
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get().wrapping_add(1)));
}

/// A `System`-backed allocator that counts allocating calls (not deallocations).
struct CountingAllocator;

// SAFETY: forwards every call unchanged to `System`, a sound `GlobalAlloc`; the only added
// work is a thread-local counter bump via `try_with`, which cannot affect returned
// pointers or panic out of the allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        // SAFETY: forwarding an unmodified `layout` to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr`/`layout` came from `System.alloc`; deallocation is not counted.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation();
        // SAFETY: `ptr`/`layout` originate from `System`; `new_size` is passed unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn no_allocations_after_construction() {
    let tree = fixture::build_tree().expect("build fixture");
    let (writers, _samples) = fixture::spin_up(&tree).expect("populate history");

    // The lidar publisher is off the imu_link <- map query path.
    let lidar = &writers[3];

    let target = tree.frame("imu_link").expect("imu frame");
    let source = tree.frame("map").expect("map frame");
    let plan = tree.plan(target, source).expect("compile plan");
    let guard = tree.guard();
    let query: Stamp = Stamp::from_nanos(fixture::NOW_NS);

    let iso = fixture::dynamic_pose(3.0, 0);
    // Stamps stay strictly monotone above the populated history; the ring wraps.
    let mut push_stamp: i64 = 20_000_000_000;

    // Warm once so any first-touch init is outside the window.
    plan.at(&guard, query).expect("warm at");
    lidar.push(push_stamp, &iso).expect("warm push");
    push_stamp += 1_000_000;

    const ITERS: usize = 1_000_000;

    let before = allocations();
    let mut acc = 0.0f64;
    for _ in 0..ITERS {
        lidar.push(push_stamp, &iso).expect("push");
        push_stamp += 1_000_000;
        let pose = plan.at(&guard, query).expect("at");
        acc += pose.t.x;
    }
    let after = allocations();

    assert!(acc.is_finite(), "accumulator went non-finite: {acc}");

    let allocations = after - before;
    assert_eq!(
        allocations, 0,
        "expected zero allocations across {ITERS} push+at calls, saw {allocations}"
    );

    let _ = &iso;
    drop(writers);
}

/// The same gate over the *large* `fleet_64` topology (1537 frames, a four-dynamic-step plan).
///
/// Pushes land on an edge the plan reads, so the retained window slides under a fixed
/// query stamp and the bracket search runs against a wrapped ring. Not reached: the
/// seqlock retry in `SampleRing::read_slot`, which needs a concurrent writer; its
/// freedom from allocation is read from the code.
#[test]
fn no_allocations_on_a_large_topology_across_ring_wraparound() {
    use tf_tree::InterpPolicy;
    use tf_tree_bench::workload::{self, Backing};

    // `fleet_64`: 256 dynamic edges, a cross-fleet plan of four dynamic steps.
    let w = workload::by_name("fleet_64").expect("fleet_64 in the catalogue");
    let built = w
        .build(InterpPolicy::LerpSlerp, Backing::Heap)
        .expect("build fleet_64");
    assert_eq!(
        built.shape.dyn_steps,
        Some(4),
        "this test is about a multi-step plan; the catalogue changed under it"
    );

    let plan = built.plans().expect("compile")[0];
    let guard = built.tree.guard();
    let query: Stamp = Stamp::from_nanos(built.stamp_at(0.5));

    // Publish onto an edge the plan reads (robot 0's `map->odom`).
    let p = &built.publishers[0];
    let parent = built.tree.frame(&p.parent).expect("parent frame");
    let child = built.tree.frame(&p.child).expect("child frame");
    let writer = built.tree.claim(child, parent).expect("claim");

    let iso = fixture::dynamic_pose(p.seed, 0);
    let step_ns = (1e9 / p.rate_hz) as i64;
    let mut push_stamp = p.next_stamp_ns;

    plan.at(&guard, query).expect("warm at");
    writer.push(push_stamp, &iso).expect("warm push");
    push_stamp += step_ns;

    // 10 s at 50 Hz is 512 slots, so this laps roughly 390 times.
    const ITERS: usize = 200_000;

    let before = allocations();
    let mut acc = 0.0f64;
    let mut answered = 0usize;
    for _ in 0..ITERS {
        writer.push(push_stamp, &iso).expect("push");
        push_stamp += step_ns;
        // The query stamp is fixed while the window slides past it; both the answer and the
        // refusal must stay allocation-free.
        if let Ok(pose) = plan.at(&guard, query) {
            acc += pose.t.x;
            answered += 1;
        }
    }
    let after = allocations();

    assert!(acc.is_finite(), "accumulator went non-finite: {acc}");
    assert!(
        answered > 0,
        "every lookup was declined, so the success path was never measured"
    );
    assert!(
        answered < ITERS,
        "no lookup was declined, so the error path was never measured — this test \
         is supposed to cross the window's edge"
    );

    let allocations = after - before;
    assert_eq!(
        allocations,
        0,
        "expected zero allocations across {ITERS} push+at calls on fleet_64 \
         ({} answered, {} declined), saw {allocations}",
        answered,
        ITERS - answered
    );

    let _ = &iso;
    drop(writer);
}

/// Stamps per batch call.
const BATCH: usize = 16;

/// Push + evaluate rounds per window: enough to lap the 512-slot ring about six times; more
/// would only repeat element-0 refusals, since a batch stops at its first declined stamp.
const WINDOW_ROUNDS: usize = 3000;

/// How one evaluation inside a measured window came back.
enum Outcome {
    /// `Ok`, and not extrapolated.
    Answered,
    /// `Ok` from `at_extrapolating` with `by_ns > 0`: the extrapolation arm ran.
    Extrapolated,
    /// `Err` of any kind.
    Declined,
}

/// Everything a window's calls read or write, built **before** the window.
struct Buffers {
    mono: Vec<i64>,
    nonmono: Vec<i64>,
    mono_stamps: Vec<Stamp>,
    nonmono_stamps: Vec<Stamp>,
    out_iso: Vec<tf_tree::Iso3>,
    /// Sized for the widest `f64` layout (`Mat4`, 16).
    out_f64: Vec<f64>,
    /// `Affine32`'s 12 per stamp.
    out_f32: Vec<f32>,
    /// Half-way through the populated window.
    mid: Stamp,
    /// Two seconds past the populated window's newest stamp, so `Hold` and
    /// `ConstantTwist` reach their extrapolation arms rather than bracketing.
    ahead: Stamp,
}

/// One measured window's tally.
struct Window {
    label: &'static str,
    allocations: usize,
    answered: usize,
    extrapolated: usize,
    declined: usize,
    /// Whether an `Ok` with `by_ns > 0` is required rather than an interpolated one.
    wants_extrapolated: bool,
}

/// Build `fleet_64` afresh (a shared tree would leave only element-0 refusals), publish
/// onto an edge the plan reads, and count this thread's allocations across
/// [`WINDOW_ROUNDS`] push + `eval` rounds.
fn measure(
    policy: tf_tree::InterpPolicy,
    label: &'static str,
    wants_extrapolated: bool,
    mut eval: impl FnMut(&tf_tree::Plan, &tf_tree::Guard, &mut Buffers) -> Outcome,
) -> Window {
    use tf_tree_bench::workload::{self, Backing};

    let w = workload::by_name("fleet_64").expect("fleet_64 in the catalogue");
    let built = w.build(policy, Backing::Heap).expect("build fleet_64");
    let plan = built.plans().expect("compile")[0];
    let guard = built.tree.guard();

    let p = &built.publishers[0];
    let parent = built.tree.frame(&p.parent).expect("parent frame");
    let child = built.tree.frame(&p.child).expect("child frame");
    let writer = built.tree.claim(child, parent).expect("claim");
    let iso = fixture::dynamic_pose(p.seed, 0);
    let step_ns = (1e9 / p.rate_hz) as i64;
    let mut push_stamp = p.next_stamp_ns;

    let (lo, hi) = (built.stamp_at(0.4), built.stamp_at(0.6));
    let mono: Vec<i64> = (0..BATCH)
        .map(|i| lo + (hi - lo) * i as i64 / BATCH as i64)
        .collect();
    // One transposition sends the batch down the cursor-free branch.
    let mut nonmono = mono.clone();
    nonmono.swap(0, BATCH - 1);
    let mut bufs = Buffers {
        mono_stamps: mono.iter().map(|&n| Stamp::from_nanos(n)).collect(),
        nonmono_stamps: nonmono.iter().map(|&n| Stamp::from_nanos(n)).collect(),
        mono,
        nonmono,
        out_iso: vec![tf_tree::Iso3::IDENTITY; BATCH],
        out_f64: vec![0.0; BATCH * 16],
        out_f32: vec![0.0; BATCH * 12],
        mid: Stamp::from_nanos(built.stamp_at(0.5)),
        ahead: Stamp::from_nanos(built.window.1 + 2_000_000_000),
    };

    let _ = eval(&plan, &guard, &mut bufs);
    writer.push(push_stamp, &iso).expect("warm push");
    push_stamp += step_ns;

    let (mut answered, mut extrapolated, mut declined) = (0, 0, 0);
    let before = allocations();
    for _ in 0..WINDOW_ROUNDS {
        writer.push(push_stamp, &iso).expect("push");
        push_stamp += step_ns;
        match eval(&plan, &guard, &mut bufs) {
            Outcome::Answered => answered += 1,
            Outcome::Extrapolated => extrapolated += 1,
            Outcome::Declined => declined += 1,
        }
    }
    let after = allocations();

    drop(writer);
    // Under `--no-capture`: the counts show each window reached both sides of the path.
    eprintln!(
        "{label:<36} allocations={} answered={answered} extrapolated={extrapolated} \
         declined={declined}",
        after - before
    );
    Window {
        label,
        allocations: after - before,
        answered,
        extrapolated,
        declined,
        wants_extrapolated,
    }
}

/// Assert every window at once, so a failure names each window that went wrong.
fn assert_windows(windows: &[Window]) {
    let mut bad = Vec::new();
    for w in windows {
        let reached = if w.wants_extrapolated {
            w.extrapolated
        } else {
            w.answered
        };
        if w.allocations != 0 || reached == 0 || w.declined == 0 {
            bad.push(format!(
                "  {:<34} allocations={} answered={} extrapolated={} declined={}{}",
                w.label,
                w.allocations,
                w.answered,
                w.extrapolated,
                w.declined,
                if reached == 0 {
                    if w.wants_extrapolated {
                        "  <- no extrapolated answer, so the extrapolation arm was never measured"
                    } else {
                        "  <- nothing answered, so the success path was never measured"
                    }
                } else if w.declined == 0 {
                    "  <- nothing declined, so the refusal path was never measured"
                } else {
                    ""
                },
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "expected zero allocations, an answer and a refusal in every window of \
         {WINDOW_ROUNDS} rounds on fleet_64:\n{}",
        bad.join("\n")
    );
}

fn batch_outcome(r: Result<(), tf_tree::LookupError>) -> Outcome {
    if r.is_ok() {
        Outcome::Answered
    } else {
        Outcome::Declined
    }
}

/// `at_many`, `at_many_into` (`Mat4`, `Quat`) and `at_many_into_f32` (`Affine32`) allocate
/// nothing, over both the monotone batch and the non-monotone one.
///
/// Every window also asserts some call answered and some was refused, so neither half
/// can pass by never running.
#[test]
fn batch_forms_allocate_nothing_monotone_or_not() {
    use tf_tree::{InterpPolicy::LerpSlerp, Layout as Out, SystemDomain as Sys};

    let windows = [
        measure(LerpSlerp, "at_many monotone", false, |p, g, b| {
            batch_outcome(p.at_many(g, &b.mono_stamps, &mut b.out_iso))
        }),
        measure(LerpSlerp, "at_many non-monotone", false, |p, g, b| {
            batch_outcome(p.at_many(g, &b.nonmono_stamps, &mut b.out_iso))
        }),
        measure(LerpSlerp, "at_many_into Mat4 monotone", false, |p, g, b| {
            batch_outcome(p.at_many_into::<Sys>(g, &b.mono, Out::Mat4, &mut b.out_f64))
        }),
        measure(
            LerpSlerp,
            "at_many_into Mat4 non-monotone",
            false,
            |p, g, b| {
                batch_outcome(p.at_many_into::<Sys>(g, &b.nonmono, Out::Mat4, &mut b.out_f64))
            },
        ),
        measure(LerpSlerp, "at_many_into Quat monotone", false, |p, g, b| {
            batch_outcome(p.at_many_into::<Sys>(g, &b.mono, Out::Quat, &mut b.out_f64))
        }),
        measure(
            LerpSlerp,
            "at_many_into Quat non-monotone",
            false,
            |p, g, b| {
                batch_outcome(p.at_many_into::<Sys>(g, &b.nonmono, Out::Quat, &mut b.out_f64))
            },
        ),
        measure(
            LerpSlerp,
            "at_many_into_f32 Affine32 monotone",
            false,
            |p, g, b| {
                batch_outcome(p.at_many_into_f32::<Sys>(g, &b.mono, Out::Affine32, &mut b.out_f32))
            },
        ),
        measure(
            LerpSlerp,
            "at_many_into_f32 Affine32 non-mono",
            false,
            |p, g, b| {
                batch_outcome(p.at_many_into_f32::<Sys>(
                    g,
                    &b.nonmono,
                    Out::Affine32,
                    &mut b.out_f32,
                ))
            },
        ),
    ];
    assert_windows(&windows);
}

/// `at_with_derivatives` and `at_many_into(Layout::QuatTwist)` allocate nothing.
///
/// On an `ScLerp` build: both refuse a `LerpSlerp` edge before sampling, so a zero
/// count there would prove nothing; `answered > 0` guards that.
#[test]
fn derivative_forms_allocate_nothing_on_an_sclerp_build() {
    use tf_tree::{InterpPolicy::ScLerp, Layout as Out, SystemDomain as Sys};

    let windows = [
        measure(
            ScLerp,
            "at_many_into QuatTwist monotone",
            false,
            |p, g, b| {
                batch_outcome(p.at_many_into::<Sys>(g, &b.mono, Out::QuatTwist, &mut b.out_f64))
            },
        ),
        measure(
            ScLerp,
            "at_many_into QuatTwist non-mono",
            false,
            |p, g, b| {
                batch_outcome(p.at_many_into::<Sys>(g, &b.nonmono, Out::QuatTwist, &mut b.out_f64))
            },
        ),
        measure(ScLerp, "at_with_derivatives", false, |p, g, b| {
            match p.at_with_derivatives(g, b.mid) {
                Ok(_) => Outcome::Answered,
                Err(_) => Outcome::Declined,
            }
        }),
    ];
    assert_windows(&windows);
}

/// `at_extrapolating` allocates nothing under each `ExtrapPolicy`.
///
/// `Hold` and `ConstantTwist` are queried two seconds past the newest sample, so their
/// extrapolation arms run; those windows require an answer with `by_ns > 0`.
#[test]
fn extrapolating_allocates_nothing_under_every_policy() {
    use tf_tree::{ExtrapPolicy, InterpPolicy::ScLerp};

    fn outcome(r: Result<tf_tree::Extrapolated, tf_tree::LookupError>) -> Outcome {
        match r {
            Ok(e) if e.by_ns > 0 => Outcome::Extrapolated,
            Ok(_) => Outcome::Answered,
            Err(_) => Outcome::Declined,
        }
    }

    let windows = [
        measure(ScLerp, "at_extrapolating Error", false, |p, g, b| {
            outcome(p.at_extrapolating(g, b.mid, ExtrapPolicy::Error))
        }),
        measure(ScLerp, "at_extrapolating Hold", true, |p, g, b| {
            outcome(p.at_extrapolating(g, b.ahead, ExtrapPolicy::Hold))
        }),
        measure(ScLerp, "at_extrapolating ConstantTwist", true, |p, g, b| {
            outcome(p.at_extrapolating(g, b.ahead, ExtrapPolicy::ConstantTwist))
        }),
    ];
    assert_windows(&windows);
}

/// A **stale plan's refusal** allocates nothing either (`docs/API.md` §8.1).
///
/// The ordering is the test: a `Guard` pins the topology generation when built, so the
/// window's guard is taken *after* the reparent; the first assertion after it pins
/// that. Positive controls: the plan answers before the reparent, and a re-planned one
/// answers under the window's guard.
#[test]
fn a_stale_plan_refusal_allocates_nothing() {
    use tf_tree::LookupError;

    let tree = fixture::build_tree().expect("build fixture");
    let (writers, _samples) = fixture::spin_up(&tree).expect("populate history");

    let target = tree.frame("imu_link").expect("imu frame");
    let source = tree.frame("map").expect("map frame");
    let plan = tree.plan(target, source).expect("compile plan");
    let query: Stamp = Stamp::from_nanos(fixture::NOW_NS);

    let before_reparent = tree.guard();
    plan.at(&before_reparent, query)
        .expect("positive control: a current plan answers");

    // Move an edge *off* the query path: only the generation moves.
    let lidar = tree.frame("lidar").expect("lidar frame");
    let base_link = tree.frame("base_link").expect("base_link frame");
    tree.reparent(lidar, base_link).expect("reparent lidar");

    assert!(
        plan.at(&before_reparent, query).is_ok(),
        "a guard built before the reparent pinned the old generation and should \
         still answer — if this changed, the window's ordering below needs \
         re-deriving"
    );
    drop(before_reparent);

    let guard = tree.guard();
    let _ = plan.at(&guard, query);

    const ITERS: usize = 1_000_000;

    let mut refused = 0usize;
    let before = allocations();
    for _ in 0..ITERS {
        match plan.at(&guard, query) {
            Err(LookupError::TopologyChanged { .. }) => refused += 1,
            other => panic!(
                "a stale plan under a guard built after the reparent must refuse, got {other:?}"
            ),
        }
    }
    let after = allocations();

    assert_eq!(refused, ITERS);
    let allocations = after - before;
    assert_eq!(
        allocations, 0,
        "expected zero allocations across {ITERS} stale-plan refusals, saw {allocations}"
    );

    let fresh = tree.plan(target, source).expect("re-plan");
    fresh
        .at(&guard, query)
        .expect("positive control: a re-compiled plan answers under the same guard");

    drop(guard);
    drop(writers);
}
