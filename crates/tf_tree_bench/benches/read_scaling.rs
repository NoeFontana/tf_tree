// Read-scaling benchmark (`docs/PHASE1.md` §11.3 gate: read throughput should scale
// >= 6x from 1 to 8 threads). Each thread runs its own `Guard` over the shared,
// lock-free `Tree` and evaluates a copy of the same depth-3 plan.
//
// This is a portable approximation of the gate row: it does NOT pin cores, so
// the scaling factor it reports is indicative, not the official number (which
// needs dedicated, core-pinned hardware). The pinned, multi-process version is
// `src/bin/contended_scaling.rs`.
//
// # Two groups, and why the second one exists
//
// `read_scaling` runs against a quiescent tree. `read_scaling_writers` runs the
// identical readers with a live publisher on every dynamic edge, which is what
// `docs/PHASE1.md` §11.2 actually specifies ("4 concurrent writers") and what
// `docs/benchmarks/tf2.md` listed under "What is still not measured".
//
// The quiescent group is kept rather than replaced. It is the continuity anchor
// for every committed number, and the *difference* between the two groups is
// the measurement — what the seqlock retry path costs a reader when somebody is
// actually writing. One group alone answers neither question.
//
// The writers here publish at the fixture's NOMINAL rates, so this group is the
// portable, always-runnable version of the question. The version with real
// pressure — a writer per core, saturating — is `src/bin/contended_scaling.rs`,
// and `writer_loop` below records what happened when this file tried to be that
// instead.
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
    scaling_group(c, "read_scaling", 0);
}

/// The same readers, with `WRITER_EDGES` live publishers on the tree.
fn read_scaling_writers(c: &mut Criterion) {
    scaling_group(c, "read_scaling_writers", WRITER_EDGES);
}

/// How many of the fixture's four dynamic edges get a live publisher in the
/// contended group. All four, which is `docs/PHASE1.md` §11.2's figure.
const WRITER_EDGES: usize = 4;

/// One writer, publishing at its edge's **nominal** rate.
///
/// # Why nominal and not as fast as possible
///
/// The first revision of this ran the writers flat out, on the reasoning that at
/// 50-1000 Hz a writer touches a cache line a few thousand times a second, which
/// against millions of lookups per second is indistinguishable from no writer at
/// all - so a full-speed writer supplies more of the thing being studied.
///
/// That reasoning is right about the contention and wrong about the benchmark.
/// Four spinning writers plus eight reader threads is twelve runnable threads,
/// and on a four-core host the readers are starved: the group ran for over ten
/// minutes without completing a single row. A benchmark nobody can run supplies
/// no contention at all.
///
/// So this publishes at the fixture's rates, which is what a robot does, and the
/// strong version of the question lives where it can be asked properly -
/// `src/bin/contended_scaling.rs`, which pins every reader and every writer to
/// its own core and knows how many it has.
fn writer_loop(tree: &tf_tree::Tree, edge: usize, stop: &AtomicBool) {
    let (parent, child, rate_hz) = fixture::DYNAMIC_EDGES[edge];
    let (Ok(p), Ok(c)) = (tree.frame(parent), tree.frame(child)) else {
        return;
    };
    let Ok(w) = tree.claim(c, p) else { return };
    let step = (1e9 / rate_hz) as i64;
    // Start above the populated history so every push is in order.
    let mut stamp = (fixture::HISTORY_SECS * rate_hz) as i64 * step + step;
    let period = std::time::Duration::from_secs_f64(1.0 / rate_hz);
    while !stop.load(Ordering::Acquire) {
        let _ = w.push(stamp, &fixture::dynamic_pose(edge as f64, stamp));
        stamp += step;
        std::thread::sleep(period);
    }
}

fn scaling_group(c: &mut Criterion, name: &str, writers: usize) {
    let tree = fixture::build_tree().expect("build fixture");
    let (populate, _samples) = fixture::spin_up(&tree).expect("populate history");
    // The populating writers must be released before the bench's own writers can
    // claim the same edges — a claim is a lease, and a second one is refused.
    drop(populate);

    let t = tree.frame("imu_link").expect("target");
    let s = tree.frame("map").expect("source");
    let plan = tree.plan(t, s).expect("plan");

    let mut group = c.benchmark_group(name);
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
        // Separate from `stop`: the writers must keep running across every
        // `b.iter` batch, including the barrier waits between them. Tying them
        // to the reader barrier would leave the tree quiescent for exactly the
        // moments the readers are being timed.
        let stop_writers = AtomicBool::new(false);
        let (tree, start, done, stop) = (&tree, &start, &done, &stop);
        let stop_writers = &stop_writers;

        thread::scope(|scope| {
            for edge in 0..writers {
                scope.spawn(move || writer_loop(tree, edge, stop_writers));
            }
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
            stop_writers.store(true, Ordering::Release);
        });
    }
    group.finish();
}

criterion_group!(benches, read_scaling, read_scaling_writers);
criterion_main!(benches);
