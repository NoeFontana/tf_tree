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

use crate::Publisher;
use std::collections::BTreeMap;

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
#[derive(Debug, Default)]
pub struct StaticStore {
    kinds: BTreeMap<(String, String), StaticKind>,
    values: BTreeMap<(String, String), ([f64; 7], Publisher)>,
    reported: BTreeMap<(String, String), u64>,
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

    /// Record that `(parent, child)` arrived on `/tf` — a dynamic edge.
    ///
    /// Returns `Err` with the declared kind if it was already static.
    ///
    /// # Errors
    ///
    /// [`StaticKind::Static`] if the edge is already a static one.
    pub fn observe_dynamic(&mut self, parent: &str, child: &str) -> Result<(), StaticKind> {
        let key = (parent.to_string(), child.to_string());
        match self.kinds.get(&key) {
            Some(StaticKind::Static) => Err(StaticKind::Static),
            Some(StaticKind::Dynamic) => Ok(()),
            None => {
                self.kinds.insert(key, StaticKind::Dynamic);
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
        let key = (parent.to_string(), child.to_string());
        if let Some(StaticKind::Dynamic) = self.kinds.get(&key) {
            return StaticVerdict::KindChanged {
                declared: StaticKind::Dynamic,
            };
        }
        let Some((existing, owner)) = self.values.get(&key) else {
            self.kinds.insert(key.clone(), StaticKind::Static);
            self.values.insert(key, (pose, publisher.clone()));
            return StaticVerdict::Declare;
        };
        if same_pose(existing, &pose) {
            return StaticVerdict::Idempotent;
        }
        let (existing, owner) = (*existing, owner.clone());
        let seen = self.reported.entry(key).or_insert(0);
        let first_time = *seen == 0;
        *seen += 1;
        self.conflicts += 1;
        StaticVerdict::Conflict {
            owner,
            intruder: publisher.clone(),
            existing,
            offered: pose,
            first_time,
        }
    }

    /// Static conflicts seen (§5.9).
    #[must_use]
    pub fn conflicts(&self) -> u64 {
        self.conflicts
    }

    /// The declared kind of an edge, if any.
    #[must_use]
    pub fn kind_of(&self, parent: &str, child: &str) -> Option<StaticKind> {
        self.kinds
            .get(&(parent.to_string(), child.to_string()))
            .copied()
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
        Publisher::Node(n.to_string())
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
}
