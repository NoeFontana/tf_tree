// Batch sampling: `at_many` with 1024 monotone stamps (`docs/PHASE1.md` §11.2),
// reported as ns/sample; each dynamic edge gallops from a resumable cursor.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    missing_docs,
    // Prints one line where it produces no rows, so a skip is not silent
    // (`docs/decisions/0060` §10.5).
    clippy::print_stderr
)]

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use tf_tree::{
    exp_se3, Capacity, EdgeCfg, InterpPolicy, Iso3, Layout, Stamp, SystemDomain, TreeBuilder,
};
use tf_tree_bench::{fixture, replay::TfStream};

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

    // The layout kernels (`docs/decisions/0005` Milestone B): compared against the
    // two-pass alternative (evaluate into `Iso3`, then convert). `Quat` is exactly
    // `Iso3`'s bytes since `0042`, so `into_quat_1024` is the kernel's own cost.
    // Raw nanoseconds: `at_many_into` takes `&[i64]` so FFI callers need no `Vec<Stamp>`.
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

    // `Layout::QuatTwist`: pose and body twist, thirteen `f64` a row. Its fold,
    // sampler and cursor branch are its own; against `into_quat_1024` it shows the
    // cost of the derivatives.
    let mut qt = vec![0.0f64; N * Layout::QuatTwist.elems()];
    group.bench_function("into_quat_twist_1024", |b| {
        b.iter(|| {
            plan.at_many_into::<SystemDomain>(
                &guard,
                black_box(&nanos),
                Layout::QuatTwist,
                &mut qt,
            )
            .expect("at_many_into");
            black_box(&qt);
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

    // The alternative, measured: `at_many` into an `Iso3` buffer, then a conversion pass.
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

/// Batches smaller than one chunk (`docs/decisions/0060` Decision A): every row
/// above is 1024 stamps and measures the steady state, not the chunked fold's
/// prologue. Step 1's stop point: no chunk size goes to step 2 whose `N < 64`
/// rows lose to the per-stamp fold by more than noise.
fn at_many_small(c: &mut Criterion) {
    let tree = fixture::build_tree().expect("build fixture");
    let (_writers, _samples) = fixture::spin_up(&tree).expect("populate history");

    let t = tree.frame("imu_link").expect("target");
    let s = tree.frame("map").expect("source");
    let plan = tree.plan(t, s).expect("plan");
    let guard = tree.guard();

    let now = fixture::NOW_NS;
    let lo = now - 100_000_000;

    let mut group = c.benchmark_group("at_many_small");
    // Not `Throughput::Elements`: at N = 1 the per-batch cost is the answer.
    for n in [1usize, 2, 3, 4, 8, 16, 63] {
        let stamps: Vec<Stamp> = (0..n)
            .map(|i| Stamp::from_nanos(lo + (now - lo) * i as i64 / n as i64))
            .collect();
        let nanos: Vec<i64> = stamps.iter().map(|s| s.nanos()).collect();
        let mut out = vec![Iso3::IDENTITY; n];
        let mut mat = vec![0.0f64; n * Layout::Mat4.elems()];

        group.bench_function(format!("at_many_{n}"), |b| {
            b.iter(|| {
                plan.at_many(&guard, black_box(&stamps), &mut out)
                    .expect("at_many");
                black_box(&out);
            });
        });
        group.bench_function(format!("into_mat4_{n}"), |b| {
            b.iter(|| {
                plan.at_many_into::<SystemDomain>(
                    &guard,
                    black_box(&nanos),
                    Layout::Mat4,
                    &mut mat,
                )
                .expect("at_many_into");
                black_box(&mat);
            });
        });
    }
    group.finish();
}

/// The plan and data shapes `docs/decisions/0060` step 2 owes: one dynamic step,
/// stamps off the publication grid, and a stationary edge.
///
/// * One dynamic step has the least arithmetic per stamp to amortise a chunk's
///   bookkeeping, and is `py_parity`'s batch shape.
/// * On the grid every bracket is an exact hit and `Interp::eval` never runs; off
///   it every bracket interpolates: different code paths.
/// * A stationary edge is §5's all-fallback regime, which `LerpSlerp` reads as
///   `h == 0` and `ScLerp` as a degenerate screw (§9.3), so the policies are kept apart.
fn at_many_shapes(c: &mut Criterion) {
    /// Samples per edge, and the stamp spacing.
    const SAMPLES: usize = 2048;
    const DT: i64 = 1_000_000;

    let mut group = c.benchmark_group("at_many_shapes");
    group.throughput(Throughput::Elements(N as u64));

    for (name, interp, moving) in [
        ("one_dyn_sclerp", InterpPolicy::ScLerp, true),
        ("one_dyn_lerpslerp", InterpPolicy::LerpSlerp, true),
        ("stationary_sclerp", InterpPolicy::ScLerp, false),
        ("stationary_lerpslerp", InterpPolicy::LerpSlerp, false),
    ] {
        let tree = TreeBuilder::new()
            .dynamic_edge(
                "map",
                "base",
                EdgeCfg::new(Capacity::slots(SAMPLES as u32 + 1)).interp(interp),
            )
            .build()
            .expect("build");
        let map = tree.frame("map").expect("map");
        let base = tree.frame("base").expect("base");
        {
            let w = tree.claim(base, map).expect("claim");
            for i in 0..SAMPLES as i64 {
                // Moving: a smooth screw. Stationary: one pose, republished.
                let f = if moving { i as f64 } else { 1.0 };
                w.push(
                    i * DT,
                    &exp_se3([
                        0.0003 * f,
                        -0.0002 * f,
                        0.0005 * f,
                        0.01 * f,
                        0.02 * f,
                        -0.01 * f,
                    ]),
                )
                .expect("push");
            }
        }
        let plan = tree.plan(base, map).expect("plan");
        let guard = tree.guard();

        // On the grid: every query is an exact hit. Off it: strictly inside a segment.
        let step = (SAMPLES as i64 - 2) / N as i64;
        let on: Vec<Stamp> = (0..N)
            .map(|i| Stamp::from_nanos(i as i64 * step * DT))
            .collect();
        let off: Vec<Stamp> = (0..N)
            .map(|i| Stamp::from_nanos(i as i64 * step * DT + DT / 3))
            .collect();

        let mut out = vec![Iso3::IDENTITY; N];
        let mut mat = vec![0.0f64; N * Layout::Mat4.elems()];

        group.bench_function(format!("{name}_on_grid_at_many_1024"), |b| {
            b.iter(|| {
                plan.at_many(&guard, black_box(&on), &mut out)
                    .expect("at_many");
                black_box(&out);
            });
        });
        group.bench_function(format!("{name}_off_grid_at_many_1024"), |b| {
            b.iter(|| {
                plan.at_many(&guard, black_box(&off), &mut out)
                    .expect("at_many");
                black_box(&out);
            });
        });
        let nanos: Vec<i64> = off.iter().map(|s| s.nanos()).collect();
        group.bench_function(format!("{name}_off_grid_into_mat4_1024"), |b| {
            b.iter(|| {
                plan.at_many_into::<SystemDomain>(
                    &guard,
                    black_box(&nanos),
                    Layout::Mat4,
                    &mut mat,
                )
                .expect("at_many_into");
                black_box(&mat);
            });
        });
    }
    group.finish();
}

/// The same two entry points over a recorded `/tf` stream (`docs/decisions/0060`
/// §9): on the one real recording in the tree, four of five dynamic edges never
/// move, and Decision A's stop rule applies to this data.
///
/// - `laser → odom_combined`: one static step and the one moving dynamic edge;
/// - `left_wheel_link → odom_combined`: adds a motionless dynamic step (§9.1).
fn at_many_recorded(c: &mut Criterion) {
    let path = std::path::Path::new("testdata/tfstream/indoor_atelier.tfstream");
    let stream = match TfStream::load(path) {
        Ok(s) => s,
        Err(e) => {
            // Run from the workspace root; elsewhere the other groups run, and
            // this prints instead of silently producing no rows (`0060` §10.5).
            eprintln!(
                "at_many_recorded: SKIPPED — {} unreadable: {e}",
                path.display()
            );
            return;
        }
    };
    let tree = stream
        .build_tree(InterpPolicy::ScLerp)
        .expect("replay tree");
    let (lo, hi) = stream.common_window().expect("common window");

    // 1024 monotone stamps, offset 1 ns so none lands on a knot (§9's `rate` sweep).
    let stamps: Vec<Stamp> = (0..N)
        .map(|i| Stamp::from_nanos(lo + 1 + (hi - lo - 2) * i as i64 / N as i64))
        .collect();
    let nanos: Vec<i64> = stamps.iter().map(|s| s.nanos()).collect();
    let guard = tree.guard();

    let mut group = c.benchmark_group("at_many_recorded");
    group.throughput(Throughput::Elements(N as u64));
    for (name, target, source) in [
        ("moving", "laser", "odom_combined"),
        ("mixed", "left_wheel_link", "odom_combined"),
    ] {
        let t = tree.frame(target).expect("target");
        let s = tree.frame(source).expect("source");
        let plan = tree.plan(t, s).expect("plan");
        let mut out = vec![Iso3::IDENTITY; N];
        let mut mat = vec![0.0f64; N * Layout::Mat4.elems()];

        group.bench_function(format!("{name}_at_many_1024"), |b| {
            b.iter(|| {
                plan.at_many(&guard, black_box(&stamps), &mut out)
                    .expect("at_many");
                black_box(&out);
            });
        });
        group.bench_function(format!("{name}_into_mat4_1024"), |b| {
            b.iter(|| {
                plan.at_many_into::<SystemDomain>(
                    &guard,
                    black_box(&nanos),
                    Layout::Mat4,
                    &mut mat,
                )
                .expect("at_many_into");
                black_box(&mat);
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    at_many,
    at_many_small,
    at_many_shapes,
    at_many_recorded
);
criterion_main!(benches);
