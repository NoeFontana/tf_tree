# 0004: Builder-time edge declaration, arena sized from the declared edges

> **Still authoritative.** Not absorbed into `docs/PROJECT.md` or `docs/PHASE1.md`: this record owns builder-time edge declaration and arena sizing from the declared edges.

**Status:** implemented

**Implementation.** `TreeBuilder`'s `static_edge`/`dynamic_edge` and the absence of any post-build `declare_*` (`crates/tf_tree/src/tree.rs`). `Open::layout_if_creating` takes a `TreeBuilder`, and Python's `open(create=[(parent, child), ...])` takes an edge list, for the same reason: an arena is sized from its declared edges.
**Owner:** @NoeFontana
**Implementation:** _(PR linked as work lands — phase1/plan-api)_

## Decision

**Topology is declared on the `TreeBuilder` before `build()`, which sizes the arena from exactly those declarations via `ArenaLayout::from_edges`.**

- `frame(name)`, `static_edge(parent, child, &iso)` (zero ring slots) and `dynamic_edge(parent, child, EdgeCfg)`.
- Capacity is `Capacity::slots(n)` (power of two) or `Capacity::history(rate_hz, duration)` = `next_pow2(ceil(rate_hz × duration_secs))`.
- Runtime operations stay on `Tree`; no capacity is allocated after `build()` (D4).
