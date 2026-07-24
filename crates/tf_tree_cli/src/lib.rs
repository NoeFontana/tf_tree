#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Implementation of the `tf_tree` diagnostics CLI.
//!
//! Printing to stdout/stderr is this crate's whole job, so the workspace
//! `print_stdout`/`print_stderr` lints are allowed here.
//!
//! # Phase 1 scope: in-process only
//!
//! Phase 1 has no cross-process attach — that arrives in Phase 2 (shared memory).
//! So every subcommand operates on an **in-process** tree: it builds the shared
//! mobile-robot [`tf_tree_bench::fixture`], populates its history, and inspects
//! *that*. When Phase 2 lands, the same commands will attach to a live external
//! arena instead of building one; the `doctor` checks in [`doctor`] already take a
//! captured snapshot, so only the capture source changes.

use anyhow::Result;
use clap::{Parser, Subcommand};

use tf_tree::{EdgeKind, Iso3, Stamp, Tree};
use tf_tree_bench::fixture;

pub mod doctor;

use doctor::{Observations, Severity, Snapshot};

/// `tf_tree` — inspect and debug a transform tree.
///
/// Phase 1 operates on an in-process fixture tree; live external attach arrives
/// in Phase 2 (shared memory).
#[derive(Parser)]
#[command(name = "tf_tree", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show topology, per-edge kind/rate/occupancy/staleness, and writer PID.
    Tree,
    /// Look up `target` relative to `source` and print the transform.
    Echo {
        /// Target frame.
        target: String,
        /// Source frame.
        source: String,
        /// Repeat the lookup across recent history, as a live stream would.
        #[arg(long)]
        rate: bool,
    },
    /// Diagnose cycles, unclaimed edges, contention, stale buffers, and more.
    Doctor,
    /// Run the runnable benchmark checks; `--gate` exits non-zero on failure.
    Bench {
        /// Fail the process if the runnable gate checks do not pass.
        #[arg(long)]
        gate: bool,
    },
}

/// Parse arguments and dispatch. Entry point shared by both binaries.
///
/// # Errors
///
/// Surfaces any failure building or inspecting the in-process fixture tree.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Tree => cmd_tree(),
        Command::Echo {
            target,
            source,
            rate,
        } => cmd_echo(&target, &source, rate),
        Command::Doctor => cmd_doctor(),
        Command::Bench { gate } => cmd_bench(gate),
    }
}

/// `tf_tree tree` — render the topology.
///
/// Each command leaks its fixture tree (`Box::leak`) so the live [`Publisher`]s,
/// which borrow the tree, can be held for the duration of the inspection without
/// a self-referential owner. The process inspects once and exits, so the single
/// intentional leak is harmless and keeps the borrow checker satisfied with no
/// `unsafe`.
fn cmd_tree() -> Result<()> {
    let tree: &Tree = Box::leak(Box::new(fixture::build_tree()?));
    let (writers, samples) = fixture::spin_up(tree)?;
    let obs = Observations::from_samples(samples);
    let snap = Snapshot::capture(tree);

    println!("tf_tree topology (in-process fixture; live external attach is Phase 2)");
    println!(
        "  {} frames, {} edges, arena {} KiB\n",
        snap.frames.len(),
        snap.edges.len(),
        tree.arena_size_bytes() / 1024
    );

    // Index edges by child frame so we can annotate each frame with its edge.
    println!(
        "  {:<22} {:>5} {:<8} {:>9} {:>12} {:>10} {:>8}",
        "frame", "depth", "kind", "rate(Hz)", "occupancy", "age(ms)", "writer"
    );
    for f in &snap.frames {
        let indent = "  ".repeat(f.depth as usize);
        let edge = snap.edges.iter().find(|e| e.child == f.id);
        let (kind, rate, occ, age, writer) = match edge {
            None => (
                "root".to_owned(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ),
            Some(e) => {
                let kind = match e.kind {
                    EdgeKind::Static => "static",
                    EdgeKind::Dynamic => "dynamic",
                    EdgeKind::Tombstone => "tombstone",
                }
                .to_owned();
                let rate = observed_rate_hz(&obs, e.id)
                    .map(|hz| format!("{hz:.0}"))
                    .unwrap_or_default();
                let occ = if e.kind == EdgeKind::Dynamic {
                    format!("{}/{}", e.occupancy(), e.capacity)
                } else {
                    String::new()
                };
                let age = e
                    .newest_stamp
                    .map(|s| format!("{}", (fixture::NOW_NS - s).max(0) / 1_000_000))
                    .unwrap_or_default();
                let writer = if e.claimed {
                    format!("pid {}", e.owner_pid)
                } else if e.kind == EdgeKind::Dynamic {
                    "UNCLAIMED".to_owned()
                } else {
                    String::new()
                };
                (kind, rate, occ, age, writer)
            }
        };
        println!(
            "  {:<22} {:>5} {:<8} {:>9} {:>12} {:>10} {:>8}",
            format!("{indent}{}", f.name),
            f.depth,
            kind,
            rate,
            occ,
            age,
            writer
        );
    }
    drop(writers);
    Ok(())
}

/// `tf_tree echo target source [--rate]`.
fn cmd_echo(target: &str, source: &str, rate: bool) -> Result<()> {
    let tree = Box::leak(Box::new(fixture::build_tree()?));
    let (writers, _samples) = fixture::spin_up(tree)?;

    if rate {
        // Simulate a live stream: sample across the last 100 ms of history.
        println!("echo {target} <- {source} (streaming recent history; Phase 2 attaches live)");
        let lo = fixture::NOW_NS - 100_000_000;
        for i in 0..10 {
            let stamp: Stamp = Stamp::from_nanos(lo + (fixture::NOW_NS - lo) * i / 10);
            print_lookup(tree, target, source, stamp);
        }
    } else {
        let stamp: Stamp = Stamp::from_nanos(fixture::NOW_NS);
        print_lookup(tree, target, source, stamp);
    }
    drop(writers);
    Ok(())
}

/// Evaluate and print one `target <- source` lookup at `stamp`.
fn print_lookup(tree: &Tree, target: &str, source: &str, stamp: Stamp) {
    match tree.lookup(target, source, stamp) {
        Ok(iso) => println!("  t={:>12} ns  {}", stamp.nanos(), fmt_iso(&iso)),
        Err(e) => println!("  t={:>12} ns  error: {}", stamp.nanos(), tree.describe(e)),
    }
}

/// A compact one-line rendering of an `Iso3`.
fn fmt_iso(iso: &Iso3) -> String {
    format!(
        "q=[{:+.4} {:+.4} {:+.4} {:+.4}]  t=[{:+.4} {:+.4} {:+.4}]",
        iso.q.w, iso.q.x, iso.q.y, iso.q.z, iso.t.x, iso.t.y, iso.t.z
    )
}

/// `tf_tree doctor`.
fn cmd_doctor() -> Result<()> {
    let tree = Box::leak(Box::new(fixture::build_tree()?));
    let (writers, samples) = fixture::spin_up(tree)?;
    let obs = Observations::from_samples(samples);
    let snap = Snapshot::capture(tree);
    let report = doctor::run(&snap, &obs);

    println!("tf_tree doctor (in-process fixture; live external attach is Phase 2)");
    if report.is_healthy() {
        println!(
            "  OK — all seven checks pass ({} frames)",
            snap.frames.len()
        );
    } else {
        for f in &report.findings {
            let sev = match f.severity {
                Severity::Warn => "WARN ",
                Severity::Error => "ERROR",
            };
            println!("  [{sev}] {}: {}", f.check.label(), f.message);
        }
    }
    drop(writers);

    if report.has_error() {
        std::process::exit(1);
    }
    Ok(())
}

/// `tf_tree bench [--gate]`.
///
/// Runs the *runnable* correctness half of the gate in-process: the naive-Rust
/// differential (tf_tree vs an independent lookup, agreement within `1e-12`). The
/// perf gate (depth-3 p50, read-scaling) and the zero-allocation gate are not
/// run from the binary — they need the workspace and dedicated hardware; use
/// `cargo xtask bench-gate` and `cargo bench`.
fn cmd_bench(gate: bool) -> Result<()> {
    println!("tf_tree bench — runnable checks (perf gate needs `cargo xtask bench-gate`)");
    let report = tf_tree_bench::differential::run_naive_rust(50_000, 0x5EED_1234_ABCD_0001)?;
    let status = if report.passed() { "PASS" } else { "FAIL" };
    println!(
        "  differential (naive-Rust reference): {status}  max_error={:e}  tol={:e}  ({} queries)",
        report.max_error, report.tolerance, report.queries
    );
    println!("  perf gate (depth-3 p50, read-scaling, tf2 ratio): run `cargo xtask bench-gate`");
    println!("  zero-alloc gate: run `cargo test -p tf_tree_bench --test zero_alloc`");

    if gate && !report.passed() {
        std::process::exit(1);
    }
    Ok(())
}

/// Observed publish rate (Hz) for an edge, from the median inter-sample interval.
fn observed_rate_hz(obs: &Observations, edge: u32) -> Option<f64> {
    let stamps: Vec<i64> = obs
        .events
        .iter()
        .filter(|s| s.edge == edge)
        .map(|s| s.stamp_ns)
        .collect();
    if stamps.len() < 2 {
        return None;
    }
    let mut intervals: Vec<i64> = stamps.windows(2).map(|w| w[1] - w[0]).collect();
    intervals.sort_unstable();
    let median = intervals[intervals.len() / 2];
    if median <= 0 {
        None
    } else {
        Some(1e9 / median as f64)
    }
}
