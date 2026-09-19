//! Long-duration steady state: does anything **drift**?
//!
//! Watches a healthy system over minutes to hours for latency drift (last
//! interval's p99.9 vs the first's), memory growth (the arena is fixed-capacity,
//! so any growth is outside it), ring wraparound (only exercised by a run that
//! laps the rings), and publish-to-visible latency (`docs/PHASE5.md` §9.2).
//!
//! Not `shm_torture` (`docs/PHASE2.md` §11.4): nothing is killed and nothing
//! is asserted about consistency.
//!
//! It can fail: the last interval's p99.9 may not exceed the first's by more
//! than [`DRIFT_FACTOR`], and RSS may not grow by more than [`RSS_GROWTH_KIB`];
//! either exits non-zero and says which.
//!
//! Usage: `just soak`, `just soak-long`, or
//! `soak --workload fleet_16 --duration 30m --interval 60s --json out.json`.
// This binary's output IS its result.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tf_tree::{InterpPolicy, Stamp};
use tf_tree_bench::fixture;
use tf_tree_bench::mp::{Histogram, ProcStats};
use tf_tree_bench::report::Metric;
use tf_tree_bench::runstore::{Run, RunRow};
use tf_tree_bench::workload::{self, Backing, Built};

/// How much the last interval's p99.9 may exceed the first's before the soak
/// fails. Loose on purpose: the target is unbounded drift, not a 20% step on a
/// shared machine.
const DRIFT_FACTOR: f64 = 3.0;

/// How much resident memory may grow between the first and last interval, in
/// KiB. Measured as Pss; the `rss` names are kept because they are serialised
/// join keys in `tf_tree.bench-run/1`. 8 MiB covers this harness's own
/// bookkeeping and allocator retention.
const RSS_GROWTH_KIB: u64 = 8 * 1024;

/// Reader threads. Kept below the core count so the soak does not spend hours
/// measuring the scheduler.
const READERS: usize = 2;
/// Writer threads, capped: past a handful the question stops being "does it
/// drift" and starts being "does it scale", which is `contended_scaling`'s.
const MAX_WRITERS: usize = 4;
/// Distinct stamps a reader sweeps within the current window.
const STAMP_STEPS: usize = 512;

struct Args {
    workload: &'static str,
    duration: Duration,
    interval: Duration,
    json: Option<PathBuf>,
}

/// Parse `90s`, `5m`, `2h`, or a bare number of seconds.
fn parse_duration(s: &str) -> Option<Duration> {
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1.0),
        'm' => (&s[..s.len() - 1], 60.0),
        'h' => (&s[..s.len() - 1], 3600.0),
        _ => (s, 1.0),
    };
    let v: f64 = num.parse().ok()?;
    (v > 0.0).then(|| Duration::from_secs_f64(v * mult))
}

fn parse_args() -> Args {
    let mut a = Args {
        workload: "robot",
        duration: Duration::from_secs(120),
        interval: Duration::from_secs(20),
        json: None,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let need = |i: usize| -> String {
            argv.get(i + 1)
                .cloned()
                .unwrap_or_else(|| panic!("{} needs a value", argv[i]))
        };
        match argv[i].as_str() {
            "--workload" => {
                a.workload = workload::by_name(&need(i))
                    .unwrap_or_else(|e| {
                        eprintln!("soak: {e}");
                        std::process::exit(2)
                    })
                    .name;
                i += 2;
            }
            "--duration" => {
                a.duration = parse_duration(&need(i)).expect("--duration (e.g. 30m)");
                i += 2;
            }
            "--interval" => {
                a.interval = parse_duration(&need(i)).expect("--interval (e.g. 60s)");
                i += 2;
            }
            "--json" => {
                a.json = Some(PathBuf::from(need(i)));
                i += 2;
            }
            "-h" | "--help" => {
                println!(
                    "usage: soak [--workload NAME] [--duration 30m] [--interval 60s] \
                     [--json PATH]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("soak: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    if a.interval > a.duration {
        eprintln!("soak: --interval must not exceed --duration");
        std::process::exit(2);
    }
    a
}

/// One interval's observations.
#[derive(Clone, Copy)]
struct Snapshot {
    index: usize,
    elapsed_s: f64,
    lookups: u64,
    declined: u64,
    p50_ns: u64,
    p999_ns: u64,
    pushes: u64,
    visible_p50_ns: u64,
    visible_p999_ns: u64,
    wraps: f64,
    rss_kib: u64,
    /// Arena error counters accrued during this interval (differenced; a
    /// cumulative column rises by construction).
    err_delta: u64,
}

fn main() {
    let args = parse_args();
    let w = workload::by_name(args.workload).expect("workload");
    let built = w
        .build(InterpPolicy::LerpSlerp, Backing::Heap)
        .unwrap_or_else(|e| {
            eprintln!("soak: {e:#}");
            std::process::exit(1);
        });

    let n_writers = built.publishers.len().min(MAX_WRITERS);
    let intervals = (args.duration.as_secs_f64() / args.interval.as_secs_f64()).ceil() as usize;

    println!("tf_tree soak  [workload: {}]", w.name);
    println!("==========================================");
    println!("{}", w.note);
    println!("{}", built.shape.describe());
    println!(
        "{:.0} s in {intervals} intervals of {:.0} s; {READERS} reader threads, \
         {n_writers} writer threads",
        args.duration.as_secs_f64(),
        args.interval.as_secs_f64()
    );
    println!();
    println!("`visible` is publish-to-visible: a probe writer records when `push` returned, and");
    println!("a probe reader on another thread spins until the arena reports that stamp. It is");
    println!("`docs/PHASE5.md` §9.2's required row and nothing else here measures it.");
    println!();
    println!("A small, STABLE `declined` rate is expected and is the harness's, not the engine's:");
    println!("the readers re-probe the retained window every few thousand lookups, and the");
    println!("writers slide it continuously, so queries aimed at the oldest end occasionally");
    println!("land just below it. A rate that GROWS is the signal — that would be the window");
    println!("sliding faster than the readers can follow.");
    println!();
    println!(
        "{:>4} {:>8} | {:>10} {:>9} {:>10} | {:>10} {:>10} | {:>8} {:>9} {:>9}",
        "int",
        "elapsed",
        "Mlookup/s",
        "p50 ns",
        "p99.9 ns",
        "vis p50",
        "vis p99.9",
        "wraps",
        "Pss KiB",
        "declined"
    );
    println!("  (`declined` is per interval, in parts per million of that interval's lookups)");

    let snapshots = run_soak(&built, &args, n_writers, intervals);

    println!();
    let ok = verdict(&snapshots);

    if let Some(path) = &args.json {
        let mut run = Run::begin(READERS);
        for s in &snapshots {
            run.push(json_row(w.name, &built, s));
        }
        run.push(summary_row(w.name, &built, &snapshots));
        match run.write(path) {
            Ok(()) => println!("wrote {}", path.display()),
            Err(e) => {
                eprintln!("soak: {e:#}");
                std::process::exit(1);
            }
        }
    }

    if !ok {
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)]
fn run_soak(built: &Built, args: &Args, n_writers: usize, intervals: usize) -> Vec<Snapshot> {
    let stop = AtomicBool::new(false);
    // Per-interval accumulators, reset by the coordinator at each boundary.
    let lookups = AtomicU64::new(0);
    let declined = AtomicU64::new(0);
    let pushes = AtomicU64::new(0);
    let latency = Mutex::new(Histogram::new());
    let visible = Mutex::new(Histogram::new());
    // Probe writer's latest (stamp, publish instant) as atomics.
    let probe_stamp = AtomicI64::new(i64::MIN);
    let probe_at_ns = AtomicU64::new(0);
    let origin = Instant::now();

    let mut snapshots = Vec::with_capacity(intervals);

    std::thread::scope(|scope| {
        let (stop, lookups, declined, pushes) = (&stop, &lookups, &declined, &pushes);
        let (latency, visible) = (&latency, &visible);
        let (probe_stamp, probe_at_ns) = (&probe_stamp, &probe_at_ns);
        let tree = &built.tree;

        // --- writers -----------------------------------------------------
        for i in 0..n_writers {
            let p = built.publishers[i].clone();
            let is_probe = i == 0;
            scope.spawn(move || {
                let (Ok(parent), Ok(child)) = (tree.frame(&p.parent), tree.frame(&p.child)) else {
                    return;
                };
                let Ok(w) = tree.claim(child, parent) else {
                    return;
                };
                let step_ns = (1e9 / p.rate_hz) as i64;
                let period = Duration::from_secs_f64(1.0 / p.rate_hz);
                let mut stamp = p.next_stamp_ns;
                while !stop.load(Ordering::Relaxed) {
                    if w.push(stamp, &fixture::dynamic_pose(p.seed, stamp)).is_ok() {
                        pushes.fetch_add(1, Ordering::Relaxed);
                        if is_probe {
                            // Store the instant before the stamp the reader keys
                            // on; `Release` on the stamp publishes both.
                            probe_at_ns
                                .store(origin.elapsed().as_nanos() as u64, Ordering::Relaxed);
                            probe_stamp.store(stamp, Ordering::Release);
                        }
                    }
                    stamp += step_ns;
                    std::thread::sleep(period);
                }
            });
        }

        // --- probe reader: publish-to-visible ----------------------------
        {
            let pairs = built.pairs.clone();
            let probe = built.publishers[0].clone();
            scope.spawn(move || {
                let _ = &pairs;
                let (Ok(parent), Ok(child)) = (tree.frame(&probe.parent), tree.frame(&probe.child))
                else {
                    return;
                };
                // A one-edge plan, so `span` reports this edge's newest stamp
                // and nothing else's.
                let Ok(plan) = tree.plan(child, parent) else {
                    return;
                };
                let mut seen = i64::MIN;
                while !stop.load(Ordering::Relaxed) {
                    let want = probe_stamp.load(Ordering::Acquire);
                    if want == i64::MIN || want == seen {
                        std::hint::spin_loop();
                        continue;
                    }
                    let published_at = probe_at_ns.load(Ordering::Relaxed);
                    let guard = tree.guard();
                    if let Ok(Some((_, newest))) = plan.span(&guard) {
                        if newest >= want {
                            let now = origin.elapsed().as_nanos() as u64;
                            if let Ok(mut v) = visible.lock() {
                                v.record(now.saturating_sub(published_at));
                            }
                            seen = want;
                        }
                    }
                }
            });
        }

        // --- readers -----------------------------------------------------
        for _ in 0..READERS {
            scope.spawn(move || {
                let mut plans = Vec::new();
                for (t, s) in &built.pairs {
                    let (Ok(tid), Ok(sid)) = (tree.frame(t), tree.frame(s)) else {
                        return;
                    };
                    let Ok(p) = tree.plan(tid, sid) else { return };
                    plans.push(p);
                }
                let mut k = 0usize;
                let mut acc = 0.0f64;
                let mut window = (0i64, 0i64);
                let mut refresh = 0usize;

                while !stop.load(Ordering::Relaxed) {
                    let guard = tree.guard();
                    // Re-probe the retained window periodically: writers slide
                    // it, and a fixed one would time only the error path.
                    if refresh == 0 {
                        let mut w: Option<(i64, i64)> = None;
                        for p in &plans {
                            if let Ok(Some((lo, hi))) = p.span(&guard) {
                                w = Some(match w {
                                    None => (lo, hi),
                                    Some((a, b)) => (a.max(lo), b.min(hi)),
                                });
                            }
                        }
                        if let Some(x) = w {
                            window = x;
                        }
                        refresh = 4096;
                    }
                    refresh -= 1;

                    let span = (window.1 - window.0).max(1);
                    let stamp: Stamp = Stamp::from_nanos(
                        window.0 + span * (k % STAMP_STEPS) as i64 / STAMP_STEPS as i64,
                    );
                    let plan = &plans[k % plans.len()];
                    let t0 = Instant::now();
                    match plan.at(&guard, stamp) {
                        Ok(p) => {
                            acc += p.t.x;
                            lookups.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            declined.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let ns = t0.elapsed().as_nanos() as u64;
                    if let Ok(mut h) = latency.lock() {
                        h.record(ns);
                    }
                    k += 1;
                }
                black_box(acc);
            });
        }

        // --- coordinator -------------------------------------------------
        // Ring laps per interval come from the retained span read off the
        // arena, not from a per-workload constant.
        let probe_plan = built
            .tree
            .frame(&built.publishers[0].parent)
            .ok()
            .zip(built.tree.frame(&built.publishers[0].child).ok())
            .and_then(|(p, c)| built.tree.plan(c, p).ok());
        let retained_secs = || -> f64 {
            let Some(plan) = probe_plan.as_ref() else {
                return f64::NAN;
            };
            let guard = built.tree.guard();
            match plan.span(&guard) {
                Ok(Some((lo, hi))) if hi > lo => (hi - lo) as f64 / 1e9,
                _ => f64::NAN,
            }
        };

        let mut prev_errors = errors(&built.tree);
        for index in 0..intervals {
            std::thread::sleep(args.interval);

            let l = lookups.swap(0, Ordering::Relaxed);
            let d = declined.swap(0, Ordering::Relaxed);
            let pu = pushes.swap(0, Ordering::Relaxed);
            let hist = latency
                .lock()
                .map(|mut h| std::mem::replace(&mut *h, Histogram::new()))
                .unwrap_or_else(|_| Histogram::new());
            let vis = visible
                .lock()
                .map(|mut h| std::mem::replace(&mut *h, Histogram::new()))
                .unwrap_or_else(|_| Histogram::new());

            let s = Snapshot {
                index,
                elapsed_s: origin.elapsed().as_secs_f64(),
                lookups: l,
                declined: d,
                p50_ns: hist.quantile(0.50),
                p999_ns: hist.quantile(0.999),
                pushes: pu,
                visible_p50_ns: vis.quantile(0.50),
                visible_p999_ns: vis.quantile(0.999),
                // Laps of the probe edge's ring during this interval.
                wraps: args.interval.as_secs_f64() / retained_secs(),
                rss_kib: ProcStats::read().pss_kib,
                err_delta: {
                    let now = errors(&built.tree);
                    let d = now.saturating_sub(prev_errors);
                    prev_errors = now;
                    d
                },
            };
            print_snapshot(&s, args.interval);
            snapshots.push(s);
        }

        stop.store(true, Ordering::Relaxed);
    });

    snapshots
}

fn print_snapshot(s: &Snapshot, interval: Duration) {
    let flag = if s.declined > s.lookups / 100 {
        " <-- DECLINED"
    } else {
        ""
    };
    let ppm = if s.lookups == 0 {
        0.0
    } else {
        s.declined as f64 / s.lookups as f64 * 1e6
    };
    println!(
        "{:>4} {:>7.0}s | {:>10.3} {:>9} {:>10} | {:>10} {:>10} | {:>8.1} {:>9} {:>9.0}{flag}",
        s.index,
        s.elapsed_s,
        s.lookups as f64 / interval.as_secs_f64() / 1e6,
        s.p50_ns,
        s.p999_ns,
        s.visible_p50_ns,
        s.visible_p999_ns,
        s.wraps,
        s.rss_kib,
        ppm,
    );
}

/// Sum of every error counter over every edge — the arena's own account of what
/// went wrong, which is the thing that should stay flat.
fn errors(tree: &tf_tree::Tree) -> u64 {
    let view = tree.arena_view();
    let mut total = 0u64;
    for edge in 0..view.header().max_edges {
        if let Some(c) = view.edge_counters(tf_tree::EdgeId(edge)) {
            total += c.err_extrap_before.load(Ordering::Relaxed)
                + c.err_extrap_after.load(Ordering::Relaxed)
                + c.err_no_data.load(Ordering::Relaxed)
                + c.err_slot_recycled.load(Ordering::Relaxed)
                + c.err_slot_contended.load(Ordering::Relaxed);
        }
    }
    total
}

/// Decide, and say why. Returns whether the soak passed.
fn verdict(s: &[Snapshot]) -> bool {
    let (Some(first), Some(last)) = (s.first(), s.last()) else {
        println!("no intervals completed; nothing to judge");
        return false;
    };
    if s.len() < 2 {
        println!(
            "only one interval completed, so nothing can be said about drift — \
             run with --duration at least twice --interval"
        );
        return false;
    }

    let mut ok = true;

    let ratio = if first.p999_ns == 0 {
        f64::NAN
    } else {
        last.p999_ns as f64 / first.p999_ns as f64
    };
    if ratio.is_finite() && ratio > DRIFT_FACTOR {
        println!(
            "FAIL latency drift: p99.9 went {} ns -> {} ns ({ratio:.2}x, limit {DRIFT_FACTOR:.1}x)",
            first.p999_ns, last.p999_ns
        );
        ok = false;
    } else {
        println!(
            "pass  latency: p99.9 {} ns -> {} ns ({ratio:.2}x, limit {DRIFT_FACTOR:.1}x)",
            first.p999_ns, last.p999_ns
        );
    }

    let growth = last.rss_kib.saturating_sub(first.rss_kib);
    if growth > RSS_GROWTH_KIB {
        println!(
            "FAIL memory growth: RSS {} KiB -> {} KiB (+{growth} KiB, limit {RSS_GROWTH_KIB})",
            first.rss_kib, last.rss_kib
        );
        ok = false;
    } else {
        println!(
            "pass  memory: RSS {} KiB -> {} KiB (+{growth} KiB, limit {RSS_GROWTH_KIB})",
            first.rss_kib, last.rss_kib
        );
    }

    let total_wraps: f64 = s.iter().map(|x| x.wraps).sum();
    if total_wraps < 1.0 {
        // A failure of the experiment, not the engine: a soak that never
        // lapped a ring is not a pass (`docs/PHASE2.md` §11.4).
        println!(
            "FAIL coverage: the rings lapped {total_wraps:.2} times, so wraparound was \
             never exercised — run longer, or pick a workload with shorter history"
        );
        ok = false;
    } else {
        println!("pass  coverage: the rings lapped {total_wraps:.1} times");
    }

    ok
}

/// Slack on a per-interval metric in the A/B differ; a single interval's tail
/// is noisy and the verdict judges the trend.
const SOAK_SLACK: f64 = 0.50;

fn json_row(workload: &str, built: &Built, s: &Snapshot) -> RunRow {
    RunRow::new("soak", workload, "tf_tree", format!("interval={}", s.index))
        .with_shape(built.shape)
        .metric(Metric::new("p50_ns", s.p50_ns as f64, "ns").lower_is_better(SOAK_SLACK))
        .metric(Metric::new("p99_9_ns", s.p999_ns as f64, "ns").lower_is_better(SOAK_SLACK))
        .metric(
            Metric::new("visible_p50_ns", s.visible_p50_ns as f64, "ns")
                .lower_is_better(SOAK_SLACK),
        )
        .metric(
            Metric::new("visible_p99_9_ns", s.visible_p999_ns as f64, "ns")
                .lower_is_better(SOAK_SLACK),
        )
        .metric(Metric::new("rss_kib", s.rss_kib as f64, "KiB").lower_is_better(SOAK_SLACK))
        .metric(Metric::new("lookups", s.lookups as f64, "lookups"))
        .metric(Metric::new("declined", s.declined as f64, "lookups"))
        .metric(Metric::new("pushes", s.pushes as f64, "pushes"))
        .metric(Metric::new("ring_wraps", s.wraps, "laps"))
        .metric(Metric::new("err_delta", s.err_delta as f64, "errors"))
}

/// The row the A/B differ should actually be read on: the drift itself, rather
/// than any single interval's value.
fn summary_row(workload: &str, built: &Built, s: &[Snapshot]) -> RunRow {
    let (first, last) = (s.first(), s.last());
    let ratio = match (first, last) {
        (Some(f), Some(l)) if f.p999_ns > 0 => l.p999_ns as f64 / f.p999_ns as f64,
        _ => f64::NAN,
    };
    let growth = match (first, last) {
        (Some(f), Some(l)) => l.rss_kib.saturating_sub(f.rss_kib) as f64,
        _ => f64::NAN,
    };
    RunRow::new("soak", workload, "tf_tree", "drift")
        .with_shape(built.shape)
        .metric(Metric::new("p99_9_drift", ratio, "x").lower_is_better(0.20))
        .metric(Metric::new("rss_growth_kib", growth, "KiB").lower_is_better(0.50))
        .metric(Metric::new("intervals", s.len() as f64, "intervals"))
        .metric(Metric::new(
            "total_ring_wraps",
            s.iter().map(|x| x.wraps).sum(),
            "laps",
        ))
}
