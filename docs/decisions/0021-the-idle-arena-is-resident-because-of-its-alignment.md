# 0021: the idle arena is resident because of its alignment, not its design

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** implemented — `crates/tf_tree_arena/src/heap.rs`, `crates/tf_tree_arena/tests/heap_alignment.rs`

## Context

`docs/PHASE5.md` §9.3 requires the benchmark artifact to report **where `tf_tree`
is worse**, and the first of its four entries is the arena memory floor:

> A tf_tree arena is fixed-capacity and allocated up front, so an idle tree costs
> its full size from the first second. A tf2 `BufferCore` starts near empty and
> grows into whatever the stream actually contains, so on a robot that publishes
> far less than it declared, tf2 uses less memory and `tf_tree` is simply worse.

That entry carried one number, `idle_arena_bytes = 2 405 696` for the stated
64-frame / 64-edge / 32768-slot geometry, and the entry said of it — correctly —
that it was *"arithmetic on the layout, not a measurement"*. It was
`ArenaLayout::total_size()`, which is the last region's `offset + size`: what the
arena **reserves**.

A reservation is not a footprint. Pages become resident when they are touched,
and an idle arena has never read or written its pose region. So the entry was
measured, expecting the resident figure to come out far below the reserved one
and the row to shrink.

**It did not shrink. The two numbers are equal.** An idle `HeapArena` is ~100%
resident, and the reason has nothing to do with fixed capacity:

`HeapArena::new` asks for **64-byte alignment** — `PoseSlot` is
`#[repr(C, align(64))]`, exactly one cache line, and that alignment is load-bearing
for the false-sharing behaviour every concurrency number in the repository rests
on. Rust's `System` allocator routes `alloc_zeroed` to `calloc` **only when the
requested alignment is at most `MIN_ALIGN`** (16 on x86-64). Above that it falls
back to `posix_memalign` followed by an explicit zero-fill — and that fill
touches every page.

`calloc` for an allocation this size hands back fresh `mmap` pages, which the
kernel guarantees are already zero and never materialises until touched. The
hand-rolled fill throws that away.

Measured directly, same size, same zeroed allocation, only the alignment differs:

| Request | Resident (Pss delta) |
|---|---|
| `alloc_zeroed`, align **16** | **4 KiB** |
| `alloc_zeroed`, align **64** | **2356 KiB** |

Reserved is 2349 KiB, so align-64 is 100.3% resident and align-16 is 0.2%.

**This is `HeapArena` only.** `MappedArena` creates a `memfd`, `ftruncate`s it and
maps it; a memfd is freshly zeroed by the kernel and its pages are already
demand-faulted, which `mapped.rs:178`'s own SAFETY comment states. The
shared-memory path does not have this defect. The affected path is the default,
single-process one — and it is the path the `arena_memory_floor` claim is about.

## Decision

**Allocate the heap arena at an alignment `alloc_zeroed` will pass to `calloc`,
and satisfy the 64-byte requirement by hand.**

`HeapArena::new` requests `Layout::from_size_align(len + 63, 16)`, then offsets
the returned pointer up to the next 64-byte boundary and uses that as the arena
base. The allocation's own pointer and layout are retained for `dealloc`, which
must free the *original* pointer with the *original* layout.

The 64-byte alignment of the base is unchanged and stays an invariant; what
changes is how it is obtained. The cost is at most 63 bytes per arena.

Verified before proposing, at the §9.3 geometry:

| | Resident | Base alignment |
|---|---|---|
| Current — `from_size_align(len, 64)` | 2352 KiB | `base % 64 == 0` |
| Proposed — `from_size_align(len + 63, 16)` + manual offset | **8 KiB** | `base % 64 == 0` |

**What this does and does not buy.** Pages still become resident as they are
touched, so a *populated* arena costs what it actually holds — this changes
nothing about a tree that is being published into at its declared rate. The win
is exactly on the case §9.3's entry describes and no other: **a robot that
declares far more capacity than it publishes into**. That is the case where the
row said `tf_tree` was worse than tf2, and it is the case where it stops being.

The §9.3 entry is **kept either way.** Reserving address space is still a cost
tf2 does not pay: it constrains a machine configured with strict overcommit, and
a fixed-capacity arena still cannot grow. The entry's numbers change; its claim
does not disappear.

## Rationale

**Why not drop the 64-byte alignment.** It is what makes `PoseSlot` one cache
line, and false sharing between adjacent slots is the thing the seqlock design
most needs not to have. A `const` assert pins `size_of::<PoseSlot>() == 64` and
`align_of::<PoseSlot>() == 64`. Not a candidate.

**Why not `mmap` the heap arena directly.** It would work and is what `calloc`
does underneath, but it puts an OS call in `tf_tree_arena`, which is
`no_std + alloc` and whose unsafe budget under
[`0007`](./0007-the-unsafe-budget-and-the-c-abi.md) is *the arena's raw memory* —
not *the OS*, which is `tf_tree_ipc`'s boundary. Over-allocating stays inside the
crate's existing budget and needs no new kind of boundary.

**Why not leave it alone and just report the number.** That was the first
instinct and it is defensible: the row is honest either way now that it is
measured. It loses because the measurement showed the cost is ~293× larger than
it needs to be, on the one axis where `tf_tree` loses to tf2 outright, for a
reason that is an allocator implementation detail rather than a design
consequence. A cost we chose is worth reporting; a cost we did not notice is
worth removing.

**Why `+ 63` and not `+ 64`.** The offset needed is `(64 - (raw % 64)) % 64`,
which is at most 63. `+ 64` would also be correct and wastes one more byte.

## Consequences

- `HeapArena` gains a second stored value: the allocation's own base pointer and
  layout, distinct from the arena base. **`dealloc` must be given the original
  pointer and the original layout** — freeing the offset pointer is undefined
  behaviour, and this is the one way to get this change wrong. It wants an
  explicit test and a `// SAFETY:` comment saying which pointer is which.
- `idle_arena_resident_bytes` in the report becomes a number that can regress.
  Once it drops, it should be **gated** (`lower_is_better`) so that a future
  change which reintroduces an eager fill fails `just bench-check` rather than
  being noticed in a year. It is a `Memory` row under `PHASE5.md` §9.3's
  amendment, so this host can gate it.
- The §9.3 statement text is rewritten again, to the post-fix numbers.
- Miri runs `tf_tree_arena`; a manually offset pointer is exactly the kind of
  thing it is there to check, and `just miri` must stay clean.

## Implementation plan

1. **Pin the current behaviour first.** A test in `tf_tree_arena` asserting the
   arena base is 64-byte aligned, so the refactor cannot silently lose it —
   verified by `cargo nextest run -p tf_tree_arena`.
2. **Over-allocate and offset in `HeapArena::new`**, storing the raw pointer and
   raw layout for `dealloc`; `// SAFETY:` naming which pointer each call uses.
   Verified by step 1's test still passing, plus a test that allocates and drops
   many arenas under `just miri` with no leak or UB report.
3. **Measure it through the artifact.** `idle_arena_resident_fraction` in
   `target/bench-report/results.json` drops from ~1.0 to under 0.05 — verified by
   `cargo run --release -p tf_tree_bench --bin bench_report`.
4. **Gate it.** Give `idle_arena_resident_bytes` a direction and a tolerance, and
   regenerate the baseline in the same commit — verified by `just bench-check`
   passing, and by a deliberate revert of step 2 making it fail.
5. **Rewrite §9.3's `arena_memory_floor` statement** to the post-fix numbers,
   keeping the reservation cost as the surviving claim — verified by
   `report::tests` and by reading it.

## Open questions

1. **Should the offset be conditional on size?** For a small arena the 63 wasted
   bytes are a larger relative overhead, and `calloc` below the mmap threshold
   memsets anyway, so the change buys nothing there. Options: always offset (one
   code path, simplest, correct); or offset only above some size, which is a
   threshold nobody has measured and a second path to test. Leaning "always" —
   but the mmap threshold is a glibc tunable (`M_MMAP_THRESHOLD`, 128 KiB by
   default and dynamic), so "the size where this starts helping" is not a
   constant we control, and that argues against ever branching on it.

2. **Is `calloc`'s laziness something we may rely on, or is it observed
   behaviour?** The kernel guarantees fresh anonymous pages are zero; glibc's
   `calloc` documents that it *may* skip the fill for freshly-mapped memory but
   does not promise which allocations qualify, and a different libc (musl, which
   the `unknown-linux-musl` release targets use) may differ. The correctness of
   the change does not depend on it — a memset would only cost what is paid
   today. But the *benefit* does, so the report should keep measuring rather than
   asserting it, and the number should be gated on this host rather than written
   into the docs as a constant.

3. **Does `FrozenArena` share the defect?** It is `mmap`-backed like
   `MappedArena` and so probably not, but it was not measured for this record and
   `PHASE5.md` §12 gate 4 is stated about exactly this kind of sharing. Settle it
   with the same instrument before that gate is claimed.

## Resolution — measured after implementing

The three open questions, answered.

**1. Conditional on size? No — always offset.** The leaning in the question was
right and for the reason it gave: `M_MMAP_THRESHOLD` is a glibc tunable, 128 KiB
by default and *dynamic* (it adapts to the freeing pattern at run time), so "the
size where this starts helping" is not a constant this crate could branch on
correctly. One code path, 63 bytes, no threshold to test or to get wrong. The
smallest geometry in `tests/heap_alignment.rs` is a 1-frame/1-edge/1-slot arena
and pays those 63 bytes; nothing about that is worth a second path.

**2. Relied on, or observed? Observed — and the report keeps measuring it.**
`calloc`'s laziness is not promised for any particular allocation, and musl may
differ from glibc. Correctness does not depend on it: a libc that memsets anyway
costs exactly what was already being paid. The *benefit* does depend on it, so
`arena_memory_floor` reports `idle_arena_resident_fraction` as a measurement on
the host that ran it, rather than the docs asserting a constant. That row is the
gate on this question.

**3. Does `FrozenArena` share the defect? No.** It is `mmap`-backed like
`MappedArena`, whose pages are demand-faulted by construction, and §12 gate 4 has
since been measured directly (`just gate4`): 16 workers on one 338 MiB `.tft`
cost 1.024× one worker, with **0.37 MiB private per worker**. A 100%-resident
private copy per process would have made that ratio ~16. The gate settles it.

### What it actually bought

| | before | after |
|---|---|---|
| `idle_arena_resident_bytes` (§9.3 geometry) | 2 408 448 B | **24 576 B** |
| `idle_arena_resident_fraction` | 1.0000 | **0.0102** |
| `scale_sweep` `rss_over_arena`, fleet_16 | 1.001 | **0.672** |
| `scale_sweep` `rss_over_arena`, fleet_64 | 1.008 | **0.678** |
| `scale_sweep` `rss_over_arena`, humanoid | 0.094 | **0.026** |
| Pss delta, §11.1 fixture, native-vs-native | 1 752 KiB | **1 272 KiB** |

**The last row is the one that mattered.** `just tf2-native-footprint` had tf2 at
1 332 KiB and `tf_tree` at 1 752 — the only instrument on which tf_tree lost, and
the one an operator actually reads. It is now **1 272 against 1 332**, so the
sign is reversed. `heap_bytes` did not move (1 411 136, bit-identical), which is
the cross-check that this changed residency and not allocation.

The saving on the fixture was **464 KiB against a prediction of 466 KiB** — the
6 472 declared-but-never-published slots at 72 B. Predicting the figure before
measuring it is the only reason to trust that the mechanism is understood.

**No timing change, proven not asserted.** `just bench-ab` over the whole
`scale_sweep` catalogue: *"78 compared: 19 info, 58 noise, 1 unmeasured. No
regression."* `just bench-check` PASS. The change is allocation-time only and
touches no read path.

**Miri is the gate that matters here.** `dealloc` must be given the allocation's
own pointer, not the offset one. Mutating `Drop` to free `self.ptr` passes all
four tests natively and aborts under Miri with *"deallocating 0x… which does not
point to the beginning of an object"* — run, not assumed.
