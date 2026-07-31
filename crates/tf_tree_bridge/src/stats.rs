//! Bridge counters — `docs/PHASE4.md` §5.9.
//!
//! §5.9 names five: messages received, transforms applied, dropped by
//! authority, dropped by non-monotonic stamp, and subscription queue depth.
//! The last is the one that matters most and is the only one this module cannot
//! compute — it comes from `rclcpp`, and the ROS half reports it in.
//!
//! **"If the queue is persistently full, the bridge is the bottleneck and the
//! operator needs to know that, not guess."** That is the whole reason a
//! high-water mark is kept rather than an instantaneous reading: a queue that is
//! full only between two samples is invisible to polling, and it is exactly the
//! condition that drops transforms.

/// Everything the bridge counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BridgeStats {
    /// `TFMessage`es received, on both topics.
    pub messages: u64,
    /// Individual transforms inside them.
    pub transforms: u64,
    /// Transforms written into the arena.
    pub applied: u64,
    /// `/tf_static` transforms that **matched the config's declared constant**
    /// and were therefore not written (§5.7 idempotent, §5.8 verification).
    ///
    /// Its own bucket rather than a share of `applied`, because it is not one:
    /// the value was in the arena before the bridge started, put there by
    /// `TopologyConfig::builder`, and the message only confirmed it. Counting
    /// it as applied made `applied` grow every time a late joiner caused the
    /// transient-local latched set to be re-delivered — an operator watching
    /// `applied` on a robot with no dynamic edges would have seen a healthy
    /// write rate for an arena nothing was writing to.
    pub static_verified: u64,
    /// Dropped because another publisher owns the edge (§5.4).
    pub dropped_authority: u64,
    /// Dropped because **time misbehaved** (§5.5). The bridge's one bucket for
    /// every refusal the clock rules produce, and it is wider than its name.
    ///
    /// Three kinds of sample land here:
    ///
    /// - the stamp went backwards by less than the reset threshold — jitter,
    ///   the case the name describes;
    /// - the stamp went backwards *past* the threshold on one edge, which is a
    ///   fact about one publisher and never on its own a clock reset;
    /// - the sample on which a **common-mode step** was detected, whether that
    ///   step was backwards or **forwards**. A forward jump leaves every stamp
    ///   perfectly monotone, so nothing about that sample is non-monotonic; it
    ///   is charged here anyway because it *is* refused (the bridge is halting
    ///   or the arena is being rebuilt), because [`BridgeStats::balanced`]
    ///   requires every counted transform to land in exactly one bucket, and
    ///   because a second bucket would be a `struct_size`-versioned growth of
    ///   the C seam's mirror of this struct.
    ///
    /// So read it as *"transforms the clock rules refused"*. `clock_resets` is
    /// what separates the third kind from the first two.
    pub dropped_non_monotonic: u64,
    /// Dropped because the frame name was empty or unusable (§5.6).
    pub dropped_bad_name: u64,
    /// Dropped because the edge kind would have changed (§5.7).
    pub dropped_kind_change: u64,
    /// Dropped because the topology config does not declare the edge (§5.8).
    ///
    /// **The counter an operator looks at first after a config change.** The
    /// engine has no runtime edge declaration (§5.8's amendment), so a
    /// transform for an edge the file forgot has nowhere to go: it is dropped
    /// silently as far as ROS is concerned, and the only symptom downstream is
    /// a lookup that returns `NoPath`. A non-zero value here turns that into
    /// "the robot publishes three edges your config does not list", which is a
    /// question with an answer.
    pub dropped_undeclared: u64,
    /// Clock resets detected (§5.5) — **promotions, not regressions**.
    ///
    /// Narrowed by `docs/decisions/0011`. The clock guard is per edge, and one
    /// edge's stamps going backwards past the threshold is a fact about one
    /// publisher: it is dropped, and counted in `dropped_non_monotonic`, and it
    /// does **not** appear here — **however far it went and however often**.
    /// This counts the times the bridge concluded that the *clock* moved, which
    /// takes either an authoritative jump report from the time source
    /// (`Ingest::note_time_jump`) or a common-mode step agreed on by at least
    /// two distinct publishers. Under the default `OnClockReset::Halt` it is 0
    /// or 1 for the life of the bridge, because the first one stops it.
    ///
    /// The distinction is the whole of that record: a counter that conflated
    /// them would report a healthy robot whose estimator lags its wheel
    /// odometry as resetting its clock hundreds of times a second.
    ///
    /// Not a term in [`BridgeStats::balanced`]. A promotion detected on an
    /// arriving transform charges that transform to `dropped_non_monotonic`,
    /// which is a term; a promotion reported by `Ingest::note_time_jump` has no
    /// transform in hand at all and charges no bucket, exactly like the startup
    /// window's close.
    pub clock_resets: u64,
    /// Static-transform value conflicts (§5.7).
    pub static_conflicts: u64,
    /// The **deepest** the subscription queue has been, not its depth now.
    ///
    /// Reported by the ROS half. A queue that fills only between two samples is
    /// invisible to polling and is exactly the condition that drops transforms,
    /// so an instantaneous reading would answer "is the bridge keeping up?" with
    /// "it is right now".
    pub queue_high_water: u32,
    /// The subscription's configured depth, so the high-water mark can be read
    /// as a fraction rather than as a bare number. `100` per §5.2.
    pub queue_capacity: u32,
}

impl BridgeStats {
    /// Whether every transform that arrived was accounted for.
    ///
    /// The bridge's own consistency check: each transform is applied or dropped
    /// for exactly one reason. A mismatch means a path returns without
    /// counting, which is how a "we are not dropping anything" claim becomes
    /// false without any test failing.
    #[must_use]
    pub fn balanced(&self) -> bool {
        self.applied
            + self.static_verified
            + self.dropped_authority
            + self.dropped_non_monotonic
            + self.dropped_bad_name
            + self.dropped_kind_change
            + self.dropped_undeclared
            == self.transforms
    }

    /// Whether the queue has ever been full — the §5.9 signal that the bridge,
    /// rather than anything downstream of it, is the bottleneck.
    #[must_use]
    pub fn queue_saturated(&self) -> bool {
        self.queue_capacity > 0 && self.queue_high_water >= self.queue_capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The ledger must balance**, and this is the test that says what
    /// balancing means before anything relies on it.
    #[test]
    fn a_transform_is_applied_or_dropped_for_exactly_one_reason() {
        let s = BridgeStats {
            transforms: 10,
            applied: 6,
            dropped_authority: 2,
            dropped_non_monotonic: 1,
            dropped_bad_name: 1,
            ..BridgeStats::default()
        };
        assert!(s.balanced());

        // …and an undeclared edge is one of the buckets, not a leak. Dropping
        // `dropped_undeclared` from `balanced()` makes this pass while the
        // ledger is short by one, which is the shape the check exists to catch.
        let s = BridgeStats {
            transforms: 11,
            applied: 6,
            dropped_authority: 2,
            dropped_non_monotonic: 1,
            dropped_bad_name: 1,
            dropped_undeclared: 1,
            ..BridgeStats::default()
        };
        assert!(s.balanced());

        // One unaccounted transform — the shape of a path that returns early
        // without counting.
        let s = BridgeStats {
            transforms: 10,
            applied: 6,
            ..BridgeStats::default()
        };
        assert!(!s.balanced());
    }

    /// Saturation needs a capacity to be measured against; an unreported one
    /// must not read as "saturated" or as "fine".
    #[test]
    fn saturation_needs_a_capacity() {
        let s = BridgeStats {
            queue_high_water: 100,
            queue_capacity: 100,
            ..BridgeStats::default()
        };
        assert!(s.queue_saturated());
        let s = BridgeStats {
            queue_high_water: 99,
            queue_capacity: 100,
            ..BridgeStats::default()
        };
        assert!(!s.queue_saturated());
        // Capacity 0 means "the ROS half did not report one".
        let s = BridgeStats {
            queue_high_water: 1000,
            queue_capacity: 0,
            ..BridgeStats::default()
        };
        assert!(!s.queue_saturated(), "unknown capacity is not saturation");
    }
}
