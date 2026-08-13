# 0022: the C ABI cannot hold a guard, and pays 49% for it on a shared arena

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`docs/PHASE4.md` §7 gate 1 records `tft_plan_at` at **1.020× native Rust** and
calls it a PASS. `docs/benchmarks/tf2.md` records a C++ caller at **306.7 ns**
where native Rust measures **201.5 ns** on the same host and fixture — **+52%**.
Both numbers are correctly measured. They disagree because they are measured on
**different arena backings**, and the gate is measuring the one no shared-memory
consumer uses.

`just abi-split` (`crates/tf_tree_bench/src/backing.rs`) walks the whole ladder
on one fixture — the §11.1 topology, `imu_link ← map`, off-grid stamps so
`I::eval` actually runs:

| Rung | API | Arena | ns/lookup |
|---|---|---|---|
| H | native Rust | heap, in-process | 200.7 |
| S | native Rust | `MAP_SHARED` memfd, in-process RW | 203.2 |
| A | native Rust | memfd, read-only, **cross-process** | 202.5 |
| C | **`tft_plan_at`** | same arena as A | **302.0** |
| C′ | **`tft_plan_at_many`** | same arena as A | **261.0** |

Every candidate except one is eliminated by measurement, not by argument:

- **the mapping**: ≤ 9.6 ns, paired over nine runs (median quotient
  1.0066–1.0112×, point estimate ~1.8 ns);
- **the cross-process read-only attach**: −0.7 ns;
- **the link mode**: ~1 ns — `crates/tf_tree_c/tests/cpp/bench.cpp`, the same
  source built against `libtf_tree_c.a` and `libtf_tree_c.so`, measures **245.4
  against 244.4 ns**.

What remains is **+99.5 ns, or +49%, inside the C ABI**, on an arena where native
Rust costs 202.5 ns.

### The mechanism

`tft_plan_at` builds a `Guard` on every call:

```rust
// crates/tf_tree_c/src/lib.rs:684
let g = h.share.tree.guard();
```

and the Rust API does not have to — `ratio.rs`, `backing.rs` and every
in-process consumer acquire one guard and reuse it across the whole sweep. On a
**heap** arena that per-call guard is nearly free, which is why §7 gate 1 reads
1.020×. On a **shared** arena `Tree::guard` takes a different path:

```rust
// crates/tf_tree/src/tree.rs:1984
let g = if self.is_shared() {
    g.with_fork_check(tf_tree_ipc::fork::generation)
} else { g };
```

`C′` is the evidence rather than the inference. `tft_plan_at_many` pays the
guard **once per batch instead of once per element**, and recovers **41 ns of
the 99.5** at a batch of 256. Nothing else about the two entry points differs in
a way that would produce that.

The residual ~58 ns between `C′` (261.0) and `A` (202.5) is **not attributed by
this record.** Candidates are the per-element layout write (`TFT_LAYOUT_QVEC7_WXYZ`
is a 56-byte store the Rust arm never makes), the batch cursor being defeated by
the descending stamp sweep, and the `catch_unwind`. Measuring it is work this
record does not do, and a decomposition asserted without measurement is exactly
what went wrong on the way here (see *What this record already got wrong*).

## What this record already got wrong

Recorded because the failure is instructive and cost two commits.

The first attempt measured the mapping (rungs H and S), found it ~free,
and **attributed the entire 105 ns residue to the shared-library boundary** —
publishing "at least 91% of the gap is the boundary" in `tf2.md`, in a commit
message, and in three source files. Nothing had measured the boundary. It was a
subtraction dressed as a finding, and it was wrong by roughly a factor of 100.

The check that caught it was cheap and should have come first: compile one
existing benchmark two ways and compare. It took four minutes.

This is the same failure mode `tf2.md` already documents twice — the withdrawn
4.7× CPU reading and the stale `.tft` reasons — and the same one that the
`heap_vs_shared` and 213-vs-217 priors fell into. **A residue is a hypothesis.**

## Decision

*(draft — nothing below is authorized)*

Give the C tier a way to hold a guard across calls, so that the hot path can
express what the Rust tier expresses. `docs/API.md` §1 **R2** — *the hot tier
never allocates, locks or converts* — is the rule the current shape fails: a
per-call guard acquisition on a shared arena is per-call synchronization work in
the tier that promises none.

Sketch, to be settled by the questions below:

```c
tft_status tft_guard_acquire(const tft_tree *tree, tft_guard **out);
tft_status tft_plan_at_guarded(const tft_plan *plan, const tft_guard *g,
                               int64_t stamp, tft_layout layout, void *out);
void       tft_guard_release(tft_guard *g);
```

Separately, and independently of whether the above is built: **§7 gate 1 must
state which backing it measures**, and should gain a shared-arena row. A gate
that passes at 1.020× while every `shm` consumer pays 1.49× is not measuring the
product.

## Open questions

1. **Is a guard handle sound to expose at all?** `Guard<'_>` borrows the tree,
   and `0017` is explicit that lifetime extension in the bindings is the thing
   being removed, not added. A `tft_guard` outliving its `tft_tree` is a
   use-after-free the C caller can write trivially. Does this need the
   `OwnedWriter` treatment (`0017`), a generation check, or refusal?
2. **How long may a guard be held?** A guard pins a topology generation. A C
   consumer holding one across a `select()` loop would silently read a stale
   topology after a declaration, which the per-call acquisition currently makes
   impossible. Is there a bounded-lifetime rule, and can it be enforced rather
   than documented?
3. **Is `tft_plan_at_many` sufficient instead?** It needs no new type, no
   lifetime question, and already exists. It recovers 41% of the gap. Is the
   remaining ~58 ns worth a new handle in the ABI, or is the honest answer
   "batch your lookups" plus a documentation change — and a measurement of what
   the other 58 ns is before anything is designed?
4. **Does the fork check have to be per-guard?** It exists so a guard created or
   dropped across `fork()` does not write into a `MADV_DONTFORK` hole. If that
   can be answered per-tree with the same safety, `Tree::guard` gets cheaper for
   *every* shared consumer, Rust included — which would make questions 1–3 moot
   and is the cheapest possible fix. **This should be measured first.**

## Implementation plan

None. This record is `draft`; `docs/decisions/README.md` forbids starting.

The first work item when it moves to `ready` is question 4, because it is the
only candidate that costs no API surface, and question 3's measurement, because
designing a handle to recover 58 ns nobody has attributed would repeat this
record's own mistake.
