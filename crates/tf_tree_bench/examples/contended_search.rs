//! Does the bracket-search cache cliff get *worse* when a writer is publishing?
//!
//! `docs/design/fast-path.md` §12 established that the search is 34% of a
//! dynamic step and that its cost is a step function of stamp-array size against
//! L1d — flat while the stamps fit, then a hard jump. §16 then showed a
//! query-to-query cursor recovers 25-29% of a sample by making the probes local.
//!
//! **Both were measured on a quiescent tree**, and that is not the deployed
//! condition. A writer pushing into the ring writes the very stamp cache lines a
//! reader is probing, so on a real machine the reader's probes can be
//! invalidated between one query and the next. Two things could follow, and they
//! point opposite ways:
//!
//! * the cliff gets **worse**, because every push evicts a line the reader was
//!   relying on — which would make the footprint levers more valuable, not less;
//! * the **cursor stops helping**, because its whole benefit is that the line it
//!   resumes into is still warm — and a writer is what makes it not warm.
//!
//! Nothing in the repository measures either. This does.
//!
//! # Shape
//!
//! One dynamic edge, one reader, and either zero or one writer publishing at the
//! edge's nominal rate. The reader queries at a fixed **lag behind the newest
//! stamp**, which is both what a real consumer does and what keeps the query
//! inside a window the writer is sliding. Failures are counted rather than
//! hidden: a query that falls out of the window is a measurement that did not
//! happen, and a run with many of them is not comparable to one without.
//!
//! The writer publishes at a nominal rate rather than flat out. A flat-out
//! writer starves the reader — `read_scaling.rs` records that failure and why —
//! and answers a question nobody has.
//!
//! **Run pinned:**
//! `taskset -c 2,3 cargo run --release -p tf_tree_bench --example contended_search`
//!
//! Two cores, not one: the reader and the writer must actually run at the same
//! time for the question to mean anything. Per-thread placement is the OS's
//! choice, because nothing here calls `sched_setaffinity` — and the reason is
//! `docs/decisions/0007` rule 1, not a lint posture. **This sentence read
//! *"`tf_tree_bench` is `#![forbid(unsafe_code)]` and cannot call
//! `sched_setaffinity`"* until 2026-09-05, which is the wrong scope**: the
//! `forbid` is on `crates/tf_tree_bench/src/lib.rs` and an example is a
//! separate crate root it does not govern. What holds is the rule: pinning a
//! *thread* would be a new OS-boundary site whose only purpose is placement,
//! and `taskset` places a *process* exactly, for free, from outside. Two
//! sibling binaries (`contended_scaling`, `load_child`) chose a
//! process-per-reader architecture on that same argument.
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

/// Sample period. 1 kHz — the rate that gives a 10 s history the 16384-slot ring
/// sitting at the far end of §12's cliff.
const DT_NS: i64 = 1_000_000;
/// Queries per timed round.
///
/// **Sized so the writer actually runs during the measurement.** At ~10 ns a
/// query, 4096 queries is 40 microseconds and a whole 41-round loop is under two
/// milliseconds — during which a 1 kHz publisher lands *four* pushes. The
/// contended columns were then a measurement of almost no writer, and the
/// derived publish rate was quantised to +/-250 Hz by a +/-1 push error, which
/// is how this was noticed: it reported 1182 Hz for a loop that sleeps 1 ms.
///
/// A million queries a round puts each loop at ~200 ms and ~200 pushes, so the
/// rate is accurate to well under a percent and the writer is unambiguously
/// running.
const N: usize = 1_048_576;
/// Timed rounds; the median is reported.
const ROUNDS: usize = 21;
/// How far behind the newest stamp the reader asks. A quarter of the retained
/// window: far enough that a sliding window never overtakes the query, close
/// enough to be what a consumer actually asks for.
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

/// `(median ns/query, failures)` for one reader loop.
///
/// `cursor` selects the search: `false` restarts the bracket search every query
/// (what `Plan::at` did before §16), `true` resumes from the previous answer
/// (what it does now).
fn read_loop(ring: &SampleRing<'_>, cursor: bool, cap: u32) -> (f64, u64) {
    let lag = (f64::from(cap) * LAG_FRACTION) as i64 * DT_NS;
    let mut fails = 0u64;
    let mut per_round = Vec::with_capacity(ROUNDS);
    let mut cur = 0u64;

    for round in 0..ROUNDS + 2 {
        let t0 = Instant::now();
        let mut acc = 0.0;
        for _ in 0..N {
            // Re-read the newest stamp every query: under a writer this is a
            // moving target, and asking relative to it is what keeps the query
            // in-window without pinning the reader to a stale region.
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

        // --- quiescent ---
        let (fresh_q, f1) = read_loop(&ring, false, cap);
        let (cursor_q, f2) = read_loop(&ring, true, cap);

        // --- with one writer at the edge's nominal rate ---
        let stop = AtomicBool::new(false);
        // **The vacuity guard.** `push` returns a `Result`, and a writer whose
        // pushes are all rejected is indistinguishable from no writer at all —
        // which would make "a writer costs nothing" a measurement of nothing.
        // Count what actually landed, and report the rate achieved rather than
        // the rate intended: `sleep` overshoots, so a 1 ms period is not 1 kHz.
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
                // Continue the stamp sequence the fill left off at, so the
                // window slides forward rather than rejecting an older stamp.
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
            // Snapshot the count *with* the clock. The writer is already
            // running by the time the reader starts, so dividing the total by
            // the reader's elapsed time credits pre-start pushes to a shorter
            // interval — which reported 1204 Hz for a 1 ms sleep period, a rate
            // that loop cannot reach.
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
            // Pushes per million reader queries. At `ns` nanoseconds a query a
            // reader issues `1e9 / ns` of them a second, so this is
            // `hz / (1e9 / ns) * 1e6`, i.e. `hz * ns / 1000`. It is the ratio
            // that explains the result: a publisher and a reader running four
            // orders of magnitude apart cannot contend for a cache line often
            // enough to show up.
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
