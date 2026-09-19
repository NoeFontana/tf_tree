# 0024: population is per-edge at take-up, which is what §7.1 always said

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** implemented — `crates/tf_tree_arena/src/mapped.rs`,
`crates/tf_tree_core/src/arena_view.rs`, `crates/tf_tree/src/tree.rs`,
`crates/tf_tree_bench/tests/population.rs`,
`crates/tf_tree_bench/src/bin/attach_bench.rs`

## Context

`docs/PHASE2.md` §7.1 is NORMATIVE: **"Page population is per-edge, not
per-arena"**, populating an edge's stamp and pose ranges "at `declare_dynamic`".
[`0004`](./0004-builder-time-edge-declaration.md) deleted `declare_dynamic`, and
`MappedArena::populate_hot` resolved that by populating the *entire* ring arenas:
per-**arena**, charging a node for every edge on the vehicle.

## Decision

**Populate a ring at the moment its edge is taken up, not at attach**:

- the writer's, at `Tree::claim`;
- the reader's, during plan compilation, over every edge `compile` walks.

`MappedArena::populate_hot` keeps the header, frame table, topology blocks, claim
table, participant table, edge table and both counter regions; only the two ring
arenas leave it. §7.1's guarantee is **no page fault inside a lookup**, and both
moments are off the query path by D3.

**Not the used prefix (`min(head, capacity)`)**: readers still fault a PTE per page
inside `Plan::at`, the win expires once `head` reaches `capacity`, and `head`
lives in `tf_tree_core`, above `populate_hot`'s crate.

`ArenaView::ring_extents` computes the ranges in core and the facade hands them to
`MappedArena::populate`; it shares its bounds check with `ring_of` (private
`ring_bytes`), since the offsets are foreign input. **`.tft` is untouched**:
`Tree::open_frozen` populates nothing (`PHASE5.md` §12 gate 4).

## Timing

Neutral on the gated axis; the cost moved to first plan compile (p50):

| row | before | after |
|---|---|---|
| attach | 99 791 ns | 12 389 ns |
| plan compile, first | 550 ns | 84 297 ns |
| first lookup after attach | 130 ns | 130 ns |

## Test plan

`crates/tf_tree_bench/tests/population.rs`:

| test | property | mutant |
|---|---|---|
| `declared_headroom_is_not_charged` | headroom stays cold | restore `MapFlags::POPULATE` |
| `declared_content_is_charged` | an edge in use is warm | drop `populate_edge_rings` from `Tree::claim` |
| `only_the_edges_this_process_uses_are_charged` | an edge *not* in use is cold | restore the ring lines in `populate_hot` |
| `the_first_lookup_after_attach_does_not_fault` | §7.1's guarantee | drop the population from `Tree::plan` |

## Consequences

- `docs/PHASE2.md` §7.1's second bullet names claim and plan compilation. No
  `FORMAT_VERSION` or `layout_hash` change.
