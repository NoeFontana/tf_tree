# 0024: population is per-edge at take-up, which is what §7.1 always said

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** implemented — `crates/tf_tree_arena/src/mapped.rs`,
`crates/tf_tree_core/src/arena_view.rs`, `crates/tf_tree/src/tree.rs`,
`crates/tf_tree_bench/tests/population.rs`,
`crates/tf_tree_bench/src/bin/attach_bench.rs`

## Context

`docs/PHASE2.md` §7.1 is NORMATIVE and its title is the whole argument: **"Page
population is per-edge, not per-arena"**. Its three bullets say to map without
`MAP_POPULATE`, to populate an edge's stamp and pose ranges "at
`declare_dynamic`", and to populate the header, frame table, topology blocks and
edge table on attach.

**The middle bullet names a function that no longer exists.**
[`0004`](./0004-builder-time-edge-declaration.md) moved edge declaration to build
time, so there is no `declare_dynamic` to hook. The code resolved that by
populating the *entire* stamp and pose arenas inside
`MappedArena::populate_hot`, with a comment giving the reason: under `0004` the
two regions are "sized to the declared rings exactly", so populating all of them
is populating exactly what was declared.

That reasoning is true and it answers the wrong question. It makes population
per-**arena**, which is the thing the section's title forbids, and it does so on
the largest region in the system: the rings are **99.8%** of a large arena
(`docs/benchmarks/tf2.md`'s cost model — 72 B/slot against 320 B/edge and
144–176 B/frame). So the over-approximation was very nearly the entire resident
cost of attaching.

**What that costs in the case it is worst for is the ordinary case.** An arena is
declared once, for a whole vehicle. A node attaches and reads a handful of
chains. Under per-arena population every such node was charged for every edge on
the vehicle, permanently, from the moment it attached — which is exactly what an
operator sees in `top`.

Measured, 64 dynamic edges of 8192 slots and no headroom, a process taking up 4
of the 64:

| | charged | of arena |
|---|---|---|
| per-arena population (before) | 38 248 448 B | 101% |
| per-edge at take-up (after) | **7 368 704 B** | **19.5%** |

**5.2×**, and it does not decay: the remaining 19.5% is the tables, charged in
full either way, plus the four rings actually in use.

## Decision

**Populate a ring at the moment its edge is taken up, not at attach.** Two
moments, one per role:

- the writer's, at `Tree::claim` — the edge it is about to publish into;
- the reader's, during plan compilation — every edge `compile` walks.

`MappedArena::populate_hot` keeps everything else exactly as it was: header,
frame table, topology blocks, claim table, participant table, edge table and
both counter regions. Only the two ring arenas leave it.

### Why this is not a weakening of §7.1

§7.1's guarantee is **no page fault inside a lookup**, and both new moments are
off the query path by D3 — `Plan::at` is the hot tier and neither `claim` nor
`compile` is in it. The guarantee is now preserved *for its reason* rather than
by populating everything and hoping the superset covers it.

This also means the change moves §7.1 toward its own text rather than away from
it, which is why the amendment below is a repair of a stale bullet and not a
relaxation. The section's normative property — per-edge — is satisfied for the
first time.

### Why take-up and not the used prefix

The obvious alternative is to populate `min(head, capacity)` — the slots that
have actually been written. It was drafted, and it is dead on four independent
grounds, any one disqualifying:

1. **Readers pay, and they pay on the lookup path.** A page the writer faults in
   still needs a per-process PTE in every reader, so each consumer would fault
   inside `Plan::at` on first read into each newly-written page, for the whole
   first lap. That is the property being traded away.
2. **The win expires.** A ring is a ring: every slot becomes used once `head`
   reaches `capacity`. At 10 Hz into `Capacity::history(1000, 10)`'s 16 384 slots
   the saving lasts 27 minutes and is zero thereafter.
3. **It is a no-op where it would matter most.** `build_shared` populates before
   any push, so every `head` is 0 and the creator would populate nothing —
   byte-identical to the mutant `population.rs` exists to catch.
4. **Layering forbids it.** `populate_hot` is in `tf_tree_arena`; `head` lives in
   `EdgeRecord`, in `tf_tree_core`, which *depends on* the arena.

Take-up has none of these. It is permanent rather than first-lap, it costs the
reader nothing on the query path, it is not a no-op at build, and the layering
works out — see below.

### Where the extents come from

`EdgeRecord`'s `stamp_off`/`pose_off`/`capacity` triple is in `tf_tree_core`,
which depends on `tf_tree_arena`, so the arena crate structurally cannot compute
a per-edge extent. `ArenaView::ring_extents` closes it: core computes the byte
ranges, the facade holds both crates and hands them to `MappedArena::populate`.

`ring_extents` shares its bounds check with `ring_of` (both go through the
private `ring_bytes`) rather than repeating it. That triple is foreign input on
every path that maps bytes this process did not write, and two copies of a bound
that must agree is how they stop agreeing.

### `.tft` is untouched

Only a `MappedArena` populates. `Tree::open_frozen` deliberately populates
nothing — a dataloader worker seeks to the four pages its batch needs, and the
win is precisely that the rest costs nothing across sixteen workers. Hooking
population into plan compilation without matching on the backing would have
silently reversed that for every frozen reader that compiles a plan, and
`PHASE5.md` §12 gate 4 is the measurement it would have broken.

## Timing

**Neutral on the gated axis, and the cost that exists moved rather than grew.**
`§11.1` fixture, `taskset -c 2`, 201 attach/lookup cycles, p50:

| row | before | after |
|---|---|---|
| attach (map + validate + populate) | 99 791 ns | **12 389 ns** |
| plan compile, first | 550 ns | 84 297 ns |
| plan compile, repeat (warm) | — | **1 333 ns** |
| **first lookup after attach** | **130 ns** | **130 ns** |

Three points, and the fixture is the worst case for this change because its plan
walks essentially every edge — so it saves no memory here and pays the whole
cost:

- **§7.1's own row does not move.** 130 ns p50, and
  `the_first_lookup_after_attach_does_not_fault` — a fault *count*, not a
  duration — still reads zero.
- **Attach is 8× faster** and first-compile absorbs it. Sum before 100.3 µs,
  after 96.7 µs: on the fixture that gains nothing from the change, it is a wash.
- **The recompile risk is bounded and measured.** A topology change invalidates
  every cached plan, so the next lookup recompiles and re-populates. Re-populating
  resident pages is **1 333 ns**, not the 84 µs of a cold compile —
  `madvise(MADV_POPULATE_READ)` over resident pages is a walk, not a fault. Had
  this come out in tens of microseconds the decision would have been different,
  because a `reparent` would have put that in front of every reader in the
  system.

### A retraction, recorded because it nearly shipped as a finding

The `first lookup after attach` row *did* read 210 ns against 130 ns for several
rounds of measurement, and three separate causes were proposed and refuted: the
2 KiB `Plan` copy introduced by binding the compiled plan to a local (refuted —
the callback form binds nothing and still read 210), the `ring_of`/`ring_bytes`
refactor sitting on the sample path (refuted — un-refactoring it changed
nothing), and the population change itself.

It was **the benchmark**. Bisecting file by file down to a tree with *every
engine file reverted* still read 210 ns. The added "plan compile, repeat" timer
had been placed inside the main loop, and one extra compile per iteration leaves
the branch predictor and caches in a different state for the next iteration's
lookup — moving it after the timed region does not help, because the damage
lands on the iteration that follows. The row now runs as its own pass and the
main loop is byte-identical to what it was before it existed; the reading
returned to 130 ns.

The measurement was the thing that changed, and it was mine. It is written down
here because the first two refuted hypotheses were both plausible enough to have
been asserted without the bisect.

## Test plan

`crates/tf_tree_bench/tests/population.rs` was two-sided and is now three-sided,
because the existing pair has a hole that this change lives in: per-arena
population passes **both** of them — headroom is not declared content, and
everything declared is charged.

| test | property | mutant, run |
|---|---|---|
| `declared_headroom_is_not_charged` | headroom stays cold | restore `MapFlags::POPULATE` ⇒ 100% charged |
| `declared_content_is_charged` | an edge in use is warm | drop `populate_edge_rings` from `Tree::claim` ⇒ 438 272 B of 37 797 888 B, 1% |
| `only_the_edges_this_process_uses_are_charged` | an edge *not* in use is cold | restore the two ring lines in `populate_hot` ⇒ 38 248 448 B, 101% |
| `the_first_lookup_after_attach_does_not_fault` | §7.1's guarantee | drop the population from `Tree::plan` ⇒ 1 minor fault |

**All four mutants were applied and run**, not reasoned about, and each test's
doc comment carries the number its mutant produced. `declared_content_is_charged`
was rewritten to claim its 64 edges rather than having its threshold softened;
softening it would have turned it into the populate-nothing-passes test the
file's own header exists to forbid.

## Consequences

- `docs/PHASE2.md` §7.1's second bullet is amended: it named `declare_dynamic`,
  which `0004` deleted. The replacement names claim and plan compilation.
- §12's *"first access after attach, per-edge population on vs off"* row is still
  not fully produced. There is still no way to attach *without* populating, so
  the "off" arm remains unexpressible — but the `attach` and `plan compile`
  columns now bracket what population costs, which is what the row was for.
- `report.rs`'s `attach_latency` entry gets a much better number, and it should
  not be read as the change being free: the cost moved to first compile.
- **No `FORMAT_VERSION` bump and no `layout_hash` change.** Residency is
  per-process page-table state; nothing crosses the boundary and no region moved.
- The `arena_memory_floor` §9.3 entry is unaffected — it is about a heap arena's
  reservation, which [`0021`](./0021-the-idle-arena-is-resident-because-of-its-alignment.md)
  addressed on a different axis.
