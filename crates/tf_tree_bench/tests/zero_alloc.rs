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
#![allow(clippy::unwrap_used, clippy::expect_used)]

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
/// fixed-size by design (`Plan` is `[Step; MAX_DEPTH]` and `Copy`, invariant 8),
/// and this asserts that the design survived contact with a 1537-frame tree, a
/// four-dynamic-step plan and a ring that laps repeatedly during the loop.
///
/// **Ring wraparound is the specific thing added here.** The test above pushes
/// a million samples into a 128-slot ring, so it laps too — but on an edge that
/// is *not* on the query path. Here the pushes land on an edge the plan reads,
/// so the reader crosses the writer's wrap on every lap, which is the path
/// where a retry allocating a scratch buffer would show up.
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
