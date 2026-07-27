#![forbid(unsafe_code)]
//! Bag ingestion for `tf_tree` — `docs/PHASE5.md` §3.
//!
//! Reads an MCAP recording's `tf2_msgs/msg/TFMessage` traffic and produces a
//! [`tf_tree::Tree`], or (with `--features shm`) a frozen `.tft` index, plus an
//! **ingest report** that §3.2 calls a first-class output.
//!
//! # Why this is its own crate
//!
//! It cannot live where the rest of the engine lives. `tf_tree_core` and
//! `tf_tree_arena` have fixed dependency budgets that `mcap` would break, and
//! `docs/PHASE5.md` §3's opening note says so. That leaves `tf_tree_cli` or a
//! new crate, and this is a new crate for two reasons:
//!
//! - §4's offline Python API and §3.3's `freeze_from_arrays` both need this
//!   logic, and `tf_tree_py` cannot depend on a binary crate. Putting it in the
//!   CLI would mean moving it later, across a release.
//! - `tf_tree_cli` is `#![forbid(unsafe_code)]`, small, and about *printing*.
//!   A ~1 500-line ingest engine with a CDR decoder inside it is not a
//!   subcommand; it is a library the subcommand calls, and it wants its own
//!   tests, its own feature flags and its own dependency surface.
//!
//! The cost is one more workspace member. The benefit is that `cargo nextest
//! run --workspace` gates the two-pass logic on every host, with no feature
//! flag and no container — the frozen-file half is what needs `shm`.
//!
//! # What this build cannot read
//!
//! `mcap` is taken with `default-features = false`, which `docs/PHASE5.md` §0.0
//! requires: its defaults are `[zstd, lz4]` and both vendor a C build step that
//! `docs/PHASE2.md` §2 forbids. The consequence is not hypothetical — a
//! recording whose chunks are zstd-compressed (Foxglove's default, and
//! `rosbag2`'s when `compression_mode` is set) fails with
//! [`IngestError::CompressedChunk`]. The remedy is one command with the
//! upstream tool:
//!
//! ```text
//! mcap compress --compression none in.mcap -o out.mcap
//! ```
//!
//! That is a worse story than transparently decompressing, and it is the story
//! the no-C-build-step rule buys. It is surfaced as its own error variant, with
//! that command in the CLI's message, rather than as a generic parse failure.
//!
//! # Status against §3
//!
//! Implemented: §3.1's two passes, §3.3's MCAP source, and every row of §3.2
//! except `--on-clock-reset=split`, which is refused with a reason
//! ([`IngestError::ClockResetSplitUnsupported`]) rather than silently doing
//! something else. `rosbag2` sqlite3 (§3.3, "lower priority") and
//! `freeze_from_arrays` are not here.

use std::path::Path;

pub mod cdr;
pub mod ingest;
pub mod report;
pub mod source;

#[cfg(feature = "fixture")]
pub mod fixture;

/// Writing a `.tft` from a recording (`docs/PHASE5.md` §2 + §3).
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod tft;

pub use ingest::{
    fill, survey, Anomalies, ClockResetPolicy, EdgeSurvey, FillStats, Frames, IngestOptions,
    Survey, DEFAULT_FUTURE_HORIZON_NS, DEFAULT_MAX_MEMORY_BYTES,
};
pub use report::IngestReport;
pub use source::TopicRoles;

/// An index into [`Frames`], which is how a `Copy` error names a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FrameId(pub u32);

/// Why an ingest failed.
///
/// `Copy` and `String`-free (`docs/PROJECT.md` §5), and the two variants that
/// are about a specific edge name it — by [`FrameId`], which indexes the
/// [`Frames`] table the caller passed in. That is why [`survey`] takes the
/// table as an `&mut` parameter instead of returning it: a failed pass still has
/// to be able to say *which* edge, and an index without its table cannot.
/// [`describe`] does the join.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IngestError {
    /// The recording could not be opened or read.
    #[error("could not read the recording (errno {raw_os_error})")]
    Io {
        /// `errno`, or `0` if the platform did not supply one.
        raw_os_error: i32,
    },
    /// The file is not a well-formed MCAP.
    #[error("the file is not a well-formed MCAP recording")]
    Mcap,
    /// A chunk uses zstd or lz4, which this build cannot decompress. See the
    /// crate docs for why, and for the one command that fixes it.
    #[error("the recording uses compressed chunks, which this build cannot read")]
    CompressedChunk,
    /// A `TFMessage` payload could not be decoded.
    #[error("bad TFMessage payload: {0}")]
    Cdr(cdr::CdrError),
    /// No TF channel in the recording carried a decodable transform.
    #[error("the recording contains no tf2_msgs/msg/TFMessage transforms")]
    NoTransforms,
    /// An edge appeared on both a static and a dynamic topic (§3.2 — a hard
    /// error naming the timestamp).
    #[error("edge {parent:?} -> {child:?} changed kind at stamp {stamp_ns}")]
    EdgeKindChanged {
        /// Parent frame.
        parent: FrameId,
        /// Child frame.
        child: FrameId,
        /// The stamp at which the contradiction was found.
        stamp_ns: i64,
    },
    /// The clock jumped backwards past the reset threshold, under
    /// [`ClockResetPolicy::Halt`].
    #[error("clock reset at stamp {at_ns} (backwards by {by_ns} ns)")]
    ClockReset {
        /// The stamp that regressed.
        at_ns: i64,
        /// How far back it went.
        by_ns: i64,
    },
    /// §3.2's `split` policy, which is not implemented.
    #[error("--on-clock-reset=split is not implemented; use halt and split the recording")]
    ClockResetSplitUnsupported,
    /// A single edge's samples exceed `--max-memory` on their own.
    #[error("edge {parent:?} -> {child:?} needs {needed_bytes} B, over the {cap_bytes} B cap")]
    EdgeExceedsMemoryCap {
        /// Parent frame.
        parent: FrameId,
        /// Child frame.
        child: FrameId,
        /// Bytes the edge's buffer would need.
        needed_bytes: u64,
        /// The configured cap.
        cap_bytes: u64,
    },
    /// The surveyed topology could not be allocated.
    #[error("could not build the tree: {0}")]
    Build(tf_tree::BuildError),
    /// An edge could not be claimed for writing.
    #[error("could not claim an edge: {0}")]
    Claim(tf_tree::ClaimApiError),
    /// A sample was rejected by the engine.
    ///
    /// `{0:?}` and not `{0}`: `PushError` is a `tf_tree_core` type and the core
    /// is `no_std` with no `Display` impls — `tf_tree::Described` is what
    /// renders those, and it needs a `Tree`, which a failing push does not have
    /// to hand.
    #[error("push rejected: {0:?}")]
    Push(tf_tree::PushError),
    /// A surveyed frame was not present in the built tree. Structurally
    /// impossible — every surveyed name is declared on the builder — and kept
    /// as an error rather than an `unwrap` because this crate denies both.
    #[error("frame {frame:?} was surveyed but is not in the built tree")]
    FrameLost {
        /// The missing frame.
        frame: FrameId,
    },
    /// Writing the `.tft` failed.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[error("could not write the .tft: {0}")]
    Frozen(tf_tree::FrozenFileError),
}

/// An [`IngestError`] with the frame table needed to print its names.
///
/// The same shape as [`tf_tree::Described`], and for the same reason: the error
/// stays `Copy` and the names stay out of it.
#[derive(Clone, Copy, Debug)]
pub struct Described<'a>(pub IngestError, pub &'a Frames);

impl core::fmt::Display for Described<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let n = |id: FrameId| self.1.name(id);
        match self.0 {
            IngestError::EdgeKindChanged {
                parent,
                child,
                stamp_ns,
            } => write!(
                f,
                "edge {} -> {} is published on both /tf and /tf_static; \
                 the contradiction appears at stamp {stamp_ns}",
                n(parent),
                n(child)
            ),
            IngestError::EdgeExceedsMemoryCap {
                parent,
                child,
                needed_bytes,
                cap_bytes,
            } => write!(
                f,
                "edge {} -> {} alone needs {needed_bytes} B of buffer, over the \
                 {cap_bytes} B --max-memory cap; raise the cap",
                n(parent),
                n(child)
            ),
            IngestError::FrameLost { frame } => {
                write!(f, "frame {} was surveyed but is not in the tree", n(frame))
            }
            other => write!(f, "{other}"),
        }
    }
}

/// Join an error to the frame table so it can name frames.
#[must_use]
pub fn describe(err: IngestError, frames: &Frames) -> Described<'_> {
    Described(err, frames)
}

/// A completed ingest: the tree, the frames that name it, and the report.
pub struct Ingested {
    /// The filled tree.
    pub tree: tf_tree::Tree,
    /// The interned frame names.
    pub frames: Frames,
    /// What pass one found.
    pub survey: Survey,
    /// The report §3.2 calls a first-class output.
    pub report: IngestReport,
}

/// Run both passes over `path`.
///
/// The `frames` table is an `&mut` parameter for the reason [`IngestError`]
/// documents: a failure has to be able to name an edge.
///
/// # Errors
///
/// Any [`IngestError`]. Use [`describe`] to render one with frame names.
pub fn run(
    path: &Path,
    opts: &IngestOptions,
    frames: &mut Frames,
) -> Result<Ingested, IngestError> {
    let survey = survey(path, opts, frames)?;
    let (tree, stats) = fill(path, opts, &survey, frames)?;
    let report = IngestReport::new(path, &survey, frames, stats);
    Ok(Ingested {
        tree,
        frames: frames.clone(),
        survey,
        report,
    })
}
