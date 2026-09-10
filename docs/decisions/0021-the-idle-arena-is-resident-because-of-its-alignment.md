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

### Step 4 was not a one-line change: the falsifier it names could not fire

**Measured 2026-09-10.** Step 4 above reads *"give `idle_arena_resident_bytes` a
direction and a tolerance, and regenerate the baseline in the same commit —
verified by `just bench-check` passing, **and by a deliberate revert of step 2
making it fail**."* The second half is the important one, and it could not
happen. A direction on that metric gated **nothing at all**:

* `arena_memory_floor` is a `where_we_are_worse` entry, not a row.
  `baseline::compare` diffed the **set** of `where_we_are_worse` ids and never
  looked inside one — `compare_column` was only ever called on a row's `tf_tree`
  and `tf2` maps. A `Worse` entry's `metrics` were written to the artifact, read
  by nobody, and compared against nothing.
* `Report::validate`'s anti-rot rule — *a thing that prints numbers must give at
  least one of them a direction, or nothing in it can ever be gated* — was
  written over `self.rows` only. `arena_memory_floor` printed five numbers from
  the day it was measured and gave none of them a direction, and no check
  minded.
* `Comparison::compared_nothing()` did not cover the hole either, and it is
  instructive why: the *row* `differential_agreement` kept `checked` at 1, so
  the report was never a zero-comparison run even though this entry contributed
  nothing. A whole-artifact anti-vacuity check does not catch a per-entry one.

**Run, not argued.** With `HeapArena::new`'s allocation request reverted to
`from_size_align(len, 64)` — step 2 undone, the idle arena back to ~100%
resident, `idle_arena_resident_bytes` at **2 412 544 B** — the committed gate on
`main` printed:

```text
regression gate against crates/tf_tree_bench/baseline/results.json (PHASE5 §10):
  PASS — 1 directional metric held.
```

Exit 0. The defect this record exists to remove passes the gate this record's own
step 4 says will catch it. That is `docs/PROJECT.md` §6's anti-vacuity smell
sitting inside the record that names it.

### What step 4 actually took

1. **`baseline::compare` descends into `where_we_are_worse` entries**
   (`compare_worse`). `parse_metrics` and `compare_column` were keyed on
   `(row id, column)`; both now take a pre-formatted `what` prefix, so a row
   column and a §9.3 entry share one spelling of the six diagnostics instead of
   growing a second.
2. **`Report::validate` applies the direction rule to `Worse` entries**, and to
   their tolerances. Scoped to a host whose **memory axis passes**, because
   `worse_entries` withholds this entry's Pss metrics where Pss cannot be read —
   and turning "this machine has no `smaps_rollup`" into "there is no artifact"
   would be a worse answer than the one the rule prevents. The memory axis is
   the only fitness axis that reaches a `Worse` entry today; a second one belongs
   in that predicate.
3. **A missing gated metric classifies its own absence, as a failure or a
   refusal.** A `Worse` entry carries no per-metric sensitivity, so the gate
   cannot say which axis withheld a number — but the report says which axes this
   host failed. On a host whose memory axis passed, an absent gated metric is a
   **failure**. On one whose memory axis failed it is a **refusal**: a note
   saying the comparison did not run, which is the honest answer on a machine
   where every memory *row* is `unavailable` for the same reason, and which is
   what `Report::validate` already does about the same host. The first revision
   of this made it a failure either way and argued that a downgrade would leave
   the gate green — true, and outweighed by the gate contradicting `validate`
   about one host and going permanently red on it. Every machine that actually
   runs `just bench-check`, CI's `bench-gate` included, is on the failure side.

   Two smaller corrections in the same place, both found by review rather than
   by me. The absence message claimed a passing axis proved the absence was the
   code's — it does not: `measure_idle_arena_resident` also withholds the figure
   when the whole-process Pss delta comes out non-positive, which is not a
   fitness failure. And the message said *"which the baseline gates"* of every
   absent key including the informational ones, so a host that cannot read Pss
   got three identical "the baseline gates this" failures for one gated metric
   and two context ones.

   `Report::validate` had the mirror-image bug and it was worse: on a fit host
   whose Pss delta came out non-positive, the floor entry would be two
   informational metrics, the new direction rule would fire, and `bench_report`
   would write **no artifact at all** — telling the author to add a direction the
   code already has. `Worse::metrics_withheld` closes it. It is deliberately
   Rust-side only: a JSON field is a `SCHEMA` bump, and a bump invalidates
   `baseline/results-tf2.json`, which can only be regenerated inside
   `docker/tf2`.

5. **The second committed baseline had to be brought along, and could not be
   regenerated.** `baseline/results-tf2.json` carried the same entry as
   `informational`, so descending into `where_we_are_worse` made
   `just tf2-bench-check` fail deterministically on a direction mismatch caused
   by a commit that cannot run that recipe. Its `drift` and `tolerance` were
   therefore edited by hand — defensible because they are a policy choice and
   not a measurement, and **no number in that file was touched**. Its *value* is
   pre-step-2 (`2408448`), so the bound it sets is ~9.6 MB and the gate is real
   but weak there until somebody runs `just tf2-bench-baseline-update` in the
   container. That is disclosed in the recipe's own comment rather than left to
   be discovered.
4. **The tolerance is 300%, and that is not laziness.** `RESIDENCY_SLACK` carries
   the argument; the short form is that the metric is *six pages* of a
   whole-process, page-quantised Pss delta, and that CI's `bench-gate` job runs
   this same comparison on `ubuntu-latest` against a baseline cut here — so this
   is the **first host-dependent number the gate has ever compared across two
   machines**, and the band has to cover a different libc or it becomes a gate
   that fails for the machine. Thirty consecutive runs here returned 24 576 B
   bit-identically and six more under full CPU load returned the same, so the
   observed spread is zero and all of the slack is headroom. The failure it
   guards is 588 pages — **24x the bound** it sets.

**The falsifier, re-run against the finished gate:**

```text
where_we_are_worse `arena_memory_floor`.idle_arena_resident_bytes regressed:
  2412544 B against a baseline of 24576 (+9716.7%), past the 300% the baseline
  allows (bound 98304)
```

Exit 1. And `just bench-check` on the real tree: **`PASS — 2 directional metrics
held`**, up from one.

**The number that ships and the number every document explains must be the same
one, and for a while they were not.** During the falsifier experiment above the
whole of `report.rs` was restored from a copy taken before `RESIDENCY_SLACK` was
raised, so `1.0` shipped while this record, the constant's own doc and the
pasted gate output all said 300% — and the baseline was then regenerated *from
the reverted code*, which made the committed file agree with the wrong number
and every gate pass. `just bench-check` structurally cannot catch that: the
tolerance it reads is the baseline's own, by design. So
`tests/baseline_file.rs::the_committed_baseline_gates_the_idle_arena_residency_at_this_builds_tolerance`
reads the committed file and the constant and asserts they agree, and it runs in
`just test`. Mutant applied: change `RESIDENCY_SLACK` without regenerating, and
it fails naming both numbers.

**What regenerating the baseline showed as a side effect.** The committed
`results.json` was cut on 2026-08-14 under rustc 1.95; the regenerated one is
2026-09-10 under 1.97.1. Across 27 days, a compiler bump and everything that
landed between: **not one metric value moved, not one row status moved, and no id
appeared or vanished.** The only semantic difference in the whole file is this
record's `drift`/`tolerance` pair. Every other line of the diff is prose the gate
ignores by construction, plus two provenance keys (`build_lto`,
`transparent_hugepage_shmem`) added since — neither in `PORTABLE_FACTS`, which is
why a baseline predating them still compared cleanly.

**Open question 2 is unchanged and now has a gate.** That question asked whether
`calloc`'s laziness may be relied on or is merely observed, and answered
*observed — and the report keeps measuring it*. Until now nothing held the
measurement to anything. `idle_arena_resident_bytes` is that hold, and a libc
that stops eliding the fill fails it by two orders of magnitude rather than being
noticed in a year.
