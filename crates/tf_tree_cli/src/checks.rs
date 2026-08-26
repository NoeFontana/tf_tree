//! Detection for the diagnostics catalogue — `docs/PHASE5.md` §6.
//!
//! [`crate::catalogue`] owns the identifiers and the printing; this module
//! decides what fired. Every check is a pure function over captured data
//! ([`EdgeStats`], [`crate::doctor::Snapshot`], [`crate::doctor::Observations`],
//! [`crate::hostfacts::HostFacts`]) so it can be tested by constructing the
//! offending state directly — including states a live arena cannot reach on its
//! own.
//!
//! # Which checks this build cannot perform, and why each one is *skipped*
//! rather than passed
//!
//! Several ids report [`crate::catalogue::Status::Skipped`] with a stated
//! reason — **three** unconditionally, and **eight** more depending on what the
//! arena, the engine build and the host can supply. A check that silently returns
//! nothing is indistinguishable from one that found nothing, and those two
//! answers mean opposite things to whoever is reading:
//!
//! * **`TFT002`** (static republished with a different value) and **`TFT003`**
//!   (edge kind changed) are detected by [`tf_tree_bridge`]'s `StaticStore`,
//!   which counts exactly these two conditions as it ingests. Wiring `doctor` to
//!   it is not a dependency-cycle problem — `tf_tree_bridge` has *no*
//!   dependencies at all — it is a locality problem: a `StaticStore` lives in
//!   the bridge process's heap, the arena stores neither a static edge's
//!   publication history nor its declared kind over time, and `doctor` is a
//!   different process. Surfacing them needs the bridge to publish its counters
//!   into the arena, which `docs/PHASE5.md` §1.2 does not reserve space for.
//! * **`TFT004`** (clock skew) needs a per-publisher *arena receipt time* to
//!   difference against the header stamp. **One is now recorded** — `tf_tree`'s
//!   `EdgeWriter` samples a wall clock into `ClaimRecord::last_push_nanos` once
//!   per second of published data where the edge declares a nominal rate, and
//!   once per 1024 pushes where it does not — 102 s at 10 Hz, and a tree built
//!   without a topology file is the common case, so this check must not assume
//!   the per-second figure
//!   ([`0036`](../../../docs/decisions/0036-the-receipt-time-the-format-already-reserved.md)
//!   step 1) — and **this check is not yet wired to it** (that record's step 3).
//!   The skip is therefore about this file and no longer about the arena, which
//!   is a smaller claim than the one that stood here: it used to say nothing
//!   recorded a receipt time, which was true of `SampleRing::push` and left a
//!   reader to conclude the field did not exist. It did, zeroed, in every
//!   shipped arena.
//!
//! And the conditional ones, which depend on what the arena, the engine build
//! and the host can supply:
//!
//! * **`TFT001`** (multi-publisher conflict) is skipped wherever the push
//!   stream carries no writer identity, which is everywhere but the fixture: a
//!   ring remembers the current claim owner and not the sequence of processes
//!   that wrote into it, and a recording's `/tf` messages are anonymous. See
//!   [`PushStream`].
//! * **`TFT005`** (stamps in the future) is skipped when the arena's stamps do
//!   not share an epoch with the system clock — see [`Clock`].
//! * **`TFT007`** (rate deviates from nominal) is skipped when **no** edge in
//!   the arena declares a nominal rate. It is no longer structurally blind:
//!   `EdgeRecord::nominal_rate_mhz` is written at declaration time from
//!   `EdgeCfg::nominal_rate_hz`, and a topology file's `rate_hz` reaches it
//!   through `TopologyConfig::builder`. An arena built without one still skips,
//!   because a `0` means *undeclared* and not *0 Hz* — see `tft007`.
//! * **`TFT010`** is skipped whenever the `docs/PHASE5.md` §5 counters carry no
//!   verdict — see [`no_counter_evidence`], which is *two* conditions: an engine
//!   built without the feature, and an arena that has served **no lookups**. The
//!   second is the one a recording always meets and the reference fixture meets
//!   too: those counters are incremented by lookups, and an arena nobody has
//!   read reads exactly like a healthy one.
//! * **`TFT011`** reports two independent pieces of evidence under one id — the
//!   counters, and `capacity x period` against a per-sample arrival delay — and
//!   skips only when *both* are blind, which is what a recording is. Where one
//!   half survives it runs, and `evidence_notes` discloses the other.
//! * **`TFT016`** is skipped when the host is not Linux.
//! * **`TFT018`** (out-of-order stamps) is skipped wherever the push stream was
//!   replayed from an arena's rings rather than recorded as it arrived, and the
//!   two ways that happens fail it differently — see [`PushStream`]. It runs on
//!   the fixture and on a recording (`doctor --from-bag`).
//! * **`TFT019`** inherits exactly that, since it is `TFT018`'s evidence, and is
//!   skipped in addition when the edges that *did* go backwards are in no
//!   wall-clock domain, naming their tags. It is an attribution rather than a
//!   detector, so it can neither run without `TFT018` nor guess about a tag
//!   `Domain`'s open trait let somebody else define.
//!
//! [`tf_tree_bridge`]: https://docs.rs/tf_tree_bridge

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use tf_tree::unstable::EdgeKind;
use tf_tree::{Domain, EdgeId, SensorDomain, SimDomain, SteadyDomain, SystemDomain, Tree};
use tf_tree_bench::fixture::PushSample;

use crate::catalogue::{CheckOutcome, Finding, Report, Tft};
use crate::doctor::{
    self, LockByte, Observations, ParticipantInfo, RecordedProcess, SlotState, Snapshot,
};
use crate::hostfacts::{HostFacts, MemLock, ShmemThp, Thp};

/// A stamp further than this from the reference clock is not a late sample, it
/// is a units error or uninitialised memory.
///
/// One year: long enough that no robot's legitimate history reaches it, short
/// enough to catch the classic fault of pushing nanoseconds into a field the
/// publisher believed was seconds (a 10<sup>9</sup>× overshoot).
const ABSURD_HORIZON_NS: i64 = 365 * 24 * 3600 * 1_000_000_000;

/// How far ahead of the wall clock a stamp may sit before `TFT005` fires.
///
/// Not zero: a publisher stamping with its own clock a few hundred microseconds
/// before the sample reaches the arena is normal, and a check that fires on
/// every healthy system is a check people turn off.
const FUTURE_TOLERANCE_NS: i64 = 50_000_000;

/// Fraction of lookups that may fail with an extrapolation error before
/// `TFT010` calls the edge a hotspot.
const EXTRAP_HOTSPOT_RATE: f64 = 0.01;

/// An interval this many times the edge's median counts as a dropout (`TFT009`).
const GAP_FACTOR: i64 = 3;

/// Occupancy above this fraction of a table's capacity fires `TFT015`.
///
/// `pub(crate)` because `tf_tree top` colours the same row on the same rule; a
/// second copy of the literal there disagreed with this one at exactly 80.0 %.
pub(crate) const OCCUPANCY_LIMIT: f64 = 0.80;

/// Where the reference clock for the time-based checks came from.
///
/// The distinction is load-bearing rather than cosmetic. `EdgeRecord::domain`
/// (D9) lets an arena carry stamps in *any* time domain — seconds since boot, a
/// simulator's clock, a bag's original timeline — so "this stamp is in the
/// future" is only a meaningful sentence when the arena's stamps and the
/// system's clock share an epoch. Assuming they do would make `doctor` report
/// every edge of a monotonic-clock arena as decades stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clock {
    /// The arena's newest stamp is within `ABSURD_HORIZON_NS` of the system
    /// clock, so the two share an epoch and wall-clock comparisons mean
    /// something.
    Wall(i64),
    /// They do not. The **median** of the per-edge newest stamps is used as
    /// "now", which still supports `TFT006`'s *distance* rule but cannot
    /// support `TFT005` at all.
    NewestStamp(i64),
}

impl Clock {
    /// The reference instant, whichever source it came from.
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

    /// Decide which clock applies, given **every** edge's newest stamp and the
    /// system clock.
    ///
    /// # Why this is a vote and not a single stamp
    ///
    /// `ABSURD_HORIZON_NS` does double duty: it is the domain-agreement
    /// threshold here *and* `TFT006`'s absurdity radius. So whatever this
    /// function picks as the reference is, by construction, the one value
    /// `TFT006` can never call absurd. Deriving it from an extremum — the
    /// arena's maximum stamp, say — hands that immunity to the single worst
    /// edge: one publisher with the classic nanoseconds-into-a-seconds-field
    /// overshoot *defines* the clock, `TFT005` skips itself because the arena
    /// now looks non-Unix, and `TFT006` fires on every **correct** edge for
    /// being far from the broken one. The check written for that exact fault
    /// exonerates it and blames the healthy majority.
    ///
    /// The estimator therefore has to have a breakdown point. Two parts:
    ///
    /// * **The domain is decided by majority.** Count the edges whose newest
    ///   stamp sits within the horizon of the system clock. A *minority* of bad
    ///   stamps can then never flip the arena out of `Wall`, which is what keeps
    ///   `TFT005` and `TFT006` pointed at the outlier instead of at everyone
    ///   else.
    /// * **Ties go to the wall clock.** At 50% contamination no estimator drawn
    ///   from the arena can tell the good half from the bad half, so the
    ///   tiebreak uses the one piece of evidence no edge can corrupt: the
    ///   external clock.
    ///
    /// When the vote says the stamps genuinely are not Unix time, the fallback
    /// reference is the **median** of the per-edge newest stamps, not the
    /// maximum, for the same reason: `TFT006` still measures distance against
    /// it, and a centre one edge can drag is a centre that check cannot use.
    #[must_use]
    pub fn decide(newest_stamps: &[i64], system_unix_nanos: i64) -> Clock {
        // The distance is taken in `i128`. A stamp is arbitrary data an arbitrary
        // publisher wrote — one near `i64::MIN` against a Unix `now` overflows
        // `i64` on the subtraction, and `.abs()` overflows again on `i64::MIN`
        // itself. Either is a panic inside `doctor` and inside `top`'s redraw
        // loop, on exactly the corrupt stamp both tools exist to report.
        let horizon = i128::from(ABSURD_HORIZON_NS);
        let now = i128::from(system_unix_nanos);
        let agree = newest_stamps
            .iter()
            .filter(|&&n| (i128::from(n) - now).abs() <= horizon)
            .count();
        // `>=` rather than `>`, and it also covers the empty arena: nothing has
        // disagreed with the system clock, so the system clock stands.
        if agree * 2 >= newest_stamps.len() {
            return Clock::Wall(system_unix_nanos);
        }
        let mut sorted = newest_stamps.to_vec();
        sorted.sort_unstable();
        // Non-empty: `agree * 2 >= 0` would have returned above otherwise.
        Clock::NewestStamp(sorted[sorted.len() / 2])
    }
}

/// Per-edge facts gathered in one pass over the arena: the counter regions
/// (`docs/PHASE5.md` §5) plus the shape of the ring's retained window.
///
/// Captured rather than read live so the checks are pure functions and can be
/// tested against a hand-built state.
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

    // Build the blame map once: for every participant slot, which edge it most
    // recently failed on. Doing it per edge would be O(edges x slots).
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
        // `u32::MAX` is the "no edge" sentinel; edge 0 is the table's sentinel
        // slot and is never handed out, so neither names a real edge.
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
            // `head - capacity` is the slot currently being overwritten, not a
            // retained sample, which is why this is `retained()` and not
            // `capacity()`.
            let retained = ring.retained().min(head);
            for i in (head - retained)..head {
                let s = ring.stamps[(i & ring.mask) as usize].load(Ordering::Relaxed);
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
///
/// # This replaced a `live: bool`, and the bool was keying on the wrong fact
///
/// `TFT001`, `TFT018` and `TFT019` used to skip *iff the arena was live*. That
/// happened to be right while `doctor` had two sources, and it stops being right
/// the moment a third one exists, because "live" is not what any of the three
/// checks actually needs. What they need is a property of the **stream**:
///
/// * `TFT001` needs a writer identity per sample.
/// * `TFT018` needs the arrivals invariant 6 *rejected*.
/// * `TFT011`'s Phase 1 half needs a per-sample arrival delay.
///
/// Keyed on liveness, a frozen `.tft` would have run `TFT018` and passed it
/// **unconditionally** — see [`PushStream::RingsAtRest`] — which is the
/// fabricated all-clear the whole catalogue is written to refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushStream {
    /// Every attempted push, recorded as it happened, with the writer that made
    /// it and the delay it arrived with. Only `tf_tree_bench::fixture` has this:
    /// it is the publisher, so it can record the pushes the engine refused.
    Observed,
    /// Every transform a recording holds, replayed in the recording's own log
    /// order (`doctor --from-bag`).
    ///
    /// Carries rejected arrivals — a bag is written in log order, so a stamp
    /// that went backwards is *in the file* in the position it arrived at — and
    /// carries neither a writer nor an arrival delay: a `/tf` message has no
    /// publisher identity in it, and the recorder's log time is a different
    /// clock from the publisher's stamp.
    Recorded,
    /// Reconstructed from an arena's rings with no writer attached: a frozen
    /// `.tft` (`doctor --from-file`), or any other arena at rest.
    ///
    /// **This variant cannot show an inversion, and that is why `TFT018` skips
    /// on it rather than passing.** `SampleRing::push` refuses a stamp older
    /// than the ring's last, so a ring holds only *accepted* pushes; the
    /// rejected arrival that `TFT018` exists to name left no trace in the arena
    /// at all. Reading the window is exact here — nobody is writing — so the
    /// result would be a guaranteed `pass`, which is worth strictly less than
    /// saying why there is no answer.
    RingsAtRest,
    /// Reconstructed from the rings of an arena being written while it is read
    /// (`doctor --attach`).
    ///
    /// Everything [`PushStream::RingsAtRest`] cannot supply, plus one it gets
    /// *wrong*: the oldest slot of the retained window is the one being
    /// overwritten, so a sample from the next lap can appear at the old end and
    /// read as an inversion on a perfectly ordered publisher.
    RingsUnderWriter,
}

impl PushStream {
    /// Why this stream cannot name the process that pushed a sample (`TFT001`),
    /// or `None` when it can.
    ///
    /// **The predicate and the reason are one function on purpose.** Two of them
    /// is how a check ends up skipping for a reason that stopped being true, or
    /// running with evidence it does not have; here a new variant cannot compile
    /// without answering both at once.
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
///
/// # A zero counter has two meanings and they are opposites
///
/// `EdgeCounters` are incremented by *lookups*. An arena nobody has looked
/// anything up in therefore reads exactly like a perfectly healthy one: zero
/// extrapolation errors, zero of everything. `TFT010` seeing that clean sheet
/// and reporting `pass` is a **fabricated all-clear** — the same shape as
/// `TFT007` comparing an observed rate against an undeclared zero, which §6's
/// amendment already had to correct once.
///
/// It is not a property of `--from-bag`. A bag-built arena is merely the source
/// where it is *guaranteed*: `tf_tree_ingest::run` pushes and never reads, so it
/// hands back an arena that has served no lookups at all. The reference fixture
/// does the same (`fixture::spin_up` publishes; nothing calls `Plan::at`), and a
/// live arena at bringup, before its first consumer, is in the same state. So
/// the predicate is read off **the evidence itself** rather than off the source
/// — which is what keeps a fifth `Source` from silently reintroducing the bug,
/// and what makes the answer right for a live arena nobody is reading yet.
///
/// # The threshold is "any lookup at all", not "enough lookups"
///
/// `lookups_ok + err_extrap_before + err_extrap_after` is incremented once per
/// lookup that touched the edge, whichever way it went, so their sum over the
/// arena is the number of lookups the counters have seen. One is enough to make
/// a zero mean *zero* rather than *unknown*; asking for more would be this
/// module inventing a significance threshold the spec does not state.
///
/// `counters` is [`tf_tree::counters_compiled_in`]. It comes first and keeps its
/// own reason: a build with the feature off also reads zero everywhere, and
/// "rebuild the engine" and "exercise the arena" are different instructions.
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
            "this arena has served no lookups — every EdgeCounter reads zero — so the counters \
             cannot distinguish a healthy arena from an unexercised one, and a pass here would \
             be an all-clear about nothing. An arena built from a recording (--from-bag, \
             tf_tree ingest, tf_tree freeze) is written and never read, so it is always in this \
             state; a live arena reaches it before its first consumer",
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
    ///
    /// A field rather than a call inside `tft019` because
    /// [`ClockStepEvidence::coverage_note`] needs the same split for
    /// `Meta.notes`, and the two must be the same split rather than two walks
    /// that happen to agree.
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
///
/// The sibling of [`PushStream`], and it exists for the same reason: the
/// question "is this participant running?" has an answer for some sources and
/// none for others, and the difference has to be a value the check reads rather
/// than a fact the caller remembers to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotTable {
    /// The table of an arena that exists now — a live shared segment
    /// (`doctor --attach`), or one this process built and still holds (the
    /// fixture, `doctor --from-bag`). Its records name processes that could
    /// still be running, so asking whether they are is a question with an
    /// answer.
    Current,
    /// A byte copy of a table as it stood at some past instant
    /// (`doctor --from-file`).
    Image,
}

impl SlotTable {
    /// Why this table cannot say whether a participant is running, or `None`
    /// when it can.
    ///
    /// One function for the predicate and the sentence, exactly as
    /// [`PushStream::no_writer_identity`] is: a new variant cannot compile
    /// without answering both at once.
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
///
/// `suppress` names ids removed from the `--exit-code` gate; they still run and
/// still print.
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
            Tft::Tft004 => CheckOutcome::skipped(
                check,
                "a per-publisher arena receipt time is now recorded (ClaimRecord::last_push_nanos, \
                 sampled once per second of published data on an edge that declares a rate and \
                 once per 1024 pushes on one that does not) but this check does not yet read it",
            ),
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
        // Empty, and the field stays: `uncatalogued` is part of the stable
        // `--json` schema, and it is the shape a future check with no id would
        // take. `docs/PHASE5.md` §6's amendment gave the last two occupants
        // `TFT017`/`TFT018`.
        uncatalogued: Vec::new(),
    }
}

/// `TFT001` — more than one writer pid on one edge.
///
/// # It needs a writer identity per sample, and only the fixture has one
///
/// `docs/PHASE4.md` §1.3 predicts that real ROS stacks have two nodes publishing
/// one edge and that `tf2` averages them silently, which makes this the check a
/// stranger's recording is most wanted for — and the recording is exactly where
/// it cannot run. A `tf2_msgs/TFMessage` carries no publisher identity; every
/// message on `/tf` is anonymous by the time a recorder writes it, and MCAP's
/// channel is the *topic*, not the node. So the skip reason names the missing
/// evidence rather than the source, because the source is not the problem.
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
///
/// Two rules, and the second is narrower than §6's title on purpose. A negative
/// stamp is invalid in every time domain. A stamp of exactly zero is only
/// invalid when the arena's stamps are Unix time, where it means 1970 — under
/// any other domain zero is a legitimate origin, and the benchmark fixture
/// publishes it. Flagging it unconditionally would make `doctor` fail on a
/// correct arena, which is the one thing a gate must never do.
fn tft006(inp: &Inputs<'_>) -> CheckOutcome {
    let now = inp.clock.nanos();
    let wall = matches!(inp.clock, Clock::Wall(_));
    let index = inp.snap.edge_index();
    let mut out = Vec::new();
    for st in inp.stats {
        let Some(e) = index.get(&st.edge) else {
            continue;
        };
        // Reasons first, label second. `edge_label` formats two linear scans of
        // `snap.frames` into a fresh `String`, and computing it before any
        // predicate paid that on every edge of a completely clean arena.
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
        // The distance rule catches the units error a range check cannot: a
        // publisher writing nanoseconds into a field it believed was seconds is
        // off by a factor of a billion, which is far outside any horizon.
        //
        // Both ends are checked because only one of them may be wrong — a
        // single garbage stamp among good ones moves exactly one end. They are
        // deduplicated so an edge holding one sample does not report twice.
        let mut ends = vec![("newest", st.newest_stamp)];
        if st.oldest_stamp != st.newest_stamp {
            ends.push(("oldest", st.oldest_stamp));
        }
        for (what, stamp) in ends {
            let Some(s) = stamp else { continue };
            // `i128`, like `Clock::decide`: the stamp is whatever a publisher
            // wrote, and `s - now` for `s` near `i64::MIN` is a panic in the
            // check written to report exactly that stamp.
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
///
/// Three-valued, and the three are not interchangeable: only the last supports
/// a comparison, and the first two are the reasons [`rate_coverage_note`] can
/// state what a `pass` did **not** cover. Both consumers match on this one
/// enum so they cannot drift into disagreeing about which edges were checked.
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
///
/// 20%, and both directions matter for different reasons. Slow is the obvious
/// fault. Fast is the quiet one: a ring sized from `rate_hz * history_secs`
/// holds proportionally less history than the operator asked for, so an edge
/// declared at 50 Hz and running at 100 Hz retains half the window every
/// consumer was tuned against.
///
/// Not tighter, because the observed side is a **median** interval over a
/// finite window: scheduler jitter moves it far less than it moves a mean, but
/// a partly-filled ring and a rate rounded to two decimals by
/// `tf_tree topology --discover` both put a few percent in. A check that fires
/// on a healthy robot is a check somebody adds to `--suppress` permanently,
/// and then it detects nothing at all.
const RATE_TOLERANCE: f64 = 0.20;

/// Intervals needed before an observed rate is worth comparing to a declared
/// one. Eight, so a single hiccup cannot carry the median.
const RATE_MIN_INTERVALS: usize = 8;

/// What one edge can tell `TFT007`.
fn rate_evidence(e: &doctor::EdgeInfo, samples: Option<&[&PushSample]>) -> RateEvidence {
    // A static edge carries its pose inline and never publishes, so it has no
    // rate to hold or miss. The builder writes no nominal for one; this guard is
    // what keeps a hand-built or corrupt record with a stray non-zero rate from
    // being counted as declared — which would suppress the whole-check skip and
    // then compare a stream that does not exist.
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
    // `None` for a non-positive median, which is what a stream of identical or
    // backwards stamps produces — both would divide into an infinite or
    // negative observed rate, and comparing either against a nominal produces a
    // finding about arithmetic rather than about the robot.
    let Some(observed_hz) = doctor::observed_rate_hz(samples) else {
        return too_few;
    };
    RateEvidence::Comparable {
        declared_hz: f64::from(mhz) / 1000.0,
        observed_hz,
    }
}

/// `TFT007` — the observed publish rate is far from the declared nominal.
///
/// # Where the declared rate comes from
///
/// `EdgeRecord::nominal_rate_mhz` (`docs/PHASE5.md` §1.2), written at
/// declaration time from `EdgeCfg::nominal_rate_hz`. The path an operator
/// actually uses is a topology file's `rate_hz` — `tf_tree topology --discover`
/// writes one, `TopologyConfig::builder` carries it into the arena, and the
/// ROS 2 bridge builds from exactly that. An edge sized by `capacity` declares
/// nothing and is not compared.
///
/// # Why the whole check skips when it compared nothing
///
/// A `Pass` from this check is an active claim: *every edge that declared a
/// rate is publishing at it*. That claim needs at least one compared edge, and
/// there are two ways to have none. Nothing declared — a zero is "not
/// declared", not "declared as 0 Hz", and comparing an observed rate against it
/// makes every edge deviate by infinity, which is a fabricated finding on a
/// correct arena. Or edges declared and none of them retained enough intervals
/// to measure: `doctor` run seconds after bringup, during a publisher restart,
/// or on an edge whose publisher has stopped entirely (no samples at all).
/// **Both skip**, with a reason naming which one it was, because a `Pass`
/// earned by the second is exactly the fabricated assurance the first case is
/// guarded against.
///
/// When *some* edges are comparable and others are not, the check runs on those
/// and [`rate_coverage_note`] states what it did not cover, because a bare
/// `pass` would otherwise read as "every edge publishes at its intended rate".
fn tft007(inp: &Inputs<'_>) -> CheckOutcome {
    let by_edge = inp.obs.by_edge();
    let mut out = Vec::new();
    let mut declared = 0usize;
    let mut comparable = 0usize;
    for e in &inp.snap.edges {
        match rate_evidence(e, by_edge.get(&e.id).map(Vec::as_slice)) {
            RateEvidence::NotDeclared => continue,
            RateEvidence::TooFewIntervals => declared += 1,
            RateEvidence::Comparable {
                declared_hz,
                observed_hz,
            } => {
                declared += 1;
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
    // Not `declared == 0`: that guard leaves a hole between itself and
    // `rate_coverage_note`, which also says nothing when it compared nothing.
    // An arena whose declaring edges all fell short of `RATE_MIN_INTERVALS`
    // would satisfy both and report a bare `pass` having compared no edge at
    // all — no finding, no skip reason, and no note in `--json` either.
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
                    "{declared} edge(s) declare a nominal rate, but none has the \
                     {RATE_MIN_INTERVALS} retained intervals needed to measure an observed one \
                     against it; the publishers may not have started, may have stopped, or the \
                     arena was read too soon after bringup"
                )
            },
        );
    }
    CheckOutcome::ran(Tft::Tft007, out)
}

/// The disclosure that pairs with `tft007`: which edges its result covers.
///
/// `None` when the answer is unambiguous — nothing was compared (the check
/// skipped and says so itself, naming which of its two gaps it hit), or every
/// dynamic edge was declared and measurable. A note is emitted only for the
/// middle case, where `pass` is true of the edges that were compared and silent
/// about the rest.
#[must_use]
pub fn rate_coverage_note(snap: &Snapshot, obs: &Observations) -> Option<String> {
    let by_edge = obs.by_edge();
    let (mut comparable, mut too_few, mut undeclared) = (0usize, 0usize, 0usize);
    for e in &snap.edges {
        if e.kind != EdgeKind::Dynamic {
            continue;
        }
        match rate_evidence(e, by_edge.get(&e.id).map(Vec::as_slice)) {
            RateEvidence::NotDeclared => undeclared += 1,
            RateEvidence::TooFewIntervals => too_few += 1,
            RateEvidence::Comparable { .. } => comparable += 1,
        }
    }
    if comparable == 0 || (undeclared == 0 && too_few == 0) {
        return None;
    }
    Some(format!(
        "TFT007 compared {comparable} of {} dynamic edge(s): {undeclared} declare no nominal \
         rate (no rate_hz in the topology) and {too_few} have fewer than {RATE_MIN_INTERVALS} \
         retained intervals to measure one from",
        comparable + too_few + undeclared
    ))
}

/// `TFT008` — inter-arrival spread. The Phase 1 `inconsistent-rate` check: a
/// coefficient of variation *is* the jitter of the inter-arrival distribution
/// about its own centre. Distinct from [`tft007`] and not redundant with it: a
/// publisher can hold a perfectly steady period at the wrong rate (`TFT007`
/// fires, this passes) or average its declared rate while alternating 1 ms and
/// 100 ms gaps (this fires, `TFT007` passes). This one needs no declaration and
/// therefore runs on every arena.
fn tft008(inp: &Inputs<'_>) -> CheckOutcome {
    CheckOutcome::ran(
        Tft::Tft008,
        doctor::check_inconsistent_rates(inp.obs)
            .into_iter()
            .map(|f| Finding::about(Tft::Tft008, "edge", f.message))
            .collect(),
    )
}

/// `TFT009` — an inter-arrival interval far above the edge's own median.
///
/// Relative to the edge's own median rather than to a fixed threshold because a
/// 200 ms gap is a dropout at 100 Hz and normal at 5 Hz. Deliberately still not
/// against the declared rate [`tft007`] now has: an edge running at half its
/// nominal has no dropouts, and reporting one for every interval would bury the
/// gaps this check exists to find under a rate deviation `TFT007` already
/// reports once.
fn tft009(inp: &Inputs<'_>) -> CheckOutcome {
    let mut out = Vec::new();
    for (edge, samples) in inp.obs.by_edge() {
        let mut intervals: Vec<i64> = samples
            .windows(2)
            .map(|w| w[1].stamp_ns - w[0].stamp_ns)
            .collect();
        if intervals.len() < 4 {
            continue;
        }
        // **Any** negative interval disqualifies the edge, not just a
        // non-positive median. A stream with a handful of inverted pairs keeps
        // a healthy positive median, but the jump back to the true timeline
        // after an inversion becomes `worst` — so TFT009 reports a dropout of
        // N x the median that never happened. The real fault is the id-less
        // `out-of-order` check, which fires on the same stream at error
        // severity; adding a warn about a phantom gap next to it sends the
        // operator looking for a lost publisher instead of a reordered one.
        //
        // Skipping loses nothing: an interval sequence that is not monotone has
        // no meaningful inter-arrival distribution to measure a gap against.
        if intervals.iter().any(|&d| d < 0) {
            continue;
        }
        let worst = intervals.iter().copied().max().unwrap_or(0);
        intervals.sort_unstable();
        let median = intervals[intervals.len() / 2];
        // A zero median (every retained stamp identical) would divide by zero
        // in the ratio and make every non-zero interval an infinite gap.
        if median <= 0 {
            continue;
        }
        if worst > median.saturating_mul(GAP_FACTOR) {
            out.push(Finding::on_edge(
                Tft::Tft009,
                edge,
                format!("edge#{edge}"),
                format!(
                    "largest gap {:.1} ms is {:.1}x the median period {:.1} ms",
                    worst as f64 / 1e6,
                    worst as f64 / median as f64,
                    median as f64 / 1e6
                ),
            ));
        }
    }
    CheckOutcome::ran(Tft::Tft009, out)
}

/// `TFT010` — an edge whose consumers keep asking outside its window.
///
/// Its evidence is *entirely* the §5 counters, so it skips whenever those carry
/// no verdict — see [`no_counter_evidence`], which covers both a build without
/// the feature and an arena nobody has looked anything up in.
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
///
/// Two independent pieces of evidence, both reported under this id:
///
/// 1. `worst_extrap_gap_ns` against the ring's **actual retained span**. This is
///    gated on `extrap_before > 0`, and that gate is the whole correctness of
///    the check: the high-water mark covers the distance past *either* end of
///    the window, and a request past the *newest* end means the publisher is
///    behind, which no amount of extra capacity fixes. Reporting that as an
///    undersized ring would send an operator to enlarge a buffer that is
///    already big enough.
/// 2. The Phase 1 `short-buffer` finding — `capacity x median period` against
///    the largest observed publish latency — which needs a per-sample arrival
///    delay and therefore only fires on the fixture
///    ([`PushStream::no_arrival_delays`]).
///
/// # It skips only when *both* halves are blind, and that is the whole rule
///
/// Either half alone is a real result, so losing one is a disclosure
/// ([`crate::evidence_notes`]) and not a skip. Losing both is not: an arena that
/// has served no lookups and a stream with no arrival delays leave this function
/// walking two empty sets and returning `pass`, which says "your rings are big
/// enough" on evidence that could not have said otherwise. That is the
/// fabricated all-clear [`no_counter_evidence`] exists to refuse, and a bag
/// source hits it on both halves at once.
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

/// `TFT012` — the topology walk does not reach everything. Both Phase 1
/// topology checks land here: a parent cycle and an unattached island are the
/// same fault seen from two directions, and §6 gives them one id.
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

/// `TFT013` — an edge declared and never published to.
///
/// Dynamic edges only. A static edge carries its pose inline in the record and
/// never pushes, so its `head` is 0 for the whole life of a correct arena.
fn tft013(inp: &Inputs<'_>) -> CheckOutcome {
    let mut out = Vec::new();
    for e in &inp.snap.edges {
        if e.kind == EdgeKind::Dynamic && e.head == 0 {
            out.push(Finding::on_edge(
                Tft::Tft013,
                e.id,
                inp.snap.edge_label(e),
                "declared as dynamic but head is 0 — nothing has ever been published to it",
            ));
        }
    }
    CheckOutcome::ran(Tft::Tft013, out)
}

/// Which of `TFT014`'s two participant leaks a slot is, if either.
///
/// `docs/decisions/0028` plan step 6. **Two findings and not one**, because the
/// responses differ and an operator sent after the wrong one at 3am has been
/// actively misled: (a) is a slot nothing will ever reassign, and (b) is a slot
/// a *live* open file description is holding on behalf of a process that no
/// longer exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotLeak {
    /// **(a)** A record that is not `FREE` over a lock byte nobody holds.
    ///
    /// `docs/decisions/0028`'s *The ordering* table: on every path that has a
    /// byte, the byte is taken *before* the arena record leaves `FREE`, so
    /// "free byte over a non-`FREE` record" is the leak and nothing else
    /// produces it.
    Abandoned,
    /// **(b)** The byte *is* held, and `/proc` says the process the identity
    /// record names is gone.
    ///
    /// **Judged without reference to the arena record, and it has to be.** The
    /// two facts are the lock file's — a held byte and the identity beside it —
    /// so this fires on a `FREE` arena row as readily as on a `LIVE` one, which
    /// is what makes the *read-only* participant's inheritor visible. That one
    /// writes no arena record at all (D18) and is the shape a `fork`ed Python
    /// worker leaves behind, so a variant that needed a record would miss the
    /// case it is mostly for.
    ///
    /// So what holds it is a descriptor that outlived its process, which on
    /// Linux means a `fork` inheritor: `docs/PHASE2.md` §6.2 is NORMATIVE that
    /// an OFD lock belongs to the open file description and that `fork` shares
    /// it. **Not reclaimable, deliberately** — the kernel's answer is *alive*,
    /// and overruling it with a `/proc` inference is the inversion §5.1 exists
    /// to forbid. `docs/decisions/0030` closes it at the source, by having the
    /// atfork handler close what it inherited; until then this is a documented
    /// limitation *with a detection*, which is the whole reason this variant
    /// exists.
    ForkInheritor,
}

/// Whether participant slot `p` is one of the two leaks above.
///
/// # One predicate, and the read order it depends on
///
/// The `state` word is consulted **first**, and it answers only *is there a
/// record here* — never *is its process alive*, which is the split
/// `docs/PHASE2.md` §5.1 draws and the reason `identity()` in the assigner was
/// a bug. The byte is consulted second. That order is a correctness constraint
/// rather than a style: `tf_tree`'s `reclamation_verdict` (`0028` piece 2, third
/// constraint) states the argument in full — under word-then-byte the `Acquire`
/// load of a live word synchronises-with `fill_slot`'s publishing `Release`
/// store, so a byte probe sequenced after it must see the byte held, and
/// reversing the two erases a published record under `loom` in 0.00 s.
///
/// This function is the *reporting* half and does not re-read either fact: the
/// word was read by [`Snapshot::capture`] and the byte by
/// [`Snapshot::probe_lock_facts`], whose signature is what makes the order
/// unhoistable — it hands the probe the captured row, so there is nothing to
/// compute before the capture. The argument that the order *matters* is
/// `loom`'s, in `tf_tree`; the argument that this program obeys it is that
/// signature.
///
/// # Every arm, and which way it fails
///
/// | `state` | byte | `/proc` | verdict |
/// |---|---|---|---|
/// | `FREE` | held | gone | **(b)** [`SlotLeak::ForkInheritor`] — the read-only participant's inheritor, and the likeliest one |
/// | `FREE` | held | running or unknown | nothing — an ordinary read-only consumer (D18) |
/// | `FREE` | free or unknown | any | nothing — an empty slot, which is most of a real table |
/// | non-`FREE` | free | gone | **(a)** [`SlotLeak::Abandoned`] |
/// | non-`FREE` | free | unknown | **(a)** — the byte alone is the leak signature, and §5.1 says the byte is the fact |
/// | non-`FREE` | free | running | nothing. See below |
/// | non-`FREE` | held | gone | **(b)** [`SlotLeak::ForkInheritor`] |
/// | non-`FREE` | held | running or unknown | nothing — somebody is holding it |
/// | `LIVE` | unknown | any | (a) iff [`ParticipantInfo::alive`] is false |
///
/// **A `FREE` record is not silence, and that was a real hole.** `FREE` says
/// only that no *arena* record is here, and the participant D18 makes the
/// consumer default — read-only, `PROT_READ`, Python's `multiprocessing`
/// worker — never writes one: it takes a lock byte, writes a lock-file identity
/// record, and leaves the arena table alone. So the fork inheritor of a
/// read-only participant is `FREE` + held + gone, it is the single most likely
/// shape on a Python deployment, and returning `None` for every `FREE` row
/// reported it as `"status": "pass"` while `docs/RUNBOOK.md` told the operator
/// this check would name it. Clause (b) is therefore judged from the byte and
/// the identity record, which are the two facts such a slot has; clause (a)
/// still needs an arena record, because an arena record is the thing it says
/// nothing will reassign.
///
/// **`free` + `running` is silence, and it is the one place this is quieter
/// than the reclaimer.** `tf_tree`'s reclamation predicate calls a non-`FREE`
/// record over a free byte reclaimable with no `/proc` conjunct at all, so on
/// that combination a report and a sweep can disagree. The disagreement is
/// deliberate: what produces it is a participant with a record and no byte,
/// which `0028` open question 1 ruled out of the deployment model and step 0b
/// removed the producer of — and naming a process `/proc` says is *running* as
/// leaked is the one false positive that gets a `warn` suppressed for good.
///
/// **The `unknown` byte row is the source with no lock file at all** — the
/// in-process fixture and `--from-bag`, whose participant table is this
/// process's own. It keeps exactly the predicate this check shipped with, so a
/// source that cannot be asked about a byte reports what it always did rather
/// than falling silent.
///
/// **The one race left in it is `register_any`'s, and it is µs-wide.** That
/// path takes the byte *before* writing the identity record (a deliberate
/// deviation from §3.3, argued in `tf_tree_ipc::Open::register_any`), so for a
/// few microseconds a freshly taken byte sits over the *previous* occupant's
/// identity — and if that occupant is dead, this reads (b). A diagnostic that
/// is momentarily wrong about a slot being re-taken is the cost of that
/// deviation, and it is priced in the direction of a report rather than a
/// reclamation: nothing acts on (b) but a human.
#[must_use]
pub fn slot_leak(p: &ParticipantInfo) -> Option<SlotLeak> {
    // The word first, and it answers exactly one question: is there an arena
    // record here? `FREE` means there is not — which is the ordinary state of
    // a live read-only joiner, whose byte is held and whose arena record was
    // never written because the mapping is `PROT_READ` (D18, and the Python
    // default). It is most of a real table. It is *not* a reason to stop
    // asking: the byte beside a `FREE` record can still be held for a process
    // that is gone, and that is clause (b) with no arena record under it.
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
        // No byte was asked for. `alive` is then the `/proc` inference
        // `Tree::participant_alive` falls back to, and `state == LIVE` is
        // already folded into it — so `RESERVED` cannot be judged from here and
        // is not.
        (LockByte::Unknown, _) => {
            (p.state == SlotState::Live && !p.alive).then_some(SlotLeak::Abandoned)
        }
    }
}

/// The evidence clause of an [`SlotLeak::Abandoned`] finding.
///
/// Three renderings because there are three evidence sets. A message that says
/// "the lock byte is free" on a run that never opened a lock file is asserting
/// a syscall it did not make, and an operator who then goes looking for the
/// holder has been sent by the tool.
fn abandoned_evidence(p: &ParticipantInfo) -> &'static str {
    match (p.byte, p.recorded) {
        (LockByte::Free, RecordedProcess::Gone) => {
            "the lock byte is free, and /proc has no running process for it"
        }
        (LockByte::Free, _) => {
            "the lock byte is free, and /proc could not say what became of the process — so \
             the kernel's answer is the whole of the evidence"
        }
        _ => {
            "/proc says its process is gone, and no lock file was read on this run — so the \
             kernel's own answer about the byte is not in this report"
        }
    }
}

/// The pid a slot finding's evidence is about, and the arena record's, in one
/// subject line.
///
/// **The lock file's pid leads, because the `/proc` sentence in every message
/// below is about that one.** `doctor` shipped
/// *"slot 8 pid 0 … /proc has no running process for it"*: the `0` is
/// [`ParticipantInfo::pid`], the arena record's field, which is still zero on a
/// `RESERVED` row and is *always* zero on the `FREE` row a read-only
/// participant leaves — while the process the sentence is about is named only
/// in the lock file. Printing the record's number beside the other one's
/// evidence gives an operator a pid to hunt that no evidence in the finding
/// concerns.
///
/// Both are named when both exist and differ, because both are then real: the
/// lock file names the process, and the arena record names the occupancy that
/// is stuck. When the record's field is zero it names nothing and is left out
/// rather than printed as a `0` somebody has to interpret.
///
/// The trailing byte clause is what separates the two shapes **in the subject
/// alone**, which is what `docs/RUNBOOK.md`'s `TFT014` rows key on: at 3am, a
/// slot to reap and a slot nothing may reap must not need a paragraph of prose
/// to tell apart.
fn slot_subject(p: &ParticipantInfo) -> String {
    let evidence_pid = p.recorded_pid.unwrap_or(p.pid);
    let byte = match p.byte {
        LockByte::Held => "byte still HELD",
        LockByte::Free => "byte free",
        LockByte::Unknown => "byte not probed",
    };
    match p.recorded_pid {
        Some(rp) if rp != p.pid && p.pid != 0 => {
            format!(
                "slot {} pid {rp} (arena record names pid {}), {byte}",
                p.slot, p.pid
            )
        }
        _ => format!("slot {} pid {evidence_pid}, {byte}", p.slot),
    }
}

/// `TFT014` — a participant slot, or a claim, that outlived its owner.
///
/// Both halves of §6's title, and they are one condition seen from two tables:
/// a process died without running `Drop`, and what it left behind is a slot
/// nothing will reassign and — if it was writing — an edge nothing will
/// reclaim. `Snapshot::participants` carries one liveness answer per slot and
/// both halves read it, so a report cannot call the same process alive on an
/// edge line and dead on a slot line.
///
/// # The predicate is the lock byte, and it is [`slot_leak`]
///
/// `docs/PHASE2.md` §5.1: liveness is the participant's OFD lock byte, never
/// the record. The **claim half** reads
/// [`crate::doctor::ParticipantInfo::alive`], which is
/// [`Tree::participant_alive`] — `F_OFD_GETLK` on the slot's byte for a tree
/// from `tf_tree::open` and a `/proc` inference otherwise, both failing safe
/// towards *alive*. Reading `state` instead is what made this check blind in
/// the state it is named for — `identity` answers for any record whose `state`
/// word reads `LIVE`, and a participant killed without running `Drop` leaves
/// one behind, so `owner_pid` came back non-zero and the check stayed quiet
/// (`docs/decisions/0028`, issue #184).
///
/// The **participant half** needs more than one boolean and takes it from
/// [`slot_leak`], whose table is the whole predicate. One `alive` cannot
/// separate a joiner healthily sitting in `RESERVED` from one that died there,
/// nor a released byte from one a forked child still holds, and those are the
/// two silences `0028` plan step 6 closes. The extra facts arrive through
/// [`Snapshot::probe_lock_facts`]; a source with no lock file supplies neither
/// and the check reports what it always did.
///
/// **Detection only. Nothing here reclaims anything**, deliberately: `0028`
/// reclaims from the *assigner* and from `Tree::reap_participants`, and a
/// `doctor` check that mutated a robot's arena as a side effect of being asked
/// a question would be the tool overstepping in the direction D18 exists to
/// prevent.
///
/// # Which shapes reach it, and which of those `--attach` can see
///
/// **Not the one issue #184 measured.** A rendezvous joiner `SIGKILL`ed under a
/// running owner is reclaimed by the owner's socket-hangup callback
/// (`crates/tf_tree/src/open.rs`, issue #191, which `0028` calls *candidate B —
/// hangup-driven owner reap*): its `ParticipantTable::release` is a real
/// `LIVE -> FREE` transition — measured with an owner, a read-write joiner and
/// a third observing process, the killed joiner's `state` word reading `FREE`
/// by the observer's first poll 50 ms later. On that arena this arm stays quiet
/// and the claim half is what speaks.
///
/// So a slot finding here means the slot was one that callback **cannot**
/// reach. `0028` enumerates those places; the ones that leave this state — a
/// `LIVE` record over a free byte — are:
///
/// * **The owner's own slot** (`0028` candidate B, hole 3). The owner registers
///   itself and nothing hangs up on it.
/// * **An owner killed between the hangup's probe and its CAS** (`0028`, *"a
///   crash between the hangup and the CAS"*). One `compare_exchange`, so
///   nothing is torn; what is lost is the reclamation.
/// * **A client the owner's `epoll::add` failed for** (hole 4). It is
///   deliberately left unwatched, so its death produces no hangup.
/// * **A `ReadWrite` `Tree::attach_shared` participant** (hole 5). No socket
///   and no grant, so no hangup, ever.
/// * **A takeover heir's inherited peers** (hole 3 again). A new owner's
///   `epoll` set holds no pre-takeover client sockets. §3.5 takeover is not
///   wired, so this is reachable only once it is.
///
/// The remaining hole, the fork-inherited connection (hole 1), leaves the byte
/// **held** by the child, so it is not in the list above — it is a finding of
/// its own shape, [`SlotLeak::ForkInheritor`], and the one that does not need
/// an arena record at all: the read-only participant D18 makes the consumer
/// default writes none, and a `fork`ed Python worker holding its dead parent's
/// byte is exactly a `FREE` row over a held byte.
///
/// **`doctor --attach` can be pointed at only two of those five today**, which
/// is worth knowing before reading a quiet report as an all-clear: attaching
/// goes through the rendezvous, so the two shapes that leave the owner dead
/// refuse a fresh join with `ArenaHeldButUnreachable` — the record is `LIVE`,
/// the leak is real, and no new process can be told. `epoll::add` and
/// `attach_shared` leave the owner running and are reachable now; takeover
/// becomes a third once §3.5 is wired.
/// `crates/tf_tree/tests/rendezvous.rs`'s
/// `the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live` stages the
/// reclaimed peer and the unreclaimable owner on one arena and asserts that
/// refusal.
///
/// # It needs a table, not a picture of one
///
/// A frozen `.tft` is a byte copy of the whole arena, participant records
/// included, so *every* slot in it names a process that exited when the freeze
/// finished. Running here would fire on every correct `.tft` ever written, for
/// an arena that has no assigner for a leaked slot to wedge — so the check
/// **skips** on [`SlotTable::Image`] and says which source can answer. That
/// replaces a `pass`, which was the worse of the two: a fabricated all-clear
/// about a question the file cannot be asked.
///
/// # Which direction it fails in
///
/// Towards silence, in three named ways, because a warn that fires on a
/// healthy robot is one that gets suppressed within a week.
///
/// **Two of the five that used to be here are now findings**, which is `0028`
/// plan step 6: `RESERVED` was unreportable while the only fact was
/// `participant_alive` (it folds `state == LIVE` in ahead of the probe and so
/// answers "not alive" for a healthy joiner mid-attach exactly as for one that
/// died there), and the fork case was unreportable while `/proc` was composed
/// away rather than carried. `doctor` now opens the lock file — the thing
/// `cmd_participants` always did and this check did not — and [`slot_leak`]
/// composes the three facts once. Note what did **not** change: the fork case
/// is still not reclaimable by anybody, because the kernel's answer for it is
/// *held*; what it gained is a name.
///
/// * **A claim over a slot that has since been re-granted is not reported, and
///   this one is reachable from the ordinary #184 flow.** The claim's owner word
///   carries the `ClaimRecord`'s own per-edge epoch, not the participant's
///   incarnation (`tf_tree_core::edge::pack_owner`), so nothing in it says
///   *which occupancy* of the slot took the claim. The hangup reap frees the
///   dead writer's slot but not its claims — nothing calls
///   `Tree::reap_participant` on hangup — so once a later joiner is granted that
///   slot, the stale claim joins to a live participant and the edge reads
///   healthy while no process is writing it. Not a regression: the
///   `owner_pid == 0` predicate was silent here too, for the same reason.
///   Closing it needs the incarnation *inside* the claim word, which is an
///   arena format change and not one `0028` proposes.
/// * **A claim caught mid-handoff (`CLAIMING`) is excluded.** It names no slot,
///   exactly as a dead owner's claim names no slot, so from a snapshot the two
///   are the same shape. `Tree::reap` may act on `CLAIMING` because it probes
///   the claim's *own* lease first; this check has no such probe, so it reports
///   neither and loses a claimer killed inside that window.
///
/// **The one place it used to fail the other way has had its producer
/// removed.** A `ReadWrite` `Tree::attach_shared` wrote an arena record and
/// took no lock byte, so its healthy participant read as a leak; `0028` step 0b
/// made both fd-attach arms refuse `ReadWrite`, so every supported read-write
/// participant now joins through the rendezvous and takes its byte before the
/// record leaves `FREE`. `TreeBuilder::build_shared` called directly still
/// registers without a byte and is still supported — but such a tree has no
/// lock file, so it reaches [`slot_leak`]'s `unknown` byte row and is judged by
/// `/proc` alone, exactly as it was before.
///
/// **The claim half rests on the owner word being decoded correctly.** It is
/// `(epoch << 16) | (slot + 1)`, so a hand-rolled `word - 1` resolves every
/// live claim to nothing and reports every claimed edge as leaked;
/// `Snapshot::capture` uses `tf_tree_core::edge::slot_of`, and
/// `doctor::tests::a_held_claim_resolves_to_the_writers_pid` pins it.
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
        // The word this row actually carries. `FREE` used to render as `LIVE`
        // because no `FREE` row could produce a finding; one can now — the
        // read-only participant's fork inheritor — and calling its record
        // `LIVE` would send an operator looking for an arena record that was
        // never written.
        let state = match p.state {
            SlotState::Reserved => "RESERVED",
            SlotState::Live => "LIVE",
            SlotState::Free => "FREE (no arena record: a read-only participant, D18)",
        };
        // The pid every sentence below is about: the lock file's, which is the
        // one `/proc` was asked about, falling back to the arena record's on a
        // source that read no lock file.
        let pid = p.recorded_pid.unwrap_or(p.pid);
        match slot_leak(p) {
            None => {}
            Some(SlotLeak::Abandoned) => out.push(Finding::about(
                Tft::Tft014,
                slot_subject(p),
                format!(
                    "a record left behind — the record is {state}, {} — pid {pid} \
                     left slot {} registered and no longer holds it, and the owner's \
                     socket-hangup reap did not clear it. That reap collects a rendezvous \
                     peer, so this is a slot it cannot reach: the owner's own, one its epoll \
                     never watched, an attach_shared participant, a takeover, or an owner \
                     that died inside the callback (docs/decisions/0028). Nothing reclaims \
                     it — {leaked} of {slots} slots are spent for the life of the segment, \
                     and at {slots} every further attach fails NoParticipantSlots. Only \
                     stopping every participant, which frees the segment, frees a slot",
                    abandoned_evidence(p),
                    p.slot
                ),
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
///
/// Worth a warning rather than an error because nothing is broken yet, and
/// worth a warning at all because the arena cannot grow (`docs/PROJECT.md` §5):
/// the failure when a table fills is a refused `intern` or `declare_edge`, at
/// which point the fix is a rebuild and a fleet restart.
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
    // **The knob that governs the live arena**, which is a sealed `memfd` mapped
    // `MAP_SHARED` — shmem, not anonymous memory. `enabled` above does not apply
    // to it. Reading only `enabled` reported a host as passing while
    // `MappedArena`'s `MADV_HUGEPAGE` was a no-op and the arena took 4 KiB
    // pages, which is the failure this check exists to catch.
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
                "RLIMIT_MEMLOCK is {limit} bytes, below the {} byte arena; mlock() of the \
                 arena will fail, so a real-time consumer cannot keep page faults out of \
                 its control loop",
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
///
/// The Phase 1 `unclaimed-dynamic` check, given an id by `docs/PHASE5.md` §6's
/// amendment. Distinct from `TFT013` (declared and *never* published to) and
/// from `TFT014` (a slot, or the claim it was holding, whose owner is gone):
/// this is an edge with no claim at all, which may have a full ring of history
/// that is now going stale.
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
///
/// The Phase 1 `out-of-order` check, given an id by `docs/PHASE5.md` §6's
/// amendment. Distinct from `TFT006`, which judges a stamp's *value*: a stream
/// of perfectly plausible stamps can still arrive backwards, and that is what
/// breaks a consumer's interpolation.
///
/// # It runs on a stream that was recorded, and skips on one that was replayed
///
/// The gate is [`PushStream::no_rejected_arrivals`], not liveness, and the two
/// are not the same question. **An arena of any kind is the wrong evidence for
/// this check**: `SampleRing::push` rejects a stamp older than the ring's last,
/// so a ring holds only accepted pushes and [`Observations::from_arena`] can
/// only ever reconstruct a non-decreasing sequence — on a live arena, on a
/// frozen `.tft`, and on an arena built from a bag, which §3.1 additionally
/// *sorts*. A live arena adds a second, opposite failure on top: the retained
/// window is read with relaxed loads while a publisher writes into it, so a
/// sample from the next lap can appear at the old end and read as an inversion
/// the publisher never made.
///
/// So there are exactly two streams it can run against: the fixture's, which
/// records the pushes the engine refused because the fixture *is* the publisher,
/// and a recording's log order (`doctor --from-bag`), where a backwards stamp is
/// in the file at the position it arrived at.
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
///
/// Taken from the type rather than written as `0`: `docs/API.md` §2.5 records
/// that a tag is a permanent choice read by every consumer and every recording
/// already on disk, so the literal and the type must not be able to drift apart.
const WALL_CLOCK_TAG: u8 = <SystemDomain as Domain>::TAG;

/// How long a burst of rejected pushes has to be before `TFT019` will call it a
/// clock step, in **arrivals on that edge**.
///
/// # This number is not in the spec
///
/// `docs/PHASE5.md` §6's amendment says "a run of rejections **concentrated in a
/// short window**" and `docs/API.md` §5.3 item 1 says "a run of rejected
/// pushes". Neither names a length, so this constant is *this implementation's*
/// choice and is stated here rather than left implicit in a `> 0`.
///
/// # Why a count of arrivals and not a duration
///
/// The stamps are the quantity under suspicion, and the observed stream carries
/// no independent arrival clock — [`Observations::from_arena`] sets
/// `arrival_delay_ns` to zero because the arena records no receipt time
/// (`TFT004`'s skip reason is the same fact). A window measured in seconds could
/// only be measured with the clock that stepped. Counting arrivals needs no
/// clock at all.
///
/// # Why eight
///
/// A clock that steps back by Δ against a publisher at *f* Hz rejects about Δ·*f*
/// arrivals in one unbroken burst, so eight is reached by an 8 ms step at 1 kHz,
/// 80 ms at 100 Hz, 800 ms at 10 Hz. A *reordered* stream — a merge, a queue, a
/// pair swapped in transit — rejects as many arrivals as the reorder distance,
/// which is one to a few. Eight sits above the second population and well below
/// the first.
///
/// **Both costs of the choice, named rather than implied.** A step shorter than
/// eight publish periods is not attributed: `TFT018` still reports the rejected
/// pushes, which is the answer that existed before `TFT019` and is not a
/// regression. A reordering burst longer than eight arrivals on a wall-clock
/// edge *is* attributed, and that is a false attribution this threshold does not
/// prevent. Raising it trades the first for the second.
const CLOCK_STEP_MIN_REJECTED_RUN: usize = 8;

/// How `TFT019` split `TFT018`'s per-edge evidence: what it attributed to a
/// wall-clock step, what was too diffuse to be one, and what it would not judge.
///
/// **Captured once per `doctor` run and read twice** — by `tft019` and by
/// [`ClockStepEvidence::coverage_note`]. That is why it is a field of [`Inputs`]
/// rather than a call inside each reader: two captures could not disagree today,
/// but the report would then be walking `obs` once per reader to answer one
/// question, and the invariant "the outcome and the note describe the same
/// split" would be a convention instead of a type.
pub struct ClockStepEvidence {
    /// Runs on a `SystemDomain` edge whose rejections are concentrated enough to
    /// be a step, each with the label to report it under.
    attributed: Vec<(doctor::OutOfOrderRun, String)>,
    /// Runs on a `SystemDomain` edge that are **not** concentrated: fewer than
    /// [`CLOCK_STEP_MIN_REJECTED_RUN`] consecutive rejected arrivals, as
    /// `(edge, longest run)`. A stray inversion on a wall clock is a publisher
    /// fault, and reporting it as a clock step is the false all-clear this check
    /// refuses in the other direction.
    diffuse: Vec<(u32, usize)>,
    /// Runs on any other tag, as `(edge, tag)`, with `None` for an edge whose
    /// tag could not be read at all. Named, not counted: the skip reason has to
    /// say *which* tag it declined to guess about.
    ///
    /// `Option<u8>` rather than a sentinel value, because every `u8` is a legal
    /// tag — `Domain` is an open trait — so a `u8::MAX` meaning "unknown" would
    /// print "tag unreadable" for an edge that declared 255.
    refused: Vec<(u32, Option<u8>)>,
}

impl ClockStepEvidence {
    /// Split `TFT018`'s per-edge evidence by the edge's declared domain tag and
    /// by whether its rejections are concentrated.
    ///
    /// Three buckets, and an edge whose tag cannot be read joins the refused
    /// ones: "the tag is unreadable" and "the tag is not a wall clock" have the
    /// same consequence here — no clock step may be attributed.
    ///
    /// Captured unconditionally, including on a live arena where `TFT019` skips
    /// and discards it. `live` is not a parameter because the split is a fact
    /// about the stream and the skip is a fact about the check; folding them
    /// would give this type two meanings.
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
                // A `const` in a pattern, so this is an equality test against
                // tag 0 and not a binding. Renaming `WALL_CLOCK_TAG` to anything
                // lowercase would silently turn it into one that matches every
                // tag.
                Some((WALL_CLOCK_TAG, e)) => {
                    if run.longest_rejected_run >= CLOCK_STEP_MIN_REJECTED_RUN {
                        ev.attributed.push((run, snap.edge_label(e)));
                    } else {
                        ev.diffuse.push((run.edge, run.longest_rejected_run));
                    }
                }
                Some((tag, _)) => ev.refused.push((run.edge, Some(tag))),
                // Reachable from a hand-assembled `Inputs` and from a recorded
                // stream whose edge is absent from the snapshot. Tag 0 is not
                // the honest default for an unknown tag — that is the fabricated
                // all-clear this whole check refuses.
                None => ev.refused.push((run.edge, None)),
            }
        }
        ev
    }

    /// The disclosure that pairs with `tft019`'s outcome: which edges its
    /// result does *not* cover.
    ///
    /// `None` when the outcome already carries the whole story — nothing was
    /// found, or everything found was attributed, or the skip reason itself
    /// names every refused tag. [`crate::catalogue::Status`] is three-valued and
    /// none of them is "ran, half blind"; this is the same gap
    /// [`rate_coverage_note`] exists to fill.
    ///
    /// `stream` is a parameter rather than a caller-side `if`: wherever `TFT019`
    /// skipped outright, a note listing edges it "did not attribute" would
    /// describe a run that did not happen.
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
///
/// Tag-specific rather than one blanket sentence: `SimDomain` (2) *does* step
/// backwards — a `/clock` reset from a bag loop or a sim restart is exactly that
/// — so telling its operator "a steady or PTP tag cannot have stepped at all"
/// would be false about the one case with its own decision record.
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
        // Unreachable: a tag-0 run is either attributed or diffuse, never
        // refused. Answered anyway so this stays a total function of the tag.
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
///
/// `docs/PHASE5.md` §6's amendment, argued in `docs/API.md` §5.3.
/// `CLOCK_REALTIME` is not monotone: an NTP step or a leap second moves it
/// backwards, `docs/PHASE1.md` §2 invariant 6 then rejects every push until the
/// clock catches up, and the result reads as a `tf_tree` defect to whoever meets
/// it at 3 a.m. `TFT018` reports that, accurately and unhelpfully, as a
/// publisher that restarted without resetting its clock.
///
/// # An attribution, not a second detector
///
/// The evidence is [`doctor::out_of_order_runs`] — literally the function
/// `TFT018` reports from — plus one fact the arena already holds, the edge's
/// declared `EdgeRecord::domain`. There is no second scan of the stream and no
/// counter: `dropped_non_monotonic` is a *bridge* counter and an arena with no
/// bridge in front of it has none.
///
/// # It fires on a *run*, not on an inversion
///
/// `docs/PHASE5.md` §6 says "a run of rejections **concentrated in a short
/// window**"; the concentration is [`CLOCK_STEP_MIN_REJECTED_RUN`] consecutive
/// rejected arrivals, which is this implementation's number and not the spec's —
/// that constant carries the argument and both costs. A single stray inversion
/// on a wall-clock edge is a publisher fault, and calling it an NTP step is the
/// same fabricated all-clear this check refuses on an unknown tag, pointed the
/// other way. Those edges land in `diffuse` and are disclosed in
/// [`ClockStepEvidence::coverage_note`].
///
/// # It fires only on tag 0, and says which tag when it does not
///
/// [`Domain`] is an open trait, so a user-declared tag carries no way to state
/// "this clock can step". Guessing that an unknown tag is steady would fabricate
/// an all-clear on the edge most likely to be a PTP driver that lost lock, and
/// guessing the other way would blame a clock for a publisher's fault. So an
/// edge on any other tag is *refused*, with the tag named — the same register as
/// `TFT007` skipping an undeclared rate rather than comparing against zero. Since
/// `SimDomain` (2) and `SteadyDomain` (3) exist, that refusal is correct rather
/// than merely conservative: a steady clock cannot step, so regressions there are
/// a real publisher defect.
///
/// # It does not demote `TFT018`
///
/// `TFT018` stays an error and keeps failing `doctor --exit-code`; this is a
/// warn that explains it. Rejected pushes are lost data whatever caused them.
///
/// # It reaches a verdict on a recording, and that is what `--from-bag` is for
///
/// This check needs a *recorded* push stream. Until `doctor` gained a recording
/// source it had two — the built-in fixture and a live `--attach` — and it
/// skipped on the second, so no run against real data could reach a verdict at
/// all. `doctor --from-bag <recording.mcap>` is that third source: a bag is
/// written in log order, so a stamp that went backwards is in the file at the
/// position it arrived at, and both this check and `TFT018` run on it.
///
/// **A frozen `.tft` (`--from-file`) is not a substitute**, and the reason is
/// worth stating because it is the obvious guess: a `.tft` is an *arena*, and an
/// arena's rings hold only the pushes the engine accepted, so the rejected
/// arrival this check attributes was never stored. See
/// [`PushStream::RingsAtRest`].
///
/// # Skips and passes
///
/// * **A stream replayed from rings** — skipped, inheriting `TFT018`'s skip
///   rather than working around it. There is nothing to attribute: on a live
///   arena the only inversions are artifacts of reading a ring while it is
///   written, and on an arena at rest there are none at all.
/// * **No regressions at all** — `Pass`, not `Skipped`. This check's evidence is
///   `TFT018`'s and it is complete: every retained stream was examined and none
///   went backwards, so there is nothing to attribute and saying so is earned.
/// * **Regressions on tag 0, none concentrated** — `Pass`, with the note. The
///   evidence is complete here too and it says "not a step"; the note is what
///   keeps that from reading as "nothing was seen".
/// * **Regressions, none on tag 0** — `Skipped`, naming the tags. `Pass` there
///   would read as "no clock step", which is an assurance about clocks this
///   check is not able to give.
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
        // A `Skipped` only when *nothing* was judged. A diffuse wall-clock run
        // was judged — "not concentrated enough to be a step" is an answer, and
        // `coverage_note` carries it — so it passes rather than skipping.
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
        // Earned: the evidence is complete, not missing.
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
///
/// **`participants` is deliberately absent, and [`PARTICIPANT_OCCUPANCY_NOTE`]
/// says so in the report.** §6 names frames, edges *and* participants, but
/// `ArenaHeader::participant_count` is never incremented anywhere in the
/// workspace — the only writes to it are the zero it is initialised with. A row
/// fed from it would read `0 / max_participants` on every arena, so `TFT015`
/// would report `pass` for participants on a fleet that had exhausted every
/// slot and could not attach another node.
///
/// Emitting the row anyway would be worse than omitting it: this catalogue's
/// whole premise is that a check without evidence says so rather than passing,
/// and a permanently-`0%` row passes silently and looks like a real result. So
/// the row is dropped and the gap is disclosed in `Meta.notes`, which exists
/// for exactly this "ran, but half blind" case. Restore the row in the same
/// commit that makes the engine maintain the counter.
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
        // Both sides include the sentinel at index 0: `edge_count` is
        // (declared + 1) and `max_edges` is the table size, so the ratio is
        // exact rather than off by one slot.
        ("edges", h.edge_count.load(Ordering::Relaxed), h.max_edges),
    ]
}

/// The disclosure that pairs with [`occupancy_of`]'s missing `participants` row.
pub const PARTICIPANT_OCCUPANCY_NOTE: &str =
    "TFT015 covers frames and edges only: ArenaHeader::participant_count is never \
     incremented by the engine, so a participants row would read 0% on every arena \
     and pass even with the slot table full";

/// Every edge's newest stamp — the sample [`Clock::decide`] votes over.
///
/// Deliberately the whole population and not an aggregate: the aggregation is
/// `decide`'s job, and it needs the individual values to be robust to an
/// outlier. Handing it a single pre-reduced stamp is what let one broken
/// publisher define the reference clock.
#[must_use]
pub fn newest_stamps(snap: &Snapshot) -> Vec<i64> {
    snap.edges.iter().filter_map(|e| e.newest_stamp).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::catalogue::Status;
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
            nominal_rate_mhz: None,
        }
    }

    /// The participant table that goes with [`edge`]: one running writer in
    /// slot 0, which is the owner every edge this helper builds names.
    ///
    /// Every fixture gets one, because `TFT014`'s claim half now asks the
    /// participant table whether the owner is running and an empty table
    /// answers "nobody is" — the state a hand-built snapshot would otherwise
    /// fall into by omission rather than by intent.
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
            // Leaked so the helper can keep its four-argument shape across
            // thirty-odd call sites: `Inputs` borrows the split, and a test
            // process that exits after one assertion has nothing to reclaim.
            clock_step: Box::leak(Box::new(ClockStepEvidence::capture(snap, obs))),
            stream: PushStream::Observed,
            slots: SlotTable::Current,
            counters: true,
        }
    }

    /// **`TFT011` must not fire on a gap past the *newest* end of the window.**
    ///
    /// `worst_extrap_gap_ns` is a high-water mark over both ends. A request
    /// newer than the newest sample means the publisher stopped, and enlarging
    /// the ring cannot help; reporting it as an undersized ring sends an
    /// operator to change the one thing that is already correct. The gate is
    /// `extrap_before > 0`.
    ///
    /// Mutant: drop the `st.extrap_before == 0` term from the guard in
    /// `tft011`. Applied: the `after`-only case fires and the first assertion
    /// fails.
    #[test]
    fn ring_undersize_is_only_claimed_when_a_request_fell_off_the_back() {
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let obs = Observations::new();

        // Both fixtures retain the same 1 s window and record the same 5 s
        // worst gap, so the *only* difference is which end it was past.
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

        // And a gap that fits inside the window is not undersizing either.
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
    ///
    /// `EdgeRecord::domain` (D9) permits any time domain, and the benchmark
    /// fixture's own history starts at stamp 0. Flagging zero unconditionally
    /// would make `doctor --exit-code` fail on a correct arena. A *negative*
    /// stamp is invalid under every domain and is flagged either way.
    ///
    /// Mutant: change the `if wall && st.zero_stamps > 0` guard in `tft006` to
    /// `if st.zero_stamps > 0`. Applied: the `NewestStamp` case fires and its
    /// `Status::Pass` assertion fails.
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

        // Boot-relative domain: stamps near zero are the origin, not a bug.
        let o = tft006(&inputs(
            &snap,
            &obs,
            &zeros,
            Clock::NewestStamp(1_000_000_000),
        ));
        assert_eq!(o.status, Status::Pass, "{o:?}");

        // Unix domain: the same stamps mean 1970.
        let o = tft006(&inputs(&snap, &obs, &zeros, Clock::Wall(1_000_000_000)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert!(o.findings[0].message.contains("1970"));

        // A negative stamp is invalid in either domain.
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
    ///
    /// A publisher writing nanoseconds into a field it believed held seconds is
    /// off by 10^9, which no plausible-range check on the value alone
    /// distinguishes from a valid stamp — but it is enormously far from every
    /// other stamp in the arena.
    ///
    /// Mutant: raise `ABSURD_HORIZON_NS` to 100 years. Applied: the fired
    /// assertion fails — the injected stamp sits ~54 years from the reference
    /// clock, which a 100-year horizon waves through.
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

        // The same publisher, having written seconds-worth of nanoseconds into
        // a nanosecond field: 1e9 times too large.
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

    /// **`TFT013` is about dynamic edges only.**
    ///
    /// A static edge carries its pose inline in `EdgeRecord::static_pose` and
    /// never pushes, so `head == 0` is its permanent, correct state. A check
    /// that did not exclude them would report every static edge on every
    /// healthy robot — which is how a diagnostic becomes noise nobody reads.
    ///
    /// Mutant: drop `e.kind == EdgeKind::Dynamic` from the `tft013` guard.
    /// Applied: the static-edge assertion fails.
    #[test]
    fn never_published_does_not_accuse_static_edges() {
        let obs = Observations::new();
        let stats: [EdgeStats; 0] = [];

        let mut e = edge(1, 1, 2, 0);
        e.kind = EdgeKind::Static;
        e.capacity = 0;
        let snap = two_frame_snapshot(e);
        assert_eq!(
            tft013(&inputs(&snap, &obs, &stats, Clock::Wall(0))).status,
            Status::Pass,
            "a static edge's head is 0 for the life of a correct arena"
        );

        let snap = two_frame_snapshot(edge(1, 1, 2, 0));
        let o = tft013(&inputs(&snap, &obs, &stats, Clock::Wall(0)));
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert_eq!(o.findings[0].edge, Some(1));

        // Non-vacuity: a dynamic edge that has published is clean.
        let snap = two_frame_snapshot(edge(1, 1, 2, 7));
        assert_eq!(
            tft013(&inputs(&snap, &obs, &stats, Clock::Wall(0))).status,
            Status::Pass
        );
    }

    /// **`TFT010` needs the counters, and says so rather than passing.**
    ///
    /// With `--no-default-features` every counter reads zero, which is exactly
    /// what a healthy arena reads. Reporting `pass` would be a clean bill of
    /// health derived from an absence of instrumentation.
    ///
    /// Mutant: return `CheckOutcome::ran(Tft::Tft010, vec![])` instead of
    /// `skipped` when `!inp.counters`. Applied: the `matches!(..., Skipped)`
    /// assertion fails.
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

        // Non-vacuity: with the counters compiled in, the same state fires.
        inp.counters = true;
        let o = tft010(&inp);
        assert_eq!(o.status, Status::Fired, "{o:?}");
        assert!(
            o.findings[0].message.contains("33.3%"),
            "rate must be errors/(errors+ok): {}",
            o.findings[0].message
        );

        // ...and a rate under the threshold does not.
        let cool = [EdgeStats {
            lookups_ok: 100_000,
            extrap_after: 1,
            ..hot[0].clone()
        }];
        inp.stats = &cool;
        assert_eq!(tft010(&inp).status, Status::Pass);
    }

    /// **The clock is only called a wall clock when the arena agrees with it.**
    ///
    /// `EdgeRecord::domain` permits stamps in any time domain. Treating a
    /// boot-relative arena's stamps as Unix time would make `TFT005` report
    /// every edge as fifty years stale and `TFT006` flag every stamp as absurd.
    ///
    /// Mutant: make `Clock::decide` always return `Clock::Wall`. Applied: the
    /// boot-relative assertion fails.
    #[test]
    fn the_reference_clock_refuses_to_mix_time_domains() {
        let unix_now = 1_700_000_000_000_000_000;
        // A boot-relative arena: 9.9 s since boot, as the benchmark fixture is.
        assert_eq!(
            Clock::decide(&[9_900_000_000, 9_800_000_000], unix_now),
            Clock::NewestStamp(9_900_000_000)
        );
        // A Unix-stamped arena a minute behind the clock is still Unix.
        assert_eq!(
            Clock::decide(&[unix_now - 60_000_000_000], unix_now),
            Clock::Wall(unix_now)
        );
        // An empty arena has nothing to disagree about.
        assert_eq!(Clock::decide(&[], unix_now), Clock::Wall(unix_now));
    }

    /// **One broken publisher must not be able to define the reference clock.**
    ///
    /// [`ABSURD_HORIZON_NS`] is both the domain-agreement threshold and
    /// `TFT006`'s absurdity radius, so the reference is the one stamp `TFT006`
    /// structurally cannot call absurd. If an extremum picks it, the single
    /// worst edge becomes immune and the healthy majority becomes the outlier:
    /// `TFT005` skips itself (the arena now looks non-Unix) and `TFT006` fires
    /// on every correct edge while exonerating the broken one. That is the
    /// diagnostic inverted — it blames five innocent publishers and clears the
    /// guilty one.
    ///
    /// The fixture is non-degenerate on the axis that matters: the five good
    /// edges carry *distinct* Unix stamps, and the bad edge's stamp is
    /// `now * 2`, the classic nanoseconds-into-a-seconds-field overshoot, which
    /// is the arena's maximum by a wide margin.
    ///
    /// Mutant: `Clock::decide` → `Clock::Wall`/`NewestStamp` chosen from
    /// `newest_stamps.iter().max()` as the old single-stamp estimator did.
    /// Applied: the arena is declared `NewestStamp(3_400_000_000_000_000_000)`,
    /// `TFT005` skips, and `TFT006` fires on edges 1-5 — the first assertion
    /// fails.
    #[test]
    fn a_single_units_error_cannot_capture_the_reference_clock() {
        let unix_now = 1_700_000_000_000_000_000;
        // Five healthy edges, each with its own stamp within a second of now,
        // plus one publisher that multiplied instead of dividing.
        let mut stamps: Vec<i64> = (0..5).map(|i| unix_now - i * 200_000_000).collect();
        let rogue = unix_now * 2;
        stamps.push(rogue);

        assert_eq!(
            Clock::decide(&stamps, unix_now),
            Clock::Wall(unix_now),
            "5 of 6 edges agree with the wall clock; the 6th must not be able to \
             redefine the domain"
        );

        // ...and with that reference, TFT006 blames exactly the rogue edge.
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
    ///
    /// `claim` parks the `CLAIMING` sentinel in the record for the few
    /// instructions between winning it and publishing an identity. `slot_of`
    /// maps that to no slot, exactly as it maps a dead owner's slot to no slot,
    /// so `owner_pid` is 0 in both cases. Firing on `owner_pid == 0` alone
    /// therefore reports a leak on a healthy arena every time `doctor` lands in
    /// that window — a publisher restart, at warn severity, on an edge that is
    /// fine. `Tree::reap` may act on `CLAIMING` only because it first probes
    /// the lock file for liveness; this check takes no such probe and so must
    /// not draw the conclusion.
    ///
    /// The fixture is non-degenerate: the two edges are identical except for
    /// `claiming`, so the assertion cannot pass by accident of some other
    /// field, and the genuinely-leaked edge proves the check still fires.
    ///
    /// Mutant: drop the `!e.claiming` term from `tft014`'s guard. Applied: both
    /// edges are reported and the first assertion fails.
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

        // The same edge with the sentinel cleared *is* a leak, and must fire —
        // otherwise the assertion above would hold for a check that never
        // reports anything.
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
    ///
    /// This is the state `docs/decisions/0028` is written about — a `LIVE`
    /// record over a free lock byte — but **not** the arena issue #184
    /// measured: #191 gave the owner a hangup reap, so a rendezvous joiner's
    /// record is released and this shape survives only where that reap cannot
    /// reach (the enumeration is on [`tft014`]). Which one it came from does
    /// not change what the check must do with it, so the fixture is built
    /// directly; the real-process version is
    /// `crates/tf_tree/tests/rendezvous.rs`'s
    /// `the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live`.
    ///
    /// **The pid is deliberately non-zero**: the previous predicate was
    /// `owner_pid == 0`, and `owner_pid` comes from
    /// `ParticipantTable::identity`, which answers for any `LIVE` record
    /// however dead its process — so the arena the check is named for was the
    /// one arena it could not see.
    ///
    /// Mutant: restore `e.owner_pid == 0` as the claim half's guard. Applied:
    /// the edge finding disappears and the count assertion fails at 1, while
    /// the slot finding still fires — which is `doctor` reporting a leaked slot
    /// and calling the edge it stranded healthy.
    /// Mutant B: drop the `p.state == SlotState::Live` conjunct from
    /// [`slot_leak`]'s `(LockByte::Unknown, _)` arm, leaving `!p.alive`.
    /// Applied: the `Reserved` slot below fires as well and the count
    /// assertion fails at *left: 3, right: 2* — which on a real no-lock-file
    /// table is every joiner caught mid-attach reported as a leak.
    #[test]
    fn a_stale_live_slot_and_the_claim_it_stranded_are_both_reported() {
        let obs = Observations::new();
        let mut held = edge(1, 1, 2, 100);
        held.owner_slot = Some(0);
        held.owner_pid = 4711;

        let mut snap = two_frame_snapshot(held);
        // **A source with no lock file**, which is what `byte: Unknown` says:
        // the fixture and `--from-bag` build their arena in this process and
        // have no rendezvous to probe. It is the row of [`slot_leak`]'s table
        // that keeps this check's original predicate, so this test pins that
        // row rather than the new byte-driven ones.
        snap.participants = vec![
            ParticipantInfo {
                slot: 0,
                state: SlotState::Live,
                pid: 4711,
                alive: false,
                byte: LockByte::Unknown,
                // No lock file was read, so there is no recorded pid either:
                // the arena record's is the only name this row has, and the
                // finding has to print that one.
                recorded_pid: None,
                recorded: RecordedProcess::Unknown,
            },
            // A joiner mid-attach. `participant_alive` folds `state == LIVE` in
            // ahead of the lock-byte probe, so this reads `alive: false` on a
            // perfectly healthy process — which is why a source with no byte to
            // probe must not report `RESERVED`. With a byte, it can: that is
            // `a_reserved_record_over_a_free_byte_is_the_leak_a_byte_can_see`.
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
        // `byte not probed`, because this source has no lock file: the subject
        // states which evidence the run actually had, so a `--from-bag` finding
        // and an `--attach` one are not read as the same claim.
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

        // **The same arena, read out of a `.tft`, must not report any of
        // it.** A frozen file is a byte copy of the whole arena (PHASE5 §2.3),
        // so its participant table names a run that ended and its claims name
        // that run's slots. Firing here is firing on every correct `.tft`, and
        // it is the only shape of this snapshot that a real source produces.
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
    ///
    /// The two pids agree here, which is the ordinary case; the row where they
    /// diverge has its own builder in
    /// [`the_subject_names_the_pid_the_evidence_is_about`].
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
            // `false` for every non-`LIVE` record and for a `LIVE` one over a
            // free byte, which is what `Tree::participant_alive` answers. None
            // of the rows below reads it — that is the point of them.
            alive: false,
            byte,
            recorded,
            // A row whose byte was never probed read no lock file, so it has no
            // identity record to name a pid either. Tying the two together here
            // keeps every fixture in this module a shape a real run produces.
            recorded_pid: (byte != LockByte::Unknown).then_some(4712),
        }
    }

    /// **A `RESERVED` record over a free byte is a leak, and the byte is what
    /// makes it visible.**
    ///
    /// `docs/decisions/0028` plan step 6 widened clause (a) from `LIVE` to any
    /// non-`FREE` record, and this is the row that needed the widening:
    /// `Tree::participant_alive` folds `state == LIVE` in ahead of the probe,
    /// so `alive` is `false` here whatever the byte says and the old predicate
    /// could not act on it in either direction. The sibling
    /// `a_joiner_holding_its_byte_mid_attach_is_not_reported` is the same
    /// record with the byte held, and the pair is the whole argument for
    /// carrying the byte separately from `alive`.
    ///
    /// Mutant: restore the predicate this check shipped with — replace
    /// [`slot_leak`]'s whole body with
    /// `(p.state == SlotState::Live && !p.alive).then_some(SlotLeak::Abandoned)`.
    /// Applied: `left: Pass, right: Fired` — the `RESERVED` leak goes back to
    /// being invisible.
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

    /// **The fork case is its own finding, with its own remedy.**
    ///
    /// `0028` plan step 6 clause (b), and the reason it is a *distinct* message
    /// rather than a second producer of the first one: the byte is held, so
    /// nothing may reclaim this slot — the kernel's answer is *alive* and
    /// overruling it with `/proc` is the inversion `PHASE2.md` §5.1 forbids.
    /// An operator told "the lock byte is free" here would go looking for a
    /// reaper that must not run.
    ///
    /// Mutant: return `SlotLeak::Abandoned` from the `(Held, Gone)` arm.
    /// Applied: it panicked on the first `contains` — *"the fork case must be
    /// named as itself"* — with the abandoned wording, which tells an operator
    /// to reap a slot the kernel says is held.
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
    ///
    /// The negative `0028` plan step 6 asks for by name, and the one that stops
    /// this becoming a check that always fires. §3.3's order is identity, then
    /// byte, then the arena record's `FREE -> RESERVED` CAS, so *every* healthy
    /// registrant passes through exactly this state.
    ///
    /// Both facts are needed to stay silent and the fixture proves it: a
    /// `Running` process behind a held byte, and an `Unknown` one behind a held
    /// byte (a `/proc` that would not answer, which must never read as death).
    ///
    /// Mutant: widen the fork arm to `(LockByte::Held, _)`. Applied:
    /// `left: Fired, right: Pass` — every healthy joiner and every participant
    /// on a `/proc`-less host reported as a fork inheritor. The same mutation
    /// fails `doctor_is_silent_about_a_joiner_that_is_mid_attach` in
    /// `tests/attach.rs`, which is the end-to-end half of this claim.
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
    ///
    /// The one row where this check is deliberately quieter than the reclaimer
    /// (`slot_leak`'s table says so in as many words): `tf_tree`'s reclamation
    /// predicate calls a non-`FREE` record over a free byte reclaimable with no
    /// `/proc` conjunct at all. What produces that combination is a participant
    /// with a record and no byte, which `0028` open question 1 ruled out and
    /// step 0b removed the producer of — and naming a *running* process as
    /// leaked is the false positive that gets a `warn` suppressed for good.
    ///
    /// Mutant: fold the `(Free, Running)` arm into the `Abandoned` one.
    /// Applied: `left: Fired, right: Pass`, with a finding whose evidence
    /// clause reads *"the lock byte is free, and /proc could not say what became
    /// of the process…"* about a process `/proc` had just said was running.
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
    ///
    /// Two sources reach clause (a) by different evidence: `--attach`, which
    /// has a lock file and can say the byte is free, and the fixture or
    /// `--from-bag`, which have none and are judged by `/proc` alone. The
    /// wording that shipped before step 6 — *"the lock byte is free"* —
    /// predates `doctor` opening a lock file at all, so on the second source it
    /// asserted a probe nobody performed and sent an operator looking for a
    /// holder.
    ///
    /// Mutant: return the `LockByte::Free` string from every arm of
    /// `abandoned_evidence`. Applied: it panicked on the second `contains`,
    /// printing a no-lock-file source's finding claiming *"the lock byte is
    /// free and /proc has no running process for it"* — a probe that run never
    /// made.
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
        // The no-lock-file source: `alive` is the whole of the evidence, and it
        // is `false` here because `slot` builds it that way.
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
                .contains("no lock file was read on this run"),
            "a run with no lock file must not claim the byte is free: {}",
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
    ///
    /// [`ParticipantInfo::alive`] is `false` for every `FREE` record in a real
    /// table — the predicate short-circuits on `state != LIVE` before it probes
    /// anything — and a 64-slot arena with one publisher has 63 of them. A
    /// check that read `alive` alone would report a healthy robot as leaking 63
    /// slots, which is the false positive that gets a warn suppressed for good.
    ///
    /// **The fixture's `/proc` answer was `Gone` and has been corrected to
    /// `Running`, which is what the comment beside it always claimed.** A
    /// read-only consumer holding its own byte is `Running`; `FREE` + held +
    /// *gone* is not that shape at all — it is the read-only participant's fork
    /// inheritor, the byte held on behalf of a process that has exited, and
    /// [`a_free_record_over_a_held_byte_for_a_dead_pid_is_the_fork_case`] is
    /// where it now belongs. Asserting `Pass` over the old fixture was
    /// asserting that `doctor` stays silent about the likeliest leak a Python
    /// deployment produces. The two halves below keep the row non-degenerate on
    /// the axis the old comment cared about: 31 slots nobody holds at all, and
    /// 32 held by running readers.
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
                // Half the table is genuinely empty and half is the real shape
                // of a robot's consumers: a free record with a *held* byte,
                // because a read-only participant writes no arena record (D18).
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
    ///
    /// The read-only participant D18 makes the consumer default writes **no**
    /// arena record: it takes a lock byte, writes a lock-file identity record,
    /// and leaves the participant table `FREE`. `fork` it — which is what
    /// Python's `multiprocessing` does by default on Linux — and let the parent
    /// die, and the child's inherited open file description keeps the byte held
    /// for a pid that no longer exists, with a `FREE` row over it.
    ///
    /// The revision that shipped returned `None` for every `FREE` row, so this
    /// came out `"status": "pass"` — while the paragraph the same commit added
    /// to `docs/RUNBOOK.md` told the operator `doctor --attach` reports it. The
    /// doc was the true half.
    ///
    /// Mutant: restore the bare `if p.state == SlotState::Free { return None }`
    /// in `slot_leak`. Applied: `left: Pass, right: Fired` here, while
    /// `a_free_slot_is_not_a_leak` still passes — the silence being total is
    /// exactly why no existing test caught it.
    #[test]
    fn a_free_record_over_a_held_byte_for_a_dead_pid_is_the_fork_case() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        snap.participants.push(ParticipantInfo {
            slot: 3,
            state: SlotState::Free,
            // Nothing was ever written to the arena record, so its pid field is
            // zero — which is why the finding has to take its pid from the lock
            // file.
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
    ///
    /// `doctor` shipped `"subject": "slot 8 pid 0"` on a `RESERVED` row whose
    /// message read *"/proc has no running process for it"*: the `0` is the
    /// arena record's `pid` field, still unwritten because `fill_slot` fills it
    /// after the `FREE -> RESERVED` CAS, and the `/proc` sentence is about the
    /// lock file's pid. An operator given `pid 0` has been given a number no
    /// evidence in the finding concerns.
    ///
    /// Mutant: build the subject from `p.pid` again
    /// (`format!("slot {} pid {}", p.slot, p.pid)`). Applied: it panicked on
    /// the first assertion with *left: "slot 8 pid 0"* — the shipped string,
    /// verbatim.
    #[test]
    fn the_subject_names_the_pid_the_evidence_is_about() {
        let obs = Observations::new();
        let mut snap = two_frame_snapshot(edge(1, 1, 2, 100));
        // A registrant that died inside `fill_slot`: the byte it took has been
        // released by its death, the arena record is `RESERVED` with an unset
        // pid, and the lock file names it.
        snap.participants.push(ParticipantInfo {
            slot: 8,
            state: SlotState::Reserved,
            pid: 0,
            alive: false,
            byte: LockByte::Free,
            recorded: RecordedProcess::Gone,
            recorded_pid: Some(1841),
        });
        // A slot re-registered by a live process while the lock file still
        // names the dead previous holder is not this shape — but a `LIVE`
        // record whose own pid differs from the recorded one is, and both
        // numbers are then real and worth printing.
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
    ///
    /// The `median <= 0` guard only catches a *wholly* non-monotone stream. A
    /// few inverted pairs leave a healthy positive median, but the jump back to
    /// the true timeline after an inversion becomes `worst` — so `TFT009`
    /// reports a gap of several times the median that never happened. The real
    /// fault fires too, as the id-less `out-of-order` check at error severity,
    /// and the operator then also gets a warn sending them to look for a lost
    /// publisher instead of a reordered one.
    ///
    /// The stream below is non-degenerate on both axes: its median interval is
    /// a healthy 100 ms (so the old guard does not catch it) and its recovery
    /// jump is 350 ms, comfortably past `GAP_FACTOR` x median (so the old code
    /// really did fire).
    ///
    /// Mutant: replace the `intervals.iter().any(|&d| d < 0)` guard with the
    /// old `median <= 0` test alone. Applied: `TFT009` fires with
    /// "largest gap 350.0 ms is 3.5x the median period 100.0 ms" and the first
    /// assertion fails.
    #[test]
    fn an_out_of_order_stream_is_not_reported_as_a_dropout() {
        const MS: i64 = 1_000_000;
        // Monotone 100 ms cadence except for one sample that arrives late and
        // is stamped 250 ms in the past; the next sample jumps 350 ms forward.
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
        assert_eq!(
            o.status,
            Status::Pass,
            "a reordered stream has no inter-arrival distribution to measure a gap \
             against; the fault is out-of-order, not a dropout: {o:?}"
        );

        // Non-vacuity, twice over. The stream really is out of order...
        assert!(
            !doctor::check_out_of_order(&obs).is_empty(),
            "the fixture must actually be non-monotone, or this asserts nothing"
        );
        // ...and TFT009 is not simply mute: the same cadence with the inversion
        // removed, and one genuine 400 ms hole, still reports the dropout.
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
    ///
    /// Before they had ids, `checks::run` took each finding's severity from
    /// [`doctor::Finding`], next to the code that knows why the condition is
    /// serious. An id owns its severity instead (`--exit-code` is only a usable
    /// gate if the set of ids that can fail it is knowable from the
    /// documentation), so the value is now stated twice — and `TFT018` is an
    /// *error*, so a drift would silently change what `doctor --exit-code`
    /// fails on.
    ///
    /// Mutant: give `Tft::Tft018` `Severity::Warn` in `catalogue::severity`.
    /// Applied: the out-of-order comparison fails with `left: Warn, right:
    /// Error`. Mutant B: make `doctor::check_unclaimed_dynamic` return
    /// `Finding::error`. Applied: the unclaimed comparison fails.
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

        // And both really do reach the report through their ids, rather than
        // through the id-less path they used to take.
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
    ///
    /// That is the shape a real clock step leaves, and it is not the same shape
    /// as one misplaced sample: the publisher does not stop, so invariant 6
    /// rejects every push until the clock climbs back past the newest accepted
    /// stamp. The run is therefore `back_ns / PERIOD_NS` consecutive rejected
    /// arrivals — the quantity `TFT019`'s concentration condition reads — while
    /// the *adjacent* inversion count `TFT018` reports stays 1 either way. A
    /// test that wants a stray inversion instead asks for a small `back_ns`.
    fn stepped_back(edge: u32, back_ns: i64) -> Vec<PushSample> {
        let mut stamps: Vec<i64> = (0..10).map(|i| i * PERIOD_NS).collect();
        let last = stamps[stamps.len() - 1];
        let mut t = last - back_ns;
        // `<= last` and not `< last`: the push that lands exactly on the newest
        // accepted stamp is *accepted* (replay is idempotent), so it ends the
        // rejected run rather than extending it.
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
    ///
    /// [`Domain`](tf_tree::Domain) is an open trait, so a user-declared tag
    /// carries no way to state "this clock can step". Firing on an unknown tag
    /// would hand a clean bill of health to the edge most likely to be a PTP
    /// driver that lost lock; since `SteadyDomain` (3) exists, it would also be
    /// provably wrong there — a steady clock cannot step, so the run *is* a
    /// publisher fault and `TFT018` alone is the honest answer.
    ///
    /// Mutant: make the wall-clock arm `Some((_, e))` (attribute every tag).
    /// Applied: the tag-3 edge is attributed and the first assertion fails —
    /// "only the wall-clock edge may be attributed", `left: [Some(1), Some(2)]`,
    /// `right: [Some(1)]`.
    /// Mutant B: `const WALL_CLOCK_TAG: u8 = 3`. Applied: the same assertion
    /// fails with `left: [Some(2)]`, `right: [Some(1)]`.
    /// Mutant C: give the `SimDomain` arm of `tag_refusal` the steady arm's
    /// text. Applied: the last assertion fails — "sim time must be sent to
    /// 0012, not told its clock cannot step".
    #[test]
    fn tft019_fires_only_on_the_wall_clock_tag_and_names_the_tag_it_refuses() {
        const MS: i64 = 1_000_000;
        // Edge 1 is SystemDomain (0); edge 2 is SteadyDomain (3). Both went
        // backwards by the same amount, so only the tag can explain the
        // difference in outcome.
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
        // The half it did not attribute is disclosed, since neither the
        // findings nor a skip reason can carry it here.
        let note = ClockStepEvidence::capture(&snap, &obs)
            .coverage_note(PushStream::Observed)
            .expect("a partial run discloses");
        assert!(
            note.contains("edge#2 tag 3") && note.contains("1 of 2"),
            "{note}"
        );

        // With *no* wall-clock edge left, the check skips and names the tag it
        // refused — a `pass` would read as "no clock step", an assurance about
        // clocks this check cannot give.
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

        // **Sim time is refused for its own reason.** A `/clock` reset from a
        // bag loop or a sim restart *is* a backwards step, so the steady tag's
        // "cannot have stepped at all" would be false here; telling it apart
        // from a publisher's `transform_tolerance` is what decision 0012 is,
        // and `doctor` has none of its signals offline.
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

        // And a monotone stream on a wall-clock edge is a `Pass`, not a skip:
        // the evidence is TFT018's and it is complete — every retained stream
        // was examined and none went backwards.
        let snap = chain_with_domains(0, 0);
        let obs = Observations::from_samples(steady(1, 8, 50 * MS));
        let o = tft019(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Pass, "{o:?}");
    }

    /// **A single stray inversion on a wall clock is not a clock step.**
    ///
    /// `docs/PHASE5.md` §6 asks for "a run of rejections *concentrated in a
    /// short window*" and `docs/API.md` §5.3 for "a run of rejected pushes".
    /// Firing on one misplaced sample is the same fabricated attribution the
    /// tag refusal exists to prevent, pointed the other way: it tells an
    /// operator the clock stepped when what actually happened is a publisher
    /// hiccup, and `TFT018` alone is the honest answer there.
    ///
    /// The threshold ([`CLOCK_STEP_MIN_REJECTED_RUN`], in arrivals) is this
    /// implementation's and not the spec's, so what this pins is the *shape* of
    /// the rule: below it, `TFT019` passes and discloses; at it, it fires.
    ///
    /// Mutant: `run.longest_rejected_run >= CLOCK_STEP_MIN_REJECTED_RUN` ->
    /// `run.regressions > 0`. Applied: the stray-inversion case is attributed
    /// and the first assertion fails with `left: Fired`, `right: Pass`.
    /// Mutant B: `>= CLOCK_STEP_MIN_REJECTED_RUN` -> `> CLOCK_STEP_MIN_REJECTED_RUN`.
    /// Applied: the exactly-at-threshold stream is no longer attributed and the
    /// last assertion fails with `left: Pass`, `right: Fired`.
    #[test]
    fn tft019_needs_a_run_of_rejections_not_a_single_inversion() {
        let snap = chain_with_domains(0, 0);

        // Two rejected arrivals: one sample out of place, not a clock that
        // stepped. TFT018 still reports it — nothing is suppressed.
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
        // A pass that covers less than it looks like it does says so.
        let note = ClockStepEvidence::capture(&snap, &obs)
            .coverage_note(PushStream::Observed)
            .expect("a diffuse wall-clock run is disclosed rather than silently passed");
        assert!(
            note.contains("edge#1 longest run 2")
                && note.contains(&format!("at least {CLOCK_STEP_MIN_REJECTED_RUN}")),
            "{note}"
        );

        // Exactly at the threshold: a step of `CLOCK_STEP_MIN_REJECTED_RUN`
        // publish periods rejects that many pushes on the way back up.
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
    ///
    /// A live push stream is reconstructed from a ring being written while it is
    /// read, so a slot at the old end can already hold the next lap's sample.
    /// That artifact has exactly this check's signature, and attributing it to
    /// an NTP step would put a fabricated cause on a fabricated effect — worse
    /// than `TFT018`'s silence, because it names a culprit.
    ///
    /// **And the skip names the source that can answer instead.** An operator
    /// who only ever meets a `skip` line has to be told where the evidence
    /// lives — a check whose silence reads as an all-clear is worse than no
    /// check.
    ///
    /// Mutant: `if let Some(why) = inp.stream.no_rejected_arrivals()` ->
    /// `if let Some(why) = None::<&str>` in `tft019`. Applied: the `Skipped`
    /// match panics with "expected a skip on a replayed stream, got Fired".
    /// Mutant B: delete the `--from-bag` sentence from
    /// `PushStream::RingsUnderWriter`'s `no_rejected_arrivals` reason. Applied:
    /// the second assertion fails — "the skip has to point at the source that
    /// can answer".
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

        // Non-vacuity: the same stream, recorded as it arrived, is attributed.
        inp.stream = PushStream::Observed;
        assert_eq!(tft019(&inp).status, Status::Fired);
    }

    /// **A stream replayed from an arena at rest cannot show an inversion, so
    /// `TFT018` skips there too rather than passing.**
    ///
    /// This is the finding that made [`PushStream`] a four-valued enum instead
    /// of a `live: bool`. A frozen `.tft` has no concurrent writer, so the
    /// live-arena skip reason — a torn window — does not apply to it, and keying
    /// on liveness would have run the check and passed it. It would have passed
    /// **every** `.tft`, because `SampleRing::push` rejects an out-of-order
    /// stamp and a ring therefore holds only accepted pushes: the evidence is
    /// absent from the arena, not merely hard to read. A guaranteed pass on the
    /// exact fault a check exists to name is the fabricated all-clear this
    /// catalogue refuses everywhere else.
    ///
    /// The stream below is deliberately the *same* inverted one the recorded
    /// case fires on — it is not reachable from a real arena, and that is the
    /// point: even handed the evidence, this variant must refuse it, because in
    /// production it would never have it.
    ///
    /// Mutant: make `PushStream::RingsAtRest` return `None` from
    /// `no_rejected_arrivals`. Applied: both `Skipped` matches panic with
    /// "expected a skip on an arena at rest, got Fired".
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

        // Non-vacuity: the same stream, read out of a recording, is judged.
        inp.stream = PushStream::Recorded;
        assert_eq!(tft018(&inp).status, Status::Fired);
        assert_eq!(tft019(&inp).status, Status::Fired);
    }

    /// **`TFT001` skips on a recording because a bag has no publisher identity,
    /// and the reason says which of the two facts is missing.**
    ///
    /// `docs/PHASE4.md` §1.3 makes multi-publisher conflict the falsifiable
    /// prediction about real stacks, so this is the check a stranger's bag is
    /// most wanted for — and it is the one a bag cannot answer. Saying "a live
    /// arena's rings remember the current owner" there would be a true sentence
    /// about the wrong source.
    ///
    /// Mutant: make `PushStream::Recorded` return the `RingsAtRest` reason from
    /// `no_writer_identity`. Applied: the `PHASE4.md §1.3` assertion fails.
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
        // Non-vacuity: the fixture's stream does carry pids and does run.
        inp.stream = PushStream::Observed;
        assert_eq!(tft001(&inp).status, Status::Pass);
    }

    /// **`TFT019` explains `TFT018`; it does not demote it.**
    ///
    /// `docs/PHASE5.md` §6's amendment is explicit: rejected pushes are lost
    /// data whatever caused them, so `TFT018` stays an error and keeps failing
    /// `doctor --exit-code` while `TFT019` is a warn beside it. The failure this
    /// pins is the tempting one — deciding that an explained fault is not a
    /// fault — which would silently stop a fleet's CI gating on lost transforms.
    ///
    /// Mutant: move `Tft::Tft019` into the `Severity::Error` arm of
    /// `catalogue::severity`. Applied: the first assertion fails with
    /// `left: Error`, `right: Warn`.
    /// Mutant B: move `Tft::Tft018` into the `Severity::Warn` arm. Applied: the
    /// second assertion fails with `left: Warn`, `right: Error`.
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

        // Suppressing the *explanation* must not disarm the gate, and
        // suppressing the *detector* must — that is what an id being a contract
        // means for `--exit-code`.
        let only_019 = run(&inp, &BTreeSet::from([Tft::Tft019]));
        assert!(only_019.has_error(), "TFT019 was never what gated");
        let only_018 = run(&inp, &BTreeSet::from([Tft::Tft018]));
        assert!(!only_018.has_error(), "TFT018 is the id that gates");
    }

    /// **`TFT019` fires on exactly `TFT018`'s evidence — the same function, not
    /// a second scan written to the same rule.**
    ///
    /// The amendment calls it "an attribution, not a second detector". Two
    /// independent walks of `obs` would agree on the day they were written and
    /// drift on the first change to either, at which point `doctor` would report
    /// a clock step on an edge it did not report as out of order, or the
    /// reverse.
    ///
    /// **The fixture is sized to the shipped threshold, and that is the whole
    /// design of this test.** Invariant 6 *accepts* a repeated stamp — replay is
    /// idempotent — so a stream of identical stamps is one the real producer
    /// finds nothing in at all. A producer that had drifted by one character to
    /// `<=` counts every repeat as a rejection, so the stream is
    /// [`CLOCK_STEP_MIN_REJECTED_RUN`]` + 1` samples long: exactly enough for
    /// the drifted count to clear [`CLOCK_STEP_MIN_REJECTED_RUN`] and be
    /// *attributed*. A shorter stream is not equivalent — the drifted run would
    /// land in `diffuse` instead, and the only thing left to notice it would be
    /// the wording of a coverage note, which is not the invariant this pins.
    ///
    /// Mutant: in [`ClockStepEvidence::capture`], replace
    /// `doctor::out_of_order_runs(obs)` with a local re-derivation over
    /// `obs.by_edge()` — the body of [`doctor::out_of_order_runs`] copied in
    /// with both of its `<` stamp tests written `<=`. Applied: `TFT018` still
    /// passes on the equal-stamps stream, because it still calls the real
    /// producer, while `TFT019` attributes it, and the `tft019` assertion fails
    /// with `left: Fired`, `right: Pass` — the two had drifted apart by one
    /// character.
    #[test]
    fn tft019_considers_exactly_the_edges_tft018_fired_on() {
        const MS: i64 = 1_000_000;
        let snap = chain_with_domains(0, 0);
        // Repeated stamps are *accepted* by invariant 6 — replay is idempotent —
        // so neither check may treat them as a regression.
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

        // And where TFT018 does fire, TFT019 considers exactly its edges.
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
    ///
    /// A live push stream is reconstructed from a ring that is being written
    /// while it is read, so a slot at the old end of the window can already hold
    /// the next lap's sample — a forward jump followed by a step back, which is
    /// this check's exact signature. Before the id, the live case was silently
    /// not run and the report said nothing about it.
    ///
    /// Mutant: make `PushStream::RingsUnderWriter` return `None` from
    /// `no_rejected_arrivals`. Applied: the status is `Fired` and the `Skipped`
    /// match panics.
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
        // Non-vacuity: the same stream off a live arena does fire.
        inp.stream = PushStream::Observed;
        assert_eq!(tft018(&inp).status, Status::Fired);
    }

    /// **`TFT007` compares only where a rate was declared, and an undeclared
    /// edge is not compared against zero.**
    ///
    /// `EdgeRecord::nominal_rate_mhz == 0` means *not declared*, not *declared
    /// as 0 Hz*. Treating it as a rate makes every undeclared edge deviate by
    /// infinity, so a correct arena where nobody wrote a `rate_hz` would report
    /// a warn on every edge — the fabricated finding this catalogue exists to
    /// avoid, and the reason the field went unread until now.
    ///
    /// The fixture is non-degenerate on both axes: three edges, two declaring
    /// and one not, and the two declaring differ in whether they hold their
    /// rate. The declared value is not the observed one by accident — the slow
    /// edge runs at exactly half.
    ///
    /// Mutant: read the declaration as `e.nominal_rate_mhz.unwrap_or(0)` in
    /// `rate_evidence` and drop the `NotDeclared` arm. Applied: undeclared
    /// edge#3 fires at `+inf%` and the assertion about which edges fired fails.
    /// Mutant B: invert the `effect` selection to `if ratio < 1.0`. Applied: the
    /// slow edge is told its ring "retains proportionally less history" — the
    /// opposite diagnosis — and the assertion on the explanation fails. The
    /// numbers alone do not catch it; the sentence is what an operator acts on.
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

        // Publishing *faster* than declared is a finding too: the ring was
        // sized from rate_hz x history_secs, so it now retains proportionally
        // less history than every consumer was tuned against.
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

        // And a rate inside the tolerance band is not a finding: 18 Hz against
        // a declared 20 Hz is a 10% miss, which is jitter and load.
        let obs = Observations::from_samples(steady(1, 12, 55_555_555));
        let o = tft007(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.status, Status::Pass, "{o:?}");
    }

    /// **An arena where nothing declares a rate skips `TFT007` with a reason
    /// instead of passing.**
    ///
    /// A `pass` here would be an active claim that every edge publishes at its
    /// intended rate, made by a check that had no intended rate to consult.
    /// That is the difference `Status::Skipped` carries a mandatory reason for.
    ///
    /// Mutant: replace `if declared == 0` with `if false` in `tft007`. Applied:
    /// the status is `Pass` and the `match` panics with "expected a skip".
    /// Mutant B: drop the `e.kind != EdgeKind::Dynamic` guard from
    /// `rate_evidence`. Applied: the static-edge case below counts as declared,
    /// the check reports `Pass` instead of skipping, and its `match` panics —
    /// a `pass` earned by an edge that cannot publish at all.
    #[test]
    fn tft007_skips_when_no_edge_declares_a_rate() {
        const MS: i64 = 1_000_000;
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        // A full, healthy, measurable stream — so the skip is about the missing
        // declaration and not about missing samples.
        let obs = Observations::from_samples(steady(1, 12, 50 * MS));
        let o = tft007(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        match &o.status {
            Status::Skipped(why) => assert!(
                why.contains("nominal rate") && why.contains("rate_hz"),
                "the skip must name the missing evidence and how to supply it: {why}"
            ),
            other => panic!("expected a skip, got {other:?}"),
        }

        // A *static* edge carrying a rate is not a declaration either: it never
        // publishes, so there is no stream to hold or miss one. Reachable only
        // from a hand-built or corrupt record — the builder writes no nominal
        // for a static edge — which is exactly why the guard is not obviously
        // dead code.
        let snap = two_frame_snapshot(EdgeInfo {
            kind: EdgeKind::Static,
            capacity: 0,
            nominal_rate_mhz: Some(20_000),
            ..edge(1, 1, 2, 0)
        });
        match &tft007(&inputs(&snap, &obs, &[], Clock::Wall(0))).status {
            Status::Skipped(_) => {}
            other => panic!("a static edge cannot declare a publish rate, got {other:?}"),
        }

        // Non-vacuity: the same stream against a declared rate does run.
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
    ///
    /// This is the state an operator is in seconds after bringup, during a
    /// publisher restart, and — worst — on an edge whose publisher has stopped
    /// dead, which reaches `rate_evidence` with no samples at all. A declaration
    /// exists, so the "nothing declares" skip does not fire; nothing is
    /// comparable, so no finding is produced and [`rate_coverage_note`] stays
    /// silent too. Reporting `pass` there would tell a fleet operator that a
    /// stopped publisher is publishing at its intended rate.
    ///
    /// Mutant: restore the guard to `if declared == 0`. Applied: the status is
    /// `Pass` and the first `match` panics with "expected a skip".
    /// Mutant B: make the `TooFewIntervals` arm `continue` instead of counting.
    /// Applied: `declared` stays 0, the skip reason becomes the "no edge
    /// declares a nominal rate" one — false, two of them do — and the assertion
    /// on "retained intervals" fails.
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
            // Edge 2 appears in no sample at all: `by_edge` has no entry, which
            // is the `samples: None` path and not the short-slice one.
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
            rate_coverage_note(&snap, &obs),
            None,
            "the note stays silent here, which is why the skip has to carry the disclosure"
        );

        // Non-vacuity: one measurable edge is enough to make the check run, and
        // then the note — not the skip — carries what it did not cover.
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
        assert!(rate_coverage_note(&snap, &obs)
            .expect("a partial run discloses itself")
            .contains("compared 1 of 2"));
    }

    /// **`RATE_TOLERANCE` is the band, and one milli-hertz either side of it
    /// decides fired from passed.**
    ///
    /// The constant is offered as the spec owner's dial, so a change to it must
    /// be visible in a test rather than silently widening what `doctor`
    /// certifies as healthy. Both cases run the *same* 25 Hz stream and differ
    /// only in the declared rate by 1 mHz — 25 Hz is 20.0019% above 20.833 Hz
    /// and 19.9962% above 20.834 Hz — so nothing but the threshold can explain
    /// the difference in outcome.
    ///
    /// Mutant: `RATE_TOLERANCE: f64 = 0.35`. Applied: the first case reports
    /// `Pass` and its assertion fails. Mutant B: `0.10`. Applied: the second
    /// case fires and its assertion fails. A test pinned only to `(0.10, 0.50)`
    /// deviations survives both.
    #[test]
    fn the_rate_tolerance_band_is_pinned_at_its_edge() {
        // 25 Hz exactly: 40 ms is representable, so the observed side carries
        // no rounding of its own into the comparison.
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
    ///
    /// The report has three statuses and none of them is "ran, half blind", so
    /// a partial run is disclosed through `Meta.notes` — the same mechanism
    /// `TFT015`'s missing participants row uses. Without it, an arena where one
    /// edge declares a rate and eleven do not reports a bare `pass` for
    /// `TFT007`, which reads as a statement about all twelve.
    ///
    /// Mutant A: make `rate_coverage_note` return `None` unconditionally.
    /// Applied: the `expect` fires. Mutant B: drop the `comparable == 0` term
    /// from its guard. Applied: the last assertion fails — the report would
    /// then carry a skip reason and a coverage note that contradict each other.
    /// Mutant C: drop the `e.kind != EdgeKind::Dynamic` guard from the note's
    /// own loop. Applied: the static edge below lands in the `undeclared`
    /// bucket and the note reads "compared 1 of 4 dynamic edge(s)" — a sentence
    /// whose noun contradicts its arithmetic, and the reason the note counts
    /// its denominator itself instead of using `snap.edges.len()`.
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
                // A static edge cannot declare a rate and cannot publish one,
                // so it belongs in neither the numerator nor the denominator:
                // a real arena is mostly static edges, and counting them would
                // make the note read as near-total blindness on a healthy tree.
                EdgeInfo {
                    kind: EdgeKind::Static,
                    capacity: 0,
                    ..edge(4, 4, 5, 0)
                },
            ],
            participants: live_writer(),
        };
        let mut events = steady(1, 12, 50 * MS);
        // Declared, but only three intervals: a rate measured from that is
        // noise, so it counts as not compared rather than compared badly.
        events.extend(steady(2, 4, 50 * MS));
        let obs = Observations::from_samples(events);

        let note = rate_coverage_note(&snap, &obs).expect("a partial run must disclose itself");
        assert!(
            note.contains("compared 1 of 3")
                && note.contains("1 declare no nominal rate")
                && note.contains("1 have fewer than 8"),
            "{note}"
        );

        // Every edge declared and measurable: nothing to disclose.
        let mut a = edge(1, 1, 2, 100);
        a.nominal_rate_mhz = Some(20_000);
        let full = two_frame_snapshot(a);
        let obs = Observations::from_samples(steady(1, 12, 50 * MS));
        assert_eq!(rate_coverage_note(&full, &obs), None);

        // Nothing declared: the check skips and says so itself, so a note here
        // would be a second, weaker statement of the same fact.
        let none = two_frame_snapshot(edge(1, 1, 2, 100));
        assert_eq!(rate_coverage_note(&none, &obs), None);
    }

    /// **An arena that has served no lookups must not read as a healthy one.**
    ///
    /// `TFT010`'s evidence is entirely the §5 counters, and those are
    /// incremented by *lookups*. A bag-built arena has served none — `ingest`
    /// pushes and never reads — so `extrap_before + extrap_after` is zero on
    /// every edge, the finding loop never runs, and the check reported `pass`:
    /// an all-clear on an extrapolation hotspot nothing could have detected.
    /// It is the same defect `TFT007` had against an undeclared rate.
    ///
    /// The gate is on the evidence, not on the source, so the *second* half of
    /// this test is the real one: one lookup anywhere in the arena makes a zero
    /// mean zero, and the check must run again.
    ///
    /// Mutant: delete the `lookups == 0` arm of `no_counter_evidence`. Applied:
    /// the first assertion fails with `Pass`.
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
            Status::Skipped(why) => assert!(
                why.contains("served no lookups"),
                "the skip must name the reason a reader can act on: {why}"
            ),
            other => panic!("an arena nobody has read must not report a verdict: {other:?}"),
        }

        // One successful lookup and nothing else changes: the counters now
        // distinguish "no failures" from "nothing counted", so the check runs.
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
    ///
    /// It reports two independent pieces of evidence under one id, so losing
    /// one is a disclosure and losing both is a skip. A bag loses both at once
    /// — no lookups have been served, and a recording carries no arrival delay
    /// — and the `pass` that came out of that said "your rings are big enough"
    /// after walking two empty sets.
    ///
    /// Mutant: make the guard `if counters.is_some() || delays.is_some()`.
    /// Applied: the `Observed` case skips and the last assertion fails.
    #[test]
    fn tft011_skips_when_neither_half_has_evidence_and_runs_when_either_does() {
        let snap = two_frame_snapshot(edge(1, 1, 2, 100));
        let obs = Observations::new();
        let unexercised = [EdgeStats {
            edge: 1,
            ..EdgeStats::default()
        }];

        // A recording: counters unexercised *and* no arrival delays.
        let mut inp = inputs(&snap, &obs, &unexercised, Clock::Wall(0));
        inp.stream = PushStream::Recorded;
        match tft011(&inp).status {
            Status::Skipped(why) => {
                assert!(
                    why.contains("served no lookups"),
                    "the counter half's reason is missing: {why}"
                );
                assert!(
                    why.contains("recorder's clock"),
                    "the capacity-vs-latency half's reason is missing: {why}"
                );
            }
            other => panic!("neither half had evidence and it still reported: {other:?}"),
        }

        // The fixture: counters unexercised, but the stream records an arrival
        // delay per sample, so half two is a real result and the check runs.
        let mut inp = inputs(&snap, &obs, &unexercised, Clock::Wall(0));
        inp.stream = PushStream::Observed;
        assert_eq!(tft011(&inp).status, Status::Pass);

        // A live arena that has served lookups: half one is a real result.
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
    ///
    /// "Rebuild the engine with the feature on" and "exercise the arena" are
    /// different instructions, and the feature check has to come first because
    /// a build without counters also reads zero everywhere — reporting *that*
    /// as "this arena has served no lookups" would send a reader to run a
    /// consumer against an engine that will never count it.
    ///
    /// Mutant: swap the two arms of `no_counter_evidence`. Applied: the first
    /// assertion fails.
    #[test]
    fn the_counters_feature_and_an_unexercised_arena_are_different_skips() {
        let off = no_counter_evidence(false, &[]).expect("a build without counters has no verdict");
        assert!(off.contains("`counters` feature"), "{off}");

        let unexercised = [EdgeStats {
            edge: 1,
            ..EdgeStats::default()
        }];
        let on = no_counter_evidence(true, &unexercised).expect("zero counters carry no verdict");
        assert!(on.contains("served no lookups"), "{on}");

        // A failed lookup counts as exercise just as much as a successful one:
        // it is the same increment site.
        let failed = [EdgeStats {
            edge: 1,
            extrap_before: 1,
            ..EdgeStats::default()
        }];
        assert_eq!(no_counter_evidence(true, &failed), None);
    }
}
