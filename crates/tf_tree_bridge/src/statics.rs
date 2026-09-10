//! Static-transform semantics — `docs/PHASE4.md` §5.7.
//!
//! # `/tf_static` repeats itself, and most repeats are not a problem
//!
//! Latched topics re-deliver to every late joiner, so the bridge sees the same
//! static transform many times. §5.7 splits that into three cases, and the
//! interesting one is the middle:
//!
//! * **Identical value** (bitwise, or within 1e-12) — idempotent, ignore
//!   silently. This is the normal case and logging it would bury the other two.
//! * **Different value** — a diagnostic naming both publishers *and both
//!   values*, then the authority policy. Two `robot_state_publisher` instances
//!   with different URDFs is a real and common misconfiguration, and it is
//!   invisible in `tf2`: whichever arrived last wins, silently, and the winner
//!   changes when the launch order does.
//! * **A kind change** — a transform arriving on `/tf_static` for an edge
//!   already declared *dynamic*, or the reverse. A hard error: the edge kind
//!   cannot change, and an arena where it did would have a ring behind an edge
//!   that consumers treat as constant.
//!
//! # Why 1e-12 and not bitwise
//!
//! §5.7 says "bitwise, or within 1e-12". Bitwise alone would report a conflict
//! every time a URDF was re-parsed by a different version of the same parser, or
//! a value round-tripped through YAML — differences of one ulp that no consumer
//! could observe. The tolerance is what makes the diagnostic mean "your two
//! URDFs disagree" rather than "your two URDFs were serialized differently".

use crate::config::{EdgeShape, TopologyConfig};
use crate::edgeindex::{EdgeIndex, EdgeSlot};
use crate::Publisher;

/// Which topic an edge was declared from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaticKind {
    /// Declared from `/tf_static`.
    Static,
    /// Declared from `/tf`.
    Dynamic,
}

/// What to do with a `/tf_static` sample.
#[derive(Clone, Debug, PartialEq)]
pub enum StaticVerdict {
    /// First time; declare it.
    Declare,
    /// The same value again. Ignore, silently.
    Idempotent,
    /// A different value for an already-declared static edge.
    Conflict {
        /// Who declared it first.
        owner: Publisher,
        /// Who is contradicting them.
        intruder: Publisher,
        /// The value on file.
        existing: [f64; 7],
        /// The value just offered. §5.7 requires **both** to be reported: an
        /// operator with two URDFs needs to know which one is installed, and a
        /// message naming only the publishers does not tell them.
        offered: [f64; 7],
        /// First occurrence of this exact conflict, for rate limiting.
        first_time: bool,
    },
    /// The edge is already declared with the other kind. **Hard error.**
    KindChanged {
        /// What it was declared as.
        declared: StaticKind,
    },
}

/// Tracks declared edges and their static values.
///
/// # One index, four parallel vectors
///
/// The `(parent, child)` tables used to be `ByEdge` — nested, so a probe
/// allocated nothing. That solved allocation and left the *work*: two
/// `BTreeMap<String, _>::get`s per probe, each `O(log n)` with a full
/// frame-name `memcmp` at every visited node, and `Ingest::offer` probed this
/// store **twice** per transform with the same key.
///
/// The declared set is fixed at construction, so it is answered once by
/// `EdgeIndex` and every table becomes a `Vec` indexed by the resulting
/// `EdgeSlot`. See `crate::edgeindex` for the measurement that motivated it.
///
/// The store still grows when it is built unseeded — `tf_tree_ingest` uses it
/// that way, discovering edges from a recording rather than from a config — so
/// the index is a growing table and not a perfect hash.
#[derive(Debug, Default)]
pub struct StaticStore {
    /// `(parent, child)` → the slot every vector below is indexed by.
    index: EdgeIndex<EdgeSlot>,
    kinds: Vec<StaticKind>,
    /// `None` for a dynamic edge, which has no declared constant.
    values: Vec<Option<([f64; 7], Publisher)>>,
    /// Conflicts already reported per edge, so the diagnostic is rate-limited by
    /// identity. A `Vec` now rather than a keyed map: the slot is already in
    /// hand at the one place it is read, so the two owned `String`s that probe
    /// used to build are gone from the conflict path entirely.
    reported: Vec<u64>,
    /// The **intruder** of an edge's first conflict, kept so
    /// [`StaticStore::conflicts_by_edge`] can name both publishers the way
    /// [`crate::Authority::conflicts`] does. `None` until an edge is
    /// contradicted; `values[slot]` already holds the owner.
    ///
    /// **One clone per edge, ever**, not one per observation: it is written only
    /// where `reported[slot]` goes from 0 to 1. The reason `reported` is a `Vec`
    /// rather than a keyed map is that the conflict path must not allocate two
    /// `String`s per sample, and this keeps that property — a latched static
    /// redelivered to a hundred late joiners clones nothing.
    ///
    /// **Why the intruder and not the whole verdict.** The owner and the declared
    /// pose are already in `values`; the offered pose is *not* kept, because
    /// §5.4's clause asks for the edge and its two publishers, and a second pose
    /// per edge would be state nothing reads.
    first_intruder: Vec<Option<Publisher>>,
    conflicts: u64,
}

/// How far two static poses may differ and still be "the same" (§5.7).
pub const STATIC_EPS: f64 = 1e-12;

impl StaticStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> StaticStore {
        StaticStore::default()
    }

    /// A store pre-loaded with a topology config's declarations.
    ///
    /// This is what `docs/PHASE4.md` §5.8's amendment means by *"reinterpreted
    /// as verify against the declared constant"*. Every static edge's value is
    /// on file **before** any message arrives, owned by
    /// [`Publisher::Declared`], so an arriving `/tf_static` runs the same
    /// [`Self::observe_static`] it always did and lands in the same three
    /// buckets — with the config as the incumbent. `/tf_static` handling
    /// becomes §5.7's conflict machinery and nothing else, which is what that
    /// machinery was for.
    #[must_use]
    pub fn seeded(config: &TopologyConfig) -> StaticStore {
        let mut s = StaticStore {
            index: EdgeIndex::with_capacity(config.edges.len()),
            ..StaticStore::default()
        };
        for e in &config.edges {
            let (parent, child) = e.key();
            match e.shape {
                EdgeShape::Static { pose } => {
                    let slot = s.slot_or_insert(parent, child, StaticKind::Static);
                    s.kinds[slot.get()] = StaticKind::Static;
                    s.values[slot.get()] = Some((pose, Publisher::Declared));
                }
                EdgeShape::Dynamic { .. } => {
                    let slot = s.slot_or_insert(parent, child, StaticKind::Dynamic);
                    s.kinds[slot.get()] = StaticKind::Dynamic;
                }
            }
        }
        s
    }

    /// Whether the topology declares this edge at all.
    ///
    /// A seeded store answers `false` for everything the config did not name,
    /// which is what makes §5.8's *"a transform arriving for an undeclared edge
    /// is dropped, counted and diagnosed"* a lookup rather than a second table.
    #[must_use]
    pub fn is_declared(&self, parent: &str, child: &str) -> bool {
        self.index.get(parent, child).is_some()
    }

    /// Record that `(parent, child)` arrived on `/tf` — a dynamic edge.
    ///
    /// Returns `Err` with the declared kind if it was already static.
    ///
    /// # Errors
    ///
    /// [`StaticKind::Static`] if the edge is already a static one.
    pub fn observe_dynamic(&mut self, parent: &str, child: &str) -> Result<(), StaticKind> {
        match self.index.get(parent, child) {
            Some(slot) if self.kinds[slot.get()] == StaticKind::Static => Err(StaticKind::Static),
            Some(_) => Ok(()),
            // The only allocating arm, and it runs once per edge ever.
            None => {
                self.slot_or_insert(parent, child, StaticKind::Dynamic);
                Ok(())
            }
        }
    }

    /// Classify a `/tf_static` sample.
    pub fn observe_static(
        &mut self,
        parent: &str,
        child: &str,
        pose: [f64; 7],
        publisher: &Publisher,
    ) -> StaticVerdict {
        let slot = self.slot_or_insert(parent, child, StaticKind::Static);
        self.observe_static_at(slot, pose, publisher)
    }

    /// [`Self::observe_static`] for a caller that already holds the slot.
    ///
    /// The whole of §5.7's machinery, with the two name probes removed. A slot
    /// exists only for an edge the store knows, which is what makes the
    /// `Declare` arm below reachable *only* from an unseeded store — a seeded
    /// one has a value on file for every static edge before any message arrives.
    pub(crate) fn observe_static_at(
        &mut self,
        slot: EdgeSlot,
        pose: [f64; 7],
        publisher: &Publisher,
    ) -> StaticVerdict {
        if self.kinds[slot.get()] == StaticKind::Dynamic {
            return StaticVerdict::KindChanged {
                declared: StaticKind::Dynamic,
            };
        }
        let Some((existing, owner)) = &self.values[slot.get()] else {
            self.kinds[slot.get()] = StaticKind::Static;
            self.values[slot.get()] = Some((pose, publisher.clone()));
            return StaticVerdict::Declare;
        };
        if same_pose(existing, &pose) {
            return StaticVerdict::Idempotent;
        }
        let (existing, owner) = (*existing, owner.clone());
        // The conflict path, and it no longer builds a key to get here: the slot
        // indexes the counter directly.
        let seen = &mut self.reported[slot.get()];
        let first_time = *seen == 0;
        *seen += 1;
        if first_time {
            // The one clone this path ever makes for a given edge. See
            // `first_intruder`.
            self.first_intruder[slot.get()] = Some(publisher.clone());
        }
        self.conflicts += 1;
        StaticVerdict::Conflict {
            owner,
            intruder: publisher.clone(),
            existing,
            offered: pose,
            first_time,
        }
    }

    /// The slot for `(parent, child)`, creating it with `kind` if it is new.
    ///
    /// The one allocating path, and it runs once per edge ever. Every vector is
    /// extended in lockstep with the index so a slot is always in range of all
    /// four — the invariant every `self.kinds[slot.get()]` below rests on.
    fn slot_or_insert(&mut self, parent: &str, child: &str, kind: StaticKind) -> EdgeSlot {
        if let Some(slot) = self.index.get(parent, child) {
            return slot;
        }
        let e = self.index.len();
        let slot = EdgeSlot(u32::try_from(e).unwrap_or(u32::MAX));
        self.index.insert(parent, child, slot);
        self.kinds.push(kind);
        self.values.push(None);
        self.reported.push(0);
        self.first_intruder.push(None);
        slot
    }

    /// The slot for `(parent, child)`, or `None` if the store does not know it.
    pub(crate) fn resolve(&self, parent: &str, child: &str) -> Option<EdgeSlot> {
        self.index.get(parent, child)
    }

    /// A slot's declared kind.
    pub(crate) fn kind_at(&self, slot: EdgeSlot) -> StaticKind {
        self.kinds[slot.get()]
    }

    /// A slot's `(parent, child)`, so a caller holding only an index can still
    /// name the edge in an `Action`.
    pub(crate) fn names_of(&self, slot: EdgeSlot) -> (&str, &str) {
        self.index.key(slot.get())
    }

    /// How many edges the store knows — the length every parallel `Vec` has.
    pub(crate) fn slots(&self) -> usize {
        self.kinds.len()
    }

    /// Static conflicts seen (§5.9).
    ///
    /// **Observations, not faults** — see [`StaticStore::conflicts_by_edge`] for
    /// why the two differ and which one a report should quote.
    #[must_use]
    pub fn conflicts(&self) -> u64 {
        self.conflicts
    }

    /// Every static edge this store has recorded a conflict on, as
    /// `(parent, child, owner, intruder, count)` — **the same shape
    /// [`crate::Authority::conflicts`] yields**, deliberately, because
    /// `docs/PHASE4.md` §5.4 asks one thing of both halves and a caller building
    /// that report should not meet two shapes.
    ///
    /// **The iterator's length is the fault count; the `u64` beside each edge is
    /// how loud that one fault was.** `/tf_static` is `transient_local`, so a
    /// misconfigured publisher's latched sample is re-delivered to every late
    /// joiner: [`StaticStore::conflicts`] counts ten redeliveries of one
    /// misconfiguration as ten, and a startup report quoting it would send an
    /// operator looking for ten faults. That is the same distinction §5.4's
    /// amendment draws about *when* a static conflict is observed being a DDS
    /// discovery artefact rather than a fault time.
    ///
    /// **Both publishers, because §5.4:1403 is normative that they are named:**
    /// *"the seam's `detail` enumerates **every** recorded edge with both of its
    /// publishers, not the first."* The first revision of this accessor yielded
    /// `(parent, child, count)` and could not satisfy that clause — and the data
    /// was not recoverable afterwards either, since `values[slot]` holds only the
    /// owner. Naming the edge without naming who disagreed about it is a report
    /// an operator cannot act on: the whole fault *is* which two nodes disagree.
    ///
    /// [`docs/decisions/0011`](../../../docs/decisions/0011-the-bridge-clock-guard-and-the-static-conflict-disposition.md)'s
    /// implementation step 5 named this accessor and **it did not land with the
    /// rest of that step.** `Ingest` stood in for it with a private counter
    /// incremented off the `first_time` flag, which could count the faults but
    /// could not name them at all.
    ///
    /// Reading `reported` is exactly equivalent to the counter it replaces rather
    /// than merely close to it, and the reason is that `Strict` closes its window
    /// **once**: at the close, a non-zero `reported[slot]` is a conflict seen
    /// before the close, which is what the counter accumulated. After the close
    /// `Strict` has degraded and nothing reads this for a halt.
    pub fn conflicts_by_edge(
        &self,
    ) -> impl Iterator<Item = (&str, &str, &Publisher, &Publisher, u64)> {
        // **Membership is `first_intruder`, and the count is `reported`.** An
        // earlier revision filtered on `reported[slot] > 0` *and* then reached
        // for the publishers with `?`, and the `reported` filter turned out to be
        // dead: the two are written in the same breath, so `filter_map` already
        // dropped every slot the filter would have. A predicate no mutation can
        // distinguish is the vacuity smell `docs/PROJECT.md` §6 names, so there is
        // one predicate now — "an intruder was recorded for this edge" — and it
        // is the one that also makes the publishers available.
        //
        // `?` on the owner rather than an `expect`: the conflict arm reaches
        // `values[slot]` to find the owner it compares against, so it is `Some`
        // wherever an intruder is, and a panic in a diagnostic accessor would
        // take down the bridge that was reporting the misconfiguration.
        self.first_intruder
            .iter()
            .enumerate()
            .filter_map(|(slot, intruder)| {
                let intruder = intruder.as_ref()?;
                let (_, owner) = self.values[slot].as_ref()?;
                let (parent, child) = self.index.key(slot);
                Some((parent, child, owner, intruder, self.reported[slot]))
            })
    }

    /// The declared kind of an edge, if any.
    #[must_use]
    pub fn kind_of(&self, parent: &str, child: &str) -> Option<StaticKind> {
        self.index.get(parent, child).map(|s| self.kinds[s.get()])
    }
}

/// Whether two static poses are "the same" for §5.7's purposes.
///
/// Compares **quaternion and translation separately against the same absolute
/// tolerance**, and treats `q` and `−q` as equal — they are the same rotation,
/// and a publisher that re-derives its quaternion from a matrix will hand back
/// whichever sign its conversion produces. Reporting that as a URDF
/// disagreement would be a false alarm on a correct system, which is the one
/// thing a conflict detector must not do.
fn same_pose(a: &[f64; 7], b: &[f64; 7]) -> bool {
    // Non-finite never compares equal: a NaN in either is a fault to report,
    // not a value to match. Without this, `NaN != NaN` would make an edge
    // conflict with *itself* forever.
    if !a.iter().chain(b.iter()).all(|v| v.is_finite()) {
        return false;
    }
    let dot: f64 = (0..4).map(|i| a[i] * b[i]).sum();
    let sign = if dot < 0.0 { -1.0 } else { 1.0 };
    let rot_ok = (0..4).all(|i| (a[i] - sign * b[i]).abs() <= STATIC_EPS);
    let trans_ok = (4..7).all(|i| (a[i] - b[i]).abs() <= STATIC_EPS);
    rot_ok && trans_ok
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn node(n: &str) -> Publisher {
        Publisher::named(&crate::gid_for_name(n), n)
    }
    const ID: [f64; 7] = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];

    /// **A latched re-delivery is silent.** This is the normal case; logging it
    /// would bury the two that matter.
    #[test]
    fn an_identical_repeat_is_idempotent() {
        let mut s = StaticStore::new();
        assert_eq!(
            s.observe_static("base", "lidar", ID, &node("/rsp")),
            StaticVerdict::Declare
        );
        for _ in 0..100 {
            assert_eq!(
                s.observe_static("base", "lidar", ID, &node("/rsp")),
                StaticVerdict::Idempotent
            );
        }
        // A *different* publisher offering the same value is also idempotent —
        // two robot_state_publishers with the same URDF is a redundant launch
        // file, not a misconfiguration, and reporting it would train operators
        // to ignore the message.
        assert_eq!(
            s.observe_static("base", "lidar", ID, &node("/rsp2")),
            StaticVerdict::Idempotent
        );
        assert_eq!(s.conflicts(), 0);
    }

    /// **A different value names both publishers and both values.**
    ///
    /// Two `robot_state_publisher`s with different URDFs. In `tf2` whichever
    /// arrived last wins, silently, and the winner changes when the launch
    /// order does.
    ///
    /// Mutant: drop `existing`/`offered` from the verdict ⇒ an operator learns
    /// there is a conflict but not which URDF is installed, which is the only
    /// actionable half.
    #[test]
    fn a_differing_value_reports_both_sides() {
        let mut s = StaticStore::new();
        let mut moved = ID;
        moved[4] = 0.25; // 25 cm along x
        s.observe_static("base", "lidar", ID, &node("/rsp_a"));
        match s.observe_static("base", "lidar", moved, &node("/rsp_b")) {
            StaticVerdict::Conflict {
                owner,
                intruder,
                existing,
                offered,
                first_time,
            } => {
                assert_eq!(owner, node("/rsp_a"));
                assert_eq!(intruder, node("/rsp_b"));
                assert_eq!(existing, ID);
                assert_eq!(offered, moved);
                assert!(first_time);
            }
            other => panic!("{other:?}"),
        }
        // Rate-limited: loud once, counted thereafter.
        for _ in 0..10 {
            match s.observe_static("base", "lidar", moved, &node("/rsp_b")) {
                StaticVerdict::Conflict { first_time, .. } => assert!(!first_time),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(s.conflicts(), 11);
    }

    /// **`q` and `−q` are the same rotation**, and a publisher that re-derives
    /// its quaternion from a matrix hands back whichever sign the conversion
    /// produced. Reporting that as a URDF disagreement is a false alarm on a
    /// correct system, which is the one failure a conflict detector cannot
    /// afford.
    ///
    /// Mutant: compare componentwise without the sign fold ⇒ every such
    /// re-delivery becomes a conflict.
    #[test]
    fn a_negated_quaternion_is_not_a_conflict() {
        let mut s = StaticStore::new();
        let q: [f64; 7] = [0.5, 0.5, 0.5, 0.5, 1.0, 2.0, 3.0];
        let neg: [f64; 7] = [-0.5, -0.5, -0.5, -0.5, 1.0, 2.0, 3.0];
        s.observe_static("a", "b", q, &node("/x"));
        assert_eq!(
            s.observe_static("a", "b", neg, &node("/y")),
            StaticVerdict::Idempotent
        );
        // ...but the *translation* is not sign-folded, because −t is a
        // different place.
        let flipped_t: [f64; 7] = [-0.5, -0.5, -0.5, -0.5, -1.0, 2.0, 3.0];
        assert!(matches!(
            s.observe_static("a", "b", flipped_t, &node("/y")),
            StaticVerdict::Conflict { .. }
        ));
    }

    /// **One ulp is not a disagreement.** A URDF re-parsed by a different
    /// version of the same parser, or round-tripped through YAML, differs in
    /// the last bit and no consumer can observe it.
    #[test]
    fn a_one_ulp_difference_is_within_tolerance() {
        let mut s = StaticStore::new();
        let a: [f64; 7] = [1.0, 0.0, 0.0, 0.0, 0.3, 0.0, 0.0];
        let mut b = a;
        b[4] = f64::from_bits(a[4].to_bits() + 1);
        assert_ne!(a[4], b[4], "the fixture must actually differ");
        s.observe_static("p", "c", a, &node("/x"));
        assert_eq!(
            s.observe_static("p", "c", b, &node("/x")),
            StaticVerdict::Idempotent
        );
        // A millimetre, however, is a real disagreement.
        let mut mm = a;
        mm[4] += 0.001;
        assert!(matches!(
            s.observe_static("p", "c", mm, &node("/x")),
            StaticVerdict::Conflict { .. }
        ));
    }

    /// **NaN never matches, including itself.**
    ///
    /// Without the finiteness guard, `NaN != NaN` makes an edge conflict with
    /// its own stored value on every re-delivery — an infinite stream of
    /// diagnostics about a single bad message.
    #[test]
    fn a_non_finite_pose_is_a_conflict_not_a_match() {
        let mut s = StaticStore::new();
        let mut nan = ID;
        nan[4] = f64::NAN;
        s.observe_static("p", "c", ID, &node("/x"));
        assert!(matches!(
            s.observe_static("p", "c", nan, &node("/x")),
            StaticVerdict::Conflict { .. }
        ));
    }

    /// **The edge kind cannot change**, in either direction.
    ///
    /// An arena where it did would have a ring behind an edge that consumers
    /// treat as constant.
    #[test]
    fn an_edge_cannot_change_kind_in_either_direction() {
        let mut s = StaticStore::new();
        s.observe_dynamic("odom", "base").unwrap();
        assert_eq!(
            s.observe_static("odom", "base", ID, &node("/x")),
            StaticVerdict::KindChanged {
                declared: StaticKind::Dynamic
            }
        );
        // ...and a static edge refuses to become dynamic.
        let mut s2 = StaticStore::new();
        s2.observe_static("base", "lidar", ID, &node("/x"));
        assert_eq!(s2.observe_dynamic("base", "lidar"), Err(StaticKind::Static));
        // A kind change must not overwrite the declaration.
        assert_eq!(s2.kind_of("base", "lidar"), Some(StaticKind::Static));
    }

    /// **`conflicts_by_edge` names the edges; `conflicts` only counts
    /// observations — and the gap between the two is the whole reason the
    /// accessor exists.**
    ///
    /// `docs/PHASE4.md` §5.4's amendment is normative that a `Strict` startup
    /// halt's `detail` "enumerates **every** recorded edge with both of its
    /// publishers, not the first". Until this accessor landed the static half of
    /// that had no way to be enumerated at all: `Ingest` kept a private `u32` of
    /// distinct edges, which could say *how many* and never *which*.
    ///
    /// The fixture keeps the two numbers apart on purpose — 2 contradicted edges
    /// against 7 conflicting observations — so a substitution of one for the
    /// other cannot pass by coincidence, the same shape
    /// `ingest::tests::the_startup_halt_counts_faults_not_observations` uses one
    /// level up.
    ///
    /// Mutant (applied, confirmed fatal): write `first_intruder[slot]` on every
    /// conflict rather than only the first — `base -> cam`'s intruder then reads
    /// `/latecomer` instead of `/intruder`, the publisher that opened the fault.
    /// The fixture puts two distinct intruders on one edge for exactly that.
    ///
    /// Mutant (applied, confirmed fatal): set `first_intruder[slot]` in
    /// `slot_for`, where every other parallel vector is grown — the two
    /// never-contradicted edges then appear, and this fails at 5 entries
    /// against 3. Fatal to five tests: the other four are `ingest`'s
    /// startup-window tests, which is the cross-check that the halt reads this
    /// accessor rather than a ledger of its own.
    ///
    /// **Mutant (applied, SURVIVED, and the code changed rather than the note):**
    /// an earlier revision of the accessor filtered on `reported[slot] > 0`
    /// before reaching for the publishers with `?`. Dropping that filter was
    /// fatal to five tests *before* the publishers were added and to none after
    /// — `filter_map` already dropped every slot the filter would have, because
    /// the two are written in the same breath. The filter is gone; membership is
    /// the intruder. A predicate no mutation can distinguish is not a predicate.
    #[test]
    fn conflicts_by_edge_names_every_contradicted_edge_its_publishers_and_no_others() {
        const OTHER: [f64; 7] = [1.0, 0.0, 0.0, 0.0, 9.0, 0.0, 0.0];
        const THIRD: [f64; 7] = [1.0, 0.0, 0.0, 0.0, 0.0, 7.0, 0.0];
        let mut s = StaticStore::new();

        // Two edges that are declared and never contradicted. They must not
        // appear: a report that named them would send an operator to look at
        // correct configuration.
        assert_eq!(
            s.observe_static("base", "lidar", ID, &node("/rsp")),
            StaticVerdict::Declare
        );
        assert_eq!(
            s.observe_static("base", "imu", ID, &node("/rsp")),
            StaticVerdict::Declare
        );
        for _ in 0..10 {
            assert_eq!(
                s.observe_static("base", "lidar", ID, &node("/rsp2")),
                StaticVerdict::Idempotent
            );
        }

        // Two that are, at different loudnesses: a latched static redelivered to
        // five late joiners is five observations of one misconfiguration.
        assert_eq!(
            s.observe_static("base", "cam", ID, &node("/rsp")),
            StaticVerdict::Declare
        );
        assert_eq!(
            s.observe_static("base", "arm", ID, &node("/rsp")),
            StaticVerdict::Declare
        );
        for _ in 0..5 {
            assert!(matches!(
                s.observe_static("base", "cam", OTHER, &node("/intruder")),
                StaticVerdict::Conflict { .. }
            ));
        }
        // A *second* intruder on the same edge. The recorded one must stay the
        // publisher that opened the fault — that is the one whose launch file
        // changed — and `first_intruder` is written once for that reason.
        assert!(matches!(
            s.observe_static("base", "cam", THIRD, &node("/latecomer")),
            StaticVerdict::Conflict { .. }
        ));
        for _ in 0..2 {
            assert!(matches!(
                s.observe_static("base", "arm", OTHER, &node("/intruder2")),
                StaticVerdict::Conflict { .. }
            ));
        }
        // And one contradicted **exactly once**, which is the boundary: an
        // off-by-one in the membership predicate drops precisely this row, and a
        // fixture whose smallest count is 2 could not see that.
        assert_eq!(
            s.observe_static("base", "gps", ID, &node("/rsp")),
            StaticVerdict::Declare
        );
        assert!(matches!(
            s.observe_static("base", "gps", OTHER, &node("/intruder3")),
            StaticVerdict::Conflict { .. }
        ));

        // The count that already existed sees nine observations...
        assert_eq!(s.conflicts(), 9, "observations");

        // ...and the accessor sees two faults, names them, and names who
        // disagreed — which is what §5.4:1403 asks for and what the private
        // counter this replaced structurally could not do.
        let mut found: Vec<(String, String, Publisher, Publisher, u64)> = s
            .conflicts_by_edge()
            .map(|(p, c, owner, intruder, n)| {
                (
                    p.to_owned(),
                    c.to_owned(),
                    owner.clone(),
                    intruder.clone(),
                    n,
                )
            })
            .collect();
        // Sorted on the edge only: `Publisher` is not `Ord`, and the edge is
        // what makes a row identifiable anyway.
        found.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        assert_eq!(
            found,
            vec![
                (
                    "base".to_owned(),
                    "arm".to_owned(),
                    node("/rsp"),
                    node("/intruder2"),
                    2
                ),
                (
                    "base".to_owned(),
                    "cam".to_owned(),
                    node("/rsp"),
                    node("/intruder"),
                    6
                ),
                (
                    "base".to_owned(),
                    "gps".to_owned(),
                    node("/rsp"),
                    node("/intruder3"),
                    1
                ),
            ],
            "every contradicted edge with both of its publishers and how loud it \
             was — neither of the two that never conflicted, and `cam`'s intruder \
             is the publisher that opened the fault rather than the latecomer"
        );
    }
}
