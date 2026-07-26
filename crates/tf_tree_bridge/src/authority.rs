//! Authority policy — `docs/PHASE4.md` §5.4, NORMATIVE.
//!
//! # The one place two incompatible models meet
//!
//! ROS permits any number of publishers per edge. `tf_tree` permits exactly one
//! (D7). The bridge is where that is reconciled, and how it is reconciled
//! decides whether a real, common misconfiguration is reported or silently
//! averaged.
//!
//! **`tf2` interleaves competing publishers by timestamp** and produces a
//! transform that is a nonsensical blend of two authorities, with no diagnostic
//! anywhere. §5.4's argument is that being able to say *"your `/ekf` and
//! `/odom_node` have both been publishing `odom→base_link` for eight months"*
//! is a better sales pitch than any latency number — and it is right, because
//! that sentence describes a bug the operator already has and does not know
//! about.
//!
//! So this module's job is not to pick a winner. It is to **notice**, and to
//! name both sides when it does.

use std::collections::BTreeMap;

use crate::Publisher;

/// How to resolve two publishers on one edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthorityPolicy {
    /// The first attributed publisher of an edge owns it; later publishers'
    /// samples are dropped and counted. **The default**, and the only one that
    /// is stable under a flapping node.
    #[default]
    FirstWriterWins,
    /// Reclaim on each new publisher. Available, documented as chaotic, and
    /// never the default: two live publishers make the edge's contents
    /// alternate between two authorities, which is `tf2`'s behaviour with the
    /// blending removed rather than an improvement on it.
    LastWriterWins,
    /// Refuse to start if a conflict appears within a startup window. For CI,
    /// where "this configuration has two publishers on one edge" should fail
    /// the build rather than produce a diagnostic nobody reads.
    Strict,
}

/// What the bridge should do with a sample.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// Publish it.
    Accept,
    /// Drop it. The edge belongs to someone else.
    ///
    /// Carries **both** sides, because §5.4 requires the diagnostic to name
    /// them and a verdict that only named the loser would make that impossible.
    Reject {
        /// Who owns the edge.
        owner: Publisher,
        /// Who tried to write it.
        intruder: Publisher,
        /// Whether this is the first time these two have collided on this edge.
        ///
        /// The bridge rate-limits on this: §5.4 asks for loud *and*
        /// rate-limited, and a per-message log at 1 kHz is neither — it is a
        /// denial of service against the operator's ability to read anything
        /// else.
        first_time: bool,
    },
    /// Under [`AuthorityPolicy::Strict`], a conflict inside the startup window.
    /// The bridge must stop.
    Fatal {
        /// The prior owner.
        owner: Publisher,
        /// The publisher that collided with it.
        intruder: Publisher,
    },
}

/// Per-edge ownership, and the conflicts seen so far.
#[derive(Debug, Default)]
pub struct Authority {
    policy: AuthorityPolicy,
    /// `(parent, child)` -> owner.
    owners: BTreeMap<(String, String), Publisher>,
    /// Conflicts already reported, so the diagnostic is rate-limited by
    /// *identity* rather than by a timer. A timer would report the same pair
    /// again every interval forever; this reports each distinct
    /// (edge, owner, intruder) once and counts the rest.
    reported: BTreeMap<(String, String, Publisher, Publisher), u64>,
    /// Samples dropped by policy, in total. §5.9 exposes it.
    dropped: u64,
}

impl Authority {
    /// A fresh table under `policy`.
    #[must_use]
    pub fn new(policy: AuthorityPolicy) -> Authority {
        Authority {
            policy,
            ..Authority::default()
        }
    }

    /// Decide what to do with a sample from `publisher` on `(parent, child)`.
    ///
    /// # An unattributed publisher does not lose by default
    ///
    /// §5.3 requires graceful degradation: on an RMW that reports no GIDs,
    /// *every* publisher is [`Publisher::Unattributed`], and a rule that
    /// rejected unattributed samples would reject the entire stream. So
    /// attribution failure is treated as "the same anonymous publisher",
    /// which on a correctly configured system is true and on a misconfigured
    /// one loses only the diagnostic — never the data.
    pub fn admit(&mut self, parent: &str, child: &str, publisher: &Publisher) -> Verdict {
        let key = (parent.to_string(), child.to_string());
        let Some(owner) = self.owners.get(&key) else {
            self.owners.insert(key, publisher.clone());
            return Verdict::Accept;
        };
        if owner == publisher {
            return Verdict::Accept;
        }

        let owner = owner.clone();
        match self.policy {
            AuthorityPolicy::LastWriterWins => {
                self.owners.insert(key, publisher.clone());
                Verdict::Accept
            }
            AuthorityPolicy::Strict => Verdict::Fatal {
                owner,
                intruder: publisher.clone(),
            },
            AuthorityPolicy::FirstWriterWins => {
                let rk = (
                    parent.to_string(),
                    child.to_string(),
                    owner.clone(),
                    publisher.clone(),
                );
                let seen = self.reported.entry(rk).or_insert(0);
                let first_time = *seen == 0;
                *seen += 1;
                self.dropped += 1;
                Verdict::Reject {
                    owner,
                    intruder: publisher.clone(),
                    first_time,
                }
            }
        }
    }

    /// Total samples dropped by policy (§5.9).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Every conflict seen, as `(parent, child, owner, intruder, count)`.
    ///
    /// This is what `tf_tree doctor` surfaces (§5.4 requires it), and it is why
    /// the counts are kept rather than only the first occurrence: "dropped 400
    /// 000 samples from `/odom_node`" is the sentence that makes an operator
    /// act, and "there was a conflict once" is not.
    pub fn conflicts(&self) -> impl Iterator<Item = (&str, &str, &Publisher, &Publisher, u64)> {
        self.reported
            .iter()
            .map(|((p, c, o, i), n)| (p.as_str(), c.as_str(), o, i, *n))
    }

    /// The owner of an edge, if one has been established.
    #[must_use]
    pub fn owner_of(&self, parent: &str, child: &str) -> Option<&Publisher> {
        self.owners.get(&(parent.to_string(), child.to_string()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn node(n: &str) -> Publisher {
        Publisher::Node(n.to_string())
    }

    /// **The default policy is stable under a flapping intruder.**
    ///
    /// The first publisher keeps the edge no matter how insistent the second
    /// is. That is the whole difference from `tf2`, which interleaves them by
    /// timestamp and hands the consumer a blend of two authorities.
    ///
    /// Mutant: make `FirstWriterWins` re-insert the owner ⇒ it becomes
    /// `LastWriterWins` and this fails on the second `admit`.
    #[test]
    fn first_writer_keeps_the_edge_however_often_the_second_tries() {
        let mut a = Authority::new(AuthorityPolicy::FirstWriterWins);
        assert_eq!(a.admit("odom", "base_link", &node("/ekf")), Verdict::Accept);
        for i in 0..100 {
            match a.admit("odom", "base_link", &node("/odom_node")) {
                Verdict::Reject {
                    owner,
                    intruder,
                    first_time,
                } => {
                    assert_eq!(owner, node("/ekf"), "the diagnostic must name the owner");
                    assert_eq!(intruder, node("/odom_node"), "...and the intruder");
                    assert_eq!(first_time, i == 0, "loud once, then counted");
                }
                other => panic!("expected a rejection, got {other:?}"),
            }
        }
        assert_eq!(a.dropped(), 100);
        assert_eq!(a.owner_of("odom", "base_link"), Some(&node("/ekf")));

        // ...and the owner is still admitted throughout.
        assert_eq!(a.admit("odom", "base_link", &node("/ekf")), Verdict::Accept);
    }

    /// **The diagnostic is rate-limited by identity, not by a timer.**
    ///
    /// §5.4 asks for loud *and* rate-limited. A per-message log at 1 kHz is
    /// neither: it is a denial of service against the operator's ability to
    /// read anything else in the log. A timer would re-report the same pair
    /// forever; this reports each distinct (edge, owner, intruder) once and
    /// counts the rest — so a *new* conflict is still loud, which is the
    /// property a timer loses.
    #[test]
    fn a_new_conflict_is_loud_even_after_an_old_one_went_quiet() {
        let mut a = Authority::new(AuthorityPolicy::FirstWriterWins);
        a.admit("odom", "base_link", &node("/ekf"));
        for _ in 0..50 {
            a.admit("odom", "base_link", &node("/odom_node"));
        }
        // A third publisher on the same edge is a different conflict.
        match a.admit("odom", "base_link", &node("/slam")) {
            Verdict::Reject { first_time, .. } => {
                assert!(
                    first_time,
                    "a new intruder must not be silenced by an old one"
                );
            }
            other => panic!("{other:?}"),
        }
        // And a different edge is a different conflict again.
        a.admit("map", "odom", &node("/ekf"));
        match a.admit("map", "odom", &node("/slam")) {
            Verdict::Reject { first_time, .. } => assert!(first_time),
            other => panic!("{other:?}"),
        }
        let all: Vec<_> = a.conflicts().collect();
        assert_eq!(all.len(), 3, "three distinct conflicts");
        let counts: Vec<u64> = all.iter().map(|c| c.4).collect();
        assert!(
            counts.contains(&50),
            "the repeat count must be kept: {counts:?}"
        );
    }

    /// **`LastWriterWins` is available and does what it says**, chaotically.
    #[test]
    fn last_writer_wins_hands_the_edge_over_every_time() {
        let mut a = Authority::new(AuthorityPolicy::LastWriterWins);
        assert_eq!(a.admit("odom", "base", &node("/a")), Verdict::Accept);
        assert_eq!(a.admit("odom", "base", &node("/b")), Verdict::Accept);
        assert_eq!(a.owner_of("odom", "base"), Some(&node("/b")));
        assert_eq!(a.admit("odom", "base", &node("/a")), Verdict::Accept);
        assert_eq!(a.owner_of("odom", "base"), Some(&node("/a")));
        assert_eq!(a.dropped(), 0, "nothing is dropped, which is the problem");
    }

    /// **`Strict` fails rather than degrades**, which is what CI wants.
    #[test]
    fn strict_reports_a_conflict_as_fatal() {
        let mut a = Authority::new(AuthorityPolicy::Strict);
        assert_eq!(a.admit("odom", "base", &node("/a")), Verdict::Accept);
        assert_eq!(
            a.admit("odom", "base", &node("/b")),
            Verdict::Fatal {
                owner: node("/a"),
                intruder: node("/b"),
            }
        );
        // Fatal does not mutate ownership: a caller that ignores it and keeps
        // going must not find the edge silently reassigned underneath it.
        assert_eq!(a.owner_of("odom", "base"), Some(&node("/a")));
    }

    /// **An RMW that reports no GIDs must not lose the whole stream.**
    ///
    /// §5.3 requires graceful degradation, and this is where it would fail
    /// silently: if unattributed samples were treated as distinct publishers,
    /// every message after the first would be dropped on an RMW without GID
    /// support — the transform tree would simply stop updating, with a
    /// diagnostic blaming a publisher that does not exist.
    ///
    /// Mutant: make `Publisher::Unattributed` compare unequal to itself (or
    /// treat it as always-new) ⇒ every sample after the first is rejected.
    #[test]
    fn unattributed_publishers_are_one_publisher_not_many() {
        let mut a = Authority::new(AuthorityPolicy::FirstWriterWins);
        for _ in 0..100 {
            assert_eq!(
                a.admit("odom", "base", &Publisher::Unattributed),
                Verdict::Accept
            );
        }
        assert_eq!(a.dropped(), 0);

        // But an *attributed* publisher colliding with the anonymous one is
        // still a real conflict worth naming — the operator can act on
        // "something unattributed owns this edge and /ekf wants it".
        match a.admit("odom", "base", &node("/ekf")) {
            Verdict::Reject { owner, .. } => assert_eq!(owner, Publisher::Unattributed),
            other => panic!("{other:?}"),
        }
    }

    /// Two edges are independent. Obvious, and the kind of thing a single
    /// shared owner field gets wrong.
    #[test]
    fn ownership_is_per_edge() {
        let mut a = Authority::new(AuthorityPolicy::FirstWriterWins);
        assert_eq!(a.admit("map", "odom", &node("/slam")), Verdict::Accept);
        assert_eq!(a.admit("odom", "base", &node("/ekf")), Verdict::Accept);
        assert_eq!(a.dropped(), 0, "different edges must not collide");
        // And direction matters: parent/child reversed is a different edge.
        assert_eq!(a.admit("odom", "map", &node("/other")), Verdict::Accept);
    }
}
