#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Implementation of the `tf_tree` diagnostics CLI.
//!
//! Printing to stdout/stderr is this crate's whole job, so the workspace
//! `print_stdout`/`print_stderr` lints are allowed here.
//!
//! # Four sources, and the fixture is only the one you get by naming none
//!
//! `tf_tree --help` names all four. The bare invocation builds the in-process
//! [`tf_tree_bench::fixture`]; `--attach` and `doctor --from-file` need Linux
//! and `--features shm`. Everything downstream reads a captured
//! [`doctor::Snapshot`], so a source is a capture, not a code path through the
//! catalogue; `checks::PushStream` says where sources differ (no arena can
//! answer `TFT018`).

// Compiles `README.md`'s `rust` fence, which no gate otherwise parses. Here
// rather than in `tf_tree` because `include_str!` of a file outside the package
// breaks `cargo package`; this crate is `publish = false`. It gates the API,
// not the output, and only the `rust` fence, against the *union* of features
// (`unstable`, `counters`) rather than the default tier a README reader has.
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
/// sources: `doctor --from-bag <recording.mcap>` and `tf_tree ingest` (read a
/// file); `--attach` (live arena, read-only) and `doctor --from-file
/// <index.tft>` (frozen); the last two need a Linux `--features shm` build.
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

/// `--color` for `tf_tree top`.
///
/// The severity floor `--exit-code` gates on.
///
/// A tier so bare `--exit-code` still means `--exit-code error`; `warn` is
/// warn-and-above.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ExitSeverity {
    /// Only error-severity findings fail (on a live arena: `TFT006`, `TFT012`).
    Error,
    /// Warn-severity findings fail too. `--suppress` silences one a fleet
    /// accepts.
    Warn,
}

/// Three states rather than a `bool`: colour belongs on a terminal, not in a
/// piped bug report.
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

/// `--interp` as a flag value.
///
/// A separate enum so `clap` stays out of the facade's dependency tree.
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
        /// (`docs/PHASE5.md` §1.2). Prints both versions, the layout hash and
        /// the action; needs no arena.
        #[arg(long)]
        explain_version: bool,
        /// Emit the report as JSON on one stream (`docs/PHASE5.md` §6). The
        /// schema (see [`catalogue::render_json`]) always carries every
        /// catalogue id.
        #[arg(long)]
        json: bool,
        /// Exit non-zero if any unsuppressed check at this severity or above
        /// fired. `--exit-code` alone means `--exit-code error`.
        ///
        /// Opt-in so `doctor` does not break `&&` in an operator's shell.
        /// `--exit-code warn` also gates on warnings (unclaimed edge,
        /// undersized ring, slot leak, ...); on a live arena `error` reduces to
        /// `TFT006` and `TFT012`.
        #[arg(long, value_name = "SEVERITY", num_args = 0..=1, default_missing_value = "error")]
        exit_code: Option<ExitSeverity>,
        /// Remove a check from the `--exit-code` gate, by id (`--suppress
        /// TFT013`). Repeatable; a suppressed check still runs and prints.
        #[arg(long, value_name = "TFTNNN")]
        suppress: Vec<String>,
        /// Diagnose an MCAP recording instead of the built-in fixture. Needs
        /// nothing installed, and is the only source `TFT018`/`TFT019` can
        /// judge (an arena holds only accepted pushes). The §3.2 ingest report
        /// goes to **stderr**, so `--json` stays parseable.
        ///
        /// Mutually exclusive with `--attach`, enforced in [`doctor_source`]
        /// rather than `conflicts_with`: `--attach` is `global = true` on the
        /// root, so `tf_tree --attach doctor --from-bag x` never reaches the
        /// `doctor` matcher.
        #[arg(long, value_name = "PATH")]
        from_bag: Option<std::path::PathBuf>,
        /// Diagnose a frozen `.tft` index (`docs/PHASE5.md` §2). §2.1 is
        /// NORMATIVE that it is read by the identical code as a live arena, so
        /// every check runs as on an attach except `TFT018`/`TFT019` (use
        /// `--from-bag`).
        ///
        /// Mutually exclusive with `--from-bag` (a `clap` conflict) and
        /// `--attach` (checked in [`doctor_source`]).
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
/// Any failure obtaining or inspecting the named tree, plus the command-line
/// refusals `doctor_source` raises.
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
            // A group's error names the flags without saying what the command
            // is for; this one does.
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

/// Default for `--max-memory`, in MiB. A constant because
/// [`IngestArgs::default`] must state the identical value
/// ([`IngestArgs::flags_set`] compares them).
const DEFAULT_MAX_MEMORY_MIB: u64 = 4096;

/// Default for `--future-horizon`, in seconds. See [`DEFAULT_MAX_MEMORY_MIB`].
const DEFAULT_FUTURE_HORIZON_S: f64 = 10.0;

/// Default for `--clock-reset-threshold`, in milliseconds. See
/// [`DEFAULT_MAX_MEMORY_MIB`].
const DEFAULT_CLOCK_RESET_THRESHOLD_MS: u64 = 100;

/// The knobs `docs/PHASE5.md` §3 puts on an ingest, shared by `ingest` and
/// `freeze --from-bag` via `#[command(flatten)]`; `doctor` flattens it
/// conditionally on `--from-bag` (see [`IngestArgs::flags_set`]).
#[derive(clap::Args, Clone, Debug)]
pub struct IngestArgs {
    /// Peak buffered-sample memory for pass two, in MiB (§3.1). Under `doctor
    /// --from-bag` it also bounds pass three
    /// ([`recording::arrival_observations`]); exceeding it is an error naming
    /// this flag, not a truncation.
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
    /// the default rule (last path segment is `tf_static`); does not narrow
    /// which topics are read.
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
    /// buffer, not peak memory: allow about 2.6× it for the decoder's working
    /// set. The default is derived from the library constant.
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
    /// guards against a corrupt length. Raise it for recordings with large
    /// attachments. Derived from the library constant.
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
    /// The flags in this group whose value differs from the default, by the
    /// spelling a user types.
    ///
    /// `doctor` flattens the group, so without this a flag given against the
    /// fixture or a `.tft` would parse and be silently ignored. It compares
    /// against [`IngestArgs::default`] because `clap`'s derive gives no
    /// `ArgMatches`; a flag passed explicitly at its default is missed,
    /// harmlessly.
    #[must_use]
    pub fn flags_set(&self) -> Vec<&'static str> {
        let d = IngestArgs::default();
        let mut out = Vec::new();
        // Field by field so the error can name the flag.
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
            // `saturating_mul`: an overflow would wrap a huge value into a tiny
            // ceiling that refuses every recording.
            max_chunk_uncompressed_bytes: self.max_chunk_size.saturating_mul(1024 * 1024),
            max_chunk_expansion_ratio: self.max_chunk_expansion,
            // `saturating_mul` for the same reason as the chunk ceiling above.
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

/// Render an ingest failure with the frame names it names by index, plus the
/// one remedy the message lacks.
/// The remedy shared by every refusal that came from a ceiling rather than
/// damage. One constant so the `BadChunk` and `AllChunksOverLimit` arms cannot
/// drift.
const LIMIT_REMEDY: &str =
    "\x20 --max-chunk-size <MiB> raises the ceiling on a chunk's uncompressed\n\
     \x20 size, and --max-chunk-expansion its ratio to the compressed bytes.\n\
     \x20 Both bound what this reader will allocate for one chunk, so raise them\n\
     \x20 to what the recording actually needs rather than to the maximum.";

pub(crate) fn ingest_err(
    e: tf_tree_ingest::IngestError,
    frames: &tf_tree_ingest::Frames,
) -> anyhow::Error {
    let text = tf_tree_ingest::describe(e, frames).to_string();
    match e {
        // The two limit refusals get the flags, not `skip` (already the
        // default, and the chunk is fine).
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
        // One message for both builds: the user cannot tell an unknown codec
        // name from a `--no-default-features` build, so it names both.
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
    /// An MCAP recording, ingested in-process (`doctor --from-bag`), carrying
    /// the transforms **in the recording's own log order**.
    ///
    /// `docs/PHASE5.md` §4.1 is NORMATIVE that there is no separate offline
    /// API: `tf_tree_ingest::run` hands back the ordinary [`Tree`]; the
    /// [`Observations`] beside it are what an arena cannot supply
    /// ([`checks::PushStream::RingsAtRest`]).
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

    /// How this source's push stream was obtained: decides whether `TFT001`,
    /// `TFT011`'s Phase 1 half, `TFT018` and `TFT019` have evidence (see
    /// [`checks::PushStream`]).
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

    /// What kind of participant table this source's arena carries; decides
    /// whether `TFT014` has evidence. Differs from [`Self::stream`]: an
    /// ingested bag's table is this process's own, and only a frozen `.tft`
    /// carries somebody else's.
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

/// Build the fixture, or attach, and keep whatever has to stay alive alive.
/// Trees and attached handles are `Box::leak`ed: the process inspects once and
/// exits, and `Drop` (slot release, owner thread) has nothing useful to do.
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

/// `doctor`'s sources: the two `source` offers, plus the two recording ones.
/// Only `doctor` has them; `tree`, `echo` and `top` are about *now*. The tree
/// is leaked as in [`source`].
fn doctor_source(
    live: Live<'_>,
    from_bag: Option<&std::path::Path>,
    from_file: Option<&std::path::Path>,
    ingest: &IngestArgs,
) -> Result<(&'static Tree, Source)> {
    let _ = from_file;
    // One arena per run: `--attach` and the recording flags name different ones
    // (see `from_bag`'s doc for why not `conflicts_with`).
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
    // Refuse a flag this invocation will ignore: `IngestArgs` is flattened
    // whole, and a silently accepted `--tf-prefix` reads as applied.
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
        // To stderr, always: `--json` keeps stdout a parseable
        // `tf_tree.doctor/1` document, and the §3.2 report must not be hidden
        // behind it.
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

/// The push stream a command's checks run against. A live arena has none, so it
/// is reconstructed from the rings (rate, ordering and buffer-depth work;
/// multi-writer cannot fire).
///
/// Taken out of the `Source`, not cloned: on `--from-bag` the stream is one
/// 24-byte sample per dynamic transform, the one footprint that scales with
/// recording length.
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

    // Index edges by child frame so we can annotate each frame with its edge.
    // The reference clock, not `fixture::NOW_NS`: on a real arena
    // (Unix-nanosecond stamps) the synthetic constant clamped every age to 0.
    // `Clock::decide` is the estimator `doctor` and `top` share.
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
    // The fixture's history is anchored to its synthetic `NOW_NS`, a live
    // arena's to its publishers' stamps; echoing a live tree at the fixture's
    // clock would report `Extrapolation` everywhere.
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

/// `tf_tree doctor` — the `docs/PHASE5.md` §6 catalogue. `--exit-code` is
/// opt-in: a diagnostic that fails by default gets wrapped in `|| true`.
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
            // Refused, not ignored: a typo would leave a gate that looks
            // configured and is not.
            anyhow::anyhow!("unknown check id {s:?} — expected one of TFT001..TFT019")
        })?;
        ids.insert(id);
    }

    let (tree, mut src) = doctor_source(live, from_bag, from_file, ingest)?;
    let obs = observations(tree, &mut src);
    // The arena first, the lock file second (`docs/decisions/0028` piece 2
    // constraint 3); [`slot_facts`] takes the captured row, so the order cannot
    // be inverted.
    #[allow(unused_mut)]
    let mut snap = Snapshot::capture(tree);
    // `TFT014`'s participant half needs the lock file (`0028` plan step 6),
    // only on `--attach`; no other source has a rendezvous.
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
        // No `else`, no error: the arena mapped. Without the file every slot
        // keeps `LockByte::Unknown`, so the check narrows rather than
        // fabricates.
    }
    let stats = checks::collect_edge_stats(tree, &snap);
    let clock = checks::Clock::decide(&checks::newest_stamps(&snap), unix_nanos_now());
    // Captured here so `TFT019`'s outcome and its `Meta.notes` entry read the
    // same split.
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
        // Same call as `tft010`/`tft011`, and the same edges the checks ran
        // against, so skip, disclosure and byte figures describe one arena.
        rings: sizing::Rings::from_edges(snap.edges.iter().map(|e| (e.capacity, e.occupancy()))),
        notes: evidence_notes(
            src.stream(),
            &snap,
            &obs,
            inputs.clock,
            &clock_step,
            checks::no_counter_evidence(inputs.counters, inputs.stats),
            // The outcome itself, not a second predicate.
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
        // `!is_healthy()`, not `count_at(Warn) > 0`: `warn` is warn-and-above,
        // so an error still fails it.
        Some(ExitSeverity::Warn) => !report.is_healthy(),
    };
    if gate_fired {
        std::process::exit(1);
    }
    Ok(())
}

/// Disclosures for a check that ran with one of its evidence sources missing.
///
/// `TFT011`: the counters half ([`checks::no_counter_evidence`]) and the
/// `capacity x period` half (needs per-sample arrival delay, which only the
/// fixture records; [`checks::PushStream::no_arrival_delays`]). Exactly one
/// note appears, since when both are blind the skip reason carries both.
/// `TFT009`'s takes the check's own outcome, not a second predicate, because
/// skip and note are independent (`checks::silence_coverage_note`; its `None`
/// is unreachable from `checks::run`). `TFT017`: every dynamic edge unclaimed,
/// a note not a skip so a fleet of dead publishers is never silent. `TFT015`:
/// unconditional (an engine gap). `TFT007`, `TFT019`: only when the check
/// covered some edges and not others.
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
        // Both blind: `tft011` skipped and said so itself.
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

/// `TFT017`'s disclosure: it fired on **every** dynamic edge. The count comes
/// from [`doctor::check_unclaimed_dynamic`], not a second walk.
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

/// The arena's instance uuid, which only a shared arena has.
/// The rendezvous runtime directory this host resolves to, with its source
/// (`docs/PHASE2.md` §15). Consults no tree and degrades to `None` rather than
/// failing. The source is included because an unexpected path is usually an
/// unexpected rule.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn resolved_runtime_dir() -> Option<String> {
    let d = tf_tree_ipc::RuntimeDir::resolve().ok()?;
    Some(format!("{} ({:?})", d.path().display(), d.source()))
}

/// See [`resolved_runtime_dir`]. Without `shm` there is no rendezvous to have a
/// directory for, so there is nothing to resolve and nothing to report.
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
/// # `--rw` is refused, not ignored
///
/// The attach flags are global, so `tf_tree --rw top` parses; a read-write
/// mapping is what D18 keeps away from a diagnostic tool, so it is refused
/// rather than silently downgraded.
fn cmd_top(
    live: Live<'_>,
    interval_ms: u64,
    iterations: u64,
    edge: Option<String>,
    color: Option<bool>,
    web: Option<std::net::SocketAddr>,
) -> Result<()> {
    // A floor, not a clamp: `--interval 0` would spin a core over a robot's
    // arena, the one way this tool can perturb what it observes.
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

    // The lock file makes read-only participants (a byte, no arena record)
    // visible, `top` itself included.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    let merge: Box<dyn Fn(&mut top::Capture) + Sync> = if live.attach {
        match live
            .rendezvous()
            .ok()
            .filter(|rv| rv.lock_path().exists())
            .and_then(|rv| tf_tree_ipc::LockFile::open(rv.lock_path()).ok())
        {
            // Not an error: the arena mapped. A missing lock file costs the
            // `mode`/`comm` columns and read-only rows.
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
/// # The rate limit is correctness, not politeness
///
/// One [`top::Sampler`] holds the per-tick state and every delta is a
/// difference between two observations; two polling tabs would each see half
/// the samples and every rate would read half. A poll sooner than `interval`
/// after the last is answered from the previous document (idempotent within a
/// tick; a reload or second tab is not an error).
///
/// # The `Mutex`
///
/// `web::serve` runs a thread per connection, so the closure is serialised; it
/// costs a cache hit or one capture, and no socket I/O is inside it. A poisoned
/// lock is recovered so one panicking handler does not end the view.
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
        // `--edge` seeds the page's selection once (`web/index.html` reads it
        // from the first document); the browser owns it after.
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
/// Runs the runnable correctness half in-process (naive-Rust differential,
/// agreement within `1e-12`). The perf and zero-allocation gates need dedicated
/// hardware: `cargo xtask bench-gate`, `cargo bench`.
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
/// Attaches **read-only** (`AttachArgs` defaults, D18) and copies the arena.
///
/// # It is a snapshot, not a transaction
///
/// Publishers keep publishing, so the image is a smear (see
/// `tf_tree_arena::write_frozen`); the output says so.
///
/// `source_digest` is all-zero for `--from-live`; `--from-bag` fills it with
/// BLAKE3 of the recording (§2.3).
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
    // `conflicts_with` makes the flags exclusive, not required, so "neither" is
    // handled here.
    anyhow::ensure!(
        from_live,
        "`freeze` needs a source; pass `--from-live` or `--from-bag <PATH>`"
    );
    // The same rule as `doctor`: `--from-live` reads no recording, so the
    // flattened §3 knobs are dead. `--report` is the exception (an output
    // path).
    let set = ingest.flags_set();
    anyhow::ensure!(
        set.is_empty(),
        "{} {} only meaningful with --from-bag: --from-live copies a live arena and reads no \
         recording.\n\x20 Drop the flag, or freeze the recording instead with --from-bag.",
        set.join(", "),
        if set.len() == 1 { "is" } else { "are" },
    );
    let tree = live.open()?;
    // Saturating: `as i64` would wrap past 2^63 ns (2262) into a negative
    // "created" stamp.
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    // One message, not an `anyhow` chain: `FrozenFileError` is `Copy` and
    // `String`-free, so the path is joined here.
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
/// # Why this one *is* an atomic snapshot and `--from-live` is not
///
/// The tree is built in this process from a file nobody else writes, so a
/// `.tft` frozen from a recording is exactly the recording.
///
/// The report is written alongside the `.tft` by default (§3.2), under a
/// derived name so two `.tft` files in one directory do not collide.
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
/// (`docs/decisions/0028` plan step 6): one iteration of `cmd_participants`'
/// loop, reshaped for [`doctor::Snapshot::probe_lock_facts`].
///
/// # It takes the captured row, and that is the read-order pin
///
/// `0028` piece 2 constraint 3 requires the `state` word be read before the
/// byte. The parameter is the already-captured [`doctor::ParticipantInfo`], not
/// a slot number, so no call can be hoisted above `Snapshot::capture`; `loom`
/// (in `tf_tree`) is where the ordering itself is argued.
///
/// Three-valued: a failed `probe_participant` is not "free" (§6.2's fail-safe
/// rule).
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
/// A diagnostic inference, never a protocol decision (`docs/PHASE2.md` §5.1),
/// spelled here rather than via `Identity::matches_running_process` because
/// that is two-valued and reads every failure as gone (`docs/decisions/0028`,
/// *"the fail-safe claim is false on this code"*). The verdict is
/// [`recorded_given`]; this function is the two reads it needs.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn recorded_process(id: Option<&tf_tree_ipc::Identity>) -> doctor::RecordedProcess {
    let Some(id) = id else {
        return doctor::RecordedProcess::Unknown;
    };
    recorded_given(
        id.start_time,
        tf_tree_ipc::start_time_of(id.pid),
        // Would this host show us an entry? Test whether it shows our own:
        // `/proc/self/stat` is running by construction.
        tf_tree_ipc::self_start_time().is_ok(),
        id.pid_ns_inode,
        // The observer's own namespace, never one read through the recorded pid
        // (`docs/decisions/0033` *Decision* 2): a namespace-local pid may name
        // an unrelated process here and fail open. Zero on a failed read keeps
        // pre-`0033` behaviour; `Unknown` for every slot would leave `TFT014`
        // unable to fire.
        tf_tree_ipc::self_pid_ns_inode().unwrap_or(0),
        // Whether this `/proc` describes this process's pid namespace, read in
        // one process (a forked shell probe disagrees in a container). `true`
        // on a failed read, by the same rule.
        tf_tree_ipc::proc_self_pid().is_none_or(|p| p == std::process::id()),
    )
}

/// Turn a recorded `start_time`, what the `/proc` read came back as, and three
/// facts about where the observer is standing, into one of the three
/// [`doctor::RecordedProcess`] answers.
///
/// `tf_tree`'s `alive_given` (`crates/tf_tree/src/tree.rs`) with a three-valued
/// result and the same bias: telling an operator a live process is gone is the
/// corruption to avoid. Host facts arrive as parameters because none can be
/// arranged in a test.
///
/// # The two guards, before the match
///
/// Placed ahead of the whole `match probe` because the namespace false
/// positives take different arms (`docs/decisions/0033` *Decision* 3): a
/// namespaced participant seen from the host takes `Ok(_)`, a host participant
/// seen from a container takes `ENOENT`, and a real fork inheritor takes
/// `ENOENT` with byte-identical text.
///
/// * `recorded_pid_ns` against `observer_pid_ns`: a record from another PID
///   namespace names a pid this `/proc` does not use. Zero on either side is
///   *unknown namespace* and keeps the pre-`0033` behaviour.
/// * `proc_is_ours`: false when this `/proc` is not the observer's namespace's
///   (a bare `unshare --fork --pid`), so every recorded pid is incomparable. A
///   failed `readlink` takes the failed-read rule at the call site, never
///   `Unknown` for all.
///
/// Both land on `slot_leak`'s `(LockByte::Held, Unknown) => None`, so
/// `checks.rs` needs no edit. They do move one verdict: a non-`FREE` record
/// with a free byte and a `Running` process goes from silence to `TFT014` *byte
/// free* once degraded to `Unknown` (accepted, `0033` *Consequences*).
///
/// The arms:
///
/// * stored `start_time` of zero ("could not read my own"): not compared, since
///   it would report a running process dead.
/// * start time matches: running; differs: pid recycled, gone.
/// * `ENOENT` on a host that would have shown an entry: gone, the only arm that
///   proves death.
/// * `ENOENT` on a host that says so about everybody: unknown.
/// * any other read or parse failure (`EACCES` from `hidepid`, `EMFILE`, ...):
///   unknown, never gone.
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
    // Zero on either side is "unknown namespace": comparing would turn every
    // pre-`0033` record into `Unknown`.
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

/// Liveness is the kernel's answer (`F_OFD_GETLK` on the participant's byte),
/// not an inference from the identity record (§5.1).
#[cfg(all(feature = "shm", target_os = "linux"))]
fn cmd_participants(live: Live<'_>) -> Result<()> {
    let rv = live.rendezvous()?;
    let path = rv.lock_path();
    println!("tf_tree participants — {}", path.display());

    if !path.exists() {
        // Not an error: "nothing is running" is a legitimate answer.
        println!("  no lock file: nothing has ever attached to this domain/name");
        return Ok(());
    }

    // `IpcError` chains into `anyhow`; the operator reads its sentence.
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
        // A byte held with no identity record: a participant between taking its
        // byte and writing its record.
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
        // "stale": the kernel released the byte but the record remains; a
        // reaper is owed.
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

    /// A `TFT009` outcome that **ran**, the state every note test here assumes;
    /// the skipping arm is driven by
    /// [`the_silence_coverage_note_reaches_the_report_metadata`].
    fn tft009_ran() -> catalogue::CheckOutcome {
        catalogue::CheckOutcome::ran(catalogue::Tft::Tft009, Vec::new())
    }

    /// One running writer in slot 0, the owner every claimed edge in these
    /// snapshots names.
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

    /// **The `TFT007` coverage note reaches `Meta.notes`.**
    /// `checks::rate_coverage_note` is unit tested but its call site is not
    /// reachable from those tests.
    ///
    /// Snapshot: two dynamic edges, one declaring a rate and measurable, one
    /// declaring nothing.
    ///
    /// Mutant: delete `notes.extend(checks::rate_coverage_note(snap, obs,
    /// clock, stream));` from `evidence_notes` ⇒ the `expect` fires with "no
    /// coverage note".
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
        // 20 Hz on edge 1, above `RATE_MIN_INTERVALS`, so it is compared; edge
        // 2 declares nothing.
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

    /// **The `TFT009` silence note reaches `Meta.notes`, and is silent when the
    /// half actually ran.** The third case is the contradiction the note could
    /// produce: `TFT009` `not run` beside a note saying gaps were measured.
    ///
    /// Mutant: delete the `silence_coverage_note` line from `evidence_notes` ⇒
    /// "no TFT009 silence note". Mutant: drop the `Status::Skipped` guard in
    /// `checks::silence_coverage_note` ⇒ the third case fails.
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

        // A source nothing is writing: the half cannot run and must say so.
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

        // And on the live case it ran, so there is nothing to disclose.
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

        // The third case: the same source with `TFT009` `not run`; the note
        // must not claim gaps were measured.
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

    /// **The stopped-publisher note reaches `Meta.notes`, and is silent when
    /// nothing was withheld.** Without it `TFT007` and `TFT008` skip with no
    /// line saying which edge was declined.
    ///
    /// Mutant: delete `notes.extend(checks::stopped_publisher_note(obs, clock,
    /// stream));` ⇒ "no stopped-publisher note".
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
        // A flawless 100 Hz stream that stopped 8 seconds ago.
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

        // The same arena read from a source nobody is writing: nothing is
        // withheld, so a note about it would be about a decision never made.
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

    /// **The `TFT019` coverage note reaches `Meta.notes` too, and is silent on
    /// a live arena** (where `TFT019` skipped, so a note would describe a run
    /// that never happened); `live` is threaded, not guarded at the call site.
    ///
    /// Mutant: delete `notes.extend(clock_step.coverage_note(live));` ⇒ "no
    /// TFT019 coverage note". Mutant B: pass `false` for `live` ⇒ "a live arena
    /// skipped TFT019 outright".
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
        // Edge 1 is on the wall clock and attributed; edge 2 is on a steady
        // clock and refused.
        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base_link", 2, 2),
            ],
            edges: vec![dyn_edge(1, 1, 2, 0), dyn_edge(2, 2, 3, 3)],
            participants: live_writer(),
        };
        // A clock step, not a stray inversion: ten pushes, the clock jumps 100
        // ms back, the publisher carries on, so ten arrivals are rejected in
        // one run (what `TFT019` reads).
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

    /// **The `CompressedChunk` remedy is a bare string** for a narrower case
    /// than before (see `ingest_err`'s arm); only reachable at this level.
    ///
    /// Mutant: delete the `CompressedChunk` arm ⇒ only the generic "uses
    /// compressed chunks" line and no command.
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

    /// **The chunk bounds default to exactly what the library defaults to, and
    /// are reachable from the command line.**
    ///
    /// Mutant: pass `DEFAULT_MAX_CHUNK_EXPANSION_RATIO` in `to_options` instead
    /// of `self.max_chunk_expansion` ⇒ the `--max-chunk-expansion 4` assertion
    /// fails with 1024. A literal `default_value_t = 64` is not caught until
    /// the library constant moves; the derived expression is what holds the
    /// property.
    #[test]
    fn the_chunk_bounds_default_to_the_librarys_and_are_settable() {
        let parse = |extra: &[&str]| -> tf_tree_ingest::IngestOptions {
            let mut args: Vec<&str> = vec!["tf_tree", "ingest", "--bag", "/nonexistent.mcap"];
            args.extend_from_slice(extra);
            match Cli::try_parse_from(args).expect("parse").command {
                Command::Ingest { opts, .. } => opts.to_options().expect("options"),
                // `Command` derives no `Debug`, so the failure names the
                // subcommand asked for.
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

        // A MiB count no multiplication can hold saturates rather than wrapping into
        // a ceiling that refuses every recording.
        let huge = parse(&["--max-chunk-size", &u64::MAX.to_string()]);
        assert_eq!(huge.max_chunk_uncompressed_bytes, u64::MAX);
    }

    /// A bad chunk's error points at the policy that would have kept the rest
    /// of the recording.
    ///
    /// Mutant: delete the `BadChunk` arm ⇒ the `--on-bad-chunk` assertion
    /// fails.
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

    /// **A refusal that came from a ceiling names the ceiling's flag, never
    /// `--on-bad-chunk=skip`.** Both policies are covered (`BadChunk` under
    /// `halt`, `AllChunksOverLimit` under `skip`).
    ///
    /// Mutant: delete the `ImplausibleSize | ImplausibleWindow` arm ⇒ the
    /// `--max-chunk-size` assertion fails while the `--on-bad-chunk` one fires.
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

    /// A window refusal reaches the same arm as a size refusal (two variants in
    /// one pattern).
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
    ///
    /// Mutant: replace the `ClockResetSplitUnsupported` arm with bare `{text}`
    /// ⇒ the `PHASE5` assertion fails.
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

    /// **`--web` with no value binds loopback, and `--web ADDR` binds what the
    /// operator named.** The loopback default lives only in
    /// `default_missing_value`; integration tests pass `127.0.0.1:0` and would
    /// not notice. Asserted through `clap`, not a fixed port.
    ///
    /// Mutant: delete `default_missing_value = web::DEFAULT_ADDR` ⇒ `--web` is
    /// rejected. Mutant: default `0.0.0.0:8787` ⇒ the `is_loopback` assertion
    /// fails.
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
        // An explicit address still wins, including the `:0` spelling the
        // integration tests rely on.
        assert_eq!(
            parse(&["tf_tree", "top", "--web", "127.0.0.1:0"])
                .expect("an explicit address")
                .port(),
            0
        );
        assert_eq!(parse(&["tf_tree", "top"]), None, "no --web, no server");
    }

    /// [`web::DEFAULT_ADDR`] spelled out, since comparing a constant to itself
    /// would pass with it changed.
    const DEFAULT_WEB_ADDR_FOR_TEST: &str = "127.0.0.1:8787";

    /// A snapshot of `n` dynamic edges, `claimed` deciding whether each carries
    /// a live writer.
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

    /// **`TFT011` discloses exactly the half that is blind, and says nothing
    /// when it skipped.**
    ///
    /// Mutant: change the `(Some(_), Some(_)) => {}` arm to push the counter
    /// note ⇒ the third assertion fails.
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

        // Counters have a verdict, the stream has no arrival delays.
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

        // The reverse: an arena nobody has read, on the fixture's stream.
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

        // Neither half: `tft011` skipped, so there is nothing to disclose.
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

        // Both halves live: nothing to disclose either.
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

    /// **`TFT017` firing on every dynamic edge is a fact about the arena, and
    /// says so.** A bag-built or frozen arena has no writer, so every healthy
    /// recording warned per edge. A note not a skip, so a fleet of dead
    /// publishers is never silent.
    ///
    /// Mutant: change the `!= dynamic` guard in `unclaimed_coverage_note` to
    /// `== 0` ⇒ the second assertion fails.
    #[test]
    fn a_wholly_unclaimed_arena_is_disclosed_as_an_arena_fact() {
        let note = unclaimed_coverage_note(&claim_snapshot(3, false))
            .expect("every dynamic edge unclaimed and nothing said so");
        assert!(
            note.contains("all 3 dynamic edge(s)") && note.contains("--from-bag"),
            "{note}"
        );

        // One writer still alive: the finding is about that edge, not the arena.
        let mut partial = claim_snapshot(3, false);
        partial.edges[0].claimed = true;
        partial.edges[0].owner_pid = 4711;
        assert_eq!(unclaimed_coverage_note(&partial), None);

        // Nothing unclaimed, and an arena with no dynamic edges at all.
        assert_eq!(unclaimed_coverage_note(&claim_snapshot(3, true)), None);
        assert_eq!(unclaimed_coverage_note(&claim_snapshot(0, false)), None);
    }

    /// **An ingest flag `doctor` will ignore is named, one at a time.** Pins
    /// both directions.
    ///
    /// Mutant: `flags_set` returns `Vec::new()` ⇒ the second assertion fails.
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

    /// **"Cannot tell" is not "dead", and every arm that could have said
    /// otherwise is here.** The three `Unknown` rows are the ones the earlier
    /// revision got wrong (`hidepid=2`, `EMFILE`).
    ///
    /// Mutant: restore `Err(_) if proc_answers => Gone` as one arm ⇒ the
    /// `EACCES` row fails with *left: Gone, right: Unknown*.
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
        // The `docs/decisions/0033` facts held at "observer where the record
        // was written", so these rows are about the `/proc` classification
        // alone.
        let here = |stored, probe, proc_answers| {
            recorded_given(stored, probe, proc_answers, HERE, HERE, true)
        };

        // The two arms that are evidence, and the only two.
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

        // Everything else is a refusal to answer.
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

        // And the record that never had a start time to compare.
        assert_eq!(
            here(0, Ok(STORED), true),
            R::Unknown,
            "`of_self_best_effort`'s zero compares unequal to every real start \
             time; treating it as a mismatch reports a running process dead"
        );
    }

    /// A namespace inode standing in for "the observer's own"; nonzero, since
    /// zero means *unknown namespace*.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    const HERE: u64 = 4_026_531_836;
    /// A second, different namespace (values as measured in
    /// `docs/decisions/0033`).
    #[cfg(all(feature = "shm", target_os = "linux"))]
    const ELSEWHERE: u64 = 4_026_532_488;

    /// **A pid from another PID namespace is not a pid this `/proc` can be
    /// asked about** (`docs/decisions/0033`). The namespace-mismatch rows hold
    /// `proc_is_ours = true` and the `proc_is_ours = false` rows hold the
    /// namespaces equal, so neither guard carries the other's rows.
    ///
    /// Arms `0033` stages: (A) a namespaced participant seen from the host
    /// (`Ok(_)`); (B) a host participant seen from a container (`ENOENT`); (C)
    /// a real fork inheritor (`ENOENT`, byte-identical to A), which must stay
    /// `Gone`; (D) participant and observer in one bare `unshare --fork --pid`.
    /// A and B are the first block, D the second. End-to-end:
    /// `tests/attach.rs`'s `tft014_namespace_*`.
    ///
    /// Mutant: drop the `recorded_pid_ns != 0` conjunct ⇒ the pre-`0033`-record
    /// row fails with *left: Unknown, right: Gone*.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[test]
    fn a_pid_from_another_namespace_is_not_a_pid_this_proc_can_answer_about() {
        use doctor::RecordedProcess as R;

        const STORED: u64 = 4242;
        let enoent = Err(tf_tree_ipc::ProcError::Unreadable {
            pid: 7,
            raw_os_error: 2,
        });

        // Arm A: recorded pid 1 exists here (`systemd`) with another start
        // time; a live participant one namespace away.
        assert_eq!(
            recorded_given(STORED, Ok(STORED + 1), true, ELSEWHERE, HERE, true),
            R::Unknown,
            "a recycled-pid verdict about a pid from another numbering is not \
             a verdict"
        );
        // Arm B, the mirror: the recorded pid is not in this `/proc`. The guard
        // is before the match for this reason.
        assert_eq!(
            recorded_given(STORED, enoent, true, ELSEWHERE, HERE, true),
            R::Unknown,
            "ENOENT about a pid this /proc does not number proves nothing"
        );
        // Arm C, the true positive: same `ENOENT` arm as B, same text; only the
        // namespace separates them.
        assert_eq!(
            recorded_given(STORED, enoent, true, HERE, HERE, true),
            R::Gone,
            "a fork inheritor in the observer's own namespace is exactly what \
             TFT014 exists to report"
        );

        // Zero is *unknown namespace* on either side and must not fire the
        // guard.
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

        // Arm D: every namespace matches, so only this guard stands between
        // `doctor` and a finding about its own slot.
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
        // And the same two facts with `/proc` back to its own: the guard is
        // about the observer's `/proc` and nothing else.
        assert_eq!(
            recorded_given(STORED, Ok(STORED), true, HERE, HERE, true),
            R::Running
        );
    }
}
