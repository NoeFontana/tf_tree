//! `tf_tree doctor` — the seven Phase 1 health checks (`docs/PHASE1.md` §12
//! *CLI*).
//!
//! Each check is a pure function over a captured [`Snapshot`] plus, where the
//! condition is only visible in history, the [`Observations`] stream. Tests
//! build offending snapshots directly, including ones a safe live tree cannot
//! reach.

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
/// Distinct from [`crate::catalogue::Severity`]; the two meet in
/// `From<Severity> for crate::catalogue::Severity`.
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
    /// A human-readable explanation.
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
    /// sentinel), which names no slot; `TFT014` stays silent rather than guess.
    pub claiming: bool,
    /// The participant slot the claim word names (the link into
    /// [`Snapshot::participants`]), or `None` when unclaimed or mid-handoff.
    pub owner_slot: Option<u32>,
    /// The current claim owner's PID (`0` if unclaimed). A label, not liveness
    /// (`docs/PHASE2.md` §5.1, decision `0028`); ask `owner_slot` and
    /// [`ParticipantInfo::alive`].
    pub owner_pid: u32,
    /// Newest published stamp, if any samples exist.
    pub newest_stamp: Option<i64>,
    /// The publisher's clock offset (host wall clock minus header stamp), or
    /// `None` when none was recorded. The arena never stores `0` for a computed
    /// zero (`docs/decisions/0036`), so the mapping is lossless.
    pub clock_offset_nanos: Option<i64>,
    /// The rate this edge was **declared** to publish at, in milli-hertz, or
    /// `None` when `EdgeRecord::nominal_rate_mhz == 0`; an absent rate means
    /// `TFT007` says nothing.
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

/// What the kernel says about a participant slot's **lock byte**
/// (`docs/PHASE2.md` §5.1). Three answers, because "nobody asked" is not
/// "free".
///
/// The byte is the only liveness fact; [`RecordedProcess`] is a `/proc`
/// inference carried beside it (see [`Snapshot::probe_lock_facts`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockByte {
    /// `F_OFD_GETLK` reported a conflicting lock: somebody's open file
    /// description holds this slot (an OFD lock names no process).
    Held,
    /// No conflict. On a rendezvous arena this is the leak signature when the
    /// record is not `FREE` (`docs/decisions/0028`, *The ordering*).
    Free,
    /// Nobody asked, or the kernel would not answer. **Not** evidence either
    /// way (§6.2's fail-safe rule).
    Unknown,
}

/// What `/proc` says about the process a slot's lock-file identity record
/// names.
///
/// A diagnostic inference, never a protocol decision (`docs/PHASE2.md` §5.1).
/// Three-valued because `Identity::matches_running_process` maps every read
/// failure to `false`, which would make [`crate::checks::slot_leak`] fire on
/// every slot without a usable `/proc` (`docs/decisions/0028`).
/// `crate::recorded_given` is the only place a `/proc` answer becomes one of
/// these; *cannot tell* is [`Self::Unknown`], never [`Self::Gone`].
/// PID-namespace inputs: `docs/decisions/0033`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordedProcess {
    /// `/proc` has an entry for the recorded pid whose start time matches.
    Running,
    /// Provably gone: no `/proc` entry on a host that would show one, or a
    /// different start time (recycled pid).
    Gone,
    /// No identity record, or `/proc` would not say. **Never evidence of
    /// death.**
    Unknown,
}

/// The lock file's facts about one participant slot, as
/// [`Snapshot::probe_lock_facts`] merges them in. A struct because the pid here
/// is the **lock file's**, not the arena record's (`docs/decisions/0028` plan
/// step 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotFacts {
    /// What the kernel said about the slot's lock byte.
    pub byte: LockByte,
    /// What `/proc` said about the process the slot's identity record names.
    pub recorded: RecordedProcess,
    /// The pid the identity record names, or `None` if none was written or no
    /// lock file was read. **The pid [`Self::recorded`] is about**; not
    /// [`ParticipantInfo::pid`], which is zero on a `RESERVED` slot.
    pub recorded_pid: Option<u32>,
}

impl SlotFacts {
    /// The facts a source with no lock file has: none.
    #[must_use]
    pub fn unasked() -> SlotFacts {
        SlotFacts {
            byte: LockByte::Unknown,
            recorded: RecordedProcess::Unknown,
            recorded_pid: None,
        }
    }
}

/// What a participant slot's `state` word says (`docs/PHASE2.md` §5.1). Not
/// liveness; see [`ParticipantInfo::alive`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    /// Nobody holds this slot.
    Free,
    /// A registrant won the slot and has not published its identity yet.
    Reserved,
    /// An identity is published here. **Says nothing about whether the process
    /// it names still exists.**
    Live,
}

/// One participant slot in a captured [`Snapshot`]. Carries only the fields the
/// checks read; `start_time` and `incarnation` are omitted (the claim owner's
/// epoch is per-edge, so no join with `incarnation` exists).
#[derive(Clone, Debug)]
pub struct ParticipantInfo {
    /// Slot index, also the lock-file byte (`docs/PHASE2.md` §3.7).
    pub slot: u32,
    /// The record's `state` word.
    pub state: SlotState,
    /// The recorded process id. A label, not an identity: composing `(pid,
    /// start_time)` here would be a second liveness spelling
    /// (`docs/decisions/0028` §6.2).
    pub pid: u32,
    /// Whether the participant is still running, from
    /// [`Tree::participant_alive`] (`F_OFD_GETLK` on the lock byte for a tree
    /// from `tf_tree::open`, a `/proc` inference otherwise). `false` for
    /// `Free`/`Reserved`. It cannot tell a joiner mid-attach from one that died
    /// there; [`Self::byte`] can (`docs/decisions/0028` plan step 6).
    pub alive: bool,
    /// The lock byte, or [`LockByte::Unknown`] if nothing asked. Filled by
    /// [`Snapshot::probe_lock_facts`], never [`Snapshot::capture`].
    pub byte: LockByte,
    /// What `/proc` said about the lock-file identity record's process, or
    /// [`RecordedProcess::Unknown`]. Judged from the identity record, not
    /// [`Self::pid`]: on a `RESERVED` slot `pid` is zero or the previous
    /// occupant's (`docs/decisions/0028`), and a read-only participant (D18)
    /// has no arena record at all.
    pub recorded: RecordedProcess,
    /// The pid the identity record names (the pid [`Self::recorded`] is about),
    /// or `None`. Differs from [`Self::pid`] on the rows `TFT014` reports.
    pub recorded_pid: Option<u32>,
}

/// A point-in-time, read-only capture of a tree's topology, edges, claims and
/// participant slots.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// All frames, id order.
    pub frames: Vec<FrameInfo>,
    /// All edges, id order.
    pub edges: Vec<EdgeInfo>,
    /// Every participant slot, free ones included.
    pub participants: Vec<ParticipantInfo>,
}

impl Snapshot {
    /// The captured slot `slot`, or `None` if this snapshot has no such slot.
    #[must_use]
    pub fn participant(&self, slot: u32) -> Option<&ParticipantInfo> {
        // Not `participants[slot]`: hand-built snapshots are sparse, and out of
        // range means `None`.
        self.participants.iter().find(|p| p.slot == slot)
    }

    /// Capture the current state of `tree` through its read-only arena view.
    #[must_use]
    pub fn capture(tree: &Tree) -> Snapshot {
        let view = tree.arena_view();
        let header = view.header();
        let topo = view.topology();

        // `Relaxed`: `finish` bumps `frame_count` before writing the record, so
        // `Acquire` would order against nothing useful. This walk is
        // best-effort against a concurrent interner; the `name_hash != 0`
        // filter below is not a synchronisation edge.
        let frame_count = header.frame_count.load(Ordering::Relaxed);
        let mut frames = Vec::with_capacity(frame_count as usize);
        for id in 1..=frame_count {
            // The three checks of `tf_tree::Tree::frames`, kept as its own walk
            // because a `FrameInfo` needs `parent`, `depth` and
            // `edge_of_child`: (1) `FrameId::new` rejects 0; (2) `id <=
            // frame_count`, since walking to `max_frames` would return zeroed
            // headroom slots; (3) `name_hash != 0`.
            let Some(fid) = FrameId::new(id) else {
                continue;
            };
            let Some(rec) = view.frame_record(fid) else {
                continue;
            };
            // The count is bumped before the record is written, so a zero
            // `name_hash` (BLAKE3 is non-zero even for "") means "not written
            // yet"; skip it.
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
            // The owner word is `(epoch << 16) | (slot + 1)`
            // (`tf_tree_core::edge::pack_owner`); `slot_of` returns `u32::MAX`
            // for unclaimed or mid-claim, both `None`.
            let owner_slot = match owner_word {
                0 => None,
                w => match tf_tree_core::edge::slot_of(w) {
                    u32::MAX => None,
                    slot => Some(slot),
                },
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
                owner_slot,
                // The claim names a slot, not a PID; resolve through the
                // participant table. No `LIVE` identity prints as pid 0, a
                // label not a verdict.
                owner_pid: owner_slot
                    .and_then(|slot| view.participants().identity(slot))
                    .map_or(0, |(pid, _start, _inc)| pid),
                newest_stamp,
                clock_offset_nanos: match claim.clock_offset_nanos.load(Ordering::Relaxed) {
                    0 => None,
                    ns => Some(ns),
                },
                nominal_rate_mhz: match rec.nominal_rate_mhz {
                    0 => None,
                    mhz => Some(mhz),
                },
            });
        }

        // Captured once per slot: `participant_alive` is a syscall, and one
        // answer per slot keeps edge and slot findings consistent.
        let table = view.participants();
        let capacity = table.capacity();
        let mut participants = Vec::with_capacity(capacity);
        let unasked = SlotFacts::unasked();
        for slot in 0..capacity as u32 {
            let Some(rec) = table.get(slot) else {
                continue;
            };
            // Two reads of the state word bracket the probe and must agree: a
            // detach or reuse landing inside `F_OFD_GETLK` looks like a leak. A
            // moved word means active use; fail safe to alive (§6.2).
            let before = rec.state.load(Ordering::Acquire);
            let alive = tree.participant_alive(slot);
            let after = rec.state.load(Ordering::Acquire);
            let state = match tf_tree_core::participant::state_of(after) {
                tf_tree_core::participant::LIVE => SlotState::Live,
                tf_tree_core::participant::RESERVED => SlotState::Reserved,
                _ => SlotState::Free,
            };
            participants.push(ParticipantInfo {
                slot,
                state,
                // Read unconditionally, unlike `identity`: a leaked slot's
                // value is the pid that should be there.
                pid: rec.pid.load(Ordering::Relaxed),
                alive: alive || before != after,
                byte: unasked.byte,
                recorded: unasked.recorded,
                recorded_pid: unasked.recorded_pid,
            });
        }

        Snapshot {
            frames,
            edges,
            participants,
        }
    }

    /// Ask the lock file about every captured slot and fold its answers into
    /// the rows.
    ///
    /// `docs/decisions/0028` piece 2 requires the state word be read **before**
    /// the byte. `probe` receives the already-captured [`ParticipantInfo`], so
    /// the call cannot be hoisted above `capture` and still type-check;
    /// `doctor::tests::the_probe_is_handed_the_word_that_was_read_first` keeps
    /// that meaningful.
    ///
    /// # It does **not** overwrite [`ParticipantInfo::alive`], and `top` does
    ///
    /// `top` assigns `alive = held` — *"the kernel's answer wins over the arena
    /// record's"* — because it renders one liveness column and the kernel is
    /// the fact behind it. That is the right rule for a column and the wrong
    /// one here: `TFT014` has to separate a slot whose byte the kernel released
    /// from one a forked child is still holding for a dead process
    /// (`docs/decisions/0028` plan step 6, cases (a) and (b)), and both facts
    /// have to survive to the check for it to. So the byte arrives beside
    /// `alive` rather than on top of it, and [`crate::checks::slot_leak`] is
    /// the one place the two are composed.
    ///
    /// # Every captured slot is probed, including the `FREE` ones
    ///
    /// [`Self::capture`] emits a row per slot of the arena's participant table,
    /// `FREE` records included, and every one of them is asked. That is not
    /// thoroughness for its own sake: a **read-only** participant holds a lock
    /// byte and writes no arena record at all (D18, and Python's default), so
    /// its record reads `FREE` while its byte reads *held* — and when such a
    /// process is `fork`ed and dies, the byte its child inherited is still held
    /// on behalf of a pid that is gone, with a `FREE` record over it. Skipping
    /// `FREE` rows here, or in [`crate::checks::slot_leak`], makes the single
    /// most likely fork leak on a Python deployment invisible.
    pub fn probe_lock_facts(&mut self, mut probe: impl FnMut(&ParticipantInfo) -> SlotFacts) {
        for p in &mut self.participants {
            let facts = probe(p);
            p.byte = facts.byte;
            p.recorded = facts.recorded;
            p.recorded_pid = facts.recorded_pid;
        }
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

/// The observed stream of pushes, in arrival order — the input to the four
/// history-dependent checks.
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
    /// Two checks are structurally unable to fire on the result, and `doctor`
    /// discloses both (`TFT001` skips, `TFT011` notes): **multi-writer** (a
    /// ring remembers only the current claim owner) and **short-buffer**
    /// (`arrival_delay_ns` is not in the arena and is set to zero). Stamps,
    /// hence rate, ordering and reachability, survive.
    #[must_use]
    pub fn from_arena(tree: &Tree, snap: &Snapshot) -> Observations {
        let view = tree.arena_view();
        let mut events = Vec::new();
        for e in &snap.edges {
            let Some(ring) = view.ring(EdgeId(e.id)) else {
                continue;
            };
            let head = ring.head.load(Ordering::Acquire);
            // The oldest retained index; `head - capacity` is the slot being
            // overwritten.
            let retained = ring.retained().min(head);
            for i in (head - retained)..head {
                events.push(PushSample {
                    edge: e.id,
                    writer_pid: e.owner_pid,
                    stamp_ns: ring.stamps[(i & ring.mask()) as usize].load(Ordering::Relaxed),
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

/// (1) A parent chain that never reaches a root within the frame budget is a
/// cycle. Always clean on a live tree; catches a corrupted or hand-built
/// topology.
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

/// (3) More than one writer PID seen publishing to a single edge (only
/// observable via history).
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

/// Intervals an edge must have retained before its spread means anything. Named
/// because `TFT008`'s skip reason quotes it.
pub(crate) const SPREAD_MIN_INTERVALS: usize = 3;

/// What [`check_inconsistent_rates`] found, **and what it looked at**: an empty
/// finding list means either "every edge is even" or "nothing to measure", and
/// `TFT008` reports the second as a stated skip (`docs/PHASE5.md` §6).
pub struct RateSpread {
    /// One finding per edge whose spread is above the threshold.
    pub findings: Vec<Finding>,
    /// How many edges the rule was applied to.
    pub judged: usize,
    /// How many were **withheld** because their publisher has stopped.
    pub withheld: usize,
}

/// (5) A frame whose inter-sample intervals vary widely (coefficient of
/// variation above a threshold) is publishing at an inconsistent rate.
///
/// `stopped` names edges whose publisher has stopped (computed by
/// `tf_tree_cli::checks`, which has the clock). They are **withheld, not
/// judged**: a dead publisher's ring is perfectly spaced. Empty when the caller
/// has no such evidence.
#[must_use]
pub fn check_inconsistent_rates(obs: &Observations, stopped: &BTreeSet<u32>) -> RateSpread {
    /// Coefficient-of-variation threshold above which a rate is "inconsistent".
    const COV_THRESHOLD: f64 = 0.5;
    let mut out = Vec::new();
    let mut judged = 0usize;
    let mut withheld = 0usize;
    for (edge, samples) in obs.by_edge() {
        let intervals: Vec<f64> = samples
            .windows(2)
            .map(|w| (w[1].stamp_ns - w[0].stamp_ns) as f64)
            .collect();
        if intervals.len() < SPREAD_MIN_INTERVALS {
            continue;
        }
        let mean = intervals.iter().sum::<f64>() / intervals.len() as f64;
        if mean <= 0.0 {
            continue;
        }
        if stopped.contains(&edge) {
            withheld += 1;
            continue;
        }
        judged += 1;
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
    RateSpread {
        findings: out,
        judged,
        withheld,
    }
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
    /// The largest single step backwards, in nanoseconds; positive when
    /// `regressions > 0`, saturating at [`i64::MAX`].
    pub worst_backstep_ns: i64,
    /// The longest run of **consecutive** arrivals that `docs/PHASE1.md` §2
    /// invariant 6 would have rejected (each strictly older than the newest
    /// *accepted* before it). Unlike `regressions`, it separates a clock step
    /// (one burst) from one misplaced sample; [`crate::checks`]'s `TFT019`
    /// needs that. Counted in arrivals.
    pub longest_rejected_run: usize,
}

/// The per-edge out-of-order evidence, in edge order; empty when every stream
/// is monotone.
///
/// One walk feeds both consumers: [`check_out_of_order`] (`TFT018`) and
/// [`crate::checks::ClockStepEvidence`], which `docs/PHASE5.md` §6 requires to
/// fire on *exactly* the first's evidence.
#[must_use]
pub fn out_of_order_runs(obs: &Observations) -> Vec<OutOfOrderRun> {
    let mut out = Vec::new();
    for (edge, samples) in obs.by_edge() {
        let mut regressions = 0usize;
        let mut worst: i128 = 0;
        // Replays the engine: only an *accepted* push advances `newest`, and
        // equal stamps are accepted, so the test is `<`.
        let mut newest = i64::MIN;
        let mut run = 0usize;
        let mut longest_rejected_run = 0usize;
        let mut prev: Option<i64> = None;
        for s in &samples {
            let stamp = s.stamp_ns;
            if let Some(p) = prev.filter(|&p| stamp < p) {
                regressions += 1;
                // In `i128`: stamps at opposite ends of `i64` differ by more
                // than `i64` holds.
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
/// interval; `None` exactly where [`median_period`] is `None`. Shared by
/// `TFT007` and the `edges` command so there is one rate.
pub(crate) fn observed_rate_hz(samples: &[&PushSample]) -> Option<f64> {
    median_period(samples).map(|ns| 1e9 / ns as f64)
}

/// The median inter-sample interval (nanoseconds) of a per-edge event slice.
///
/// `None` when the median is not a usable period; out-of-order stamps can make
/// it negative.
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
/// Returns the raw list; `crate::catalogue::Report` is the one gate
/// `--exit-code` consults.
#[must_use]
pub fn all_findings(snap: &Snapshot, obs: &Observations) -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.extend(check_cycles(snap));
    findings.extend(check_unclaimed_dynamic(snap));
    findings.extend(check_multi_writer(obs));
    findings.extend(check_short_buffers(snap, obs));
    // No `stopped` set: this aggregate has no clock or source; `TFT008` is the
    // caller that does.
    findings.extend(check_inconsistent_rates(obs, &BTreeSet::new()).findings);
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
            owner_slot: claimed.then_some(0),
            owner_pid: if claimed { 1234 } else { 0 },
            newest_stamp: None,
            clock_offset_nanos: None,
            nominal_rate_mhz: None,
        }
    }

    /// A participant table of one running process, in slot 0 — the owner every
    /// claimed [`dyn_edge`] names.
    fn one_live_participant() -> Vec<ParticipantInfo> {
        vec![ParticipantInfo {
            slot: 0,
            state: SlotState::Live,
            pid: 1234,
            alive: true,
            byte: LockByte::Held,
            recorded: RecordedProcess::Running,
            recorded_pid: Some(1234),
        }]
    }

    /// The probe is called **once per captured row**, and each call is handed
    /// *that row* with its word already populated, so the read order documented
    /// on [`Snapshot::probe_lock_facts`] cannot be hoisted.
    ///
    /// Mutant: probe only rows whose `state` is `Live` ⇒ the `RESERVED` and
    /// `FREE` rows vanish from `seen` and the first assertion fails.
    #[test]
    fn the_probe_is_handed_the_word_that_was_read_first() {
        let row = |slot: u32, state: SlotState| ParticipantInfo {
            slot,
            state,
            pid: 4711,
            alive: false,
            byte: LockByte::Unknown,
            recorded: RecordedProcess::Unknown,
            recorded_pid: None,
        };
        let mut snap = Snapshot {
            frames: vec![],
            edges: vec![],
            participants: vec![
                row(0, SlotState::Live),
                row(1, SlotState::Reserved),
                row(2, SlotState::Free),
            ],
        };

        let mut seen = Vec::new();
        snap.probe_lock_facts(|p| {
            // The probe's whole input is the captured row.
            seen.push((p.slot, p.state));
            SlotFacts {
                byte: LockByte::Held,
                recorded: RecordedProcess::Gone,
                recorded_pid: Some(1841 + p.slot),
            }
        });

        assert_eq!(
            seen,
            vec![
                (0, SlotState::Live),
                (1, SlotState::Reserved),
                (2, SlotState::Free),
            ],
            "every captured row is probed exactly once, and is handed its own \
             already-read state word"
        );
        assert_eq!(snap.participants[2].byte, LockByte::Held);
        assert_eq!(snap.participants[2].recorded_pid, Some(1843));
    }

    fn sample(edge: u32, pid: u32, stamp_ns: i64, delay_ns: i64) -> PushSample {
        PushSample {
            edge,
            writer_pid: pid,
            stamp_ns,
            arrival_delay_ns: delay_ns,
        }
    }

    #[test]
    fn detects_cycle() {
        // a -> b -> a is a cycle; neither reaches a root.
        let snap = Snapshot {
            frames: vec![frame(1, "a", 2, 0), frame(2, "b", 1, 0)],
            edges: vec![],
            participants: one_live_participant(),
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
            participants: one_live_participant(),
        };
        assert!(check_cycles(&snap).is_empty());
    }

    #[test]
    fn detects_unclaimed_dynamic_edge() {
        let snap = Snapshot {
            frames: vec![frame(1, "map", 0, 0), frame(2, "odom", 1, 1)],
            edges: vec![dyn_edge(1, 1, 2, 512, false)],
            participants: one_live_participant(),
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
            participants: one_live_participant(),
        };
        assert!(check_unclaimed_dynamic(&snap).is_empty());
    }

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

    #[test]
    fn detects_short_buffer() {
        // capacity 4, ~10 ms period -> ~40 ms span; a 500 ms latency overruns it.
        let snap = Snapshot {
            frames: vec![frame(1, "map", 0, 0), frame(2, "odom", 1, 1)],
            edges: vec![dyn_edge(1, 1, 2, 4, true)],
            participants: one_live_participant(),
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
            participants: one_live_participant(),
        };
        let obs = Observations::from_samples(vec![
            sample(1, 1, 0, 20_000_000),
            sample(1, 1, 10_000_000, 20_000_000),
            sample(1, 1, 20_000_000, 20_000_000),
            sample(1, 1, 30_000_000, 20_000_000),
        ]);
        assert!(check_short_buffers(&snap, &obs).is_empty());
    }

    /// Regression: out-of-order stamps give a **negative** `median_period`,
    /// which flagged every dynamic edge as a short buffer.
    #[test]
    fn out_of_order_stamps_do_not_fake_short_buffers() {
        let snap = Snapshot {
            frames: vec![frame(1, "map", 0, 0), frame(2, "odom", 1, 1)],
            edges: vec![dyn_edge(1, 1, 2, 4096, true)],
            participants: one_live_participant(),
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
        let spread = check_inconsistent_rates(&obs, &BTreeSet::new());
        assert_eq!(spread.findings.len(), 1);
        assert_eq!(spread.findings[0].check, Check::InconsistentRate);
        assert_eq!(spread.judged, 1, "the edge was measured, not skipped");
        assert_eq!(spread.withheld, 0);
    }

    #[test]
    fn steady_rate_is_clean() {
        let obs =
            Observations::from_samples((0..10).map(|k| sample(1, 1, k * 10_000_000, 0)).collect());
        let spread = check_inconsistent_rates(&obs, &BTreeSet::new());
        assert!(spread.findings.is_empty());
        // Non-vacuity: an empty list also means "nothing to measure"; `judged`
        // tells them apart.
        assert_eq!(spread.judged, 1);
    }

    /// **An edge whose publisher has stopped is withheld rather than judged.**
    ///
    /// Mutant: drop the `stopped.contains(&edge)` arm ⇒ `judged` reads 1 and
    /// `withheld` 0.
    #[test]
    fn a_stopped_publisher_is_withheld_from_the_spread() {
        let obs =
            Observations::from_samples((0..10).map(|k| sample(1, 1, k * 10_000_000, 0)).collect());
        let spread = check_inconsistent_rates(&obs, &BTreeSet::from([1]));
        assert_eq!(spread.judged, 0, "a stopped publisher must not be judged");
        assert_eq!(spread.withheld, 1);
        assert!(spread.findings.is_empty());
    }

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
            participants: one_live_participant(),
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
            participants: one_live_participant(),
        };
        assert!(check_unreachable(&snap).is_empty());
    }

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

    /// **`longest_rejected_run` replays invariant 6, not adjacent inversions.**
    /// A rejected push does not advance the newest-accepted mark.
    ///
    /// Stream: 0, 10, 20, 5, 15, 25 — one adjacent inversion, two rejected
    /// arrivals.
    ///
    /// Mutant: move `newest = stamp;` out of the `else` arm ⇒ `left: 1`,
    /// `right: 2`. Mutant B: `<` to `<=` ⇒ `left: 3`, `right: 2` (an equal
    /// stamp counted as rejected).
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

        // Equal stamps are accepted, so an arrival exactly on the newest ends
        // the run.
        let runs = out_of_order_runs(&stream(&[0, 10, 20, 5, 15, 20]));
        assert_eq!(runs[0].longest_rejected_run, 2);
    }

    /// **A live writer's claim must resolve to that writer's pid.** The owner
    /// word is `(epoch << 16) | (slot + 1)`, so `word - 1` never names a slot.
    ///
    /// Mutant: decode with `u32::try_from(owner_word - 1).ok()` instead of
    /// `slot_of` ⇒ `owner_pid` is 0 for all four claimed edges.
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
    /// frame.** `frame_record` bounds against `max_frames` (`frame_count + 1 +
    /// frame_headroom`), so the `1..=frame_count` bound matters; this fixture
    /// has headroom.
    ///
    /// The bound and the `name_hash != 0` filter are redundant against this
    /// state, so no single mutation fails it. Mutant C, both removed ⇒ `["map",
    /// "odom", "base", "", "", "", ""]` against `["map", "odom", "base"]`.
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

        // Non-vacuity: a runtime-interned name does appear.
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
