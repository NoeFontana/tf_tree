// Read-scaling benchmark (decision `0003` gate: read throughput should scale
// >= 6x from 1 to 8 threads). Each thread runs its own `Guard` over the shared,
// lock-free `Tree` and evaluates a copy of the same depth-3 plan.
//
// This is a portable approximation of the gate row: it does NOT pin cores or run
// concurrent writers, so the scaling factor it reports is indicative, not the
// official number (which needs dedicated, core-pinned hardware).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::hint::black_box;
use std::thread;

use criterion::{criterion_group, criterion_main, Criterion};

use tf_tree::{Stamp, Tree};
use tf_tree_bench::fixture;

const PER_THREAD: usize = 2_048;

fn parallel_lookups(tree: &Tree, plan: tf_tree::Plan, threads: usize) {
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(move || {
                let guard = tree.guard();
                let mut acc = 0.0f64;
                for i in 0..PER_THREAD {
                    let stamp: Stamp =
                        Stamp::from_nanos(fixture::NOW_NS - (i as i64 % 100_000_000));
                    let iso = plan.at(&guard, stamp).expect("eval");
                    acc += iso.t.x;
                }
                black_box(acc);
            });
        }
    });
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
        group.bench_function(format!("{threads}_threads"), |b| {
            b.iter(|| parallel_lookups(&tree, plan, threads));
        });
    }
    group.finish();
}

criterion_group!(benches, read_scaling);
criterion_main!(benches);
