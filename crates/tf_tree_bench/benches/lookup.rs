// Hot-cache lookup benchmarks at depths 1, 3, and 6 over the shared fixture.
//
// Setup (tree build, history population, plan compile, guard) happens outside the
// timed closure, so each sample measures only plan evaluation against the seqlock
// rings — the depth-3 row is the one the go/no-go gate cares about. These runs
// are NOT the official gate: that needs dedicated, core-pinned hardware.
//
// **The stamp is `fixture::QUERY_NS`, not `fixture::NOW_NS`, and that is the
// whole subject of `docs/decisions/0013`.** `NOW_NS` is a knot on all four
// dynamic grids, so every edge took `SampleRing::sample`'s exact-hit branch and
// `I::eval` never ran: what this file used to report as "depth-3 lookup" was
// `bracket` plus the seqlock read, with the interpolator — the thing the gate
// exists to bound — absent. `QUERY_NS` is 500 µs off every grid; `fixture`'s
// `the_latency_query_stamp_is_off_every_dynamic_grid` pins that.
//
// **This file is still not `docs/PHASE5.md` §9.2's cross-crate row, and cannot
// be extended into it.** That row needs a denominator compiled *inside*
// `tf_tree_core`, and every body a bench target here compiles is codegen'd in
// `tf_tree_bench`. `crates/tf_tree_bench/src/embed.rs` states the reasons in
// full, under "Why not `benches/lookup.rs`".
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
    // The on-grid best case, kept as its own labelled row by `0013`'s Q4.
    //
    // `NOW_NS` is an exact multiple of all four dynamic periods, so every edge
    // takes `SampleRing::sample`'s exact-hit branch and `I::eval` never runs.
    // This row is therefore `bracket` plus a seqlock read and nothing else —
    // which is what the depth-3 gate row unknowingly measured for the life of
    // the gate, ~4.7x faster than the off-grid row directly above it.
    //
    // It exists so that gap is a documented property of the engine rather than
    // a trap for whoever next picks a round-numbered stamp. **It is not a gate
    // row and must never be quoted as depth-3 lookup latency**; the label says
    // `exact_hit` for that reason.
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
