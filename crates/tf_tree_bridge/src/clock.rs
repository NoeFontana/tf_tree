//! Time domains and clock resets — `docs/PHASE4.md` §5.5, NORMATIVE.
//!
//! Bag loops and sim resets move `/clock` backwards. Phase 1 rejects
//! non-monotonic stamps one at a time, so a bridge that forwards them logs one
//! error per message per edge while the tree freezes. §5.5: detect the jump
//! **once** and halt or recreate the arena.
//!
//! §5.5 also requires the bridge to refuse, **at startup**, an edge whose
//! declared domain differs from its own.
//!
//! Inference from `/tf` stamps is specified by `docs/decisions/0012`
//! (§Context *The three rules, and what killed each*, §Decision *The five
//! principles* and *L1*-*L3*); "P1"-"P5" are its principles, "the ladder" is *L3*,
//! "defect 1"/"defect 3" are from its Context.

use crate::interner::StrInterner;

/// A reading of a local **steady** (monotonic) clock, in nanoseconds.
///
/// Distinct from a publisher's stamp because confusing the two is the bug class
/// this design removes. Never derived from `/clock` or a publisher.
///
/// Online: `rclcpp::Clock(RCL_STEADY_TIME).now()`, read **once per `TFMessage`**
/// and copied onto every [`crate::Sample`] it expands into. Offline: the
/// recording's log time (`RawRecord::log_time_ns`).
///
/// Only differences mean anything, so `0` ([`SteadyNanos::UNKNOWN`]) is free to
/// mean "no receipt clock supplied" and that sample skips the offset path.
/// **Do not substitute `stamp_nanos` for a missing receipt time**: that makes
/// `offset` zero for every publisher and resurrects defect 1.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SteadyNanos(pub i64);

impl SteadyNanos {
    /// "No receipt clock was supplied", what [`Default`] produces.
    /// [`OffsetTable::observe`] ignores such a sample.
    pub const UNKNOWN: SteadyNanos = SteadyNanos(0);
}

/// What to do when the clock jumps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnClockReset {
    /// Stop and report. The default: continuing would mix two recordings' transforms in one arena.
    #[default]
    Halt,
    /// Build a fresh arena instance, for bag-replay loops.
    Recreate,
}

/// What the guard decided about a stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockVerdict {
    /// Time moved forward, or not at all. Publish.
    Forward,
    /// Time went backwards by less than the threshold: one publisher slightly out
    /// of order (see [`DEFAULT_RESET_THRESHOLD_NANOS`]). Dropped and counted.
    Jitter {
        /// How far back, in nanoseconds.
        by_nanos: i64,
    },
    /// A regression past the threshold on **this one edge**: a fact, not a
    /// judgment. [`crate::Ingest::offer`] treats it like [`ClockVerdict::Jitter`]
    /// (drop, count, diagnose); promotion needs corroboration ([`OffsetTable`])
    /// or [`crate::Ingest::note_time_jump`]. The offline half (`tf_tree_ingest`)
    /// still halts on the first one, which is why `policy` travels with it.
    Reset {
        /// How far back, in nanoseconds. Always positive.
        by_nanos: i64,
        /// The policy to apply, carried so a caller cannot forget it.
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
/// Above a single publisher's routine out-of-order stamps (a few ms) and below
/// any bag loop or sim reset (seconds). Not sized for the offset *between*
/// publishers: [`OffsetTable`] measures and subtracts that.
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
        // `saturating_sub`: both stamps are caller-supplied, and `i64::MIN`
        // against `i64::MAX` overflows — silently, in a release build.
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
    /// Separate from `observe`: recreating the arena is the caller's job and can fail.
    pub fn accept_reset(&mut self, stamp_nanos: i64) {
        self.newest = Some(stamp_nanos);
    }

    /// Forget the high-water mark entirely, as if the guard were new.
    ///
    /// Counters are kept. Unlike [`ClockGuard::accept_reset`], which seeds the mark
    /// from one edge's stamp, this leaves other edges uncontaminated; rewinding in
    /// place keeps the per-edge table's shape and avoids re-allocating keys.
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
    /// Regressions, **not** clock resets: `BridgeStats::clock_resets` counts promotions.
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
/// Mirrors `rcl_time_jump_t`: a change of time *source* versus motion within
/// one, with `delta` = new time minus last time before the jump (a rewind is
/// negative).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JumpKind {
    /// The clock *source* changed (sim time toggled); the delta compares two time bases.
    ClockTypeChanged,
    /// Time moved backwards: a bag loop, a sim reset, an NTP step back.
    Backward,
    /// Time moved forwards past the threshold: a bag seek, sim fast-forward, NTP
    /// step. Invisible to a backward-regression watcher.
    Forward,
}

/// Why the bridge concluded the clock moved.
///
/// Carried on the halt: a reported jump is a fact, a common-mode step an inference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockEvidence {
    /// The time source reported the jump itself ([`crate::Ingest::note_time_jump`]).
    Reported {
        /// What the source said it was.
        kind: JumpKind,
    },
    /// This many **distinct publishers** (always >= 2) stepped inside the
    /// correlation window and agreed on the size. Publishers, not edges: one node
    /// owning two edges moves both when it restarts.
    CommonMode {
        /// How many agreed, including the one whose step completed it.
        publishers: u32,
    },
}

/// Every knob §5.5's detection has, in physical units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockPolicy {
    /// How far a stamp must move against its own last stamp or offset baseline to
    /// stop being noise. Default [`DEFAULT_RESET_THRESHOLD_NANOS`] (100 ms); one
    /// threshold serves both the guard and the step detector so they cannot
    /// disagree about a sample.
    pub reset_threshold_nanos: i64,
    /// How close together, **in receipt time**, two publishers' steps must fall to
    /// share a cause. Default **1 s**: above the slowest ordinary `/tf`
    /// publisher's period (1 Hz localizers), and no wider, to limit coincident
    /// unrelated restarts. Physical time, not a count (P3).
    pub correlation_window_nanos: i64,
    /// How far two steps may differ, as a fraction of the larger, and still be
    /// one step. Default **0.25**: a 5 s loop seen by a 10 Hz and a 1 Hz
    /// publisher differs by ~20 %, a 5 s replay and a 400 ms hiccup by 92 %. A
    /// negative or `NaN` value cannot go below the floor.
    pub common_mode_tolerance_ratio: f64,
    /// The tolerance never falls below this. Default **50 ms**: half the
    /// threshold, enough to absorb scheduling jitter between first post-step messages.
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
/// Cap on distinct publishers [`OffsetTable`] tracks (see [`OffsetTable::observe`]).
pub(crate) const MAX_TRACKED_PUBLISHERS: usize = 64;

/// The EWMA divisor: the baseline moves by a **1/8** of each residual.
///
/// Integer arithmetic keeps the update deterministic. 1/8: a steady drift `d`
/// per sample lags by `7d` (well inside the threshold), while a real step is
/// barely absorbed (a 5 s rewind leaves 4.4 s residual on the next sample).
/// Truncation toward zero gives a symmetric 8 ns dead zone.
const BASELINE_DIVISOR: i64 = 8;

/// One publisher's offset baseline and its most recent step.
#[derive(Clone, Copy, Debug)]
struct Offset {
    /// The smoothed `stamp - received` for this publisher.
    ///
    /// The publisher's `transform_tolerance`, measured: a localizer dating
    /// `map -> odom` 300 ms ahead has baseline +300 ms and residual ~0.
    baseline: i64,
    /// Receipt time of the last step; ages out against [`ClockPolicy::correlation_window_nanos`].
    stepped_at: Option<SteadyNanos>,
    /// Signed size of that step; meaningless unless `stepped_at` is `Some`.
    step_delta: i64,
}

/// A common-mode step: several publishers moved together, by the same amount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommonMode {
    /// New offset minus old; negative for a rewind, as `rcl_time_jump_t::delta`.
    pub delta_nanos: i64,
    /// Distinct publishers that agreed, including the completing one. Always >= 2.
    pub publishers: u32,
}

/// Per-publisher stamp-to-receipt offsets, and the common-mode rule over them.
///
/// The fallback rung of the ladder (`docs/decisions/0012`, *L3*), above
/// [`ClockGuard`] and never inside it.
#[derive(Debug)]
pub struct OffsetTable {
    /// Publisher name -> id, capped at [`MAX_TRACKED_PUBLISHERS`].
    ids: StrInterner,
    /// One row per interned publisher; `None` until its first sample. [`OffsetTable::clear`]
    /// blanks rows and keeps ids, so a recreate does not re-intern.
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
    /// `owner` is the publisher's identity (`Ingest::owner_key`); `stamp_nanos` is
    /// the publisher's and `received` the local steady clock. They must come from
    /// different sources (P2).
    ///
    /// Returns `Some` exactly when at least two distinct publishers stepped within
    /// [`ClockPolicy::correlation_window_nanos`] of `received` and agree in size;
    /// one publisher never does.
    ///
    /// Allocation-free after a publisher's first sample (`get_mut(&str)`, never
    /// `entry`), because a publisher replaying stale stamps sits on this path at
    /// message rate. The publisher name comes from the ROS graph, so rows are
    /// capped at `MAX_TRACKED_PUBLISHERS` (64); past it a publisher gets no row and
    /// cannot corroborate, which only makes a halt harder to reach.
    pub fn observe(
        &mut self,
        owner: &str,
        stamp_nanos: i64,
        received: SteadyNanos,
    ) -> Option<CommonMode> {
        // No physical reference, no inference.
        if received == SteadyNanos::UNKNOWN {
            return None;
        }
        let offset = stamp_nanos.saturating_sub(received.0);

        let Some(id) = self.ids.intern(owner) else {
            // Past the cap.
            return None;
        };
        if self.rows.len() <= id.get() {
            self.rows.resize(id.get() + 1, None);
        }
        // Scoped so the row's mutable borrow ends before the agreement scan.
        let delta = {
            let Some(row) = self.rows[id.get()].as_mut() else {
                self.rows[id.get()] = Some(Offset {
                    baseline: offset,
                    stepped_at: None,
                    step_delta: 0,
                });
                return None;
            };
            let residual = offset.saturating_sub(row.baseline);
            if residual.saturating_abs() <= self.policy.reset_threshold_nanos {
                row.baseline = row
                    .baseline
                    .saturating_add(residual / BASELINE_DIVISOR.max(1));
                return None;
            }
            // A step: snap the baseline, or it would re-report the step until it caught up.
            row.baseline = offset;
            row.stepped_at = Some(received);
            row.step_delta = residual;
            residual
        };
        self.steps += 1;

        // Agreement, not coincidence.
        let mut publishers: u32 = 1;
        for (other_id, other) in self.rows.iter().enumerate() {
            if other_id == id.get() {
                continue;
            }
            let Some(other) = other else {
                continue;
            };
            let Some(at) = other.stepped_at else {
                continue;
            };
            // `received < at` is a stale reading: out of window, so a broken clock only makes a halt harder.
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
    /// `max(floor, ratio * max(|a|, |b|))`. The floor clamps a negative or `NaN`
    /// ratio, so a hostile config can make agreement stricter, never looser.
    fn tolerance(&self, a: i64, b: i64) -> i64 {
        let scale = a.saturating_abs().max(b.saturating_abs());
        let scaled = (scale as f64 * self.policy.common_mode_tolerance_ratio) as i64;
        scaled.max(self.policy.common_mode_tolerance_floor_nanos.max(0))
    }

    /// Forget every baseline, after an [`OnClockReset::Recreate`] or an
    /// authoritative jump.
    ///
    /// The baselines refer to a time base that no longer exists; keeping them would
    /// make every first post-reset sample a step, and the steps would agree.
    pub fn clear(&mut self) {
        // Rows blanked, ids kept (see `rows`); `tracked()` counts live rows.
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

    /// Common-mode verdicts reported (§5.9), not distinct clock events.
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

    /// Ordinary interleaving is not a reset.
    ///
    /// Mutant: classify any `stamp < newest` as a reset ⇒ this fails on the first out-of-order message.
    #[test]
    fn a_few_milliseconds_out_of_order_is_jitter_not_a_reset() {
        let mut g = ClockGuard::new(OnClockReset::Halt);
        assert_eq!(g.observe(1_000 * MS), ClockVerdict::Forward);
        assert_eq!(g.observe(1_010 * MS), ClockVerdict::Forward);
        // 8 ms late — a second publisher's message, arriving after a faster one's.
        assert_eq!(
            g.observe(1_002 * MS),
            ClockVerdict::Jitter { by_nanos: 8 * MS }
        );
        assert_eq!(g.resets(), 0, "no reset may be reported");
        assert_eq!(g.jitter_drops(), 1);
        // The high-water mark did not move backwards.
        assert_eq!(g.newest(), Some(1_010 * MS));
    }

    /// A bag loop is a reset, and is reported once.
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
        // **The mark does not move until the caller says the recreate worked**:
        // otherwise the next message reads as forward motion into an arena that
        // still holds the previous recording.
        assert_eq!(g.newest(), Some(1_009 * MS));
        g.accept_reset(0);
        assert_eq!(g.observe(MS), ClockVerdict::Forward);
    }

    /// Exactly at the threshold is a reset, one nanosecond short is not.
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

    /// An equal stamp is forward motion, not a jump.
    #[test]
    fn an_equal_stamp_is_forward() {
        let mut g = ClockGuard::new(OnClockReset::Halt);
        g.observe(500);
        assert_eq!(g.observe(500), ClockVerdict::Forward);
        assert_eq!(g.jitter_drops(), 0);
    }

    /// Extreme stamps must not overflow.
    ///
    /// Mutant: `newest - stamp_nanos` instead of `saturating_sub` ⇒ this panics in debug and, worse, passes in release with the wrong verdict.
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

    /// A rewound guard has seen nothing, not "seen the stamp some other
    /// edge happened to be at".
    ///
    /// Mutant: `pub fn forget(&mut self) {}`.
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

    /// A table under the shipped defaults, so the unit tests below exercise the
    /// constants an operator actually gets.
    fn table() -> OffsetTable {
        OffsetTable::new(ClockPolicy::default())
    }

    /// A steady `transform_tolerance` is measured and subtracted, so it is
    /// never a step.
    ///
    /// Mutant: compare `offset` against the threshold instead of `offset - baseline` (`if offset.saturating_abs() <= ...`).
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

    /// One publisher stepping is never a common-mode step, however large.
    ///
    /// Mutant: `if publishers < 1 { return None; }` — i.e. promote on one witness, which is defect 3 restored.
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

    /// Two publishers moved by the same amount inside the window are the
    /// clock.
    ///
    /// Mutant: drop the `if name == owner { continue; }` guard, so a publisher corroborates itself.
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

    /// A forward jump is detected, which no backward-regression watcher can
    /// see at all.
    ///
    /// Mutant: `if residual > -self.policy.reset_threshold_nanos` in place of `if residual.saturating_abs() <= self.policy.reset_threshold_nanos` — i.e. a watcher that only looks for backward motion.
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

    /// Agreement is what decides, not mere coincidence in time.
    ///
    /// Mutant: drop the agreement test (count every stepped row inside the window).
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

    /// The agreement tolerance is proportional, with a floor.
    ///
    /// Mutant: `scaled.min(floor)` instead of `.max(floor)`.
    ///
    /// Mutant: `let scale = a.saturating_abs().min(b.saturating_abs());`.
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

    /// The correlation window is physical time, and its boundary is exact.
    ///
    /// Mutant: `age >= self.policy.correlation_window_nanos` instead of `>`.
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

    /// A publisher that has been broken for hours is not evidence about the
    /// clock.
    ///
    /// Mutant: leave the baseline smoothing (`row.baseline = row.baseline.saturating_add(residual / BASELINE_DIVISOR.max(1))`) in place of the snap on a step.
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

    /// A caller with no steady clock gets no inference at all, rather than
    /// inference over a fiction.
    ///
    /// Mutant: delete the `received == SteadyNanos::UNKNOWN` early return.
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

    /// The table is bounded, because its keys are chosen by somebody else.
    ///
    /// Mutant: drop the `self.rows.len() < MAX_TRACKED_PUBLISHERS` guard.
    #[test]
    fn the_publisher_table_is_capped() {
        let mut t = table();
        for i in 0..3_000i64 {
            let received = SteadyNanos(100 * S + i * MS);
            t.observe(&format!("/node{i}"), received.0, received);
        }
        assert_eq!(t.tracked(), MAX_TRACKED_PUBLISHERS);
    }

    /// A stale receipt reading cannot forge a correlation, or overflow.
    ///
    /// Mutant: drop the `age < 0` arm.
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

    /// A recreate throws the baselines away with the arena.
    ///
    /// Mutant: `pub fn clear(&mut self) {}`.
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
