//! Bridge counters — `docs/PHASE4.md` §5.9. The subscription queue depth comes
//! from `rclcpp`; the ROS half reports it in, and a high-water mark is kept
//! because a queue full only between two samples is invisible to polling.

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
    /// and were therefore not written (§5.7, §5.8). Not part of `applied`: the
    /// value was already in the arena.
    pub static_verified: u64,
    /// Dropped because another publisher owns the edge (§5.4).
    pub dropped_authority: u64,
    /// Dropped because **time misbehaved** (§5.5): every refusal the clock rules
    /// produce, i.e. jitter, a backwards stamp past the reset threshold on one
    /// edge, and the sample on which a common-mode step (backwards or forwards)
    /// was detected. `clock_resets` separates the last from the first two.
    pub dropped_non_monotonic: u64,
    /// Dropped because the frame name was empty or unusable (§5.6).
    pub dropped_bad_name: u64,
    /// Dropped because the edge kind would have changed (§5.7).
    pub dropped_kind_change: u64,
    /// Dropped because the topology config does not declare the edge (§5.8).
    ///
    /// The engine has no runtime edge declaration, so the only downstream
    /// symptom of an undeclared edge is a lookup returning `NoPath`.
    pub dropped_undeclared: u64,
    /// Clock resets detected (§5.5) — **promotions, not regressions**
    /// (`docs/decisions/0011`).
    ///
    /// One edge's stamps going backwards is a fact about one publisher: it is
    /// counted in `dropped_non_monotonic` and never here. This counts the times
    /// the bridge concluded the *clock* moved: an authoritative report
    /// (`Ingest::note_time_jump`) or a common-mode step agreed on by at least two
    /// publishers. Under `OnClockReset::Halt` it is 0 or 1.
    ///
    /// Not a term in [`BridgeStats::balanced`]: a promotion on an arriving
    /// transform charges `dropped_non_monotonic`; one from `note_time_jump`
    /// has no transform and charges no bucket.
    pub clock_resets: u64,
    /// Static-transform value conflicts (§5.7).
    pub static_conflicts: u64,
    /// The **deepest** the subscription queue has been, not its depth now.
    pub queue_high_water: u32,
    /// The subscription's configured depth (`100` per §5.2).
    pub queue_capacity: u32,
}

impl BridgeStats {
    /// Whether every transform that arrived was applied or dropped for exactly
    /// one reason. A mismatch means a path returns without counting.
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

    /// Whether the queue has ever been full (§5.9).
    #[must_use]
    pub fn queue_saturated(&self) -> bool {
        self.queue_capacity > 0 && self.queue_high_water >= self.queue_capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transform is applied or dropped for exactly one reason.
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

        // An undeclared edge is one of the buckets, not a leak.
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

        // One unaccounted transform.
        let s = BridgeStats {
            transforms: 10,
            applied: 6,
            ..BridgeStats::default()
        };
        assert!(!s.balanced());
    }

    /// An unreported capacity reads as neither "saturated" nor "fine".
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
