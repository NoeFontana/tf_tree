# 0024: population is per-edge at take-up, which is what §7.1 always said

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** implemented — `crates/tf_tree_arena/src/mapped.rs`,
`crates/tf_tree_core/src/arena_view.rs`, `crates/tf_tree/src/tree.rs`,
`crates/tf_tree_bench/tests/population.rs`,
`crates/tf_tree_bench/src/bin/attach_bench.rs`

## Context

`docs/PHASE2.md` §7.1 is NORMATIVE: **"Page population is per-edge, not
per-arena"** — map without `MAP_POPULATE`, populate an edge's stamp and pose
ranges "at `declare_dynamic`", populate the header, frame table, topology blocks
and edge table on attach. [`0004`](./0004-builder-time-edge-declaration.md)
deleted `declare_dynamic`, and the code resolved that by populating the *entire*
stamp and pose arenas in `MappedArena::populate_hot`: per-**arena**, on the
largest region in the system (rings are **99.8%** of a large arena,
`docs/benchmarks/tf2.md`). A node that reads a handful of chains was charged for
every edge on the vehicle.

Measured, 64 dynamic edges of 8192 slots and no headroom, a process taking up 4:

| | charged | of arena |
|---|---|---|
| per-arena population (before) | 38 248 448 B | 101% |
| per-edge at take-up (after) | **7 368 704 B** | **19.5%** |

**5.2×**; the remaining 19.5% is the tables plus the four rings in use.

## Decision

**Populate a ring at the moment its edge is taken up, not at attach**:

- the writer's, at `Tree::claim`;
- the reader's, during plan compilation — every edge `compile` walks.

`MappedArena::populate_hot` keeps the header, frame table, topology blocks, claim
table, participant table, edge table and both counter regions; only the two ring
arenas leave it.

**Not a weakening of §7.1.** Its guarantee is **no page fault inside a lookup**,
and both moments are off the query path by D3 (`Plan::at` is the hot tier).

### Why take-up and not the used prefix (`min(head, capacity)`)

1. **Readers pay on the lookup path**: a page the writer faults in still needs a
   PTE per reader, faulting inside `Plan::at` for the whole first lap.
2. **The win expires**: every slot is used once `head` reaches `capacity` (27
   minutes at 10 Hz into `Capacity::history(1000, 10)`).
3. **A no-op where it matters**: `build_shared` populates before any push, so
   every `head` is 0.
4. **Layering**: `populate_hot` is in `tf_tree_arena`; `head` is in `EdgeRecord`,
   in `tf_tree_core`, which depends on the arena.

### Where the extents come from

`ArenaView::ring_extents` computes the byte ranges in core; the facade holds both
crates and hands them to `MappedArena::populate`. It shares its bounds check with
`ring_of` (both via the private `ring_bytes`), since the `stamp_off`/`pose_off`/
`capacity` triple is foreign input on every path that maps bytes this process did
not write.

### `.tft` is untouched

Only a `MappedArena` populates. `Tree::open_frozen` populates nothing — a
dataloader worker seeks to the pages its batch needs, and hooking population into
plan compilation without matching on the backing would reverse that (`PHASE5.md`
§12 gate 4 is the measurement it would break).

## Timing

**Neutral on the gated axis; the cost moved rather than grew.** `§11.1` fixture,
`taskset -c 2`, 201 attach/lookup cycles, p50 (the worst case for this change: its
plan walks essentially every edge):

| row | before | after |
|---|---|---|
| attach (map + validate + populate) | 99 791 ns | **12 389 ns** |
| plan compile, first | 550 ns | 84 297 ns |
| plan compile, repeat (warm) | — | **1 333 ns** |
| **first lookup after attach** | **130 ns** | **130 ns** |

- §7.1's own row does not move, and
  `the_first_lookup_after_attach_does_not_fault` (a fault *count*) still reads
  zero.
- Attach is 8× faster and first-compile absorbs it (sum 100.3 µs before, 96.7 µs
  after).
- **The recompile risk is bounded**: a topology change invalidates every cached
  plan, but re-populating resident pages is 1 333 ns, not a cold compile's 84 µs.
  In tens of microseconds the decision would have been different.

## Test plan

`crates/tf_tree_bench/tests/population.rs` is three-sided, because per-arena
population passes the first two of the old pair. Each mutant was applied and run,
and each test's doc comment carries its number:

| test | property | mutant |
|---|---|---|
| `declared_headroom_is_not_charged` | headroom stays cold | restore `MapFlags::POPULATE` ⇒ 100% charged |
| `declared_content_is_charged` | an edge in use is warm | drop `populate_edge_rings` from `Tree::claim` ⇒ 1% |
| `only_the_edges_this_process_uses_are_charged` | an edge *not* in use is cold | restore the ring lines in `populate_hot` ⇒ 101% |
| `the_first_lookup_after_attach_does_not_fault` | §7.1's guarantee | drop the population from `Tree::plan` ⇒ 1 minor fault |

## Consequences

- `docs/PHASE2.md` §7.1's second bullet is amended: it names claim and plan
  compilation instead of the deleted `declare_dynamic`.
- §12's *"per-edge population on vs off"* row is not fully produced (no way to attach
  without populating), but the `attach` and `plan compile` columns bracket the cost;
  `report.rs`'s `attach_latency` improves because the cost moved to first compile.
- **No `FORMAT_VERSION` bump and no `layout_hash` change**: residency is per-process
  page-table state. The `arena_memory_floor` §9.3 entry is unaffected (see
  [`0021`](./0021-the-idle-arena-is-resident-because-of-its-alignment.md)).
