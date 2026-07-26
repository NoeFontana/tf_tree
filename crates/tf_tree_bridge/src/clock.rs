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

/// What to do when the clock jumps backwards.
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
    /// **Not a reset**, and the distinction matters: a `/tf` stream carries
    /// messages from several publishers whose stamps interleave by a few
    /// milliseconds all the time, and treating that as a bag loop would restart
    /// the arena roughly continuously. The sample is dropped and counted;
    /// Phase 1 would have rejected it anyway, and dropping it here means the
    /// engine never sees an error worth logging.
    Jitter {
        /// How far back, in nanoseconds.
        by_nanos: i64,
    },
    /// A genuine reset. The caller applies its [`OnClockReset`] policy.
    Reset {
        /// How far back, in nanoseconds.
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
/// Chosen against what actually interleaves. A `/tf` stream carries several
/// publishers at different rates, and their stamps arrive a few milliseconds out
/// of order routinely; 100 ms is comfortably above that and comfortably below
/// any bag loop or sim reset, which move time by seconds or by the whole
/// recording. There is no value that is right for both, which is why this is a
/// threshold and not a `< 0` test.
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

    /// Samples dropped as jitter (§5.9).
    #[must_use]
    pub fn jitter_drops(&self) -> u64 {
        self.jitter_drops
    }

    /// Resets detected (§5.9).
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

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
}
