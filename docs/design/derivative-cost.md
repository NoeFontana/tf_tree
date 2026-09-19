# Design: what `at_with_derivatives` actually costs

**Status:** implemented; one tuning lever **falsified by measurement** (§4).
**Reproduce:** `cargo run --release -p tf_tree_bench --example deriv_cost` and `--example adjoint_cost`

## 1. Corrections to `PHASE4.md` §2's cost model

| Claim | Reality |
|---|---|
| §2.3: "ξ is *already computed*" | recovering it costs **+1.6 ns on a 43 ns interpolation, 1.04×** |
| §2.3 / §7: "roughly 2× a plain lookup" | **1.22× at depth 3** |

## 2. ξ-recovery is nearly free

`ScLerp::eval_with_twist` vs `ScLerp::eval`: ratio **1.04** (adjacent 1 kHz),
1.06 (slow 10 Hz), 1.06 (large arc). The ratio is flat because on the
small-angle path (up to 50 Hz) `φ` comes from a `sqrt` of a value the power
already computes.

`eval_with_twist` tests `s == 0.0` / `s == 1.0` **before** raising `rel` to a
power; the twist is still computed. Endpoints save **58.6%** (adjacent 1 kHz) and
**60.0%** (large arc).

## 3. At plan level, the adjoint is the cost

| depth | `at` ns | `at_with_derivatives` ns | ratio | delta/step ns |
|---|---|---|---|---|
| 1 | 97.8 | 118.1 | 1.21 | 20.30 |
| 3 | 281.1 | 343.7 | 1.22 | **20.87** |
| 8 | 738.1 | 907.9 | 1.23 | 21.23 |

The per-step delta is flat at ~21 ns: 1.6 ns is ξ-recovery, the rest is the
adjoint and the `sample_with_twist` path. **§7 gate row: 1.22× at depth 3.**

## 4. Falsified: a matrix form of the adjoint

Hypothesis: build the rotation matrix once, then two mat-vecs, instead of
`Iso3::adjoint_inv`'s two `Quat::rotate` calls.

| regime | quaternion ns | matrix ns | speedup |
|---|---|---|---|
| independent inputs | 1.45 | 1.62 | **0.90** |
| sequential (as the fold runs) | 10.62 | 9.60 | **1.11** |

**Rejected:** 1.11× is below the 1.15× bar for a second code path, and the
quaternion form is *faster* on independent inputs.
