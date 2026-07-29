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
//! reason — **three** unconditionally, and **five** more depending on what the
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
//!   difference against the header stamp. Nothing records one: `SampleRing::push`
//!   stores the stamp the publisher supplied and no second timestamp of its own.
//!
//! And the conditional ones, which depend on what the arena, the engine build
//! and the host can supply:
//!
//! * **`TFT001`** (multi-publisher conflict) is skipped *on a live arena only*:
//!   a ring remembers the current claim owner, not the sequence of processes
//!   that wrote into it, so every reconstructed sample carries the same pid.
//!   Against the fixture's recorded push stream it runs.
//! * **`TFT005`** (stamps in the future) is skipped when the arena's stamps do
//!   not share an epoch with the system clock — see [`Clock`].
//! * **`TFT007`** (rate deviates from nominal) is skipped when **no** edge in
//!   the arena declares a nominal rate. It is no longer structurally blind:
//!   `EdgeRecord::nominal_rate_mhz` is written at declaration time from
//!   `EdgeCfg::nominal_rate_hz`, and a topology file's `rate_hz` reaches it
//!   through `TopologyConfig::builder`. An arena built without one still skips,
//!   because a `0` means *undeclared* and not *0 Hz* — see [`tft007`].
//! * **`TFT010`**/**`TFT016`** are skipped when the engine has no counters and
//!   when the host is not Linux, respectively.
//!
//! [`tf_tree_bridge`]: https://docs.rs/tf_tree_bridge

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use tf_tree::{EdgeId, EdgeKind, Tree};
use tf_tree_bench::fixture::PushSample;

use crate::catalogue::{CheckOutcome, Finding, Report, Severity, Tft, Uncatalogued};
use crate::doctor::{self, Observations, Snapshot};
use crate::hostfacts::{HostFacts, MemLock, Thp};

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
    /// The arena's newest stamp is within [`ABSURD_HORIZON_NS`] of the system
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
    /// [`ABSURD_HORIZON_NS`] does double duty: it is the domain-agreement
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
    /// Whether the push stream was reconstructed from a live arena rather than
    /// recorded as it happened.
    pub live: bool,
    /// Whether the engine compiled `docs/PHASE5.md` §5's counters in.
    pub counters: bool,
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
                "no per-publisher arena receipt time is recorded — a push stores the \
                 publisher's stamp and nothing of its own to difference against",
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
        };
        o.suppressed = suppress.contains(&check);
        outcomes.push(o);
    }

    // The two Phase 1 checks §6 assigns no identifier to. See the
    // `crate::catalogue` module docs for why they are not forced into one.
    // Severity comes from the finding, not from a literal here: `doctor` sets
    // it next to the code that knows why the condition is serious, and
    // restating it at this seam is how the two answers drift apart. `--exit-code`
    // keys on it, so a drift is a gate that silently changes meaning.
    let mut uncatalogued = Vec::new();
    for f in doctor::check_unclaimed_dynamic(inp.snap) {
        uncatalogued.push(Uncatalogued {
            check: "unclaimed-dynamic",
            severity: Severity::from(f.severity),
            subject: "tree".to_owned(),
            message: f.message,
        });
    }
    if !inp.live {
        for f in doctor::check_out_of_order(inp.obs) {
            uncatalogued.push(Uncatalogued {
                check: "out-of-order",
                severity: Severity::from(f.severity),
                subject: "tree".to_owned(),
                message: f.message,
            });
        }
    }

    Report {
        outcomes,
        uncatalogued,
    }
}

/// `TFT001` — more than one writer pid on one edge.
fn tft001(inp: &Inputs<'_>) -> CheckOutcome {
    if inp.live {
        return CheckOutcome::skipped(
            Tft::Tft001,
            "a live arena's push stream is reconstructed from the rings, which remember the \
             current claim owner and not the sequence of writers, so every sample carries one pid",
        );
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
    /// `EdgeRecord::nominal_rate_mhz` is 0 — nothing declared a rate for this
    /// edge, so there is nothing to deviate from.
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
fn rate_evidence(e: &doctor::EdgeInfo, samples: Option<&Vec<&PushSample>>) -> RateEvidence {
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
/// # Why the whole check skips when nothing declares
///
/// A zero is "not declared", not "declared as 0 Hz". Comparing an observed rate
/// against it makes every edge deviate by infinity, which is a fabricated
/// finding on a correct arena — the failure this catalogue exists to avoid.
/// When *some* edges declare, the check runs on those and
/// [`rate_coverage_note`] states what it did not cover, because a bare `pass`
/// would otherwise read as "every edge publishes at its intended rate".
fn tft007(inp: &Inputs<'_>) -> CheckOutcome {
    let by_edge = inp.obs.by_edge();
    let mut out = Vec::new();
    let mut declared = 0usize;
    for e in &inp.snap.edges {
        match rate_evidence(e, by_edge.get(&e.id)) {
            RateEvidence::NotDeclared => continue,
            RateEvidence::TooFewIntervals => declared += 1,
            RateEvidence::Comparable {
                declared_hz,
                observed_hz,
            } => {
                declared += 1;
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
    if declared == 0 {
        return CheckOutcome::skipped(
            Tft::Tft007,
            "no edge in this arena declares a nominal rate (EdgeRecord::nominal_rate_mhz is 0 \
             on all of them); declare one with rate_hz in the topology file, or via \
             EdgeCfg::nominal_rate_hz, and this check has something to compare against",
        );
    }
    CheckOutcome::ran(Tft::Tft007, out)
}

/// The disclosure that pairs with [`tft007`]: which edges its result covers.
///
/// `None` when the answer is unambiguous — nothing declared a rate (the check
/// skipped and says so itself), or every declared edge was measurable. A note
/// is emitted only for the middle case, where `pass` is true of the edges that
/// were compared and silent about the rest.
#[must_use]
pub fn rate_coverage_note(snap: &Snapshot, obs: &Observations) -> Option<String> {
    let by_edge = obs.by_edge();
    let (mut comparable, mut too_few, mut undeclared) = (0usize, 0usize, 0usize);
    for e in &snap.edges {
        if e.kind != EdgeKind::Dynamic {
            continue;
        }
        match rate_evidence(e, by_edge.get(&e.id)) {
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
fn tft010(inp: &Inputs<'_>) -> CheckOutcome {
    if !inp.counters {
        return CheckOutcome::skipped(
            Tft::Tft010,
            "the engine was built without the `counters` feature (PHASE5 §5.5), so every counter \
             reads zero and \"no failures\" cannot be told from \"nothing counted\"",
        );
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
///    the largest observed publish latency — which needs a recorded push stream
///    and therefore only fires on the fixture.
fn tft011(inp: &Inputs<'_>) -> CheckOutcome {
    let mut out = Vec::new();
    if inp.counters {
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

/// `TFT014` — a claim that outlived its owner.
///
/// The claim word names a *participant slot*, not a pid. A slot that no longer
/// resolves to a `LIVE` identity while the claim is still held is precisely a
/// leaked claim: the writer is gone and nothing released its edge, so no other
/// process can take it.
///
/// A record caught mid-claim (`CLAIMING`) also resolves to no slot, and is
/// **excluded**, because from a snapshot the two are not the same thing. A
/// normal handoff parks `CLAIMING` in the record for the few instructions
/// between winning it and publishing an identity, so a `doctor` run that lands
/// in that window on a *healthy* arena would report a leak on an edge whose
/// publisher is restarting. `Tree::reap` may act on `CLAIMING` only because it
/// first consults an independent liveness source — `probe_claim` on the lock
/// file — and a live claimer in that window is protected by the lock still
/// being held. This check takes no such probe, so it has no evidence with which
/// to tell a dead mid-claim from a live one, and reports neither. The cost is a
/// claimer killed inside that window going unreported; the alternative is a
/// warn-severity false positive on every publisher restart, which is what
/// teaches operators to ignore the check.
///
/// **This rests entirely on the owner word being decoded correctly.** It is
/// `(epoch << 16) | (slot + 1)`, so a hand-rolled `word - 1` resolves every
/// live claim to nothing and reports every claimed edge as leaked;
/// `Snapshot::capture` uses `tf_tree_core::edge::slot_of`, and
/// `doctor::tests::a_held_claim_resolves_to_the_writers_pid` pins it.
fn tft014(inp: &Inputs<'_>) -> CheckOutcome {
    let mut out = Vec::new();
    for e in &inp.snap.edges {
        if e.claimed && !e.claiming && e.owner_pid == 0 {
            out.push(Finding::on_edge(
                Tft::Tft014,
                e.id,
                inp.snap.edge_label(e),
                "claim is held by a participant slot that no longer resolves to a live \
                 identity — the writer is gone and the edge cannot be reclaimed",
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
            owner_pid: 4711,
            newest_stamp: Some(1_000_000_000),
            nominal_rate_mhz: None,
        }
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
            live: false,
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

        // Publishing *faster* than declared is a finding too: the ring was
        // sized from rate_hz x history_secs, so it now retains proportionally
        // less history than every consumer was tuned against.
        let obs = Observations::from_samples(steady(1, 12, 20 * MS)); // 50 Hz
        let o = tft007(&inputs(&snap, &obs, &[], Clock::Wall(0)));
        assert_eq!(o.findings.len(), 1, "{o:?}");
        assert!(
            o.findings[0].message.contains("+150%"),
            "{}",
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

        // Non-vacuity: the same stream against a declared rate does run.
        let mut declared = edge(1, 1, 2, 100);
        declared.nominal_rate_mhz = Some(20_000);
        let snap = two_frame_snapshot(declared);
        assert_eq!(
            tft007(&inputs(&snap, &obs, &[], Clock::Wall(0))).status,
            Status::Pass
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
            ],
            edges: vec![declared, short, edge(3, 3, 4, 100)],
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
}
