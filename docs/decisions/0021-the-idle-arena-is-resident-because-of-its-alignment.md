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

## Context

`PHASE5.md` §9.3's `arena_memory_floor` entry reported the reserved size. An idle `HeapArena` was ~100% resident anyway: `PoseSlot` needs 64-byte alignment, Rust's `System` allocator routes `alloc_zeroed` to `calloc` only at alignment <= `MIN_ALIGN` (16), and above that it uses `posix_memalign` plus an explicit zero-fill that touches every page. `MappedArena` (memfd) and `FrozenArena` (mmap) do not have the defect.

## Decision

**Allocate the heap arena at an alignment `alloc_zeroed` will pass to `calloc`, and satisfy the 64-byte requirement by hand.**

`HeapArena::new` requests `Layout::from_size_align(len + 63, 16)`, offsets the returned pointer up to the next 64-byte boundary and uses that as the arena base. The allocation's own pointer and layout are retained, and `dealloc` must free the *original* pointer with the *original* layout. The base stays 64-byte aligned; the cost is at most 63 bytes per arena, always offset (no size threshold).

A populated arena costs what it holds; the win is a robot that declares more capacity than it publishes. The §9.3 entry is kept: reserving address space is still a cost tf2 does not pay.

## Rationale

- The 64-byte alignment is not droppable: a `const` assert pins `size_of/align_of::<PoseSlot>() == 64`.
- No direct `mmap`: it would put an OS call in `tf_tree_arena`, outside its `0007` budget.
- `calloc`'s laziness is observed, not promised (musl may differ). The report measures `idle_arena_resident_fraction` per host rather than asserting a constant.

## Consequences

- Freeing the offset pointer is undefined behaviour. Mutating `Drop` to free `self.ptr` passes natively and aborts under Miri; `just miri` must stay clean.
- `idle_arena_resident_bytes` is gated (`lower_is_better`), so reintroducing an eager fill fails `just bench-check`.

## Implementation plan

1. Pin 64-byte base alignment (`crates/tf_tree_arena/tests/heap_alignment.rs`).
2. Over-allocate and offset in `HeapArena::new`, with a `// SAFETY:` naming which pointer each call uses. Result: `idle_arena_resident_bytes` 2 408 448 B to 24 576 B, `idle_arena_resident_fraction` 1.0 to 0.0102.
3. Measure through the artifact: `idle_arena_resident_fraction` under 0.05.
4. Gate it: give `idle_arena_resident_bytes` a direction and tolerance and regenerate the baseline in the same commit; verified by `just bench-check` and by a revert of step 2 making it fail.
5. Rewrite §9.3's `arena_memory_floor` statement to the post-fix numbers, keeping the reservation cost as the surviving claim.

## Open questions

Resolved: always offset; laziness observed and measured; `FrozenArena` does not share the defect (`just gate4`, 16 workers cost 1.024x one worker).

### Step 4 was not a one-line change: the falsifier it names could not fire

A direction on `idle_arena_resident_bytes` gated nothing: `baseline::compare` diffed the *set* of `where_we_are_worse` ids and never looked inside an entry, and `Report::validate`'s direction rule covered `self.rows` only. Step 4 therefore took:

1. `baseline::compare` descends into `where_we_are_worse` entries (`compare_worse`); `parse_metrics` and `compare_column` take a pre-formatted `what` prefix.
2. `Report::validate` applies the direction rule to `Worse` entries and their tolerances, scoped to a host whose memory axis passes. `Worse::metrics_withheld` covers a non-positive Pss delta and is Rust-side only (a JSON field would be a `SCHEMA` bump invalidating `baseline/results-tf2.json`).
3. A missing gated metric is a failure on a host whose memory axis passed and a refusal on one whose memory axis failed.
4. The tolerance is 300% (`RESIDENCY_SLACK` carries the argument): a six-page, page-quantised Pss delta compared across two machines. The guarded failure is 24x the bound.
5. `baseline/results-tf2.json` had its `drift` and `tolerance` edited by hand (no number touched); its value predates step 2 until `just tf2-bench-baseline-update` runs in the container.

`crates/tf_tree_bench/tests/baseline_file.rs::the_committed_baseline_gates_the_idle_arena_residency_at_this_builds_tolerance` asserts the committed baseline's tolerance equals `RESIDENCY_SLACK`; `just bench-check` cannot, because it reads the baseline's own tolerance.
