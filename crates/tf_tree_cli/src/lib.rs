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

pub mod catalogue;
pub mod checks;
pub mod doctor;
pub mod hostfacts;

/// Live-arena attach (`--attach`) and `tf_tree participants`.
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod attach;

use doctor::{Observations, Snapshot};

/// `tf_tree` — inspect and debug a transform tree.
///
/// Phase 1 operates on an in-process fixture tree; live external attach arrives
/// in Phase 2 (shared memory).
#[derive(Parser)]
#[command(name = "tf_tree", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Live-arena flags, shared by `tree`, `echo` and `doctor`.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[command(flatten)]
    attach: attach::AttachArgs,
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
    Doctor {
        /// Explain this build's arena format version and what a mismatch means.
        ///
        /// `docs/PHASE5.md` §1.2 asks for this by name, because a format
        /// mismatch is **the error operators will meet during the v2 -> v3
        /// upgrade** and the message they get from the attach path is
        /// necessarily terse — it comes from a library that has just refused to
        /// map a segment. This prints both versions, the layout hash, and the
        /// action, and it needs no arena to do it.
        #[arg(long)]
        explain_version: bool,
        /// Emit the report as JSON on one stream (`docs/PHASE5.md` §6).
        ///
        /// The schema is documented on [`catalogue::render_json`] and is
        /// stable: it always carries every catalogue id, so a consumer can tell
        /// "this check did not fire" from "this build has no such check".
        #[arg(long)]
        json: bool,
        /// Exit non-zero if any unsuppressed error-severity check fired.
        ///
        /// Opt-in rather than always-on because `doctor` is run by hand far more
        /// often than by CI, and a diagnostic that returns 1 breaks `&&` in an
        /// operator's shell for no benefit. A gate asks for one.
        #[arg(long)]
        exit_code: bool,
        /// Remove a check from the `--exit-code` gate, by id (`--suppress TFT013`).
        ///
        /// Repeatable. A suppressed check still runs and still prints — the flag
        /// changes the exit status, not the report.
        #[arg(long, value_name = "TFTNNN")]
        suppress: Vec<String>,
    },
    /// Run the runnable benchmark checks; `--gate` exits non-zero on failure.
    Bench {
        /// Fail the process if the runnable gate checks do not pass.
        #[arg(long)]
        gate: bool,
    },
    /// List the processes attached to an arena, from the lock file alone.
    ///
    /// Reads `<runtime_dir>/<domain>/<name>.lock` and **never maps the arena**
    /// (`docs/PHASE2.md` §3.3). That is the point: when the segment is gone, or
    /// this build cannot read its layout, or the owner is wedged, this is the
    /// command that still answers.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    Participants,
}

/// Parse arguments and dispatch. Entry point shared by both binaries.
///
/// # Errors
///
/// Surfaces any failure building or inspecting the in-process fixture tree.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    #[cfg(all(feature = "shm", target_os = "linux"))]
    let live = &cli.attach;
    #[cfg(not(all(feature = "shm", target_os = "linux")))]
    let live = &();
    match cli.command {
        Command::Tree => cmd_tree(live),
        Command::Echo {
            target,
            source,
            rate,
        } => cmd_echo(live, &target, &source, rate),
        Command::Doctor {
            explain_version,
            json,
            exit_code,
            suppress,
        } => {
            if explain_version {
                explain_format_version();
                Ok(())
            } else {
                cmd_doctor(live, json, exit_code, &suppress)
            }
        }
        Command::Bench { gate } => cmd_bench(gate),
        #[cfg(all(feature = "shm", target_os = "linux"))]
        Command::Participants => cmd_participants(live),
    }
}

/// The live-arena flags, or `()` on a build without `shm`.
///
/// A type alias rather than `#[cfg]` at every call site: the three inspection
/// commands differ between builds only in where their tree comes from, and
/// duplicating each of them to say so would be three chances to let the two
/// copies drift.
#[cfg(all(feature = "shm", target_os = "linux"))]
type Live<'a> = &'a attach::AttachArgs;
#[cfg(not(all(feature = "shm", target_os = "linux")))]
type Live<'a> = &'a ();

/// Where a command's tree came from, which is the one thing the output has to be
/// honest about.
enum Source {
    /// The in-process benchmark fixture, with its recorded push stream.
    Fixture(Observations),
    /// A live arena somebody else is publishing into.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    Live,
}

impl Source {
    fn banner(&self) -> &'static str {
        match self {
            Source::Fixture(_) => "in-process fixture",
            #[cfg(all(feature = "shm", target_os = "linux"))]
            Source::Live => "live arena",
        }
    }

    /// Whether the push stream was reconstructed from the rings rather than
    /// recorded as it happened — which is what makes `TFT001` unanswerable.
    fn is_live(&self) -> bool {
        match self {
            Source::Fixture(_) => false,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            Source::Live => true,
        }
    }
}

/// Build the fixture, or attach — and keep whatever has to stay alive alive.
///
/// The fixture's trees are `Box::leak`ed because its [`tf_tree::EdgeWriter`]s
/// borrow the tree and are held for the duration of the inspection. The process
/// inspects once and exits, so one intentional leak is cheaper than a
/// self-referential owner, and it needs no `unsafe`.
///
/// An attached tree is leaked for the same reason and one more: its `Drop`
/// releases the participant slot and stops the owner thread, and there is
/// nothing useful to do with either between the last `println!` and `exit`.
fn source(live: Live<'_>) -> Result<(&'static Tree, Source)> {
    #[cfg(all(feature = "shm", target_os = "linux"))]
    if live.attach {
        let tree: &'static Tree = Box::leak(Box::new(live.open()?));
        return Ok((tree, Source::Live));
    }
    let _ = live;
    let tree: &'static Tree = Box::leak(Box::new(fixture::build_tree()?));
    let (writers, samples) = fixture::spin_up(tree)?;
    // Leaked for the same reason as the tree: the claims must stay held while
    // the snapshot is taken, or every dynamic edge reports UNCLAIMED.
    core::mem::forget(writers);
    Ok((tree, Source::Fixture(Observations::from_samples(samples))))
}

/// The push stream a command's checks run against.
///
/// A live arena has no recorded push stream — nobody was watching when those
/// samples arrived — so it is reconstructed from what the rings retain. That is
/// strictly less than the fixture knows: the ring holds the newest `capacity`
/// stamps and the *current* claim owner, so rate, ordering and buffer-depth
/// checks all work, and the multi-writer check cannot fire because a ring cannot
/// remember a writer that has been replaced.
fn observations(tree: &Tree, src: &Source) -> Observations {
    // Used only by the live arm, which does not exist without `shm`.
    let _ = tree;
    match src {
        Source::Fixture(obs) => obs.clone(),
        #[cfg(all(feature = "shm", target_os = "linux"))]
        Source::Live => Observations::from_arena(tree, &Snapshot::capture(tree)),
    }
}

/// `tf_tree tree` — render the topology.
///
/// Each command leaks its fixture tree (`Box::leak`) so the live [`Publisher`]s,
/// which borrow the tree, can be held for the duration of the inspection without
/// a self-referential owner. The process inspects once and exits, so the single
/// intentional leak is harmless and keeps the borrow checker satisfied with no
/// `unsafe`.
fn cmd_tree(live: Live<'_>) -> Result<()> {
    let (tree, src) = source(live)?;
    let obs = observations(tree, &src);
    let snap = Snapshot::capture(tree);

    println!("tf_tree topology ({})", src.banner());
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
    Ok(())
}

/// `tf_tree echo target source [--rate]`.
fn cmd_echo(live: Live<'_>, target: &str, source_frame: &str, rate: bool) -> Result<()> {
    let (tree, src) = source(live)?;
    // The fixture's history is anchored to its own synthetic `NOW_NS`; a live
    // arena's is anchored to whatever its publishers last stamped. Echoing a
    // live tree at the fixture's clock would report `Extrapolation` for every
    // sample and look like a broken arena.
    let now = newest_stamp(tree).unwrap_or(fixture::NOW_NS);

    if rate {
        println!(
            "echo {target} <- {source_frame} ({}, recent history)",
            src.banner()
        );
        let lo = now - 100_000_000;
        for i in 0..10 {
            let stamp: Stamp = Stamp::from_nanos(lo + (now - lo) * i / 10);
            print_lookup(tree, target, source_frame, stamp);
        }
    } else {
        print_lookup(tree, target, source_frame, Stamp::from_nanos(now));
    }
    Ok(())
}

/// The newest stamp on any edge, which is "now" as far as this arena is
/// concerned.
///
/// `None` for an arena with no samples at all, which is a real state — an arena
/// that was just created, or whose publishers have not started.
fn newest_stamp(tree: &Tree) -> Option<i64> {
    Snapshot::capture(tree)
        .edges
        .iter()
        .filter_map(|e| e.newest_stamp)
        .max()
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

/// `tf_tree doctor` — the `docs/PHASE5.md` §6 catalogue.
///
/// **`--exit-code` is opt-in, and the previous unconditional `exit(1)` on any
/// error is gone.** `doctor` is run by hand far more often than by CI, and a
/// diagnostic that returns non-zero by default breaks `&&` in an operator's
/// shell and gets wrapped in `|| true`, at which point the gate is worthless
/// where it was wanted. §6 asks for the flag; the flag is the whole mechanism.
fn cmd_doctor(live: Live<'_>, json: bool, exit_code: bool, suppress: &[String]) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    for s in suppress {
        let id = catalogue::Tft::parse(s).ok_or_else(|| {
            // Refused rather than ignored: a typo that silently suppresses
            // nothing leaves a gate that looks configured and is not.
            anyhow::anyhow!("unknown check id {s:?} — expected one of TFT001..TFT016")
        })?;
        ids.insert(id);
    }

    let (tree, src) = source(live)?;
    let obs = observations(tree, &src);
    let snap = Snapshot::capture(tree);
    let stats = checks::collect_edge_stats(tree, &snap);
    let clock = checks::Clock::decide(&checks::newest_stamps(&snap), unix_nanos_now());

    let inputs = checks::Inputs {
        snap: &snap,
        obs: &obs,
        stats: &stats,
        host: host_facts(),
        clock,
        arena_bytes: tree.arena_size_bytes() as u64,
        occupancy: checks::occupancy_of(tree),
        live: src.is_live(),
        counters: tf_tree::counters_compiled_in(),
    };
    let report = checks::run(&inputs, &ids);

    let meta = catalogue::Meta {
        source: src.banner(),
        format_version: tf_tree::arena_format_version(),
        layout_hash: tf_tree::arena_layout_hash(),
        instance: instance_uuid(tree, &src),
        frames: snap.frames.len(),
        edges: snap.edges.len(),
        generated_unix_nanos: unix_nanos_now(),
        now_nanos: clock.nanos(),
        clock_source: clock.label(),
        counters_compiled_in: tf_tree::counters_compiled_in(),
        notes: live_evidence_notes(src.is_live()),
    };

    if json {
        print!("{}", catalogue::render_json(&report, &meta));
    } else {
        print!("{}", catalogue::render_human(&report, &meta));
    }

    if exit_code && report.has_error() {
        std::process::exit(1);
    }
    Ok(())
}

/// Disclosures for a check that ran with one of its evidence sources missing.
///
/// `TFT011` has two: the counters, which a live arena has, and the Phase 1
/// `capacity x period` against observed publish latency, which needs a recorded
/// push stream. A live arena's stream is reconstructed from the rings, where
/// `arrival_delay_ns` is unknown and set to zero — and zero latency never
/// exceeds any buffer span, so that half of the check is structurally silent.
/// Reporting `pass` without saying so would claim a result it did not earn.
fn live_evidence_notes(live: bool) -> Vec<String> {
    if !live {
        return Vec::new();
    }
    vec![
        "TFT011 ran on its counter evidence only: a live arena has no recorded publish \
         latency, so the capacity-vs-latency half of the check cannot fire"
            .to_owned(),
    ]
}

/// The system clock as nanoseconds since the Unix epoch.
///
/// Saturates rather than panicking on a clock before 1970: `doctor` reporting a
/// bad clock is useful, `doctor` aborting because of one is not.
fn unix_nanos_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Host facts for `TFT016`, or `None` where `/sys` and `/proc` do not exist.
fn host_facts() -> Option<hostfacts::HostFacts> {
    #[cfg(target_os = "linux")]
    {
        Some(hostfacts::probe())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The arena's instance uuid, which only a shared arena has.
fn instance_uuid(tree: &Tree, src: &Source) -> Option<String> {
    let _ = (tree, src);
    #[cfg(all(feature = "shm", target_os = "linux"))]
    if matches!(src, Source::Live) {
        return Some(hex16(tree.instance_uuid()));
    }
    None
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

/// Sixteen bytes as 32 lowercase hex characters.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn hex16(bytes: [u8; 16]) -> String {
    use core::fmt::Write;
    bytes.iter().fold(String::with_capacity(32), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `tf_tree participants` — who is attached, from the lock file alone.
///
/// **Never maps the arena** (`docs/PHASE2.md` §3.3), and that is the entire
/// value of it. Every other command needs a segment this build can read; this
/// one answers when the segment is gone, when its layout hash does not match, or
/// when the owner is wedged and nobody can complete a handshake. Those are
/// exactly the situations in which somebody runs a diagnostic tool.
///
/// Liveness is the kernel's answer — `F_OFD_GETLK` on the participant's byte —
/// not an inference from the identity record, which is why a `SIGSTOP`ped
/// process correctly reads as alive (§5.1).
#[cfg(all(feature = "shm", target_os = "linux"))]
fn cmd_participants(live: Live<'_>) -> Result<()> {
    let rv = live.rendezvous()?;
    let path = rv.lock_path();
    println!("tf_tree participants — {}", path.display());

    if !path.exists() {
        // Not an error. "Nothing is running" is a legitimate and common answer,
        // and exiting non-zero would make it indistinguishable from a failure to
        // look.
        println!("  no lock file: nothing has ever attached to this domain/name");
        return Ok(());
    }

    // `IpcError` is `Copy` and `String`-free by design (`docs/PROJECT.md` §5),
    // so it is not `std::error::Error` and cannot be `?`-ed into `anyhow`
    // directly. Formatting it here is the seam where a `no_std` error becomes a
    // human-facing one.
    let lock = tf_tree_ipc::LockFile::open(path)
        .map_err(|e| anyhow::anyhow!("opening {}: {e:?}", path.display()))?;

    println!("  slot       pid  mode    state    comm");
    let mut live_count = 0;
    for slot in 0..tf_tree_ipc::MAX_PARTICIPANTS {
        let held = lock
            .probe_participant(slot)
            .map(|p| p.held)
            .unwrap_or(false);
        let id = lock.read_identity(slot).ok().flatten();
        // A byte held with no identity record is a participant caught between
        // taking its byte and writing its record — a real, momentary state, and
        // worth showing rather than skipping.
        if !held && id.is_none() {
            continue;
        }
        if held {
            live_count += 1;
        }
        let (pid, mode, comm) = match &id {
            None => (0, "-", String::from("<no record>")),
            Some(i) => (
                i.pid,
                match i.mode {
                    tf_tree_ipc::AccessMode::ReadOnly => "ro",
                    tf_tree_ipc::AccessMode::ReadWrite => "rw",
                },
                {
                    let n = i.name.iter().position(|b| *b == 0).unwrap_or(i.name.len());
                    String::from_utf8_lossy(&i.name[..n]).into_owned()
                },
            ),
        };
        // "stale" is the interesting one: a record whose byte the kernel has
        // already released, i.e. the process is gone and left its record behind.
        // That is what a reaper collects, and seeing it here is how an operator
        // knows one is owed.
        let state = if held { "live" } else { "stale" };
        println!("  {slot:>4}  {pid:>8}  {mode:<6}  {state:<7}  {comm}");
    }
    if live_count == 0 {
        println!("  (no live participants)");
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

/// Print this build's arena format version and what a mismatch means.
///
/// `docs/PHASE5.md` §1.2 requires this alongside the `FORMAT_VERSION = 3` bump,
/// and the reason is operational rather than tidy: a version mismatch is the
/// one error an operator is *guaranteed* to hit during the upgrade, and the
/// message the attach path can give them is necessarily terse — it comes from a
/// library that has just declined to map a segment and has no vocabulary for
/// "restart your fleet together".
///
/// It reads no arena and takes no lock, so it answers on a machine where
/// nothing is running and on one where everything is wedged.
fn explain_format_version() {
    let v = tf_tree::arena_format_version();
    let h = tf_tree::arena_layout_hash();
    println!("tf_tree arena format");
    println!("  format_version  {v}");
    println!("  layout_hash     0x{h:08X}");
    println!();
    println!("Both are checked when a process attaches to a shared arena, and a");
    println!("mismatch on either is refused. They mean different things:");
    println!();
    println!("  format_version  the *set of fields* in the arena header changed.");
    println!("                  A different version is never compatible.");
    println!("  layout_hash     the *geometry* changed — a record grew, a region");
    println!("                  was added, an alignment moved. Two builds with");
    println!("                  the same version and different hashes disagree");
    println!("                  about where things are, which is worse than");
    println!("                  disagreeing about what they are.");
    println!();
    println!("If you are seeing a mismatch:");
    println!();
    println!("  1. Every participant must be rebuilt from the same commit and");
    println!("     restarted TOGETHER. There is no compatibility layer, and a");
    println!("     rolling restart leaves half the fleet unable to attach.");
    println!("  2. The arena does not survive the restart. Kill every attached");
    println!("     process (`tf_tree participants` lists them, and works without");
    println!("     mapping the arena), then start the publisher first.");
    println!("  3. A stale segment from a previous boot is a different fault and");
    println!("     reports differently; `tf_tree doctor` names that one.");
    println!();
    if v >= 3 {
        println!("Version 3 (docs/PHASE5.md §1) is a deliberate one-time break. It");
        println!("added the diagnostic counter regions and reserved header space");
        println!("for Phase 6's covariance and spline regions, so that those land");
        println!("without a second break. A version-2 arena cannot be attached.");
    }
}
