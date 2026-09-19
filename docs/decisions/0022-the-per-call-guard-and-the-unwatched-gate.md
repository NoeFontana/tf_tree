# 0022: the C ABI builds a guard per call, and §7 gate 1 has been failing unwatched

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** none, and none is planned. **The decision is to build
nothing** — see *Decision*. The measurement half of this record has all landed
(`just abi-cost`, `just abi-split`, `just abi-attached`, `just guard-cost`), and
the one documentation change it owes is named in *Implementation plan*.

**Why `ready`.** No code to freeze; the decision has a reopening condition.

## Context

`docs/PHASE4.md` §7 gate 1 recorded `tft_plan_at` at 1.020× native Rust;
`docs/benchmarks/tf2.md` records a C++ caller at 306.7 ns against native 201.5 ns.
`just abi-split` (`crates/tf_tree_bench/src/backing.rs`) walks one fixture (the
§11.1 topology, `imu_link ← map`, off-grid stamps):

| Rung | API | Arena | ns/lookup |
|---|---|---|---|
| H | native Rust | heap, in-process | 200.7 |
| S | native Rust | `MAP_SHARED` memfd, in-process RW | 203.2 |
| A | native Rust | memfd, read-only, **cross-process** | 202.5 |
| C | **`tft_plan_at`** | same arena as A | **302.0** |
| C′ | **`tft_plan_at_many`** | same arena as A | **261.0** |

The mapping, the cross-process attach and the link mode are eliminated. The
remaining +99.5 ns is inside the C ABI: `tft_plan_at` builds a `Guard` on every
call (`h.share.tree.guard()` in `crates/tf_tree_c/src/lib.rs`), because the C
signature has nowhere to keep one.

### §7 gate criterion 1 is failing

`examples/abi_cost.rs` prints **FAIL**: `tft_plan_at` measures 1.34–1.46× against
its 1.05 gate. `just abi-cost` runs it and `docs/PHASE4.md` §7 records the failing
state.

## Amendment 4 — what is inside the 48 ns

At `[profile.embedder]` (`lto = false`), `just abi-attached`, each part a
difference between two loops of identical shape (±2 ns):

| part | ns |
|---|---|
| `tf_tree_ipc::fork::generation()` | +0.2 |
| `Tree::view()` | +3.7 |
| `Guard::new(view)` | +4.8 |
| rest of `Tree::guard` (`detached()`, `is_shared()`, `with_fork_check`) | +6.7 |
| **build + drop a guard, in isolation** | **15.1** |
| same, on `Plan::at`'s critical path | ~22 |
| cold bracket-search cursor | ~4.8 |
| still unattributed | ~16 |
| rung 1, for reference | 43–47 |

The whole fork-safety half of `Tree::guard` is 6.7 ns, ~15% of rung 1; a record
proposing to weaken or move the fork check has to argue against that number.
`#[inline]` on `Tree::guard` and shrinking `MAX_DEPTH` (16 → 8) move nothing.

## Amendment 5 — question 1 is CLOSED: nobody pays it

`just guard-cost` runs {release, embedder} x {counters on, off} on **writable**
arenas, the only configuration where `Guard::drop` reaches the counter flush
(`tf_tree_core/src/plan.rs`): at `embedder`, +50.3 / +34.4 ns (heap) and
+51.6 / +35.8 ns (memfd), so the flush is ~16 ns per drop.

- A C or C++ consumer attaches read-only (`tft_tree_open`; D18), so the flush is
  never reached: question 1's win for the C ABI is zero.
- A Rust consumer hoists the guard, so the flush amortises to nothing.
- `Tree::lookup` builds a guard per call on a writable tree and pays the flush
  (~6.5% of ~245 ns), but is `docs/API.md` §1 R1's collapsed convenience tier. No
  conditional flush and no counters that lie about that workload.

**Question 1 is withdrawn as a proposal.**

## Amendment 3 — question 5 is CLOSED: no unexplained residue

Full ladder at `[profile.embedder]`, `just abi-attached`:

| rung | ns | Δ |
|---|---|---|
| native Rust, guard hoisted | 242 | — |
| **+ guard built per call** | **290** | **+48** |
| + the 56-byte `QVEC7` store | 289 | ~0 |
| the ABI, no panic guard | 296 | +7 |
| `tft_plan_at`, from Rust | 297 | +1 |
| `tft_plan_at`, from C++ | 302 | — |

The per-call `Guard` is ~48 of ~56 ns; validation plus the un-inlinable call are
~7 ns. A native Rust caller that builds a guard per lookup costs the same as the
C ABI. Holding a guard across calls would recover ~48 ns, but soundness of a
`tft_guard` outliving its `tft_tree` (`0017`) and staleness of a pinned topology
generation block the design.

## Decision

**Do not build a `tft_guard` handle.** The declined sketch:

```c
tft_status tft_guard_acquire(const tft_tree *tree, tft_guard **out);
tft_status tft_plan_at_guarded(const tft_plan *plan, const tft_guard *g,
                               int64_t stamp, tft_layout layout, void *out);
void       tft_guard_release(tft_guard *g);
```

### 1. `tft_plan_at_many` already collects essentially the whole prize

One guard per batch: 302.0 (rung C) − 261.0 (rung C′, n = 256) = **41.0 ns** of
the 43–47 ns per-call guard. A handle's remaining prize is 0–7 ns, inside rung 1's
spread. Batching wants stamps in order and recovers nothing at n = 1.

### 2. The caller who cannot batch is not paying enough to care

One lookup per control cycle against a 1–10 ms period is 0.0045%–0.00045%.

### 3. The handle costs what `0017` spent seven steps removing

A `tft_guard` outliving its `tft_tree` is a use-after-free a C caller writes in one
line; `API.md` §2.1 would force an `Arc<Tree>` into the ABI for a 45 ns saving. A
held guard also pins a topology generation and reads a stale topology after a
declaration.

### 4. `API.md` R2 is not an argument

`Guard::new` allocates nothing and takes no lock (a single acquire load,
`TopologyView::stable_generation`, plus zeroing a cursor array); the cost is work,
not synchronization.

### The alternative, kept on the shelf

Cache the guard inside the **existing** `tft_plan` handle and revalidate against
`header.topo`'s generation on each call: no new public type, one acquire load per
call, a stale topology is impossible. The difficulty is self-referential storage
internal to `tf_tree_c`, which adds an `unsafe` site and so needs a record
(`0007`; `docs/design/fast-path.md` §12).

### What reopens this decision

`docs/PHASE7.md`'s `tf2`-shaped shim is inherently scalar
(`lookupTransform(target, source, t)`) and header-only over the C ABI. PHASE7 is
gated by D21 (§0.0 lists four gates, none met). If they open, **this decision is
to be re-taken, starting from the shelf alternative.**

## Open questions

**None.**

| # | subject | closed by | kind |
|---|---|---|---|
| 1 | the conditional counter flush | amendment 5 | **withdrawn** |
| 2 | is a guard handle sound to expose? | *Decision* §3 | **declined** |
| 3 | how long may a guard be held? | *Decision* §3 | **declined** |
| 4 | is `tft_plan_at_many` sufficient instead? | *Decision* §1–2 | **yes** |
| 5 | what is the rest of the ~60 ns? | amendment 3 | **measured** |

## Implementation plan

**No engine, ABI or arena change. One documentation change:**

1. **Say "batch" where a C or C++ embedder looks.** ✅ Landed: `tft_plan_at`'s doc
   comment in `crates/tf_tree_c/include/tf_tree.h` and `Plan::at`'s in
   `tf_tree.hpp` open with "On a hot path, prefer `tft_plan_at_many`".


## Related

- `docs/PHASE4.md` §7, *§7 gate criterion 1 is failing*.
- `docs/benchmarks/tf2.md`, *Where the 52% actually goes*.
- [`0017`](./0017-owned-handles-and-the-lifetime-rule.md), [`0023`](./0023-the-gate-that-could-not-gate.md).
- `docs/PHASE7.md` §0.0 and §2.
