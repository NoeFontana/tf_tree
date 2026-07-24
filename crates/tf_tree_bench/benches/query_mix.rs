// The query-mix benchmark (decision `0003`, *Measurements*): 70% At(t) uniform in
// [now-100ms, now], 20% Latest, 10% LatestCommon, over a depth-3 plan.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use tf_tree::{Query, Stamp};
use tf_tree_bench::fixture;

fn query_mix(c: &mut Criterion) {
    let tree = fixture::build_tree().expect("build fixture");
    let (_writers, _samples) = fixture::spin_up(&tree).expect("populate history");

    let t = tree.frame("imu_link").expect("target");
    let s = tree.frame("map").expect("source");
    let plan = tree.plan(t, s).expect("plan");
    let guard = tree.guard();

    // Precompute a fixed mix so the timed loop does no RNG work: indices 0..70 are
    // At(t) at spread stamps, 70..90 Latest, 90..100 LatestCommon.
    let now = fixture::NOW_NS;
    let lo = now - 100_000_000;
    let mut queries: Vec<Query> = Vec::with_capacity(100);
    for i in 0..70 {
        let stamp = lo + (now - lo) * i / 70;
        queries.push(Query::At(Stamp::from_nanos(stamp)));
    }
    for _ in 0..20 {
        queries.push(Query::Latest);
    }
    for _ in 0..10 {
        queries.push(Query::LatestCommon);
    }

    let mut i = 0usize;
    c.bench_function("query_mix/depth3", |b| {
        b.iter(|| {
            let q = queries[i % queries.len()];
            i = i.wrapping_add(1);
            black_box(plan.query(&guard, black_box(q)).expect("query"))
        });
    });
}

criterion_group!(benches, query_mix);
criterion_main!(benches);
