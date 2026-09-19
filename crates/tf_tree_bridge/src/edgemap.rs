//! A `(parent, child)` map that can be probed by reference without allocating.
//!
//! `BTreeMap<(String, String), T>` cannot: `Borrow` does not reach inside a
//! tuple, so every probe would build two owned `String`s. The one table this
//! shape still serves is `Ingest::undeclared`, whose keys are by definition not
//! in the config; it is reached only on the drop path. Declared-topology tables
//! use `crate::edgeindex`. Gate: `tests/steady_state_alloc.rs`.

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
