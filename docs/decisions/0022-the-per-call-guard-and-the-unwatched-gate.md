# 0022: the C ABI builds a guard per call, and §7 gate 1 has been failing unwatched

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** none, and none is planned. **The decision is to build
nothing** — see *Decision*. The measurement half of this record has all landed
(`just abi-cost`, `just abi-split`, `just abi-attached`, `just guard-cost`), and
the one documentation change it owes is named in *Implementation plan*.

**Why `ready` and not `implemented`.** There is no code, and freezing the document
would be wrong, because this decision has a stated reopening condition
(`PHASE7.md`, below). Zero open questions is what makes it `ready`: questions 1
and 5 were answered by measurement (amendments 3 and 5), and 2–4 are closed by
declining.

## Context

`docs/PHASE4.md` §7 gate 1 recorded `tft_plan_at` at **1.020× native Rust**;
`docs/benchmarks/tf2.md` records a C++ caller at **306.7 ns** against native
**201.5 ns** (+52%). Both are correctly measured on **different arena backings**.

`just abi-split` (`crates/tf_tree_bench/src/backing.rs`) walks the ladder on one
fixture — the §11.1 topology, `imu_link ← map`, off-grid stamps:

| Rung | API | Arena | ns/lookup |
|---|---|---|---|
| H | native Rust | heap, in-process | 200.7 |
| S | native Rust | `MAP_SHARED` memfd, in-process RW | 203.2 |
| A | native Rust | memfd, read-only, **cross-process** | 202.5 |
| C | **`tft_plan_at`** | same arena as A | **302.0** |
| C′ | **`tft_plan_at_many`** | same arena as A | **261.0** |

Eliminated by measurement: the mapping (≤ 9.6 ns), the cross-process read-only
attach (−0.7 ns), the link mode (245.4 vs 244.4 ns,
`crates/tf_tree_c/tests/cpp/bench.cpp` against `.a` and `.so`). What remains is
**+99.5 ns inside the C ABI**.

### The mechanism, measured

`tft_plan_at` builds a `Guard` on every call (`h.share.tree.guard()` in
`crates/tf_tree_c/src/lib.rs`); the C signature has nowhere to keep one between
calls. `backing::guard_cost_both` prices that in safe Rust with no C ABI: the same
`Plan::at` sweep with a hoisted guard versus one acquired per iteration.

| build | heap arena | memfd arena |
|---|---|---|
| `counters` on (default) | **+35.4 ns** | **+27.0 ns** |
| `counters` off | +16.8 ns | +18.9 ns |

The backing is irrelevant; Phase 5's counters roughly double the per-call guard.

### §7 gate criterion 1 is failing

`examples/abi_cost.rs` prints **FAIL**: `tft_plan_at` measures **1.34–1.46×**
against its 1.05 gate. The recorded 1.020× was stale because `abi_cost.rs` was run
by no recipe or workflow. `just abi-cost` now runs it and `docs/PHASE4.md` §7
records the failing state.

## What this record already got wrong

The first attempt attributed the entire 105 ns residue to the shared-library
boundary without measuring it; it was wrong by about 100×. **A residue is a
hypothesis.**

## Amendment 4 — what is *inside* the 48 ns, and the three things it is not

At `[profile.embedder]` (`lto = false`), same fixture and shared arena,
`just abi-attached`, six runs, `taskset -c 2`; every arm is a difference between
two loops of identical shape:

| part | ns | how it was isolated |
|---|---|---|
| `tf_tree_ipc::fork::generation()` | **+0.2** | the same loop with and without the call |
| `Tree::view()` | +3.7 | a view built per iteration |
| `Guard::new(view)` | +4.8 | over the view arm |
| the rest of `Tree::guard` — `detached()`, `is_shared()`, `with_fork_check` | +6.7 | over `Guard::new` |
| **= build + drop a guard, in isolation** | **15.1** | |
| the same, on `Plan::at`'s critical path | ~22 | arm `E` |
| the cold bracket-search cursor | ~4.8 | arm `B` − `A` |
| **still unattributed** | **~16** | |
| **rung 1, for reference** | **43–47** | |

Spread on rung 1 is 43.0–46.9 ns; treat every figure as ±2.

**Refuted:** `fork::generation` costs 0.2 ns (it is `#[inline]`), so there is
nothing to fix in the fork-safety mechanism (`docs/decisions/0005` step 9;
`tf_tree_ipc::fork`'s module header). `#[inline]` on `Tree::guard` moves nothing.
The `Guard` is 208 bytes, 128 of them the `[Cell<u64>; MAX_DEPTH]` cursor;
`MAX_DEPTH` 16 → 8 moved nothing, so "shrink the cursor" is off the board.

**The whole fork-safety half of `Tree::guard` is 6.7 ns, ~15% of rung 1**: the
entire prize available from touching it. A record proposing to weaken or move the
fork check has to argue against that number.

The ~16 ns unattributed is evaluating through a *fresh* guard beyond the cold
cursor; the plausible reading (fields not held in registers across `Plan::at`) is
a hypothesis nothing depends on.

**Question 1 loses its number**: the ~18 ns was taken at the workspace `release`
profile; at a real boundary the guard's whole drop path is inside the 15.1 ns
figure, and a read-only arena's flush early-returns. Questions 2–4 are untouched.

## Amendment 5 — question 1 is CLOSED, and the answer is that nobody pays it

`just guard-cost` runs the 2x2 on **writable** arenas, the only configuration
where `Guard::drop` reaches the flush:

| profile | counters | heap | memfd |
|---|---|---|---|
| `release` (`lto = "thin"`) | on / off | +33.8 / +17.0 ns | +25.5 / +18.4 ns |
| **`embedder`** (`lto = false`) | **on / off** | **+50.3 / +34.4 ns** | **+51.6 / +35.8 ns** |

At a real boundary the counter flush costs ~16 ns per guard drop, on both
backings. It is reached only when a guard is built *per call* **and** the arena is
*writable* (`Guard::drop` early-returns on `n == 0 || !self.view.is_writable()`,
`tf_tree_core/src/plan.rs`).

- **A C or C++ consumer attaches read-only** (`tft_tree_open`; D18), so the tier
  forced into per-call guards never reaches the flush. Question 1's win for the C
  ABI is exactly zero.
- **A Rust consumer hoists the guard**, so the flush amortises to nothing
  (`counter_cost` finds no measurable difference at the hoisted cadence).

**Precision, 2026-08-28.** `Tree::lookup` builds a guard per call and a
`TreeBuilder::build()` tree is writable, so that one facade entry point does pay
the flush. The decision is unchanged: `Tree::lookup` is `docs/API.md` §1 R1's
*collapsed convenience*, by construction not the hot tier, and the flush is ~6.5%
of its ~245 ns lookup (`abi_cost.rs`). That does not buy a conditional flush or
counters that lie about the one workload that would trigger it.

**Question 1 is withdrawn as a proposal.** A `tft_guard` handle's ceiling is the
~35 ns counter-free per-call guard, not the ~50 ns counters-on row; the C tier
never pays the counter half.

## Amendment 3 — question 5 is CLOSED, and this record's original premise was right

Full ladder at `[profile.embedder]`, §11.1 fixture, shared arena, `just
abi-attached`, three runs:

| rung | ns | Δ |
|---|---|---|
| native Rust, guard hoisted | 242 | — |
| **+ guard built per call** | **290** | **+48** |
| + the 56-byte `QVEC7` store the ABI must make | 289 | ~0 |
| the ABI, no panic guard | 296 | +7 |
| `tft_plan_at`, from Rust | 297 | +1 |
| `tft_plan_at`, from C++ | 302 | — |

**There is no unexplained residue.** The per-call `Guard` is **~48 of ~56 ns**
(~85%); validation plus the un-inlinable call are ~7 ns. A native Rust caller that
builds a guard per lookup costs the same as the C ABI. Amendments 1 and 2 were
wrong (below) because a number carried across build profiles answers a different
question. What was wrong throughout is the *title*: **nothing non-inlined** can
hold a guard, and the C ABI is where that is forced.

Holding a guard across calls would recover ~48 of ~56 ns; questions 1–4 still
block the design (soundness of a `tft_guard` outliving its `tft_tree`, per `0017`;
staleness of a pinned topology generation). `tft_plan_at_many` needs no new type.

## Amendments 2 and 1 — superseded, kept for the reasoning error

**Amendment 2** measured `tft_plan_at` from Rust at `release` (thin LTO): 200.5
native vs 225.8 (+25), and at `embedder`: 241.3 vs 298.4 (+57) with C++ at 302.0,
so a Rust and a C++ caller agree within 4 ns at a real boundary. It concluded the
guard was "a third" (~19 ns at `release`), which was the LTO-erased baseline.

**Amendment 1** found `abi_cost.rs`'s native baseline unstable (133 → ~190 ns from a
second `Tree::guard()` call site) and retracted "gate 1 is failing"; amendments 2–3
reinstated it. Measure comparands in one build.

## Decision

**Do not build a `tft_guard` handle.** Questions 2, 3 and 4 are closed by
*declining* the design: what those questions would buy is already bought by an
API that exists. The declined sketch:

```c
tft_status tft_guard_acquire(const tft_tree *tree, tft_guard **out);
tft_status tft_plan_at_guarded(const tft_plan *plan, const tft_guard *g,
                               int64_t stamp, tft_layout layout, void *out);
void       tft_guard_release(tft_guard *g);
```

### 1. `tft_plan_at_many` already collects essentially the whole prize

One guard per batch instead of one per element:

| what | ns | where |
|---|---|---|
| `tft_plan_at`, C++ caller | 302.0 | rung C |
| `tft_plan_at_many`, n = 256 | 261.0 | rung C′ |
| **recovered by batching** | **41.0** | |
| the whole per-call guard, `[profile.embedder]` | **43–47** | amendment 4, rung 1 |
| the whole C++-over-native gap | ~56–60 | amendment 3 |

Batching recovers ~90% of the guard; a handle's remaining prize over
`tft_plan_at_many` is 0–7 ns, inside rung 1's spread. The two harnesses are not a
paired measurement. Amendment 5's counters-free ~35 ns figure is smaller and would
make batching exceed the whole guard, so the conclusion does not depend on which
is used; picking the larger flatters the declined handle. The 302.0/261.0 pair is
a C++ caller against `target/release/libtf_tree_c.a`, a real boundary (302.0 under
both workspace profiles).

Batching wants stamps in order and recovers nothing at n = 1.

### 2. The caller who cannot batch is not paying enough to care

A caller doing enough lookups for 35–45 ns each to matter can hand them over as an
array. One lookup per control cycle against a 1–10 ms period is 0.0045%–0.00045%.

### 3. The handle costs what `0017` spent seven steps removing

A `tft_guard` outliving its `tft_tree` is a use-after-free a C caller writes in
one line; [`0017`](./0017-owned-handles-and-the-lifetime-rule.md) deleted both
hand-rolled `extend_to_static` helpers. `API.md` §2.1 ("no type a user stores
carries a lifetime") would force an `Arc<Tree>` in the ABI for a 45 ns saving. A
held guard also pins a topology generation, so it reads a stale topology after a
declaration; per-call acquisition makes that impossible today.

### 4. The first draft's `API.md` R2 argument is withdrawn

`Guard::new` allocates nothing, takes no lock, waits for nothing: pinning a
generation is a single acquire load (`TopologyView::stable_generation`) plus
zeroing a cursor array. The cost is *work*, not *synchronization*.

### The alternative, kept on the shelf

If the cost ever has to be recovered, do not start from a new public type. Cache
the guard inside the **existing** `tft_plan` handle and revalidate against the
topology generation on each call:

- No new public type or caller-visible lifetime; the ABI's soundness story does
  not change.
- One acquire load per call (the load `Guard::new` already does) compared against
  `header.topo`'s generation; a mismatch rebuilds the guard, so a stale topology
  is *impossible*.
- Costs one load instead of ~15 ns of construction, plus whatever the warm cursor
  is worth (the larger half on a realistic ring, near zero on a toy one; `0023`,
  `docs/design/fast-path.md` §12).
- The difficulty is self-referential storage (a `Guard<'a>` beside the
  `Arc<Tree>` it borrows), internal to `tf_tree_c` and the shape `OwnedWriter`
  solves. It would still be a decision record: `0007`'s budget is a criterion and
  this adds an `unsafe` site to the C ABI's crate.

### What reopens this decision

**`docs/PHASE7.md`'s `tf2`-shaped shim is inherently scalar**:
`lookupTransform(target, source, t)` has no batch spelling, and §2 puts the shim
"header-only over the C ABI", so its steady state is the caller point 2 dismisses,
in bulk. That does not reopen anything today: PHASE7 is gated by D21 and its §0.0
lists four gates, none met. If they open, **this decision is to be re-taken, and
the shelf alternative is where it starts.**

### The §7 half of the first draft, superseded

[`0023`](./0023-the-gate-that-could-not-gate.md) re-cuts criterion 1 into three
rungs at a stated profile; its open question 3 is which fixture R3 should gate.
The backing is not the variable (heap and memfd differ by ~1.4 ns at
`[profile.embedder]`, amendment 5): "gate a realistic *fixture*" is the right ask.

## Open questions

**None. All five are closed:**

| # | subject | closed by | kind |
|---|---|---|---|
| 1 | the conditional counter flush | amendment 5 | **withdrawn**: ~16 ns is real, but no hot-tier configuration reaches the flush |
| 2 | is a guard handle sound to expose? | *Decision* §3 | **declined** |
| 3 | how long may a guard be held? | *Decision* §3 | **declined**, answered anyway by the shelf alternative's revalidation |
| 4 | is `tft_plan_at_many` sufficient instead? | *Decision* §1–2 | **answered: yes** — 41 of 43–47 ns |
| 5 | what is the rest of the ~60 ns? | amendment 3 | **measured**: no unexplained residue |

Question 4 decided the record. The first draft's question 4 (could the
`is_shared()` fork check move per-tree?) was refuted: +2.1 ns counters off, −8.4 ns
on, noise.

Question 1 was whether `Guard`'s counter flush should be conditional: withdrawn
(amendment 5; `docs/PHASE5.md` §5 says whether losing fidelity on single-lookup
guards is acceptable). Question 2: is a guard handle sound to expose (`0017`)?
Question 3: how long may a guard be held, given it pins a topology generation?
Question 4: is `tft_plan_at_many` sufficient instead? Yes. Question 5: the guard
explains ~48 of ~56 ns; validation, the un-inlinable call and the panic guard the
rest (`abi_cost.rs` prices them for the publish path at +17.0, +1.3, +0.7 ns).

## Implementation plan

**No engine, ABI or arena change. One documentation change:**

1. **Say "batch" where a C or C++ embedder looks.** ✅ Landed: `tft_plan_at`'s doc
   comment in `crates/tf_tree_c/include/tf_tree.h` and `Plan::at`'s in
   `tf_tree.hpp` open with "On a hot path, prefer `tft_plan_at_many`" and carry the
   numbers (one guard per batch, ~41 ns of ~302 ns at n = 256) and the sort caveat.
   A reader who reaches `tft_plan_at` and never learns `tft_plan_at_many` exists is
   the only way this decision goes wrong.

Everything else is merged: `just abi-cost`, `abi-split`, `abi-attached`,
`guard-cost`, and the corrections in `docs/PHASE4.md` §7 and
`docs/benchmarks/tf2.md`. Nothing in this record authorizes a `tft_guard` handle;
reopening runs through *What reopens this decision*, D21's gates and a new record.

## Related

- `docs/PHASE4.md` §7, *§7 gate criterion 1 is failing*.
- `docs/benchmarks/tf2.md`, *Where the 52% actually goes*.
- [`0017`](./0017-owned-handles-and-the-lifetime-rule.md) — the lifetime-extension cost.
- [`0023`](./0023-the-gate-that-could-not-gate.md) — gates R3 and inherits the "which fixture" question.
- `docs/PHASE7.md` §0.0 and §2 — the gated shim that would reopen this.
- Amendments above *Decision* are in the order 4, 5, 3, 2, 1.
