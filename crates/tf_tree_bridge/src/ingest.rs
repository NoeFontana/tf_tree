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
//! # Facts are per edge; judgments need corroboration
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
//!   latched the bridge on a healthy robot. The guard is now per edge and
//!   decides one thing only: whether *this* sample is dropped. Promotion to
//!   "the clock moved" is a separate ladder with its own evidence — see
//!   `crate::clock`'s module docs, which record why `0011`'s quorum was itself
//!   wrong three times over and what replaced it.
//! - **`Strict`** (§5.4) is defined by §5.4's table as *"refuse to start if a
//!   conflict is detected within a startup window"*, and there was no startup
//!   window. There is one now: conflicts are accumulated while it is open and
//!   the halt happens once, at its close, naming everything found. Outside it,
//!   `Strict` is `FirstWriterWins` plus counters.
//!
//! # The two windows are counted in different units, on purpose
//!
//! The **startup** window is counted in transforms offered
//! ([`BridgeStats::transforms`], incremented unconditionally at the top of
//! `offer`), with an explicit [`Ingest::close_startup_window`] as the real
//! mechanism and the ordinal only as a backstop. `0011` chose that because the
//! crate had no clock at all; it now has one, and the ordinal is kept **by
//! choice**. What the window asks is *"is this deployment still starting up"*,
//! and the answer belongs to a caller that knows what it launched — the rclcpp
//! node drives it from a one-shot steady timer. A backstop measured in
//! transforms is deterministic, forgeable by nobody, and needs no reading from a
//! caller who supplied none.
//!
//! The **correlation** window in `crate::clock` is nanoseconds of receipt time
//! ([`crate::SteadyNanos`]) and could not be anything else: it asks whether two
//! publishers moved *at the same time*, and a transform ordinal makes that mean
//! two seconds on a busy stream and minutes on a sparse one. That difference is
//! the whole of P3 in `crate::clock`'s module docs.

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
    /// The clock moved under [`OnClockReset::Recreate`]: build a fresh arena,
    /// then re-offer this sample.
    RecreateArena {
        /// New time minus old time — **negative for a rewind**.
        ///
        /// Signed, and named for the sign, because the clock can now be judged
        /// to have moved *forward* as well: an authoritative jump report
        /// ([`Ingest::note_time_jump`]) and a common-mode step both see a sim
        /// fast-forward or a bag seek, which no backward-regression watcher can.
        /// The convention is `rcl_time_jump_t::delta`'s — *"the new time minus
        /// the last time before the jump"* — so the two ends of the seam agree
        /// about the sign without a conversion nobody would remember to write.
        delta_nanos: i64,
        /// Which rung of §5.5's ladder fired, and how strong it was.
        ///
        /// Carried for the same reason [`HaltReason::ClockReset`] carries it,
        /// and it was missing here first: under `Recreate` the C seam had no way
        /// to distinguish a rebuild the *time source reported* from one this
        /// bridge *inferred* from two publishers agreeing, so it reported
        /// `TFT_BRIDGE_EVIDENCE_NONE` for every inferred recreate. An operator
        /// whose arena is being rebuilt in the field needs to know which of
        /// those happened before anything else — a reported jump means the sim
        /// or the bag did it, an inferred one means the bridge decided, and only
        /// the second one can be wrong.
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
    /// `Halt` policy, and the clock was judged to have moved.
    ClockReset {
        /// New time minus old time — **negative for a rewind**. See
        /// [`Action::RecreateArena`]'s `delta_nanos` for the sign convention
        /// and why it is signed at all.
        delta_nanos: i64,
        /// Which rung of the ladder fired, and how strong it was.
        ///
        /// Carried because the halt cannot name its members: the C seam's
        /// outcome has room for exactly one `(parent, child)` pair, filled from
        /// the arriving sample, and growing that POD is a
        /// `struct_size`-versioned break. This is what is left of the evidence,
        /// and it is not decoration — *"the time source reported it"* and
        /// *"two publishers stepped together"* send an operator to different
        /// places.
        evidence: ClockEvidence,
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

/// A publisher's identity as one borrowed string, keying [`OffsetTable`]'s
/// per-publisher offset baselines.
///
/// Borrowed and not owned because this runs on **every** dynamic sample: an
/// owned key would allocate at message rate for a table that stops growing after
/// each publisher's first sample.
///
/// # What happens when the middleware cannot attribute anything
///
/// The three non-node variants collapse to *fixed* sentinels, so an RMW with no
/// endpoint introspection reports one identity for the whole robot. One identity
/// means one baseline, which means common mode can never reach two publishers,
/// which means **every** regression degrades to a drop and the bridge never
/// halts on the inference path at all.
///
/// That is §5.3 satisfied by construction rather than by promise: *"attribution
/// is diagnostic value, never a correctness dependency"*. Attribution quality
/// now changes how well a clock event is *described* and how likely the fallback
/// rung is to fire; it cannot make the bridge stop, and it cannot stop the
/// bridge from stopping — the authoritative rung
/// ([`Ingest::note_time_jump`]) needs no attribution whatsoever.
///
/// The predecessor of this function fed a quorum floor, where the same collapse
/// had the *opposite* effect: one identity meant a floor of one, and a floor of
/// one meant a lone regression halted. Identical code, inverted consequence —
/// which is why the ladder, and not the key, is where the safety lives.
///
/// Bracketed because a ROS node name cannot contain `<`, so a real node can
/// never collide with a sentinel.
pub(crate) fn owner_key(p: &Publisher) -> &str {
    match p {
        Publisher::Node(n) => n.as_str(),
        Publisher::UnknownGid => "<unknown-gid>",
        Publisher::Unattributed => "<unattributed>",
        Publisher::Declared => "<declared>",
    }
}

/// What §5.6 and §5.8 together decided about a sample's frames.
enum Resolved {
    /// The pair names this declared edge.
    Declared(EdgeSlot),
    /// It normalized, and the config does not declare it. The names are owned
    /// because `Action::UndeclaredEdge` is about to take them.
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
    /// seen once skips §5.6's normalization entirely.
    ///
    /// **Populated lazily, from the slow path, and that is the correctness
    /// argument.** An entry exists only if that exact raw pair has already been
    /// through [`NameNormalizer::normalize`] at least once, so every side effect
    /// a skipped normalize could lose has already happened: both raw names are
    /// in `seen`, so `first_sight` would be `false`, so `remaps` would not grow.
    /// The one exception is the per-*occurrence* stripped-slash count, which
    /// [`Ingest::resolve`] replays explicitly through
    /// `NameNormalizer::note_stripped`.
    ///
    /// **Pre-seeding it at construction would be wrong**, and subtly: the config
    /// declares `odom`, the wire sends `/odom`, and it is the first `normalize`
    /// of `/odom` that appends `("/odom", "odom")` to `remaps()`. That row
    /// crosses the C ABI as `tft_bridge_get_remap` and is what §5.6's *"log the
    /// resulting mapping table at startup"* prints. A pre-seeded cache would
    /// never call `normalize` on it and the row would vanish silently.
    ///
    /// **Bounded without a cap.** `normalize`'s preimage of a name is at most
    /// two strings — the name, and the name with a leading `/` — so at most four
    /// raw pairs per declared edge can ever be inserted, and a pair that does
    /// not resolve to a declared edge is never inserted at all. Nothing the wire
    /// says can grow it.
    raw: EdgeIndex<EdgeSlot>,
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
    clocks: Vec<ClockGuard>,
    /// Every knob §5.5's detection has, in physical units.
    ///
    /// Held whole rather than distributed into the guards because a guard cannot
    /// be asked what policy it holds and there may be no guard yet to ask: an
    /// edge's guard is built on its first sample, which can be an hour after
    /// construction. The threshold and the reset action both come from here, so
    /// the per-edge drop decision and the promotion decision cannot be
    /// configured out of step with each other.
    clock: ClockPolicy,
    /// Per-publisher stamp-to-receipt offsets, and the common-mode rule over
    /// them — the *fallback* rung of `crate::clock`'s ladder.
    ///
    /// Strictly above the guards, never inside them: a guard's job is the exact
    /// per-edge fact, and mixing a global judgment into it is the shape
    /// `docs/decisions/0011` records as the original defect.
    offsets: OffsetTable,
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

    /// A pipeline with an explicit authority policy and clock *action*, and
    /// otherwise the default [`ClockPolicy`].
    ///
    /// The convenience shape, because `--on-clock-reset={halt,recreate}` is the
    /// only clock knob §5.5 puts on the command line. Reach for
    /// [`Ingest::with_policies`] to set a threshold, a correlation window or an
    /// agreement tolerance.
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
        let statics = StaticStore::seeded(&declared);
        // **One guard per declared edge, built here rather than on first sight.**
        // `ClockGuard::with_threshold` is a pure value constructor, and creating
        // it now is observationally identical to creating it at an edge's first
        // sample — which is what the old `lookup_mut`-then-`insert` pair did, one
        // branch and one map write per new edge. Nothing exposes the guards, so
        // an unsampled edge holding a fresh one is invisible; what it buys is
        // that `forget_the_old_recording` has no keys to preserve and nothing on
        // the post-recreate path allocates at all.
        let clocks = (0..statics.slots())
            .map(|_| ClockGuard::with_threshold(clock.on_reset, clock.reset_threshold_nanos))
            .collect();
        // Seeded from the same `declared` `StaticStore` was, and in the same
        // order, so a slot means the same edge in both tables — the invariant
        // `the_authority_and_the_statics_agree_about_slots` pins.
        let authority = Authority::seeded(authority, &declared);
        Ingest {
            // Four raw spellings per declared edge is the bound; see the field.
            raw: EdgeIndex::with_capacity(4 * statics.slots()),
            statics,
            names,
            declared,
            authority,
            clocks,
            clock,
            offsets: OffsetTable::new(clock),
            startup_window_open: true,
            startup_static_conflicts: 0,
            stats: BridgeStats {
                queue_capacity: 100, // §5.2's KeepLast(100)
                ..BridgeStats::default()
            },
            undeclared: BTreeMap::new(),
        }
    }
    /// §5.6 and §5.8 in one step: normalize, then find the declared edge.
    ///
    /// **A repeated spelling does not re-normalize.** See [`Ingest::raw`] for why
    /// that is sound and for the one side effect it replays.
    /// A slot's canonical `(parent, child)`, owned.
    ///
    /// The two allocations `Action` costs, isolated at the arms that actually
    /// carry names so the drop arms pay nothing. They are §5.6's spelling from
    /// the declared topology, not the wire's, so an `Action` names an edge the
    /// same way however the sample spelled it.
    fn edge_names(&self, slot: EdgeSlot) -> (String, String) {
        let (p, c) = self.statics.names_of(slot);
        (p.to_string(), c.to_string())
    }

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
                // Only declared pairs are cached, which is what bounds the table.
                self.raw.insert(rp, rc, slot);
                Resolved::Declared(slot)
            }
            None => Resolved::Undeclared { parent, child },
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

        // 1+2. Names and the declared edge, in one step. §5.6 still runs first —
        //      everything below keys on normalized names — but a spelling this
        //      bridge has already seen resolves straight to its slot without
        //      normalizing, allocating or probing anything by name. See
        //      `Ingest::resolve` and `Ingest::raw`.
        //
        //      Declared? is answered before the kind check, because an
        //      undeclared edge has no declared kind to clash with, and reporting
        //      `KindChange` for it would send an operator looking at
        //      `/tf_static` for an edge nobody ever wrote down.
        let slot = match self.resolve(sample) {
            Resolved::Declared(slot) => slot,
            Resolved::BadName => {
                self.stats.dropped_bad_name += 1;
                return Action::Drop {
                    reason: DropReason::BadName,
                };
            }
            Resolved::Undeclared { parent, child } => {
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
        };
        // **The names are NOT materialized here.** Every arm below that returns
        // an `Action` carrying them clones them from the declared topology at
        // that point instead, because the arms that do *not* carry them — the
        // two `Action::Drop`s — are the ones a misconfigured or stuck publisher
        // occupies at full rate for the life of the robot. Cloning up here cost
        // those paths two allocations per message to build names nothing read.
        //
        // Cloned from the declared topology rather than from the wire, so they
        // are §5.6's canonical spelling whichever way the sample spelled them.

        // 3. Kind. A hard error, and one fault gets one diagnostic.
        //
        //    One array read, where this used to be a second full two-level
        //    descent of `StaticStore::kinds` with the same key step 2 had just
        //    probed — whose entire product was one bit, and whose inserting arm
        //    was unreachable from here because step 2 rejects everything the
        //    config does not declare.
        if topic == Topic::Tf && self.statics.kind_at(slot) == StaticKind::Static {
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
            match self.statics.observe_static_at(slot, sample.pose, publisher) {
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

        // 5. Authority, before the clock — see the module docs.
        //
        //    **Neither arm halts.** `Verdict::Fatal` is `Strict` reporting a
        //    fact — two publishers on one edge — and §5.4 defines `Strict` as
        //    refusing *"within a startup window"*, which is a judgment about
        //    when. So both drops are disposed of identically here and the
        //    window decides at its close, naming everything it found.
        //    `Authority::admit` records the conflict under either policy, so
        //    there is nothing for this function to accumulate.
        // **Destructured, so the two borrows are disjoint.** `admit` wants the
        // names by reference and they live in `statics`; taking them through
        // `self` would borrow all of `self` immutably while `authority` needs to
        // be mutable, and the obvious way out — cloning the names first — put
        // two allocations on the accept path to satisfy the borrow checker
        // rather than to produce anything. Naming the fields is free.
        let Ingest {
            statics, authority, ..
        } = self;
        let (sp, sc) = statics.names_of(slot);
        match authority.admit_at(slot, publisher) {
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

        // 6. Clock, last: only a sample that will be written may advance time.
        //
        //    Two independent things happen here, and the order matters. First
        //    the *fallback* rung of `crate::clock`'s ladder folds this sample's
        //    stamp-to-receipt offset into its publisher's baseline, because a
        //    common-mode step is a fact about the clock whichever way the
        //    per-edge guard is about to rule — a forward jump leaves every edge
        //    perfectly monotone, so a detector that only ran on regressions
        //    could not see one at all. Then the per-edge guard rules on this
        //    sample.
        //
        //    Statics never reach here (they returned at step 4), which is what
        //    keeps a `robot_state_publisher`'s meaningless zero stamp out of its
        //    own offset baseline.
        if let Some(common) =
            self.offsets
                .observe(owner_key(publisher), sample.stamp_nanos, sample.received)
        {
            // **Charged to `dropped_non_monotonic`, including for a forward
            // jump.** The ledger has exactly one bucket for "refused because
            // time misbehaved", `balanced()` demands every counted transform
            // land in one, and inventing a second bucket is a
            // `struct_size`-versioned growth of `tft_bridge_stats` that this
            // work does not own. The bucket's own doc states the widened
            // meaning rather than leaving the name to imply a narrower one.
            self.stats.dropped_non_monotonic += 1;
            return self.apply_clock_reset(
                common.delta_nanos,
                ClockEvidence::CommonMode {
                    publishers: common.publishers,
                },
            );
        }

        // This edge's guard, by index. It used to be a `lookup_mut` and, on an
        // edge's first sample, an `insert` — a two-level descent plus a branch
        // that could only be taken once per edge but was tested on every
        // message. The vector is sized at construction, so there is neither.
        //
        // A sample that promoted above never reaches the guard, so it does not
        // move that edge's high-water mark. Under `Halt` that is what keeps
        // every later sample refused on a bridge whose caller has no latch;
        // under `Recreate` the marks have just been thrown away wholesale.
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
            // **The ladder's bottom rung, and the two arms are deliberately one
            // arm.** A regression past the threshold and a few milliseconds of
            // jitter are the same *decision* — drop this sample, count it,
            // diagnose it — and differ only in how alarming the number is. A
            // single source regressing **never halts**, however far and however
            // often, because one publisher restarting, hiccuping or replaying
            // its own buffer is observationally exactly this and halting a
            // healthy robot for it is an outage caused by the diagnostic rather
            // than by the fault.
            //
            // Nothing is lost by not halting: Phase 1's ring would refuse these
            // stamps anyway, so the arena is protected regardless, and
            // `dropped_non_monotonic` is what `tf_tree doctor` reads. The
            // guard's mark is deliberately not moved, so the offending
            // publisher stays refused until it catches up.
            //
            // `clock_resets` is *not* touched here: it counts promotions, and
            // this is a fact about one publisher.
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
    /// **No inference, no threshold, no window, no corroboration.** ROS 2
    /// publishes clock jumps (`rcl_clock_add_jump_callback`, surfaced by rclcpp
    /// as `Clock::create_jump_callback`), and a `/clock` regression *is* the
    /// event, observed once at its source. This handles the case §5.5 was
    /// actually written for — a bag loop, a sim reset — exactly, and it handles
    /// it without any of the ambiguity the fallback rung exists to manage.
    ///
    /// `delta_nanos` is the new time minus the last time before the jump, so a
    /// rewind is **negative**; that is `rcl_time_jump_t::delta`'s own convention
    /// and passing it through unnegated is what keeps the two ends of the seam
    /// agreeing about the sign.
    ///
    /// Under [`OnClockReset::Recreate`] every per-edge guard and every offset
    /// baseline is forgotten: they describe a time base that no longer exists,
    /// and the new recording has to be able to start. Under
    /// [`OnClockReset::Halt`] they are deliberately kept — see
    /// `Ingest::apply_clock_reset`, which owns that argument for both rungs.
    ///
    /// # Threading, for the rclcpp caller
    ///
    /// rclcpp's jump post-callback does **not** run on the bridge's ingest
    /// thread — with `NodeOptions::use_clock_thread` at its default of `true`
    /// the node's `TimeSource` owns a dedicated `/clock` thread — and every
    /// entry point on the C seam is thread-affine. The callback must therefore
    /// record the jump into a slot the ingest thread drains, and call this from
    /// there. This function makes no attempt to be thread-safe and should not:
    /// a lock here would be a lock taken from inside `rcl`'s clock update.
    ///
    /// # No counter bucket
    ///
    /// Like [`Ingest::close_startup_window`], and for the same reason: this is
    /// not an event about an arriving transform, so charging it a bucket in
    /// [`BridgeStats::balanced`]'s ledger would make the ledger a lie in order
    /// to keep it balanced. `clock_resets` *is* incremented — it counts
    /// promotions and is not a ledger term.
    pub fn note_time_jump(&mut self, delta_nanos: i64, kind: JumpKind) -> Action {
        self.apply_clock_reset(delta_nanos, ClockEvidence::Reported { kind })
    }

    /// Apply [`ClockPolicy::on_reset`] to a clock event, whichever rung of the
    /// ladder produced it.
    ///
    /// One place, so the two rungs cannot drift about what a reset *does* — the
    /// mistake that put a promotion rule inside [`ClockGuard`] in the first
    /// place. The caller charges the ledger, because only the caller knows
    /// whether a transform is in hand.
    /// # Why only `Recreate` forgets the old recording
    ///
    /// A rewind under `Halt` leaves every high-water mark exactly where it was,
    /// and that is load-bearing rather than an omission. This type has no latch
    /// — the C seam and the `rclcpp` node hold that — so a caller that keeps
    /// offering after a halt keeps reaching this pipeline. With the guards
    /// forgotten, the very next post-rewind sample would read as forward motion
    /// and come back `Action::Publish`, writing the new recording's transforms
    /// into an arena the bridge has just been told to stop using. Keeping the
    /// marks means every later sample keeps being refused, which is the
    /// behaviour a stopped bridge should have with or without a latch above it.
    ///
    /// Under `Recreate` the arena really is being rebuilt, so the marks describe
    /// a recording that no longer exists and forgetting them is the whole point.
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

    /// Rewind **every** edge's guard, and every offset baseline with them.
    ///
    /// Every guard, not the one that regressed, because the arena is rebuilt
    /// whole: a mark left behind describes a recording that no longer exists,
    /// and the edges that did not happen to be sampled during the loop would
    /// spend the next recording refusing everything until it caught up with the
    /// last one.
    ///
    /// The offset baselines go the same way, and for a sharper reason: they are
    /// measured against a time base that has just been replaced, so *every*
    /// publisher's first post-reset sample is a step — and those steps all agree
    /// with each other, because they are all the same jump. Kept, they would
    /// make the bridge report a second clock reset caused by nothing but its own
    /// response to the first.
    ///
    /// [`ClockGuard::forget`] rather than dropping the map, so the two owned
    /// `String` keys per edge survive and the first sample on every edge after
    /// a recreate does not re-enter the allocating path. `accept_reset` is the
    /// wrong call here twice over: the only stamp in hand belongs to the one
    /// edge that regressed, and seeding every other edge's guard from it is
    /// precisely the cross-edge contamination per-edge guards exist to remove.
    fn forget_the_old_recording(&mut self) {
        for guard in &mut self.clocks {
            guard.forget();
        }
        self.offsets.clear();
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

    /// The per-publisher offset baselines behind the fallback rung of
    /// `crate::clock`'s ladder, for `doctor` and for tests.
    ///
    /// `OffsetTable::tracked()` reading 1 on a robot with several `/tf`
    /// publishers is the visible symptom of an RMW that cannot attribute — the
    /// bridge still works, and this is where an operator sees that the inference
    /// rung is dormant.
    #[must_use]
    pub fn offsets(&self) -> &OffsetTable {
        &self.offsets
    }

    /// The clock policy in force (§5.5).
    #[must_use]
    pub fn clock_policy(&self) -> ClockPolicy {
        self.clock
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
    use crate::clock::{CommonMode, SteadyNanos, DEFAULT_RESET_THRESHOLD_NANOS};

    fn node(n: &str) -> Publisher {
        Publisher::Node(n.to_string())
    }
    const MS: i64 = 1_000_000;
    const S: i64 = 1_000_000_000;

    /// A sample carrying a **receipt time**, which is what the common-mode rung
    /// of `crate::clock`'s ladder needs and what `Sample::identity` deliberately
    /// leaves unknown.
    ///
    /// Every test that is about the clock uses this; every test that is about
    /// something else uses `Sample::identity` and thereby leaves the inference
    /// rung dormant, which keeps those tests measuring what they claim to.
    fn at(parent: &str, child: &str, stamp_nanos: i64, received: i64) -> Sample {
        Sample::identity(parent, child, stamp_nanos).received_at(SteadyNanos(received))
    }

    /// The topology every test below runs against: **two** dynamic edges and
    /// **two** static ones, written in the real config format so these tests
    /// exercise the parser the operator will use rather than a struct literal
    /// that could drift from it.
    ///
    /// Two of each, and not one, because a fixture with one edge of a kind
    /// cannot express the questions §5.5 asks. Corroboration is between distinct
    /// *publishers*, and the interesting negative cases — one node owning both
    /// dynamic edges, two nodes stepping by unrelated amounts — need two edges
    /// to arrange at all; the rule that statics never touch the clock is only
    /// observable when two static edges can move together. The shape is also the
    /// realistic one: `map -> odom` from a localizer and `odom -> base` from a
    /// wheel driver is the pair whose steady stamp offset was the original
    /// defect.
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

    /// **A static's stamp must not touch the clock — on either rung of the
    /// ladder.**
    ///
    /// `robot_state_publisher` commonly stamps statics with zero, and
    /// `/tf_static` is `transient_local`, so the same latched value is
    /// re-delivered by whichever publisher a late joiner discovers — one of
    /// which may stamp with `now()` and the next with the epoch. That is not a
    /// fault: a static transform is constant and its stamp is meaningless.
    ///
    /// Originally the damage was to the *dynamic* stream: one shared high-water
    /// mark, dragged to the epoch, made every subsequent dynamic sample look
    /// like a bag loop. Per-edge guards moved that damage rather than removing
    /// it, and the common-mode rung makes it worse again in a new way: two
    /// latching nodes re-delivering at the epoch, against a receipt clock that
    /// is genuinely now, are two publishers whose offsets step by the *same*
    /// thousand seconds inside the correlation window — textbook agreement, and
    /// a halt. So "statics return at step 4" is what stands between a
    /// meaningless stamp and a stopped bridge, and it got more load-bearing, not
    /// less.
    ///
    /// **Two publishers and not merely two edges**, and the fixture says so on
    /// purpose: agreement is between distinct owners, so a single
    /// `robot_state_publisher` re-latching both edges is one witness and would
    /// leave the mutant merely dropping. Two independent latching nodes — a URDF
    /// publisher and a `static_transform_publisher` for a bracket — is the
    /// ordinary shape.
    ///
    /// Mutant: hoist step 6's `self.offsets.observe(..)` (and the common-mode
    /// arm with it) above the `/tf_static` block — applied, and this failed at
    /// `only the dynamic publisher has an offset baseline at all`, `left: 5,
    /// right: 1`. The `tracked()` assertion is the tighter form of the check and
    /// fires first: five latching publishers had been given offset baselines
    /// measured from a meaningless stamp, any two of which agree the moment a
    /// late joiner is served the latched set.
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
        // `static_transform_publisher` for the GPS bracket. Both stamp with
        // wall time.
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
        // them at the epoch. Two edges, both "regressing" a thousand seconds.
        //
        // Collected rather than asserted one at a time on purpose: the second
        // edge is where a mutant stops merely dropping a static and starts
        // halting the bridge, and a per-offer `assert!` would stop at the first
        // and never show it.
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

    // ---- §5.5, the clock ladder --------------------------------------------
    //
    // Every test below fixes a receipt-clock origin `t0` and derives a healthy
    // publisher's stamp from it as `STAMP0 + (received - t0)`, so a correct
    // publisher's `stamp - received` offset is *exactly* constant and every
    // step size in an assertion is exact rather than approximately right. `t0`
    // is never 0, because `SteadyNanos(0)` is the "no receipt clock" sentinel.

    /// The stamp a healthy publisher would emit at receipt-clock origin.
    const STAMP0: i64 = 10_000 * MS;

    /// **Two publishers a `transform_tolerance` apart never halt the bridge.**
    ///
    /// The original defect, at the level of the pipeline, and it is what a
    /// correctly configured robot looks like: AMCL and `robot_localization` date
    /// `map -> odom` into the future while the wheel driver stamps
    /// `odom -> base` at publish time. Each edge is perfectly monotone on its
    /// own; only the *gap between them* is large, and it is large by
    /// configuration, for the life of the robot.
    ///
    /// 300 ms of skew, chosen to be **three times** the 100 ms reset threshold,
    /// because no threshold separates these two publishers from a bag loop —
    /// `transform_tolerance` is a user parameter with no ceiling. Two different
    /// mechanisms have to hold for this to pass, and the test names both.
    ///
    /// Mutant: one shared guard for the whole stream (key both the `lookup_mut`
    /// and the `insert` in step 6 on one entry, `"*", "*"`) — applied, and this
    /// failed at `and so are the wheel driver's, at k=0: Drop { reason:
    /// NonMonotonic { by_nanos: 300000000 } }`: the wheel driver refused, once
    /// per message, for the life of the robot. Mutating only the `lookup_mut`
    /// is *not* this mutant and passes — every lookup then misses, so every
    /// sample gets a fresh guard and everything reads as forward motion.
    ///
    /// Mutant: threshold the raw offset instead of the residual in
    /// `OffsetTable::observe` (`if offset.saturating_abs() <= ...`) — applied,
    /// and this failed at `the localizer's own stamps are monotone, at k=10:
    /// Halt { reason: ClockReset { delta_nanos: 0, evidence: CommonMode
    /// { publishers: 2 } } }`. Both publishers' raw offsets are enormous — a
    /// stamp and a steady clock have unrelated epochs — so *every* sample was a
    /// step, every step had a residual of zero, and zero agrees with zero: a
    /// halt on a robot with nothing wrong with it, reporting a clock jump of
    /// exactly nothing.
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
    /// The ladder's bottom rung. One publisher restarting, hiccuping, or
    /// replaying its own buffer regresses exactly one edge, however far and
    /// however often, and halting a healthy robot for it is an outage caused by
    /// the diagnostic rather than by the fault. It is still refused and still
    /// counted — Phase 1's ring would refuse it too — and `dropped_non_monotonic`
    /// is what `tf_tree doctor` reads.
    ///
    /// The second edge keeps working throughout, which is the property a global
    /// guard could not offer.
    ///
    /// Fifty consecutive regressions and **one** recorded step: a publisher that
    /// stays broken must not sit in the correlation window re-arming itself at
    /// message rate, or the next unrelated hiccup anywhere in the tree
    /// corroborates it.
    ///
    /// Mutant: promote on the guard's own `Reset` verdict (give
    /// `ClockVerdict::Reset` its own arm returning `self.apply_clock_reset(..)`)
    /// — applied, and this failed at `one publisher is one publisher, at k=0`,
    /// `left: Halt { reason: ClockReset { delta_nanos: -4900000000, evidence:
    /// CommonMode { publishers: 1 } } }, right: Drop { reason: NonMonotonic
    /// { by_nanos: 4900000000 } }`: a permanently latched bridge, from one node
    /// restarting.
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
    /// The proof that the redesign did not quietly delete §5.5's detection. Both
    /// publishers' `stamp - received` offsets drop by exactly 5 s, within 10 ms
    /// of each other, because the thing underneath them moved by 5 s. Neither
    /// publisher restarted; nothing about either edge alone says so.
    ///
    /// It also pins the ledger on the halting path: `transforms` is incremented
    /// for every sample, so a path that returns without an outcome bucket leaves
    /// `balanced()` false forever.
    ///
    /// Mutant: drop `self.stats.dropped_non_monotonic += 1;` from the
    /// common-mode arm — applied, and this failed at `both refusals are
    /// counted`, `left: 1, right: 2`; the `balanced()` assertion below it would
    /// have failed too, at 11 buckets for 12 transforms.
    ///
    /// Mutant: `if publishers < 3` in `OffsetTable::observe` — applied, and this
    /// failed at `left: Drop { reason: NonMonotonic { by_nanos: 4980000000 } },
    /// right: Halt { reason: ClockReset { delta_nanos: -5000000000, evidence:
    /// CommonMode { publishers: 2 } } }`: a bag loop across two publishers
    /// stopped being detectable on a two-publisher robot, which is most
    /// robots.
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
        // Two publishers do not restart in lockstep by the same amount; a clock
        // they share does move both at once.
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
    /// The negative twin of the test above, at the same window and the same
    /// receipt times: only the *sizes* differ. A launch file respawning two
    /// nodes, or a machine coming back from suspend, moves two publishers within
    /// a second of each other by whatever each had buffered. A rule that asked
    /// only *"did two publishers step inside the window"* halts on that, which
    /// is the same false halt in a new costume.
    ///
    /// 5 s against 400 ms is 92 % apart, so no plausible tolerance admits them.
    ///
    /// Mutant: drop the agreement test from `OffsetTable::observe` and count
    /// every stepped row inside the window — applied, and this failed at
    /// `left: Halt { reason: ClockReset { delta_nanos: -400000000, evidence:
    /// CommonMode { publishers: 2 } } }, right: Drop { reason: NonMonotonic
    /// { by_nanos: 380000000 } }` on the second regression: a 400 ms clock
    /// rewind reported, that never happened.
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
    /// `docs/decisions/0011` measured coincidence in transforms offered, so "at
    /// the same time" meant two seconds on a busy stream and minutes on a sparse
    /// one. Here the stream is deliberately busy — 150 healthy messages fill the
    /// gap — and the gap is still 1.5 s, still outside the 1 s window, and still
    /// two faults. Under the old unit those 150 transforms would have been well
    /// *inside* a 4096-observation window and the bridge would have halted.
    ///
    /// Mutant: `let age = 0;` in `OffsetTable::observe`, so every recorded step
    /// looks adjacent to every other — applied, and this failed at `an old fault
    /// is not corroboration for this one`, `left: Halt { reason: ClockReset
    /// { delta_nanos: -5000000000, evidence: CommonMode { publishers: 2 } } },
    /// right: Drop { reason: NonMonotonic { by_nanos: 4990000000 } }`. An
    /// unattended bridge would stop the robot on two unrelated single-publisher
    /// faults an hour apart and blame the clock.
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
    /// A sim fast-forward, a bag seek, an NTP step ahead: every stamp moves
    /// *forward*, so every edge stays perfectly monotone, every guard reports
    /// `Forward`, and the pre-redesign detector was structurally blind — a
    /// bridge would keep writing across a discontinuity nothing reported. What
    /// this rung tests is agreement, not regression, so the forward case falls
    /// out rather than needing a rule of its own.
    ///
    /// Note the ledger shape, which is peculiar to this case and deliberate: the
    /// *first* publisher's jumped sample is monotone and is **published**, and
    /// only the sample that completes the corroboration is refused. So `applied`
    /// counts it and `dropped_non_monotonic` counts the other — one bucket each,
    /// as always.
    ///
    /// Mutant: `if residual > -self.policy.reset_threshold_nanos` in place of
    /// `residual.saturating_abs() <= self.policy.reset_threshold_nanos`, i.e. a
    /// watcher that looks only for backward motion — applied, and this failed at
    /// `…two of them jumping forward together is the clock`, `left: Publish
    /// { parent: "odom", child: "base", stamp_nanos: 40060000000, .. }, right:
    /// Halt { reason: ClockReset { delta_nanos: 30000000000, evidence:
    /// CommonMode { publishers: 2 } } }`: a 30 s forward seek went entirely
    /// unnoticed and the bridge carried on writing across it.
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
    /// [`Publisher::UnknownGid`] and [`Publisher::Unattributed`] are *unit*
    /// variants, so on an RMW without endpoint introspection every publisher on
    /// the robot compares equal. Under the quorum that was fatal: the floor was
    /// derived from `Authority::distinct_owners()`, which read 1, so a quorum of
    /// 1 was demanded and the **first** single-edge regression latched the
    /// bridge permanently. §5.3 says in as many words that attribution is
    /// *"diagnostic value, never a correctness dependency"*, and that was a
    /// correctness dependency.
    ///
    /// Under the ladder the same collapse means one offset baseline, which means
    /// common mode can never reach two, which means every regression degrades to
    /// a drop. The inference rung goes dormant; nothing halts wrongly.
    ///
    /// The attributed half of this test runs the **identical** stamps, receipt
    /// times and edges and *does* halt, so a broken fixture cannot make the
    /// unattributed half pass by accident.
    ///
    /// Mutant: key the offset table on the edge rather than on the publisher
    /// (`self.offsets.observe(&format!("{parent}/{child}"), ..)` in `offer`) —
    /// applied, and this failed on the unattributed half at `unattributed: Halt
    /// { reason: ClockReset { delta_nanos: -5000000000, evidence: CommonMode
    /// { publishers: 2 } } }`, `left: true, right: false`. Under that key the
    /// two halves stop differing at all, which is the tell: the rule would no
    /// longer be about publishers, so attribution would be back in the halt
    /// decision — by removing it rather than by using it.
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

    /// **The authoritative path halts with no inference at all** — no
    /// threshold, no window, no corroboration, no transform in hand.
    ///
    /// ROS 2 publishes clock jumps, and a `/clock` regression *is* the event
    /// rather than something to be deduced from its consequences. This is the
    /// rung that handles what §5.5 was actually written for — a bag loop, a sim
    /// reset — and it handles it exactly, on a bridge that has never seen a
    /// single sample and has no receipt clock anywhere.
    ///
    /// It charges no counter bucket, like `close_startup_window` and for the
    /// same reason: it is not an event about an arriving transform, so a bucket
    /// would make [`BridgeStats::balanced`]'s ledger a lie in order to keep it
    /// balanced. `clock_resets` moves, because it is not a ledger term.
    ///
    /// Mutant: `self.stats.dropped_non_monotonic += 1;` inside
    /// `apply_clock_reset` rather than at the one call site that has a transform
    /// in hand — applied, and this failed at `…so no bucket may be charged`,
    /// `left: 1, right: 0`: one bucket charged for no transform, which the
    /// `balanced()` assertion below would then have reported as 1 bucket for 0
    /// transforms.
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
    /// Under `recreate` this is the bag-loop workflow end to end, and note what
    /// it does *not* need: the samples here carry no receipt clock at all
    /// (`Sample::identity` leaves it unknown), so the inference rung is dormant
    /// throughout and the whole thing runs on the reported signal.
    ///
    /// `JumpKind::ClockTypeChanged` and `JumpKind::Forward` take the same route:
    /// the kind is evidence for the operator, not a branch — `use_sim_time`
    /// being switched at runtime replaces the time base exactly as a rewind
    /// does.
    ///
    /// Mutant: drop `self.forget_the_old_recording()` from `apply_clock_reset`'s
    /// `Recreate` arm — applied, and this failed at `the new recording, on
    /// map -> odom: Drop { reason: NonMonotonic { by_nanos: 8940000000 } }`
    /// where a `Publish` was expected: the new recording's every sample refused
    /// against the old recording's high-water mark, forever.
    ///
    /// Mutant: drop `self.offsets.clear()` from `forget_the_old_recording`,
    /// keeping the guard rewind — applied, and this failed at `the new
    /// recording, on odom -> base: RecreateArena { delta_nanos: -9000000000 }`:
    /// the bridge rebuilding the arena a second time in response to nothing but
    /// its own first rebuild. This is the failure a *`Recreate` reached through
    /// `offer`* cannot show — there the step that caused it has already re-based
    /// every participating publisher, and
    /// `a_recreate_rewinds_every_edge_not_only_the_one_that_tripped_it` passes
    /// under this mutant. Only a jump reported from *outside* leaves stale
    /// baselines behind, and then every publisher's first post-jump sample is a
    /// step and they all agree with each other. Hence the receipt clocks below:
    /// this is the one test that pins the clear.
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

        // The `/clock` subscription reports a nine-second rewind. Nothing was
        // inferred: no publisher had regressed, and the last thing every edge
        // saw was ordinary forward motion.
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
    /// The arena is rebuilt whole, so a high-water mark left behind describes a
    /// recording that no longer exists, and the edges that happened not to be
    /// sampled during the loop would spend the whole next recording refusing
    /// everything until it caught up with the last one. The localizer here is
    /// the edge that did *not* complete the corroboration, so its guard is
    /// rewound on somebody else's evidence or not at all.
    ///
    /// The **offset baselines** are deliberately not what this test pins. A
    /// `Recreate` reached through `offer` is caused by a step, and a step has
    /// already re-based every publisher that took part in it, so dropping
    /// `self.offsets.clear()` leaves this fixture passing.
    /// `an_authoritative_jump_rewinds_every_edge` is where that clear is pinned,
    /// and its doc explains why only a jump reported from outside can show it.
    ///
    /// Mutant: drop `self.forget_the_old_recording()` from `apply_clock_reset`'s
    /// `Recreate` arm — applied, and this failed at `the new recording, at j=0,
    /// on map -> odom: Drop { reason: NonMonotonic { by_nanos: 8940000000 } }`:
    /// an arena rebuilt, and then every sample of the new recording refused
    /// against the recording it replaced.
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
    /// The inverse of what `docs/decisions/0011` shipped, and the reversal is
    /// the point. That record floored the quorum by `distinct_owners()` so a
    /// single-publisher deployment would halt on its first regression "because
    /// nothing else could ever agree with it". Two failures followed: at boot
    /// the floor reads 1 on a robot that *has* two publishers because the second
    /// has not published yet (AMCL waits for a map), so the wheel driver's first
    /// hiccup latches the bridge; and an unattributable RMW makes the floor
    /// permanently 1 for everyone.
    ///
    /// So the answer is inverted. With one witness there is no evidence that
    /// separates "this node restarted" from "the clock moved", and the
    /// conservative disposition of *no evidence* is to drop the sample — which
    /// Phase 1 would do anyway — not to stop the robot. An operator of a genuine
    /// single-publisher rig who wants a bag loop detected has the authoritative
    /// path, which needs no witnesses at all.
    ///
    /// Both edges regress **in the same message**, which is exactly the topology
    /// that broke the edge-counting quorum, and then a ten-hour regression
    /// follows to pin "whatever the magnitude".
    ///
    /// Mutant: count rows rather than distinct publishers (key `OffsetTable` per
    /// edge, `self.offsets.observe(&format!("{parent}/{child}"), ..)`) —
    /// applied, and this failed at `one node owns both edges`, `left: 2,
    /// right: 1`, one line before it could reach the restart. That assertion is
    /// the cheap form of the whole test: two rows for one node is already the
    /// bug, and the `Drop`s below are its consequence.
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
    /// `Ingest::with` fixes four of the five at their defaults, so
    /// `Ingest::with_policies` is the only way an operator moves them — and a
    /// knob that is accepted and then ignored is worse than no knob, because the
    /// operator believes they have changed something and stops looking.
    ///
    /// One knob from each rung is pinned. `reset_threshold_nanos` decides what
    /// counts as motion at all, and it has to reach the per-edge guard *and* the
    /// offset step detector — the two measure the same publisher misbehaving by
    /// the same amount, and a build where one moved and the other did not would
    /// disagree about a single sample. `correlation_window_nanos` decides how
    /// close together two steps must fall. Each half feeds identical stamps and
    /// receipt times under the default and under a changed policy, so nothing
    /// but the policy can explain the difference.
    ///
    /// Note what the threshold half does *not* assert: both builds return
    /// `Action::Drop` for the regressing sample, because jitter and a
    /// past-threshold regression have the same disposition. The threshold is
    /// visible only in whether a *step* was recorded, which is what makes the
    /// step counter worth exposing.
    ///
    /// Mutant: `ClockGuard::new(self.clock.on_reset)` in place of
    /// `ClockGuard::with_threshold(self.clock.on_reset,
    /// self.clock.reset_threshold_nanos)` — applied, and this test **passed**.
    /// Recorded because it is a true statement about the pipeline and not a gap
    /// in the fixture: under the ladder a jitter drop and a past-threshold
    /// regression have the same disposition *and* the same counter, so the
    /// per-edge guard's threshold has no observable effect through `Ingest` at
    /// all. It is fed from the same field anyway, so a future arm that does
    /// distinguish them cannot be configured out of step with the step
    /// detector, and the guard's own boundary is pinned directly by
    /// `clock::tests::the_threshold_boundary_is_exact`. What the first half
    /// below actually pins is the *step detector's* copy of the constant.
    ///
    /// Mutant: `offsets: OffsetTable::new(ClockPolicy::default())` in
    /// `Ingest::with_policies` — applied, and this failed at `a 1 s threshold
    /// must not see a 500 ms hiccup at all`, `left: 1, right: 0`.
    ///
    /// Mutant: hard-code the window (`age > 1_000_000_000` in
    /// `OffsetTable::observe`), which leaves the threshold half passing —
    /// applied, and this failed at `a 10 ms window cannot correlate steps 50 ms
    /// apart: Halt { reason: ClockReset { delta_nanos: -5000000000, evidence:
    /// CommonMode { publishers: 2 } } }`, `left: true, right: false`.
    #[test]
    fn the_clock_policy_knobs_reach_the_pipeline() {
        // --- reset_threshold_nanos --------------------------------------------
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

        // --- correlation_window_nanos -----------------------------------------
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
    /// A halt crosses the C ABI as a POD with one `(parent, child)` pair and a
    /// free-text `detail`, so the count of agreeing publishers is the whole of
    /// the evidence an operator gets. This pins that `CommonMode` carries a real
    /// count rather than a constant, by making a third publisher join.
    ///
    /// Mutant: `publishers: 2` hard-coded in `OffsetTable::observe`'s return —
    /// applied, and this failed at `left: Some(CommonMode { delta_nanos:
    /// -5000000000, publishers: 2 }), right: Some(CommonMode { delta_nanos:
    /// -5000000000, publishers: 3 })`, which is the difference between "two
    /// publishers coincided" and "the whole robot moved".
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
    /// **A cached spelling still counts every stripped slash.**
    ///
    /// `Ingest::resolve` skips `NameNormalizer::normalize` for a raw pair it has
    /// seen before, and §5.9's stripped-slash figure counts every *occurrence*
    /// rather than every first sight — so it is the one thing that skip would
    /// silently freeze. `resolve` replays it through
    /// `NameNormalizer::note_stripped`, and this is the assertion that the
    /// replay is exact rather than approximately right.
    ///
    /// Mutant: delete the `note_stripped` call from `Ingest::resolve` — applied,
    /// and this failed with `2 != 200`: the first sighting still counted, and
    /// every one of the ninety-nine repeats after it vanished.
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
    /// The config declares `odom`; the wire sends `/odom`. It is the *first*
    /// `normalize` of `/odom` that appends `("/odom", "odom")` to `remaps()`,
    /// and that row crosses the C ABI as `tft_bridge_get_remap` and is what
    /// §5.6's "log the resulting mapping table at startup" prints. A cache
    /// filled at construction from the declared names would never call
    /// `normalize` on the slashed spelling and the row would vanish with no
    /// error anywhere.
    ///
    /// The second half — that a hundred repeats add no second row — is what says
    /// the cache is actually being taken; without it this test would pass on an
    /// implementation that never cached at all.
    ///
    /// Mutant: pre-seed `raw` in `Ingest::with_policies` by inserting every
    /// declared pair under its own spelling *and* its slashed one — applied, and
    /// this failed on the `remaps` assertion with an empty table.
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
    /// This is the invariant every `Vec` index in the pipeline rests on: the
    /// clock guards, the kinds, the static values and the conflict counters are
    /// all addressed by it, and `Action::Publish`'s names come back through it.
    /// A slot that named the wrong edge would attribute one publisher's
    /// transform to another with nothing anywhere reporting it.
    ///
    /// Mutant: in `StaticStore::slot_or_insert`, push to `kinds`/`values` before
    /// taking `self.index.len()` as the slot — applied, and this failed at the
    /// first edge with an off-by-one that put every edge's kind one slot late.
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
    /// `Ingest::offer` resolves an edge once and then addresses *both* tables
    /// with the resulting slot — the kind from one, the owner from the other.
    /// They are separate structures that happen to be seeded from the same
    /// `config.edges` in the same order, and nothing in the type system says so.
    /// If they ever disagreed, a transform would be checked against one edge's
    /// kind and written under another edge's owner, with no error anywhere: the
    /// silent misattribution this whole indexing scheme exists to avoid.
    ///
    /// Mutant: seed `Authority` from `config.edges.iter().rev()` — applied, and
    /// this failed at the first edge with `("base", "gps")` where `("map",
    /// "odom")` was expected.
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
