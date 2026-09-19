//! The two passes — `docs/PHASE5.md` §3.1, NORMATIVE.
//!
//! # Why two
//!
//! `TreeBuilder::build()` needs every frame, edge kind and ring capacity before it
//! allocates (`docs/PROJECT.md` §5 D4), and a recording tells you none of that
//! until read. Pass one discovers topology and counts; pass two fills.
//!
//! # Divergence from the live bridge
//!
//! Pure decisions are reused from `tf_tree_bridge` — name normalization (§5.6),
//! the static store (§5.7), the clock guard (§5.5) — so a recording and a live
//! system classify a stream alike. **A backward stamp is dropped online and kept
//! offline**: §3.1 sorts each edge before pushing, so the sample lands correctly.
//!
//! # The guard is per edge
//!
//! `/tf` interleaves publishers whose stamps differ by pipeline latency (hundreds
//! of ms), so a guard over the merged stream reports a reset on an ordinary
//! recording. One [`ClockGuard`] per edge matches §3.1's per-edge sort and Phase 1
//! invariant 6, and still catches a real reset, where every edge regresses at once
//! (`docs/decisions/0011`; `tf_tree_bridge` does the same).
//!
//! The halves differ in promotion, not scope. **Offline, the first regressing edge
//! halts**: the recording is closed and a human reads the answer. **Online, one
//! witness never halts**: an authoritative time-source jump or a common-mode step
//! across two or more publishers promotes; anything less is dropped and counted,
//! because halting an unattended robot on one node's restart is an outage, and
//! attribution must not be a correctness dependency (`docs/PHASE4.md` §5.3).
//!
//! # The reference clock is not the one under test
//!
//! Offline, that reference is [`RawRecord::log_time_ns`], the recorder's clock.
//! The future-stamp anomaly is `stamp - log_time` against a horizon, and
//! [`IngestError::ClockReset`] reports both coordinates because after a rewind
//! only the log time locates a place in the file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tf_tree::{Capacity, EdgeCfg, Iso3, Quat, Tree, TreeBuilder, Vec3};
use tf_tree_bridge::clock::{ClockGuard, ClockVerdict, OnClockReset};
use tf_tree_bridge::names::NameNormalizer;
use tf_tree_bridge::statics::{StaticStore, StaticVerdict};
use tf_tree_bridge::Publisher;

use crate::decompress::ChunkLimits;
use crate::source::{read_tf, OnBadChunk, RawRecord, ReadPolicy, TopicRoles};
use crate::spill;
use crate::{FrameId, IngestError};

/// Bytes one buffered sample costs during pass two: an `i64` stamp beside the
/// canonical `[f64; 7]` pose (the order the push takes, so no per-sample
/// conversion).
const SAMPLE_BYTES: u64 = 8 + 7 * 8;

/// Default `--max-memory` (§3.1): 4 GiB.
pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Default horizon past a message's own log time before its stamp counts as
/// "far in the future" (§3.2): **10 seconds**. Below a second this fires on
/// sensor pipelines that stamp at capture and publish after processing.
pub const DEFAULT_FUTURE_HORIZON_NS: i64 = 10_000_000_000;

/// Default ceiling on one chunk's declared `uncompressed_size`: **64 MiB**,
/// an order above what recorders write (1–8 MiB).
pub const DEFAULT_MAX_CHUNK_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;

/// Default ceiling on one chunk's `uncompressed_size / compressed_size`: **1024**.
///
/// The absolute ceiling alone is not a bomb guard; real CDR chunks compress at
/// roughly 2–5×, so 1024 admits every plausible recording.
pub const DEFAULT_MAX_CHUNK_EXPANSION_RATIO: u64 = 1024;

/// Default ceiling on one top-level MCAP record's declared body length, in bytes.
///
/// **256 MiB.** An oversized record the reader does not need is stepped over and
/// counted in [`Anomalies::oversized_records_skipped`]; one it does need (`Chunk`,
/// `Schema`, `Channel`, `Message`) refuses, since skipping loses transforms
/// silently. The skip length is unvalidated, so `source` requires it to land on a
/// plausible record boundary (else `RecordTooLarge`), and no report row claims the
/// stream past a skip is intact (`docs/decisions/0010`).
pub const DEFAULT_MAX_RECORD_BYTES: u64 = 256 * 1024 * 1024;

/// How ingest should handle a backward clock jump (§3.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClockResetPolicy {
    /// Stop, naming the timestamp. The default, as in
    /// `tf_tree_bridge::clock::OnClockReset`: silently merged clocks answer every
    /// query wrongly.
    #[default]
    Halt,
    /// §3.2's `split`, **not implemented** — see
    /// [`IngestError::ClockResetSplitUnsupported`]. A variant so the CLI can refuse
    /// it with the reason.
    Split,
}

/// Knobs for one ingest.
#[derive(Clone, Debug)]
pub struct IngestOptions {
    /// Which topics carry static transforms, and which to read at all.
    pub roles: TopicRoles,
    /// Peak bytes of buffered samples allowed during pass two (§3.1).
    pub max_memory_bytes: u64,
    /// What to do on a backward clock jump.
    pub on_clock_reset: ClockResetPolicy,
    /// What to do about a chunk that does not decompress or does not check out.
    pub on_bad_chunk: OnBadChunk,
    /// How far backwards a stamp must jump to count as a reset rather than
    /// ordinary interleaving. Defaults to the bridge's own threshold.
    pub clock_reset_threshold_ns: i64,
    /// How far ahead of its log time a stamp may be before it is reported.
    pub future_horizon_ns: i64,
    /// A `tf_prefix` to apply to every frame name (§5.6).
    pub tf_prefix: Option<String>,
    /// Ceiling on one chunk's declared `uncompressed_size`, in bytes.
    ///
    /// Defaults to [`DEFAULT_MAX_CHUNK_UNCOMPRESSED_BYTES`]. Bounds the compressed
    /// path only; the uncompressed path borrows and allocates nothing
    /// (`crate::decompress::chunk_records`).
    pub max_chunk_uncompressed_bytes: u64,
    /// Ceiling on one top-level record's declared body length, in bytes.
    /// Defaults to [`DEFAULT_MAX_RECORD_BYTES`]. Guards the allocation made from a
    /// length read off disk; independent of `max_chunk_uncompressed_bytes`, which
    /// bounds a decompressed chunk.
    pub max_record_bytes: u64,
    /// Ceiling on one chunk's `uncompressed_size / compressed_size` — the second
    /// half of the decompression-bomb guard. Defaults to
    /// [`DEFAULT_MAX_CHUNK_EXPANSION_RATIO`].
    pub max_chunk_expansion_ratio: u64,
    /// Where §3.1's spill file goes when one edge alone exceeds
    /// [`max_memory_bytes`](IngestOptions::max_memory_bytes). `None` means
    /// `std::env::temp_dir()`.
    ///
    /// A knob because `/tmp` may be a RAM tmpfs, defeating the cap.
    pub spill_dir: Option<PathBuf>,
}

impl Default for IngestOptions {
    fn default() -> IngestOptions {
        IngestOptions {
            roles: TopicRoles::default(),
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            on_clock_reset: ClockResetPolicy::default(),
            on_bad_chunk: OnBadChunk::default(),
            clock_reset_threshold_ns: tf_tree_bridge::clock::DEFAULT_RESET_THRESHOLD_NANOS,
            future_horizon_ns: DEFAULT_FUTURE_HORIZON_NS,
            tf_prefix: None,
            max_chunk_uncompressed_bytes: DEFAULT_MAX_CHUNK_UNCOMPRESSED_BYTES,
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
            max_chunk_expansion_ratio: DEFAULT_MAX_CHUNK_EXPANSION_RATIO,
            spill_dir: None,
        }
    }
}

impl IngestOptions {
    /// The two chunk bounds as the reader wants them.
    #[must_use]
    pub fn chunk_limits(&self) -> ChunkLimits {
        ChunkLimits {
            max_uncompressed_bytes: self.max_chunk_uncompressed_bytes,
            max_expansion_ratio: self.max_chunk_expansion_ratio,
        }
    }

    /// Everything [`read_tf`] needs before it believes a length off disk.
    #[must_use]
    pub fn read_policy(&self) -> ReadPolicy {
        ReadPolicy {
            on_bad: self.on_bad_chunk,
            limits: self.chunk_limits(),
            max_record_bytes: self.max_record_bytes,
        }
    }
}

/// The interned frame names, owned by the caller so a failed pass still has names
/// for its error (`IngestError` names edges by index; `docs/PROJECT.md` §5).
#[derive(Clone, Debug, Default)]
pub struct Frames {
    names: Vec<String>,
    index: BTreeMap<String, u32>,
}

impl Frames {
    /// Intern `name`, returning its stable index.
    pub fn intern(&mut self, name: &str) -> FrameId {
        if let Some(&i) = self.index.get(name) {
            return FrameId(i);
        }
        let i = self.names.len() as u32;
        self.names.push(name.to_owned());
        self.index.insert(name.to_owned(), i);
        FrameId(i)
    }

    /// The index of an already-interned name, or `None`.
    ///
    /// Read-only half of [`intern`](Frames::intern); a miss in pass two means the
    /// transform was dropped in pass one.
    #[must_use]
    pub fn id(&self, name: &str) -> Option<FrameId> {
        self.index.get(name).copied().map(FrameId)
    }

    /// The name behind an index, or `"?"` if it is out of range.
    #[must_use]
    pub fn name(&self, id: FrameId) -> &str {
        self.names.get(id.0 as usize).map_or("?", String::as_str)
    }

    /// Every interned name, in first-seen order.
    #[must_use]
    pub fn all(&self) -> &[String] {
        &self.names
    }

    /// How many frames have been interned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether nothing has been interned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// What pass one learned about one edge.
#[derive(Clone, Debug)]
pub struct EdgeSurvey {
    /// Parent frame index.
    pub parent: FrameId,
    /// Child frame index.
    pub child: FrameId,
    /// The topic it was seen on, for the report.
    pub topic: String,
    /// Its constant value, if it is a static edge.
    pub static_pose: Option<[f64; 7]>,
    /// Dynamic samples that survived pass one's drops.
    pub samples: u64,
    /// Oldest surviving stamp — the true oldest in the source (§2.3's amendment).
    pub source_oldest_ns: Option<i64>,
    /// Newest stamp in the source.
    pub source_newest_ns: Option<i64>,
}

impl EdgeSurvey {
    /// Whether this edge is static.
    #[must_use]
    pub fn is_static(&self) -> bool {
        self.static_pose.is_some()
    }
}

/// One `/tf_static` contradiction, with **both** values (§3.2 — "report both
/// values"; `docs/PHASE4.md` §5.7).
///
/// On [`Survey`], not [`Anomalies`], because `Anomalies` derives `Eq` and
/// `[f64; 7]` has none. One row per edge: only the first occurrence
/// (`StaticVerdict::Conflict::first_time`) is recorded, while
/// [`Anomalies::static_conflicts`] counts every message.
#[derive(Clone, Debug, PartialEq)]
pub struct StaticConflict {
    /// Parent frame of the contradicted edge.
    pub parent: FrameId,
    /// Child frame of the contradicted edge.
    pub child: FrameId,
    /// The value on file — the winner under the first-writer policy.
    pub existing: [f64; 7],
    /// The value that was offered and refused.
    pub offered: [f64; 7],
    /// Which topic declared [`existing`](StaticConflict::existing).
    ///
    /// A topic, not a node name: a recording has no GID or graph
    /// (`Publisher::Topic`), so two publishers on one topic are told apart only by
    /// their values.
    pub declared_by: String,
    /// Which topic offered [`offered`](StaticConflict::offered).
    pub contradicted_by: String,
}

/// Everything §3.2 asks to be counted and reported; none is an error on its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Anomalies {
    /// Samples with `stamp == 0`, dropped (§3.2 — "extremely common").
    pub zero_stamp_drops: u64,
    /// Samples stamped more than the horizon ahead of their own log time.
    pub future_stamps: u64,
    /// The worst such offset seen, in nanoseconds.
    pub worst_future_offset_ns: i64,
    /// Samples whose stamp went backwards by less than the reset threshold —
    /// ordinary interleaving. **Kept**, not dropped; see the module docs.
    pub out_of_order: u64,
    /// Backward jumps beyond the threshold.
    pub clock_resets: u64,
    /// Where the first one was **in stamp space**, if any.
    ///
    /// Never observable today: both [`ClockResetPolicy`] arms error on the first
    /// reset, so read [`IngestError::ClockReset`]. Reserved for `split`.
    pub first_reset_at_ns: Option<i64>,
    /// `/tf_static` samples offering a different value for an already-declared
    /// static edge (§5.7's tolerance).
    pub static_conflicts: u64,
    /// `(edge, stamp)` pairs seen more than once; the last occurrence in log
    /// order wins (Phase 1 invariant 6).
    pub duplicate_stamps: u64,
    /// Names that arrived with a leading `/` and were normalized (§5.6).
    pub stripped_slash_names: u64,
    /// Transforms whose parent or child name was empty, dropped.
    pub empty_names: u64,
    /// **Channels** carrying the TF schema that the operator's own
    /// `--tf-topic`/`--tf-static-topic` narrowing excluded — one per channel id.
    /// Nothing is wrong with the recording when this is non-zero.
    pub filtered_channels: u64,
    /// **Channels** carrying the TF schema whose encoding this build cannot
    /// decode — a `json` or `protobuf` channel under the TF schema name. One per
    /// channel id; separate from [`Self::filtered_channels`] (different remedies).
    pub non_cdr_channels: u64,
    /// The recording stopped mid-record: everything before the cut was read, so
    /// every number in the report describes a prefix.
    pub truncated: bool,
    /// Top-level records the reader does not need that were larger than
    /// `--max-record-size`, and were stepped over rather than refusing the file.
    ///
    /// `source::reader_needs` is the line: a `Chunk`, `Schema`, `Channel` or
    /// `Message` over the ceiling still refuses with
    /// [`crate::IngestError::RecordTooLarge`].
    ///
    /// It says nothing about what the record was or about records after it: the
    /// reader steps over the on-disk length unparsed, and a corrupt length landing
    /// on a later boundary passes every check. It means this run declined to look.
    pub oversized_records_skipped: u64,
    /// Chunks that were unreadable and skipped (`OnBadChunk::Skip`).
    ///
    /// Like [`truncated`](Anomalies::truncated), the counts cover only part of the
    /// recording. A skipped chunk takes its `Schema` and `Channel` records with it,
    /// so later messages on a channel declared only there drop uncounted.
    /// Truncation does not count here.
    pub bad_chunks: u64,
    /// Of [`bad_chunks`](Anomalies::bad_chunks), how many were refused by one of
    /// this reader's limits rather than found damaged; the remedy is a flag. See
    /// [`crate::IngestError::AllChunksOverLimit`].
    pub chunks_over_limit: u64,
    /// The span the skipped chunks covered, from their own declared message times.
    pub bad_chunk_span_ns: Option<(u64, u64)>,
}

/// The output of pass one: an exact topology and the counts that size it.
#[derive(Clone, Debug)]
pub struct Survey {
    /// Every edge, in first-seen order. Edge index is this vector's index.
    pub edges: Vec<EdgeSurvey>,
    /// What was odd about the recording.
    pub anomalies: Anomalies,
    /// §3.2's "report both values": one row per contradicted static edge. Named
    /// `_details` so the JSON key differs from the count
    /// [`Anomalies::static_conflicts`].
    pub static_conflict_details: Vec<StaticConflict>,
    /// Transforms read, before any drop.
    pub transforms_read: u64,
    /// Frame-name remappings applied, as `(raw, normalized)`.
    pub remaps: Vec<(String, String)>,
}

impl Survey {
    /// Dynamic edges that ended pass one with no samples — §3.2's "frame declared,
    /// never published": every sample it carried was dropped.
    #[must_use]
    pub fn edges_without_samples(&self) -> Vec<usize> {
        self.edges
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_static() && e.samples == 0)
            .map(|(i, _)| i)
            .collect()
    }

    /// The recording's overall time span across every dynamic edge.
    #[must_use]
    pub fn span_ns(&self) -> Option<(i64, i64)> {
        let lo = self.edges.iter().filter_map(|e| e.source_oldest_ns).min()?;
        let hi = self.edges.iter().filter_map(|e| e.source_newest_ns).max()?;
        Some((lo, hi))
    }
}

/// Key for the edge table during a pass.
type EdgeKey = (u32, u32);

/// Pass one — count (§3.1).
///
/// # Errors
///
/// [`IngestError::EdgeKindChanged`] when an edge appears on both a static and a
/// dynamic topic (§3.2 calls this a hard error naming the timestamp);
/// [`IngestError::ClockReset`] under [`ClockResetPolicy::Halt`]; whatever
/// [`read_tf`] failed with.
pub fn survey(
    path: &Path,
    opts: &IngestOptions,
    frames: &mut Frames,
) -> Result<Survey, IngestError> {
    let mut normalizer = match &opts.tf_prefix {
        Some(p) => NameNormalizer::with_prefix(p),
        None => NameNormalizer::new(),
    };
    let mut statics = StaticStore::new();
    // One guard per edge; `clocks[i]` and `dynamic_seen[i]` grow with `out.edges[i]`.
    let mut clocks: Vec<ClockGuard> = Vec::new();
    // Dynamic half of the edge-kind check, one bit per slot: asking `StaticStore`
    // by name per transform re-derives what the slot lookup already resolved.
    let mut dynamic_seen: Vec<bool> = Vec::new();
    let mut index: BTreeMap<EdgeKey, usize> = BTreeMap::new();
    let mut out = Survey {
        edges: Vec::new(),
        anomalies: Anomalies::default(),
        static_conflict_details: Vec::new(),
        transforms_read: 0,
        remaps: Vec::new(),
    };

    let skips = read_tf(path, &opts.roles, opts.read_policy(), |rec| {
        out.transforms_read += 1;
        let Some((parent, child)) = normalize_pair(&mut normalizer, &rec, frames) else {
            out.anomalies.empty_names += 1;
            return Ok(());
        };
        let key = (parent.0, child.0);
        let slot = match index.get(&key) {
            Some(&s) => s,
            None => {
                out.edges.push(EdgeSurvey {
                    parent,
                    child,
                    topic: rec.topic.to_owned(),
                    static_pose: None,
                    samples: 0,
                    source_oldest_ns: None,
                    source_newest_ns: None,
                });
                clocks.push(ClockGuard::with_threshold(
                    // The guard's own policy is unused; `ClockResetPolicy` applies.
                    OnClockReset::Halt,
                    opts.clock_reset_threshold_ns,
                ));
                dynamic_seen.push(false);
                let s = out.edges.len() - 1;
                index.insert(key, s);
                s
            }
        };

        if rec.is_static {
            // Dynamic-then-static is caught by `dynamic_seen`, not `StaticStore`.
            if dynamic_seen[slot] {
                return Err(IngestError::EdgeKindChanged {
                    parent,
                    child,
                    stamp_ns: rec.stamp_ns,
                });
            }
            // §5.7's order: compare values first and report both, before any policy
            // picks a survivor.
            match statics.observe_static(
                frames.name(parent),
                frames.name(child),
                rec.pose,
                &Publisher::Topic(rec.topic.to_owned()),
            ) {
                StaticVerdict::Declare => out.edges[slot].static_pose = Some(rec.pose),
                StaticVerdict::Idempotent => {}
                StaticVerdict::Conflict {
                    owner,
                    intruder,
                    existing,
                    offered,
                    first_time,
                } => {
                    out.anomalies.static_conflicts += 1;
                    // `first_time` is `StaticStore`'s own rate limit.
                    if first_time {
                        out.static_conflict_details.push(StaticConflict {
                            parent,
                            child,
                            existing,
                            offered,
                            declared_by: publisher_label(&owner),
                            contradicted_by: publisher_label(&intruder),
                        });
                    }
                }
                StaticVerdict::KindChanged { .. } => {
                    return Err(IngestError::EdgeKindChanged {
                        parent,
                        child,
                        stamp_ns: rec.stamp_ns,
                    })
                }
            }
            // A static's stamp never touches the clock (`docs/PHASE4.md` §5.5):
            // statics are stamped zero, which would drag the high-water mark to
            // the epoch.
            return Ok(());
        }

        // `static_pose` is `Some` exactly when the store holds `Static` for this
        // edge (only `Declare` sets both).
        if out.edges[slot].static_pose.is_some() {
            return Err(IngestError::EdgeKindChanged {
                parent,
                child,
                stamp_ns: rec.stamp_ns,
            });
        }
        dynamic_seen[slot] = true;

        if rec.stamp_ns == 0 {
            out.anomalies.zero_stamp_drops += 1;
            return Ok(());
        }
        // The recorder's clock is the reference; the header stamp cannot check itself.
        let ahead = rec.stamp_ns.saturating_sub(rec.log_time_ns);
        if ahead > opts.future_horizon_ns {
            out.anomalies.future_stamps += 1;
            out.anomalies.worst_future_offset_ns = out.anomalies.worst_future_offset_ns.max(ahead);
        }
        match clocks[slot].observe(rec.stamp_ns) {
            ClockVerdict::Forward => {}
            // Kept, unlike online (module docs).
            ClockVerdict::Jitter { .. } => out.anomalies.out_of_order += 1,
            ClockVerdict::Reset { by_nanos, .. } => {
                out.anomalies.clock_resets += 1;
                if out.anomalies.first_reset_at_ns.is_none() {
                    out.anomalies.first_reset_at_ns = Some(rec.stamp_ns);
                }
                match opts.on_clock_reset {
                    ClockResetPolicy::Halt => {
                        return Err(IngestError::ClockReset {
                            parent,
                            child,
                            at_ns: rec.stamp_ns,
                            at_log_time_ns: rec.log_time_ns,
                            by_ns: by_nanos,
                        })
                    }
                    ClockResetPolicy::Split => return Err(IngestError::ClockResetSplitUnsupported),
                }
            }
        }

        let e = &mut out.edges[slot];
        e.samples += 1;
        e.source_oldest_ns = Some(match e.source_oldest_ns {
            Some(v) => v.min(rec.stamp_ns),
            None => rec.stamp_ns,
        });
        e.source_newest_ns = Some(match e.source_newest_ns {
            Some(v) => v.max(rec.stamp_ns),
            None => rec.stamp_ns,
        });
        Ok(())
    })?;

    out.anomalies.filtered_channels = skips.filtered_channels;
    out.anomalies.non_cdr_channels = skips.non_cdr;
    out.anomalies.truncated = skips.truncated;
    out.anomalies.bad_chunks = skips.bad_chunks;
    out.anomalies.chunks_over_limit = skips.chunks_over_limit;
    out.anomalies.oversized_records_skipped = skips.oversized_records_skipped;
    out.anomalies.bad_chunk_span_ns = skips.bad_chunk_span_ns;
    out.anomalies.stripped_slash_names = normalizer.stripped_count();
    out.remaps = normalizer.remaps().to_vec();
    if out.edges.is_empty() {
        // Limit case first: it is the only one with an actionable remedy (a flag);
        // `Anomalies::truncated` still says the file is a prefix.
        return Err(if out.anomalies.chunks_over_limit > 0 {
            IngestError::AllChunksOverLimit {
                skipped: out.anomalies.chunks_over_limit,
            }
        } else if out.anomalies.truncated {
            IngestError::TruncatedBeforeAnyChunk
        } else {
            IngestError::NoTransforms
        });
    }
    Ok(out)
}

/// How a [`Publisher`] is named in a report. Offline only `Publisher::Topic`
/// occurs; other variants render as `Debug`.
fn publisher_label(p: &Publisher) -> String {
    match p {
        Publisher::Topic(t) => t.clone(),
        other => format!("{other:?}"),
    }
}

/// Normalize one record's frame pair, interning both. `None` when either name
/// is empty after normalization (§5.6's `NameError::Empty`).
fn normalize_pair(
    normalizer: &mut NameNormalizer,
    rec: &RawRecord<'_>,
    frames: &mut Frames,
) -> Option<(FrameId, FrameId)> {
    let parent = normalizer.normalize(rec.parent).ok()?;
    let child = normalizer.normalize(rec.child).ok()?;
    Some((frames.intern(&parent.name), frames.intern(&child.name)))
}

/// What pass two did, beyond producing the tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FillStats {
    /// How many times the recording had to be re-read to stay under
    /// `--max-memory`. `1` is the ordinary case.
    pub passes: u32,
    /// Peak bytes of buffered **samples** across those passes — the sort buffers,
    /// the stable sort's own scratch (an upper bound: one extra copy of the buffer
    /// sorted, the bound `plan_groups` packs against), the spill path's merge
    /// windows and staging. This is the number `--max-memory` bounds.
    pub peak_buffer_bytes: u64,
    /// Peak bytes of the spill path's *run index* — sixteen bytes per sorted run.
    /// `0` unless an edge spilled. Not bounded by `--max-memory` (it crosses the
    /// cap near `cap² / 2048` samples), so kept apart from
    /// [`peak_buffer_bytes`](FillStats::peak_buffer_bytes).
    pub peak_run_index_bytes: u64,
    /// Samples pushed into the arena.
    pub pushed: u64,
    /// Duplicate `(edge, stamp)` pairs collapsed, last-wins.
    pub duplicates: u64,
    /// Sorted runs written to a temporary spill file (§3.1), summed over every
    /// edge and every reduce pass. `0` is the ordinary case.
    pub spilled_runs: u32,
    /// Bytes written to those files. Disk, not memory; not bounded by `--max-memory`.
    pub spilled_bytes: u64,
}

/// Pass two — fill (§3.1).
///
/// Groups by edge, sorts by stamp within each edge, then pushes in order.
///
/// # What `--max-memory` bounds
///
/// It bounds the sort buffers, not the process:
///
/// | Allocation | Size | Bounded by `--max-memory`? |
/// |---|---|---|
/// | The arena, from `builder.build()` | 78 B per sample, measured | **No** |
/// | Pass two's sort buffers | `SAMPLE_BYTES` = 64 B per sample | Yes |
/// | The **stable sort's own scratch** | up to another 64 B per sample of the largest buffer in the group | Yes — see `plan_groups` |
/// | The spill path's run index | 16 B per sorted run | **No** — reported as [`FillStats::peak_run_index_bytes`] |
///
/// The arena is the output and cannot be capped (`docs/PROJECT.md` §5 D4;
/// `Capacity::slots` rounds each ring up to a power of two); budget for it
/// separately. The cap removes the *second* copy: `tests/memory.rs` measures
/// **164 B/sample** peak in one pass against **121 B/sample** at a 4 MiB cap, for
/// two extra sequential re-reads.
///
/// A **single** edge over the cap on its own takes §3.1's other route:
/// `crate::spill` sorts it in cap-sized runs in one temporary file and k-way
/// merges them. Second choice because a run file can leak or fill a filesystem.
///
/// # Errors
///
/// [`IngestError::Build`] if the surveyed topology cannot be allocated,
/// [`IngestError::Push`] if a sample is rejected, or [`IngestError::Spill`] if
/// the run file could not be written or read back.
pub fn fill(
    path: &Path,
    opts: &IngestOptions,
    survey: &Survey,
    frames: &Frames,
) -> Result<(Tree, FillStats), IngestError> {
    let mut builder = TreeBuilder::new();
    // Canonical (name-sorted), not first-seen, declaration order: §11 requires a
    // shuffled recording to yield a byte-identical `.tft`, and ids follow
    // declaration order.
    let mut sorted_frames: Vec<&String> = frames.all().iter().collect();
    sorted_frames.sort_unstable();
    for name in sorted_frames {
        builder = builder.frame(name);
    }
    let order = canonical_order(survey, frames);
    for &i in &order {
        let e = &survey.edges[i];
        let (p, c) = (frames.name(e.parent), frames.name(e.child));
        builder = match e.static_pose {
            Some(pose) => builder.static_edge(p, c, &iso_from_canonical(pose)),
            // `Capacity::slots` rounds up, so pass two cannot lap the ring;
            // duplicates only shrink the count.
            None => builder.dynamic_edge(p, c, EdgeCfg::new(Capacity::slots(clamp_u32(e.samples)))),
        };
    }
    let tree = builder.build().map_err(IngestError::Build)?;

    let groups = plan_groups(survey, &order, opts.max_memory_bytes);
    // The edge index is a function of the survey alone; build it once.
    let index: BTreeMap<EdgeKey, usize> = survey
        .edges
        .iter()
        .enumerate()
        .map(|(i, e)| ((e.parent.0, e.child.0), i))
        .collect();
    let mut stats = FillStats::default();
    for group in &groups {
        let slots = match group {
            Group::InMemory(slots) => slots,
            Group::Spilled(slot) => {
                fill_spilled(path, opts, survey, frames, &tree, *slot, &mut stats)?;
                continue;
            }
        };
        stats.passes += 1;
        let mut buffers: BTreeMap<usize, Vec<(i64, [f64; 7])>> = BTreeMap::new();
        for &slot in slots {
            buffers.insert(
                slot,
                Vec::with_capacity(survey.edges[slot].samples as usize),
            );
        }
        // Not hoisted: the normalizer accumulates counts that would multiply by passes.
        let mut normalizer = match &opts.tf_prefix {
            Some(p) => NameNormalizer::with_prefix(p),
            None => NameNormalizer::new(),
        };

        read_tf(path, &opts.roles, opts.read_policy(), |rec| {
            if rec.is_static || rec.stamp_ns == 0 {
                return Ok(());
            }
            let (Ok(p), Ok(c)) = (
                normalizer.normalize(rec.parent),
                normalizer.normalize(rec.child),
            ) else {
                return Ok(());
            };
            let (Some(pi), Some(ci)) = (frames.id(&p.name), frames.id(&c.name)) else {
                return Ok(());
            };
            if let Some(buf) = index.get(&(pi.0, ci.0)).and_then(|s| buffers.get_mut(s)) {
                buf.push((rec.stamp_ns, rec.pose));
            }
            Ok(())
        })?;

        // `buffers` drains by value, so at the k-th buffer the peak is
        // `remaining + scratch(k)`, which `plan_groups` reserves for.
        let mut remaining: u64 = buffers
            .values()
            .map(|b| b.len() as u64 * SAMPLE_BYTES)
            .sum();

        for (slot, mut buf) in buffers {
            let held = buf.len() as u64 * SAMPLE_BYTES;
            // The bound, not a measurement (`FillStats::peak_buffer_bytes`).
            stats.peak_buffer_bytes = stats.peak_buffer_bytes.max(remaining + held);
            // Stable: "last wins" means last in the recording.
            buf.sort_by_key(|(s, _)| *s);
            let e = &survey.edges[slot];
            let parent = tree
                .frame(frames.name(e.parent))
                .map_err(|_| IngestError::FrameLost { frame: e.parent })?;
            let child = tree
                .frame(frames.name(e.child))
                .map_err(|_| IngestError::FrameLost { frame: e.child })?;
            let writer = tree.claim(child, parent).map_err(IngestError::Claim)?;
            for i in 0..buf.len() {
                let (stamp, pose) = buf[i];
                // Last wins: skip all but the final duplicate (pushing them would
                // burn unbudgeted ring slots and inflate the manifest's `samples`).
                // `buf` is sorted, so one lookahead suffices.
                if buf.get(i + 1).is_some_and(|(next, _)| *next == stamp) {
                    stats.duplicates += 1;
                    continue;
                }
                writer
                    .push(stamp, &iso_from_canonical(pose))
                    .map_err(IngestError::Push)?;
                stats.pushed += 1;
            }
            remaining -= held;
        }
    }
    Ok((tree, stats))
}

/// One re-read of the recording, and what pass two does with it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Group {
    /// Edges whose sort buffers fit the cap together: buffer, sort in memory,
    /// drain. The ordinary case, and the one with no temporary file.
    InMemory(Vec<usize>),
    /// One edge that does not fit the cap on its own, so it takes §3.1's other
    /// route through [`crate::spill`].
    Spilled(usize),
}

/// Partition edges into re-reads whose buffered samples each fit `cap`.
///
/// Static edges are left out. An edge too large on its own becomes a
/// [`Group::Spilled`]; grouping cannot subdivide an edge.
///
/// # The reserve
///
/// The stable sort allocates up to one extra copy of the buffer being sorted, and
/// `fill` still owns the unreached buffers, so a group's peak is
/// `sum(buffers) + max(scratch)`, not `sum <= cap` (which overran the cap by up to
/// 1.95×; `tests/memory.rs`). The reserve is the group's largest member — an upper
/// bound, since std promises nothing — so an edge over `cap / 2` takes the spill
/// path. A sort that allocates nothing would be a decision record, not a patch.
fn plan_groups(survey: &Survey, order: &[usize], cap: u64) -> Vec<Group> {
    // The effective cap: the spill path raises anything below `spill::MIN_CAP`.
    let cap = spill::cap_of(cap);
    let mut groups: Vec<Group> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_bytes = 0u64;
    // The largest buffer in the group: the sort scratch to leave room for.
    let mut cur_max = 0u64;
    let flush =
        |cur: &mut Vec<usize>, cur_bytes: &mut u64, cur_max: &mut u64, groups: &mut Vec<Group>| {
            if !cur.is_empty() {
                groups.push(Group::InMemory(core::mem::take(cur)));
            }
            *cur_bytes = 0;
            *cur_max = 0;
        };
    // First-fit decreasing: each group is a whole re-read, so fewer bins is the
    // main saving (11/9 of optimal vs 17/10 for name order). Ids and §11's
    // byte-identical `.tft` are unaffected — `fill` declared from `order` already.
    // `rank` (canonical position) breaks ties so the plan is a function of the survey.
    let mut rank = vec![usize::MAX; survey.edges.len()];
    for (pos, &i) in order.iter().enumerate() {
        rank[i] = pos;
    }
    let mut packing: Vec<usize> = order.to_vec();
    packing.sort_by_key(|&i| (core::cmp::Reverse(survey.edges[i].samples), rank[i]));
    for &i in &packing {
        let e = &survey.edges[i];
        if e.is_static() || e.samples == 0 {
            continue;
        }
        let need = e.samples.saturating_mul(SAMPLE_BYTES);
        // `2 * need`: an edge alone in a group still pays for its own sort scratch.
        if need.saturating_mul(2) > cap {
            flush(&mut cur, &mut cur_bytes, &mut cur_max, &mut groups);
            groups.push(Group::Spilled(i));
            continue;
        }
        // `sum + max(scratch)`; saturating because `cap` can be `u64::MAX`
        // (saturation flushes, the safe direction).
        if cur_bytes
            .saturating_add(need)
            .saturating_add(cur_max.max(need))
            > cap
        {
            flush(&mut cur, &mut cur_bytes, &mut cur_max, &mut groups);
        }
        cur.push(i);
        cur_bytes += need;
        cur_max = cur_max.max(need);
    }
    flush(&mut cur, &mut cur_bytes, &mut cur_max, &mut groups);
    groups
}

/// Pass two for one edge that does not fit `--max-memory` on its own: §3.1's
/// spill-to-run-file and k-way merge.
///
/// Reads the recording once, keeping only this edge, in cap-sized sorted runs;
/// then merges them back and pushes. See [`crate::spill`] for the budget.
fn fill_spilled(
    path: &Path,
    opts: &IngestOptions,
    survey: &Survey,
    frames: &Frames,
    tree: &Tree,
    slot: usize,
    stats: &mut FillStats,
) -> Result<(), IngestError> {
    let edge = &survey.edges[slot];
    let (want_parent, want_child) = (edge.parent, edge.child);
    let (run_samples, staging) = spill::spill_budget(opts.max_memory_bytes);
    let dir = match &opts.spill_dir {
        Some(d) => d.clone(),
        None => std::env::temp_dir(),
    };
    let mut runs = spill::RunFile::create(&dir, staging)?;
    // Full capacity up front, so the recorded peak is the resident amount.
    let mut buf: Vec<spill::Sample> = Vec::with_capacity(run_samples);
    let mut normalizer = match &opts.tf_prefix {
        Some(p) => NameNormalizer::with_prefix(p),
        None => NameNormalizer::new(),
    };
    stats.passes += 1;

    read_tf(path, &opts.roles, opts.read_policy(), |rec| {
        if rec.is_static || rec.stamp_ns == 0 {
            return Ok(());
        }
        let (Ok(p), Ok(c)) = (
            normalizer.normalize(rec.parent),
            normalizer.normalize(rec.child),
        ) else {
            return Ok(());
        };
        let (Some(pi), Some(ci)) = (frames.id(&p.name), frames.id(&c.name)) else {
            return Ok(());
        };
        if (pi, ci) != (want_parent, want_child) {
            return Ok(());
        }
        buf.push((rec.stamp_ns, rec.pose));
        if buf.len() == run_samples {
            spill::sort_run(&mut buf);
            runs.write_run(&buf)?;
            buf.clear();
        }
        Ok(())
    })?;
    if !buf.is_empty() {
        spill::sort_run(&mut buf);
        runs.write_run(&buf)?;
    }
    // Buffer, its sort scratch and the staging (`spill::spill_budget`'s arithmetic).
    stats.peak_buffer_bytes = stats
        .peak_buffer_bytes
        .max(2 * run_samples as u64 * SAMPLE_BYTES + staging as u64);
    stats.peak_run_index_bytes = stats.peak_run_index_bytes.max(runs.index_bytes());
    stats.spilled_runs = stats.spilled_runs.saturating_add(runs.runs() as u32);
    stats.spilled_bytes += runs.bytes();
    // Released before the merge allocates its windows.
    drop(buf);

    // Reduce until one merge can hold every run (`spill`'s module docs). Windows
    // are contiguous, preserving the recording-order tie break.
    let fan_in = spill::fan_in(opts.max_memory_bytes);
    while runs.runs() > fan_in {
        let mut next = spill::RunFile::create(&dir, staging)?;
        let spans = runs.spans();
        for chunk in spans.chunks(fan_in) {
            let window = spill::merge_window_samples(opts.max_memory_bytes, chunk.len());
            let mut m = runs.merge_runs(chunk, window)?;
            // Two staging buffers: `runs` and `next` are both open.
            stats.peak_buffer_bytes = stats
                .peak_buffer_bytes
                .max(m.resident_bytes() + 2 * staging as u64);
            next.begin_run();
            while let Some(s) = m.next_sample()? {
                next.append(s)?;
            }
            drop(m);
            next.end_run()?;
        }
        // Measured at the end of the pass, where both indices and the span snapshot are live.
        stats.peak_run_index_bytes = stats.peak_run_index_bytes.max(
            runs.index_bytes()
                + next.index_bytes()
                + spans.len() as u64 * core::mem::size_of::<spill::RunSpan>() as u64,
        );
        stats.spilled_runs = stats.spilled_runs.saturating_add(next.runs() as u32);
        stats.spilled_bytes += next.bytes();
        // Dropping the previous file bounds disk use to two consecutive passes.
        runs = next;
    }

    let window = spill::merge_window_samples(opts.max_memory_bytes, runs.runs());
    let spans = runs.spans();
    // Read before the merge borrows the file mutably.
    let runs_index_bytes = runs.index_bytes();
    let mut merged = runs.merge_runs(&spans, window)?;
    // One staging buffer here, not two: only the file being read is open.
    stats.peak_buffer_bytes = stats
        .peak_buffer_bytes
        .max(merged.resident_bytes() + staging as u64);
    stats.peak_run_index_bytes = stats
        .peak_run_index_bytes
        .max(runs_index_bytes + spans.len() as u64 * core::mem::size_of::<spill::RunSpan>() as u64);

    let parent = tree
        .frame(frames.name(want_parent))
        .map_err(|_| IngestError::FrameLost { frame: want_parent })?;
    let child = tree
        .frame(frames.name(want_child))
        .map_err(|_| IngestError::FrameLost { frame: want_child })?;
    // These pushes record a clock offset against *ingest* time, so an old bag
    // reads as a huge offset; `TFT004` skips replayed sources via
    // `PushStream::no_live_receipt` (`docs/decisions/0036`). `ros2 bag play` into a
    // live stack is not covered (`tft004`'s doc).
    let writer = tree.claim(child, parent).map_err(IngestError::Claim)?;
    // One-element delay: hold a sample until the next proves it is not a duplicate.
    let mut pending: Option<spill::Sample> = None;
    while let Some(next) = merged.next_sample()? {
        if let Some(prev) = pending {
            if prev.0 == next.0 {
                stats.duplicates += 1;
            } else {
                writer
                    .push(prev.0, &iso_from_canonical(prev.1))
                    .map_err(IngestError::Push)?;
                stats.pushed += 1;
            }
        }
        pending = Some(next);
    }
    if let Some(last) = pending {
        writer
            .push(last.0, &iso_from_canonical(last.1))
            .map_err(IngestError::Push)?;
        stats.pushed += 1;
    }
    Ok(())
}

/// Survey indices sorted by `(parent name, child name)`.
///
/// Independent of arrival order. `pub(crate)` so [`crate::report`] shares the
/// comparator and its row order cannot drift from the arena's `EdgeId` order.
pub(crate) fn canonical_order(survey: &Survey, frames: &Frames) -> Vec<usize> {
    let mut order: Vec<usize> = (0..survey.edges.len()).collect();
    order.sort_by(|&a, &b| {
        let (ea, eb) = (&survey.edges[a], &survey.edges[b]);
        (frames.name(ea.parent), frames.name(ea.child))
            .cmp(&(frames.name(eb.parent), frames.name(eb.child)))
    });
    order
}

/// `[qw qx qy qz tx ty tz]` to an [`Iso3`].
///
/// The quaternion is normalized here and nowhere else: recordings are only
/// approximately unit and `Iso3::new` does not normalize.
fn iso_from_canonical(p: [f64; 7]) -> Iso3 {
    Iso3::new(
        Quat::new(p[0], p[1], p[2], p[3]).normalize(),
        Vec3::new(p[4], p[5], p[6]),
    )
}

/// Saturating `u64` to `u32` for a ring capacity; `build()` rejects the excess.
fn clamp_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn survey_with(samples: &[u64]) -> Survey {
        Survey {
            edges: samples
                .iter()
                .enumerate()
                .map(|(i, &n)| EdgeSurvey {
                    parent: FrameId(0),
                    child: FrameId(i as u32 + 1),
                    topic: "/tf".into(),
                    static_pose: None,
                    samples: n,
                    source_oldest_ns: Some(0),
                    source_newest_ns: Some(1),
                })
                .collect(),
            anomalies: Anomalies::default(),
            static_conflict_details: Vec::new(),
            transforms_read: 0,
            remaps: Vec::new(),
        }
    }

    /// A cap smaller than the dataset splits the edges across passes, and no
    /// group's peak, `sum(buffers) + max(buffer)`, exceeds the cap.
    #[test]
    fn groups_respect_the_cap() {
        let s = survey_with(&[10, 10, 10, 10]);
        let cap = 25 * SAMPLE_BYTES;
        let order: Vec<usize> = (0..s.edges.len()).collect();
        let groups = plan_groups(&s, &order, cap);
        assert!(groups.len() >= 2, "expected a split, got {groups:?}");
        let mut seen: Vec<usize> = Vec::new();
        for g in &groups {
            let Group::InMemory(slots) = g else {
                panic!("no edge here exceeds half the cap alone: {g:?}")
            };
            let each: Vec<u64> = slots
                .iter()
                .map(|&i| s.edges[i].samples * SAMPLE_BYTES)
                .collect();
            let bytes: u64 = each.iter().sum();
            let peak = bytes + each.iter().copied().max().unwrap_or(0);
            assert!(peak <= cap, "group {slots:?} peaks at {peak} > {cap}");
            seen.extend(slots);
        }
        // Every non-empty edge appears exactly once.
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3]);
    }

    /// An edge over **half** the cap cannot be sorted in memory inside it, so it
    /// takes the spill path.
    ///
    /// A group of one still pays for its own sort scratch: 15 samples is 960 B and
    /// peaks at 1 920 against a 1 600 B cap.
    #[test]
    fn an_edge_over_half_the_cap_takes_the_spill_path() {
        let s = survey_with(&[15]);
        let order: Vec<usize> = (0..s.edges.len()).collect();
        assert_eq!(
            plan_groups(&s, &order, 25 * SAMPLE_BYTES),
            vec![Group::Spilled(0)],
            "a group of one peaks at twice its buffer"
        );
        // Control: a hair under half the cap still sorts in memory.
        let s = survey_with(&[12]);
        assert_eq!(
            plan_groups(&s, &order, 25 * SAMPLE_BYTES),
            vec![Group::InMemory(vec![0])]
        );
    }

    /// One edge larger than the whole cap becomes a spilled group of its own.
    #[test]
    fn one_oversized_edge_spills_on_its_own() {
        let s = survey_with(&[100, 5]);
        let order: Vec<usize> = (0..s.edges.len()).collect();
        let groups = plan_groups(&s, &order, 25 * SAMPLE_BYTES);
        assert_eq!(
            groups,
            vec![Group::Spilled(0), Group::InMemory(vec![1])],
            "the oversized edge must not join a group it would blow"
        );
    }

    /// A cap below `spill::MIN_CAP` (1 024) is planned at the floor: four samples
    /// (256 B) at `--max-memory 200` stay in memory.
    #[test]
    fn a_cap_below_the_floor_is_planned_at_the_floor() {
        let s = survey_with(&[4]);
        let order: Vec<usize> = (0..s.edges.len()).collect();
        assert_eq!(
            plan_groups(&s, &order, 200),
            vec![Group::InMemory(vec![0])],
            "an edge that fits the *effective* cap must not take the spill path"
        );
    }

    /// Static and empty edges reserve no buffer, so an all-static recording needs
    /// no passes.
    #[test]
    fn statics_and_empty_edges_take_no_buffer() {
        let mut s = survey_with(&[0, 0]);
        s.edges[0].static_pose = Some([1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let order: Vec<usize> = (0..s.edges.len()).collect();
        assert!(plan_groups(&s, &order, 1024).is_empty());
    }

    /// An off-unit quaternion is normalized on the way into the arena.
    #[test]
    fn poses_are_normalized_on_the_way_in() {
        let iso = iso_from_canonical([1.02, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0]);
        assert!((iso.q.norm() - 1.0).abs() < 1e-15, "norm {}", iso.q.norm());
        assert_eq!(iso.t, Vec3::new(1.0, 2.0, 3.0));
    }
}
