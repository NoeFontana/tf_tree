#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Implementation of the `tf_tree` diagnostics CLI.
//!
//! Printing to stdout/stderr is this crate's whole job, so the workspace
//! `print_stdout`/`print_stderr` lints are allowed here.
//!
//! # Four sources, and the fixture is only the one you get by naming none
//!
//! A bare invocation builds the mobile-robot [`tf_tree_bench::fixture`] in
//! process and inspects *that*, which is what keeps every subcommand runnable on
//! a machine with nothing deployed. The other three are named on the command
//! line: a live arena (`--attach`), an MCAP recording (`doctor --from-bag`,
//! `tf_tree ingest`) and a frozen `.tft` index (`doctor --from-file`). The
//! middle one needs nothing installed — `docs/PHASE5.md` §2.2's wedge — and the
//! other two are Linux-plus-`--features shm` builds, because that is where the
//! mapping code is.
//!
//! What the Phase 1 design got right is the shape, and it is why adding those
//! three cost no new checks: everything downstream reads a captured
//! [`doctor::Snapshot`], so a source is a capture and never a code path through
//! the catalogue. Where the sources are *not* interchangeable, `checks::PushStream`
//! says so per source rather than per check — a ring holds only the pushes the
//! engine accepted, so no arena, live or frozen, can answer `TFT018`.

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
/// Name no source and every subcommand builds an in-process fixture, so this
/// binary answers on a machine with nothing deployed. The sources that are
/// somebody's real data: `doctor --from-bag <recording.mcap>` and `tf_tree
/// ingest`, which need nothing installed and read a file you already have;
/// `--attach`, which maps a live arena read-only; and `doctor --from-file
/// <index.tft>`, which maps a frozen one. The last two exist only in a Linux
/// build with `--features shm`.
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
/// Three states rather than a `bool`, because the useful default is neither:
/// colour belongs on a terminal and must be absent from the file an operator
/// pipes into a bug report, and `--color true` is not a spelling anyone reaches
/// for.
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
/// A separate enum rather than deriving on `tf_tree::InterpPolicy`: the facade
/// is `#![forbid(unsafe_code)]` and dependency-disciplined, and giving it a
/// `clap` derive would put a CLI argument parser in the dependency tree of
/// every library that links the engine.
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
        /// Diagnose an MCAP recording instead of the built-in fixture.
        ///
        /// **This is the one `doctor` invocation that needs nothing installed**:
        /// point it at a bag you already have and it reports on the `/tf`
        /// traffic in it. §2.2's wedge argument applied to the catalogue — the
        /// user changes nothing about their robot.
        ///
        /// It is also the only source `TFT018` and `TFT019` can reach a verdict
        /// on. A recording is written in log order, so a stamp that went
        /// backwards is in the file at the position it arrived at; an arena
        /// — live or frozen — holds only the pushes the engine accepted.
        ///
        /// The §3.2 ingest report is printed to **stderr**, so `--json` keeps
        /// stdout parseable.
        ///
        /// Mutually exclusive with `--attach`: `doctor` reports on exactly one
        /// arena and the two flags name different ones. Left un-declared, the
        /// source precedence silently won — `--attach --name prod doctor
        /// --from-bag x.mcap` exited 0 having never opened `prod`, which is an
        /// operator reading a clean bill of health about the wrong thing. That
        /// one is enforced in [`doctor_source`] and **not** with `clap`'s
        /// `conflicts_with`, which does not fire here: `--attach` is
        /// `global = true` and declared on the root command, so when it is typed
        /// *before* the subcommand — `tf_tree --attach doctor --from-bag x` —
        /// `clap` matches it against the root and the `doctor` matcher never
        /// sees a conflict to report. Verified both ways round.
        #[arg(long, value_name = "PATH")]
        from_bag: Option<std::path::PathBuf>,
        /// Diagnose a frozen `.tft` index (`docs/PHASE5.md` §2).
        ///
        /// §2.1 is NORMATIVE that a frozen arena is read by the identical code
        /// as a live one, so every topology, occupancy and rate check runs
        /// exactly as it does on an attach. `TFT018`/`TFT019` do **not**: a
        /// `.tft` is an arena, and an arena never stored the rejected arrival
        /// they are about. Use `--from-bag` for those.
        ///
        /// Mutually exclusive with `--from-bag` and with `--attach`, for the
        /// reason `--from-bag` states. Only the first of those two is a `clap`
        /// conflict — both are declared on this subcommand, so `clap` sees it —
        /// and the `--attach` half is checked in [`doctor_source`].
        #[cfg(all(feature = "shm", target_os = "linux"))]
        #[arg(long, value_name = "PATH", conflicts_with = "from_bag")]
        from_file: Option<std::path::PathBuf>,
        #[command(flatten)]
        ingest: IngestArgs,
    },
    /// Live view of an arena: rates, staleness, claims, participants, feed.
    ///
    /// `docs/PHASE5.md` §7. Read-only, always — see [`top`] for why there is no
    /// `ratatui` here and what that costs.
    Top {
        /// Redraw interval in milliseconds.
        #[arg(long, default_value_t = 1000, value_name = "MS")]
        interval: u64,
        /// Stop after this many frames; `0` runs until interrupted.
        ///
        /// Not only a test affordance: `tf_tree top --iterations 1 > frame.txt`
        /// is how an operator attaches a snapshot of a live arena to a bug
        /// report, and it is why the non-tty path emits no escape sequences.
        ///
        /// **With `--web` it bounds connections, not frames** — the browser
        /// decides when a frame happens, so "frame" has no meaning on that side.
        #[arg(long, default_value_t = 0, value_name = "N")]
        iterations: u64,
        /// Show the per-edge detail pane, with the inter-arrival histogram.
        ///
        /// Takes an edge id or a substring of its `parent->child` label. A flag
        /// rather than a cursor because there is no raw-mode key handling
        /// without a `libc` dependency this crate does not have.
        #[arg(long, value_name = "ID|NAME")]
        edge: Option<String>,
        /// `auto` (the default) follows whether stdout is a tty.
        ///
        /// Ignored under `--web`: that view's colours come from the page's own
        /// stylesheet, which follows the browser's light/dark preference.
        #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
        color: ColorChoice,
        /// Serve §7's embedded web view instead of drawing to the terminal.
        ///
        /// `--web` alone binds `127.0.0.1:8787`; `--web ADDR` binds what you
        /// name, and `--web 127.0.0.1:0` lets the kernel pick a free port. The
        /// chosen URL is printed. Loopback is the default because serving a
        /// robot's live transform state on `0.0.0.0` is a security bug in
        /// somebody's deployment (§7); a non-loopback bind is accepted and
        /// warned about, not refused.
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
    /// **This is the subcommand that needs nothing installed.** It changes
    /// nothing about anybody's robot, reads a file they already have, and
    /// prints the §3.2 ingest report — which is D28's whole wedge. It is
    /// deliberately *not* behind `--features shm`: writing a `.tft` needs the
    /// frozen backend and therefore the mapping code, but running the two
    /// passes and printing what they found needs neither.
    ///
    /// Use `tf_tree freeze --from-bag` to keep the result.
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
    /// from a recording.
    ///
    /// Exactly one source is required. Neither is implied: a `freeze` that
    /// silently meant "live" would have had to change meaning when `--from-bag`
    /// landed, which is why `--from-live` was a required flag before there was
    /// anything to disambiguate it from.
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
        /// for `--from-bag`; ignored for `--from-live`, which has no recording
        /// to report on.
        #[arg(long, value_name = "PATH")]
        report: Option<std::path::PathBuf>,
        #[command(flatten)]
        ingest: IngestArgs,
    },
    /// Obtain, validate or explain a bridge topology file (`docs/PHASE4.md` §5.8).
    ///
    /// The engine has no runtime edge declaration, so the ingest bridge is told
    /// its topology up front. `--discover` is how an operator obtains that file
    /// from a robot; `--config` is the pre-flight that fails on a laptop rather
    /// than at bridge startup.
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
        /// Prefix every discovered frame with this `tf_prefix` (§5.6).
        ///
        /// Use it when the bridge that will read this file runs with the same
        /// prefix: a config keyed on the unprefixed names declares every edge
        /// and matches none.
        #[arg(long, value_name = "PREFIX", requires = "discover")]
        tf_prefix: Option<String>,
        /// Interpolation policy the discovered file should default to.
        #[arg(long, value_enum, requires = "discover")]
        interp: Option<InterpArg>,
        /// Check the file's per-edge time domains against the bridge's (§5.5).
        ///
        /// The startup refusal a bridge would perform, performed on a laptop.
        #[arg(long, value_name = "N", requires = "config")]
        domain: Option<u8>,
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
/// Surfaces any failure obtaining or inspecting the tree this invocation names —
/// building the fixture, attaching to a live arena, ingesting a recording, or
/// mapping a frozen `.tft` — plus the refusals `doctor_source` raises before any
/// of that, which are about the *command line* rather than about an arena.
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
            // clap cannot express "one of these two" without a group, and a
            // group's error message names the flags without saying what the
            // command is for. This one does.
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

/// Default for `--max-memory`, in MiB.
///
/// A named constant rather than a literal in the `#[arg]` attribute because
/// [`IngestArgs::default`] has to state the identical value: the two are what
/// [`IngestArgs::flags_set`] compares, so a drift between them would report a
/// flag as set that the user never typed, or miss one they did.
const DEFAULT_MAX_MEMORY_MIB: u64 = 4096;

/// Default for `--future-horizon`, in seconds. See [`DEFAULT_MAX_MEMORY_MIB`].
const DEFAULT_FUTURE_HORIZON_S: f64 = 10.0;

/// Default for `--clock-reset-threshold`, in milliseconds. See
/// [`DEFAULT_MAX_MEMORY_MIB`].
const DEFAULT_CLOCK_RESET_THRESHOLD_MS: u64 = 100;

/// The knobs `docs/PHASE5.md` §3 puts on an ingest.
///
/// Shared by `ingest` and `freeze --from-bag` through `#[command(flatten)]`
/// rather than duplicated: the two commands run the identical two passes, and
/// two copies of seven flags is two chances for a default to drift between the
/// command that previews a recording and the command that keeps it.
///
/// `doctor` flattens it too, and there it is **conditional** on `--from-bag`:
/// see [`IngestArgs::flags_set`].
#[derive(clap::Args, Clone, Debug)]
pub struct IngestArgs {
    /// Peak buffered-sample memory for pass two, in MiB (§3.1).
    ///
    /// Under `doctor --from-bag` it bounds pass **three** as well — the
    /// arrival-order replay in [`recording::arrival_observations`], which holds
    /// one 24-byte sample per dynamic transform in the recording. Exceeding it
    /// there is an error naming this flag rather than a truncation, because a
    /// truncated arrival stream would let `TFT018` report `pass` about a prefix.
    #[arg(long, value_name = "MIB", default_value_t = DEFAULT_MAX_MEMORY_MIB)]
    pub max_memory: u64,
    /// What to do when the recording's clock jumps backwards (§3.2).
    #[arg(long, value_enum, default_value_t = ClockResetArg::Halt)]
    pub on_clock_reset: ClockResetArg,
    /// What to do about a chunk that will not decompress or fails its CRC.
    ///
    /// `skip` (the default) drops that chunk, counts it, and reports the span of
    /// time it took with it — one bad chunk in four hundred thousand must not cost
    /// the recording. `halt` refuses instead, for when you have to know the
    /// recording is whole before trusting a number derived from it.
    ///
    /// A codec this build cannot decompress is **never** skipped either way: every
    /// chunk would use it, and the result would be "no transforms" about a file
    /// that is perfectly intact.
    #[arg(long, value_enum, default_value_t = BadChunkArg::Skip, value_name = "POLICY")]
    pub on_bad_chunk: BadChunkArg,
    /// Treat this topic as carrying static transforms. Repeatable.
    ///
    /// Without it the rule is "the last path segment is `tf_static`", which
    /// covers `/tf_static` and `/robot1/tf_static`. Passing this **replaces**
    /// that classification rule rather than adding to it, and does **not**
    /// narrow which topics are read — `--tf-topic` is the flag that does that.
    #[arg(long, value_name = "TOPIC")]
    pub static_topic: Vec<String>,
    /// Read only this topic's dynamic transforms. Repeatable.
    ///
    /// This is the only flag that narrows the read; without it every channel
    /// carrying the TF schema is ingested, remapped ones included (§3.3).
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
    /// Largest chunk this reader will decompress, in MiB.
    ///
    /// A chunk header's `uncompressed_size` is a number off a disk, and it is both
    /// the allocation size and the decompression-bomb bound. Real recorders chunk
    /// at 1–8 MiB, so the default is over an order of magnitude above them — but a
    /// recording written with larger chunks is unusual rather than corrupt, and the
    /// person who meets the limit is the person who cannot patch the library.
    ///
    /// **It bounds the output buffer, not this process's peak.** The decoder allocates
    /// its own working set alongside it: measured, `ruzstd` adds about 2 MiB of peak
    /// while decoding a 1 MiB chunk and 6.5 MiB for a 4 MiB one, tracking the frame's
    /// declared window rather than this number. Sizing a container against this flag
    /// should allow for roughly 2.6× it.
    ///
    /// The default is derived from the library constant rather than written down, so
    /// the two cannot drift.
    #[arg(
        long,
        value_name = "MIB",
        default_value_t = tf_tree_ingest::DEFAULT_MAX_CHUNK_UNCOMPRESSED_BYTES / (1024 * 1024)
    )]
    pub max_chunk_size: u64,
    /// Largest `uncompressed_size / compressed_size` a chunk may claim.
    ///
    /// The other half of the bomb guard: `--max-chunk-size` alone cannot refuse
    /// 64 MiB of output from 200 bytes of input, and no ceiling loose enough for a
    /// real 8 MiB chunk can.
    #[arg(
        long,
        value_name = "RATIO",
        default_value_t = tf_tree_ingest::DEFAULT_MAX_CHUNK_EXPANSION_RATIO
    )]
    pub max_chunk_expansion: u64,
    /// Where to put §3.1's temporary run file. Defaults to the system temporary
    /// directory.
    ///
    /// Only used when a *single* edge exceeds `--max-memory`; every other
    /// recording is handled by re-reading, with no file at all. Worth setting
    /// when `/tmp` is a tmpfs, because a spill into RAM does not bound RAM.
    #[arg(long, value_name = "DIR")]
    pub spill_dir: Option<std::path::PathBuf>,
}

/// `--on-clock-reset`, as §3.2 spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ClockResetArg {
    /// Stop and name the timestamp.
    Halt,
    /// §3.2's multi-file split, which is **not implemented** — the value exists
    /// so the tool can say so with a reason. Rejecting the spelling outright
    /// would leave a user reading §3.2 unable to tell whether they had the name
    /// wrong or the feature missing.
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
    ///
    /// Every value here is either the named constant the `#[arg]` attribute
    /// uses or the empty value `clap` gives an `Option`/`Vec` flag, so this
    /// cannot say "default" about something the parser would not.
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
            spill_dir: None,
        }
    }
}

impl IngestArgs {
    /// The flags in this group whose value differs from the default, by the
    /// spelling a user types.
    ///
    /// # Why `doctor` needs this at all
    ///
    /// `doctor` flattens the whole group so that `--from-bag` accepts the same
    /// knobs `tf_tree ingest` does. The consequence, undeclared, was that all
    /// eleven parsed on **every** `doctor` invocation and were then dropped on
    /// the floor: `doctor --max-memory 64` against the built-in fixture, and
    /// `doctor --from-file x.tft --tf-prefix robot1` against a frozen index that
    /// no ingest will touch, both exited 0 having ignored the flag. Silently
    /// accepting a flag you ignore is worse than rejecting it — the user gets no
    /// signal that the thing they asked for did not happen.
    ///
    /// # It compares values, and the one case it misses is the one that costs
    /// nothing
    ///
    /// `clap`'s derive API does not hand back an `ArgMatches`, so "was this
    /// flag on the command line" is not directly available; this compares
    /// against [`IngestArgs::default`] instead. A flag passed *explicitly at its
    /// default value* is therefore not reported. That is the one miss, and it is
    /// harmless by construction: the run behaves identically to the one without
    /// the flag, so there is nothing the user asked for that did not happen.
    #[must_use]
    pub fn flags_set(&self) -> Vec<&'static str> {
        let d = IngestArgs::default();
        let mut out = Vec::new();
        // Field by field rather than a derived `PartialEq`, because the error
        // has to name the flag: "one of your ingest options was ignored" sends
        // the reader back to `--help` to work out which.
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
        // Bit-for-bit, not an epsilon: the question is "did this come from the
        // constant" and not "is it close to it", and `to_bits` also makes a
        // `--future-horizon NaN` differ from the default rather than compare
        // equal to everything and slip through.
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
            // `saturating_mul` for the same reason the library's ratio guard uses
            // it: `--max-chunk-size 18446744073709551615` is a number a user can
            // type, and an overflow here would wrap it into a *tiny* ceiling that
            // refuses every recording.
            max_chunk_uncompressed_bytes: self.max_chunk_size.saturating_mul(1024 * 1024),
            max_chunk_expansion_ratio: self.max_chunk_expansion,
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

/// Render an ingest failure with the frame names it names by index, and attach
/// the one remedy that is not obvious from the message.
///
/// `IngestError` is `Copy` and `String`-free by house rule, so it cannot carry a
/// frame name or a suggested command; both are joined on here, at the only layer
/// that has a terminal to print to.
/// The remedy shared by every refusal that came from a ceiling rather than from
/// damage.
///
/// One constant and not two copies: the `BadChunk` arm (under
/// `--on-bad-chunk=halt`) and the `AllChunksOverLimit` arm (under the default
/// `skip`) are the same condition met at different policies, and a reader who fixes
/// the wording in one place should not be able to leave the other stale.
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
        // **The two limit refusals get the flags, not the skip policy.** Under
        // `--on-bad-chunk=halt` a chunk over a ceiling arrives here as `BadChunk`
        // like any other, and suggesting `skip` — which is already the default —
        // would be advice to lose the chunk when the chunk is fine and the reader's
        // ceiling is what refused it. `AllChunksOverLimit` below is the same
        // condition once it has taken the whole recording.
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
        // **One message for both builds, deliberately.** zstd and lz4 are decoded
        // by pure-Rust codecs behind `tf_tree_ingest`'s default-on `compression`
        // feature, so reaching here means either a codec name outside the MCAP
        // specification or a `--no-default-features` build — and a user cannot tell
        // which from the outside, so the message names both and the remedy that
        // covers either. Splitting it on `#[cfg]` would make the arm a user meets
        // depend on how their binary was built, which is the one fact they are
        // least able to check.
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
    /// An MCAP recording, ingested in-process (`doctor --from-bag`), carrying
    /// the transforms **in the recording's own log order**.
    ///
    /// `docs/PHASE5.md` §4.1 is NORMATIVE that there is no separate offline API,
    /// and this obeys it literally: `tf_tree_ingest::run` hands back the
    /// ordinary [`Tree`] that `tf_tree ingest` and `tf_tree freeze --from-bag`
    /// already build. The [`Observations`] beside it are the one thing an arena
    /// cannot supply — see [`checks::PushStream::RingsAtRest`].
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

    /// How this source's push stream was obtained, which is what decides whether
    /// `TFT001`, `TFT011`'s Phase 1 half, `TFT018` and `TFT019` have evidence.
    ///
    /// **This used to be an `is_live()`, and that was keying on the wrong
    /// fact** — see [`checks::PushStream`]. A frozen arena is not live and still
    /// cannot answer `TFT018`.
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

    /// What kind of participant table this source's arena carries, which is what
    /// decides whether `TFT014` has evidence.
    ///
    /// The split is *not* the same one [`Self::stream`] makes, and the two
    /// disagree on both recording sources: an ingested bag builds an ordinary
    /// arena in **this** process, so its participant table is this process's
    /// own and perfectly answerable, while its push stream is a replay. Only a
    /// frozen `.tft` carries somebody else's table.
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

/// `doctor`'s sources: the two `source` offers, plus the two recording ones.
///
/// Separate from [`source`] because only `doctor` has them. `tree`, `echo` and
/// `top` are about *now* — a live view of an arena somebody is publishing into —
/// and pointing them at a recording would be a different feature with a
/// different argument surface. `doctor` is the one whose whole value is a
/// verdict about data that already exists.
///
/// The tree is `Box::leak`ed for the same reason [`source`] leaks its own: the
/// process inspects once and exits, and a frozen mapping or an ingested arena
/// has nothing useful to do between the last `println!` and `exit`.
fn doctor_source(
    live: Live<'_>,
    from_bag: Option<&std::path::Path>,
    from_file: Option<&std::path::Path>,
    ingest: &IngestArgs,
) -> Result<(&'static Tree, Source)> {
    let _ = from_file;
    // **One arena per run.** `--attach` names a live one and the two recording
    // flags name a file; without this, `doctor_source`'s precedence quietly
    // picked the recording and `tf_tree --attach --name prod doctor --from-bag
    // x.mcap` exited 0 having never touched `prod`. See `from_bag`'s doc for
    // why `clap`'s `conflicts_with` cannot do this one.
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
    // **Refuse a flag this invocation will ignore.** `IngestArgs` is flattened
    // whole so `--from-bag` takes the knobs `tf_tree ingest` takes; the flags
    // are meaningless to every other source, and accepting one silently is how
    // a user comes away believing a `--tf-prefix` was applied.
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
        // **To stderr, always.** `--json` writes a `tf_tree.doctor/1` document to
        // stdout and a consumer parses it; the §3.2 report is the other half of
        // what a stranger needs to know about their own file, and dropping it
        // would hide "12 000 zero stamps were discarded" behind a clean-looking
        // catalogue. Two streams is what lets both be true at once.
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

/// The push stream a command's checks run against.
///
/// A live arena has no recorded push stream — nobody was watching when those
/// samples arrived — so it is reconstructed from what the rings retain. That is
/// strictly less than the fixture knows: the ring holds the newest `capacity`
/// stamps and the *current* claim owner, so rate, ordering and buffer-depth
/// checks all work, and the multi-writer check cannot fire because a ring cannot
/// remember a writer that has been replaced.
///
/// **Taken out of the `Source`, not cloned.** The clone doubled peak memory for
/// the two variants that carry a stream, and on `--from-bag` that stream is one
/// 24-byte sample per dynamic transform in the recording — the one place in
/// `doctor` whose footprint scales with the recording's length rather than with
/// the arena's fixed capacity. Nothing reads the
/// `Observations` back out of the `Source` afterwards; `banner` and `stream`
/// are the only other accessors and neither looks inside.
fn observations(tree: &Tree, src: &mut Source) -> Observations {
    // Used only by the arms that replay from the rings, which do not exist
    // without `shm`.
    let _ = tree;
    match src {
        Source::Fixture(obs) | Source::Bag(obs) => core::mem::take(obs),
        #[cfg(all(feature = "shm", target_os = "linux"))]
        Source::Live | Source::Frozen => Observations::from_arena(tree, &Snapshot::capture(tree)),
    }
}

/// `tf_tree tree` — render the topology.
///
/// The tree comes from [`source`], which is a fixture only when nothing else was
/// named, and that is also where the argument for leaking it lives. This used to
/// carry its own copy of that argument saying "its fixture tree"; a second copy
/// is a second thing to keep true, and this one had stopped being.
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
#[allow(clippy::too_many_arguments)]
fn cmd_doctor(
    live: Live<'_>,
    json: bool,
    exit_code: bool,
    suppress: &[String],
    from_bag: Option<&std::path::Path>,
    from_file: Option<&std::path::Path>,
    ingest: &IngestArgs,
) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    for s in suppress {
        let id = catalogue::Tft::parse(s).ok_or_else(|| {
            // Refused rather than ignored: a typo that silently suppresses
            // nothing leaves a gate that looks configured and is not.
            anyhow::anyhow!("unknown check id {s:?} — expected one of TFT001..TFT019")
        })?;
        ids.insert(id);
    }

    let (tree, mut src) = doctor_source(live, from_bag, from_file, ingest)?;
    let obs = observations(tree, &mut src);
    // **The arena first, the lock file second** — `Snapshot::capture` reads
    // every slot's `state` word and the probe below asks about every slot's
    // byte, and that order is `docs/decisions/0028` piece 2's third constraint
    // rather than a preference. It is not kept by these two statements being in
    // this order: `slot_facts` takes the *captured row*, so there is nothing
    // here that could be computed first. See [`slot_facts`].
    #[allow(unused_mut)]
    let mut snap = Snapshot::capture(tree);
    // **`TFT014`'s participant half needs the lock file, and this is where
    // `doctor` finally opens one** (`0028` plan step 6). Only on `--attach`: no
    // other source has a rendezvous, and a `.tft`'s or a bag's table is
    // answered — or skipped — without one.
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
        // No `else`, and deliberately no error: the arena mapped, so there is a
        // tree to report on. Without the file every slot keeps
        // `doctor::LockByte::Unknown`, which `checks::slot_leak` reads as "not
        // asked" rather than as "free" — the check narrows, it does not
        // fabricate.
    }
    let stats = checks::collect_edge_stats(tree, &snap);
    let clock = checks::Clock::decide(&checks::newest_stamps(&snap), unix_nanos_now());
    // Captured here, not inside the check: `TFT019`'s outcome and its note in
    // `Meta.notes` are two views of one split, and this is the only place both
    // can read the same one.
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
        frames: snap.frames.len(),
        edges: snap.edges.len(),
        generated_unix_nanos: unix_nanos_now(),
        now_nanos: clock.nanos(),
        clock_source: clock.label(),
        counters_compiled_in: tf_tree::counters_compiled_in(),
        // The same call `tft010` and `tft011` make, so a skip and its
        // disclosure cannot disagree about whether the counters said anything.
        // The same edges the checks ran against, so the header's byte figures
        // and any capacity finding below describe one arena rather than two.
        rings: sizing::Rings::from_edges(snap.edges.iter().map(|e| (e.capacity, e.occupancy()))),
        notes: evidence_notes(
            src.stream(),
            &snap,
            &obs,
            inputs.clock,
            &clock_step,
            checks::no_counter_evidence(inputs.counters, inputs.stats),
        ),
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
/// `TFT011` has two: the `docs/PHASE5.md` §5 counters, and the Phase 1
/// `capacity x period` against observed publish latency, which needs a
/// per-sample arrival delay. Only the fixture records one — a replayed ring has
/// no receipt time and a recording's log time is the recorder's clock, not the
/// publisher's — so everywhere else `arrival_delay_ns` is zero, and zero latency
/// never exceeds any buffer span. That half of the check is then structurally
/// silent, and reporting `pass` without saying so would claim a result it did
/// not earn. [`checks::PushStream::no_arrival_delays`] is both the predicate and
/// the sentence.
///
/// The counter half fails the same way and is disclosed the same way: an arena
/// that has served no lookups reads exactly like a healthy one
/// ([`checks::no_counter_evidence`]). **Exactly one of the two notes appears**,
/// because when *both* halves are blind `TFT011` skips outright and its skip
/// reason already carries both sentences — a note repeating them next to a
/// `not run` line would be the report explaining itself twice.
///
/// `TFT017`'s is an all-or-nothing disclosure rather than a partial-coverage
/// one: when *every* dynamic edge is unclaimed, the finding is about the arena
/// and not about any edge in it, and an arena built from a recording or opened
/// frozen is always in that state. It is a note and not a skip on purpose — a
/// fleet in which every publisher has died reaches the same state, and that is
/// the one thing this check must never fall silent about.
///
/// `TFT015`'s disclosure is unconditional rather than source-specific: the
/// missing participants row is a gap in the engine, not in this run's evidence,
/// so it applies to every source alike.
///
/// `TFT007`'s is per-arena and computed from the snapshot: it appears only when
/// the check compared *some* edges and not others, which is the one case where
/// its `pass` covers less than it looks like it does.
///
/// `TFT019`'s is the same shape: it appears only when the check attributed some
/// out-of-order edges to a wall-clock step and did not attribute others — a tag
/// it declined to guess about, or a wall-clock edge whose rejections were not
/// concentrated enough to be a step — which is the case where neither its
/// findings nor a skip reason carries what it did not cover.
fn evidence_notes(
    stream: checks::PushStream,
    snap: &Snapshot,
    obs: &Observations,
    clock: checks::Clock,
    clock_step: &checks::ClockStepEvidence,
    counter_evidence: Option<&'static str>,
) -> Vec<String> {
    let mut notes = vec![checks::PARTICIPANT_OCCUPANCY_NOTE.to_owned()];
    notes.extend(checks::rate_coverage_note(snap, obs));
    notes.extend(checks::clock_offset_note(snap, stream, clock));
    notes.extend(clock_step.coverage_note(stream));
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

/// `TFT017`'s disclosure: it fired on **every** dynamic edge in the arena.
///
/// The count comes from [`doctor::check_unclaimed_dynamic`] rather than from a
/// second walk of `snap.edges` with the same predicate, so the note cannot come
/// to disagree with the findings it is about.
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

/// The system clock as nanoseconds since the Unix epoch.
///
/// Saturates rather than panicking on a clock before 1970: `doctor` reporting a
/// bad clock is useful, `doctor` aborting because of one is not.
///
/// `pub(crate)` because `top` needs the same value for the same reason:
/// `checks::Clock::decide` votes the arena's stamps against it.
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
/// The attach flags are global, so `tf_tree --rw top` parses. A read-write
/// mapping is exactly what D18 exists to keep away from a diagnostic tool, and
/// a live view is the tool most likely to be left running unattended on a
/// robot. Silently downgrading would be friendlier and worse: the operator would
/// believe they had asked for something and got it. Refusing states the rule
/// once, where it is violated.
fn cmd_top(
    live: Live<'_>,
    interval_ms: u64,
    iterations: u64,
    edge: Option<String>,
    color: Option<bool>,
    web: Option<std::net::SocketAddr>,
) -> Result<()> {
    // A floor rather than a clamp: `--interval 0` is a request to spin a core
    // reading a robot's arena as fast as it can, which is the one way this tool
    // *can* perturb what it observes (cache-line traffic on every ring head).
    // Answering "no" is more useful than quietly doing something else.
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

    // The lock file is what makes read-only participants visible at all: they
    // hold a byte and write no arena record, so without this the participant
    // pane would list only the writers — and `top` itself would be invisible in
    // its own output.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    let merge: Box<dyn Fn(&mut top::Capture) + Sync> = if live.attach {
        match live
            .rendezvous()
            .ok()
            .filter(|rv| rv.lock_path().exists())
            .and_then(|rv| tf_tree_ipc::LockFile::open(rv.lock_path()).ok())
        {
            // Not an error: the arena mapped, so there is something to watch.
            // A missing or unreadable lock file costs the `mode`/`comm` columns
            // and the read-only rows, and the pane's `record` column already
            // says which rows came from where.
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
/// # The rate limit is not politeness, it is correctness
///
/// One [`top::Sampler`] holds the only per-tick state there is, and every delta
/// in the document (`delta_head`, `delta_errors`, `observed_hz`) is a difference
/// between two of its observations. Two browser tabs polling at 1 Hz would take
/// alternate observations, so each would see half the samples over a full
/// interval and every rate on both pages would read half of what the arena is
/// doing — a wrong number, silently, with no error anywhere.
///
/// So a poll arriving sooner than `interval` after the last one is answered from
/// the previous document. That makes the endpoint idempotent within a tick,
/// which is also what lets a reload not perturb the view.
///
/// A refresh younger than the interval is *not* an error: it is what a second
/// tab, an F5, or a `watch curl` does, and all three should show the current
/// tick rather than a 429.
///
/// # The `Mutex` is the same argument, not a second one
///
/// `web::serve` runs a thread per connection, so the sampler is now reachable
/// from several at once and the correctness above becomes a data race as well
/// as a wrong number. Serialising the whole closure is exactly right and costs
/// nothing worth measuring: everything inside it is either a cache hit or one
/// arena capture, and the interval is 50 ms at its fastest. What must *not* be
/// serialised is the socket I/O, and none of it is here.
///
/// A poisoned lock is recovered rather than propagated. The alternative is that
/// one panicking handler ends the operator's view, which is the failure mode
/// the threading exists to remove.
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
        // `--edge` seeds the page's selection; after that the browser owns it,
        // because there is no key handling to take it back with. `web/index.html`
        // reads this field once, from the first document it paints.
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
/// `tf_tree freeze --from-live` — `docs/PHASE5.md` §2, and §5.6's capture.
///
/// Attaches **read-only** (`AttachArgs` defaults, D18) and copies the arena.
/// A diagnostic that had to map a robot's tree read-write in order to take a
/// snapshot of it would be a strictly worse tool than one that could not take
/// the snapshot at all.
///
/// # It is a snapshot, not a transaction
///
/// Publishers keep publishing while this runs, so the image is a smear rather
/// than a point in time — see `tf_tree_arena::write_frozen`. The output says so,
/// because an operator who reads "frozen 233 MB" and assumes a consistent
/// instant will eventually be surprised by a `SlotContended` in an offline
/// query and have nothing to attribute it to.
///
/// `source_digest` is all-zero for `--from-live`: a live arena is not a
/// recording and has no content hash to name. `--from-bag` fills it with BLAKE3
/// of the recording (§2.3).
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
    // `conflicts_with` makes the two flags mutually exclusive but not mutually
    // *required*, so "neither" reaches here and is stated where the code depends
    // on it rather than in an attribute two hundred lines away.
    anyhow::ensure!(
        from_live,
        "`freeze` needs a source; pass `--from-live` or `--from-bag <PATH>`"
    );
    // **The same rule `doctor` applies, for the same reason.** `IngestArgs` is
    // flattened here so `--from-bag` takes the §3 knobs; `--from-live` reads no
    // recording, so every one of them is dead on this path and accepting one
    // silently tells a user their `--tf-prefix` was applied to a frozen index it
    // never touched. `--report` is the deliberate exception and says so in its
    // own help text — it names an output path, not an ingest behaviour.
    let set = ingest.flags_set();
    anyhow::ensure!(
        set.is_empty(),
        "{} {} only meaningful with --from-bag: --from-live copies a live arena and reads no \
         recording.\n\x20 Drop the flag, or freeze the recording instead with --from-bag.",
        set.join(", "),
        if set.len() == 1 { "is" } else { "are" },
    );
    let tree = live.open()?;
    // `as i64` would wrap silently once `as_nanos` passes 2^63 (2262-04-11) and
    // hand the header a negative "created" stamp that reads as 1901. Saturating
    // costs nothing on a once-per-freeze path, and the field is provenance only
    // — a clamped far-future stamp is visibly wrong, a wrapped one is not.
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    // One message rather than an `anyhow` context chain: `FrozenFileError` is
    // `Copy` and `String`-free by house rule, so all it can say is *what* went
    // wrong — the path is the missing half, and it belongs in the same line an
    // operator reads, not one frame above it.
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
/// The tree is built in this process from a file nobody else is writing, so
/// there is no publisher to race and no smear. That is the difference worth
/// stating: a `.tft` frozen from a recording is exactly the recording, and a
/// `.tft` frozen from a live arena is a best effort.
///
/// The report is written **alongside** the `.tft` by default, because §3.2 says
/// it is a first-class output and a report that has to be asked for is a report
/// nobody has when they need it. Its default name is derived rather than fixed
/// so two `.tft` files in one directory do not overwrite each other's.
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

/// The lock file's facts about **one** captured participant slot, for
/// `TFT014`.
///
/// `docs/decisions/0028` plan step 6: `doctor` needs the lock file, which
/// `tft014` did not take. `cmd_participants` is the pattern — open the file,
/// probe each byte, read each identity — and this is one iteration of that loop
/// reshaped into a value [`doctor::Snapshot::probe_lock_facts`] can carry into
/// the check.
///
/// # It takes the captured row, and that is the read-order pin
///
/// The `state` word is observed by `Snapshot::capture` and the byte here, and
/// `0028` piece 2's third constraint says the word must be read first: under
/// word-then-byte the `Acquire` load of a live word synchronises-with
/// `fill_slot`'s publishing `Release` store, so a probe sequenced after it must
/// see the byte held; reversed, a sweep can see a byte free before a registrant
/// takes it and then read the record it published, and `loom` erases a
/// published record that way in 0.00 s.
///
/// The argument that the order matters is `loom`'s, in `tf_tree`'s model of
/// `reclamation_verdict` — no sequence of stable slot states can show which
/// read happened first, so no test in this crate can. What *is* pinned here is
/// that this program obeys it, and the pin is this signature: the parameter is
/// the already-captured [`doctor::ParticipantInfo`], not a slot number, so
/// there is no call to hoist above `Snapshot::capture` that compiles. The
/// earlier revision took `&LockFile` alone and returned a whole table; moving
/// its one call site above the capture compiled and passed every test.
///
/// Three-valued on purpose. `probe_participant` failing is not "free" — the
/// `top` merge collapses it that way because a missing byte costs it a display
/// column, where here it would be an accusation (§6.2's fail-safe rule).
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
/// **A diagnostic inference and never a protocol decision** (`docs/PHASE2.md`
/// §5.1). It is the sentence `tf_tree_ipc::Identity::matches_running_process`
/// was written for — *"slot 3's record names pid 1841, which is gone"* — and it
/// is spelled here rather than called there for one reason, which
/// `docs/decisions/0028` works through in *"the fail-safe claim is false on this
/// code"*: that method is two-valued and maps every read failure to `false`, so
/// on a host whose `/proc` is not mounted every participant reads as gone.
/// `TFT014`'s fork arm fires on *byte held plus process gone*, so under the
/// two-valued answer it would fire on every healthy slot in the table on such a
/// host — the "check that always fires" failure, at `warn`, on a correct robot.
///
/// The verdict itself is [`recorded_given`], which is `tf_tree`'s `alive_given`
/// with `tf_tree`'s three-way `ProcStartTime` split spelled through
/// `tf_tree_ipc::ProcError`, whose variants already draw the same line. This
/// function is the two reads that classification needs and nothing else.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn recorded_process(id: Option<&tf_tree_ipc::Identity>) -> doctor::RecordedProcess {
    let Some(id) = id else {
        return doctor::RecordedProcess::Unknown;
    };
    recorded_given(
        id.start_time,
        tf_tree_ipc::start_time_of(id.pid),
        // The honest test for "would this host have shown us an entry" is
        // whether it will show us our *own*: `/proc/self/stat` is about a
        // process that is running by construction. `tf_tree`'s
        // `proc_answers_here` latches the same probe; this is a one-shot
        // command and does not need the latch.
        tf_tree_ipc::self_start_time().is_ok(),
        id.pid_ns_inode,
        // **The observer's own namespace, never one read through the recorded
        // pid** (`docs/decisions/0033` *Decision* 2). The probe the issue
        // suggested — `readlink /proc/<recorded_pid>/ns/pid` — fails *open*: a
        // namespace-local pid names a different process here, so on a host
        // where that number happens to be in use it reads an unrelated
        // same-uid process, finds a matching namespace, and confirms
        // `ForkInheritor` with false confidence. That is the same
        // successful-read-of-the-wrong-process class `recorded_given`'s own doc
        // records as the previously shipped bug here.
        //
        // Zero on a failed read, which lands on the same arm as a pre-`0033`
        // record: keep today's behaviour. Degrading to `Unknown` for every slot
        // instead would make `TFT014` unable to fire at all, trading a false
        // positive for a blind spot.
        tf_tree_ipc::self_pid_ns_inode().unwrap_or(0),
        // Whether this `/proc` describes *this* process's pid namespace. Read
        // in one process against a number this process already holds: the
        // shell spelling of this probe forks, so its two halves are about two
        // processes, and it disagrees in a container where the answer is yes.
        //
        // `true` on a failed read, by the same failed-read rule as the line
        // above.
        tf_tree_ipc::proc_self_pid().is_none_or(|p| p == std::process::id()),
    )
}

/// Turn a recorded `start_time`, what the `/proc` read came back as, and three
/// facts about where the observer is standing, into one of the three answers
/// [`doctor::RecordedProcess`] allows.
///
/// **This is `tf_tree`'s `alive_given` (`crates/tf_tree/src/tree.rs`),
/// transposed onto a three-valued result instead of a fail-safe boolean, and
/// deliberately not a second classification.** Same arms, same bias —
/// `record_is_alive`'s doc states it: *a false "dead" lets a rescuer take an
/// entry from a running process, which is corruption; a false "alive" only
/// delays recovery*. In an operator tool the corresponding corruption is
/// telling somebody a live process is gone, and the arms below exist to keep
/// that out.
///
/// It is **not** "same three inputs" any more, and the two that were added are
/// not more of the same kind. `alive_given`'s three answer *"what does `/proc`
/// say about this pid?"*; `docs/decisions/0033`'s two answer *"is this pid a
/// number I can ask `/proc` about at all?"*, which has to be settled first
/// because a pid is namespace-local and the classification below reads it as if
/// it were not.
///
/// Every host fact arrives as a parameter rather than as a read, for the reason
/// `alive_given`'s do: none of them is a thing a test can arrange. Whether
/// `/proc` answers is a property of the machine the suite runs on, staging pid
/// reuse means exhausting the pid space, and the two namespace facts need a
/// second pid namespace around the *observer*. Passing them in is what makes
/// the bias assertable instead of merely stated.
///
/// # The two guards, before the match, and why they are two
///
/// Both sit ahead of the whole `match probe` rather than as an arm ahead of
/// `Ok(_) => Gone`, and that placement is a measurement rather than a taste
/// (`0033` *Decision* 3). The namespace-shaped false positives take **different
/// arms**: a namespaced participant seen from the host takes `Ok(_)`, because
/// its recorded pid 1 exists here as `systemd` with another start time, while a
/// host participant seen from a container takes `ENOENT`, because its recorded
/// pid is not in that `/proc` at all. A genuine surviving fork inheritor takes
/// the `ENOENT` arm too and renders **byte-identical** text to the first — so
/// arm membership carries no information about which fault is present, and any
/// fix expressed at an arm either misses one false positive or silences the one
/// true positive.
///
/// * **`recorded_pid_ns` against `observer_pid_ns`** asks *is the recorded
///   process comparable to me?* A record from another PID namespace names a pid
///   drawn from a numbering this `/proc` does not use, so no probe result about
///   it is evidence. Zero on either side is *unknown namespace* — a pre-`0033`
///   record, or a `/proc` this process could not read — and means keep the
///   behaviour that shipped before the field existed.
/// * **`proc_is_ours`** asks a different question: *is my `/proc` describing my
///   own namespace at all?* Inside a bare `unshare --fork --pid` that never
///   remounted `/proc`, it is not — and then every recorded pid in the file is
///   incomparable **including this process's own**, which is how `doctor` came
///   to report its own participant slot as a fork inheritor and tell the
///   operator to stop it. The guard above is structurally blind to that: every
///   participant there is in the *same* namespace, so the inodes match and it
///   never fires.
///
///   `Unknown`-for-everything is right here and wrong for a failed read, and
///   the difference is that this is a **successful** read establishing that the
///   pid column of this file is drawn from a numbering `/proc` does not use.
///   There is no verdict left to give, so giving none is not a degradation. A
///   `readlink` that *fails* is the failed-read case and takes the failed-read
///   rule at the call site: today's behaviour, never `Unknown` for every slot,
///   because a check that can never fire is a blind spot traded for a false
///   positive.
///
/// Both land on `slot_leak`'s existing `(LockByte::Held, Unknown) => None`, so
/// the check reports nothing rather than reporting a different wrong thing, and
/// a same-namespace fork inheritor observed from a `/proc` that is its own is
/// classified exactly as before. **`checks.rs` needs no edit for that shape,
/// and a reader who does not know it will go looking for one.**
///
/// **They are not verdict-neutral, though, and the one verdict they move is
/// named here rather than left to be found.** `slot_leak`'s other arm reads
/// `(LockByte::Free, Gone | Unknown) => Some(SlotLeak::Abandoned)`, deliberately
/// — its own doc table says *"the byte alone is the leak signature, and §5.1
/// says the byte is the fact"*. So a slot with a **non-`FREE` record, a free
/// byte, and a recorded process that reads `Running` today** goes from silence
/// to a `TFT014` *byte free* report once either guard degrades it to `Unknown`.
/// For the first guard that needs the host process at the recorded
/// namespace-local pid to have a *matching* start time, which is the pid-reuse
/// collision the identity triple exists to exclude. The second is wider,
/// because it degrades every slot in the file at once — but only on a `/proc`
/// that is not the observer's namespace's, where the alternative reading of
/// that same slot is an accusation. Accepted, `0033` *Consequences*.
///
/// The arms, and which way each fails:
///
/// * **A stored `start_time` of zero** is `Identity::of_self_best_effort`'s
///   *"could not read my own"*, not a start time. It compares unequal to every
///   real one, so comparing anyway reports a *running* process dead — the
///   inversion `0028` names and `alive_given` carries its own arm for.
/// * **A start time that matches**: running.
/// * **A start time that differs**: the pid was recycled, so the process that
///   took this slot is gone even though the number is in use.
/// * **`ENOENT`, on a host that would have shown us an entry**: gone. This is
///   the *only* arm that proves death, exactly as `ProcStartTime::NoSuchProcess`
///   plus `proc_answers` is the only one in `alive_given`.
/// * **`ENOENT` on a host whose `/proc` says that about everybody**: unknown.
/// * **Any other read failure, and any parse failure**: unknown. An `EACCES`
///   from a `hidepid` mount, an `EMFILE`, an `ENOMEM`, a `stat` line this
///   parser did not understand — none of them is information about whether a
///   process exists, and the revision that shipped mapped every one of them to
///   *gone*: `Err(_) if self_start_time().is_ok() => Gone`. On a `hidepid=2`
///   host that reads a running publisher's slot as a fork inheritor and prints
///   a `warn` telling an operator its process no longer exists.
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
    // Zero on either side is "unknown namespace", not a namespace — a record
    // written before the field existed, or a writer that could not read
    // `/proc`. Comparing it would turn every such record into `Unknown`, which
    // is the blind spot rather than the fix.
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

/// Observed publish rate (Hz) for an edge, from the median inter-sample
/// interval — [`doctor::observed_rate_hz`], which `TFT007` also measures with,
/// so the column an operator reads and the check that judges it cannot differ.
fn observed_rate_hz(obs: &Observations, edge: u32) -> Option<f64> {
    let samples: Vec<&fixture::PushSample> = obs.events.iter().filter(|s| s.edge == edge).collect();
    doctor::observed_rate_hz(&samples)
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

    /// One running writer in slot 0 — the owner every claimed edge in this
    /// module's hand-built snapshots names.
    ///
    /// A snapshot whose claims name a slot no participant table holds is a
    /// snapshot of a wedged arena, and these fixtures are about healthy ones.
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

    /// **The `TFT007` coverage note reaches `Meta.notes`, which is its only
    /// route to an operator.**
    ///
    /// `Status` is three-valued and none of them is "ran, half blind", so the
    /// design argument for a partial `TFT007` pass being honest rests entirely
    /// on this disclosure being emitted. `checks::rate_coverage_note` is unit
    /// tested; the line that *calls* it is not reachable from any of those
    /// tests, and deleting it leaves every partial run reading as a full pass
    /// with all 531 other tests green.
    ///
    /// The snapshot is the shape that produces one: two dynamic edges, one
    /// declaring a rate and measurable, one declaring nothing — so the note is
    /// about coverage and not about a skip.
    ///
    /// Mutant: delete `notes.extend(checks::rate_coverage_note(snap, obs));`
    /// from `evidence_notes`. Applied: the `expect` fires with "no coverage
    /// note".
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
        // 20 Hz on edge 1, comfortably more than `RATE_MIN_INTERVALS`, so it is
        // compared and passes; edge 2 declares nothing and is not.
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

    /// **The `TFT019` coverage note reaches `Meta.notes` too, and is silent on a
    /// live arena.**
    ///
    /// Same argument as the `TFT007` test above:
    /// `ClockStepEvidence::coverage_note`
    /// is unit tested, but the line that *calls* it is not reachable from those
    /// tests, and deleting it leaves a partially-attributed run reading as a
    /// fully-attributed one. The `live` argument is passed here rather than
    /// guarded at the call site, so the second half checks the argument is
    /// actually threaded — on a live arena `TFT019` skipped outright and a note
    /// about edges it "did not attribute" would describe a run that never
    /// happened.
    ///
    /// Mutant: delete `notes.extend(clock_step.coverage_note(live));`
    /// from `evidence_notes`. Applied: the `expect` fires with "no TFT019
    /// coverage note".
    /// Mutant B: pass `false` for `live` at that call site. Applied: the live
    /// assertion fails with "a live arena skipped TFT019 outright".
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
        // Edge 1 is on the wall clock and is attributed; edge 2 is on a steady
        // clock and is refused — the one case neither a finding nor a skip
        // reason can carry.
        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base_link", 2, 2),
            ],
            edges: vec![dyn_edge(1, 1, 2, 0), dyn_edge(2, 2, 3, 3)],
            participants: live_writer(),
        };
        // A clock step, not a stray inversion: ten pushes at a 10 ms period,
        // the clock jumps 100 ms backwards, and the publisher carries on at the
        // same rate — so the ten arrivals on the way back up are all rejected.
        // That unbroken run is what `TFT019`'s concentration condition reads,
        // and a four-sample stream with one inversion would not be attributed.
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
                None
            )
            .iter()
            .any(|n| n.starts_with("TFT019")),
            "a live arena skipped TFT019 outright, so there is no coverage to disclose"
        );
    }

    /// **The `CompressedChunk` remedy is a bare string, and it is now the message
    /// for a *narrower* case than it used to be.**
    ///
    /// zstd and lz4 are decoded (`tf_tree_ingest`'s default-on `compression`
    /// feature), so an ordinary compressed recording no longer reaches this arm at
    /// all. What does is a codec name outside the MCAP specification, or a build
    /// with the feature off — and a user cannot tell those apart from the outside,
    /// which is why the message names both rather than being split on `#[cfg]`.
    ///
    /// It is still only reachable at this level: `ingest_err` is a `match` on an
    /// error value, and an end-to-end test would have to fabricate a recording with
    /// an invented codec name to get here.
    ///
    /// Mutant: delete the `CompressedChunk` arm, leaving the `_` fallthrough —
    /// applied, and this failed with only the generic "uses compressed chunks"
    /// line and no command.
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

    /// **The chunk bounds default to exactly what the library defaults to, and are
    /// reachable from the command line at all.**
    ///
    /// Two separate claims, and both are the kind that rot silently. A CLI default
    /// written as a literal drifts from the library constant the moment either
    /// moves, and the drift is invisible: both numbers are plausible, and every
    /// existing test passes with a ceiling that is wrong by a factor of two. And a
    /// bound with no flag is a bound whose whole justification — "the person who
    /// meets a limit cannot patch the crate" — is false for the only shipped
    /// consumer.
    ///
    /// Mutant: `default_value_t = 64` in place of the derived expression — applied,
    /// and this test still passed, because 64 MiB *is* the current default. So the
    /// derived expression is what makes the property hold, and this assertion only
    /// catches the drift *after* the constant moves; that is what it is for, and
    /// pretending the mutant died would be worse than saying so. Mutant 2: pass
    /// `DEFAULT_MAX_CHUNK_EXPANSION_RATIO` in `to_options` instead of
    /// `self.max_chunk_expansion` — applied, and the `--max-chunk-expansion 4`
    /// assertion failed with 1024, i.e. a flag that parses and does nothing.
    #[test]
    fn the_chunk_bounds_default_to_the_librarys_and_are_settable() {
        let parse = |extra: &[&str]| -> tf_tree_ingest::IngestOptions {
            let mut args: Vec<&str> = vec!["tf_tree", "ingest", "--bag", "/nonexistent.mcap"];
            args.extend_from_slice(extra);
            match Cli::try_parse_from(args).expect("parse").command {
                Command::Ingest { opts, .. } => opts.to_options().expect("options"),
                // `Command` derives no `Debug` (its variants hold types that do
                // not), so the failure names the subcommand asked for rather than
                // the one received.
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

    /// A bad chunk's error points at the policy that would have kept the rest of
    /// the recording.
    ///
    /// An operator who meets this under `--on-bad-chunk=halt` has already decided
    /// they want strictness; one who meets it because they *set* halt without
    /// meaning to needs to be told the other option exists. The error is the only
    /// place that can say so.
    ///
    /// Mutant: delete the `BadChunk` arm, leaving the `_` fallthrough ⇒ the
    /// `--on-bad-chunk` assertion fails and the user is told a chunk is unreadable
    /// with no indication that the recording is still usable.
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

    /// **A refusal that came from a ceiling names the ceiling's flag, and never
    /// `--on-bad-chunk=skip`.**
    ///
    /// The generic `BadChunk` advice is actively wrong for these two kinds. `skip`
    /// is already the default, and following it drops a chunk that is not damaged —
    /// the recording is sound and this reader declined to allocate for it. Both
    /// policies are covered because an operator meets the same condition as
    /// `BadChunk` under `halt` and as `AllChunksOverLimit` under `skip`, and a
    /// remedy that appears under only one of them is a remedy half the users never
    /// see.
    ///
    /// Mutant: delete the `ImplausibleSize | ImplausibleWindow` arm, so both fall
    /// through to the generic `BadChunk` one — applied, and the `--max-chunk-size`
    /// assertion fails while the `--on-bad-chunk` one fires, which is the
    /// misdirection in one line.
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
    ///
    /// Its own test because the two kinds are separate variants matched in one
    /// pattern, and an edit that splits the pattern would leave one of them falling
    /// through to advice about `--on-bad-chunk` with nothing to catch it.
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

    /// The `split` refusal cites the section that records it as unbuilt, so a
    /// user can tell a missing feature from a typo.
    ///
    /// Mutant: replace the `ClockResetSplitUnsupported` arm with the bare
    /// `{text}` — applied, and the `PHASE5` assertion failed.
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
    /// operator named.**
    ///
    /// The bare spelling is the one the documentation leads with and the one an
    /// operator types, and it is the *only* place the loopback default lives:
    /// `bind` binds whatever it is handed, so if `default_missing_value` were
    /// wrong or absent, §7's loopback-by-default rule would be gone with no
    /// code change anywhere near `web.rs`. Every integration test passes
    /// `127.0.0.1:0` explicitly and would not notice.
    ///
    /// Asserted through `clap` rather than by launching a server on port 8787:
    /// a fixed port collides with whatever else is on this machine and with a
    /// second copy of the test suite, and the property is about argument
    /// parsing.
    ///
    /// Mutant: delete `default_missing_value = web::DEFAULT_ADDR` from the
    /// `web` argument. Applied: `clap` rejects `--web` with "a value is
    /// required" and `try_parse_from` returns `Err`, so the first assertion
    /// fails. Second mutant: change the default to `0.0.0.0:8787` — the
    /// `is_loopback` assertion fails, which is the security-relevant half.
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

    /// [`web::DEFAULT_ADDR`] as the test above expects to see it printed.
    ///
    /// Spelled out rather than compared against the constant: comparing a
    /// constant to itself would pass with the constant changed to `0.0.0.0`,
    /// and the assertion above is about what `--web` binds, not about
    /// `SocketAddr`'s `Display`.
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
    /// The check reports two independent pieces of evidence under one id.
    /// Before this, only the capacity-vs-latency half had a disclosure, so a
    /// run whose *counter* half was structurally silent — every arena that has
    /// served no lookups, which includes the reference fixture — reported a
    /// bare `pass`. And when both halves are blind the check skips and its own
    /// reason carries both sentences, so a note there would be the report
    /// explaining itself twice.
    ///
    /// Mutant: change the `(Some(_), Some(_)) => {}` arm to push the counter
    /// note. Applied: the third assertion fails on a note beside a `not run`.
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
                None
            )),
            ""
        );
    }

    /// **`TFT017` firing on every dynamic edge is a fact about the arena, and
    /// says so.**
    ///
    /// A bag-built arena and a frozen `.tft` have no writer by construction, so
    /// `--from-bag` warned once per dynamic edge on every healthy recording a
    /// stranger could point it at — the shape of check people learn to ignore.
    /// It stays a warning rather than becoming a skip because a fleet whose
    /// publishers have all died reaches the identical state, and that is the
    /// one thing this check must never fall silent about; the note is what
    /// tells the two apart.
    ///
    /// Mutant: change the `!= dynamic` guard in `unclaimed_coverage_note` to
    /// `== 0`. Applied: the partially-claimed case gains a note and the second
    /// assertion fails.
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

    /// **An ingest flag `doctor` will ignore is named, one at a time.**
    ///
    /// `IngestArgs` is flattened whole so `--from-bag` takes the same knobs
    /// `tf_tree ingest` does; the consequence was that all eleven parsed on
    /// every `doctor` invocation and were dropped. The rejection is only as
    /// good as [`IngestArgs::flags_set`] being able to tell a set flag from a
    /// default, so this pins both directions.
    ///
    /// Mutant: make `flags_set` return `Vec::new()`. Applied: the second
    /// assertion fails.
    #[test]
    fn an_ingest_flag_left_at_its_default_is_not_reported_as_set() {
        assert_eq!(IngestArgs::default().flags_set(), Vec::<&str>::new());

        let a = IngestArgs {
            tf_prefix: Some("robot1".to_owned()),
            max_memory: 64,
            ..IngestArgs::default()
        };
        assert_eq!(a.flags_set(), vec!["--max-memory", "--tf-prefix"]);

        // `to_bits`, not `==`: a NaN horizon differs from the default rather
        // than comparing unequal to itself and reporting the flag on every run.
        let b = IngestArgs {
            future_horizon: f64::NAN,
            ..IngestArgs::default()
        };
        assert_eq!(b.flags_set(), vec!["--future-horizon"]);
        assert_eq!(IngestArgs::default().flags_set(), Vec::<&str>::new());
    }

    /// **"Cannot tell" is not "dead", and every arm that could have said
    /// otherwise is here.**
    ///
    /// `record_is_alive`'s doc states the bias this test enforces: *a false
    /// "dead" lets a rescuer take an entry from a running process, which is
    /// corruption; a false "alive" only delays recovery*. `doctor` is where an
    /// operator decides what to kill, so the corresponding corruption is
    /// telling somebody a running process is gone — and `TFT014`'s fork arm
    /// fires on *byte held plus process gone*, so a `/proc` failure classified
    /// as death puts a `warn` on a healthy publisher.
    ///
    /// The revision this replaced was
    /// `Err(_) if self_start_time().is_ok() => Gone`, which called **every**
    /// non-`ENOENT` failure and every parse failure death. The three `Unknown`
    /// rows below are the ones it got wrong; on a `hidepid=2` mount or under
    /// `EMFILE`, every one of them is a live participant.
    ///
    /// `proc_answers` and the probe are parameters for the reason
    /// `alive_given`'s are: neither is arrangeable from a test. Staging pid
    /// reuse means exhausting the pid space, and unmounting `/proc` is not
    /// something a test suite may do to the machine it runs on.
    ///
    /// Mutant: restore `Err(_) if proc_answers => Gone` as a single arm.
    /// Applied: it panicked on the `EACCES` row with *left: Gone, right:
    /// Unknown* — a `hidepid` host's every participant reported as a dead
    /// process. The `EMFILE` and `Parse` rows go the same way behind it.
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
        // The `docs/decisions/0033` facts, held at "the observer is standing
        // where the record was written": same namespace, and a `/proc` that is
        // this process's own. Every row below is then about the `/proc`
        // classification alone, which is what it was about before those two
        // parameters existed. The rows where they are *not* held are the test
        // beneath this one.
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

    /// A namespace inode standing in for "the observer's own". The value is
    /// arbitrary — nothing here reads a real one — but it must be nonzero,
    /// because zero is the *unknown namespace* marker and would disable the
    /// guard rather than exercise it.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    const HERE: u64 = 4_026_531_836;
    /// A second, different namespace. The two nsfs inums measured in
    /// `docs/decisions/0033`; they are one allocator's numbers, so a real pair
    /// differs by very little, which is worth reproducing rather than picking
    /// `1` and `2`.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    const ELSEWHERE: u64 = 4_026_532_488;

    /// **A pid from another PID namespace is not a pid this `/proc` can be
    /// asked about, and until `docs/decisions/0033` `doctor` asked anyway.**
    ///
    /// The two guards answer different questions and the rows below are
    /// arranged to show that rather than to state it: the namespace-mismatch
    /// rows all hold `proc_is_ours = true`, and the `proc_is_ours = false` rows
    /// all hold the namespaces *equal*. Neither guard can carry the other's
    /// rows, which is the whole reason there are two.
    ///
    /// The four arms `0033` stages are (A) a namespaced participant seen from
    /// the host, which takes `Ok(_)`; (B) a host participant seen from a
    /// container, which takes `ENOENT`; (C) a genuine surviving fork inheritor,
    /// which takes `ENOENT` too and whose finding renders byte-identical to
    /// A's; and (D) participant and observer both inside one bare
    /// `unshare --fork --pid`, where every namespace matches and `/proc` is the
    /// parent's. A and B are the first block, C is the row that must stay
    /// `Gone`, D is the second block. The end-to-end stagings are
    /// `tests/attach.rs`'s `tft014_namespace_*`.
    ///
    /// Mutant: drop the `recorded_pid_ns != 0` conjunct. Applied: the
    /// pre-`0033`-record row fails with *left: Unknown, right: Gone* — every
    /// record written before the field existed becomes unclassifiable, which is
    /// `TFT014` unable to fire rather than fixed.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[test]
    fn a_pid_from_another_namespace_is_not_a_pid_this_proc_can_answer_about() {
        use doctor::RecordedProcess as R;

        const STORED: u64 = 4242;
        let enoent = Err(tf_tree_ipc::ProcError::Unreadable {
            pid: 7,
            raw_os_error: 2,
        });

        // Arm A: the recorded pid exists here — it is pid 1, `systemd` — and
        // its start time differs, so the probe succeeds and the classification
        // below would call it `Gone`. It is a live participant one namespace
        // away.
        assert_eq!(
            recorded_given(STORED, Ok(STORED + 1), true, ELSEWHERE, HERE, true),
            R::Unknown,
            "a recycled-pid verdict about a pid from another numbering is not \
             a verdict"
        );
        // Arm B, the mirror: the recorded pid is not in this `/proc` at all.
        // Same fault, the other arm — which is why the guard is before the
        // match and not inside it.
        assert_eq!(
            recorded_given(STORED, enoent, true, ELSEWHERE, HERE, true),
            R::Unknown,
            "ENOENT about a pid this /proc does not number proves nothing"
        );
        // Arm C, the true positive, and the reason the guard is not written at
        // an arm: it takes the same `ENOENT` arm as B and renders the same
        // text. Only the namespace separates them.
        assert_eq!(
            recorded_given(STORED, enoent, true, HERE, HERE, true),
            R::Gone,
            "a fork inheritor in the observer's own namespace is exactly what \
             TFT014 exists to report"
        );

        // Zero is *unknown namespace*, on either side, and must not fire the
        // guard: a record written before `0033` reads zero, and so does one
        // whose writer could not read `/proc`.
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

        // Arm D. Every namespace here matches — that is what makes it arm D
        // rather than arm A — so the guard above is silent and this one is the
        // only thing between `doctor` and a finding about its own slot.
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
