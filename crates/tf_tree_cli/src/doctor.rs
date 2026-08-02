//! `tf_tree doctor` — the seven Phase 1 health checks (`docs/PHASE1.md` §12
//! *CLI*).
//!
//! Each check is a pure function over a captured [`Snapshot`] of the tree plus,
//! where the condition is only visible in history, the [`Observations`] stream of
//! observed pushes. Splitting detection from a live `Tree` this way is what makes
//! every check independently testable — including conditions a *safe* live tree
//! can never reach on its own (a topology cycle is rejected by the builder; a
//! second writer is rejected by the claim table), which the tests exercise by
//! constructing the offending snapshot directly.
//!
//! The seven checks (`docs/PHASE1.md` §12):
//!
//! 1. [`check_cycles`] — a parent chain that never reaches a root.
//! 2. [`check_unclaimed_dynamic`] — a dynamic edge with no live writer.
//! 3. [`check_multi_writer`] — more than one writer PID seen on one edge.
//! 4. [`check_short_buffers`] — a ring shorter than the observed publish latency.
//! 5. [`check_inconsistent_rates`] — a frame whose publish intervals vary widely.
//! 6. [`check_unreachable`] — frames not connected to the main root component.
//! 7. [`check_out_of_order`] — stamps arriving out of order on an edge.

use std::collections::{BTreeMap, BTreeSet};

use std::sync::atomic::Ordering;

use tf_tree::unstable::EdgeKind;
use tf_tree::{EdgeId, FrameId, InterpPolicy, Tree};
use tf_tree_bench::fixture::PushSample;

/// Which of the seven diagnostics produced a finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// A parent chain that cycles (never reaches a root).
    Cycle,
    /// A dynamic edge with no live writer holding its claim.
    UnclaimedDynamic,
    /// More than one writer PID observed publishing to one edge.
    MultiWriter,
    /// A ring buffer shorter than the observed publish latency.
    ShortBuffer,
    /// A frame published at a wildly inconsistent rate.
    InconsistentRate,
    /// Frames not reachable from the main root component.
    Unreachable,
    /// Stamps observed arriving out of monotonic order.
    OutOfOrder,
}

impl Check {
    /// A short, stable label for this check.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Check::Cycle => "cycle",
            Check::UnclaimedDynamic => "unclaimed-dynamic",
            Check::MultiWriter => "multi-writer",
            Check::ShortBuffer => "short-buffer",
            Check::InconsistentRate => "inconsistent-rate",
            Check::Unreachable => "unreachable",
            Check::OutOfOrder => "out-of-order",
        }
    }
}

/// How serious a finding is.
///
/// Distinct from [`crate::catalogue::Severity`], which is the *reporting*
/// vocabulary and carries an `Info` level this layer has no use for. The two
/// meet in one place — `From<Severity> for crate::catalogue::Severity` — so
/// that a check's severity is declared once, here, next to the code that knows
/// why the condition is serious, instead of being restated by whatever maps the
/// finding into the catalogue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Worth attention but not necessarily broken.
    Warn,
    /// A genuine fault.
    Error,
}

/// One diagnostic finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Which check raised it.
    pub check: Check,
    /// How serious it is.
    pub severity: Severity,
    /// A human-readable explanation (this is the print layer's crate, so a
    /// `String` here is fine — the engine's errors stay `Copy`).
    pub message: String,
}

impl Finding {
    fn warn(check: Check, message: String) -> Finding {
        Finding {
            check,
            severity: Severity::Warn,
            message,
        }
    }
    fn error(check: Check, message: String) -> Finding {
        Finding {
            check,
            severity: Severity::Error,
            message,
        }
    }
}

impl From<Severity> for crate::catalogue::Severity {
    fn from(s: Severity) -> crate::catalogue::Severity {
        match s {
            Severity::Warn => crate::catalogue::Severity::Warn,
            Severity::Error => crate::catalogue::Severity::Error,
        }
    }
}

// ---------------------------------------------------------------------------
// Captured tree model
// ---------------------------------------------------------------------------

/// One frame in a captured [`Snapshot`].
#[derive(Clone, Debug)]
pub struct FrameInfo {
    /// Frame id (1-based; `0` is the root sentinel).
    pub id: u32,
    /// Display name (truncated as stored).
    pub name: String,
    /// Parent frame id (`0` = root or unattached).
    pub parent: u32,
    /// Depth from the root.
    pub depth: u16,
    /// The edge whose child is this frame (`0` if none).
    pub edge_of_child: u32,
}

/// One edge in a captured [`Snapshot`].
#[derive(Clone, Debug)]
pub struct EdgeInfo {
    /// Edge id.
    pub id: u32,
    /// Parent frame id.
    pub parent: u32,
    /// Child frame id.
    pub child: u32,
    /// Static / dynamic / tombstone.
    pub kind: EdgeKind,
    /// Ring capacity (`0` for static edges).
    pub capacity: u32,
    /// Interpolation policy.
    pub interp: InterpPolicy,
    /// Time-domain tag.
    pub domain: u8,
    /// Total samples ever published (monotone head).
    pub head: u64,
    /// Whether a live writer currently holds the claim.
    pub claimed: bool,
    /// Whether the claim record was caught **mid-handoff** (the `CLAIMING`
    /// sentinel) rather than actually held.
    ///
    /// Separate from `claimed`/`owner_pid` because both of those collapse it
    /// into "claimed by nobody", which is also what a genuinely leaked claim
    /// looks like. Distinguishing them needs a liveness source a snapshot does
    /// not have, so `TFT014` uses this to stay silent instead of guessing.
    pub claiming: bool,
    /// The current claim owner's PID (`0` if unclaimed).
    pub owner_pid: u32,
    /// Newest published stamp, if any samples exist.
    pub newest_stamp: Option<i64>,
    /// The rate this edge was **declared** to publish at, in milli-hertz, or
    /// `None` when nothing declared one (`EdgeRecord::nominal_rate_mhz == 0`).
    ///
    /// `Option` rather than the raw `0` sentinel because the two answers send
    /// `TFT007` in opposite directions: a declared rate is something to compare
    /// against, and an absent one is a reason to say nothing at all. A `u32`
    /// here invites `rate != 0` to be forgotten at one call site, and the
    /// failure that produces — every undeclared edge reported as deviating from
    /// 0 Hz by infinity — is a fabricated finding on every edge of a correct
    /// arena.
    pub nominal_rate_mhz: Option<u32>,
}

impl EdgeInfo {
    /// Current ring occupancy (`min(head, capacity)`).
    #[must_use]
    pub fn occupancy(&self) -> u64 {
        if self.capacity == 0 {
            0
        } else {
            self.head.min(u64::from(self.capacity))
        }
    }
}

/// A point-in-time, read-only capture of a tree's topology, edges, and claims.
///
/// Built from a live [`Tree`] via [`Snapshot::capture`], or by hand in tests to
/// exhibit a specific condition.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// All frames, id order.
    pub frames: Vec<FrameInfo>,
    /// All edges, id order.
    pub edges: Vec<EdgeInfo>,
}

impl Snapshot {
    /// Capture the current state of `tree` through its read-only arena view.
    #[must_use]
    pub fn capture(tree: &Tree) -> Snapshot {
        let view = tree.arena_view();
        let header = view.header();
        let topo = view.topology();

        // **`Relaxed`, because no stronger ordering on *this* load would mean
        // anything.** `tf_tree_core::frame`'s `finish` does
        // `frame_count.fetch_add` *first*, then `write_record`, then the Release
        // publish into the intern table. An `Acquire` load here would therefore
        // order this thread against everything the interner did *before* it took
        // its id — and against nothing it did after, which is precisely the
        // record about to be read. Acquire would read like a guarantee and buy
        // none, so `Relaxed` is the honest spelling.
        //
        // **What it does not buy is a race-free read, and nothing here does.**
        // `FrameRecord`'s own documentation states the rule: its fields are
        // plain integers, so "a reader sees the record only after
        // `ids[slot].load(Acquire) != ID_UNPUBLISHED`" — the publication edge is
        // keyed by *name*, through the intern table, and there is no per-id
        // signal at all. An enumeration keyed by id, which is what every caller
        // that wants to list the frames must do, therefore has no edge to
        // acquire and can materialize a record a concurrent interner is still
        // writing. The `name_hash != 0` filter below is not that missing edge
        // and is not claimed to be: it is a plausibility filter that turns the
        // usual lost race into "the frame appears one `doctor` run later"
        // instead of "a frame with an empty name". Against a genuinely
        // concurrent interner this walk — and its three siblings — is
        // best-effort, and the consolidation onto `Tree` noted below is where
        // the ordering can be stated once and checked by `just loom`.
        let frame_count = header.frame_count.load(Ordering::Relaxed);
        let mut frames = Vec::with_capacity(frame_count as usize);
        for id in 1..=frame_count {
            // Three checks, the strictest set applied by any copy of this walk.
            // **The consolidation this comment used to file has happened**:
            // `tf_tree::Tree::frames` now carries exactly these three, with the
            // ordering argument stated once, in the crate `just loom` and
            // `just miri` can see.
            //
            // This copy did not collapse onto it and will not: `Tree::frames`
            // answers *names*, and a `FrameInfo` needs `parent`, `depth` and
            // `edge_of_child` out of the topology block as well — `doctor` is
            // the consumer the arena-shaped view exists for. What it stops
            // being is a *divergent* copy: the three checks below and the
            // `Relaxed` load above are now the facade's, verbatim, so a change
            // to either has one obvious other place to go.
            // `tf_tree_py::offline::frames_impl` and
            // `tf_tree_c::unstable::tft_tree_frame_name` are still their own
            // walks and can now forward.
            //
            //  1. `FrameId::new` rejects 0, the root sentinel.
            //  2. `id <= frame_count` — *this loop's bound*, and load-bearing
            //     rather than incidental: `frame_record` bounds against
            //     `max_frames`, which is `frame_count + 1 + frame_headroom`, so
            //     walking to `max_frames` would hand back zeroed headroom slots
            //     as though they were frames.
            //  3. `name_hash != 0`, below.
            let Some(fid) = FrameId::new(id) else {
                continue;
            };
            let Some(rec) = view.frame_record(fid) else {
                continue;
            };
            // **The count is bumped before the record is written**, so an
            // interner in another process can be counted here one instant
            // before its name exists and the slot still reads as zeros. A
            // written record's `name_hash` is BLAKE3 of the name — non-zero
            // even for the empty string — so a zero hash means "not written
            // yet". Skipping it lists that frame one `doctor` run later;
            // taking it prints a frame with an empty name, which reads as our
            // bug rather than as a race lost by a microsecond. This is a
            // *filter on the value read*, not a synchronisation edge — see the
            // ordering note above for what that does and does not buy.
            if rec.name_hash == 0 {
                continue;
            }
            let n = rec.name_len as usize;
            let name = core::str::from_utf8(&rec.name[..n.min(rec.name.len())])
                .unwrap_or("<invalid-utf8>")
                .to_owned();
            let Some((parent, depth, edge_of_child, _gen)) = topo.read_frame(fid) else {
                continue;
            };
            frames.push(FrameInfo {
                id,
                name,
                parent,
                depth,
                edge_of_child,
            });
        }

        // `edge_count` is stored as (declared edges + 1 sentinel); real ids are
        // `1..edge_count`.
        let edge_count = header.edge_count.load(Ordering::Relaxed);
        let mut edges = Vec::with_capacity(edge_count.saturating_sub(1) as usize);
        for id in 1..edge_count {
            let eid = EdgeId(id);
            let (Some(rec), Some(claim)) = (view.edge(eid), view.claim(eid)) else {
                continue;
            };
            let kind = EdgeKind::from_u8(rec.kind);
            let owner_word = claim.owner.load(Ordering::Relaxed);
            // **The owner word is `(epoch << 16) | (slot + 1)`, not `slot + 1`**
            // (`tf_tree_core::edge::pack_owner`, A3 plus decision 0005 §6's
            // "one acquisition, not just one slot"). `claim` starts the epoch at
            // 1, so `word - 1` is never a slot for any real claim — it is
            // `epoch << 16` and resolves to no participant at all. Decoding it
            // by hand is what produced `pid 0` for every live writer.
            //
            // `slot_of` returns `u32::MAX` for an unclaimed record and for one
            // caught mid-claim, and `identity` rejects that, so both read as
            // "no owner" rather than as a plausible wrong pid.
            let owner_slot = if owner_word == 0 {
                None
            } else {
                Some(tf_tree_core::edge::slot_of(owner_word))
            };
            // `ring` is `None` for a static/tombstoned edge (capacity 0), so this
            // needs no separate power-of-two guard.
            let newest_stamp = view.ring(eid).and_then(|r| r.newest_stamp());
            edges.push(EdgeInfo {
                id,
                parent: rec.parent,
                child: rec.child,
                kind,
                capacity: rec.capacity,
                interp: InterpPolicy::from_u8(rec.interp),
                domain: rec.domain,
                head: rec.head.load(Ordering::Relaxed),
                claimed: owner_word != 0,
                claiming: tf_tree_core::edge::is_claiming(owner_word),
                // A3: the claim names a *participant slot*, not a PID, so the
                // owning process is resolved through the participant table. A
                // slot that no longer resolves means the owner detached or died
                // between the two reads, which reports as pid 0 — the honest
                // answer, and what the reaper will act on.
                owner_pid: owner_slot
                    .and_then(|slot| view.participants().identity(slot))
                    .map_or(0, |(pid, _start, _inc)| pid),
                newest_stamp,
                nominal_rate_mhz: match rec.nominal_rate_mhz {
                    0 => None,
                    mhz => Some(mhz),
                },
            });
        }

        Snapshot { frames, edges }
    }

    /// The display name of frame `id`, or `#id` if it is not in the snapshot.
    #[must_use]
    pub fn frame_label(&self, id: u32) -> String {
        self.frames
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.name.clone())
            .unwrap_or_else(|| format!("frame#{id}"))
    }

    /// An `id -> edge` map for the checks that walk [`crate::checks::EdgeStats`]
    /// and need the corresponding [`EdgeInfo`].
    ///
    /// Built once per check rather than re-scanning `edges` per entry: the
    /// naive `edges.iter().find(...)` inside a loop over `stats` is O(E^2), and
    /// on a 5 000-edge arena that is tens of millions of comparisons to answer
    /// a question a single pass already knows. Not a `zip` against `stats`,
    /// even though `collect_edge_stats` happens to build them in the same
    /// order: a caller assembling `Inputs` by hand can supply stats for a
    /// subset, and a silently misaligned zip would put the wrong frame names on
    /// a finding — trading a correctness risk for speed on a cold path.
    #[must_use]
    pub fn edge_index(&self) -> BTreeMap<u32, &EdgeInfo> {
        self.edges.iter().map(|e| (e.id, e)).collect()
    }

    /// A `"parent->child"` label for edge `id`.
    #[must_use]
    pub fn edge_label(&self, e: &EdgeInfo) -> String {
        format!(
            "{}->{} (edge#{})",
            self.frame_label(e.parent),
            self.frame_label(e.child),
            e.id
        )
    }
}

// ---------------------------------------------------------------------------
// Observed publish history
// ---------------------------------------------------------------------------

/// The observed stream of pushes, in arrival order — the input to the four
/// history-dependent checks (multi-writer, short-buffer, inconsistent-rate,
/// out-of-order).
#[derive(Clone, Debug, Default)]
pub struct Observations {
    /// Every recorded push, in the order it was observed.
    pub events: Vec<PushSample>,
}

impl Observations {
    /// An empty observation stream.
    #[must_use]
    pub fn new() -> Observations {
        Observations { events: Vec::new() }
    }

    /// Wrap an already-collected push stream (e.g. from
    /// [`tf_tree_bench::fixture::spin_up`]).
    #[must_use]
    pub fn from_samples(events: Vec<PushSample>) -> Observations {
        Observations { events }
    }

    /// Reconstruct what can be reconstructed from a **live** arena's rings.
    ///
    /// Nobody was watching when those samples arrived, so this is strictly less
    /// than the fixture knows, and the difference is not cosmetic. Two of the
    /// seven checks are **structurally unable to fire** on the result, and
    /// `doctor` discloses both rather than printing a clean bill of health it
    /// did not earn — `TFT001` skips outright, and `TFT011` carries a note:
    ///
    /// * **multi-writer** — a ring remembers the *current* claim owner, not the
    ///   sequence of processes that wrote into it. Every sample therefore
    ///   carries the same pid and the distinct-pid count is always one.
    /// * **short-buffer** — `arrival_delay_ns` is how late a sample arrived
    ///   relative to its own stamp, which is a fact about the *publisher's*
    ///   clock at push time. Nothing in the arena records it; it is set to zero
    ///   here, and zero latency never exceeds any buffer span.
    ///
    /// What does survive is every stamp the rings still retain, in order, which
    /// is what the rate, ordering and reachability checks run on.
    #[must_use]
    pub fn from_arena(tree: &Tree, snap: &Snapshot) -> Observations {
        let view = tree.arena_view();
        let mut events = Vec::new();
        for e in &snap.edges {
            let Some(ring) = view.ring(EdgeId(e.id)) else {
                continue;
            };
            let head = ring.head.load(Ordering::Acquire);
            // The oldest logical index a reader may touch — `head - capacity`
            // is the slot currently being overwritten, not a retained sample.
            let retained = ring.retained().min(head);
            for i in (head - retained)..head {
                events.push(PushSample {
                    edge: e.id,
                    writer_pid: e.owner_pid,
                    stamp_ns: ring.stamps[(i & ring.mask) as usize].load(Ordering::Relaxed),
                    arrival_delay_ns: 0,
                });
            }
        }
        Observations { events }
    }

    /// Record one observed push.
    pub fn record(&mut self, sample: PushSample) {
        self.events.push(sample);
    }

    /// Group event indices by edge, preserving arrival order within each edge.
    #[must_use]
    pub fn by_edge(&self) -> BTreeMap<u32, Vec<&PushSample>> {
        let mut map: BTreeMap<u32, Vec<&PushSample>> = BTreeMap::new();
        for s in &self.events {
            map.entry(s.edge).or_default().push(s);
        }
        map
    }
}

// ---------------------------------------------------------------------------
// The seven checks
// ---------------------------------------------------------------------------

/// (1) A parent chain that never reaches a root within the frame budget is a
/// cycle. The builder rejects cycles, so on a live tree this is always clean;
/// the check exists to catch a corrupted or hand-built topology.
#[must_use]
pub fn check_cycles(snap: &Snapshot) -> Vec<Finding> {
    let max_id = snap.frames.iter().map(|f| f.id).max().unwrap_or(0);
    let mut parent = vec![0u32; (max_id as usize) + 1];
    for f in &snap.frames {
        parent[f.id as usize] = f.parent;
    }
    let budget = snap.frames.len() + 1;

    let mut in_cycle: BTreeSet<u32> = BTreeSet::new();
    for f in &snap.frames {
        let mut cur = f.id;
        let mut reached_root = false;
        for _ in 0..budget {
            let p = parent.get(cur as usize).copied().unwrap_or(0);
            if p == 0 {
                reached_root = true;
                break;
            }
            cur = p;
        }
        if !reached_root {
            in_cycle.insert(f.id);
        }
    }

    if in_cycle.is_empty() {
        return Vec::new();
    }
    let names: Vec<String> = in_cycle.iter().map(|&id| snap.frame_label(id)).collect();
    vec![Finding::error(
        Check::Cycle,
        format!(
            "{} frame(s) sit on a parent cycle (never reach a root): {}",
            names.len(),
            names.join(", ")
        ),
    )]
}

/// (2) A dynamic edge with no live writer holding its claim — data will go stale.
#[must_use]
pub fn check_unclaimed_dynamic(snap: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    for e in &snap.edges {
        if e.kind == EdgeKind::Dynamic && !e.claimed {
            out.push(Finding::warn(
                Check::UnclaimedDynamic,
                format!("dynamic edge {} has no live writer", snap.edge_label(e)),
            ));
        }
    }
    out
}

/// (3) More than one writer PID seen publishing to a single edge — the
/// single-writer invariant was violated (only observable via history; the claim
/// table forbids it on a live tree).
#[must_use]
pub fn check_multi_writer(obs: &Observations) -> Vec<Finding> {
    let mut out = Vec::new();
    for (edge, samples) in obs.by_edge() {
        let pids: BTreeSet<u32> = samples.iter().map(|s| s.writer_pid).collect();
        if pids.len() > 1 {
            let list: Vec<String> = pids.iter().map(u32::to_string).collect();
            out.push(Finding::error(
                Check::MultiWriter,
                format!(
                    "edge#{edge} saw {} distinct writer PIDs: {}",
                    pids.len(),
                    list.join(", ")
                ),
            ));
        }
    }
    out
}

/// (4) A ring buffer whose temporal span (`capacity × median period`) is shorter
/// than the largest observed publish latency: reads that far in the past fall off
/// the back of the ring.
#[must_use]
pub fn check_short_buffers(snap: &Snapshot, obs: &Observations) -> Vec<Finding> {
    let mut out = Vec::new();
    let by_edge = obs.by_edge();
    for e in &snap.edges {
        if e.kind != EdgeKind::Dynamic || e.capacity == 0 {
            continue;
        }
        let Some(samples) = by_edge.get(&e.id) else {
            continue;
        };
        let Some(period) = median_period(samples) else {
            continue;
        };
        let max_latency = samples
            .iter()
            .map(|s| s.arrival_delay_ns)
            .max()
            .unwrap_or(0);
        let span = i128::from(e.capacity) * i128::from(period);
        if i128::from(max_latency) > span {
            out.push(Finding::warn(
                Check::ShortBuffer,
                format!(
                    "edge {} holds ~{} ms but publish latency reaches {} ms",
                    snap.edge_label(e),
                    span / 1_000_000,
                    i128::from(max_latency) / 1_000_000
                ),
            ));
        }
    }
    out
}

/// (5) A frame whose inter-sample intervals vary widely (coefficient of variation
/// above a threshold) is publishing at an inconsistent rate.
#[must_use]
pub fn check_inconsistent_rates(obs: &Observations) -> Vec<Finding> {
    /// Coefficient-of-variation threshold above which a rate is "inconsistent".
    const COV_THRESHOLD: f64 = 0.5;
    let mut out = Vec::new();
    for (edge, samples) in obs.by_edge() {
        let intervals: Vec<f64> = samples
            .windows(2)
            .map(|w| (w[1].stamp_ns - w[0].stamp_ns) as f64)
            .collect();
        if intervals.len() < 3 {
            continue;
        }
        let mean = intervals.iter().sum::<f64>() / intervals.len() as f64;
        if mean <= 0.0 {
            continue;
        }
        let var =
            intervals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / intervals.len() as f64;
        let cov = var.sqrt() / mean;
        if cov > COV_THRESHOLD {
            out.push(Finding::warn(
                Check::InconsistentRate,
                format!(
                    "edge#{edge} publishes at an inconsistent rate (CoV {cov:.2}, mean period {:.1} ms)",
                    mean / 1_000_000.0
                ),
            ));
        }
    }
    out
}

/// (6) Frames not reachable from the main (largest) root component — an
/// unattached island in the tree.
#[must_use]
pub fn check_unreachable(snap: &Snapshot) -> Vec<Finding> {
    if snap.frames.is_empty() {
        return Vec::new();
    }
    let max_id = snap.frames.iter().map(|f| f.id).max().unwrap_or(0);
    let mut parent = vec![0u32; (max_id as usize) + 1];
    let mut present = vec![false; (max_id as usize) + 1];
    for f in &snap.frames {
        parent[f.id as usize] = f.parent;
        present[f.id as usize] = true;
    }
    let budget = snap.frames.len() + 1;

    // Root of each frame (walk parents to 0, cycle-safe via the step budget).
    let root_of = |mut cur: u32| -> u32 {
        for _ in 0..budget {
            let p = parent.get(cur as usize).copied().unwrap_or(0);
            if p == 0 || !present.get(p as usize).copied().unwrap_or(false) {
                return cur;
            }
            cur = p;
        }
        cur
    };

    // Tally component sizes by root; the biggest is the "main" tree.
    let mut sizes: BTreeMap<u32, usize> = BTreeMap::new();
    for f in &snap.frames {
        *sizes.entry(root_of(f.id)).or_default() += 1;
    }
    let Some((&main_root, _)) = sizes.iter().max_by_key(|(_, &n)| n) else {
        return Vec::new();
    };

    let unreachable: Vec<String> = snap
        .frames
        .iter()
        .filter(|f| root_of(f.id) != main_root)
        .map(|f| f.name.clone())
        .collect();

    if unreachable.is_empty() {
        return Vec::new();
    }
    vec![Finding::error(
        Check::Unreachable,
        format!(
            "{} frame(s) unreachable from the main root '{}': {}",
            unreachable.len(),
            snap.frame_label(main_root),
            unreachable.join(", ")
        ),
    )]
}

/// One edge's out-of-order evidence: how far its observed stream went
/// backwards, and how often.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfOrderRun {
    /// The edge the regressions were observed on.
    pub edge: u32,
    /// How many adjacent arrivals carried a stamp older than their predecessor.
    pub regressions: usize,
    /// The largest single step backwards, in nanoseconds. Always positive when
    /// `regressions > 0`; saturates at [`i64::MAX`] rather than wrapping, since
    /// a stamp is whatever an arbitrary publisher wrote.
    pub worst_backstep_ns: i64,
    /// The longest run of **consecutive** arrivals that `docs/PHASE1.md` §2
    /// invariant 6 would have rejected — each one carrying a stamp strictly
    /// older than the newest *accepted* before it.
    ///
    /// This is the "concentration" half of the evidence, and it is a different
    /// question from `regressions`. A clock that steps back by Δ against a
    /// publisher running at *f* rejects about Δ·*f* arrivals in one unbroken
    /// burst while producing a single adjacent regression; one misplaced sample
    /// produces a single adjacent regression too, and a rejected run of one.
    /// [`crate::checks`]'s `TFT019` is the consumer that needs to tell those
    /// apart. `regressions > 0` and `longest_rejected_run > 0` are equivalent
    /// conditions, so carrying this changes nothing about when `TFT018` fires.
    ///
    /// Counted in **arrivals**, not in seconds: the stamps are the quantity
    /// under suspicion and the stream carries no independent arrival clock.
    pub longest_rejected_run: usize,
}

/// The per-edge out-of-order evidence, in edge order. Empty when every observed
/// stream is monotone.
///
/// **One producer, two consumers, and that is the point.**
/// [`check_out_of_order`] reports this as `TFT018` and
/// [`crate::checks::ClockStepEvidence`] attributes it to a clock step.
/// `docs/PHASE5.md` §6's amendment requires the second to fire on *exactly* the
/// first's evidence — "an attribution, not a second detector" — and a second
/// scan of `obs` written to the same rule is precisely how that stops being
/// true after one refactor. One walk therefore computes both what `TFT018`
/// reports (`regressions`, `worst_backstep_ns`) and the concentration fact only
/// `TFT019` reads (`longest_rejected_run`).
#[must_use]
pub fn out_of_order_runs(obs: &Observations) -> Vec<OutOfOrderRun> {
    let mut out = Vec::new();
    for (edge, samples) in obs.by_edge() {
        let mut regressions = 0usize;
        let mut worst: i128 = 0;
        // The engine's own state, replayed: `newest` is what invariant 6 would
        // have compared against, which only an *accepted* push advances. Equal
        // stamps are accepted (replay is idempotent), so the test is `<`.
        let mut newest = i64::MIN;
        let mut run = 0usize;
        let mut longest_rejected_run = 0usize;
        let mut prev: Option<i64> = None;
        for s in &samples {
            let stamp = s.stamp_ns;
            if let Some(p) = prev.filter(|&p| stamp < p) {
                regressions += 1;
                // In `i128`: two stamps at opposite ends of `i64` differ by more
                // than `i64` holds, and both are values a publisher can write.
                worst = worst.max(i128::from(p) - i128::from(stamp));
            }
            if stamp < newest {
                run += 1;
                longest_rejected_run = longest_rejected_run.max(run);
            } else {
                run = 0;
                newest = stamp;
            }
            prev = Some(stamp);
        }
        if regressions > 0 {
            out.push(OutOfOrderRun {
                edge,
                regressions,
                worst_backstep_ns: i64::try_from(worst).unwrap_or(i64::MAX),
                longest_rejected_run,
            });
        }
    }
    out
}

/// (7) Stamps observed arriving out of monotonic order on an edge (a later
/// arrival carried an older stamp than an earlier one).
#[must_use]
pub fn check_out_of_order(obs: &Observations) -> Vec<Finding> {
    out_of_order_runs(obs)
        .into_iter()
        .map(|r| {
            Finding::error(
                Check::OutOfOrder,
                format!(
                    "edge#{} saw {} out-of-order stamp arrival(s)",
                    r.edge, r.regressions
                ),
            )
        })
        .collect()
}

/// The observed publish rate (Hz) of a per-edge event slice, from its median
/// interval. `None` exactly where [`median_period`] is `None`.
///
/// One definition, two consumers — `TFT007`'s comparison against the declared
/// nominal and the `edges` command's rate column — because "the rate this edge
/// publishes at" must be one number. A second copy is free to lose the
/// non-positive-median guard, and a backwards stream then reports a *negative*
/// rate: printed in one place, and compared against a nominal in the other.
pub(crate) fn observed_rate_hz(samples: &[&PushSample]) -> Option<f64> {
    median_period(samples).map(|ns| 1e9 / ns as f64)
}

/// The median inter-sample interval (nanoseconds) of a per-edge event slice.
///
/// `None` when the median is not a usable period. Intervals are raw stamp
/// differences, so an edge whose stamps arrive out of order (check 7's condition)
/// can have a **negative** median; letting that through made `capacity × period`
/// negative and flagged every dynamic edge as a short buffer, burying the real
/// finding under noise.
pub(crate) fn median_period(samples: &[&PushSample]) -> Option<i64> {
    if samples.len() < 2 {
        return None;
    }
    let mut intervals: Vec<i64> = samples
        .windows(2)
        .map(|w| w[1].stamp_ns - w[0].stamp_ns)
        .collect();
    intervals.sort_unstable();
    let median = intervals[intervals.len() / 2];
    if median <= 0 {
        return None;
    }
    Some(median)
}

/// Every Phase 1 finding over a captured snapshot and observed history.
///
/// There is deliberately no `Report`/`is_healthy`/`has_error` wrapper here.
/// `crate::catalogue::Report` is the one the `--exit-code` gate consults, and a
/// second type with the same name and near-identical methods one module away is
/// a trap: a reader landing on `has_error` has no way to tell which gate it
/// feeds. The catalogue routes each of these findings to a `TFT` id (or to
/// `Uncatalogued`), so this returns the raw list and lets that layer decide.
#[must_use]
pub fn all_findings(snap: &Snapshot, obs: &Observations) -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.extend(check_cycles(snap));
    findings.extend(check_unclaimed_dynamic(snap));
    findings.extend(check_multi_writer(obs));
    findings.extend(check_short_buffers(snap, obs));
    findings.extend(check_inconsistent_rates(obs));
    findings.extend(check_unreachable(snap));
    findings.extend(check_out_of_order(obs));
    findings
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn frame(id: u32, name: &str, parent: u32, depth: u16) -> FrameInfo {
        FrameInfo {
            id,
            name: name.to_owned(),
            parent,
            depth,
            edge_of_child: 0,
        }
    }

    fn dyn_edge(id: u32, parent: u32, child: u32, capacity: u32, claimed: bool) -> EdgeInfo {
        EdgeInfo {
            id,
            parent,
            child,
            kind: EdgeKind::Dynamic,
            capacity,
            interp: InterpPolicy::ScLerp,
            domain: 0,
            head: 0,
            claimed,
            claiming: false,
            owner_pid: if claimed { 1234 } else { 0 },
            newest_stamp: None,
            nominal_rate_mhz: None,
        }
    }

    fn sample(edge: u32, pid: u32, stamp_ns: i64, delay_ns: i64) -> PushSample {
        PushSample {
            edge,
            writer_pid: pid,
            stamp_ns,
            arrival_delay_ns: delay_ns,
        }
    }

    // --- (1) cycles -----------------------------------------------------

    #[test]
    fn detects_cycle() {
        // a -> b -> a is a cycle; neither reaches a root.
        let snap = Snapshot {
            frames: vec![frame(1, "a", 2, 0), frame(2, "b", 1, 0)],
            edges: vec![],
        };
        let findings = check_cycles(&snap);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, Check::Cycle);
        assert_eq!(findings[0].severity, Severity::Error);
    }

    #[test]
    fn healthy_tree_has_no_cycle() {
        // map(1) <- odom(2) <- base(3): a proper rooted chain.
        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base", 2, 2),
            ],
            edges: vec![],
        };
        assert!(check_cycles(&snap).is_empty());
    }

    // --- (2) unclaimed dynamic edges ------------------------------------

    #[test]
    fn detects_unclaimed_dynamic_edge() {
        let snap = Snapshot {
            frames: vec![frame(1, "map", 0, 0), frame(2, "odom", 1, 1)],
            edges: vec![dyn_edge(1, 1, 2, 512, false)],
        };
        let findings = check_unclaimed_dynamic(&snap);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, Check::UnclaimedDynamic);
    }

    #[test]
    fn claimed_dynamic_edge_is_clean() {
        let snap = Snapshot {
            frames: vec![frame(1, "map", 0, 0), frame(2, "odom", 1, 1)],
            edges: vec![dyn_edge(1, 1, 2, 512, true)],
        };
        assert!(check_unclaimed_dynamic(&snap).is_empty());
    }

    // --- (3) multi-writer contention ------------------------------------

    #[test]
    fn detects_multi_writer() {
        let obs = Observations::from_samples(vec![
            sample(1, 100, 0, 0),
            sample(1, 200, 1, 0), // a different PID on the same edge
        ]);
        let findings = check_multi_writer(&obs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, Check::MultiWriter);
        assert_eq!(findings[0].severity, Severity::Error);
    }

    #[test]
    fn single_writer_is_clean() {
        let obs = Observations::from_samples(vec![sample(1, 100, 0, 0), sample(1, 100, 1, 0)]);
        assert!(check_multi_writer(&obs).is_empty());
    }

    // --- (4) buffers shorter than observed latency ----------------------

    #[test]
    fn detects_short_buffer() {
        // capacity 4, ~10 ms period -> ~40 ms span; a 500 ms latency overruns it.
        let snap = Snapshot {
            frames: vec![frame(1, "map", 0, 0), frame(2, "odom", 1, 1)],
            edges: vec![dyn_edge(1, 1, 2, 4, true)],
        };
        let obs = Observations::from_samples(vec![
            sample(1, 1, 0, 500_000_000),
            sample(1, 1, 10_000_000, 500_000_000),
            sample(1, 1, 20_000_000, 500_000_000),
            sample(1, 1, 30_000_000, 500_000_000),
        ]);
        let findings = check_short_buffers(&snap, &obs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, Check::ShortBuffer);
    }

    #[test]
    fn ample_buffer_is_clean() {
        // capacity 4096, ~10 ms period -> ~40 s span; a 20 ms latency fits easily.
        let snap = Snapshot {
            frames: vec![frame(1, "map", 0, 0), frame(2, "odom", 1, 1)],
            edges: vec![dyn_edge(1, 1, 2, 4096, true)],
        };
        let obs = Observations::from_samples(vec![
            sample(1, 1, 0, 20_000_000),
            sample(1, 1, 10_000_000, 20_000_000),
            sample(1, 1, 20_000_000, 20_000_000),
            sample(1, 1, 30_000_000, 20_000_000),
        ]);
        assert!(check_short_buffers(&snap, &obs).is_empty());
    }

    /// Regression: `median_period` is a median of raw stamp differences, so a
    /// stream that arrives out of order has a **negative** one. Using it made
    /// `capacity × period` negative, so `max_latency > span` held for every
    /// dynamic edge and one bad stream turned into a ShortBuffer warning on all
    /// of them — burying check 7's real finding.
    #[test]
    fn out_of_order_stamps_do_not_fake_short_buffers() {
        let snap = Snapshot {
            frames: vec![frame(1, "map", 0, 0), frame(2, "odom", 1, 1)],
            edges: vec![dyn_edge(1, 1, 2, 4096, true)],
        };
        // Stamps march backwards: every interval, and so the median, is negative.
        let obs = Observations::from_samples(
            (0..8)
                .map(|k| sample(1, 1, 100_000_000 - k * 10_000_000, 20_000_000))
                .collect(),
        );
        assert!(
            check_short_buffers(&snap, &obs).is_empty(),
            "an out-of-order stream must not be reported as a short buffer"
        );
        // The condition is still reported, by the check that owns it.
        assert_eq!(check_out_of_order(&obs).len(), 1);
    }

    // --- (5) inconsistent publish rate ----------------------------------

    #[test]
    fn detects_inconsistent_rate() {
        // Wildly varying gaps: 1, 100, 1, 100 ms.
        let obs = Observations::from_samples(vec![
            sample(1, 1, 0, 0),
            sample(1, 1, 1_000_000, 0),
            sample(1, 1, 101_000_000, 0),
            sample(1, 1, 102_000_000, 0),
            sample(1, 1, 202_000_000, 0),
        ]);
        let findings = check_inconsistent_rates(&obs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, Check::InconsistentRate);
    }

    #[test]
    fn steady_rate_is_clean() {
        let obs =
            Observations::from_samples((0..10).map(|k| sample(1, 1, k * 10_000_000, 0)).collect());
        assert!(check_inconsistent_rates(&obs).is_empty());
    }

    // --- (6) unreachable frames -----------------------------------------

    #[test]
    fn detects_unreachable_frame() {
        // map(1)<-odom(2)<-base(3) is the main tree; island(4) is its own root.
        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base", 2, 2),
                frame(4, "island", 0, 0),
            ],
            edges: vec![],
        };
        let findings = check_unreachable(&snap);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, Check::Unreachable);
        assert!(findings[0].message.contains("island"));
    }

    #[test]
    fn fully_connected_tree_is_reachable() {
        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base", 2, 2),
            ],
            edges: vec![],
        };
        assert!(check_unreachable(&snap).is_empty());
    }

    // --- (7) out-of-order stamps ----------------------------------------

    #[test]
    fn detects_out_of_order_stamps() {
        let obs = Observations::from_samples(vec![
            sample(1, 1, 100, 0),
            sample(1, 1, 50, 0), // arrived later but older
        ]);
        let findings = check_out_of_order(&obs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, Check::OutOfOrder);
    }

    #[test]
    fn monotone_stamps_are_clean() {
        let obs = Observations::from_samples(vec![sample(1, 1, 0, 0), sample(1, 1, 1, 0)]);
        assert!(check_out_of_order(&obs).is_empty());
    }

    /// **`longest_rejected_run` replays invariant 6, which is not the same
    /// question as counting adjacent inversions.**
    ///
    /// A push is rejected when its stamp is older than the newest *accepted*
    /// one, and a rejected push does not advance that mark — so one step
    /// backwards rejects everything until the publisher climbs back over it,
    /// however monotone those arrivals are among themselves. That run length is
    /// the concentration evidence `TFT019` reads; `regressions` is 1 for both
    /// the stray inversion and the clock step, which is exactly why a second
    /// number is needed.
    ///
    /// The stream here is 0, 10, 20, then a 15-back step, then the publisher
    /// carrying on at 10: 5, 15, 25. `5` and `15` are below the newest accepted
    /// `20`; `25` is not. One adjacent inversion, two rejected arrivals.
    ///
    /// Mutant: move `newest = stamp;` out of the `else` arm so every arrival
    /// advances it. Applied: `left: 1`, `right: 2` — with the mark advancing on
    /// a rejected push, the run collapses to the adjacent-inversion count and
    /// the two numbers stop being different questions.
    /// Mutant B: `if stamp < newest` -> `if stamp <= newest`. Applied:
    /// `left: 3`, `right: 2` — the equal stamp at the end of the second stream
    /// is counted as rejected, though invariant 6 accepts it.
    #[test]
    fn a_rejected_run_is_measured_against_the_newest_accepted_stamp() {
        let stream = |stamps: &[i64]| {
            Observations::from_samples(stamps.iter().map(|&s| sample(1, 1, s, 0)).collect())
        };

        let runs = out_of_order_runs(&stream(&[0, 10, 20, 5, 15, 25]));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].regressions, 1, "one adjacent step backwards");
        assert_eq!(runs[0].longest_rejected_run, 2);
        assert_eq!(runs[0].worst_backstep_ns, 15);

        // Equal stamps are accepted (replay is idempotent), so the arrival that
        // lands exactly on the newest accepted stamp ends the run.
        let runs = out_of_order_runs(&stream(&[0, 10, 20, 5, 15, 20]));
        assert_eq!(runs[0].longest_rejected_run, 2);
    }

    // --- healthy live fixture -------------------------------------------

    /// **A live writer's claim must resolve to that writer's pid.**
    ///
    /// The owner word packs the acquisition epoch above the slot
    /// (`(epoch << 16) | (slot + 1)`), and `claim` starts the epoch at 1, so a
    /// hand-rolled `word - 1` never names a slot. It produced `pid 0` for every
    /// claimed edge — which reads as "the writer is gone", is what the `tree`
    /// command printed in its writer column, and is the exact condition
    /// `TFT014` reports as a leaked claim.
    ///
    /// Mutant: decode with `u32::try_from(owner_word - 1).ok()` instead of
    /// `slot_of`. Applied: `owner_pid` is 0 for all four claimed edges and the
    /// assertion fails.
    #[test]
    fn a_held_claim_resolves_to_the_writers_pid() {
        let tree = tf_tree_bench::fixture::build_tree().expect("build fixture");
        let (writers, _samples) = tf_tree_bench::fixture::spin_up(&tree).expect("claim and push");
        let snap = Snapshot::capture(&tree);

        let claimed: Vec<&EdgeInfo> = snap.edges.iter().filter(|e| e.claimed).collect();
        // Non-vacuity: the fixture holds four dynamic claims for the whole test.
        assert_eq!(claimed.len(), 4, "the fixture must hold its claims");
        let me = std::process::id();
        for e in claimed {
            assert_eq!(
                e.owner_pid, me,
                "edge#{} is claimed by this process but resolved to pid {}",
                e.id, e.owner_pid
            );
        }
        drop(writers);
    }

    /// **`Snapshot::capture` must not report a reserved headroom slot as a
    /// frame.**
    ///
    /// `ArenaView::frame_record` bounds an id against `max_frames`, which is
    /// `frame_count + 1 + frame_headroom` — *not* against `frame_count`. So the
    /// `1..=frame_count` bound is the check, not a convenience: walking to
    /// `max_frames` hands back zeroed reserved slots, which `frame_record`
    /// returns happily and which would print as frames with an empty name.
    /// `tf_tree_c::unstable::tft_tree_frame_name` shipped exactly that bug, and
    /// the test that missed it used a zero-headroom fixture, which makes the two
    /// bounds coincide. This one has headroom.
    ///
    /// **The two guards are redundant against *this* state, and no single
    /// mutation fails this test — which is stated rather than hidden.** A
    /// headroom slot is both out of `1..=frame_count` and zeroed, so either
    /// check alone excludes it. Both are kept because they are justified
    /// independently: the bound is the semantic one (`frame_count` is what a
    /// frame id may reach), while `name_hash != 0` filters the *value* a race
    /// the bound cannot touch produces — `frame_count` is bumped **before** the
    /// record is written, so an interner in another process can be counted one
    /// instant before its name exists. It filters rather than closes: nothing
    /// here orders that record's stores against this reader, and the ordering
    /// note on `capture` says what does and does not follow from that. The
    /// window needs two processes and is **not** asserted anywhere here.
    ///
    /// Mutant: change the loop bound to `1..=header.max_frames`. Applied: still
    /// `PASS` — the `name_hash` filter absorbs it, which is how it was found
    /// that the bound alone is not what this test pins.
    /// Mutant B: delete the `name_hash == 0` filter. Applied: still `PASS` —
    /// the bound absorbs it. That is the pre-change state.
    /// Mutant C: both together. Applied: `left: ["map", "odom", "base", "", "",
    /// "", ""]` against `right: ["map", "odom", "base"]` — the four reserved
    /// slots print as frames with an empty name, which is the failure this
    /// guards.
    #[test]
    fn capture_does_not_report_reserved_frame_slots_as_frames() {
        let tree = tf_tree::TreeBuilder::new()
            .dynamic_edge(
                "map",
                "odom",
                tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(8)),
            )
            .dynamic_edge(
                "odom",
                "base",
                tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(8)),
            )
            .frame_headroom(4)
            .build()
            .expect("build");

        let snap = Snapshot::capture(&tree);
        let names: Vec<&str> = snap.frames.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["map", "odom", "base"],
            "the four reserved headroom slots are not frames"
        );

        // Non-vacuity: a name interned at runtime *does* appear, so the bound
        // excludes empty slots rather than everything past the declaration.
        tree.frame("laser").expect("intern into the headroom");
        let snap = Snapshot::capture(&tree);
        assert_eq!(
            snap.frames
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["map", "odom", "base", "laser"]
        );
    }

    #[test]
    fn healthy_fixture_reports_clean() {
        let tree = tf_tree_bench::fixture::build_tree().expect("build fixture");
        // Hold the writers so the dynamic edges stay claimed during capture.
        let (writers, samples) = tf_tree_bench::fixture::spin_up(&tree).expect("populate history");
        let snap = Snapshot::capture(&tree);
        let obs = Observations::from_samples(samples);
        let findings = all_findings(&snap, &obs);
        assert!(
            findings.is_empty(),
            "healthy fixture produced findings: {findings:?}"
        );
        assert_eq!(snap.frames.len(), 24, "fixture should have 24 frames");
        assert_eq!(snap.edges.len(), 23, "fixture should have 23 edges");
        drop(writers);
    }
}
