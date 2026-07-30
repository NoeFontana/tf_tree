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
//! # What this build can and cannot read
//!
//! `mcap` is taken with `default-features = false`, which `docs/PHASE5.md` §0.0
//! requires: its defaults are `[zstd, lz4]` and both vendor a C build step that
//! `docs/PHASE2.md` §2 forbids. **That rule still holds and the cost it used to
//! carry is gone.** `crate::decompress` takes each chunk over whole and decodes
//! it with pure-Rust codecs of our own — `ruzstd` and `lz4_flex`, behind the
//! default-on `compression` feature — so a zstd- or lz4-compressed recording
//! (Foxglove's default, and `rosbag2`'s when `compression_mode` is set) ingests
//! transparently, with no `*-sys` crate in the graph and no C toolchain.
//!
//! An earlier revision of this section told the user to run
//! `mcap compress --compression none` instead. That is no longer the ordinary
//! path; it is the remedy for the two cases that remain:
//!
//! * a codec neither we nor the MCAP specification names, which is
//!   [`IngestError::CompressedChunk`] with [`ChunkCodec::Other`], and
//! * a build with `--no-default-features`, where zstd and lz4 are compiled out
//!   and report themselves as exactly that rather than as corruption.
//!
//! Decompression is bounded: `uncompressed_size` is a number off a disk, so
//! [`IngestOptions::max_chunk_uncompressed_bytes`] and
//! [`IngestOptions::max_chunk_expansion_ratio`] are checked before anything is
//! allocated for it. Neither codec crate bounds its total output for us —
//! `crate::decompress` says exactly what each of them does bound.
//!
//! # Status against §3
//!
//! Implemented: §3.1's two passes **including the spill-to-run-file**, §3.3's
//! MCAP source, and every row of §3.2 except `--on-clock-reset=split`, which is
//! refused with a reason ([`IngestError::ClockResetSplitUnsupported`]) rather
//! than silently doing something else. `docs/PHASE5.md` §3.2 carries the
//! argument for leaving it refused; it is a decision, not a backlog entry.
//! `rosbag2` sqlite3 (§3.3, "lower priority") and `freeze_from_arrays` are not
//! here.

use std::path::Path;

pub mod cdr;
pub mod ingest;
pub mod report;
pub mod source;

/// §3.1's spill-to-run-file. Private: it is a strategy `ingest::fill` chooses,
/// not a surface a caller picks — the only knob is
/// [`IngestOptions::spill_dir`](ingest::IngestOptions::spill_dir).
mod spill;

/// Chunk handling: taking MCAP chunks whole and reading inside them.
///
/// Private for the same reason as [`spill`] — it is how `source` reads a file,
/// not a choice a caller makes. What a caller sees is that a compressed
/// recording either works or reports why.
mod decompress;

pub use decompress::{BadChunkKind, ChunkCodec, ChunkLimits};

#[cfg(feature = "fixture")]
pub mod fixture;

/// Writing a `.tft` from a recording (`docs/PHASE5.md` §2 + §3).
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod tft;

pub use ingest::{
    fill, survey, Anomalies, ClockResetPolicy, EdgeSurvey, FillStats, Frames, IngestOptions,
    Survey, DEFAULT_FUTURE_HORIZON_NS, DEFAULT_MAX_CHUNK_EXPANSION_RATIO,
    DEFAULT_MAX_CHUNK_UNCOMPRESSED_BYTES, DEFAULT_MAX_MEMORY_BYTES,
};
pub use report::IngestReport;
pub use source::{ChunkPolicy, OnBadChunk, TopicRoles};

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
    /// §3.1's temporary run file could not be created, written or read back.
    ///
    /// Distinct from [`IngestError::Io`] because the remedy is different and the
    /// user cannot guess which file failed: this one is about the *spill*
    /// directory — a full or read-only `/tmp` — not about the recording, which
    /// was read fine.
    #[error("could not use the spill file (errno {raw_os_error}); check --spill-dir")]
    Spill {
        /// `errno`, or `0` if the platform did not supply one.
        raw_os_error: i32,
    },
    /// The file is not a well-formed MCAP.
    #[error("the file is not a well-formed MCAP recording")]
    Mcap,
    /// The file is a SQLite database — almost certainly a rosbag2 `.db3` bag,
    /// which §3.3 lists as a source and which this crate does not read.
    ///
    /// A variant of its own rather than [`IngestError::Mcap`]: it is the most
    /// likely wrong file to be handed, "not a well-formed MCAP recording" is a
    /// true statement that sends the user hunting for corruption, and the remedy
    /// (`ros2 bag convert`) is one command. See the amendment at
    /// `docs/PHASE5.md` §3.3 for why the reader is absent.
    #[error("the file is a SQLite database, not an MCAP recording")]
    Rosbag2Sqlite,
    /// A chunk names a codec this build has no decoder for.
    ///
    /// **Not the ordinary compressed recording.** zstd and lz4 are decoded
    /// transparently by the default-on `compression` feature, so reaching this
    /// means either a codec name outside the MCAP specification
    /// ([`ChunkCodec::Other`]) or a `--no-default-features` build. See the crate
    /// docs, and note that a chunk which *claims* zstd and carries something else
    /// is [`IngestError::BadChunk`] instead — that is damage, not a missing
    /// decoder.
    ///
    /// **Never skippable, unlike [`IngestError::BadChunk`].** Every chunk in a
    /// recording uses the same codec, so skipping them all would yield
    /// [`IngestError::NoTransforms`] — an answer that explains nothing about a
    /// file which is perfectly intact.
    #[error("the recording uses {codec}-compressed chunks, which this build cannot read")]
    CompressedChunk {
        /// Which codec, as far as it could be identified.
        codec: decompress::ChunkCodec,
    },
    /// A chunk was unreadable and the policy was
    /// [`OnBadChunk::Halt`](ingest::OnBadChunk::Halt).
    ///
    /// It names the chunk, and that is not decoration — the same reason
    /// [`cdr::CdrError`] carries a byte offset. "Chunk 3 of 812 failed its CRC" is
    /// a damaged recording; "chunk 0 failed", and then 811 more, is a file that is
    /// not what it claims to be, and the two want different responses.
    #[error("chunk {chunk} is unreadable: {kind}")]
    BadChunk {
        /// Zero-based index of the chunk in read order.
        ///
        /// An ordinal rather than a byte offset because it is what `mcap info`
        /// numbers chunks by, and so is directly comparable against it.
        chunk: u64,
        /// What was wrong with it.
        kind: decompress::BadChunkKind,
    },
    /// A `TFMessage` payload could not be decoded.
    #[error("bad TFMessage payload: {0}")]
    Cdr(cdr::CdrError),
    /// No TF channel in the recording carried a decodable transform.
    #[error("the recording contains no tf2_msgs/msg/TFMessage transforms")]
    NoTransforms,
    /// The recording is truncated and its surviving prefix held no transform —
    /// distinct from [`IngestError::NoTransforms`], which means the recording is
    /// whole and simply has no TF in it.
    ///
    /// **Why this needs its own variant.** The two are indistinguishable from the
    /// outside and have opposite remedies. Reporting an incomplete file as "this
    /// recording contains no transforms" sends the user looking for a publisher
    /// that was never running, when what they have is a recorder that was killed.
    #[error("the recording is truncated, and the part that survived holds no transforms")]
    TruncatedBeforeAnyChunk,
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
    /// One edge's stamps jumped backwards past the reset threshold, under
    /// [`ClockResetPolicy::Halt`].
    ///
    /// **It names the edge**, and that is not decoration. The guard is per edge
    /// (`ingest`'s module docs say why), so the only honest thing this error can
    /// report is *which* edge regressed — an earlier revision watched the merged
    /// stream and said "clock reset" when the truth was two publishers with
    /// different latencies, which is a diagnosis a user cannot act on.
    #[error(
        "clock reset on edge {parent:?} -> {child:?} at stamp {at_ns} (backwards by {by_ns} ns)"
    )]
    ClockReset {
        /// Parent frame of the regressing edge.
        parent: FrameId,
        /// Child frame of the regressing edge.
        child: FrameId,
        /// The stamp that regressed.
        at_ns: i64,
        /// How far back it went.
        by_ns: i64,
    },
    /// §3.2's `split` policy, which is not implemented.
    #[error("--on-clock-reset=split is not implemented; use halt and split the recording")]
    ClockResetSplitUnsupported,
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
            IngestError::ClockReset {
                parent,
                child,
                at_ns,
                by_ns,
            } => write!(
                f,
                "edge {} -> {} jumped {by_ns} ns backwards at stamp {at_ns}, past the \
                 reset threshold; raise --clock-reset-threshold if this publisher is \
                 merely late rather than replayed",
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
