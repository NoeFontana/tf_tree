//! The two passes — `docs/PHASE5.md` §3.1, NORMATIVE.
//!
//! # Why there are two, and why it is not a performance question
//!
//! `TreeBuilder::build()` needs every frame, every edge kind and every ring
//! capacity *before* it allocates, because the arena has fixed capacity and does
//! not grow (`docs/PROJECT.md` §5 D4). A recording tells you none of that until
//! you have read it. So pass one discovers the topology and the counts and pass
//! two fills — not to be fast, but because one pass cannot be done at all.
//! `docs/PHASE4.md` §5.8's amendment records the same constraint blocking the
//! ROS bridge, from the other direction.
//!
//! # Where this diverges from the live bridge, deliberately
//!
//! `tf_tree_bridge` is the ROS-independent decision half of the *online* path,
//! and everything in it that is a pure decision is reused here — name
//! normalization (§5.6), the static store (§5.7), the clock guard (§5.5) — so
//! that a recording and a live system classify the same stream the same way.
//!
//! One rule is inverted, and it has to be: **a backward stamp is dropped online
//! and kept offline.** Online, the ring cannot accept it — Phase 1 invariant 6
//! makes a regressing push an error, and the sample is already unusable by the
//! time the bridge sees it. Offline, §3.1 sorts each edge before pushing, so the
//! same sample lands in the right place. Treating out-of-order as a drop here
//! would discard exactly the samples §3.1's sort exists to recover, and a
//! recording routinely has thousands of them.
//!
//! # And the guard is per **edge**, not per stream
//!
//! `/tf` carries every publisher on the robot interleaved into one stream, and
//! they do not stamp at the same instant. A localization node stamps
//! `map -> odom` at the scan it processed and publishes 200 ms later, while
//! `odom -> base_link` is stamped as it is published. Both are correct; their
//! stamps interleave by whatever the slower pipeline's latency is, which is
//! hundreds of milliseconds and not the tens the 100 ms threshold assumes.
//!
//! A single guard over the merged stream therefore reports a *reset* — the whole
//! ingest halting, at the default, on an ordinary recording — for something that
//! is not a clock reset at all but two publishers. One guard per edge is the only
//! monotonicity that means anything here anyway, because §3.1 sorts per edge and
//! Phase 1 invariant 6 is a per-edge rule. It still catches what the check is
//! for: a bag loop or a sim reset moves `/clock` itself, so **every** edge
//! regresses at once and the first one to be observed halts.
//!
//! # The two halves agree about scope and diverge about promotion
//!
//! An earlier revision of the section above opened by contrasting this with the
//! online bridge — *"the online bridge watches one clock, because it **is** one
//! publisher"*. That was never true of `/tf`, which carries the same interleaved
//! publishers live as it does in a recording, and
//! `docs/decisions/0011` acted on it: `tf_tree_bridge` keeps one [`ClockGuard`]
//! per edge too, for exactly the reasons above.
//!
//! What the two halves still differ about is deliberate, and it is not *scope*
//! but **promotion** — what it takes to turn "this edge regressed" into "the
//! clock moved":
//!
//! - **Offline, here, the first regressing edge halts.** A recording is a closed
//!   artefact, either coherent or not, and a human is reading the answer.
//!   Stopping to say *"`map -> odom` regresses at t"* costs a rerun and nothing
//!   else, and the operator can look at the file.
//! - **Online, one witness never halts, at any magnitude.** The bridge runs a
//!   ladder: an authoritative jump reported by the time source itself, or a
//!   *common-mode* step — two or more distinct publishers whose stamp-to-receipt
//!   offsets moved inside the same window **by the same amount** — promotes to a
//!   halt; anything less is dropped, counted and diagnosed. That bridge runs
//!   unattended on a robot, where halting on one node's restart is an outage
//!   caused by the diagnostic rather than by the fault, and nobody is there to
//!   rerun anything.
//!
//! So the asymmetry is a difference in what a wrong answer costs, not a
//! disagreement about what a clock reset is.
//!
//! **An earlier revision of this paragraph claimed the asymmetry closes** on a
//! deployment that cannot supply a second publisher, because the online promotion
//! rule was floored by what the topology declared. It no longer does, and the gap
//! is wider than it was, on purpose: making a *diagnostic's* strength depend on
//! attribution — which RMW implementations report unevenly — made attribution a
//! correctness dependency, which `docs/PHASE4.md` §5.3 forbids outright. The
//! online half now degrades to a drop when it cannot corroborate, so a single
//! dynamic edge regressing there is *never* a halt, where here it always is.
//! Nothing is lost by that: online, Phase 1 invariant 6 refuses the regressing
//! sample anyway, so the arena is protected by the drop and the halt was only
//! ever the *announcement*.
//!
//! # Both halves anchor on a clock that is not the one under test
//!
//! The rule the online redesign is built on is that a detector's reference clock
//! must be independent of the signal it is judging — inferring "`/clock` was
//! reset" from `/clock`-derived stamps alone is what produced three successive
//! wrong rules. Online that reference is injected: a local **steady** clock read
//! once per `TFMessage` and carried on every sample as `SteadyNanos`.
//!
//! **This half has had the same kind of reference all along**, and it is
//! [`RawRecord::log_time_ns`] — when the recorder wrote the message, not when a
//! publisher stamped it. Its own documentation states the reason in the same
//! terms: §3.2's future-stamp check *"needs a reference clock that is not the
//! header stamp, or the check is circular"*. It is the only reference a bag
//! actually contains, and the two halves now use it for the same purpose rather
//! than by coincidence:
//!
//! - the future-stamp anomaly is `stamp - log_time` against a horizon, which is
//!   the same `stamp - received` quantity the online offset table smooths per
//!   publisher; and
//! - a reset is reported with **both** coordinates on
//!   [`IngestError::ClockReset`], because after a rewind the stamp is the one
//!   coordinate that no longer identifies a place in the file. `at_ns` occurs
//!   twice in a looped recording; the log time occurs once, and it is what
//!   `mcap` and `ros2 bag` cut on — which is what makes the message's own
//!   advice, *split the recording*, an instruction rather than a suggestion.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tf_tree::{Capacity, EdgeCfg, Iso3, Quat, Tree, TreeBuilder, Vec3};
use tf_tree_bridge::clock::{ClockGuard, ClockVerdict, OnClockReset};
use tf_tree_bridge::names::NameNormalizer;
use tf_tree_bridge::statics::{StaticStore, StaticVerdict};
use tf_tree_bridge::Publisher;

use crate::decompress::ChunkLimits;
use crate::source::{read_tf, ChunkPolicy, OnBadChunk, RawRecord, TopicRoles};
use crate::spill;
use crate::{FrameId, IngestError};

/// Bytes one buffered sample costs during pass two: an `i64` stamp beside the
/// canonical `[f64; 7]` pose.
///
/// The pose is buffered in canonical order rather than as an [`Iso3`], and the
/// reason it was written for has since gone away. It read: *"`Iso3` is
/// `align(64)`, so a `(i64, Iso3)` pair occupies **128** bytes and would double
/// the memory this module is trying to bound"* — true then, and
/// [`0042`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0042-the-cacheline-the-arena-never-asked-for.md)
/// dropped that alignment, so the pair is **64** bytes now and this constant
/// would be right either way.
///
/// The buffer stays `[f64; 7]` regardless: it is the canonical order the push
/// takes, so buffering it avoids a conversion per sample rather than a padding
/// per sample. That this module had already paid to route *around* the
/// alignment is part of why `0042` removed it.
const SAMPLE_BYTES: u64 = 8 + 7 * 8;

/// Default `--max-memory` (§3.1): 4 GiB.
pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Default horizon past a message's own log time before its stamp counts as
/// "far in the future" (§3.2): **10 seconds**.
///
/// Chosen against what a recorder actually produces: a message is written
/// within milliseconds of being published, and a publisher stamping ahead by
/// more than a few seconds is misconfigured rather than merely late. Below a
/// second this would fire on ordinary sensor pipelines that stamp at capture
/// and publish after processing.
pub const DEFAULT_FUTURE_HORIZON_NS: i64 = 10_000_000_000;

/// Default ceiling on one chunk's declared `uncompressed_size`: **64 MiB**.
///
/// Chosen against what recorders write, not against what the format permits:
/// rosbag2 and Foxglove chunk at 1–8 MiB, and `mcap`'s own writer defaults to
/// 1 MiB. 64 MiB is an order of magnitude above any of them and two orders below
/// a size that can exhaust a machine, which is the gap a bound wants to sit in.
pub const DEFAULT_MAX_CHUNK_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;

/// Default ceiling on one chunk's `uncompressed_size / compressed_size`: **1024**.
///
/// The absolute ceiling alone is not a bomb guard, because 64 MiB of output from
/// 200 bytes of input fits under any ceiling generous enough for a real
/// recording. Real MCAP chunks of CDR transforms compress at roughly 2–5×; zstd's
/// theoretical maximum on a degenerate frame is far past this, so 1024 admits
/// every plausible recording and refuses the shape whose only purpose is to
/// allocate.
pub const DEFAULT_MAX_CHUNK_EXPANSION_RATIO: u64 = 1024;

/// How ingest should handle a backward clock jump (§3.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClockResetPolicy {
    /// Stop, naming the timestamp. **The default here**, and it is the default
    /// for a reason `tf_tree_bridge::clock::OnClockReset` states as well: an
    /// index that silently merges two recordings' clocks answers every query
    /// wrongly and says nothing about it.
    #[default]
    Halt,
    /// §3.2's `split`, which is **not implemented** — see
    /// [`IngestError::ClockResetSplitUnsupported`]. It is spelled out as a
    /// variant rather than omitted so the CLI can refuse it with the reason
    /// instead of rejecting an argument the specification lists.
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
    /// ordinary interleaving. Defaults to the bridge's own threshold, so a
    /// recording and a live system draw the line in the same place.
    pub clock_reset_threshold_ns: i64,
    /// How far ahead of its log time a stamp may be before it is reported.
    pub future_horizon_ns: i64,
    /// A `tf_prefix` to apply to every frame name (§5.6).
    pub tf_prefix: Option<String>,
    /// Ceiling on one chunk's declared `uncompressed_size`, in bytes.
    ///
    /// **A knob and not a constant, because the person who meets a limit is the
    /// person who cannot patch the crate.** A recording written with 128 MiB
    /// chunks is unusual and is not corrupt; a reader that refuses it with no
    /// number to raise is a reader that has to be forked. Defaults to
    /// [`DEFAULT_MAX_CHUNK_UNCOMPRESSED_BYTES`].
    ///
    /// It bounds the compressed path only. The uncompressed path returns the
    /// records **by borrow** and allocates nothing, so there is no allocation for
    /// a ceiling to bound — see `crate::decompress::chunk_records`.
    pub max_chunk_uncompressed_bytes: u64,
    /// Ceiling on one chunk's `uncompressed_size / compressed_size`. Defaults to
    /// [`DEFAULT_MAX_CHUNK_EXPANSION_RATIO`].
    ///
    /// The second half of the decompression-bomb guard; the absolute ceiling
    /// above cannot do this job on its own.
    pub max_chunk_expansion_ratio: u64,
    /// Where §3.1's spill file goes when one edge alone exceeds
    /// [`max_memory_bytes`](IngestOptions::max_memory_bytes). `None` means
    /// `std::env::temp_dir()`.
    ///
    /// A knob rather than a hardcoded `/tmp` because the user this exists for —
    /// §3.1's "4-hour recording", tens of gigabytes on one edge — is exactly the
    /// user whose `/tmp` is a small tmpfs *in RAM*, where spilling would defeat
    /// the cap it is enforcing.
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
            max_chunk_expansion_ratio: DEFAULT_MAX_CHUNK_EXPANSION_RATIO,
            spill_dir: None,
        }
    }
}

impl IngestOptions {
    /// The two chunk bounds as the reader wants them.
    ///
    /// A method rather than a field holding a [`ChunkLimits`], because the options
    /// are a flat struct every caller builds with `..Default::default()` and a
    /// nested struct would make the two numbers reachable two ways. This is the
    /// join, done once.
    #[must_use]
    pub fn chunk_limits(&self) -> ChunkLimits {
        ChunkLimits {
            max_uncompressed_bytes: self.max_chunk_uncompressed_bytes,
            max_expansion_ratio: self.max_chunk_expansion_ratio,
        }
    }

    /// Everything [`read_tf`] needs to know about how to treat a chunk.
    #[must_use]
    pub fn chunk_policy(&self) -> ChunkPolicy {
        ChunkPolicy {
            on_bad: self.on_bad_chunk,
            limits: self.chunk_limits(),
        }
    }
}

/// The interned frame names, owned by the caller so that a failed pass still
/// has names to put in its error message.
///
/// This is why [`survey`] takes `&mut Frames` rather than returning them inside
/// the `Survey`: [`IngestError`] is `Copy` and `String`-free like every other
/// error in this workspace (`docs/PROJECT.md` §5), so it names the offending
/// edge by index — and an index is only useful next to the table it indexes.
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
    /// The read-only half of [`intern`](Frames::intern), for pass two: pass one
    /// interned every name the recording contains, so a miss here means the
    /// transform was dropped in pass one and has no buffer to go in.
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
    /// Oldest surviving stamp — the **true** oldest in the source, which is what
    /// §2.3's amendment says the manifest's `oldest_ns` is not.
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

/// Everything §3.2 asks to be counted and reported.
///
/// Every field is a count of something that happens in real recordings; none of
/// them is an error on its own.
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
    /// **Structurally always `None` today, and deliberately left that way here.**
    /// Both [`ClockResetPolicy`] arms return an error on the first reset, so this
    /// is written and then discarded with the rest of the [`Survey`]; the
    /// coordinates a caller can actually read are the ones on
    /// [`IngestError::ClockReset`], which is why the log-time coordinate was
    /// added *there* and not given a dead twin beside this. It stays as the
    /// field a non-halting policy would fill (§3.2's unimplemented `split`)
    /// rather than being removed and re-added.
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
    /// Messages on a TF channel this build could not decode.
    pub filtered_channels: u64,
    /// The recording stopped mid-record: everything before the cut was read.
    ///
    /// Not a count, because it is not a quantity — the recording either ends
    /// where it says it does or it does not. It is in `Anomalies` rather than
    /// beside it because §3.2's report is where a user finds out, and finding
    /// out matters: every number in this report is then a number about a
    /// *prefix* of the run they recorded.
    pub truncated: bool,
    /// Chunks that were unreadable and skipped (`OnBadChunk::Skip`).
    ///
    /// Reported next to [`truncated`](Anomalies::truncated), and for the same
    /// reason: both mean the counts below cover only part of the recording. A
    /// skipped chunk also takes its `Schema` and `Channel` records with it, so a
    /// later message on a channel that was only ever declared there is dropped
    /// with no counter of its own — which is why this row is not merely a tally.
    ///
    /// **Truncation does not count here.** A cut chunk's last record necessarily
    /// runs past the end of the file; calling that corruption would tell an
    /// operator their recording is damaged when it is only incomplete.
    pub bad_chunks: u64,
    /// Of [`bad_chunks`](Anomalies::bad_chunks), how many were refused by one of
    /// this reader's limits rather than found damaged.
    ///
    /// Reported separately because the remedy differs and only this one has a
    /// remedy at all: a flag. See [`crate::IngestError::AllChunksOverLimit`], which
    /// is what the same condition becomes when it takes *every* chunk.
    pub chunks_over_limit: u64,
    /// The span the skipped chunks covered, from their own declared message times.
    ///
    /// "Three chunks were unreadable" is not actionable; "the transforms between
    /// 14:22:07 and 14:22:19 are missing" is.
    pub bad_chunk_span_ns: Option<(u64, u64)>,
}

/// The output of pass one: an exact topology and the counts that size it.
#[derive(Clone, Debug)]
pub struct Survey {
    /// Every edge, in first-seen order. Edge index is this vector's index.
    pub edges: Vec<EdgeSurvey>,
    /// What was odd about the recording.
    pub anomalies: Anomalies,
    /// Transforms read, before any drop.
    pub transforms_read: u64,
    /// Frame-name remappings applied, as `(raw, normalized)`.
    pub remaps: Vec<(String, String)>,
}

impl Survey {
    /// Dynamic edges that ended pass one with no samples at all — §3.2's
    /// "frame declared, never published", in the only form a recording can show
    /// it: the edge is in the topology because something announced it, and every
    /// sample it carried was dropped.
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
    // One guard per edge — see the module docs. `clocks[i]` belongs to
    // `out.edges[i]`; the two vectors are grown together and never separately.
    let mut clocks: Vec<ClockGuard> = Vec::new();
    // **The dynamic half of the edge-kind check, as one bit per slot.**
    //
    // `StaticStore` can answer "has this edge been seen as dynamic?", but only by
    // name: `observe_dynamic` is two `BTreeMap<String, _>` descents, and asking it
    // per transform costs ~ten `str` comparisons to re-derive something the slot
    // lookup above already resolved. The static direction is read straight off
    // `out.edges[slot].static_pose`; this vector is the other direction, and it is
    // grown with `clocks` and `out.edges`, never separately.
    let mut dynamic_seen: Vec<bool> = Vec::new();
    let mut index: BTreeMap<EdgeKey, usize> = BTreeMap::new();
    let mut out = Survey {
        edges: Vec::new(),
        anomalies: Anomalies::default(),
        transforms_read: 0,
        remaps: Vec::new(),
    };

    let skips = read_tf(path, &opts.roles, opts.chunk_policy(), |rec| {
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
                    // The guard's own policy is not consulted: this crate applies
                    // `ClockResetPolicy` itself, because `split` has no online
                    // counterpart to borrow a meaning from.
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
            // **The dynamic-then-static direction, checked here rather than by
            // `StaticStore`.** It used to fall out of `observe_static`'s
            // `KindChanged` arm, which fires on the `Dynamic` kind that
            // `observe_dynamic` inserted — and that call is gone from the dynamic
            // branch below, because it re-probed by name what the slot already
            // knows. So the bit is kept here instead. The arm below stays as a
            // match arm; it is simply no longer the thing that catches this.
            if dynamic_seen[slot] {
                return Err(IngestError::EdgeKindChanged {
                    parent,
                    child,
                    stamp_ns: rec.stamp_ns,
                });
            }
            // §5.7's order, which `docs/PHASE4.md` §0.0 records getting wrong
            // once: compare values *first*, and report both, before any policy
            // decides which one survives.
            match statics.observe_static(
                frames.name(parent),
                frames.name(child),
                rec.pose,
                &Publisher::Topic(rec.topic.to_owned()),
            ) {
                StaticVerdict::Declare => out.edges[slot].static_pose = Some(rec.pose),
                StaticVerdict::Idempotent => {}
                StaticVerdict::Conflict { .. } => {
                    // First writer wins, matching the bridge's default
                    // authority policy; the count is what the report surfaces.
                    out.anomalies.static_conflicts += 1;
                }
                StaticVerdict::KindChanged { .. } => {
                    return Err(IngestError::EdgeKindChanged {
                        parent,
                        child,
                        stamp_ns: rec.stamp_ns,
                    })
                }
            }
            // **A static's stamp never touches the clock** (`docs/PHASE4.md`
            // §5.5's third ordering note): `robot_state_publisher` stamps
            // statics with zero, and feeding that to the reset detector drags
            // the high-water mark to the epoch and halts on a correct robot.
            return Ok(());
        }

        // **The edge kind is a property of the slot the lookup above already
        // resolved**, so it is read from there rather than re-probed by name.
        // `StaticStore::observe_dynamic` is two `BTreeMap<String, _>` descents —
        // ~ten `str` comparisons — and it ran on *every* dynamic transform in the
        // recording to answer a question an index answers in one load.
        // `static_pose` is `Some` exactly when the store holds `Static` for this
        // edge: `observe_static` inserts `Static` only on its `Declare` path, and
        // `Declare` is the only arm that sets `static_pose`.
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
        // `stamp - log_time`: the recorder's clock is the reference, because the
        // header stamp cannot check itself. This is the offline twin of the
        // online `stamp - received` offset — see the module docs' section on the
        // reference clock — and it is the reason `log_time_ns` exists on
        // [`RawRecord`] at all.
        let ahead = rec.stamp_ns.saturating_sub(rec.log_time_ns);
        if ahead > opts.future_horizon_ns {
            out.anomalies.future_stamps += 1;
            out.anomalies.worst_future_offset_ns = out.anomalies.worst_future_offset_ns.max(ahead);
        }
        match clocks[slot].observe(rec.stamp_ns) {
            ClockVerdict::Forward => {}
            // Kept, unlike online. See the module docs.
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

    // Both terms, and the `+ non_cdr` is not decoration: a TF-schema channel
    // this build cannot decode is skipped for a *different* reason than one the
    // topic filter excluded, and the report's single "channels were skipped"
    // line has to account for either or it under-reports silently.
    out.anomalies.filtered_channels = skips.filtered_channels + skips.non_cdr;
    out.anomalies.truncated = skips.truncated;
    out.anomalies.bad_chunks = skips.bad_chunks;
    out.anomalies.chunks_over_limit = skips.chunks_over_limit;
    out.anomalies.bad_chunk_span_ns = skips.bad_chunk_span_ns;
    out.anomalies.stripped_slash_names = normalizer.stripped_count();
    out.remaps = normalizer.remaps().to_vec();
    if out.edges.is_empty() {
        // **Which of the two "nothing here" cases this is, is the whole value of
        // the message.** Reading is record-granular, even inside a chunk (see
        // `source::read_tf`), so a recording cut before its first complete *record*
        // yields zero transforms for a reason that has nothing to do with what was
        // published — and `NoTransforms` would send the user looking for a publisher
        // instead of at their file. An earlier revision of this comment said
        // *chunk*-granular, which would make this arm look reachable only for files
        // smaller than one chunk; `truncation_recovery_is_record_granular` shows
        // records inside a cut chunk are recovered, so the reachable class is a very
        // different and much larger one.
        //
        // **The limit case is tested first, ahead of truncation.** It is the only
        // one of the three with a remedy the operator can act on — a flag — and a
        // recording can be both truncated and written with chunks over the ceiling,
        // in which case naming the knob is strictly more useful than naming the cut.
        // `Anomalies::truncated` still says the file is a prefix, so nothing is lost
        // by ordering it second.
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
    /// Peak bytes of buffered **samples** across those passes — the sort
    /// buffers, the spill path's merge windows and its encode/decode staging.
    /// This is the number `--max-memory` bounds.
    pub peak_buffer_bytes: u64,
    /// Peak bytes of the spill path's *run index* — sixteen bytes per sorted
    /// run, live for as long as the file that holds them. `0` unless an edge
    /// spilled.
    ///
    /// Its own field, and not folded into
    /// [`peak_buffer_bytes`](FillStats::peak_buffer_bytes), because
    /// `--max-memory` does **not** bound it: the run count is
    /// `samples / (cap / 64)`, so the index crosses the cap itself at roughly
    /// `cap² / 2048` samples — 512 samples at a 1 KiB cap, and past any real
    /// recording at the 4 GiB default. Adding it to the capped number would make
    /// that number a lie; leaving it unreported would make the report one.
    pub peak_run_index_bytes: u64,
    /// Samples pushed into the arena.
    pub pushed: u64,
    /// Duplicate `(edge, stamp)` pairs collapsed, last-wins.
    pub duplicates: u64,
    /// Sorted runs written to a temporary spill file (§3.1), summed over every
    /// edge that needed one **and every reduce pass** — so it can exceed the
    /// number of runs that existed at any one instant. `0` is the ordinary case.
    pub spilled_runs: u32,
    /// Bytes written to those files. Disk, not memory — it is reported separately
    /// from [`peak_buffer_bytes`](FillStats::peak_buffer_bytes) because
    /// `--max-memory` does not bound it and conflating the two is how a cap
    /// starts meaning nothing.
    pub spilled_bytes: u64,
}

/// Pass two — fill (§3.1).
///
/// Groups by edge, sorts by stamp within each edge, then pushes in order.
///
/// # What `--max-memory` bounds, and what it does not
///
/// **It bounds the sort buffers, not the process.** Saying otherwise would be
/// the more flattering claim and it is false, so it is worth being exact about
/// which half is capped:
///
/// | Allocation | Size | Bounded by `--max-memory`? |
/// |---|---|---|
/// | The arena, from `builder.build()` | 78 B per sample, measured | **No** |
/// | Pass two's sort buffers | `SAMPLE_BYTES` = 64 B per sample | Yes |
/// | The spill path's run index | 16 B per sorted run | **No** — reported as [`FillStats::peak_run_index_bytes`] |
///
/// The arena is not capped because it *cannot* be: it is the output. Every
/// sample the recording contains has to be resident in it for the index to
/// answer, the arena has fixed capacity and does not grow
/// (`docs/PROJECT.md` §5 D4), and `Capacity::slots` rounds each ring up to a
/// power of two on top of that. A user sizing a machine for a large recording
/// must budget for the arena separately, and there is no flag that changes it —
/// only fewer samples would.
///
/// What the cap removes is the *second* copy. Buffering the whole recording
/// before sorting costs another 64 B per sample on top of the arena; splitting
/// into groups replaces that with the cap. Measured here on a 90 000-sample,
/// three-edge fixture (`tests/memory.rs`): 7 096 704 B of arena either way, and
/// a peak of 142 B/sample in one pass against 121 B/sample at a 4 MiB cap. The
/// saving is bounded by how finely the edges divide — three equal edges split
/// 2–1, so this is close to the worst case — and it is paid for with one extra
/// sequential re-read. That is the trade §3.1 asks for, reached without a
/// temporary run-file to leak, to fill a different filesystem, or to leave
/// behind when the process is killed.
///
/// Grouping's smallest unit is an edge, so it cannot serve a **single** edge
/// whose samples exceed the cap on their own — at the default 4 GiB, 67 million
/// samples on one edge. That case, and only that case, takes §3.1's other route:
/// `crate::spill` sorts the edge in cap-sized runs, writes them to one
/// temporary file, and k-way merges them back. It is second choice rather than
/// the general mechanism because a run file is a thing that can leak, fill a
/// different filesystem, or outlive the process; re-reading the recording is
/// none of those.
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
    // **Declaration order is canonical, not first-seen, and §11 is the reason.**
    //
    // §11 requires that shuffling a recording's messages produce a byte-identical
    // `.tft`. Declaring frames and edges as they are first encountered cannot
    // satisfy that: `FrameId` and `EdgeId` are assigned in declaration order, so
    // two ingests of the same transforms in a different order produce arenas
    // whose ids — and therefore whose ring offsets, whose topology block and
    // whose `LookupError::Extrapolation { edge }` — disagree. The values were
    // identical; the identities were not, and the shuffle test caught exactly
    // that.
    //
    // Sorting by name makes the arena a pure function of the *content* of the
    // recording. It also makes the ingest report diffable between runs, which is
    // worth having on its own.
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
            // `Capacity::slots` rounds up to a power of two, so a ring is never
            // *smaller* than the source's sample count and pass two cannot lap
            // it. Duplicates only shrink the true count, so this stays an upper
            // bound even after the last-wins collapse below.
            None => builder.dynamic_edge(p, c, EdgeCfg::new(Capacity::slots(clamp_u32(e.samples)))),
        };
    }
    let tree = builder.build().map_err(IngestError::Build)?;

    let groups = plan_groups(survey, &order, opts.max_memory_bytes);
    // Re-derive the edge index the same way pass one did, **once**: it is a
    // function of the survey alone and the survey does not change between
    // groups. Building it from the survey rather than re-interning keeps the two
    // passes agreeing about which edge is which even if a name normalizes
    // differently.
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
        // The normalizer, by contrast, is *not* hoisted: it accumulates the
        // remap and stripped-slash counts, and reusing one across groups would
        // multiply them by the number of passes.
        let mut normalizer = match &opts.tf_prefix {
            Some(p) => NameNormalizer::with_prefix(p),
            None => NameNormalizer::new(),
        };

        read_tf(path, &opts.roles, opts.chunk_policy(), |rec| {
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

        let live: u64 = buffers
            .values()
            .map(|b| b.len() as u64 * SAMPLE_BYTES)
            .sum();
        stats.peak_buffer_bytes = stats.peak_buffer_bytes.max(live);

        for (slot, mut buf) in buffers {
            // **Stable** sort, and that is what makes "last wins" mean the last
            // occurrence *in the recording*. An unstable sort would pick an
            // arbitrary one of two duplicates, so ingesting the same bag twice
            // could produce two different `.tft` files.
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
                // Last wins: skip every duplicate but the final one, rather
                // than pushing each and letting the ring absorb them. Pushing
                // them all is *correct* — equal stamps are accepted and the
                // newer value wins — but it burns ring slots the counting pass
                // did not budget for and inflates the manifest's `samples`.
                //
                // **One lookahead is the whole rule**, and a trailing check
                // against the previously pushed stamp would be dead code: `buf`
                // is sorted, so a run of equal stamps is contiguous, and every
                // element of that run except the last takes this `continue`
                // *without* pushing. The stamp that does get pushed is therefore
                // strictly greater than the one before it.
                if buf.get(i + 1).is_some_and(|(next, _)| *next == stamp) {
                    stats.duplicates += 1;
                    continue;
                }
                writer
                    .push(stamp, &iso_from_canonical(pose))
                    .map_err(IngestError::Push)?;
                stats.pushed += 1;
            }
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
/// Static edges take no buffer and are left out entirely. An edge too large for
/// the cap on its own becomes a [`Group::Spilled`] of its own rather than
/// joining a group it would blow — grouping cannot subdivide an edge, which is
/// the whole reason the spill path exists.
fn plan_groups(survey: &Survey, order: &[usize], cap: u64) -> Vec<Group> {
    // The *effective* cap, not the requested one: the spill path raises anything
    // below `spill::MIN_CAP` to it, so deciding "does this edge fit?" against
    // the raw request routes an edge that would have fit — four samples at
    // `--max-memory 200` — to a run file that then runs on a 1 024 B budget
    // anyway. One number, asked once.
    let cap = spill::cap_of(cap);
    let mut groups: Vec<Group> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_bytes = 0u64;
    let flush = |cur: &mut Vec<usize>, cur_bytes: &mut u64, groups: &mut Vec<Group>| {
        if !cur.is_empty() {
            groups.push(Group::InMemory(core::mem::take(cur)));
        }
        *cur_bytes = 0;
    };
    // **First-fit *decreasing*, and the bin is the expensive thing.** Every
    // `Group::InMemory` is one whole re-read of the recording — reopen, re-walk,
    // re-decompress, re-CDR-decode — so a bin saved is the largest saving
    // available on this path. Packing in canonical *name* order is plain first
    // fit, whose worst case is 17/10 of the optimal bin count; largest-first
    // needs 11/9.
    //
    // This does **not** touch the canonical order. `fill` has already declared
    // every frame and edge from `order` before it calls here, so `FrameId`,
    // `EdgeId` and §11's byte-identical `.tft` are unaffected; what changes is
    // only which re-read buffers which edge, and `fill`'s `buffers` is keyed by
    // slot, so the per-edge push order is unchanged too. `rank` — position in the
    // canonical order — is the tie break, which keeps the plan a pure function of
    // the survey rather than of a sort's stability.
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
        if need > cap {
            // Emitted in place in the **packing** order, not the canonical one —
            // so the spilled edge still keeps a fixed position and the number of
            // re-reads stays a function of the survey alone. `rank` is the tie
            // break, which is what makes "fixed" mean deterministic here rather
            // than merely stable.
            flush(&mut cur, &mut cur_bytes, &mut groups);
            groups.push(Group::Spilled(i));
            continue;
        }
        if cur_bytes + need > cap {
            flush(&mut cur, &mut cur_bytes, &mut groups);
        }
        cur.push(i);
        cur_bytes += need;
    }
    flush(&mut cur, &mut cur_bytes, &mut groups);
    groups
}

/// Pass two for one edge that does not fit `--max-memory` on its own: §3.1's
/// spill-to-run-file and k-way merge.
///
/// Reads the recording once, keeping only this edge, in cap-sized sorted runs;
/// then merges the runs back and pushes. See [`crate::spill`] for the memory
/// budget and for why ties break by run index.
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
    // Allocated to its full capacity up front, so the peak recorded below is the
    // resident amount for the whole pass rather than a high-water mark that a
    // reallocation could briefly double.
    let mut buf: Vec<spill::Sample> = Vec::with_capacity(run_samples);
    let mut normalizer = match &opts.tf_prefix {
        Some(p) => NameNormalizer::with_prefix(p),
        None => NameNormalizer::new(),
    };
    stats.passes += 1;

    read_tf(path, &opts.roles, opts.chunk_policy(), |rec| {
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
            // **Stable**, for the same reason the in-memory path is stable: it
            // is what makes "last wins" mean the last occurrence in the
            // recording. See `spill`'s module docs for the other half — the tie
            // break across runs.
            buf.sort_by_key(|(s, _)| *s);
            runs.write_run(&buf)?;
            buf.clear();
        }
        Ok(())
    })?;
    if !buf.is_empty() {
        buf.sort_by_key(|(s, _)| *s);
        runs.write_run(&buf)?;
    }
    stats.peak_buffer_bytes = stats
        .peak_buffer_bytes
        .max(run_samples as u64 * SAMPLE_BYTES + staging as u64);
    stats.peak_run_index_bytes = stats.peak_run_index_bytes.max(runs.index_bytes());
    stats.spilled_runs = stats.spilled_runs.saturating_add(runs.runs() as u32);
    stats.spilled_bytes += runs.bytes();
    // Released before the merge allocates its windows: holding both would double
    // the peak, which is the number `--max-memory` is supposed to be about.
    drop(buf);

    // **Reduce until one merge can hold every remaining run.** A merge keeps at
    // least one sample of each run resident, so beyond `fan_in` runs it exceeds
    // the cap however the window is chosen — see `spill`'s module docs. Each
    // pass merges a *contiguous* window of runs, which is what preserves the
    // recording-order tie break that "last wins" depends on.
    let fan_in = spill::fan_in(opts.max_memory_bytes);
    while runs.runs() > fan_in {
        let mut next = spill::RunFile::create(&dir, staging)?;
        let spans = runs.spans();
        for chunk in spans.chunks(fan_in) {
            let window = spill::merge_window_samples(opts.max_memory_bytes, chunk.len());
            let mut m = runs.merge_runs(chunk, window)?;
            // **Two** staging buffers, not one: `runs` and `next` are both open
            // here, and `RunFile::staging` is allocated to capacity at `create`
            // and never released. Counting one understates the pass by
            // `staging` bytes, which at a small cap is an eighth of it.
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
        // Measured at the end of the pass, which is where it peaks: both files'
        // run indices and the snapshot of the one being read are live at once,
        // and `next`'s index is the only one of the three still growing.
        stats.peak_run_index_bytes = stats.peak_run_index_bytes.max(
            runs.index_bytes()
                + next.index_bytes()
                + spans.len() as u64 * core::mem::size_of::<spill::RunSpan>() as u64,
        );
        stats.spilled_runs = stats.spilled_runs.saturating_add(next.runs() as u32);
        stats.spilled_bytes += next.bytes();
        // The previous file is dropped here, and with it the spill it held: disk
        // use is bounded by two consecutive passes, not by their number.
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
    // **These pushes record a clock offset, and it is measured against *ingest*
    // time** (`docs/decisions/0036`). `EdgeWriter::push` samples
    // `wall clock - stamp` into `ClaimRecord::clock_offset_nanos`, so replaying
    // a 2024 recording in 2026 records an offset of about two years. That is
    // arithmetically correct and diagnostically meaningless: the publisher's
    // clock was fine, the recording is simply old. A bag-sourced tree is an
    // ordinary live heap `Tree`, so neither `TFT004`'s frozen-source skip nor its
    // epoch condition catches it. **`TFT004` skips a replayed source for exactly
    // this reason** — `PushStream::no_live_receipt`, added by `0036` step 3 —
    // which covers `doctor --from-bag` because that path knows it is reading a
    // recording.
    //
    // **What it does not cover is `ros2 bag play` into a live stack**, where the
    // same old stamps reach a shared arena through the §5 bridge and `doctor
    // --attach` sees an ordinary live arena. Provenance is a property of the
    // writer and the arena records none, so that gap is named in `tft004`'s doc
    // and in the finding text rather than closed here.
    //
    // The cost is the sampler's ~1.1 ns per push plus one 38 ns clock read per
    // interval, on a bulk load of millions of samples: single-digit
    // milliseconds, against a run that parses a bag.
    let writer = tree.claim(child, parent).map_err(IngestError::Claim)?;
    // The in-memory path's one-element lookahead, spelled as a one-element
    // delay: a merged stream has no random access, so the sample is held back
    // until the next one proves it is not a duplicate.
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
/// The one ordering that does not depend on when a message happened to arrive.
/// See [`fill`]'s comment for why that matters.
///
/// `pub(crate)` so [`crate::report`] calls *this* rather than keeping its own
/// copy of the comparator. Two copies is how the report's row order and the
/// arena's `EdgeId` order drift apart, and nothing would report the drift.
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
/// The quaternion is **normalized here and nowhere else**. A recording's
/// quaternions are only approximately unit — a bag converted through YAML or
/// float32 arrives off by ~1e-7 — and `Iso3::new` documents that it performs no
/// normalization and that callers guarantee unitness. Skipping this puts a
/// non-unit quaternion in the arena, where `slerp` between two of them drifts.
fn iso_from_canonical(p: [f64; 7]) -> Iso3 {
    Iso3::new(
        Quat::new(p[0], p[1], p[2], p[3]).normalize(),
        Vec3::new(p[4], p[5], p[6]),
    )
}

/// Saturating `u64` to `u32` for a ring capacity. A single edge with more than
/// 4 billion samples is not something the arena can hold anyway; `build()`
/// rejects it with a layout error that names the real limit.
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
            transforms_read: 0,
            remaps: Vec::new(),
        }
    }

    /// A cap smaller than the dataset splits the edges across several passes,
    /// and no group exceeds the cap.
    ///
    /// Mutant: change the flush condition to `cur_bytes + need > cap * 2` —
    /// applied, and the per-group budget assertion failed at 2 × cap.
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
                panic!("no edge here exceeds the cap alone: {g:?}")
            };
            let bytes: u64 = slots
                .iter()
                .map(|&i| s.edges[i].samples * SAMPLE_BYTES)
                .sum();
            assert!(bytes <= cap, "group {slots:?} needs {bytes} > {cap}");
            seen.extend(slots);
        }
        // Every non-empty edge appears exactly once.
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3]);
    }

    /// One edge larger than the whole cap becomes a spilled group of its own —
    /// grouping cannot subdivide an edge, so this is the case §3.1's run file
    /// exists for.
    ///
    /// Mutant: delete the `if need > cap` arm — applied, and this failed with
    /// `InMemory([0, 1])`, a group 3× over budget, instead of the two groups
    /// asserted here.
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

    /// A cap below what the spill path can honour is planned against the value
    /// it *will* be raised to, so a small edge is not spilled for nothing.
    ///
    /// Four samples are 256 B. At `--max-memory 200` they fit in memory —
    /// because the spill path would raise 200 to `spill::MIN_CAP` = 1 024 and
    /// buffer them anyway — so routing them to a temporary file buys a file, a
    /// merge and a `Drop` and saves not one byte.
    ///
    /// Mutant: plan against `cap.max(SAMPLE_BYTES)` instead of
    /// `spill::cap_of(cap)` — applied, and this failed with `[Spilled(0)]`.
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

    /// Static and empty edges reserve no buffer at all, so a recording that is
    /// all statics needs no passes.
    ///
    /// Mutant: remove the `e.is_static() || e.samples == 0` skip — applied, and
    /// this test failed with one group of two edges instead of none.
    #[test]
    fn statics_and_empty_edges_take_no_buffer() {
        let mut s = survey_with(&[0, 0]);
        s.edges[0].static_pose = Some([1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let order: Vec<usize> = (0..s.edges.len()).collect();
        assert!(plan_groups(&s, &order, 1024).is_empty());
    }

    /// A quaternion that is off-unit by more than `slerp` tolerates is
    /// normalized on the way into the arena.
    ///
    /// Mutant: drop the `.normalize()` in `iso_from_canonical` — applied, and
    /// the norm assertion failed at 1.0198.
    #[test]
    fn poses_are_normalized_on_the_way_in() {
        let iso = iso_from_canonical([1.02, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0]);
        assert!((iso.q.norm() - 1.0).abs() < 1e-15, "norm {}", iso.q.norm());
        assert_eq!(iso.t, Vec3::new(1.0, 2.0, 3.0));
    }
}
