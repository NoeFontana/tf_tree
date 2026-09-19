//! Static-transform semantics — `docs/PHASE4.md` §5.7.
//!
//! `/tf_static` is latched and re-delivered to late joiners. §5.7's three cases:
//!
//! * **Identical value** (bitwise, or within 1e-12): idempotent, silent. The
//!   tolerance keeps one-ulp re-serialization differences from reading as a
//!   URDF disagreement.
//! * **Different value**: a diagnostic naming both publishers *and both values*,
//!   then the authority policy.
//! * **A kind change** (static vs dynamic for one edge): a hard error.

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
        /// The value just offered. §5.7 requires **both** values reported.
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
/// Every table is a `Vec` indexed by the `EdgeSlot` that `EdgeIndex` answers once
/// per `(parent, child)` (see `crate::edgeindex`). Built unseeded (as
/// `tf_tree_ingest` does) the index grows.
#[derive(Debug, Default)]
pub struct StaticStore {
    /// `(parent, child)` → the slot every vector below is indexed by.
    index: EdgeIndex<EdgeSlot>,
    kinds: Vec<StaticKind>,
    /// `None` for a dynamic edge, which has no declared constant.
    values: Vec<Option<([f64; 7], Publisher)>>,
    /// Conflicts already reported per edge, for rate limiting.
    reported: Vec<u64>,
    /// The **intruder** of an edge's first conflict, for
    /// [`StaticStore::conflicts_by_edge`]; `values[slot]` holds the owner. Written
    /// only where `reported[slot]` goes 0 to 1: one clone per edge, ever.
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

    /// A store pre-loaded with a topology config's declarations (§5.8's
    /// amendment): every static edge's value is on file, owned by
    /// [`Publisher::Declared`], before any message arrives, so `/tf_static` is
    /// §5.7's conflict machinery with the config as incumbent.
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

    /// Whether the topology declares this edge at all (§5.8's undeclared-edge
    /// check).
    #[must_use]
    pub fn is_declared(&self, parent: &str, child: &str) -> bool {
        self.index.get(parent, child).is_some()
    }

    /// Record that `(parent, child)` arrived on `/tf` — a dynamic edge.
    ///
    /// # Errors
    ///
    /// [`StaticKind::Static`] if the edge is already a static one.
    pub fn observe_dynamic(&mut self, parent: &str, child: &str) -> Result<(), StaticKind> {
        match self.index.get(parent, child) {
            Some(slot) if self.kinds[slot.get()] == StaticKind::Static => Err(StaticKind::Static),
            Some(_) => Ok(()),
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

    /// [`Self::observe_static`] for a caller that already holds the slot. `Declare`
    /// is reachable only from an unseeded store.
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
        let seen = &mut self.reported[slot.get()];
        let first_time = *seen == 0;
        *seen += 1;
        if first_time {
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

    /// The slot for `(parent, child)`, creating it with `kind` if new. Every
    /// vector is extended in lockstep, so a slot is always in range of all four.
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

    /// A slot's `(parent, child)`.
    pub(crate) fn names_of(&self, slot: EdgeSlot) -> (&str, &str) {
        self.index.key(slot.get())
    }

    /// How many edges the store knows.
    pub(crate) fn slots(&self) -> usize {
        self.kinds.len()
    }

    /// Static conflict **observations** (§5.9); see [`StaticStore::conflicts_by_edge`]
    /// for the fault count.
    #[must_use]
    pub fn conflicts(&self) -> u64 {
        self.conflicts
    }

    /// Every static edge with a recorded conflict, as
    /// `(parent, child, owner, intruder, count)` — the shape
    /// [`crate::Authority::conflicts`] yields (§5.4 requires both publishers
    /// named for **every** recorded edge).
    ///
    /// The iterator's length is the fault count; the count beside each edge is
    /// how many times it was observed. `/tf_static` is `transient_local`, so one
    /// misconfiguration redelivered to ten late joiners is ten observations in
    /// [`StaticStore::conflicts`] and one fault here.
    ///
    /// Reading `reported` equals the old counter because `Strict` closes its
    /// window once.
    pub fn conflicts_by_edge(
        &self,
    ) -> impl Iterator<Item = (&str, &str, &Publisher, &Publisher, u64)> {
        // Membership is `first_intruder`; `?` on the owner rather than a panic in
        // a diagnostic accessor.
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
/// Quaternion and translation are compared separately against `STATIC_EPS`, and
/// `q` and `-q` are equal (same rotation, sign depends on the conversion).
fn same_pose(a: &[f64; 7], b: &[f64; 7]) -> bool {
    // Non-finite never matches: otherwise `NaN != NaN` conflicts with itself forever.
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

    /// A latched re-delivery is silent.
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
        // A different publisher offering the same value is also idempotent.
        assert_eq!(
            s.observe_static("base", "lidar", ID, &node("/rsp2")),
            StaticVerdict::Idempotent
        );
        assert_eq!(s.conflicts(), 0);
    }

    /// A different value names both publishers and both values.
    ///
    /// Mutant: drop `existing`/`offered` from the verdict.
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

    /// `q` and `-q` are the same rotation.
    ///
    /// Mutant: compare componentwise without the sign fold.
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
        // The translation is not sign-folded.
        let flipped_t: [f64; 7] = [-0.5, -0.5, -0.5, -0.5, -1.0, 2.0, 3.0];
        assert!(matches!(
            s.observe_static("a", "b", flipped_t, &node("/y")),
            StaticVerdict::Conflict { .. }
        ));
    }

    /// One ulp is not a disagreement.
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
        let mut mm = a;
        mm[4] += 0.001;
        assert!(matches!(
            s.observe_static("p", "c", mm, &node("/x")),
            StaticVerdict::Conflict { .. }
        ));
    }

    /// NaN never matches, including itself.
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

    /// The edge kind cannot change, in either direction.
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
        let mut s2 = StaticStore::new();
        s2.observe_static("base", "lidar", ID, &node("/x"));
        assert_eq!(s2.observe_dynamic("base", "lidar"), Err(StaticKind::Static));
        assert_eq!(s2.kind_of("base", "lidar"), Some(StaticKind::Static));
    }

    /// `conflicts_by_edge` names 3 contradicted edges against 9 observations, so
    /// neither can be substituted for the other (§5.4).
    ///
    /// Mutant: write `first_intruder[slot]` on every conflict, not only the first
    /// (`cam`'s intruder then reads `/latecomer`).
    ///
    /// Mutant: set `first_intruder[slot]` in `slot_or_insert` (never-contradicted
    /// edges then appear: 5 entries against 3).
    #[test]
    fn conflicts_by_edge_names_every_contradicted_edge_its_publishers_and_no_others() {
        const OTHER: [f64; 7] = [1.0, 0.0, 0.0, 0.0, 9.0, 0.0, 0.0];
        const THIRD: [f64; 7] = [1.0, 0.0, 0.0, 0.0, 0.0, 7.0, 0.0];
        let mut s = StaticStore::new();

        // Two edges never contradicted; they must not appear.
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
        // A second intruder: the recorded one stays the publisher that opened the fault.
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
        // One contradicted exactly once: the membership boundary.
        assert_eq!(
            s.observe_static("base", "gps", ID, &node("/rsp")),
            StaticVerdict::Declare
        );
        assert!(matches!(
            s.observe_static("base", "gps", OTHER, &node("/intruder3")),
            StaticVerdict::Conflict { .. }
        ));

        assert_eq!(s.conflicts(), 9, "observations");

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
