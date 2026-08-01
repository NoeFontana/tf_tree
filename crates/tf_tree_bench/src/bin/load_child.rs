//! One reader or writer process for the contended-scaling harness.
//!
//! Attaches to the shared arena handed over on stdin (`shm_util`) and plays one
//! of two roles, selected by argv:
//!
//! * `reader` — two passes over the same query set, because
//!   `docs/PHASE1.md` §11.2 asks for **both** aggregate throughput and per-lookup
//!   p99.9, and one loop cannot honestly produce both. See below.
//! * `writer` — claims one edge and publishes at its rate for the window.
//!
//! # Why a reader runs two passes rather than one
//!
//! This is `src/bin/tf2_scaling.rs`'s finding, and it is not an implementation
//! detail: *"reading a clock around every lookup costs ~20 ns, which would
//! visibly distort a ~100 ns operation"*. So the throughput pass times whole
//! batches and never touches a clock inside the loop, and only the latency pass
//! pays for per-op timestamps — and it **measures** what that costs
//! (`clock_overhead_ns`) and reports it, so a reader can subtract it rather than
//! take the harness's word.
//!
//! The window is split in half between the passes. The writers, spawned by the
//! coordinator, run across both, so every reader pass sees a live tree.
//!
//! # Why a reader is a process and not a thread
//!
//! Per-*thread* core pinning needs `sched_setaffinity`, which is `unsafe` and
//! which `CLAUDE.md`'s unsafe budget would route to a decision record —
//! `tf_tree_bench`'s library is `#![forbid(unsafe_code)]`. A process can be
//! placed exactly by `taskset -c N`, needs no `unsafe` at all, and is closer to
//! the shape a deployment actually has. The coordinator does the placing; this
//! binary knows nothing about it.
//!
//! Output is a line protocol on stdout, read by the coordinator. Usage errors go
//! to stderr so they cannot be mistaken for measurements.
// `panic!` is in this list with the others: every use below names the frame,
// pair or edge that failed. A harness that dies saying only "unwrap on a None"
// costs a re-run to find out which of sixteen children it was.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::hint::black_box;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use tf_tree::{AttachMode, Plan, Stamp, Tree};
use tf_tree_bench::fixture;
use tf_tree_bench::mp::{Histogram, ProcStats, RateLoop};

/// How many clock-pair reads the overhead control performs.
const CLOCK_CALIBRATION_ITERS: usize = 100_000;

fn usage() -> ! {
    eprintln!(
        "usage:\n  \
         load_child reader <hz> <seconds> <lo_ns> <hi_ns> <target|source> [...]\n  \
         load_child writer <rate_hz> <seconds> <seed> <next_stamp_ns> <parent> <child>\n\
         \n\
         The shared arena arrives as this process's standard input."
    );
    std::process::exit(2)
}

fn attach(mode: AttachMode) -> Tree {
    let fd = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .expect("segment from stdin");
    Tree::attach_shared(fd, mode).expect("attach to the shared arena")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("reader") if args.len() >= 6 => reader(&args[1..]),
        Some("writer") if args.len() == 7 => writer(&args[1..]),
        _ => usage(),
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

fn reader(args: &[String]) {
    let hz: f64 = args[0].parse().expect("hz");
    let seconds: f64 = args[1].parse().expect("seconds");
    let lo: i64 = args[2].parse().expect("lo_ns");
    let hi: i64 = args[3].parse().expect("hi_ns");

    let tree = attach(AttachMode::ReadOnly);
    let plans: Vec<Plan> = args[4..]
        .iter()
        .map(|spec| {
            let (t, s) = spec
                .split_once('|')
                .unwrap_or_else(|| panic!("pair {spec:?} is not `target|source`"));
            let tid = tree.frame(t).unwrap_or_else(|e| panic!("frame {t}: {e:?}"));
            let sid = tree.frame(s).unwrap_or_else(|e| panic!("frame {s}: {e:?}"));
            tree.plan(tid, sid)
                .unwrap_or_else(|e| panic!("plan {t} <- {s}: {e:?}"))
        })
        .collect();

    // Stamps sweep the whole retained window rather than sitting in one spot.
    // `read_scaling.rs` records why: a sweep collapsed into a microsecond lands
    // in one bracket on one pair of cache lines, which is a degenerate best case
    // for exactly the benchmark meant to expose contention.
    let stamps: Vec<i64> = (0..STAMP_STEPS)
        .map(|k| lo + (hi - lo) * k as i64 / STAMP_STEPS as i64)
        .collect();

    // Warm the plans and the pages before any clock starts; first-touch cost is
    // `docs/PHASE2.md` §7.1's separate measurement, not part of steady state.
    {
        let guard = tree.guard();
        for p in &plans {
            let warm: Stamp = Stamp::from_nanos(stamps[0]);
            let _ = p.at(&guard, warm);
        }
    }

    let third = Duration::from_secs_f64(seconds / 3.0);
    let before = ProcStats::read();

    let (ops, elapsed_ns, declined_t) = throughput_pass(&tree, &plans, &stamps, third);
    let (service, declined_s, done) = service_pass(&tree, &plans, &stamps, third);
    let (cycle, declined_c) = cycle_pass(&tree, &plans, &stamps, hz, third);
    let clock_overhead_ns = clock_overhead();

    let after = ProcStats::read();
    let d = after.since(before);

    println!("throughput {ops} {elapsed_ns}");
    println!("service {}", service.encode());
    println!("cycle {}", cycle.encode());
    println!("clock_overhead_ns {clock_overhead_ns}");
    println!("declined {}", declined_t + declined_s + declined_c);
    println!("attempted {}", ops + done + cycle.count());
    println!("cpu_ns {}", d.cpu_ns);
    println!("pss_kib {}", d.pss_kib);
}

/// Distinct stamps a reader sweeps across the retained window.
const STAMP_STEPS: usize = 1024;

/// Closed loop, no clock inside: the aggregate-throughput half of §11.2.
///
/// Returns `(lookups, elapsed_ns, declined)`. The clock is read once per
/// `BATCH` lookups, which at ~100 ns each puts the reading three orders below
/// the batch it bounds.
fn throughput_pass(
    tree: &Tree,
    plans: &[Plan],
    stamps: &[i64],
    window: Duration,
) -> (u64, u64, u64) {
    const BATCH: usize = 1024;
    let start = Instant::now();
    let (mut ops, mut declined) = (0u64, 0u64);
    let mut k = 0usize;
    let mut acc = 0.0f64;

    while start.elapsed() < window {
        // One guard per batch, not one per lookup. That is what a node doing a
        // burst of work does, and it is also what `docs/PHASE5.md`'s
        // "convenience-path guard reuse" rule asks of the engine's own
        // convenience path — O(1) atomic flushes, not O(n).
        let guard = tree.guard();
        for _ in 0..BATCH {
            let stamp: Stamp = Stamp::from_nanos(stamps[k % stamps.len()]);
            let plan = &plans[k % plans.len()];
            match plan.at(&guard, stamp) {
                Ok(p) => acc += p.t.x,
                Err(_) => declined += 1,
            }
            k += 1;
        }
        ops += BATCH as u64;
    }
    black_box(acc);
    (ops, start.elapsed().as_nanos() as u64, declined)
}

/// Dense, one clock pair per lookup: **§11.2's per-lookup p99.9**.
///
/// Returns `(histogram, declined, attempted)`.
///
/// This is the engine's own distribution, and it is deliberately *not* rate
/// limited. The first revision of this harness measured latency only on an
/// open-loop schedule and reported a p50 of 61 µs for an operation that costs
/// roughly 100 ns — because at any achievable tick rate the dominant term is the
/// OS deciding to run you. `src/mp.rs` documents the same split and the same
/// magnitudes: "~75 µs of it is the OS deciding to run you, against ~2 µs of
/// actual work". A comparison between two engines belongs in *this* column.
///
/// The `clock_overhead_ns` control is included in every sample here and is
/// reported separately so it can be subtracted rather than taken on trust.
fn service_pass(
    tree: &Tree,
    plans: &[Plan],
    stamps: &[i64],
    window: Duration,
) -> (Histogram, u64, u64) {
    let mut hist = Histogram::new();
    let (mut declined, mut attempted) = (0u64, 0u64);
    let start = Instant::now();
    let mut k = 0usize;
    let mut acc = 0.0f64;

    // The elapsed check is hoisted out of the per-lookup loop: it is itself a
    // clock read, and doing it per lookup would double the overhead this pass
    // is trying to bound.
    while start.elapsed() < window {
        let guard = tree.guard();
        for _ in 0..256 {
            let stamp: Stamp = Stamp::from_nanos(stamps[k % stamps.len()]);
            let plan = &plans[k % plans.len()];
            let t0 = Instant::now();
            match plan.at(&guard, stamp) {
                Ok(p) => acc += p.t.x,
                Err(_) => declined += 1,
            }
            hist.record(t0.elapsed().as_nanos() as u64);
            attempted += 1;
            k += 1;
        }
    }
    black_box(acc);
    (hist, declined, attempted)
}

/// Open loop at `hz`, measured from each tick's **intended** time: what a node
/// experiences.
///
/// Open loop matters for the reason `src/mp.rs` gives at length: a closed loop
/// cannot measure this, because a stall *reduces* the offered load and every
/// recorded sample then looks fast. That is coordinated omission. What this
/// column mostly measures on an idle machine is scheduler wakeup — which is the
/// truth about a node's latency, and the reason it is reported next to, and not
/// instead of, the service distribution.
fn cycle_pass(
    tree: &Tree,
    plans: &[Plan],
    stamps: &[i64],
    hz: f64,
    window: Duration,
) -> (Histogram, u64) {
    let mut hist = Histogram::new();
    let mut declined = 0u64;
    let mut rate = RateLoop::new(hz);
    let start = Instant::now();
    let mut k = 0usize;
    let mut acc = 0.0f64;

    while start.elapsed() < window {
        let due = rate.next_due();
        let stamp: Stamp = Stamp::from_nanos(stamps[k % stamps.len()]);
        let plan = &plans[k % plans.len()];
        let guard = tree.guard();
        match plan.at(&guard, stamp) {
            Ok(p) => acc += p.t.x,
            Err(_) => declined += 1,
        }
        drop(guard);
        hist.record(Instant::now().duration_since(due).as_nanos() as u64);
        k += 1;
    }
    black_box(acc);
    (hist, declined)
}

/// What one `Instant::now()` pair costs on this host, in nanoseconds.
///
/// Reported rather than assumed. The latency pass includes it in every sample,
/// and on a host where `clock_gettime` is not in the vDSO it is not ~20 ns but
/// hundreds — which would be indistinguishable from a slow engine if the number
/// were never taken.
fn clock_overhead() -> u64 {
    let start = Instant::now();
    for _ in 0..CLOCK_CALIBRATION_ITERS {
        black_box(Instant::now());
    }
    start.elapsed().as_nanos() as u64 / CLOCK_CALIBRATION_ITERS as u64
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

fn writer(args: &[String]) {
    let rate_hz: f64 = args[0].parse().expect("rate_hz");
    let seconds: f64 = args[1].parse().expect("seconds");
    let seed: f64 = args[2].parse().expect("seed");
    let next_stamp_ns: i64 = args[3].parse().expect("next_stamp_ns");
    let (parent_name, child_name) = (&args[4], &args[5]);

    let tree = attach(AttachMode::ReadWrite);
    let parent = tree
        .frame(parent_name)
        .unwrap_or_else(|e| panic!("frame {parent_name}: {e:?}"));
    let child = tree
        .frame(child_name)
        .unwrap_or_else(|e| panic!("frame {child_name}: {e:?}"));
    let w = tree
        .claim(child, parent)
        .unwrap_or_else(|e| panic!("claim {parent_name}->{child_name}: {e}"));

    let step_ns = (1e9 / rate_hz) as i64;
    let mut rate = RateLoop::new(rate_hz);
    let start = Instant::now();
    let window = Duration::from_secs_f64(seconds);
    let (mut pushed, mut rejected) = (0u64, 0u64);
    let mut stamp = next_stamp_ns;

    let before = ProcStats::read();
    while start.elapsed() < window {
        // Open loop here too: a writer that falls behind must publish the
        // schedule it owes rather than silently reduce the tree's update rate,
        // which is the thing the readers are being contended by.
        let _due = rate.next_due();
        match w.push(stamp, &fixture::dynamic_pose(seed, stamp)) {
            Ok(()) => pushed += 1,
            Err(_) => rejected += 1,
        }
        stamp += step_ns;
    }
    let after = ProcStats::read();
    let d = after.since(before);

    println!("pushed {pushed}");
    // Reported rather than swallowed. A writer whose pushes are being rejected
    // is not contending with anything, and a scaling row taken against one is a
    // row against a quiescent tree wearing a writer's name.
    println!("rejected {rejected}");
    println!("cpu_ns {}", d.cpu_ns);
    println!("pss_kib {}", d.pss_kib);
}
