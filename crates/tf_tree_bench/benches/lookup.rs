// Hot-cache lookup benchmarks at depths 1, 3, and 6 over the shared fixture.
//
// Setup (tree build, history population, plan compile, guard) happens outside the
// timed closure, so each sample measures only plan evaluation against the seqlock
// rings — the depth-3 row is the one the go/no-go gate cares about (p50 < 150 ns
// ScLerp / < 100 ns LerpSlerp). These runs are NOT the official gate: that needs
// dedicated, core-pinned hardware.
//
// **This file is not `docs/PHASE5.md` §9.2's cross-crate row, and cannot be
// extended into it.** That row needs a denominator compiled *inside*
// `tf_tree_core`, and every body a bench target here compiles is codegen'd in
// `tf_tree_bench`; it also needs both halves to interpolate, and `bench_pair`
// below queries `fixture::NOW_NS`, which is a knot on all three dynamic edges of
// the depth-3 path (`docs/decisions/0013`). `crates/tf_tree_bench/src/embed.rs`
// states the three reasons in full, under "Why not `benches/lookup.rs`".
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use tf_tree::{InterpPolicy, Stamp};
use tf_tree_bench::fixture;

/// Build a populated fixture, compile the `target <- source` plan, and time its
/// hot evaluation at a fixed in-window stamp.
fn bench_pair(c: &mut Criterion, label: &str, target: &str, source: &str, interp: InterpPolicy) {
    let tree = fixture::build_tree_with(interp).expect("build fixture");
    let (_writers, _samples) = fixture::spin_up(&tree).expect("populate history");

    let t = tree.frame(target).expect("target frame");
    let s = tree.frame(source).expect("source frame");
    let plan = tree.plan(t, s).expect("compile plan");
    let guard = tree.guard();
    let stamp: Stamp = Stamp::from_nanos(fixture::NOW_NS);

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
}

criterion_group!(benches, lookup_benches);
criterion_main!(benches);
