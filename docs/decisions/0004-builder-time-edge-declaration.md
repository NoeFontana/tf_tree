# 0004: Builder-time edge declaration, arena sized from the declared edges

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** _(PR linked as work lands — phase1/plan-api)_

> Refines the *Public API surface* of
> [`0003-phase-1-single-process-core.md`](./0003-phase-1-single-process-core.md).
> It does not change any layout, concurrency, or numeric decision; it resolves an
> internal inconsistency in how the tree is constructed.

## Context

The Phase 1 spec (`0003`) is internally inconsistent about how edges are
declared:

- Its *Public API surface* sketch declares edges **after** `build()`
  (`tree.declare_dynamic(odom, base, EdgeCfg { capacity: 8192, .. })`).
- The arena is a **single fixed-size allocation** whose pose region is
  `sum(per-edge capacity) × 64 B`, fixed when the bytes are allocated, with **no
  growth** ever (D4, load-bearing invariant 3).
- The same section provides `ArenaLayout::from_edges(&[(FrameName, EdgeKind,
  Capacity)])` and states: *"in a real robot only a handful of edges are dynamic,
  so size capacities per edge rather than uniformly."*

These cannot all hold. If edges are declared after the arena is allocated, their
ring capacities must fit space reserved **before** the declarations existed — so
construction must either pre-reserve uniformly (wasteful, and caps each edge at
the reserved size) or collect the declarations **before** allocating.

An initial implementation took the post-build path with uniform
`default_capacity` reservation. It is memory-wasteful for the common sparse tree
(e.g. 256 edges × 4096 slots ≈ 67 MB even with 4 dynamic edges — the exact waste
`0003` warns against) and cannot express a per-edge capacity above
`default_capacity` (it cannot run `0003`'s own `capacity: 8192` example).

## Decision

**The transform tree's topology is declared on the `TreeBuilder`, before
`build()`. `build()` sizes the arena from exactly those declarations via
`ArenaLayout::from_edges`.**

- `TreeBuilder` accepts frames and edges: `frame(name)`, `static_edge(parent,
  child, &iso)`, and `dynamic_edge(parent, child, EdgeCfg)`. Static edges reserve
  **zero** ring slots; each dynamic edge reserves its own capacity.
- `EdgeCfg` capacity may be given directly (`Capacity::slots(n)`, rounded up to a
  power of two) **or** as a retention window (`Capacity::history(rate_hz,
  duration)` → `next_pow2(ceil(rate_hz × duration_secs))`). The window form is the
  documented default idiom; it is how operators reason and what URDF ingestion
  (Phase 5) will feed.
- `build()` computes `ArenaLayout::from_edges(...)`, allocates the `HeapArena`,
  and returns a `Tree`. The arena therefore holds rings **only** for dynamic
  edges, each sized to its own capacity.
- Runtime operations stay post-`build()` on `Tree` and are unchanged: `claim`,
  `plan`, `guard`, `lookup`, `push`. Only *declaration* (which fixes the layout)
  moves to the builder.

The topology remains mutable at runtime only through re-parenting within the
already-declared frame/edge budget (append-only identity, tombstoning); no new
capacity is ever allocated after `build()`.

## Rationale

- **The structure is config-time; only the data is runtime.** Frames, the
  static/dynamic split, and expected rates come from URDF/config at startup. An
  API that discovers structure after allocation models the problem backwards.
- **Heterogeneous rates are intrinsic.** 50 Hz odom, 1 kHz IMU, and a static
  camera mount need different (or zero) ring depths; per-edge sizing is required,
  not an optimization.
- **Embedded memory is a hard constraint.** `from_edges` sizing makes a real
  robot's arena small; uniform reservation is disqualifying on a Jetson.
- **It is the only option that honors D4 without waste.** A fixed, no-growth,
  Phase-2-mappable arena with tight sizing is possible *only* if capacities are
  known before allocation.

Alternatives rejected:

- **Post-build declaration + uniform reservation** — wasteful; caps per-edge
  capacity at the uniform reserve; contradicts the "size per edge" guidance.
- **Post-build declaration + growable arena** — violates D4 (growth invalidates
  reader mappings in Phase 2). Non-starter.

## Consequences

- The facade `TreeBuilder`/`Tree` construction path is (re)written to collect
  declarations and size via `from_edges`. Core (`Plan`, `Guard`, `compile`,
  evaluation, batch) and the entire concurrency/arena stack are untouched.
- Declaration errors (unknown parent, duplicate edge, capacity overflow) surface
  at `build()` / declaration time, not at first push.
- `0003`'s post-build `declare_*` sketch is understood as illustrative and is
  superseded by this construction model.

## Implementation plan

1. Facade: `TreeBuilder` collects frames + static/dynamic edges (`EdgeCfg`,
   `Capacity::{slots,history}`); `build()` → `from_edges` → `HeapArena` → `Tree`.
   Verified by the existing lookup/batch/behavior suites plus a test that a
   sparse tree's arena size tracks only its dynamic edges, and that a per-edge
   `capacity` above the previous `default_capacity` is honored.

## Open questions

None.
