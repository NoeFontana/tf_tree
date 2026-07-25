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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use tf_tree::{InterpPolicy, Plan, Stamp, Tree};
use tf_tree_bench::{fixture, replay, replay_tf2};
use tf_tree_tf2_sys::{FrameName, Tf2Buffer};

/// Thread counts to sweep.
const THREADS: &[usize] = &[1, 2, 4, 8];
/// Lookups per thread per timed round.
const PER_ROUND: usize = 4096;
/// Timed rounds per configuration; the median round is reported.
const ROUNDS: usize = 25;
/// Lookups per thread in the latency pass.
const LATENCY_SAMPLES: usize = 20_000;

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
    let stamps = (0..PER_ROUND as i64)
        .map(|k| lo + (now - lo) * k / PER_ROUND as i64)
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
    let stamps = (0..PER_ROUND as i64)
        .map(|k| lo + (hi - lo) * k / PER_ROUND as i64)
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

/// Which engine a worker drives. Both do the same `PER_ROUND` lookups over the
/// same stamps, so the only difference is the engine underneath.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Engine {
    TfTree,
    Tf2,
}

impl Engine {
    fn label(self) -> &'static str {
        match self {
            Engine::TfTree => "tf_tree",
            Engine::Tf2 => "tf2",
        }
    }
}

/// One worker's pass: `PER_ROUND` lookups. Returns an accumulator so the
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

/// Throughput at `threads`, in lookups per second, from the median timed round.
fn measure_throughput(engine: Engine, load: &Load, plan: &Plan, threads: usize) -> f64 {
    let start = Barrier::new(threads);
    let done = Barrier::new(threads);
    let stop = AtomicBool::new(false);
    let (start, done, stop) = (&start, &done, &stop);
    let mut round_ns: Vec<u128> = Vec::with_capacity(ROUNDS);

    thread::scope(|scope| {
        for _ in 0..threads - 1 {
            scope.spawn(move || loop {
                start.wait();
                if stop.load(Ordering::Acquire) {
                    break;
                }
                black_box(pass(engine, load, plan));
                done.wait();
            });
        }

        // Warm up: page in the arenas, settle the caches, let the scheduler
        // place the threads.
        for _ in 0..3 {
            start.wait();
            black_box(pass(engine, load, plan));
            done.wait();
        }

        for _ in 0..ROUNDS {
            start.wait();
            let t0 = Instant::now();
            black_box(pass(engine, load, plan));
            done.wait();
            round_ns.push(t0.elapsed().as_nanos());
        }

        stop.store(true, Ordering::Release);
        start.wait();
    });

    round_ns.sort_unstable();
    let median = round_ns[round_ns.len() / 2] as f64;
    let total = (threads * PER_ROUND) as f64;
    total / (median / 1e9)
}

/// Per-lookup latency percentiles at `threads`, in nanoseconds.
///
/// Includes the cost of two `Instant::now()` calls per lookup (~20 ns on this
/// class of machine). That overhead is identical for both engines, so the
/// comparison stands even though the absolute values are inflated.
fn measure_latency(engine: Engine, load: &Load, plan: &Plan, threads: usize) -> Percentiles {
    let start = Barrier::new(threads);
    let mut samples: Vec<u64> = Vec::new();

    thread::scope(|scope| {
        let start = &start;
        let mut handles = Vec::new();
        for _ in 0..threads - 1 {
            handles.push(scope.spawn(move || {
                start.wait();
                sample_latencies(engine, load, plan)
            }));
        }
        start.wait();
        samples.extend(sample_latencies(engine, load, plan));
        for h in handles {
            samples.extend(h.join().expect("worker"));
        }
    });

    samples.sort_unstable();
    Percentiles::from_sorted(&samples)
}

fn sample_latencies(engine: Engine, load: &Load, plan: &Plan) -> Vec<u64> {
    let guard = load.tree.guard();
    let mut out = Vec::with_capacity(LATENCY_SAMPLES);
    for i in 0..LATENCY_SAMPLES {
        let ns = load.stamps[i % load.stamps.len()];
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
        out.push(t0.elapsed().as_nanos() as u64);
    }
    out
}

struct Percentiles {
    p50: u64,
    p99: u64,
    p999: u64,
}

impl Percentiles {
    fn from_sorted(v: &[u64]) -> Percentiles {
        let at = |q: f64| v[((v.len() as f64 * q) as usize).min(v.len() - 1)];
        Percentiles {
            p50: at(0.50),
            p99: at(0.99),
            p999: at(0.999),
        }
    }
}

fn main() {
    println!("tf_tree vs tf2 — concurrent read scaling");
    println!("========================================\n");
    println!(
        "cores available: {}",
        thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get)
    );
    println!(
        "config: {PER_ROUND} lookups/thread/round, {ROUNDS} rounds (median reported), \
         {LATENCY_SAMPLES} latency samples/thread\n"
    );
    println!("NOTE: run this on an otherwise idle machine. Competing load makes");
    println!("      these numbers meaningless, especially at 8 threads.\n");

    for load in [fixture_load(), replay_load()] {
        let t = load.tree.frame(&load.target).unwrap();
        let s = load.tree.frame(&load.source).unwrap();
        let plan = load.tree.plan(t, s).unwrap();

        println!("## {}", load.name);
        println!();
        println!(
            "{:<8} {:>14} {:>14} {:>8} {:>10} {:>10}",
            "threads", "tf_tree Mlk/s", "tf2 Mlk/s", "ratio", "tf_tree x1", "tf2 x1"
        );

        let mut base = [0.0f64; 2];
        for (i, &n) in THREADS.iter().enumerate() {
            let ours = measure_throughput(Engine::TfTree, &load, &plan, n);
            let theirs = measure_throughput(Engine::Tf2, &load, &plan, n);
            if i == 0 {
                base = [ours, theirs];
            }
            println!(
                "{n:<8} {:>14.2} {:>14.2} {:>7.2}x {:>9.2}x {:>9.2}x",
                ours / 1e6,
                theirs / 1e6,
                ours / theirs,
                ours / base[0],
                theirs / base[1],
            );
        }

        println!();
        println!(
            "{:<8} {:<9} {:>9} {:>9} {:>9}",
            "threads", "engine", "p50 ns", "p99 ns", "p99.9 ns"
        );
        for &n in THREADS {
            for engine in [Engine::TfTree, Engine::Tf2] {
                let p = measure_latency(engine, &load, &plan, n);
                println!(
                    "{n:<8} {:<9} {:>9} {:>9} {:>9}",
                    engine.label(),
                    p.p50,
                    p.p99,
                    p.p999
                );
            }
        }
        println!();
    }

    println!("Latencies include ~20 ns of `Instant::now()` overhead per lookup,");
    println!("identical for both engines. Throughput rows do not (batch-timed).");
    let _ = Duration::from_secs(0);
}
