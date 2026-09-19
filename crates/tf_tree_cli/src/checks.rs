//! Detection for the diagnostics catalogue — `docs/PHASE5.md` §6.
//!
//! [`crate::catalogue`] owns the identifiers and the printing; this module
//! decides what fired, as pure functions over captured data.
//!
//! # Skipped, not passed
//!
//! A check that cannot judge reports [`crate::catalogue::Status::Skipped`] with a
//! reason, because silence is indistinguishable from "found nothing"
//! (`docs/PHASE5.md` §0.0 enumerates them).
//!
//! * `TFT002`/`TFT003` detect nothing here: their evidence lives in the bridge
//!   process (`tf_tree_bridge`'s `StaticStore`), not in the arena.
//! * `TFT001`, `TFT004`, `TFT018`, `TFT019` depend on the [`PushStream`].
//! * `TFT005` depends on the [`Clock`]; `TFT007`–`TFT011` on evidence being
//!   present (`tft007`, `tft009`, [`stopped_publishers`], [`no_counter_evidence`]).
//! * `TFT013` has a grace period (`tft013`); `TFT014` skips on a frozen `.tft`
//!   ([`SlotTable`]); `TFT016` skips off Linux.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

use tf_tree::unstable::EdgeKind;
use tf_tree::{Domain, EdgeId, SensorDomain, SimDomain, SteadyDomain, SystemDomain, Tree};
use tf_tree_bench::fixture::PushSample;

use crate::catalogue::{CheckOutcome, Finding, Report, Status, Tft};
use crate::doctor::{
    self, EdgeInfo, LockByte, Observations, ParticipantInfo, RecordedProcess, SlotState, Snapshot,
};
use crate::hostfacts::{HostFacts, MemLock, ShmemThp, Thp};

/// A stamp further than this from the reference clock is a units error, not lateness.
const ABSURD_HORIZON_NS: i64 = 365 * 24 * 3600 * 1_000_000_000;

/// How far ahead of the wall clock a stamp may sit before `TFT005` fires.
const FUTURE_TOLERANCE_NS: i64 = 50_000_000;

/// Fraction of lookups that may fail with an extrapolation error before
/// `TFT010` calls the edge a hotspot.
const EXTRAP_HOTSPOT_RATE: f64 = 0.01;

/// An interval this many times the edge's median counts as a dropout (`TFT009`).
const GAP_FACTOR: i64 = 3;

/// The largest `|offset|` any publish pipeline could account for.
pub const OFFSET_BEYOND_ANY_PIPELINE_NS: i64 = 10_000_000_000;

/// Occupancy above this fraction of a table's capacity fires `TFT015`.
pub(crate) const OCCUPANCY_LIMIT: f64 = 0.80;

/// Where the reference clock for the time-based checks came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clock {
    /// The newest stamp is within `ABSURD_HORIZON_NS` of the system clock.
    Wall(i64),
    /// The stamps share no epoch with the system clock; the median stands in.
    NewestStamp(i64),
}

impl Clock {
    /// The reference instant.
    #[must_use]
    pub fn nanos(self) -> i64 {
        match self {
            Clock::Wall(n) | Clock::NewestStamp(n) => n,
        }
    }

    /// A label for the report header.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Clock::Wall(_) => "system wall clock",
            Clock::NewestStamp(_) => {
                "median arena stamp (its stamps do not share an epoch with the system clock)"
            }
        }
    }

    /// Decide which clock applies, given every edge's newest stamp.
    #[must_use]
    pub fn decide(newest_stamps: &[i64], system_unix_nanos: i64) -> Clock {
        // `i128`: a corrupt stamp must not overflow the distance.
        let horizon = i128::from(ABSURD_HORIZON_NS);
        let now = i128::from(system_unix_nanos);
        let agree = newest_stamps
            .iter()
            .filter(|&&n| (i128::from(n) - now).abs() <= horizon)
            .count();
        // `>=` also covers the empty arena.
        if agree * 2 >= newest_stamps.len() {
            return Clock::Wall(system_unix_nanos);
        }
        let mut sorted = newest_stamps.to_vec();
        sorted.sort_unstable();
        Clock::NewestStamp(sorted[sorted.len() / 2])
    }
}

/// Per-edge facts gathered in one pass over the arena: the counter regions
/// (`docs/PHASE5.md` §5) plus the shape of the ring's retained window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EdgeStats {
    /// The edge these are about.
    pub edge: u32,
    /// Successful lookups that traversed this edge — the denominator.
    pub lookups_ok: u64,
    /// Requests older than the retained window.
    pub extrap_before: u64,
    /// Requests newer than the newest sample.
    pub extrap_after: u64,
    /// High-water mark of the distance past either end of the window.
    pub worst_extrap_gap_ns: i64,
    /// When the most recent failure happened; `0` means never.
    pub last_err_nanos: i64,
    /// Oldest stamp the ring still retains, if any.
    pub oldest_stamp: Option<i64>,
    /// Newest stamp the ring holds, if any.
    pub newest_stamp: Option<i64>,
    /// How many of the retained stamps are strictly negative.
    pub negative_stamps: u64,
    /// How many of the retained stamps are exactly zero.
    pub zero_stamps: u64,
    /// Participants whose *most recent* failure was on this edge, as
    /// `(slot, pid)` — `ParticipantCounters::last_err_edge` closes the loop from
    /// "this edge is failing" to "this process is failing".
    pub blamed: Vec<(u32, u32)>,
}

impl EdgeStats {
    /// The temporal span the ring currently holds, or `None` with under two
    /// samples.
    #[must_use]
    pub fn retained_span_ns(&self) -> Option<i64> {
        match (self.oldest_stamp, self.newest_stamp) {
            (Some(o), Some(n)) if n > o => Some(n - o),
            _ => None,
        }
    }
}

/// Read the counter regions and ring windows for every edge in `snap`.
#[must_use]
pub fn collect_edge_stats(tree: &Tree, snap: &Snapshot) -> Vec<EdgeStats> {
    let view = tree.arena_view();
    let max_participants = view.header().max_participants;

    // Blame map: for every participant slot, the edge it most recently failed on.
    let mut blame: Vec<(u32, u32, u32)> = Vec::new(); // (edge, slot, pid)
    for slot in 0..max_participants {
        let Some(pc) = view.participant_counters(slot) else {
            continue;
        };
        let extrap = pc.err_extrap_before.load(Ordering::Relaxed)
            + pc.err_extrap_after.load(Ordering::Relaxed);
        if extrap == 0 {
            continue;
        }
        let edge = pc.last_err_edge.load(Ordering::Relaxed);
        // `u32::MAX` and edge 0 are sentinels, not real edges.
        if edge == u32::MAX || edge == 0 {
            continue;
        }
        let pid = view.participants().identity(slot).map_or(0, |(p, _, _)| p);
        blame.push((edge, slot, pid));
    }

    let mut out = Vec::with_capacity(snap.edges.len());
    for e in &snap.edges {
        let eid = EdgeId(e.id);
        let mut st = EdgeStats {
            edge: e.id,
            newest_stamp: e.newest_stamp,
            blamed: blame
                .iter()
                .filter(|(edge, _, _)| *edge == e.id)
                .map(|(_, slot, pid)| (*slot, *pid))
                .collect(),
            ..EdgeStats::default()
        };
        if let Some(c) = view.edge_counters(eid) {
            st.lookups_ok = c.lookups_ok.load(Ordering::Relaxed);
            st.extrap_before = c.err_extrap_before.load(Ordering::Relaxed);
            st.extrap_after = c.err_extrap_after.load(Ordering::Relaxed);
            st.worst_extrap_gap_ns = c.worst_extrap_gap_ns.load(Ordering::Relaxed);
            st.last_err_nanos = c.last_err_nanos.load(Ordering::Relaxed);
        }
        if let Some(ring) = view.ring(eid) {
            let head = ring.head.load(Ordering::Acquire);
            // `head - capacity` is being overwritten, hence `retained()`.
            let retained = ring.retained().min(head);
            for i in (head - retained)..head {
                let s = ring.stamps[(i & ring.mask()) as usize].load(Ordering::Relaxed);
                if s < 0 {
                    st.negative_stamps += 1;
                } else if s == 0 {
                    st.zero_stamps += 1;
                }
                st.oldest_stamp = Some(st.oldest_stamp.map_or(s, |o: i64| o.min(s)));
            }
        }
        out.push(st);
    }
    out
}

/// How the push stream a check reads was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushStream {
    /// Every attempted push, recorded as it happened, with the writer that made it and
    /// the delay it arrived with.
    Observed,
    /// Every transform a recording holds, replayed in the recording's own log
    /// order (`doctor --from-bag`).
    Recorded,
    /// Reconstructed from an arena's rings with no writer attached: a frozen
    /// `.tft` (`doctor --from-file`), or any other arena at rest.
    RingsAtRest,
    /// Reconstructed from the rings of an arena being written while it is read
    /// (`doctor --attach`).
    RingsUnderWriter,
}

impl PushStream {
    /// Why this stream cannot name the process that pushed a sample (`TFT001`),
    /// or `None` when it can.
    #[must_use]
    pub fn no_writer_identity(self) -> Option<&'static str> {
        match self {
            PushStream::Observed => None,
            PushStream::Recorded => Some(
                "a recording carries no publisher identity — a tf2_msgs/TFMessage has no sender \
                 field and an MCAP channel names the topic, not the node — so two nodes \
                 publishing one edge are indistinguishable from one. This is the check \
                 docs/PHASE4.md §1.3 predicts a real stack will fail, and a bag cannot answer it",
            ),
            PushStream::RingsAtRest | PushStream::RingsUnderWriter => Some(
                "this push stream was replayed from the rings, which remember the current claim \
                 owner and not the sequence of writers, so every sample carries one pid",
            ),
        }
    }

    /// Why the clock offsets in this arena were not recorded against *this*
    /// host's clock, now (`TFT004`), or `None` when they were.
    #[must_use]
    pub fn no_live_receipt(self) -> Option<&'static str> {
        match self {
            // `Observed` is the fixture: it is the publisher, running now.
            PushStream::Observed | PushStream::RingsUnderWriter => None,
            PushStream::Recorded => Some(
                "this arena was built by replaying a recording, so its clock offsets were \
                 measured between *ingest* time and the recording's stamps: a 2024 bag read in \
                 2026 records a two-year offset on every edge, which is arithmetically correct \
                 and says nothing about any publisher's clock. Point doctor at a live arena \
                 (--attach) to ask this question",
            ),
            PushStream::RingsAtRest => Some(
                "this arena is at rest — a frozen .tft is a byte copy, offsets included — so its \
                 recorded offsets are from whenever it was written and not from now. The same \
                 reason TFT014 skips a frozen source, one field over",
            ),
        }
    }

    /// Why this stream cannot contain an arrival invariant 6 would have rejected
    /// (`TFT018`, and therefore `TFT019`), or `None` when it can.
    #[must_use]
    pub fn no_rejected_arrivals(self) -> Option<&'static str> {
        match self {
            PushStream::Observed | PushStream::Recorded => None,
            PushStream::RingsAtRest => Some(
                "this push stream was replayed from an arena's rings, and a ring holds only the \
                 pushes the engine accepted: SampleRing::push refuses a stamp older than the \
                 last one, so an out-of-order arrival was rejected and left no trace to find. \
                 Running here would pass unconditionally, which is a fabricated all-clear and \
                 not a result. Point doctor at the recording instead (--from-bag), where the \
                 arrivals are in the order they happened",
            ),
            PushStream::RingsUnderWriter => Some(
                "this push stream was replayed from a ring that is being written while it is \
                 read, so a slot at the old end can already hold the next lap's sample — which \
                 reads as an inversion on a correctly ordered publisher. The rings also hold \
                 only accepted pushes, so a real rejected arrival would be absent even without \
                 the tearing. Freeze the arena and use --from-file, or point doctor at a \
                 recording with --from-bag",
            ),
        }
    }

    /// Why this stream carries no per-sample arrival delay (`TFT011`'s Phase 1
    /// `capacity × period` half), or `None` when it does.
    #[must_use]
    pub fn no_arrival_delays(self) -> Option<&'static str> {
        match self {
            PushStream::Observed => None,
            PushStream::Recorded => Some(
                "a recording's log time is the recorder's clock and its stamp is the \
                 publisher's, so differencing them would report clock offset as publish latency",
            ),
            PushStream::RingsAtRest | PushStream::RingsUnderWriter => {
                Some("an arena records no receipt time, so a replayed sample has no arrival delay")
            }
        }
    }
}

/// Why the `docs/PHASE5.md` §5 counters carry no verdict about an arena, or
/// `None` when they do.
#[must_use]
pub fn no_counter_evidence(counters: bool, stats: &[EdgeStats]) -> Option<&'static str> {
    if !counters {
        return Some(
            "the engine was built without the `counters` feature (PHASE5 §5.5), so every counter \
             reads zero and \"no failures\" cannot be told from \"nothing counted\"",
        );
    }
    let lookups: u64 = stats
        .iter()
        .map(|s| {
            s.lookups_ok
                .saturating_add(s.extrap_before)
                .saturating_add(s.extrap_after)
        })
        .sum();
    if lookups == 0 {
        return Some(
            "no *writable* participant has served a lookup — every EdgeCounter reads zero — so \
             the counters cannot distinguish a healthy arena from an unexercised one, and a \
             pass here would be an all-clear about nothing. Four states produce it, and the \
             fourth is the one a running robot is usually in: an arena built from a recording \
             (--from-bag, tf_tree ingest, tf_tree freeze) is written and never read; a live \
             arena reaches it before its first consumer; a publish-only node never looks up \
             what it writes; and **a read-only consumer cannot record a counter at all** — \
             writing one is a write, and read-only is the consumer default (D18). So consumers \
             may be hammering this arena while every counter reads zero. `tf_tree \
             participants` shows which attachments are rw and which are ro; if they are all \
             ro, attaching one read-write consumer is what produces evidence, at the cost of \
             the MMU protection D18 exists for",
        );
    }
    None
}

/// Everything the catalogue runs against.
pub struct Inputs<'a> {
    /// Captured topology, edges and claims.
    pub snap: &'a Snapshot,
    /// Observed push history. Reconstructed from the rings on a live arena,
    /// which is strictly less than a fixture records.
    pub obs: &'a Observations,
    /// Per-edge counters and ring windows.
    pub stats: &'a [EdgeStats],
    /// Facts about the host, `None` off Linux.
    pub host: Option<HostFacts>,
    /// The reference clock and where it came from.
    pub clock: Clock,
    /// Arena size in bytes, for the `RLIMIT_MEMLOCK` comparison.
    pub arena_bytes: u64,
    /// Table occupancies as `(what, used, capacity)`.
    pub occupancy: Vec<(&'static str, u32, u32)>,
    /// `TFT018`'s per-edge evidence, already split by domain tag and by whether
    /// its rejections are concentrated.
    pub clock_step: &'a ClockStepEvidence,
    /// How the push stream in `obs` was obtained, which is what decides whether
    /// `TFT001`, `TFT011`'s Phase 1 half, `TFT018` and `TFT019` have evidence.
    pub stream: PushStream,
    /// What kind of participant table `snap` carries, which is what decides
    /// whether `TFT014` has evidence.
    pub slots: SlotTable,
    /// Whether the engine compiled `docs/PHASE5.md` §5's counters in.
    pub counters: bool,
}

/// Where the participant table in a [`Snapshot`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotTable {
    /// The table of an arena that exists now — a live shared segment (`doctor
    /// --attach`), or one this process built and still holds (the fixture, `doctor
    /// --from-bag`).
    Current,
    /// A byte copy of a table as it stood at some past instant
    /// (`doctor --from-file`).
    Image,
}

impl SlotTable {
    /// Why this table cannot say whether a participant is running, or `None`
    /// when it can.
    #[must_use]
    pub fn no_liveness(self) -> Option<&'static str> {
        match self {
            SlotTable::Current => None,
            SlotTable::Image => Some(
                "a frozen .tft holds a byte copy of the whole arena (PHASE5 §2.3), participant \
                 records included, so every slot in it names a process that exited when the \
                 freeze finished and every claim names a slot from that run. Reporting them \
                 would fire on every correct .tft ever written; a file has no assigner for a \
                 leaked slot to wedge. Ask the arena instead: tf_tree doctor --attach",
            ),
        }
    }
}

/// Run every catalogue entry plus the two id-less Phase 1 checks.
#[must_use]
pub fn run(inp: &Inputs<'_>, suppress: &BTreeSet<Tft>) -> Report {
    let mut outcomes = Vec::with_capacity(Tft::ALL.len());
    for check in Tft::ALL {
        let mut o = match check {
            Tft::Tft001 => tft001(inp),
            Tft::Tft002 => CheckOutcome::skipped(
                check,
                "detected by tf_tree_bridge::StaticStore during ingest; \
                 its state is process-local and the arena keeps no history of a static edge's value",
            ),
            Tft::Tft003 => CheckOutcome::skipped(
                check,
                "detected by tf_tree_bridge::StaticStore during ingest; \
                 EdgeRecord::kind is fixed at declaration, so no arena reader can observe it change",
            ),
            Tft::Tft004 => tft004(inp),
            Tft::Tft005 => tft005(inp),
            Tft::Tft006 => tft006(inp),
            Tft::Tft007 => tft007(inp),
            Tft::Tft008 => tft008(inp),
            Tft::Tft009 => tft009(inp),
            Tft::Tft010 => tft010(inp),
            Tft::Tft011 => tft011(inp),
            Tft::Tft012 => tft012(inp),
            Tft::Tft013 => tft013(inp),
            Tft::Tft014 => tft014(inp),
            Tft::Tft015 => tft015(inp),
            Tft::Tft016 => tft016(inp),
            Tft::Tft017 => tft017(inp),
            Tft::Tft018 => tft018(inp),
            Tft::Tft019 => tft019(inp),
        };
        o.suppressed = suppress.contains(&check);
        outcomes.push(o);
    }

    Report {
        outcomes,
        // Empty, and the field stays: `uncatalogued` is part of the stable `--json`
        // schema, and it is the shape a future check with no id would take.
        uncatalogued: Vec::new(),
    }
}

/// `TFT001` — more than one writer pid on one edge.
fn tft001(inp: &Inputs<'_>) -> CheckOutcome {
    if let Some(why) = inp.stream.no_writer_identity() {
        return CheckOutcome::skipped(Tft::Tft001, why);
    }
    CheckOutcome::ran(
        Tft::Tft001,
        doctor::check_multi_writer(inp.obs)
            .into_iter()
            .map(|f| Finding::about(Tft::Tft001, "edge", f.message))
            .collect(),
    )
}

/// Every clock offset in `snap` that says something about a **live** publisher,
/// or the one reason there are none.
fn live_clock_offsets(
    snap: &Snapshot,
    stream: PushStream,
    clock: Clock,
) -> Result<Vec<(&EdgeInfo, i64)>, &'static str> {
    if let Some(why) = stream.no_live_receipt() {
        return Err(why);
    }
    if !matches!(clock, Clock::Wall(_)) {
        return Err(
            "the arena's stamps do not share an epoch with the system clock, so a recorded \
             (wall clock - stamp) is the epoch difference and not a publisher's clock offset; \
             this is TFT005's condition and deliberately the same one",
        );
    }
    // `claimed`, for the reason `tft014` gates the same loop.
    let measured: Vec<(&EdgeInfo, i64)> = snap
        .edges
        .iter()
        .filter(|e| e.claimed && !e.claiming)
        .filter_map(|e| e.clock_offset_nanos.map(|ns| (e, ns)))
        .collect();
    if measured.is_empty() {
        return Err(
            "no live claim in this arena has recorded a clock offset. A writer samples one on its \
             first push and then once per second of published data on an edge that declares a \
             rate — once per 1024 pushes on one that does not, which is 102 s at 10 Hz — and \
             only on a SystemDomain edge, since (wall clock - stamp) is an offset in no other \
             time domain. So: no writer, none that has pushed, none whose edge is in the wall \
             clock's domain, or none that has reached its first sample",
        );
    }
    Ok(measured)
}

/// What `TFT004` measured, for the report metadata — the fleet's offsets, which
/// the check itself deliberately does not turn into findings.
#[must_use]
pub fn clock_offset_note(snap: &Snapshot, stream: PushStream, clock: Clock) -> Option<String> {
    let measured = live_clock_offsets(snap, stream, clock).ok()?;
    let mut offsets: Vec<i64> = measured.iter().map(|&(_, ns)| ns).collect();
    offsets.sort_unstable();
    let ms = |ns: i64| ns as f64 / 1e6;
    // The mean of the two middles on an even count, not `[len / 2]`.
    let mid = offsets.len() / 2;
    let median = if offsets.len().is_multiple_of(2) {
        ((i128::from(offsets[mid - 1]) + i128::from(offsets[mid])) / 2) as i64
    } else {
        offsets[mid]
    };
    Some(format!(
        "TFT004 measured {} publisher clock offset(s): median {:.3} ms, range {:.3} ms to \
         {:.3} ms. An offset is the publisher's clock error *plus* its stamp-to-push latency and \
         one sample cannot separate them, so only offsets beyond {:.0} s are reported as \
         findings; read the spread against a pipeline you know",
        offsets.len(),
        ms(median),
        ms(offsets[0]),
        ms(offsets[offsets.len() - 1]),
        OFFSET_BEYOND_ANY_PIPELINE_NS as f64 / 1e9,
    ))
}

/// `TFT004` — a publisher whose clock cannot be reconciled with this host's.
fn tft004(inp: &Inputs<'_>) -> CheckOutcome {
    let measured = match live_clock_offsets(inp.snap, inp.stream, inp.clock) {
        Ok(m) => m,
        Err(why) => return CheckOutcome::skipped(Tft::Tft004, why),
    };

    let out = measured
        .iter()
        // `unsigned_abs`, not `abs`.
        .filter(|(_, ns)| ns.unsigned_abs() > OFFSET_BEYOND_ANY_PIPELINE_NS.unsigned_abs())
        .map(|(e, ns)| {
            let secs = *ns as f64 / 1e9;
            // Replay, not staleness, is the alternative reading of a large positive
            // offset.
            let direction = if *ns > 0 {
                "behind this host's — or it is republishing recorded data, which reads the same \
                 way here"
            } else {
                "ahead of this host's"
            };
            Finding::on_edge(
                Tft::Tft004,
                e.id,
                inp.snap.edge_label(e),
                format!(
                    "its clock reads {:.1} s {direction}, measured at a push. No publish \
                     pipeline accounts for {:.1} s between stamping a transform and pushing it, \
                     so this is the clock and not latency",
                    secs.abs(),
                    secs.abs()
                ),
            )
        })
        .collect();
    CheckOutcome::ran(Tft::Tft004, out)
}

/// `TFT005` — a newest stamp ahead of the wall clock.
fn tft005(inp: &Inputs<'_>) -> CheckOutcome {
    let Clock::Wall(now) = inp.clock else {
        return CheckOutcome::skipped(
            Tft::Tft005,
            "the arena's stamps do not share an epoch with the system clock (EdgeRecord::domain \
             permits any time domain), so \"in the future\" has no meaning here",
        );
    };
    let mut out = Vec::new();
    for e in &inp.snap.edges {
        let Some(newest) = e.newest_stamp else {
            continue;
        };
        let ahead = newest - now;
        if ahead > FUTURE_TOLERANCE_NS {
            out.push(Finding::on_edge(
                Tft::Tft005,
                e.id,
                inp.snap.edge_label(e),
                format!(
                    "newest stamp is {} ms ahead of the wall clock (tolerance {} ms)",
                    ahead / 1_000_000,
                    FUTURE_TOLERANCE_NS / 1_000_000
                ),
            ));
        }
    }
    CheckOutcome::ran(Tft::Tft005, out)
}

/// `TFT006` — stamps whose *value* is impossible.
fn tft006(inp: &Inputs<'_>) -> CheckOutcome {
    let now = inp.clock.nanos();
    let wall = matches!(inp.clock, Clock::Wall(_));
    let index = inp.snap.edge_index();
    let mut out = Vec::new();
    for st in inp.stats {
        let Some(e) = index.get(&st.edge) else {
            continue;
        };
        // Reasons first, label second.
        let mut reasons: Vec<String> = Vec::new();
        if st.negative_stamps > 0 {
            reasons.push(format!(
                "{} retained stamp(s) are negative, which is invalid in every time domain",
                st.negative_stamps
            ));
        }
        if wall && st.zero_stamps > 0 {
            reasons.push(format!(
                "{} retained stamp(s) are exactly 0; this arena's stamps are Unix time, \
                 so that is 1970 and means the field was never set",
                st.zero_stamps
            ));
        }
        // The distance rule catches the units error a range check cannot.
        let mut ends = vec![("newest", st.newest_stamp)];
        if st.oldest_stamp != st.newest_stamp {
            ends.push(("oldest", st.oldest_stamp));
        }
        for (what, stamp) in ends {
            let Some(s) = stamp else { continue };
            // `i128`, like `Clock::decide`.
            let dist = (i128::from(s) - i128::from(now)).abs();
            if dist > i128::from(ABSURD_HORIZON_NS) {
                reasons.push(format!(
                    "{what} retained stamp {s} is {} days from the reference clock",
                    dist / i128::from(24 * 3600 * 1_000_000_000i64)
                ));
            }
        }
        if reasons.is_empty() {
            continue;
        }
        let label = inp.snap.edge_label(e);
        for reason in reasons {
            out.push(Finding::on_edge(
                Tft::Tft006,
                st.edge,
                label.clone(),
                reason,
            ));
        }
    }
    CheckOutcome::ran(Tft::Tft006, out)
}

/// What evidence one edge offers `TFT007`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum RateEvidence {
    /// Nothing declared a rate for this edge — `EdgeRecord::nominal_rate_mhz`
    /// is 0, or the edge is static and cannot have one — so there is nothing to
    /// deviate from.
    NotDeclared,
    /// A rate is declared but too few intervals were observed to measure one
    /// against it.
    TooFewIntervals,
    /// Both halves exist, in hertz.
    Comparable {
        /// What the topology declared.
        declared_hz: f64,
        /// What the retained stamps show, from their median interval.
        observed_hz: f64,
    },
}

/// Relative deviation from a declared rate before `TFT007` fires.
const RATE_TOLERANCE: f64 = 0.20;

/// Intervals needed before an observed rate is worth comparing to a declared one.
const RATE_MIN_INTERVALS: usize = 8;

/// What one edge can tell `TFT007`.
fn rate_evidence(e: &doctor::EdgeInfo, samples: Option<&[&PushSample]>) -> RateEvidence {
    // A static edge carries its pose inline and never publishes, so it has no rate to
    // hold or miss.
    if e.kind != EdgeKind::Dynamic {
        return RateEvidence::NotDeclared;
    }
    let Some(mhz) = e.nominal_rate_mhz else {
        return RateEvidence::NotDeclared;
    };
    let too_few = RateEvidence::TooFewIntervals;
    let Some(samples) = samples else {
        return too_few;
    };
    if samples.len() < RATE_MIN_INTERVALS + 1 {
        return too_few;
    }
    // `None` for a non-positive median (identical or backwards stamps).
    let Some(observed_hz) = doctor::observed_rate_hz(samples) else {
        return too_few;
    };
    RateEvidence::Comparable {
        declared_hz: f64::from(mhz) / 1000.0,
        observed_hz,
    }
}

/// `TFT007` — the observed publish rate is far from the declared nominal.
fn tft007(inp: &Inputs<'_>) -> CheckOutcome {
    let by_edge = inp.obs.by_edge();
    let stopped = stopped_publishers(inp.obs, inp.clock, inp.stream);
    let mut out = Vec::new();
    let mut declared = 0usize;
    let mut comparable = 0usize;
    let mut withheld = 0usize;
    for e in &inp.snap.edges {
        match rate_evidence(e, by_edge.get(&e.id).map(Vec::as_slice)) {
            RateEvidence::NotDeclared => continue,
            RateEvidence::TooFewIntervals => declared += 1,
            RateEvidence::Comparable {
                declared_hz,
                observed_hz,
            } => {
                declared += 1;
                // Withheld, not reported: a second warn id for one fault
                // inflates the `--exit-code warn` count (`docs/PHASE5.md` §6).
                if stopped.contains_key(&e.id) {
                    withheld += 1;
                    continue;
                }
                comparable += 1;
                let ratio = observed_hz / declared_hz;
                if (ratio - 1.0).abs() <= RATE_TOLERANCE {
                    continue;
                }
                let effect = if ratio > 1.0 {
                    "the ring therefore retains proportionally less history than the \
                     rate_hz x history_secs it was sized from"
                } else {
                    "consumers interpolating across the gap see a longer step than the \
                     declared rate implies"
                };
                out.push(Finding::on_edge(
                    Tft::Tft007,
                    e.id,
                    inp.snap.edge_label(e),
                    format!(
                        "publishes at {observed_hz:.2} Hz against a declared {declared_hz:.2} Hz \
                         ({:+.0}%, tolerance {:.0}%); {effect}",
                        (ratio - 1.0) * 100.0,
                        RATE_TOLERANCE * 100.0
                    ),
                ));
            }
        }
    }
    // Not `declared == 0`: that leaves a hole before `rate_coverage_note`.
    if comparable == 0 {
        return CheckOutcome::skipped(
            Tft::Tft007,
            if declared == 0 {
                "no edge in this arena declares a nominal rate (EdgeRecord::nominal_rate_mhz is \
                 0 on all of them); declare one with rate_hz in the topology file, or via \
                 EdgeCfg::nominal_rate_hz, and this check has something to compare against"
                    .to_owned()
            } else {
                format!(
                    "{declared} edge(s) declare a nominal rate and none of them was compared: \
                     {withheld} have stopped publishing, which TFT009 reports and this check \
                     will not certify as nominal, and {} have fewer than {RATE_MIN_INTERVALS} \
                     retained intervals to measure an observed rate from; the publishers may \
                     not have started, may have stopped, or the arena was read too soon after \
                     bringup",
                    declared - withheld
                )
            },
        );
    }
    CheckOutcome::ran(Tft::Tft007, out)
}

/// The disclosure that pairs with `tft007`: which edges its result covers.
#[must_use]
pub fn rate_coverage_note(
    snap: &Snapshot,
    obs: &Observations,
    clock: Clock,
    stream: PushStream,
) -> Option<String> {
    let by_edge = obs.by_edge();
    let stopped = stopped_publishers(obs, clock, stream);
    let (mut comparable, mut too_few, mut undeclared, mut withheld) =
        (0usize, 0usize, 0usize, 0usize);
    for e in &snap.edges {
        if e.kind != EdgeKind::Dynamic {
            continue;
        }
        match rate_evidence(e, by_edge.get(&e.id).map(Vec::as_slice)) {
            RateEvidence::NotDeclared => undeclared += 1,
            RateEvidence::TooFewIntervals => too_few += 1,
            RateEvidence::Comparable { .. } if stopped.contains_key(&e.id) => withheld += 1,
            RateEvidence::Comparable { .. } => comparable += 1,
        }
    }
    if comparable == 0 || (undeclared == 0 && too_few == 0 && withheld == 0) {
        return None;
    }
    Some(format!(
        "TFT007 compared {comparable} of {} dynamic edge(s): {undeclared} declare no nominal \
         rate (no rate_hz in the topology), {too_few} have fewer than {RATE_MIN_INTERVALS} \
         retained intervals to measure one from, and {withheld} have stopped publishing, which \
         TFT009 reports and this check will not certify as nominal",
        comparable + too_few + undeclared + withheld
    ))
}

/// `TFT008` — inter-arrival spread.
fn tft008(inp: &Inputs<'_>) -> CheckOutcome {
    let withheld: BTreeSet<u32> = stopped_publishers(inp.obs, inp.clock, inp.stream)
        .into_keys()
        .collect();
    let spread = doctor::check_inconsistent_rates(inp.obs, &withheld);
    if spread.judged == 0 {
        return CheckOutcome::skipped(
            Tft::Tft008,
            if spread.withheld > 0 {
                format!(
                    "every edge with a measurable inter-arrival distribution has stopped \
                     publishing ({} of them): a stopped publisher leaves an evenly spaced ring, \
                     so its coefficient of variation is ~0 and a pass here would certify the \
                     cadence of a stream that is no longer arriving. TFT009 reports the silence",
                    spread.withheld
                )
            } else {
                format!(
                    "no edge in this arena has retained the {} intervals an inter-arrival spread \
                     needs, so there is no distribution to call inconsistent; the publishers may \
                     not have started, or the arena was read too soon after bringup",
                    doctor::SPREAD_MIN_INTERVALS
                )
            },
        );
    }
    CheckOutcome::ran(
        Tft::Tft008,
        spread
            .findings
            .into_iter()
            .map(|f| Finding::about(Tft::Tft008, "edge", f.message))
            .collect(),
    )
}

/// Intervals an edge must retain before `TFT009` has an inter-arrival distribution to
/// measure a gap against.
const GAP_MIN_INTERVALS: usize = 4;

/// The shape of one edge's retained inter-arrival distribution: the median
/// period it publishes at, and the largest gap between two retained stamps.
struct IntervalShape {
    /// The median retained inter-arrival interval. Strictly positive.
    median_ns: i64,
    /// The largest retained inter-arrival interval.
    worst_ns: i64,
}

/// Why one edge's retained stream supports no [`IntervalShape`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShapeGap {
    /// Fewer than [`GAP_MIN_INTERVALS`] intervals. The remedy is to wait for
    /// the ring to fill, or to size it above the floor.
    TooFewIntervals,
    /// Any negative interval, not just a non-positive median.
    NotMonotone,
    /// A non-positive median, which every retained stamp being identical
    /// produces: the ratio would divide by zero and make every non-zero
    /// interval an infinite gap.
    NoPeriod,
}

/// One edge's [`IntervalShape`], or which [`ShapeGap`] its stream fell into.
fn interval_shape(samples: &[&PushSample]) -> Result<IntervalShape, ShapeGap> {
    let mut worst = i64::MIN;
    let mut intervals = 0usize;
    for w in samples.windows(2) {
        let d = w[1].stamp_ns - w[0].stamp_ns;
        if d < 0 {
            return Err(ShapeGap::NotMonotone);
        }
        worst = worst.max(d);
        intervals += 1;
    }
    if intervals < GAP_MIN_INTERVALS {
        return Err(ShapeGap::TooFewIntervals);
    }
    match doctor::median_period(samples) {
        Some(median_ns) => Ok(IntervalShape {
            median_ns,
            worst_ns: worst,
        }),
        None => Err(ShapeGap::NoPeriod),
    }
}

/// Why `TFT009` judged no edge at all, in terms an operator can act on.
fn gap_evidence_skip(too_few: usize, not_monotone: usize, no_period: usize) -> String {
    let mut why: Vec<String> = Vec::new();
    if too_few > 0 {
        why.push(format!(
            "{too_few} retained fewer than the {GAP_MIN_INTERVALS} intervals a median needs, so \
             the publishers may not have started or the ring is sized below that floor"
        ));
    }
    if not_monotone > 0 {
        why.push(format!(
            "{not_monotone} carry a stamp that goes backwards, and a gap measured across an \
             inversion is a dropout that never happened — TFT018 is the id for that fault"
        ));
    }
    if no_period > 0 {
        why.push(format!(
            "{no_period} hold every retained stamp at one instant, so there is no period for a \
             gap to be a multiple of"
        ));
    }
    if why.is_empty() {
        return "nothing has published to any edge of this arena, so there is no inter-arrival \
                distribution to call a gap in and no newest stamp to measure a silence from"
            .to_owned();
    }
    format!(
        "no edge in this arena has a retained inter-arrival distribution, so there is no gap to \
         call a dropout and nothing to measure a trailing silence against: {}",
        why.join("; ")
    )
}

/// How long one edge has been silent, and what its own cadence says about that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Silence {
    /// `now - newest retained stamp`.
    pub silent_ns: i64,
    /// The edge's own median period, which is what makes the number above a
    /// judgement rather than a duration.
    pub median_ns: i64,
}

/// Every edge whose publisher has **stopped** — the gap that has not ended.
#[must_use]
pub fn stopped_publishers(
    obs: &Observations,
    clock: Clock,
    stream: PushStream,
) -> BTreeMap<u32, Silence> {
    let Some(now) = live_wall_now(clock, stream) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (edge, samples) in obs.by_edge() {
        let Ok(shape) = interval_shape(&samples) else {
            continue;
        };
        // Monotone by `interval_shape`'s guard, so the last stamp is the newest.
        let newest = samples.last().map_or(0, |s| s.stamp_ns);
        let silent = now.saturating_sub(newest);
        if silent > shape.median_ns.saturating_mul(GAP_FACTOR) {
            out.insert(
                edge,
                Silence {
                    silent_ns: silent,
                    median_ns: shape.median_ns,
                },
            );
        }
    }
    out
}

/// `TFT009` — an inter-arrival interval far above the edge's own median, and
/// the one that has not ended.
fn tft009(inp: &Inputs<'_>) -> CheckOutcome {
    let stopped = stopped_publishers(inp.obs, inp.clock, inp.stream);
    let mut out = Vec::new();
    let (mut judged, mut too_few, mut not_monotone, mut no_period) = (0usize, 0, 0, 0);
    for (edge, samples) in inp.obs.by_edge() {
        match interval_shape(&samples) {
            Ok(shape) => {
                judged += 1;
                if shape.worst_ns > shape.median_ns.saturating_mul(GAP_FACTOR) {
                    out.push(Finding::on_edge(
                        Tft::Tft009,
                        edge,
                        format!("edge#{edge}"),
                        format!(
                            "largest gap {:.1} ms is {:.1}x the median period {:.1} ms",
                            shape.worst_ns as f64 / 1e6,
                            shape.worst_ns as f64 / shape.median_ns as f64,
                            shape.median_ns as f64 / 1e6
                        ),
                    ));
                }
            }
            Err(ShapeGap::TooFewIntervals) => too_few += 1,
            Err(ShapeGap::NotMonotone) => not_monotone += 1,
            Err(ShapeGap::NoPeriod) => no_period += 1,
        }
        if let Some(s) = stopped.get(&edge) {
            out.push(Finding::on_edge(
                Tft::Tft009,
                edge,
                format!("edge#{edge}"),
                format!(
                    "no sample for {:.1} s, {:.1}x the median period {:.1} ms — its \
                     publisher has stopped, and every retained sample still reads healthy",
                    s.silent_ns as f64 / 1e9,
                    s.silent_ns as f64 / s.median_ns as f64,
                    s.median_ns as f64 / 1e6
                ),
            ));
        }
    }
    if judged == 0 {
        // `stopped_publishers` is a subset of `judged`, so no finding is dropped here.
        debug_assert!(
            out.is_empty(),
            "TFT009 judged no edge and produced {} finding(s): a skip would discard them",
            out.len()
        );
        return CheckOutcome::skipped(
            Tft::Tft009,
            gap_evidence_skip(too_few, not_monotone, no_period),
        );
    }
    CheckOutcome::ran(Tft::Tft009, out)
}

/// The reference instant to measure a *trailing* silence against, or `None`
/// when there is no such thing for this source.
fn live_wall_now(clock: Clock, stream: PushStream) -> Option<i64> {
    match (clock, stream) {
        (Clock::Wall(now), PushStream::RingsUnderWriter) => Some(now),
        _ => None,
    }
}

/// Why `TFT009` could not look for a publisher that **stopped**, or `None` when
/// it could — or when it did not run at all.
#[must_use]
pub fn silence_coverage_note(
    tft009: &CheckOutcome,
    clock: Clock,
    stream: PushStream,
) -> Option<String> {
    debug_assert_eq!(
        tft009.check,
        Tft::Tft009,
        "the outcome read here has to be the one this note is about"
    );
    if matches!(tft009.status, Status::Skipped(_)) {
        return None;
    }
    match (clock, stream) {
        (Clock::Wall(_), PushStream::RingsUnderWriter) => None,
        (_, PushStream::RingsUnderWriter) => Some(
            "TFT009 measured gaps between retained samples but not the gap since the newest one: \
             this arena's stamps do not share an epoch with the system clock, so \"how long since \
             the last sample\" has no answer here. A publisher that stopped leaves a full ring \
             that still reads healthy"
                .into(),
        ),
        _ => Some(
            "TFT009 measured gaps between retained samples but not the gap since the newest one: \
             nothing is writing this source, so that distance is the age of the recording rather \
             than a stopped publisher"
                .into(),
        ),
    }
}

/// The disclosure that pairs with the withholding: which edges `TFT007` and
/// `TFT008` declined to judge because [`stopped_publishers`] named them.
#[must_use]
pub fn stopped_publisher_note(
    obs: &Observations,
    clock: Clock,
    stream: PushStream,
) -> Option<String> {
    let stopped = stopped_publishers(obs, clock, stream);
    if stopped.is_empty() {
        return None;
    }
    Some(format!(
        "TFT007 and TFT008 withheld judgement on {} edge(s) whose publisher has stopped: a \
         dead publisher leaves an evenly spaced ring, so a declared-rate comparison and an \
         inter-arrival spread over it both read healthy. TFT009 reports the silence",
        stopped.len()
    ))
}

/// `TFT010` — an edge whose consumers keep asking outside its window.
fn tft010(inp: &Inputs<'_>) -> CheckOutcome {
    if let Some(why) = no_counter_evidence(inp.counters, inp.stats) {
        return CheckOutcome::skipped(Tft::Tft010, why);
    }
    let index = inp.snap.edge_index();
    let mut out = Vec::new();
    for st in inp.stats {
        let errs = st.extrap_before + st.extrap_after;
        if errs == 0 {
            continue;
        }
        let total = errs + st.lookups_ok;
        let rate = errs as f64 / total as f64;
        if rate <= EXTRAP_HOTSPOT_RATE {
            continue;
        }
        let who = if st.blamed.is_empty() {
            // Not a contradiction: a participant's counter names only its *most
            // recent* failing edge, so a consumer that has since failed
            // elsewhere leaves this edge's errors unattributed.
            "no participant currently names this edge as its last failure".to_owned()
        } else {
            let list: Vec<String> = st
                .blamed
                .iter()
                .map(|(slot, pid)| format!("slot {slot} (pid {pid})"))
                .collect();
            format!("last failed by {}", list.join(", "))
        };
        let subject = index
            .get(&st.edge)
            .map_or_else(|| format!("edge#{}", st.edge), |e| inp.snap.edge_label(e));
        out.push(Finding::on_edge(
            Tft::Tft010,
            st.edge,
            subject,
            format!(
                "{errs} extrapolation error(s) against {} ok ({:.1}% of lookups); \
                 {} before / {} after the window; {who}",
                st.lookups_ok,
                rate * 100.0,
                st.extrap_before,
                st.extrap_after
            ),
        ));
    }
    CheckOutcome::ran(Tft::Tft010, out)
}

/// `TFT011` — the ring is shorter than the lag its consumers actually showed.
fn tft011(inp: &Inputs<'_>) -> CheckOutcome {
    let counters = no_counter_evidence(inp.counters, inp.stats);
    let delays = inp.stream.no_arrival_delays();
    if let (Some(a), Some(b)) = (counters, delays) {
        return CheckOutcome::skipped(
            Tft::Tft011,
            format!(
                "neither half of this check has evidence here. Its counter half: {a}. Its \
                     capacity-vs-latency half: {b}"
            ),
        );
    }
    let mut out = Vec::new();
    if counters.is_none() {
        let index = inp.snap.edge_index();
        for st in inp.stats {
            if st.extrap_before == 0 || st.worst_extrap_gap_ns <= 0 {
                continue;
            }
            let Some(span) = st.retained_span_ns() else {
                continue;
            };
            if st.worst_extrap_gap_ns <= span {
                continue;
            }
            let subject = index
                .get(&st.edge)
                .map_or_else(|| format!("edge#{}", st.edge), |e| inp.snap.edge_label(e));
            out.push(Finding::on_edge(
                Tft::Tft011,
                st.edge,
                subject,
                format!(
                    "ring retains {:.1} ms but a consumer asked {:.1} ms outside it \
                     ({} request(s) fell off the back); enlarge the ring by at least {:.1}x",
                    span as f64 / 1e6,
                    st.worst_extrap_gap_ns as f64 / 1e6,
                    st.extrap_before,
                    st.worst_extrap_gap_ns as f64 / span as f64
                ),
            ));
        }
    }
    for f in doctor::check_short_buffers(inp.snap, inp.obs) {
        out.push(Finding::about(Tft::Tft011, "edge", f.message));
    }
    CheckOutcome::ran(Tft::Tft011, out)
}

/// `TFT012` — the topology walk does not reach everything.
fn tft012(inp: &Inputs<'_>) -> CheckOutcome {
    let mut out = Vec::new();
    for f in doctor::check_cycles(inp.snap) {
        out.push(Finding::about(Tft::Tft012, "topology", f.message));
    }
    for f in doctor::check_unreachable(inp.snap) {
        out.push(Finding::about(Tft::Tft012, "topology", f.message));
    }
    CheckOutcome::ran(Tft::Tft012, out)
}

/// How long *something* in an arena must have been publishing before `TFT013`
/// will call an edge that has not published a fault — §6's *"after a grace
/// period"*.
const DECLARATION_GRACE_NS: i64 = 5_000_000_000;

/// How long **anything** in this arena has been publishing, or which of the two
/// absences stands in the way of an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishActivity {
    /// No dynamic edge has ever accepted a push.
    NoPublisher,
    /// Something **has** published — `largest_head` pushes on the busiest
    /// dynamic edge — and no dynamic edge yielded the two samples
    /// [`doctor::median_period`] needs, so the span below cannot be computed
    /// from any of them.
    Unmeasurable {
        /// Accepted pushes on the busiest dynamic edge. The evidence that this
        /// is not the arena above.
        largest_head: u64,
        /// The most samples any published dynamic edge can retain (`capacity - 1`).
        retained_capacity: u32,
        /// The most samples actually recovered from any of those edges.
        observed: usize,
    },
    /// A lower bound on how long the longest-running dynamic publisher has been
    /// going, in nanoseconds.
    Running(i64),
}

/// [`PublishActivity`] for this arena.
fn publish_activity(inp: &Inputs<'_>) -> PublishActivity {
    let by_edge = inp.obs.by_edge();
    let mut best: Option<i64> = None;
    let mut largest_head = 0u64;
    let mut retained_capacity = 0u32;
    let mut observed = 0usize;
    for e in &inp.snap.edges {
        if e.kind != EdgeKind::Dynamic || e.head == 0 {
            continue;
        }
        largest_head = largest_head.max(e.head);
        // `SampleRing::retained` is `capacity - 1`.
        retained_capacity = retained_capacity.max(e.capacity.saturating_sub(1));
        let Some(samples) = by_edge.get(&e.id) else {
            continue;
        };
        observed = observed.max(samples.len());
        let Some(period) = doctor::median_period(samples) else {
            continue;
        };
        let span = i128::from(e.head - 1) * i128::from(period);
        let span = i64::try_from(span).unwrap_or(i64::MAX);
        best = Some(best.map_or(span, |b: i64| b.max(span)));
    }
    match (best, largest_head) {
        (Some(span), _) => PublishActivity::Running(span),
        (None, 0) => PublishActivity::NoPublisher,
        (None, largest_head) => PublishActivity::Unmeasurable {
            largest_head,
            retained_capacity,
            observed,
        },
    }
}

/// `TFT013` — an edge declared and never published to, **after a grace period**.
fn tft013(inp: &Inputs<'_>) -> CheckOutcome {
    let activity = match publish_activity(inp) {
        PublishActivity::Running(ns) => ns,
        PublishActivity::NoPublisher => {
            return CheckOutcome::skipped(
                Tft::Tft013,
                "nothing in this arena has published a measurable stream, so there is no \
                 evidence that any publisher has had time to start: an edge with head == 0 is \
                 what every dynamic edge of a correct arena reads as at bringup. TFT017 is the \
                 id for an edge whose writer is gone",
            );
        }
        PublishActivity::Unmeasurable {
            largest_head,
            retained_capacity,
            observed,
        } => {
            // Which of the three arenas this is, said from the numbers rather
            // than assumed.
            let cause = if retained_capacity < 2 {
                format!(
                    "no ring in it can hold two — the largest retains {retained_capacity}. A \
                     ring of four slots retains three; RingSize::History rounds rate_hz x secs \
                     up to a power of two, so anything at or below two rounds to a ring that \
                     retains one sample for the life of the arena"
                )
            } else if observed < 2 {
                format!(
                    "the rings are large enough — the largest retains {retained_capacity} — \
                     and {observed} sample(s) came back from the best-supplied edge. A \
                     publisher that has pushed once reads like this at bringup, and so does a \
                     recording that carries one dated record for an edge; a record with no \
                     stamp is not an observation"
                )
            } else {
                format!(
                    "the samples are there — the largest ring retains {retained_capacity} and \
                     {observed} came back from the best-supplied edge — and no edge has a \
                     positive median period, so there is no cadence to multiply. TFT009 names \
                     that gap on the edge it belongs to, and TFT018 names stamps that go \
                     backwards"
                )
            };
            return CheckOutcome::skipped(
                Tft::Tft013,
                format!(
                    "this arena has publishers — the busiest dynamic edge has accepted \
                     {largest_head} push(es) — but no dynamic edge yielded the two samples a \
                     median period needs, so how long they have been running cannot be measured \
                     from it and the grace period this check owes them has no clock: {cause}"
                ),
            );
        }
    };
    if activity < DECLARATION_GRACE_NS {
        return CheckOutcome::skipped(
            Tft::Tft013,
            format!(
                "the longest-running publisher in this arena has produced {:.1} s of samples, \
                 inside the {:.0} s grace period this check allows for publishers to start \
                 (PHASE5 §6's \"after a grace period\"); an edge with head == 0 here is not yet \
                 evidence of anything",
                activity as f64 / 1e9,
                DECLARATION_GRACE_NS as f64 / 1e9
            ),
        );
    }
    let mut out = Vec::new();
    for e in &inp.snap.edges {
        if e.kind == EdgeKind::Dynamic && e.head == 0 {
            out.push(Finding::on_edge(
                Tft::Tft013,
                e.id,
                inp.snap.edge_label(e),
                format!(
                    "declared as dynamic but head is 0 — nothing has ever been published to it, \
                     and another publisher in this arena has been running for {:.1} s",
                    activity as f64 / 1e9
                ),
            ));
        }
    }
    CheckOutcome::ran(Tft::Tft013, out)
}

/// Which of `TFT014`'s two participant leaks a slot is, if either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotLeak {
    /// **(a)** A record that is not `FREE` over a lock byte nobody holds.
    Abandoned,
    /// **(b)** The byte *is* held, and `/proc` says the process the identity
    /// record names is gone.
    ForkInheritor,
}

/// Whether participant slot `p` is one of the two leaks above.
#[must_use]
pub fn slot_leak(p: &ParticipantInfo) -> Option<SlotLeak> {
    // The word first, and it answers exactly one question: is there an arena record
    // here?
    if p.state == SlotState::Free {
        return (p.byte == LockByte::Held && p.recorded == RecordedProcess::Gone)
            .then_some(SlotLeak::ForkInheritor);
    }
    match (p.byte, p.recorded) {
        (LockByte::Free, RecordedProcess::Running) => None,
        (LockByte::Free, RecordedProcess::Gone | RecordedProcess::Unknown) => {
            Some(SlotLeak::Abandoned)
        }
        (LockByte::Held, RecordedProcess::Gone) => Some(SlotLeak::ForkInheritor),
        (LockByte::Held, RecordedProcess::Running | RecordedProcess::Unknown) => None,
        // No byte was asked for.
        (LockByte::Unknown, _) => {
            (p.state == SlotState::Live && !p.alive).then_some(SlotLeak::Abandoned)
        }
    }
}

/// The evidence clause of an [`SlotLeak::Abandoned`] finding.
fn abandoned_evidence(p: &ParticipantInfo) -> &'static str {
    match (p.byte, p.recorded) {
        (LockByte::Free, RecordedProcess::Gone) => {
            "the lock byte is free, and /proc has no running process for it"
        }
        (LockByte::Free, RecordedProcess::Unknown) if named_pid(p).is_none() => {
            "the lock byte is free, this run got no identity record out of the lock file for \
             this slot — none written, or none readable — and the arena record carries no pid \
             either, so there is no process here to ask /proc about and none to check"
        }
        (LockByte::Free, RecordedProcess::Unknown) if p.recorded_pid.is_none() => {
            "the lock byte is free, and this run got no identity record out of the lock file \
             for this slot — none written, or none readable — so /proc was asked about \
             nobody and the pid this finding names is the arena record's own"
        }
        (LockByte::Free, _) => {
            "the lock byte is free, and /proc could not say what became of the process — so \
             the kernel's answer is the whole of the evidence"
        }
        _ => {
            "this run has no kernel answer about the byte — it read no lock file \
             (`--from-bag`, the fixture), or the probe itself failed — so the verdict rests \
             on the record being LIVE over a process this run could not confirm is running"
        }
    }
}

/// The pid a slot finding's evidence is about, and the arena record's, in one
/// subject line.
fn slot_subject(p: &ParticipantInfo) -> String {
    let byte = match p.byte {
        LockByte::Held => "byte still HELD",
        LockByte::Free => "byte free",
        LockByte::Unknown => "byte not probed",
    };
    match (p.recorded_pid, named_pid(p)) {
        (_, None) => format!("slot {}, no pid recorded, {byte}", p.slot),
        (Some(rp), Some(_)) if rp != p.pid && p.pid != 0 => {
            format!(
                "slot {} pid {rp} (arena record names pid {}), {byte}",
                p.slot, p.pid
            )
        }
        (_, Some(pid)) => format!("slot {} pid {pid}, {byte}", p.slot),
    }
}

/// The pid this finding is about, or `None` when nothing named one.
fn named_pid(p: &ParticipantInfo) -> Option<u32> {
    p.recorded_pid.or(Some(p.pid).filter(|&n| n != 0))
}

/// `TFT014` — a participant slot, or a claim, that outlived its owner.
fn tft014(inp: &Inputs<'_>) -> CheckOutcome {
    if let Some(why) = inp.slots.no_liveness() {
        return CheckOutcome::skipped(Tft::Tft014, why);
    }
    let mut out = Vec::new();
    let slots = inp.snap.participants.len();
    // The budget figure is about slots nothing will reassign, so it counts (a)
    // only: a fork inheritor's byte comes back when the last inheritor exits,
    // and folding it in would report a recoverable slot as permanently spent.
    let leaked = inp
        .snap
        .participants
        .iter()
        .filter(|p| slot_leak(p) == Some(SlotLeak::Abandoned))
        .count();
    for p in &inp.snap.participants {
        // The word this row actually carries.
        let state = match p.state {
            SlotState::Reserved => "RESERVED",
            SlotState::Live => "LIVE",
            SlotState::Free => "FREE (no arena record: a read-only participant, D18)",
        };
        // One pid answer for this whole function, through `named_pid`, so a change to
        // that predicate reaches every rendering.
        let pid = named_pid(p).unwrap_or(0);
        match slot_leak(p) {
            None => {}
            Some(SlotLeak::Abandoned) => out.push(Finding::about(
                Tft::Tft014,
                slot_subject(p),
                {
                    // **The pid appears three times in this finding and nothing
                    // may print a zero.** `named_pid` is the one answer; where
                    // it is `None` the clause naming a process goes, and so
                    // does the instruction to check it, because there is
                    // nothing to check and telling an operator otherwise is
                    // the defect `slot_subject`'s doc is about.
                    let (left, check) = match named_pid(p) {
                        Some(n) => (
                            format!(
                                "pid {n} left slot {} registered and no longer holds it",
                                p.slot
                            ),
                            " CHECK THE PID IS GONE before you reap: on that last one it \
                             is not.",
                        ),
                        None => (
                            format!(
                                "slot {} was left registered by a process this run cannot \
                                 name — which rules out the last cause below, since a \
                                 build_shared creator's record always carries its pid",
                                p.slot
                            ),
                            "",
                        ),
                    };
                    format!(
                        "a record left behind — the record is {state}, {} — {left}, \
                         and the owner's \
                         socket-hangup reap did not clear it. That reap collects a rendezvous \
                         peer, so this is a slot it cannot reach: the owner's own, one its epoll \
                         never watched, a takeover heir's inherited peer, an owner that died \
                         inside the callback (docs/decisions/0028) — or a live publisher that \
                         never took a byte, which is a TreeBuilder::build_shared arena served \
                         by hand and out of contract (docs/decisions/0031).{check} Nothing reclaims \
                         it — {leaked} of {slots} slots are spent for the life of the segment, \
                         and at {slots} every further attach fails NoParticipantSlots. Only \
                         stopping every participant, which frees the segment, frees a slot",
                        abandoned_evidence(p),
                    )
                },
            )),
            Some(SlotLeak::ForkInheritor) => out.push(Finding::about(
                Tft::Tft014,
                slot_subject(p),
                format!(
                    "a fork inheritor — byte still HELD, recorded pid gone. The record is \
                     {state} and its lock byte is still HELD, but /proc says the pid the \
                     lock file records for it, {pid}, is gone — so slot {} is held by an open \
                     file description that outlived its process, which on Linux means a \
                     forked child inherited it (PHASE2 §6.2: an OFD lock belongs to the \
                     description, and fork shares it). This is NOT the same fault as a slot \
                     whose byte is free and NOT one to go looking for a reaper for: nothing \
                     may reclaim this slot, because the kernel's own answer is 'held' and \
                     overruling that with a /proc guess is what would evict a running \
                     participant. The child cannot use the slot either — a shared arena is \
                     mapped MADV_DONTFORK, so the handle it inherited is poisoned — and the \
                     byte comes back on its own when the last inheritor exits. Stop the \
                     child, or start workers with a start method that inherits no \
                     descriptors: multiprocessing's `spawn` (Python's default on Linux is \
                     `fork`), or fork+exec (docs/decisions/0030)",
                    p.slot
                ),
            )),
        }
    }
    for e in &inp.snap.edges {
        if !e.claimed || e.claiming {
            continue;
        }
        // No slot at all (a word that resolves outside this table) counts as
        // dead: there is no participant it could be asking about, and the edge
        // is held by nobody either way.
        let owner_alive = e
            .owner_slot
            .and_then(|slot| inp.snap.participant(slot))
            .is_some_and(|p| p.alive);
        if !owner_alive {
            out.push(Finding::on_edge(
                Tft::Tft014,
                e.id,
                inp.snap.edge_label(e),
                "claim is held by a participant slot whose owner is not running — the writer \
                 is gone and nothing released the edge, so no other process can take it",
            ));
        }
    }
    CheckOutcome::ran(Tft::Tft014, out)
}

/// `TFT015` — a fixed-capacity table nearly full.
fn tft015(inp: &Inputs<'_>) -> CheckOutcome {
    let mut out = Vec::new();
    for &(what, used, cap) in &inp.occupancy {
        if cap == 0 {
            continue;
        }
        let frac = f64::from(used) / f64::from(cap);
        if frac > OCCUPANCY_LIMIT {
            out.push(Finding::about(
                Tft::Tft015,
                what,
                format!(
                    "{used} of {cap} used ({:.0}%), above the {:.0}% mark; the arena has fixed \
                     capacity and cannot grow",
                    frac * 100.0,
                    OCCUPANCY_LIMIT * 100.0
                ),
            ));
        }
    }
    CheckOutcome::ran(Tft::Tft015, out)
}

/// `TFT016` — host settings that silently change how the arena performs.
fn tft016(inp: &Inputs<'_>) -> CheckOutcome {
    let Some(host) = inp.host else {
        return CheckOutcome::skipped(
            Tft::Tft016,
            "transparent huge pages and RLIMIT_MEMLOCK are read from /sys and /proc, \
             which exist only on Linux",
        );
    };
    let mut out = Vec::new();
    match host.thp {
        Thp::Never => out.push(Finding::about(
            Tft::Tft016,
            "host",
            "transparent huge pages are 'never'; the arena's 2 MiB alignment (PHASE5 §2.3) \
             buys no TLB reach on this host",
        )),
        Thp::Unknown => out.push(Finding::about(
            Tft::Tft016,
            "host",
            "/sys/kernel/mm/transparent_hugepage/enabled was absent or unrecognised, \
             so the huge-page policy is unknown",
        )),
        Thp::Always | Thp::Madvise => {}
    }
    // The knob that governs the live arena, which is a sealed `memfd` mapped
    // `MAP_SHARED` — shmem, not anonymous memory.
    if !host.shmem_thp.honours_madvise() {
        out.push(Finding::about(
            Tft::Tft016,
            "host",
            if host.shmem_thp == ShmemThp::Unknown {
                "/sys/kernel/mm/transparent_hugepage/shmem_enabled was absent or \
                 unrecognised, so the huge-page policy for the live arena's memfd \
                 mapping is unknown"
                    .to_string()
            } else {
                format!(
                    "shmem transparent huge pages are '{}', so MADV_HUGEPAGE on the arena's \
                     MAP_SHARED memfd does nothing and the live arena takes 4 KiB pages \
                     regardless of what transparent_hugepage/enabled says; set \
                     shmem_enabled to 'advise' to make PHASE5 §2.3's alignment count",
                    host.shmem_thp.name()
                )
            },
        ));
    }
    match host.memlock {
        MemLock::Bytes(limit) if limit < inp.arena_bytes => out.push(Finding::about(
            Tft::Tft016,
            "host",
            format!(
                "RLIMIT_MEMLOCK is {limit} bytes, below the {} byte arena; the arena \
                 alone cannot be pinned, so a consumer cannot keep page faults out of its \
                 control loop. tf_tree itself never calls mlock: pinning is the embedding \
                 application's to do (mlockall(MCL_CURRENT|MCL_FUTURE|MCL_ONFAULT) — the \
                 ONFAULT is what stops it prefaulting the whole over-provisioned arena), \
                 and this is one term of the limit it needs. The other term is not \
                 visible from here: mlockall charges the process's whole address space, \
                 not the arena, so a limit above the arena size is not sufficient and this \
                 check staying quiet is not a clearance (docs/API.md §8.3, \
                 docs/decisions/0049)",
                inp.arena_bytes
            ),
        )),
        MemLock::Unknown => out.push(Finding::about(
            Tft::Tft016,
            "host",
            "/proc/self/limits gave no 'Max locked memory' row, so the mlock limit is unknown",
        )),
        MemLock::Bytes(_) | MemLock::Unlimited => {}
    }
    CheckOutcome::ran(Tft::Tft016, out)
}

/// `TFT017` — a dynamic edge nobody is writing to.
fn tft017(inp: &Inputs<'_>) -> CheckOutcome {
    CheckOutcome::ran(
        Tft::Tft017,
        doctor::check_unclaimed_dynamic(inp.snap)
            .into_iter()
            .map(|f| Finding::about(Tft::Tft017, "tree", f.message))
            .collect(),
    )
}

/// `TFT018` — a later arrival carried an older stamp than an earlier one.
fn tft018(inp: &Inputs<'_>) -> CheckOutcome {
    if let Some(why) = inp.stream.no_rejected_arrivals() {
        return CheckOutcome::skipped(Tft::Tft018, why);
    }
    CheckOutcome::ran(
        Tft::Tft018,
        doctor::check_out_of_order(inp.obs)
            .into_iter()
            .map(|f| Finding::about(Tft::Tft018, "tree", f.message))
            .collect(),
    )
}

/// The one domain tag `TFT019` will attribute a clock step to.
const WALL_CLOCK_TAG: u8 = <SystemDomain as Domain>::TAG;

/// How long a burst of rejected pushes has to be before `TFT019` will call it a
/// clock step, in **arrivals on that edge**.
const CLOCK_STEP_MIN_REJECTED_RUN: usize = 8;

/// How `TFT019` split `TFT018`'s per-edge evidence: what it attributed to a
/// wall-clock step, what was too diffuse to be one, and what it would not judge.
pub struct ClockStepEvidence {
    /// Runs on a `SystemDomain` edge whose rejections are concentrated enough to
    /// be a step, each with the label to report it under.
    attributed: Vec<(doctor::OutOfOrderRun, String)>,
    /// Runs on a `SystemDomain` edge that are not concentrated: fewer than
    /// [`CLOCK_STEP_MIN_REJECTED_RUN`] consecutive rejected arrivals, as `(edge,
    /// longest run)`.
    diffuse: Vec<(u32, usize)>,
    /// Runs on any other tag, as `(edge, tag)`, with `None` for an edge whose tag could
    /// not be read at all.
    refused: Vec<(u32, Option<u8>)>,
}

impl ClockStepEvidence {
    /// Split `TFT018`'s per-edge evidence by the edge's declared domain tag and
    /// by whether its rejections are concentrated.
    #[must_use]
    pub fn capture(snap: &Snapshot, obs: &Observations) -> ClockStepEvidence {
        let index = snap.edge_index();
        let mut ev = ClockStepEvidence {
            attributed: Vec::new(),
            diffuse: Vec::new(),
            refused: Vec::new(),
        };
        for run in doctor::out_of_order_runs(obs) {
            match index.get(&run.edge).map(|e| (e.domain, *e)) {
                // A `const` in a pattern, so this is an equality test against tag 0
                // and not a binding.
                Some((WALL_CLOCK_TAG, e)) => {
                    if run.longest_rejected_run >= CLOCK_STEP_MIN_REJECTED_RUN {
                        ev.attributed.push((run, snap.edge_label(e)));
                    } else {
                        ev.diffuse.push((run.edge, run.longest_rejected_run));
                    }
                }
                Some((tag, _)) => ev.refused.push((run.edge, Some(tag))),
                // Reachable from a hand-assembled `Inputs` and from a recorded stream
                // whose edge is absent from the snapshot.
                None => ev.refused.push((run.edge, None)),
            }
        }
        ev
    }

    /// The disclosure that pairs with `tft019`'s outcome: which edges its
    /// result does *not* cover.
    #[must_use]
    pub fn coverage_note(&self, stream: PushStream) -> Option<String> {
        if stream.no_rejected_arrivals().is_some() {
            return None;
        }
        // Nothing attributed and nothing diffuse: either `TFT018` found nothing
        // at all, or the skip reason names every refused tag itself.
        if self.attributed.is_empty() && self.diffuse.is_empty() {
            return None;
        }
        // Everything found was attributed.
        if self.diffuse.is_empty() && self.refused.is_empty() {
            return None;
        }
        let total = self.attributed.len() + self.diffuse.len() + self.refused.len();
        let mut parts = Vec::new();
        if !self.diffuse.is_empty() {
            parts.push(format!(
                "{} in the wall-clock domain but with no run of at least \
                 {CLOCK_STEP_MIN_REJECTED_RUN} consecutive rejected arrivals, so a stray inversion \
                 rather than a step ({})",
                self.diffuse.len(),
                self.diffuse
                    .iter()
                    .map(|&(edge, run)| format!("edge#{edge} longest run {run}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.refused.is_empty() {
            parts.push(format!(
                "{} in another time domain ({})",
                self.refused.len(),
                tag_list(&self.refused)
            ));
        }
        Some(format!(
            "TFT019 attributed {} of {total} edge(s) with out-of-order arrivals to a wall-clock \
             step; the rest are not attributed: {}",
            self.attributed.len(),
            parts.join("; ")
        ))
    }
}

/// Render `(edge, tag)` pairs for a skip reason or a note, each with the reason
/// that tag specifically is not a clock this check will blame.
fn tag_list(refused: &[(u32, Option<u8>)]) -> String {
    refused
        .iter()
        .map(|&(edge, tag)| match tag {
            Some(tag) => format!("edge#{edge} tag {tag} ({})", tag_refusal(tag)),
            None => format!("edge#{edge} (not in the snapshot, tag unreadable)"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why a run on `tag` is not attributed to a wall clock stepping.
fn tag_refusal(tag: u8) -> &'static str {
    match tag {
        // Unreachable: a tag-0 run is either attributed or diffuse, never refused.
        WALL_CLOCK_TAG => "the system wall clock",
        <SensorDomain as Domain>::TAG => {
            "a sensor's own clock, which this build has no way to call steppable or steady"
        }
        <SimDomain as Domain>::TAG => {
            "simulated time, which does step backwards on a /clock reset — telling that apart from \
             a publisher's transform_tolerance needs the authoritative rcl signal decision 0012 \
             specifies, and doctor has none offline"
        }
        <SteadyDomain as Domain>::TAG => {
            "a steady clock, which cannot have stepped, so this is a real publisher fault"
        }
        _ => {
            "a user-declared domain; Domain is an open trait, so the tag carries no statement \
              that its clock can step"
        }
    }
}

/// `TFT019` — a wall clock stepped backwards, which is `TFT018`'s cause.
fn tft019(inp: &Inputs<'_>) -> CheckOutcome {
    if let Some(why) = inp.stream.no_rejected_arrivals() {
        return CheckOutcome::skipped(
            Tft::Tft019,
            format!(
                "inherited from TFT018, whose evidence this is — {why}. Attributing an \
                 out-of-order arrival to a clock step needs one to exist in the stream first"
            ),
        );
    }
    let ev = inp.clock_step;
    if ev.attributed.is_empty() {
        // A `Skipped` only when *nothing* was judged.
        if !ev.refused.is_empty() && ev.diffuse.is_empty() {
            return CheckOutcome::skipped(
                Tft::Tft019,
                format!(
                    "the edge(s) with out-of-order arrivals are not in the system wall-clock \
                     domain (tag {WALL_CLOCK_TAG}): {}. Reporting a clock step here would \
                     fabricate an all-clear on what TFT018 reports as a publisher fault",
                    tag_list(&ev.refused)
                ),
            );
        }
        // Either TFT018 found nothing, or what it found is not step-shaped.
        return CheckOutcome::ran(Tft::Tft019, Vec::new());
    }
    let findings = ev
        .attributed
        .iter()
        .map(|(run, label)| {
            Finding::on_edge(
                Tft::Tft019,
                run.edge,
                label.clone(),
                format!(
                    "{} out-of-order arrival(s) including a run of {} consecutive rejected \
                     pushes, worst {:.3} ms backwards, on an edge declared in the system wall \
                     clock domain (tag {WALL_CLOCK_TAG}): CLOCK_REALTIME is not monotone, so an \
                     NTP step or a leap second is the likely cause and restarting the publisher \
                     will not help. Declare anything published at rate with SteadyDomain (tag 3), \
                     or your own tag for a PTP-disciplined clock. TFT018 still reports the \
                     rejected pushes — the data lost during the step is gone either way",
                    run.regressions,
                    run.longest_rejected_run,
                    run.worst_backstep_ns as f64 / 1e6,
                ),
            )
        })
        .collect();
    CheckOutcome::ran(Tft::Tft019, findings)
}

/// The occupancy triples for [`Inputs::occupancy`], read from the header.
#[must_use]
pub fn occupancy_of(tree: &Tree) -> Vec<(&'static str, u32, u32)> {
    let view = tree.arena_view();
    let h = view.header();
    vec![
        (
            "frames",
            h.frame_count.load(Ordering::Relaxed),
            h.max_frames,
        ),
        // Both sides include the sentinel at index 0.
        ("edges", h.edge_count.load(Ordering::Relaxed), h.max_edges),
    ]
}

/// The disclosure that pairs with [`occupancy_of`]'s missing `participants` row.
pub const PARTICIPANT_OCCUPANCY_NOTE: &str =
    "TFT015 covers frames and edges only: participant occupancy lives in the lock \
     file, not the arena — ArenaHeader::participant_count is never incremented, and \
     the arena participant table misses every read-only attachment (D18's default \
     writes a lock byte and no record) — so either arena-side row would read far \
     under the truth and pass with the slot table full. See docs/decisions/0056";

/// Every edge's newest stamp — the sample [`Clock::decide`] votes over.
#[must_use]
pub fn newest_stamps(snap: &Snapshot) -> Vec<i64> {
    snap.edges.iter().filter_map(|e| e.newest_stamp).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::doctor::{EdgeInfo, FrameInfo};
    use tf_tree::InterpPolicy;

    fn frame(id: u32, name: &str, parent: u32, depth: u16) -> FrameInfo {
        FrameInfo {
            id,
            name: name.to_owned(),
            parent,
            depth,
            edge_of_child: 0,
        }
    }

    fn edge(id: u32, parent: u32, child: u32, head: u64) -> EdgeInfo {
        EdgeInfo {
            id,
            parent,
            child,
            kind: EdgeKind::Dynamic,
            capacity: 512,
            interp: InterpPolicy::ScLerp,
            domain: 0,
            head,
            claimed: true,
            claiming: false,
            owner_slot: Some(0),
            owner_pid: 4711,
            newest_stamp: Some(1_000_000_000),
            clock_offset_nanos: None,
            nominal_rate_mhz: None,
        }
    }

    /// The participant table that goes with [`edge`]: one running writer in
    /// slot 0, which is the owner every edge this helper builds names.
    fn live_writer() -> Vec<ParticipantInfo> {
        vec![ParticipantInfo {
            slot: 0,
            state: SlotState::Live,
            pid: 4711,
            alive: true,
            byte: LockByte::Held,
            recorded: RecordedProcess::Running,
            recorded_pid: Some(4711),
        }]
    }

    /// `n` samples on `edge`, one every `period_ns`, from a single writer.
    fn steady(edge: u32, n: usize, period_ns: i64) -> Vec<PushSample> {
        (0..n as i64)
            .map(|k| PushSample {
                edge,
                writer_pid: 4711,
                stamp_ns: k * period_ns,
                arrival_delay_ns: 0,
            })
            .collect()
    }

    fn two_frame_snapshot(e: EdgeInfo) -> Snapshot {
        Snapshot {
            frames: vec![frame(1, "map", 0, 0), frame(2, "odom", 1, 1)],
            edges: vec![e],
            participants: live_writer(),
        }
    }

    fn inputs<'a>(
        snap: &'a Snapshot,
        obs: &'a Observations,
        stats: &'a [EdgeStats],
        clock: Clock,
    ) -> Inputs<'a> {
        Inputs {
            snap,
            obs,
            stats,
            host: None,
            clock,
            arena_bytes: 1 << 20,
            occupancy: Vec::new(),
            clock_step: Box::leak(Box::new(ClockStepEvidence::capture(snap, obs))),
            stream: PushStream::Observed,
            slots: SlotTable::Current,
            counters: true,
        }
    }

    /// [`inputs`] as `doctor --attach` sees it: an arena somebody is writing.
    fn live_inputs<'a>(
        snap: &'a Snapshot,
        obs: &'a Observations,
        stats: &'a [EdgeStats],
        clock: Clock,
    ) -> Inputs<'a> {
        Inputs {
            stream: PushStream::RingsUnderWriter,
            ..inputs(snap, obs, stats, clock)
        }
    }

    /// **A publisher that stopped is the most common fault in the field, and
    /// every rule in `TFT009` was blind to it.**
    #[test]
    fn a_publisher_that_stopped_is_reported_where_every_retained_stamp_reads_healthy() {
        const MS: i64 = 1_000_000;
        let stamps: Vec<i64> = (0..40).map(|k| k * 10 * MS).collect();
        let newest = *stamps.last().unwrap();
        let obs = Observations::from_samples(
            stamps
                .iter()
                .map(|&ns| tf_tree_bench::fixture::PushSample {
                    edge: 1,
                    writer_pid: 4711,
                    stamp_ns: ns,
                    arrival_delay_ns: 0,
                })
                .collect(),
        );
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let now = Clock::Wall(newest + 8_000 * MS);

        let o = tft009(&inputs(&snap, &obs, &[], now));
        assert_eq!(
            o.status,
            Status::Pass,
            "the retained samples are flawless, so the between-stamps rules must \
             stay quiet: {o:?}"
        );
        assert!(
            silence_coverage_note(&o, now, PushStream::Observed).is_some(),
            "a source this half cannot run on must say so"
        );

        let o = tft009(&live_inputs(&snap, &obs, &[], now));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(o.findings.len(), 1, "{o:?}");
        let msg = &o.findings[0].message;
        assert!(
            msg.contains("no sample for 8.0 s") && msg.contains("publisher has stopped"),
            "the finding must name the silence and what it means: {msg}"
        );
        assert!(
            silence_coverage_note(&o, now, PushStream::RingsUnderWriter).is_none(),
            "the half ran, so there is nothing to disclose"
        );

        let o = tft009(&live_inputs(&snap, &obs, &[], Clock::NewestStamp(newest)));
        assert_eq!(
            o.status,
            Status::Pass,
            "a non-wall clock has no answer for \"how long since the last sample\": {o:?}"
        );
        assert!(silence_coverage_note(
            &o,
            Clock::NewestStamp(newest),
            PushStream::RingsUnderWriter
        )
        .is_some());
    }

    /// **A publisher that stopped is not certified healthy by `TFT007` and
    /// `TFT008` while `TFT009` is calling it dead.**
    #[test]
    fn a_stopped_publisher_is_not_certified_healthy_by_tft007_and_tft008() {
        const MS: i64 = 1_000_000;
        let obs = Observations::from_samples(steady(1, 40, 10 * MS));
        let newest = obs.by_edge()[&1]
            .last()
            .expect("the fixture has samples on edge 1")
            .stamp_ns;
        let mut e = edge(1, 1, 2, 40);
        e.nominal_rate_mhz = Some(100_000); // 100 Hz, in milli-hertz.
        let snap = two_frame_snapshot(e);
        let now = Clock::Wall(newest + 8_000 * MS);

        let at_rest = inputs(&snap, &obs, &[], now);
        assert_eq!(
            tft007(&at_rest).status,
            Status::Pass,
            "{:?}",
            tft007(&at_rest)
        );
        assert_eq!(
            tft008(&at_rest).status,
            Status::Pass,
            "{:?}",
            tft008(&at_rest)
        );
        assert_eq!(
            stopped_publisher_note(&obs, now, PushStream::Observed),
            None,
            "nothing was withheld here, so there is nothing to disclose"
        );

        let live = live_inputs(&snap, &obs, &[], now);
        assert_eq!(
            tft009(&live).status,
            Status::Fired,
            "the fixture must be one TFT009 reports, or this test asserts nothing"
        );
        match &tft007(&live).status {
            Status::Skipped(why) => assert!(
                why.contains("stopped publishing"),
                "TFT007 must name the stopped publisher as what it declined to \
                 compare: {why}"
            ),
            other => panic!(
                "TFT007 judged a stopped publisher against its declaration and reported \
                 {other:?}: its ring is evenly spaced at exactly the declared rate, so \
                 that verdict is a fabricated all-clear"
            ),
        }
        match &tft008(&live).status {
            Status::Skipped(why) => assert!(
                why.contains("stopped publishing"),
                "TFT008 must name the stopped publisher: {why}"
            ),
            other => panic!(
                "TFT008 reported {other:?} about a stopped publisher: a dead ring's \
                 coefficient of variation is ~0, which is what a perfect publisher looks \
                 like"
            ),
        }
        let note = stopped_publisher_note(&obs, now, PushStream::RingsUnderWriter)
            .expect("a withheld edge has to be disclosed");
        assert!(note.contains("withheld judgement on 1 edge(s)"), "{note}");
    }

    /// **The coverage note and `TFT007`'s own status describe the same run.**
    #[test]
    fn the_rate_coverage_note_and_tft007_agree_about_a_stopped_publisher() {
        const MS: i64 = 1_000_000;
        let sample = |edge: u32, stamp_ns: i64| PushSample {
            edge,
            writer_pid: 4711,
            stamp_ns,
            arrival_delay_ns: 0,
        };
        let declaring = |id: u32, parent: u32, child: u32| {
            let mut e = edge(id, parent, child, 40);
            e.nominal_rate_mhz = Some(100_000); // 100 Hz, in milli-hertz.
            e
        };

        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base", 2, 2),
            ],
            edges: vec![declaring(1, 1, 2), edge(2, 2, 3, 40)],
            participants: live_writer(),
        };
        let obs = Observations::from_samples(steady(1, 40, 10 * MS));
        let newest = obs.by_edge()[&1]
            .last()
            .expect("the fixture has samples on edge 1")
            .stamp_ns;
        let now = Clock::Wall(newest + 8_000 * MS);
        let live = live_inputs(&snap, &obs, &[], now);
        match &tft007(&live).status {
            Status::Skipped(why) => assert!(why.contains("stopped publishing"), "{why}"),
            other => panic!("the fixture must be one TFT007 declines to judge, got {other:?}"),
        }
        assert_eq!(
            rate_coverage_note(&snap, &obs, now, PushStream::RingsUnderWriter),
            None,
            "TFT007 skipped having compared nothing; a note claiming coverage \
             contradicts the status in the same report"
        );

        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base", 2, 2),
                frame(4, "laser", 3, 3),
            ],
            edges: vec![declaring(1, 1, 2), declaring(2, 2, 3), edge(3, 3, 4, 40)],
            participants: live_writer(),
        };
        let mut events = steady(1, 40, 10 * MS);
        let start = newest + 8_000 * MS - 39 * 10 * MS;
        events.extend((0..40i64).map(|k| sample(2, start + k * 10 * MS)));
        let obs = Observations::from_samples(events);
        let live = live_inputs(&snap, &obs, &[], now);
        assert_eq!(
            tft007(&live).status,
            Status::Pass,
            "{:?}",
            tft007(&live).status
        );
        let note = rate_coverage_note(&snap, &obs, now, PushStream::RingsUnderWriter)
            .expect("a partial run must disclose itself");
        assert!(
            note.contains("compared 1 of 3")
                && note.contains("1 declare no nominal rate")
                && note.contains("0 have fewer than 8")
                && note.contains("1 have stopped publishing"),
            "{note}"
        );
    }

    /// **`TFT008` skips rather than passing on an arena it could not measure a
    /// spread over at all.**
    #[test]
    fn tft008_skips_when_nothing_retained_enough_intervals_to_measure() {
        const MS: i64 = 1_000_000;
        let snap = two_frame_snapshot(edge(1, 1, 2, 3));

        let obs = Observations::from_samples(steady(1, 3, 10 * MS));
        let o = tft008(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("has retained the 3 intervals") && !why.contains("stopped"),
                "the skip must name the missing evidence, and must not be the \
                 stopped-publisher one: {why}"
            ),
            other => panic!(
                "TFT008 reported {other:?} about an arena in which it measured no \
                 distribution at all: an empty finding list there means \"nothing to \
                 measure\", not \"every edge is even\""
            ),
        }

        let obs = Observations::from_samples(steady(1, 4, 10 * MS));
        assert_eq!(
            tft008(&inputs(&snap, &obs, &[], Clock::Wall(0))).status,
            Status::Pass
        );
    }

    /// **Every finding-producing arm of `TFT016` fires, and the two arms nobody
    /// could have noticed breaking are pinned by more than a count.**
    #[test]
    fn every_tft016_arm_fires_and_the_two_corrected_strings_are_pinned() {
        let snap = two_frame_snapshot(edge(1, 1, 2, 4));
        let obs = Observations::new();
        let host_inputs = |host: HostFacts| Inputs {
            host: Some(host),
            ..inputs(&snap, &obs, &[], Clock::Wall(0))
        };
        let arena = inputs(&snap, &obs, &[], Clock::Wall(0)).arena_bytes;
        let facts = |thp, shmem_thp, memlock| HostFacts {
            thp,
            shmem_thp,
            memlock,
        };

        for quiet in [
            facts(Thp::Madvise, ShmemThp::Advise, MemLock::Unlimited),
            facts(Thp::Always, ShmemThp::Always, MemLock::Unlimited),
            facts(Thp::Madvise, ShmemThp::WithinSize, MemLock::Unlimited),
            facts(Thp::Madvise, ShmemThp::Advise, MemLock::Bytes(arena)),
        ] {
            let o = tft016(&host_inputs(quiet));
            assert_eq!(
                o.status,
                Status::Pass,
                "nothing is wrong with {quiet:?} and TFT016 reported {o:?}"
            );
        }

        let one = |host: HostFacts, needle: &str| {
            let o = tft016(&host_inputs(host));
            assert_eq!(o.status, Status::Fired, "{host:?} must fire: {o:?}");
            assert_eq!(o.findings.len(), 1, "{host:?} must fire one arm: {o:?}");
            assert!(
                o.findings[0].message.contains(needle),
                "the {host:?} finding must name {needle}: {}",
                o.findings[0].message
            );
            o.findings[0].message.clone()
        };
        one(
            facts(Thp::Never, ShmemThp::Advise, MemLock::Unlimited),
            "transparent huge pages are 'never'",
        );
        one(
            facts(Thp::Unknown, ShmemThp::Advise, MemLock::Unlimited),
            "transparent_hugepage/enabled was absent",
        );
        one(
            facts(Thp::Madvise, ShmemThp::Never, MemLock::Unlimited),
            "shmem transparent huge pages are 'never'",
        );
        one(
            facts(Thp::Madvise, ShmemThp::Deny, MemLock::Unlimited),
            "shmem transparent huge pages are 'deny'",
        );
        one(
            facts(Thp::Madvise, ShmemThp::Unknown, MemLock::Unlimited),
            "transparent_hugepage/shmem_enabled was absent",
        );
        one(
            facts(Thp::Madvise, ShmemThp::Advise, MemLock::Unknown),
            "no 'Max locked memory' row",
        );

        let msg = one(
            facts(Thp::Madvise, ShmemThp::Advise, MemLock::Bytes(arena - 1)),
            "RLIMIT_MEMLOCK is",
        );
        assert!(
            msg.contains(&format!("{} byte arena", arena)),
            "the limit is compared against the arena and the finding must say so: {msg}"
        );
        assert!(
            msg.contains("MCL_ONFAULT") && msg.contains("not a clearance"),
            "0049's two corrections must survive a rewording of the paragraph: {msg}"
        );

        let o = tft016(&host_inputs(facts(
            Thp::Unknown,
            ShmemThp::Unknown,
            MemLock::Unknown,
        )));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(o.findings.len(), 3, "one finding per unknown source: {o:?}");

        let o = tft016(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert!(
            matches!(&o.status, Status::Skipped(why) if why.contains("only on Linux")),
            "with no host facts the check must skip with its reason: {o:?}"
        );

        let recommended = api_md_recommended_mlockall();
        assert_eq!(
            mlockall_args(&msg),
            vec![recommended.clone()],
            "TFT016's advice and docs/API.md §8.3's recommendation name \
             different flags ({recommended} there); 0049 corrected this string \
             once and correcting one site alone is the outcome its Consequences \
             names as the worst"
        );
    }

    /// The `mlockall(...)` flags `docs/API.md` §8.3 **recommends**, whitespace
    /// and backticks removed.
    fn api_md_recommended_mlockall() -> String {
        const API_MD: &str = include_str!("../../../docs/API.md");
        let (_, after) = API_MD
            .split_once("### 8.3 ")
            .expect("docs/API.md must still carry §8.3");
        let section = after.split_once("\n### ").map_or(after, |(body, _)| body);
        mlockall_args(section)
            .into_iter()
            .next()
            .expect("docs/API.md §8.3 must still recommend an mlockall(...) call")
    }

    /// Every `mlockall(...)` argument list in `s`, whitespace and backticks
    /// removed, in document order.
    fn mlockall_args(s: &str) -> Vec<String> {
        s.split("mlockall(")
            .skip(1)
            .filter_map(|rest| rest.split_once(')'))
            .map(|(args, _)| {
                args.chars()
                    .filter(|c| !c.is_whitespace() && *c != '`')
                    .collect()
            })
            .collect()
    }

    /// **`TFT013`'s unmeasurable skip names the obstacle this arena has, and
    /// there are three of them.**
    #[test]
    fn tft013_names_which_of_the_three_unmeasurable_arenas_this_is() {
        let one_sample = |edge: u32, stamp: i64, n: usize| {
            Observations::from_samples(
                (0..n)
                    .map(|_| PushSample {
                        edge,
                        writer_pid: 4711,
                        stamp_ns: stamp,
                        arrival_delay_ns: 0,
                    })
                    .collect(),
            )
        };

        let mut e = edge(1, 1, 2, 3_600);
        e.capacity = 2;
        let snap = two_frame_snapshot(e);
        let obs = one_sample(1, 7_000_000, 1);
        let o = tft013(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("3600 push(es)")
                    && why.contains("no ring in it can hold two")
                    && why.contains("four slots"),
                "the ring is the obstacle and the reason has to say so: {why}"
            ),
            other => panic!("TFT013 reported {other:?} on an arena it cannot measure"),
        }

        let snap = two_frame_snapshot(edge(1, 1, 2, 1));
        let o = tft013(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("rings are large enough")
                    && why.contains("511")
                    && !why.contains("four slots"),
                "511 free slots are not a ring-size problem: {why}"
            ),
            other => panic!("TFT013 reported {other:?} on an arena it cannot measure"),
        }

        let obs = one_sample(1, 7_000_000, 5);
        let snap = two_frame_snapshot(edge(1, 1, 2, 5));
        let o = tft013(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("no edge has a positive median period")
                    && why.contains("TFT009")
                    && !why.contains("four slots")
                    && !why.contains("rings are large enough"),
                "the samples are there and their cadence is what is missing: {why}"
            ),
            other => panic!("TFT013 reported {other:?} on an arena it cannot measure"),
        }
    }

    /// **`TFT009` skips rather than passing on an arena in which it judged no
    /// edge at all**, which is `TFT008`'s defect one row over and reached
    /// further than it.
    #[test]
    fn tft009_skips_when_no_edge_retained_enough_intervals_to_measure_a_gap() {
        const MS: i64 = 1_000_000;
        let snap = two_frame_snapshot(edge(1, 1, 2, 4));
        let now = Clock::Wall(5_030 * MS);
        let stamps = [0, 10 * MS, 20 * MS, 5_020 * MS];
        let obs = Observations::from_samples(
            stamps
                .iter()
                .map(|&ns| tf_tree_bench::fixture::PushSample {
                    edge: 1,
                    writer_pid: 4711,
                    stamp_ns: ns,
                    arrival_delay_ns: 0,
                })
                .collect(),
        );
        let o = tft009(&live_inputs(&snap, &obs, &[], now));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("fewer than the 4 intervals")
                    && !why.contains("backwards")
                    && !why.contains("one instant"),
                "the skip must name the missing evidence, and must not be one of the \
                 other two gaps: {why}"
            ),
            other => panic!(
                "TFT009 reported {other:?} about an arena in which it judged no edge at \
                 all — while a 5 s dropout is in its own samples and TFT008 fires on the \
                 same edge in the same document"
            ),
        }
        assert_ne!(
            tft008(&live_inputs(&snap, &obs, &[], now)).status,
            Status::Pass,
            "if TFT008 ever passes here this test no longer says what it claims"
        );
        assert!(
            silence_coverage_note(&o, Clock::Wall(0), PushStream::Observed).is_none(),
            "TFT009 skipped; a note claiming it measured the retained gaps is the \
             report contradicting itself: {o:?}"
        );

        let obs = Observations::from_samples(
            [0, 10 * MS, 20 * MS, 30 * MS, 5_030 * MS]
                .iter()
                .map(|&ns| tf_tree_bench::fixture::PushSample {
                    edge: 1,
                    writer_pid: 4711,
                    stamp_ns: ns,
                    arrival_delay_ns: 0,
                })
                .collect(),
        );
        let o = tft009(&live_inputs(&snap, &obs, &[], Clock::Wall(5_040 * MS)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert!(
            o.findings.iter().any(|f| f.message.contains("largest gap")),
            "the gap was in the samples all along: {o:?}"
        );

        let obs = Observations::from_samples(
            (0..5)
                .map(|_| tf_tree_bench::fixture::PushSample {
                    edge: 1,
                    writer_pid: 4711,
                    stamp_ns: 7 * MS,
                    arrival_delay_ns: 0,
                })
                .collect(),
        );
        let o = tft009(&live_inputs(&snap, &obs, &[], now));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("one instant") && !why.contains("intervals a median"),
                "a publisher stamping one instant is its own gap, and the remedy is \
                 neither a fuller ring nor TFT018: {why}"
            ),
            other => panic!(
                "TFT009 reported {other:?} about a stream with no period to be a \
                 multiple of"
            ),
        }

        let o = tft009(&live_inputs(&snap, &Observations::new(), &[], now));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("nothing has published") && !why.contains("intervals a median"),
                "an empty observation set is not the too-few-intervals arena: {why}"
            ),
            other => panic!("TFT009 reported {other:?} about an arena with no samples"),
        }
    }

    /// **A live publisher at its declared cadence is not reported as stopped**,
    /// which is the false positive that would make an operator stop reading.
    #[test]
    fn a_publisher_still_publishing_is_not_reported_as_stopped() {
        const MS: i64 = 1_000_000;
        let stamps: Vec<i64> = (0..40).map(|k| k * 10 * MS).collect();
        let newest = *stamps.last().unwrap();
        let obs = Observations::from_samples(
            stamps
                .iter()
                .map(|&ns| tf_tree_bench::fixture::PushSample {
                    edge: 1,
                    writer_pid: 4711,
                    stamp_ns: ns,
                    arrival_delay_ns: 0,
                })
                .collect(),
        );
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        for late in [10 * MS, GAP_FACTOR * 10 * MS] {
            let o = tft009(&live_inputs(&snap, &obs, &[], Clock::Wall(newest + late)));
            assert_eq!(
                o.status,
                Status::Pass,
                "a publisher {late} ns past its last sample is publishing, not stopped: {o:?}"
            );
        }
    }

    /// **`TFT004` fires on a clock no publish pipeline explains, and stays quiet
    /// on one that a pipeline does.**
    #[test]
    fn tft004_fires_only_past_any_plausible_pipeline_latency() {
        let mut plausible = edge(1, 1, 2, 100);
        plausible.clock_offset_nanos = Some(200_000_000); // 200 ms: a localiser.
        let snap = two_frame_snapshot(plausible);
        let obs = Observations::new();
        let out = tft004(&inputs(&snap, &obs, &[], Clock::Wall(2_000_000_000)));
        assert!(
            out.findings.is_empty(),
            "a 200 ms offset was reported as a clock fault: that is an ordinary \
             stamp-to-push latency, and flagging it calls a healthy fleet skewed — {:?}",
            out.findings
        );

        let mut broken = edge(1, 1, 2, 100);
        broken.clock_offset_nanos = Some(3_600_000_000_000); // one hour behind.
        let snap = two_frame_snapshot(broken);
        let out = tft004(&inputs(&snap, &obs, &[], Clock::Wall(2_000_000_000)));
        assert_eq!(out.findings.len(), 1, "an hour-wrong clock went unreported");
        let msg = &out.findings[0].message;
        assert!(
            msg.contains("3600.0 s") && msg.contains("behind"),
            "the finding does not say how far wrong or which way: {msg}"
        );

        let mut extreme = edge(1, 1, 2, 100);
        extreme.clock_offset_nanos = Some(i64::MIN);
        let snap = two_frame_snapshot(extreme);
        let out = tft004(&inputs(&snap, &obs, &[], Clock::Wall(2_000_000_000)));
        assert_eq!(
            out.findings.len(),
            1,
            "an i64::MIN offset was dropped rather than reported: {:?}",
            out.status
        );

        let mut ahead = edge(1, 1, 2, 100);
        ahead.clock_offset_nanos = Some(-3_600_000_000_000);
        let snap = two_frame_snapshot(ahead);
        let out = tft004(&inputs(&snap, &obs, &[], Clock::Wall(2_000_000_000)));
        assert!(
            out.findings[0].message.contains("ahead"),
            "a publisher whose clock is ahead was described as behind: {}",
            out.findings[0].message
        );
    }

    /// **Every reason `TFT004` has no answer, each with its own skip.**
    #[test]
    fn tft004_skips_name_which_of_the_five_conditions_fired() {
        let mut e = edge(1, 1, 2, 100);
        e.clock_offset_nanos = Some(3_600_000_000_000);
        let snap = two_frame_snapshot(e);
        let obs = Observations::new();
        let wall = Clock::Wall(2_000_000_000);

        for (stream, want) in [
            (PushStream::Recorded, "replaying a recording"),
            (PushStream::RingsAtRest, "at rest"),
        ] {
            let mut inp = inputs(&snap, &obs, &[], wall);
            inp.stream = stream;
            let out = tft004(&inp);
            let Status::Skipped(reason) = &out.status else {
                panic!(
                    "a replayed or at-rest source has no live offset: {:?}",
                    out.status
                )
            };
            assert!(
                reason.contains(want),
                "the skip for {stream:?} does not say why: {reason}"
            );
            assert!(
                out.findings.is_empty(),
                "{stream:?} produced findings from offsets that are not about any publisher's clock"
            );
        }

        let out = tft004(&inputs(&snap, &obs, &[], Clock::NewestStamp(2_000_000_000)));
        let Status::Skipped(reason) = &out.status else {
            panic!(
                "a non-wall clock cannot support this check: {:?}",
                out.status
            )
        };
        assert!(
            reason.contains("do not share an epoch") && reason.contains("TFT005"),
            "the epoch skip does not point at the rule it shares: {reason}"
        );

        let mut departed = edge(1, 1, 2, 100);
        departed.clock_offset_nanos = Some(3_600_000_000_000);
        departed.claimed = false;
        departed.owner_slot = None;
        let orphaned = two_frame_snapshot(departed);
        let out = tft004(&inputs(&orphaned, &obs, &[], wall));
        assert!(
            matches!(out.status, Status::Skipped(_)),
            "an unclaimed edge's leftover offset was billed to a publisher that \
             does not exist: {out:?}"
        );
        assert!(
            clock_offset_note(&orphaned, PushStream::Observed, wall).is_none(),
            "the fleet spread counted an edge with no writer"
        );

        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let out = tft004(&inputs(&snap, &obs, &[], wall));
        let Status::Skipped(reason) = &out.status else {
            panic!(
                "an unsampled arena has nothing to compare: {:?}",
                out.status
            )
        };
        assert!(
            reason.contains("no live claim in this arena has recorded a clock offset"),
            "the unsampled skip does not say so: {reason}"
        );
    }

    /// **The fleet spread reaches the report, and says what it is not.**
    #[test]
    fn the_clock_offset_spread_reaches_the_report_and_carries_its_caveat() {
        let mut a = edge(1, 1, 2, 100);
        a.clock_offset_nanos = Some(1_000_000); // 1 ms
        let mut b = edge(2, 2, 3, 100);
        b.clock_offset_nanos = Some(45_000_000); // 45 ms
        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base", 2, 2),
            ],
            edges: vec![a, b],
            participants: live_writer(),
        };

        let note = clock_offset_note(&snap, PushStream::Observed, Clock::Wall(2_000_000_000))
            .expect("two measured offsets are a spread");
        assert!(
            note.contains("2 publisher clock offset(s)")
                && note.contains("1.000 ms")
                && note.contains("45.000 ms"),
            "the note does not carry the fleet's range: {note}"
        );
        assert!(
            note.contains("median 23.000 ms"),
            "an even fleet's median is the mean of the two middles, not the \
             upper one: {note}"
        );
        assert!(
            note.contains("stamp-to-push latency"),
            "the note reports a spread without saying an offset is not only clock error, so a \
             reader will chase a pipeline difference as skew: {note}"
        );

        assert!(
            clock_offset_note(&snap, PushStream::Recorded, Clock::Wall(2_000_000_000)).is_none(),
            "a replayed source emitted a spread the check itself skips on: a note that \
             contradicts a skip is worse than no note"
        );
    }

    /// **`TFT011` must not fire on a gap past the *newest* end of the window.**
    #[test]
    fn ring_undersize_is_only_claimed_when_a_request_fell_off_the_back() {
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let obs = Observations::new();

        let after_only = [EdgeStats {
            edge: 1,
            lookups_ok: 1000,
            extrap_before: 0,
            extrap_after: 40,
            worst_extrap_gap_ns: 5_000_000_000,
            oldest_stamp: Some(0),
            newest_stamp: Some(1_000_000_000),
            ..EdgeStats::default()
        }];
        let o = tft011(&inputs(&snap, &obs, &after_only, Clock::Wall(0)));
        assert_eq!(
            o.status,
            Status::Pass,
            "a gap past the newest end is a stopped publisher, not a small ring: {o:?}"
        );

        let before = [EdgeStats {
            extrap_before: 40,
            extrap_after: 0,
            ..after_only[0].clone()
        }];
        let o = tft011(&inputs(&snap, &obs, &before, Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(o.findings.len(), 1);
        assert_eq!(o.findings[0].edge, Some(1));
        assert!(
            o.findings[0].message.contains("1000.0 ms"),
            "the retained span must be the ring's real one: {}",
            o.findings[0].message
        );

        let fits = [EdgeStats {
            worst_extrap_gap_ns: 500_000_000,
            ..before[0].clone()
        }];
        assert_eq!(
            tft011(&inputs(&snap, &obs, &fits, Clock::Wall(0))).status,
            Status::Pass
        );
    }

    /// **A zero stamp is only a fault when the arena's stamps are Unix time.**
    #[test]
    fn a_zero_stamp_is_a_fault_only_under_a_wall_clock_domain() {
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let obs = Observations::new();
        let zeros = [EdgeStats {
            edge: 1,
            zero_stamps: 3,
            oldest_stamp: Some(0),
            newest_stamp: Some(1_000_000_000),
            ..EdgeStats::default()
        }];

        let o = tft006(&inputs(
            &snap,
            &obs,
            &zeros,
            Clock::NewestStamp(1_000_000_000),
        ));
        assert_eq!(o.status, Status::Pass, "{o:?}");

        let o = tft006(&inputs(&snap, &obs, &zeros, Clock::Wall(1_000_000_000)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert!(o.findings[0].message.contains("1970"));

        let negs = [EdgeStats {
            negative_stamps: 1,
            zero_stamps: 0,
            ..zeros[0].clone()
        }];
        assert_eq!(
            tft006(&inputs(
                &snap,
                &obs,
                &negs,
                Clock::NewestStamp(1_000_000_000)
            ))
            .status,
            Status::Fired
        );
    }

    /// **The units error `TFT006` exists to catch is a distance, not a range.**
    /// A publisher writing nanoseconds into a field it believed held seconds is
    /// off by 10^9, which no plausible-range check on the value alone
    /// distinguishes from a valid stamp — but it is enormously far from every
    /// other stamp in the arena.
    #[test]
    fn an_absurd_stamp_is_measured_against_the_reference_clock() {
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let obs = Observations::new();
        let now = 1_700_000_000_000_000_000; // ~2023 in Unix nanos
        let sane = [EdgeStats {
            edge: 1,
            oldest_stamp: Some(now - 1_000_000_000),
            newest_stamp: Some(now),
            ..EdgeStats::default()
        }];
        assert_eq!(
            tft006(&inputs(&snap, &obs, &sane, Clock::Wall(now))).status,
            Status::Pass
        );

        let absurd = [EdgeStats {
            newest_stamp: Some(now.saturating_mul(2)),
            ..sane[0].clone()
        }];
        let o = tft006(&inputs(&snap, &obs, &absurd, Clock::Wall(now)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert!(o.findings[0]
            .message
            .contains("days from the reference clock"));
    }

    /// An arena in which edge 1 has been publishing at 100 Hz for `activity_ns`,
    /// plus the `subject` edge (id 2) the test is about.
    fn with_a_running_publisher(subject: EdgeInfo, activity_ns: i64) -> (Snapshot, Observations) {
        const PERIOD_NS: i64 = 10_000_000; // 100 Hz.
        let head = u64::try_from(activity_ns / PERIOD_NS).unwrap() + 1;
        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base", 2, 2),
            ],
            edges: vec![edge(1, 1, 2, head), subject],
            participants: live_writer(),
        };
        (snap, Observations::from_samples(steady(1, 10, PERIOD_NS)))
    }

    /// **`TFT005` fires past its tolerance, stays quiet inside it, and refuses
    /// the question outright when the arena's stamps are not wall-clock time.**
    #[test]
    fn tft005_fires_past_the_future_tolerance_and_not_inside_it() {
        let obs = Observations::new();
        let stats: [EdgeStats; 0] = [];
        const NOW: i64 = 1_700_000_000_000_000_000;
        let ahead_by = |ns: i64| {
            let mut e = edge(1, 1, 2, 100);
            e.newest_stamp = Some(NOW + ns);
            two_frame_snapshot(e)
        };

        let snap = ahead_by(FUTURE_TOLERANCE_NS);
        let o = tft005(&inputs(&snap, &obs, &stats, Clock::Wall(NOW)));
        assert_eq!(
            o.status,
            Status::Pass,
            "a stamp at the tolerance is the normal case the constant exists for: {o:?}"
        );

        let snap = ahead_by(FUTURE_TOLERANCE_NS + 1);
        let o = tft005(&inputs(&snap, &obs, &stats, Clock::Wall(NOW)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(o.findings.len(), 1, "{:?}", o.findings);
        assert_eq!(o.findings[0].edge, Some(1));

        let snap = ahead_by(500_000_000);
        let o = tft005(&inputs(&snap, &obs, &stats, Clock::Wall(NOW)));
        let msg = &o.findings[0].message;
        assert!(
            msg.contains("500 ms ahead of the wall clock") && msg.contains("tolerance 50 ms"),
            "the finding must say how far ahead and against what tolerance: {msg}"
        );

        let o = tft005(&inputs(&snap, &obs, &stats, Clock::NewestStamp(NOW)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("share an epoch"),
                "the skip must name the epoch condition: {why}"
            ),
            other => panic!(
                "a non-wall-clock arena has no \"in the future\": {other:?} — every edge of a \
                 monotonic-clock arena would be reported decades ahead"
            ),
        }
    }

    /// **`TFT013` is about dynamic edges only.**
    #[test]
    fn never_published_does_not_accuse_static_edges() {
        let stats: [EdgeStats; 0] = [];
        let long_enough = DECLARATION_GRACE_NS * 2;

        let mut e = edge(2, 2, 3, 0);
        e.kind = EdgeKind::Static;
        e.capacity = 0;
        let (snap, obs) = with_a_running_publisher(e, long_enough);
        assert_eq!(
            tft013(&inputs(&snap, &obs, &stats, Clock::Wall(0))).status,
            Status::Pass,
            "a static edge's head is 0 for the life of a correct arena"
        );

        let (snap, obs) = with_a_running_publisher(edge(2, 2, 3, 0), long_enough);
        let o = tft013(&inputs(&snap, &obs, &stats, Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(o.findings.len(), 1, "{:?}", o.findings);
        assert_eq!(o.findings[0].edge, Some(2));

        let (snap, obs) = with_a_running_publisher(edge(2, 2, 3, 7), long_enough);
        assert_eq!(
            tft013(&inputs(&snap, &obs, &stats, Clock::Wall(0))).status,
            Status::Pass
        );
    }

    /// **`TFT013` waits out the grace period §6's row has always required, and
    /// says so instead of accusing every dynamic edge at bringup.**
    #[test]
    fn an_unpublished_edge_is_not_accused_inside_the_grace_period() {
        let stats: [EdgeStats; 0] = [];
        const MS: i64 = 1_000_000;

        let (snap, obs) = with_a_running_publisher(edge(2, 2, 3, 0), DECLARATION_GRACE_NS - MS);
        let o = tft013(&inputs(&snap, &obs, &stats, Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("grace period"),
                "the skip must name the grace period as the reason: {why}"
            ),
            other => panic!(
                "an arena younger than the grace period must not accuse an unpublished \
                 edge: {other:?}"
            ),
        }

        let (snap, obs) = with_a_running_publisher(edge(2, 2, 3, 0), DECLARATION_GRACE_NS + MS);
        let o = tft013(&inputs(&snap, &obs, &stats, Clock::Wall(0)));
        assert_eq!(
            o.status,
            Status::Fired,
            "past the grace period an edge nothing has ever published to is the fault \
             this check is for: {o:?}"
        );

        let snap = two_frame_snapshot(edge(1, 1, 2, 0));
        let o = tft013(&inputs(&snap, &Observations::new(), &stats, Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(why.contains("TFT017"), "{why}"),
            other => panic!("expected a skip on an arena with no published stream: {other:?}"),
        }
    }

    /// **The grace period is measured against the *longest*-running publisher
    /// in the arena, and only against a dynamic one.**
    #[test]
    fn the_grace_period_reads_the_longest_running_dynamic_publisher() {
        const MS: i64 = 1_000_000;
        const PERIOD_NS: i64 = 10_000_000; // 100 Hz, as `with_a_running_publisher`.
        let stats: [EdgeStats; 0] = [];
        let head_for = |activity_ns: i64| u64::try_from(activity_ns / PERIOD_NS).unwrap() + 1;
        let arena = |a: i64, b: i64, kind_of_3: EdgeKind| {
            let mut third = edge(3, 3, 4, head_for(b));
            third.kind = kind_of_3;
            let snap = Snapshot {
                frames: vec![
                    frame(1, "map", 0, 0),
                    frame(2, "odom", 1, 1),
                    frame(3, "base", 2, 2),
                    frame(4, "laser", 3, 3),
                ],
                edges: vec![edge(1, 1, 2, head_for(a)), edge(2, 2, 3, 0), third],
                participants: live_writer(),
            };
            let mut events = steady(1, 10, PERIOD_NS);
            events.extend(steady(3, 10, PERIOD_NS));
            (snap, Observations::from_samples(events))
        };

        for (a, b) in [
            (DECLARATION_GRACE_NS - MS, DECLARATION_GRACE_NS * 2),
            (DECLARATION_GRACE_NS * 2, DECLARATION_GRACE_NS - MS),
        ] {
            let (snap, obs) = arena(a, b, EdgeKind::Dynamic);
            let o = tft013(&inputs(&snap, &obs, &stats, Clock::Wall(0)));
            assert_eq!(
                o.status,
                Status::Fired,
                "a publisher that has been up for twice the grace period is evidence \
                 that an edge with head == 0 should have seen something: {o:?}"
            );
            assert_eq!(o.findings.len(), 1, "{:?}", o.findings);
            assert_eq!(o.findings[0].edge, Some(2));
        }

        let (snap, obs) = arena(
            DECLARATION_GRACE_NS - MS,
            DECLARATION_GRACE_NS * 2,
            EdgeKind::Static,
        );
        let o = tft013(&inputs(&snap, &obs, &stats, Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("grace period"),
                "only a dynamic publisher's activity clears the grace: {why}"
            ),
            other => panic!(
                "a static edge's stream cleared the grace period for a check about \
                 dynamic publishers: {other:?}"
            ),
        }
    }

    /// **`TFT010` needs the counters, and says so rather than passing.**
    #[test]
    fn the_hotspot_check_reports_missing_instrumentation_as_not_run() {
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let obs = Observations::new();
        let hot = [EdgeStats {
            edge: 1,
            lookups_ok: 100,
            extrap_after: 50,
            ..EdgeStats::default()
        }];

        let mut inp = inputs(&snap, &obs, &hot, Clock::Wall(0));
        inp.counters = false;
        let o = tft010(&inp);
        assert!(matches!(o.status, Status::Skipped(_)), "{o:?}");

        inp.counters = true;
        let o = tft010(&inp);
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert!(
            o.findings[0].message.contains("33.3%"),
            "rate must be errors/(errors+ok): {}",
            o.findings[0].message
        );

        let cool = [EdgeStats {
            lookups_ok: 100_000,
            extrap_after: 1,
            ..hot[0].clone()
        }];
        inp.stats = &cool;
        assert_eq!(tft010(&inp).status, Status::Pass);
    }

    /// **The clock is only called a wall clock when the arena agrees with it.**
    #[test]
    fn the_reference_clock_refuses_to_mix_time_domains() {
        let unix_now = 1_700_000_000_000_000_000;
        assert_eq!(
            Clock::decide(&[9_900_000_000, 9_800_000_000], unix_now),
            Clock::NewestStamp(9_900_000_000)
        );
        assert_eq!(
            Clock::decide(&[unix_now - 60_000_000_000], unix_now),
            Clock::Wall(unix_now)
        );
        assert_eq!(Clock::decide(&[], unix_now), Clock::Wall(unix_now));
    }

    /// **One broken publisher must not be able to define the reference clock.**
    #[test]
    fn a_single_units_error_cannot_capture_the_reference_clock() {
        let unix_now = 1_700_000_000_000_000_000;
        let mut stamps: Vec<i64> = (0..5).map(|i| unix_now - i * 200_000_000).collect();
        let rogue = unix_now * 2;
        stamps.push(rogue);

        assert_eq!(
            Clock::decide(&stamps, unix_now),
            Clock::Wall(unix_now),
            "5 of 6 edges agree with the wall clock; the 6th must not be able to \
             redefine the domain"
        );

        let snap = Snapshot {
            frames: vec![frame(1, "map", 0, 0), frame(2, "odom", 1, 1)],
            edges: (0..6).map(|i| edge(i + 1, 1, 2, 100)).collect(),
            participants: live_writer(),
        };
        let stats: Vec<EdgeStats> = stamps
            .iter()
            .enumerate()
            .map(|(i, &s)| EdgeStats {
                edge: u32::try_from(i).unwrap() + 1,
                oldest_stamp: Some(s),
                newest_stamp: Some(s),
                ..EdgeStats::default()
            })
            .collect();
        let obs = Observations::new();
        let o = tft006(&inputs(
            &snap,
            &obs,
            &stats,
            Clock::decide(&stamps, unix_now),
        ));
        let blamed: Vec<u32> = o.findings.iter().filter_map(|f| f.edge).collect();
        assert_eq!(blamed, vec![6], "only the rogue edge is absurd: {o:?}");
    }

    /// **A claim caught mid-handoff is not a leaked claim.**
    #[test]
    fn a_claim_caught_mid_handoff_is_not_reported_as_a_leak() {
        let obs = Observations::new();
        let mut mid = edge(1, 1, 2, 100);
        mid.claimed = true;
        mid.claiming = true;
        mid.owner_slot = None;
        mid.owner_pid = 0;

        let snap = two_frame_snapshot(mid.clone());
        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(
            o.status,
            Status::Pass,
            "a record in CLAIMING is a handoff in flight, not a leak: {o:?}"
        );

        let mut leaked = mid;
        leaked.claiming = false;
        let snap = two_frame_snapshot(leaked);
        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(
            o.findings.len(),
            1,
            "a claim held by a slot with no live identity is still a leak: {o:?}"
        );
    }

    /// **The wedge state: a `LIVE` record whose process is gone, and the claim
    /// it is still holding.** Both halves of `TFT014`'s title, on one arena.
    #[test]
    fn a_stale_live_slot_and_the_claim_it_stranded_are_both_reported() {
        let obs = Observations::new();
        let mut held = edge(1, 1, 2, 100);
        held.owner_slot = Some(0);
        held.owner_pid = 4711;

        let mut snap = two_frame_snapshot(held);
        snap.participants = vec![
            ParticipantInfo {
                slot: 0,
                state: SlotState::Live,
                pid: 4711,
                alive: false,
                byte: LockByte::Unknown,
                recorded_pid: None,
                recorded: RecordedProcess::Unknown,
            },
            ParticipantInfo {
                slot: 1,
                state: SlotState::Reserved,
                pid: 4712,
                alive: false,
                byte: LockByte::Unknown,
                recorded_pid: None,
                recorded: RecordedProcess::Unknown,
            },
        ];

        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(o.findings.len(), 2, "one slot and one edge: {o:?}");
        assert_eq!(o.findings[0].subject, "slot 0 pid 4711, byte not probed");
        assert_eq!(o.findings[0].edge, None, "a slot leak is not about an edge");
        assert!(
            o.findings[0].message.contains("1 of 2 slots"),
            "the slot finding must say how much of the fixed budget is spent: {:?}",
            o.findings[0].message
        );
        assert_eq!(
            o.findings[1].edge,
            Some(1),
            "the claim half must still name the edge: {o:?}"
        );

        let frozen = tft014(&Inputs {
            slots: SlotTable::Image,
            ..inputs(&snap, &obs, &[], Clock::Wall(0))
        });
        assert!(
            matches!(frozen.status, Status::Skipped(_)),
            "a frozen .tft cannot be asked this and must say so, not pass: {frozen:?}"
        );
    }

    /// One participant slot with the three facts [`slot_leak`] reads.
    fn slot(
        n: u32,
        state: SlotState,
        byte: LockByte,
        recorded: RecordedProcess,
    ) -> ParticipantInfo {
        ParticipantInfo {
            slot: n,
            state,
            pid: 4712,
            alive: false,
            byte,
            recorded,
            recorded_pid: (byte != LockByte::Unknown).then_some(4712),
        }
    }

    /// **A `RESERVED` record over a free byte is a leak, and the byte is what
    /// makes it visible.**
    #[test]
    fn a_reserved_record_over_a_free_byte_is_the_leak_a_byte_can_see() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        snap.participants.push(slot(
            1,
            SlotState::Reserved,
            LockByte::Free,
            RecordedProcess::Gone,
        ));

        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(o.findings.len(), 1, "one slot, no edge: {o:?}");
        assert!(
            o.findings[0].message.starts_with("a record left behind —"),
            "the two TFT014 shapes must be separable from the first few words: {}",
            o.findings[0].message
        );
        assert!(
            o.findings[0].message.contains("the record is RESERVED"),
            "the message must say which word it found, or an operator cannot \
             tell a half-finished registration from a finished one: {}",
            o.findings[0].message
        );
    }

    /// **A byte-less record in a served arena is accused of leaking, and the
    /// accusation is about a process that is running.**
    #[test]
    fn a_byteless_record_in_a_served_arena_is_accused_of_leaking() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        snap.participants.push(ParticipantInfo {
            slot: 1,
            state: SlotState::Live,
            pid: 4712,
            alive: false,
            byte: LockByte::Free,
            recorded: RecordedProcess::Unknown,
            recorded_pid: None,
        });

        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(o.findings.len(), 1, "one slot, no edge: {o:?}");
        let m = &o.findings[0].message;
        assert!(
            m.starts_with("a record left behind —"),
            "the abandoned shape, not the fork one: {m}"
        );
        assert!(
            m.contains("the lock byte is free"),
            "the evidence must name the byte this run probed, which is what tells an `--attach` finding from a `--from-bag` one: {m}"
        );
        assert!(
            m.contains("no identity record out of the lock file"),
            "this run got no lock-file record for the slot: {m}"
        );
        assert!(
            m.contains("none written, or none readable"),
            "the clause must not settle which of the two it was — a failed read and an absent record fold together upstream: {m}"
        );
        assert!(
            m.contains("the pid this finding names is the arena record's own")
                && m.contains("pid 4712"),
            "the finding prints a pid and its remedy says to check it, so the evidence must say where that number came from: {m}"
        );
        assert!(
            !m.contains("/proc could not say"),
            "/proc was never asked here — that clause is for a record that exists and a probe that would not answer: {m}"
        );
        assert!(
            !m.contains("this run has no kernel answer about the byte"),
            "that clause belongs to the `LockByte::Unknown` row, which this shape does not take: reaching it would mean the retracted sentence was right: {m}"
        );
    }

    /// **A record caught mid-registration names no pid, and the finding must
    /// not send an operator to check one.**
    #[test]
    fn a_record_with_no_pid_at_all_is_not_something_to_go_and_check() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        snap.participants.push(ParticipantInfo {
            slot: 1,
            state: SlotState::Reserved,
            pid: 0,
            alive: false,
            byte: LockByte::Free,
            recorded: RecordedProcess::Unknown,
            recorded_pid: None,
        });

        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        let m = &o.findings[0].message;
        assert!(
            m.contains("no process here to ask /proc about and none to check"),
            "a record with no pid must say so, not point at the zero it printed: {m}"
        );
        assert!(
            !m.contains("the pid below is the arena record's own"),
            "there is no pid below — that clause is the sibling's: {m}"
        );
        assert!(
            !m.contains("pid 0") && !m.contains("CHECK THE PID"),
            "nothing may print a zero as a pid or send anybody to check one: {m}"
        );
        assert!(
            m.contains("slot 1 was left registered"),
            "the clause that named a process must still say which slot: {m}"
        );
        assert_eq!(
            o.findings[0].subject, "slot 1, no pid recorded, byte free",
            "the subject is the line `docs/RUNBOOK.md`'s rows key on, so it is \
             the third place the zero must not appear"
        );
    }

    /// **The fork case is its own finding, with its own remedy.**
    #[test]
    fn a_held_byte_over_a_dead_pid_is_the_fork_case_and_says_so() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        snap.participants.push(slot(
            1,
            SlotState::Live,
            LockByte::Held,
            RecordedProcess::Gone,
        ));

        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(o.findings.len(), 1, "{o:?}");
        let m = &o.findings[0].message;
        assert!(
            m.contains("forked child inherited it"),
            "the fork case must be named as itself: {m}"
        );
        assert!(
            m.contains("`spawn`"),
            "the remedy is a start method and the message has to name it: {m}"
        );
        assert!(
            !m.contains("Only stopping every participant"),
            "a fork-held byte comes back when the last inheritor exits, so the \
             permanent-loss wording belongs to the other finding: {m}"
        );
    }

    /// **A joiner mid-attach, holding its byte, is not reported — either way.**
    #[test]
    fn a_joiner_holding_its_byte_mid_attach_is_not_reported() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        snap.participants.push(slot(
            1,
            SlotState::Reserved,
            LockByte::Held,
            RecordedProcess::Running,
        ));
        snap.participants.push(slot(
            2,
            SlotState::Reserved,
            LockByte::Held,
            RecordedProcess::Unknown,
        ));

        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Pass, "{o:?}");
    }

    /// **A process `/proc` says is running is not called leaked, whatever its
    /// byte says.**
    #[test]
    fn a_running_process_over_a_free_byte_is_not_called_leaked() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        snap.participants.push(slot(
            1,
            SlotState::Live,
            LockByte::Free,
            RecordedProcess::Running,
        ));

        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Pass, "{o:?}");
    }

    /// **A message never claims a syscall the run did not make.**
    #[test]
    fn the_evidence_clause_names_the_facts_the_run_actually_had() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        snap.participants.push(slot(
            1,
            SlotState::Live,
            LockByte::Free,
            RecordedProcess::Gone,
        ));
        snap.participants.push(slot(
            2,
            SlotState::Live,
            LockByte::Unknown,
            RecordedProcess::Unknown,
        ));

        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.findings.len(), 2, "{o:?}");
        assert!(
            o.findings[0]
                .message
                .contains("the lock byte is free, and /proc has no running process"),
            "{}",
            o.findings[0].message
        );
        assert!(
            o.findings[1]
                .message
                .contains("this run has no kernel answer about the byte"),
            "a run with no byte answer must not claim the byte is free: {}",
            o.findings[1].message
        );
        assert!(
            !o.findings[1]
                .message
                .contains("/proc says its process is gone"),
            "this row's verdict does not rest on /proc either — `alive` can come \
             from the kernel — so the clause must not say it did: {}",
            o.findings[1].message
        );
        assert!(
            o.findings[1].message.contains("2 of 3 slots"),
            "the budget must count both abandoned slots: {}",
            o.findings[1].message
        );
    }

    /// **A slot nobody holds is not a leak, and neither is a read-only
    /// consumer that is running.**
    #[test]
    fn a_free_slot_is_not_a_leak() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        snap.participants
            .extend((1..64).map(|slot| ParticipantInfo {
                slot,
                state: SlotState::Free,
                pid: 0,
                alive: false,
                byte: if slot % 2 == 0 {
                    LockByte::Held
                } else {
                    LockByte::Free
                },
                recorded: if slot % 2 == 0 {
                    RecordedProcess::Running
                } else {
                    RecordedProcess::Unknown
                },
                recorded_pid: (slot % 2 == 0).then_some(9000 + slot),
            }));

        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Pass, "{o:?}");
    }

    /// **A `FREE` record over a held byte whose pid is gone is the fork case,
    /// and it is the one `doctor` most needs to report.**
    #[test]
    fn a_free_record_over_a_held_byte_for_a_dead_pid_is_the_fork_case() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        snap.participants.push(ParticipantInfo {
            slot: 3,
            state: SlotState::Free,
            pid: 0,
            alive: false,
            byte: LockByte::Held,
            recorded: RecordedProcess::Gone,
            recorded_pid: Some(1841),
        });

        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(o.findings.len(), 1, "{o:?}");
        assert_eq!(
            o.findings[0].subject, "slot 3 pid 1841, byte still HELD",
            "the subject must name the lock file's pid, not the empty arena \
             record's, and must say the byte is held"
        );
        let m = &o.findings[0].message;
        assert!(
            m.contains("forked child inherited it"),
            "the fork case must be named as itself: {m}"
        );
        assert!(
            m.contains("read-only participant"),
            "an operator told the record is FREE needs to be told why a leak has \
             no record: {m}"
        );
    }

    /// **The subject names the pid the evidence is about, and names the arena
    /// record's separately when they differ.**
    #[test]
    fn the_subject_names_the_pid_the_evidence_is_about() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        snap.participants.push(ParticipantInfo {
            slot: 8,
            state: SlotState::Reserved,
            pid: 0,
            alive: false,
            byte: LockByte::Free,
            recorded: RecordedProcess::Gone,
            recorded_pid: Some(1841),
        });
        snap.participants.push(ParticipantInfo {
            slot: 9,
            state: SlotState::Live,
            pid: 4711,
            alive: false,
            byte: LockByte::Free,
            recorded: RecordedProcess::Gone,
            recorded_pid: Some(1842),
        });

        let o = tft014(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(
            o.findings[0].subject, "slot 8 pid 1841, byte free",
            "a RESERVED record's pid field is 0 and naming it is naming nobody"
        );
        assert!(
            o.findings[0]
                .message
                .contains("pid 1841 left slot 8 registered"),
            "the message must be about the same pid as the subject: {}",
            o.findings[0].message
        );
        assert_eq!(
            o.findings[1].subject, "slot 9 pid 1842 (arena record names pid 4711), byte free",
            "when both pids are real, both are worth an operator's time"
        );
    }

    /// **An out-of-order stream is not a dropout.**
    #[test]
    fn an_out_of_order_stream_is_not_reported_as_a_dropout() {
        const MS: i64 = 1_000_000;
        let stamps = [0, 100, 200, 300, 50, 400, 500, 600];
        let obs = Observations::from_samples(
            stamps
                .iter()
                .map(|&ms| tf_tree_bench::fixture::PushSample {
                    edge: 1,
                    writer_pid: 4711,
                    stamp_ns: ms * MS,
                    arrival_delay_ns: 0,
                })
                .collect(),
        );
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let o = tft009(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("goes backwards") && why.contains("TFT018"),
                "a reordered stream has no inter-arrival distribution to measure a gap \
                 against, and the skip must name the fault that does explain it: {why}"
            ),
            other => panic!(
                "TFT009 reported {other:?} about a stream whose only edge it declined: \
                 the fault is out-of-order, not a dropout, and an empty subject set is \
                 not a pass"
            ),
        }

        assert!(
            !doctor::check_out_of_order(&obs).is_empty(),
            "the fixture must actually be non-monotone, or this asserts nothing"
        );
        let clean = [0, 100, 200, 300, 400, 800, 900, 1000];
        let obs = Observations::from_samples(
            clean
                .iter()
                .map(|&ms| tf_tree_bench::fixture::PushSample {
                    edge: 1,
                    writer_pid: 4711,
                    stamp_ns: ms * MS,
                    arrival_delay_ns: 0,
                })
                .collect(),
        );
        let o = tft009(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(
            o.findings.len(),
            1,
            "a real gap in a monotone stream must still fire: {o:?}"
        );
    }

    /// **`TFT017` and `TFT018` report at the severity their Phase 1 checks
    /// assign, and the two answers are compared here because nothing else
    /// compares them.**
    #[test]
    fn the_two_new_ids_keep_their_phase_1_severities() {
        let unclaimed = Snapshot {
            frames: vec![frame(1, "map", 0, 0), frame(2, "odom", 1, 1)],
            edges: vec![EdgeInfo {
                claimed: false,
                owner_pid: 0,
                ..edge(1, 1, 2, 100)
            }],
            participants: live_writer(),
        };
        let f = doctor::check_unclaimed_dynamic(&unclaimed);
        assert_eq!(f.len(), 1, "the fixture must fire the check it is about");
        assert_eq!(
            crate::catalogue::Severity::from(f[0].severity),
            Tft::Tft017.severity(),
            "TFT017's severity must be the one `unclaimed-dynamic` assigns"
        );

        let obs = Observations::from_samples(vec![
            PushSample {
                edge: 1,
                writer_pid: 1,
                stamp_ns: 100,
                arrival_delay_ns: 0,
            },
            PushSample {
                edge: 1,
                writer_pid: 1,
                stamp_ns: 50,
                arrival_delay_ns: 0,
            },
        ]);
        let f = doctor::check_out_of_order(&obs);
        assert_eq!(f.len(), 1);
        assert_eq!(
            crate::catalogue::Severity::from(f[0].severity),
            Tft::Tft018.severity(),
            "TFT018's severity must be the one `out-of-order` assigns"
        );

        let snap = two_frame_snapshot(EdgeInfo {
            claimed: false,
            owner_pid: 0,
            ..edge(1, 1, 2, 100)
        });
        let report = run(&inputs(&snap, &obs, &[], Clock::Wall(0)), &BTreeSet::new());
        assert!(
            report.uncatalogued.is_empty(),
            "neither check is id-less any more: {:?}",
            report.uncatalogued
        );
        let fired: Vec<&str> = report
            .outcomes
            .iter()
            .filter(|o| o.status == Status::Fired)
            .map(|o| o.check.id())
            .collect();
        assert!(
            fired.contains(&"TFT017") && fired.contains(&"TFT018"),
            "{fired:?}"
        );
        assert!(report.has_error(), "an out-of-order stream must still gate");
    }

    /// One publish period in the synthetic streams below.
    const PERIOD_NS: i64 = 10_000_000;

    /// A stream on `edge` at [`PERIOD_NS`] that runs forward, has its clock
    /// stepped back by `back_ns`, and then keeps publishing at the same rate.
    fn stepped_back(edge: u32, back_ns: i64) -> Vec<PushSample> {
        let mut stamps: Vec<i64> = (0..10).map(|i| i * PERIOD_NS).collect();
        let last = stamps[stamps.len() - 1];
        let mut t = last - back_ns;
        while t <= last {
            stamps.push(t);
            t += PERIOD_NS;
        }
        stamps.push(t);
        stamps
            .into_iter()
            .map(|stamp_ns| PushSample {
                edge,
                writer_pid: 4711,
                stamp_ns,
                arrival_delay_ns: 0,
            })
            .collect()
    }

    /// A two-edge chain `map -> odom -> base` whose second edge carries `domain`.
    fn chain_with_domains(first_domain: u8, second_domain: u8) -> Snapshot {
        Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base", 2, 2),
            ],
            edges: vec![
                EdgeInfo {
                    domain: first_domain,
                    ..edge(1, 1, 2, 100)
                },
                EdgeInfo {
                    domain: second_domain,
                    ..edge(2, 2, 3, 100)
                },
            ],
            participants: live_writer(),
        }
    }

    /// **`TFT019` attributes a backwards run to a clock step only on tag 0, and
    /// on any other tag it says which tag rather than guessing.**
    #[test]
    fn tft019_fires_only_on_the_wall_clock_tag_and_names_the_tag_it_refuses() {
        const MS: i64 = 1_000_000;
        let snap = chain_with_domains(0, 3);
        let mut events = stepped_back(1, 100 * MS);
        events.extend(stepped_back(2, 100 * MS));
        let obs = Observations::from_samples(events);

        let o = tft019(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(
            o.findings.iter().map(|f| f.edge).collect::<Vec<_>>(),
            vec![Some(1)],
            "only the wall-clock edge may be attributed: {:?}",
            o.findings
        );
        assert!(
            o.findings[0].message.contains("100.000 ms")
                && o.findings[0].message.contains("CLOCK_REALTIME")
                && o.findings[0].message.contains("SteadyDomain"),
            "the finding must carry the size of the step, the cause, and the fix: {}",
            o.findings[0].message
        );
        let note = ClockStepEvidence::capture(&snap, &obs)
            .coverage_note(PushStream::Observed)
            .expect("a partial run discloses");
        assert!(
            note.contains("edge#2 tag 3") && note.contains("1 of 2"),
            "{note}"
        );

        let snap = chain_with_domains(1, 3);
        let o = tft019(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("edge#1 tag 1")
                    && why.contains("edge#2 tag 3")
                    && why.contains("a steady clock, which cannot have stepped"),
                "the skip must name every tag it declined to guess about: {why}"
            ),
            other => panic!("expected a skip on non-wall-clock tags, got {other:?}"),
        }
        assert_eq!(
            ClockStepEvidence::capture(&snap, &obs).coverage_note(PushStream::Observed),
            None,
            "the skip reason carries the whole disclosure here, so the note stays silent"
        );

        let snap = chain_with_domains(2, 2);
        let o = tft019(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("simulated time, which does step backwards")
                    && why.contains("0012")
                    && !why.contains("cannot have stepped"),
                "sim time must be sent to 0012, not told its clock cannot step: {why}"
            ),
            other => panic!("expected a skip on the sim tag, got {other:?}"),
        }

        let snap = chain_with_domains(0, 0);
        let obs = Observations::from_samples(steady(1, 8, 50 * MS));
        let o = tft019(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Pass, "{o:?}");
    }

    /// **A single stray inversion on a wall clock is not a clock step.**
    #[test]
    fn tft019_needs_a_run_of_rejections_not_a_single_inversion() {
        let snap = chain_with_domains(0, 0);

        let obs = Observations::from_samples(stepped_back(1, 2 * PERIOD_NS));
        let inp = inputs(&snap, &obs, &[], Clock::Wall(0));
        assert_eq!(
            tft019(&inp).status,
            Status::Pass,
            "a two-arrival inversion is a publisher fault, not an NTP step"
        );
        assert_eq!(
            tft018(&inp).status,
            Status::Fired,
            "the detector is untouched: rejected pushes are lost data either way"
        );
        let note = ClockStepEvidence::capture(&snap, &obs)
            .coverage_note(PushStream::Observed)
            .expect("a diffuse wall-clock run is disclosed rather than silently passed");
        assert!(
            note.contains("edge#1 longest run 2")
                && note.contains(&format!("at least {CLOCK_STEP_MIN_REJECTED_RUN}")),
            "{note}"
        );

        let obs = Observations::from_samples(stepped_back(
            1,
            PERIOD_NS * CLOCK_STEP_MIN_REJECTED_RUN as i64,
        ));
        assert_eq!(
            doctor::out_of_order_runs(&obs)[0].longest_rejected_run,
            CLOCK_STEP_MIN_REJECTED_RUN,
            "the fixture must sit exactly on the boundary for this to pin it"
        );
        assert_eq!(
            tft019(&inputs(&snap, &obs, &[], Clock::Wall(0))).status,
            Status::Fired
        );
    }

    /// **`TFT019` inherits `TFT018`'s live-arena skip rather than working around
    /// it**, which `docs/PHASE5.md` §6's amendment requires in those words.
    #[test]
    fn tft019_inherits_tft018s_replayed_stream_skip() {
        const MS: i64 = 1_000_000;
        let snap = chain_with_domains(0, 0);
        let obs = Observations::from_samples(stepped_back(1, 100 * MS));
        let mut inp = inputs(&snap, &obs, &[], Clock::Wall(0));
        inp.stream = PushStream::RingsUnderWriter;

        for o in [tft018(&inp), tft019(&inp)] {
            match &o.status {
                Status::Skipped(why) => assert!(
                    why.contains("next lap"),
                    "{} must name the artifact it refuses to report: {why}",
                    o.check.id()
                ),
                other => panic!("expected a skip on a replayed stream, got {other:?}"),
            }
        }
        match &tft019(&inp).status {
            Status::Skipped(why) => assert!(
                why.contains("--from-bag"),
                "the skip has to point at the source that can answer: {why}"
            ),
            other => panic!("expected a skip on a replayed stream, got {other:?}"),
        }
        assert_eq!(
            ClockStepEvidence::capture(&snap, &obs).coverage_note(PushStream::RingsUnderWriter),
            None,
            "a note about edges the check did not attribute would describe a run that did not \
             happen"
        );

        inp.stream = PushStream::Observed;
        assert_eq!(tft019(&inp).status, Status::Fired);
    }

    /// **A stream replayed from an arena at rest cannot show an inversion, so
    /// `TFT018` skips there too rather than passing.**
    #[test]
    fn tft018_and_tft019_skip_on_an_arena_at_rest_rather_than_passing_vacuously() {
        const MS: i64 = 1_000_000;
        let snap = chain_with_domains(0, 0);
        let obs = Observations::from_samples(stepped_back(1, 100 * MS));
        let mut inp = inputs(&snap, &obs, &[], Clock::Wall(0));
        inp.stream = PushStream::RingsAtRest;

        for o in [tft018(&inp), tft019(&inp)] {
            match &o.status {
                Status::Skipped(why) => assert!(
                    why.contains("only the pushes the engine accepted")
                        && why.contains("--from-bag"),
                    "{} must say the evidence is absent and where to get it: {why}",
                    o.check.id()
                ),
                other => panic!("expected a skip on an arena at rest, got {other:?}"),
            }
        }

        inp.stream = PushStream::Recorded;
        assert_eq!(tft018(&inp).status, Status::Fired);
        assert_eq!(tft019(&inp).status, Status::Fired);
    }

    /// **`TFT001` skips on a recording because a bag has no publisher identity,
    /// and the reason says which of the two facts is missing.**
    #[test]
    fn tft001_skips_on_a_recording_for_the_recordings_own_reason() {
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let obs = Observations::from_samples(steady(1, 4, 10_000_000));
        let mut inp = inputs(&snap, &obs, &[], Clock::Wall(0));
        inp.stream = PushStream::Recorded;
        match &tft001(&inp).status {
            Status::Skipped(why) => assert!(
                why.contains("no publisher identity") && why.contains("PHASE4.md §1.3"),
                "the reason must be the recording's own, not the ring's: {why}"
            ),
            other => panic!("expected a skip on a recording, got {other:?}"),
        }
        inp.stream = PushStream::Observed;
        assert_eq!(tft001(&inp).status, Status::Pass);
    }

    /// **`TFT019` explains `TFT018`; it does not demote it.**
    #[test]
    fn tft019_explains_tft018_without_demoting_it() {
        const MS: i64 = 1_000_000;
        let snap = chain_with_domains(0, 0);
        let obs = Observations::from_samples(stepped_back(1, 100 * MS));
        let inp = inputs(&snap, &obs, &[], Clock::Wall(0));

        assert_eq!(Tft::Tft019.severity(), crate::catalogue::Severity::Warn);
        assert_eq!(Tft::Tft018.severity(), crate::catalogue::Severity::Error);

        let report = run(&inp, &BTreeSet::new());
        let fired: Vec<&str> = report
            .outcomes
            .iter()
            .filter(|o| o.status == Status::Fired)
            .map(|o| o.check.id())
            .collect();
        assert!(
            fired.contains(&"TFT018") && fired.contains(&"TFT019"),
            "both the detector and its attribution must reach the report: {fired:?}"
        );
        assert!(
            report.has_error(),
            "an explained clock step is still lost data and must still gate --exit-code"
        );

        let only_019 = run(&inp, &BTreeSet::from([Tft::Tft019]));
        assert!(only_019.has_error(), "TFT019 was never what gated");
        let only_018 = run(&inp, &BTreeSet::from([Tft::Tft018]));
        assert!(!only_018.has_error(), "TFT018 is the id that gates");
    }

    /// **`TFT019` fires on exactly `TFT018`'s evidence — the same function, not
    /// a second scan written to the same rule.**
    #[test]
    fn tft019_considers_exactly_the_edges_tft018_fired_on() {
        const MS: i64 = 1_000_000;
        let snap = chain_with_domains(0, 0);
        let repeats: Vec<PushSample> = (0..=CLOCK_STEP_MIN_REJECTED_RUN)
            .map(|_| PushSample {
                edge: 1,
                writer_pid: 4711,
                stamp_ns: 100 * MS,
                arrival_delay_ns: 0,
            })
            .collect();
        assert!(
            repeats.len() > CLOCK_STEP_MIN_REJECTED_RUN,
            "a producer that counted repeats as rejections has to reach the attribution \
             threshold on this fixture, or the drift shows up as a note instead of a firing"
        );
        let obs = Observations::from_samples(repeats);
        let inp = inputs(&snap, &obs, &[], Clock::Wall(0));
        assert_eq!(tft018(&inp).status, Status::Pass);
        assert_eq!(
            tft019(&inp).status,
            Status::Pass,
            "the attribution must read invariant 6's rule from TFT018's producer, not a \
             second copy of it"
        );

        let mut events = stepped_back(1, 100 * MS);
        events.extend(stepped_back(2, 200 * MS));
        let obs = Observations::from_samples(events);
        let inp = inputs(&snap, &obs, &[], Clock::Wall(0));
        let attributed: Vec<u32> = tft019(&inp)
            .findings
            .iter()
            .filter_map(|f| f.edge)
            .collect();
        let detected: Vec<u32> = doctor::out_of_order_runs(inp.obs)
            .iter()
            .map(|r| r.edge)
            .collect();
        assert_eq!(
            attributed, detected,
            "both edges are tag 0, so the two sets coincide"
        );
        assert_eq!(detected, vec![1, 2]);
    }

    /// **`TFT018` skips on a live arena instead of reporting an inversion the
    /// publisher never made.**
    #[test]
    fn tft018_skips_on_a_live_arena_and_says_so() {
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let obs = Observations::from_samples(vec![
            PushSample {
                edge: 1,
                writer_pid: 1,
                stamp_ns: 100,
                arrival_delay_ns: 0,
            },
            PushSample {
                edge: 1,
                writer_pid: 1,
                stamp_ns: 50,
                arrival_delay_ns: 0,
            },
        ]);
        let mut inp = inputs(&snap, &obs, &[], Clock::Wall(0));
        inp.stream = PushStream::RingsUnderWriter;
        match &tft018(&inp).status {
            Status::Skipped(why) => assert!(
                why.contains("next lap"),
                "the skip must name what would have been misread: {why}"
            ),
            other => panic!("expected a skip on a live arena, got {other:?}"),
        }
        inp.stream = PushStream::Observed;
        assert_eq!(tft018(&inp).status, Status::Fired);
    }

    /// **`TFT007` compares only where a rate was declared, and an undeclared
    /// edge is not compared against zero.**
    #[test]
    fn tft007_compares_only_where_a_rate_was_declared() {
        const MS: i64 = 1_000_000;
        let mut on_rate = edge(1, 1, 2, 100);
        on_rate.nominal_rate_mhz = Some(20_000); // 20 Hz
        let mut too_slow = edge(2, 2, 3, 100);
        too_slow.nominal_rate_mhz = Some(20_000);
        let undeclared = edge(3, 3, 4, 100);

        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base", 2, 2),
                frame(4, "laser", 3, 3),
            ],
            edges: vec![on_rate, too_slow, undeclared],
            participants: live_writer(),
        };
        let mut events = steady(1, 12, 50 * MS); // 20 Hz: exactly nominal
        events.extend(steady(2, 12, 100 * MS)); // 10 Hz: half of nominal
        events.extend(steady(3, 12, 100 * MS)); // 10 Hz, but nothing declared
        let obs = Observations::from_samples(events);

        let o = tft007(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(
            o.findings.iter().map(|f| f.edge).collect::<Vec<_>>(),
            vec![Some(2)],
            "only the declared edge that missed its rate may be reported: {:?}",
            o.findings
        );
        assert!(
            o.findings[0].message.contains("10.00 Hz")
                && o.findings[0].message.contains("20.00 Hz")
                && o.findings[0].message.contains("-50%"),
            "the finding must carry both rates and the deviation: {}",
            o.findings[0].message
        );
        assert!(
            o.findings[0].message.contains("see a longer step"),
            "a slow publisher must be told the consequence of *slow*: {}",
            o.findings[0].message
        );

        let obs = Observations::from_samples(steady(1, 12, 20 * MS)); // 50 Hz
        let o = tft007(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.findings.len(), 1, "{o:?}");
        assert!(
            o.findings[0].message.contains("+150%")
                && o.findings[0]
                    .message
                    .contains("retains proportionally less history"),
            "and a fast one the consequence of *fast*: {}",
            o.findings[0].message
        );

        let obs = Observations::from_samples(steady(1, 12, 55_555_555));
        let o = tft007(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Pass, "{o:?}");
    }

    /// **An arena where nothing declares a rate skips `TFT007` with a reason
    /// instead of passing.**
    #[test]
    fn tft007_skips_when_no_edge_declares_a_rate() {
        const MS: i64 = 1_000_000;
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let obs = Observations::from_samples(steady(1, 12, 50 * MS));
        let o = tft007(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("nominal rate") && why.contains("rate_hz"),
                "the skip must name the missing evidence and how to supply it: {why}"
            ),
            other => panic!("expected a skip, got {other:?}"),
        }

        let snap = two_frame_snapshot(EdgeInfo {
            kind: EdgeKind::Static,
            capacity: 0,
            clock_offset_nanos: None,
            nominal_rate_mhz: Some(20_000),
            ..edge(1, 1, 2, 0)
        });
        match &tft007(&inputs(&snap, &obs, &[], Clock::Wall(0))).status {
            Status::Skipped(_) => {}
            other => panic!("a static edge cannot declare a publish rate, got {other:?}"),
        }

        let mut declared = edge(1, 1, 2, 100);
        declared.nominal_rate_mhz = Some(20_000);
        let snap = two_frame_snapshot(declared);
        assert_eq!(
            tft007(&inputs(&snap, &obs, &[], Clock::Wall(0))).status,
            Status::Pass
        );
    }

    /// **`TFT007` skips rather than passing when every declaring edge is
    /// unmeasurable — a `pass` that compared nothing is a fabricated
    /// assurance.**
    #[test]
    fn tft007_skips_rather_than_passing_when_it_compared_nothing() {
        const MS: i64 = 1_000_000;
        let mut short = edge(1, 1, 2, 100);
        short.nominal_rate_mhz = Some(20_000);
        let mut stopped = edge(2, 2, 3, 100);
        stopped.nominal_rate_mhz = Some(20_000);
        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base", 2, 2),
            ],
            edges: vec![short, stopped],
            participants: live_writer(),
        };
        let obs = Observations::from_samples(steady(1, 4, 50 * MS));

        let o = tft007(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("2 edge(s) declare") && why.contains("retained intervals"),
                "the skip must say a declaration exists and that the *stream* is what is \
                 missing, not the declaration: {why}"
            ),
            other => panic!("expected a skip, got {other:?}"),
        }
        assert_eq!(
            rate_coverage_note(&snap, &obs, Clock::Wall(0), PushStream::Observed),
            None,
            "the note stays silent here, which is why the skip has to carry the disclosure"
        );

        let obs = Observations::from_samples(
            steady(1, 12, 50 * MS)
                .into_iter()
                .chain(steady(2, 4, 50 * MS))
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            tft007(&inputs(&snap, &obs, &[], Clock::Wall(0))).status,
            Status::Pass
        );
        assert!(
            rate_coverage_note(&snap, &obs, Clock::Wall(0), PushStream::Observed)
                .expect("a partial run discloses itself")
                .contains("compared 1 of 2")
        );
    }

    /// **`RATE_TOLERANCE` is the band, and one milli-hertz either side of it
    /// decides fired from passed.**
    #[test]
    fn the_rate_tolerance_band_is_pinned_at_its_edge() {
        let obs = Observations::from_samples(steady(1, 12, 40_000_000));
        let outcome = |mhz: u32| {
            let mut e = edge(1, 1, 2, 100);
            e.nominal_rate_mhz = Some(mhz);
            tft007(&inputs(&two_frame_snapshot(e), &obs, &[], Clock::Wall(0))).status
        };
        assert_eq!(
            outcome(20_833),
            Status::Fired,
            "25 Hz against 20.833 Hz is +20.0019%, outside a 20% band"
        );
        assert_eq!(
            outcome(20_834),
            Status::Pass,
            "25 Hz against 20.834 Hz is +19.9962%, inside it"
        );
    }

    /// **A `TFT007` pass says which edges it covered, whenever it covered fewer
    /// than all of them.**
    #[test]
    fn the_rate_coverage_note_states_what_a_pass_did_not_cover() {
        const MS: i64 = 1_000_000;
        let mut declared = edge(1, 1, 2, 100);
        declared.nominal_rate_mhz = Some(20_000);
        let mut short = edge(2, 2, 3, 100);
        short.nominal_rate_mhz = Some(20_000);
        let snap = Snapshot {
            frames: vec![
                frame(1, "map", 0, 0),
                frame(2, "odom", 1, 1),
                frame(3, "base", 2, 2),
                frame(4, "laser", 3, 3),
                frame(5, "imu", 4, 4),
            ],
            edges: vec![
                declared,
                short,
                edge(3, 3, 4, 100),
                EdgeInfo {
                    kind: EdgeKind::Static,
                    capacity: 0,
                    ..edge(4, 4, 5, 0)
                },
            ],
            participants: live_writer(),
        };
        let mut events = steady(1, 12, 50 * MS);
        events.extend(steady(2, 4, 50 * MS));
        let obs = Observations::from_samples(events);

        let note = rate_coverage_note(&snap, &obs, Clock::Wall(0), PushStream::Observed)
            .expect("a partial run must disclose itself");
        assert!(
            note.contains("compared 1 of 3")
                && note.contains("1 declare no nominal rate")
                && note.contains("1 have fewer than 8"),
            "{note}"
        );

        let mut a = edge(1, 1, 2, 100);
        a.nominal_rate_mhz = Some(20_000);
        let full = two_frame_snapshot(a);
        let obs = Observations::from_samples(steady(1, 12, 50 * MS));
        assert_eq!(
            rate_coverage_note(&full, &obs, Clock::Wall(0), PushStream::Observed),
            None
        );

        let none = two_frame_snapshot(edge(1, 1, 2, 100));
        assert_eq!(
            rate_coverage_note(&none, &obs, Clock::Wall(0), PushStream::Observed),
            None
        );
    }

    /// **An arena that has served no lookups must not read as a healthy one.**
    #[test]
    fn an_unexercised_counter_sheet_skips_tft010_rather_than_passing_it() {
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let obs = Observations::new();

        let unexercised = [EdgeStats {
            edge: 1,
            ..EdgeStats::default()
        }];
        let inp = inputs(&snap, &obs, &unexercised, Clock::Wall(0));
        match tft010(&inp).status {
            Status::Skipped(why) => {
                assert!(
                    why.contains("served a lookup"),
                    "the skip must name the reason a reader can act on: {why}"
                );
                assert!(
                    why.contains("read-only consumer cannot record a counter"),
                    "the deployment-shaped cause is the one an operator meets: {why}"
                );
                assert!(
                    why.contains("tf_tree participants"),
                    "a reason without the command that tells rw from ro is not \
                     actionable: {why}"
                );
            }
            other => panic!("an arena nobody has read must not report a verdict: {other:?}"),
        }

        let exercised = [EdgeStats {
            edge: 1,
            lookups_ok: 1,
            ..EdgeStats::default()
        }];
        let inp = inputs(&snap, &obs, &exercised, Clock::Wall(0));
        assert_eq!(
            tft010(&inp).status,
            Status::Pass,
            "one lookup is enough to make a zero error count a real result"
        );
    }

    /// **`TFT011` skips only when *both* of its halves are blind.**
    #[test]
    fn tft011_skips_when_neither_half_has_evidence_and_runs_when_either_does() {
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let obs = Observations::new();
        let unexercised = [EdgeStats {
            edge: 1,
            ..EdgeStats::default()
        }];

        let mut inp = inputs(&snap, &obs, &unexercised, Clock::Wall(0));
        inp.stream = PushStream::Recorded;
        match tft011(&inp).status {
            Status::Skipped(why) => {
                assert!(
                    why.contains("served a lookup"),
                    "the counter half's reason is missing: {why}"
                );
                assert!(
                    why.contains("recorder's clock"),
                    "the capacity-vs-latency half's reason is missing: {why}"
                );
            }
            other => panic!("neither half had evidence and it still reported: {other:?}"),
        }

        let mut inp = inputs(&snap, &obs, &unexercised, Clock::Wall(0));
        inp.stream = PushStream::Observed;
        assert_eq!(tft011(&inp).status, Status::Pass);

        let exercised = [EdgeStats {
            edge: 1,
            lookups_ok: 1,
            ..EdgeStats::default()
        }];
        let mut inp = inputs(&snap, &obs, &exercised, Clock::Wall(0));
        inp.stream = PushStream::RingsUnderWriter;
        assert_eq!(tft011(&inp).status, Status::Pass);
    }

    /// **A build without `counters` keeps its own reason.**
    #[test]
    fn the_counters_feature_and_an_unexercised_arena_are_different_skips() {
        let off = no_counter_evidence(false, &[]).expect("a build without counters has no verdict");
        assert!(off.contains("`counters` feature"), "{off}");

        let unexercised = [EdgeStats {
            edge: 1,
            ..EdgeStats::default()
        }];
        let on = no_counter_evidence(true, &unexercised).expect("zero counters carry no verdict");
        assert!(on.contains("served a lookup"), "{on}");

        let failed = [EdgeStats {
            edge: 1,
            extrap_before: 1,
            ..EdgeStats::default()
        }];
        assert_eq!(no_counter_evidence(true, &failed), None);
    }
}
