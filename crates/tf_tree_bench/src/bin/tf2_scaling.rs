//! Concurrent read scaling: tf_tree (lock-free readers) vs `tf2::BufferCore`
//! (one mutex per lookup) at 1 / 2 / 4 / 8 threads (`docs/PHASE1.md` §11.2).
//!
//! Run with `just tf2-scaling` on an idle machine. Standalone rather than
//! criterion because §11.2 wants p99.9 of per-lookup latencies. Throughput
//! (whole batches) and latency (per-op clock, ~20 ns) are separate passes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Barrier;
use std::thread;
use std::time::Instant;

use tf_tree::{InterpPolicy, Plan, Stamp, Tree};
use tf_tree_bench::{fixture, replay, replay_tf2};
use tf_tree_tf2_sys::{FrameName, Tf2Buffer};

/// The sweep used when `TF2_THREADS` is unset or unusable.
const DEFAULT_THREADS: [usize; 4] = [1, 2, 4, 8];

/// Thread counts to sweep; override with `TF2_THREADS=1,2,4,8`.
fn thread_counts() -> Vec<usize> {
    let parsed: Vec<usize> = std::env::var("TF2_THREADS")
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|s| s.trim().parse::<usize>().ok())
                .filter(|&n| n > 0)
                .collect()
        })
        .unwrap_or_default();
    if parsed.is_empty() {
        DEFAULT_THREADS.to_vec()
    } else {
        parsed
    }
}

/// A count read from the environment, clamped to at least 1.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
        .max(1)
}

/// A workload both engines can serve identically.
struct Load {
    name: &'static str,
    tree: Tree,
    tf2: Tf2Buffer,
    target: String,
    source: String,
    /// The same names pre-converted for the FFI boundary.
    target_c: FrameName,
    source_c: FrameName,
    stamps: Vec<i64>,
    /// Dynamic edges the query path does not traverse, as `(parent, child)`.
    writable: Vec<(String, String)>,
    /// The next stamp any writer may publish, shared across passes and thread
    /// counts; a per-writer counter would republish old stamps and tf2 would warn
    /// `TF_OLD_DATA`.
    next_stamp: std::sync::atomic::AtomicI64,
}

fn fixture_load() -> Load {
    let tree = fixture::build_tree_with(InterpPolicy::LerpSlerp).expect("fixture");
    let (w, _) = fixture::spin_up(&tree).expect("populate");
    drop(w);
    let tf2 = tf_tree_bench::tf2::Tf2Fixture::load()
        .expect("tf2 fixture")
        .into_buffer();
    let now = fixture::NOW_NS;
    let lo = now - 100_000_000;
    let per_round = env_usize("TF2_PER_ROUND", 4096) as i64;
    let stamps: Vec<i64> = (0..per_round)
        .map(|k| lo + (now - lo) * k / per_round)
        .collect();
    let writable = writable_edges(
        &tree,
        "camera_optical",
        "map",
        &fixture::DYNAMIC_EDGES
            .iter()
            .map(|(p, c, _)| ((*p).to_owned(), (*c).to_owned()))
            .collect::<Vec<_>>(),
    );
    Load {
        next_stamp: std::sync::atomic::AtomicI64::new(
            stamps.last().copied().unwrap_or(0) + 1_000_000,
        ),
        writable,
        name: "fixture_depth6",
        tree,
        tf2,
        target: "camera_optical".to_owned(),
        source: "map".to_owned(),
        target_c: FrameName::new("camera_optical").unwrap(),
        source_c: FrameName::new("map").unwrap(),
        stamps,
    }
}

fn replay_load() -> Load {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/tfstream/indoor_atelier.tfstream");
    let stream = replay::TfStream::load(&path).expect("recording");
    let tree = stream.build_tree(InterpPolicy::LerpSlerp).expect("tree");
    let tf2 = replay_tf2::load_tf2(&stream).expect("tf2");
    let (lo, hi) = stream.common_window().expect("window");
    let per_round = env_usize("TF2_PER_ROUND", 4096) as i64;
    let stamps: Vec<i64> = (0..per_round)
        .map(|k| lo + (hi - lo) * k / per_round)
        .collect();
    let writable = writable_edges(&tree, "camera_link", "odom_combined", &stream.dynamic_edges);
    Load {
        next_stamp: std::sync::atomic::AtomicI64::new(
            stamps.last().copied().unwrap_or(0) + 1_000_000,
        ),
        writable,
        name: "recorded_stream",
        tree,
        tf2,
        target: "camera_link".to_owned(),
        source: "odom_combined".to_owned(),
        target_c: FrameName::new("camera_link").unwrap(),
        source_c: FrameName::new("odom_combined").unwrap(),
        stamps,
    }
}

/// Writer threads per engine: `TF2_WRITERS`, default **0**.
fn writer_count() -> usize {
    std::env::var("TF2_WRITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// The dynamic edges of `tree` that the `target <- source` plan does **not**
/// traverse.
///
/// Off-path writing isolates tf2's whole-buffer mutex against per-edge rings;
/// on-path writing would time the error path (`src/bin/contended_scaling.rs`).
/// Edges are compared by `EdgeId` from a one-step plan, so an edge reached
/// inverted still counts.
fn writable_edges(
    tree: &Tree,
    target: &str,
    source: &str,
    dynamic: &[(String, String)],
) -> Vec<(String, String)> {
    let edge_of = |parent: &str, child: &str| -> Option<tf_tree::EdgeId> {
        let (p, c) = (tree.frame(parent).ok()?, tree.frame(child).ok()?);
        tree.plan(c, p).ok()?.steps().iter().find_map(|s| match s {
            tf_tree::Step::Dyn { edge, .. } => Some(*edge),
            tf_tree::Step::Static(_) => None,
        })
    };

    let on_path: Vec<tf_tree::EdgeId> = (|| {
        let (t, s) = (tree.frame(target).ok()?, tree.frame(source).ok()?);
        Some(
            tree.plan(t, s)
                .ok()?
                .steps()
                .iter()
                .filter_map(|st| match st {
                    tf_tree::Step::Dyn { edge, .. } => Some(*edge),
                    tf_tree::Step::Static(_) => None,
                })
                .collect(),
        )
    })()
    .unwrap_or_default();

    dynamic
        .iter()
        .filter(|(p, c)| edge_of(p, c).is_some_and(|e| !on_path.contains(&e)))
        .cloned()
        .collect()
}

/// One writer thread: publish to `edge` on the engine the round selected.
///
/// `which` is `Some(a)` in the throughput pass (writer follows the reader's
/// engine) and `None` in the latency pass (writer alternates as readers do).
fn writer_loop(
    load: &Load,
    edge: &(String, String),
    which: Option<&AtomicUsize>,
    stop: &AtomicBool,
    seed: usize,
) {
    let (parent, child) = edge;
    let (Ok(p), Ok(c)) = (load.tree.frame(parent), load.tree.frame(child)) else {
        return;
    };
    let Ok(w) = load.tree.claim(c, p) else { return };
    let (Ok(pc), Ok(cc)) = (FrameName::new(parent), FrameName::new(child)) else {
        return;
    };

    // Stamps come from `Load::next_stamp`; both engines get the same sequence.
    let pose = fixture::dynamic_pose(seed as f64, 0);
    let mut alternating = 0usize;

    while !stop.load(Ordering::Acquire) {
        let stamp = load.next_stamp.fetch_add(1_000_000, Ordering::Relaxed);
        let engine = match which {
            Some(a) => ENGINES[a.load(Ordering::Acquire)],
            None => {
                alternating ^= 1;
                ENGINES[alternating]
            }
        };
        match engine {
            Engine::TfTree => {
                let _ = w.push(stamp, &pose);
            }
            Engine::Tf2 => {
                let _ = load
                    .tf2
                    .set_transform_by_name(&pc, &cc, stamp, &pose, false);
            }
        }
    }
}

/// Which engine a worker drives; the lookups and stamps are identical.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Engine {
    TfTree,
    Tf2,
}

/// Index-addressable engine list, selected through a single atomic.
const ENGINES: [Engine; 2] = [Engine::TfTree, Engine::Tf2];

impl Engine {
    fn label(self) -> &'static str {
        match self {
            Engine::TfTree => "tf_tree",
            Engine::Tf2 => "tf2",
        }
    }
}

/// One worker's pass over the stamp sweep; returns an accumulator so the work is not optimised out.
fn pass(engine: Engine, load: &Load, plan: &Plan) -> f64 {
    let mut acc = 0.0f64;
    match engine {
        Engine::TfTree => {
            // A fresh guard per pass, as a real reader takes per batch.
            let guard = load.tree.guard();
            for &ns in &load.stamps {
                let stamp: Stamp = Stamp::from_nanos(ns);
                if let Ok(p) = plan.at(&guard, stamp) {
                    acc += p.t.x;
                }
            }
        }
        Engine::Tf2 => {
            for &ns in &load.stamps {
                if let Ok(p) = load.tf2.lookup_by_name(&load.target_c, &load.source_c, ns) {
                    acc += p.t.x;
                }
            }
        }
    }
    acc
}

/// Throughput of both engines at `threads`, interleaved per round so drift hits both.
fn measure_throughput_pair(load: &Load, plan: &Plan, threads: usize) -> [Stats; 2] {
    let rounds = env_usize("TF2_ROUNDS", 51);
    let per_round = load.stamps.len();
    let start = Barrier::new(threads);
    let done = Barrier::new(threads);
    let stop = AtomicBool::new(false);
    // Engine this round: 0 = tf_tree, 1 = tf2; published before the barrier.
    let which = AtomicUsize::new(0);
    let (start, done, stop, which) = (&start, &done, &stop, &which);

    let mut ns: [Vec<u128>; 2] = [Vec::with_capacity(rounds), Vec::with_capacity(rounds)];

    // Separate from `stop`: writers run across barrier waits.
    let stop_writers = AtomicBool::new(false);
    let stop_writers = &stop_writers;
    let writers = writer_count().min(load.writable.len());

    thread::scope(|scope| {
        for i in 0..writers {
            let edge = &load.writable[i % load.writable.len()];
            scope.spawn(move || writer_loop(load, edge, Some(which), stop_writers, i));
        }
        for _ in 0..threads - 1 {
            scope.spawn(move || loop {
                start.wait();
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let e = ENGINES[which.load(Ordering::Acquire)];
                black_box(pass(e, load, plan));
                done.wait();
            });
        }

        // Warm up both engines.
        for (w, &engine) in ENGINES.iter().enumerate() {
            for _ in 0..3 {
                which.store(w, Ordering::Release);
                start.wait();
                black_box(pass(engine, load, plan));
                done.wait();
            }
        }

        for _ in 0..rounds {
            for (w, &engine) in ENGINES.iter().enumerate() {
                which.store(w, Ordering::Release);
                start.wait();
                let t0 = Instant::now();
                black_box(pass(engine, load, plan));
                done.wait();
                ns[w].push(t0.elapsed().as_nanos());
            }
        }

        stop.store(true, Ordering::Release);
        start.wait();
        stop_writers.store(true, Ordering::Release);
    });

    let total = (threads * per_round) as f64;
    core::array::from_fn(|w| {
        let mut v = ns[w].clone();
        v.sort_unstable();
        let rate = |x: u128| total / (x as f64 / 1e9);
        Stats {
            // Fastest round is least disturbed; median is the headline; the spread is trust.
            best: rate(v[0]),
            median: rate(v[v.len() / 2]),
            worst: rate(v[v.len() - 1]),
        }
    })
}

/// Throughput summary for one engine at one thread count, in lookups/s.
#[derive(Clone, Copy)]
struct Stats {
    best: f64,
    median: f64,
    worst: f64,
}

impl Stats {
    /// Median's distance from the best round, in percent; large means a soft number.
    fn spread_pct(self) -> f64 {
        (self.best - self.median) / self.best * 100.0
    }

    /// Slowest round as a fraction of the fastest.
    fn worst_ratio(self) -> f64 {
        self.worst / self.best
    }
}

/// Per-lookup latency percentiles for both engines at `threads`, alternating
/// sample by sample; includes two `Instant::now()` calls (~20 ns) per lookup.
fn measure_latency_pair(load: &Load, plan: &Plan, threads: usize) -> [Percentiles; 2] {
    let samples = env_usize("TF2_LATENCY_SAMPLES", 50_000);
    let start = Barrier::new(threads);
    let mut all: [Vec<u64>; 2] = [Vec::new(), Vec::new()];

    let stop_writers = AtomicBool::new(false);
    let stop_writers = &stop_writers;
    let writers = writer_count().min(load.writable.len());

    thread::scope(|scope| {
        let start = &start;
        for i in 0..writers {
            let edge = &load.writable[i % load.writable.len()];
            scope.spawn(move || writer_loop(load, edge, None, stop_writers, i));
        }
        let mut handles = Vec::new();
        for _ in 0..threads - 1 {
            handles.push(scope.spawn(move || {
                start.wait();
                sample_latencies(load, plan, samples)
            }));
        }
        start.wait();
        let mine = sample_latencies(load, plan, samples);
        all[0].extend(mine[0].iter().copied());
        all[1].extend(mine[1].iter().copied());
        for h in handles {
            let got = h.join().expect("worker");
            all[0].extend(got[0].iter().copied());
            all[1].extend(got[1].iter().copied());
        }
        stop_writers.store(true, Ordering::Release);
    });

    core::array::from_fn(|w| {
        all[w].sort_unstable();
        Percentiles::from_sorted(&all[w])
    })
}

/// Alternate engines sample by sample, returning `[tf_tree, tf2]` latencies.
fn sample_latencies(load: &Load, plan: &Plan, samples: usize) -> [Vec<u64>; 2] {
    let guard = load.tree.guard();
    let mut out: [Vec<u64>; 2] = [Vec::with_capacity(samples), Vec::with_capacity(samples)];
    for i in 0..samples {
        let ns = load.stamps[i % load.stamps.len()];
        for (w, engine) in ENGINES.iter().enumerate() {
            let t0 = Instant::now();
            match engine {
                Engine::TfTree => {
                    let stamp: Stamp = Stamp::from_nanos(ns);
                    black_box(plan.at(&guard, stamp).ok());
                }
                Engine::Tf2 => {
                    black_box(
                        load.tf2
                            .lookup_by_name(&load.target_c, &load.source_c, ns)
                            .ok(),
                    );
                }
            }
            out[w].push(t0.elapsed().as_nanos() as u64);
        }
    }
    out
}

struct Percentiles {
    p50: u64,
    p99: u64,
    p999: u64,
    p9999: u64,
}

impl Percentiles {
    fn from_sorted(v: &[u64]) -> Percentiles {
        let at = |q: f64| v[((v.len() as f64 * q) as usize).min(v.len() - 1)];
        Percentiles {
            p50: at(0.50),
            p99: at(0.99),
            p999: at(0.999),
            p9999: at(0.9999),
        }
    }
}

/// Physical cores, not logical CPUs: past the physical count threads share execution units.
fn physical_cores() -> Option<usize> {
    let txt = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let mut ids = std::collections::BTreeSet::new();
    let mut phys = None;
    for line in txt.lines() {
        if let Some(v) = line.strip_prefix("physical id") {
            phys = v
                .split(':')
                .nth(1)
                .and_then(|x| x.trim().parse::<u32>().ok());
        } else if let Some(v) = line.strip_prefix("core id") {
            if let (Some(p), Some(c)) = (
                phys,
                v.split(':')
                    .nth(1)
                    .and_then(|x| x.trim().parse::<u32>().ok()),
            ) {
                ids.insert((p, c));
            }
        }
    }
    (!ids.is_empty()).then_some(ids.len())
}

fn main() {
    let logical = thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
    let cores = physical_cores();
    let threads = thread_counts();
    let rounds = env_usize("TF2_ROUNDS", 51);
    let lat = env_usize("TF2_LATENCY_SAMPLES", 50_000);

    println!("tf_tree vs tf2 — concurrent read scaling");
    println!("========================================\n");
    println!("logical CPUs   : {logical}");
    match cores {
        Some(c) => println!(
            "physical cores : {c}   <- scaling ceiling; beyond this, threads share \n\
             {:17}execution units (SMT) and throughput flattens by design",
            ""
        ),
        None => println!("physical cores : unknown"),
    }
    println!("thread counts  : {threads:?}");
    println!("rounds         : {rounds} per engine per thread count, engines interleaved");
    println!("latency        : {lat} samples/thread/engine, interleaved\n");
    println!("Engines alternate within every round, so drift lands on both equally.");
    println!("`spread` is (best - median)/best: small means the machine was quiet.");
    let writers = writer_count();
    if writers == 0 {
        println!("writers        : 0 (quiescent tree). Set TF2_WRITERS=N for the contended rows —");
        println!(
            "                 that is docs/PHASE1.md §11.2's configuration, and the row where"
        );
        println!(
            "                 tf2's single buffer mutex and tf_tree's per-edge seqlock differ."
        );
    } else {
        println!("writers        : {writers} per engine, on dynamic edges the query path does NOT");
        println!("                 traverse. That is the measurement: tf2::BufferCore takes one");
        println!("                 mutex for the WHOLE buffer, so a write to any edge excludes");
        println!("                 every reader; tf_tree's rings are per edge, so it costs a");
        println!("                 reader of another edge nothing. Writing ON path additionally");
        println!("                 slides the queried window and is not measured here.");
        println!(
            "                 Budget cores: {writers} writers + N readers must fit, or the rows"
        );
        println!("                 are scheduler noise.");
    }
    println!();

    for load in [fixture_load(), replay_load()] {
        let t = load.tree.frame(&load.target).unwrap();
        let s = load.tree.frame(&load.source).unwrap();
        let plan = load.tree.plan(t, s).unwrap();

        println!("## {}\n", load.name);
        println!(
            "{:<8} {:>12} {:>8} {:>12} {:>8} {:>8} {:>10} {:>8} {:>7}",
            "threads",
            "tf_tree M/s",
            "spread",
            "tf2 M/s",
            "spread",
            "ratio",
            "tf_tree x1",
            "tf2 x1",
            "worst"
        );

        let mut base = [0.0f64; 2];
        for (i, &n) in threads.iter().enumerate() {
            let [ours, theirs] = measure_throughput_pair(&load, &plan, n);
            if i == 0 {
                base = [ours.median, theirs.median];
            }
            println!(
                "{n:<8} {:>12.2} {:>7.1}% {:>12.2} {:>7.1}% {:>7.2}x {:>9.2}x {:>7.2}x {:>6.2}",
                ours.median / 1e6,
                ours.spread_pct(),
                theirs.median / 1e6,
                theirs.spread_pct(),
                ours.median / theirs.median,
                ours.median / base[0],
                theirs.median / base[1],
                ours.worst_ratio().min(theirs.worst_ratio()),
            );
        }

        println!();
        println!(
            "{:<8} {:<9} {:>9} {:>9} {:>10} {:>11}",
            "threads", "engine", "p50 ns", "p99 ns", "p99.9 ns", "p99.99 ns"
        );
        for &n in &threads {
            let pcts = measure_latency_pair(&load, &plan, n);
            for (w, p) in pcts.iter().enumerate() {
                println!(
                    "{n:<8} {:<9} {:>9} {:>9} {:>10} {:>11}",
                    ENGINES[w].label(),
                    p.p50,
                    p.p99,
                    p.p999,
                    p.p9999
                );
            }
        }
        println!();
    }

    println!("Latencies include ~20 ns of `Instant::now()` overhead per lookup,");
    println!("identical for both engines. Throughput rows are batch-timed and do not.");
}
