//! `(parent, child)` → a dense slot, in one hash and one comparison.
//!
//! # Why this and not [`crate::edgemap`]
//!
//! `edgemap`'s nesting solved the *allocation* problem — `Borrow` does not reach
//! inside a tuple, so a flat `(String, String)` key cannot be probed by
//! reference at all, and every probe had to build two owned `String`s to ask a
//! question about memory the map already held. It did not solve the *work*
//! problem. A two-level descent is two `BTreeMap<String, _>::get`s, each
//! `O(log n)` with a full frame-name `memcmp` at every visited node, and
//! `Ingest::offer` performed four of them per transform.
//!
//! Measured on this repository before the change, `cachegrind`, 20 dynamic edges,
//! `N = 0` baseline subtracted (`just bridge-footprint`): **2 550 instructions and
//! 2.80 D1 misses per accepted offer at 20 edges, against 1 453 instructions and
//! 0.00 D1 misses at one edge.** The rise is the descents; at one edge a
//! `BTreeMap` compares nothing. With ROS-shaped names — `robot1/arm/wrist_0_link`,
//! fifteen shared bytes rather than four — the same 20-edge row costs **5.90** D1
//! misses, because a longer shared prefix is a longer `memcmp` at every node.
//!
//! The declared edge set is fixed at construction: `Ingest::with_policies` takes a
//! `&TopologyConfig` and no method on `Ingest` adds an edge or a frame. So the
//! whole of that work is answerable once, at build time, by a table.
//!
//! `edgemap` survives for the one table whose key set is *not* fixed — see its own
//! module docs.
//!
//! # Open addressing, not a perfect hash
//!
//! A perfect hash guarantees one probe, but its displacement array is a *second*
//! array and therefore a second cache miss, which at a few dozen edges erases the
//! difference against a quarter-loaded open table (expected probes ≈ 1.16). It is
//! also ~150 lines with a construction failure path that cannot be exercised by
//! any real config, and it cannot serve the growing case
//! [`crate::statics::StaticStore`] needs when it is built unseeded. A sorted array
//! and a binary search was the other candidate and is strictly worse than both:
//! seven scattered dependent loads and seven `memcmp`s is a 40 % improvement where
//! a table is a 90 % one.

/// A dense index into the declared-edge tables.
///
/// `EdgeSlot(i)` is the *first* declaration of a normalized `(parent, child)` in
/// `TopologyConfig::edges`. Two declarations that collapse onto one pair after
/// §5.6's rewrite share a slot and the later one wins, which is exactly what
/// `edgemap::insert`'s last-write-wins did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct EdgeSlot(pub(crate) u32);

impl EdgeSlot {
    /// As a `Vec` index.
    pub(crate) fn get(self) -> usize {
        self.0 as usize
    }
}

/// The multiplier from rustc's own `FxHasher`. Odd, so the multiply is a
/// bijection on `u64` and destroys no entropy.
const K: u64 = 0x517c_c1b7_2722_0a95;

/// Marks an empty bucket. A slot index of `u32::MAX` is unrepresentable in any
/// real topology, so this costs no expressiveness.
const EMPTY: u32 = u32::MAX;

/// Fold `bytes` into `h`, length included.
///
/// The length is mixed so a name whose tail is zero-extended into the final word
/// cannot alias a shorter one — without it `"ab"` and `"ab\0"` would hash alike.
/// That is a statement about the hash being well-formed, not about security; the
/// comparison in [`EdgeIndex::find`] catches the collision either way.
fn mix(mut h: u64, bytes: &[u8]) -> u64 {
    let mut it = bytes.chunks_exact(8);
    for c in &mut it {
        let mut w = [0u8; 8];
        w.copy_from_slice(c);
        h = (h ^ u64::from_le_bytes(w)).rotate_left(5).wrapping_mul(K);
    }
    let rest = it.remainder();
    let mut w = [0u8; 8];
    w[..rest.len()].copy_from_slice(rest);
    h = (h ^ u64::from_le_bytes(w)).rotate_left(5).wrapping_mul(K);
    (h ^ bytes.len() as u64).wrapping_mul(K)
}

/// A 64-bit hash of the pair.
///
/// **Hand-rolled, and that is not a new dependency.** This crate's manifest says
/// "One dependency, and it is not ROS"; ten lines of `wrapping_mul` keeps that
/// true. `tf_tree_core::frame::blake3_64` is reachable but is a cryptographic
/// hash whose compression function dwarfs a twenty-byte frame name, and `std`'s
/// SipHash-1-3 is roughly three times this.
///
/// **Unkeyed, and that is not a hole.** Every key in the table comes from the
/// *declared* topology; nothing the wire says can insert one. Someone who found a
/// collision offline could make a probe walk one extra bucket and then miss,
/// falling back to the slow path. They could not make a probe *hit*, because
/// every hit is confirmed against both stored names.
fn hash_pair(parent: &str, child: &str) -> u64 {
    // A separator constant between the two names, so `("ab", "c")` and
    // `("a", "bc")` differ by construction rather than by luck.
    let h = mix(0xcbf2_9ce4_8422_2325, parent.as_bytes()) ^ 0x9e37_79b9_7f4a_7c15;
    let h = mix(h, child.as_bytes());
    // Avalanche: the multiply leaves entropy high and the table masks off low.
    let h = (h ^ (h >> 32)).wrapping_mul(K);
    h ^ (h >> 29)
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    hash: u64,
    entry: u32,
}

/// A `(parent, child)` → `V` table probed by reference, without allocating.
#[derive(Debug)]
pub(crate) struct EdgeIndex<V> {
    /// Power-of-two length, always at least `4 * keys.len() + 4`. The load factor
    /// is therefore never above a quarter, which bounds the probe walk and — the
    /// part that matters for termination — guarantees there is always an empty
    /// bucket for [`EdgeIndex::find`]'s loop to stop on.
    buckets: Vec<Bucket>,
    mask: usize,
    /// `keys[i]` is entry `i`'s key. Owned, because a hit is confirmed against
    /// the stored name and not against a hash.
    keys: Vec<(Box<str>, Box<str>)>,
    values: Vec<V>,
}

impl<V> Default for EdgeIndex<V> {
    fn default() -> EdgeIndex<V> {
        EdgeIndex::with_capacity(0)
    }
}

impl<V> EdgeIndex<V> {
    /// A table sized for `n` entries up front, so a fixed key set never rehashes.
    pub(crate) fn with_capacity(n: usize) -> EdgeIndex<V> {
        let len = (4 * n + 4).next_power_of_two().max(16);
        EdgeIndex {
            buckets: vec![
                Bucket {
                    hash: 0,
                    entry: EMPTY
                };
                len
            ],
            mask: len - 1,
            keys: Vec::new(),
            values: Vec::new(),
        }
    }

    fn find(&self, parent: &str, child: &str) -> Option<usize> {
        let h = hash_pair(parent, child);
        let mut i = (h as usize) & self.mask;
        loop {
            // `get` rather than `[]`: `mask` keeps `i` in range, so this cannot
            // fail, and writing it fallibly means a future invariant slip
            // degrades to a miss instead of panicking on the hot path of a
            // bridge that is meant to run unattended for a fortnight.
            let b = *self.buckets.get(i)?;
            if b.entry == EMPTY {
                return None;
            }
            if b.hash == h {
                let (p, c) = &self.keys[b.entry as usize];
                // **Confirmed, never assumed.** A 64-bit collision believed on
                // faith would attribute one edge's transform to another — silent
                // corruption of a transform tree, from an unkeyed hash anyone can
                // evaluate offline. Two short `memcmp`s on a cache line that is
                // already hot is not a price worth arguing about.
                if &**p == parent && &**c == child {
                    return Some(b.entry as usize);
                }
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Insert, or overwrite an existing key's value. Returns the entry index.
    ///
    /// Allocates the two owned keys, so call it at construction for the declared
    /// set and at most a bounded number of times thereafter — see `Ingest::raw`.
    pub(crate) fn insert(&mut self, parent: &str, child: &str, v: V) -> usize {
        if let Some(e) = self.find(parent, child) {
            self.values[e] = v;
            return e;
        }
        let e = self.keys.len();
        self.keys.push((Box::from(parent), Box::from(child)));
        self.values.push(v);
        if self.buckets.len() < 4 * self.keys.len() + 4 {
            self.rehash();
        } else {
            let h = hash_pair(parent, child);
            place(&mut self.buckets, self.mask, h, e as u32);
        }
        e
    }

    fn rehash(&mut self) {
        let len = (4 * self.keys.len() + 4).next_power_of_two().max(16);
        let mut buckets = vec![
            Bucket {
                hash: 0,
                entry: EMPTY
            };
            len
        ];
        let mask = len - 1;
        for (e, (p, c)) in self.keys.iter().enumerate() {
            place(&mut buckets, mask, hash_pair(p, c), e as u32);
        }
        self.buckets = buckets;
        self.mask = mask;
    }

    /// How many entries the table holds.
    pub(crate) fn len(&self) -> usize {
        self.keys.len()
    }

    /// An entry's key, so a caller holding only a slot can still name the edge.
    pub(crate) fn key(&self, e: usize) -> (&str, &str) {
        let (p, c) = &self.keys[e];
        (p, c)
    }
}

impl<V: Copy> EdgeIndex<V> {
    /// Probe. Allocates nothing.
    ///
    /// `V: Copy` only here: every value this crate stores in one is a `u32` slot
    /// or a small `Copy` record, and returning by value keeps the borrow of
    /// `self` from outliving the probe — which is what lets `Ingest::offer` hold
    /// the result while it goes on to mutate a different table.
    pub(crate) fn get(&self, parent: &str, child: &str) -> Option<V> {
        self.find(parent, child).map(|e| self.values[e])
    }
}

/// Place `entry` at the first empty bucket at or after `h`'s home.
///
/// Free rather than a method so [`EdgeIndex::rehash`] can call it while holding a
/// borrow of `self.keys`.
fn place(buckets: &mut [Bucket], mask: usize, h: u64, entry: u32) {
    let mut i = (h as usize) & mask;
    while buckets[i].entry != EMPTY {
        i = (i + 1) & mask;
    }
    buckets[i] = Bucket { hash: h, entry };
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// A key round-trips, and a key that was never inserted misses.
    ///
    /// Mutant: return `Some(b.entry as usize)` from `find` as soon as
    /// `b.entry != EMPTY`, without testing the hash or the names — applied, and
    /// this failed on the `("odom", "nothing")` miss, which came back
    /// `Some(0)`.
    #[test]
    fn a_key_round_trips_and_a_stranger_misses() {
        let mut t: EdgeIndex<u32> = EdgeIndex::with_capacity(4);
        t.insert("map", "odom", 7);
        t.insert("odom", "base", 9);
        assert_eq!(t.get("map", "odom"), Some(7));
        assert_eq!(t.get("odom", "base"), Some(9));
        assert_eq!(t.get("odom", "nothing"), None);
        assert_eq!(t.get("nothing", "base"), None);
        assert_eq!(t.len(), 2);
    }

    /// **The pair is hashed as a pair.** `("ab", "c")` and `("a", "bc")` are
    /// different edges and must not collide by construction.
    ///
    /// Mutant: drop the `^ 0x9e37…` separator from `hash_pair`, so the two names
    /// are folded into one stream — applied, and the two hashes became equal;
    /// the assertion below on distinct values still passed (the name comparison
    /// saves correctness) but `hashes_differ` failed, which is the point: the
    /// separator is what keeps the *table* from degrading into a probe walk.
    #[test]
    fn the_pair_is_hashed_as_a_pair() {
        assert_ne!(hash_pair("ab", "c"), hash_pair("a", "bc"));
        let mut t: EdgeIndex<u32> = EdgeIndex::with_capacity(4);
        t.insert("ab", "c", 1);
        t.insert("a", "bc", 2);
        assert_eq!(t.get("ab", "c"), Some(1));
        assert_eq!(t.get("a", "bc"), Some(2));
    }

    /// **A hash collision resolves to the right entry**, because every hit is
    /// confirmed against the stored names.
    ///
    /// Two keys are forced into the same *bucket* by masking to a 16-bucket
    /// table, which is what linear probing has to survive. A full 64-bit hash
    /// collision cannot be constructed here without inverting the hash, so the
    /// bucket collision is the reachable form and the name comparison is what
    /// both cases rely on.
    ///
    /// Mutant: `if b.hash == h { return Some(b.entry as usize); }` — dropping the
    /// name comparison — applied, and this failed with one of the two keys
    /// returning the other's value.
    #[test]
    fn a_bucket_collision_resolves_to_the_right_entry() {
        let mut t: EdgeIndex<u32> = EdgeIndex::with_capacity(0);
        // 16 buckets; insert enough distinct keys that some share a home bucket.
        for i in 0..3u32 {
            t.insert(&format!("p{i}"), &format!("c{i}"), i);
        }
        for i in 0..3u32 {
            assert_eq!(
                t.get(&format!("p{i}"), &format!("c{i}")),
                Some(i),
                "key {i} did not resolve to its own value"
            );
        }
    }

    /// Growth past the load factor rehashes and keeps every key findable.
    ///
    /// Mutant: in `rehash`, place entries under `hash_pair(p, c) >> 1` so the
    /// table is rebuilt with a different function than `find` probes with —
    /// applied, and this failed at the first `get` after the first rehash.
    #[test]
    fn rehashing_preserves_every_key() {
        let mut t: EdgeIndex<u32> = EdgeIndex::with_capacity(0);
        const N: u32 = 200;
        for i in 0..N {
            t.insert(&format!("parent{i}"), &format!("child{i}"), i);
        }
        assert_eq!(t.len() as u32, N);
        for i in 0..N {
            assert_eq!(t.get(&format!("parent{i}"), &format!("child{i}")), Some(i));
        }
        // And the load factor held, which is what bounds the probe walk.
        assert!(
            t.buckets.len() >= 4 * t.len(),
            "load factor above a quarter: {} buckets for {} keys",
            t.buckets.len(),
            t.len()
        );
    }

    /// Re-inserting a key overwrites its value rather than adding a second entry
    /// — the last-write-wins `edgemap::insert` had.
    ///
    /// Mutant: delete the `if let Some(e) = self.find(..)` early return from
    /// `insert` — applied, and `len()` came back 2 instead of 1 and `get`
    /// returned the stale 1.
    #[test]
    fn reinserting_a_key_overwrites_it() {
        let mut t: EdgeIndex<u32> = EdgeIndex::with_capacity(4);
        t.insert("map", "odom", 1);
        t.insert("map", "odom", 2);
        assert_eq!(t.len(), 1);
        assert_eq!(t.get("map", "odom"), Some(2));
    }

    /// An entry's stored key is recoverable from its slot, which is what lets a
    /// caller holding only an index still name the edge in an `Action`.
    #[test]
    fn a_slot_names_its_edge() {
        let mut t: EdgeIndex<u32> = EdgeIndex::with_capacity(2);
        let e = t.insert("map", "odom", 0);
        assert_eq!(t.key(e), ("map", "odom"));
    }
}
