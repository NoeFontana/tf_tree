//! Time domains and clock resets — `docs/PHASE4.md` §5.5, NORMATIVE.
//!
//! # A backward clock jump is not a stream of bad samples
//!
//! Bag loops and sim resets move `/clock` backwards. Phase 1 rejects
//! non-monotonic stamps **one at a time**, so a bridge that just forwards them
//! produces one `NonMonotonicStamp` per message per edge, forever, while the
//! tree quietly stops updating. The operator sees a log full of errors and a
//! robot that has frozen, and the two do not obviously have the same cause.
//!
//! §5.5's answer is that the bridge detects the jump *once* and either stops or
//! recreates the arena — a decision, taken at the moment the clock moves,
//! instead of a symptom repeated at the message rate.
//!
//! # And the domain is checked at startup, not at first message
//!
//! §5.5 is NORMATIVE about this too: the bridge refuses to write to an edge
//! whose declared domain differs from its own, and **fails at startup**. Sim and
//! real transforms in one arena is a class of bug worth making impossible, and
//! discovering it at the first message means discovering it after the arena
//! already contains a mixture.
//!
//! # Three rules were tried on `/tf` stamps alone, and all three were wrong
//!
//! `docs/decisions/0011` narrowed one global [`ClockGuard`] to one per edge and
//! promoted a regression to "the clock moved" by a quorum. Each successive
//! refinement was caught with a reproduction:
//!
//! 1. **One global guard.** A publisher's `transform_tolerance` — AMCL and
//!    `robot_localization` date `map -> odom` 0.1 to 1.0 s into the *future* —
//!    is a steady offset of one edge relative to another, larger than any
//!    threshold, and against one shared high-water mark it is indistinguishable
//!    from a rewind. A correctly configured robot latched the bridge.
//! 2. **Per-edge guards plus a quorum over distinct *edges*.** One node owning
//!    two dynamic edges regresses both when it restarts, so a single process
//!    hiccuping formed a quorum of edges — the false halt the quorum existed to
//!    remove, reintroduced by the mechanism meant to remove it.
//! 3. **A quorum floored by `Authority::distinct_owners()`.** Two defects. At
//!    boot the second publisher has not published yet (AMCL waits for a map), so
//!    the floor is 1 and the wheel driver's first regression latches a permanent
//!    halt. And the then-`Publisher::UnknownGid` — since replaced by
//!    [`crate::Publisher::Gid`], which compares on the GID rather than on the
//!    name — and [`crate::Publisher::Unattributed`] were *unit* variants, so on
//!    an RMW without endpoint introspection every publisher compared equal, the
//!    floor was permanently 1, and every single-edge regression halted — which makes
//!    attribution a **correctness dependency**, the one thing §5.3 forbids in as
//!    many words.
//!
//! The common root is not any of the three rules. It is that all three infer a
//! property of the **time source** from observations of the very signal under
//! suspicion, anchored on proxies — an edge, an owner, a transform ordinal —
//! that are not physical time.
//!
//! # The five principles this module is built on
//!
//! - **P1. Prefer the authoritative signal to inference.** ROS 2 *publishes*
//!   clock jumps: `rcl_clock_add_jump_callback`, surfaced by rclcpp as
//!   `Clock::create_jump_callback`. A `/clock` regression **is** the event,
//!   observed once at its source. [`crate::Ingest::note_time_jump`] is the path for it,
//!   and it needs no threshold, no window and no corroboration.
//! - **P2. A detector's reference clock must be independent of the clock under
//!   test.** `RCL_STEADY_TIME` is monotonic and is not affected by
//!   `use_sim_time`; it is the reference. A publisher's stamp never is.
//! - **P3. Windows are physical time, never event counts.** `0011` measured its
//!   correlation window in transforms offered because "this crate does not have
//!   a clock". It does now — [`SteadyNanos`], supplied by the caller — so the
//!   window is nanoseconds and a stream's message rate no longer changes what
//!   "at the same time" means.
//! - **P4. Time is injected, never read ambiently.** Nothing in this crate calls
//!   `Instant::now()`. The receipt clock is read **once per message** by the
//!   caller (the rclcpp bridge reads `rclcpp::Clock(RCL_STEADY_TIME)` at
//!   callback entry) and rides in on [`crate::Sample::received`], so the tests
//!   stay deterministic and the hot path stays free of syscalls.
//! - **P5. A diagnostic may never become a correctness dependency** (§5.3).
//!   Attribution quality now changes only how well a clock event is *described*.
//!   It cannot change whether the bridge halts.
//!
//! # The degradation ladder — this is what kills the bug class
//!
//! | Evidence | Action |
//! | --- | --- |
//! | An authoritative jump signal ([`crate::Ingest::note_time_jump`]) | [`OnClockReset`] — exact |
//! | A common-mode step across **≥ 2** publishers ([`OffsetTable`]) | [`OnClockReset`] |
//! | A single-source regression ([`ClockGuard`]) | **Drop, count, diagnose. Never halt.** |
//!
//! Because the bridge never halts on one witness, there is **no floor** and so
//! nothing about a floor to get wrong. Defect 3 above is not fixed, it is
//! unrepresentable. Phase 1 rejects those stamps anyway, so the arena is
//! protected whatever the ladder concludes; what the ladder decides is only
//! whether the *bridge* stops, and stopping is the expensive answer.
//!
//! # Inference, when it is still needed, is common-mode rejection
//!
//! The authoritative path is `rclcpp`-only. A non-ROS caller, a system-clock
//! step (NTP), and defence in depth all still want a fallback — so [`OffsetTable`]
//! keeps, per publisher,
//!
//! ```text
//! offset = sample.stamp_nanos - sample.received.0
//! ```
//!
//! against a smoothed baseline. **A publisher's `transform_tolerance` is exactly
//! this offset**: it is measured and subtracted, so it stops looking like a jump
//! at all. That dissolves defect 1 rather than working around it — there is no
//! threshold to choose between "tolerance" and "rewind", because the tolerance
//! is no longer in the residual.
//!
//! A *step* in one publisher's offset is still only one witness. What promotes
//! it is **agreement**: a real `/clock` step moves every publisher by the *same*
//! amount, and independent restarts do not. Two publishers stepping by
//! −5.000 s and −5.001 s inside a second of each other share a cause; two
//! stepping by −5 s and −0.4 s are two faults that happened to collide. That
//! also makes **forward** jumps detectable, which a backward-regression watcher
//! structurally cannot see at all.
//!
//! The per-edge [`ClockGuard`] survives all of this unchanged in what it
//! measures — one publisher's regression against its own last accepted stamp,
//! which is Phase 1 invariant 6 restated — and changed in what it may conclude:
//! it makes the per-edge **drop** decision and nothing else.

use crate::interner::StrInterner;

/// A reading of a local **steady** (monotonic) clock, in nanoseconds.
///
/// A distinct type from a publisher's stamp because confusing the two is the
/// entire bug class this design removes. Never derived from `/clock`, never from
/// a publisher. `repr(transparent)` so it costs nothing at the C boundary.
///
/// # Where it comes from
///
/// Online, `rclcpp::Clock(RCL_STEADY_TIME).now().nanoseconds()`, read **once per
/// `TFMessage`** at subscription-callback entry and copied onto every
/// [`crate::Sample`] the message expands into — not once per transform, which
/// would put a clock read on a 1 kHz path and would give transforms from one
/// message different receipt times, so that one measurement became twenty.
/// Offline, the recording's log time (`RawRecord::log_time_ns`), which is when
/// the recorder wrote the message rather than when a publisher stamped it.
///
/// # The epoch is arbitrary, and [`SteadyNanos::UNKNOWN`] exploits that
///
/// A steady clock's zero is unspecified — on Linux it is boot. Only
/// *differences* mean anything, so this type is only ever subtracted, and
/// `0` is free to serve as "no receipt clock was supplied". A caller that cannot
/// produce one (the `.tfstream` replay behind `tf_tree topology --discover` has
/// no log-time column at all) leaves it at [`Default`], and the offset path is
/// skipped for that sample rather than fed a fiction. The residual risk is a
/// caller whose steady clock genuinely reads 0 within a nanosecond of boot; the
/// consequence is that one sample does not contribute to an offset baseline,
/// which is never a wrong halt.
///
/// **Do not substitute `stamp_nanos` for a missing receipt time.** That makes
/// `offset ≡ 0` for every publisher, which silently re-enables the inference
/// path on raw stamps and resurrects defect 1 for exactly the callers who cannot
/// see the fix.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SteadyNanos(pub i64);

impl SteadyNanos {
    /// "No receipt clock was supplied", which is what [`Default`] produces.
    ///
    /// [`OffsetTable::observe`] returns without touching any state for a sample
    /// carrying this, so the whole common-mode layer is simply absent for a
    /// caller that cannot supply physical time — the honest degradation, and a
    /// safe one, because the ladder never halts on one witness anyway.
    pub const UNKNOWN: SteadyNanos = SteadyNanos(0);
}

/// What to do when the clock jumps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnClockReset {
    /// Stop and report. **The default**: a bridge that keeps running across a
    /// reset is writing one recording's transforms into an arena that still
    /// holds another's, and no consumer can tell which is which.
    #[default]
    Halt,
    /// Build a fresh arena instance. For a bag-replay workflow, where the loop
    /// is expected and a clean restart per iteration is what the user wants.
    Recreate,
}

/// What the guard decided about a stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockVerdict {
    /// Time moved forward, or not at all. Publish.
    Forward,
    /// Time went backwards, but by less than the threshold.
    ///
    /// **Not a reset**, and the distinction matters. A guard watches one edge,
    /// so this is one publisher's stamps arriving slightly out of order — which
    /// happens routinely, from a node that fills several `TransformStamped`s
    /// with slightly different capture times into one `TFMessage`, from a
    /// best-effort transport that reorders, and from any publisher with more
    /// than one thread. The sample is dropped and counted; Phase 1 would have
    /// rejected it anyway, and dropping it here means the engine never sees an
    /// error worth logging.
    Jitter {
        /// How far back, in nanoseconds.
        by_nanos: i64,
    },
    /// A regression past the threshold, on **this one edge**.
    ///
    /// **This is a fact, not a judgment, and the online bridge no longer
    /// promotes it on its own.** One publisher regressing — restarting,
    /// hiccuping, replaying its own buffer — looks exactly like this, and
    /// halting a healthy robot for it is an outage caused by the diagnostic
    /// rather than by the fault. [`crate::Ingest::offer`] therefore disposes of
    /// this identically to [`ClockVerdict::Jitter`]: drop, count, diagnose.
    /// Promotion needs corroboration ([`OffsetTable`]) or an authoritative
    /// signal ([`crate::Ingest::note_time_jump`]).
    ///
    /// The offline half (`tf_tree_ingest`) still halts on the first one, and
    /// deliberately: a recording is a closed artefact that is either coherent or
    /// is not, and a human is reading the answer. That is why `policy` still
    /// travels with the verdict.
    Reset {
        /// How far back, in nanoseconds. Always positive.
        by_nanos: i64,
        /// The policy to apply, carried so a caller cannot forget to consult
        /// it — the two decisions are made in one place or they drift.
        policy: OnClockReset,
    },
}

/// Watches a monotone-ish clock and classifies backward motion.
#[derive(Debug)]
pub struct ClockGuard {
    newest: Option<i64>,
    threshold_nanos: i64,
    policy: OnClockReset,
    jitter_drops: u64,
    resets: u64,
}

/// Default backward-jump threshold: **100 ms**.
///
/// Chosen against what a *single publisher's* own stamps actually do. A node
/// that fills one `TFMessage` from several sensors, or publishes from more than
/// one thread, or ships over a best-effort transport that reorders, emits stamps
/// a few milliseconds out of order routinely; 100 ms is comfortably above that
/// and comfortably below any bag loop or sim reset, which move time by seconds
/// or by the whole recording. There is no value that is right for both, which is
/// why this is a threshold and not a `< 0` test.
///
/// It is emphatically **not** sized for the offset *between* two publishers —
/// `transform_tolerance` is configurable up to seconds, so no fixed threshold
/// covers it. That is what per-edge scoping is for on the guard, and what
/// [`OffsetTable`]'s per-publisher baseline is for on the inference path:
/// the tolerance is *measured and subtracted* rather than thresholded.
pub const DEFAULT_RESET_THRESHOLD_NANOS: i64 = 100_000_000;

impl ClockGuard {
    /// A guard with the default threshold.
    #[must_use]
    pub fn new(policy: OnClockReset) -> ClockGuard {
        ClockGuard::with_threshold(policy, DEFAULT_RESET_THRESHOLD_NANOS)
    }

    /// A guard with an explicit threshold, for a workflow whose clock is
    /// noisier or quieter than the default assumes.
    #[must_use]
    pub fn with_threshold(policy: OnClockReset, threshold_nanos: i64) -> ClockGuard {
        ClockGuard {
            newest: None,
            threshold_nanos: threshold_nanos.max(0),
            policy,
            jitter_drops: 0,
            resets: 0,
        }
    }

    /// Classify `stamp`, updating the high-water mark on forward motion.
    pub fn observe(&mut self, stamp_nanos: i64) -> ClockVerdict {
        let Some(newest) = self.newest else {
            self.newest = Some(stamp_nanos);
            return ClockVerdict::Forward;
        };
        if stamp_nanos >= newest {
            self.newest = Some(stamp_nanos);
            return ClockVerdict::Forward;
        }
        // `saturating_sub`: both stamps are caller-supplied, and a bag whose
        // first message is near `i64::MIN` against a live clock near `i64::MAX`
        // would otherwise overflow — in a *release* build, silently.
        let by_nanos = newest.saturating_sub(stamp_nanos);
        if by_nanos < self.threshold_nanos {
            self.jitter_drops += 1;
            return ClockVerdict::Jitter { by_nanos };
        }
        self.resets += 1;
        ClockVerdict::Reset {
            by_nanos,
            policy: self.policy,
        }
    }

    /// Forget the high-water mark, after a [`OnClockReset::Recreate`].
    ///
    /// Separate from `observe` on purpose: recreating an arena is the caller's
    /// job and can fail, and a guard that reset itself optimistically would
    /// leave a failed recreate looking like a successful one.
    pub fn accept_reset(&mut self, stamp_nanos: i64) {
        self.newest = Some(stamp_nanos);
    }

    /// Forget the high-water mark entirely, as if the guard were new.
    ///
    /// The counters are kept: they are diagnostics about this bridge's life, not
    /// about this recording's.
    ///
    /// Distinct from [`ClockGuard::accept_reset`], which *sets* the mark to a
    /// stamp the caller has in hand. That is the right call for the edge that
    /// regressed, and the wrong one for every other edge in the arena after an
    /// [`OnClockReset::Recreate`] — seeding them all from one edge's stamp is
    /// precisely the cross-edge contamination per-edge guards exist to remove.
    /// A caller rebuilding the arena whole wants each guard rewound to "no
    /// stamp seen yet", which is this.
    ///
    /// The alternative — dropping the whole `parent → child → ClockGuard` map —
    /// also frees the two owned `String` keys per edge, so the first sample on
    /// every edge after the recreate re-enters the allocating path. Rewinding in
    /// place keeps the table's shape, which is the shape the topology fixed at
    /// startup anyway.
    pub fn forget(&mut self) {
        self.newest = None;
    }

    /// Samples dropped as jitter (§5.9).
    #[must_use]
    pub fn jitter_drops(&self) -> u64 {
        self.jitter_drops
    }

    /// Past-threshold regressions seen on this edge (§5.9).
    ///
    /// Regressions, **not** clock resets. Under the ladder a single edge's
    /// regression never promotes on its own, so this counts a fact about one
    /// publisher; `BridgeStats::clock_resets` counts the promotions.
    #[must_use]
    pub fn resets(&self) -> u64 {
        self.resets
    }

    /// The newest stamp accepted so far.
    #[must_use]
    pub fn newest(&self) -> Option<i64> {
        self.newest
    }
}

/// Which way, and in what sense, the time source said it jumped.
///
/// Mirrors what `rcl_time_jump_t` carries: `rcl_clock_change_t` distinguishes a
/// change of time *source* (`RCL_ROS_TIME_ACTIVATED` / `..._DEACTIVATED`, i.e.
/// `use_sim_time` being switched at runtime) from motion within one source, and
/// `rcl_duration_t delta` is *"the new time minus the last time before the
/// jump"*, so a rewind is negative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JumpKind {
    /// The clock *source* changed — sim time activated or deactivated. The
    /// delta across that boundary compares two different time bases and is not
    /// a duration in any one of them, which is why this is its own kind rather
    /// than a large `Backward` or `Forward`.
    ClockTypeChanged,
    /// Time moved backwards: a bag loop, a sim reset, an NTP step back.
    Backward,
    /// Time moved forwards past the reporting threshold: a bag seek, a sim
    /// fast-forward, an NTP step. **The inference path cannot see this from a
    /// backward-regression watcher**, which is half of why the authoritative
    /// path exists and half of why agreement, not regression, is what
    /// [`OffsetTable`] tests.
    Forward,
}

/// Why the bridge concluded the clock moved.
///
/// Carried on the halt because the two rungs of the ladder are not equally
/// strong and an operator should be told which one fired: *"the time source
/// reported a 5 s rewind"* is a fact, and *"three publishers stepped together by
/// about 5 s"* is an inference that happens to be very well corroborated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockEvidence {
    /// The time source reported the jump itself — [`crate::Ingest::note_time_jump`].
    /// No threshold, no window, no corroboration was involved.
    Reported {
        /// What the source said it was.
        kind: JumpKind,
    },
    /// This many **distinct publishers** stepped inside the correlation window
    /// and agreed about the size of the step. Always ≥ 2: one witness never
    /// promotes.
    ///
    /// Publishers and not edges, and the difference is the whole rule. One node
    /// owning two dynamic edges moves both the instant it restarts, so an edge
    /// count is met by exactly the single-publisher event the rule exists to
    /// tolerate.
    CommonMode {
        /// How many agreed, including the one whose step completed it.
        publishers: u32,
    },
}

/// Every knob §5.5's detection has, in physical units.
///
/// Each default is derived below rather than chosen; a knob whose value has no
/// argument behind it is a knob nobody can safely change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockPolicy {
    /// How far a publisher's stamp must move, against its own last accepted
    /// stamp or against its own offset baseline, to stop being noise.
    ///
    /// Default [`DEFAULT_RESET_THRESHOLD_NANOS`] — **100 ms**; see that
    /// constant for the derivation. One threshold serves both the per-edge guard
    /// and the offset step detector on purpose: they measure the same publisher
    /// misbehaving by the same amount, and two thresholds that can disagree
    /// about one sample is how this pipeline got its first defect.
    pub reset_threshold_nanos: i64,
    /// How close together, **in receipt time**, two publishers' steps must fall
    /// to be candidates for one common cause.
    ///
    /// Default **1 s**. Bounded below by how long a real `/clock` step takes to
    /// become visible on every publisher: a publisher only reveals its new
    /// offset when it next publishes, so the window must exceed the slowest
    /// interesting `/tf` publisher's period — 1 Hz localizers and 2 Hz map
    /// servers are ordinary, 1 s covers them. Bounded above by the false-halt
    /// risk: the wider it is, the more likely two genuinely unrelated restarts
    /// land inside it, and agreement is then the only thing standing between
    /// that coincidence and a halt. A second is the smallest value that covers
    /// the slow publishers, which is the conservative end of that trade.
    ///
    /// It is physical time, not a transform count (P3). `0011` used 4096
    /// observations because the crate had no clock; that made "at the same time"
    /// mean 2 s on a busy stream and minutes on a sparse one.
    pub correlation_window_nanos: i64,
    /// How far two steps may differ, as a fraction of the larger, and still
    /// count as the same step.
    ///
    /// Default **0.25**. A real `/clock` step moves every publisher by exactly
    /// the same amount; what spreads the measurements is only *when* each
    /// publisher next published relative to the step, which shows up as a
    /// difference bounded by their publish periods. A quarter is loose enough
    /// that a 5 s bag loop measured by a 10 Hz and a 1 Hz publisher still agrees
    /// (they can differ by at most about a second, i.e. 20 %), and tight enough
    /// that two unrelated restarts have to be suspiciously similar to pass —
    /// a 5 s replay and a 400 ms hiccup are 92 % apart.
    ///
    /// A negative, `NaN` or absurd value cannot widen the tolerance below
    /// [`ClockPolicy::common_mode_tolerance_floor_nanos`]; see
    /// [`OffsetTable::observe`].
    pub common_mode_tolerance_ratio: f64,
    /// The tolerance never falls below this, however small the steps are.
    ///
    /// Default **50 ms**. Without a floor, a ratio alone makes agreement
    /// arbitrarily strict for small steps — two publishers stepping by 120 ms
    /// and 160 ms would have to match to 30 ms — and 100 ms is already the
    /// threshold at which a step is worth noticing at all, so the floor is set
    /// at half of it: strictly inside the smallest step that can exist, and
    /// large enough to absorb the scheduling jitter between two nodes' first
    /// post-step messages.
    pub common_mode_tolerance_floor_nanos: i64,
    /// What to do once the clock is judged to have moved.
    pub on_reset: OnClockReset,
}

impl Default for ClockPolicy {
    fn default() -> ClockPolicy {
        ClockPolicy {
            reset_threshold_nanos: DEFAULT_RESET_THRESHOLD_NANOS,
            correlation_window_nanos: 1_000_000_000,
            common_mode_tolerance_ratio: 0.25,
            common_mode_tolerance_floor_nanos: 50_000_000,
            on_reset: OnClockReset::default(),
        }
    }
}

/// How many distinct publishers [`OffsetTable`] will track.
///
/// See the bound's justification at its use in [`OffsetTable::observe`].
pub(crate) const MAX_TRACKED_PUBLISHERS: usize = 64;

/// The EWMA divisor: the baseline moves by a **1/8** of each residual.
///
/// Stated as a divisor rather than a float because the whole update is integer
/// arithmetic — deterministic on every target, with no floating-point drift in a
/// value that is compared against a threshold.
///
/// # Why 1/8
///
/// The baseline has to satisfy two opposite demands.
///
/// - **Fast enough that a step self-heals.** After a genuine step the baseline
///   is snapped to the new offset outright (see [`OffsetTable::observe`]), so
///   this only governs ordinary drift — but a baseline that lagged a slow drift
///   badly would eventually cross the 100 ms threshold on its own and
///   manufacture a step out of nothing. Under a steady drift of `d` per sample
///   the steady-state lag of an `α = 1/8` EWMA is `(1-α)/α · d = 7d`. A stamp
///   stream advancing 1 ms per message against a frozen receipt clock — the
///   pathological case, and what `tests/steady_state_alloc.rs` used to do
///   accidentally — therefore sits 7 ms behind, an order of magnitude inside the
///   threshold.
/// - **Slow enough that a real step is not absorbed.** 1/8 moves the baseline by
///   12.5 % of the first sample of a step, so a 5 s rewind still leaves 4.4 s of
///   residual on the *next* sample. It is only ever asked to absorb the
///   millisecond-scale jitter a publisher shows against a steady clock, where it
///   reaches 63 % of a change in 8 samples and 95 % in 24 — 80 ms and 240 ms at
///   100 Hz, comfortably inside the 1 s correlation window.
///
/// Integer division truncates toward zero, so a residual smaller than 8 ns moves
/// the baseline not at all. That dead zone is eight nanoseconds wide against a
/// hundred-million-nanosecond threshold, and it is symmetric, which an
/// arithmetic shift would not be.
const BASELINE_DIVISOR: i64 = 8;

/// One publisher's offset baseline and its most recent step.
#[derive(Clone, Copy, Debug)]
struct Offset {
    /// The smoothed `stamp - received` for this publisher.
    ///
    /// **This is the publisher's `transform_tolerance`, measured.** A localizer
    /// dating `map -> odom` 300 ms into the future has a baseline of +300 ms and
    /// a residual of ~0, which is why a correct configuration stops looking like
    /// a jump instead of being thresholded against one.
    baseline: i64,
    /// Receipt time of the most recent step, or `None` if this publisher has
    /// never stepped. Ages out against
    /// [`ClockPolicy::correlation_window_nanos`].
    stepped_at: Option<SteadyNanos>,
    /// How big that step was, signed: negative for a rewind. Meaningless unless
    /// `stepped_at` is `Some`.
    step_delta: i64,
}

/// A common-mode step: several publishers moved together, by the same amount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommonMode {
    /// New offset minus old, signed — negative for a rewind, following
    /// `rcl_time_jump_t::delta`'s convention so the two rungs of the ladder
    /// report the same quantity the same way.
    pub delta_nanos: i64,
    /// How many distinct publishers agreed, including the one whose step
    /// completed it. Always ≥ 2.
    pub publishers: u32,
}

/// Per-publisher stamp-to-receipt offsets, and the common-mode rule over them.
///
/// This is the **fallback** rung of the ladder (see the module docs): it exists
/// for callers with no authoritative jump signal, for system-clock steps that
/// `/clock` never reports, and as defence in depth. It sits above
/// [`ClockGuard`], never inside it — the guard answers an exact per-edge
/// question and mixing a global judgment into it is the shape `0011` records as
/// the original defect.
#[derive(Debug)]
pub struct OffsetTable {
    /// Publisher name → id, capped at [`MAX_TRACKED_PUBLISHERS`]. The cap lives
    /// here now rather than on the row count, which is the same bound stated in
    /// the place that owns the identity.
    ids: StrInterner,
    /// One row per interned publisher, indexed by its id. `None` until that
    /// publisher's first sample defines a baseline — and after
    /// [`OffsetTable::clear`], which blanks the rows and **keeps the ids**, so a
    /// recreate does not make every publisher's next sample re-intern its name.
    rows: Vec<Option<Offset>>,
    policy: ClockPolicy,
    steps: u64,
    common_modes: u64,
}

impl Default for OffsetTable {
    fn default() -> OffsetTable {
        OffsetTable::new(ClockPolicy::default())
    }
}

impl OffsetTable {
    /// An empty table under `policy`.
    #[must_use]
    pub fn new(policy: ClockPolicy) -> OffsetTable {
        OffsetTable {
            ids: StrInterner::with_cap(MAX_TRACKED_PUBLISHERS),
            rows: Vec::new(),
            policy,
            steps: 0,
            common_modes: 0,
        }
    }

    /// Fold one sample's offset in, and say whether it completed a common-mode
    /// step.
    ///
    /// `owner` is the publisher's identity as one borrowed string
    /// (`Ingest::owner_key`). `stamp_nanos` is the publisher's, `received` is
    /// the local steady clock — **the two must not be swapped and must not come
    /// from the same source**, which is the whole of P2.
    ///
    /// Returns `Some` exactly when at least two distinct publishers have stepped
    /// inside [`ClockPolicy::correlation_window_nanos`] of this sample's receipt
    /// time *and* their steps agree in size. One publisher stepping returns
    /// `None`, however large the step: that is the ladder's bottom rung and it
    /// never halts.
    ///
    /// # No allocation after a publisher's first sample
    ///
    /// The row is probed with `get_mut(&str)` and inserted only on first sight —
    /// never `entry`, which needs an owned key whether or not it inserts. The
    /// agreement scan borrows and builds nothing. This matters because the
    /// *regression* path is not rare: a publisher stuck replaying stale stamps
    /// occupies it at message rate for as long as it is stuck, and the code this
    /// replaced allocated twice per such sample, indefinitely, at 1 kHz.
    ///
    /// # Bounded
    ///
    /// The key is a node name resolved from the ROS graph — chosen outside this
    /// process, exactly the class of key that already forced caps on
    /// [`crate::NameNormalizer`] and on `Ingest`'s undeclared-edge table. At most
    /// `MAX_TRACKED_PUBLISHERS` (64) rows are kept, and past the cap a
    /// previously unseen publisher gets no row. That degrades in the safe
    /// direction and only in the safe direction: a publisher with no row can
    /// never corroborate anything, so the cap makes a halt *harder* to reach,
    /// never easier.
    pub fn observe(
        &mut self,
        owner: &str,
        stamp_nanos: i64,
        received: SteadyNanos,
    ) -> Option<CommonMode> {
        // No physical reference, no inference. The honest degradation: this
        // sample contributes nothing rather than contributing a fiction.
        if received == SteadyNanos::UNKNOWN {
            return None;
        }
        let offset = stamp_nanos.saturating_sub(received.0);

        // Scoped so the mutable borrow of one row ends before the agreement scan
        // reads all of them.
        // One hash, where this was a `BTreeMap<String, _>` descent — six
        // node-name comparisons at the cap — on every accepted transform.
        let Some(id) = self.ids.intern(owner) else {
            // Past the cap. A publisher with no row can never corroborate
            // anything, which makes a halt harder to reach and never easier.
            return None;
        };
        if self.rows.len() <= id.get() {
            self.rows.resize(id.get() + 1, None);
        }
        let delta = {
            let Some(row) = self.rows[id.get()].as_mut() else {
                self.rows[id.get()] = Some(Offset {
                    baseline: offset,
                    stepped_at: None,
                    step_delta: 0,
                });
                // A publisher's first sample defines its baseline; there is
                // nothing yet for it to have stepped away from.
                return None;
            };
            let residual = offset.saturating_sub(row.baseline);
            if residual.saturating_abs() <= self.policy.reset_threshold_nanos {
                row.baseline = row
                    .baseline
                    .saturating_add(residual / BASELINE_DIVISOR.max(1));
                return None;
            }
            // A step. Snap the baseline rather than smoothing toward the new
            // offset: the step *is* the new truth, and a baseline that crawled
            // toward it would keep re-reporting the same step for as many
            // samples as it took to catch up — the "one fault, one diagnostic"
            // rule, applied to a rule that could otherwise fire at 1 kHz.
            row.baseline = offset;
            row.stepped_at = Some(received);
            row.step_delta = residual;
            residual
        };
        self.steps += 1;

        // Agreement, not coincidence. A real `/clock` step moves everyone by the
        // same amount; two nodes restarting independently do not.
        let mut publishers: u32 = 1;
        for (other_id, other) in self.rows.iter().enumerate() {
            // An index compare, where this was a node-name `memcmp` per row.
            if other_id == id.get() {
                continue;
            }
            let Some(other) = other else {
                continue;
            };
            let Some(at) = other.stepped_at else {
                continue;
            };
            // A receipt clock is monotone, so `received < at` means the caller
            // handed back a stale reading. Treat it as out of window: a broken
            // reference clock must only make a halt harder to reach.
            let age = received.0.saturating_sub(at.0);
            if age < 0 || age > self.policy.correlation_window_nanos {
                continue;
            }
            if (delta.saturating_sub(other.step_delta)).saturating_abs()
                <= self.tolerance(delta, other.step_delta)
            {
                publishers = publishers.saturating_add(1);
            }
        }
        if publishers < 2 {
            return None;
        }
        self.common_modes += 1;
        Some(CommonMode {
            delta_nanos: delta,
            publishers,
        })
    }

    /// How far two step sizes may differ and still be called one step.
    ///
    /// `max(floor, ratio · max(|a|, |b|))`. The `f64` multiply is the only
    /// floating-point arithmetic on this path and its result is clamped by the
    /// floor, so a `ratio` that is negative or `NaN` — `as i64` yields `0` for
    /// `NaN` and saturates at the extremes — cannot widen the tolerance at all.
    /// A hostile config can therefore make agreement *stricter* (fewer halts)
    /// and never looser.
    fn tolerance(&self, a: i64, b: i64) -> i64 {
        let scale = a.saturating_abs().max(b.saturating_abs());
        let scaled = (scale as f64 * self.policy.common_mode_tolerance_ratio) as i64;
        scaled.max(self.policy.common_mode_tolerance_floor_nanos.max(0))
    }

    /// Forget every baseline, after an [`OnClockReset::Recreate`] or an
    /// authoritative jump.
    ///
    /// The baselines describe offsets against a time base that no longer exists.
    /// Carrying them across would make every publisher's first post-reset sample
    /// a step, and those steps would agree — so the bridge would report a second
    /// clock reset caused by nothing but its own response to the first.
    pub fn clear(&mut self) {
        // **The rows are blanked and the ids are kept.** Forgetting the names
        // too would make every publisher's first post-recreate sample re-intern
        // and re-allocate, which is the same reason
        // `Ingest::forget_the_old_recording` rewinds its guards in place rather
        // than dropping them. `tracked()` counts live rows, so it still reports
        // zero here — which is what the assertion in
        // `clear_forgets_the_time_base_that_was_thrown_away` reads.
        //
        // `fill`, not a loop: `clippy::manual_slice_fill` became a `-D warnings`
        // error when the rolling stable toolchain moved to 1.98. `Offset` is
        // `Copy`, so this is the same store with no clone in it.
        self.rows.fill(None);
    }

    /// How many publishers have a row.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.rows.iter().filter(|r| r.is_some()).count()
    }

    /// Offset steps observed, promoted or not (§5.9).
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Common-mode steps reported (§5.9). Counts verdicts, not distinct clock
    /// events: a caller that keeps offering after one keeps being told the same
    /// thing, which is what the latch on the C seam is for.
    #[must_use]
    pub fn common_modes(&self) -> u64 {
        self.common_modes
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;
    const S: i64 = 1_000_000_000;

    /// **Ordinary interleaving is not a reset.**
    ///
    /// Several publishers at different rates put stamps a few milliseconds out
    /// of order all the time. Treating that as a bag loop would restart the
    /// arena roughly continuously — so this is the test that stops the reset
    /// detector from being worse than no detector.
    ///
    /// Mutant: classify any `stamp < newest` as a reset ⇒ this fails on the
    /// first out-of-order message.
    #[test]
    fn a_few_milliseconds_out_of_order_is_jitter_not_a_reset() {
        let mut g = ClockGuard::new(OnClockReset::Halt);
        assert_eq!(g.observe(1_000 * MS), ClockVerdict::Forward);
        assert_eq!(g.observe(1_010 * MS), ClockVerdict::Forward);
        // 8 ms late — a second publisher's message, arriving after a faster
        // one's. Normal, and must not restart anything.
        assert_eq!(
            g.observe(1_002 * MS),
            ClockVerdict::Jitter { by_nanos: 8 * MS }
        );
        assert_eq!(g.resets(), 0, "no reset may be reported");
        assert_eq!(g.jitter_drops(), 1);
        // The high-water mark did not move backwards.
        assert_eq!(g.newest(), Some(1_010 * MS));
    }

    /// **A bag loop is a reset, and is reported once.**
    #[test]
    fn a_bag_loop_is_a_reset_carrying_the_policy() {
        let mut g = ClockGuard::new(OnClockReset::Recreate);
        for i in 0..10 {
            assert_eq!(g.observe(1_000 * MS + i * MS), ClockVerdict::Forward);
        }
        match g.observe(0) {
            ClockVerdict::Reset { by_nanos, policy } => {
                assert_eq!(by_nanos, 1_009 * MS);
                assert_eq!(policy, OnClockReset::Recreate, "the policy travels with it");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(g.resets(), 1);
        // **The mark does not move until the caller says the recreate worked.**
        // A guard that reset itself here would make a *failed* recreate look
        // like a successful one, and the next message would read as forward
        // motion into an arena that still holds the previous recording.
        assert_eq!(g.newest(), Some(1_009 * MS));
        g.accept_reset(0);
        assert_eq!(g.observe(MS), ClockVerdict::Forward);
    }

    /// **Exactly at the threshold is a reset, one nanosecond short is not.**
    ///
    /// The boundary is where an off-by-one lives, and both sides of it have to
    /// be pinned or the constant can drift by one without any test noticing.
    #[test]
    fn the_threshold_boundary_is_exact() {
        let mut g = ClockGuard::with_threshold(OnClockReset::Halt, 100 * MS);
        g.observe(1_000 * MS);
        assert!(matches!(
            g.observe(1_000 * MS - (100 * MS - 1)),
            ClockVerdict::Jitter { .. }
        ));
        assert!(matches!(
            g.observe(1_000 * MS - 100 * MS),
            ClockVerdict::Reset { .. }
        ));
    }

    /// **An equal stamp is forward motion, not a jump.**
    ///
    /// Phase 1 accepts equal stamps (the newer value wins), so the bridge must
    /// too — classifying them as jitter would drop legitimate corrections.
    #[test]
    fn an_equal_stamp_is_forward() {
        let mut g = ClockGuard::new(OnClockReset::Halt);
        g.observe(500);
        assert_eq!(g.observe(500), ClockVerdict::Forward);
        assert_eq!(g.jitter_drops(), 0);
    }

    /// **Extreme stamps must not overflow.**
    ///
    /// Both are caller-supplied. A bag starting near `i64::MIN` against a live
    /// clock near `i64::MAX` overflows a plain subtraction — and in a release
    /// build it wraps silently, turning the largest possible jump into a small
    /// positive number and classifying a total clock replacement as jitter.
    ///
    /// Mutant: `newest - stamp_nanos` instead of `saturating_sub` ⇒ this panics
    /// in debug and, worse, passes in release with the wrong verdict.
    #[test]
    fn an_extreme_backward_jump_saturates_rather_than_wrapping() {
        let mut g = ClockGuard::new(OnClockReset::Halt);
        g.observe(i64::MAX);
        match g.observe(i64::MIN) {
            ClockVerdict::Reset { by_nanos, .. } => {
                assert_eq!(by_nanos, i64::MAX, "saturated, not wrapped");
            }
            other => panic!("a full-range jump must be a reset, got {other:?}"),
        }
    }

    /// A negative threshold cannot make every sample a reset.
    #[test]
    fn a_negative_threshold_is_clamped_to_zero() {
        let mut g = ClockGuard::with_threshold(OnClockReset::Halt, -5);
        g.observe(100);
        // Any backward motion is >= 0 >= threshold, so it is a reset — but
        // forward motion is still forward, which a negative threshold used
        // arithmetically could not guarantee.
        assert_eq!(g.observe(200), ClockVerdict::Forward);
        assert!(matches!(g.observe(199), ClockVerdict::Reset { .. }));
    }

    /// **A rewound guard has seen nothing**, not "seen the stamp some other
    /// edge happened to be at".
    ///
    /// This is the whole difference between `forget` and `accept_reset`, and it
    /// is what lets a caller rewind *every* edge's guard after a
    /// [`OnClockReset::Recreate`] without seeding twenty edges from the one
    /// stamp that tripped the reset.
    ///
    /// Mutant: `pub fn forget(&mut self) {}` — applied, and this failed at
    /// `Some(10000000000) != None`: the mark the recreate was supposed to have
    /// thrown away was still there, and the next stamp would have read as a
    /// second reset.
    #[test]
    fn a_forgotten_guard_accepts_the_next_stamp_whatever_it_is() {
        let mut g = ClockGuard::new(OnClockReset::Halt);
        assert_eq!(g.observe(10_000 * MS), ClockVerdict::Forward);
        assert!(matches!(g.observe(5_000 * MS), ClockVerdict::Reset { .. }));
        g.forget();
        assert_eq!(g.newest(), None, "no mark, not somebody else's mark");
        assert_eq!(g.observe(5_000 * MS), ClockVerdict::Forward);
        assert_eq!(
            g.resets(),
            1,
            "the counters describe the bridge's life, not the recording's"
        );
    }

    // ---- OffsetTable -------------------------------------------------------

    /// A table under the shipped defaults, so the unit tests below exercise the
    /// constants an operator actually gets.
    fn table() -> OffsetTable {
        OffsetTable::new(ClockPolicy::default())
    }

    /// **A steady `transform_tolerance` is measured and subtracted, so it is
    /// never a step.**
    ///
    /// The original defect, at the level of the primitive. A localizer dating
    /// `map -> odom` 300 ms into the future has an *offset* of +300 ms — three
    /// times the reset threshold — and it never changes. Against a threshold on
    /// the raw quantity that is a permanent jump; against a baseline it is a
    /// residual of zero.
    ///
    /// Mutant: compare `offset` against the threshold instead of
    /// `offset - baseline` (`if offset.saturating_abs() <= ...`) — applied, and
    /// this failed at `a configuration is not an event`, `left: 39, right: 0`:
    /// every sample from a correctly configured localizer read as a step.
    #[test]
    fn a_steady_offset_is_a_baseline_not_a_step() {
        let mut t = table();
        for k in 0..40i64 {
            let received = SteadyNanos(100 * S + k * 10 * MS);
            // Stamped 300 ms into the future, every single time.
            assert_eq!(t.observe("/amcl", received.0 + 300 * MS, received), None);
        }
        assert_eq!(t.steps(), 0, "a configuration is not an event");
        assert_eq!(t.tracked(), 1);
    }

    /// **One publisher stepping is never a common-mode step**, however large.
    ///
    /// The ladder's bottom rung, at the level of the primitive: with one witness
    /// there is nothing to agree with, so there is no floor to get wrong and no
    /// deployment shape in which a lone restart can stop the bridge.
    ///
    /// Mutant: `if publishers < 1 { return None; }` — i.e. promote on one
    /// witness, which is defect 3 restored — applied, and this failed at
    /// `Some(CommonMode { delta_nanos: -5000000000, publishers: 1 }) != None`
    /// on the first regression.
    #[test]
    fn one_publisher_stepping_is_never_common_mode() {
        let mut t = table();
        for k in 0..10i64 {
            let received = SteadyNanos(100 * S + k * 10 * MS);
            assert_eq!(t.observe("/wheels", received.0, received), None);
        }
        // It restarts and replays from five seconds ago.
        for k in 0..10i64 {
            let received = SteadyNanos(200 * S + k * 10 * MS);
            assert_eq!(
                t.observe("/wheels", received.0 - 5 * S, received),
                None,
                "one witness is one witness, at k={k}"
            );
        }
        assert_eq!(t.common_modes(), 0);
        assert_eq!(t.steps(), 1, "one bout of stepping is one step");
    }

    /// **Two publishers moved by the same amount inside the window are the
    /// clock.**
    ///
    /// The positive case, and the proof that the redesign did not quietly delete
    /// §5.5's detection. Both publishers' offsets drop by exactly 5 s because
    /// the thing underneath them moved by 5 s.
    ///
    /// Mutant: drop the `if name == owner { continue; }` guard, so a publisher
    /// corroborates itself — applied, and this failed at `one witness only`,
    /// `left: Some(CommonMode { delta_nanos: -5000000000, publishers: 2 }),
    /// right: None`: the *first* publisher's step already read as two
    /// witnesses, so a bag loop would have been called on one node's evidence.
    /// It also failed `one_publisher_stepping_is_never_common_mode` the same
    /// way, which is the more alarming of the two.
    #[test]
    fn two_publishers_stepping_together_are_the_clock() {
        let mut t = table();
        let base = 100 * S;
        for k in 0..10i64 {
            for who in ["/amcl", "/wheels"] {
                let received = SteadyNanos(base + k * 10 * MS);
                assert_eq!(t.observe(who, received.0, received), None);
            }
        }
        // The bag loops. `/amcl` notices first.
        let a = SteadyNanos(base + 200 * MS);
        assert_eq!(t.observe("/amcl", a.0 - 5 * S, a), None, "one witness only");
        // …and `/wheels` corroborates 50 ms later, having moved by the same 5 s.
        let b = SteadyNanos(base + 250 * MS);
        assert_eq!(
            t.observe("/wheels", b.0 - 5 * S, b),
            Some(CommonMode {
                delta_nanos: -5 * S,
                publishers: 2,
            })
        );
        assert_eq!(t.common_modes(), 1);
    }

    /// **A forward jump is detected**, which no backward-regression watcher can
    /// see at all.
    ///
    /// A sim fast-forward or a bag seek moves every stamp *ahead*. Every edge
    /// stays perfectly monotone, so [`ClockGuard`] reports `Forward` for all of
    /// it and the pre-redesign detector was structurally blind. Agreement, not
    /// regression, is what is tested — so this falls out rather than needing its
    /// own rule.
    ///
    /// Mutant: `if residual > -self.policy.reset_threshold_nanos` in place of
    /// `if residual.saturating_abs() <= self.policy.reset_threshold_nanos` —
    /// i.e. a watcher that only looks for backward motion — applied, and this
    /// failed at `a forward step is a clock event too`, `left: None, right:
    /// Some(CommonMode { delta_nanos: 30000000000, publishers: 2 })`: a 30 s
    /// forward seek went entirely unnoticed.
    #[test]
    fn a_forward_common_mode_jump_is_detected() {
        let mut t = table();
        let base = 100 * S;
        for k in 0..10i64 {
            for who in ["/amcl", "/wheels"] {
                let received = SteadyNanos(base + k * 10 * MS);
                assert_eq!(t.observe(who, received.0, received), None);
            }
        }
        let a = SteadyNanos(base + 200 * MS);
        assert_eq!(t.observe("/amcl", a.0 + 30 * S, a), None);
        let b = SteadyNanos(base + 210 * MS);
        assert_eq!(
            t.observe("/wheels", b.0 + 30 * S, b),
            Some(CommonMode {
                delta_nanos: 30 * S,
                publishers: 2,
            }),
            "a forward step is a clock event too"
        );
    }

    /// **Agreement is what decides, not mere coincidence in time.**
    ///
    /// Two nodes that restart within a second of each other — a launch file
    /// respawning both, a machine coming back from a suspend — step by whatever
    /// each had buffered, which is unrelated. A rule that only asked "did two
    /// publishers step inside the window" would halt on that, which is the same
    /// false halt in a new costume.
    ///
    /// The deltas here are 5 s and 400 ms: 92 % apart, so no plausible ratio
    /// admits them.
    ///
    /// Mutant: drop the agreement test (count every stepped row inside the
    /// window) — applied, and this failed at
    /// `Some(CommonMode { delta_nanos: -400000000, publishers: 2 }) != None`.
    #[test]
    fn two_publishers_stepping_by_unrelated_amounts_are_two_faults() {
        let mut t = table();
        let base = 100 * S;
        for k in 0..10i64 {
            for who in ["/amcl", "/wheels"] {
                let received = SteadyNanos(base + k * 10 * MS);
                assert_eq!(t.observe(who, received.0, received), None);
            }
        }
        let a = SteadyNanos(base + 200 * MS);
        assert_eq!(t.observe("/amcl", a.0 - 5 * S, a), None);
        let b = SteadyNanos(base + 250 * MS);
        assert_eq!(
            t.observe("/wheels", b.0 - 400 * MS, b),
            None,
            "two restarts inside a second are still two restarts"
        );
        assert_eq!(t.steps(), 2, "…and both are recorded as steps");
        assert_eq!(t.common_modes(), 0);
    }

    /// **The agreement tolerance is proportional, with a floor.**
    ///
    /// Two publishers measuring one 5 s step disagree by however long each
    /// waited before publishing again, so a fixed tolerance is either too tight
    /// for large steps or too loose for small ones. A 1 Hz publisher can
    /// therefore report a second less than a 10 Hz one for the very same jump.
    ///
    /// 5.0 s against 4.0 s differs by 1.0 s, which is 20 % of the larger and
    /// inside the 25 % ratio; 5.0 s against 3.0 s differs by 2.0 s, which is
    /// 40 % and outside it. The two halves differ only in the second delta, so
    /// nothing but the ratio can explain the different answers.
    ///
    /// Mutant: `scaled.min(floor)` instead of `.max(floor)` — applied, and this
    /// failed at `5.0 s against 4000 ms: None`, `left: false, right: true`: the
    /// tolerance collapsed to the 50 ms floor and no two real measurements of
    /// one step could ever agree, so the whole rung was dead.
    ///
    /// Mutant: `let scale = a.saturating_abs().min(b.saturating_abs());` —
    /// applied, and it did *not* fail here (5 s and 4 s are close enough either
    /// way at 25 %), which is why the second case exists: with the smaller
    /// operand the 3.0 s pair gets a 750 ms tolerance instead of 1250 ms and
    /// still disagrees, so only a case that *should* agree can catch it. Left
    /// recorded rather than silently unmutated: this test pins the ratio, and
    /// `two_publishers_stepping_by_unrelated_amounts_are_two_faults` pins that
    /// the tolerance is not simply enormous.
    #[test]
    fn the_agreement_tolerance_scales_with_the_step() {
        for (second, agrees) in [(4_000 * MS, true), (3_000 * MS, false)] {
            let mut t = table();
            let base = 100 * S;
            for k in 0..10i64 {
                for who in ["/a", "/b"] {
                    let received = SteadyNanos(base + k * 10 * MS);
                    t.observe(who, received.0, received);
                }
            }
            let x = SteadyNanos(base + 200 * MS);
            t.observe("/a", x.0 - 5_000 * MS, x);
            let y = SteadyNanos(base + 250 * MS);
            let v = t.observe("/b", y.0 - second, y);
            assert_eq!(
                v.is_some(),
                agrees,
                "5.0 s against {} ms: {v:?}",
                second / MS
            );
        }
    }

    /// **The correlation window is physical time, and its boundary is exact.**
    ///
    /// `0011` measured this in transforms offered, so "at the same time" meant
    /// two seconds on a busy stream and minutes on a sparse one — a rule about
    /// coincidence whose meaning was set by message rate. Both sides of the
    /// boundary are pinned or the constant can drift by one with nothing
    /// noticing.
    ///
    /// Mutant: `age >= self.policy.correlation_window_nanos` instead of `>` —
    /// applied, and this failed at `a gap of 1000000000 ns`, `left: false,
    /// right: true`, for the pair separated by exactly one second.
    #[test]
    fn the_correlation_window_boundary_is_exact_and_in_nanoseconds() {
        for (gap, agrees) in [(1_000 * MS, true), (1_000 * MS + 1, false)] {
            let mut t = table();
            let base = 100 * S;
            for k in 0..10i64 {
                for who in ["/a", "/b"] {
                    let received = SteadyNanos(base + k * 10 * MS);
                    t.observe(who, received.0, received);
                }
            }
            let x = SteadyNanos(base + 10 * S);
            t.observe("/a", x.0 - 5 * S, x);
            let y = SteadyNanos(x.0 + gap);
            assert_eq!(
                t.observe("/b", y.0 - 5 * S, y).is_some(),
                agrees,
                "a gap of {gap} ns"
            );
        }
    }

    /// **A publisher that has been broken for hours is not evidence about the
    /// clock.**
    ///
    /// A node stuck replaying stale stamps regresses on every message forever.
    /// Snapping the baseline to the new offset is what stops it re-reporting the
    /// same step at message rate and sitting in the correlation window
    /// permanently, ready to corroborate the next unrelated hiccup anywhere in
    /// the tree.
    ///
    /// Mutant: leave the baseline smoothing (`row.baseline =
    /// row.baseline.saturating_add(residual / BASELINE_DIVISOR.max(1))`) in
    /// place of the snap on a step — applied, and this failed at `one bout of
    /// being broken is one step`, `left: 30, right: 1`: the residual took thirty
    /// samples to fall back under the threshold, so one restart was reported as
    /// thirty steps, each of them a standing invitation to a false common
    /// mode.
    #[test]
    fn a_persistently_stale_publisher_steps_once_per_bout() {
        let mut t = table();
        let base = 100 * S;
        for k in 0..10i64 {
            let received = SteadyNanos(base + k * 10 * MS);
            t.observe("/wheels", received.0, received);
        }
        // Stuck: the stamp advances at the same rate as real time, but five
        // seconds behind it, for a thousand messages.
        for k in 0..1_000i64 {
            let received = SteadyNanos(base + S + k * MS);
            assert_eq!(t.observe("/wheels", received.0 - 5 * S, received), None);
        }
        assert_eq!(t.steps(), 1, "one bout of being broken is one step");
    }

    /// **A caller with no steady clock gets no inference at all**, rather than
    /// inference over a fiction.
    ///
    /// [`SteadyNanos::UNKNOWN`] is what [`crate::Sample::identity`] leaves
    /// behind and what the `.tfstream` replay path can honestly supply. The
    /// tempting alternative — defaulting `received` to `stamp_nanos` — makes
    /// `offset ≡ 0` for everyone, so every publisher's baseline is 0, every
    /// `transform_tolerance` reads as a step, and defect 1 returns for exactly
    /// the callers who cannot see the fix.
    ///
    /// Mutant: delete the `received == SteadyNanos::UNKNOWN` early return —
    /// applied, and this failed at the first regressing `observe`, `left:
    /// Some(CommonMode { delta_nanos: -5004104603, publishers: 2 }), right:
    /// None`: two publishers in a corpus with no receipt clock formed a common
    /// mode out of nothing, and the reported delta is visibly a difference of
    /// two *stamps* rather than of anything physical.
    #[test]
    fn no_receipt_clock_means_no_inference() {
        let mut t = table();
        for k in 0..10i64 {
            t.observe("/a", 100 * S + k * MS, SteadyNanos::UNKNOWN);
            t.observe("/b", 100 * S + k * MS, SteadyNanos::UNKNOWN);
        }
        assert_eq!(t.observe("/a", 95 * S, SteadyNanos::UNKNOWN), None);
        assert_eq!(t.observe("/b", 95 * S, SteadyNanos::UNKNOWN), None);
        assert_eq!(t.tracked(), 0, "no row is even created");
        assert_eq!(t.steps(), 0);
    }

    /// **The table is bounded**, because its keys are chosen by somebody else.
    ///
    /// A publisher identity is a node name resolved from the ROS graph. A bridge
    /// asked to run unattended for a fortnight against a graph that churns is
    /// the growth bug `NameNormalizer::seen` already had to cap. Past the cap a
    /// new publisher gets no row, which can only make a halt harder to reach.
    ///
    /// Mutant: drop the `self.rows.len() < MAX_TRACKED_PUBLISHERS` guard —
    /// applied, and this failed at `3000 != 64`, i.e. unbounded growth keyed on
    /// whatever the graph reported.
    #[test]
    fn the_publisher_table_is_capped() {
        let mut t = table();
        for i in 0..3_000i64 {
            let received = SteadyNanos(100 * S + i * MS);
            t.observe(&format!("/node{i}"), received.0, received);
        }
        assert_eq!(t.tracked(), MAX_TRACKED_PUBLISHERS);
    }

    /// **A stale receipt reading cannot forge a correlation, or overflow.**
    ///
    /// The receipt clock is the caller's, and a caller that hands back a reading
    /// below one already seen is buggy. Ageing is a subtraction, so this is
    /// handled rather than trusted: a negative age is treated as out of window,
    /// which pushes the same way every other degradation here does — a broken
    /// reference clock makes a halt harder to reach, never easier.
    ///
    /// Mutant: drop the `age < 0` arm — applied, and this failed at
    /// `Some(CommonMode { delta_nanos: -5000000000, publishers: 2 }) != None`:
    /// a step from an hour in the future corroborated one from now.
    #[test]
    fn a_stale_receipt_reading_cannot_forge_a_correlation() {
        let mut t = table();
        let base = 100 * S;
        for k in 0..10i64 {
            for who in ["/a", "/b"] {
                let received = SteadyNanos(base + k * 10 * MS);
                t.observe(who, received.0, received);
            }
        }
        // `/a` steps, timed an hour into the future.
        let far = SteadyNanos(base + 3_600 * S);
        assert_eq!(t.observe("/a", far.0 - 5 * S, far), None);
        // `/b` steps by the same amount, now.
        let now = SteadyNanos(base + 200 * MS);
        assert_eq!(
            t.observe("/b", now.0 - 5 * S, now),
            None,
            "a reading from the future is not corroboration for the present"
        );
    }

    /// **A recreate throws the baselines away with the arena.**
    ///
    /// They describe offsets against a time base that no longer exists. Kept,
    /// every publisher's first post-reset sample is a step — and those steps
    /// *agree*, because they are all the same jump — so the bridge would report
    /// a second clock reset caused by nothing but its own response to the first.
    ///
    /// Mutant: `pub fn clear(&mut self) {}` — applied, and this failed one line
    /// after the clear, at `left: 2, right: 0` on `tracked()`. With that
    /// assertion removed it goes on to fail at the second post-clear `observe`
    /// with `Some(CommonMode { delta_nanos: 5000000000, publishers: 2 })`, which
    /// is the self-inflicted second reset the `tracked()` line is a proxy for.
    #[test]
    fn clear_forgets_the_time_base_that_was_thrown_away() {
        let mut t = table();
        let base = 100 * S;
        for k in 0..10i64 {
            for who in ["/a", "/b"] {
                let received = SteadyNanos(base + k * 10 * MS);
                t.observe(who, received.0, received);
            }
        }
        let x = SteadyNanos(base + 200 * MS);
        t.observe("/a", x.0 - 5 * S, x);
        let y = SteadyNanos(base + 250 * MS);
        assert!(t.observe("/b", y.0 - 5 * S, y).is_some());
        t.clear();
        assert_eq!(t.tracked(), 0);

        // The new recording starts. Both publishers are back on the old stamps,
        // which against a kept baseline would be a +5 s step for each.
        let p = SteadyNanos(base + 300 * MS);
        assert_eq!(t.observe("/a", p.0, p), None);
        let q = SteadyNanos(base + 310 * MS);
        assert_eq!(t.observe("/b", q.0, q), None);
        assert_eq!(
            t.common_modes(),
            1,
            "the counter describes the bridge's life and survives the clear"
        );
    }
}
