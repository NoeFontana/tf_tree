//! A map keyed on `(parent, child)` that can be probed without allocating.
//!
//! # Why not `BTreeMap<(String, String), T>`
//!
//! Because it cannot be looked up by reference. `BTreeMap::get` takes `&Q` where
//! `K: Borrow<Q>`, and `Borrow` does not reach inside a tuple: there is no `Q`
//! such that `(String, String): Borrow<Q>` and `Q` can be built from
//! `(&str, &str)` for free. Every probe therefore had to construct two owned
//! `String`s and drop them again — to answer a question about memory the map
//! was already holding.
//!
//! That is affordable in a config parser and not on the ingest path. The bridge
//! runs [`crate::Ingest::offer`] on **every** transform a robot publishes, and
//! §5.8 put two such lookups on it (is the edge declared; who owns it). At
//! twenty edges and 1 kHz the tuple keys cost tens of thousands of allocations a
//! second for nothing.
//!
//! Nesting `parent → child → T` makes both levels probe with `&str` through the
//! ordinary `String: Borrow<str>` impl. Allocation then happens once per edge,
//! when it is first inserted, which is what "allocate at construction" means for
//! a table whose key set is fixed by the topology file.
//!
//! `crates/tf_tree_bridge/tests/steady_state_alloc.rs` is the gate on this.
//!
//! [`crate::Discovery`] deliberately keeps a flat tuple key: it runs offline
//! over a recording, once, and its map is iterated in key order far more often
//! than it is probed.

use std::collections::BTreeMap;

/// `parent → child → T`. See the module docs for why it is nested.
pub(crate) type ByEdge<T> = BTreeMap<String, BTreeMap<String, T>>;

/// Probe by reference. Allocates nothing.
pub(crate) fn lookup<'a, T>(m: &'a ByEdge<T>, parent: &str, child: &str) -> Option<&'a T> {
    m.get(parent).and_then(|c| c.get(child))
}

/// Probe by reference for mutation. Allocates nothing.
pub(crate) fn lookup_mut<'a, T>(
    m: &'a mut ByEdge<T>,
    parent: &str,
    child: &str,
) -> Option<&'a mut T> {
    m.get_mut(parent).and_then(|c| c.get_mut(child))
}

/// Insert, allocating the two keys. Call only on the first sight of an edge.
pub(crate) fn insert<T>(m: &mut ByEdge<T>, parent: &str, child: &str, v: T) {
    m.entry(parent.to_string())
        .or_default()
        .insert(child.to_string(), v);
}

/// Every `(parent, child, &T)`, in key order.
pub(crate) fn iter<T>(m: &ByEdge<T>) -> impl Iterator<Item = (&str, &str, &T)> {
    m.iter()
        .flat_map(|(p, cs)| cs.iter().map(move |(c, v)| (p.as_str(), c.as_str(), v)))
}
