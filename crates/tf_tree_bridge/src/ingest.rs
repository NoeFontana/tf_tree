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

use std::collections::BTreeMap;

use crate::authority::{Authority, AuthorityPolicy, Verdict};
use crate::clock::{ClockGuard, ClockVerdict, OnClockReset};
use crate::config::TopologyConfig;
use crate::edgemap::{insert, lookup_mut, ByEdge};
use crate::names::NameNormalizer;
use crate::statics::{StaticStore, StaticVerdict};
use crate::stats::BridgeStats;
use crate::{Publisher, Sample};

/// Distinct undeclared *parent* frames remembered by [`Ingest::undeclared`].
///
/// See the bound's justification at its use in [`Ingest::offer`]. A topology
/// this far off its config is already a misconfiguration the report names; what
/// the cap buys is that the misconfiguration cannot also exhaust the bridge.
const MAX_UNDECLARED_PARENTS: usize = 256;
/// Distinct undeclared children remembered per parent. See
/// [`MAX_UNDECLARED_PARENTS`].
const MAX_UNDECLARED_CHILDREN: usize = 256;

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
    /// A `/tf_static` value that **matches the declared constant**. Nothing to
    /// write; the arena already holds it.
    ///
    /// This is what `docs/PHASE4.md` §5.8's amendment turned `DeclareStatic`
    /// into. That variant asked the caller to perform an operation the engine
    /// does not have — a static edge's pose is inline in
    /// `EdgeRecord::static_pose`, written at build time, and `Tree::claim`
    /// refuses it with `NotDynamic` because its capacity is 0. Nothing outside
    /// this crate consumed it, because nothing could. What is left once the
    /// config declares the constant is *verification*, and that is a report,
    /// not a write.
    StaticVerified {
        /// Normalized parent frame.
        parent: String,
        /// Normalized child frame.
        child: String,
    },
    /// A transform for an edge the topology config does not declare (§5.8).
    ///
    /// **Not a `Drop`**, for the same reason as [`Action::StaticConflict`]: the
    /// amendment requires the diagnostic to name *both frames*, and a
    /// `Drop { reason }` carries neither. `first_time` is what keeps it to one
    /// line per edge rather than one per message — an undeclared 1 kHz edge
    /// would otherwise emit a thousand identical lines a second, the same
    /// failure §5.6 avoids for frame names.
    UndeclaredEdge {
        /// Normalized parent frame.
        parent: String,
        /// Normalized child frame.
        child: String,
        /// First sighting of this edge, for rate limiting.
        first_time: bool,
    },
    /// A sample from a publisher that does not own the edge (§5.4).
    ///
    /// **Not a `Drop`**, and this is the variant §5.4's headline requirement
    /// needs: *"Later publishers' samples are dropped and counted, with a
    /// diagnostic naming **both** nodes and the edge"*, and the diagnostic must
    /// be *"loud, rate-limited"*. A `Drop { reason: NotTheOwner }` carries none
    /// of the three, so the sentence §5.4 calls the better sales pitch —
    /// *"your `/ekf` and `/odom_node` have both been publishing
    /// `odom -> base_link` for eight months"* — could not be written by any
    /// caller, even though [`Verdict::Reject`] had all of it in hand one line
    /// earlier.
    ///
    /// The sample is dropped either way; `stats.dropped_authority` counts it.
    AuthorityConflict {
        /// Normalized parent frame.
        parent: String,
        /// Normalized child frame.
        child: String,
        /// Who owns the edge.
        owner: Publisher,
        /// Who tried to write it.
        intruder: Publisher,
        /// First collision between these two on this edge, for rate limiting.
        first_time: bool,
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
    /// The stamp went backwards, but not far enough to be a reset (§5.5).
    NonMonotonic {
        /// By how much.
        by_nanos: i64,
    },
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

/// The four tables, plus the declared topology and the counters, applied in
/// order.
#[derive(Debug)]
pub struct Ingest {
    names: NameNormalizer,
    /// The declared topology **after** §5.6's normalization — the names this
    /// pipeline actually keys on.
    ///
    /// Kept rather than recomputed because the arena has to be built from
    /// exactly these names: `tft_bridge_create` asks for it instead of building
    /// from the file, so there is one normalized topology in the process and the
    /// store, the claims and the frame table cannot disagree about what an edge
    /// is called.
    declared: TopologyConfig,
    statics: StaticStore,
    authority: Authority,
    clock: ClockGuard,
    stats: BridgeStats,
    /// Undeclared edges seen, and how many times — the rate limiter behind
    /// `Action::UndeclaredEdge`'s `first_time`, and `doctor`'s list of what the
    /// robot publishes that the config forgot.
    ///
    /// A [`ByEdge`] for the same reason [`StaticStore`]'s tables are: the
    /// counter is bumped on **every** message from an undeclared edge, and a
    /// `(String, String)` key cannot be probed by reference. `first_time`
    /// silences the log for a 1 kHz undeclared edge; without this it left the
    /// allocator running at 1 kHz anyway, which is the more expensive half of
    /// what the rate limiter was there to stop.
    undeclared: ByEdge<u64>,
}

impl Ingest {
    /// A pipeline over `config` with the default policies: `FirstWriterWins`,
    /// `Halt`, no `tf_prefix`.
    ///
    /// **The topology is not optional**, and that is the whole of
    /// `docs/PHASE4.md` §5.8's amendment: the engine cannot declare an edge
    /// after `build()`, so a bridge that learned topology from the wire would
    /// be collecting names it can never turn into arena slots. Everything the
    /// bridge will ever write has to be in this file.
    #[must_use]
    pub fn new(config: &TopologyConfig) -> Ingest {
        Ingest::with(
            config,
            AuthorityPolicy::default(),
            OnClockReset::default(),
            None,
        )
    }

    /// A pipeline with explicit policies.
    #[must_use]
    pub fn with(
        config: &TopologyConfig,
        authority: AuthorityPolicy,
        on_clock_reset: OnClockReset,
        tf_prefix: Option<&str>,
    ) -> Ingest {
        // **`config` carries the names as the file writes them; everything this
        // pipeline keys on is the rewritten form.** `tf_prefix` (§5.6) rewrites
        // the wire, and the declared topology has to be rewritten with it or the
        // two never meet: a prefixed bridge would look up `robot1/odom ->
        // robot1/base` in a store seeded with `odom -> base` and report 100 % of
        // a correctly configured robot's traffic as undeclared edges. The
        // rewrite goes through *this* normalizer, the one the wire will use, so
        // the two cannot drift — see `TopologyConfig::rewritten`.
        let mut names = tf_prefix.map_or_else(NameNormalizer::new, NameNormalizer::with_prefix);
        let declared = config.rewritten(&mut names);
        Ingest {
            statics: StaticStore::seeded(&declared),
            names,
            declared,
            authority: Authority::new(authority),
            clock: ClockGuard::new(on_clock_reset),
            stats: BridgeStats {
                queue_capacity: 100, // §5.2's KeepLast(100)
                ..BridgeStats::default()
            },
            undeclared: BTreeMap::new(),
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

        // 2. Declared? Before the kind check, because an undeclared edge has no
        //    declared kind to clash with, and reporting `KindChange` for it
        //    would send an operator looking at `/tf_static` for an edge nobody
        //    ever wrote down.
        if !self.statics.is_declared(&parent, &child) {
            // Fast path first: a repeat must not allocate. `entry()` needs an
            // owned key whether or not it inserts, so reaching for it
            // unconditionally cloned both names on every message of an edge
            // already known to be undeclared.
            // **And bounded.** This table is keyed by a name that arrived from
            // *outside the declared topology* — the one input nothing in the
            // process constrains — and `undeclared()` `collect()`s the whole of
            // it for `doctor`, so an unbounded table is also an unbounded
            // allocation the moment anyone asks the bridge how it is doing. The
            // cap is read before `lookup_mut` because that call holds the
            // mutable borrow across the match.
            //
            // Past the cap the transform is still dropped and still counted in
            // `dropped_undeclared`; only the per-edge breakdown stops growing,
            // and `first_time` reports `false` so the caller stays quiet.
            let at_cap = self.undeclared.len() >= MAX_UNDECLARED_PARENTS
                || self
                    .undeclared
                    .get(parent.as_str())
                    .is_some_and(|c| c.len() >= MAX_UNDECLARED_CHILDREN);
            let first_time = match lookup_mut(&mut self.undeclared, &parent, &child) {
                Some(n) => {
                    *n += 1;
                    false
                }
                None if at_cap => false,
                None => {
                    insert(&mut self.undeclared, &parent, &child, 1);
                    true
                }
            };
            self.stats.dropped_undeclared += 1;
            return Action::UndeclaredEdge {
                parent,
                child,
                first_time,
            };
        }

        // 3. Kind. A hard error, and one fault gets one diagnostic.
        if topic == Topic::Tf && self.statics.observe_dynamic(&parent, &child).is_err() {
            self.stats.dropped_kind_change += 1;
            return Action::Drop {
                reason: DropReason::KindChange,
            };
        }

        // 4. Static value, before authority and only for `/tf_static` — §5.7
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
                // `Declare` is unreachable through this pipeline, and is
                // folded in here rather than given its own arm: an edge is
                // either undeclared (returned at step 2) or seeded by
                // `StaticStore::seeded` with the config's constant, so
                // `observe_static` always finds a value on file. Folding it
                // into "verified" is the safe direction if seeding is ever
                // skipped — the alternatives would either write a wire value
                // the arena has no slot for, or report a conflict against
                // nothing.
                StaticVerdict::Idempotent | StaticVerdict::Declare => {
                    // Silent, per §5.7, **including from a different
                    // publisher**: two robot_state_publishers with the same
                    // URDF is a redundant launch file, not a misconfiguration,
                    // and an authority diagnostic here would train operators to
                    // ignore the message that matters. So this returns before
                    // authority is consulted at all.
                    self.stats.static_verified += 1;
                    return Action::StaticVerified { parent, child };
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
                    // The diagnostic, carrying **both values** — the half that
                    // tells an operator which URDF is installed.
                    //
                    // **And that is all that happens: the authority policy is
                    // NOT consulted on this path**, because this arm returns and
                    // every arm of this block returns, so step 5 below is
                    // unreachable for a `/tf_static` sample. An earlier revision
                    // of this comment said the policy "decides the disposition",
                    // which was never true of the code beneath it.
                    //
                    // Whether it *should* be consulted is open, not settled:
                    // §5.7 does say "then apply the authority policy", but §5.4
                    // defines `Strict` as refusing "within a startup window" and
                    // there is no startup window in this crate to refuse within.
                    // `docs/decisions/0011` carries the question; until it is
                    // resolved, `Strict` does not halt on a static conflict and
                    // this paragraph is the warning.
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
            }
        }

        // 5. Authority, before the clock — see the module docs.
        match self.authority.admit(&parent, &child, publisher) {
            Verdict::Accept => {}
            Verdict::Reject {
                owner,
                intruder,
                first_time,
            } => {
                self.stats.dropped_authority += 1;
                return Action::AuthorityConflict {
                    parent,
                    child,
                    owner,
                    intruder,
                    first_time,
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

        // 6. Clock, last: only a sample that will be written may advance time.
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

    /// The declared topology as this pipeline keys on it — §5.6's
    /// normalization, `tf_prefix` included, already applied.
    ///
    /// **Build the arena from this, not from the parsed file.** The two differ
    /// exactly when a `tf_prefix` is configured, and a bridge that built from
    /// the file would hold an arena whose frames no approved sample can name.
    #[must_use]
    pub fn declared(&self) -> &TopologyConfig {
        &self.declared
    }

    /// §5.6's remap table: `(name on the wire, name in the arena)`.
    ///
    /// *"A silent remap is worse than no remap"* — §5.6 requires this logged at
    /// startup, and it is complete at startup because
    /// [`TopologyConfig::rewritten`] runs every declared frame through the
    /// normalizer before the first message arrives. Later rows can only be
    /// frames the config never declared.
    #[must_use]
    pub fn remaps(&self) -> &[(String, String)] {
        self.names.remaps()
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

    /// Edges the robot publishes that the topology config does not declare,
    /// with how many transforms each swallowed.
    ///
    /// §5.8 requires the diagnostic once per edge; this is the list behind it,
    /// and it is the first thing to look at when a lookup returns `NoPath` on a
    /// bridge that reports no drops the operator recognises.
    #[must_use]
    pub fn undeclared(&self) -> Vec<(&str, &str, u64)> {
        crate::edgemap::iter(&self.undeclared)
            .map(|(p, c, n)| (p, c, *n))
            .collect()
    }

    /// The remap table, for the startup log (§5.6).
    #[must_use]
    pub fn names(&self) -> &NameNormalizer {
        &self.names
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

    /// The topology every test below runs against: one dynamic edge and one
    /// static one, written in the real config format so these tests exercise
    /// the parser the operator will use rather than a struct literal that could
    /// drift from it.
    const TOPO: &str = r#"
[[edge]]
parent = "odom"
child = "base"
kind = "dynamic"
capacity = 256

[[edge]]
parent = "base"
child = "lidar"
kind = "static"
pose = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
"#;

    fn topo() -> TopologyConfig {
        TopologyConfig::parse(TOPO).unwrap()
    }

    fn ingest() -> Ingest {
        Ingest::new(&topo())
    }

    /// **A `tf_prefix` rewrites the declared topology as well as the wire.**
    ///
    /// §5.6 applies the prefix to incoming frame names, and §5.8's amendment
    /// makes the config the sole source of declared edges. If only the wire side
    /// is rewritten the two never meet: the pipeline looks up
    /// `robot1/odom -> robot1/base` in a store seeded with `odom -> base`,
    /// misses, and reports every transform on a correctly configured robot as an
    /// undeclared edge — with the diagnostic blaming the config rather than the
    /// prefix. Setting `tf_prefix` was therefore a switch that dropped 100 % of
    /// the traffic and said nothing at startup.
    ///
    /// The direction is settled by the documented operator workflow: `tf_tree
    /// topology --discover` writes the names as they appear on the wire, and
    /// adding a prefix for a second robot must not require hand-editing the file
    /// it just produced.
    ///
    /// Mutant: seed `StaticStore` from `config` rather than from
    /// `config.rewritten(&mut names)` in `Ingest::with` ⇒ the offer comes back
    /// `Action::UndeclaredEdge` and the `Action::Publish` assertion fails.
    #[test]
    fn a_tf_prefix_rewrites_the_declared_edges_not_only_the_wire() {
        let mut i = Ingest::with(
            &topo(),
            AuthorityPolicy::FirstWriterWins,
            OnClockReset::Halt,
            Some("robot1"),
        );
        // The declared topology the arena must be built from.
        let e: Vec<(&str, &str)> = i
            .declared()
            .edges
            .iter()
            .map(|e| (e.parent.as_str(), e.child.as_str()))
            .collect();
        assert_eq!(
            e,
            [
                ("robot1/odom", "robot1/base"),
                ("robot1/base", "robot1/lidar")
            ]
        );

        // …and the wire's raw names land on it.
        let a = i.offer(
            Topic::Tf,
            &Sample::identity("odom", "base", 1_000 * MS),
            &node("/ekf"),
        );
        match a {
            Action::Publish { parent, child, .. } => {
                assert_eq!(
                    (parent.as_str(), child.as_str()),
                    ("robot1/odom", "robot1/base")
                );
            }
            other => panic!("a declared edge must publish, got {other:?}"),
        }

        // §5.6's table is complete before the first message, which is what
        // "log the resulting mapping table at startup" needs.
        assert_eq!(
            i.remaps(),
            [
                ("odom".to_string(), "robot1/odom".to_string()),
                ("base".to_string(), "robot1/base".to_string()),
                ("lidar".to_string(), "robot1/lidar".to_string()),
            ]
        );
    }

    /// **No prefix leaves the declared topology exactly as the file wrote it**,
    /// and the remap table empty.
    ///
    /// The other half of the rewrite: it must be a no-op when nothing asked for
    /// it, or every unprefixed bridge silently renames its own frames.
    ///
    /// Mutant: make `NameNormalizer::with_prefix("")` keep `Some("")` instead of
    /// `None` ⇒ every frame becomes `/base`, the edge assertion fails, and the
    /// remap table is three rows rather than none.
    #[test]
    fn no_prefix_leaves_the_declared_topology_alone() {
        let i = Ingest::new(&topo());
        assert_eq!(i.declared(), &topo());
        assert!(i.remaps().is_empty());

        let blank = Ingest::with(
            &topo(),
            AuthorityPolicy::FirstWriterWins,
            OnClockReset::Halt,
            Some("   "),
        );
        assert_eq!(blank.declared(), &topo(), "an unset launch argument");
        assert!(blank.remaps().is_empty());
    }

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
        let mut i = ingest();
        let s = |t: i64| Sample::identity("odom", "base", t);

        assert!(matches!(
            i.offer(Topic::Tf, &s(1_000 * MS), &node("/ekf")),
            Action::Publish { .. }
        ));
        // An intruder, from an hour in the future.
        assert_eq!(
            i.offer(Topic::Tf, &s(3_600_000 * MS), &node("/rogue")),
            Action::AuthorityConflict {
                parent: "odom".to_string(),
                child: "base".to_string(),
                owner: node("/ekf"),
                intruder: node("/rogue"),
                first_time: true,
            }
        );
        // The owner keeps working.
        assert!(matches!(
            i.offer(Topic::Tf, &s(1_010 * MS), &node("/ekf")),
            Action::Publish { .. }
        ));
        assert_eq!(i.stats().dropped_non_monotonic, 0);
    }

    /// **Names are normalized before anything keys on them** — including the
    /// declared-topology lookup, which is now the first table.
    ///
    /// Mutant: normalize after the declared check ⇒ `/odom` -> `/base` does not
    /// match the config's `odom` -> `base`, every slash-prefixed transform on a
    /// correctly configured robot is reported undeclared, and this fails on the
    /// first offer.
    #[test]
    fn a_slash_prefixed_name_is_the_same_edge() {
        let mut i = ingest();
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
        assert_eq!(i.stats().dropped_undeclared, 0);
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
        let mut i = ingest();
        i.offer(
            Topic::Tf,
            &Sample::identity("odom", "base", 1_000_000 * MS),
            &node("/ekf"),
        );
        // A latched static, stamped at the epoch, matching the declared value.
        assert!(matches!(
            i.offer(
                Topic::TfStatic,
                &Sample::identity("base", "lidar", 0),
                &node("/rsp")
            ),
            Action::StaticVerified { .. }
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
    /// Mutant: return early from any arm without touching a counter — e.g. drop
    /// `dropped_undeclared += 1` from the undeclared arm ⇒ this fails, naming
    /// the totals.
    #[test]
    fn every_transform_is_accounted_for() {
        let mut i = ingest();
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
        // A declared static, then the same edge on `/tf` — a kind clash.
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
        // An edge the config never declared.
        i.offer(
            Topic::Tf,
            &Sample::identity("base", "camera", 1_500 * MS),
            &node("/rsp"),
        );

        let s = i.stats();
        assert!(
            s.balanced(),
            "unbalanced: {} transforms vs applied {} + auth {} + mono {} + name {} + kind {} + undeclared {}",
            s.transforms,
            s.applied,
            s.dropped_authority,
            s.dropped_non_monotonic,
            s.dropped_bad_name,
            s.dropped_kind_change,
            s.dropped_undeclared
        );
        assert!(s.dropped_authority > 0 && s.dropped_bad_name == 1 && s.dropped_kind_change == 1);
        assert!(s.dropped_non_monotonic > 0);
        assert_eq!(s.dropped_undeclared, 1);
    }

    /// **§5.7's whole feature, re-aimed by §5.8's amendment: a URDF that
    /// disagrees with the declared constant, reported with both values.**
    ///
    /// The incumbent is now the config file rather than whichever publisher
    /// arrived first, and that is the point of the reinterpretation: the
    /// operator is told *"your file says the lidar is at x = 0, `/rsp_b` says
    /// 0.25"*, which names the installed URDF against the intended one. Before,
    /// the answer depended on launch order.
    ///
    /// Mutant: seed only the kinds and not the values in `StaticStore::seeded`
    /// ⇒ the first publisher declares and the config is no longer a party to
    /// the conflict, so `owner` is `/rsp_a` and this fails.
    #[test]
    fn a_urdf_that_disagrees_with_the_declared_constant_is_reported_with_both_values() {
        let mut i = ingest();
        let mut moved = Sample::identity("base", "lidar", 0);
        // The first publisher agrees with the file: silent verification.
        assert_eq!(
            i.offer(Topic::TfStatic, &moved, &node("/rsp_a")),
            Action::StaticVerified {
                parent: "base".to_string(),
                child: "lidar".to_string()
            }
        );
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
                assert_eq!(owner, Publisher::Declared, "the config is the incumbent");
                assert_eq!(intruder, node("/rsp_b"));
                assert_eq!(existing[4], 0.0, "and both values are reported");
                assert!((offered[4] - 0.25).abs() < 1e-12);
                assert!(first_time);
            }
            other => panic!("§5.7's diagnostic must be reachable: {other:?}"),
        }
        assert_eq!(i.stats().static_conflicts, 1);
        assert!(i.stats().balanced());
    }

    /// **§5.4's headline diagnostic is reachable: both nodes, the edge, and a
    /// rate-limit flag.**
    ///
    /// §5.4 requires *"a diagnostic naming **both** nodes and the edge"* and
    /// that it be *"loud, rate-limited"*. `Verdict::Reject` has carried all
    /// three since it was written; the pipeline used to collapse it into
    /// `Action::Drop { reason: NotTheOwner }`, which carries none of them — so
    /// the sentence §5.4 calls the better sales pitch was unprintable by any
    /// caller of `offer`, and a 1 kHz intruder could only be logged once per
    /// message or not at all.
    ///
    /// Mutant: return `Action::Drop { reason: … }` from the `Reject` arm again
    /// ⇒ this fails to match. Mutant: return `first_time: true` unconditionally
    /// ⇒ the second offer's assertion fails.
    #[test]
    fn an_authority_conflict_names_both_publishers_the_edge_and_is_rate_limited() {
        let mut i = ingest();
        let s = |t: i64| Sample::identity("odom", "base", t);
        assert!(matches!(
            i.offer(Topic::Tf, &s(1_000 * MS), &node("/ekf")),
            Action::Publish { .. }
        ));
        match i.offer(Topic::Tf, &s(1_001 * MS), &node("/odom_node")) {
            Action::AuthorityConflict {
                parent,
                child,
                owner,
                intruder,
                first_time,
            } => {
                assert_eq!((parent.as_str(), child.as_str()), ("odom", "base"));
                assert_eq!(owner, node("/ekf"));
                assert_eq!(intruder, node("/odom_node"));
                assert!(first_time, "the first collision is the loud one");
            }
            other => panic!("§5.4's diagnostic must be reachable: {other:?}"),
        }
        for k in 2..40i64 {
            match i.offer(Topic::Tf, &s(1_000 * MS + k * MS), &node("/odom_node")) {
                Action::AuthorityConflict { first_time, .. } => {
                    assert!(!first_time, "rate-limited after the first");
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(i.stats().dropped_authority, 39);
        assert!(i.stats().balanced(), "{:?}", i.stats());
    }

    /// **An identical latched value from a second publisher is silent** — §5.7
    /// says so, and it is the normal case for a redundant launch file.
    ///
    /// Mutant: consult authority before the static store ⇒ `/rsp_b` is rejected
    /// as `NotTheOwner`, `dropped_authority` becomes 1, and an operator gets a
    /// loud diagnostic about a correct system.
    #[test]
    fn an_identical_static_from_a_second_publisher_is_silent() {
        let mut i = ingest();
        let s = Sample::identity("base", "lidar", 0);
        i.offer(Topic::TfStatic, &s, &node("/rsp_a"));
        assert_eq!(
            i.offer(Topic::TfStatic, &s, &node("/rsp_b")),
            Action::StaticVerified {
                parent: "base".to_string(),
                child: "lidar".to_string()
            }
        );
        assert_eq!(i.stats().static_conflicts, 0);
        assert_eq!(
            i.stats().dropped_authority,
            0,
            "a redundant launch file is not an authority conflict"
        );
    }

    /// **An undeclared edge is dropped, counted, and diagnosed once — naming
    /// both frames.**
    ///
    /// This is §5.8's amendment's fifth requirement and the failure mode it
    /// exists to make visible: with no runtime edge declaration, a transform
    /// for an edge the config forgot has nowhere to go, and the only downstream
    /// symptom is a lookup returning `NoPath` with nothing anywhere saying why.
    ///
    /// Mutant: return `first_time: true` unconditionally ⇒ a 1 kHz undeclared
    /// edge emits a thousand identical lines a second, and this fails on the
    /// second offer.
    #[test]
    fn an_undeclared_edge_is_dropped_and_diagnosed_once() {
        let mut i = ingest();
        let s = |t: i64| Sample::identity("base", "camera", t);
        match i.offer(Topic::Tf, &s(1_000 * MS), &node("/cam")) {
            Action::UndeclaredEdge {
                parent,
                child,
                first_time,
            } => {
                assert_eq!((parent.as_str(), child.as_str()), ("base", "camera"));
                assert!(first_time, "the first sighting is the loud one");
            }
            other => panic!("{other:?}"),
        }
        for k in 1..50i64 {
            match i.offer(Topic::Tf, &s(1_000 * MS + k * MS), &node("/cam")) {
                Action::UndeclaredEdge { first_time, .. } => {
                    assert!(!first_time, "rate-limited after the first");
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(i.stats().dropped_undeclared, 50);
        assert_eq!(i.stats().applied, 0);
        assert!(i.stats().balanced());
        assert_eq!(i.undeclared(), [("base", "camera", 50)]);

        // …and it never reached the authority or clock tables, so it cannot
        // have taken ownership of an edge or moved the high-water mark.
        assert_eq!(i.stats().dropped_authority, 0);
        assert_eq!(i.stats().dropped_non_monotonic, 0);
    }

    /// **An undeclared edge on `/tf_static` is undeclared, not a kind change.**
    ///
    /// The two diagnostics send an operator to different places: one says "add
    /// this edge to your config", the other says "you have a publisher on the
    /// wrong topic". Reporting the second for the first is a wrong answer with
    /// a confident tone.
    ///
    /// Mutant: move the declared check *below* the static-value step ⇒
    /// `observe_static` declares the unknown edge itself, this returns
    /// `StaticVerified`, and the bridge silently accepts a static edge the
    /// arena has no slot for. (Moving it below the *kind* check alone does
    /// nothing: that check is `/tf`-only, so a `/tf_static` sample skips it.)
    #[test]
    fn an_undeclared_static_is_reported_as_undeclared() {
        let mut i = ingest();
        assert!(matches!(
            i.offer(
                Topic::TfStatic,
                &Sample::identity("base", "imu", 0),
                &node("/rsp")
            ),
            Action::UndeclaredEdge { .. }
        ));
        assert_eq!(i.stats().dropped_undeclared, 1);
        assert_eq!(i.stats().dropped_kind_change, 0);
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
        let c = topo();
        let mut i = Ingest::with(&c, AuthorityPolicy::Strict, OnClockReset::Halt, None);
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
        let mut j = ingest();
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
    ///
    /// Mutant: assign rather than `max` in `note_queue_depth` ⇒ the final `0`
    /// wins and a saturated queue reports as idle.
    #[test]
    fn the_queue_high_water_mark_is_a_maximum_not_a_reading() {
        let mut i = ingest();
        i.note_queue_depth(3);
        i.note_queue_depth(100);
        i.note_queue_depth(0);
        assert_eq!(i.stats().queue_high_water, 100);
        assert!(
            i.stats().queue_saturated(),
            "a queue that hit its KeepLast(100) depth must report saturated"
        );
    }

    /// **Every edge in the config is writable through the pipeline and through
    /// the arena the same config builds** — the two halves of §5.8's
    /// resolution, checked against each other.
    ///
    /// Without this, `TopologyConfig::builder` and `Ingest` could disagree
    /// about which edges exist and each would still pass its own tests: the
    /// pipeline would emit `Publish` for an edge `Tree::claim` refuses, which
    /// is the exact failure the amendment describes (`NoEdge` on a slot no API
    /// can fill), only moved one layer up.
    ///
    /// Mutant: seed the store from `config.frames` instead of `config.edges` ⇒
    /// no edge is declared, the first offer is `UndeclaredEdge`, and this fails.
    #[test]
    fn the_pipeline_and_the_arena_agree_about_which_edges_exist() {
        let c = topo();
        let tree = c.builder().build().unwrap();
        let mut i = Ingest::new(&c);
        match i.offer(
            Topic::Tf,
            &Sample::identity("odom", "base", 1_000 * MS),
            &node("/ekf"),
        ) {
            Action::Publish { parent, child, .. } => {
                let p = tree.frame(&parent).unwrap();
                let ch = tree.frame(&child).unwrap();
                let w = tree
                    .claim(ch, p)
                    .unwrap_or_else(|e| panic!("pipeline said Publish, arena said {e:?}"));
                w.push(1_000 * MS, &tf_tree::Iso3::IDENTITY).unwrap();
            }
            other => panic!("{other:?}"),
        }
    }

    /// **A verified static is not an applied transform.** `applied` is
    /// documented as *"transforms written into the arena"*, and a `/tf_static`
    /// message that matches the config's declared constant writes nothing — the
    /// value was placed by `TopologyConfig::builder` before the bridge started.
    ///
    /// This is not a pedantic distinction. `/tf_static` is transient-local, so
    /// every late joiner causes the whole latched set to be re-delivered; an
    /// operator watching `applied` on a robot whose only edges are static used
    /// to see a healthy write rate for an arena nothing was writing to.
    ///
    /// The fixture pushes a real dynamic sample too, so `applied` is non-zero
    /// and the assertion cannot pass by everything being zero.
    ///
    /// Mutant: put `self.stats.applied += 1` back in the `StaticVerified` arm
    /// (dropping `static_verified`) ⇒ `applied` reads 4 and the ledger stays
    /// balanced, so the first assertion is the one that catches it.
    #[test]
    fn a_verified_static_is_counted_as_verified_not_applied() {
        let mut i = ingest();
        let stat = Sample {
            pose: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            ..Sample::identity("base", "lidar", 0)
        };
        // Three deliveries of the latched set, as three late joiners produce.
        for _ in 0..3 {
            assert!(matches!(
                i.offer(Topic::TfStatic, &stat, &node("/rsp")),
                Action::StaticVerified { .. }
            ));
        }
        assert!(matches!(
            i.offer(
                Topic::Tf,
                &Sample::identity("odom", "base", 1_000 * MS),
                &node("/ekf")
            ),
            Action::Publish { .. }
        ));

        let s = i.stats();
        assert_eq!(s.applied, 1, "only the dynamic sample was written");
        assert_eq!(s.static_verified, 3);
        assert_eq!(s.transforms, 4);
        assert!(
            s.balanced(),
            "the ledger must still balance: {s:?}" // `static_verified` is a bucket
        );
    }
}
