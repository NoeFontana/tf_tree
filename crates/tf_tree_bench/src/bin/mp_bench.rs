//! Multi-process evaluation: N consumer nodes, one publisher, both engines.
//!
//! The question this answers is the deployment question — *sixteen nodes each
//! need transforms at their own rate; what does each experience, and what does
//! it cost the machine?* — rather than `shm_scaling`'s roofline question, *how
//! many lookups can N processes extract in total?*
//!
//! What is different, and why, is documented in `src/mp.rs`. In one line each:
//! an open-loop schedule so a slow tick shows up as latency instead of as fewer
//! samples; a live publisher so the seqlock and tf2's mutex are actually
//! exercised; per-consumer tail latency instead of one aggregate mean; CPU per
//! consumer, because "O(1) in the number of consumers" was a claim nobody had
//! measured; and PSS instead of summed RSS.
//!
//! **Run pinned and idle.** `taskset` the whole thing, and expect the rows above
//! the physical core count to be scheduler noise rather than engine behaviour.
//!
//! Usage: `just mp-bench` (tf_tree) or `just mp-bench-tf2` (both, in the
//! container).
// This binary's output IS its result, and its refusal message must reach a
// terminal even when stdout is piped into a report.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::io::{BufRead, BufReader};
use std::time::Duration;

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Tree, TreeBuilder};
use tf_tree_bench::fixture;
use tf_tree_bench::mp::{busy_fraction, require_quiet_machine, Histogram};
use tf_tree_bench::shm_util::{sibling_binary, spawn_attached};

/// Consumer counts to sweep.
const CONSUMERS: &[usize] = &[1, 2, 4, 8, 16];
/// Per-consumer tick rate. 100 Hz is a plausible perception/planning node.
const HZ: f64 = 100.0;
/// Measurement window per point.
const SECONDS: f64 = 6.0;

fn build_shared() -> Tree {
    let mut b = TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
    for e in fixture::EDGES {
        b = match e.kind {
            fixture::EdgeDefKind::Static { xi } => {
                b.static_edge(e.parent, e.child, &tf_tree_math::exp_se3(xi))
            }
            fixture::EdgeDefKind::Dynamic { rate_hz } => b.dynamic_edge(
                e.parent,
                e.child,
                EdgeCfg::new(Capacity::history(rate_hz, fixture::HISTORY_SECS)),
            ),
        };
    }
    let tree = b.build_shared("tf_tree.mp").expect("build shared arena");
    let (writers, samples) = fixture::spin_up(&tree).expect("spin up");
    drop(writers);
    drop(samples);
    tree
}

fn physical_cores() -> usize {
    let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut ids = std::collections::HashSet::new();
    let (mut phys, mut core) = (None, None);
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("physical id") {
            phys = v
                .split(':')
                .nth(1)
                .and_then(|x| x.trim().parse::<u32>().ok());
        } else if let Some(v) = line.strip_prefix("core id") {
            core = v
                .split(':')
                .nth(1)
                .and_then(|x| x.trim().parse::<u32>().ok());
        }
        if let (Some(p), Some(c)) = (phys, core) {
            ids.insert((p, c));
            phys = None;
            core = None;
        }
    }
    if ids.is_empty() {
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    } else {
        ids.len()
    }
}

/// One consumer's reported measurement.
struct Report {
    /// Intended tick time to completion — what the node experiences.
    cycle: Histogram,
    /// Work start to completion — what the engine costs.
    service: Histogram,
    cpu_ns: u64,
    pss_kib: u64,
}

fn collect(child: &mut std::process::Child) -> Report {
    let out = BufReader::new(child.stdout.take().expect("child stdout"));
    let (mut cycle, mut service) = (Histogram::new(), Histogram::new());
    let (mut cpu_ns, mut pss_kib) = (0u64, 0u64);
    for line in out.lines() {
        let line = line.expect("child line");
        if let Some(rest) = line.strip_prefix("cpu_ns ") {
            cpu_ns = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("pss_kib ") {
            pss_kib = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("cycle ") {
            cycle = Histogram::decode(rest).expect("cycle histogram");
        } else if let Some(rest) = line.strip_prefix("service ") {
            service = Histogram::decode(rest).expect("service histogram");
        }
    }
    Report {
        cycle,
        service,
        cpu_ns,
        pss_kib,
    }
}

fn main() {
    let engine = std::env::args().nth(1).unwrap_or_else(|| "tf_tree".into());

    // Refuse before doing any work. Latency here is largely a measurement of
    // the scheduler, so a run taken against somebody else's workload describes
    // that workload. The first run of this harness was taken on a machine
    // carrying an unrelated 600%-CPU job and nothing in the output said so.
    let baseline_busy = match require_quiet_machine() {
        Ok(b) => b,
        Err(msg) => {
            eprintln!("mp_bench: {msg}");
            std::process::exit(1);
        }
    };

    let tree = build_shared();
    let child_bin = sibling_binary("mp_consumer").expect("mp_consumer — build with --features shm");
    let cores = physical_cores();

    println!("tf_tree multi-process node evaluation  [engine: {engine}]");
    println!("========================================================");
    println!(
        "{} consumer nodes x {HZ:.0} Hz x {SECONDS:.0} s, 8 lookups/tick, depth-3 chain",
        CONSUMERS
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("/")
    );
    println!("physical cores: {cores}");
    println!();
    println!("Latency is measured from each tick's INTENDED time, so a consumer that");
    println!("falls behind reports the backlog instead of silently taking fewer samples.");
    println!("A publisher runs throughout at the fixture's rates.");
    println!();
    println!("  service = engine cost (work start -> done).  cycle = what the node");
    println!("  experiences (intended tick -> done), which at {HZ:.0} Hz is mostly OS wakeup.");
    println!();
    println!(
        "{:>6} | {:>9} {:>9} {:>10} | {:>9} {:>9} {:>10} | {:>10} {:>9}",
        "nodes",
        "svc p50",
        "svc p99",
        "svc p99.9",
        "cyc p50",
        "cyc p99",
        "cyc p99.9",
        "CPU %/node",
        "PSS MiB"
    );
    println!(
        "{:>6} | {:>31} | {:>31} |    (busy% is the machine during that row)",
        "", "microseconds", "microseconds"
    );

    for &n in CONSUMERS {
        // A publisher runs for the whole window. It must write into the *same*
        // arena the consumers read — an earlier draft built a second one, which
        // would have measured readers against a tree nobody was touching, i.e.
        // exactly the quiescent-tree flaw this harness exists to remove.
        //
        // `thread::scope` is what lets the writer borrow the coordinator's tree
        // while `shared_fd()` keeps being used to spawn consumers.
        let stop = std::sync::atomic::AtomicBool::new(false);
        let (reports, _) = std::thread::scope(|scope| {
            let stop_ref = &stop;
            let tree_ref = &tree;

            let writer = scope.spawn(move || {
                let (parent, child, rate_hz) = fixture::DYNAMIC_EDGES[2]; // the 1 kHz edge
                let (Ok(p), Ok(c)) = (tree_ref.frame(parent), tree_ref.frame(child)) else {
                    return 0u64;
                };
                let Ok(w) = tree_ref.claim(c, p) else {
                    return 0;
                };
                let period = Duration::from_secs_f64(1.0 / rate_hz);
                let step = (1e9 / rate_hz) as i64;
                let mut k = (fixture::HISTORY_SECS * rate_hz) as i64;
                let mut pushed = 0u64;
                while !stop_ref.load(std::sync::atomic::Ordering::Relaxed) {
                    let stamp = k * step;
                    if w.push(stamp, &fixture::dynamic_pose(2.0, stamp)).is_ok() {
                        pushed += 1;
                    }
                    k += 1;
                    std::thread::sleep(period);
                }
                pushed
            });

            let fd = tree_ref.shared_fd().expect("shared fd");
            let args: Vec<String> = vec![engine.clone(), HZ.to_string(), SECONDS.to_string()];
            let mut children: Vec<_> = (0..n)
                .map(|_| spawn_attached(&child_bin, fd, &args).expect("spawn consumer"))
                .collect();

            let reports: Vec<Report> = children.iter_mut().map(collect).collect();
            for c in &mut children {
                assert!(c.wait().expect("wait").success(), "a consumer failed");
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let pushed = writer.join().expect("writer thread");
            (reports, pushed)
        });

        // Worst tail across the fleet — the number an integrator lives with is
        // the unluckiest node's, not the average node's.
        let (mut svc, mut cyc) = (Histogram::new(), Histogram::new());
        let (mut worst_svc_p999, mut worst_cyc_p999) = (0u64, 0u64);
        let (mut cpu_total, mut pss_total) = (0u64, 0u64);
        for r in &reports {
            svc.merge(&r.service);
            cyc.merge(&r.cycle);
            worst_svc_p999 = worst_svc_p999.max(r.service.quantile(0.999));
            worst_cyc_p999 = worst_cyc_p999.max(r.cycle.quantile(0.999));
            cpu_total += r.cpu_ns;
            pss_total += r.pss_kib;
        }
        let cpu_pct_per_node = (cpu_total as f64 / n as f64) / (SECONDS * 1e9) * 100.0;
        let us = |v: u64| v as f64 / 1000.0;

        // Per-row noise, sampled right after the row. A row taken while the
        // machine was busy is a different experiment, and it must be visible in
        // the output rather than inferable only from the operator's memory.
        let row_busy = busy_fraction(Duration::from_millis(200));
        // The harness's own load is a fraction of one core, so subtract the
        // consumers' measured CPU to leave what somebody *else* was doing.
        let ours = cpu_total as f64 / (SECONDS * 1e9) / cores as f64;
        let foreign = (row_busy - ours).max(0.0);
        let flag = if foreign > 0.10 { " <-- NOISY" } else { "" };

        println!(
            // CPU %/node gets three decimals, not one: the column exists to
            // show whether per-node cost *rises* with n, and at ~0.1% a single
            // decimal renders a doubling and a flat line identically.
            "{n:>6} | {:>9.2} {:>9.2} {:>10.2} | {:>9.1} {:>9.1} {:>10.1} | {:>10.3} {:>9.2}  {:>5.0}%{flag}",
            us(svc.quantile(0.50)),
            us(svc.quantile(0.99)),
            us(worst_svc_p999),
            us(cyc.quantile(0.50)),
            us(cyc.quantile(0.99)),
            us(worst_cyc_p999),
            cpu_pct_per_node,
            pss_total as f64 / 1024.0,
            foreign * 100.0,
        );
    }

    println!();
    println!(
        "Machine was {:.0}% busy before the run; any row flagged NOISY had foreign load",
        baseline_busy * 100.0
    );
    println!("above 10% of the machine and should not be published.");
    println!();
    println!("CPU %/node is per-consumer, so a flat column is the O(1)-in-consumers claim");
    println!("holding; a rising one is it failing. PSS is summed across consumers and counts");
    println!("each shared page once — summed RSS would count the arena n times over.");
    if engine == "tf2" {
        println!();
        println!("NOTE: this tf2 column is a FLOOR. Each consumer holds a private BufferCore");
        println!("built from the identical stream, so it shows the memory and CPU duplication");
        println!("that having no shared arena forces — but no transport. A deployed tf2");
        println!("consumer reaches the tree only through a TransformListener over DDS and");
        println!("additionally pays deserialization and fan-out. See docs/benchmarks/tf2.md.");
    }
}
