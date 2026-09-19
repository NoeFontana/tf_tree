//! Same topology, same stamps, same process: heap arena against shared arena.
//!
//! ```text
//! heap arena                   51.1 ns
//! shared arena (rw, creator)   51.3 ns
//! shared arena (ro, attached)  51.2 ns
//! ro + 100 Hz writer           51.9 ns
//! ```
//! One **exact grid hit** is queried, so `I::eval` never runs (`docs/decisions/0013`); `src/backing.rs`
//! (`just abi-split`) carries the off-grid mapping claim. This is kept for the read-only and writer rows.
#![allow(missing_docs, clippy::unwrap_used, clippy::print_stdout)]
use std::time::Instant;
use tf_tree::{
    AttachMode, Capacity, EdgeCfg, InterpPolicy, Stamp, SystemDomain, Tree, TreeBuilder,
};

fn layout() -> TreeBuilder {
    TreeBuilder::new()
        .default_interp(InterpPolicy::LerpSlerp)
        .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(4096)))
        .dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(4096)))
        .dynamic_edge("base", "cam", EdgeCfg::new(Capacity::slots(4096)))
}

/// Claim every edge, publish 2000 samples, and return the writers.
fn fill(tree: &Tree) -> Vec<tf_tree::EdgeWriter<'_>> {
    let mut out = Vec::new();
    for (c, p) in [("odom", "map"), ("base", "odom"), ("cam", "base")] {
        let ci = tree.frame(c).unwrap();
        let pi = tree.frame(p).unwrap();
        let w = tree.claim(ci, pi).unwrap();
        for i in 0..2000i64 {
            w.push(
                1_000_000 + i * 1_000_000,
                &tf_tree_math::exp_se3([0.0, 0.0, 0.0, 0.001 * i as f64, 0.0, 0.0]),
            )
            .unwrap();
        }
        out.push(w);
    }
    out
}

fn time(tree: &Tree, label: &str) {
    let t = tree.frame("map").unwrap();
    let s = tree.frame("cam").unwrap();
    let plan = tree.plan(t, s).unwrap();
    let stamp = Stamp::<SystemDomain>::from_nanos(1_500_000_000);
    const N: u32 = 2_000_000;
    let mut best = f64::MAX;
    for _ in 0..7 {
        let t0 = Instant::now();
        for _ in 0..N {
            let g = tree.guard();
            std::hint::black_box(plan.at(&g, stamp).unwrap());
        }
        best = best.min(t0.elapsed().as_nanos() as f64 / f64::from(N));
    }
    println!("{label:<28} {best:.1} ns");
}

fn main() {
    let heap = layout().build().unwrap();
    let _hw = fill(&heap);
    time(&heap, "heap arena");

    let shared = layout().build_shared("tf_tree.heap_vs_shared").unwrap();
    let mut writers = fill(&shared);
    time(&shared, "shared arena (rw, creator)");

    let fd = shared.shared_fd().unwrap().try_clone_to_owned().unwrap();
    let ro = Tree::attach_shared(fd, AttachMode::ReadOnly).unwrap();
    time(&ro, "shared arena (ro, attached)");

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // 100 Hz only: at 10 kHz the writer laps the reader (`SlotRecycled`).
    {
        let (label, hz) = ("100 Hz", 100u64);
        let flag = std::sync::Arc::clone(&stop);
        flag.store(false, std::sync::atomic::Ordering::Relaxed);
        let w = writers.pop().unwrap();
        std::thread::scope(|sc| {
            sc.spawn(move || {
                let mut i = 2000i64;
                let period = std::time::Duration::from_nanos(1_000_000_000 / hz);
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    i += 1;
                    let _ = w.push(
                        1_000_000 + i * 1_000_000,
                        &tf_tree_math::exp_se3([0.0, 0.0, 0.0, 0.001 * i as f64, 0.0, 0.0]),
                    );
                    std::thread::sleep(period);
                }
            });
            time(&ro, &format!("ro + {label} writer"));
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }
}
