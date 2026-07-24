// Batch sampling: `at_many` with 1024 monotone stamps (decision `0003`,
// *Measurements* — reported as ns/sample). Monotone input lets each dynamic edge
// gallop from a resumable cursor, so this is the O(1)-amortized path.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use tf_tree::{Iso3, Stamp};
use tf_tree_bench::fixture;

const N: usize = 1024;

fn at_many(c: &mut Criterion) {
    let tree = fixture::build_tree().expect("build fixture");
    let (_writers, _samples) = fixture::spin_up(&tree).expect("populate history");

    let t = tree.frame("imu_link").expect("target");
    let s = tree.frame("map").expect("source");
    let plan = tree.plan(t, s).expect("plan");
    let guard = tree.guard();

    // 1024 monotone stamps spread across the last 100 ms.
    let now = fixture::NOW_NS;
    let lo = now - 100_000_000;
    let stamps: Vec<Stamp> = (0..N)
        .map(|i| Stamp::from_nanos(lo + (now - lo) * i as i64 / N as i64))
        .collect();
    let mut out = vec![Iso3::IDENTITY; N];

    let mut group = c.benchmark_group("at_many");
    group.throughput(Throughput::Elements(N as u64));
    group.bench_function("monotone_1024", |b| {
        b.iter(|| {
            plan.at_many(&guard, black_box(&stamps), &mut out)
                .expect("at_many");
            black_box(&out);
        });
    });
    group.finish();
}

criterion_group!(benches, at_many);
criterion_main!(benches);
