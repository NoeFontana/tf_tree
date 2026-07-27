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

use tf_tree::{EdgeId, EdgeKind, FrameId, InterpPolicy, Tree};
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

/// The result of running all seven checks.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// All findings, in check order.
    pub findings: Vec<Finding>,
}

impl Report {
    /// Whether the tree is clean (no findings).
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.findings.is_empty()
    }

    /// Whether any finding is an [`Severity::Error`].
    #[must_use]
    pub fn has_error(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
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
    /// The current claim owner's PID (`0` if unclaimed).
    pub owner_pid: u32,
    /// Newest published stamp, if any samples exist.
    pub newest_stamp: Option<i64>,
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

        let frame_count = header.frame_count.load(Ordering::Relaxed);
        let mut frames = Vec::with_capacity(frame_count as usize);
        for id in 1..=frame_count {
            let Some(fid) = FrameId::new(id) else {
                continue;
            };
            let Some(rec) = view.frame_record(fid) else {
                continue;
            };
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
            // `owner_word == 0` means *unclaimed*. Without this guard the
            // `saturating_sub(1)` below yields slot 0 and the edge is reported as
            // owned by whichever process happens to hold participant slot 0 — a
            // plausible-looking wrong pid, which is worse than none.
            let owner_slot = if owner_word == 0 {
                None
            } else {
                u32::try_from(owner_word - 1).ok()
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
                // A3: the claim names a *participant slot*, not a PID, so the
                // owning process is resolved through the participant table. A
                // slot that no longer resolves means the owner detached or died
                // between the two reads, which reports as pid 0 — the honest
                // answer, and what the reaper will act on.
                owner_pid: owner_slot
                    .and_then(|slot| view.participants().identity(slot))
                    .map_or(0, |(pid, _start, _inc)| pid),
                newest_stamp,
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
    /// [`Self::LOST_ON_A_LIVE_ARENA`] names them so `doctor` can say so instead
    /// of printing a clean bill of health it did not earn:
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

    /// The checks [`Self::from_arena`] cannot supply evidence for.
    pub const LOST_ON_A_LIVE_ARENA: &'static [Check] = &[Check::MultiWriter, Check::ShortBuffer];

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

/// (7) Stamps observed arriving out of monotonic order on an edge (a later
/// arrival carried an older stamp than an earlier one).
#[must_use]
pub fn check_out_of_order(obs: &Observations) -> Vec<Finding> {
    let mut out = Vec::new();
    for (edge, samples) in obs.by_edge() {
        let mut regressions = 0usize;
        for w in samples.windows(2) {
            if w[1].stamp_ns < w[0].stamp_ns {
                regressions += 1;
            }
        }
        if regressions > 0 {
            out.push(Finding::error(
                Check::OutOfOrder,
                format!("edge#{edge} saw {regressions} out-of-order stamp arrival(s)"),
            ));
        }
    }
    out
}

/// The median inter-sample interval (nanoseconds) of a per-edge event slice.
///
/// `None` when the median is not a usable period. Intervals are raw stamp
/// differences, so an edge whose stamps arrive out of order (check 7's condition)
/// can have a **negative** median; letting that through made `capacity × period`
/// negative and flagged every dynamic edge as a short buffer, burying the real
/// finding under noise.
fn median_period(samples: &[&PushSample]) -> Option<i64> {
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

/// Run all seven checks over a captured snapshot and observed history.
#[must_use]
pub fn run(snap: &Snapshot, obs: &Observations) -> Report {
    let mut findings = Vec::new();
    findings.extend(check_cycles(snap));
    findings.extend(check_unclaimed_dynamic(snap));
    findings.extend(check_multi_writer(obs));
    findings.extend(check_short_buffers(snap, obs));
    findings.extend(check_inconsistent_rates(obs));
    findings.extend(check_unreachable(snap));
    findings.extend(check_out_of_order(obs));
    Report { findings }
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
            owner_pid: if claimed { 1234 } else { 0 },
            newest_stamp: None,
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

    // --- healthy live fixture -------------------------------------------

    #[test]
    fn healthy_fixture_reports_clean() {
        let tree = tf_tree_bench::fixture::build_tree().expect("build fixture");
        // Hold the writers so the dynamic edges stay claimed during capture.
        let (writers, samples) = tf_tree_bench::fixture::spin_up(&tree).expect("populate history");
        let snap = Snapshot::capture(&tree);
        let obs = Observations::from_samples(samples);
        let report = run(&snap, &obs);
        assert!(
            report.is_healthy(),
            "healthy fixture produced findings: {:?}",
            report.findings
        );
        assert_eq!(snap.frames.len(), 24, "fixture should have 24 frames");
        assert_eq!(snap.edges.len(), 23, "fixture should have 23 edges");
        drop(writers);
    }
}
