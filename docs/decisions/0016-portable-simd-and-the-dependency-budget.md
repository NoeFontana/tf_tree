# 0016: portable SIMD, and what it costs the dependency budget

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none yet — the spike was reverted; see *Open questions*

## Context

`CLAUDE.md` fixes a dependency budget and marks it "do not relitigate":

> `tf_tree_core` = `libm` + `bytemuck` + `blake3` (no_std) and nothing else.
> `tf_tree_math` = `libm` + `bytemuck`.

`docs/design/fast-path.md` §12 measured where a dynamic step's ~72.5 ns goes and
found **interpolation at 22.2 ns, 31%** — the second-largest term and the only
large one that is arithmetic-bound rather than memory-bound. A wider ALU is the
obvious lever, and §7's Lever 5 proposed exactly that.

Two constraints shape how it could be reached. `tf_tree_math` is
`#![forbid(unsafe_code)]`, and explicit SIMD intrinsics are `unsafe`; `core::simd`
is nightly and the MSRV floor is stable 1.85. [`0007`](./0007-the-unsafe-budget-and-the-c-abi.md)
permits `unsafe` at four named boundaries and says a fifth needs a decision
record — so intrinsics in the math crate would need *this* record anyway, for a
worse reason.

**What forces the decision now** is that §14 moved the goalposts after the
approach was chosen. Lever 2 — restructuring the fold so the *d* steps overlap —
was measured and **rejected**: per-sample cost does not fall as independent
chains are added, it rises. Cross-*step* vectorisation dies with it, because
there is no step-level parallelism to widen. Only SIMD across **stamps**, on the
batch path, survives. The scope shrank from "the hot path" to `at_many` /
`at_many_into` / `at_adaptive` between the moment `pulp` was chosen and the
moment the spike was run, and the budget cost did not shrink with it.

## Decision

**Proposed** (not yet accepted — see *Open questions*): add `pulp` to
`tf_tree_math` as

```toml
pulp = { version = "0.22", default-features = false, features = ["x86-v3"] }
```

and use it for **batch interpolation only** — the `Interp::eval` inner loop
reached from `Plan::at_many`, `at_many_into`, `at_many_into_f32` and
`at_adaptive`. The scalar `Plan::at` path is not touched.

`#![forbid(unsafe_code)]` stays on `tf_tree_math`: `pulp`'s whole purpose is to
keep the `unsafe` inside `pulp` and expose a safe `WithSimd`/`Simd` API. So this
does **not** open a fifth `0007` boundary. That is the single strongest argument
for `pulp` over the alternatives and it is why it was chosen.

## The spike — what was measured

Run on this branch and then reverted; `cargo deny check`, `just msrv`,
`cargo nextest run --workspace` and `cargo +nightly miri test -p tf_tree_math`
were run against a real `Arch::dispatch` call, not merely against the dependency
being present.

| Gate | Result |
|---|---|
| Builds `no_std` (`default-features = false`) | **pass** |
| `#![forbid(unsafe_code)]` preserved with a real dispatch | **pass** |
| MSRV — `just msrv` on 1.85, `--locked` | **pass** |
| `cargo deny check` — advisories, bans, licences, sources | **pass** (MIT; no `*-sys`, so `deny.toml`'s no-C-build-step ban is untouched) |
| `cargo nextest run --workspace` | **pass**, 698 tests |
| Miri can interpret the dispatch | **pass** |
| Vectorised result **bit-identical** to scalar | **pass**, all 33 lanes including the tail |

### The feature flags are the whole story, and the obvious setting is wrong

`pulp`'s defaults are `["std", "x86-v3", "relaxed-simd"]`, and `std` must go to
keep `no_std`. The naive `default-features = false` **silently produces scalar
code**:

| features | f64 lanes selected on this AVX2 host |
|---|---|
| `default-features = false` | **1** — scalar fallback |
| `["std"]` | **1** — scalar fallback |
| `["x86-v3"]` | **4** |
| `["std", "x86-v3"]` | 4 |

`x86-v3` is what compiles the AVX2 implementation in at all; `std` only selects
which detection mechanism is used. Taking the dependency and turning off default
features — the reflex that keeps `no_std` — would have bought nine crates and
**no vectorisation whatsoever**, and every test would still have passed, because
"bit-identical to scalar" is trivially true when the scalar path is what ran.

The spike test therefore reports the **selected lane count** and says so out loud
when it is 1. Without that guard the whole exercise is vacuous. Any
implementation must keep it.

### `x86-v3` is runtime-detected, not a compile-time assumption

This needed checking, because a compile-time AVX2 assumption would produce
binaries that `SIGILL` on pre-2013 x86-64 and no test on this host would ever
catch it. It is not one: `V3::try_new()` → `is_available()` →
`__detect_is_available()`, and with `std` off that resolves to `pulp`'s
`raw_cpuid_detect` path (`core_arch/mod.rs`, `#[cfg(all(not(feature = "std"), …))]`).
Absent AVX2, `Arch::new()` falls back to scalar. Portable.

**Miri selects the scalar path** (1 lane), so Miri proves the dispatch machinery
is sound but does **not** check the wide arithmetic. That gap has to be covered
by a differential test on real hardware, not by Miri.

### The budget cost is larger than it looks

`tf_tree_math` has exactly **two** dependencies today. `pulp` brings **nine** more:

```
pulp
├── bytemuck        (already budgeted)
├── libm            (already budgeted)
├── cfg-if
├── num-complex ── num-traits
├── paste           (proc-macro)
├── pulp-wasm-simd-flag
├── raw-cpuid
└── reborrow
    [build] version_check
```

`num-complex` is unused by us — quaternions are not complex numbers — and comes
along regardless. An earlier estimate said six; it is nine, and `raw-cpuid`,
`num-traits` and `version_check` were the ones missed.

## Rationale

Alternatives considered:

- **Explicit `core::arch` intrinsics.** `unsafe`, in a crate that forbids it, so
  it needs a fifth `0007` boundary *and* per-architecture code. Loses on both
  counts.
- **`core::simd`.** Nightly. The MSRV floor is stable 1.85 and `just msrv` gates
  it. Not available.
- **Autovectorisation only** (`fast-path.md` §4's phase 2). Free, no dependency —
  but it must be *verified by reading asm* on every edit, and §14 removed the
  cross-step axis it was aimed at. Still the right first thing to try on the
  batch axis, and *this record does not preclude it*: if a shaped scalar loop
  vectorises, `pulp` is unnecessary.
- **Do nothing.** Interpolation stays at 22.2 ns/step. The scalar path — which is
  what `Plan::at`, the C ABI and the ROS bridge use — is unaffected either way,
  since batch SIMD only helps `at_many`.

## Consequences

- `tf_tree_math` goes from 2 dependencies to 11. That is the largest single
  expansion of the budget since it was written, and `CLAUDE.md`'s budget line
  must be amended rather than quietly contradicted.
- **A new correctness obligation.** `docs/PHASE5.md` §2.1's live/frozen
  bit-for-bit replay and the committed tf2 differential baseline both assume one
  arithmetic path. A dispatch that is not bit-identical across tiers breaks them
  *on some hosts only*. `crates/tf_tree/tests/batch.rs` already asserts
  `at_many == at` per stamp, which is the right guard — it must be extended to
  run against **every tier `pulp` would select on the host**, not just the one it
  happens to pick.
- Miri no longer covers the arithmetic that actually ships on an AVX2 host.
- The win is confined to the batch path. `at_many` is the Python zero-copy path
  and the `.tft` dataloader story, so it is not nothing — but no ROS consumer,
  no C ABI caller and no `Plan::at` user sees it.

## Implementation plan

1. Amend `CLAUDE.md`'s dependency budget and add the `pulp` line to the workspace
   manifest with `features = ["x86-v3"]` — verified by `just msrv`, `just lint`.
2. Restore the spike's lane-count-reporting bit-identity test in
   `tf_tree_math` — verified by it failing when features are set to
   `default-features = false` (the vacuity guard).
3. Try **autovectorisation first**: shape the batch interpolation loop over
   `[f64; 4]` and read the asm. If it vectorises, stop — steps 4–6 are
   unnecessary and the dependency is not taken.
4. Vectorise `LerpSlerp::eval` across stamps behind `WithSimd` — verified by
   `cargo nextest run -p tf_tree` (`batch.rs`) and `just bench-check`
   (`DEVIATION_SLACK` is the tripwire for arithmetic that moved).
5. Extend `batch.rs` to compare every `pulp` tier against scalar — verified by
   forcing each tier and watching the comparison run rather than skip.
6. Measure with `step_cost --json` + `bench_ab`, and record the result in
   `fast-path.md` §15 the way §13 and §14 recorded theirs — including if it is
   another falsification.

## Open questions

1. **Is a batch-only win worth nine dependencies?** The scope halved between
   choosing `pulp` and running the spike, because §14 killed the cross-step axis.
   The honest framing for review: 31% of per-step interpolation cost, on
   `at_many` only, against `tf_tree_math` going from 2 dependencies to 11. This
   record does not presume the answer.
2. **Does autovectorisation get there for free?** Step 3 is deliberately ordered
   before the dependency is relied on. Nobody has read the asm yet.
3. **Is `num-complex` acceptable as an unused transitive?** It is pulled in
   unconditionally and nothing here uses complex numbers.
4. **What replaces Miri's coverage of the wide path?** Miri selects scalar, so
   the shipped arithmetic on an AVX2 host is unverified by it. Step 5 is the
   proposal; whether a per-tier differential is sufficient needs review.
