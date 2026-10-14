# 0064: `tf_tree_math` uses `helicoid` by default

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** this record's PR; ships in 0.0.7

## Context

[`0063`](./0063-the-math-crate-delegates-behind-a-feature.md) put `tf_tree_math`'s SO(3) bodies
behind an off-by-default feature, `helicoid`, and left the default to "a separate, later
decision" (its decision 6). Wave 1 (`exp_so3`, `log_so3`, `quat_from_rot3`, `slerp`,
`Quat::rotate`) now runs on the published `helicoid` 0.0.4. The measurements below come from
`helicoid`'s conformance corpus, whose `tf_tree_math` oracle runner is this crate's native body,
and from this crate's own tests.

**Accuracy.** Each function's worst error per corpus stratum, in units of `2^-53`, at binary64:

| function | strata | `helicoid` better | worse | equal | worst, native | worst, `helicoid` |
|---|---|---|---|---|---|---|
| `log_so3` | 29 | 6 | 4 | 19 | 3.97e14 | 2.63 |
| `exp_so3` | 28 | 3 | 1 | 24 | 2.74 | 2.74 |
| `slerp` (`so3_geodesic`) | 3 | 2 | 0 | 1 | 2.19 | 1.74 |
| `Quat::rotate` (`so3_act`) | 29 | 0 | 0 | 29 | 4.90 | 4.90 |
| `quat_from_rot3` (`so3_from_matrix`) | 30 | 0 | 0 | 30 | 1.71 | 1.71 |

- **`log_so3`'s 3.97e14** is the native body at `θ:subnormal` (`θ ≈ 1e-310`). Its `θ / n` scaling
  loses every digit there, while `helicoid` reads 2.5e-16. The other differences are a few
  tenths of a unit either way.
- **`slerp` on drifted pairs.** It stays at 6.5 units where the native chord loses up to about
  4 000 at the carried band's edge (`0063` decision 3).

**Bits.** `Quat::rotate` is the native body to the bit, drifted or not. `quat_from_rot3` is the
native body's bits on 97.0 % of 1.2 M rotations, and within `9.5e-16` on the rest, where the two
pick different pivots.

**Latency.** `lookup/depth3/lerpslerp` reads 211.7 ns against 209.5 ns native, inside the
3 ns A/A spread (`0063` decision 3).

**What it costs.** `helicoid` and `helicoid-linalg` are `no_std`, no `alloc`, `forbid(unsafe_code)`,
MIT OR Apache-2.0, and depend on `libm` alone. `libm` is already in the closure. Their MSRV is 1.85,
below this workspace's 1.87. But `tf_tree_core` depends on `tf_tree_math`, so a default feature puts
both crates in the core's closure, which D14 lists as exactly `libm`, `bytemuck` and `blake3`.

## Decision

1. **`tf_tree_math`'s `helicoid` feature is on by default:** `default = ["helicoid"]`. Every crate
   that depends on `tf_tree_math` with default features, `tf_tree_core` and `tf_tree` included,
   runs Wave 1 on `helicoid`. `--no-default-features` gives the native bodies, which stay
   compiled as the adapter twins.
2. **D14 is amended.** `tf_tree_core`'s third-party closure is `libm`, `bytemuck`, `blake3`, and
   `helicoid` and `helicoid-linalg` through `tf_tree_math`. Those two must stay `no_std`, free of
   `alloc` and `unsafe`, and depend on `libm` alone. A release of either that adds a dependency
   is not adopted without a new record. The pin stays exact (`=0.0.x`) while their line is
   `0.0.x`.
3. **Both arms stay tested.** `just test-rust` runs `tf_tree_math` with `--no-default-features`,
   where the native-contract tests run, and `just lint` clippies that arm. The workspace run is now
   the `helicoid` arm.
4. **Waves 2 and 3 (SE(3) and the screw path) are unchanged.** They join the default, under the
   same feature, only when each meets `helicoid` `0041` decision 4's bar (`0063` decision 4).

## Rationale

The feature was off so that adoption could be measured before it was imposed, and the
measurement is in:
- no function is less accurate beyond a few tenths of a unit;
- one function stops failing outright at subnormal angles;
- two are bit-identical or nearly so;
- latency is at parity.

Keeping it off would make every default consumer run the less accurate bodies, and leave the
`helicoid` arm exercised only by the users who know to ask for it.

D14 exists to keep `serde`, async runtimes, linear-algebra frameworks and logging out of the
core: dependencies that bring a transitive tree, `std`, or policy. These two bring none of that.
They add two crates whose own closure is the `libm` the core already has. So the amendment names them and the conditions that keep the reason
for D14 true, rather than loosening the rule.

Rejected:
- **Default on the facade only.** Cargo unifies features, so which arm a build gets would
  depend on which crates it happens to include: one with `tf_tree_core` or `tf_tree_tf2_sys` but
  not `tf_tree` would get the native bodies. The default belongs to the crate that owns the
  bodies.
- **Waiting for Waves 2 and 3.** The feature already works function by function (`0063`
  decision 5), and Wave 1's measurement doesn't depend on them.

## Consequences

- **0.0.7 changes behaviour for default builds:**
  - `log_so3` is finite and accurate at subnormal angles.
  - About 3 % of `quat_from_rot3`'s outputs move in their last bits.
  - `slerp` extrapolates off `[0, 1]`, returns `qa` to a few ulp (not the bit) for identical
    inputs, and returns NaN for a NaN `s`.

  A consumer that needs the old bits builds with `default-features = false`.
- **The arena format and ABI are unchanged:** no layout, `Pod` type or signature moves. Bits that
  a process writes from these functions can differ between a 0.0.6 and a 0.0.7 participant.
  0.0.x already requires every participant on one version.
- **Tooling:** `cargo deny` and the SBOM list `helicoid` and `helicoid-linalg` in the default build.
- **Docs:** D14's text, `CLAUDE.md`'s dependency budget, and `PHASE2.md` §15's D14 line name the
  two crates.

## Implementation plan

1. This record, the feature default, the recipes, the D14 amendment and the changelog. Verified
   by:
   - `just test-rust`, both arms;
   - `just lint`;
   - `cargo deny check`;
   - `cargo tree -p tf_tree_core -e normal`, which shows `helicoid` and `helicoid-linalg` with
     `libm` alone beneath them.

## Open questions

None.
