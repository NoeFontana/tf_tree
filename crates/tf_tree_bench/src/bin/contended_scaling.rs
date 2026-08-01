//! Read scaling **with concurrent writers and per-core pinning** — the
//! `docs/PHASE1.md` §11.2 row that was measured nowhere.
//!
//! §11.2 specifies: *"read scaling: 1/2/4/8/16 reader threads, 4 concurrent
//! writers, cores pinned — aggregate throughput, per-thread p99.9"*. Every
//! reader benchmark in this repository before this one runs against a
//! **quiescent** tree: `benches/read_scaling.rs` says so in its header, and
//! `docs/benchmarks/tf2.md`'s "What is still not measured" lists both the
//! writers and the pinning. That matters because the quiescent case exercises
//! neither of the two mechanisms the comparison is actually about — tf_tree's
//! seqlock retry path, and the exclusion `tf2::BufferCore`'s single mutex
//! imposes between a writer and every reader.
//!
//! # What this measures, and what its companion measures
//!
//! This binary is **tf_tree, across processes, pinned**: N reader processes and
//! M writer processes on one shared arena, each placed on its own core. That is
//! the deployment shape, and it is the shape tf2 cannot enter at all — a second
//! process reaches a `BufferCore` only over DDS.
//!
//! The head-to-head *under writers* lives in `src/bin/tf2_scaling.rs`, which
//! already has an interleaved two-engine thread harness; `TF2_WRITERS` there
//! drives it. Splitting them this way is deliberate: putting an in-process tf2
//! arm in here would mean one binary running two incomparable topologies of
//! processes and threads, and the resulting table would invite exactly the
//! apples-to-oranges reading it should prevent.
//!
//! # Pinning without `unsafe`
//!
//! Per-thread placement needs `sched_setaffinity`; `tf_tree_bench`'s library is
//! `#![forbid(unsafe_code)]` and `CLAUDE.md`'s unsafe budget routes a new kind of
//! `unsafe` to a decision record. A *process* is placed exactly by `taskset -c
//! N`, which costs no `unsafe`, and it is the closer model of a robot anyway.
//! When `taskset` is absent the run continues unpinned and **says so in the
//! output and in the emitted JSON**, because an unpinned scaling row is a
//! different experiment.
//!
//! Usage: `just contended-scaling`, or
//! `contended_scaling --workload fleet_16 --json out.json`.
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

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Child;

use tf_tree::{InterpPolicy, Tree};
use tf_tree_bench::mp::{busy_fraction, require_quiet_machine, Histogram};
use tf_tree_bench::report::Metric;
use tf_tree_bench::runstore::{Run, RunRow};
use tf_tree_bench::shm_util::{sibling_binary, spawn_attached};
use tf_tree_bench::workload::{self, Backing, Built};

/// Reader counts to sweep, matching §11.2's list.
const READERS: &[usize] = &[1, 2, 4, 8, 16];
/// Writer counts to sweep. `0` is the continuity row — it must reproduce the
/// existing quiescent-tree numbers, or this harness is measuring something else.
/// `4` is §11.2's figure.
const WRITERS: &[usize] = &[0, 1, 2, 4];
/// Per-reader tick rate in the latency pass. 1 kHz is a demanding control loop
/// and still leaves the open-loop schedule achievable on an idle core.
const READER_HZ: f64 = 1000.0;
/// Measurement window per point, split in half between the two reader passes.
const SECONDS: f64 = 4.0;
/// Slack added to the writers' window so they outlive the readers they contend
/// with. A writer that exits first turns the tail of every reader row into a
/// quiescent-tree measurement, silently.
const WRITER_SLACK_S: f64 = 1.0;
/// Query pairs handed to each reader. Capped because they travel on argv, and
/// because a node resolving more than a handful of chains per cycle is not the
/// shape being modelled.
const MAX_PAIRS: usize = 8;

/// The rendezvous name for this harness's arena.
const ARENA: &str = "tf_tree.contended";

struct Args {
    workload: &'static str,
    json: Option<PathBuf>,
    seconds: f64,
    readers: Vec<usize>,
    writers: Vec<usize>,
}

fn parse_args() -> Args {
    let mut a = Args {
        workload: "robot",
        json: None,
        seconds: SECONDS,
        readers: READERS.to_vec(),
        writers: WRITERS.to_vec(),
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
                let name = need(i);
                a.workload = workload::by_name(&name)
                    .unwrap_or_else(|e| {
                        eprintln!("contended_scaling: {e}");
                        std::process::exit(2)
                    })
                    .name;
                i += 2;
            }
            "--json" => {
                a.json = Some(PathBuf::from(need(i)));
                i += 2;
            }
            "--seconds" => {
                a.seconds = need(i).parse().expect("--seconds");
                i += 2;
            }
            "--readers" => {
                a.readers = parse_list(&need(i));
                i += 2;
            }
            "--writers" => {
                a.writers = parse_list(&need(i));
                i += 2;
            }
            "--list" => {
                for w in workload::CATALOGUE {
                    println!("{:<14} {}", w.name, w.note);
                }
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!(
                    "usage: contended_scaling [--workload NAME] [--json PATH] \
                     [--seconds N] [--readers 1,2,4] [--writers 0,4] [--list]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("contended_scaling: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    a
}

fn parse_list(s: &str) -> Vec<usize> {
    let v: Vec<usize> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    if v.is_empty() {
        eprintln!("contended_scaling: {s:?} parsed to no counts");
        std::process::exit(2);
    }
    v
}

/// Whether `taskset` exists on this host.
///
/// Checked once, up front. A run that discovered this per spawn would produce a
/// table whose early rows are pinned and whose later ones are not.
fn taskset_available() -> bool {
    std::process::Command::new("taskset")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Spawn `bin args...` attached to `tree`'s segment, pinned to `core` when
/// pinning is available.
fn spawn_on_core(
    tree: &Tree,
    bin: &Path,
    core: usize,
    args: &[String],
    pin: bool,
) -> std::io::Result<Child> {
    let fd = tree.shared_fd().expect("shared arena fd");
    let (program, full): (PathBuf, Vec<String>) = if pin {
        // `taskset` `exec`s the target, so the segment installed on fd 0 by
        // `spawn_attached` survives into the process that actually reads it.
        let mut v = vec!["-c".to_owned(), core.to_string(), bin.display().to_string()];
        v.extend_from_slice(args);
        (PathBuf::from("taskset"), v)
    } else {
        (bin.to_path_buf(), args.to_vec())
    };
    spawn_attached(&program, fd, &full).map_err(std::io::Error::other)
}

/// One reader's report.
struct ReaderReport {
    ops: u64,
    elapsed_ns: u64,
    /// Per-lookup engine cost. **This is the column an engine comparison
    /// belongs in** — see `load_child`'s `service_pass`.
    service: Histogram,
    /// Intended-tick-to-done. Mostly scheduler wakeup, and reported because
    /// that is what a node actually experiences.
    cycle: Histogram,
    clock_overhead_ns: u64,
    declined: u64,
    attempted: u64,
    cpu_ns: u64,
    pss_kib: u64,
}

fn collect_reader(child: &mut Child) -> ReaderReport {
    let mut r = ReaderReport {
        ops: 0,
        elapsed_ns: 0,
        service: Histogram::new(),
        cycle: Histogram::new(),
        clock_overhead_ns: 0,
        declined: 0,
        attempted: 0,
        cpu_ns: 0,
        pss_kib: 0,
    };
    let out = BufReader::new(child.stdout.take().expect("reader stdout"));
    for line in out.lines() {
        let line = line.expect("reader line");
        if let Some(rest) = line.strip_prefix("throughput ") {
            let mut it = rest.split_whitespace();
            r.ops = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            r.elapsed_ns = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("service ") {
            r.service = Histogram::decode(rest).expect("service histogram");
        } else if let Some(rest) = line.strip_prefix("cycle ") {
            r.cycle = Histogram::decode(rest).expect("cycle histogram");
        } else if let Some(rest) = line.strip_prefix("attempted ") {
            r.attempted = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("clock_overhead_ns ") {
            r.clock_overhead_ns = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("declined ") {
            r.declined = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("cpu_ns ") {
            r.cpu_ns = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("pss_kib ") {
            r.pss_kib = rest.trim().parse().unwrap_or(0);
        }
    }
    r
}

/// One writer's report.
struct WriterReport {
    pushed: u64,
    rejected: u64,
}

fn collect_writer(child: &mut Child) -> WriterReport {
    let mut w = WriterReport {
        pushed: 0,
        rejected: 0,
    };
    let out = BufReader::new(child.stdout.take().expect("writer stdout"));
    for line in out.lines() {
        let line = line.expect("writer line");
        if let Some(rest) = line.strip_prefix("pushed ") {
            w.pushed = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("rejected ") {
            w.rejected = rest.trim().parse().unwrap_or(0);
        }
    }
    w
}

/// The two counters that are evidence of read/write contention.
///
/// **Neither counts a *successful* retry**, and that has to be said wherever
/// they are reported: `tf_tree_core::counters::EdgeCounters` records
/// `err_slot_recycled` (the ring lapped a reader mid-read) and
/// `err_slot_contended` (a slot stayed mid-write past the retry limit), both of
/// which are *failures*. A zero here means no read failed — not that no read
/// retried. The retries that succeed are invisible to the arena by design, and
/// what they cost shows up as latency, which is the column next door.
#[derive(Clone, Copy, Default)]
struct Contention {
    recycled: u64,
    contended: u64,
}

fn read_contention(tree: &Tree) -> Contention {
    let view = tree.arena_view();
    let mut c = Contention::default();
    for edge in 0..view.header().max_edges {
        if let Some(k) = view.edge_counters(tf_tree::EdgeId(edge)) {
            c.recycled += k
                .err_slot_recycled
                .load(std::sync::atomic::Ordering::Relaxed);
            c.contended += k
                .err_slot_contended
                .load(std::sync::atomic::Ordering::Relaxed);
        }
    }
    c
}

fn main() {
    let args = parse_args();

    // Refuse before doing any work, for `mp_bench`'s reason: latency here is
    // largely a measurement of the scheduler, so a run taken against somebody
    // else's workload describes that workload.
    let baseline_busy = match require_quiet_machine() {
        Ok(b) => b,
        Err(msg) => {
            eprintln!("contended_scaling: {msg}");
            std::process::exit(1);
        }
    };

    let w = workload::by_name(args.workload).expect("workload");
    let built: Built = w
        .build(InterpPolicy::LerpSlerp, Backing::Shared(ARENA))
        .unwrap_or_else(|e| {
            eprintln!("contended_scaling: {e:#}");
            std::process::exit(1);
        });

    let child_bin = sibling_binary("load_child").expect("load_child — build with --features shm");
    let pin = taskset_available();
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let max_writers = built.publishers.len();

    let pairs: Vec<String> = built
        .pairs
        .iter()
        .take(MAX_PAIRS)
        .map(|(t, s)| format!("{t}|{s}"))
        .collect();

    // Compiled once and reused to probe the retained window before each point.
    // These are the coordinator's own plans; the children compile their own.
    let plans = built.plans().expect("compiling the workload's query pairs");
    let guard_tree = &built.tree;
    let live_window = |plans: &[tf_tree::Plan]| -> Option<(i64, i64)> {
        let guard = guard_tree.guard();
        let mut out: Option<(i64, i64)> = None;
        for p in plans {
            let (lo, hi) = p.span(&guard).ok().flatten()?;
            out = Some(match out {
                None => (lo, hi),
                Some((a, b)) => (a.max(lo), b.min(hi)),
            });
        }
        out
    };

    // Where each publisher resumes. Mutated as the sweep advances; see the
    // writer spawn below for why a fixed value cannot work.
    let mut next_stamps: Vec<i64> = built.publishers.iter().map(|p| p.next_stamp_ns).collect();

    let mut run = Run::begin(*args.readers.iter().max().unwrap_or(&1));

    println!("tf_tree contended read scaling  [workload: {}]", w.name);
    println!("=========================================================");
    println!("{}", w.note);
    println!("{}", built.shape.describe());
    println!(
        "{} logical CPUs; pinning: {}",
        cpus,
        if pin {
            "taskset, one process per core"
        } else {
            "NONE (taskset not found) — these rows are not the pinned experiment"
        }
    );
    println!(
        "{} pairs queried, {:.1} s per point split three ways: throughput, \
         per-lookup service, and an open loop at {READER_HZ:.0} Hz",
        pairs.len(),
        args.seconds
    );
    println!();
    println!("`service` is the engine's own per-lookup distribution — a dense loop with one");
    println!("clock pair per lookup, and the clock's own cost measured and reported so it can");
    println!("be subtracted. `cycle p99.9` is the open-loop number a node experiences, measured");
    println!("from each tick's INTENDED time; on an idle machine it is mostly scheduler wakeup,");
    println!("which is why it sits beside the service columns rather than replacing them.");
    println!();
    println!(
        "{:>7} {:>7} | {:>12} {:>8} | {:>9} {:>9} {:>10} | {:>10} | {:>9} {:>8} {:>6}",
        "readers",
        "writers",
        "Mlookup/s",
        "scale",
        "svc p50",
        "svc p99",
        "svc p99.9",
        "cyc p99.9",
        "pushed/s",
        "declined",
        "fail"
    );
    println!(
        "{:>7} {:>7} | {:>12} {:>8} | {:>31} | {:>10} |",
        "", "", "", "", "nanoseconds", "us"
    );

    let mut baseline_throughput: Option<f64> = None;

    for &n_writers in &args.writers {
        if n_writers > max_writers {
            println!(
                "  (skipping {n_writers} writers: {} has only {max_writers} dynamic edges)",
                w.name
            );
            continue;
        }
        for &n_readers in &args.readers {
            // **The readers' window must survive the writers, and it must be
            // read fresh at every point.** Two things move it. A writer
            // publishes forward for the whole point, and a fixed-capacity ring
            // evicts an equal span off the bottom; and that eviction is
            // *cumulative* across the sweep, so a window computed once from the
            // populated history is wrong by several multiples of the ring by the
            // last row. The first revision of this harness got both wrong and
            // reported 29% of its lookups declined — 29% of its samples timing
            // the error path.
            //
            // `Plan::span` is the arena's own answer to "what is retained right
            // now", intersected over every dynamic edge on the path. Insetting
            // the bottom by the span the writers are about to publish is what
            // keeps the whole point inside it.
            let Some((live_lo, live_hi)) = live_window(&plans) else {
                println!(
                    "  (skipping readers={n_readers}, writers={n_writers}: no stamp \
                     window is currently retained by every edge on the query path)"
                );
                continue;
            };
            let slide_ns = if n_writers == 0 {
                0
            } else {
                ((args.seconds + WRITER_SLACK_S) * 1e9) as i64
            };
            let read_lo = live_lo + slide_ns;
            if read_lo >= live_hi {
                println!(
                    "  (skipping readers={n_readers}, writers={n_writers}: {:.1} s of \
                     publishing would evict the whole {:.1} s currently retained)",
                    args.seconds + WRITER_SLACK_S,
                    (live_hi - live_lo) as f64 / 1e9,
                );
                continue;
            }

            let before = read_contention(&built.tree);

            let mut writers: Vec<Child> = (0..n_writers)
                .map(|i| {
                    let p = &built.publishers[i];
                    // **Where this writer resumes, not where the last one
                    // started.** Every point leaves the edge's newest stamp
                    // further along; restarting from the populated history's end
                    // makes every push out of order, and `push` rejects those —
                    // which is how the first revision of this harness ran four
                    // "writers" that between them published one sample and still
                    // reported a contended row.
                    let a = vec![
                        "writer".to_owned(),
                        p.rate_hz.to_string(),
                        (args.seconds + WRITER_SLACK_S).to_string(),
                        p.seed.to_string(),
                        next_stamps[i].to_string(),
                        p.parent.clone(),
                        p.child.clone(),
                    ];
                    // Writers take cores from the top, readers from the bottom,
                    // so the two groups only share a core once the machine is
                    // genuinely oversubscribed.
                    let core = (cpus - 1).saturating_sub(i % cpus);
                    spawn_on_core(&built.tree, &child_bin, core, &a, pin).expect("spawn writer")
                })
                .collect();

            let mut readers: Vec<Child> = (0..n_readers)
                .map(|i| {
                    let mut a = vec![
                        "reader".to_owned(),
                        READER_HZ.to_string(),
                        args.seconds.to_string(),
                        read_lo.to_string(),
                        live_hi.to_string(),
                    ];
                    a.extend(pairs.iter().cloned());
                    spawn_on_core(&built.tree, &child_bin, i % cpus, &a, pin).expect("spawn reader")
                })
                .collect();

            let reports: Vec<ReaderReport> = readers.iter_mut().map(collect_reader).collect();
            for c in &mut readers {
                assert!(c.wait().expect("wait").success(), "a reader failed");
            }
            let wreports: Vec<WriterReport> = writers.iter_mut().map(collect_writer).collect();
            for c in &mut writers {
                assert!(c.wait().expect("wait").success(), "a writer failed");
            }
            // Advance past what was just published, with two periods of margin.
            // Overshooting is safe — it leaves a gap in the stamp sequence, and
            // the readers' window is re-probed from the arena anyway — while
            // undershooting is not, because it is an out-of-order push.
            for (stamp, p) in next_stamps
                .iter_mut()
                .zip(&built.publishers)
                .take(n_writers)
            {
                let step_ns = (1e9 / p.rate_hz) as i64;
                *stamp += ((args.seconds + WRITER_SLACK_S) * 1e9) as i64 + 2 * step_ns;
            }

            let after = read_contention(&built.tree);
            let contention = Contention {
                recycled: after.recycled.saturating_sub(before.recycled),
                contended: after.contended.saturating_sub(before.contended),
            };

            let row = summarise(
                &reports,
                &wreports,
                n_readers,
                n_writers,
                contention,
                args.seconds,
            );

            if n_writers == 0 && n_readers == 1 {
                baseline_throughput = Some(row.lookups_per_s);
            }
            let scale = baseline_throughput.map_or(f64::NAN, |b| row.lookups_per_s / b);

            let busy = busy_fraction(std::time::Duration::from_millis(150));
            let ours = row.cpu_ns as f64 / (args.seconds * 1e9) / cpus as f64;
            let flag = if (busy - ours).max(0.0) > 0.10 {
                " <-- NOISY"
            } else {
                ""
            };

            // A row whose lookups were largely declined is timing the error
            // path, not the engine, and it must not read as a result. The
            // threshold is 1%: below that a handful of edge-of-window declines
            // is ordinary, above it the sweep and the retained window disagree.
            let decline_pct = if row.attempted == 0 {
                0.0
            } else {
                row.declined as f64 / row.attempted as f64 * 100.0
            };
            let decline_flag = if decline_pct > 1.0 {
                " <-- DECLINED"
            } else {
                ""
            };
            // A writer whose pushes are refused is contending with nothing, and
            // the row is a quiescent-tree row wearing a writer's name.
            let writer_flag = if n_writers > 0 && row.rejected > row.pushes_per_s as u64 {
                " <-- WRITERS REJECTED"
            } else {
                ""
            };

            println!(
                "{n_readers:>7} {n_writers:>7} | {:>12.3} {:>8} | {:>9.0} {:>9.0} {:>10.0} \
                 | {:>10.1} | {:>9.0} {:>7.2}% {:>6}{flag}{decline_flag}{writer_flag}",
                row.lookups_per_s / 1e6,
                if scale.is_finite() {
                    format!("{scale:.2}x")
                } else {
                    "-".to_owned()
                },
                row.svc_p50_ns,
                row.svc_p99_ns,
                row.svc_p999_ns,
                row.cycle_p999_ns / 1000.0,
                row.pushes_per_s,
                decline_pct,
                contention.recycled + contention.contended,
            );

            run.push(json_row(w.name, &built, &row, scale, pin));
        }
    }

    println!();
    println!(
        "Machine was {:.0}% busy before the run.",
        baseline_busy * 100.0
    );
    println!("`fail` is err_slot_recycled + err_slot_contended, summed over every edge. It counts");
    println!("reads that FAILED, not reads that retried — a successful seqlock retry is invisible");
    println!("to the arena by design, and what it costs shows up in the service columns.");
    println!();
    println!("A row flagged DECLINED had over 1% of its lookups refused, which means the query");
    println!("sweep and the retained window disagreed — that row is timing the error path and");
    println!("is not a result. `declined` is shown as a percentage for exactly that reason.");
    println!();
    println!("The `scale` column is against the 1-reader/0-writer row, which is also the row");
    println!("that must reproduce `just tf2-scaling`'s quiescent numbers; if it does not, this");
    println!("harness is measuring something else and the rest of the table is not comparable.");

    if let Some(path) = &args.json {
        match run.write(path) {
            Ok(()) => println!("\nwrote {}", path.display()),
            Err(e) => {
                eprintln!("contended_scaling: {e:#}");
                std::process::exit(1);
            }
        }
    }
}

struct Summary {
    readers: usize,
    writers: usize,
    lookups_per_s: f64,
    svc_p50_ns: f64,
    svc_p99_ns: f64,
    svc_p999_ns: f64,
    worst_svc_p999_ns: f64,
    cycle_p999_ns: f64,
    pushes_per_s: f64,
    declined: u64,
    attempted: u64,
    rejected: u64,
    cpu_ns: u64,
    pss_kib: u64,
    clock_overhead_ns: u64,
    contention: Contention,
}

fn summarise(
    readers: &[ReaderReport],
    writers: &[WriterReport],
    n_readers: usize,
    n_writers: usize,
    contention: Contention,
    seconds: f64,
) -> Summary {
    let mut service = Histogram::new();
    let mut cycle = Histogram::new();
    let mut worst_p999 = 0u64;
    let (mut ops, mut cpu_ns, mut pss_kib, mut declined) = (0u64, 0u64, 0u64, 0u64);
    let mut attempted = 0u64;
    let mut elapsed_ns_max = 0u64;
    let mut clock_overhead = 0u64;

    for r in readers {
        service.merge(&r.service);
        cycle.merge(&r.cycle);
        // The worst reader's tail, not the fleet's: the number an integrator
        // lives with is the unluckiest node's, not the average node's.
        worst_p999 = worst_p999.max(r.service.quantile(0.999));
        ops += r.ops;
        cpu_ns += r.cpu_ns;
        pss_kib += r.pss_kib;
        declined += r.declined;
        attempted += r.attempted;
        // The *longest* pass bounds the aggregate: dividing a sum of lookups by
        // the shortest reader's window would inflate throughput by however much
        // the unluckiest reader was delayed.
        elapsed_ns_max = elapsed_ns_max.max(r.elapsed_ns);
        clock_overhead = clock_overhead.max(r.clock_overhead_ns);
    }

    let lookups_per_s = if elapsed_ns_max == 0 {
        0.0
    } else {
        ops as f64 / (elapsed_ns_max as f64 / 1e9)
    };
    let pushed: u64 = writers.iter().map(|w| w.pushed).sum();
    let rejected: u64 = writers.iter().map(|w| w.rejected).sum();

    Summary {
        readers: n_readers,
        writers: n_writers,
        lookups_per_s,
        svc_p50_ns: service.quantile(0.50) as f64,
        svc_p99_ns: service.quantile(0.99) as f64,
        svc_p999_ns: service.quantile(0.999) as f64,
        worst_svc_p999_ns: worst_p999 as f64,
        cycle_p999_ns: cycle.quantile(0.999) as f64,
        pushes_per_s: pushed as f64 / seconds,
        declined,
        attempted,
        rejected,
        cpu_ns,
        pss_kib,
        clock_overhead_ns: clock_overhead,
        contention,
    }
}

/// Slack the A/B differ allows on a latency percentile from this harness.
///
/// Looser than `report::LATENCY_SLACK` (25%) on purpose: those rows are only
/// ever taken on a host that *passed* the fitness probe, and this harness runs
/// wherever it is asked to. 40% is above the run-to-run spread seen on an
/// oversubscribed host and still well under the size of a regression worth a
/// bisect — an extra atomic in the read path costs more than that.
const LATENCY_SLACK: f64 = 0.40;
/// Slack on throughput, which is the steadier of the two measurements because
/// it averages over a whole window rather than reporting a tail.
const THROUGHPUT_SLACK: f64 = 0.15;

fn json_row(workload: &str, built: &Built, s: &Summary, scale: f64, pinned: bool) -> RunRow {
    RunRow::new(
        "contended_scaling",
        workload,
        "tf_tree",
        format!("readers={},writers={}", s.readers, s.writers),
    )
    .with_shape(built.shape)
    .metric(
        Metric::new("lookups_per_s", s.lookups_per_s, "lookup/s")
            .higher_is_better(THROUGHPUT_SLACK),
    )
    .metric(Metric::new("service_p50_ns", s.svc_p50_ns, "ns").lower_is_better(LATENCY_SLACK))
    .metric(Metric::new("service_p99_ns", s.svc_p99_ns, "ns").lower_is_better(LATENCY_SLACK))
    .metric(Metric::new("service_p99_9_ns", s.svc_p999_ns, "ns").lower_is_better(LATENCY_SLACK))
    .metric(
        Metric::new("worst_reader_service_p99_9_ns", s.worst_svc_p999_ns, "ns")
            .lower_is_better(LATENCY_SLACK),
    )
    // Informational, not directional: on an idle machine this is dominated by
    // scheduler wakeup, so gating on it would gate the kernel's timer slack.
    .metric(Metric::new("cycle_p99_9_ns", s.cycle_p999_ns, "ns"))
    // Informational, all of them, and deliberately so. `scale` is a ratio
    // against a row in the same table, so it moves whenever its own denominator
    // does; the contention counters are legitimately zero on an uncontended run
    // and a directional verdict against zero is noise by construction; and
    // `pinned` is a fact about the run, not a result.
    .metric(Metric::new("scale_vs_1r0w", scale, "x"))
    .metric(Metric::new("pushes_per_s", s.pushes_per_s, "push/s"))
    .metric(Metric::new("declined", s.declined as f64, "lookups"))
    .metric(Metric::new("attempted", s.attempted as f64, "lookups"))
    .metric(Metric::new("pushes_rejected", s.rejected as f64, "pushes"))
    .metric(Metric::new(
        "err_slot_recycled",
        s.contention.recycled as f64,
        "reads",
    ))
    .metric(Metric::new(
        "err_slot_contended",
        s.contention.contended as f64,
        "reads",
    ))
    .metric(Metric::new("total_pss_kib", s.pss_kib as f64, "KiB"))
    .metric(Metric::new("cpu_ns", s.cpu_ns as f64, "ns"))
    .metric(Metric::new(
        "clock_overhead_ns",
        s.clock_overhead_ns as f64,
        "ns",
    ))
    .metric(Metric::new("pinned", f64::from(u8::from(pinned)), "bool"))
}
