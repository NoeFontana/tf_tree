# 0021: the idle arena is resident because of its alignment, not its design

**Status:** implemented — all five plan steps have landed and been verified,
step 4 last, on 2026-09-10. Frozen: a correction goes in
[`decisions/README.md`](./README.md), not here.
**Owner:** @NoeFontana
**Implementation:** `crates/tf_tree_arena/src/heap.rs`,
`crates/tf_tree_arena/tests/heap_alignment.rs` (steps 1–3);
`crates/tf_tree_bench/src/baseline.rs`, `crates/tf_tree_bench/src/report.rs`,
`crates/tf_tree_bench/baseline/results.json` (step 4 — read *Step 4 was not a
one-line change* below before assuming it was the one line it reads like)

## Decision

**Allocate the heap arena at an alignment `alloc_zeroed` will pass to `calloc`, and satisfy the 64-byte requirement by hand.** `HeapArena::new` requests `Layout::from_size_align(len + 63, 16)` and offsets the pointer up to the next 64-byte boundary. `dealloc` must free the *original* pointer with the *original* layout (freeing the offset pointer is UB; `just miri` must stay clean).

## Implementation plan

1. Pin 64-byte base alignment (`crates/tf_tree_arena/tests/heap_alignment.rs`).
2. Over-allocate and offset in `HeapArena::new`, with a `// SAFETY:` naming which pointer each call uses.
3. Measure through the artifact: `idle_arena_resident_fraction` under 0.05.
4. Gate it: `idle_arena_resident_bytes` gets a direction (`lower_is_better`) and tolerance, baseline regenerated in the same commit.
5. Rewrite §9.3's `arena_memory_floor` statement.

### Step 4 was not a one-line change: the falsifier it names could not fire

`baseline::compare` now descends into `where_we_are_worse` entries (`compare_worse`); `Report::validate` applies the direction rule to `Worse` entries on a host whose memory axis passes; a missing gated metric fails there and refuses elsewhere; the tolerance is 300% (`RESIDENCY_SLACK`). `crates/tf_tree_bench/tests/baseline_file.rs::the_committed_baseline_gates_the_idle_arena_residency_at_this_builds_tolerance` asserts the committed tolerance equals `RESIDENCY_SLACK`.
