# 0016: portable SIMD, and what it costs the dependency budget

**Status:** withdrawn
**Owner:** @NoeFontana
**Implementation:** none, and none is planned. **This record is withdrawn**
(2026-08-29, by the owner). The spike was reverted and is not coming back; what
the record established is kept below and in the *Withdrawal* section, which is
the only part a reader needs.

## Withdrawal (2026-08-29)

**Withdrawn rather than taken to `ready`, and the reason is in the record's own
amendment.** `0001`'s gate requires a *Decision* that is final and a plan
detailed enough "that the implementer does not need to invent". This record's
Decision names "the `Interp::eval` inner loop reached from `Plan::at_many`" as
the site to vectorise, and its own Amendment §1 then proves that loop does not
exist: there is no loop across stamps for anything to vectorise. A record whose
Decision names an absent site cannot be implemented as written, and rewriting it
would be writing a different record.

**What the owner decided, and it is not "no SIMD".**

* **`-C target-cpu=x86-64-v3` is the accepted alternative — and it is measurably
  the wrong trade on this workload, so it is permitted and not adopted.** It
  costs nothing in the dependency budget and needs no amendment to `CLAUDE.md`'s
  "do not relitigate" line. Its price was thought to be only a binary that
  `SIGILL`s on pre-AVX2 hardware. **Measured 2026-08-29 on `at_many`, it is also
  slower**, on an AMD EPYC-Milan (Zen 3) host with AVX2, FMA and BMI2:

  | `at_many` bench | baseline | `-C target-cpu=x86-64-v3` |
  |---|---|---|
  | `monotone_1024` | 271.9–275.9 µs | **299.7–308.5 µs** |
  | `into_mat4_1024` | 271.8 µs | **297.9 µs** |
  | `into_quat_1024` | 274.4 µs | **289.0 µs** |

  Four alternating runs of `monotone_1024`, criterion's own paired comparison
  reporting **+8.0% and +13.6% on switching to the flag and −9.6% and −9.3% on
  switching back, every one at p = 0.00**. The intervals do not overlap, and the
  alternation is what rules out the thermal drift an unpaired pair would have
  been indistinguishable from.

  **This is the amendment's own finding arriving from the other direction.**
  §4a recorded that *suppressing* SLP vectorisation made this code ~11% faster;
  widening the lanes costs 8–14%. Both say the same thing: the shuffle traffic
  needed to feed wider vectors exceeds what the arithmetic saves here, so this
  fold does not want more lanes. That is also the strongest available answer to
  the `pulp` question, since `pulp` would have bought the same four lanes through
  a dependency.

  So the flag stays a *named contrast* available to whoever measures a workload
  where it wins — never ambient: not in `.cargo/config.toml`, and **not** in
  `wheels.yml`, whose `x86_64` manylinux and musllinux rows are the only machine
  code this project ships to strangers.
* **`pulp` is not rejected in principle; it is rejected on the evidence
  available.** The owner's condition was "fine if we show it noticeably improves
  performance", and the headline that motivated this record does not survive its
  own amendment: the 31% was measured against a baseline that was never scalar —
  it is 2-lane SLP with the shuffles already paid — which puts the gain nearer
  ~12% of the step, on `at_many` only, against `tf_tree_math` going from 2
  dependencies to 11. Reopening needs a measurement that clears that bar, not a
  new argument.
* **The unused `num-complex` transitive** was the owner's other objection and is
  moot at this disposition. Nothing in `deny.toml` would have rejected it — its
  `[bans]` names exactly three crates and has no rule about unused transitives —
  so it was a budget-philosophy objection, and the budget stands unamended.

**The one durable finding, which outlives the record.** Question 4 asked what
replaces Miri's coverage of a wide path. Under `-C target-cpu` the answer is
**nothing is lost**: Miri interprets MIR, `target-cpu` is a backend flag, and
there is no `cfg(target_feature)`, `core::arch` or `std::arch` anywhere in
`crates/*/src`, so the MIR Miri sees is identical and its UB coverage is
unchanged. What `pulp` would have removed was interpretation of *its own*
`unsafe`, and that dies with the dependency.

**What the widened codegen does create is different in kind, and is not
covered.** Every bit-identity harness this repository owns compares within one
build — `crates/tf_tree/tests/batch.rs` compares batch against scalar in the
*same* binary — so nothing here can observe arithmetic moving when the codegen
moves. Anyone adopting the flag owes a **cross-build** differential, and it must
carry a non-vacuity guard, because two identical codegens agree trivially: this
record's own words about the spike were "without that guard the whole exercise is
vacuous. Any implementation must keep it." Counting `%ymm` in the disassembly —
zero at baseline, non-zero under the flag — is the guard that shows the two
builds really differ.

**Corrected on the way out.** `crates/tf_tree_math/src/lib.rs` claimed its
"property tests run under Miri in seconds". They do not, and never have: `just
miri` selects `-p tf_tree_arena -p tf_tree_core`, which builds no `tf_tree_math`
test target, so `tests/proptests.rs` and `tests/slerp_public.rs` are interpreted
by no recipe. The crate's *library* code is reached as a callee of
`tf_tree_core`'s tests, which is what `forbid(unsafe_code)` makes cheap. That
sentence was on a published crate's docs.rs front page.

---

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
   **Answered — the asm has now been read; see the *Amendment* below. The short
   version is "no, and not for the reason this record assumed", and it moves the
   headline number.**
3. **Is `num-complex` acceptable as an unused transitive?** It is pulled in
   unconditionally and nothing here uses complex numbers.
4. **What replaces Miri's coverage of the wide path?** Miri selects scalar, so
   the shipped arithmetic on an AVX2 host is unverified by it. Step 5 is the
   proposal; whether a per-tier differential is sufficient needs review.

---

## Amendment — open question 2, answered by reading the asm

**Status unchanged: this record is still `draft`, and questions 1, 3 and 4 are
still open.** What follows answers question 2 only.

**Measured on:** AMD EPYC-Milan, 4 physical / 8 logical, SMT on, `taskset -c 2`.
This host fails `docs/PHASE5.md` §9.2's fitness probe, so nothing below is a
multi-process comparison; every number is a single-process criterion row or a
best-of-N in-process loop, which is the class this repository has used credibly
all along.
**Reproduce:** `taskset -c 2 cargo run --release -p tf_tree_bench --example autovec_probe`,
then again with `RUSTFLAGS="-C no-vectorize-slp -C no-vectorize-loops"`.
**Tooling:** `cargo asm` is not installed here and could not be, so the asm is
`objdump -d` of the linked bench binary plus `cargo rustc -- --emit asm` for the
per-CGU view. **A methodology note, because it cost an hour and would cost the
next reader the same:** under `lto = "thin"` rustc defers the vectorisers to the
LTO backend, so `cargo rustc --release -- --emit asm` on this workspace reports
**zero** vector instructions in code that is heavily vectorised once linked. The
`--emit asm` view is only truthful under `[profile.embedder]` (`lto = false`);
for `[profile.release]` the linked binary must be disassembled. Both were done,
and they agree.

### 1. What the asm shows

**There is no loop across stamps for anything to vectorise.** In the linked
release binary, `Plan::at_many_into`'s monotone branch is a single loop whose
backedge spans ~12.4 kB of code and whose body is **one complete plan fold** —
`fold_at_cursors`, `Guard::sample_from`, the galloping bracket search, two
seqlock `read_slot`s and one `Interp::eval`, all inlined, one stamp per
iteration. `at_many`, `at_many_into` and `at_many_into_f32` all share
`fold_batch`'s loop and differ only in the emitter; `at_adaptive` bisects and has
no flat loop over stamps at all. This is the
first thing the record got wrong by not looking: it speaks of "the `Interp::eval`
inner loop reached from `Plan::at_many`" as though such a loop existed. It does
not. `Interp::eval` is called once per (stamp × dynamic step), from inside a
seqlock.

**The arithmetic is nevertheless already vectorised — by the SLP vectoriser,
within a single `eval`, at two lanes.** Counting `Plan::at_many_into` in the
linked `[profile.release]` binary:

| | instructions | packed FP arith | scalar FP arith | shuffles | stack frame |
|---|---|---|---|---|---|
| as built | 2460 | **480** | 76 | 227 | 0x380 |
| `-C no-vectorize-slp` | 2785 | 0 | 882 | 0 | 0x300 |

**Every one of those 480 packed operations is `%xmm`.** There is no `ymm` or
`zmm` anywhere in the engine — the only AVX in the whole binary belongs to
blake3's and `memchr`'s hand-written runtime-dispatched kernels. The reason is
that nothing in this workspace sets `-C target-cpu`: `.cargo/config.toml` carries
only aliases, the `justfile`'s three `RUSTFLAGS` lines are all sanitizers, and
the default `x86_64-unknown-linux-gnu` baseline is SSE2. So the compiler's
ceiling here is **two** `f64` lanes, permanently, and no amount of loop shaping
raises it. That is the real content of "`pulp` gives 4 lanes": not *SIMD versus
scalar*, but *runtime dispatch versus a baseline compile target*.

The same shape holds one level down. `ScLerp::eval` compiles to 107 packed and
135 scalar FP operations plus 54 shuffles; `LerpSlerp::eval` to 47 packed, 82
scalar, 19 shuffles, with `libm::acos` left as an out-of-line call on the
large-arc branch.

### 2. Why it is not vectorised across stamps — the blockers, named

In descending order of how immovable they are. The first four are properties of
the *engine*, not of the arithmetic, and **`pulp` does not remove any of them**:

1. `crates/tf_tree_core/src/sample.rs:151` and `:207` — `self.head.load(Ordering::Acquire)`.
   LLVM's `LoopVectorizationLegality` rejects any loop containing a non-simple
   (atomic or volatile) load outright. Two of them bracket every sample.
2. `crates/tf_tree_core/src/buffer.rs:328`–`:349` — `read_slot`'s seqlock: an
   inner retry loop with a data-dependent trip count, and `fence(Ordering::Acquire)`
   at `:343`, which is an unconditional barrier for the optimiser. Reached twice
   per interpolated sample.
3. `crates/tf_tree_core/src/plan.rs:1501` (and `:1329`) — the `?` on
   `Result<Iso3, LookupError>`. A data-dependent early exit; the loop vectoriser
   wants a countable loop with a single latch exit, and early-exit vectorisation
   is off by default.
4. `crates/tf_tree_core/src/sample.rs:193` → `bracket_from`, whose probe
   addresses come from `stamp_at` at `sample.rs:219` — an inner loop with a
   data-dependent trip count over gathered, serially dependent loads.
5. Only then, inside the arithmetic itself:
   `crates/tf_tree_math/src/interp.rs:122`–`:127` (the two endpoint shortcuts),
   `:170` (the degenerate-input early return), `:176`–`:186` (the threshold
   branch) and `:190`–`:193` (`libm::acos` / `libm::sin`, opaque calls with no
   vector form).

Blocker 5 is the only one this record ever contemplated, and it is the only one
that is genuinely removable.

### 3. What is reachable for free — measured, not argued

The cheap test §14 asks for. `crates/tf_tree_bench/examples/autovec_probe.rs`
times four shapes of the *same* interpolation over the same 1024 pose pairs, one
200 Hz tick apart on a body turning at 180 °/s so the series branch is the one
taken. The harness asserts every variant is **bit-identical** to
`LerpSlerp::eval` on that data before it reports a timing — it is (0.000e0
absolute), so these are four spellings of one function and not four functions.
Best of 7 × 20 000 rounds within a run, best over ≥3 runs per build, ns/element.
**The host was shared for part of this session and a single run excursed by up to
+50%** — that is why best-of is used and why nothing under ~5% below is claimed
as a result.

| shape | as built | `-C no-vectorize-slp` | + `-C no-vectorize-loops` | what the compiler did |
|---|---|---|---|---|
| **A** `LerpSlerp::eval` in a loop | **17.81** | 19.26 | 19.37 | SLP only, ×2, *within* one eval |
| **A'** `ScLerp::eval` in a loop | **44.66** | 46.49 | 46.66 | SLP only |
| **B** branch-free, array-of-structs | **11.87** | 12.00 | 17.86 | **loop-vectorised across stamps, ×2** |
| **C** branch-free, structure-of-arrays | **10.65** | 10.70 | 20.24 | **loop-vectorised across stamps, ×2** |
| **D** `[f64; 4]` blocks | 19.32 | 19.21 | 19.03 | nothing |

Three things fall out, and the third is the one that matters.

- **Autovectorisation across stamps works, with no dependency and no `unsafe`.**
  B and C are unaffected by disabling SLP and collapse by 1.49× and 1.89× when
  the *loop* vectoriser is disabled, which is what proves they are widened across
  `i` rather than within an element. 17.81 → 10.65 ns is a **1.67×** on the
  interpolation arithmetic, reached by deleting blocker 5 and nothing else.
- **It is unreachable from where the engine stands.** B and C are loops that do
  *only* arithmetic. Getting one requires splitting the fused fold — locate and
  read every stamp's bracket first, interpolate second — which buffers 2N `Iso3`
  (128 KiB at N = 1024, against this host's 32 KiB L1d) and is exactly the
  footprint trade §12 measured as the thing that decides this engine's speed.
  That restructure is not in this record, is not costed by it, and **is a
  prerequisite for step 4 as much as for step 3**: `pulp` cannot vectorise a loop
  containing a seqlock either. The record's implementation plan reads as though
  step 3 and step 4 were alternatives at the same site. They are not; step 4
  silently assumes step 3's restructure already happened.
- **Step 3's literal instruction is a pessimisation.** "Shape the batch
  interpolation loop over `[f64; 4]`" is variant D. It vectorises not at all —
  no flag moves it — and it is **~8% slower than the shipped scalar loop**. The
  `[f64; 4]` blocking that was supposed to give the vectoriser a known trip count
  instead gives it four live accumulator arrays and a two-pass body it will not
  fuse. Do not spend a day on it; it has been spent.

### 4. The finding that inverts this record's premise

The premise is that interpolation is arithmetic-bound and under-served by a
2-lane ALU, so widening it is the lever. The asm says it is already 2-lane. The
measurement says the 2 lanes are **costing** us.

`at_many`, criterion, `taskset -c 2`, 1 s warm-up / 3 s measurement, µs per 1024
elements. **Both binaries are built first and then run alternately, ON/OFF/ON/OFF,
four pairs** — the first pass at this was not interleaved and reported −15.3% on
the first row, which was host load sitting on the ON side. Interleaving is what
makes the comparison survive a shared machine, and it is also what makes the
result unanimous: **OFF is faster in 24 of 24 row-pairs**. Code alignment is ruled
out separately by three further builds (`-C llvm-args=-slp-threshold=` at 50, 200
and 5000) all landing in the OFF band; a layout fluke does not reproduce across
five codegens.

| row | SLP on (best of 4) | SLP suppressed (best of 4) | Δ |
|---|---|---|---|
| `monotone_1024` | 293.0 | 260.0 | **−11.3%** |
| `into_affine32_1024` | 287.5 | 255.3 | **−11.2%** |
| `two_pass_mat4_1024` | 296.8 | 263.4 | **−11.2%** |
| `into_mat4_1024` | 286.5 | 254.9 | **−11.0%** |
| `into_quat_1024` | 283.7 | 254.5 | **−10.3%** |
| `into_quat_twist_1024` | 349.6 | 341.1 | −2.4% |

And on the *scalar* path, which this record correctly says batch SIMD would never
help — `lookup`, ns, same interleaved method, best of 3 pairs:

| row | SLP on | SLP suppressed | Δ |
|---|---|---|---|
| `depth3/lerpslerp` | 146.68 | 124.32 | **−15.2%** |
| `depth3/sclerp` | 190.25 | 190.24 | 0.0% |
| `depth1/sclerp` | 67.96 | 68.14 | +0.3% |
| `depth6/sclerp` | 132.88 | 132.94 | +0.0% |

**The counts say what the mechanism is not.** Turning SLP off *raises* the
instruction count of `Plan::at_many_into` (2460 → 2785) and *raises* the spill
count (117 → 145 stores, 139 → 206 reloads), and it is still ~11% faster. So
this is not register pressure and not code size. The only class that shrinks to
zero is the 227 `shufpd`/`unpckhpd`/`unpcklpd` needed to get `Quat` and `Vec3`
components into and out of lane pairs — 227 shuffles bought in exchange for 326
fewer FP operations. That trade is consistent with shuffles being port-limited
where the FP arithmetic is not, but this host has neither `perf` nor `valgrind`
available, so **the port explanation is the shape the counts are consistent with,
not a measured one**, and it is written down here as a hypothesis for whoever has
a machine with counters.

Why `at_many` under `ScLerp` moves ~11% while `lookup/depth3/sclerp` under the
same policy does not move at all is **not explained**. The two go through
different folds (`fold_at_cursors` versus `fold_at`) and the `lookup` bench asks
for one fixed stamp forever while `at_many` sweeps 1024, so both the code and the
cache behaviour differ. Stating a cause here would be the exact failure §14 names
as a rule. It is recorded as unexplained.

### 5. What this does to the cost/benefit

The framing in open question 1 — *"31% of per-step interpolation cost, on
`at_many` only, against `tf_tree_math` going from 2 dependencies to 11"* —
survives as an order of magnitude but three of its terms are now wrong:

- **"31%" is not on the table for `pulp` any more than for the compiler.** The
  22.2 ns is not scalar; it is 2-lane SLP with the shuffles already paid. What a
  wider ALU can win over *today's* code is the 1.67× that variant C shows, and
  only after a loop split neither this record nor `pulp` provides. Against the
  72.5 ns step that is ~9 ns, ~12% — not 31%.
- **The `x86-v3` advantage is real but it is an advantage over the *build
  target*, not over the *compiler*.** Anyone weighing nine dependencies should
  first be told that `-C target-cpu=x86-64-v3` is free, costs nothing in the
  budget, and buys the same 4 lanes for code that is already being vectorised —
  at the price of a binary that `SIGILL`s on pre-2013 hardware, which is exactly
  the trade `pulp`'s runtime detection exists to avoid. That comparison belongs
  in the decision and was missing from it.
- **There is a cheaper lever in front of this one, and it is subtraction.**
  Suppressing SLP is worth ~11% on `at_many` and 15.2% on the `LerpSlerp`
  scalar lookup — comparable to what the dependency is being considered for, on
  more paths, for no crates. **This amendment does not propose taking it**, and
  no code was changed: there is no stable per-function way to say it, and putting
  `-C no-vectorize-slp` in `.cargo/config.toml` would apply to this workspace's
  builds and not to an embedder's, which would make every number this repository
  publishes describe a binary no consumer builds — the precise failure
  `[profile.embedder]` and `docs/PHASE5.md` §9.2 exist to prevent. It is recorded
  because it is larger than the thing being bought and because it means the
  premise "the arithmetic is not vectorised" was false.

**What this amendment does not do.** It does not close this record, change its
status, or touch questions 1, 3 or 4. It answers question 2: *no, autovectorisation
does not get there for free — but the reason is the engine's loop, not the
arithmetic, and that same reason blocks `pulp`.*
