//! The zero-allocation gate (decision `0003`, *Test plan* → *Allocation* and
//! load-bearing invariant 8: "every heap allocation happens at construction").
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
use std::sync::atomic::{AtomicUsize, Ordering};

use tf_tree::Stamp;
use tf_tree_bench::fixture;

/// Number of allocating calls (`alloc` + `realloc`) seen since process start.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// A `System`-backed allocator that counts allocating calls. Deallocations are
/// not counted — the gate asserts that no *new* allocation happens in the hot
/// loop, which is what invariant 8 requires.
struct CountingAllocator;

// SAFETY: `CountingAllocator` forwards every call unchanged to `System`, which is
// a sound `GlobalAlloc`. The only added work is a `Relaxed` atomic increment on
// the allocating paths, which cannot affect the returned pointers or their
// validity. This impl therefore upholds every `GlobalAlloc` invariant that
// `System` upholds. (Test-only binary; the crate proper is `#![forbid(unsafe_code)]`.)
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
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
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
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
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let mut acc = 0.0f64;
    for _ in 0..ITERS {
        lidar.push(push_stamp, &iso).expect("push");
        push_stamp += 1_000_000;
        let pose = plan.at(&guard, query).expect("at");
        acc += pose.t.x;
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);

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
