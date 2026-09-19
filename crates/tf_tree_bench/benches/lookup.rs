// Hot-cache lookup at depths 1, 3 and 6 over the shared fixture. Setup is outside
// the timed closure; the depth-3 row is the gate's but this is NOT the official gate
// (`docs/PHASE1.md` §11.3).
//
// The stamp is `fixture::QUERY_NS`, not `NOW_NS` (`docs/decisions/0013`): `NOW_NS`
// is a knot on every dynamic grid, so `I::eval` never ran. Not `docs/PHASE5.md`
// §9.2's cross-crate row either; see `src/embed.rs`, "Why not `benches/lookup.rs`".
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use tf_tree::{InterpPolicy, Stamp};
use tf_tree_bench::fixture;

/// Build a populated fixture, compile the `target <- source` plan, and time its
/// hot evaluation at a fixed in-window stamp.
fn bench_pair(c: &mut Criterion, label: &str, target: &str, source: &str, interp: InterpPolicy) {
    bench_pair_at(c, label, target, source, interp, fixture::QUERY_NS);
}

/// [`bench_pair`] at an explicit stamp, so the on-grid best case can be timed
/// beside the off-grid one the gate is written against.
fn bench_pair_at(
    c: &mut Criterion,
    label: &str,
    target: &str,
    source: &str,
    interp: InterpPolicy,
    query_ns: i64,
) {
    let tree = fixture::build_tree_with(interp).expect("build fixture");
    let (_writers, _samples) = fixture::spin_up(&tree).expect("populate history");

    let t = tree.frame(target).expect("target frame");
    let s = tree.frame(source).expect("source frame");
    let plan = tree.plan(t, s).expect("compile plan");
    let guard = tree.guard();
    let stamp: Stamp = Stamp::from_nanos(query_ns);

    c.bench_function(label, |b| {
        b.iter(|| black_box(plan.at(&guard, black_box(stamp)).expect("eval")));
    });
}

fn lookup_benches(c: &mut Criterion) {
    // Depth 1: map <- odom (one dynamic edge).
    bench_pair(
        c,
        "lookup/depth1/sclerp",
        "odom",
        "map",
        InterpPolicy::ScLerp,
    );
    // Depth 3: map <- imu_link (three dynamic edges: imu, base_link, odom).
    bench_pair(
        c,
        "lookup/depth3/sclerp",
        "imu_link",
        "map",
        InterpPolicy::ScLerp,
    );
    bench_pair(
        c,
        "lookup/depth3/lerpslerp",
        "imu_link",
        "map",
        InterpPolicy::LerpSlerp,
    );
    // Depth 6: map <- camera_optical (six-edge path; the static tail folds).
    bench_pair(
        c,
        "lookup/depth6/sclerp",
        "camera_optical",
        "map",
        InterpPolicy::ScLerp,
    );
    // The on-grid best case, its own labelled row (`0013`'s Q4): every edge takes
    // the exact-hit branch, so this is `bracket` plus a seqlock read. Not a gate
    // row; never quote it as depth-3 lookup latency.
    bench_pair_at(
        c,
        "lookup/depth3/sclerp/exact_hit",
        "imu_link",
        "map",
        InterpPolicy::ScLerp,
        fixture::NOW_NS,
    );
}

criterion_group!(benches, lookup_benches);
criterion_main!(benches);
