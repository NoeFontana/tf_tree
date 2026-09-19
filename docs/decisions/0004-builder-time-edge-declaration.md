# 0004: Builder-time edge declaration, arena sized from the declared edges

> **Still authoritative.** Not absorbed into `docs/PROJECT.md` or `docs/PHASE1.md`: this record owns builder-time edge declaration and arena sizing from the declared edges.

**Status:** implemented

**Implementation.** `TreeBuilder`'s `static_edge`/`dynamic_edge` and the absence of any post-build `declare_*` (`crates/tf_tree/src/tree.rs`). `Open::layout_if_creating` takes a `TreeBuilder`, and Python's `open(create=[(parent, child), ...])` takes an edge list, for the same reason: an arena is sized from its declared edges.
**Owner:** @NoeFontana
**Implementation:** _(PR linked as work lands — phase1/plan-api)_

## Decision

**Topology is declared on the `TreeBuilder` before `build()`, which sizes the arena from exactly those declarations via `ArenaLayout::from_edges`.**

- `TreeBuilder` accepts `frame(name)`, `static_edge(parent, child, &iso)` and `dynamic_edge(parent, child, EdgeCfg)`. Static edges reserve zero ring slots; each dynamic edge reserves its own capacity.
- Capacity is `Capacity::slots(n)` (rounded up to a power of two) or `Capacity::history(rate_hz, duration)` = `next_pow2(ceil(rate_hz × duration_secs))`, the documented default idiom.
- Runtime operations (`claim`, `plan`, `guard`, `lookup`, `push`) stay on `Tree`. Only declaration moves to the builder.
- Topology is mutable at runtime only by re-parenting within the declared budget (append-only identity, tombstoning); no capacity is allocated after `build()`.

Post-build declaration was rejected: uniform reservation wastes memory and caps per-edge capacity, and a growable arena violates D4.

## Consequences

- Declaration errors (unknown parent, duplicate edge, capacity overflow) surface at declaration or `build()`, not at first push.
- Verified by a test that a sparse tree's arena size tracks only its dynamic edges and that a per-edge capacity above the old uniform default is honoured.
