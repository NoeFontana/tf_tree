//! Publisher names → dense `u32` ids, with a hard cap.
//!
//! # Why publishers get an interner and edges get an index
//!
//! An edge's identity is a *pair* fixed by the config, so [`crate::edgeindex`]
//! answers it once at construction. A publisher's identity is a single name that
//! arrives on the wire and is not in any config — §5.3's GID→node resolution
//! produces it, and on an RMW with no endpoint introspection it degrades to one
//! of three fixed sentinels. So this table grows at runtime and must therefore be
//! **capped**, exactly as `NameNormalizer::seen` and `Ingest::undeclared` are, and
//! for the same reason: the key comes from outside and nothing upstream bounds
//! how many distinct ones a misbehaving node can invent.
//!
//! Past the cap [`StrInterner::intern`] returns `None`. Every caller treats that
//! as "no row", which in `crate::clock::OffsetTable` means the publisher cannot
//! corroborate a clock step and in `crate::authority::Authority` means its
//! conflicts are counted but not broken out per pair. Both degradations make an
//! outcome *harder* to reach, never easier — the safe direction, since the
//! outcomes in question are a halted bridge and a dropped transform.

use crate::edgeindex::{buckets_for, mix};

/// A dense publisher id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PublisherId(pub(crate) u32);

impl PublisherId {
    /// As a `Vec` index.
    pub(crate) fn get(self) -> usize {
        self.0 as usize
    }
}

const EMPTY: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
struct Bucket {
    hash: u64,
    entry: u32,
}

/// `&str` → [`PublisherId`], probed without allocating, bounded by a cap.
#[derive(Debug)]
pub(crate) struct StrInterner {
    buckets: Vec<Bucket>,
    mask: usize,
    names: Vec<Box<str>>,
    cap: usize,
}

impl StrInterner {
    /// An interner holding at most `cap` distinct names.
    pub(crate) fn with_cap(cap: usize) -> StrInterner {
        // Shared with `crate::edgeindex`, so the two tables cannot drift about
        // what "full" means — see `buckets_for`.
        let len = buckets_for(cap);
        StrInterner {
            buckets: vec![
                Bucket {
                    hash: 0,
                    entry: EMPTY
                };
                len
            ],
            mask: len - 1,
            names: Vec::new(),
            cap,
        }
    }

    fn hash(s: &str) -> u64 {
        let h = mix(0xcbf2_9ce4_8422_2325, s.as_bytes());
        let h = (h ^ (h >> 32)).wrapping_mul(0x517c_c1b7_2722_0a95);
        h ^ (h >> 29)
    }

    fn find(&self, s: &str) -> Option<usize> {
        let h = Self::hash(s);
        let mut i = (h as usize) & self.mask;
        loop {
            let b = *self.buckets.get(i)?;
            if b.entry == EMPTY {
                return None;
            }
            // Confirmed against the stored name, never on the hash alone — the
            // same argument `crate::edgeindex` makes, and here it decides which
            // *publisher* owns an edge.
            if b.hash == h && &*self.names[b.entry as usize] == s {
                return Some(b.entry as usize);
            }
            i = (i + 1) & self.mask;
        }
    }

    /// The id of `s`, without inserting. Allocates nothing.
    pub(crate) fn id_of(&self, s: &str) -> Option<PublisherId> {
        self.find(s)
            .and_then(|e| u32::try_from(e).ok())
            .map(PublisherId)
    }

    /// The id of `s`, interning it if the cap allows. Allocates once per new
    /// name and nothing thereafter.
    pub(crate) fn intern(&mut self, s: &str) -> Option<PublisherId> {
        if let Some(id) = self.id_of(s) {
            return Some(id);
        }
        if self.names.len() >= self.cap {
            return None;
        }
        let e = u32::try_from(self.names.len()).ok()?;
        self.names.push(Box::from(s));
        let h = Self::hash(s);
        let mut i = (h as usize) & self.mask;
        while self.buckets[i].entry != EMPTY {
            i = (i + 1) & self.mask;
        }
        self.buckets[i] = Bucket { hash: h, entry: e };
        Some(PublisherId(e))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// A name round-trips to its own id and back to its own string.
    ///
    /// Mutant: return `Some(b.entry as usize)` from `find` on a hash match
    /// without comparing the stored name — applied, and this failed on the
    /// `id_of("/other")` assertion, which came back as `/ekf`'s id.
    #[test]
    fn a_name_round_trips() {
        let mut t = StrInterner::with_cap(8);
        let a = t.intern("/ekf").unwrap();
        let b = t.intern("/odom_node").unwrap();
        assert_ne!(a, b);
        assert_eq!(t.intern("/ekf"), Some(a), "interning is idempotent");
        assert_eq!(t.id_of("/ekf"), Some(a));
        assert_eq!(t.id_of("/odom_node"), Some(b));
        assert_eq!(t.id_of("/other"), None);
    }

    /// **Past the cap, a new name gets no id**, and an already-interned one still
    /// resolves.
    ///
    /// The degradation has to be that way round: a publisher that already has a
    /// row keeps working, and a new one is simply not tracked. Refusing the
    /// *known* ones instead would make a full table lose the information it
    /// already had.
    ///
    /// Mutant: drop the `self.names.len() >= self.cap` guard — applied, and this
    /// failed at `Some(PublisherId(4))` where `None` was asserted.
    #[test]
    fn the_cap_refuses_new_names_and_keeps_old_ones() {
        let mut t = StrInterner::with_cap(4);
        let first = t.intern("p0").unwrap();
        for i in 1..4 {
            assert!(t.intern(&format!("p{i}")).is_some());
        }
        assert_eq!(t.intern("p4"), None, "past the cap");
        assert_eq!(t.id_of("p0"), Some(first), "a known name still resolves");
    }

    /// Enough distinct names to walk the probe sequence still resolve to
    /// themselves — the linear-probing invariant.
    #[test]
    fn every_name_under_the_cap_resolves_to_itself() {
        let mut t = StrInterner::with_cap(64);
        let ids: Vec<_> = (0..64)
            .map(|i| t.intern(&format!("/node_{i}")).unwrap())
            .collect();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(t.id_of(&format!("/node_{i}")), Some(*id));
        }
    }
}
