# 0063: `tf_tree_math` delegates SO(3) and SE(3) to `helicoid` behind an off-by-default feature

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none

## Context

`helicoid` 0.0.2 and `helicoid-linalg` 0.0.2 are on crates.io. `helicoid`'s records
`0041` and `0049` (both `ready`) decide that it replaces `tf_tree_math`'s Lie-group
bodies through an adapter at this crate, not by substituting types, and that the
`tf_tree` side is a feature. That decision names a dependency this workspace's
budget (D14: `tf_tree_math` = `libm` + `bytemuck`) does not list, so it is taken here
too, as `CLAUDE.md` requires.

## Decision

1. `tf_tree_math` gains an **optional** dependency on `helicoid` and `helicoid-linalg`
   (`=0.0.2`, pinned exactly while the line is `0.0.x`) and a feature
   `helicoid = ["dep:helicoid", "dep:helicoid-linalg"]`. **Off by default.** Both are
   `no_std`, no `alloc`, no `unsafe`, and add only `libm` (already a dependency) to the
   closure.
2. With the feature on, a function listed below calls `helicoid` through a private
   module; with it off, the body that exists today runs. The public signatures,
   `Quat`/`Iso3` layouts (`Pod`) and facade re-exports do not change.
3. **Wave 1 — SO(3):** `exp_so3`, `log_so3`, `quat_from_rot3`, `slerp`, `Quat::rotate`.
   **Every quaternion enters `helicoid` on its *carried* path** (`helicoid` `0058`): a struct
   literal moved into `SO3::from_quat_unchecked`, never the vouching `Quat::from_wxyz_unchecked`.
   `Iso3` composition never normalizes, so this crate's quaternions drift by design. A 50k-compose
   test reaches `‖q‖² − 1 ≈ 1e-9`, past the vouching assert's `2^-40` and inside the carried
   path's `2^-26.29` band, where `helicoid` states each operation's error. The carried path asserts
   nothing and propagates NaN, as this crate's functions do.
   - `Quat::rotate` is **bit-identical** to its native twin, drifted or not: `SO3::act` is the
     same sandwich in the same order (`rotate_is_the_native_twin_to_the_bit_drifted_or_not`).
   - `quat_from_rot3` is a **recorded behaviour change** under the feature. It returns the
     normalized quaternion `SO3::from_matrix` returns, where today's body returns an
     un-normalized one (`helicoid` `0049` decision 4).
   - Two `slerp` differences are accepted and pinned. It extrapolates off `[0, 1]`, where the
     native series collapses. Identical inputs return `qa` to a few ulp, where native returns the
     bit, and a NaN `s` returns NaN, because there is no early return.
   - One drift difference is a gain. On near pairs, `slerp`'s chord-based angle reads the norm
     difference as rotation and loses up to `~4 000 u` at `2^-26.29`, while `helicoid`'s provided
     body is scale-invariant and stays at `6.5 u` (`helicoid` `0058`, Measured).
   - `slerp`'s latency is at parity from `helicoid` 0.0.2 (`helicoid` `0059`):
     `lookup/depth3/lerpslerp` reads 211.7 ns against 209.3 ns native, where 0.0.1 read
     239.1 ns. So decision 5 keeps it delegated.
   - The native-contract tests that pin these differences are `cfg(not(feature = "helicoid"))`,
     and the feature arm has its own.
   - `Quat::{dot, norm, norm_squared, normalize}` and `Iso3::normalized` stay this crate's
     permanently (`0049`).
4. **Waves 2 and 3** (SE(3); the screw path) are separate PRs under the same feature, each
   gated as `helicoid` `0041` decision 4 says. Nothing here decides them.
5. A function that regresses on accuracy or latency stays on its old body; the feature is
   not all-or-nothing. The old bodies stay as the *adapter twins* (`helicoid` `0049`
   Consequences): they answer "did delegation change the answer".
6. Making the feature default is a separate, later decision.

## Rationale

An adapter keeps the ABI, the arena storage contract and the `same_item` facade tests
fixed, and reduces the integration to "do these two implementations agree", which
`helicoid`'s oracle runner already measures (`helicoid` `0010`, `0041`). An optional,
off-by-default dependency keeps D14 true for every consumer who does not opt in.

## Consequences

- `cargo deny` and the SBOM see two more crates under `--all-features`; both are MIT OR
  Apache-2.0 and add no `*-sys` crate.
- CI gains a second arm: `tf_tree_math`'s tests run with and without `helicoid`.
- `tf_tree_math`'s semver now includes the feature name.

## Implementation plan

1. This record.
2. The feature scaffold and Wave 1, with a `just` recipe running `tf_tree_math`'s tests under
   both arms — verified by `cargo nextest run -p tf_tree_math` and
   `cargo nextest run -p tf_tree_math --features helicoid`.
3. Wave 2, then Wave 3 with `lookup/depth3/sclerp` A/B for parity (`helicoid` `0041`, as amended)
   — verified by `just bench-check` under both arms.

## Open questions

None.
