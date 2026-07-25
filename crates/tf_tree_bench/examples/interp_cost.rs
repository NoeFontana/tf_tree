//! Cost of one interpolation, isolated from the tree.
//!
//! `cost_model` measures `Plan::at`, where `sample::<LerpSlerp>` and
//! `sample::<ScLerp>` are monomorphized into the *same* hot function behind a
//! `match` on the policy byte. Changing the size of either one therefore
//! relocates the other's code, and a ±15% swing in the untouched policy is a
//! routine artifact of that. It makes `cost_model` unable to attribute a change
//! in the interpolation math to the math.
//!
//! This benchmark calls `Interp::eval` directly on pre-built pose pairs, so a
//! number here moves only when the interpolation itself moves.
//!
//! Three regimes, because the two policies branch on arc size:
//!
//! * **adjacent** — the case that dominates: two samples one 1 kHz tick apart on
//!   a body rotating at a brisk 180 °/s, so ~3 mrad. Both policies take their
//!   transcendental-free series path.
//! * **slow-rate** — a 10 Hz edge on the same body, ~314 mrad. Past the 0.15
//!   half-angle threshold, so both fall to the exact `acos`/`atan2` path.
//! * **near-identity** — a stationary robot. Formerly `screw_pow`'s
//!   `SCREW_SMALL` fallback, which routed every such lookup through the full
//!   `log_se3`/`exp_se3` reference form.
//!
//! Run pinned — unpinned runs migrate cores and swing by >30%:
//! `taskset -c 2 cargo run --release -p tf_tree_bench --example interp_cost`
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::hint::black_box;
use std::time::Instant;

use tf_tree::{Interp, Iso3, LerpSlerp, Quat, ScLerp, Vec3};

/// Pose pairs per timed round.
const N: usize = 4096;
/// Timed rounds; the median is reported.
const ROUNDS: usize = 41;

/// A pair of poses whose *relative* rotation is `theta` about a tumbling axis,
/// with a metre-scale relative translation. Both endpoints are themselves
/// arbitrary rigid transforms, so nothing degenerates into an identity-times-x
/// special case the optimizer could exploit.
fn pairs(theta: f64) -> Vec<(Iso3, Iso3)> {
    (0..N)
        .map(|i| {
            let u = i as f64 * 0.000_613;
            // An arbitrary, well-conditioned base pose.
            let a = Iso3::new(
                axis_angle(0.3 + u, 0.267_261, 0.534_522, 0.801_784),
                Vec3::new(1.5 + u, -0.7 + 2.0 * u, 3.1 - u),
            );
            // Relative rotation of exactly `theta`, about an axis that walks so
            // successive pairs do not share a branch history.
            let (sx, sy, sz) = ((u * 7.0).cos(), (u * 5.0).sin(), (u * 3.0 + 1.0).cos());
            let n = (sx * sx + sy * sy + sz * sz).sqrt();
            let rel = Iso3::new(
                axis_angle(theta, sx / n, sy / n, sz / n),
                Vec3::new(0.01 * u, -0.02 + u, 0.03 * u),
            );
            (a, a * rel)
        })
        .collect()
}

fn axis_angle(theta: f64, x: f64, y: f64, z: f64) -> Quat {
    let half = 0.5 * theta;
    let s = half.sin();
    Quat::new(half.cos(), s * x, s * y, s * z)
}

/// Median nanoseconds per `eval` over `ROUNDS` rounds.
fn time<I: Interp>(data: &[(Iso3, Iso3)]) -> f64 {
    let run = || {
        let mut acc = 0.0f64;
        for (i, (a, b)) in data.iter().enumerate() {
            // s walks over (0,1) exclusive: the endpoint short-circuits in
            // `eval` would otherwise skip the work being measured.
            let s = (i as f64).mul_add(1.0 / N as f64, 0.5).fract() * 0.98 + 0.01;
            let r = I::eval(a, b, black_box(s));
            acc += r.t.x + r.q.w;
        }
        acc
    };
    for _ in 0..8 {
        black_box(run());
    }
    let mut per_round: Vec<f64> = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        black_box(run());
        per_round.push(t0.elapsed().as_nanos() as f64 / data.len() as f64);
    }
    per_round.sort_by(f64::total_cmp);
    per_round[per_round.len() / 2]
}

fn main() {
    println!("interpolation cost, isolated from the tree");
    println!("==========================================");
    println!("{N} pairs/round, median of {ROUNDS} rounds\n");
    println!(
        "{:>16} {:>12} {:>12} {:>12}",
        "regime", "rel. angle", "ScLerp ns", "LerpSlerp ns"
    );

    for (label, theta) in [
        ("adjacent 1 kHz", 3.0e-3),
        ("200 Hz", 1.6e-2),
        ("50 Hz", 6.3e-2),
        ("slow 10 Hz", 3.14e-1),
        ("large arc", 1.5),
        ("near-identity", 1.0e-9),
        ("stationary", 0.0),
    ] {
        let data = pairs(theta);
        let sc = time::<ScLerp>(&data);
        let ls = time::<LerpSlerp>(&data);
        println!("{label:>16} {theta:>12.3e} {sc:>12.1} {ls:>12.1}");
    }
}
