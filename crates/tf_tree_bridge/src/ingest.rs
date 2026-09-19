//! The decision pipeline — where §5.4 to §5.7 meet, in `offer`'s order:
//!
//! 0. **The startup window** (§5.4): the only step that can answer before the
//!    transform is counted.
//! 1. **Names** (§5.6): everything downstream keys on the normalized pair.
//! 2. **Declared?** (§5.8): an undeclared edge has no kind to clash with.
//! 3. **Kind** (§5.7): a hard error; one fault, one diagnostic.
//! 4. **Static value** (§5.7), only on `/tf_static` and *before* authority, so a
//!    differing value reaches the conflict payload (both values) instead of
//!    being rejected as `NotTheOwner`, and an identical value from a second
//!    publisher stays silent.
//! 5. **Authority** (§5.4), before the clock, so a rejected intruder cannot move
//!    the owner's high-water mark.
//! 6. **Clock** (§5.5), dynamic only, so only samples that will be written
//!    advance time.
//!
//! Every table answers an exact question about one edge and one message;
//! "the clock was reset" and "this deployment must not start" are judgments about
//! a set of those facts (`docs/decisions/0011`, `0012`). The [`ClockGuard`] is
//! per edge and decides only whether *this* sample is dropped; promotion is
//! `crate::clock`'s ladder. `Strict` accumulates conflicts while the startup
//! window is open and halts once at its close; outside it, `Strict` is
//! `FirstWriterWins` plus counters.
//!
//! The two windows use different units on purpose. The **startup** window counts
//! transforms offered ([`BridgeStats::transforms`]) as a backstop behind
//! [`Ingest::close_startup_window`]; the **correlation** window in `crate::clock`
//! is receipt-time nanoseconds ([`crate::SteadyNanos`]), because it asks whether
//! two publishers moved at the same time.

use std::collections::BTreeMap;

use crate::authority::{Authority, AuthorityPolicy, Verdict};
use crate::clock::{
    ClockEvidence, ClockGuard, ClockPolicy, ClockVerdict, JumpKind, OffsetTable, OnClockReset,
};
use crate::config::TopologyConfig;
use crate::edgeindex::{EdgeIndex, EdgeSlot};
use crate::edgemap::{insert, lookup_mut, ByEdge};
use crate::names::NameNormalizer;
use crate::statics::{StaticKind, StaticStore, StaticVerdict};
use crate::stats::BridgeStats;
use crate::{Publisher, Sample};

/// Distinct undeclared *parent* frames remembered by [`Ingest::undeclared`]; the
/// cap keeps a misconfigured topology from also exhausting the bridge.
const MAX_UNDECLARED_PARENTS: usize = 256;
/// Distinct undeclared children remembered per parent.
const MAX_UNDECLARED_CHILDREN: usize = 256;

/// How long §5.4's startup window stays open without an explicit close:
/// **4096 transforms**, a backstop for a caller that never closes it. A caller
/// with a real clock calls [`Ingest::close_startup_window`] from a one-shot
/// **steady** timer, not `node_->get_clock()`, which is `/clock` under
/// `use_sim_time`. A poor proxy for a duration (`docs/decisions/0011`).
const STARTUP_WINDOW_TRANSFORMS: u64 = 4096;

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
    /// write; the arena already holds it (§5.8's amendment).
    StaticVerified {
        /// Normalized parent frame.
        parent: String,
        /// Normalized child frame.
        child: String,
    },
    /// A transform for an edge the topology config does not declare (§5.8).
    ///
    /// Not a `Drop`: the diagnostic names both frames, and `first_time` keeps it
    /// to one line per edge.
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
    /// Not a `Drop`: §5.4 requires a loud, rate-limited diagnostic naming both
    /// nodes and the edge. The sample is dropped either way and
    /// `stats.dropped_authority` counts it.
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
    /// A `/tf_static` value that disagrees with the one on file (§5.7). Not a
    /// `Drop`: the diagnostic names both publishers and both values. The sample
    /// is not written either way.
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
    /// The clock moved under [`OnClockReset::Recreate`]: build a fresh arena,
    /// then re-offer this sample.
    RecreateArena {
        /// New time minus old time — **negative for a rewind**; signed because a
        /// jump report or common-mode step can also see a forward jump. Same
        /// sign as `rcl_time_jump_t::delta`.
        delta_nanos: i64,
        /// Which rung of §5.5's ladder fired and how strong it was: a reported
        /// jump means the time source did it, an inferred one means the bridge
        /// decided.
        evidence: ClockEvidence,
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
    ///
    /// **No longer produced by `offer`**: `docs/decisions/0011` moved `Strict`'s
    /// halt to [`HaltReason::StartupConflicts`]. Kept because §5.4's contract is
    /// that a `Strict` conflict stops the bridge.
    AuthorityConflict {
        /// The prior owner.
        owner: Publisher,
        /// The publisher that collided with it.
        intruder: Publisher,
    },
    /// `Halt` policy, and the clock was judged to have moved.
    ClockReset {
        /// New time minus old time — **negative for a rewind**; see
        /// [`Action::RecreateArena`].
        delta_nanos: i64,
        /// Which rung of the ladder fired and how strong it was; the C seam's
        /// outcome has room for one `(parent, child)` pair only, so this is what
        /// is left of the evidence.
        evidence: ClockEvidence,
    },
    /// `Strict` policy, and the startup window closed with conflicts recorded
    /// (§5.4, `docs/decisions/0011`).
    ///
    /// One halt for the whole startup: conflicts accumulate while the window is
    /// open and this is raised once at its close. The enumeration lives in
    /// [`Authority::conflicts`].
    StartupConflicts {
        /// Distinct `(edge, owner, intruder)` authority conflicts recorded.
        authority: u32,
        /// Distinct static edges whose value was contradicted (§5.7).
        statics: u32,
    },
}

/// A publisher's identity as one borrowed string, keying [`OffsetTable`]'s
/// per-publisher baselines. Borrowed because it runs on every dynamic sample.
///
/// An RMW that reports no GID yields [`Publisher::Unattributed`] for everything:
/// one identity, one baseline, so common mode never reaches two publishers and
/// every regression degrades to a drop. Unresolved names with GIDs still
/// distinguish publishers. Attribution quality changes how well a clock event is
/// described, never whether the bridge can stop (§5.3); the authoritative rung
/// ([`Ingest::note_time_jump`]) needs none.
pub(crate) fn owner_key(p: &Publisher) -> &str {
    p.key()
}

/// What §5.6 and §5.8 together decided about a sample's frames.
enum Resolved {
    /// The pair names this declared edge.
    Declared(EdgeSlot),
    /// It normalized, and the config does not declare it.
    Undeclared { parent: String, child: String },
    /// It did not normalize (§5.6's `NameError`).
    BadName,
}

/// The four tables, plus the declared topology and the counters, applied in
/// order.
#[derive(Debug)]
pub struct Ingest {
    names: NameNormalizer,
    /// Raw wire `(parent, child)` → the declared edge it names, so a spelling
    /// seen once skips §5.6's normalization.
    ///
    /// Populated lazily from the slow path: an entry exists only after that pair
    /// went through [`NameNormalizer::normalize`], so `seen` and `remaps` already
    /// hold what a skip would lose; the per-occurrence stripped-slash count is
    /// replayed by [`Ingest::resolve`] via `NameNormalizer::note_stripped`.
    /// Pre-seeding would drop the `("/odom", "odom")` remap row that
    /// `tft_bridge_get_remap` reports. Bounded without a cap: at most four raw
    /// pairs per declared edge, and undeclared pairs are never inserted.
    raw: EdgeIndex<EdgeSlot>,
    /// The declared topology **after** §5.6's normalization; the arena is built
    /// from exactly these names (`tft_bridge_create`).
    declared: TopologyConfig,
    statics: StaticStore,
    authority: Authority,
    /// One clock guard per edge, indexed by slot (§5.5, `docs/decisions/0011`):
    /// it measures one publisher's regression against its own last accepted
    /// stamp. `tests/steady_state_alloc.rs` gates the steady-state probe.
    clocks: Vec<ClockGuard>,
    /// Every knob §5.5's detection has, so the per-edge drop decision and the
    /// promotion decision cannot be configured out of step.
    clock: ClockPolicy,
    /// Per-publisher stamp-to-receipt offsets and the common-mode rule: the
    /// *fallback* rung, kept above the guards (`docs/decisions/0011`).
    offsets: OffsetTable,
    /// Whether §5.4's startup window is open: from construction until
    /// [`Ingest::close_startup_window`] or the [`STARTUP_WINDOW_TRANSFORMS`]
    /// backstop. Under `Strict` the close is the only thing that halts.
    startup_window_open: bool,
    stats: BridgeStats,
    /// Undeclared edges seen and how many times: the rate limiter behind
    /// `Action::UndeclaredEdge`'s `first_time`. A [`ByEdge`] so the per-message
    /// bump allocates nothing.
    undeclared: ByEdge<u64>,
}

impl Ingest {
    /// A pipeline over `config` with the default policies: `FirstWriterWins`,
    /// `Halt`, no `tf_prefix`.
    ///
    /// The topology is not optional (§5.8's amendment): the engine cannot declare
    /// an edge after `build()`, so everything the bridge writes must be in the
    /// config.
    #[must_use]
    pub fn new(config: &TopologyConfig) -> Ingest {
        Ingest::with(
            config,
            AuthorityPolicy::default(),
            OnClockReset::default(),
            None,
        )
    }

    /// A pipeline with an explicit authority policy and clock *action*, and
    /// otherwise the default [`ClockPolicy`].
    ///
    /// The convenience shape for `--on-clock-reset`; see [`Ingest::with_policies`]
    /// for the other clock knobs.
    #[must_use]
    pub fn with(
        config: &TopologyConfig,
        authority: AuthorityPolicy,
        on_clock_reset: OnClockReset,
        tf_prefix: Option<&str>,
    ) -> Ingest {
        Ingest::with_policies(
            config,
            authority,
            ClockPolicy {
                on_reset: on_clock_reset,
                ..ClockPolicy::default()
            },
            tf_prefix,
        )
    }

    /// A pipeline with every policy stated.
    #[must_use]
    pub fn with_policies(
        config: &TopologyConfig,
        authority: AuthorityPolicy,
        clock: ClockPolicy,
        tf_prefix: Option<&str>,
    ) -> Ingest {
        // The declared topology is rewritten through the wire's own normalizer
        // (`TopologyConfig::rewritten`), or a prefixed bridge reports every edge
        // as undeclared.
        let mut names = tf_prefix.map_or_else(NameNormalizer::new, NameNormalizer::with_prefix);
        let declared = config.rewritten(&mut names);
        let statics = StaticStore::seeded(&declared);
        // One guard per declared edge, built here so nothing allocates after a recreate.
        let clocks = (0..statics.slots())
            .map(|_| ClockGuard::with_threshold(clock.on_reset, clock.reset_threshold_nanos))
            .collect();
        // Same `declared`, same order as `StaticStore`, so slots agree
        // (`the_authority_and_the_statics_agree_about_slots`).
        let authority = Authority::seeded(authority, &declared);
        Ingest {
            raw: EdgeIndex::with_capacity(4 * statics.slots()),
            statics,
            names,
            declared,
            authority,
            clocks,
            clock,
            offsets: OffsetTable::new(clock),
            startup_window_open: true,
            stats: BridgeStats {
                queue_capacity: 100, // §5.2's KeepLast(100)
                ..BridgeStats::default()
            },
            undeclared: BTreeMap::new(),
        }
    }
    /// A slot's canonical `(parent, child)`, owned; only the arms that carry
    /// names pay for it.
    fn edge_names(&self, slot: EdgeSlot) -> (String, String) {
        let (p, c) = self.statics.names_of(slot);
        (p.to_string(), c.to_string())
    }

    /// §5.6 and §5.8 in one step: normalize, then find the declared edge. A
    /// repeated spelling does not re-normalize (see [`Ingest::raw`]).
    fn resolve(&mut self, sample: &Sample) -> Resolved {
        let (rp, rc) = (sample.frame_id.as_str(), sample.child_frame_id.as_str());
        if let Some(slot) = self.raw.get(rp, rc) {
            self.names
                .note_stripped(u64::from(rp.starts_with('/')) + u64::from(rc.starts_with('/')));
            return Resolved::Declared(slot);
        }
        let (Ok(parent), Ok(child)) = (self.names.normalize(rp), self.names.normalize(rc)) else {
            return Resolved::BadName;
        };
        let (parent, child) = (parent.name, child.name);
        match self.statics.resolve(&parent, &child) {
            Some(slot) => {
                self.raw.insert(rp, rc, slot);
                Resolved::Declared(slot)
            }
            None => Resolved::Undeclared { parent, child },
        }
    }

    /// Push one transform through every table.
    pub fn offer(&mut self, topic: Topic, sample: &Sample, publisher: &Publisher) -> Action {
        // 0. The startup window's backstop (§5.4), before the transform is
        //    counted (see `close_startup_window`); the arriving sample is not
        //    processed, since the caller latches on this outcome.
        if self.startup_window_open && self.stats.transforms >= STARTUP_WINDOW_TRANSFORMS {
            if let Some(halt) = self.close_startup_window() {
                return halt;
            }
        }

        self.stats.transforms += 1;

        // 1+2. Names and the declared edge in one step (`Ingest::resolve`).
        //      Declared? precedes the kind check: `KindChange` for an
        //      undeclared edge would misdirect the operator.
        let slot = match self.resolve(sample) {
            Resolved::Declared(slot) => slot,
            Resolved::BadName => {
                self.stats.dropped_bad_name += 1;
                return Action::Drop {
                    reason: DropReason::BadName,
                };
            }
            Resolved::Undeclared { parent, child } => {
                // Bounded because the key comes from outside the declared topology
                // and `undeclared()` collects the whole table. Past the cap the
                // transform is still dropped and counted; only the per-edge
                // breakdown stops and `first_time` reads `false`. The cap is read
                // before `lookup_mut`, which holds the mutable borrow.
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
        };
        // Names are cloned from the declared topology (canonical spelling) only in
        // the arms whose `Action` carries them; the drop arms run at full rate
        // for a stuck publisher and must not allocate.

        // 3. Kind: one array read.
        if topic == Topic::Tf && self.statics.kind_at(slot) == StaticKind::Static {
            self.stats.dropped_kind_change += 1;
            return Action::Drop {
                reason: DropReason::KindChange,
            };
        }

        // 4. Static value, before authority and only for `/tf_static` (§5.7).
        //    A static's stamp is meaningless (often zero) and would drag the
        //    clock guard's mark to the epoch, so statics never reach step 6.
        if topic == Topic::TfStatic {
            match self.statics.observe_static_at(slot, sample.pose, publisher) {
                // `Declare` is unreachable here (the store is seeded from the
                // config); folding it into "verified" is the safe direction.
                StaticVerdict::Idempotent | StaticVerdict::Declare => {
                    // Silent per §5.7, including from a different publisher, so
                    // this returns before authority is consulted.
                    self.stats.static_verified += 1;
                    let (parent, child) = self.edge_names(slot);
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
                    // The authority policy is not consulted: `Strict` is applied
                    // by the startup window at its close (`docs/decisions/0011`),
                    // and `/tf_static` is `transient_local`, so *when* a latched
                    // conflict is seen carries no information. Routing it into
                    // `Authority::admit` would also hand an unwritten edge to the
                    // intruder.
                    self.stats.static_conflicts += 1;
                    self.stats.dropped_authority += 1;
                    // No startup bookkeeping: `close_startup_window` reads
                    // `StaticStore::conflicts_by_edge()`.
                    let (parent, child) = self.edge_names(slot);
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

        // 5. Authority, before the clock. Neither arm halts: the startup window
        //    decides at its close, and `Authority::admit` records the conflict
        //    under either policy.
        // Destructured so the borrows of `statics` (names) and `authority`
        // (mutable) are disjoint, with no clones on the accept path.
        let Ingest {
            statics, authority, ..
        } = self;
        let (sp, sc) = statics.names_of(slot);
        match authority.admit_at(slot, publisher) {
            Verdict::Accept => {}
            // `Fatal` outside the window means `Strict` has degraded to
            // `FirstWriterWins` plus counters; inside it, this is the accumulation.
            Verdict::Reject {
                owner,
                intruder,
                first_time,
            }
            | Verdict::Fatal {
                owner,
                intruder,
                first_time,
            } => {
                // Count it, or `balanced()` stays false forever.
                let (parent, child) = (sp.to_string(), sc.to_string());
                self.stats.dropped_authority += 1;
                return Action::AuthorityConflict {
                    parent,
                    child,
                    owner,
                    intruder,
                    first_time,
                };
            }
        }

        // 6. Clock, last. First the fallback rung folds this sample's
        //    stamp-to-receipt offset into its publisher's baseline (a forward
        //    jump leaves every edge monotone, so it is visible only there); then
        //    the per-edge guard rules on the sample.
        if let Some(common) =
            self.offsets
                .observe(owner_key(publisher), sample.stamp_nanos, sample.received)
        {
            // Charged to `dropped_non_monotonic`, forward jumps included: the
            // ledger has one "time misbehaved" bucket and a second would grow
            // `tft_bridge_stats`.
            self.stats.dropped_non_monotonic += 1;
            return self.apply_clock_reset(
                common.delta_nanos,
                ClockEvidence::CommonMode {
                    publishers: common.publishers,
                },
            );
        }

        // A sample that promoted above never reaches the guard, so it does not
        // move the edge's mark (under `Halt`, later samples stay refused).
        let verdict = self.clocks[slot.get()].observe(sample.stamp_nanos);
        match verdict {
            ClockVerdict::Forward => {
                self.stats.applied += 1;
                let (parent, child) = self.edge_names(slot);
                Action::Publish {
                    parent,
                    child,
                    stamp_nanos: sample.stamp_nanos,
                    pose: sample.pose,
                }
            }
            // Jitter and a regression past the threshold are one decision: drop,
            // count, diagnose. A single source regressing never halts (a
            // restart or replay looks identical), and the ring would refuse the
            // stamp anyway. The guard's mark is not moved. `clock_resets` is
            // untouched: it counts promotions.
            ClockVerdict::Jitter { by_nanos } | ClockVerdict::Reset { by_nanos, .. } => {
                self.stats.dropped_non_monotonic += 1;
                Action::Drop {
                    reason: DropReason::NonMonotonic { by_nanos },
                }
            }
        }
    }

    /// The time source itself reported a jump — §5.5's authoritative path.
    ///
    /// No inference, no threshold, no window: ROS 2 publishes clock jumps
    /// (`rcl_clock_add_jump_callback`), so a `/clock` regression is the event
    /// itself. `delta_nanos` follows [`Action::RecreateArena`]'s sign convention.
    ///
    /// Under `Recreate` every guard and offset baseline is forgotten; under
    /// `Halt` they are kept (see `apply_clock_reset`).
    ///
    /// # Threading, for the rclcpp caller
    ///
    /// The jump callback does not run on the ingest thread (`use_clock_thread`
    /// defaults to `true`) and the C seam is thread-affine, so the callback must
    /// record the jump for the ingest thread to drain and call this from there.
    /// This function is deliberately not thread-safe.
    ///
    /// # No counter bucket
    ///
    /// No transform is in hand, so [`BridgeStats::balanced`] takes no term;
    /// `clock_resets` is incremented.
    pub fn note_time_jump(&mut self, delta_nanos: i64, kind: JumpKind) -> Action {
        self.apply_clock_reset(delta_nanos, ClockEvidence::Reported { kind })
    }

    /// Apply [`ClockPolicy::on_reset`] to a clock event from either rung; the
    /// caller charges the ledger.
    ///
    /// Only `Recreate` forgets the old recording. This type has no latch, so a
    /// caller that keeps offering after a `Halt` must keep being refused: with
    /// the guards forgotten the next post-rewind sample would read as forward
    /// and come back `Publish`.
    fn apply_clock_reset(&mut self, delta_nanos: i64, evidence: ClockEvidence) -> Action {
        self.stats.clock_resets += 1;
        match self.clock.on_reset {
            OnClockReset::Halt => Action::Halt {
                reason: HaltReason::ClockReset {
                    delta_nanos,
                    evidence,
                },
            },
            OnClockReset::Recreate => {
                self.forget_the_old_recording();
                Action::RecreateArena {
                    delta_nanos,
                    evidence,
                }
            }
        }
    }

    /// Rewind **every** edge's guard and every offset baseline: the arena is
    /// rebuilt whole, and stale baselines would make every publisher's first
    /// post-reset sample agree on a second, self-inflicted reset.
    /// [`ClockGuard::forget`] keeps the per-edge keys, so nothing reallocates.
    fn forget_the_old_recording(&mut self) {
        for guard in &mut self.clocks {
            guard.forget();
        }
        self.offsets.clear();
    }

    /// Close §5.4's startup window and report what it found.
    ///
    /// Returns `Some(Action::Halt { StartupConflicts })` under
    /// [`AuthorityPolicy::Strict`] if any authority or static conflict was
    /// recorded while it was open, else `None`. Idempotent; the
    /// `STARTUP_WINDOW_TRANSFORMS` backstop is the fallback. No counter moves:
    /// the causing transforms were already counted.
    ///
    /// The counts summarise; the per-edge report is [`Authority::conflicts`] and
    /// [`StaticStore::conflicts_by_edge`], same shape. §5.4 requires the seam's
    /// `detail` to enumerate every edge with both publishers, and
    /// `crates/tf_tree_c/src/bridge.rs` does so from these two accessors.
    pub fn close_startup_window(&mut self) -> Option<Action> {
        if !self.startup_window_open {
            return None;
        }
        self.startup_window_open = false;

        // Only `Strict` refuses to start.
        if self.authority.policy() != AuthorityPolicy::Strict {
            return None;
        }
        let authority = u32::try_from(self.authority.conflicts().count()).unwrap_or(u32::MAX);
        // Distinct edges, not `StaticStore::conflicts()` observations
        // (`the_startup_halt_counts_faults_not_observations`).
        let statics = u32::try_from(self.statics.conflicts_by_edge().count()).unwrap_or(u32::MAX);
        if authority == 0 && statics == 0 {
            return None;
        }
        Some(Action::Halt {
            reason: HaltReason::StartupConflicts { authority, statics },
        })
    }

    /// The declared topology as this pipeline keys on it (§5.6 and `tf_prefix`
    /// applied). Build the arena from this, not from the parsed file.
    #[must_use]
    pub fn declared(&self) -> &TopologyConfig {
        &self.declared
    }

    /// §5.6's remap table: `(name on the wire, name in the arena)`. Complete at
    /// startup for declared frames ([`TopologyConfig::rewritten`]).
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
    /// alongside §5.4's.
    #[must_use]
    pub fn statics(&self) -> &StaticStore {
        &self.statics
    }

    /// The per-publisher offset baselines behind the fallback rung, for `doctor`
    /// and tests. `OffsetTable::tracked()` reading 1 with several `/tf`
    /// publishers means the RMW cannot attribute and the inference rung is dormant.
    #[must_use]
    pub fn offsets(&self) -> &OffsetTable {
        &self.offsets
    }

    /// The clock policy in force (§5.5).
    #[must_use]
    pub fn clock_policy(&self) -> ClockPolicy {
        self.clock
    }

    /// Edges published that the config does not declare, with how many
    /// transforms each swallowed (§5.8). Look here when a lookup returns `NoPath`.
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
    use crate::clock::{CommonMode, SteadyNanos, DEFAULT_RESET_THRESHOLD_NANOS};

    fn node(n: &str) -> Publisher {
        Publisher::named(&crate::gid_for_name(n), n)
    }
    const MS: i64 = 1_000_000;
    const S: i64 = 1_000_000_000;

    /// A sample carrying a receipt time, which the common-mode rung needs;
    /// `Sample::identity` leaves the inference rung dormant.
    fn at(parent: &str, child: &str, stamp_nanos: i64, received: i64) -> Sample {
        Sample::identity(parent, child, stamp_nanos).received_at(SteadyNanos(received))
    }

    /// The fixture topology in the real config format: two dynamic and two
    /// static edges, the minimum for §5.5's multi-publisher and
    /// statics-never-touch-the-clock cases.
    const TOPO: &str = r#"
[[edge]]
parent = "map"
child = "odom"
kind = "dynamic"
capacity = 256

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

[[edge]]
parent = "base"
child = "gps"
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
    /// Mutant: seed `StaticStore` from `config` rather than from
    /// `config.rewritten(&mut names)` in `Ingest::with`.
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
                ("robot1/map", "robot1/odom"),
                ("robot1/odom", "robot1/base"),
                ("robot1/base", "robot1/lidar"),
                ("robot1/base", "robot1/gps"),
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
                ("map".to_string(), "robot1/map".to_string()),
                ("odom".to_string(), "robot1/odom".to_string()),
                ("base".to_string(), "robot1/base".to_string()),
                ("lidar".to_string(), "robot1/lidar".to_string()),
                ("gps".to_string(), "robot1/gps".to_string()),
            ]
        );
    }

    /// **No prefix leaves the declared topology exactly as the file wrote it**,
    /// and the remap table empty.
    ///
    /// Mutant: make `NameNormalizer::with_prefix("")` keep `Some("")` instead of
    /// `None`.
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

    /// **Authority is decided before the clock**, and this is what goes wrong if
    /// it is not.
    ///
    /// Mutant: move the clock check above the authority check.
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
    /// Mutant: normalize after the declared check.
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

    /// **A static's stamp must not touch the clock — on either rung of the
    /// ladder.**
    ///
    /// Mutant: hoist step 6's `self.offsets.observe(..)` (and the common-mode
    /// arm with it) above the `/tf_static` block.
    #[test]
    fn a_zero_stamped_static_does_not_reset_the_clock() {
        let mut i = ingest();
        // A receipt clock that really is running, so a static's meaningless
        // stamp has something real to be measured against — which is exactly
        // what makes the mutant reachable.
        let t0 = 5_000 * S;
        i.offer(
            Topic::Tf,
            &at("odom", "base", 1_000_000 * MS, t0),
            &node("/ekf"),
        );
        // Two independent latching publishers, one edge each — the ordinary
        // shape: `robot_state_publisher` for the URDF's lidar mount, a
        // `static_transform_publisher` for the GPS bracket.
        for (child, publisher) in [("lidar", "/rsp_a"), ("gps", "/gps_mount_a")] {
            assert!(matches!(
                i.offer(
                    Topic::TfStatic,
                    &at("base", child, 1_000_000 * MS, t0 + 10 * MS),
                    &node(publisher)
                ),
                Action::StaticVerified { .. }
            ));
        }
        // A late joiner is served the same values by a publisher that stamps
        // them at the epoch.
        let redelivered: Vec<Action> = [("lidar", "/rsp_b"), ("gps", "/gps_mount_b")]
            .into_iter()
            .enumerate()
            .map(|(k, (child, publisher))| {
                i.offer(
                    Topic::TfStatic,
                    &at(
                        "base",
                        child,
                        0,
                        t0 + 20 * MS + i64::try_from(k).unwrap() * MS,
                    ),
                    &node(publisher),
                )
            })
            .collect();
        assert!(
            redelivered
                .iter()
                .all(|a| matches!(a, Action::StaticVerified { .. })),
            "a static's stamp is meaningless and must not reach any clock rule: {redelivered:?}"
        );
        // The dynamic stream is unaffected.
        assert!(matches!(
            i.offer(
                Topic::Tf,
                &at("odom", "base", 1_000_001 * MS, t0 + 30 * MS),
                &node("/ekf")
            ),
            Action::Publish { .. }
        ));
        assert_eq!(i.stats().clock_resets, 0);
        assert_eq!(i.stats().dropped_non_monotonic, 0);
        assert_eq!(
            i.offsets().tracked(),
            1,
            "only the dynamic publisher has an offset baseline at all"
        );
    }

    /// **The ledger balances over a realistic mixed stream**, which is what
    /// makes `BridgeStats::balanced` worth having: every path either applies or
    /// drops for exactly one reason.
    ///
    /// Mutant: return early from any arm without touching a counter — e.g.
    #[test]
    fn every_transform_is_accounted_for() {
        let mut i = ingest();
        // **`/ekf` publishes first, deliberately.** An earlier version of this
        // fixture used `k % 5 == 0`, which is true at `k == 0` — so `/rogue`
        // took the edge, every `/ekf` sample was dropped on authority, and the
        // clock never advanced.
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
    /// Mutant: seed only the kinds and not the values in `StaticStore::seeded`.
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
    /// Mutant: return `Action::Drop { reason: … }` from the `Reject` arm again.
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
    /// Mutant: consult authority before the static store.
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
    /// Mutant: return `first_time: true` unconditionally.
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
    /// Mutant: move the declared check *below* the static-value step.
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

    // §5.5, the clock ladder.

    /// The stamp a healthy publisher would emit at receipt-clock origin.
    const STAMP0: i64 = 10_000 * MS;

    /// **Two publishers a `transform_tolerance` apart never halt the bridge.**
    ///
    /// Mutant: one shared guard for the whole stream (key both the `lookup_mut`
    /// and the `insert` in step 6 on one entry, `"*", "*"`).
    ///
    /// Mutant: threshold the raw offset instead of the residual in
    /// `OffsetTable::observe` (`if offset.saturating_abs() <= ...`).
    #[test]
    fn two_publishers_a_transform_tolerance_apart_never_halt() {
        let mut i = ingest();
        /// AMCL's `transform_tolerance`, three times the reset threshold.
        const TOLERANCE: i64 = 300 * MS;
        let t0 = 5_000 * S;
        for k in 0..100i64 {
            let r = t0 + k * 10 * MS; // the wheel driver, 100 Hz
            if k % 10 == 0 {
                // The localizer, 10 Hz, dating its edge into the future.
                let v = i.offer(
                    Topic::Tf,
                    &at("map", "odom", STAMP0 + (r - t0) + TOLERANCE, r),
                    &node("/amcl"),
                );
                assert!(
                    matches!(v, Action::Publish { .. }),
                    "the localizer's own stamps are monotone, at k={k}: {v:?}"
                );
            }
            let v = i.offer(
                Topic::Tf,
                &at("odom", "base", STAMP0 + (r - t0), r),
                &node("/ekf"),
            );
            assert!(
                matches!(v, Action::Publish { .. }),
                "and so are the wheel driver's, at k={k}: {v:?}"
            );
        }
        let s = i.stats();
        assert_eq!(s.applied, 110, "every sample was written");
        assert_eq!(s.dropped_non_monotonic, 0);
        assert_eq!(s.clock_resets, 0, "nothing here is a clock reset");
        assert!(s.balanced(), "{s:?}");
        assert_eq!(
            i.offsets().steps(),
            0,
            "a configuration is not an event: the tolerance is measured, not thresholded"
        );
    }

    /// **A lone edge regressing past the threshold is a drop, not a halt.**
    ///
    /// Mutant: promote on the guard's own `Reset` verdict (give
    /// `ClockVerdict::Reset` its own arm returning
    /// `self.apply_clock_reset(..)`).
    #[test]
    fn a_lone_edge_regressing_past_the_threshold_is_dropped_not_halted() {
        let mut i = ingest();
        let t0 = 5_000 * S;
        i.offer(Topic::Tf, &at("map", "odom", STAMP0, t0), &node("/amcl"));
        i.offer(Topic::Tf, &at("odom", "base", STAMP0, t0), &node("/ekf"));

        // The wheel driver restarts and replays its buffer from five seconds
        // ago, once per message, for fifty messages.
        for k in 0..50i64 {
            let r = t0 + 100 * MS + k * MS;
            assert_eq!(
                i.offer(
                    Topic::Tf,
                    &at("odom", "base", STAMP0 + (r - t0) - 5 * S, r),
                    &node("/ekf")
                ),
                Action::Drop {
                    reason: DropReason::NonMonotonic {
                        by_nanos: 5 * S - 100 * MS - k * MS
                    }
                },
                "one publisher is one publisher, at k={k}"
            );
        }
        // …and the localizer is untouched by any of it.
        let r = t0 + 200 * MS;
        assert!(matches!(
            i.offer(
                Topic::Tf,
                &at("map", "odom", STAMP0 + (r - t0), r),
                &node("/amcl")
            ),
            Action::Publish { .. }
        ));

        let s = i.stats();
        assert_eq!(s.clock_resets, 0, "no promotion, so no reset");
        assert_eq!(s.dropped_non_monotonic, 50);
        assert!(s.balanced(), "{s:?}");
        assert_eq!(
            i.offsets().steps(),
            1,
            "one bout of being broken is one step, not fifty"
        );
    }

    /// **A real `/clock` rewind moves both publishers by the same amount, and
    /// that is what halts the bridge.**
    ///
    /// Mutant: drop `self.stats.dropped_non_monotonic += 1;` from the
    /// common-mode arm.
    ///
    /// Mutant: `if publishers < 3` in `OffsetTable::observe`.
    #[test]
    fn a_clock_rewind_moving_both_publishers_by_the_same_delta_halts() {
        let mut i = ingest();
        let t0 = 5_000 * S;
        for k in 0..5i64 {
            let r = t0 + k * 10 * MS;
            for (parent, child, who) in [("map", "odom", "/amcl"), ("odom", "base", "/ekf")] {
                assert!(matches!(
                    i.offer(
                        Topic::Tf,
                        &at(parent, child, STAMP0 + (r - t0), r),
                        &node(who)
                    ),
                    Action::Publish { .. }
                ));
            }
        }

        // The bag loops back five seconds. The first publisher to notice is
        // still only one publisher.
        let r1 = t0 + 50 * MS;
        assert_eq!(
            i.offer(
                Topic::Tf,
                &at("map", "odom", STAMP0 + (r1 - t0) - 5 * S, r1),
                &node("/amcl")
            ),
            Action::Drop {
                reason: DropReason::NonMonotonic {
                    by_nanos: 5 * S - 10 * MS
                }
            }
        );
        assert_eq!(i.stats().clock_resets, 0);

        // The second one agrees, to within the 10 ms between their messages.
        let r2 = t0 + 60 * MS;
        assert_eq!(
            i.offer(
                Topic::Tf,
                &at("odom", "base", STAMP0 + (r2 - t0) - 5 * S, r2),
                &node("/ekf")
            ),
            Action::Halt {
                reason: HaltReason::ClockReset {
                    delta_nanos: -5 * S,
                    evidence: ClockEvidence::CommonMode { publishers: 2 },
                }
            }
        );

        let s = i.stats();
        assert_eq!(s.clock_resets, 1, "one promotion, not one per regression");
        assert_eq!(s.dropped_non_monotonic, 2, "both refusals are counted");
        assert!(s.balanced(), "{s:?}");
    }

    /// **Two publishers restarting by unrelated amounts inside the window are
    /// two faults — agreement is what decides, not coincidence in time.**
    ///
    /// Mutant: drop the agreement test from `OffsetTable::observe` and count
    /// every stepped row inside the window.
    #[test]
    fn two_publishers_restarting_by_unrelated_amounts_do_not_halt() {
        let mut i = ingest();
        let t0 = 5_000 * S;
        for k in 0..5i64 {
            let r = t0 + k * 10 * MS;
            for (parent, child, who) in [("map", "odom", "/amcl"), ("odom", "base", "/ekf")] {
                i.offer(
                    Topic::Tf,
                    &at(parent, child, STAMP0 + (r - t0), r),
                    &node(who),
                );
            }
        }

        let r1 = t0 + 50 * MS;
        assert!(matches!(
            i.offer(
                Topic::Tf,
                &at("map", "odom", STAMP0 + (r1 - t0) - 5 * S, r1),
                &node("/amcl")
            ),
            Action::Drop { .. }
        ));
        let r2 = t0 + 60 * MS;
        assert_eq!(
            i.offer(
                Topic::Tf,
                &at("odom", "base", STAMP0 + (r2 - t0) - 400 * MS, r2),
                &node("/ekf")
            ),
            Action::Drop {
                reason: DropReason::NonMonotonic { by_nanos: 380 * MS }
            },
            "two restarts inside a second are still two restarts"
        );

        let s = i.stats();
        assert_eq!(s.clock_resets, 0, "no clock moved");
        assert_eq!(s.dropped_non_monotonic, 2);
        assert!(s.balanced(), "{s:?}");
        assert_eq!(
            (i.offsets().steps(), i.offsets().common_modes()),
            (2, 0),
            "both stepped; neither corroborated the other"
        );
    }

    /// **Two regressions a correlation window apart are two faults, not one
    /// clock — and the window is a second and a half of *physical* time.**
    ///
    /// Mutant: `let age = 0;` in `OffsetTable::observe`, so every recorded step
    /// looks adjacent to every other.
    #[test]
    fn two_regressions_a_correlation_window_apart_are_two_faults() {
        let mut i = ingest();
        let t0 = 5_000 * S;
        for k in 0..5i64 {
            let r = t0 + k * 10 * MS;
            for (parent, child, who) in [("map", "odom", "/amcl"), ("odom", "base", "/ekf")] {
                i.offer(
                    Topic::Tf,
                    &at(parent, child, STAMP0 + (r - t0), r),
                    &node(who),
                );
            }
        }

        // The localizer restarts and replays from five seconds ago: one
        // publisher, one edge, a drop.
        let r1 = t0 + 50 * MS;
        assert!(matches!(
            i.offer(
                Topic::Tf,
                &at("map", "odom", STAMP0 + (r1 - t0) - 5 * S, r1),
                &node("/amcl")
            ),
            Action::Drop { .. }
        ));

        // Then the robot is healthy for a second and a half. The wheel driver
        // alone carries the stream, at 100 Hz, monotonically.
        for j in 1..150i64 {
            let r = r1 + j * 10 * MS;
            assert!(
                matches!(
                    i.offer(
                        Topic::Tf,
                        &at("odom", "base", STAMP0 + (r - t0), r),
                        &node("/ekf")
                    ),
                    Action::Publish { .. }
                ),
                "nothing is wrong with the stream, at j={j}"
            );
        }

        // …and only now does the wheel driver have its own, unrelated hiccup —
        // by exactly the same 5 s, so *only* the window can separate the two.
        let r2 = r1 + 1_500 * MS;
        assert_eq!(
            i.offer(
                Topic::Tf,
                &at("odom", "base", STAMP0 + (r2 - t0) - 5 * S, r2),
                &node("/ekf")
            ),
            Action::Drop {
                reason: DropReason::NonMonotonic {
                    by_nanos: 5 * S - 10 * MS
                }
            },
            "an old fault is not corroboration for this one"
        );

        let s = i.stats();
        assert_eq!(s.clock_resets, 0, "two faults, no promotion");
        assert_eq!(s.dropped_non_monotonic, 2);
        assert!(s.balanced(), "{s:?}");
        assert_eq!(i.offsets().steps(), 2);
    }

    /// **A forward jump is detected**, which no backward-regression watcher can
    /// see at all.
    ///
    /// Mutant: `if residual > -self.policy.reset_threshold_nanos` in place of
    /// `residual.saturating_abs() <= self.policy.reset_threshold_nanos`, i.e.
    #[test]
    fn a_forward_common_mode_jump_is_detected() {
        let mut i = ingest();
        let t0 = 5_000 * S;
        for k in 0..5i64 {
            let r = t0 + k * 10 * MS;
            for (parent, child, who) in [("map", "odom", "/amcl"), ("odom", "base", "/ekf")] {
                i.offer(
                    Topic::Tf,
                    &at(parent, child, STAMP0 + (r - t0), r),
                    &node(who),
                );
            }
        }

        // Somebody seeks the bag thirty seconds ahead.
        let r1 = t0 + 50 * MS;
        assert!(
            matches!(
                i.offer(
                    Topic::Tf,
                    &at("map", "odom", STAMP0 + (r1 - t0) + 30 * S, r1),
                    &node("/amcl")
                ),
                Action::Publish { .. }
            ),
            "one publisher jumping forward is monotone and is written"
        );
        let r2 = t0 + 60 * MS;
        assert_eq!(
            i.offer(
                Topic::Tf,
                &at("odom", "base", STAMP0 + (r2 - t0) + 30 * S, r2),
                &node("/ekf")
            ),
            Action::Halt {
                reason: HaltReason::ClockReset {
                    delta_nanos: 30 * S,
                    evidence: ClockEvidence::CommonMode { publishers: 2 },
                }
            },
            "…two of them jumping forward together is the clock"
        );

        let s = i.stats();
        assert_eq!(s.applied, 11, "ten warm-up samples and the forward one");
        assert_eq!(s.dropped_non_monotonic, 1, "only the halting sample");
        assert_eq!(s.clock_resets, 1);
        assert!(s.balanced(), "{s:?}");
    }

    /// **Attribution is not a correctness dependency** (§5.3, P5) — and this is
    /// the regression test for the defect that motivated the whole redesign.
    ///
    /// Mutant: key the offset table on the edge rather than on the publisher
    /// (`self.offsets.observe(&format!("{parent}/{child}"), ..)` in `offer`).
    #[test]
    fn unattributed_publishers_never_halt() {
        for (label, localizer, driver, halts) in [
            ("attributed", node("/amcl"), node("/ekf"), true),
            (
                "unattributed",
                Publisher::Unattributed,
                Publisher::Unattributed,
                false,
            ),
        ] {
            let mut i = ingest();
            let t0 = 5_000 * S;
            for k in 0..5i64 {
                let r = t0 + k * 10 * MS;
                i.offer(
                    Topic::Tf,
                    &at("map", "odom", STAMP0 + (r - t0), r),
                    &localizer,
                );
                i.offer(
                    Topic::Tf,
                    &at("odom", "base", STAMP0 + (r - t0), r),
                    &driver,
                );
            }
            // A genuine, unambiguous, five-second `/clock` rewind.
            let r1 = t0 + 50 * MS;
            assert!(matches!(
                i.offer(
                    Topic::Tf,
                    &at("map", "odom", STAMP0 + (r1 - t0) - 5 * S, r1),
                    &localizer
                ),
                Action::Drop { .. }
            ));
            let r2 = t0 + 60 * MS;
            let second = i.offer(
                Topic::Tf,
                &at("odom", "base", STAMP0 + (r2 - t0) - 5 * S, r2),
                &driver,
            );
            assert_eq!(
                matches!(second, Action::Halt { .. }),
                halts,
                "{label}: {second:?}"
            );
            assert_eq!(i.stats().clock_resets, u64::from(halts), "{label}");
            assert_eq!(
                i.offsets().tracked(),
                if halts { 2 } else { 1 },
                "{label}: an unattributable robot has exactly one identity"
            );
            assert!(i.stats().balanced(), "{label}: {:?}", i.stats());
        }
    }

    /// **The authoritative path halts with no inference at all** — no threshold,
    /// no window, no corroboration, no transform in hand.
    ///
    /// Mutant: `self.stats.dropped_non_monotonic += 1;` inside
    /// `apply_clock_reset` rather than at the one call site that has a transform
    /// in hand.
    #[test]
    fn note_time_jump_halts_with_no_inference_at_all() {
        let mut i = ingest();
        assert_eq!(
            i.note_time_jump(-5 * S, JumpKind::Backward),
            Action::Halt {
                reason: HaltReason::ClockReset {
                    delta_nanos: -5 * S,
                    evidence: ClockEvidence::Reported {
                        kind: JumpKind::Backward
                    },
                }
            }
        );
        let s = i.stats();
        assert_eq!(s.transforms, 0, "no transform was involved");
        assert_eq!(s.dropped_non_monotonic, 0, "…so no bucket may be charged");
        assert_eq!(s.clock_resets, 1, "…but it is a promotion");
        assert!(s.balanced(), "{s:?}");
    }

    /// **An authoritative jump rewinds every guard and forgets every baseline,
    /// so the next recording starts clean.**
    ///
    /// Mutant: drop `self.forget_the_old_recording()` from `apply_clock_reset`'s
    /// `Recreate` arm.
    ///
    /// Mutant: drop `self.offsets.clear()` from `forget_the_old_recording`,
    /// keeping the guard rewind.
    #[test]
    fn an_authoritative_jump_rewinds_every_edge() {
        let mut i = Ingest::with(
            &topo(),
            AuthorityPolicy::FirstWriterWins,
            OnClockReset::Recreate,
            None,
        );
        let t0 = 5_000 * S;
        for k in 0..5i64 {
            let r = t0 + k * 10 * MS;
            for (parent, child, who) in [("map", "odom", "/amcl"), ("odom", "base", "/ekf")] {
                i.offer(
                    Topic::Tf,
                    &at(parent, child, STAMP0 + (r - t0), r),
                    &node(who),
                );
            }
        }

        // The `/clock` subscription reports a nine-second rewind.
        assert_eq!(
            i.note_time_jump(-9 * S, JumpKind::Backward),
            Action::RecreateArena {
                delta_nanos: -9 * S,
                evidence: ClockEvidence::Reported {
                    kind: JumpKind::Backward
                },
            }
        );

        // The new recording starts nine seconds earlier, and **both** edges
        // accept it — including the one no jump was ever observed on.
        let r = t0 + 100 * MS;
        for (parent, child, who) in [("map", "odom", "/amcl"), ("odom", "base", "/ekf")] {
            let v = i.offer(
                Topic::Tf,
                &at(parent, child, STAMP0 + (r - t0) - 9 * S, r),
                &node(who),
            );
            assert!(
                matches!(v, Action::Publish { .. }),
                "the new recording, on {parent} -> {child}: {v:?}"
            );
        }

        // The other two kinds are the same decision.
        for kind in [JumpKind::ClockTypeChanged, JumpKind::Forward] {
            assert_eq!(
                i.note_time_jump(3 * S, kind),
                Action::RecreateArena {
                    delta_nanos: 3 * S,
                    evidence: ClockEvidence::Reported { kind },
                },
                "{kind:?}"
            );
        }
        assert_eq!(i.stats().clock_resets, 3);
        assert!(i.stats().balanced(), "{:?}", i.stats());
    }

    /// **A recreate rewinds *every* edge**, not only the one that tripped it.
    ///
    /// Mutant: drop `self.forget_the_old_recording()` from `apply_clock_reset`'s
    /// `Recreate` arm.
    #[test]
    fn a_recreate_rewinds_every_edge_not_only_the_one_that_tripped_it() {
        let mut i = Ingest::with(
            &topo(),
            AuthorityPolicy::FirstWriterWins,
            OnClockReset::Recreate,
            None,
        );
        let t0 = 5_000 * S;
        for k in 0..5i64 {
            let r = t0 + k * 10 * MS;
            for (parent, child, who) in [("map", "odom", "/amcl"), ("odom", "base", "/ekf")] {
                i.offer(
                    Topic::Tf,
                    &at(parent, child, STAMP0 + (r - t0), r),
                    &node(who),
                );
            }
        }
        let r1 = t0 + 50 * MS;
        assert!(matches!(
            i.offer(
                Topic::Tf,
                &at("map", "odom", STAMP0 + (r1 - t0) - 9 * S, r1),
                &node("/amcl")
            ),
            Action::Drop { .. }
        ));
        let r2 = t0 + 60 * MS;
        assert_eq!(
            i.offer(
                Topic::Tf,
                &at("odom", "base", STAMP0 + (r2 - t0) - 9 * S, r2),
                &node("/ekf")
            ),
            Action::RecreateArena {
                delta_nanos: -9 * S,
                evidence: ClockEvidence::CommonMode { publishers: 2 },
            }
        );

        // The new recording runs. Both edges accept it, twice each — the second
        // pass is what a stale baseline would turn into an agreeing step pair.
        for j in 0..2i64 {
            let r = t0 + 100 * MS + j * 10 * MS;
            for (parent, child, who) in [("map", "odom", "/amcl"), ("odom", "base", "/ekf")] {
                let v = i.offer(
                    Topic::Tf,
                    &at(parent, child, STAMP0 + (r - t0) - 9 * S, r),
                    &node(who),
                );
                assert!(
                    matches!(v, Action::Publish { .. }),
                    "the new recording, at j={j}, on {parent} -> {child}: {v:?}"
                );
            }
        }
        // And an ordinary single-publisher hiccup inside the new recording is an
        // ordinary drop, not a second rebuild.
        let r = t0 + 130 * MS;
        assert!(matches!(
            i.offer(
                Topic::Tf,
                &at("odom", "base", STAMP0 + (r - t0) - 9 * S - 200 * MS, r),
                &node("/ekf")
            ),
            Action::Drop {
                reason: DropReason::NonMonotonic { .. }
            }
        ));
        assert_eq!(i.stats().clock_resets, 1, "one rebuild, not two");
        assert!(i.stats().balanced(), "{:?}", i.stats());
    }

    /// **One node owning every edge never halts, whatever the magnitude.**
    ///
    /// Mutant: count rows rather than distinct publishers (key `OffsetTable` per
    /// edge, `self.offsets.observe(&format!("{parent}/{child}"), ..)`).
    #[test]
    fn one_publisher_owning_every_edge_never_halts() {
        let mut i = ingest();
        let solo = node("/ekf"); // an EKF publishing the whole chain
        let t0 = 5_000 * S;
        for k in 0..5i64 {
            let r = t0 + k * 10 * MS;
            for (parent, child) in [("map", "odom"), ("odom", "base")] {
                assert!(matches!(
                    i.offer(Topic::Tf, &at(parent, child, STAMP0 + (r - t0), r), &solo),
                    Action::Publish { .. }
                ));
            }
        }
        assert_eq!(i.offsets().tracked(), 1, "one node owns both edges");

        // It restarts, and both of its edges regress in the same `TFMessage` —
        // one receipt time, so nothing could be closer together in the window.
        let r1 = t0 + 50 * MS;
        for (parent, child) in [("map", "odom"), ("odom", "base")] {
            let v = i.offer(
                Topic::Tf,
                &at(parent, child, STAMP0 + (r1 - t0) - 9 * S, r1),
                &solo,
            );
            assert!(
                matches!(
                    v,
                    Action::Drop {
                        reason: DropReason::NonMonotonic { .. }
                    }
                ),
                "one publisher cannot corroborate itself, on {parent} -> {child}: {v:?}"
            );
        }

        // …and a ten-hour regression is still one publisher.
        let r2 = t0 + 60 * MS;
        assert!(matches!(
            i.offer(
                Topic::Tf,
                &at("odom", "base", STAMP0 + (r2 - t0) - 36_000 * S, r2),
                &solo
            ),
            Action::Drop { .. }
        ));

        let s = i.stats();
        assert_eq!(s.clock_resets, 0, "no magnitude promotes a lone witness");
        assert_eq!(s.dropped_non_monotonic, 3);
        assert!(s.balanced(), "{s:?}");
    }

    /// **Every knob in [`ClockPolicy`] actually reaches the pipeline.**
    ///
    /// Mutant: `ClockGuard::new(self.clock.on_reset)` in place of
    /// `ClockGuard::with_threshold(self.clock.on_reset,
    /// self.clock.reset_threshold_nanos)`.
    ///
    /// Mutant: `offsets: OffsetTable::new(ClockPolicy::default())` in
    /// `Ingest::with_policies`.
    ///
    /// Mutant: hard-code the window (`age > 1_000_000_000` in
    /// `OffsetTable::observe`), which leaves the threshold half passing.
    #[test]
    fn the_clock_policy_knobs_reach_the_pipeline() {
        // reset_threshold_nanos.
        for (threshold, steps) in [(DEFAULT_RESET_THRESHOLD_NANOS, 1u64), (S, 0)] {
            let mut i = Ingest::with_policies(
                &topo(),
                AuthorityPolicy::FirstWriterWins,
                ClockPolicy {
                    reset_threshold_nanos: threshold,
                    ..ClockPolicy::default()
                },
                None,
            );
            let t0 = 5_000 * S;
            for k in 0..5i64 {
                let r = t0 + k * 10 * MS;
                i.offer(
                    Topic::Tf,
                    &at("odom", "base", STAMP0 + (r - t0), r),
                    &node("/ekf"),
                );
            }
            let r = t0 + 50 * MS;
            assert!(
                matches!(
                    i.offer(
                        Topic::Tf,
                        &at("odom", "base", STAMP0 + (r - t0) - 500 * MS, r),
                        &node("/ekf")
                    ),
                    Action::Drop { .. }
                ),
                "either way the sample is refused, at threshold {threshold}"
            );
            assert_eq!(
                i.offsets().steps(),
                steps,
                "a 1 s threshold must not see a 500 ms hiccup at all"
            );
        }

        // correlation_window_nanos.
        for (window, halts) in [(S, true), (10 * MS, false)] {
            let mut i = Ingest::with_policies(
                &topo(),
                AuthorityPolicy::FirstWriterWins,
                ClockPolicy {
                    correlation_window_nanos: window,
                    ..ClockPolicy::default()
                },
                None,
            );
            let t0 = 5_000 * S;
            for k in 0..5i64 {
                let r = t0 + k * 10 * MS;
                for (parent, child, who) in [("map", "odom", "/amcl"), ("odom", "base", "/ekf")] {
                    i.offer(
                        Topic::Tf,
                        &at(parent, child, STAMP0 + (r - t0), r),
                        &node(who),
                    );
                }
            }
            let r1 = t0 + 50 * MS;
            i.offer(
                Topic::Tf,
                &at("map", "odom", STAMP0 + (r1 - t0) - 5 * S, r1),
                &node("/amcl"),
            );
            // Fifty milliseconds later: inside a one-second window, well outside
            // a ten-millisecond one.
            let r2 = t0 + 100 * MS;
            let v = i.offer(
                Topic::Tf,
                &at("odom", "base", STAMP0 + (r2 - t0) - 5 * S, r2),
                &node("/ekf"),
            );
            assert_eq!(
                matches!(v, Action::Halt { .. }),
                halts,
                "a 10 ms window cannot correlate steps 50 ms apart: {v:?}"
            );
            assert!(i.stats().balanced(), "{:?}", i.stats());
        }
    }

    /// **The common-mode verdict is shaped for the seam that has to report it.**
    ///
    /// Mutant: `publishers: 2` hard-coded in `OffsetTable::observe`'s return.
    #[test]
    fn the_agreeing_publisher_count_is_real() {
        let mut t = OffsetTable::new(ClockPolicy::default());
        let t0 = 5_000 * S;
        for k in 0..5i64 {
            for who in ["/a", "/b", "/c"] {
                let r = t0 + k * 10 * MS;
                t.observe(who, STAMP0 + (r - t0), SteadyNanos(r));
            }
        }
        // Two publishers step; the third is the one whose count is under test.
        for (j, who) in [(0i64, "/a"), (1, "/b")] {
            let r = t0 + 50 * MS + j * MS;
            t.observe(who, STAMP0 + (r - t0) - 5 * S, SteadyNanos(r));
        }
        let r = t0 + 52 * MS;
        assert_eq!(
            t.observe("/c", STAMP0 + (r - t0) - 5 * S, SteadyNanos(r)),
            Some(CommonMode {
                delta_nanos: -5 * S,
                publishers: 3,
            })
        );
    }

    /// **`Strict` accumulates inside the startup window and halts once at its
    /// close, naming everything it found.**
    ///
    /// Mutant: return the halt from the `Verdict::Fatal` arm of `offer`, per
    /// message, as before the change.
    #[test]
    fn strict_accumulates_conflicts_inside_the_window_and_halts_once_at_its_close() {
        let mut i = Ingest::with(&topo(), AuthorityPolicy::Strict, OnClockReset::Halt, None);
        let s = |t: i64| Sample::identity("odom", "base", t);
        assert!(matches!(
            i.offer(Topic::Tf, &s(1_000 * MS), &node("/a")),
            Action::Publish { .. }
        ));
        // A second odometry publisher: recorded, dropped, diagnosed — and the
        // bridge keeps running, because "within a startup window" is a question
        // about time that this message cannot answer.
        match i.offer(Topic::Tf, &s(1_010 * MS), &node("/b")) {
            Action::AuthorityConflict {
                owner,
                intruder,
                first_time,
                ..
            } => {
                assert_eq!((owner, intruder), (node("/a"), node("/b")));
                assert!(first_time);
            }
            other => panic!("Strict does not halt per message: {other:?}"),
        }
        // …and a second URDF, on a different edge, is found in the same run.
        let mut moved = Sample::identity("base", "lidar", 0);
        moved.pose[4] = 0.25;
        assert!(matches!(
            i.offer(Topic::TfStatic, &moved, &node("/rsp_b")),
            Action::StaticConflict { .. }
        ));

        assert_eq!(
            i.close_startup_window(),
            Some(Action::Halt {
                reason: HaltReason::StartupConflicts {
                    authority: 1,
                    statics: 1,
                }
            }),
            "one halt, naming both misconfigurations"
        );
        // The enumeration a caller prints from is on the tables, not on the
        // halt: §5.4 wants both nodes and the edge, and the POD across the C
        // seam has room for neither.
        let conflicts: Vec<_> = i.authority().conflicts().collect();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            (conflicts[0].0, conflicts[0].1),
            ("odom", "base"),
            "the offending edge is nameable"
        );

        // The close charges no bucket: it is caused by transforms already
        // counted, each in its own, at the time they arrived.
        let s = i.stats();
        assert_eq!(s.transforms, 3);
        assert_eq!(
            (s.applied, s.dropped_authority, s.static_conflicts),
            (1, 2, 1)
        );
        assert!(s.balanced(), "{s:?}");
        // Idempotent: the window does not reopen and cannot halt twice.
        assert_eq!(i.close_startup_window(), None);
    }

    /// **A conflict first seen after the window closes does not halt**, and a
    /// clean startup does not halt at all.
    ///
    /// Mutant: leave the window re-closable — drop the `if
    /// !self.startup_window_open { return None; }` guard.
    #[test]
    fn a_conflict_first_seen_after_the_window_closes_does_not_halt() {
        let mut i = Ingest::with(&topo(), AuthorityPolicy::Strict, OnClockReset::Halt, None);
        let s = |t: i64| Sample::identity("odom", "base", t);
        assert!(matches!(
            i.offer(Topic::Tf, &s(1_000 * MS), &node("/a")),
            Action::Publish { .. }
        ));
        assert_eq!(
            i.close_startup_window(),
            None,
            "a clean startup must not halt"
        );

        // An hour later, a late joiner collides.
        match i.offer(Topic::Tf, &s(1_010 * MS), &node("/b")) {
            Action::AuthorityConflict { first_time, .. } => assert!(first_time),
            other => panic!("still loud, still counted, still not fatal: {other:?}"),
        }
        assert_eq!(i.close_startup_window(), None, "the window does not reopen");
        assert_eq!(i.stats().dropped_authority, 1, "…but it is still counted");
        assert!(i.stats().balanced(), "{:?}", i.stats());
    }

    /// **A caller that never closes the window still gets its report.**
    ///
    /// Mutant: move the backstop check below `self.stats.transforms += 1`.
    #[test]
    fn the_startup_window_closes_itself_after_the_backstop() {
        let mut i = Ingest::with(&topo(), AuthorityPolicy::Strict, OnClockReset::Halt, None);
        let s = |t: i64| Sample::identity("odom", "base", t);
        i.offer(Topic::Tf, &s(1_000 * MS), &node("/a"));
        i.offer(Topic::Tf, &s(1_001 * MS), &node("/b")); // the conflict

        let mut halted = None;
        for k in 2..(STARTUP_WINDOW_TRANSFORMS as i64 + 100) {
            if let Action::Halt { reason } =
                i.offer(Topic::Tf, &s(1_000 * MS + k * MS), &node("/a"))
            {
                halted = Some(reason);
                break;
            }
        }
        assert_eq!(
            halted,
            Some(HaltReason::StartupConflicts {
                authority: 1,
                statics: 0
            })
        );
        assert_eq!(
            i.stats().transforms,
            STARTUP_WINDOW_TRANSFORMS,
            "the halting offer is not a transform the bridge processed"
        );
        assert!(i.stats().balanced(), "{:?}", i.stats());
    }

    /// **Only `Strict` refuses to start.** The other two policies close the
    /// window with conflicts on the books and carry on.
    ///
    /// Mutant: drop the `if self.authority.policy() != AuthorityPolicy::Strict {
    /// return None; }` guard from `close_startup_window`.
    #[test]
    fn only_strict_refuses_to_start_at_the_close_of_the_window() {
        for policy in [
            AuthorityPolicy::FirstWriterWins,
            AuthorityPolicy::LastWriterWins,
        ] {
            let mut i = Ingest::with(&topo(), policy, OnClockReset::Halt, None);
            // A second URDF: a conflict every policy records.
            let mut moved = Sample::identity("base", "lidar", 0);
            moved.pose[4] = 0.25;
            assert!(matches!(
                i.offer(Topic::TfStatic, &moved, &node("/rsp_b")),
                Action::StaticConflict { .. }
            ));
            // …and a second odometry publisher, which `FirstWriterWins` records
            // and `LastWriterWins` deliberately does not.
            let s = |t: i64| Sample::identity("odom", "base", t);
            i.offer(Topic::Tf, &s(1_000 * MS), &node("/a"));
            i.offer(Topic::Tf, &s(1_010 * MS), &node("/b"));

            assert_eq!(
                i.close_startup_window(),
                None,
                "{policy:?} does not refuse to start"
            );
            assert_eq!(
                i.stats().static_conflicts,
                1,
                "…and it had a conflict to refuse over, under {policy:?}"
            );
            assert_eq!(
                i.authority().conflicts().count(),
                usize::from(policy == AuthorityPolicy::FirstWriterWins),
                "LastWriterWins reassigns rather than recording, under {policy:?}"
            );
            assert!(i.stats().balanced(), "{:?}", i.stats());
        }
    }

    /// **`StartupConflicts` counts misconfigurations, not messages.**
    ///
    /// Mutant: `self.authority.dropped()` in place of
    /// `self.authority.conflicts().count()`.
    ///
    /// Mutant: `self.statics.conflicts()` in place of
    /// `self.statics.conflicts_by_edge().count()`.
    #[test]
    fn the_startup_halt_counts_faults_not_observations() {
        let mut i = Ingest::with(&topo(), AuthorityPolicy::Strict, OnClockReset::Halt, None);
        let odom = |t: i64| Sample::identity("odom", "base", t);
        let map = |t: i64| Sample::identity("map", "odom", t);

        // One conflict on `odom -> base`, at message rate: five drops.
        i.offer(Topic::Tf, &odom(1_000 * MS), &node("/a"));
        for k in 0..5i64 {
            assert!(matches!(
                i.offer(Topic::Tf, &odom(1_010 * MS + k * MS), &node("/b")),
                Action::AuthorityConflict { .. }
            ));
        }
        // A second, genuinely different misconfiguration on another edge: three
        // more drops.
        i.offer(Topic::Tf, &map(1_000 * MS), &node("/c"));
        for k in 0..3i64 {
            assert!(matches!(
                i.offer(Topic::Tf, &map(1_010 * MS + k * MS), &node("/d")),
                Action::AuthorityConflict { .. }
            ));
        }
        // One bad lidar bracket, re-delivered to four late joiners.
        let mut moved = Sample::identity("base", "lidar", 0);
        moved.pose[4] = 0.25;
        for _ in 0..4 {
            assert!(matches!(
                i.offer(Topic::TfStatic, &moved, &node("/rsp_b")),
                Action::StaticConflict { .. }
            ));
        }

        // The observation counts, which are what the halt must *not* report.
        assert_eq!(i.authority().dropped(), 8);
        assert_eq!(i.stats().static_conflicts, 4);

        assert_eq!(
            i.close_startup_window(),
            Some(Action::Halt {
                reason: HaltReason::StartupConflicts {
                    authority: 2,
                    statics: 1,
                }
            }),
            "two publisher collisions and one bad bracket"
        );
        assert!(i.stats().balanced(), "{:?}", i.stats());
    }

    /// **The queue high-water mark only rises**, so a queue that fills between
    /// two polls is still visible.
    ///
    /// Mutant: assign rather than `max` in `note_queue_depth`.
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
    /// the arena the same config builds** — the two halves of §5.8's resolution,
    /// checked against each other.
    ///
    /// Mutant: seed the store from `config.frames` instead of `config.edges`.
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
    /// Mutant: put `self.stats.applied += 1` back in the `StaticVerified` arm
    /// (dropping `static_verified`).
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
    /// **A cached spelling still counts every stripped slash.**
    ///
    /// Mutant: delete the `note_stripped` call from `Ingest::resolve`.
    #[test]
    fn a_cached_spelling_still_counts_every_stripped_slash() {
        let mut i = ingest();
        for k in 0..100i64 {
            let s = at("/odom", "/base", 1_000_000_000 + k * MS, 5_000 * S + k * MS);
            assert!(matches!(
                i.offer(Topic::Tf, &s, &node("/ekf")),
                Action::Publish { .. }
            ));
        }
        // Two slashes per message, a hundred messages, however many of them
        // took the cache.
        assert_eq!(i.names().stripped_count(), 200);
    }

    /// **A cached spelling still produces its remap row**, because the cache is
    /// populated lazily rather than pre-seeded.
    ///
    /// Mutant: pre-seed `raw` in `Ingest::with_policies` by inserting every
    /// declared pair under its own spelling *and* its slashed one.
    #[test]
    fn a_cached_spelling_still_produces_its_remap_row() {
        let mut i = ingest();
        for k in 0..100i64 {
            let s = at("/odom", "/base", 1_000_000_000 + k * MS, 5_000 * S + k * MS);
            let _ = i.offer(Topic::Tf, &s, &node("/ekf"));
        }
        let rows: Vec<(&str, &str)> = i
            .remaps()
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        assert!(
            rows.contains(&("/odom", "odom")),
            "§5.6's remap table lost the row the wire's spelling produced: {rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|(a, _)| *a == "/odom").count(),
            1,
            "one row per distinct raw spelling, not one per message: {rows:?}"
        );
    }

    /// Every declared edge resolves to its own slot, and the slot names it back.
    ///
    /// Mutant: in `StaticStore::slot_or_insert`, push to `kinds`/`values` before
    /// taking `self.index.len()` as the slot.
    #[test]
    fn every_declared_edge_resolves_to_a_slot_that_names_it_back() {
        let i = ingest();
        for e in &i.declared().edges {
            let (p, c) = e.key();
            let slot = i
                .statics
                .resolve(p, c)
                .unwrap_or_else(|| panic!("declared edge {p} -> {c} has no slot"));
            assert_eq!(
                i.statics.names_of(slot),
                (p, c),
                "slot {slot:?} does not name the edge it was resolved from"
            );
        }
    }
    /// **`StaticStore` and `Authority` agree about what a slot means.**
    ///
    /// Mutant: seed `Authority` from `config.edges.iter().rev()`.
    #[test]
    fn the_authority_and_the_statics_agree_about_slots() {
        let i = ingest();
        for e in &i.declared().edges {
            let (p, c) = e.key();
            let statics_slot = i
                .statics
                .resolve(p, c)
                .unwrap_or_else(|| panic!("declared edge {p} -> {c} has no slot in statics"));
            let authority_slot = i
                .authority
                .slot_of(p, c)
                .unwrap_or_else(|| panic!("declared edge {p} -> {c} has no slot in authority"));
            assert_eq!(
                statics_slot, authority_slot,
                "{p} -> {c}: statics says {statics_slot:?}, authority says {authority_slot:?}"
            );
        }
    }
}
