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
//! **One table is left, and it is the one this shape is still right for.**
//! `Ingest::undeclared` is keyed by names that are by definition *not* in the
//! config, so no construction-time table can hold them and the argument above is
//! the whole of what is available. Every other `(parent, child)` map in this
//! crate was keyed by the *declared* topology, which is fixed at construction —
//! so its key set is turned into dense slots once and probed with one hash. See
//! `crate::edgeindex`, and the measurement in its module docs.
//!
//! `undeclared` is also only reached on the drop path, where the two owned
//! normalized names already exist for `Action::UndeclaredEdge`.
//!
//! `crates/tf_tree_bridge/tests/steady_state_alloc.rs` is the gate on this.
//!
//! [`crate::Discovery`] deliberately keeps a flat tuple key: it runs offline
//! over a recording, once, and its map is iterated in key order far more often
//! than it is probed.

use std::collections::BTreeMap;

/// `parent → child → T`. See the module docs for why it is nested.
pub(crate) type ByEdge<T> = BTreeMap<String, BTreeMap<String, T>>;

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
