//! Does the bracket-search cache cliff get *worse* when a writer is publishing?
//!
//! Extends `docs/design/fast-path.md` §12/§16 (quiescent cliff, cursor recovery) to a concurrent writer.
//!
//! # Shape
//! One dynamic edge, one reader at a fixed lag behind the newest stamp, zero or one writer at the edge's
//! nominal rate. Failed queries are counted.
//!
//! **Run pinned:** `taskset -c 2,3 cargo run --release -p tf_tree_bench --example contended_search`
//!
//! Two cores, so reader and writer overlap; threads are not pinned individually (`docs/decisions/0007` rule 1).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Tree, TreeBuilder};
use tf_tree_bench::fixture::dynamic_pose;
use tf_tree_core::buffer::SampleRing;
use tf_tree_core::plan::Step;
use tf_tree_core::sample::ExtrapPolicy;
use tf_tree_core::EdgeId;
use tf_tree_math::LerpSlerp;

/// Sample period: 1 kHz.
const DT_NS: i64 = 1_000_000;
/// Queries per timed round, sized so the writer runs during the measurement.
const N: usize = 1_048_576;
/// Timed rounds; the median is reported.
const ROUNDS: usize = 21;
/// How far behind the newest stamp the reader asks: a quarter of the retained window.
const LAG_FRACTION: f64 = 0.25;

fn build(cap: u32) -> (Tree, EdgeId) {
    let tree = TreeBuilder::new()
        .default_interp(InterpPolicy::LerpSlerp)
        .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(cap)))
        .build()
        .unwrap();
    let map = tree.frame("map").unwrap();
    let base = tree.frame("base").unwrap();
    let w = tree.claim(base, map).unwrap();
    for k in 0..(cap as usize - 1) {
        w.push(k as i64 * DT_NS, &dynamic_pose(0.0, k as i64 * DT_NS))
            .unwrap();
    }
    drop(w);
    let plan = tree.plan(base, map).unwrap();
    let edge = plan
        .steps()
        .iter()
        .find_map(|s| match s {
            Step::Dyn { edge, .. } => Some(*edge),
            Step::Static(_) => None,
        })
        .unwrap();
    (tree, edge)
}

/// `(median ns/query, failures)`; `cursor` resumes the search from the previous answer instead of restarting.
fn read_loop(ring: &SampleRing<'_>, cursor: bool, cap: u32) -> (f64, u64) {
    let lag = (f64::from(cap) * LAG_FRACTION) as i64 * DT_NS;
    let mut fails = 0u64;
    let mut per_round = Vec::with_capacity(ROUNDS);
    let mut cur = 0u64;

    for round in 0..ROUNDS + 2 {
        let t0 = Instant::now();
        let mut acc = 0.0;
        for _ in 0..N {
            let Some(newest) = ring.newest_stamp() else {
                fails += 1;
                continue;
            };
            let t = black_box(newest - lag);
            let r = if cursor {
                ring.sample_from::<LerpSlerp>(t, ExtrapPolicy::Error, &mut cur)
            } else {
                ring.sample::<LerpSlerp>(t, ExtrapPolicy::Error)
            };
            match r {
                Ok(p) => acc += p.t.x,
                Err(_) => fails += 1,
            }
        }
        let dt = t0.elapsed().as_nanos() as f64 / N as f64;
        black_box(acc);
        if round >= 2 {
            per_round.push(dt);
        }
    }
    per_round.sort_by(f64::total_cmp);
    (per_round[per_round.len() / 2], fails)
}

fn main() {
    println!("tf_tree: the search cliff under a concurrent writer");
    println!("===================================================");
    println!(
        "1 kHz edge, reader asks at newest-{:.0}% of the window, {N} queries/round, median of {ROUNDS}\n",
        LAG_FRACTION * 100.0
    );
    println!(
        "{:>9} {:>10} {:>11} {:>11} {:>9} {:>11} {:>11} {:>9} {:>10} {:>8}",
        "capacity",
        "stamps",
        "fresh q",
        "fresh +w",
        "w cost",
        "cursor q",
        "cursor +w",
        "w cost",
        "writer Hz",
        "push/Mq"
    );

    for &cap in &[1024u32, 4096, 16384] {
        let (tree, edge) = build(cap);
        let g = tree.guard();
        let (_, ring) = g.view().sampler(edge).unwrap();

        let (fresh_q, f1) = read_loop(&ring, false, cap);
        let (cursor_q, f2) = read_loop(&ring, true, cap);

        let stop = AtomicBool::new(false);
        // Vacuity guard: count landed pushes; a writer that publishes nothing looks like no writer.
        let pushed = AtomicU64::new(0);
        let refused = AtomicU64::new(0);
        let (fresh_w, cursor_w, f3, f4, hz) = std::thread::scope(|s| {
            let tref = &tree;
            let stop_ref = &stop;
            let ok_ref = &pushed;
            let bad_ref = &refused;
            s.spawn(move || {
                let map = tref.frame("map").unwrap();
                let base = tref.frame("base").unwrap();
                let w = tref.claim(base, map).unwrap();
                let mut k = i64::from(cap);
                let period = Duration::from_nanos(DT_NS as u64);
                while !stop_ref.load(Ordering::Relaxed) {
                    let t = k * DT_NS;
                    match w.push(t, &dynamic_pose(0.0, t)) {
                        Ok(()) => ok_ref.fetch_add(1, Ordering::Relaxed),
                        Err(_) => bad_ref.fetch_add(1, Ordering::Relaxed),
                    };
                    k += 1;
                    std::thread::sleep(period);
                }
            });
            let t0 = Instant::now();
            let base = pushed.load(Ordering::Relaxed);
            let a = read_loop(&ring, false, cap);
            let b = read_loop(&ring, true, cap);
            let secs = t0.elapsed().as_secs_f64();
            let landed = pushed.load(Ordering::Relaxed) - base;
            stop.store(true, Ordering::Relaxed);
            (a.0, b.0, a.1, b.1, landed as f64 / secs)
        });
        assert!(
            pushed.load(Ordering::Relaxed) > 0 && refused.load(Ordering::Relaxed) == 0,
            "capacity {cap}: writer landed {} pushes and had {} refused — a writer that \
             did not publish makes the '+writer' columns a measurement of nothing",
            pushed.load(Ordering::Relaxed),
            refused.load(Ordering::Relaxed)
        );

        println!(
            "{cap:>9} {:>9} K {fresh_q:>11.2} {fresh_w:>11.2} {:>8.2}x {cursor_q:>11.2} {cursor_w:>11.2} {:>8.2}x {:>10} {:>8.1}",
            u64::from(cap) * 8 / 1024,
            fresh_w / fresh_q,
            cursor_w / cursor_q,
            format!("{hz:.0}"),
            // Pushes per million reader queries.
            hz * ((fresh_w + cursor_w) / 2.0) / 1000.0
        );
        assert_eq!(
            f1 + f2 + f3 + f4,
            0,
            "capacity {cap}: queries fell outside the retained window, so the columns \
             are not comparable"
        );
    }

    println!("\n  'w cost' is the writer's multiplier on that column: >1 means the writer");
    println!("  made the search slower. Compare the two — if the writer costs the cursor");
    println!("  much more than it costs a fresh search, the cursor's benefit is a");
    println!("  quiescent-tree artifact and §16's number does not survive deployment.");
    println!("\n  'writer Hz' is what the publisher **achieved**, not what it asked for:");
    println!("  `sleep` overshoots, so a 1 ms period is not 1 kHz. A writer whose pushes");
    println!("  were all refused would be indistinguishable from no writer, so the run");
    println!("  asserts that pushes landed, that none were refused, and that no query");
    println!("  fell outside the retained window.");
}
