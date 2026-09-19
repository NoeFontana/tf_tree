//! `Ingest::offer` must not allocate more as it runs longer.
//!
//! A steady-state bound, not zero-allocation: `NameNormalizer` owns the
//! normalized names it returns. Each figure is measured over two consecutive
//! equal windows, so growth landing in either one fails. The counter is
//! thread-local (libtest's main thread allocates during a window otherwise);
//! `CountingAllocator` is copied from `crates/tf_tree_bench/tests/zero_alloc.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// `docs/decisions/0007` rule 1, kind 6 (`0048`): `unsafe impl GlobalAlloc` in a
// test crate root.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use tf_tree_bridge::{Action, Ingest, Publisher, Sample, SteadyNanos, Topic, TopologyConfig};

thread_local! {
    /// Allocating calls (`alloc` + `realloc`) made by this thread; other threads
    /// (libtest's main thread) must not be charged to a window. `const` init
    /// because a lazy thread-local could allocate inside the allocator.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

/// This thread's allocating-call count (`try_with`: the local may be destroyed
/// at thread teardown).
fn allocations() -> usize {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

/// Record one allocating call on this thread, if the local is still live.
fn note_allocation() {
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get().wrapping_add(1)));
}

/// A `System`-backed allocator that counts allocating calls.
struct CountingAllocator;

// SAFETY: every call is forwarded unchanged to `System`; the only added work is a
// non-allocating, non-panicking thread-local increment, so `System`'s
// `GlobalAlloc` invariants carry over.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        // SAFETY: forwarding an unmodified `layout` to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr`/`layout` came from `System.alloc`. Deallocation is not counted.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation();
        // SAFETY: `ptr`/`layout` originate from `System` and `new_size` is
        // passed through unchanged, satisfying `System::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Offers per window (two windows). Bounded above by 2_496: past that the
/// regressing fixture rejoins the happy path and its `Action::Drop` asserts fail.
const ITERS: usize = 2_000;

/// One dynamic edge, plus a second the config deliberately does *not* declare.
const TOPO: &str = r#"
[[edge]]
parent = "odom"
child = "base"
kind = "dynamic"
capacity = 256
"#;

/// A receipt-clock origin. Never 0: `SteadyNanos(0)` means "no receipt clock".
const T0: i64 = 5_000_000_000_000;
/// The stamp a healthy publisher emits at `T0`.
const STAMP0: i64 = 10_000_000_000;
/// One millisecond.
const MS: i64 = 1_000_000;

/// Allocating calls over each of two consecutive `ITERS`-message windows.
///
/// Stamp and receipt clock advance together so the offset never crosses the
/// reset threshold mid-measurement.
fn allocs_per_offer(sample: &Sample, publisher: &Publisher) -> (usize, usize) {
    let config = TopologyConfig::parse(TOPO).unwrap();
    let mut ingest = Ingest::new(&config);
    let mut s = sample.clone();

    // Warm-up: the first message per edge and publisher legitimately allocates.
    for k in 0..8i64 {
        s.stamp_nanos = STAMP0 + k * MS;
        s.received = SteadyNanos(T0 + k * MS);
        ingest.offer(Topic::Tf, &s, publisher);
    }

    let mut window = |from: i64| {
        let before = allocations();
        for k in 0..ITERS {
            let k = from + k as i64;
            s.stamp_nanos = STAMP0 + k * MS;
            s.received = SteadyNanos(T0 + k * MS);
            ingest.offer(Topic::Tf, &s, publisher);
        }
        allocations() - before
    };
    let first = window(8);
    (first, window(8 + ITERS as i64))
}

/// Allocating calls per `offer` for a publisher stuck below its high-water mark:
/// every iteration takes the clock rules' refusal path.
fn allocs_per_regressing_offer(publisher: &Publisher) -> (usize, usize) {
    let config = TopologyConfig::parse(TOPO).unwrap();
    let mut ingest = Ingest::new(&config);
    let mut s = Sample::identity("odom", "base", STAMP0).received_at(SteadyNanos(T0));

    // One good message sets the high-water mark, names and offset baseline…
    assert!(matches!(
        ingest.offer(Topic::Tf, &s, publisher),
        Action::Publish { .. }
    ));
    // …and then the publisher restarts and replays from five seconds ago.
    for k in 0..8i64 {
        s.stamp_nanos = STAMP0 - 5_000 * MS + k * MS;
        s.received = SteadyNanos(T0 + (1 + k) * MS);
        ingest.offer(Topic::Tf, &s, publisher);
    }

    let mut window = |from: i64| {
        let before = allocations();
        for k in 0..ITERS {
            let k = from + k as i64;
            s.stamp_nanos = STAMP0 - 5_000 * MS + k * MS;
            s.received = SteadyNanos(T0 + (1 + k) * MS);
            assert!(matches!(
                ingest.offer(Topic::Tf, &s, publisher),
                Action::Drop { .. }
            ));
        }
        allocations() - before
    };
    let first = window(8);
    let second = window(8 + ITERS as i64);
    assert_eq!(
        ingest.stats().dropped_non_monotonic,
        (2 * ITERS + 8) as u64,
        "the fixture must really be on the regression path"
    );
    (first, second)
}

/// No path allocates for its table lookups, including the refusal path of a
/// broken publisher. `base -> lidar` is absent from `TOPO`, so the undeclared
/// path is really taken; the regressing fixture asserts its own drops.
#[test]
fn offer_does_not_allocate_for_its_table_lookups() {
    let publisher = Publisher::named(&tf_tree_bridge::gid_for_name("/ekf"), "/ekf");

    let declared = allocs_per_offer(&Sample::identity("odom", "base", 0), &publisher);
    let undeclared = allocs_per_offer(&Sample::identity("base", "lidar", 0), &publisher);
    let regressing = allocs_per_regressing_offer(&publisher);

    // Two per offer: the owned normalized name `NameNormalizer::normalize`
    // returns, one per frame. Exact totals, not averages, so one
    // allocation per 2001 messages cannot round to zero.
    const DECLARED_PER_OFFER: usize = 2;
    const UNDECLARED_PER_OFFER: usize = 2;
    const REGRESSING_PER_OFFER: usize = 0;

    let check = |what: &str, got: (usize, usize), want_each: usize| {
        let want = want_each * ITERS;
        for (which, got) in [("first", got.0), ("second", got.1)] {
            assert_eq!(
                got,
                want,
                "{what} path, {which} window: {got} allocations across {ITERS} offers \
                 ({:.4} per offer), expected exactly {want} ({want_each} per offer)",
                got as f64 / ITERS as f64
            );
        }
    };
    check("declared", declared, DECLARED_PER_OFFER);
    check("undeclared", undeclared, UNDECLARED_PER_OFFER);
    check("regressing", regressing, REGRESSING_PER_OFFER);
}
