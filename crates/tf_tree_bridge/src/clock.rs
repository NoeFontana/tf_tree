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
//! # The fact is per edge; the judgment is a quorum
//!
//! §5.5 says the bridge stops on "a detected backward jump beyond a threshold"
//! and does not say what the threshold is measured against. That silence is
//! where `docs/decisions/0011` found a defect: a single [`ClockGuard`] fed every
//! accepted sample on every edge cannot tell two different things apart.
//!
//! - A real `/clock` reset — a bag loop, a sim reset — moves **every** edge
//!   backwards at once.
//! - A publisher's `transform_tolerance` (AMCL and `robot_localization` date
//!   `map -> odom` up to a second into the future; a SLAM node dates it hundreds
//!   of milliseconds behind its last keyframe) is a *persistent* offset of
//!   **one** edge relative to another, while each edge on its own stays
//!   perfectly monotone.
//!
//! Against one shared high-water mark those two look identical, and no choice of
//! threshold separates them, because the offset is a configurable parameter that
//! ranges over exactly the same magnitudes a reset does. So the guard is per
//! edge — where it measures one publisher's own stamp regression, a quantity
//! that means something — and the promotion from "this edge regressed" to "the
//! clock moved" is an explicit rule with its own window: [`ResetQuorum`].
//!
//! The offline half, `tf_tree_ingest`, already keeps a guard per edge and has
//! carried the argument for it in its module docs for longer. It does **not**
//! have the quorum: it halts on the first edge that regresses, because a
//! recording is a closed artefact that is either coherent or is not, and a
//! human is reading the answer. The online half runs unattended on a robot,
//! where a wrong halt is an outage, so it asks for corroboration first. The two
//! halves therefore agree on *scope* and diverge on *promotion*, deliberately.
//!
//! The divergence closes exactly where corroboration is impossible. A deployment
//! with one dynamic publisher has no second party to mistake a restart for, so
//! the online half floors its quorum at what that deployment can supply and
//! halts on the first regression, as the offline half always does — see the
//! floor's argument at [`ResetQuorum::record`].
//!
//! # What the correlation window is measured in, and why it is not time
//!
//! "Inside a short correlation window" needs a clock, and **this crate does not
//! have one**. Every candidate is either untrustworthy or absent:
//!
//! - `sample.stamp_nanos` is the publisher's, in the very domain under
//!   suspicion. Using it would measure the window with the instrument whose
//!   failure is being diagnosed, and `/tf_static` is routinely stamped `0`.
//! - `std::time::Instant::now()` is *available* — this crate is `std` — but it
//!   puts a clock read on a path that runs once per transform at 1 kHz, and it
//!   would make `tests/steady_state_alloc.rs` and the CLI's corpus tests depend
//!   on wall-clock timing. Nothing else in the bridge reads a clock, and this is
//!   not the feature to start with.
//! - There is no tick, timer or callback: the crate is a pure decision pipeline
//!   driven entirely by the caller's `offer`.
//!
//! So the window is measured in **observations** — a monotone ordinal the caller
//! already keeps (`BridgeStats::transforms`, incremented unconditionally on
//! entry to `Ingest::offer`, or a record index in a recording). Three properties
//! make this the right unit rather than a consolation prize:
//!
//! 1. **A publisher cannot forge it.** It counts what the bridge did, not what
//!    anyone claimed the time was. The most a hostile or broken publisher can do
//!    is spend the window faster by sending more messages — which makes a halt
//!    *less* likely, and false halts are the failure this decision exists to
//!    remove.
//! 2. **It self-scales.** A window of N observations is a shorter wall-clock
//!    window on a busier stream — and a busier stream also re-publishes every
//!    edge sooner, so the two effects move together.
//! 3. **It is deterministic**, so the tests below pin the boundary exactly
//!    instead of sleeping.
//!
//! The honest limitation: a tree whose aggregate rate is dominated by one very
//! fast edge can burn the window before a very slow edge produces its first
//! post-reset sample. The quorum is then not reached and the bridge degrades to
//! dropping and counting — which is the conservative direction (Phase 1 would
//! reject those stamps anyway; `dropped_non_monotonic` climbs and `tf_tree
//! doctor` reads it), not a silent acceptance of bad data.

use crate::edgemap::{insert, iter, lookup_mut, ByEdge};

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
    /// **Not a reset**, and the distinction matters. A guard watches one edge,
    /// so this is one publisher's stamps arriving slightly out of order — which
    /// happens routinely, from a node that fills several `TransformStamped`s
    /// with slightly different capture times into one `TFMessage`, from a
    /// best-effort transport that reorders, and from any publisher with more
    /// than one thread. Treating a few milliseconds of that as a bag loop would
    /// restart the arena roughly continuously. The sample is dropped and
    /// counted; Phase 1 would have rejected it anyway, and dropping it here
    /// means the engine never sees an error worth logging.
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
/// covers it. That is what per-edge scoping is for, and why raising this
/// constant was rejected as the fix (`docs/decisions/0011`).
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

/// How many **distinct publishers** must be regressing before the bridge calls
/// it a clock reset.
///
/// Two, and not three, because two is the smallest number that cannot be one
/// publisher. One publisher regressing is a node restarting, hiccuping, or
/// replaying its own buffer, and the correct response is to drop that sample and
/// carry on. The moment a second, independent publisher regresses inside the
/// same window, the only common cause left is the clock they share.
///
/// The name is historical: this counted distinct *edges* until the correction
/// recorded on [`Regression::owner`] showed that one node owning two dynamic
/// edges made a quorum of edges fire on exactly the single-publisher event the
/// rule exists to tolerate. It is a ceiling, not a fixed demand —
/// [`ResetQuorum::record`] floors it by what the deployment can supply.
pub const QUORUM_EDGES: usize = 2;

/// Default correlation window: **4096 observations**.
///
/// See the module docs for why the unit is observations and not nanoseconds.
/// The size is chosen between two failure modes:
///
/// - **Too short** and a real reset is missed, because a slow edge has not
///   produced its first post-reset sample before the window closes.
/// - **Too long** and two unrelated single-publisher faults minutes apart get
///   correlated into a halt — the false halt this whole mechanism exists to
///   remove, reintroduced through the back door.
///
/// 4096 is `docs/decisions/0011`'s figure, and its derivation: a typical `/tf`
/// carries about 20 transforms per message at 100 Hz, so 4096 observations is
/// roughly **two seconds** of a busy stream — comfortably more than one publish
/// period of a 1 Hz publisher, so every publisher contributes a post-reset
/// sample before the window closes. On a sparser stream it is proportionally
/// more wall time, which is the right direction: a slow stream also takes longer
/// to deliver each publisher's next message, and 0011 records the two effects
/// moving together as the reason the unit is observations at all.
pub const DEFAULT_CORRELATION_WINDOW: u64 = 4096;

/// How many distinct regressing edges the quorum will remember.
///
/// See the bound's justification at its use in [`ResetQuorum::record`].
const MAX_TRACKED_EDGES: usize = 1024;

/// What the quorum decided about a regression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuorumVerdict {
    /// No second publisher is regressing right now. **One publisher**: drop
    /// the sample, count it, carry on.
    Isolated,
    /// Enough distinct publishers regressed inside the window. **The clock**:
    /// apply the [`OnClockReset`] policy.
    Reached {
        /// How many distinct **edges** are inside the window.
        ///
        /// Edges, not publishers: the publisher count is what decided the
        /// verdict, and the edge count is what an operator can go and look at.
        /// It is at least one, and is *not* bounded below by [`QUORUM_EDGES`] —
        /// a deployment with a single dynamic edge has its quorum floored to one
        /// by [`ResetQuorum::record`], and reports `edges: 1`.
        ///
        /// Carried because the C seam has exactly one `(parent, child)` pair on
        /// its outcome POD and a free-text `detail`: naming the triggering edge
        /// and saying how many corroborated is the whole diagnosis an operator
        /// gets, and "3 edges" versus "2 edges" is the difference between a
        /// confident reset and a marginal one.
        edges: usize,
    },
}

/// One edge's regression history, to window resolution.
///
/// Not `Copy` since it gained [`Regression::owner`]; nothing copied one.
#[derive(Clone, Debug)]
struct Regression {
    /// When this bout of regressing **started**.
    ///
    /// Deliberately not refreshed while the edge keeps regressing. An edge that
    /// has been publishing stale stamps for an hour is a broken publisher, not
    /// evidence about the clock, and if its onset were renewed on every sample
    /// it would sit in the window forever and turn the next unrelated hiccup
    /// anywhere in the tree into a halt.
    onset: u64,
    /// The most recent regression, which is what keeps the row alive. A row
    /// whose edge has been quiet for a whole window is forgotten, so the same
    /// edge regressing again much later is a fresh onset and not a continuation.
    last: u64,
    /// Who was publishing this edge when it regressed.
    ///
    /// **The quorum counts distinct publishers, not distinct edges**, and the
    /// difference is the whole premise. Every argument for the rule is about
    /// publishers — "two publishers do not restart in lockstep" — and one node
    /// that owns two dynamic edges falsifies it: when it restarts, both of its
    /// edges regress in the same instant, two *edges* reach a quorum, and the
    /// bridge halts on exactly the single-publisher event this design exists to
    /// stop halting on. A localization node owning `map -> odom` and
    /// `odom -> base_link` is an ordinary deployment, not a corner.
    ///
    /// Owned rather than borrowed because the row outlives the offer. It costs
    /// one allocation per edge *per bout of regressing* — a path that is by
    /// definition not the steady state — and `MAX_TRACKED_EDGES` bounds how many
    /// can exist.
    owner: String,
}

/// Promotes per-edge regressions into a clock-reset judgment.
///
/// Sits **above** [`ClockGuard`], never inside it: the guard's job is the exact
/// per-edge fact, and mixing the promotion rule into it would put a global
/// judgment back in the primitive, which is the defect `docs/decisions/0011`
/// records. The guards stay reusable by the offline half, which wants the fact
/// and applies a different promotion rule to it.
#[derive(Debug)]
pub struct ResetQuorum {
    regressed: ByEdge<Regression>,
    window: u64,
    /// The highest ordinal seen, so a caller that hands over a stale one cannot
    /// make two far-apart regressions look adjacent. See [`ResetQuorum::record`].
    newest_seq: u64,
    quorums: u64,
}

impl Default for ResetQuorum {
    fn default() -> ResetQuorum {
        ResetQuorum::new()
    }
}

impl ResetQuorum {
    /// A quorum with the default window.
    #[must_use]
    pub fn new() -> ResetQuorum {
        ResetQuorum::with_window(DEFAULT_CORRELATION_WINDOW)
    }

    /// A quorum with an explicit correlation window, in observations.
    ///
    /// A window of `0` is legal and means "the two regressions must land on the
    /// same observation ordinal" — effectively never, so the bridge degrades to
    /// dropping and counting. That is a coherent thing to ask for and is the
    /// conservative direction, so it is not clamped upwards.
    #[must_use]
    pub fn with_window(window: u64) -> ResetQuorum {
        ResetQuorum {
            regressed: ByEdge::new(),
            window,
            newest_seq: 0,
            quorums: 0,
        }
    }

    /// Record that `parent -> child` regressed past its guard's threshold, at
    /// observation `at_seq`, and say what that means.
    ///
    /// `at_seq` is the caller's monotone observation ordinal — `Ingest` passes
    /// `BridgeStats::transforms`, which is incremented unconditionally on entry
    /// to `offer` and so cannot be skipped, and an offline caller passes a
    /// record index. It is **not** a timestamp; see the module docs.
    ///
    /// A caller that hands over an ordinal *below* one already seen is buggy,
    /// and this handles it rather than trusting it. The ageing basis is the
    /// high-water mark — subtracting a stale ordinal from it would underflow a
    /// `u64` and panic — while the row itself keeps the stale ordinal it was
    /// given, so it arrives already outside the window instead of arriving
    /// fresh. Both halves push the same way: a broken ordinal can only make a
    /// quorum harder to reach, never easier, which is the safe direction for a
    /// rule whose false positive is halting a healthy robot.
    ///
    /// The magnitude of the regression is deliberately **not** an argument. The
    /// threshold decision has already been taken, per edge and exactly, by
    /// [`ClockGuard::observe`]; taking `by_nanos` here would invite a second
    /// threshold, tuned separately, in a second place — and two thresholds that
    /// can disagree about the same sample is how this pipeline got its first
    /// defect.
    ///
    /// # Bounded
    ///
    /// The keys come from outside — `Ingest` reaches the clock step only for
    /// edges the topology file declared, but this type cannot verify that its
    /// caller filtered anything, and a table keyed by a string somebody else
    /// chose is the growth bug [`crate::NameNormalizer`] already had to cap.
    /// So at most `MAX_TRACKED_EDGES` (1024) rows are held, and past that a
    /// previously unseen edge is not recorded.
    ///
    /// Reaching the cap means the caller ignored more than a thousand
    /// consecutive `Reached` verdicts, since the second distinct edge already
    /// returns one. Refusing the row is nonetheless the right degradation: it
    /// can only make a halt harder to reach, never easier.
    /// `owner` names the publisher of the regressing edge; the quorum counts
    /// distinct owners, for the reason [`Regression::owner`] gives. An
    /// unattributed publisher collapses to one identity, which can only make a
    /// quorum *harder* to reach — the safe direction.
    ///
    /// `corroborators` is how many distinct publishers this deployment could
    /// possibly supply — **the floor on what may be demanded of it.** The
    /// effective quorum is `min(QUORUM_EDGES, corroborators)`, never more, and
    /// never less than one.
    ///
    /// # Why a floor exists at all
    ///
    /// Without it a robot with a single dynamic publisher can never reach a
    /// quorum, so §5.5's reset detection is not merely degraded there but
    /// **structurally unreachable, and silently so**: every regression forever
    /// returns [`QuorumVerdict::Isolated`] and `clock_resets` stays zero through
    /// a bag loop. A single-publisher deployment is the shape of half this
    /// repository's own fixtures.
    ///
    /// And demanding corroboration there was never justified in the first place.
    /// Quorum exists to separate "this publisher restarted" from "the clock
    /// moved", and that ambiguity requires two publishers to exist. With one,
    /// there is no second party to mistake it for: a backward jump past the
    /// threshold is unambiguous, and the pre-quorum behaviour — halt on the
    /// first one — is not a fallback but the correct answer. The floor is what
    /// makes the rule degrade *into* correctness rather than out of it.
    pub fn record(
        &mut self,
        parent: &str,
        child: &str,
        owner: &str,
        at_seq: u64,
        corroborators: usize,
    ) -> QuorumVerdict {
        let now = at_seq.max(self.newest_seq);
        self.newest_seq = now;
        self.forget_quiet(now);

        // Two steps rather than one `entry`, and the borrow is released between
        // them on purpose: probing by reference allocates nothing, and the cap
        // is only worth computing when a row would actually be added.
        let known = match lookup_mut(&mut self.regressed, parent, child) {
            // Still the same bout: `last` moves, `onset` does not. The owner is
            // refreshed, because an edge that changed hands between two bouts is
            // being published by whoever is publishing it *now*.
            Some(r) => {
                r.last = now;
                if r.owner != owner {
                    r.owner.clear();
                    r.owner.push_str(owner);
                }
                true
            }
            None => false,
        };
        if !known && self.tracked() < MAX_TRACKED_EDGES {
            // `onset: at_seq`, not `now`: see the note above on a stale ordinal.
            let r = Regression {
                onset: at_seq,
                last: now,
                owner: owner.to_string(),
            };
            insert(&mut self.regressed, parent, child, r);
        }

        let publishers = self.fresh_publishers(now);
        // The floor: never demand more corroboration than the deployment can
        // supply, and never demand less than one.
        let needed = QUORUM_EDGES.min(corroborators.max(1));
        if publishers >= needed {
            self.quorums += 1;
            QuorumVerdict::Reached {
                edges: self.fresh(now),
            }
        } else {
            QuorumVerdict::Isolated
        }
    }

    /// Forget everything, after an [`OnClockReset::Recreate`].
    ///
    /// The regressions that caused the recreate describe the arena that is being
    /// thrown away; carrying them into the new one would let a single hiccup
    /// after the rebuild join a quorum with edges from before it.
    pub fn clear(&mut self) {
        self.regressed.clear();
    }

    /// How many edges have a live row, whether or not their onset is still
    /// inside the window.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.regressed.values().map(|c| c.len()).sum()
    }

    /// How many quorums have been reported (§5.9).
    ///
    /// Counts verdicts, not distinct resets: a caller that keeps offering after
    /// a `Reached` keeps being told the same thing, which is what the latch on
    /// the C seam is for.
    #[must_use]
    pub fn quorums(&self) -> u64 {
        self.quorums
    }

    /// Distinct edges whose bout of regressing **started** inside the window.
    ///
    /// The plain subtraction is safe because `record` establishes
    /// `now >= newest_seq >= every stored onset and last` before calling this —
    /// that is what the clamp on `at_seq` is for, and the invariant is stated
    /// here rather than hidden behind a `saturating_sub` that would silently
    /// paper over a caller bug.
    fn fresh(&self, now: u64) -> usize {
        iter(&self.regressed)
            .filter(|(_, _, r)| now - r.onset <= self.window)
            .count()
    }

    /// Distinct **publishers** among the rows still inside the window.
    ///
    /// This, and not [`fresh`](ResetQuorum::fresh), is what the quorum compares:
    /// see [`Regression::owner`]. `fresh` survives as the number reported in
    /// [`QuorumVerdict::Reached`], because "three edges regressed" is what an
    /// operator can go and look at, while the publisher count is what decided it.
    ///
    /// A `BTreeSet` of borrowed names rather than a sort: the row count is capped
    /// at `MAX_TRACKED_EDGES` and this runs only on a regression, never in the
    /// steady state.
    fn fresh_publishers(&self, now: u64) -> usize {
        let mut owners: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for (_, _, r) in iter(&self.regressed) {
            if now - r.onset <= self.window {
                owners.insert(r.owner.as_str());
            }
        }
        owners.len()
    }

    /// Drop rows whose edge has not regressed for a whole window.
    ///
    /// Retention is keyed on `last` rather than `onset` so a persistently broken
    /// publisher keeps exactly one row instead of re-inserting a fresh one — the
    /// row survives, its onset ages out, and it stops counting toward a quorum
    /// it should not be evidence for.
    fn forget_quiet(&mut self, now: u64) {
        let window = self.window;
        self.regressed.retain(|_, children| {
            children.retain(|_, r| now - r.last <= window);
            !children.is_empty()
        });
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

    // ---- ResetQuorum -------------------------------------------------------

    /// A window small enough to write the boundary cases out by hand. The
    /// default (4096) is sized for a robot; these tests are about the rule.
    const W: u64 = 8;

    /// A deployment with more dynamic edges than [`QUORUM_EDGES`] could ever
    /// ask for, so the corroboration floor never binds.
    ///
    /// Every test *about the quorum rule* passes this. Passing exactly 2 would
    /// silently disarm the `QUORUM_EDGES: usize = 3` mutants below — the floor
    /// would clamp the raised constant straight back to 2 and the tests would
    /// keep passing on mutated code. The floor's own behaviour is pinned
    /// separately, by the two `corroborators` tests at the end.
    const PLENTY: usize = 8;

    /// **One edge regressing is a publisher, not the clock.**
    ///
    /// This is the false halt `docs/decisions/0011` exists to remove: a wheel
    /// driver restarting, or a node replaying its own buffer, regresses exactly
    /// one edge, and halting a healthy robot for it is an outage caused by the
    /// diagnostic rather than by the fault.
    ///
    /// Mutant: `QUORUM_EDGES: usize = 1` — applied, and this failed at
    /// `Reached { edges: 1 } != Isolated` on the very first regression, which
    /// is exactly the pre-0011 behaviour restored.
    #[test]
    fn a_single_edge_regressing_is_a_publisher_not_the_clock() {
        let mut q = ResetQuorum::with_window(W);
        for seq in 0..50 {
            assert_eq!(
                q.record("odom", "base", "wheels", seq, PLENTY),
                QuorumVerdict::Isolated,
                "one publisher hiccuping, at observation {seq}"
            );
        }
        assert_eq!(q.quorums(), 0);
    }

    /// **Two edges from two different publishers inside the window are the
    /// clock.**
    ///
    /// A real `/clock` rewind moves every edge at once, so the second
    /// publisher's first post-rewind sample arrives within a handful of
    /// observations of the first's. That corroboration is the only thing that
    /// separates a reset from a publisher, and this is the test that the
    /// narrowing to per-edge guards did not quietly delete the reset detector.
    ///
    /// Every edge here has a **distinct** owner, and that is load-bearing rather
    /// than decorative: the quorum counts publishers, so three edges owned by
    /// one node would prove nothing about this rule. Its converse — two edges,
    /// one owner — is
    /// `two_edges_from_one_publisher_are_one_restart_not_the_clock`.
    ///
    /// Mutant: `QUORUM_EDGES: usize = 3` — applied, and this failed at
    /// `Isolated != Reached { edges: 2 }`: a bag loop across two publishers
    /// stopped being detectable at all on a two-publisher tree.
    #[test]
    fn two_distinct_edges_inside_the_window_are_the_clock() {
        let mut q = ResetQuorum::with_window(W);
        assert_eq!(
            q.record("odom", "base", "wheels", 100, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(
            q.record("map", "odom", "amcl", 102, PLENTY),
            QuorumVerdict::Reached { edges: 2 },
            "two publishers cannot both be wrong by themselves"
        );
        // A third edge joining raises the count, because the C seam has one
        // `(parent, child)` pair and a detail string, and "3 edges" is the
        // difference between a confident diagnosis and a marginal one.
        assert_eq!(
            q.record("base", "lidar", "lidar_driver", 103, PLENTY),
            QuorumVerdict::Reached { edges: 3 }
        );
        assert_eq!(q.quorums(), 2);
    }

    /// **Two edges far apart are two publishers**, however many they add up to.
    ///
    /// Without a window the quorum degenerates into "two edges have *ever*
    /// regressed", which on a fortnight-long unattended run is true of any
    /// robot — and the halt then fires on the second unrelated fault, hours
    /// after the first, naming neither cause.
    ///
    /// Mutant: make `forget_quiet` a no-op, so rows accumulate forever —
    /// applied, and this failed at `2 != 1` on the tracked-rows assertion. The
    /// *verdict* survived that mutant, because `fresh` windows on `onset` as
    /// well and the two mechanisms overlap for an edge that went quiet; the
    /// case that separates them is an edge that never goes quiet, which is
    /// `a_persistently_regressing_edge_stops_counting_toward_a_quorum`. Hence
    /// the second assertion here: without it this test proves less than it
    /// looks like it proves.
    #[test]
    fn two_distinct_edges_outside_the_window_are_two_publishers() {
        let mut q = ResetQuorum::with_window(W);
        assert_eq!(
            q.record("odom", "base", "wheels", 0, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(
            q.record("map", "odom", "amcl", 10_000, PLENTY),
            QuorumVerdict::Isolated,
            "ten thousand observations later is a different incident"
        );
        assert_eq!(q.quorums(), 0);
        assert_eq!(
            q.tracked(),
            1,
            "the first edge's row was forgotten, not merely ignored"
        );
    }

    /// **The same edge twice is one edge.**
    ///
    /// The rule counts *distinct* rows — and, above them, distinct publishers —
    /// never regressions, and the difference is the whole decision: a single
    /// publisher republishing a stale buffer emits a regression per message at
    /// message rate, and a quorum that counted events would reach two within a
    /// millisecond of the first fault.
    ///
    /// Mutant: count regressions instead of edges (`self.events += 1` per
    /// `record`, compared against `QUORUM_EDGES` in place of
    /// `fresh_publishers(now)`) — applied, and this failed at
    /// `Reached { edges: 2 } != Isolated` on the second call, from one
    /// publisher.
    #[test]
    fn the_same_edge_regressing_twice_is_still_one_edge() {
        let mut q = ResetQuorum::with_window(W);
        assert_eq!(
            q.record("odom", "base", "wheels", 10, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(
            q.record("odom", "base", "wheels", 11, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(
            q.record("odom", "base", "wheels", 12, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(q.tracked(), 1, "one row per edge, not one per regression");
        assert_eq!(q.quorums(), 0);
    }

    /// **Exactly at the window is inside it; one observation past is not.**
    ///
    /// The boundary is where an off-by-one lives, and both sides have to be
    /// pinned or the window can drift by one with nothing noticing.
    ///
    /// Mutant: `now - r.onset < self.window` instead of `<=` — applied, and
    /// this failed at `Isolated != Reached { edges: 2 }` for the pair separated
    /// by exactly `W`.
    #[test]
    fn the_correlation_window_boundary_is_exact() {
        let mut inside = ResetQuorum::with_window(W);
        assert_eq!(
            inside.record("odom", "base", "wheels", 0, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(
            inside.record("map", "odom", "amcl", W, PLENTY),
            QuorumVerdict::Reached { edges: 2 }
        );

        let mut outside = ResetQuorum::with_window(W);
        assert_eq!(
            outside.record("odom", "base", "wheels", 0, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(
            outside.record("map", "odom", "amcl", W + 1, PLENTY),
            QuorumVerdict::Isolated
        );
    }

    /// **An edge that has been broken for hours is not evidence about the
    /// clock.**
    ///
    /// A publisher stuck emitting stale stamps regresses on every message
    /// forever. If each of those refreshed its onset, that edge would sit
    /// inside the window permanently and the *next* unrelated hiccup anywhere
    /// in the tree — a different node restarting, days later — would form a
    /// quorum with it and halt the robot. So the onset is the start of the
    /// bout, and only going quiet for a whole window can start a new one.
    ///
    /// Mutant: refresh the onset too (`r.onset = now;` beside `r.last = now;`)
    /// — applied, and this failed at `Reached { edges: 2 } != Isolated`: the
    /// permanently broken edge conspired with a hiccup 100 observations later.
    #[test]
    fn a_persistently_regressing_edge_stops_counting_toward_a_quorum() {
        let mut q = ResetQuorum::with_window(W);
        for seq in 0..100 {
            assert_eq!(
                q.record("odom", "base", "wheels", seq, PLENTY),
                QuorumVerdict::Isolated
            );
        }
        assert_eq!(
            q.record("map", "odom", "amcl", 100, PLENTY),
            QuorumVerdict::Isolated,
            "a fresh fault plus an old one is not a clock reset"
        );
        assert_eq!(
            q.tracked(),
            2,
            "the broken edge keeps exactly one row, however long it misbehaves"
        );

        // And going quiet for a whole window does start a new bout: after the
        // row is forgotten, the two edges regressing together are the clock
        // again.
        assert_eq!(
            q.record("odom", "base", "wheels", 200, PLENTY),
            QuorumVerdict::Isolated,
            "the map edge's row aged out too"
        );
        assert_eq!(
            q.record("map", "odom", "amcl", 201, PLENTY),
            QuorumVerdict::Reached { edges: 2 }
        );
    }

    /// **A stale observation ordinal cannot forge a quorum, or panic.**
    ///
    /// The ordinal is the caller's, and the ageing arithmetic is unsigned, so a
    /// caller that hands over an ordinal below one already seen would subtract
    /// its way off the bottom of a `u64`. Clamping the *basis* to the
    /// high-water mark fixes that; keeping the *row's* onset at the value it
    /// was given is what stops the stale row from arriving fresh and
    /// completing a quorum it is not entitled to.
    ///
    /// Mutant: `let now = at_seq;` (no clamp) — applied, and this failed at
    /// `attempt to subtract with overflow` in `forget_quiet`, which in a
    /// release build would not have panicked at all: it would have wrapped to
    /// a colossal age and dropped every row.
    #[test]
    fn a_stale_ordinal_cannot_forge_a_quorum() {
        let mut q = ResetQuorum::with_window(W);
        assert_eq!(
            q.record("odom", "base", "wheels", 1_000, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(
            q.record("map", "odom", "amcl", 5, PLENTY),
            QuorumVerdict::Isolated,
            "an ordinal from the distant past is not corroboration for now"
        );
        assert_eq!(q.quorums(), 0);
    }

    /// **The table is bounded**, because its keys are chosen by somebody else.
    ///
    /// `Ingest` only reaches the clock step for declared edges, but this type
    /// cannot check that its caller filtered anything, and a map keyed by a
    /// publisher-controlled string on a bridge asked to run unattended for a
    /// fortnight is the growth bug `NameNormalizer::seen` already had to cap.
    /// Past the cap a new edge is refused, which can only make a halt harder to
    /// reach — never easier.
    ///
    /// Mutant: drop the `self.tracked() < MAX_TRACKED_EDGES` guard — applied,
    /// and this failed at `3000 != 1024`, i.e. unbounded growth keyed on
    /// whatever a publisher sent.
    #[test]
    fn the_regression_table_is_capped() {
        // A window long enough that nothing ages out, so the cap is the only
        // thing that can bound the table.
        let mut q = ResetQuorum::with_window(u64::MAX);
        for i in 0..3_000u64 {
            // One owner throughout, so the flood cannot reach a quorum and this
            // test stays about the row count and nothing else.
            q.record("odom", &format!("child{i}"), "flood", i, PLENTY);
        }
        assert_eq!(q.tracked(), MAX_TRACKED_EDGES);
    }

    /// **A recreate throws the evidence away with the arena.**
    ///
    /// The rows describe regressions against a high-water mark that no longer
    /// exists. Carrying them over would let the first ordinary hiccup after the
    /// rebuild join a quorum with edges from the recording before it, and
    /// `--on-clock-reset=recreate` exists precisely for a bag replay that loops
    /// repeatedly.
    ///
    /// Mutant: `pub fn clear(&mut self) {}` — applied, and this failed at
    /// `2 != 0`: the pre-recreate edges were still on the books, and the first
    /// ordinary hiccup after the rebuild halted the freshly built arena.
    #[test]
    fn clear_forgets_the_arena_that_was_thrown_away() {
        let mut q = ResetQuorum::with_window(W);
        assert_eq!(
            q.record("odom", "base", "wheels", 10, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(
            q.record("map", "odom", "amcl", 11, PLENTY),
            QuorumVerdict::Reached { edges: 2 }
        );
        q.clear();
        assert_eq!(q.tracked(), 0);
        assert_eq!(
            q.record("base", "lidar", "lidar_driver", 12, PLENTY),
            QuorumVerdict::Isolated,
            "the new arena starts with no history"
        );
        assert_eq!(
            q.quorums(),
            1,
            "the counter describes the bridge's life and survives the clear"
        );
    }

    /// **One node owning two edges is one restart, not the clock.**
    ///
    /// This is the correction that turned the rule from edges to publishers, and
    /// it is not a corner case: a localization node that owns `map -> odom` and
    /// `odom -> base_link` is an ordinary deployment. When it restarts, both of
    /// its edges regress in the same instant — a quorum of *edges* is met by one
    /// process hiccuping, which is precisely the false halt `0011` exists to
    /// remove, reintroduced by the mechanism meant to remove it.
    ///
    /// Mutant: count edges rather than owners (`self.fresh(now)` in place of
    /// `self.fresh_publishers(now)` on the quorum comparison) — applied, and
    /// this failed at `Reached { edges: 2 } != Isolated` on the second edge,
    /// from the single node that owns them both.
    #[test]
    fn two_edges_from_one_publisher_are_one_restart_not_the_clock() {
        let mut q = ResetQuorum::with_window(W);
        assert_eq!(
            q.record("map", "odom", "amcl", 10, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(
            q.record("odom", "base", "amcl", 11, PLENTY),
            QuorumVerdict::Isolated,
            "the same node's other edge is not a second witness"
        );
        // A third edge, still the same node, still one restart.
        assert_eq!(
            q.record("base", "lidar", "amcl", 12, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(q.tracked(), 3, "three rows, and still one publisher");
        assert_eq!(q.quorums(), 0);
    }

    /// **Two edges from two nodes are the clock, at the same ordinals that one
    /// node's two edges were not.**
    ///
    /// The pair to
    /// `two_edges_from_one_publisher_are_one_restart_not_the_clock`: identical
    /// edges, identical window, identical observation ordinals, and only the
    /// owners differ — so this pins that narrowing the rule to publishers did
    /// not narrow it into never firing. Two independent nodes do not restart in
    /// lockstep; a clock they share does move both at once.
    ///
    /// Mutant: ignore the owner and count one identity for everything
    /// (`owners.insert("")` in `fresh_publishers`) — applied, and this failed at
    /// `Isolated != Reached { edges: 2 }`: the reset detector was gone
    /// altogether, which the same-owner test alone would not have caught.
    #[test]
    fn two_edges_from_different_publishers_are_the_clock() {
        let mut q = ResetQuorum::with_window(W);
        assert_eq!(
            q.record("map", "odom", "amcl", 10, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(
            q.record("odom", "base", "wheel_driver", 11, PLENTY),
            QuorumVerdict::Reached { edges: 2 },
            "two nodes do not restart in lockstep; their shared clock moves"
        );
        assert_eq!(q.quorums(), 1);
    }

    /// **A robot with one dynamic edge halts on the first regression**, because
    /// there is nobody there to corroborate it.
    ///
    /// Without the floor, `QUORUM_EDGES` demands a second publisher a
    /// single-publisher deployment cannot ever produce, so §5.5's reset
    /// detection is not degraded there but **structurally unreachable, and
    /// silently so** — a bag loop reports `dropped_non_monotonic: 500` and
    /// `clock_resets: 0`, and nothing in the diagnostics says the rule was never
    /// applicable. Demanding corroboration was never justified here anyway: the
    /// quorum separates "this publisher restarted" from "the clock moved", and
    /// that ambiguity needs two publishers to exist. With one, a past-threshold
    /// jump is unambiguous.
    ///
    /// Mutant: drop the floor (`let needed = QUORUM_EDGES;`) — applied, and this
    /// failed at `Isolated != Reached { edges: 1 }`, which is the silent
    /// unreachability exactly: the one regression this deployment can ever
    /// produce never promotes.
    #[test]
    fn a_lone_dynamic_edge_reaches_a_quorum_by_itself() {
        let mut q = ResetQuorum::with_window(W);
        assert_eq!(
            q.record("odom", "base", "wheels", 0, 1),
            QuorumVerdict::Reached { edges: 1 },
            "with one possible witness, the first regression is the answer"
        );
        assert_eq!(q.quorums(), 1);
    }

    /// **A deployment that declares no corroborators still needs one.**
    ///
    /// The floor is `corroborators.max(1)`, and the `max` is not decoration: a
    /// caller that reports zero — a topology with no dynamic edges at all, or a
    /// future caller that has not counted them — must not drive the demand to
    /// zero, because a quorum of zero is reached by the empty set. That is a
    /// halt caused by no evidence whatsoever.
    ///
    /// Reaching that state takes a deliberate arrangement, because `record`
    /// normally leaves the arriving edge fresh: here the arriving edge's row is
    /// kept **alive** by its recent `last` while its `onset` — pinned to the
    /// stale ordinal it was created with — ages out, and the only other row is
    /// dropped by `forget_quiet` on the same call. So the comparison runs
    /// against zero fresh publishers, which is the only place the two clamps
    /// differ.
    ///
    /// Mutant: `corroborators` in place of `corroborators.max(1)` — applied, and
    /// this failed at `Reached { edges: 0 } != Isolated`: a halt reported with
    /// no corroborating edge at all.
    #[test]
    fn a_zero_corroborator_count_still_demands_one_witness() {
        let mut q = ResetQuorum::with_window(W);
        // Row 1 is created at 1000 and never touched again; it dies at 1009.
        q.record("odom", "base", "wheels", 1_000, 0);
        // Row 2 is created with a *stale* ordinal, so its onset is 5 while the
        // high-water mark is 1000 — already outside the window at birth.
        q.record("map", "odom", "amcl", 5, 0);
        // Kept alive: `last` moves to 1005, `onset` stays 5.
        q.record("map", "odom", "amcl", 1_005, 0);
        // Those three only arranged the state. Each still had a fresh publisher
        // on the books and so reached the floored quorum, which is what
        // `a_lone_dynamic_edge_reaches_a_quorum_by_itself` is for and not what
        // is under test here.
        let before = q.quorums();
        // At 1009 row 1 has been quiet for 9 > W and is forgotten, and row 2's
        // onset is 1004 observations old. Nothing is fresh.
        assert_eq!(
            q.record("map", "odom", "amcl", 1_009, 0),
            QuorumVerdict::Isolated,
            "zero fresh publishers is not a quorum, whatever was declared"
        );
        assert_eq!(
            q.quorums(),
            before,
            "no halt may be reported on no evidence"
        );
    }

    /// **An edge that changes hands is counted under whoever publishes it
    /// now.**
    ///
    /// A row outlives the publisher that created it — retention is keyed on
    /// `last`, so an edge regressing steadily keeps one row across a handover —
    /// and the owner stored in it is evidence about the present, not a record of
    /// who was there first. A row still naming a departed node would count that
    /// node as a distinct witness for edges it no longer publishes, and
    /// conversely, as here, would hide a genuine second publisher behind a stale
    /// name.
    ///
    /// Mutant: drop the refresh (delete the `if r.owner != owner { … }` block in
    /// the `Some` arm) — applied, and this failed at `Isolated != Reached
    /// { edges: 2 }`: `map -> odom` was still credited to `nav`, so two real
    /// publishers read as one and the clock reset went undetected.
    #[test]
    fn an_edge_that_changes_hands_is_counted_under_its_new_owner() {
        let mut q = ResetQuorum::with_window(W);
        // Both edges are `nav`'s: one node, one restart, no quorum.
        assert_eq!(
            q.record("map", "odom", "nav", 0, PLENTY),
            QuorumVerdict::Isolated
        );
        assert_eq!(
            q.record("odom", "base", "nav", 1, PLENTY),
            QuorumVerdict::Isolated
        );
        // `map -> odom` is taken over by a second node, inside the window and
        // without the row ageing out, so the row is refreshed rather than
        // rebuilt.
        assert_eq!(
            q.record("map", "odom", "slam", 2, PLENTY),
            QuorumVerdict::Reached { edges: 2 },
            "two publishers now, on the same two edges"
        );
        assert_eq!(q.tracked(), 2, "a handover refreshes the row, it adds none");
    }
}
