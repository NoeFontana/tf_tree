// Single-writer push throughput (`docs/PHASE1.md` §11.2 *Measurements* — ns/push).
//
// Claims one dynamic edge and times `Publisher::push` with monotone stamps. The
// pose is precomputed so the loop measures only the seqlock publish protocol.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::cell::Cell;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use tf_tree::Iso3;
use tf_tree_bench::fixture;

fn push(c: &mut Criterion) {
    let tree = fixture::build_tree().expect("build fixture");
    let parent = tree.frame("base_link").expect("parent");
    let child = tree.frame("imu_link").expect("child");
    let publisher = tree.claim(child, parent).expect("claim imu edge");

    let iso = fixture::dynamic_pose(2.0, 0);
    // A monotone stamp source; the ring wraps freely as it advances.
    let stamp = Cell::new(0i64);

    c.bench_function("push/single_writer", |b| {
        b.iter(|| {
            let s = stamp.get();
            stamp.set(s + 1_000_000);
            publisher.push(black_box(s), black_box(&iso)).expect("push");
        });
    });

    // Keep the publisher alive across the whole measurement.
    let _ = black_box::<&Iso3>(&iso);
    drop(publisher);
}

criterion_group!(benches, push);
criterion_main!(benches);
