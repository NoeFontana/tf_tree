//! Is the plan fold's adjoint worth restructuring? — a tuning probe.
//!
//! `deriv_cost` measures `at_with_derivatives` at **+21 ns per plan step** over
//! `at`, flat across depth 1/3/8, while ξ-recovery costs only **1.6 ns**. So the
//! per-step cost is almost entirely the adjoint, and that is where any tuning has
//! to happen.
//!
//! `Iso3::adjoint_inv` is two [`tf_tree::Quat::rotate`] calls and a cross product
//! — about 40 flops. At 21 ns that is ~2 flops/ns, far off this core's
//! throughput, which says the chain is **latency**-bound rather than
//! throughput-bound. `Quat::rotate` is already the optimized Rodrigues form, but
//! its dependency chain is deep: `cross → scale → scale → add → cross → add`,
//! roughly five dependent steps, and the second rotation's input depends on a
//! cross product of its own.
//!
//! The candidate is a **matrix form**: build `R` from the quaternion once (12
//! independent flops), then two mat-vecs whose nine products are mutually
//! independent. Same arithmetic count, far more instruction-level parallelism.
//!
//! This measures both, plus a "sequential" variant that threads each result into
//! the next input, because the plan fold *is* sequential — a throughput number
//! measured on independent inputs would flatter both forms and answer the wrong
//! question.
//!
//! Run pinned:
//! `taskset -c 2 cargo run --release -p tf_tree_bench --example adjoint_cost`
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::hint::black_box;
use std::time::Instant;

use tf_tree::{exp_se3, Iso3, Twist, Vec3};

const N: usize = 4096;
const ROUNDS: usize = 41;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn bench(mut run: impl FnMut() -> f64) -> f64 {
    for _ in 0..8 {
        black_box(run());
    }
    median(
        (0..ROUNDS)
            .map(|_| {
                let t0 = Instant::now();
                black_box(run());
                t0.elapsed().as_nanos() as f64 / N as f64
            })
            .collect(),
    )
}

/// Rotation matrix from a unit quaternion. Twelve flops, all independent.
#[inline(always)]
fn rot_matrix(t: &Iso3) -> [[f64; 3]; 3] {
    let (w, x, y, z) = (t.q.w, t.q.x, t.q.y, t.q.z);
    let (xx, yy, zz) = (x * x, y * y, z * z);
    let (xy, xz, yz) = (x * y, x * z, y * z);
    let (wx, wy, wz) = (w * x, w * y, w * z);
    [
        [1.0 - 2.0 * (yy + zz), 2.0 * (xy - wz), 2.0 * (xz + wy)],
        [2.0 * (xy + wz), 1.0 - 2.0 * (xx + zz), 2.0 * (yz - wx)],
        [2.0 * (xz - wy), 2.0 * (yz + wx), 1.0 - 2.0 * (xx + yy)],
    ]
}

/// `Rᵀ·v` — the transpose apply, which is what `Ad(T⁻¹)` needs.
#[inline(always)]
fn mt_vec(r: &[[f64; 3]; 3], v: Vec3) -> Vec3 {
    Vec3::new(
        r[0][0] * v.x + r[1][0] * v.y + r[2][0] * v.z,
        r[0][1] * v.x + r[1][1] * v.y + r[2][1] * v.z,
        r[0][2] * v.x + r[1][2] * v.y + r[2][2] * v.z,
    )
}

/// Candidate `Ad(T⁻¹)` via a matrix built once.
#[inline(always)]
fn adjoint_inv_matrix(t: &Iso3, x: &Twist) -> Twist {
    let r = rot_matrix(t);
    Twist::new(mt_vec(&r, x.omega), mt_vec(&r, x.v.sub(t.t.cross(x.omega))))
}

fn main() {
    println!("adjoint form — quaternion vs matrix");
    println!("===================================");
    println!("{N} applications/round, median of {ROUNDS} rounds\n");

    let poses: Vec<Iso3> = (0..N)
        .map(|i| {
            let f = i as f64 * 0.001;
            exp_se3([
                0.3 + f,
                -0.7 + 2.0 * f,
                0.2 - f,
                1.1 + f,
                -0.5 + f,
                3.7 - 2.0 * f,
            ])
        })
        .collect();
    let twists: Vec<Twist> = (0..N)
        .map(|i| {
            let f = i as f64 * 0.0013;
            Twist::new(
                Vec3::new(0.1 + f, -0.2 + f, 0.3 - f),
                Vec3::new(1.0 - f, 2.0 + f, -3.0 + f),
            )
        })
        .collect();

    // Correctness first: a tuning candidate that disagrees is not a candidate.
    let mut worst = 0.0f64;
    for (p, x) in poses.iter().zip(&twists) {
        let a = p.adjoint_inv(x);
        let b = adjoint_inv_matrix(p, x);
        worst = worst.max(a.sub(b).amax() / b.amax());
    }
    println!("agreement (quat vs matrix): {worst:.3e}");
    assert!(worst < 1e-13, "the matrix form does not agree");

    // --- independent inputs: pure throughput, lots of ILP available ---
    let quat_tp = bench(|| {
        let mut acc = 0.0;
        for (p, x) in poses.iter().zip(&twists) {
            acc += p.adjoint_inv(black_box(x)).omega.x;
        }
        acc
    });
    let mat_tp = bench(|| {
        let mut acc = 0.0;
        for (p, x) in poses.iter().zip(&twists) {
            acc += adjoint_inv_matrix(p, black_box(x)).omega.x;
        }
        acc
    });

    // --- sequential: each result feeds the next, as the plan fold does ---
    let quat_seq = bench(|| {
        let mut v = twists[0];
        for p in &poses {
            v = p.adjoint_inv(&v);
        }
        v.omega.x + v.v.z
    });
    let mat_seq = bench(|| {
        let mut v = twists[0];
        for p in &poses {
            v = adjoint_inv_matrix(p, &v);
        }
        v.omega.x + v.v.z
    });

    println!(
        "\n{:>14} {:>12} {:>12} {:>8}",
        "regime", "quat ns", "matrix ns", "speedup"
    );
    println!(
        "{:>14} {quat_tp:>12.2} {mat_tp:>12.2} {:>8.2}",
        "independent",
        quat_tp / mat_tp
    );
    println!(
        "{:>14} {quat_seq:>12.2} {mat_seq:>12.2} {:>8.2}",
        "sequential",
        quat_seq / mat_seq
    );
    println!(
        "\nThe fold is sequential, so the second row is the one that decides.\n\
         A speedup below ~1.15x is not worth a second code path."
    );
}
