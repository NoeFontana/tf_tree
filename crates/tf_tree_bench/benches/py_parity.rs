// The native counterpart of the Python batch path, on the *identical* fixture.
//
// `docs/PHASE3.md` §12.2 criterion 2: "`at_many` at n = 4096 within 1.3x of
// native per-sample cost". That comparison is only meaningful against the same
// tree — `benches/at_many.rs` uses the deep mobile-robot fixture, so its
// ns/sample is not the number the Python figure should be divided by.
//
// So: one dynamic edge, 2000 samples at 1 ms, `at_many_into(Layout::Mat4)` —
// exactly what `Plan.at(stamps)` calls through to.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Layout, SystemDomain, TreeBuilder};

fn py_parity(c: &mut Criterion) {
    let tree = TreeBuilder::new()
        .default_interp(InterpPolicy::LerpSlerp)
        .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(1024)))
        .build()
        .expect("build");

    let child = tree.frame("base").unwrap();
    let parent = tree.frame("map").unwrap();
    {
        let p = tree.claim(child, parent).expect("claim");
        for k in 0..2000i64 {
            let iso = tf_tree::Iso3::new(
                tf_tree::Quat {
                    w: 1.0,
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                },
                tf_tree::Vec3::new(k as f64, 0.0, 0.0),
            );
            p.push(k * 1_000_000, &iso).expect("push");
        }
    }

    let plan = tree.plan(parent, child).expect("plan");
    let guard = tree.guard();

    let mut group = c.benchmark_group("py_parity");
    for n in [64usize, 4096] {
        let lo = 1_000_000_000i64;
        let hi = 1_998_000_000i64;
        let stamps: Vec<i64> = (0..n)
            .map(|i| lo + (hi - lo) * i as i64 / n as i64)
            .collect();
        let mut out = vec![0.0f64; n * Layout::Mat4.elems()];

        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(format!("native_mat4_{n}"), |b| {
            b.iter(|| {
                plan.at_many_into::<SystemDomain>(
                    &guard,
                    black_box(&stamps),
                    Layout::Mat4,
                    &mut out,
                )
                .expect("at_many_into");
                black_box(&out);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, py_parity);
criterion_main!(benches);
