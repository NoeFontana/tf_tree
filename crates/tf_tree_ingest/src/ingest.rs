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
//! The online bridge watches one clock, because it *is* one publisher. A
//! recording is not: `/tf` carries every publisher on the robot interleaved into
//! one file, and they do not stamp at the same instant. A localization node
//! stamps `map -> odom` at the scan it processed and publishes 200 ms later,
//! while `odom -> base_link` is stamped as it is published. Both are correct;
//! their stamps interleave by whatever the slower pipeline's latency is, which is
//! hundreds of milliseconds and not the tens the 100 ms threshold assumes.
//!
//! A single guard over the merged stream therefore reports a *reset* — the whole
//! ingest halting, at the default, on an ordinary recording — for something that
//! is not a clock reset at all but two publishers. One guard per edge is the only
//! monotonicity that means anything here anyway, because §3.1 sorts per edge and
//! Phase 1 invariant 6 is a per-edge rule. It still catches what the check is
//! for: a bag loop or a sim reset moves `/clock` itself, so **every** edge
//! regresses at once and the first one to be observed halts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tf_tree::{Capacity, EdgeCfg, Iso3, Quat, Tree, TreeBuilder, Vec3};
use tf_tree_bridge::clock::{ClockGuard, ClockVerdict, OnClockReset};
use tf_tree_bridge::names::NameNormalizer;
use tf_tree_bridge::statics::{StaticKind, StaticStore, StaticVerdict};
use tf_tree_bridge::Publisher;

use crate::source::{read_tf, RawRecord, TopicRoles};
use crate::spill;
use crate::{FrameId, IngestError};

/// Bytes one buffered sample costs during pass two: an `i64` stamp beside the
/// canonical `[f64; 7]` pose.
///
/// The pose is buffered in canonical order rather than as an [`Iso3`] on
/// purpose: `Iso3` is `align(64)`, so a `(i64, Iso3)` pair occupies **128**
/// bytes and would double the memory this module is trying to bound. The
/// conversion happens at the push, one sample at a time.
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
    /// How far backwards a stamp must jump to count as a reset rather than
    /// ordinary interleaving. Defaults to the bridge's own threshold, so a
    /// recording and a live system draw the line in the same place.
    pub clock_reset_threshold_ns: i64,
    /// How far ahead of its log time a stamp may be before it is reported.
    pub future_horizon_ns: i64,
    /// A `tf_prefix` to apply to every frame name (§5.6).
    pub tf_prefix: Option<String>,
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
            clock_reset_threshold_ns: tf_tree_bridge::clock::DEFAULT_RESET_THRESHOLD_NANOS,
            future_horizon_ns: DEFAULT_FUTURE_HORIZON_NS,
            tf_prefix: None,
            spill_dir: None,
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
    /// Where the first one was, if any.
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
    let mut index: BTreeMap<EdgeKey, usize> = BTreeMap::new();
    let mut out = Survey {
        edges: Vec::new(),
        anomalies: Anomalies::default(),
        transforms_read: 0,
        remaps: Vec::new(),
    };

    let skips = read_tf(path, &opts.roles, |rec| {
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
                let s = out.edges.len() - 1;
                index.insert(key, s);
                s
            }
        };

        if rec.is_static {
            // §5.7's order, which `docs/PHASE4.md` §0.0 records getting wrong
            // once: compare values *first*, and report both, before any policy
            // decides which one survives.
            match statics.observe_static(
                frames.name(parent),
                frames.name(child),
                rec.pose,
                &Publisher::Node(rec.topic.to_owned()),
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

        if statics.observe_dynamic(frames.name(parent), frames.name(child))
            == Err(StaticKind::Static)
        {
            return Err(IngestError::EdgeKindChanged {
                parent,
                child,
                stamp_ns: rec.stamp_ns,
            });
        }

        if rec.stamp_ns == 0 {
            out.anomalies.zero_stamp_drops += 1;
            return Ok(());
        }
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
    out.anomalies.stripped_slash_names = normalizer.stripped_count();
    out.remaps = normalizer.remaps().to_vec();
    if out.edges.is_empty() {
        return Err(IngestError::NoTransforms);
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
/// | Pass two's sort buffers | [`SAMPLE_BYTES`] = 64 B per sample | Yes |
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
/// [`crate::spill`] sorts the edge in cap-sized runs, writes them to one
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

        read_tf(path, &opts.roles, |rec| {
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
    for &i in order {
        let e = &survey.edges[i];
        if e.is_static() || e.samples == 0 {
            continue;
        }
        let need = e.samples.saturating_mul(SAMPLE_BYTES);
        if need > cap {
            // Emitted in place, so the spilled edge keeps its position in the
            // canonical order and the number of re-reads stays a function of the
            // survey alone.
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

    read_tf(path, &opts.roles, |rec| {
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
