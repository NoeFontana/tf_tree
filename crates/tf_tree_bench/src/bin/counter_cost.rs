//! What the diagnostic counters cost — `docs/PHASE5.md` §5.7.
//!
//! Measures §5.7's two questions: the cost of the non-atomic `Guard` increment,
//! and whether flush-on-drop contends under sixteen concurrent readers.
//!
//! Result on a 4-core + SMT host: no measurable contention at or below the CPU
//! count (ranges of three runs overlap with the counters-off control), so §5.7's
//! sharding fallback is not justified. Rows past the CPU count are scheduler
//! artifacts and are not quoted.
//!
//! The `counters` feature is compile-time, so the control is a loop in the same
//! build that drops a `Guard` every iteration (one flush per lookup, not per
//! batch). §5.4's requirement for a long-lived `Guard` is withdrawn; the batched
//! arm prices the flush only.
//!
//! Run pinned on an idle machine:
//!   `taskset -c 0-7 cargo run --release -p tf_tree_bench --bin counter_cost`
//!   `taskset -c 0-7 cargo run --release -p tf_tree_bench --bin counter_cost`
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::expect_used)]

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tf_tree::{Capacity, EdgeCfg, Stamp, SystemDomain, Tree, TreeBuilder};

const N: usize = 8192;
const ROUNDS: usize = 41;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn build() -> Tree {
    let cfg = EdgeCfg::new(Capacity::slots(1024));
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .dynamic_edge("odom", "base", cfg)
        .build()
        .unwrap();
    for (p, c) in [("map", "odom"), ("odom", "base")] {
        let pf = tree.frame(p).unwrap();
        let cf = tree.frame(c).unwrap();
        let w = tree.claim(cf, pf).unwrap();
        for i in 0..512i64 {
            w.push(
                i * 1_000_000,
                &tf_tree::exp_se3([0.0, 0.0, 0.001 * i as f64, 0.0, 0.0, 0.0]),
            )
            .unwrap();
        }
        core::mem::forget(w);
    }
    tree
}

fn main() {
    let tree = build();
    let src = tree.frame("map").unwrap();
    let dst = tree.frame("odom").unwrap();
    let plan = tree.plan(src, dst).unwrap();
    let stamps: Vec<i64> = (0..N)
        .map(|i| ((i * 7919) % 511) as i64 * 1_000_000)
        .collect();

    println!("counter cost — PHASE5 §5.7");
    println!("==========================");
    println!(
        "counters feature: {}\n",
        if cfg!(feature = "counters") {
            "ON"
        } else {
            "OFF — the rows below measure nothing; rebuild with it"
        }
    );

    // --- one guard for the whole batch: `Plan::at` with a hoisted guard ---
    let batched = median(
        (0..ROUNDS)
            .map(|_| {
                let t0 = Instant::now();
                let g = tree.guard();
                let mut acc = 0.0;
                for &s in &stamps {
                    let iso = plan
                        .at(&g, Stamp::<SystemDomain>::from_nanos(black_box(s)))
                        .unwrap();
                    acc += iso.t.x;
                }
                black_box(acc);
                drop(g);
                t0.elapsed().as_nanos() as f64 / N as f64
            })
            .collect(),
    );

    // --- a guard per lookup: one flush per lookup, which is what a
    //     write-through counter would cost, plus the guard construction ---
    let per_lookup = median(
        (0..ROUNDS)
            .map(|_| {
                let t0 = Instant::now();
                let mut acc = 0.0;
                for &s in &stamps {
                    let g = tree.guard();
                    let iso = plan
                        .at(&g, Stamp::<SystemDomain>::from_nanos(black_box(s)))
                        .unwrap();
                    acc += iso.t.x;
                }
                black_box(acc);
                t0.elapsed().as_nanos() as f64 / N as f64
            })
            .collect(),
    );

    println!("{:>34} {:>10}", "path", "ns/lookup");
    println!("{:>34} {batched:>10.1}", "one guard per batch (Plan::at)");
    println!("{:>34} {per_lookup:>10.1}", "one guard per lookup");
    println!(
        "\n  a flush per lookup costs {:+.1} ns — the price of the per-call",
        per_lookup - batched
    );
    println!("  guard the convenience path keeps (PHASE5 §5.4's requirement for a");
    println!("  long-lived one is WITHDRAWN; read its amendment). It is an upper");
    println!("  bound on the atomic it replaces — the guard's own construction is");
    println!("  in there too — and `just guard-cost` is the registered artifact");
    println!("  for that decomposition.");

    // §5.7's second question: sixteen threads on the same edge, the worst case.
    println!("\nconcurrent readers on ONE edge — the worst case for the flush");
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
    println!("  (this host reports {cores} logical CPUs)");
    let mut base = 0.0;
    for threads in [1usize, 2, 4, 8, 16] {
        let ns = concurrent(&tree, threads, &stamps);
        if threads == 1 {
            base = ns;
        }
        let note = if threads > cores && cores > 0 {
            "  <- oversubscribed"
        } else {
            ""
        };
        println!(
            "{threads:>34} {ns:>10.1}  ns/lookup/thread  ({:.2}x){note}",
            ns / base
        );
    }
    println!("\n  **Read the rows at or below the CPU count.** Past that, threads");
    println!("  are time-slicing and the per-thread number rises for a reason that");
    println!("  has nothing to do with the counter — this host has {cores} logical");
    println!("  CPUs, so the 16-thread row is roughly 2x oversubscribed before any");
    println!("  contention is considered. §5.7 asks whether the *flush* contends;");
    println!("  a scheduling artifact answering for it would be the wrong number.");
    println!();
    println!("  §5.7's sharding fallback (counters[edge][slot & 7]) is justified");
    println!("  only by a bad ratio in the rows that fit. Rebuild with");
    println!("  `--no-default-features` to get the control: the same rows with the");
    println!("  counters compiled out, which is the only way to separate the");
    println!("  flush's cost from the scheduler's.");
}

/// `threads` readers, each with its own guard per batch, on one shared tree.
fn concurrent(tree: &Tree, threads: usize, stamps: &[i64]) -> f64 {
    let src = tree.frame("map").unwrap();
    let dst = tree.frame("odom").unwrap();
    let go = Arc::new(AtomicBool::new(false));
    let mut totals = Vec::new();

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let go = Arc::clone(&go);
                scope.spawn(move || {
                    let plan = tree.plan(src, dst).unwrap();
                    while !go.load(Ordering::Acquire) {
                        std::hint::spin_loop();
                    }
                    let t0 = Instant::now();
                    let mut n = 0u64;
                    // A fixed window so every thread contends for the same interval.
                    while t0.elapsed() < Duration::from_millis(300) {
                        let g = tree.guard();
                        let mut acc = 0.0;
                        for &s in stamps {
                            if let Ok(iso) =
                                plan.at(&g, Stamp::<SystemDomain>::from_nanos(black_box(s)))
                            {
                                acc += iso.t.x;
                            }
                            n += 1;
                        }
                        black_box(acc);
                    }
                    t0.elapsed().as_nanos() as f64 / n as f64
                })
            })
            .collect();
        go.store(true, Ordering::Release);
        for h in handles {
            totals.push(h.join().unwrap());
        }
    });
    median(totals)
}
