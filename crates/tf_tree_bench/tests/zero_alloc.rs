//! The zero-allocation gate (`docs/PHASE1.md` §10.4 *Allocation* and
//! load-bearing invariant 8 in §2: "every heap allocation happens at
//! construction").
//!
//! A `CountingAllocator` wraps the system allocator and tallies every
//! `alloc`/`realloc`. After the tree is built and its history populated (all
//! allocation is allowed there), we snapshot the counter and run **>= 1e6**
//! `push` + `at` calls; the tally must not move. This is a hard correctness gate
//! and it runs on any machine — unlike the perf gate, it needs no special
//! hardware.
//!
//! `push` targets the lidar edge and `at` evaluates the `imu_link <- map` plan;
//! the two touch disjoint edges, so the pushes never slide the queried edges'
//! windows out from under the fixed query stamp.
//!
//! The later tests carry the same counter over the rest of `docs/API.md`
//! §8.1's list — the batch forms in every layout, `at_with_derivatives`,
//! `at_extrapolating` under each policy — and over a stale plan's
//! `TopologyChanged` refusal. Each one records the mutant it was run against.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]
// **`docs/decisions/0007` rule 1, kind 6 — a trait the language requires be
// implemented unsafely, in a target that never ships** (`docs/decisions/0048`:
// a kind is a property, not a crate name). Here that is `unsafe impl GlobalAlloc for CountingAllocator`. The posture is
// declared rather than inherited, because a test is a **separate crate root**.
// `0048` step 4.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use tf_tree::Stamp;
use tf_tree_bench::fixture;

thread_local! {
    /// Allocating calls (`alloc` + `realloc`) made **by this thread**.
    ///
    /// Thread-local, and that is load-bearing rather than tidy. `cargo test`
    /// runs this file's two tests on separate threads by default, and the other
    /// test's construction phase legitimately allocates several thousand times.
    /// Against a *process-global* counter those allocations land inside this
    /// test's measured window, so the gate failed by ~4000 on every commit
    /// anyone ran it against — a false failure, which is why it reported `FAIL`
    /// while the engine was in fact allocation-free. `--test-threads=1` also
    /// hides it, so pinning the counter to the thread is the fix that does not
    /// depend on how the runner is invoked.
    ///
    /// `const { Cell::new(0) }` is required, not stylistic: a lazily-initialised
    /// thread-local can allocate on first access, and doing that *inside* the
    /// global allocator would recurse.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

/// This thread's allocating-call count.
///
/// `try_with` rather than `with`: during thread teardown the local may already
/// be destroyed, and a panic from inside the allocator is not recoverable.
fn allocations() -> usize {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

/// Record one allocating call on this thread, if the local is still live.
fn note_allocation() {
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get().wrapping_add(1)));
}

/// A `System`-backed allocator that counts allocating calls. Deallocations are
/// not counted — the gate asserts that no *new* allocation happens in the hot
/// loop, which is what invariant 8 requires.
struct CountingAllocator;

// SAFETY: `CountingAllocator` forwards every call unchanged to `System`, which is
// a sound `GlobalAlloc`. The only added work is a thread-local counter bump on
// the allocating paths, which cannot affect the returned pointers or their
// validity, and which uses `try_with` so a destroyed local cannot panic out of
// the allocator. This impl therefore upholds every `GlobalAlloc` invariant that
// `System` upholds. (Test-only binary; the crate proper is `#![forbid(unsafe_code)]`.)
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        // SAFETY: forwarding an unmodified `layout` to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr`/`layout` came from `System.alloc` (this allocator only
        // ever forwards to `System`), so returning them to `System.dealloc` is
        // sound. Deallocation is intentionally not counted.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation();
        // SAFETY: `ptr`/`layout` originate from `System` and `new_size` is passed
        // through unchanged, so this satisfies `System::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn no_allocations_after_construction() {
    // --- construction (allocation permitted) ---------------------------
    let tree = fixture::build_tree().expect("build fixture");
    let (writers, _samples) = fixture::spin_up(&tree).expect("populate history");

    // The lidar publisher (4th dynamic edge) is the push target; it is not on the
    // imu_link <- map query path, so pushing to it never invalidates the query.
    let lidar = &writers[3];

    let target = tree.frame("imu_link").expect("imu frame");
    let source = tree.frame("map").expect("map frame");
    let plan = tree.plan(target, source).expect("compile plan");
    let guard = tree.guard();
    let query: Stamp = Stamp::from_nanos(fixture::NOW_NS);

    let iso = fixture::dynamic_pose(3.0, 0);
    // Push stamps continue strictly above the lidar edge's populated history so
    // they stay monotone; the ring simply wraps.
    let mut push_stamp: i64 = 20_000_000_000;

    // Warm once so any first-touch lazy init (there is none expected on this path)
    // is outside the measured window.
    plan.at(&guard, query).expect("warm at");
    lidar.push(push_stamp, &iso).expect("warm push");
    push_stamp += 1_000_000;

    const ITERS: usize = 1_000_000;

    // --- measured window: must not allocate ----------------------------
    let before = allocations();
    let mut acc = 0.0f64;
    for _ in 0..ITERS {
        lidar.push(push_stamp, &iso).expect("push");
        push_stamp += 1_000_000;
        let pose = plan.at(&guard, query).expect("at");
        acc += pose.t.x;
    }
    let after = allocations();

    // Keep `acc` observable so the loop is not optimized away.
    assert!(acc.is_finite(), "accumulator went non-finite: {acc}");

    let allocations = after - before;
    assert_eq!(
        allocations, 0,
        "expected zero allocations across {ITERS} push+at calls, saw {allocations}"
    );

    // `_ = iso` guards against an over-eager drop of the pose we keep reusing.
    let _ = &iso;
    drop(writers);
}

/// The same gate over the *large* topologies the performance suite added.
///
/// The test above proves the hot path allocates nothing on a 24-frame tree with
/// a three-step plan. Neither of those is where an allocation would hide. The
/// things that scale with the workload — the compiled plan's step array, the
/// guard's per-edge bookkeeping, the bracket search's state — are all
/// fixed-size by design (`Plan` is `[Step; MAX_DEPTH]` and `Copy`, invariant 8;
/// `0034` moved `MAX_DEPTH` 16 → 32, which changes the array's size and not the
/// property this test is about),
/// and this asserts that the design survived contact with a 1537-frame tree, a
/// four-dynamic-step plan and a ring that laps repeatedly during the loop.
///
/// **Ring wraparound is the specific thing added here.** The test above pushes
/// a million samples into a 128-slot ring, so it laps too — but on an edge that
/// is *not* on the query path. Here the pushes land on an edge the plan reads,
/// so the reader's retained window slides under a fixed query stamp on every
/// lap and the bracket search runs against a ring whose oldest slots have been
/// overwritten.
///
/// **What it does not reach is the seqlock retry.** This sentence used to say
/// the wrap was *"the path where a retry allocating a scratch buffer would show
/// up"*, and it is not: push and query alternate on one thread, so a reader
/// never meets a slot mid-write and `SampleRing::read_slot`'s retry arm (an odd
/// `seq`, or a `seq` that moved under the payload loads) is never taken. That
/// arm needs a concurrent writer, and this file has none — so its freedom from
/// allocation is read from the code (a bounded loop over stack locals), not
/// executed here.
///
/// Host-independent, so unlike everything else in this suite it is a hard gate
/// and runs in `cargo nextest run --workspace`.
#[test]
fn no_allocations_on_a_large_topology_across_ring_wraparound() {
    use tf_tree::InterpPolicy;
    use tf_tree_bench::workload::{self, Backing};

    // `fleet_64`: 1537 frames, 256 dynamic edges, a cross-fleet plan of four
    // dynamic steps. `av` would add depth but not width; this adds both the
    // width and the multi-robot plan shape.
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

    // Publish onto an edge the plan *reads*, so the reader crosses the writer's
    // ring wrap. `publishers[0]` is robot 0's `map->odom`, which the cross-fleet
    // pair traverses.
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

    // The ring holds 10 s at 50 Hz — 512 slots — so this laps roughly 390 times.
    // A first revision of this test pushed 1000 samples and lapped twice, which
    // is enough to be true and not enough to be evidence.
    const ITERS: usize = 200_000;

    let before = allocations();
    let mut acc = 0.0f64;
    let mut answered = 0usize;
    for _ in 0..ITERS {
        writer.push(push_stamp, &iso).expect("push");
        push_stamp += step_ns;
        // The query stamp is fixed while the window slides past it, so the later
        // iterations legitimately fall out of the retained window. Both branches
        // are on the no-allocation path and both must stay on it — an error path
        // that formats a message would allocate, which is exactly what
        // `CLAUDE.md`'s "no `String` in any error type" rule is protecting.
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

// ---------------------------------------------------------------------------
// The rest of `docs/API.md` §8.1's list.
//
// §8.1 claims the allocation property for `at_many_into`,
// `at_with_derivatives` and `at_extrapolating` as well as `at`, and names this
// file as the check — but until these tests the two above were the file, and
// both only ever call `Plan::at`. §8.1 arrived after them (#278) and cited them
// for three methods neither runs.
// ---------------------------------------------------------------------------

/// Stamps per batch call.
const BATCH: usize = 16;

/// Push + evaluate rounds per window.
///
/// **Not the 200 000 the test above uses, and deliberately.** A batch call
/// stops at its first declined stamp, so once the ring has slid past the batch
/// every further round is an element-0 refusal: more rounds add the same
/// refusal again, not coverage. 3000 rounds at 50 Hz is about six laps of the
/// 512-slot ring, which is enough to see both sides of the window's edge.
const WINDOW_ROUNDS: usize = 3000;

/// How one evaluation inside a measured window came back.
enum Outcome {
    /// `Ok`, and not extrapolated (every batch form and `at_with_derivatives`
    /// report this for any `Ok`).
    Answered,
    /// `Ok` from `at_extrapolating` with `by_ns > 0`: the pose was invented
    /// past the newest common sample, so the policy's extrapolation arm ran.
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
    /// Whether this window exists to measure an extrapolation arm, so an `Ok`
    /// with `by_ns > 0` is required rather than an interpolated one.
    wants_extrapolated: bool,
}

/// Build `fleet_64` afresh, publish onto an edge the plan reads, and count this
/// thread's allocations across [`WINDOW_ROUNDS`] push + `eval` rounds.
///
/// A fresh build per window rather than one shared tree: the ring slides past
/// a fixed batch within a few hundred rounds, so a second window on the same
/// tree would measure nothing but element-0 refusals.
fn measure(
    policy: tf_tree::InterpPolicy,
    label: &'static str,
    wants_extrapolated: bool,
    mut eval: impl FnMut(&tf_tree::Plan, &tf_tree::Guard, &mut Buffers) -> Outcome,
) -> Window {
    use tf_tree_bench::workload::{self, Backing};

    // --- construction (allocation permitted) ---------------------------
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
    // One transposition is enough to fail `windows(2).all(<=)` and send the
    // batch down the cursor-free branch.
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

    // Warm once, outside the window.
    let _ = eval(&plan, &guard, &mut bufs);
    writer.push(push_stamp, &iso).expect("warm push");
    push_stamp += step_ns;

    // --- measured window: must not allocate ----------------------------
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
    // Visible under `--no-capture`: the counts are the evidence that each
    // window reached both sides of the path, not only a verdict.
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

/// Assert every window at once, so a failure names **each** window that went
/// wrong rather than stopping at the first — which is what lets one mutant in
/// a shared function show that every window here measures its own call.
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

/// `at_many`, `at_many_into` (`Mat4`, `Quat`) and `at_many_into_f32`
/// (`Affine32`) allocate nothing, over both the monotone batch — resumable
/// galloping cursors — and the non-monotone one, which re-searches per stamp.
///
/// Every window also asserts that some call answered and some was refused, so
/// neither half of the path can pass by never running.
///
/// Mutants, each run (every one reverted, the file touched, and the five other
/// tests in this file observed green or red as stated):
///
/// * `let _ = core::hint::black_box(alloc::boxed::Box::new(0u8));` as the first
///   statement of `Plan::note` (`tf_tree_core/src/plan.rs`) — reached once per
///   element by every evaluation here, answered or refused. **FAILS, naming all
///   eight windows**: `allocations=6120` for each monotone window and
///   `allocations=6296` for each non-monotone one, `answered=208 declined=2792`
///   in all eight.
/// * The same statement inside `Plan::fold_batch`'s **monotone** loop only.
///   FAILS naming exactly three windows — `at_many_into Mat4 monotone`,
///   `at_many_into Quat monotone`, `at_many_into_f32 Affine32 monotone`, each
///   `allocations=6120`. `at_many` has its own loop and the non-monotone windows
///   take the other arm, and all five stay green: the windows are specific, not
///   merely sensitive.
/// * `WINDOW_ROUNDS` lowered to 100: FAILS every window with `declined=0 <-
///   nothing declined, so the refusal path was never measured` — the
///   anti-vacuity assertion is itself failable.
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

/// `at_with_derivatives` and its batch layout, `at_many_into(Layout::QuatTwist)`,
/// allocate nothing.
///
/// **On an `ScLerp` build, and that is the test rather than a detail.** Both
/// refuse a `LerpSlerp` edge with `DerivativesUnavailable` before sampling, so
/// on the `LerpSlerp` build every other window in this file uses, every call
/// here was a refusal and a zero count proved nothing about the fold — measured:
/// 3000 of 3000 declined, 0 answered. The `answered > 0` assertion is what
/// would have said so.
///
/// Mutants, each run:
///
/// * The `Box` statement in `Plan::note`: **FAILS, naming all three windows**
///   (`allocations=6120`, `6296` and `3000`).
/// * `let _ = core::hint::black_box(alloc::boxed::Box::new(0u8));` as the first
///   statement of `Plan::fold_with_derivatives` — the fold under both the
///   cursor and the cursor-free arms. FAILS naming all three windows with the
///   same counts; `batch_forms_allocate_nothing_monotone_or_not` and
///   `extrapolating_allocates_nothing_under_every_policy` stay green.
/// * The monotone `QuatTwist` window and the `at_with_derivatives` window
///   built with `InterpPolicy::LerpSlerp`: FAILS naming exactly those two with
///   `allocations=0 answered=0 extrapolated=0 declined=3000 <- nothing
///   answered` — a zero count that would have passed, caught by the
///   anti-vacuity assertion on the trap it exists for.
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
/// `Error` is queried mid-window, where it interpolates until the ring slides
/// past. `Hold` and `ConstantTwist` are queried **two seconds past the newest
/// populated sample**: a mid-window stamp brackets and never reaches either
/// policy's extrapolation arm, so a window there would measure the
/// interpolating fold three times. Those two windows require an answer with
/// `by_ns > 0` — one the policy invented — not merely an `Ok`.
///
/// Mutants, each run:
///
/// * The `Box` statement in `Plan::note`: **FAILS, naming all three windows**,
///   each `allocations=3000`.
/// * `let _ = core::hint::black_box(alloc::boxed::Box::new(0u8));` as the first
///   statement of `SampleRing::constant_twist` (`tf_tree_core/src/sample.rs`),
///   which only the `ConstantTwist` arm calls: FAILS naming exactly
///   `at_extrapolating ConstantTwist allocations=4304 … extrapolated=605`;
///   `Error` and `Hold` stay green.
/// * `SampleRing::sample_from`'s `ExtrapPolicy::Hold => self` arm rewritten to
///   `core::hint::black_box((alloc::boxed::Box::new(0u8), self).1)`: FAILS
///   naming exactly `at_extrapolating Hold allocations=4304`.
/// * The `Hold` window queried at `mid` instead of `ahead`: FAILS with
///   `answered=100 extrapolated=0 … <- no extrapolated answer` (run together
///   with `WINDOW_ROUNDS = 100`), so `wants_extrapolated` is failable.
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

/// A **stale plan's refusal** allocates nothing either.
///
/// `docs/API.md` §8.1's scope is evaluation "under a `Guard` the caller already
/// holds", and a plan compiled before a reparent is one such evaluation: the
/// caller is handed `LookupError::TopologyChanged` and re-plans. That refusal
/// is the one a control loop meets on every topology change, and no allocation
/// window here ever contained it — both windows above evaluate a current plan.
///
/// # The ordering is the test
///
/// A `Guard` pins the topology generation **when it is built**. So the guard for
/// the window is taken *after* the reparent: a guard taken before it still sees
/// the plan's own generation and answers `Ok`, and a window over that guard would
/// measure the success path under a refusal's name. The first assertion after
/// the reparent pins exactly that, so moving the guard up fails loudly rather
/// than silently measuring the wrong thing.
///
/// The positive control is the same plan answering `Ok` before the reparent,
/// and a freshly compiled plan answering `Ok` under the window's guard after the
/// window — which is what says the refusals were about staleness and not a tree
/// the reparent broke.
///
/// Mutants, each run:
///
/// * `let _ = core::hint::black_box(alloc::boxed::Box::new(0u8));` on the cold
///   side of `Plan::check_generation` (`tf_tree_core/src/plan.rs`, after the
///   `cur == self.generation` early return). **FAILS**:
///   `expected zero allocations across 1000000 stale-plan refusals, saw 1000000`.
///   Every other test in this file stays green under it — none takes that
///   side — which is the gap this test closes.
/// * The `Box` statement in `Plan::note` instead: this test stays **green** and
///   the other five fail. A refusal leaves through `check_generation`'s `?`
///   before any fold is `note`d, so this window and the §8.1 windows above
///   measure disjoint code.
/// * The window run over the guard built *before* the reparent
///   (`let guard = before_reparent;` in place of the drop and the fresh guard):
///   FAILS at the window's first evaluation, `a stale plan under a guard built
///   after the reparent must refuse, got Ok(Iso3 { .. })`.
#[test]
fn a_stale_plan_refusal_allocates_nothing() {
    use tf_tree::LookupError;

    // --- construction (allocation permitted) ---------------------------
    let tree = fixture::build_tree().expect("build fixture");
    let (writers, _samples) = fixture::spin_up(&tree).expect("populate history");

    let target = tree.frame("imu_link").expect("imu frame");
    let source = tree.frame("map").expect("map frame");
    let plan = tree.plan(target, source).expect("compile plan");
    let query: Stamp = Stamp::from_nanos(fixture::NOW_NS);

    // Positive control: the plan is current and answers.
    let before_reparent = tree.guard();
    plan.at(&before_reparent, query)
        .expect("positive control: a current plan answers");

    // Move a dynamic edge that is *off* the query path, so the query's answer
    // would be unchanged — only the generation moves.
    let lidar = tree.frame("lidar").expect("lidar frame");
    let base_link = tree.frame("base_link").expect("base_link frame");
    tree.reparent(lidar, base_link).expect("reparent lidar");

    // The guard built before the reparent still holds the old generation.
    assert!(
        plan.at(&before_reparent, query).is_ok(),
        "a guard built before the reparent pinned the old generation and should \
         still answer — if this changed, the window's ordering below needs \
         re-deriving"
    );
    drop(before_reparent);

    let guard = tree.guard();
    // Warm once, outside the window.
    let _ = plan.at(&guard, query);

    const ITERS: usize = 1_000_000;

    // --- measured window: must not allocate ----------------------------
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

    // Positive control, after: re-planning is the documented response, and it
    // answers under the very guard the window used.
    let fresh = tree.plan(target, source).expect("re-plan");
    fresh
        .at(&guard, query)
        .expect("positive control: a re-compiled plan answers under the same guard");

    drop(guard);
    drop(writers);
}
