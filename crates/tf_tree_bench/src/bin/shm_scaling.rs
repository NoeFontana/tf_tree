//! Multi-process read scaling on one shared arena, and what it costs in memory.
//!
//! `tf2_scaling` measures N reader *threads* in one process. This measures N
//! reader **processes** — which is how robot software is actually deployed:
//! perception, planning and control are separate executables, not threads.
//!
//! # There is no tf2 column here, and that is the result
//!
//! `tf2::BufferCore` has no shared-memory mode. Every process that needs
//! transforms runs its own `tf2_ros::TransformListener`, which subscribes to
//! `/tf`, deserializes every message, and maintains a **private, complete copy**
//! of the history. So for N consumers, tf2 costs N copies of the buffer, N
//! deserialization pipelines, and N× the DDS fan-out — and the copies drift
//! apart in time because each one is updated by its own callback thread.
//!
//! That is not a benchmark tf2 loses; it is a benchmark tf2 cannot enter. The
//! honest way to report it is to measure tf_tree's actual numbers and state
//! tf2's structural cost as arithmetic, which `docs/benchmarks/tf2.md` does.
//! Timing a `tf2_ros` listener here would additionally drag DDS into the
//! measurement, which every other row in that document is careful to exclude.
//!
//! # What is measured
//!
//! * **Aggregate throughput** across N reader processes on one arena.
//! * **Per-process RSS**, and the system-wide total, because `MAP_SHARED` means
//!   the arena is resident *once* however many processes map it — the whole
//!   point, and invisible from any single process's `/proc/self/statm`.
//!
//! Run: `just shm-scaling` (needs `--features shm`, Linux).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::io::{BufRead, BufReader};
use std::time::Instant;

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Tree, TreeBuilder};
use tf_tree_bench::fixture;
use tf_tree_bench::shm_util::{sibling_binary, spawn_attached};

/// Lookups each reader process performs per round.
const LOOKUPS: usize = 2_000_000;

/// Process counts to sweep.
///
/// Deliberately run past the physical core count: the oversubscribed row is
/// informative (it shows latency degrading exactly in proportion while aggregate
/// throughput stays at the roofline). `main` prints the core count so a row
/// above it can be read as SMT-assisted rather than as clean scaling.
const PROCESSES: &[usize] = &[1, 2, 4, 8];

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
    let tree = b.build_shared("tf_tree.bench").expect("build shared arena");
    let (writers, samples) = fixture::spin_up(&tree).expect("spin up");
    drop(writers);
    drop(samples);
    tree
}

/// Physical cores, so an SMT-assisted row can be labelled as such rather than
/// quoted as clean scaling.
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

/// Resident bytes of the arena, shared across every mapper.
fn arena_kib(tree: &Tree) -> u64 {
    tree.arena_size_bytes() as u64 / 1024
}

fn main() {
    let tree = build_shared();
    let child_bin =
        sibling_binary("shm_child").expect("shm_child binary — build with --features shm");
    let cores = physical_cores();

    println!("tf_tree multi-process read scaling");
    println!("==================================");
    println!(
        "fixture: {} frames, arena {} KiB, {LOOKUPS} lookups/process/round",
        fixture::frame_names().len(),
        arena_kib(&tree)
    );
    println!("physical cores: {cores} (rows above this are not SMT-assisted)\n");

    println!(
        "{:>10} {:>14} {:>12} {:>11} {:>12} {:>12} {:>12}",
        "processes",
        "aggregate M/s",
        "ns/lookup",
        "vs 1 proc",
        "sum RSS",
        "unique res",
        "tf2 would be"
    );
    println!(
        "{:>10} {:>14} {:>12} {:>11} {:>12} {:>12} {:>12}\n",
        "", "", "", "", "MiB", "MiB", "MiB (hist)"
    );

    let mut baseline = 0.0f64;
    for &n in PROCESSES {
        let fd = tree.shared_fd().expect("shared fd");
        let args: Vec<String> = ["bench", "imu_link", "map", &LOOKUPS.to_string()]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();

        let t0 = Instant::now();
        let mut children: Vec<_> = (0..n)
            .map(|_| spawn_attached(&child_bin, fd, &args).expect("spawn reader"))
            .collect();

        let mut rss_total = 0u64;
        let mut worst_ns = 0.0f64;
        for c in &mut children {
            let out = BufReader::new(c.stdout.take().expect("child stdout"));
            for line in out.lines() {
                let line = line.expect("child line");
                let mut it = line.split_whitespace();
                match it.next() {
                    Some("ns_per_lookup") => {
                        let v: f64 = it.next().unwrap().parse().unwrap();
                        worst_ns = worst_ns.max(v);
                    }
                    Some("rss_kib") => rss_total += it.next().unwrap().parse::<u64>().unwrap(),
                    _ => {}
                }
            }
        }
        for c in &mut children {
            assert!(c.wait().expect("wait").success(), "a reader process failed");
        }
        let wall = t0.elapsed().as_secs_f64();

        // Aggregate throughput is total work over wall clock, which includes
        // process startup. Reported anyway rather than subtracted: a fleet of
        // readers really does pay it, and hiding it would flatter the design.
        let total = (n * LOOKUPS) as f64;
        let mps = total / wall / 1e6;
        if n == 1 {
            baseline = mps;
        }
        // `sum RSS` double-counts the arena: every process's /proc/self/statm
        // includes the pages it maps, but MAP_SHARED means one physical copy.
        // Subtracting the (n-1) redundant copies gives what the machine actually
        // holds — and reporting only the sum would have understated the very
        // property this benchmark exists to demonstrate.
        let sum_mib = rss_total as f64 / 1024.0;
        let arena_mib = arena_kib(&tree) as f64 / 1024.0;
        let unique_mib = sum_mib - (n - 1) as f64 * arena_mib;
        // tf2's transform history for n consumers: n private BufferCore copies,
        // at the 1,421,392 B measured by `just footprint` on this same fixture.
        let tf2_mib = n as f64 * 1_421_392.0 / (1024.0 * 1024.0);
        println!(
            "{n:>10} {mps:>14.2} {worst_ns:>12.1} {:>10.2}x {sum_mib:>12.1} {unique_mib:>12.1} {tf2_mib:>12.1}",
            mps / baseline,
        );
    }

    println!(
        "\nThe arena is MAP_SHARED, so its {} KiB is resident ONCE however many",
        arena_kib(&tree)
    );
    println!("processes map it. `sum RSS` double-counts it; `unique res` does not.");
    println!();
    println!("The `tf2 would be` column is arithmetic, not a measurement, and is labelled");
    println!("as such: tf2::BufferCore has no shared-memory mode, so each consumer process");
    println!("runs its own TransformListener holding a private, complete copy of the");
    println!("history. n consumers cost n x the 1,421,392 B that `just footprint` measured");
    println!("for one. It is not a benchmark tf2 loses; it is one tf2 cannot enter.");
}
