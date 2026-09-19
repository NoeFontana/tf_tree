# Design: what `at_with_derivatives` actually costs

**Status:** implemented; one tuning lever **falsified by measurement** (§4).
**Measured on:** AMD EPYC-Milan, 4 physical cores, idle, `taskset -c 2`
**Reproduce:** `cargo run --release -p tf_tree_bench --example deriv_cost` and `--example adjoint_cost`

## 1. Corrections to `PHASE4.md` §2's cost model

| Claim | Reality |
|---|---|
| §2.3: "ξ is *already computed*" | `ScLerp::eval` never materializes ξ. Recovering it costs **+1.6 ns on a 43 ns interpolation, 1.04×** |
| §2.3 / §7: "roughly 2× a plain lookup" | **1.22× at depth 3** |

## 2. ξ-recovery is nearly free

`ScLerp::eval_with_twist` vs `ScLerp::eval`, identical pose pairs:

| regime | rel. angle | `eval` ns | `eval_with_twist` ns | ratio |
|---|---|---|---|---|
| adjacent 1 kHz | 3.0e-3 | 42.95 | 44.51 | **1.04** |
| slow 10 Hz | 3.14e-1 | 51.85 | 55.04 | 1.06 |
| large arc | 1.5 | 58.31 | 61.82 | 1.06 |

The ratio is flat because `ScrewParts::twist` mirrors `pow`'s branch: on the
small-angle path (everything up to 50 Hz) `φ` comes from a `sqrt` of the
`theta_sq_from_chord` value the power already computes, so no transcendental is added.

### 2a. The endpoints skip the power

`ScLerp::eval_with_twist` tests `s == 0.0` / `s == 1.0` **before** raising `rel`
to a power (LLVM does not sink the call out of the untaken branch). The twist is
still computed; only the power is skipped. Both endpoints are common: `s == 0.0`
on an exact hit, `s == 1.0` at `t == t_new`, where every `latest`-style query lands.

| regime | interior ns | endpoint ns | saved |
|---|---|---|---|
| adjacent 1 kHz | 54.60 | 22.62 | **58.6%** |
| large arc | 69.02 | 27.59 | **60.0%** |

## 3. At plan level, the adjoint is the cost

| depth | `at` ns | `at_with_derivatives` ns | ratio | delta/step ns |
|---|---|---|---|---|
| 1 | 97.8 | 118.1 | 1.21 | 20.30 |
| 3 | 281.1 | 343.7 | 1.22 | **20.87** |
| 8 | 738.1 | 907.9 | 1.23 | 21.23 |

The per-step delta is flat at ~21 ns: 1.6 ns is ξ-recovery, the other ~19 ns is
the adjoint and the `sample` → `sample_with_twist` path change, so tuning belongs
there. **§7 gate row: 1.22× at depth 3 against an expected ~2×.**

## 4. Falsified: a matrix form of the adjoint

`Iso3::adjoint_inv` (two `Quat::rotate` calls plus a cross, ~40 flops) is
latency-bound. Hypothesis: build the rotation matrix once, then two mat-vecs with
independent products. Forms agree to 7.5e-16.

| regime | quaternion ns | matrix ns | speedup |
|---|---|---|---|
| independent inputs | 1.45 | 1.62 | **0.90** |
| sequential (as the fold runs) | 10.62 | 9.60 | **1.11** |

**Rejected:** 1.11× is below the 1.15× bar for a second code path (~1 ns per step
on an API already 1.6× inside budget), and the quaternion form is *faster* on
independent inputs, so a parallel fold or batch API would regress. The probe stays
as `examples/adjoint_cost.rs`.

## 5. Not attempted

The ~19 ns that is neither ξ nor the adjoint: a const-generic `WANT_TWIST: bool`
could merge `sample_with_twist` into `sample`'s search (measure first; `sample` is
the hottest function). And `at_many_with_derivatives` over the galloping cursor.
Neither has a requester.
