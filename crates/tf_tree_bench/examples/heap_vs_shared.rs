//! Same topology, same stamps, same process: heap arena against shared arena.
//!
//! The Python multi-process benchmark reported ~2.7 us per lookup where the
//! in-process figure is ~0.17 us, and the Rust `mp_bench` reports ~2 us for its
//! own fixture. Two languages seeing the same thing says it is not the binding.
//! This isolates the one remaining variable — which arena the bytes live in —
//! with everything else held fixed.
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
//! # This example cannot carry the mapping claim, and should not be cited for it
//!
//! The conclusion above is right, but the measurement under it has
//! `docs/decisions/0013`'s defect. It queries the single stamp `1_500_000_000`
//! against samples laid down at `1_000_000 + i * 1_000_000` — an **exact grid
//! hit** at `i = 1499` — so `SampleRing::sample` takes its exact-hit branch,
//! `I::eval` never runs, and all four rows are `bracket` plus a seqlock read.
//! A mapping, if it costs anything, costs it on the loads the interpolation
//! issues; this never issues them. The 51 ns figures are also far below the
//! ~200 ns the §11.1 fixture measures off-grid, which is the tell.
//!
//! **`crates/tf_tree_bench/src/backing.rs` is the measurement that carries it**
//! (`just abi-split`): the §11.1 fixture, off-grid, paired and interleaved,
//! reporting the shared mapping at <= 9.6 ns worst-case over nine runs, ~1.8 ns
//! typically. This example is retained for the two rows that one does not
//! cover: the read-only *attached* mapping, and the concurrent-writer row.
//!
//! So the ~2.7 us a Python consumer sees in the multi-process benchmark is the
//! **deployment environment**: separate processes, descheduled between ticks,
//! reading through caches that another process has been evicting. It is not the
//! arena, not concurrency, and not the binding (which adds ~120 ns). There is no
//! engine headroom left to spend on that number, which is also why tf2 pays the
//! same environmental cost and the ratio survives it.
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

    // The one variable left. A concurrent writer's stores invalidate the lines a
    // reader holds; a thread on another core is enough to produce that, and it
    // isolates coherence traffic from anything cross-process.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // 100 Hz only. At 10 kHz into a 4096-slot ring the writer laps the reader
    // mid-read and `at` correctly returns `SlotRecycled` — which is the ring
    // protocol working, not a latency number, so it is not reported as one.
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
