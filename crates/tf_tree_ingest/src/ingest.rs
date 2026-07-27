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
use std::path::Path;

use tf_tree::{Capacity, EdgeCfg, Iso3, Quat, Tree, TreeBuilder, Vec3};
use tf_tree_bridge::clock::{ClockGuard, ClockVerdict, OnClockReset};
use tf_tree_bridge::names::NameNormalizer;
use tf_tree_bridge::statics::{StaticKind, StaticStore, StaticVerdict};
use tf_tree_bridge::Publisher;

use crate::source::{read_tf, RawRecord, TopicRoles};
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
    /// Peak bytes of buffered samples across those passes.
    pub peak_buffer_bytes: u64,
    /// Samples pushed into the arena.
    pub pushed: u64,
    /// Duplicate `(edge, stamp)` pairs collapsed, last-wins.
    pub duplicates: u64,
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
/// The one case this cannot serve is a **single** edge whose samples exceed the
/// cap on their own — a true k-way merge would spill within the edge. That is
/// [`IngestError::EdgeExceedsMemoryCap`], which names the edge and both numbers.
/// At the default 4 GiB it takes 67 million samples on one edge.
///
/// # Errors
///
/// [`IngestError::Build`] if the surveyed topology cannot be allocated,
/// [`IngestError::Push`] if a sample is rejected, or
/// [`IngestError::EdgeExceedsMemoryCap`] as above.
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

    let groups = plan_groups(survey, &order, opts.max_memory_bytes)?;
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
    let mut stats = FillStats {
        passes: 0,
        peak_buffer_bytes: 0,
        pushed: 0,
        duplicates: 0,
    };
    for group in &groups {
        stats.passes += 1;
        let mut buffers: BTreeMap<usize, Vec<(i64, [f64; 7])>> = BTreeMap::new();
        for &slot in group {
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

/// Partition edges into groups whose buffered samples each fit `cap`.
///
/// Static edges take no buffer and are left out entirely.
fn plan_groups(survey: &Survey, order: &[usize], cap: u64) -> Result<Vec<Vec<usize>>, IngestError> {
    let cap = cap.max(SAMPLE_BYTES);
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_bytes = 0u64;
    for &i in order {
        let e = &survey.edges[i];
        if e.is_static() || e.samples == 0 {
            continue;
        }
        let need = e.samples.saturating_mul(SAMPLE_BYTES);
        if need > cap {
            return Err(IngestError::EdgeExceedsMemoryCap {
                parent: e.parent,
                child: e.child,
                needed_bytes: need,
                cap_bytes: cap,
            });
        }
        if cur_bytes + need > cap && !cur.is_empty() {
            groups.push(core::mem::take(&mut cur));
            cur_bytes = 0;
        }
        cur.push(i);
        cur_bytes += need;
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    Ok(groups)
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
    /// Mutant: drop the `&& !cur.is_empty()` guard's companion — change the
    /// flush condition to `cur_bytes + need > cap * 2` — applied, and the
    /// per-group budget assertion failed at 2 × cap.
    #[test]
    fn groups_respect_the_cap() {
        let s = survey_with(&[10, 10, 10, 10]);
        let cap = 25 * SAMPLE_BYTES;
        let order: Vec<usize> = (0..s.edges.len()).collect();
        let groups = plan_groups(&s, &order, cap).unwrap();
        assert!(groups.len() >= 2, "expected a split, got {groups:?}");
        for g in &groups {
            let bytes: u64 = g.iter().map(|&i| s.edges[i].samples * SAMPLE_BYTES).sum();
            assert!(bytes <= cap, "group {g:?} needs {bytes} > {cap}");
        }
        // Every non-empty edge appears exactly once.
        let mut seen: Vec<usize> = groups.iter().flatten().copied().collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3]);
    }

    /// One edge larger than the whole cap is an error naming that edge, not a
    /// group that quietly exceeds the budget.
    ///
    /// Mutant: delete the `if need > cap` arm — applied, and the test failed
    /// with `Ok` where it expects `EdgeExceedsMemoryCap` (the edge went into a
    /// group 4× over budget).
    #[test]
    fn one_oversized_edge_is_named() {
        let s = survey_with(&[100]);
        let order: Vec<usize> = (0..s.edges.len()).collect();
        let err = plan_groups(&s, &order, 25 * SAMPLE_BYTES).unwrap_err();
        assert!(
            matches!(
                err,
                IngestError::EdgeExceedsMemoryCap { child, .. } if child == FrameId(1)
            ),
            "got {err:?}"
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
        assert!(plan_groups(&s, &order, 1024).unwrap().is_empty());
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
