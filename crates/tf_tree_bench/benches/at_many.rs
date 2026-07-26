// Batch sampling: `at_many` with 1024 monotone stamps (`docs/PHASE1.md` §11.2
// *Measurements* — reported as ns/sample). Monotone input lets each dynamic edge
// gallop from a resumable cursor, so this is the O(1)-amortized path.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use tf_tree::{Iso3, Layout, Stamp, SystemDomain};
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

    // The layout kernels (`docs/decisions/0005` Milestone B). The comparison
    // that matters is not kernel-vs-kernel but **kernel vs. the two-pass
    // alternative** a consumer is otherwise forced into: evaluate into an
    // `Iso3` buffer, then convert. `Iso3` is 64 B with 8 of padding and no
    // layout below shares its stride, so that second pass is not avoidable by
    // any amount of care on the caller's side.
    // Raw nanoseconds: `at_many_into` takes `&[i64]` so an FFI caller does not
    // have to allocate a `Vec<Stamp>` to use it.
    let nanos: Vec<i64> = stamps.iter().map(|s| s.nanos()).collect();
    let mut mat = vec![0.0f64; N * Layout::Mat4.elems()];
    group.bench_function("into_mat4_1024", |b| {
        b.iter(|| {
            plan.at_many_into::<SystemDomain>(&guard, black_box(&nanos), Layout::Mat4, &mut mat)
                .expect("at_many_into");
            black_box(&mat);
        });
    });

    let mut quat = vec![0.0f64; N * Layout::Quat.elems()];
    group.bench_function("into_quat_1024", |b| {
        b.iter(|| {
            plan.at_many_into::<SystemDomain>(&guard, black_box(&nanos), Layout::Quat, &mut quat)
                .expect("at_many_into");
            black_box(&quat);
        });
    });

    let mut aff = vec![0.0f32; N * Layout::Affine32.elems()];
    group.bench_function("into_affine32_1024", |b| {
        b.iter(|| {
            plan.at_many_into_f32::<SystemDomain>(
                &guard,
                black_box(&nanos),
                Layout::Affine32,
                &mut aff,
            )
            .expect("at_many_into_f32");
            black_box(&aff);
        });
    });

    // The alternative, measured rather than asserted: `at_many` into an `Iso3`
    // buffer followed by a conversion pass. This is what every consumer that
    // wants a matrix pays today.
    group.bench_function("two_pass_mat4_1024", |b| {
        b.iter(|| {
            plan.at_many(&guard, black_box(&stamps), &mut out)
                .expect("at_many");
            for (i, iso) in out.iter().enumerate() {
                let q = iso.q;
                let (w, x, y, z) = (q.w, q.x, q.y, q.z);
                let (xx, yy, zz) = (x * x, y * y, z * z);
                let (xy, xz, yz) = (x * y, x * z, y * z);
                let (wx, wy, wz) = (w * x, w * y, w * z);
                let m = &mut mat[i * 16..(i + 1) * 16];
                m[0] = 1.0 - 2.0 * (yy + zz);
                m[1] = 2.0 * (xy - wz);
                m[2] = 2.0 * (xz + wy);
                m[3] = iso.t.x;
                m[4] = 2.0 * (xy + wz);
                m[5] = 1.0 - 2.0 * (xx + zz);
                m[6] = 2.0 * (yz - wx);
                m[7] = iso.t.y;
                m[8] = 2.0 * (xz - wy);
                m[9] = 2.0 * (yz + wx);
                m[10] = 1.0 - 2.0 * (xx + yy);
                m[11] = iso.t.z;
                m[12] = 0.0;
                m[13] = 0.0;
                m[14] = 0.0;
                m[15] = 1.0;
            }
            black_box(&mat);
        });
    });

    group.finish();
}

criterion_group!(benches, at_many);
criterion_main!(benches);
