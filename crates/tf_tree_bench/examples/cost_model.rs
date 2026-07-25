//! Where does a lookup's time actually go?
//!
//! Decomposes the hot path into its three cost terms so a design can be aimed at
//! the dominant one instead of at the one that is easiest to think about:
//!
//!   t(depth, capacity) ≈ fixed + depth × (search(capacity) + interp + compose)
//!
//! Each sweep varies exactly one term:
//!
//! * **capacity sweep** — same depth, same query count, ring capacity 64 → 65536.
//!   Bracket search is a binary search over logical indices, so its cost is
//!   `log2(capacity)` *serially dependent* loads. If search dominates, this sweep
//!   is a straight line in `log2(capacity)` and its slope is the per-probe cost.
//! * **depth sweep** — same capacity, chain depth 1 → 6. The slope is the
//!   marginal cost of one more dynamic edge.
//! * **locality sweep** — identical stamp repeatedly vs a stamp swept across the
//!   window. Same instruction count; the difference is purely the cache and
//!   branch-predictor behaviour of the search.
//! * **interp sweep** — LerpSlerp vs ScLerp at fixed depth and capacity.
//!
//! **Run pinned, or do not run it at all:**
//! `taskset -c 2 cargo run --release -p tf_tree_bench --example cost_model`
//!
//! Unpinned, this harness migrates cores and swings by >30% — enough to invent
//! a 16% "regression" in a policy whose code did not change. Pinned, it repeats
//! to under 1% (measured: three consecutive runs at 253.8 / 255.3 / 254.4 ns).
//!
//! A second caveat that pinning does *not* fix: `sample::<LerpSlerp>` and
//! `sample::<ScLerp>` are monomorphized into the same hot function behind a
//! `match` on the policy byte, so changing the size of one relocates the other.
//! Cross-policy comparisons within one build are sound; comparing one policy
//! across two builds is not. Use `interp_cost` for that — it calls
//! `Interp::eval` directly and moves only when the interpolation math moves.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::hint::black_box;
use std::time::Instant;

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Iso3, Stamp, Tree, TreeBuilder};
use tf_tree_bench::fixture::dynamic_pose;

/// Lookups per timed round.
const N: usize = 8192;
/// Timed rounds; the median is reported.
const ROUNDS: usize = 41;

/// Build a chain `f0 -> f1 -> ... -> f{depth}` of dynamic edges, each with
/// `capacity` slots filled to `fill` samples at 1 kHz.
fn chain(depth: usize, capacity: u32, fill: usize, interp: InterpPolicy) -> (Tree, Vec<String>) {
    let names: Vec<String> = (0..=depth).map(|i| format!("f{i}")).collect();
    let mut b = TreeBuilder::new().default_interp(interp);
    for i in 0..depth {
        b = b.dynamic_edge(
            &names[i],
            &names[i + 1],
            EdgeCfg::new(Capacity::slots(capacity)),
        );
    }
    let tree = b.build().expect("build chain");

    let mut writers = Vec::new();
    for i in 0..depth {
        let p = tree.frame(&names[i]).unwrap();
        let c = tree.frame(&names[i + 1]).unwrap();
        writers.push(tree.claim(c, p).unwrap());
    }
    for k in 0..fill {
        let stamp = k as i64 * 1_000_000; // 1 kHz
        for (i, w) in writers.iter().enumerate() {
            w.push(stamp, &dynamic_pose(i as f64, stamp)).unwrap();
        }
    }
    drop(writers);
    (tree, names)
}

/// Median nanoseconds per lookup over `ROUNDS` timed rounds.
fn time_lookups(tree: &Tree, target: &str, source: &str, stamps: &[i64]) -> f64 {
    let t = tree.frame(target).unwrap();
    let s = tree.frame(source).unwrap();
    let plan = tree.plan(t, s).unwrap();
    let guard = tree.guard();

    let run = || {
        let mut acc = 0.0f64;
        for &ns in stamps {
            let stamp: Stamp = Stamp::from_nanos(ns);
            if let Ok(p) = plan.at(&guard, stamp) {
                acc += p.t.x;
            }
        }
        acc
    };

    for _ in 0..5 {
        black_box(run());
    }
    let mut per_round: Vec<f64> = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        black_box(run());
        per_round.push(t0.elapsed().as_nanos() as f64 / stamps.len() as f64);
    }
    per_round.sort_by(f64::total_cmp);
    per_round[per_round.len() / 2]
}

/// Stamps sweeping the whole filled window — the search does real work.
fn swept(fill: usize) -> Vec<i64> {
    let hi = (fill as i64 - 2) * 1_000_000;
    (0..N as i64)
        .map(|k| 1_000_000 + (hi - 1_000_000) * k / N as i64)
        .collect()
}

/// The same stamp every time — maximal temporal locality.
fn pinned(fill: usize) -> Vec<i64> {
    let mid = (fill as i64 / 2) * 1_000_000 + 500_000;
    std::vec![mid; N]
}

fn main() {
    println!("tf_tree lookup cost model");
    println!("=========================");
    println!("{N} lookups/round, median of {ROUNDS} rounds, LerpSlerp unless stated\n");

    // --- capacity sweep: isolates bracket-search cost -----------------------
    println!("## capacity sweep (depth 3, window fully filled, stamps swept)");
    println!(
        "{:>10} {:>7} {:>12} {:>14} {:>12}",
        "capacity", "log2", "ns/lookup", "ns/step", "ns/probe*"
    );
    let mut prev: Option<(f64, f64)> = None;
    for &cap in &[64u32, 256, 1024, 4096, 16384, 65536] {
        let fill = cap as usize - 1; // fill without lapping (retained = cap - 1)
        let (tree, names) = chain(3, cap, fill, InterpPolicy::LerpSlerp);
        let ns = time_lookups(&tree, &names[3], &names[0], &swept(fill));
        let l2 = (cap as f64).log2();
        // Marginal cost per extra probe = Δ(ns/step) / Δlog2(capacity).
        let per_probe = prev.map_or(f64::NAN, |(pl2, pns)| ((ns - pns) / 3.0) / (l2 - pl2));
        println!(
            "{cap:>10} {l2:>7.0} {ns:>12.1} {:>14.1} {per_probe:>12.2}",
            ns / 3.0
        );
        prev = Some((l2, ns));
    }

    // --- depth sweep: isolates per-step cost -------------------------------
    println!("\n## depth sweep (capacity 4096, stamps swept)");
    println!("{:>7} {:>12} {:>14}", "depth", "ns/lookup", "marginal/step");
    let mut last: Option<f64> = None;
    for depth in 1..=6usize {
        let (tree, names) = chain(depth, 4096, 4095, InterpPolicy::LerpSlerp);
        let ns = time_lookups(&tree, &names[depth], &names[0], &swept(4095));
        let marginal = last.map_or(f64::NAN, |p| ns - p);
        println!("{depth:>7} {ns:>12.1} {marginal:>14.1}");
        last = Some(ns);
    }

    // --- locality sweep: how much of the cost is cache/branch behaviour ----
    println!("\n## locality sweep (depth 3, capacity 16384)");
    let (tree, names) = chain(3, 16384, 16383, InterpPolicy::LerpSlerp);
    let sw = time_lookups(&tree, &names[3], &names[0], &swept(16383));
    let pin = time_lookups(&tree, &names[3], &names[0], &pinned(16383));
    println!("{:>24} {:>12}", "stamp pattern", "ns/lookup");
    println!("{:>24} {sw:>12.1}", "swept across window");
    println!("{:>24} {pin:>12.1}", "pinned (same stamp)");
    println!(
        "{:>24} {:>12.1}  <- attributable to search locality",
        "difference",
        sw - pin
    );

    // --- interp sweep -------------------------------------------------------
    println!("\n## interpolation policy (depth 3, capacity 4096, stamps swept)");
    println!("{:>12} {:>12} {:>14}", "policy", "ns/lookup", "ns/step");
    for (label, pol) in [
        ("LerpSlerp", InterpPolicy::LerpSlerp),
        ("ScLerp", InterpPolicy::ScLerp),
    ] {
        let (tree, names) = chain(3, 4096, 4095, pol);
        let ns = time_lookups(&tree, &names[3], &names[0], &swept(4095));
        println!("{label:>12} {ns:>12.1} {:>14.1}", ns / 3.0);
    }

    // --- exact-hit vs interpolated -----------------------------------------
    println!("\n## exact-stamp hit vs interpolated (depth 3, capacity 4096)");
    let (tree, names) = chain(3, 4096, 4095, InterpPolicy::LerpSlerp);
    let exact: Vec<i64> = (0..N as i64).map(|k| (k % 4000 + 1) * 1_000_000).collect();
    let between: Vec<i64> = (0..N as i64)
        .map(|k| (k % 4000 + 1) * 1_000_000 + 500_000)
        .collect();
    let e = time_lookups(&tree, &names[3], &names[0], &exact);
    let b = time_lookups(&tree, &names[3], &names[0], &between);
    println!("{:>24} {e:>12.1}", "exact hit (no interp)");
    println!("{:>24} {b:>12.1}", "interpolated");
    println!(
        "{:>24} {:>12.1}  <- interpolation math, 3 steps",
        "difference",
        b - e
    );

    let _ = Iso3::IDENTITY;
}
