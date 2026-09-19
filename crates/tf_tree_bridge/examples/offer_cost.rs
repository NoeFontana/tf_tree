//! What one [`Ingest::offer`] costs, in instructions — `docs/PHASE4.md` §7.
//!
//! Instructions, not time: this host fails `Fitness::probe` (SMT, unreadable
//! governor, `perf_event_paranoid` 4). `cachegrind`'s simulated `Ir` is exact
//! under load but is not a latency figure.
//!
//! As in `tf_tree_bench::bin::footprint`: `N = 0` does setup only, so
//! `Ir(N) - Ir(0)` isolates the offers; no clock reads in the loop (receipt
//! stamps are synthesised); one mode per process.
//!
//! The edge count is swept because a one-key `BTreeMap` never compares
//! anything: `ByEdge` cost is `O(log E)` descents times the shared name prefix,
//! so `Ir` per offer should rise with edges before an index exists and be flat
//! after. Both `short` (4 shared bytes) and `ros` (15) names are reported.
//!
//! Run: `just bridge-footprint` (container; needs `valgrind`).
// This binary's entire output *is* its result.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::hint::black_box;
use std::time::Instant;

use tf_tree_bridge::{Action, Ingest, Publisher, Sample, SteadyNanos, Topic, TopologyConfig};

/// `docs/PHASE4.md` §7's row is "1 kHz × 20 edges", so 20 is the headline.
const EDGES_DEFAULT: usize = 20;
/// Never zero: `SteadyNanos(0)` is the "no receipt clock" sentinel.
const T0: i64 = 5_000_000_000_000;
const STAMP0: i64 = 10_000_000_000;
const MS: i64 = 1_000_000;

/// How frame names are spelled, which decides what a `memcmp` costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Names {
    /// `link0 -> link1`. Four shared bytes.
    Short,
    /// `robot1/arm/wrist_0_link -> robot1/arm/wrist_1_link`. Fifteen shared.
    Ros,
}

fn edge_names(style: Names, i: usize) -> (String, String) {
    match style {
        Names::Short => (format!("link{i}"), format!("link{}", i + 1)),
        Names::Ros => (
            format!("robot1/arm/wrist_{i}_link"),
            format!("robot1/arm/wrist_{}_link", i + 1),
        ),
    }
}

/// A chain of `edges` dynamic edges.
fn topology(edges: usize, style: Names) -> String {
    let mut s = String::new();
    for i in 0..edges {
        let (p, c) = edge_names(style, i);
        s.push_str(&format!(
            "[[edge]]\nparent = \"{p}\"\nchild = \"{c}\"\nkind = \"dynamic\"\ncapacity = 256\n\n"
        ));
    }
    s
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The accepted `/tf` transform. The path a healthy robot spends its life on.
    Declared,
    /// An edge the config does not declare.
    Undeclared,
    /// A publisher replaying stamps from five seconds ago: refused every time, never latching.
    Regressing,
}

/// The only loop; the counted and timed entry points share it.
#[inline(never)]
fn run(mode: Mode, edges: usize, style: Names, n: usize) -> u64 {
    let cfg = TopologyConfig::parse(&topology(edges, style)).unwrap();
    let mut ingest = Ingest::new(&cfg);
    let publisher = Publisher::named(&tf_tree_bridge::gid_for_name("/ekf"), "/ekf");

    // Built before the loop: a per-iteration `String` clone would be charged to `offer`.
    let mut samples: Vec<Sample> = Vec::with_capacity(edges);
    for i in 0..edges {
        let (p, c) = match mode {
            // Absent from the config, distinct per index.
            Mode::Undeclared => (format!("ghost{i}"), format!("ghost{}", i + 1)),
            _ => edge_names(style, i),
        };
        samples.push(Sample::identity(&p, &c, STAMP0).received_at(SteadyNanos(T0)));
    }

    // Warm-up outside the counted window: first sightings intern and allocate.
    let mut stamp = STAMP0;
    let mut received = T0;
    for k in 0..(edges * 4) {
        let i = k % edges;
        if i == 0 {
            stamp += MS;
            received += MS;
        }
        let mut s = samples[i].clone();
        s.stamp_nanos = stamp;
        s.received = SteadyNanos(received);
        let _ = ingest.offer(Topic::Tf, &s, &publisher);
    }

    // Regressing: rewind five seconds and stay there.
    let base_stamp = if mode == Mode::Regressing {
        stamp - 5_000 * MS
    } else {
        stamp
    };

    let mut accepted = 0u64;
    for k in 0..n {
        let i = k % edges;
        if i == 0 && mode != Mode::Regressing {
            stamp += MS;
            received += MS;
        }
        if mode == Mode::Regressing {
            received += MS;
        }
        let s = &mut samples[i];
        s.stamp_nanos = black_box(if mode == Mode::Regressing {
            base_stamp
        } else {
            stamp
        });
        s.received = SteadyNanos(black_box(received));
        if matches!(
            ingest.offer(Topic::Tf, black_box(&*s), &publisher),
            Action::Publish { .. }
        ) {
            accepted += 1;
        }
    }
    black_box(accepted)
}

/// Wall-clock ns per offer per thread, over `threads` **independent** bridges
/// (`Ingest::offer` is `&mut self` and thread-affine, so none share one).
/// Measures allocator and bandwidth effects, not lock contention. Min of
/// `ROUNDS` rounds, since noise only adds time.
fn time(mode: Mode, edges: usize, style: Names, n: usize, threads: usize) {
    const ROUNDS: usize = 7;
    let mut best = f64::INFINITY;
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        std::thread::scope(|sc| {
            for _ in 0..threads {
                sc.spawn(move || run(mode, edges, style, n));
            }
        });
        let per = t0.elapsed().as_secs_f64() * 1e9 / (n as f64);
        if per < best {
            best = per;
        }
    }
    println!("offer_time mode={mode:?} edges={edges} threads={threads} ns_per_offer={best:.1}");
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mode = match a.get(1).map(String::as_str).unwrap_or("declared") {
        "declared" => Mode::Declared,
        "undeclared" => Mode::Undeclared,
        "regressing" => Mode::Regressing,
        other => {
            eprintln!("offer_cost: unknown mode {other:?}");
            eprintln!("usage: offer_cost <declared|undeclared|regressing> N [EDGES] [short|ros]");
            std::process::exit(2);
        }
    };
    let n: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let edges: usize = a
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(EDGES_DEFAULT)
        .max(1);
    let style = match a.get(4).map(String::as_str).unwrap_or("short") {
        "ros" => Names::Ros,
        _ => Names::Short,
    };

    // A `threads` argument switches to the timed mode.
    if let Some(threads) = a.get(5).and_then(|s| s.parse::<usize>().ok()) {
        time(mode, edges, style, n, threads.max(1));
        return;
    }
    let accepted = run(mode, edges, style, n);
    // One line for `just bridge-footprint`; `accepted` shows the run did work.
    println!("offer_cost n={n} edges={edges} accepted={accepted}");
}
