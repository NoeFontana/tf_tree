// What the `docs/decisions/0036` receipt-time sampler costs a publisher —
// measured as a **paired delta in one process**, which is the only form of the
// number this host can produce.
//
// `benches/push.rs` times `EdgeWriter::push` and reports ns/push. That is the
// right shape for `docs/PHASE1.md` §11.2's row and the wrong shape for this
// question: `bench_report`'s fitness probe rejects this host outright (SMT on,
// 8 logical CPUs over 4 physical cores, no readable frequency governor), and
// two `cargo bench` invocations minutes apart drift by more than the effect.
// Measured, on the change this file exists for: the same unsampled push read
// 5.94 ns in one run and 4.82 ns in the next, while the effect under test is
// ~1.1 ns. A before/after taken across two runs said **+47%**; the paired form
// below says **+23%**, four runs, and it is the one that is true.
//
// The two arms differ by the sampler and nothing else:
//
// * `a_publisher_only` calls `Publisher::push` through `EdgeWriter`'s `Deref`.
// * `b_edgewriter_sampled` calls the inherent `EdgeWriter::push`.
//
// **Reaching `Publisher::push` directly is the one thing `EdgeWriter`'s doc
// tells you not to do**, because it skips the post-`fork` check — and that is
// exactly why it is the control: it is `EdgeWriter::push` minus the code under
// test. It is sound here and only here: one process, no `fork`, and this crate
// builds without `shm`, where the check the arm skips does not exist at all.
// **Do not copy this call shape into anything that is not a control arm.**
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::cell::Cell;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use tf_tree_bench::fixture;

fn push_sampler(c: &mut Criterion) {
    let tree = fixture::build_tree().expect("build fixture");
    let parent = tree.frame("base_link").expect("parent");
    let child = tree.frame("imu_link").expect("child");
    let w = tree.claim(child, parent).expect("claim imu edge");
    let iso = fixture::dynamic_pose(2.0, 0);
    // A monotone stamp source shared by both arms, so neither pays a different
    // ring-wrap pattern than the other.
    let stamp = Cell::new(0i64);

    // The fixture declares no `nominal_rate_hz`, so this edge samples at
    // `DEFAULT_SAMPLE_EVERY` — 1 push in 1024. That is the configuration a tree
    // built without a topology file gets, which is the common one.
    let mut g = c.benchmark_group("push_sampler");
    g.bench_function("a_publisher_only", |b| {
        b.iter(|| {
            let s = stamp.get();
            stamp.set(s + 1_000_000);
            let p: &tf_tree::Publisher<'_> = &w;
            p.push(black_box(s), black_box(&iso)).expect("push");
        });
    });
    g.bench_function("b_edgewriter_sampled", |b| {
        b.iter(|| {
            let s = stamp.get();
            stamp.set(s + 1_000_000);
            w.push(black_box(s), black_box(&iso)).expect("push");
        });
    });
    g.finish();
}

criterion_group!(benches, push_sampler);
criterion_main!(benches);
