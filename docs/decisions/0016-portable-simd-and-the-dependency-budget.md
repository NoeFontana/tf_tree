# 0016: portable SIMD, and what it costs the dependency budget

**Status:** withdrawn
**Owner:** @NoeFontana
**Implementation:** none, and none is planned. **This record is withdrawn**
(2026-08-29, by the owner). The spike was reverted and is not coming back; what
the record established is kept below and in the *Withdrawal* section, which is
the only part a reader needs.

## Withdrawal (2026-08-29)

Withdrawn because the *Decision*'s site, "the `Interp::eval` inner loop reached from `Plan::at_many`", does not exist (Amendment §1).

* **`-C target-cpu=x86-64-v3` is permitted and not adopted: on this workload it is slower** (`at_many`, EPYC-Milan): `monotone_1024` 271.9-275.9 us to 299.7-308.5 us; `into_mat4_1024` 271.8 to 297.9 us; `into_quat_1024` 274.4 to 289.0 us. The flag stays a named contrast, never ambient: not in `.cargo/config.toml`, not in `wheels.yml`.
* **`pulp` is rejected on the evidence:** ~12% of the step on `at_many` only, against `tf_tree_math` going from 2 dependencies to 11. Reopening needs a measurement that clears that bar.
* **Anyone adopting the flag owes a cross-build differential** with a non-vacuity guard that counts `%ymm` **per symbol** over only the functions compared (`LerpSlerp::eval`, `ScLerp::eval`, `slerp`, `exp_se3`/`log_se3`): `blake3`'s runtime-dispatched AVX kernels make a whole-binary count nonzero at baseline.

## Decision

**Proposed, never accepted:** add `pulp` (`features = ["x86-v3"]`) to `tf_tree_math` for batch interpolation only (`Plan::at_many`, `at_many_into`, `at_many_into_f32`, `at_adaptive`), keeping `#![forbid(unsafe_code)]`.

## Implementation plan

Never executed; step 3 (autovectorisation over `[f64; 4]`) is a pessimisation (Amendment §3).

## Open questions

1. **Is a batch-only win worth nine dependencies?** No (Amendment §5).
2. **Does autovectorisation get there for free?** No (Amendment §1-§4).
3. **Is `num-complex` acceptable as an unused transitive?** Moot.
4. **What replaces Miri's coverage of the wide path?** Nothing is lost: no `core::arch` or `cfg(target_feature)` exists in `crates/*/src`.

## Amendment — open question 2, answered by reading the asm

### 1. What the asm shows

There is no loop across stamps to vectorise: `Plan::at_many_into`'s monotone branch is one loop whose body is a complete plan fold, one stamp per iteration. SLP already vectorises a single `eval` at two lanes.

### 2. The blockers

`SampleRing::sample_from`'s `head.load(Acquire)`; `read_slot`'s seqlock and `fence(Acquire)`; the `?` in `Plan::fold_batch`; `bracket_from` / `stamp_at`; `LerpSlerp::eval`'s endpoint shortcuts, `slerp`'s branches, `libm::acos` / `libm::sin`. `pulp` removes none of them.

### 3. What is reachable for free

`autovec_probe` (bit-identical to `LerpSlerp::eval`), ns/element: **A** `LerpSlerp::eval` in a loop 17.81; **B** branch-free AoS 11.87; **C** branch-free SoA 10.65; **D** `[f64; 4]` blocks 19.32 (not vectorised, ~8% slower than A). B and C need a fold split buffering 2N `Iso3` (128 KiB at N = 1024).

### 4. The finding that inverts the premise

With SLP suppressed `at_many` is ~11% faster and `lookup/depth3/lerpslerp` 15.2% faster.

### 5. Cost/benefit

A wider ALU wins C's 1.67x only after a loop split, ~12% of the step. `-C target-cpu=x86-64-v3` buys the same lanes at no budget cost, but `SIGILL`s on pre-2013 hardware. Suppressing SLP is not proposed: a `.cargo/config.toml` flag would not reach an embedder's builds (`[profile.embedder]`, `docs/PHASE5.md` §9.2).
