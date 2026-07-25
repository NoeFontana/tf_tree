// Read-scaling benchmark (`docs/PHASE1.md` §11.3 gate: read throughput should scale
// >= 6x from 1 to 8 threads). Each thread runs its own `Guard` over the shared,
// lock-free `Tree` and evaluates a copy of the same depth-3 plan.
//
// This is a portable approximation of the gate row: it does NOT pin cores or run
// concurrent writers, so the scaling factor it reports is indicative, not the
// official number (which needs dedicated, core-pinned hardware).
//
// # What is and is not inside the timed region
//
// Worker threads are spawned **once per benchmark function**, outside `b.iter`,
// and parked on a barrier. Each timed iteration only releases the barrier and
// waits for the workers to report back. Spawning inside `b.iter` would put one
// thread creation + join per worker (tens of microseconds) inside a measurement
// of 2048 lookups at roughly 100 ns each — the result would be a thread-spawn
// benchmark that gets monotonically *worse* with thread count, which is the
// exact opposite of what this row is supposed to show.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Barrier;
use std::thread;

use criterion::{criterion_group, criterion_main, Criterion};

use tf_tree::{Stamp, Tree};
use tf_tree_bench::fixture;

const PER_THREAD: usize = 2_048;

/// Width of the stamp sweep each worker walks: 100 ms of history, which every
/// dynamic edge of the fixture retains.
const SWEEP_NS: i64 = 100_000_000;

/// The stamp worker `i` queries on its `k`-th lookup.
///
/// The sweep is spread across the whole 100 ms window. Writing this as
/// `NOW_NS - (k % SWEEP_NS)` — as an earlier version did — is a no-op, because
/// `k < PER_THREAD` is always far below `SWEEP_NS`: every query collapses into a
/// 2 µs window, landing in one bracket and on one pair of cache lines. That is a
/// degenerate best case for precisely the benchmark meant to expose contention.
#[inline]
fn stamp_for(k: usize) -> Stamp {
    let offset = (k as i64) * SWEEP_NS / (PER_THREAD as i64);
    Stamp::from_nanos(fixture::NOW_NS - offset)
}

/// One worker's share of the work: `PER_THREAD` plan evaluations.
fn worker_pass(tree: &Tree, plan: &tf_tree::Plan) {
    let guard = tree.guard();
    let mut acc = 0.0f64;
    for k in 0..PER_THREAD {
        let iso = plan.at(&guard, stamp_for(k)).expect("eval");
        acc += iso.t.x;
    }
    black_box(acc);
}

fn read_scaling(c: &mut Criterion) {
    let tree = fixture::build_tree().expect("build fixture");
    let (_writers, _samples) = fixture::spin_up(&tree).expect("populate history");

    let t = tree.frame("imu_link").expect("target");
    let s = tree.frame("map").expect("source");
    let plan = tree.plan(t, s).expect("plan");

    let mut group = c.benchmark_group("read_scaling");
    for &threads in &[1usize, 2, 4, 8] {
        group.throughput(criterion::Throughput::Elements(
            (threads * PER_THREAD) as u64,
        ));

        // The criterion driver is one of the `threads` participants and carries a
        // share of the work itself, so only `threads - 1` workers are spawned.
        // Spawning `threads` of them would leave `threads + 1` runnable threads
        // competing for `threads` cores: at 8 the driver gets descheduled at every
        // rendezvous, and the row reports scheduler latency rather than scaling.
        let start = Barrier::new(threads);
        let done = Barrier::new(threads);
        let stop = AtomicBool::new(false);
        let (tree, start, done, stop) = (&tree, &start, &done, &stop);

        thread::scope(|scope| {
            for _ in 0..threads - 1 {
                scope.spawn(move || loop {
                    start.wait();
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    worker_pass(tree, &plan);
                    done.wait();
                });
            }

            group.bench_function(format!("{threads}_threads"), |b| {
                b.iter(|| {
                    start.wait();
                    worker_pass(tree, &plan);
                    done.wait();
                });
            });

            // Release the workers one last time with the stop flag set, so
            // `thread::scope` can join them instead of deadlocking on `start`.
            stop.store(true, Ordering::Release);
            start.wait();
        });
    }
    group.finish();
}

criterion_group!(benches, read_scaling);
criterion_main!(benches);
