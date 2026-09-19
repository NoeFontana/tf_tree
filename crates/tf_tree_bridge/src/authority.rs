//! Authority policy — `docs/PHASE4.md` §5.4, NORMATIVE.
//!
//! One publisher per edge (D7); conflicts are noticed and both sides named.

use std::collections::BTreeMap;

use crate::config::TopologyConfig;
use crate::edgeindex::{EdgeIndex, EdgeSlot};
use crate::interner::StrInterner;
use crate::Publisher;

/// How to resolve two publishers on one edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthorityPolicy {
    /// The first attributed publisher owns the edge; later samples are dropped and counted.
    #[default]
    FirstWriterWins,
    /// Reclaim on each new publisher. Chaotic; never the default.
    LastWriterWins,
    /// A conflict is [`Verdict::Fatal`] within [`crate::Ingest`]'s startup window
    /// (`docs/decisions/0011`); outside it, degrades to `FirstWriterWins`.
    Strict,
}

/// What the bridge should do with a sample.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// Publish it.
    Accept,
    /// Drop it. The edge belongs to someone else.
    Reject {
        /// The edge's owner.
        owner: Publisher,
        /// The publisher that tried to write it.
        intruder: Publisher,
        /// Whether this is the first collision of these two on this edge.
        first_time: bool,
    },
    /// Under [`AuthorityPolicy::Strict`], a conflict: dropped and recorded like
    /// [`Verdict::Reject`]; [`crate::Ingest`] decides whether it halts.
    Fatal {
        /// The edge's owner.
        owner: Publisher,
        /// The publisher that collided with it.
        intruder: Publisher,
        /// As [`Verdict::Reject`]'s.
        first_time: bool,
    },
}

/// Per-edge ownership, and the conflicts seen so far.
#[derive(Debug)]
pub struct Authority {
    policy: AuthorityPolicy,
    /// `(parent, child)` -> slot, in [`crate::StaticStore::seeded`]'s order.
    index: EdgeIndex<EdgeSlot>,
    owners: Vec<Option<Publisher>>,
    ids: StrInterner,
    pubs: Vec<Publisher>,
    /// Conflicts already reported, keyed by interned ids; bounded at `slots x cap x cap`.
    reported: BTreeMap<(u32, u32, u32), u64>,
    /// Samples dropped by policy, in total.
    dropped: u64,
}

impl Default for Authority {
    fn default() -> Authority {
        Authority {
            policy: AuthorityPolicy::default(),
            index: EdgeIndex::default(),
            owners: Vec::new(),
            ids: StrInterner::with_cap(crate::clock::MAX_TRACKED_PUBLISHERS),
            pubs: Vec::new(),
            reported: BTreeMap::new(),
            dropped: 0,
        }
    }
}

impl Authority {
    /// A fresh table under `policy`.
    #[must_use]
    pub fn new(policy: AuthorityPolicy) -> Authority {
        Authority {
            policy,
            ids: StrInterner::with_cap(crate::clock::MAX_TRACKED_PUBLISHERS),
            ..Authority::default()
        }
    }

    /// A table whose slots are `config`'s edges, in [`crate::StaticStore::seeded`]'s order.
    #[must_use]
    pub(crate) fn seeded(policy: AuthorityPolicy, config: &TopologyConfig) -> Authority {
        let mut a = Authority::new(policy);
        a.index = EdgeIndex::with_capacity(config.edges.len());
        for e in &config.edges {
            let (p, c) = e.key();
            a.slot_or_insert(p, c);
        }
        a
    }

    fn slot_or_insert(&mut self, parent: &str, child: &str) -> EdgeSlot {
        if let Some(slot) = self.index.get(parent, child) {
            return slot;
        }
        let slot = EdgeSlot(u32::try_from(self.index.len()).unwrap_or(u32::MAX));
        self.index.insert(parent, child, slot);
        self.owners.push(None);
        slot
    }

    /// The id for a publisher, interning it if the cap allows.
    fn id_for(&mut self, publisher: &Publisher) -> u32 {
        let key = crate::ingest::owner_key(publisher);
        match self.ids.intern(key) {
            Some(id) => {
                if self.pubs.len() <= id.get() {
                    self.pubs.resize(id.get() + 1, Publisher::Unattributed);
                    self.pubs[id.get()] = publisher.clone();
                }
                id.0
            }
            None => u32::MAX,
        }
    }

    /// Decide what to do with a sample from `publisher` on `(parent, child)`.
    pub fn admit(&mut self, parent: &str, child: &str, publisher: &Publisher) -> Verdict {
        let slot = self.slot_or_insert(parent, child);
        self.admit_at(slot, publisher)
    }

    /// [`Self::admit`] for a caller that already holds the slot.
    pub(crate) fn admit_at(&mut self, slot: EdgeSlot, publisher: &Publisher) -> Verdict {
        let Some(owner) = self.owners[slot.get()].as_ref() else {
            self.owners[slot.get()] = Some(publisher.clone());
            return Verdict::Accept;
        };
        if owner == publisher {
            return Verdict::Accept;
        }

        let owner = owner.clone();
        match self.policy {
            AuthorityPolicy::LastWriterWins => {
                self.owners[slot.get()] = Some(publisher.clone());
                Verdict::Accept
            }
            // `Strict` leaves `owners` alone, like `FirstWriterWins`.
            AuthorityPolicy::Strict => {
                let first_time = self.record(slot, &owner, publisher);
                Verdict::Fatal {
                    owner,
                    intruder: publisher.clone(),
                    first_time,
                }
            }
            AuthorityPolicy::FirstWriterWins => {
                let first_time = self.record(slot, &owner, publisher);
                Verdict::Reject {
                    owner,
                    intruder: publisher.clone(),
                    first_time,
                }
            }
        }
    }

    /// Count a dropped sample against this conflict; true if it is the first of its kind.
    fn record(&mut self, slot: EdgeSlot, owner: &Publisher, intruder: &Publisher) -> bool {
        let (o, i) = (self.id_for(owner), self.id_for(intruder));
        let seen = self.reported.entry((slot.0, o, i)).or_insert(0);
        let first_time = *seen == 0;
        *seen += 1;
        self.dropped += 1;
        first_time
    }

    /// The policy this table was built with.
    #[must_use]
    pub fn policy(&self) -> AuthorityPolicy {
        self.policy
    }

    /// Total samples dropped by policy (§5.9).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Every conflict seen, as `(parent, child, owner, intruder, count)`.
    pub fn conflicts(&self) -> impl Iterator<Item = (&str, &str, &Publisher, &Publisher, u64)> {
        self.reported.iter().map(|((slot, o, i), n)| {
            let (p, c) = self.index.key(*slot as usize);
            (p, c, &self.pubs[*o as usize], &self.pubs[*i as usize], *n)
        })
    }

    /// The slot assigned to an edge, if any.
    #[cfg(test)]
    pub(crate) fn slot_of(&self, parent: &str, child: &str) -> Option<EdgeSlot> {
        self.index.get(parent, child)
    }

    /// The owner of an edge, if one has been established.
    #[must_use]
    pub fn owner_of(&self, parent: &str, child: &str) -> Option<&Publisher> {
        self.index
            .get(parent, child)
            .and_then(|s| self.owners[s.get()].as_ref())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn node(n: &str) -> Publisher {
        Publisher::named(&crate::gid_for_name(n), n)
    }

    /// The first publisher keeps the edge however insistent the second is.
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

        assert_eq!(a.admit("odom", "base_link", &node("/ekf")), Verdict::Accept);
    }

    /// The diagnostic is rate-limited by identity: a new conflict is still loud.
    #[test]
    fn a_new_conflict_is_loud_even_after_an_old_one_went_quiet() {
        let mut a = Authority::new(AuthorityPolicy::FirstWriterWins);
        a.admit("odom", "base_link", &node("/ekf"));
        for _ in 0..50 {
            a.admit("odom", "base_link", &node("/odom_node"));
        }
        match a.admit("odom", "base_link", &node("/slam")) {
            Verdict::Reject { first_time, .. } => {
                assert!(
                    first_time,
                    "a new intruder must not be silenced by an old one"
                );
            }
            other => panic!("{other:?}"),
        }
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

    /// `Strict` reports `Fatal` and still records the conflict (`docs/decisions/0011`).
    #[test]
    fn strict_reports_a_conflict_as_fatal_and_still_records_it() {
        let mut a = Authority::new(AuthorityPolicy::Strict);
        assert_eq!(a.policy(), AuthorityPolicy::Strict);
        assert_eq!(a.admit("odom", "base", &node("/a")), Verdict::Accept);
        assert_eq!(
            a.admit("odom", "base", &node("/b")),
            Verdict::Fatal {
                owner: node("/a"),
                intruder: node("/b"),
                first_time: true,
            }
        );
        assert_eq!(a.owner_of("odom", "base"), Some(&node("/a")));

        assert_eq!(
            a.admit("odom", "base", &node("/b")),
            Verdict::Fatal {
                owner: node("/a"),
                intruder: node("/b"),
                first_time: false,
            }
        );
        assert_eq!(a.dropped(), 2);
        let all: Vec<_> = a.conflicts().collect();
        assert_eq!(all.len(), 1, "one distinct conflict, seen twice: {all:?}");
        assert_eq!(all[0].4, 2);
    }

    /// An RMW with no GIDs must not lose the stream (§5.3).
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

        match a.admit("odom", "base", &node("/ekf")) {
            Verdict::Reject { owner, .. } => assert_eq!(owner, Publisher::Unattributed),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ownership_is_per_edge() {
        let mut a = Authority::new(AuthorityPolicy::FirstWriterWins);
        assert_eq!(a.admit("map", "odom", &node("/slam")), Verdict::Accept);
        assert_eq!(a.admit("odom", "base", &node("/ekf")), Verdict::Accept);
        assert_eq!(a.dropped(), 0, "different edges must not collide");
        assert_eq!(a.admit("odom", "map", &node("/other")), Verdict::Accept);
    }
}
