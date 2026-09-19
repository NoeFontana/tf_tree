//! `(parent, child)` → a dense slot, in one hash and one comparison, for the
//! declared edge set fixed at construction (measured by `just bridge-footprint`).
//! The table with an unfixed key set stays in [`crate::edgemap`].

/// A dense index into the declared-edge tables: the *first* declaration of a
/// normalized `(parent, child)`; declarations that collapse onto one pair share a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct EdgeSlot(pub(crate) u32);

impl EdgeSlot {
    /// As a `Vec` index.
    pub(crate) fn get(self) -> usize {
        self.0 as usize
    }
}

const K: u64 = 0x517c_c1b7_2722_0a95;

const EMPTY: u32 = u32::MAX;

/// Fold `bytes` into `h`, length included so `"ab"` and `"ab\0"` differ.
pub(crate) fn mix(mut h: u64, bytes: &[u8]) -> u64 {
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

/// A 64-bit hash of the pair. Unkeyed by design: keys come only from the declared
/// topology and every hit is confirmed against both stored names.
fn hash_pair(parent: &str, child: &str) -> u64 {
    let h = mix(0xcbf2_9ce4_8422_2325, parent.as_bytes()) ^ 0x9e37_79b9_7f4a_7c15;
    let h = mix(h, child.as_bytes());
    let h = (h ^ (h >> 32)).wrapping_mul(K);
    h ^ (h >> 29)
}

/// Bucket count for `n` entries: a power of two, at least `4n + 4`, never below 16.
/// [`EdgeIndex::find`] terminates only because the table is never full; shared with
/// `crate::interner`.
#[inline]
pub(crate) fn buckets_for(n: usize) -> usize {
    (4 * n + 4).next_power_of_two().max(16)
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    hash: u64,
    entry: u32,
}

/// A `(parent, child)` → `V` table probed by reference, without allocating.
#[derive(Debug)]
pub(crate) struct EdgeIndex<V> {
    buckets: Vec<Bucket>,
    mask: usize,
    /// `keys[i]` is entry `i`'s key, owned so a hit is confirmed by name.
    keys: Vec<(Box<str>, Box<str>)>,
    values: Vec<V>,
}

impl<V> Default for EdgeIndex<V> {
    fn default() -> EdgeIndex<V> {
        EdgeIndex::with_capacity(0)
    }
}

impl<V> EdgeIndex<V> {
    /// A table sized for `n` entries up front.
    pub(crate) fn with_capacity(n: usize) -> EdgeIndex<V> {
        let len = buckets_for(n);
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
            // Fallible `get`: an invariant slip degrades to a miss, not a panic.
            let b = *self.buckets.get(i)?;
            if b.entry == EMPTY {
                return None;
            }
            if b.hash == h {
                let (p, c) = &self.keys[b.entry as usize];
                // Confirmed by name: a believed 64-bit collision would corrupt the tree.
                if &**p == parent && &**c == child {
                    return Some(b.entry as usize);
                }
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Insert, or overwrite an existing key's value. Returns the entry index. Allocates.
    pub(crate) fn insert(&mut self, parent: &str, child: &str, v: V) -> usize {
        if let Some(e) = self.find(parent, child) {
            self.values[e] = v;
            return e;
        }
        let e = self.keys.len();
        self.keys.push((Box::from(parent), Box::from(child)));
        self.values.push(v);
        if self.buckets.len() < buckets_for(self.keys.len()) {
            self.rehash();
        } else {
            let h = hash_pair(parent, child);
            place(&mut self.buckets, self.mask, h, e as u32);
        }
        e
    }

    fn rehash(&mut self) {
        let len = buckets_for(self.keys.len());
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
    /// Probe without allocating; returns by value so the borrow ends with the probe.
    pub(crate) fn get(&self, parent: &str, child: &str) -> Option<V> {
        self.find(parent, child).map(|e| self.values[e])
    }
}

/// Place `entry` at the first empty bucket at or after `h`'s home.
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

    /// The pair is hashed as a pair: `("ab", "c")` and `("a", "bc")` differ.
    #[test]
    fn the_pair_is_hashed_as_a_pair() {
        assert_ne!(hash_pair("ab", "c"), hash_pair("a", "bc"));
        let mut t: EdgeIndex<u32> = EdgeIndex::with_capacity(4);
        t.insert("ab", "c", 1);
        t.insert("a", "bc", 2);
        assert_eq!(t.get("ab", "c"), Some(1));
        assert_eq!(t.get("a", "bc"), Some(2));
    }

    /// A bucket collision resolves to the right entry.
    #[test]
    fn a_bucket_collision_resolves_to_the_right_entry() {
        let mut t: EdgeIndex<u32> = EdgeIndex::with_capacity(0);
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
        assert!(
            t.buckets.len() >= 4 * t.len(),
            "load factor above a quarter: {} buckets for {} keys",
            t.buckets.len(),
            t.len()
        );
    }

    #[test]
    fn reinserting_a_key_overwrites_it() {
        let mut t: EdgeIndex<u32> = EdgeIndex::with_capacity(4);
        t.insert("map", "odom", 1);
        t.insert("map", "odom", 2);
        assert_eq!(t.len(), 1);
        assert_eq!(t.get("map", "odom"), Some(2));
    }

    #[test]
    fn a_slot_names_its_edge() {
        let mut t: EdgeIndex<u32> = EdgeIndex::with_capacity(2);
        let e = t.insert("map", "odom", 0);
        assert_eq!(t.key(e), ("map", "odom"));
    }

    /// The table is never full, at any size.
    #[test]
    fn the_table_is_never_full() {
        for n in 0..600usize {
            let b = buckets_for(n);
            assert!(
                b > n,
                "n = {n}: {b} buckets cannot hold {n} keys and an empty one"
            );
            assert!(
                b >= 4 * n,
                "n = {n}: load factor above a quarter ({b} buckets)"
            );
            assert!(b.is_power_of_two(), "n = {n}: {b} is not a power of two");
        }
    }

    #[test]
    fn a_stranger_misses_at_every_size() {
        let mut t: EdgeIndex<u32> = EdgeIndex::with_capacity(0);
        for i in 0..300u32 {
            t.insert(&format!("p{i}"), &format!("c{i}"), i);
            assert_eq!(t.get("absent", "absent"), None, "after {i} inserts");
        }
    }
}
