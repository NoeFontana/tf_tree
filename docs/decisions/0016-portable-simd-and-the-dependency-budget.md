# 0016: portable SIMD, and what it costs the dependency budget

**Status:** withdrawn
**Owner:** @NoeFontana
**Implementation:** none, and none is planned. **This record is withdrawn**
(2026-08-29, by the owner). The spike was reverted and is not coming back; what
the record established is kept below and in the *Withdrawal* section, which is
the only part a reader needs.

## Withdrawal (2026-08-29)

**Withdrawn rather than taken to `ready`:** the *Decision* names "the
`Interp::eval` inner loop reached from `Plan::at_many`" as the site to vectorise,
and Amendment §1 proves that loop does not exist, so it cannot be implemented as
written.

* **`-C target-cpu=x86-64-v3` is permitted and not adopted: on this workload it is
  slower.** On an AMD EPYC-Milan host (`at_many`, baseline vs the flag, alternating
  runs, every difference at p = 0.00):

  | `at_many` bench | baseline | `-C target-cpu=x86-64-v3` |
  |---|---|---|
  | `monotone_1024` | 271.9-275.9 us | **299.7-308.5 us** |
  | `into_mat4_1024` | 271.8 us | **297.9 us** |
  | `into_quat_1024` | 274.4 us | **289.0 us** |

  Suppressing SLP made the code ~11% faster (§4); widening the lanes costs 8-14%:
  this fold does not want more lanes. The flag stays a named contrast, never
  ambient: not in `.cargo/config.toml`, and not in `wheels.yml`.
* **`pulp` is rejected on the evidence, not in principle**: the gain is ~12% of
  the step on `at_many` only, against `tf_tree_math` going from 2 dependencies to
  11. Reopening needs a measurement that clears that bar.
* **Anyone adopting the flag owes a cross-build differential** with a non-vacuity
  guard that counts `%ymm` **per symbol** over only the functions compared
  (`LerpSlerp::eval`, `ScLerp::eval`, `slerp`, `exp_se3`/`log_se3`): `blake3`'s
  runtime-dispatched AVX kernels make a whole-binary count nonzero at baseline.

## Decision

**Proposed, never accepted:** add `pulp` (`features = ["x86-v3"]`) to
`tf_tree_math` and use it for batch interpolation only (`Plan::at_many`,
`at_many_into`, `at_many_into_f32`, `at_adaptive`), keeping
`#![forbid(unsafe_code)]`. The scalar `Plan::at` path is not touched.

## Implementation plan

Never executed; step 3 (autovectorisation over `[f64; 4]`) is a pessimisation (Amendment §3).

## Open questions

1. **Is a batch-only win worth nine dependencies?** Amendment §5: no, on the
   evidence.
2. **Does autovectorisation get there for free?** Answered by the Amendment: no,
   and not for the reason this record assumed.
3. **Is `num-complex` acceptable as an unused transitive?** Moot.
4. **What replaces Miri's coverage of the wide path?** Nothing is lost: Miri
   interprets MIR, and no `core::arch` or `cfg(target_feature)` exists in
   `crates/*/src`.

## Amendment — open question 2, answered by reading the asm

### 1. What the asm shows

There is no loop across stamps for anything to vectorise: `Plan::at_many_into`'s
monotone branch is one loop whose body is a complete plan fold (`fold_at_cursors`,
`Guard::sample_from`, the bracket search, two seqlock `read_slot`s and one
`Interp::eval`), one stamp per iteration. The arithmetic is already vectorised by
SLP within a single `eval`, at two lanes (480 packed `%xmm` operations; none
wider, because the baseline target is SSE2).

### 2. Why it is not vectorised across stamps — the blockers, named

The engine blocks it, and `pulp` removes none of it: `SampleRing::sample_from`'s
`head.load(Acquire)`; `SampleRing::read_slot`'s seqlock and `fence(Acquire)`; the
`?` in `Plan::fold_batch`; `bracket_from` / `stamp_at`. Only then the arithmetic:
`LerpSlerp::eval`'s endpoint shortcuts, `slerp`'s early return and threshold
branch, and `libm::acos` / `libm::sin`.

### 3. What is reachable for free — measured, not argued

`autovec_probe` (bit-identical to `LerpSlerp::eval` before timing), ns/element:
**A** `LerpSlerp::eval` in a loop 17.81; **B** branch-free AoS 11.87; **C**
branch-free SoA 10.65; **D** `[f64; 4]` blocks 19.32. B and C are loop-vectorised
across stamps (1.49x and 1.89x collapse when loop vectorisation is disabled); D
is not vectorised and is ~8% slower than the shipped loop. Reaching B/C needs a
fold split buffering 2N `Iso3` (128 KiB at N = 1024 against a 32 KiB L1d), a
prerequisite for any batch SIMD. Nothing under ~5% is claimed as a result.

### 4. The finding that inverts this record's premise

With SLP suppressed `at_many` is ~11% faster and `lookup/depth3/lerpslerp` 15.2%
faster; the cause (shuffle port pressure) is a hypothesis.

### 5. What this does to the cost/benefit

A wider ALU wins variant C's 1.67x only after a loop split, ~12% of the step. `-C target-cpu=x86-64-v3` buys the
same 4 lanes at no budget cost, at the price of a binary that `SIGILL`s on pre-2013
hardware. Suppressing SLP is a larger lever, not proposed: a `.cargo/config.toml`
flag would not reach an embedder's builds, so every published number would describe
a binary no consumer builds (`[profile.embedder]`, `docs/PHASE5.md` §9.2).
