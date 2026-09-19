#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Implementation of the `tf_tree` diagnostics CLI.
//!
//! Printing to stdout/stderr is this crate's whole job, so the `print_stdout`
//! and `print_stderr` lints are allowed here.
//!
//! # Four sources
//!
//! The bare invocation builds the in-process [`tf_tree_bench::fixture`];
//! `--attach` and `doctor --from-file` need Linux and `--features shm`.
//! Everything downstream reads a captured [`doctor::Snapshot`];
//! `checks::PushStream` says where sources differ.

// Compiles `README.md`'s `rust` fence (gates the API, not the output) against the
// union of features; here because `include_str!` outside the package breaks `cargo package`.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
mod root_readme {}

use anyhow::Result;
use clap::{Parser, Subcommand};

use tf_tree::unstable::EdgeKind;
use tf_tree::{Iso3, Stamp, Tree};
use tf_tree_bench::fixture;

pub mod catalogue;
pub mod checks;
pub mod doctor;
pub mod hostfacts;
/// `doctor`'s recording sources (`docs/PHASE5.md` §6): `--from-bag`, `--from-file`.
pub mod recording;
pub mod sizing;
pub mod top;
pub mod topology;
pub mod web;

/// Live-arena attach (`--attach`) and `tf_tree participants`.
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod attach;

use doctor::{Observations, Snapshot};

/// `tf_tree` — inspect and debug a transform tree.
///
/// Name no source and every subcommand builds an in-process fixture. Real-data
/// sources: `doctor --from-bag <recording.mcap>`, `tf_tree ingest`, `--attach`
/// (live, read-only) and `doctor --from-file <index.tft>`; the last two need a
/// Linux `--features shm` build.
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

/// The severity floor `--exit-code` gates on; bare `--exit-code` means `error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ExitSeverity {
    /// Only error-severity findings fail (on a live arena: `TFT006`, `TFT012`).
    Error,
    /// Warn-severity findings fail too. `--suppress` silences one a fleet
    /// accepts.
    Warn,
}

/// `--color` for `tf_tree top`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ColorChoice {
    /// Colour if and only if stdout is a terminal.
    Auto,
    /// Always emit colour, even into a pipe.
    Always,
    /// Never emit colour.
    Never,
}

impl ColorChoice {
    /// `None` means "decide from the terminal".
    fn forced(self) -> Option<bool> {
        match self {
            ColorChoice::Auto => None,
            ColorChoice::Always => Some(true),
            ColorChoice::Never => Some(false),
        }
    }
}

/// `--interp` as a flag value (separate so `clap` stays out of the facade).
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum InterpArg {
    /// Screw-linear interpolation — the default.
    Sclerp,
    /// Separate lerp of translation and slerp of rotation.
    Lerpslerp,
}

impl InterpArg {
    fn policy(self) -> tf_tree::InterpPolicy {
        match self {
            InterpArg::Sclerp => tf_tree::InterpPolicy::ScLerp,
            InterpArg::Lerpslerp => tf_tree::InterpPolicy::LerpSlerp,
        }
    }
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
        /// Explain this build's arena format version and what a mismatch means
        /// (`docs/PHASE5.md` §1.2); needs no arena.
        #[arg(long)]
        explain_version: bool,
        /// Emit the report as JSON on one stream (`docs/PHASE5.md` §6; see
        /// [`catalogue::render_json`]).
        #[arg(long)]
        json: bool,
        /// Exit non-zero if any unsuppressed check at this severity or above
        /// fired. `--exit-code` alone means `--exit-code error`.
        /// `--exit-code warn` also gates on warnings.
        #[arg(long, value_name = "SEVERITY", num_args = 0..=1, default_missing_value = "error")]
        exit_code: Option<ExitSeverity>,
        /// Remove a check from the `--exit-code` gate, by id (`--suppress
        /// TFT013`). Repeatable; a suppressed check still runs and prints.
        #[arg(long, value_name = "TFTNNN")]
        suppress: Vec<String>,
        /// Diagnose an MCAP recording instead of the built-in fixture; the only
        /// source `TFT018`/`TFT019` can judge. The §3.2 ingest report goes to
        /// **stderr**, so `--json` stays parseable.
        ///
        /// Exclusive with `--attach`, enforced in [`doctor_source`]: `--attach` is
        /// `global = true`, so `conflicts_with` never sees it.
        #[arg(long, value_name = "PATH")]
        from_bag: Option<std::path::PathBuf>,
        /// Diagnose a frozen `.tft` index (`docs/PHASE5.md` §2); every check runs
        /// as on an attach except `TFT018`/`TFT019` (use `--from-bag`).
        ///
        /// Exclusive with `--from-bag` (`clap`) and `--attach` ([`doctor_source`]).
        #[cfg(all(feature = "shm", target_os = "linux"))]
        #[arg(long, value_name = "PATH", conflicts_with = "from_bag")]
        from_file: Option<std::path::PathBuf>,
        #[command(flatten)]
        ingest: IngestArgs,
    },
    /// Live view of an arena: rates, staleness, claims, participants, feed.
    ///
    /// `docs/PHASE5.md` §7. Read-only; see [`top`].
    Top {
        /// Redraw interval in milliseconds.
        #[arg(long, default_value_t = 1000, value_name = "MS")]
        interval: u64,
        /// Stop after this many frames; `0` runs until interrupted. With
        /// `--web` it bounds connections, not frames.
        #[arg(long, default_value_t = 0, value_name = "N")]
        iterations: u64,
        /// Show the per-edge detail pane, with the inter-arrival histogram.
        /// Takes an edge id or a substring of its `parent->child` label.
        #[arg(long, value_name = "ID|NAME")]
        edge: Option<String>,
        /// `auto` (the default) follows whether stdout is a tty. Ignored under
        /// `--web`.
        #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
        color: ColorChoice,
        /// Serve §7's embedded web view instead of drawing to the terminal.
        /// `--web` alone binds `127.0.0.1:8787`; `--web ADDR` binds what you
        /// name (`:0` picks a free port). A non-loopback bind is accepted and
        /// warned about (§7).
        #[arg(long, value_name = "ADDR", num_args = 0..=1, default_missing_value = web::DEFAULT_ADDR)]
        web: Option<std::net::SocketAddr>,
    },
    /// Run the runnable benchmark checks; `--gate` exits non-zero on failure.
    Bench {
        /// Fail the process if the runnable gate checks do not pass.
        #[arg(long)]
        gate: bool,
    },
    /// Read an MCAP recording and report what is in its `/tf` traffic
    /// (`docs/PHASE5.md` §3).
    ///
    /// Needs nothing installed and is not behind `--features shm`. Use `tf_tree
    /// freeze --from-bag` to keep the result.
    Ingest {
        /// The `.mcap` recording to read.
        #[arg(long, value_name = "PATH")]
        bag: std::path::PathBuf,
        /// Also write the report as JSON to this path.
        #[arg(long, value_name = "PATH")]
        report: Option<std::path::PathBuf>,
        #[command(flatten)]
        opts: IngestArgs,
    },
    /// Write a frozen `.tft` index (`docs/PHASE5.md` §2), from a live arena or
    /// from a recording. Exactly one source is required.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    Freeze {
        /// Freeze the arena named by the global attach flags.
        #[arg(long, conflicts_with = "from_bag")]
        from_live: bool,
        /// Ingest this `.mcap` recording and freeze the result (§3).
        #[arg(long, value_name = "PATH", conflicts_with = "from_live")]
        from_bag: Option<std::path::PathBuf>,
        /// Destination path. Overwritten if it exists.
        #[arg(long, short)]
        out: std::path::PathBuf,
        /// Where to write the ingest report. Defaults to `<out>.ingest.json`
        /// for `--from-bag`; ignored for `--from-live`.
        #[arg(long, value_name = "PATH")]
        report: Option<std::path::PathBuf>,
        #[command(flatten)]
        ingest: IngestArgs,
    },
    /// Obtain, validate or explain a bridge topology file (`docs/PHASE4.md`
    /// §5.8). `--discover` obtains the file from a robot; `--config` is the
    /// pre-flight.
    Topology {
        /// Read a recorded `/tf` stream and print the config it implies.
        #[arg(long, value_name = "FILE.tfstream", conflicts_with = "config")]
        discover: Option<std::path::PathBuf>,
        /// Parse a topology file, build the arena it describes, and print it.
        #[arg(long, value_name = "FILE.toml")]
        config: Option<std::path::PathBuf>,
        /// Write the discovered config here instead of to stdout.
        #[arg(long, short, requires = "discover")]
        out: Option<std::path::PathBuf>,
        /// Seconds of history the discovered rings should retain.
        #[arg(long, default_value_t = 10.0, requires = "discover")]
        history_secs: f64,
        /// Prefix every discovered frame with this `tf_prefix` (§5.6), to match
        /// the bridge that will read the file.
        #[arg(long, value_name = "PREFIX", requires = "discover")]
        tf_prefix: Option<String>,
        /// Interpolation policy the discovered file should default to.
        #[arg(long, value_enum, requires = "discover")]
        interp: Option<InterpArg>,
        /// Check the file's per-edge time domains against the bridge's (§5.5).
        #[arg(long, value_name = "N", requires = "config")]
        domain: Option<u8>,
    },
    /// List the processes attached to an arena, from the lock file alone. Reads
    /// `<runtime_dir>/<domain>/<name>.lock` and **never maps the arena**
    /// (`docs/PHASE2.md` §3.3), so it answers when the segment is gone or
    /// unreadable.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    Participants,
}

/// Parse arguments and dispatch. Entry point shared by both binaries.
///
/// # Errors
///
/// Any failure obtaining or inspecting the named tree, or a `doctor_source` refusal.
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
            from_bag,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            from_file,
            ingest,
        } => {
            #[cfg(not(all(feature = "shm", target_os = "linux")))]
            let from_file: Option<std::path::PathBuf> = None;
            if explain_version {
                explain_format_version();
                Ok(())
            } else {
                cmd_doctor(
                    live,
                    json,
                    exit_code,
                    &suppress,
                    from_bag.as_deref(),
                    from_file.as_deref(),
                    &ingest,
                )
            }
        }
        Command::Top {
            interval,
            iterations,
            edge,
            color,
            web,
        } => cmd_top(live, interval, iterations, edge, color.forced(), web),
        Command::Bench { gate } => cmd_bench(gate),
        Command::Ingest { bag, report, opts } => cmd_ingest(&bag, report.as_deref(), &opts),
        Command::Topology {
            discover,
            config,
            out,
            history_secs,
            tf_prefix,
            interp,
            domain,
        } => match (discover, config) {
            (Some(src), _) => topology::cmd_discover(
                &src,
                out.as_deref(),
                history_secs,
                tf_prefix.as_deref(),
                interp.map(InterpArg::policy),
            ),
            (None, Some(cfg)) => topology::cmd_check(&cfg, domain),
            (None, None) => Err(anyhow::anyhow!(
                "give --discover <file.tfstream> to obtain a topology file, \
                 or --config <file.toml> to check one"
            )),
        },
        #[cfg(all(feature = "shm", target_os = "linux"))]
        Command::Freeze {
            from_live,
            from_bag,
            out,
            report,
            ingest,
        } => cmd_freeze(
            live,
            from_live,
            from_bag.as_deref(),
            &out,
            report.as_deref(),
            &ingest,
        ),
        #[cfg(all(feature = "shm", target_os = "linux"))]
        Command::Participants => cmd_participants(live),
    }
}

/// Default for `--max-memory`, in MiB; [`IngestArgs::flags_set`] compares against it.
const DEFAULT_MAX_MEMORY_MIB: u64 = 4096;

/// Default for `--future-horizon`, in seconds. See [`DEFAULT_MAX_MEMORY_MIB`].
const DEFAULT_FUTURE_HORIZON_S: f64 = 10.0;

/// Default for `--clock-reset-threshold`, in milliseconds. See
/// [`DEFAULT_MAX_MEMORY_MIB`].
const DEFAULT_CLOCK_RESET_THRESHOLD_MS: u64 = 100;

/// The `docs/PHASE5.md` §3 ingest knobs, flattened into `ingest`, `freeze
/// --from-bag` and `doctor` (see [`IngestArgs::flags_set`]).
#[derive(clap::Args, Clone, Debug)]
pub struct IngestArgs {
    /// Peak buffered-sample memory for pass two, in MiB (§3.1); under `doctor
    /// --from-bag` it also bounds [`recording::arrival_observations`].
    #[arg(long, value_name = "MIB", default_value_t = DEFAULT_MAX_MEMORY_MIB)]
    pub max_memory: u64,
    /// What to do when the recording's clock jumps backwards (§3.2).
    #[arg(long, value_enum, default_value_t = ClockResetArg::Halt)]
    pub on_clock_reset: ClockResetArg,
    /// What to do about a chunk that will not decompress or fails its CRC.
    /// `skip` (default) drops it, counts it and reports the lost span; `halt`
    /// refuses. A codec this build cannot decompress is never skipped.
    #[arg(long, value_enum, default_value_t = BadChunkArg::Skip, value_name = "POLICY")]
    pub on_bad_chunk: BadChunkArg,
    /// Treat this topic as carrying static transforms. Repeatable. **Replaces**
    /// the default rule (last path segment `tf_static`).
    #[arg(long, value_name = "TOPIC")]
    pub static_topic: Vec<String>,
    /// Read only this topic's dynamic transforms. Repeatable. The only flag
    /// that narrows the read (§3.3).
    #[arg(long, value_name = "TOPIC")]
    pub tf_topic: Vec<String>,
    /// Prefix every frame name, as a `tf_prefix` would (`docs/PHASE4.md` §5.6).
    #[arg(long, value_name = "PREFIX")]
    pub tf_prefix: Option<String>,
    /// How far ahead of its own recorded time a stamp may be before it is
    /// reported, in seconds (§3.2).
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_FUTURE_HORIZON_S)]
    pub future_horizon: f64,
    /// How far backwards a stamp must jump to count as a clock reset rather
    /// than ordinary interleaving, in milliseconds.
    #[arg(long, value_name = "MILLIS", default_value_t = DEFAULT_CLOCK_RESET_THRESHOLD_MS)]
    pub clock_reset_threshold: u64,
    /// Largest chunk this reader will decompress, in MiB. Bounds the output
    /// buffer, not peak memory: allow about 2.6× it for the decoder.
    #[arg(
        long,
        value_name = "MIB",
        default_value_t = tf_tree_ingest::DEFAULT_MAX_CHUNK_UNCOMPRESSED_BYTES / (1024 * 1024)
    )]
    pub max_chunk_size: u64,
    /// Largest `uncompressed_size / compressed_size` a chunk may claim; the
    /// other half of the bomb guard.
    #[arg(
        long,
        value_name = "RATIO",
        default_value_t = tf_tree_ingest::DEFAULT_MAX_CHUNK_EXPANSION_RATIO
    )]
    pub max_chunk_expansion: u64,
    /// Largest top-level record body the reader will allocate for, in MiB;
    /// raise it for recordings with large attachments.
    #[arg(
        long,
        value_name = "MIB",
        default_value_t = tf_tree_ingest::DEFAULT_MAX_RECORD_BYTES / (1024 * 1024)
    )]
    pub max_record_size: u64,
    /// Where to put §3.1's temporary run file (default: the system temp
    /// directory). Only used when a single edge exceeds `--max-memory`; avoid a
    /// tmpfs.
    #[arg(long, value_name = "DIR")]
    pub spill_dir: Option<std::path::PathBuf>,
}

/// `--on-clock-reset`, as §3.2 spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ClockResetArg {
    /// Stop and name the timestamp.
    Halt,
    /// §3.2's multi-file split, **not implemented**; the value exists so the
    /// tool can say so.
    Split,
}

/// `--on-bad-chunk`, mapped 1:1 onto `tf_tree_ingest`'s policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum BadChunkArg {
    /// Skip the chunk, count it, report the span it lost.
    Skip,
    /// Refuse the recording, naming the chunk.
    Halt,
}

impl Default for IngestArgs {
    /// Exactly what `clap` would produce from an empty command line.
    fn default() -> IngestArgs {
        IngestArgs {
            max_memory: DEFAULT_MAX_MEMORY_MIB,
            on_clock_reset: ClockResetArg::Halt,
            on_bad_chunk: BadChunkArg::Skip,
            static_topic: Vec::new(),
            tf_topic: Vec::new(),
            tf_prefix: None,
            future_horizon: DEFAULT_FUTURE_HORIZON_S,
            clock_reset_threshold: DEFAULT_CLOCK_RESET_THRESHOLD_MS,
            max_chunk_size: tf_tree_ingest::DEFAULT_MAX_CHUNK_UNCOMPRESSED_BYTES / (1024 * 1024),
            max_chunk_expansion: tf_tree_ingest::DEFAULT_MAX_CHUNK_EXPANSION_RATIO,
            max_record_size: tf_tree_ingest::DEFAULT_MAX_RECORD_BYTES / (1024 * 1024),
            spill_dir: None,
        }
    }
}

impl IngestArgs {
    /// The flags whose value differs from the default, by the spelling a user
    /// types, so `doctor` can refuse one it would ignore. A flag passed at its
    /// default is missed, harmlessly.
    #[must_use]
    pub fn flags_set(&self) -> Vec<&'static str> {
        let d = IngestArgs::default();
        let mut out = Vec::new();
        if self.max_memory != d.max_memory {
            out.push("--max-memory");
        }
        if self.on_clock_reset != d.on_clock_reset {
            out.push("--on-clock-reset");
        }
        if self.on_bad_chunk != d.on_bad_chunk {
            out.push("--on-bad-chunk");
        }
        if self.static_topic != d.static_topic {
            out.push("--static-topic");
        }
        if self.tf_topic != d.tf_topic {
            out.push("--tf-topic");
        }
        if self.tf_prefix != d.tf_prefix {
            out.push("--tf-prefix");
        }
        // Bit-for-bit, so `--future-horizon NaN` differs from the default.
        if self.future_horizon.to_bits() != d.future_horizon.to_bits() {
            out.push("--future-horizon");
        }
        if self.clock_reset_threshold != d.clock_reset_threshold {
            out.push("--clock-reset-threshold");
        }
        if self.max_chunk_size != d.max_chunk_size {
            out.push("--max-chunk-size");
        }
        if self.max_chunk_expansion != d.max_chunk_expansion {
            out.push("--max-chunk-expansion");
        }
        if self.max_record_size != d.max_record_size {
            out.push("--max-record-size");
        }
        if self.spill_dir != d.spill_dir {
            out.push("--spill-dir");
        }
        out
    }

    /// Convert to the library's options, failing on a value that cannot be
    /// represented.
    fn to_options(&self) -> Result<tf_tree_ingest::IngestOptions> {
        let horizon = self.future_horizon * 1e9;
        anyhow::ensure!(
            horizon.is_finite() && (0.0..=9.2e18).contains(&horizon),
            "--future-horizon {} is not a usable number of seconds",
            self.future_horizon
        );
        Ok(tf_tree_ingest::IngestOptions {
            roles: tf_tree_ingest::TopicRoles {
                static_topics: self.static_topic.clone(),
                dynamic_topics: self.tf_topic.clone(),
            },
            max_memory_bytes: self.max_memory.saturating_mul(1024 * 1024),
            on_bad_chunk: match self.on_bad_chunk {
                BadChunkArg::Skip => tf_tree_ingest::OnBadChunk::Skip,
                BadChunkArg::Halt => tf_tree_ingest::OnBadChunk::Halt,
            },
            on_clock_reset: match self.on_clock_reset {
                ClockResetArg::Halt => tf_tree_ingest::ClockResetPolicy::Halt,
                ClockResetArg::Split => tf_tree_ingest::ClockResetPolicy::Split,
            },
            clock_reset_threshold_ns: i64::try_from(self.clock_reset_threshold)
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000_000),
            future_horizon_ns: horizon as i64,
            tf_prefix: self.tf_prefix.clone(),
            // `saturating_mul`: wrapping would give a tiny ceiling that refuses everything.
            max_chunk_uncompressed_bytes: self.max_chunk_size.saturating_mul(1024 * 1024),
            max_chunk_expansion_ratio: self.max_chunk_expansion,
            max_record_bytes: self.max_record_size.saturating_mul(1024 * 1024),
            spill_dir: self.spill_dir.clone(),
        })
    }
}

/// `tf_tree ingest --bag` — the two passes, and the §3.2 report.
fn cmd_ingest(
    bag: &std::path::Path,
    report: Option<&std::path::Path>,
    args: &IngestArgs,
) -> Result<()> {
    let opts = args.to_options()?;
    let mut frames = tf_tree_ingest::Frames::default();
    let out = tf_tree_ingest::run(bag, &opts, &mut frames).map_err(|e| ingest_err(e, &frames))?;
    print!("{}", out.report.summary());
    if let Some(path) = report {
        std::fs::write(path, out.report.to_json())
            .map_err(|e| anyhow::anyhow!("could not write {}: {e}", path.display()))?;
        println!("  report written to {}", path.display());
    }
    Ok(())
}

/// The remedy shared by the `BadChunk` and `AllChunksOverLimit` arms.
const LIMIT_REMEDY: &str =
    "\x20 --max-chunk-size <MiB> raises the ceiling on a chunk's uncompressed\n\
     \x20 size, and --max-chunk-expansion its ratio to the compressed bytes.\n\
     \x20 Both bound what this reader will allocate for one chunk, so raise them\n\
     \x20 to what the recording actually needs rather than to the maximum.";

/// Render an ingest failure with its frame names, plus the one remedy the message lacks.
pub(crate) fn ingest_err(
    e: tf_tree_ingest::IngestError,
    frames: &tf_tree_ingest::Frames,
) -> anyhow::Error {
    let text = tf_tree_ingest::describe(e, frames).to_string();
    match e {
        // Limit refusals get the flags, not `skip` (already the default).
        tf_tree_ingest::IngestError::BadChunk {
            kind:
                tf_tree_ingest::BadChunkKind::ImplausibleSize { .. }
                | tf_tree_ingest::BadChunkKind::ImplausibleWindow { .. },
            ..
        } => anyhow::anyhow!("{text}\n{LIMIT_REMEDY}"),
        tf_tree_ingest::IngestError::BadChunk { .. } => anyhow::anyhow!(
            "{text}\n\
             \x20 --on-bad-chunk=skip (the default) would drop just this chunk and\n\
             \x20 report the span of time it takes with it, instead of refusing the\n\
             \x20 whole recording."
        ),
        tf_tree_ingest::IngestError::AllChunksOverLimit { .. } => anyhow::anyhow!(
            "{text}\n\
             \x20 The recording is not damaged: every chunk in it was larger than\n\
             \x20 this reader will allocate for, which is what a writer configured\n\
             \x20 with big chunks produces.\n\
             {LIMIT_REMEDY}"
        ),
        // One message for both builds: it names an unknown codec and `--no-default-features`.
        tf_tree_ingest::IngestError::CompressedChunk { .. } => anyhow::anyhow!(
            "{text}\n\
             \x20 zstd and lz4 are read by pure-Rust codecs behind tf_tree_ingest's\n\
             \x20 default-on `compression` feature — mcap's own would vendor a C\n\
             \x20 build step, which docs/PHASE2.md §2 forbids. So this is either a\n\
             \x20 codec outside the MCAP specification, or a build with that feature\n\
             \x20 off. Either way, rewriting the recording uncompressed works:\n\
             \x20   mcap compress --compression none <in.mcap> -o <out.mcap>"
        ),
        tf_tree_ingest::IngestError::ClockResetSplitUnsupported => anyhow::anyhow!(
            "{text}\n\
             \x20 docs/PHASE5.md §3.2 records --on-clock-reset=split as deliberately\n\
             \x20 refused, with the argument. Cut the recording at the stamp `halt`\n\
             \x20 reports and ingest each part."
        ),
        tf_tree_ingest::IngestError::Rosbag2Sqlite => anyhow::anyhow!(
            "{text}\n\
             \x20 rosbag2's sqlite3 storage is not read by this build (docs/PHASE5.md\n\
             \x20 §3.3: every pure-Rust SQLite reader is either unlicensed or a header\n\
             \x20 parser, and the C ones are ruled out by docs/PHASE2.md §2).\n\
             \x20 Convert it once, with ROS 2's own tool:\n\
             \x20   ros2 bag convert -i <bag.db3> -o <out.yaml>   # storage_id: mcap"
        ),
        _ => anyhow::anyhow!("{text}"),
    }
}

/// The live-arena flags, or `()` on a build without `shm`; an alias so commands
/// need no `#[cfg]` duplicates.
#[cfg(all(feature = "shm", target_os = "linux"))]
type Live<'a> = &'a attach::AttachArgs;
#[cfg(not(all(feature = "shm", target_os = "linux")))]
type Live<'a> = &'a ();

/// Where a command's tree came from, which is the one thing the output has to be
/// honest about.
enum Source {
    /// The in-process benchmark fixture, with its recorded push stream.
    Fixture(Observations),
    /// An MCAP recording ingested in-process (`doctor --from-bag`), with its
    /// transforms in log order as [`Observations`] (`docs/PHASE5.md` §4.1;
    /// [`checks::PushStream::RingsAtRest`]).
    Bag(Observations),
    /// A live arena somebody else is publishing into.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    Live,
    /// A frozen `.tft` index (`doctor --from-file`), mapped read-only.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    Frozen,
}

impl Source {
    fn banner(&self) -> &'static str {
        match self {
            Source::Fixture(_) => "in-process fixture",
            Source::Bag(_) => "MCAP recording",
            #[cfg(all(feature = "shm", target_os = "linux"))]
            Source::Live => "live arena",
            #[cfg(all(feature = "shm", target_os = "linux"))]
            Source::Frozen => "frozen .tft index",
        }
    }

    /// How this source's push stream was obtained; decides whether `TFT001`,
    /// `TFT011`'s Phase 1 half, `TFT018` and `TFT019` have evidence.
    fn stream(&self) -> checks::PushStream {
        match self {
            Source::Fixture(_) => checks::PushStream::Observed,
            Source::Bag(_) => checks::PushStream::Recorded,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            Source::Live => checks::PushStream::RingsUnderWriter,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            Source::Frozen => checks::PushStream::RingsAtRest,
        }
    }

    /// What kind of participant table this source carries; decides whether
    /// `TFT014` has evidence.
    fn slot_table(&self) -> checks::SlotTable {
        match self {
            Source::Fixture(_) | Source::Bag(_) => checks::SlotTable::Current,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            Source::Live => checks::SlotTable::Current,
            #[cfg(all(feature = "shm", target_os = "linux"))]
            Source::Frozen => checks::SlotTable::Image,
        }
    }
}

/// Build the fixture, or attach. Trees and handles are `Box::leak`ed: the process
/// inspects once and exits.
fn source(live: Live<'_>) -> Result<(&'static Tree, Source)> {
    #[cfg(all(feature = "shm", target_os = "linux"))]
    if live.attach {
        let tree: &'static Tree = Box::leak(Box::new(live.open()?));
        return Ok((tree, Source::Live));
    }
    let _ = live;
    let tree: &'static Tree = Box::leak(Box::new(fixture::build_tree()?));
    let (writers, samples) = fixture::spin_up(tree)?;
    // Leaked so the claims stay held while the snapshot is taken.
    core::mem::forget(writers);
    Ok((tree, Source::Fixture(Observations::from_samples(samples))))
}

/// `doctor`'s sources: those of [`source`] plus the two recording ones; the tree is leaked.
fn doctor_source(
    live: Live<'_>,
    from_bag: Option<&std::path::Path>,
    from_file: Option<&std::path::Path>,
    ingest: &IngestArgs,
) -> Result<(&'static Tree, Source)> {
    let _ = from_file;
    // One arena per run (see `from_bag`'s doc for why not `conflicts_with`).
    #[cfg(all(feature = "shm", target_os = "linux"))]
    if live.attach {
        let other = if from_bag.is_some() {
            "--from-bag"
        } else if from_file.is_some() {
            "--from-file"
        } else {
            ""
        };
        anyhow::ensure!(
            other.is_empty(),
            "--attach and {other} name different arenas and doctor reports on one.\n\x20 Drop \
             --attach to diagnose the file, or drop {other} to diagnose the live arena."
        );
    }
    // Refuse a flag this invocation will ignore.
    if from_bag.is_none() {
        let set = ingest.flags_set();
        anyhow::ensure!(
            set.is_empty(),
            "{} {} only meaningful with --from-bag: {} reads a recording and nothing else \
             here does.\n\x20 Drop the flag, or add --from-bag <recording.mcap>.",
            set.join(", "),
            if set.len() == 1 { "is" } else { "are" },
            if set.len() == 1 { "it" } else { "they" },
        );
    }
    if let Some(bag) = from_bag {
        let opts = ingest.to_options()?;
        let ingested = recording::open_bag(bag, &opts)?;
        // Stderr, always: `--json` keeps stdout a parseable `tf_tree.doctor/1` document.
        eprint!("{}", ingested.report.summary());
        let tree: &'static Tree = Box::leak(Box::new(ingested.tree));
        let snap = Snapshot::capture(tree);
        let obs = recording::arrival_observations(
            bag,
            &opts,
            tree,
            &snap,
            ingest.max_memory.saturating_mul(1024 * 1024),
        )?;
        return Ok((tree, Source::Bag(obs)));
    }
    #[cfg(all(feature = "shm", target_os = "linux"))]
    if let Some(path) = from_file {
        let tree = Tree::open_frozen(path).map_err(|e| {
            anyhow::anyhow!(
                "{}: {e}\n\x20 If this is a recording rather than a frozen index, \
                 use --from-bag.\n\x20 If it is a .tft this build cannot read, re-freeze it: \
                 a .tft is a cache, not an archive (docs/PHASE5.md §2.4).",
                path.display()
            )
        })?;
        return Ok((Box::leak(Box::new(tree)), Source::Frozen));
    }
    source(live)
}

/// The push stream a command's checks run against; a live arena has none, so it
/// is reconstructed from the rings. Taken out of the `Source`, not cloned.
fn observations(tree: &Tree, src: &mut Source) -> Observations {
    // Used only by the arms that replay from the rings (need `shm`).
    let _ = tree;
    match src {
        Source::Fixture(obs) | Source::Bag(obs) => core::mem::take(obs),
        #[cfg(all(feature = "shm", target_os = "linux"))]
        Source::Live | Source::Frozen => Observations::from_arena(tree, &Snapshot::capture(tree)),
    }
}

/// `tf_tree tree` — render the topology. The tree comes from [`source`].
fn cmd_tree(live: Live<'_>) -> Result<()> {
    let (tree, mut src) = source(live)?;
    let obs = observations(tree, &mut src);
    let snap = Snapshot::capture(tree);

    println!("tf_tree topology ({})", src.banner());
    println!(
        "  {} frames, {} edges, arena {} KiB\n",
        snap.frames.len(),
        snap.edges.len(),
        tree.arena_size_bytes() / 1024
    );

    // The reference clock (`Clock::decide`, shared with `doctor` and `top`), not `fixture::NOW_NS`.
    let clock = checks::Clock::decide(&checks::newest_stamps(&snap), unix_nanos_now());
    println!(
        "  {:<22} {:>5} {:<8} {:>9} {:>12} {:>10} {:>8}",
        "frame", "depth", "kind", "rate(Hz)", "occupancy", "age(ms)", "writer"
    );
    println!("  age(ms) is measured against the {}\n", clock.label());
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
                    .map(|s| format!("{}", (clock.nanos() - s).max(0) / 1_000_000))
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
    // A live arena is anchored to its publishers' stamps, not the fixture's `NOW_NS`.
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

/// The newest stamp on any edge, "now" for this arena; `None` for an arena with
/// no samples.
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

/// `tf_tree doctor` — the `docs/PHASE5.md` §6 catalogue; `--exit-code` is opt-in.
#[allow(clippy::too_many_arguments)]
fn cmd_doctor(
    live: Live<'_>,
    json: bool,
    exit_code: Option<ExitSeverity>,
    suppress: &[String],
    from_bag: Option<&std::path::Path>,
    from_file: Option<&std::path::Path>,
    ingest: &IngestArgs,
) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    for s in suppress {
        let id = catalogue::Tft::parse(s).ok_or_else(|| {
            anyhow::anyhow!("unknown check id {s:?} — expected one of TFT001..TFT019")
        })?;
        ids.insert(id);
    }

    let (tree, mut src) = doctor_source(live, from_bag, from_file, ingest)?;
    let obs = observations(tree, &mut src);
    // The arena first, the lock file second (`docs/decisions/0028` piece 2 constraint 3).
    #[allow(unused_mut)]
    let mut snap = Snapshot::capture(tree);
    // `TFT014`'s participant half needs the lock file (`0028` plan step 6); only `--attach` has one.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    if live.attach {
        if let Some(lock) = live
            .rendezvous()
            .ok()
            .filter(|rv| rv.lock_path().exists())
            .and_then(|rv| tf_tree_ipc::LockFile::open(rv.lock_path()).ok())
        {
            snap.probe_lock_facts(|row| slot_facts(&lock, row));
        }
        // No `else`: without the file every slot keeps `LockByte::Unknown`.
    }
    let stats = checks::collect_edge_stats(tree, &snap);
    let clock = checks::Clock::decide(&checks::newest_stamps(&snap), unix_nanos_now());
    let clock_step = checks::ClockStepEvidence::capture(&snap, &obs);

    let inputs = checks::Inputs {
        snap: &snap,
        obs: &obs,
        stats: &stats,
        host: host_facts(),
        clock,
        arena_bytes: tree.arena_size_bytes() as u64,
        occupancy: checks::occupancy_of(tree),
        clock_step: &clock_step,
        stream: src.stream(),
        slots: src.slot_table(),
        counters: tf_tree::counters_compiled_in(),
    };
    let report = checks::run(&inputs, &ids);

    let meta = catalogue::Meta {
        source: src.banner(),
        format_version: tf_tree::arena_format_version(),
        layout_hash: tf_tree::arena_layout_hash(),
        instance: instance_uuid(tree, &src),
        runtime_dir: resolved_runtime_dir(),
        frames: snap.frames.len(),
        edges: snap.edges.len(),
        generated_unix_nanos: unix_nanos_now(),
        now_nanos: clock.nanos(),
        clock_source: clock.label(),
        counters_compiled_in: tf_tree::counters_compiled_in(),
        rings: sizing::Rings::from_edges(snap.edges.iter().map(|e| (e.capacity, e.occupancy()))),
        notes: evidence_notes(
            src.stream(),
            &snap,
            &obs,
            inputs.clock,
            &clock_step,
            checks::no_counter_evidence(inputs.counters, inputs.stats),
            report.outcome(catalogue::Tft::Tft009),
        ),
    };

    if json {
        print!("{}", catalogue::render_json(&report, &meta));
    } else {
        print!("{}", catalogue::render_human(&report, &meta));
    }

    let gate_fired = match exit_code {
        None => false,
        Some(ExitSeverity::Error) => report.has_error(),
        // `!is_healthy()`, not `count_at(Warn) > 0`: `warn` is warn-and-above.
        Some(ExitSeverity::Warn) => !report.is_healthy(),
    };
    if gate_fired {
        std::process::exit(1);
    }
    Ok(())
}

/// Disclosures for a check that ran with one of its evidence sources missing.
///
/// `TFT011`: the counters half ([`checks::no_counter_evidence`]) and the arrival
/// delay half ([`checks::PushStream::no_arrival_delays`]); when both are blind
/// the skip reason carries both. `TFT009` takes the check's own outcome
/// (`checks::silence_coverage_note`). `TFT017`: every dynamic edge unclaimed.
/// `TFT015`: unconditional. `TFT007`, `TFT019`: only when the check covered some
/// edges and not others.
fn evidence_notes(
    stream: checks::PushStream,
    snap: &Snapshot,
    obs: &Observations,
    clock: checks::Clock,
    clock_step: &checks::ClockStepEvidence,
    counter_evidence: Option<&'static str>,
    tft009: Option<&catalogue::CheckOutcome>,
) -> Vec<String> {
    let mut notes = vec![checks::PARTICIPANT_OCCUPANCY_NOTE.to_owned()];
    notes.extend(checks::rate_coverage_note(snap, obs, clock, stream));
    notes.extend(checks::clock_offset_note(snap, stream, clock));
    notes.extend(clock_step.coverage_note(stream));
    notes.extend(tft009.and_then(|o| checks::silence_coverage_note(o, clock, stream)));
    notes.extend(checks::stopped_publisher_note(obs, clock, stream));
    match (counter_evidence, stream.no_arrival_delays()) {
        (Some(_), Some(_)) => {}
        (None, Some(why)) => notes.push(format!(
            "TFT011 ran on its counter evidence only — its capacity-vs-latency half cannot \
             fire, because {why}"
        )),
        (Some(why), None) => notes.push(format!(
            "TFT011 ran on its capacity-vs-latency evidence only — its counter half cannot \
             fire, because {why}"
        )),
        (None, None) => {}
    }
    notes.extend(unclaimed_coverage_note(snap));
    notes
}

/// `TFT017`'s disclosure: it fired on **every** dynamic edge ([`doctor::check_unclaimed_dynamic`]).
fn unclaimed_coverage_note(snap: &Snapshot) -> Option<String> {
    let dynamic = snap
        .edges
        .iter()
        .filter(|e| e.kind == tf_tree::unstable::EdgeKind::Dynamic)
        .count();
    if dynamic == 0 || doctor::check_unclaimed_dynamic(snap).len() != dynamic {
        return None;
    }
    Some(format!(
        "TFT017 fired on all {dynamic} dynamic edge(s), so it names this arena rather than any \
         edge in it: an arena nobody is writing to — one built from a recording (--from-bag), a \
         frozen .tft (--from-file), or a fleet in which every publisher has stopped — reports \
         every dynamic edge unclaimed. Only the last of those three is a fault"
    ))
}

/// The system clock as nanoseconds since the Unix epoch, saturating on a
/// pre-1970 clock. `pub(crate)` for `top` (`checks::Clock::decide`).
pub(crate) fn unix_nanos_now() -> i64 {
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

/// The rendezvous runtime directory this host resolves to, with its source
/// (`docs/PHASE2.md` §15); `None` rather than failing.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn resolved_runtime_dir() -> Option<String> {
    let d = tf_tree_ipc::RuntimeDir::resolve().ok()?;
    Some(format!("{} ({:?})", d.path().display(), d.source()))
}

/// See [`resolved_runtime_dir`]; without `shm` there is nothing to resolve.
#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn resolved_runtime_dir() -> Option<String> {
    None
}

fn instance_uuid(tree: &Tree, src: &Source) -> Option<String> {
    let _ = (tree, src);
    #[cfg(all(feature = "shm", target_os = "linux"))]
    if matches!(src, Source::Live) {
        return Some(hex16(tree.instance_uuid()));
    }
    None
}

/// `tf_tree top` — `docs/PHASE5.md` §7's live view.
///
/// `--rw` is refused, not ignored (D18).
fn cmd_top(
    live: Live<'_>,
    interval_ms: u64,
    iterations: u64,
    edge: Option<String>,
    color: Option<bool>,
    web: Option<std::net::SocketAddr>,
) -> Result<()> {
    // A floor: `--interval 0` would spin a core over a robot's arena.
    anyhow::ensure!(
        interval_ms >= 50,
        "--interval {interval_ms} is below the 50 ms floor: a faster redraw perturbs the arena it \
         is reading and cannot be read by a human anyway"
    );

    #[cfg(all(feature = "shm", target_os = "linux"))]
    anyhow::ensure!(
        !live.rw,
        "`top` is a read-only observer (D18) and refuses --rw; drop the flag"
    );

    let (tree, src) = source(live)?;

    #[cfg(all(feature = "shm", target_os = "linux"))]
    let merge: Box<dyn Fn(&mut top::Capture) + Sync> = if live.attach {
        match live
            .rendezvous()
            .ok()
            .filter(|rv| rv.lock_path().exists())
            .and_then(|rv| tf_tree_ipc::LockFile::open(rv.lock_path()).ok())
        {
            // Not an error: a missing lock file costs the `mode`/`comm` columns.
            None => Box::new(|_: &mut top::Capture| {}),
            Some(lock) => Box::new(move |cap: &mut top::Capture| {
                let mut rows = Vec::new();
                for slot in 0..tf_tree_ipc::MAX_PARTICIPANTS {
                    let held = lock
                        .probe_participant(slot)
                        .map(|p| p.held)
                        .unwrap_or(false);
                    let id = lock.read_identity(slot).ok().flatten();
                    if !held && id.is_none() {
                        continue;
                    }
                    let (pid, mode, comm) = match id {
                        None => (0, "?", String::new()),
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
                    rows.push((slot, pid, mode, comm, held));
                }
                cap.merge_lock_rows(&rows);
            }),
        }
    } else {
        Box::new(|_: &mut top::Capture| {})
    };
    #[cfg(not(all(feature = "shm", target_os = "linux")))]
    let merge: Box<dyn Fn(&mut top::Capture) + Sync> = Box::new(|_: &mut top::Capture| {});

    let interval = core::time::Duration::from_millis(interval_ms);
    if let Some(addr) = web {
        return cmd_top_web(
            tree,
            src.banner(),
            interval,
            iterations,
            edge,
            addr,
            &*merge,
        );
    }

    top::run(
        tree,
        src.banner(),
        interval,
        iterations,
        edge,
        color,
        &*merge,
    )
}

/// `tf_tree top --web` — the same sampler, served instead of drawn.
///
/// One [`top::Sampler`] holds per-tick state, so two polling tabs would each see
/// half the samples: a poll sooner than `interval` is answered from the previous
/// document. The closure is serialised by a `Mutex` (poison recovered).
fn cmd_top_web(
    tree: &Tree,
    source: &'static str,
    interval: core::time::Duration,
    iterations: u64,
    edge: Option<String>,
    addr: std::net::SocketAddr,
    merge: &(dyn Fn(&mut top::Capture) + Sync),
) -> Result<()> {
    let (listener, bound) = web::bind(addr)?;
    let mut sampler = top::Sampler::new();
    let mut last = std::time::Instant::now();
    let mut cached: Option<(std::time::Instant, String)> = None;
    let selected_at_start = edge;

    let sample = std::sync::Mutex::new(move || {
        let now = std::time::Instant::now();
        if let Some((at, doc)) = &cached {
            if now.duration_since(*at) < interval {
                return doc.clone();
            }
        }
        let mut capture = top::Capture::from_tree(tree, source);
        merge(&mut capture);
        let selected = selected_at_start
            .as_deref()
            .and_then(|needle| top::select_edge(&capture.edges, needle))
            .map(|e| e.id);
        let t = sampler.observe(capture, now.duration_since(last));
        last = now;
        let doc = web::tick_json(&t, interval, selected);
        cached = Some((now, doc.clone()));
        doc
    });
    let tick = || {
        let mut guard = sample
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard()
    };

    web::serve(&listener, bound, iterations, &tick)
}

/// `tf_tree bench [--gate]`.
///
/// Runs the correctness half in-process (naive-Rust differential, `1e-12`); the
/// perf gates are `cargo xtask bench-gate` and `cargo bench`.
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

/// `tf_tree freeze --from-live` — `docs/PHASE5.md` §2, and §5.6's capture.
///
/// Attaches **read-only** (D18) and copies the arena: a smear, not a transaction
/// (see `tf_tree_arena::write_frozen`). `source_digest` is all-zero here; §2.3.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn cmd_freeze(
    live: Live<'_>,
    from_live: bool,
    from_bag: Option<&std::path::Path>,
    out: &std::path::Path,
    report: Option<&std::path::Path>,
    ingest: &IngestArgs,
) -> Result<()> {
    if let Some(bag) = from_bag {
        return cmd_freeze_bag(bag, out, report, ingest);
    }
    anyhow::ensure!(
        from_live,
        "`freeze` needs a source; pass `--from-live` or `--from-bag <PATH>`"
    );
    // As `doctor`: the §3 knobs are dead under `--from-live` (`--report` excepted).
    let set = ingest.flags_set();
    anyhow::ensure!(
        set.is_empty(),
        "{} {} only meaningful with --from-bag: --from-live copies a live arena and reads no \
         recording.\n\x20 Drop the flag, or freeze the recording instead with --from-bag.",
        set.join(", "),
        if set.len() == 1 { "is" } else { "are" },
    );
    let tree = live.open()?;
    // Saturating: `as i64` would wrap past 2^63 ns.
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    let header = tree
        .freeze_to(out, None, [0; 32], created)
        .map_err(|e| anyhow::anyhow!("could not freeze to {}: {e}", out.display()))?;
    println!(
        "froze {} bytes of arena to {}",
        header.arena_size,
        out.display()
    );
    println!(
        "  arena at file offset {} ({} MiB aligned), manifest {} bytes at {}",
        header.arena_off,
        tf_tree_arena_align_mib(),
        header.manifest_len,
        header.manifest_off
    );
    println!("  snapshot is not atomic: publishers were free to write during the copy");
    Ok(())
}

/// `tf_tree freeze --from-bag` — `docs/PHASE5.md` §3 into §2's container.
///
/// Unlike `--from-live`, this is an exact snapshot of the recording. The report
/// is written alongside the `.tft` by default (§3.2), under a derived name.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn cmd_freeze_bag(
    bag: &std::path::Path,
    out: &std::path::Path,
    report: Option<&std::path::Path>,
    args: &IngestArgs,
) -> Result<()> {
    let opts = args.to_options()?;
    let mut frames = tf_tree_ingest::Frames::default();
    let (ingested, header) = tf_tree_ingest::tft::freeze_bag(bag, out, &opts, &mut frames)
        .map_err(|e| ingest_err(e, &frames))?;
    print!("{}", ingested.report.summary());
    println!(
        "froze {} bytes of arena to {}",
        header.arena_size,
        out.display()
    );
    println!(
        "  arena at file offset {} ({} MiB aligned), manifest {} bytes at {}",
        header.arena_off,
        tf_tree_arena_align_mib(),
        header.manifest_len,
        header.manifest_off
    );
    let report_path = match report {
        Some(p) => p.to_path_buf(),
        None => {
            let mut name = out.as_os_str().to_os_string();
            name.push(".ingest.json");
            std::path::PathBuf::from(name)
        }
    };
    std::fs::write(&report_path, ingested.report.to_json())
        .map_err(|e| anyhow::anyhow!("could not write {}: {e}", report_path.display()))?;
    println!("  ingest report written to {}", report_path.display());
    Ok(())
}

/// The `.tft` arena alignment, in MiB, for the message above.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn tf_tree_arena_align_mib() -> u64 {
    tf_tree::ARENA_FILE_ALIGN / (1024 * 1024)
}

/// The lock file's facts about **one** captured participant slot, for `TFT014`
/// (`docs/decisions/0028` plan step 6).
///
/// It takes the already-captured [`doctor::ParticipantInfo`], so the `state` word
/// is read before the byte (`0028` piece 2 constraint 3). Three-valued: a failed
/// `probe_participant` is not "free" (§6.2).
#[cfg(all(feature = "shm", target_os = "linux"))]
fn slot_facts(lock: &tf_tree_ipc::LockFile, row: &doctor::ParticipantInfo) -> doctor::SlotFacts {
    let byte = match lock.probe_participant(row.slot) {
        Ok(p) if p.held => doctor::LockByte::Held,
        Ok(_) => doctor::LockByte::Free,
        Err(_) => doctor::LockByte::Unknown,
    };
    let id = lock.read_identity(row.slot).ok().flatten();
    doctor::SlotFacts {
        byte,
        recorded: recorded_process(id.as_ref()),
        recorded_pid: id.map(|i| i.pid),
    }
}

/// What `/proc` says about the process a lock-file identity record names.
///
/// A diagnostic inference, never a protocol decision (`docs/PHASE2.md` §5.1); not
/// `Identity::matches_running_process`, which reads every failure as gone
/// (`docs/decisions/0028`). The verdict is [`recorded_given`].
#[cfg(all(feature = "shm", target_os = "linux"))]
fn recorded_process(id: Option<&tf_tree_ipc::Identity>) -> doctor::RecordedProcess {
    let Some(id) = id else {
        return doctor::RecordedProcess::Unknown;
    };
    recorded_given(
        id.start_time,
        tf_tree_ipc::start_time_of(id.pid),
        // Would this host show us an entry? Test our own.
        tf_tree_ipc::self_start_time().is_ok(),
        id.pid_ns_inode,
        // The observer's own namespace (`docs/decisions/0033` *Decision* 2); zero on a failed read.
        tf_tree_ipc::self_pid_ns_inode().unwrap_or(0),
        // Whether this `/proc` is this process's pid namespace; `true` on a failed read.
        tf_tree_ipc::proc_self_pid().is_none_or(|p| p == std::process::id()),
    )
}

/// Turn a recorded `start_time`, the `/proc` read and three host facts into a
/// [`doctor::RecordedProcess`]; `alive_given` (`crates/tf_tree/src/tree.rs`) made
/// three-valued with the same bias: never call a live process gone.
///
/// The two guards precede the match (`docs/decisions/0033` *Decision* 3):
/// `recorded_pid_ns` against `observer_pid_ns` (zero on either side is unknown
/// namespace), and `proc_is_ours`. Both yield `Unknown`.
///
/// * stored `start_time` of zero: not compared.
/// * start time matches: running; differs: gone.
/// * `ENOENT` on a host that would have shown an entry: gone, the only proof of death.
/// * `ENOENT` on a host that says so about everybody, or any other failure
///   (`EACCES` from `hidepid`, `EMFILE`, ...): unknown.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn recorded_given(
    stored_start_time: u64,
    probe: Result<u64, tf_tree_ipc::ProcError>,
    proc_answers: bool,
    recorded_pid_ns: u64,
    observer_pid_ns: u64,
    proc_is_ours: bool,
) -> doctor::RecordedProcess {
    use doctor::RecordedProcess as R;
    if stored_start_time == 0 {
        return R::Unknown;
    }
    if recorded_pid_ns != 0 && observer_pid_ns != 0 && recorded_pid_ns != observer_pid_ns {
        return R::Unknown;
    }
    if !proc_is_ours {
        return R::Unknown;
    }
    match probe {
        Ok(start) if start == stored_start_time => R::Running,
        Ok(_) => R::Gone,
        Err(tf_tree_ipc::ProcError::Unreadable { raw_os_error, .. })
            if std::io::Error::from_raw_os_error(raw_os_error).kind()
                == std::io::ErrorKind::NotFound =>
        {
            if proc_answers {
                R::Gone
            } else {
                R::Unknown
            }
        }
        Err(_) => R::Unknown,
    }
}

/// Liveness is the kernel's answer (`F_OFD_GETLK` on the participant's byte), §5.1.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn cmd_participants(live: Live<'_>) -> Result<()> {
    let rv = live.rendezvous()?;
    let path = rv.lock_path();
    println!("tf_tree participants — {}", path.display());

    if !path.exists() {
        println!("  no lock file: nothing has ever attached to this domain/name");
        return Ok(());
    }

    use anyhow::Context as _;
    let lock =
        tf_tree_ipc::LockFile::open(path).with_context(|| format!("opening {}", path.display()))?;

    println!("  slot       pid  mode    state    comm");
    let mut live_count = 0;
    for slot in 0..tf_tree_ipc::MAX_PARTICIPANTS {
        let held = lock
            .probe_participant(slot)
            .map(|p| p.held)
            .unwrap_or(false);
        let id = lock.read_identity(slot).ok().flatten();
        // A byte held with no identity record: a participant mid-attach.
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
        // "stale": the byte is released but the record remains.
        let state = if held { "live" } else { "stale" };
        println!("  {slot:>4}  {pid:>8}  {mode:<6}  {state:<7}  {comm}");
    }
    if live_count == 0 {
        println!("  (no live participants)");
    }
    Ok(())
}

/// Observed publish rate (Hz) for an edge: [`doctor::observed_rate_hz`], shared
/// with `TFT007` so column and check cannot differ.
fn observed_rate_hz(obs: &Observations, edge: u32) -> Option<f64> {
    let samples: Vec<&fixture::PushSample> = obs.events.iter().filter(|s| s.edge == edge).collect();
    doctor::observed_rate_hz(&samples)
}

/// Print this build's arena format version and what a mismatch means
/// (`docs/PHASE5.md` §1.2). Reads no arena and takes no lock.
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
        println!("for Phase 6's spline region, so that it lands without a second");
        println!("break. A version-2 arena cannot be attached. The eight bytes");
        println!("that were reserved for covariance stay reserved and unused:");
        println!("docs/decisions/0009 descoped covariance, and tf_tree carries");
        println!("no uncertainty.");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A `TFT009` outcome that **ran**, the state the note tests assume.
    fn tft009_ran() -> catalogue::CheckOutcome {
        catalogue::CheckOutcome::ran(catalogue::Tft::Tft009, Vec::new())
    }

    /// One running writer in slot 0, the owner every claimed edge names.
    fn live_writer() -> Vec<doctor::ParticipantInfo> {
        vec![doctor::ParticipantInfo {
            slot: 0,
            state: doctor::SlotState::Live,
            pid: 4711,
            alive: true,
            byte: doctor::LockByte::Held,
            recorded: doctor::RecordedProcess::Running,
            recorded_pid: Some(4711),
        }]
    }

    /// The `TFT007` coverage note reaches `Meta.notes`.
    #[test]
    fn the_rate_coverage_note_reaches_the_report_metadata() {
        use doctor::{EdgeInfo, FrameInfo};
        use tf_tree::InterpPolicy;
        use tf_tree_bench::fixture::PushSample;

        let dyn_edge = |id: u32, parent: u32, child: u32, mhz: Option<u32>| EdgeInfo {
            id,
            parent,
            child,
            kind: EdgeKind::Dynamic,
            capacity: 512,
            interp: InterpPolicy::ScLerp,
            domain: 0,
            head: 100,
            claimed: true,
            claiming: false,
            owner_slot: Some(0),
            owner_pid: 4711,
            newest_stamp: Some(1_000_000_000),
            clock_offset_nanos: None,
            nominal_rate_mhz: mhz,
        };
        let frame = |id: u32, name: &str, parent: u32, depth: u16| FrameInfo {
            id,
            name: name.to_owned(),
            parent,
            depth,
            edge_of_child: 0,
        };
        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base_link", 2, 2),
            ],
            edges: vec![dyn_edge(1, 1, 2, Some(20_000)), dyn_edge(2, 2, 3, None)],
            participants: live_writer(),
        };
        let obs = Observations::from_samples(
            (0..12i64)
                .map(|k| PushSample {
                    edge: 1,
                    writer_pid: 4711,
                    stamp_ns: k * 50_000_000,
                    arrival_delay_ns: 0,
                })
                .collect(),
        );

        let notes = evidence_notes(
            checks::PushStream::Observed,
            &snap,
            &obs,
            checks::Clock::Wall(0),
            &checks::ClockStepEvidence::capture(&snap, &obs),
            None,
            Some(&tft009_ran()),
        );
        let note = notes.iter().find(|n| n.starts_with("TFT007")).expect(
            "no coverage note in Meta.notes: a partial TFT007 pass would read as a full \
                     one",
        );
        assert!(
            note.contains("compared 1 of 2"),
            "the note must state the coverage it reached the operator with: {note}"
        );
    }

    /// The `TFT009` silence note reaches `Meta.notes`, and is silent when the half ran.
    #[test]
    fn the_silence_coverage_note_reaches_the_report_metadata() {
        use doctor::{EdgeInfo, FrameInfo};
        use tf_tree::InterpPolicy;

        let snap = Snapshot {
            frames: vec![
                FrameInfo {
                    id: 1,
                    name: "map".to_owned(),
                    parent: 0,
                    depth: 0,
                    edge_of_child: 0,
                },
                FrameInfo {
                    id: 2,
                    name: "odom".to_owned(),
                    parent: 1,
                    depth: 1,
                    edge_of_child: 0,
                },
            ],
            edges: vec![EdgeInfo {
                id: 1,
                parent: 1,
                child: 2,
                kind: EdgeKind::Dynamic,
                capacity: 512,
                interp: InterpPolicy::ScLerp,
                domain: 0,
                head: 100,
                claimed: true,
                claiming: false,
                owner_slot: Some(0),
                owner_pid: 4711,
                newest_stamp: Some(1_000_000_000),
                clock_offset_nanos: None,
                nominal_rate_mhz: Some(20_000),
            }],
            participants: live_writer(),
        };
        let obs = Observations::new();
        let step = checks::ClockStepEvidence::capture(&snap, &obs);

        let notes = evidence_notes(
            checks::PushStream::RingsAtRest,
            &snap,
            &obs,
            checks::Clock::Wall(0),
            &step,
            None,
            Some(&tft009_ran()),
        );
        let note = notes
            .iter()
            .find(|n| n.starts_with("TFT009"))
            .expect("no TFT009 silence note: a half-run would read as a full one");
        assert!(
            note.contains("nothing is writing"),
            "the note must say which of the two reasons applied: {note}"
        );

        let notes = evidence_notes(
            checks::PushStream::RingsUnderWriter,
            &snap,
            &obs,
            checks::Clock::Wall(0),
            &step,
            None,
            Some(&tft009_ran()),
        );
        assert!(
            !notes.iter().any(|n| n.starts_with("TFT009")),
            "the half ran; a note about coverage it did reach is noise: {notes:?}"
        );

        // With `TFT009` `not run`, the note must not claim gaps were measured.
        let notes = evidence_notes(
            checks::PushStream::RingsAtRest,
            &snap,
            &obs,
            checks::Clock::Wall(0),
            &step,
            None,
            Some(&catalogue::CheckOutcome::skipped(
                catalogue::Tft::Tft009,
                "no edge in this arena has a retained inter-arrival distribution",
            )),
        );
        assert!(
            !notes.iter().any(|n| n.starts_with("TFT009")),
            "TFT009 did not run; a note describing half of its work is the report \
             contradicting itself: {notes:?}"
        );
    }

    /// The stopped-publisher note reaches `Meta.notes`, and is silent when nothing was withheld.
    #[test]
    fn the_stopped_publisher_note_reaches_the_report_metadata() {
        use doctor::{EdgeInfo, FrameInfo};
        use tf_tree::InterpPolicy;
        use tf_tree_bench::fixture::PushSample;

        const MS: i64 = 1_000_000;
        let snap = Snapshot {
            frames: vec![
                FrameInfo {
                    id: 1,
                    name: "map".to_owned(),
                    parent: 0,
                    depth: 0,
                    edge_of_child: 0,
                },
                FrameInfo {
                    id: 2,
                    name: "odom".to_owned(),
                    parent: 1,
                    depth: 1,
                    edge_of_child: 0,
                },
            ],
            edges: vec![EdgeInfo {
                id: 1,
                parent: 1,
                child: 2,
                kind: EdgeKind::Dynamic,
                capacity: 512,
                interp: InterpPolicy::ScLerp,
                domain: 0,
                head: 40,
                claimed: true,
                claiming: false,
                owner_slot: Some(0),
                owner_pid: 4711,
                newest_stamp: Some(390 * MS),
                clock_offset_nanos: None,
                nominal_rate_mhz: Some(100_000),
            }],
            participants: live_writer(),
        };
        let obs = Observations::from_samples(
            (0..40i64)
                .map(|k| PushSample {
                    edge: 1,
                    writer_pid: 4711,
                    stamp_ns: k * 10 * MS,
                    arrival_delay_ns: 0,
                })
                .collect(),
        );
        let step = checks::ClockStepEvidence::capture(&snap, &obs);
        let now = checks::Clock::Wall(390 * MS + 8_000 * MS);

        let notes = evidence_notes(
            checks::PushStream::RingsUnderWriter,
            &snap,
            &obs,
            now,
            &step,
            None,
            Some(&tft009_ran()),
        );
        let note = notes
            .iter()
            .find(|n| n.contains("withheld judgement"))
            .expect("no stopped-publisher note: two silent skips with no named subject");
        assert!(note.contains("1 edge(s)"), "{note}");

        let notes = evidence_notes(
            checks::PushStream::RingsAtRest,
            &snap,
            &obs,
            now,
            &step,
            None,
            Some(&tft009_ran()),
        );
        assert!(
            !notes.iter().any(|n| n.contains("withheld judgement")),
            "nothing was withheld on a source at rest: {notes:?}"
        );
    }

    /// The `TFT019` coverage note reaches `Meta.notes`, and is silent on a live arena.
    #[test]
    fn the_clock_step_coverage_note_reaches_the_report_metadata() {
        use doctor::{EdgeInfo, FrameInfo};
        use tf_tree::InterpPolicy;
        use tf_tree_bench::fixture::PushSample;

        const MS: i64 = 1_000_000;
        let dyn_edge = |id: u32, parent: u32, child: u32, domain: u8| EdgeInfo {
            id,
            parent,
            child,
            kind: EdgeKind::Dynamic,
            capacity: 512,
            interp: InterpPolicy::ScLerp,
            domain,
            head: 100,
            claimed: true,
            claiming: false,
            owner_slot: Some(0),
            owner_pid: 4711,
            newest_stamp: Some(1_000_000_000),
            clock_offset_nanos: None,
            nominal_rate_mhz: None,
        };
        let frame = |id: u32, name: &str, parent: u32, depth: u16| FrameInfo {
            id,
            name: name.to_owned(),
            parent,
            depth,
            edge_of_child: 0,
        };
        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base_link", 2, 2),
            ],
            edges: vec![dyn_edge(1, 1, 2, 0), dyn_edge(2, 2, 3, 3)],
            participants: live_writer(),
        };
        // A clock step: the clock jumps 100 ms back and ten arrivals are rejected in one run.
        let back = |edge: u32| {
            let mut stamps: Vec<i64> = (0..10).map(|i| i * 10 * MS).collect();
            let last = stamps[stamps.len() - 1];
            stamps.extend((0..=10).map(|i| last - 100 * MS + i * 10 * MS));
            stamps.into_iter().map(move |stamp_ns| PushSample {
                edge,
                writer_pid: 4711,
                stamp_ns,
                arrival_delay_ns: 0,
            })
        };
        let obs = Observations::from_samples(back(1).chain(back(2)).collect());
        let ev = checks::ClockStepEvidence::capture(&snap, &obs);

        let notes = evidence_notes(
            checks::PushStream::Observed,
            &snap,
            &obs,
            checks::Clock::Wall(0),
            &ev,
            None,
            Some(&tft009_ran()),
        );
        let note = notes
            .iter()
            .find(|n| n.starts_with("TFT019"))
            .expect("no TFT019 coverage note in Meta.notes: a partially attributed run would read as a fully attributed one");
        assert!(
            note.contains("1 of 2") && note.contains("edge#2 tag 3"),
            "the note must name what it did not attribute, and its tag: {note}"
        );

        assert!(
            !evidence_notes(
                checks::PushStream::RingsUnderWriter,
                &snap,
                &obs,
                checks::Clock::Wall(0),
                &ev,
                None,
                Some(&tft009_ran()),
            )
            .iter()
            .any(|n| n.starts_with("TFT019")),
            "a live arena skipped TFT019 outright, so there is no coverage to disclose"
        );
    }

    /// The `CompressedChunk` remedy is a bare string (see `ingest_err`'s arm).
    #[test]
    fn the_compressed_chunk_error_carries_the_command_that_fixes_it() {
        let frames = tf_tree_ingest::Frames::default();
        let text = ingest_err(
            tf_tree_ingest::IngestError::CompressedChunk {
                codec: tf_tree_ingest::ChunkCodec::Zstd,
            },
            &frames,
        )
        .to_string();
        assert!(
            text.contains("mcap compress --compression none"),
            "the remedy must be a literal command a user can paste: {text}"
        );
        assert!(
            text.contains("PHASE2"),
            "and it must say why this build cannot simply decompress: {text}"
        );
        assert!(
            text.contains("zstd"),
            "and it must name the codec, so a user can tell which of the two \
             they have: {text}"
        );
    }

    /// The chunk bounds default to the library's and are reachable from the command line.
    #[test]
    fn the_chunk_bounds_default_to_the_librarys_and_are_settable() {
        let parse = |extra: &[&str]| -> tf_tree_ingest::IngestOptions {
            let mut args: Vec<&str> = vec!["tf_tree", "ingest", "--bag", "/nonexistent.mcap"];
            args.extend_from_slice(extra);
            match Cli::try_parse_from(args).expect("parse").command {
                Command::Ingest { opts, .. } => opts.to_options().expect("options"),
                _ => panic!("`ingest` did not parse as Command::Ingest"),
            }
        };

        let defaults = parse(&[]);
        let library = tf_tree_ingest::IngestOptions::default();
        assert_eq!(
            defaults.max_chunk_uncompressed_bytes, library.max_chunk_uncompressed_bytes,
            "the CLI's --max-chunk-size default has drifted from the library's"
        );
        assert_eq!(
            defaults.max_chunk_expansion_ratio, library.max_chunk_expansion_ratio,
            "the CLI's --max-chunk-expansion default has drifted from the library's"
        );

        let set = parse(&["--max-chunk-size", "7", "--max-chunk-expansion", "4"]);
        assert_eq!(set.max_chunk_uncompressed_bytes, 7 * 1024 * 1024);
        assert_eq!(set.max_chunk_expansion_ratio, 4);

        // A MiB count no multiplication can hold saturates rather than wrapping.
        let huge = parse(&["--max-chunk-size", &u64::MAX.to_string()]);
        assert_eq!(huge.max_chunk_uncompressed_bytes, u64::MAX);
    }

    /// A bad chunk's error points at the policy that would have kept the rest.
    #[test]
    fn a_bad_chunk_error_names_the_policy_that_would_recover() {
        let frames = tf_tree_ingest::Frames::default();
        let text = ingest_err(
            tf_tree_ingest::IngestError::BadChunk {
                chunk: 7,
                kind: tf_tree_ingest::BadChunkKind::Crc {
                    saved: 0xDEAD_BEEF,
                    calculated: 0x0BAD_F00D,
                },
            },
            &frames,
        )
        .to_string();
        assert!(
            text.contains("--on-bad-chunk=skip"),
            "the alternative policy must be named: {text}"
        );
        assert!(
            text.contains('7'),
            "and the chunk must be identified: {text}"
        );
    }

    /// A ceiling refusal names the ceiling's flag, never `--on-bad-chunk=skip`.
    #[test]
    fn a_ceiling_refusal_names_the_flag_that_raises_it() {
        let frames = tf_tree_ingest::Frames::default();
        let halt = ingest_err(
            tf_tree_ingest::IngestError::BadChunk {
                chunk: 3,
                kind: tf_tree_ingest::BadChunkKind::ImplausibleSize {
                    declared: 200 * 1024 * 1024,
                },
            },
            &frames,
        )
        .to_string();
        let skip = ingest_err(
            tf_tree_ingest::IngestError::AllChunksOverLimit { skipped: 812 },
            &frames,
        )
        .to_string();

        for (label, text) in [("halt", &halt), ("skip", &skip)] {
            assert!(
                text.contains("--max-chunk-size"),
                "{label}: the flag that raises the ceiling must be named: {text}"
            );
            assert!(
                text.contains("--max-chunk-expansion"),
                "{label}: and the other ceiling, which refuses the same chunk for a \
                 different reason: {text}"
            );
            assert!(
                !text.contains("--on-bad-chunk"),
                "{label}: skipping a sound chunk is not the remedy here: {text}"
            );
        }
        assert!(
            skip.contains("812") && skip.contains("not damaged"),
            "the whole-recording case must say the file is intact: {skip}"
        );
    }

    /// A window refusal reaches the same arm as a size refusal.
    #[test]
    fn a_window_refusal_reaches_the_ceiling_remedy_too() {
        let frames = tf_tree_ingest::Frames::default();
        let text = ingest_err(
            tf_tree_ingest::IngestError::BadChunk {
                chunk: 0,
                kind: tf_tree_ingest::BadChunkKind::ImplausibleWindow {
                    requested: 64 * 1024 * 1024,
                    ceiling: 8 * 1024 * 1024,
                },
            },
            &frames,
        )
        .to_string();
        assert!(text.contains("--max-chunk-size"), "{text}");
        assert!(!text.contains("--on-bad-chunk"), "{text}");
    }

    /// The `split` refusal cites the section that records it as unbuilt.
    #[test]
    fn the_split_refusal_cites_the_section_that_records_it() {
        let frames = tf_tree_ingest::Frames::default();
        let text = ingest_err(
            tf_tree_ingest::IngestError::ClockResetSplitUnsupported,
            &frames,
        )
        .to_string();
        assert!(
            text.contains("not implemented") && text.contains("PHASE5"),
            "{text}"
        );
    }

    /// `--web` with no value binds loopback; `--web ADDR` binds what was named.
    #[test]
    fn bare_web_binds_the_loopback_default() {
        let parse = |args: &[&str]| -> Option<std::net::SocketAddr> {
            match Cli::try_parse_from(args).expect("parse").command {
                Command::Top { web, .. } => web,
                _ => panic!("not `top`"),
            }
        };
        let addr = parse(&["tf_tree", "top", "--web"]).expect("--web alone must bind a default");
        assert_eq!(addr.to_string(), DEFAULT_WEB_ADDR_FOR_TEST);
        assert!(addr.ip().is_loopback(), "§7: loopback by default");
        assert_eq!(
            parse(&["tf_tree", "top", "--web", "127.0.0.1:0"])
                .expect("an explicit address")
                .port(),
            0
        );
        assert_eq!(parse(&["tf_tree", "top"]), None, "no --web, no server");
    }

    /// [`web::DEFAULT_ADDR`] spelled out: a constant compared to itself proves nothing.
    const DEFAULT_WEB_ADDR_FOR_TEST: &str = "127.0.0.1:8787";

    /// A snapshot of `n` dynamic edges, each with a live writer iff `claimed`.
    fn claim_snapshot(n: u32, claimed: bool) -> Snapshot {
        use doctor::{EdgeInfo, FrameInfo};
        use tf_tree::InterpPolicy;

        Snapshot {
            frames: (0..=n)
                .map(|i| FrameInfo {
                    id: i + 1,
                    name: format!("f{i}"),
                    parent: i,
                    depth: u16::try_from(i).unwrap(),
                    edge_of_child: 0,
                })
                .collect(),
            edges: (0..n)
                .map(|i| EdgeInfo {
                    id: i + 1,
                    parent: i + 1,
                    child: i + 2,
                    kind: EdgeKind::Dynamic,
                    capacity: 512,
                    interp: InterpPolicy::ScLerp,
                    domain: 0,
                    head: 100,
                    claimed,
                    claiming: false,
                    owner_slot: claimed.then_some(0),
                    owner_pid: if claimed { 4711 } else { 0 },
                    newest_stamp: Some(1_000_000_000),
                    clock_offset_nanos: None,
                    nominal_rate_mhz: None,
                })
                .collect(),
            participants: live_writer(),
        }
    }

    /// `TFT011` discloses exactly the blind half, and nothing when it skipped.
    #[test]
    fn the_tft011_disclosure_names_the_half_that_could_not_fire() {
        let snap = claim_snapshot(1, true);
        let obs = Observations::new();
        let ev = checks::ClockStepEvidence::capture(&snap, &obs);
        let tft011 = |notes: Vec<String>| {
            notes
                .into_iter()
                .find(|n| n.starts_with("TFT011"))
                .unwrap_or_default()
        };

        let note = tft011(evidence_notes(
            checks::PushStream::RingsUnderWriter,
            &snap,
            &obs,
            checks::Clock::Wall(0),
            &ev,
            None,
            Some(&tft009_ran()),
        ));
        assert!(
            note.contains("counter evidence only") && note.contains("no receipt time"),
            "{note}"
        );

        let note = tft011(evidence_notes(
            checks::PushStream::Observed,
            &snap,
            &obs,
            checks::Clock::Wall(0),
            &ev,
            Some("this arena has served no lookups"),
            Some(&tft009_ran()),
        ));
        assert!(
            note.contains("capacity-vs-latency evidence only")
                && note.contains("served no lookups"),
            "{note}"
        );

        assert_eq!(
            tft011(evidence_notes(
                checks::PushStream::Recorded,
                &snap,
                &obs,
                checks::Clock::Wall(0),
                &ev,
                Some("this arena has served no lookups"),
                Some(&tft009_ran()),
            )),
            "",
            "a note beside a `not run` line repeats the skip reason"
        );

        assert_eq!(
            tft011(evidence_notes(
                checks::PushStream::Observed,
                &snap,
                &obs,
                checks::Clock::Wall(0),
                &ev,
                None,
                Some(&tft009_ran()),
            )),
            ""
        );
    }

    /// `TFT017` firing on every dynamic edge is disclosed as a fact about the arena.
    #[test]
    fn a_wholly_unclaimed_arena_is_disclosed_as_an_arena_fact() {
        let note = unclaimed_coverage_note(&claim_snapshot(3, false))
            .expect("every dynamic edge unclaimed and nothing said so");
        assert!(
            note.contains("all 3 dynamic edge(s)") && note.contains("--from-bag"),
            "{note}"
        );

        let mut partial = claim_snapshot(3, false);
        partial.edges[0].claimed = true;
        partial.edges[0].owner_pid = 4711;
        assert_eq!(unclaimed_coverage_note(&partial), None);

        assert_eq!(unclaimed_coverage_note(&claim_snapshot(3, true)), None);
        assert_eq!(unclaimed_coverage_note(&claim_snapshot(0, false)), None);
    }

    /// An ingest flag `doctor` will ignore is named, one at a time.
    #[test]
    fn an_ingest_flag_left_at_its_default_is_not_reported_as_set() {
        assert_eq!(IngestArgs::default().flags_set(), Vec::<&str>::new());

        let a = IngestArgs {
            tf_prefix: Some("robot1".to_owned()),
            max_memory: 64,
            ..IngestArgs::default()
        };
        assert_eq!(a.flags_set(), vec!["--max-memory", "--tf-prefix"]);

        // `to_bits`, not `==`: a NaN horizon must differ from the default.
        let b = IngestArgs {
            future_horizon: f64::NAN,
            ..IngestArgs::default()
        };
        assert_eq!(b.flags_set(), vec!["--future-horizon"]);
        assert_eq!(IngestArgs::default().flags_set(), Vec::<&str>::new());
    }

    /// "Cannot tell" is not "dead": every `Unknown` arm is covered (`hidepid=2`, `EMFILE`).
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[test]
    fn a_proc_read_that_cannot_answer_is_never_read_as_death() {
        use doctor::RecordedProcess as R;
        use tf_tree_ipc::{ProcError, ProcParseError};

        const STORED: u64 = 4242;
        let unreadable = |errno: i32| {
            Err(ProcError::Unreadable {
                pid: 7,
                raw_os_error: errno,
            })
        };
        // `0033` facts held at "observer where the record was written".
        let here = |stored, probe, proc_answers| {
            recorded_given(stored, probe, proc_answers, HERE, HERE, true)
        };

        assert_eq!(here(STORED, Ok(STORED), true), R::Running);
        assert_eq!(
            here(STORED, Ok(STORED + 1), true),
            R::Gone,
            "a different start time on the same pid is a recycled pid"
        );
        assert_eq!(
            here(STORED, unreadable(2), true),
            R::Gone,
            "ENOENT on a host that would have shown us an entry is the one \
             proof of death"
        );

        assert_eq!(
            here(STORED, unreadable(2), false),
            R::Unknown,
            "ENOENT on a host that says that about everybody proves nothing"
        );
        assert_eq!(
            here(STORED, unreadable(13), true),
            R::Unknown,
            "EACCES is a hidepid mount, not a dead process"
        );
        assert_eq!(
            here(STORED, unreadable(24), true),
            R::Unknown,
            "EMFILE is this process being out of descriptors"
        );
        assert_eq!(
            here(
                STORED,
                Err(ProcError::Parse {
                    pid: 7,
                    cause: ProcParseError::NoClosingParen,
                }),
                true,
            ),
            R::Unknown,
            "a stat line we could not parse is not a death certificate"
        );

        assert_eq!(
            here(0, Ok(STORED), true),
            R::Unknown,
            "`of_self_best_effort`'s zero compares unequal to every real start \
             time; treating it as a mismatch reports a running process dead"
        );
    }

    /// The observer's own namespace inode; nonzero (zero is unknown).
    #[cfg(all(feature = "shm", target_os = "linux"))]
    const HERE: u64 = 4_026_531_836;
    /// A second, different namespace.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    const ELSEWHERE: u64 = 4_026_532_488;

    /// A pid from another PID namespace is not askable of this `/proc`
    /// (`docs/decisions/0033`, arms A-D; end-to-end `tests/attach.rs`'s
    /// `tft014_namespace_*`). Each guard is exercised with the other neutral.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[test]
    fn a_pid_from_another_namespace_is_not_a_pid_this_proc_can_answer_about() {
        use doctor::RecordedProcess as R;

        const STORED: u64 = 4242;
        let enoent = Err(tf_tree_ipc::ProcError::Unreadable {
            pid: 7,
            raw_os_error: 2,
        });

        // Arm A: a live participant one namespace away.
        assert_eq!(
            recorded_given(STORED, Ok(STORED + 1), true, ELSEWHERE, HERE, true),
            R::Unknown,
            "a recycled-pid verdict about a pid from another numbering is not \
             a verdict"
        );
        // Arm B: the recorded pid is not in this `/proc`.
        assert_eq!(
            recorded_given(STORED, enoent, true, ELSEWHERE, HERE, true),
            R::Unknown,
            "ENOENT about a pid this /proc does not number proves nothing"
        );
        // Arm C, the true positive: same `ENOENT` as B; only the namespace differs.
        assert_eq!(
            recorded_given(STORED, enoent, true, HERE, HERE, true),
            R::Gone,
            "a fork inheritor in the observer's own namespace is exactly what \
             TFT014 exists to report"
        );

        // Zero is unknown namespace and must not fire the guard.
        assert_eq!(
            recorded_given(STORED, enoent, true, 0, HERE, true),
            R::Gone,
            "a pre-0033 record keeps the behaviour it was written under"
        );
        assert_eq!(
            recorded_given(STORED, enoent, true, ELSEWHERE, 0, true),
            R::Gone,
            "an observer that could not read its own namespace degrades to \
             today's behaviour, not to a check that can never fire"
        );

        // Arm D: only `proc_is_ours` stands between `doctor` and a finding about its own slot.
        assert_eq!(
            recorded_given(STORED, enoent, true, HERE, HERE, false),
            R::Unknown,
            "if /proc is not this namespace's, no pid in the file is \
             resolvable here, including this process's own"
        );
        assert_eq!(
            recorded_given(STORED, Ok(STORED), true, HERE, HERE, false),
            R::Unknown,
            "not even a start time that matches, since it matched some other \
             namespace's pid 7"
        );
        assert_eq!(
            recorded_given(STORED, Ok(STORED), true, HERE, HERE, true),
            R::Running
        );
    }
}
