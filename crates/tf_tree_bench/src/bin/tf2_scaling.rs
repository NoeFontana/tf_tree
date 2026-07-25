//! Concurrent read scaling: tf_tree (lock-free readers) vs `tf2::BufferCore`
//! (one internal mutex per lookup), at 1 / 2 / 4 / 8 threads.
//!
//! This is the measurement decision `0003` calls for and the one most likely to
//! separate the two engines: tf_tree's readers take no lock at all, while every
//! `tf2::lookupTransform` acquires `BufferCore`'s frame mutex. If that matters,
//! it shows up here and nowhere else.
//!
//! Run it with `just tf2-scaling`. **Run it on an otherwise idle machine** — the
//! numbers are worthless under competing load, and the harness says so in its
//! own output.
//!
//! # Why a standalone binary rather than criterion
//!
//! Decision `0003` is explicit that "p99.9 is the number that matters, not the
//! mean. A control loop cares about the tail." Criterion reports the
//! distribution of *batch* times, which is the wrong distribution — it hides
//! exactly the per-lookup outliers a lock introduces. This harness records
//! per-lookup latencies and reports the tail directly.
//!
//! # Method
//!
//! * **One** shared tree and **one** shared `BufferCore`, as both engines are
//!   meant to be used. Per-thread buffers would erase the contention.
//! * Threads are spawned once and parked on a barrier; the timed region contains
//!   no thread creation. The driver takes a share of the work itself, so `N`
//!   threads means `N` runnable threads, not `N + 1` competing for `N` cores.
//! * Stamps sweep the whole retained window, so the bracket search does real
//!   work instead of hitting one cached pair.
//! * Throughput and latency are measured in **separate passes**: reading a clock
//!   around every lookup costs ~20 ns, which would visibly distort a ~100 ns
//!   operation. The throughput pass therefore times whole batches, and only the
//!   latency pass pays for per-op timestamps.
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

/// Thread counts to sweep. Override with `TF2_THREADS=1,2,4,8`.
fn thread_counts() -> Vec<usize> {
    match std::env::var("TF2_THREADS") {
        Ok(v) => v
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .filter(|&n: &usize| n > 0)
            .collect(),
        Err(_) => std::vec![1, 2, 4, 8],
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A workload both engines can serve identically.
struct Load {
    name: &'static str,
    tree: Tree,
    tf2: Tf2Buffer,
    target: String,
    source: String,
    /// The same names pre-converted for the FFI boundary. Converting per call
    /// would heap-allocate twice per lookup and charge it to tf2, while
    /// `Plan::at` takes no strings at all.
    target_c: FrameName,
    source_c: FrameName,
    stamps: Vec<i64>,
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
    let stamps = (0..per_round)
        .map(|k| lo + (now - lo) * k / per_round)
        .collect();
    Load {
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
    let stamps = (0..per_round)
        .map(|k| lo + (hi - lo) * k / per_round)
        .collect();
    Load {
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

/// Which engine a worker drives. Both do the same lookups over the
/// same stamps, so the only difference is the engine underneath.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Engine {
    TfTree,
    Tf2,
}

/// Index-addressable engine list, so a worker can be told which to run through
/// a single atomic.
const ENGINES: [Engine; 2] = [Engine::TfTree, Engine::Tf2];

impl Engine {
    fn label(self) -> &'static str {
        match self {
            Engine::TfTree => "tf_tree",
            Engine::Tf2 => "tf2",
        }
    }
}

/// One worker's pass over the whole stamp sweep. Returns an accumulator so the
/// optimiser cannot delete the work.
fn pass(engine: Engine, load: &Load, plan: &Plan) -> f64 {
    let mut acc = 0.0f64;
    match engine {
        Engine::TfTree => {
            // A fresh guard per pass, as a real reader would take per batch.
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

/// Throughput of **both** engines at `threads`, measured interleaved.
///
/// The two engines alternate within every round rather than being measured in
/// separate blocks. Anything that drifts over the run — a background task
/// waking, the host migrating a vCPU, a thermal or steal-time excursion — then
/// lands on both engines equally instead of on whichever was measured while it
/// happened. Measuring A fully, then B fully, silently attributes drift to the
/// engine unlucky enough to be second.
fn measure_throughput_pair(load: &Load, plan: &Plan, threads: usize) -> [Stats; 2] {
    let rounds = env_usize("TF2_ROUNDS", 51);
    let per_round = load.stamps.len();
    let start = Barrier::new(threads);
    let done = Barrier::new(threads);
    let stop = AtomicBool::new(false);
    // Which engine this round: 0 = tf_tree, 1 = tf2. Published by the driver
    // before releasing the barrier, so every worker reads the same value.
    let which = AtomicUsize::new(0);
    let (start, done, stop, which) = (&start, &done, &stop, &which);

    let mut ns: [Vec<u128>; 2] = [Vec::with_capacity(rounds), Vec::with_capacity(rounds)];

    thread::scope(|scope| {
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

        // Warm up both engines: page in the arenas, settle the caches, let the
        // scheduler place the threads.
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
    });

    let total = (threads * per_round) as f64;
    core::array::from_fn(|w| {
        let mut v = ns[w].clone();
        v.sort_unstable();
        let rate = |x: u128| total / (x as f64 / 1e9);
        Stats {
            // Fastest round is the least-disturbed one; median is the headline;
            // the spread between them is how much to trust it.
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
    /// How far the median sits from the best round, as a percentage. A large
    /// value means the machine was disturbed and the number is soft.
    fn spread_pct(self) -> f64 {
        (self.best - self.median) / self.best * 100.0
    }

    /// Slowest round, as a fraction of the fastest. A tail far below 1.0 means
    /// at least one round was badly disturbed; kept so a quiet-machine claim can
    /// be checked rather than asserted.
    fn worst_ratio(self) -> f64 {
        self.worst / self.best
    }
}

/// Per-lookup latency percentiles for **both** engines at `threads`.
///
/// Interleaved for the same reason as throughput: each thread alternates
/// engines sample by sample, so any disturbance is shared rather than charged to
/// one engine.
///
/// The figures include two `Instant::now()` calls per lookup (~20 ns here).
/// That overhead is identical for both engines, so comparisons hold, but the
/// absolute values are inflated by roughly that much.
fn measure_latency_pair(load: &Load, plan: &Plan, threads: usize) -> [Percentiles; 2] {
    let samples = env_usize("TF2_LATENCY_SAMPLES", 50_000);
    let start = Barrier::new(threads);
    let mut all: [Vec<u64>; 2] = [Vec::new(), Vec::new()];

    thread::scope(|scope| {
        let start = &start;
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

/// Physical cores, distinguished from logical CPUs.
///
/// This matters more than anything else for reading the scaling table: on an
/// SMT machine, thread counts past the physical core count share execution
/// units, so the ceiling is the *core* count, not the CPU count. Reporting 8
/// logical CPUs and then wondering why scaling stops near 4x would be a
/// self-inflicted mystery.
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
    println!("`spread` is (best - median)/best: small means the machine was quiet.\n");

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
