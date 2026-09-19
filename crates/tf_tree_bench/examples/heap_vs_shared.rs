//! Same topology, same stamps, same process: heap arena against shared arena.
//!
//! Python's multi-process benchmark reported ~2.7 us per lookup against ~0.17 us in-process, and
//! Rust's `mp_bench` ~2 us; this isolates the one remaining variable, which arena the bytes live in.
//! # What it found
//!
//! ```text
//! heap arena                   51.1 ns
//! shared arena (rw, creator)   51.3 ns
//! shared arena (ro, attached)  51.2 ns
//! ro + 100 Hz writer           51.9 ns
//! ```
//!
//! **None of it is the engine.** A `MAP_SHARED` memfd reads exactly as fast as a
//! heap allocation — `docs/PHASE2.md` §4's "zero lines in the read path" claim,
//! which `relocation.rs` proves for *correctness*, holds for latency too — and a
//! concurrent cross-core publisher at 100 Hz costs 0.7 ns.
//!
//! # This example cannot carry the mapping claim
//!
//! It queries one stamp that is an **exact grid hit**, so `I::eval` never runs and all four rows are
//! `bracket` plus a seqlock read (`docs/decisions/0013`'s defect). `src/backing.rs` (`just abi-split`)
//! is the off-grid, paired measurement that carries it; this is kept for the read-only attached and
//! concurrent-writer rows.
//!
//! The ~2.7 us a Python consumer sees is the deployment environment (separate processes, descheduled,
//! evicted caches), not the arena, concurrency, or the binding (~120 ns).
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

/// Claim every edge, publish 2000 samples, and **return the writers** so a
/// caller can keep publishing. Claims stay held while they live.
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

    // A thread on another core suffices for coherence traffic.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // 100 Hz only: at 10 kHz the writer
    // laps the reader and `at` correctly returns `SlotRecycled`.
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
