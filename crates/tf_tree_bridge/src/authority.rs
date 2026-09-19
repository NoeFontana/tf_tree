//! Authority policy — `docs/PHASE4.md` §5.4, NORMATIVE.
//!
//! ROS permits many publishers per edge; `tf_tree` exactly one (D7). `tf2`
//! silently blends competing publishers; this module's job is to **notice** and
//! name both sides.

use std::collections::BTreeMap;

use crate::config::TopologyConfig;
use crate::edgeindex::{EdgeIndex, EdgeSlot};
use crate::interner::StrInterner;
use crate::Publisher;

/// How to resolve two publishers on one edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthorityPolicy {
    /// The first attributed publisher owns the edge; later samples are dropped and
    /// counted. The default, and stable under a flapping node.
    #[default]
    FirstWriterWins,
    /// Reclaim on each new publisher. Chaotic; never the default.
    LastWriterWins,
    /// Conflict within the startup window is fatal (for CI). The window is
    /// [`crate::Ingest`]'s (`docs/decisions/0011`): `admit` reports
    /// [`Verdict::Fatal`] whenever it sees a conflict, and outside the window
    /// `Strict` degrades to `FirstWriterWins` plus counters.
    Strict,
}

/// What the bridge should do with a sample.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// Publish it.
    Accept,
    /// Drop it. The edge belongs to someone else.
    Reject {
        /// Who owns the edge.
        owner: Publisher,
        /// Who tried to write it.
        intruder: Publisher,
        /// Whether this is the first collision of these two on this edge (the
        /// bridge rate-limits its diagnostic on it).
        first_time: bool,
    },
    /// Under [`AuthorityPolicy::Strict`], a conflict: a fact, not an order.
    /// Dropped and recorded like [`Verdict::Reject`], so [`Authority::conflicts`]
    /// and `tf_tree doctor` see it; [`crate::Ingest`] decides whether it halts.
    Fatal {
        /// The prior owner.
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
    /// `(parent, child)` -> slot; same order as [`crate::StaticStore::seeded`], so
    /// one slot addresses both (a test asserts they agree).
    index: EdgeIndex<EdgeSlot>,
    /// Owner per slot. `None` until an edge is first written.
    owners: Vec<Option<Publisher>>,
    /// Publisher name -> id, capped (see [`crate::interner`]).
    ids: StrInterner,
    /// id -> publisher, for [`Self::conflicts`].
    pubs: Vec<Publisher>,
    /// Conflicts already reported, keyed by interned ids so the probe allocates
    /// nothing and the table is bounded at `slots x cap x cap`. Rate-limits by
    /// identity, not by timer.
    reported: BTreeMap<(u32, u32, u32), u64>,
    /// Samples dropped by policy, in total. §5.9 exposes it.
    dropped: u64,
}

impl Default for Authority {
    /// Hand-written: a derived `Default` would give the interner a zero cap.
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

    /// The slot for an edge, creating it if the table is growing.
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
    ///
    /// Past the cap publishers collapse onto one sentinel id: the breakdown is
    /// lost, never a count (§5.3's degradation).
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
    ///
    /// An unattributed publisher does not lose by default (§5.3): on an RMW with
    /// no GIDs all publishers are [`Publisher::Unattributed`], treated as one.
    pub fn admit(&mut self, parent: &str, child: &str, publisher: &Publisher) -> Verdict {
        // Probed by reference: a `(String, String)` key cost two allocations per message.
        let slot = self.slot_or_insert(parent, child);
        self.admit_at(slot, publisher)
    }

    /// [`Self::admit`] for a caller that already holds the slot.
    ///
    ///
    /// One array index instead of a two-level descent.
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
            // `Strict` records what `FirstWriterWins` records and leaves `owners`
            // alone, so a caller ignoring the verdict finds nothing reassigned.
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

    /// Count a dropped sample against this conflict, and say whether it is the
    /// first of its kind.
    ///
    /// Shared by the two dropping arms.
    fn record(&mut self, slot: EdgeSlot, owner: &Publisher, intruder: &Publisher) -> bool {
        // The only allocating path in `admit`, and it is the fault path.
        let (o, i) = (self.id_for(owner), self.id_for(intruder));
        let seen = self.reported.entry((slot.0, o, i)).or_insert(0);
        let first_time = *seen == 0;
        *seen += 1;
        self.dropped += 1;
        first_time
    }

    /// The policy this table was built with.
    ///
    /// Read by [`crate::Ingest`] at the close of the startup window.
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
    ///
    /// What `tf_tree doctor` surfaces (§5.4); counts are kept, not only the first.
    pub fn conflicts(&self) -> impl Iterator<Item = (&str, &str, &Publisher, &Publisher, u64)> {
        self.reported.iter().map(|((slot, o, i), n)| {
            let (p, c) = self.index.key(*slot as usize);
            (p, c, &self.pubs[*o as usize], &self.pubs[*i as usize], *n)
        })
    }

    /// The slot this table has assigned to an edge, if any.
    ///
    /// For the test that this table and `StaticStore` number slots alike.
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
    ///
    /// Mutant: make `FirstWriterWins` re-insert the owner; fails on the second `admit`.
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

    /// The diagnostic is rate-limited by identity: a new conflict is still loud.
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

    /// `LastWriterWins` hands the edge over every time.
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
    ///
    /// Mutant: drop `self.record(..)` from the arm; the second `admit` returns
    /// `first_time: true` and `dropped()`/`conflicts()` are empty.
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
        // Fatal does not mutate ownership.
        assert_eq!(a.owner_of("odom", "base"), Some(&node("/a")));

        // The conflict is on the books, rate-limited like `FirstWriterWins`.
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
    ///
    /// Mutant: make `Publisher::Unattributed` unequal to itself; every sample after the first is rejected.
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

        // An attributed publisher colliding with the anonymous one is still a conflict.
        match a.admit("odom", "base", &node("/ekf")) {
            Verdict::Reject { owner, .. } => assert_eq!(owner, Publisher::Unattributed),
            other => panic!("{other:?}"),
        }
    }

    /// Ownership is per edge.
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
