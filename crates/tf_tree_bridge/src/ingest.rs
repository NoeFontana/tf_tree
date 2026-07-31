//! The decision pipeline — where §5.4 to §5.7 meet.
//!
//! Each of those sections is a table that answers one question. This is the
//! order they are asked in, and the order is not arbitrary. The numbering is
//! `offer`'s own: an earlier revision's list was one step short of the code,
//! because *"declared?"* was inserted later and nobody renumbered.
//!
//! 0. **The startup window** (§5.4). Not a table and not about this sample —
//!    the only step that can answer before the transform is even counted. See
//!    below.
//! 1. **Names** (§5.6). Everything downstream keys on `(parent, child)`, so a
//!    name that is going to be rewritten must be rewritten before anything
//!    records it. Getting this wrong gives you an authority table keyed on
//!    `/base_link` and a static table keyed on `base_link`, which agree about
//!    nothing.
//! 2. **Declared?** (§5.8). Before the kind check, because an undeclared edge
//!    has no declared kind to clash with.
//! 3. **Kind** (§5.7). A hard error, and cheaper to detect than a clock
//!    comparison — but more importantly, an edge whose kind is wrong should not
//!    also be reported as a clock or authority problem. One fault, one
//!    diagnostic.
//! 4. **Static value** (§5.7), *before* authority and **only on
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
//! 5. **Authority** (§5.4). Whether this publisher may write the edge at all.
//!    Before the clock, because a sample from the wrong publisher should not
//!    move that edge's high-water mark — otherwise a rejected intruder
//!    publishing from the future makes the *owner's* subsequent samples look
//!    non-monotonic.
//! 6. **Clock** (§5.5). Last, and **dynamic only**, so that only samples which
//!    are actually going to be written may advance time.
//!
//! Step 5 before step 6 is the other one that is easy to get backwards and hard
//! to notice: with those reversed, one misconfigured node can silently stall the
//! correct one, and the diagnostic blames the victim.
//!
//! # Facts are per edge; judgments have a window
//!
//! `docs/decisions/0011` is the reason two of the steps above have a shape that
//! is otherwise surprising. Every table below answers an exact question about
//! one edge and one message: does this publisher own it, does this value match
//! the declared one, is this stamp behind the newest this edge accepted. *"The
//! clock has been reset"* and *"this deployment must not start"* are judgments
//! **about a set of those facts**, and deriving either from a single fact is
//! what that record found:
//!
//! - **The clock** (§5.5) had one [`ClockGuard`] for the whole stream, so a
//!   publisher's `transform_tolerance` — a steady, correct offset of one edge
//!   relative to another, larger than any threshold — read as a bag loop and
//!   latched the bridge on a healthy robot. The guard is now per edge, and the
//!   promotion to "the clock moved" is a [`ResetQuorum`] over *distinct
//!   publishers* regressing inside a correlation window — publishers and not
//!   edges, because one node owning two dynamic edges regresses both of them
//!   the instant it restarts, and a quorum of edges would fire on exactly the
//!   single-publisher event the rule exists to tolerate. The quorum is floored
//!   by how many publishers the declared topology could supply, so a deployment
//!   with one dynamic edge halts on the first regression rather than never.
//! - **`Strict`** (§5.4) is defined by §5.4's table as *"refuse to start if a
//!   conflict is detected within a startup window"*, and there was no startup
//!   window. There is one now: conflicts are accumulated while it is open and
//!   the halt happens once, at its close, naming everything found. Outside it,
//!   `Strict` is `FirstWriterWins` plus counters.
//!
//! Both windows are counted in **transforms offered**, because
//! [`BridgeStats::transforms`] is this crate's only clock: it is incremented
//! unconditionally at the top of `offer`, it is in-process and monotone, and —
//! decisively for the first of the two — it cannot be moved by a publisher's
//! stamp, which is the very quantity under suspicion. See `crate::clock`'s
//! module docs for the full argument, and `docs/decisions/0011` for what a
//! transform ordinal costs the second.

use std::collections::BTreeMap;

use crate::authority::{Authority, AuthorityPolicy, Verdict};
use crate::clock::{ClockGuard, ClockVerdict, OnClockReset, QuorumVerdict, ResetQuorum};
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

/// How long §5.4's startup window stays open without an explicit close:
/// **4096 transforms**.
///
/// A backstop, not the mechanism. The window exists to answer "did this
/// *deployment* start up misconfigured", which is a question about a duration,
/// and this crate has no clock but the transform ordinal (see the module docs).
/// A caller that owns a real clock closes the window itself with
/// [`Ingest::close_startup_window`] — the `rclcpp` node is expected to drive it
/// from a one-shot **steady** timer, not from `node_->get_clock()`, which is
/// `/clock` under `use_sim_time` and regresses on exactly the bag loop §5.5
/// detects.
///
/// The backstop is what keeps a caller that never closes it — a binding in
/// another language, a test — from accumulating conflicts forever and never
/// reporting them. 4096 transforms is roughly two seconds of a typical
/// 20-transform, 100 Hz `/tf`, and proportionally longer on a sparse stream,
/// where startup is correspondingly slower anyway. It is a poor proxy for a
/// duration and `docs/decisions/0011` says so in as many words rather than
/// pretending otherwise.
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
    ///
    /// **No longer produced by `offer`** — `docs/decisions/0011` moved
    /// `Strict`'s halt to the close of the startup window, where it can name
    /// every conflict instead of the first, so a per-message authority halt is
    /// now [`HaltReason::StartupConflicts`]. The variant is kept because §5.4's
    /// contract is that a `Strict` conflict stops the bridge, and a future
    /// policy that stops on a *specific* pair — a named edge allow-list, say —
    /// is the natural user of it. If it is still unconstructed when Phase 5
    /// closes, delete it then, deliberately.
    AuthorityConflict {
        /// The prior owner.
        owner: Publisher,
        /// The publisher that collided with it.
        intruder: Publisher,
    },
    /// `Halt` policy, and a quorum of publishers said the clock went backwards.
    ClockReset {
        /// By how much, on the edge whose regression completed the quorum.
        by_nanos: i64,
        /// How many **distinct** edges were regressing inside the correlation
        /// window, including the one that tripped it.
        ///
        /// Edges, while the verdict was decided by distinct *publishers* —
        /// [`crate::clock::QuorumVerdict::Reached`] explains the split, and the
        /// short of it is that the edge count is the number an operator can go
        /// and look at. It is at least one and is **not** bounded below by
        /// [`crate::clock::QUORUM_EDGES`]: a deployment with a single dynamic
        /// edge has its quorum floored to one witness and reports `1`.
        ///
        /// Carried because the halt cannot name its members: the C seam's
        /// outcome has room for exactly one `(parent, child)` pair, filled from
        /// the arriving sample, and growing that POD is a `struct_size`-versioned
        /// break. The count is what is left of the evidence, and it is not
        /// decoration — "5 edges" is a bag loop and "2 edges" is a coincidence
        /// of two publishers that an operator may want to go and look at.
        correlated_edges: u32,
    },
    /// `Strict` policy, and the startup window closed with conflicts recorded
    /// (§5.4, `docs/decisions/0011`).
    ///
    /// **One halt for the whole startup, not one per conflict.** `Strict`
    /// exists for CI, and CI wants every misconfiguration out of one run: a
    /// deployment with four bad publishers should take one boot to diagnose,
    /// not four. So both conflict kinds are accumulated while the window is
    /// open and this is raised once at its close, if anything was found.
    ///
    /// The counts are the summary; the enumeration lives in
    /// [`Authority::conflicts`], which a caller reads to print every offending
    /// edge with both of its publishers.
    StartupConflicts {
        /// Distinct `(edge, owner, intruder)` authority conflicts recorded.
        authority: u32,
        /// Distinct static edges whose value was contradicted (§5.7).
        statics: u32,
    },
}

/// A publisher's identity as one borrowed string, for [`ResetQuorum`].
///
/// The three unattributed variants collapse to *fixed* sentinels rather than to
/// per-sample identities: as far as a quorum is concerned an unattributed
/// publisher is one identity, which can only make a quorum harder to reach — the
/// safe direction, since a quorum reached in error is a halt on a healthy robot.
///
/// Bracketed because a ROS node name cannot contain `<`, so a real node can
/// never collide with a sentinel.
fn owner_key(p: &Publisher) -> &str {
    match p {
        Publisher::Node(n) => n.as_str(),
        Publisher::UnknownGid => "<unknown-gid>",
        Publisher::Unattributed => "<unattributed>",
        Publisher::Declared => "<declared>",
    }
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
    /// **One clock guard per edge** (§5.5, `docs/decisions/0011`).
    ///
    /// A single guard over the merged stream measured a quantity that does not
    /// mean anything: the gap between two *different* publishers' stamps. AMCL
    /// and `robot_localization` date `map -> odom` up to a second into the
    /// future; a SLAM node dates it hundreds of milliseconds behind. Either is
    /// a steady offset larger than any threshold on a correctly configured
    /// robot, and the lagging edge's next message then read as a backward jump
    /// off the leading edge's high-water mark — a latched bridge, on a robot
    /// with nothing wrong with it.
    ///
    /// Per edge, the guard measures one publisher's regression against its own
    /// last accepted stamp, which is exactly what Phase 1's ring would refuse
    /// anyway. A [`ByEdge`] rather than a `(String, String)` key so the
    /// steady-state probe allocates nothing; the two owned keys are paid once,
    /// on an edge's first sample. `tests/steady_state_alloc.rs` is the gate.
    clocks: ByEdge<ClockGuard>,
    /// The policy every new per-edge guard is built with.
    ///
    /// Held because a guard cannot be asked what policy it holds and there may
    /// be no guard yet to ask: an edge's guard is built on its first sample,
    /// which can be an hour after construction.
    on_clock_reset: OnClockReset,
    /// Promotes per-edge regressions into a clock-reset judgment.
    ///
    /// Strictly above the guards, never inside them — see [`ResetQuorum`].
    quorum: ResetQuorum,
    /// Whether §5.4's startup window is still open.
    ///
    /// Open from construction. Closed by [`Ingest::close_startup_window`], or
    /// by the [`STARTUP_WINDOW_TRANSFORMS`] backstop, whichever comes first.
    /// Under [`AuthorityPolicy::Strict`] the close is the only thing that
    /// halts.
    startup_window_open: bool,
    /// Distinct static edges contradicted while the window was open.
    ///
    /// The window reads the conflicts already recorded rather than keeping a
    /// second ledger — [`Authority::conflicts`] enumerates the authority half
    /// exactly. [`StaticStore`] has no equivalent: it exposes a `u64` of
    /// conflicting *observations*, and a latched static re-delivered to ten
    /// late joiners is ten observations of one misconfiguration. So the count
    /// of distinct edges is taken here, off the `first_time` flag the store
    /// already computes. A `StaticStore::conflicts_by_edge()` accessor would
    /// remove this field and let the halt name the edges as well as count
    /// them; that is a change to a file this commit does not own.
    startup_static_conflicts: u32,
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
            clocks: BTreeMap::new(),
            on_clock_reset,
            quorum: ResetQuorum::new(),
            startup_window_open: true,
            startup_static_conflicts: 0,
            stats: BridgeStats {
                queue_capacity: 100, // §5.2's KeepLast(100)
                ..BridgeStats::default()
            },
            undeclared: BTreeMap::new(),
        }
    }

    /// Push one transform through every table.
    pub fn offer(&mut self, topic: Topic, sample: &Sample, publisher: &Publisher) -> Action {
        // 0. The startup window's backstop (§5.4), **before the transform is
        //    counted**. A window-close halt is not an event about the arriving
        //    sample — it is caused by samples already counted, minutes ago — so
        //    charging it a bucket in `BridgeStats::balanced()`'s ledger would
        //    make the ledger a lie in order to keep it balanced. The arriving
        //    sample is not processed either: the bridge is stopping, and the
        //    caller latches on this outcome.
        if self.startup_window_open && self.stats.transforms >= STARTUP_WINDOW_TRANSFORMS {
            if let Some(halt) = self.close_startup_window() {
                return halt;
            }
        }

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
                    // **The authority policy is not consulted here, and that is
                    // now a decision rather than an omission.** §5.7 says "then
                    // apply the authority policy", and §5.4's only policy that
                    // does anything beyond dropping-and-counting is `Strict`,
                    // whose definition is "refuse to start if a conflict is
                    // detected within a startup window". So the policy *is*
                    // applied — by the window, at its close, which is where
                    // `docs/decisions/0011` put it. What this arm owes the
                    // window is the record, and the record is what the two
                    // lines below and `StaticStore::reported` are.
                    //
                    // Routing this into `Authority::admit` instead would be
                    // wrong twice over: the intruder would take ownership of an
                    // edge nobody writes to, and `/tf_static` is
                    // `transient_local`, so *when* a latched conflict is
                    // observed is a DDS discovery artefact. A per-message halt
                    // here fires at a time that carries no information about
                    // when anything went wrong, and on a bridge that has been
                    // healthy for an hour it kills a working robot because a
                    // publisher it had never matched finally appeared.
                    self.stats.static_conflicts += 1;
                    self.stats.dropped_authority += 1;
                    // Distinct edges, not observations: see the field's doc.
                    if first_time && self.startup_window_open {
                        self.startup_static_conflicts =
                            self.startup_static_conflicts.saturating_add(1);
                    }
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
        //
        //    **Neither arm halts.** `Verdict::Fatal` is `Strict` reporting a
        //    fact — two publishers on one edge — and §5.4 defines `Strict` as
        //    refusing *"within a startup window"*, which is a judgment about
        //    when. So both drops are disposed of identically here and the
        //    window decides at its close, naming everything it found.
        //    `Authority::admit` records the conflict under either policy, so
        //    there is nothing for this function to accumulate.
        match self.authority.admit(&parent, &child, publisher) {
            Verdict::Accept => {}
            // `Fatal` outside the window means `Strict` has degraded to
            // `FirstWriterWins` plus counters, deliberately and permanently: a
            // bridge that has been healthy for an hour must not be killed by a
            // late-joining publisher. Inside it, this is the accumulation.
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
                // **Count it.** `stats.transforms` was already incremented, so
                // returning without an outcome bucket leaves `balanced()` false
                // forever — precisely the shape `BridgeStats::balanced`'s own
                // doc names as the bug it exists to detect.
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

        // 6. Clock, last: only a sample that will be written may advance time.
        //
        //    The guard is this edge's, built on its first sample. `lookup_mut`
        //    then `insert` rather than `entry`, because `entry` needs owned
        //    keys whether or not it inserts — two allocations on every message
        //    of every edge, for a table that stops growing after each edge's
        //    first sample.
        let verdict = match lookup_mut(&mut self.clocks, &parent, &child) {
            Some(guard) => guard.observe(sample.stamp_nanos),
            None => {
                let mut guard = ClockGuard::new(self.on_clock_reset);
                let verdict = guard.observe(sample.stamp_nanos);
                insert(&mut self.clocks, &parent, &child, guard);
                verdict
            }
        };
        match verdict {
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
                // **Counted whatever the quorum says.** The transform is
                // refused either way — Phase 1 would refuse it too — and this
                // term is in `balanced()`, so a path that skipped it would
                // leave the ledger permanently short. `clock_resets` is *not*
                // incremented here: it counts promotions, and this is a fact
                // about one publisher until the quorum says otherwise.
                self.stats.dropped_non_monotonic += 1;
                match self.quorum.record(
                    &parent,
                    &child,
                    owner_key(publisher),
                    self.stats.transforms,
                    // **Distinct publishers, not declared edges.** The count of
                    // dynamic edges is a proxy that fails on the topology the
                    // quorum was corrected for: one node owning two of them
                    // would declare two corroborators and never be able to
                    // supply the second. `Authority` already knows who owns
                    // what, so the floor is derived from the truth rather than
                    // from a stand-in for it.
                    self.authority.distinct_owners(),
                ) {
                    // No second publisher — one node restarting, hiccuping, or
                    // replaying its own buffer, on however many of its own
                    // edges. Its guard's mark is deliberately not moved, so
                    // that publisher stays refused until it catches up — which
                    // is what the arena would do anyway.
                    QuorumVerdict::Isolated => Action::Drop {
                        reason: DropReason::NonMonotonic { by_nanos },
                    },
                    QuorumVerdict::Reached { edges } => {
                        self.stats.clock_resets += 1;
                        let correlated_edges = u32::try_from(edges).unwrap_or(u32::MAX);
                        match policy {
                            OnClockReset::Halt => Action::Halt {
                                reason: HaltReason::ClockReset {
                                    by_nanos,
                                    correlated_edges,
                                },
                            },
                            OnClockReset::Recreate => {
                                self.forget_the_old_recording();
                                Action::RecreateArena { by_nanos }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Rewind **every** edge's guard, and the quorum with them, for a
    /// [`OnClockReset::Recreate`].
    ///
    /// Every guard, not the one that regressed, because the arena is rebuilt
    /// whole: a mark left behind describes a recording that no longer exists,
    /// and the edges that did not happen to be sampled during the loop would
    /// spend the next recording refusing everything until it caught up with the
    /// last one. The quorum's rows go the same way — carrying them across would
    /// let the first ordinary hiccup after the rebuild form a quorum with edges
    /// from before it, and `Recreate` exists for a bag replay that loops
    /// repeatedly.
    ///
    /// [`ClockGuard::forget`] rather than dropping the map, so the two owned
    /// `String` keys per edge survive and the first sample on every edge after
    /// a recreate does not re-enter the allocating path. `accept_reset` is the
    /// wrong call here twice over: the only stamp in hand belongs to the one
    /// edge that regressed, and seeding every other edge's guard from it is
    /// precisely the cross-edge contamination per-edge guards exist to remove.
    fn forget_the_old_recording(&mut self) {
        for children in self.clocks.values_mut() {
            for guard in children.values_mut() {
                guard.forget();
            }
        }
        self.quorum.clear();
    }

    /// Close §5.4's startup window, and report what it found.
    ///
    /// Returns `Some(Action::Halt { StartupConflicts })` under
    /// [`AuthorityPolicy::Strict`] if any conflict — authority or static — was
    /// recorded while the window was open, and `None` otherwise. Idempotent: a
    /// second call, and the [`STARTUP_WINDOW_TRANSFORMS`] backstop after an
    /// explicit close, return `None`.
    ///
    /// **This is the mechanism; the backstop is the fallback.** A caller with a
    /// real clock — the `rclcpp` node, from a one-shot steady timer — decides
    /// what "startup" means in seconds, which is the unit the question is
    /// actually about. See [`STARTUP_WINDOW_TRANSFORMS`].
    ///
    /// **No counter moves.** A window-close halt is caused by transforms
    /// already counted and dropped, each in its own bucket, at the time they
    /// arrived; charging it a bucket again would double-count them, and this
    /// entry point may be called with no transform in hand at all.
    ///
    /// # What a caller does with the answer
    ///
    /// The counts are a summary. [`Ingest::authority`]'s
    /// [`Authority::conflicts`] enumerates every offending edge with both of
    /// its publishers, which is the report §5.4 wants CI to print — one run,
    /// every misconfiguration.
    pub fn close_startup_window(&mut self) -> Option<Action> {
        if !self.startup_window_open {
            return None;
        }
        self.startup_window_open = false;

        // Only `Strict` refuses to start. The other two policies have already
        // done everything they are going to do, per message, on the way past.
        if self.authority.policy() != AuthorityPolicy::Strict {
            return None;
        }
        // Read at close, and no filtering: nothing recorded so far can have
        // happened after a window that is only now closing.
        let authority = u32::try_from(self.authority.conflicts().count()).unwrap_or(u32::MAX);
        let statics = self.startup_static_conflicts;
        if authority == 0 && statics == 0 {
            return None;
        }
        Some(Action::Halt {
            reason: HaltReason::StartupConflicts { authority, statics },
        })
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
    use crate::clock::DEFAULT_CORRELATION_WINDOW;

    fn node(n: &str) -> Publisher {
        Publisher::Node(n.to_string())
    }
    const MS: i64 = 1_000_000;

    /// The topology every test below runs against: **two** dynamic edges and
    /// **two** static ones, written in the real config format so these tests
    /// exercise the parser the operator will use rather than a struct literal
    /// that could drift from it.
    ///
    /// Two of each, and not one, because after `docs/decisions/0011` a fixture
    /// with one edge of a kind cannot express the questions §5.5 now asks. A
    /// clock reset is a *quorum of distinct edges*, so a one-dynamic-edge
    /// topology can no longer reach a clock halt at all; and the rule that
    /// statics never touch the clock is only observable when two static edges
    /// can regress together. The shape is also the realistic one — `map -> odom`
    /// from a localizer and `odom -> base` from a wheel driver is the pair whose
    /// steady stamp offset was the defect that record was opened about.
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
    /// authority. If **that edge's** guard had already seen its stamp, the
    /// *owner's* subsequent samples would look non-monotonic and be dropped —
    /// one bad node silently stalls the correct one, and the diagnostic blames
    /// the victim.
    ///
    /// The ordering survives `docs/decisions/0011` untouched, and is one of the
    /// few clock-adjacent rules that does: an authority intruder collides, by
    /// definition, on an edge that already has an owner, so the poisoning it
    /// describes was always *within* one edge and per-edge guards do not reach
    /// it. Only the word "the clock" had to become "that edge's clock".
    ///
    /// Mutant: move the clock check above the authority check — applied, and
    /// this failed at `matches!(.., Action::Publish { .. })` on the owner's
    /// third sample, which came back
    /// `Drop { reason: NonMonotonic { by_nanos: 3598990000000 } }` — the hour
    /// the intruder had put into that edge's guard.
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

    /// **A static's stamp must not touch the clock — and under a quorum that
    /// rule got more load-bearing, not less.**
    ///
    /// `robot_state_publisher` commonly stamps statics with zero, and
    /// `/tf_static` is `transient_local`, so the same latched value is
    /// re-delivered by whichever publisher a late joiner discovers — one of
    /// which may stamp with `now()` and the next with the epoch. That is not a
    /// fault: a static transform is constant and its stamp is meaningless.
    ///
    /// Before `docs/decisions/0011` the damage was to the *dynamic* stream: one
    /// shared high-water mark, dragged to the epoch, made every subsequent
    /// dynamic sample look like a bag loop. Per-edge guards move the damage,
    /// they do not remove it — two static edges re-delivered at the epoch by
    /// two different nodes regress two **distinct publishers** inside the
    /// correlation window, which is precisely the quorum. So the rule got
    /// *more* load-bearing: with one latching publisher the mutant costs a
    /// dropped `/tf_static` message, and with two it stops the bridge.
    ///
    /// **Two publishers and not merely two edges**, and the fixture says so on
    /// purpose: the quorum counts distinct owners, so a single
    /// `robot_state_publisher` re-latching both edges is one restart and would
    /// leave the mutant merely dropping. Two independent latching nodes — a URDF
    /// publisher and a `static_transform_publisher` for a bracket — is the
    /// ordinary shape, and it is the shape under which this rule is what stands
    /// between a meaningless stamp and a halted bridge.
    ///
    /// Mutant: run statics through their edge's guard, by hoisting step 6's
    /// `observe` above the `/tf_static` block and honouring a `Reset` there —
    /// applied, and this failed at `[Drop { reason: NonMonotonic { by_nanos:
    /// 1000000000000 } }, Halt { reason: ClockReset { by_nanos: 1000000000000,
    /// correlated_edges: 2 } }]`. The second element is the one that needs the
    /// second latching publisher: with both re-deliveries attributed to one
    /// node the mutant produces two `Drop`s and no halt, and with the
    /// *original* fixture — a single static stamped 0 and never stamped
    /// anything else — it produced neither, because that edge's guard saw the
    /// epoch first and had nothing to regress from.
    #[test]
    fn a_zero_stamped_static_does_not_reset_the_clock() {
        let mut i = ingest();
        i.offer(
            Topic::Tf,
            &Sample::identity("odom", "base", 1_000_000 * MS),
            &node("/ekf"),
        );
        // Two independent latching publishers, one edge each — the ordinary
        // shape: `robot_state_publisher` for the URDF's lidar mount, a
        // `static_transform_publisher` for the GPS bracket. Both stamp with
        // wall time.
        for (child, publisher) in [("lidar", "/rsp_a"), ("gps", "/gps_mount_a")] {
            assert!(matches!(
                i.offer(
                    Topic::TfStatic,
                    &Sample::identity("base", child, 1_000_000 * MS),
                    &node(publisher)
                ),
                Action::StaticVerified { .. }
            ));
        }
        // A late joiner is served the same values by a publisher that stamps
        // them at the epoch. Two edges, both "regressing" a thousand seconds.
        //
        // Collected rather than asserted one at a time on purpose: the second
        // edge is where a mutant stops merely dropping a static and starts
        // halting the bridge, and a per-offer `assert!` would stop at the first
        // and never show it.
        let redelivered: Vec<Action> = [("lidar", "/rsp_b"), ("gps", "/gps_mount_b")]
            .into_iter()
            .map(|(child, publisher)| {
                i.offer(
                    Topic::TfStatic,
                    &Sample::identity("base", child, 0),
                    &node(publisher),
                )
            })
            .collect();
        assert!(
            redelivered
                .iter()
                .all(|a| matches!(a, Action::StaticVerified { .. })),
            "a static's stamp is meaningless and must not reach any guard: {redelivered:?}"
        );
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
        assert_eq!(i.stats().dropped_non_monotonic, 0);
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

    /// **Two publishers a `transform_tolerance` apart do not halt the bridge.**
    ///
    /// This is the regression the whole of `docs/decisions/0011`'s first
    /// finding exists to fix, and it is what a correctly configured robot looks
    /// like: AMCL and `robot_localization` date `map -> odom` up to a second
    /// into the future while the wheel driver stamps `odom -> base` at publish
    /// time. Each edge is perfectly monotone on its own; only the *gap between
    /// them* is large, and it is large by configuration, for the life of the
    /// robot.
    ///
    /// 200 ms of skew, chosen to be **above** the 100 ms reset threshold — that
    /// is what makes this a test about the guard's scope rather than about its
    /// constant. No threshold separates these two publishers from a bag loop,
    /// because `transform_tolerance` is a user parameter with no ceiling; only
    /// correlation across edges does. (The offline half's
    /// `two_publishers_with_different_latencies_ingest_at_the_defaults` is the
    /// same test against the same skew, and predates this one by a phase.)
    ///
    /// Mutant: one shared guard for the whole stream, as before the change
    /// (key every edge's guard on one entry — `lookup_mut(&mut self.clocks,
    /// "*", "*")`) — applied, and this failed at k=0, on the very first `/ekf`
    /// sample after `/amcl`'s, which came back
    /// `Drop { reason: NonMonotonic { by_nanos: 200000000 } }`: the wheel
    /// driver refused, once per message, for the life of the robot.
    #[test]
    fn two_publishers_a_transform_tolerance_apart_do_not_halt() {
        let mut i = ingest();
        /// AMCL's default `transform_tolerance`, and twice the reset threshold.
        const TOLERANCE: i64 = 200 * MS;
        for k in 0..100i64 {
            let now = 1_000 * MS + k * 10 * MS; // the wheel driver, 100 Hz
            if k % 10 == 0 {
                // The localizer, 10 Hz, dating its edge into the future.
                assert!(
                    matches!(
                        i.offer(
                            Topic::Tf,
                            &Sample::identity("map", "odom", now + TOLERANCE),
                            &node("/amcl")
                        ),
                        Action::Publish { .. }
                    ),
                    "the localizer's own stamps are monotone, at k={k}"
                );
            }
            assert!(
                matches!(
                    i.offer(
                        Topic::Tf,
                        &Sample::identity("odom", "base", now),
                        &node("/ekf")
                    ),
                    Action::Publish { .. }
                ),
                "and so are the wheel driver's, at k={k}"
            );
        }
        let s = i.stats();
        assert_eq!(s.applied, 110, "every sample was written");
        assert_eq!(s.dropped_non_monotonic, 0);
        assert_eq!(s.clock_resets, 0, "nothing here is a clock reset");
        assert!(s.balanced(), "{s:?}");
    }

    /// **A lone edge regressing past the threshold is a drop, not a halt.**
    ///
    /// The other half of the boundary. One publisher restarting, hiccuping, or
    /// replaying its own buffer regresses exactly one edge, however far and
    /// however often, and halting a healthy robot for it is an outage caused by
    /// the diagnostic rather than by the fault. It is still refused and still
    /// counted — Phase 1's ring would refuse it too — and `dropped_non_monotonic`
    /// is what `tf_tree doctor` reads.
    ///
    /// The second edge keeps working throughout, which is the property a global
    /// guard could not offer: under one shared high-water mark the restarting
    /// publisher's stale stamps were only ever half the problem.
    ///
    /// Mutant: `QuorumVerdict::Isolated => Action::Halt { .. }` — i.e. promote
    /// on the first regressing edge, which is the pre-0011 behaviour — applied,
    /// and this failed at k=0 with `Halt { reason: ClockReset { by_nanos:
    /// 5000000000, correlated_edges: 1 } }` where a `Drop` was expected: a
    /// permanently latched bridge, from one node restarting.
    #[test]
    fn a_lone_edge_regressing_past_the_threshold_is_dropped_not_halted() {
        let mut i = ingest();
        let map = |t: i64| Sample::identity("map", "odom", t);
        let odom = |t: i64| Sample::identity("odom", "base", t);
        i.offer(Topic::Tf, &map(10_000 * MS), &node("/amcl"));
        i.offer(Topic::Tf, &odom(10_000 * MS), &node("/ekf"));

        // The wheel driver restarts and replays its buffer from five seconds
        // ago, once per message, for fifty messages.
        for k in 0..50i64 {
            assert_eq!(
                i.offer(Topic::Tf, &odom(5_000 * MS + k), &node("/ekf")),
                Action::Drop {
                    reason: DropReason::NonMonotonic {
                        by_nanos: 5_000 * MS - k
                    }
                },
                "one publisher is one publisher, at k={k}"
            );
        }
        // …and the localizer is untouched by any of it.
        assert!(matches!(
            i.offer(Topic::Tf, &map(10_100 * MS), &node("/amcl")),
            Action::Publish { .. }
        ));

        let s = i.stats();
        assert_eq!(s.clock_resets, 0, "no promotion, so no reset");
        assert_eq!(s.dropped_non_monotonic, 50);
        assert!(s.balanced(), "{s:?}");
    }

    /// **A bag loop regresses every edge, and still halts the bridge — with the
    /// ledger balanced.**
    ///
    /// The half of `docs/decisions/0011` that says the narrowing was not a
    /// silent removal of the check. A real `/clock` rewind moves every
    /// publisher at once, so the second edge's first post-rewind sample arrives
    /// within a handful of transforms of the first's, the quorum is met on it,
    /// and §5.5's *"stops and reports"* still happens.
    ///
    /// It also pins the ledger on the halting path: `transforms` is incremented
    /// for every sample, so a path that returns without an outcome bucket
    /// leaves `balanced()` false forever — the exact shape
    /// `BridgeStats::balanced`'s own doc names as the bug it detects.
    ///
    /// Mutant: drop `self.stats.dropped_non_monotonic += 1;` from the `Reset`
    /// arm, keeping it only on the `Isolated` path — applied, and this failed
    /// at `1 != 2` on "both refusals are counted". The `balanced()` assertion
    /// one line below would have failed too, at 3 buckets for 4 transforms;
    /// the explicit count is what says *which* refusal went missing.
    #[test]
    fn a_bag_loop_regresses_every_edge_and_still_halts() {
        let mut i = ingest();
        let map = |t: i64| Sample::identity("map", "odom", t);
        let odom = |t: i64| Sample::identity("odom", "base", t);
        i.offer(Topic::Tf, &map(10_000 * MS), &node("/amcl"));
        i.offer(Topic::Tf, &odom(10_000 * MS), &node("/ekf"));

        // The bag loops back to five seconds in. The first publisher to notice
        // is still only one publisher.
        assert_eq!(
            i.offer(Topic::Tf, &map(5_000 * MS), &node("/amcl")),
            Action::Drop {
                reason: DropReason::NonMonotonic {
                    by_nanos: 5_000 * MS
                }
            }
        );
        assert_eq!(i.stats().clock_resets, 0);
        // The second one is the corroboration: two publishers do not restart in
        // lockstep, so the only thing left underneath them is the clock.
        assert_eq!(
            i.offer(Topic::Tf, &odom(5_000 * MS), &node("/ekf")),
            Action::Halt {
                reason: HaltReason::ClockReset {
                    by_nanos: 5_000 * MS,
                    correlated_edges: 2,
                }
            }
        );

        let s = i.stats();
        assert_eq!(s.clock_resets, 1, "one promotion, not one per regression");
        assert_eq!(s.dropped_non_monotonic, 2, "both refusals are counted");
        assert!(s.balanced(), "{s:?}");
    }

    /// **Two regressions a whole correlation window apart are two faults, not
    /// one clock.**
    ///
    /// The quorum is a rule about *coincidence*, and coincidence is measured on
    /// the ordinal `offer` hands the quorum — `stats.transforms`, this crate's
    /// only clock. `a_bag_loop_regresses_every_edge_and_still_halts` pins the
    /// promotion when two edges regress side by side; nothing pinned that the
    /// number carrying "side by side" from `offer` to `ResetQuorum` is the real
    /// one. So this is the negative half: a healthy stream runs for longer than
    /// the window between one publisher's hiccup and another's, and the two must
    /// not be added together into a halt. Without it, an unattended bridge that
    /// saw two unrelated single-publisher faults an hour apart would stop the
    /// robot and blame the clock — the exact false halt `docs/decisions/0011`
    /// exists to remove, reintroduced through the correlation window.
    ///
    /// The gap is `DEFAULT_CORRELATION_WINDOW + 2` transforms, one more than the
    /// window admits, and the transform-count assertions state it rather than
    /// leaving it to be re-derived from the loop bounds.
    ///
    /// Mutant: pass a constant `0` as `at_seq` (`self.quorum.record(&parent,
    /// &child, owner_key(publisher), 0, ..)`), so every
    /// regression looks adjacent to every other — applied, and this failed at
    /// the last offer with `Halt { reason: ClockReset { by_nanos: 9097000000,
    /// correlated_edges: 2 } }` where a `Drop` was expected. The other 89 tests
    /// in the crate stayed green under it, because every other quorum test
    /// wants its regressions *correlated*.
    #[test]
    fn two_regressions_a_window_apart_are_two_faults_not_one_clock() {
        let mut i = ingest();
        let map = |t: i64| Sample::identity("map", "odom", t);
        let odom = |t: i64| Sample::identity("odom", "base", t);
        i.offer(Topic::Tf, &map(10_000 * MS), &node("/amcl"));
        i.offer(Topic::Tf, &odom(10_000 * MS), &node("/ekf"));

        // The localizer restarts and replays from five seconds ago: one
        // publisher, one edge, a drop.
        assert_eq!(
            i.offer(Topic::Tf, &map(5_000 * MS), &node("/amcl")),
            Action::Drop {
                reason: DropReason::NonMonotonic {
                    by_nanos: 5_000 * MS
                }
            }
        );
        assert_eq!(i.stats().transforms, 3, "the first regression's ordinal");

        // Then the robot is healthy for longer than the window. The wheel
        // driver alone carries the stream, monotonically.
        for k in 0..=DEFAULT_CORRELATION_WINDOW {
            let t = 10_000 * MS + (i64::try_from(k).unwrap() + 1) * MS;
            assert!(
                matches!(
                    i.offer(Topic::Tf, &odom(t), &node("/ekf")),
                    Action::Publish { .. }
                ),
                "nothing is wrong with the stream, at k={k}"
            );
        }

        // …and only now does the wheel driver have its own, unrelated hiccup.
        assert_eq!(
            i.offer(Topic::Tf, &odom(5_000 * MS), &node("/ekf")),
            Action::Drop {
                reason: DropReason::NonMonotonic {
                    by_nanos: 9_097 * MS
                }
            },
            "an old fault is not corroboration for this one"
        );
        assert_eq!(
            i.stats().transforms,
            3 + DEFAULT_CORRELATION_WINDOW + 2,
            "the second regression is a full window past the first"
        );

        let s = i.stats();
        assert_eq!(s.clock_resets, 0, "two faults, no promotion");
        assert_eq!(s.dropped_non_monotonic, 2);
        assert!(s.balanced(), "{s:?}");
    }

    /// **A recreate rewinds *every* edge, not only the one that tripped it.**
    ///
    /// The arena is rebuilt whole, so a high-water mark left behind describes a
    /// recording that no longer exists — and the edges that happened not to be
    /// sampled during the loop would spend the whole next recording refusing
    /// everything until it caught up with the last one. The quorum's rows go
    /// the same way, or the first ordinary hiccup after the rebuild forms a
    /// quorum with edges from before it and halts a freshly built arena.
    ///
    /// Mutant: rewind only the observing edge (`accept_reset(sample.stamp_nanos)`
    /// on the one guard, as before the change, instead of
    /// `forget_the_old_recording()`) — applied, and this failed at
    /// `matches!(.., Action::Publish { .. })` where the localizer's first
    /// sample of the *new* recording came back
    /// `RecreateArena { by_nanos: 8900000000 }`: an arena rebuilt, and then
    /// immediately rebuilt again, by an edge nothing was wrong with.
    ///
    /// Mutant: drop the `self.quorum.clear()` from `forget_the_old_recording`
    /// — applied, and this failed at the *last* assertion, where an ordinary
    /// 200 ms hiccup came back `RecreateArena { by_nanos: 200000000 }`: the two
    /// edges' rows from before the rebuild were still on the books, so one
    /// post-rebuild regression completed a quorum with a recording that no
    /// longer existed.
    #[test]
    fn a_recreate_rewinds_every_edge_not_only_the_one_that_tripped_it() {
        let mut i = Ingest::with(
            &topo(),
            AuthorityPolicy::FirstWriterWins,
            OnClockReset::Recreate,
            None,
        );
        let map = |t: i64| Sample::identity("map", "odom", t);
        let odom = |t: i64| Sample::identity("odom", "base", t);
        i.offer(Topic::Tf, &map(10_000 * MS), &node("/amcl"));
        i.offer(Topic::Tf, &odom(10_000 * MS), &node("/ekf"));
        assert!(matches!(
            i.offer(Topic::Tf, &map(1_000 * MS), &node("/amcl")),
            Action::Drop { .. }
        ));
        assert_eq!(
            i.offer(Topic::Tf, &odom(1_000 * MS), &node("/ekf")),
            Action::RecreateArena {
                by_nanos: 9_000 * MS
            }
        );

        // The new recording starts, and both edges accept it — including the
        // localizer's, which is not the edge the reset was detected on.
        assert!(matches!(
            i.offer(Topic::Tf, &map(1_100 * MS), &node("/amcl")),
            Action::Publish { .. }
        ));
        assert!(matches!(
            i.offer(Topic::Tf, &odom(1_100 * MS), &node("/ekf")),
            Action::Publish { .. }
        ));
        // And an ordinary single-edge hiccup inside the new recording is an
        // ordinary drop, not a quorum with the recording that was thrown away.
        assert!(matches!(
            i.offer(Topic::Tf, &odom(900 * MS), &node("/ekf")),
            Action::Drop {
                reason: DropReason::NonMonotonic { .. }
            }
        ));
        assert!(i.stats().balanced(), "{:?}", i.stats());
    }

    /// **`Strict` accumulates inside the startup window and halts once at its
    /// close, naming everything it found.**
    ///
    /// §5.4 defines `Strict` as *"refuse to start if a conflict is detected
    /// within a startup window. For CI."*, and CI wants every misconfiguration
    /// out of one run: a deployment with a duplicate odometry publisher **and**
    /// two `robot_state_publisher`s carrying different URDFs should take one
    /// boot to diagnose, not two. A halt on the first conflict delivers a
    /// quarter of what the policy is for.
    ///
    /// Both conflict kinds reach the window, which is the second half of what
    /// `docs/decisions/0011` settled: §5.7 says a differing static value is
    /// followed by *"then apply the authority policy"*, and the only policy
    /// that does anything is this one.
    ///
    /// Mutant: return the halt from the `Verdict::Fatal` arm of `offer`, per
    /// message, as before the change — applied, and this failed at
    /// `Strict does not halt per message: Halt { reason: AuthorityConflict {
    /// owner: Node("/a"), intruder: Node("/b") } }` on the second offer. The
    /// static conflict below it was never reached at all: the run that was
    /// supposed to report both misconfigurations reported one, which is the
    /// three-quarters of the policy's value a halt-on-first throws away.
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
    /// `Strict` outside the window is `FirstWriterWins` plus counters, stated
    /// rather than incidental. Two reasons it must be: a bridge that has been
    /// healthy for an hour must not be killed by a late-joining publisher; and
    /// `/tf_static` is `transient_local`, so *when* a latched conflict is
    /// observed is a DDS discovery artefact and not a fault time — a
    /// late-joining subscriber can otherwise surface a startup fault an hour in
    /// and take down a working robot.
    ///
    /// Mutant: leave the window re-closable — drop the
    /// `if !self.startup_window_open { return None; }` guard — applied, and
    /// this failed at the second `close_startup_window()` with
    /// `Some(Halt { reason: StartupConflicts { authority: 1, statics: 0 } })`
    /// against `None`, which is the late-joiner outage this rule exists to
    /// prevent.
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
    /// The backstop. `close_startup_window` is the mechanism — a caller with a
    /// real clock decides what "startup" means in seconds — but a binding in
    /// another language, or a test, may never call it, and a `Strict` bridge
    /// that accumulated conflicts and reported none would be the worst of both
    /// designs. So the window also closes on its own at
    /// `STARTUP_WINDOW_TRANSFORMS`.
    ///
    /// The halt lands on the offer *after* the ordinal reaches the backstop and
    /// **charges that transform nothing**: it is not an event about the
    /// arriving sample, which is why the check sits above `transforms += 1`.
    ///
    /// Mutant: move the backstop check below `self.stats.transforms += 1` —
    /// applied, and this failed at the `balanced()` assertion with
    /// `BridgeStats { transforms: 4096, applied: 4094, dropped_authority: 1, .. }`
    /// — 4095 buckets for 4096 transforms, because the halting offer had been
    /// counted and then given no bucket. The transform-count assertion above it
    /// passes under this mutant (both orders halt at the same ordinal), which
    /// is why the ledger is the assertion that catches it.
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
    /// Every other startup-window test runs under `Strict`, because `Strict` is
    /// the only policy that can halt — which left the *guard* that says so
    /// unpinned, and it guards the default. `FirstWriterWins` is what a robot
    /// runs, and a robot that stopped 4096 transforms in because two
    /// `robot_state_publisher`s disagree about a lidar bracket would be an
    /// outage introduced by a policy nobody selected. `LastWriterWins` is here
    /// too because it records *fewer* authority conflicts than
    /// `FirstWriterWins` — it reassigns the edge instead — so a guard keyed on
    /// the wrong thing could pass on one and fail on the other.
    ///
    /// The static conflict is what makes this test non-vacuous under both: the
    /// static store is consulted before authority, so it lands on the books
    /// whatever the policy is, and the `static_conflicts` assertion is what says
    /// the window had something it *could* have halted on.
    ///
    /// Mutant: drop the `if self.authority.policy() != AuthorityPolicy::Strict
    /// { return None; }` guard from `close_startup_window` — applied, and this
    /// failed on the first iteration with `Some(Halt { reason:
    /// StartupConflicts { authority: 1, statics: 1 } })` against `None`, i.e.
    /// the default policy halting a healthy robot at startup.
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
    /// Both numbers on the halt are counts of *distinct faults*, and both have
    /// an observation count sitting right next to them that a reader could take
    /// instead. The distinction is the whole diagnostic: `Strict` exists to tell
    /// CI *"you have two problems"*, and a halt reporting `authority: 4200`
    /// because one intruder published at 100 Hz for 42 seconds says nothing
    /// about how many things are wrong with the deployment. `/tf_static` makes
    /// the same point harder: it is `transient_local`, so one misconfigured
    /// bracket is re-delivered to every late joiner, and counting deliveries
    /// would make the number a function of how many subscribers happened to
    /// appear.
    ///
    /// The fixture keeps the two pairs of numbers far apart on purpose — 8
    /// authority *drops* across 2 conflicts, 4 static *observations* across 1
    /// edge — so neither substitution can pass by coincidence.
    ///
    /// Mutant: `self.authority.dropped()` in place of
    /// `self.authority.conflicts().count()` — applied, and this failed at
    /// `StartupConflicts { authority: 8, statics: 1 }` against
    /// `{ authority: 2, statics: 1 }`.
    ///
    /// Mutant: `self.stats.static_conflicts` in place of
    /// `self.startup_static_conflicts` — applied, and this failed at
    /// `StartupConflicts { authority: 2, statics: 4 }`, which is the
    /// late-joiner count and not a fault count at all.
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

    /// **A deployment with one publisher halts on its first regression**, and
    /// the floor that makes it do so is derived from *who owns the edges*, not
    /// from how many edges there are.
    ///
    /// This pins the wiring, which is the half a `clock.rs` unit test cannot
    /// see. `ResetQuorum` takes the corroboration floor as a parameter and is
    /// correct for whatever it is given; whether `Ingest` hands it the truth is
    /// decided here.
    ///
    /// An earlier revision passed the count of declared dynamic *edges* as a
    /// proxy for the number of publishers. It fails on exactly the topology the
    /// quorum was corrected for: one node owning `map -> odom` and
    /// `odom -> base` declares two edges, so the floor stayed at two, so that
    /// node could never corroborate itself, so §5.5's detection was silently
    /// unreachable for it — the very defect the floor exists to prevent, moved
    /// one step along rather than removed.
    ///
    /// Halting here is not a weaker answer. With one publisher a backward jump
    /// is *observationally identical* whether that node restarted or the clock
    /// moved; there is no second party whose agreement could separate them, so
    /// §5.5's "stop and report" is the only conclusion the evidence supports.
    ///
    /// Mutant: pass a constant `2` as `corroborators` in place of
    /// `self.authority.distinct_owners()` — i.e. the old edge-count proxy for
    /// this topology — applied, and this failed with
    /// `Drop { reason: NonMonotonic { by_nanos: 9000000000 } }` where a halt was
    /// asserted. Every other test in the crate stayed green under it, because
    /// every other one has two publishers.
    #[test]
    fn one_publisher_owning_every_edge_halts_on_its_first_regression() {
        let mut i = ingest();
        let map = |t: i64| Sample::identity("map", "odom", t);
        let odom = |t: i64| Sample::identity("odom", "base", t);
        // One node owns both dynamic edges — an EKF publishing the whole chain.
        let solo = node("/ekf");
        assert!(matches!(
            i.offer(Topic::Tf, &map(10_000_000_000), &solo),
            Action::Publish { .. }
        ));
        assert!(matches!(
            i.offer(Topic::Tf, &odom(10_000_000_000), &solo),
            Action::Publish { .. }
        ));
        assert_eq!(i.authority.distinct_owners(), 1, "one node owns both edges");

        // It restarts. There is no second publisher to corroborate with, so the
        // floor is one and this is a reset by the only standard available.
        let v = i.offer(Topic::Tf, &map(1_000_000_000), &solo);
        assert!(
            matches!(
                v,
                Action::Halt {
                    reason: HaltReason::ClockReset { .. }
                }
            ),
            "a lone publisher's regression is a reset, because nothing else could \
             ever agree with it: {v:?}"
        );
        assert!(i.stats().balanced(), "{:?}", i.stats());
    }
}
