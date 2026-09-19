// Read-scaling benchmark (`docs/PHASE1.md` §11.3 gate: read throughput should scale
// >= 6x from 1 to 8 threads). Each thread runs its own `Guard` over the shared
// `Tree` and evaluates the same depth-3 plan. Unpinned, so indicative only; the
// pinned multi-process version is `src/bin/contended_scaling.rs`.
//
// `read_scaling` runs a quiescent tree; `read_scaling_writers` adds a live
// publisher per dynamic edge (`docs/PHASE1.md` §11.2), at the fixture's nominal
// rates. The difference between the groups is the seqlock retry cost.
//
// Workers are spawned once per benchmark function, outside `b.iter`, and parked
// on a barrier; spawning inside would time thread creation instead.
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

/// The stamp worker `i` queries on its `k`-th lookup, spread across the whole
/// 100 ms window (`NOW_NS - (k % SWEEP_NS)` collapses into one bracket).
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

/// One writer, publishing at its edge's nominal rate. Flat-out writers plus eight
/// readers starve a four-core host, so the strong version lives in
/// `src/bin/contended_scaling.rs`.
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
    // Release the populating writers first: a claim is a lease.
    drop(populate);

    let t = tree.frame("imu_link").expect("target");
    let s = tree.frame("map").expect("source");
    let plan = tree.plan(t, s).expect("plan");

    let mut group = c.benchmark_group(name);
    for &threads in &[1usize, 2, 4, 8] {
        group.throughput(criterion::Throughput::Elements(
            (threads * PER_THREAD) as u64,
        ));

        // The criterion driver is one of the `threads` participants, so only
        // `threads - 1` workers are spawned.
        let start = Barrier::new(threads);
        let done = Barrier::new(threads);
        let stop = AtomicBool::new(false);
        // Separate from `stop`: writers keep running across every `b.iter` batch.
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

            // Release the workers once more with `stop` set so `thread::scope` can join.
            stop.store(true, Ordering::Release);
            start.wait();
            stop_writers.store(true, Ordering::Release);
        });
    }
    group.finish();
}

criterion_group!(benches, read_scaling, read_scaling_writers);
criterion_main!(benches);
