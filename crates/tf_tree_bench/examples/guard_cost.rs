//! Is `Tree::guard()` per call, or the fold, the Python scalar path's cost?
#![allow(missing_docs, clippy::unwrap_used, clippy::print_stdout)]
use std::time::Instant;
use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Stamp, SystemDomain, TreeBuilder};

fn main() {
    let tree = TreeBuilder::new()
        .default_interp(InterpPolicy::LerpSlerp)
        .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(1024)))
        .dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(1024)))
        .dynamic_edge("base", "cam", EdgeCfg::new(Capacity::slots(1024)))
        .build()
        .unwrap();
    for (c, p) in [("odom", "map"), ("base", "odom"), ("cam", "base")] {
        let ci = tree.frame(c).unwrap();
        let pi = tree.frame(p).unwrap();
        let w = tree.claim(ci, pi).unwrap();
        for i in 0..200i64 {
            w.push(
                1_000_000 + i * 1_000_000,
                &tf_tree_math::exp_se3([0.0, 0.0, 0.0, 0.001 * i as f64, 0.0, 0.0]),
            )
            .unwrap();
        }
        core::mem::forget(w);
    }
    let t = tree.frame("map").unwrap();
    let s = tree.frame("cam").unwrap();
    let plan = tree.plan(t, s).unwrap();
    let stamp = Stamp::<SystemDomain>::from_nanos(100_500_000);
    const N: u32 = 3_000_000;

    let mut best = f64::MAX;
    for _ in 0..7 {
        let g = tree.guard();
        let t0 = Instant::now();
        for _ in 0..N {
            std::hint::black_box(plan.at(&g, stamp).unwrap());
        }
        best = best.min(t0.elapsed().as_nanos() as f64 / f64::from(N));
    }
    println!("guard hoisted, at()      {best:.1} ns");

    let mut best2 = f64::MAX;
    for _ in 0..7 {
        let t0 = Instant::now();
        for _ in 0..N {
            let g = tree.guard();
            std::hint::black_box(plan.at(&g, stamp).unwrap());
        }
        best2 = best2.min(t0.elapsed().as_nanos() as f64 / f64::from(N));
    }
    println!("guard per call, at()     {best2:.1} ns");

    let mut best3 = f64::MAX;
    for _ in 0..7 {
        let t0 = Instant::now();
        for _ in 0..N {
            std::hint::black_box(tree.guard());
        }
        best3 = best3.min(t0.elapsed().as_nanos() as f64 / f64::from(N));
    }
    println!("guard alone              {best3:.1} ns");
    println!(
        "=> guard costs           {:.1} ns of the per-call path",
        best2 - best
    );
}
