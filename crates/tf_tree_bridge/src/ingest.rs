//! The decision pipeline — where §5.4 to §5.7 meet.
//!
//! Each of those sections is a table that answers one question. This is the
//! order they are asked in, and the order is not arbitrary:
//!
//! 1. **Names** (§5.6). Everything downstream keys on `(parent, child)`, so a
//!    name that is going to be rewritten must be rewritten before anything
//!    records it. Getting this wrong gives you an authority table keyed on
//!    `/base_link` and a static table keyed on `base_link`, which agree about
//!    nothing.
//! 2. **Kind** (§5.7). A hard error, and cheaper to detect than a clock
//!    comparison — but more importantly, an edge whose kind is wrong should not
//!    also be reported as a clock or authority problem. One fault, one
//!    diagnostic.
//! 3. **Static value** (§5.7), *before* authority and **only on
//!    `/tf_static`**. §5.7 says it in that order and it means it: on a
//!    differing value, *"a diagnostic naming both publishers and both values,
//!    **then** apply the authority policy"*.
//!
//!    An earlier version of this pipeline asked authority first, and the
//!    consequence was that §5.7 became **inert for exactly the case it exists
//!    for**. Two `robot_state_publisher`s with different URDFs is the canonical
//!    misconfiguration; under `FirstWriterWins` the first one owns the edge, so
//!    the second was rejected as `NotTheOwner` and the static store never saw
//!    it. The conflict payload — both values, which is the only actionable half
//!    — was unreachable through the pipeline. The converse broke too: a second
//!    publisher offering an *identical* latched value got a loud authority
//!    diagnostic where §5.7 requires silence.
//! 4. **Authority** (§5.4). Whether this publisher may write the edge at all.
//!    Before the clock, because a sample from the wrong publisher should not
//!    move the clock's high-water mark — otherwise a rejected intruder
//!    publishing from the future makes the *owner's* subsequent samples look
//!    non-monotonic.
//! 5. **Clock** (§5.5). Last, and **dynamic only**, so that only samples which
//!    are actually going to be written may advance time.
//!
//! Step 4 before step 5 is the other one that is easy to get backwards and hard
//! to notice: with those reversed, one misconfigured node can silently stall the
//! correct one, and the diagnostic blames the victim.

use crate::authority::{Authority, AuthorityPolicy, Verdict};
use crate::clock::{ClockGuard, ClockVerdict, OnClockReset};
use crate::names::NameNormalizer;
use crate::statics::{StaticStore, StaticVerdict};
use crate::stats::BridgeStats;
use crate::{Publisher, Sample};

/// Which topic a sample arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topic {
    /// `/tf` — dynamic.
    Tf,
    /// `/tf_static` — latched, transient-local.
    TfStatic,
}

/// What the bridge should do with a sample, after every table has spoken.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Publish `pose` on `(parent, child)` at `stamp_nanos`.
    Publish {
        /// Normalized parent frame.
        parent: String,
        /// Normalized child frame.
        child: String,
        /// The stamp, unchanged.
        stamp_nanos: i64,
        /// `[qw qx qy qz tx ty tz]`.
        pose: [f64; 7],
    },
    /// Declare a static edge with this constant value.
    DeclareStatic {
        /// Normalized parent frame.
        parent: String,
        /// Normalized child frame.
        child: String,
        /// The constant.
        pose: [f64; 7],
    },
    /// A `/tf_static` value that disagrees with the one on file (§5.7).
    ///
    /// **Not a `Drop`**, because §5.7 requires a diagnostic naming both
    /// publishers *and both values* — and a `Drop { reason }` can carry
    /// neither. The sample is not written either way; what this variant buys is
    /// that the caller can print the sentence an operator can act on.
    StaticConflict {
        /// Normalized parent frame.
        parent: String,
        /// Normalized child frame.
        child: String,
        /// Who declared the value on file.
        owner: Publisher,
        /// Who is contradicting them.
        intruder: Publisher,
        /// The value on file.
        existing: [f64; 7],
        /// The value just offered.
        offered: [f64; 7],
        /// First occurrence, for rate limiting.
        first_time: bool,
    },
    /// Drop it. `reason` is for the log; the counters already moved.
    Drop {
        /// Why, in a form a human reads.
        reason: DropReason,
    },
    /// Stop the bridge. Only [`AuthorityPolicy::Strict`] and
    /// [`OnClockReset::Halt`] produce this.
    Halt {
        /// Why.
        reason: HaltReason,
    },
    /// The clock went backwards past the threshold under
    /// [`OnClockReset::Recreate`]: build a fresh arena, then re-offer this
    /// sample.
    RecreateArena {
        /// How far back time went.
        by_nanos: i64,
    },
}

/// Why a sample was dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// The frame name was empty or only a slash (§5.6).
    BadName,
    /// Another publisher owns the edge (§5.4).
    NotTheOwner,
    /// The stamp went backwards, but not far enough to be a reset (§5.5).
    NonMonotonic {
        /// By how much.
        by_nanos: i64,
    },
    /// The same static value again (§5.7). Silent, and counted as applied
    /// nowhere — it is not a failure, it is a latched re-delivery.
    StaticRepeat,
    /// The edge is already declared with the other kind (§5.7).
    KindChange,
}

/// Why the bridge must stop.
#[derive(Clone, Debug, PartialEq)]
pub enum HaltReason {
    /// `Strict` policy, and two publishers appeared on one edge.
    AuthorityConflict {
        /// The prior owner.
        owner: Publisher,
        /// The publisher that collided with it.
        intruder: Publisher,
    },
    /// `Halt` policy, and the clock went backwards.
    ClockReset {
        /// By how much.
        by_nanos: i64,
    },
}

/// The four tables, plus the counters, applied in order.
#[derive(Debug)]
pub struct Ingest {
    names: NameNormalizer,
    statics: StaticStore,
    authority: Authority,
    clock: ClockGuard,
    stats: BridgeStats,
}

impl Ingest {
    /// A pipeline with the default policies: `FirstWriterWins`, `Halt`, no
    /// `tf_prefix`.
    #[must_use]
    pub fn new() -> Ingest {
        Ingest::with(AuthorityPolicy::default(), OnClockReset::default(), None)
    }

    /// A pipeline with explicit policies.
    #[must_use]
    pub fn with(
        authority: AuthorityPolicy,
        on_clock_reset: OnClockReset,
        tf_prefix: Option<&str>,
    ) -> Ingest {
        Ingest {
            names: tf_prefix.map_or_else(NameNormalizer::new, NameNormalizer::with_prefix),
            statics: StaticStore::new(),
            authority: Authority::new(authority),
            clock: ClockGuard::new(on_clock_reset),
            stats: BridgeStats {
                queue_capacity: 100, // §5.2's KeepLast(100)
                ..BridgeStats::default()
            },
        }
    }

    /// Push one transform through every table.
    pub fn offer(&mut self, topic: Topic, sample: &Sample, publisher: &Publisher) -> Action {
        self.stats.transforms += 1;

        // 1. Names, first, because everything below keys on them.
        let (Ok(parent), Ok(child)) = (
            self.names.normalize(&sample.frame_id),
            self.names.normalize(&sample.child_frame_id),
        ) else {
            self.stats.dropped_bad_name += 1;
            return Action::Drop {
                reason: DropReason::BadName,
            };
        };
        let (parent, child) = (parent.name, child.name);

        // 2. Kind. A hard error, and one fault gets one diagnostic.
        if topic == Topic::Tf && self.statics.observe_dynamic(&parent, &child).is_err() {
            self.stats.dropped_kind_change += 1;
            return Action::Drop {
                reason: DropReason::KindChange,
            };
        }

        // 3. Static value, before authority and only for `/tf_static` — §5.7
        //    orders it this way, and the module docs record what putting it
        //    after cost.
        //
        //    A static carries a stamp and it is meaningless: a latched
        //    transform is constant, and `robot_state_publisher` commonly stamps
        //    with zero. Running it past the clock guard would drag the
        //    high-water mark to the epoch and make every dynamic sample
        //    afterwards look like a bag loop, so statics never reach step 5.
        if topic == Topic::TfStatic {
            match self
                .statics
                .observe_static(&parent, &child, sample.pose, publisher)
            {
                StaticVerdict::Idempotent => {
                    // Silent, per §5.7, **including from a different
                    // publisher**: two robot_state_publishers with the same
                    // URDF is a redundant launch file, not a misconfiguration,
                    // and an authority diagnostic here would train operators to
                    // ignore the message that matters. So this returns before
                    // authority is consulted at all.
                    self.stats.applied += 1;
                    return Action::Drop {
                        reason: DropReason::StaticRepeat,
                    };
                }
                StaticVerdict::KindChanged { .. } => {
                    self.stats.dropped_kind_change += 1;
                    return Action::Drop {
                        reason: DropReason::KindChange,
                    };
                }
                StaticVerdict::Conflict {
                    owner,
                    intruder,
                    existing,
                    offered,
                    first_time,
                } => {
                    // The diagnostic first — carrying **both values**, which is
                    // the half that tells an operator which URDF is installed —
                    // and then the authority policy decides the disposition.
                    self.stats.static_conflicts += 1;
                    self.stats.dropped_authority += 1;
                    return Action::StaticConflict {
                        parent,
                        child,
                        owner,
                        intruder,
                        existing,
                        offered,
                        first_time,
                    };
                }
                StaticVerdict::Declare => {}
            }
        }

        // 4. Authority, before the clock — see the module docs.
        match self.authority.admit(&parent, &child, publisher) {
            Verdict::Accept => {}
            Verdict::Reject { .. } => {
                self.stats.dropped_authority += 1;
                return Action::Drop {
                    reason: DropReason::NotTheOwner,
                };
            }
            Verdict::Fatal { owner, intruder } => {
                // **Count it before halting.** `stats.transforms` was already
                // incremented, so returning without an outcome bucket leaves
                // `balanced()` false forever — precisely the shape
                // `BridgeStats::balanced`'s own doc names as the bug it exists
                // to detect. The clock-reset halt below always counted; these
                // two paths disagreeing is what review found.
                self.stats.dropped_authority += 1;
                return Action::Halt {
                    reason: HaltReason::AuthorityConflict { owner, intruder },
                };
            }
        }

        if topic == Topic::TfStatic {
            // Reached only for `StaticVerdict::Declare` — every other verdict
            // returned in step 3, above authority.
            self.stats.applied += 1;
            return Action::DeclareStatic {
                parent,
                child,
                pose: sample.pose,
            };
        }

        // 5. Clock, last: only a sample that will be written may advance time.
        match self.clock.observe(sample.stamp_nanos) {
            ClockVerdict::Forward => {
                self.stats.applied += 1;
                Action::Publish {
                    parent,
                    child,
                    stamp_nanos: sample.stamp_nanos,
                    pose: sample.pose,
                }
            }
            ClockVerdict::Jitter { by_nanos } => {
                self.stats.dropped_non_monotonic += 1;
                Action::Drop {
                    reason: DropReason::NonMonotonic { by_nanos },
                }
            }
            ClockVerdict::Reset { by_nanos, policy } => {
                self.stats.clock_resets += 1;
                self.stats.dropped_non_monotonic += 1;
                match policy {
                    OnClockReset::Halt => Action::Halt {
                        reason: HaltReason::ClockReset { by_nanos },
                    },
                    OnClockReset::Recreate => {
                        self.clock.accept_reset(sample.stamp_nanos);
                        Action::RecreateArena { by_nanos }
                    }
                }
            }
        }
    }

    /// Note that a `TFMessage` arrived, whatever it contained.
    pub fn note_message(&mut self) {
        self.stats.messages += 1;
    }

    /// Report the subscription queue depth (§5.9). Keeps the high-water mark.
    pub fn note_queue_depth(&mut self, depth: u32) {
        self.stats.queue_high_water = self.stats.queue_high_water.max(depth);
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> &BridgeStats {
        &self.stats
    }

    /// The authority table, for `doctor` (§5.4 requires it surfaced there).
    #[must_use]
    pub fn authority(&self) -> &Authority {
        &self.authority
    }

    /// The static-transform table, so `doctor` can surface §5.7's conflicts
    /// alongside §5.4's. Without this the conflict payload would be visible
    /// only on the one `Action` that reports it, and a caller that logged and
    /// moved on could never answer "what disagreed, in total".
    #[must_use]
    pub fn statics(&self) -> &StaticStore {
        &self.statics
    }

    /// The remap table, for the startup log (§5.6).
    #[must_use]
    pub fn names(&self) -> &NameNormalizer {
        &self.names
    }
}

impl Default for Ingest {
    fn default() -> Ingest {
        Ingest::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn node(n: &str) -> Publisher {
        Publisher::Node(n.to_string())
    }
    const MS: i64 = 1_000_000;

    /// **Authority is decided before the clock**, and this is what goes wrong
    /// if it is not.
    ///
    /// A misconfigured node publishing from the future is rejected on
    /// authority. If the clock had already seen its stamp, the *owner's*
    /// subsequent samples would look non-monotonic and be dropped — one bad
    /// node silently stalls the correct one, and the diagnostic blames the
    /// victim.
    ///
    /// Mutant: move the clock check above the authority check ⇒ the owner's
    /// samples after the intruder's are dropped as non-monotonic and this
    /// fails.
    #[test]
    fn a_rejected_publisher_cannot_move_the_clock() {
        let mut i = Ingest::new();
        let s = |t: i64| Sample::identity("odom", "base", t);

        assert!(matches!(
            i.offer(Topic::Tf, &s(1_000 * MS), &node("/ekf")),
            Action::Publish { .. }
        ));
        // An intruder, from an hour in the future.
        assert_eq!(
            i.offer(Topic::Tf, &s(3_600_000 * MS), &node("/rogue")),
            Action::Drop {
                reason: DropReason::NotTheOwner
            }
        );
        // The owner keeps working.
        assert!(matches!(
            i.offer(Topic::Tf, &s(1_010 * MS), &node("/ekf")),
            Action::Publish { .. }
        ));
        assert_eq!(i.stats().dropped_non_monotonic, 0);
    }

    /// **Names are normalized before anything keys on them.**
    ///
    /// Mutant: normalize after the authority lookup ⇒ `/odom` and `odom` become
    /// two edges with two owners, and the same publisher conflicts with itself.
    #[test]
    fn a_slash_prefixed_name_is_the_same_edge() {
        let mut i = Ingest::new();
        assert!(matches!(
            i.offer(
                Topic::Tf,
                &Sample::identity("/odom", "/base", 1_000 * MS),
                &node("/ekf")
            ),
            Action::Publish { .. }
        ));
        match i.offer(
            Topic::Tf,
            &Sample::identity("odom", "base", 1_010 * MS),
            &node("/ekf"),
        ) {
            Action::Publish { parent, child, .. } => {
                assert_eq!((parent.as_str(), child.as_str()), ("odom", "base"));
            }
            other => panic!("the same edge under both spellings: {other:?}"),
        }
        assert_eq!(i.stats().dropped_authority, 0);
    }

    /// **A static's stamp must not touch the clock.**
    ///
    /// `robot_state_publisher` commonly stamps statics with zero. Feeding that
    /// to the clock guard drags the high-water mark to the epoch and makes
    /// every dynamic sample afterwards look like a bag loop — halting the
    /// bridge on a correctly configured robot.
    ///
    /// Mutant: run statics through `clock.observe` ⇒ the dynamic sample after
    /// the zero-stamped static is a `Halt`.
    #[test]
    fn a_zero_stamped_static_does_not_reset_the_clock() {
        let mut i = Ingest::new();
        i.offer(
            Topic::Tf,
            &Sample::identity("odom", "base", 1_000_000 * MS),
            &node("/ekf"),
        );
        // A latched static, stamped at the epoch.
        assert!(matches!(
            i.offer(
                Topic::TfStatic,
                &Sample::identity("base", "lidar", 0),
                &node("/rsp")
            ),
            Action::DeclareStatic { .. }
        ));
        // The dynamic stream is unaffected.
        assert!(matches!(
            i.offer(
                Topic::Tf,
                &Sample::identity("odom", "base", 1_000_001 * MS),
                &node("/ekf")
            ),
            Action::Publish { .. }
        ));
        assert_eq!(i.stats().clock_resets, 0);
    }

    /// **The ledger balances over a realistic mixed stream**, which is what
    /// makes `BridgeStats::balanced` worth having: every path either applies or
    /// drops for exactly one reason.
    ///
    /// Mutant: return early from any arm without touching a counter ⇒ this
    /// fails, naming the totals.
    #[test]
    fn every_transform_is_accounted_for() {
        let mut i = Ingest::new();
        // **`/ekf` publishes first, deliberately.** An earlier version of this
        // fixture used `k % 5 == 0`, which is true at `k == 0` — so `/rogue`
        // took the edge, every `/ekf` sample was dropped on authority, and the
        // clock never advanced. The ledger still balanced; what failed was the
        // assertion that jitter had been exercised at all, which is the test
        // catching its own fixture.
        let pubs = [node("/ekf"), node("/rogue")];
        for k in 0..200i64 {
            let p = &pubs[usize::from(k > 0 && k % 5 == 0)];
            i.offer(
                Topic::Tf,
                &Sample::identity("odom", "base", 1_000 * MS + k * MS),
                p,
            );
        }
        // Some jitter.
        i.offer(
            Topic::Tf,
            &Sample::identity("odom", "base", 1_100 * MS),
            &node("/ekf"),
        );
        // A bad name.
        i.offer(
            Topic::Tf,
            &Sample::identity("/", "base", 1_300 * MS),
            &node("/ekf"),
        );
        // A static, then a kind clash.
        i.offer(
            Topic::TfStatic,
            &Sample::identity("base", "lidar", 0),
            &node("/rsp"),
        );
        i.offer(
            Topic::Tf,
            &Sample::identity("base", "lidar", 1_400 * MS),
            &node("/rsp"),
        );

        let s = i.stats();
        assert!(
            s.balanced(),
            "unbalanced: {} transforms vs applied {} + auth {} + mono {} + name {} + kind {}",
            s.transforms,
            s.applied,
            s.dropped_authority,
            s.dropped_non_monotonic,
            s.dropped_bad_name,
            s.dropped_kind_change
        );
        assert!(s.dropped_authority > 0 && s.dropped_bad_name == 1 && s.dropped_kind_change == 1);
        assert!(s.dropped_non_monotonic > 0);
    }

    /// **§5.7's whole feature: two `robot_state_publisher`s with different
    /// URDFs, reported with both values.**
    ///
    /// This is the case §5.4 calls the sales pitch, and it was **inert**. The
    /// pipeline asked authority before the static store, so under the default
    /// `FirstWriterWins` the first publisher owned the edge and the second was
    /// rejected as `NotTheOwner` — `observe_static` never ran, and the
    /// `existing`/`offered` payload that tells an operator *which URDF is
    /// installed* was unreachable through the pipeline. Every unit test in
    /// `statics.rs` passed, because they call `StaticStore` directly.
    ///
    /// Mutant: move `observe_static` back below `authority.admit` ⇒ this
    /// returns `Drop { NotTheOwner }` and `static_conflicts` stays 0.
    #[test]
    fn two_urdfs_disagreeing_is_reported_with_both_values() {
        let mut i = Ingest::new();
        let mut moved = Sample::identity("base", "lidar", 0);
        assert!(matches!(
            i.offer(Topic::TfStatic, &moved, &node("/rsp_a")),
            Action::DeclareStatic { .. }
        ));
        moved.pose[4] = 0.25; // the second URDF puts the lidar 25 cm forward

        match i.offer(Topic::TfStatic, &moved, &node("/rsp_b")) {
            Action::StaticConflict {
                parent,
                child,
                owner,
                intruder,
                existing,
                offered,
                first_time,
            } => {
                assert_eq!((parent.as_str(), child.as_str()), ("base", "lidar"));
                assert_eq!(owner, node("/rsp_a"), "both publishers named");
                assert_eq!(intruder, node("/rsp_b"));
                assert_eq!(existing[4], 0.0, "and both values");
                assert!((offered[4] - 0.25).abs() < 1e-12);
                assert!(first_time);
            }
            other => panic!("§5.7's diagnostic must be reachable: {other:?}"),
        }
        assert_eq!(i.stats().static_conflicts, 1);
        assert!(i.stats().balanced());
    }

    /// **An identical latched value from a second publisher is silent** — §5.7
    /// says so, and it is the normal case for a redundant launch file.
    ///
    /// With authority first, this produced a *loud* `NotTheOwner` diagnostic
    /// with `first_time: true`, training an operator to ignore the message that
    /// matters.
    #[test]
    fn an_identical_static_from_a_second_publisher_is_silent() {
        let mut i = Ingest::new();
        let s = Sample::identity("base", "lidar", 0);
        i.offer(Topic::TfStatic, &s, &node("/rsp_a"));
        assert_eq!(
            i.offer(Topic::TfStatic, &s, &node("/rsp_b")),
            Action::Drop {
                reason: DropReason::StaticRepeat
            }
        );
        assert_eq!(i.stats().static_conflicts, 0);
        assert_eq!(
            i.stats().dropped_authority,
            0,
            "a redundant launch file is not an authority conflict"
        );
    }

    /// **A `Strict` halt still balances the ledger.**
    ///
    /// `transforms` is incremented for every sample, so a path that returns
    /// without an outcome bucket leaves `balanced()` false forever — the exact
    /// shape `BridgeStats::balanced`'s own doc names as the bug it detects. The
    /// clock-reset halt always counted; the authority halt did not, and the two
    /// disagreeing is what review found.
    ///
    /// Mutant: drop the `dropped_authority += 1` from the `Fatal` arm ⇒ this
    /// fails.
    #[test]
    fn a_strict_halt_leaves_the_ledger_balanced() {
        let mut i = Ingest::with(AuthorityPolicy::Strict, OnClockReset::Halt, None);
        let s = |t: i64| Sample::identity("odom", "base", t);
        assert!(matches!(
            i.offer(Topic::Tf, &s(1_000 * MS), &node("/a")),
            Action::Publish { .. }
        ));
        assert!(matches!(
            i.offer(Topic::Tf, &s(1_010 * MS), &node("/b")),
            Action::Halt {
                reason: HaltReason::AuthorityConflict { .. }
            }
        ));
        assert!(i.stats().balanced(), "{:?}", i.stats());

        // ...and so does the clock halt, which is the path that was already
        // right and is what made the disagreement visible.
        let mut j = Ingest::new();
        j.offer(Topic::Tf, &s(10_000 * MS), &node("/a"));
        assert!(matches!(
            j.offer(Topic::Tf, &s(0), &node("/a")),
            Action::Halt {
                reason: HaltReason::ClockReset { .. }
            }
        ));
        assert!(j.stats().balanced(), "{:?}", j.stats());
    }

    /// **The queue high-water mark only rises**, so a queue that fills between
    /// two polls is still visible.
    #[test]
    fn the_queue_high_water_mark_is_a_maximum_not_a_reading() {
        let mut i = Ingest::new();
        i.note_queue_depth(3);
        i.note_queue_depth(100);
        i.note_queue_depth(0);
        assert_eq!(i.stats().queue_high_water, 100);
        assert!(
            i.stats().queue_saturated(),
            "a queue that hit its KeepLast(100) depth must report saturated"
        );
    }
}
