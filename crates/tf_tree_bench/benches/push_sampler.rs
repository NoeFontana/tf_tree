// What the `docs/decisions/0036` clock-offset sampler costs a publisher, as a
// paired delta in one process: two `cargo bench` runs drift by more than the
// effect on this host, unlike `benches/push.rs`'s ns/push.
//
// * `a_publisher_only` calls `Publisher::push` through `EdgeWriter`'s `Deref`.
// * `b_edgewriter_sampled` calls the inherent `EdgeWriter::push`.
//
// Calling `Publisher::push` skips the post-`fork` check, which is what makes it
// the control (`EdgeWriter::push` minus the code under test). Sound only here:
// one process, no `fork`. Do not copy this shape outside a control arm.
//
// Only the right control without `shm`, so the bench refuses to run with it:
// under `shm` the fork check adds +0.195 ns to one arm. A run-time refusal, not
// `compile_error!`, because the `--features shm` clippy row compiles this file.
// `clippy::panic` is allowed for that one refusal.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::cell::Cell;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use tf_tree_bench::fixture;

fn push_sampler(c: &mut Criterion) {
    // Not `compile_error!` (breaks the `--features shm` clippy row) nor
    // `assert!(!cfg!(..))` (clippy rejects it): a plain `if` over a `const`.
    const CONTAMINATED: bool = cfg!(feature = "shm");
    if CONTAMINATED {
        panic!(
            "push_sampler measures EdgeWriter::push against Publisher::push, and \
         with `shm` on those two differ by the post-fork check (+0.195 ns) as \
         well as by the sampler under test — 18% of the effect, reported as \
         part of it. Run `just push-sampler-cost`, which does not pass the \
             feature."
        );
    }
    let tree = fixture::build_tree().expect("build fixture");
    let parent = tree.frame("base_link").expect("parent");
    let child = tree.frame("imu_link").expect("child");
    let w = tree.claim(child, parent).expect("claim imu edge");
    let iso = fixture::dynamic_pose(2.0, 0);
    // A monotone stamp source shared by both arms.
    let stamp = Cell::new(0i64);

    // No `nominal_rate_hz`, so the edge samples at `DEFAULT_SAMPLE_EVERY` (1 in 1024),
    // the common no-topology-file configuration.
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
