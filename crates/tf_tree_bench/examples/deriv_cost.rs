//! Cost of `at_with_derivatives` against `at` — `docs/PHASE4.md` §7, last row.
//!
//! §2.3 predicts "roughly 2× a plain lookup", and §2.1 claims the derivative adds
//! **no transcendental call**. Both are checkable here, and neither should be
//! taken on faith: §2.3's original cost model ("the first derivative costs one
//! scalar multiply") was false against the shipped code, which is why the section
//! now carries an amendment.
//!
//! Three things are measured, at two levels, because a ratio at plan level mixes
//! two different costs and would hide which one moved:
//!
//! * **interpolation level** — `ScLerp::eval` vs `ScLerp::eval_with_twist` on
//!   pre-built pose pairs. This isolates ξ-recovery: one `sqrt` plus `φ`, where
//!   `φ` is a second `sqrt` on the small-angle branch and the already-computed
//!   `atan2` on the large-arc branch. Swept across the arc regimes so the
//!   branch's payoff is visible rather than asserted.
//! * **plan level** — `Plan::at` vs `Plan::at_with_derivatives` at depth 1, 3
//!   and 8, which adds one adjoint application per step (two `Quat::rotate` and
//!   one `Vec3::cross`). The slope across depth is the adjoint's real cost.
//! * **the §7 gate row** — the depth-3 ratio, which is what the spec asks for.
//!
//! Run pinned; unpinned runs migrate cores and swing by >30%:
//! `taskset -c 2 cargo run --release -p tf_tree_bench --example deriv_cost`
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::expect_used)]

use std::hint::black_box;
use std::time::Instant;

use tf_tree::{
    exp_se3, Capacity, EdgeCfg, Interp, Iso3, Quat, ScLerp, Stamp, SystemDomain, Tree, TreeBuilder,
    Vec3,
};

const N: usize = 4096;
const ROUNDS: usize = 41;

fn axis_angle(theta: f64, x: f64, y: f64, z: f64) -> Quat {
    let half = 0.5 * theta;
    let s = half.sin();
    Quat::new(half.cos(), s * x, s * y, s * z)
}

/// Same pair construction as `interp_cost`, so the two benchmarks' numbers are
/// directly comparable.
fn pairs(theta: f64) -> Vec<(Iso3, Iso3)> {
    (0..N)
        .map(|i| {
            let u = i as f64 * 0.000_613;
            let a = Iso3::new(
                axis_angle(0.3 + u, 0.267_261, 0.534_522, 0.801_784),
                Vec3::new(1.5 + u, -0.7 + 2.0 * u, 3.1 - u),
            );
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

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn bench(mut run: impl FnMut() -> f64, per: usize) -> f64 {
    for _ in 0..8 {
        black_box(run());
    }
    median(
        (0..ROUNDS)
            .map(|_| {
                let t0 = Instant::now();
                black_box(run());
                t0.elapsed().as_nanos() as f64 / per as f64
            })
            .collect(),
    )
}

fn s_at(i: usize) -> f64 {
    (i as f64).mul_add(1.0 / N as f64, 0.5).fract() * 0.98 + 0.01
}

/// A chain `f0 -> f1 -> ... -> fdepth`, every edge dynamic and ScLerp, each with a
/// distinct non-commuting screw so no step degenerates.
fn chain(depth: usize) -> (Tree, Vec<String>) {
    let names: Vec<String> = (0..=depth).map(|i| format!("f{i}")).collect();
    let cfg = EdgeCfg::new(Capacity::slots(1024));
    let mut b = TreeBuilder::new();
    for i in 0..depth {
        let p: &'static str = Box::leak(names[i].clone().into_boxed_str());
        let c: &'static str = Box::leak(names[i + 1].clone().into_boxed_str());
        b = b.dynamic_edge(p, c, cfg);
    }
    let tree = b.build().expect("build");
    for i in 0..depth {
        let parent = tree.frame(&names[i]).unwrap();
        let child = tree.frame(&names[i + 1]).unwrap();
        let w = tree.claim(child, parent).unwrap();
        let k = i as f64 + 1.0;
        for j in 0..512i64 {
            let f = j as f64;
            w.push(
                j * 1_000_000,
                &exp_se3([
                    0.004 * k * f,
                    -0.003 * f,
                    0.002 * k * f,
                    0.05 * f,
                    -0.02 * k * f,
                    0.01 * f,
                ]),
            )
            .unwrap();
        }
        core::mem::forget(w);
    }
    (tree, names)
}

fn main() {
    println!("at_with_derivatives vs at — PHASE4 §7");
    println!("=====================================");
    println!("{N} items/round, median of {ROUNDS} rounds\n");

    // ---- interpolation level: what xi-recovery actually costs ----
    println!("interpolation level — ScLerp::eval vs eval_with_twist");
    println!(
        "{:>16} {:>12} {:>10} {:>12} {:>8}",
        "regime", "rel. angle", "eval ns", "w/ twist ns", "ratio"
    );
    for (label, theta) in [
        ("adjacent 1 kHz", 3.0e-3),
        ("200 Hz", 1.6e-2),
        ("50 Hz", 6.3e-2),
        ("slow 10 Hz", 3.14e-1),
        ("large arc", 1.5),
        ("near-identity", 1.0e-9),
    ] {
        let d = pairs(theta);
        let plain = bench(
            || {
                let mut acc = 0.0;
                for (i, (a, b)) in d.iter().enumerate() {
                    let r = <ScLerp as Interp>::eval(a, b, black_box(s_at(i)));
                    acc += r.t.x + r.q.w;
                }
                acc
            },
            N,
        );
        let deriv = bench(
            || {
                let mut acc = 0.0;
                for (i, (a, b)) in d.iter().enumerate() {
                    let (r, w) = ScLerp::eval_with_twist(a, b, black_box(s_at(i)));
                    acc += r.t.x + r.q.w + w.omega.x + w.v.z;
                }
                acc
            },
            N,
        );
        println!(
            "{label:>16} {theta:>12.3e} {plain:>10.2} {deriv:>12.2} {:>8.2}",
            deriv / plain
        );
    }

    // ---- plan level: the adjoint per step ----
    println!("\nplan level — Plan::at vs Plan::at_with_derivatives");
    println!(
        "{:>8} {:>10} {:>14} {:>8} {:>14}",
        "depth", "at ns", "at_deriv ns", "ratio", "delta/step ns"
    );
    let row = |depth: usize| -> (f64, f64) {
        let (tree, names) = chain(depth);
        let src = tree.frame(&names[0]).unwrap();
        let dst = tree.frame(&names[depth]).unwrap();
        let plan = tree.plan(src, dst).unwrap();
        let g = tree.guard();
        // Stamps stay strictly inside the published span and sweep non-monotonically
        // enough that the galloping cursor is not what is being measured.
        let stamps: Vec<i64> = (0..N)
            .map(|i| 10_000_000 + ((i * 7919) % 480_000_000) as i64)
            .collect();
        let plain = bench(
            || {
                let mut acc = 0.0;
                for &t in &stamps {
                    acc += plan
                        .at(&g, Stamp::<SystemDomain>::from_nanos(black_box(t)))
                        .unwrap()
                        .t
                        .x;
                }
                acc
            },
            N,
        );
        let deriv = bench(
            || {
                let mut acc = 0.0;
                for &t in &stamps {
                    let s = plan
                        .at_with_derivatives(&g, Stamp::<SystemDomain>::from_nanos(black_box(t)))
                        .unwrap();
                    acc += s.pose.t.x + s.twist.omega.x;
                }
                acc
            },
            N,
        );
        println!(
            "{depth:>8} {plain:>10.1} {deriv:>14.1} {:>8.2} {:>14.2}",
            deriv / plain,
            (deriv - plain) / depth as f64
        );
        (plain, deriv)
    };
    row(1);
    let (p3, d3) = row(3);
    row(8);

    // ---- endpoints: the two most-queried stamps on any edge ----
    // s == 0.0 is an exact hit on a published sample; s == 1.0 is `t == t_new`,
    // which is what every `latest` query lands on. Both discard the screw power,
    // so `eval_with_twist` skips it -- this is the row that shows whether that
    // shortcut is real.
    println!("\nendpoints — eval_with_twist at s in {{0, 1}} vs interior");
    println!(
        "{:>16} {:>12} {:>12} {:>10}",
        "regime", "interior ns", "endpoint ns", "saved"
    );
    for (label, theta) in [("adjacent 1 kHz", 3.0e-3), ("large arc", 1.5)] {
        let d = pairs(theta);
        let interior = bench(
            || {
                let mut acc = 0.0;
                for (i, (a, b)) in d.iter().enumerate() {
                    let (r, w) = ScLerp::eval_with_twist(a, b, black_box(s_at(i)));
                    acc += r.t.x + w.omega.x;
                }
                acc
            },
            N,
        );
        let endpoint = bench(
            || {
                let mut acc = 0.0;
                for (i, (a, b)) in d.iter().enumerate() {
                    let s = if i % 2 == 0 { 0.0 } else { 1.0 };
                    let (r, w) = ScLerp::eval_with_twist(a, b, black_box(s));
                    acc += r.t.x + w.omega.x;
                }
                acc
            },
            N,
        );
        println!(
            "{label:>16} {interior:>12.2} {endpoint:>12.2} {:>9.1}%",
            100.0 * (interior - endpoint) / interior
        );
    }

    println!("\n§7 gate row — at_with_derivatives vs at, depth 3");
    println!("  measured ratio {:.2}x   (spec expects ~2x)", d3 / p3);
    println!(
        "  {}",
        if d3 / p3 <= 2.5 {
            "PASS — within the stated expectation"
        } else {
            "OVER — re-derive §2.3's cost model before quoting it"
        }
    );
}
