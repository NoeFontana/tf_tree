#![forbid(unsafe_code)]
//! Bag ingestion for `tf_tree` — `docs/PHASE5.md` §3.
//!
//! Reads an MCAP recording's `tf2_msgs/msg/TFMessage` traffic into a
//! [`tf_tree::Tree`], or (with `--features shm`) a frozen `.tft`, plus an ingest
//! report (§3.2).
//!
//! # Compression
//! `mcap` is taken with `default-features = false` (`docs/PHASE5.md` §0.0). zstd
//! and lz4 chunks decode with pure-Rust codecs behind the default-on
//! `compression` feature. An unnamed codec ([`IngestError::CompressedChunk`] with
//! [`ChunkCodec::Other`]) and a `--no-default-features` build are unreadable;
//! the remedy is `mcap compress --compression none`. Decompression is bounded
//! by [`IngestOptions::max_chunk_uncompressed_bytes`],
//! [`IngestOptions::max_chunk_expansion_ratio`] and a zstd window limit
//! (`crate::decompress`).
//!
//! # Status against §3
//! Implemented: §3.1's passes, §3.3's MCAP source, and every row of §3.2 except
//! `--on-clock-reset=split` ([`IngestError::ClockResetSplitUnsupported`]). Not
//! here: `rosbag2` sqlite3 and `freeze_from_arrays`.
//!
//! **No time-domain detection** (§3.1's amendment): every ingested edge takes
//! `TreeBuilder`'s default (`SystemDomain`, tag 0).
//!
//! Every test reads a recording this crate wrote (`crate::fixture`), so the
//! suite gates this reader's bookkeeping, not agreement with a real `rosbag2`
//! writer (`testdata/ATTRIBUTION.md`).

use std::path::Path;

pub mod cdr;
pub mod ingest;
pub mod report;
pub mod source;

/// §3.1's spill-to-run-file; the only knob is
/// [`IngestOptions::spill_dir`](ingest::IngestOptions::spill_dir).
mod spill;

/// Chunk handling: taking MCAP chunks whole and reading inside them.
mod decompress;

pub use decompress::{BadChunkKind, ChunkCodec, ChunkLimits};

/// Whether this build compiled the zstd and lz4 chunk decoders in.
/// Evaluated here because a consumer's `cfg!(feature = "compression")` reports
/// what *it* asked for (Cargo unifies features). Mirrors
/// [`tf_tree::counters_compiled_in`].
#[must_use]
pub fn compression_compiled_in() -> bool {
    cfg!(feature = "compression")
}

#[cfg(feature = "fixture")]
pub mod fixture;

/// BLAKE3 of a recording's bytes, streamed in 1 MiB chunks (§2.3).
///
/// Not behind the `shm` cfg (`docs/decisions/0046`).
///
/// # Errors
///
/// [`IngestError::Io`] carrying the `errno`, for a file that cannot be read.
pub fn digest_file(path: &std::path::Path) -> Result<[u8; 32], IngestError> {
    use std::io::Read as _;
    let io = |e: &std::io::Error| IngestError::Io {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    };
    let mut f = std::fs::File::open(path).map_err(|e| io(&e))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| io(&e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod tft;

pub use ingest::{
    fill, survey, Anomalies, ClockResetPolicy, EdgeSurvey, FillStats, Frames, IngestOptions,
    StaticConflict, Survey, DEFAULT_FUTURE_HORIZON_NS, DEFAULT_MAX_CHUNK_EXPANSION_RATIO,
    DEFAULT_MAX_CHUNK_UNCOMPRESSED_BYTES, DEFAULT_MAX_MEMORY_BYTES, DEFAULT_MAX_RECORD_BYTES,
};
pub use report::{IngestReport, StaticConflictRow};
pub use source::{OnBadChunk, ReadPolicy, TopicRoles};

/// An index into [`Frames`], which is how a `Copy` error names a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FrameId(pub u32);

/// Why an ingest failed.
///
/// `Copy` and `String`-free (D11); edge variants name frames by [`FrameId`].
/// [`describe`] joins the names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IngestError {
    /// The recording could not be opened or read.
    #[error("could not read the recording (errno {raw_os_error})")]
    Io {
        /// `errno`, or `0` if the platform did not supply one.
        raw_os_error: i32,
    },
    /// §3.1's temporary run file could not be created, written or read back —
    /// the *spill* directory, not the recording.
    #[error("could not use the spill file (errno {raw_os_error}); check --spill-dir")]
    Spill {
        /// `errno`, or `0` if the platform did not supply one.
        raw_os_error: i32,
    },
    /// The file is not a well-formed MCAP.
    #[error("the file is not a well-formed MCAP recording")]
    Mcap,
    /// A top-level record declared a body larger than the reader will allocate
    /// for — [`IngestOptions::max_record_bytes`], `--max-record-size` at the CLI.
    ///
    /// Not [`IngestError::Mcap`]: the file may be well formed. `declared` is the
    /// value to pass to `--max-record-size`.
    #[error(
        "a record declared {declared} bytes, over the {ceiling}-byte ceiling; \
         raise --max-record-size"
    )]
    RecordTooLarge {
        /// The length the record header declared.
        declared: u64,
        /// The ceiling it was measured against.
        ceiling: u64,
    },
    /// The file is a SQLite database — almost certainly a rosbag2 `.db3` bag,
    /// which this crate does not read; the remedy is `ros2 bag convert`
    /// (`docs/PHASE5.md` §3.3).
    #[error("the file is a SQLite database, not an MCAP recording")]
    Rosbag2Sqlite,
    /// A chunk names a codec this build has no decoder for.
    ///
    /// Never skippable: every chunk shares the codec, so skipping would yield a
    /// misleading [`IngestError::NoTransforms`].
    #[error("the recording uses {codec}-compressed chunks, which this build cannot read")]
    CompressedChunk {
        /// Which codec, as far as it could be identified.
        codec: decompress::ChunkCodec,
    },
    /// A chunk was unreadable and the policy was [`OnBadChunk::Halt`].
    #[error("chunk {chunk} is unreadable: {kind}")]
    BadChunk {
        /// Zero-based index in read order, as `mcap info` numbers chunks.
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
    /// The recording is truncated and its surviving prefix held no transform;
    /// [`IngestError::NoTransforms`] means a whole recording with no TF.
    #[error("the recording is truncated, and the part that survived holds no transforms")]
    TruncatedBeforeAnyChunk,
    /// Every chunk that could have held a transform was refused by one of this
    /// reader's own limits; the remedy is a flag. The refusals stay skippable.
    #[error(
        "every chunk was refused by this reader's limits ({skipped} of them), so nothing was read"
    )]
    AllChunksOverLimit {
        /// How many chunks were refused that way.
        skipped: u64,
    },
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
    /// `at_ns` is the regressed stamp; `at_log_time_ns` is the recorder's monotone
    /// clock.
    #[error(
        "clock reset on edge {parent:?} -> {child:?} at stamp {at_ns} \
         (log time {at_log_time_ns}, backwards by {by_ns} ns)"
    )]
    ClockReset {
        /// Parent frame of the regressing edge.
        parent: FrameId,
        /// Child frame of the regressing edge.
        child: FrameId,
        /// The stamp that regressed.
        at_ns: i64,
        /// The recorder's log time for the record carrying that stamp.
        at_log_time_ns: i64,
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
    #[error("push rejected: {0}")]
    Push(tf_tree::PushError),
    /// A surveyed frame was not present in the built tree. Structurally
    /// impossible; an error rather than an `unwrap`.
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
/// Same shape as [`tf_tree::Described`] (`docs/API.md` §R5); fields are private.
#[derive(Clone, Copy, Debug)]
pub struct Described<'a>(IngestError, &'a Frames);

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
                at_log_time_ns,
                by_ns,
            } => write!(
                f,
                "edge {} -> {} jumped {by_ns} ns backwards at stamp {at_ns}, past the \
                 reset threshold; the recording's own log time there is {at_log_time_ns}, \
                 which is where to cut it. Raise --clock-reset-threshold if this \
                 publisher is merely late rather than replayed",
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
    /// The ingest report.
    pub report: IngestReport,
}

/// Run both passes over `path`.
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
