# 0063: `tf_tree_math` delegates SO(3) and SE(3) to `helicoid` behind an off-by-default feature

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none

## Context

`helicoid` 0.0.1 and `helicoid-linalg` 0.0.1 are on crates.io. `helicoid`'s records
`0041` and `0049` (both `ready`) decide that it replaces `tf_tree_math`'s Lie-group
bodies through an adapter at this crate, not by substituting types, and that the
`tf_tree` side is a feature. That decision names a dependency this workspace's
budget (D14: `tf_tree_math` = `libm` + `bytemuck`) does not list, so it is taken here
too, as `CLAUDE.md` requires.

## Decision

1. `tf_tree_math` gains an **optional** dependency on `helicoid` and `helicoid-linalg`
   (`=0.0.1` while the line is `0.0.x`) and a feature `helicoid = ["dep:helicoid",
   "dep:helicoid-linalg"]`. **Off by default.** Both are `no_std`, no `alloc`, no
   `unsafe`, and add only `libm` (already a dependency) to the closure.
2. With the feature on, a function listed below calls `helicoid` through a private
   module; with it off, the body that exists today runs. The public signatures,
   `Quat`/`Iso3` layouts (`Pod`) and facade re-exports do not change.
3. **Wave 1 — SO(3):** `exp_so3`, `log_so3`, `quat_from_rot3`, `slerp`. **`Quat::rotate` takes
   decision 5's fallback**: `Iso3` composition feeds it quaternions drifted past `helicoid`'s
   `2^-40` debug domain by design (a 50k-compose test reaches norm error `1e-9`), and the formula
   is `SO3::act`'s, so delegating buys no accuracy.
   `quat_from_rot3` is a **recorded behaviour change** under the feature: it returns the
   normalized quaternion `SO3::from_matrix` returns, where today's body returns an
   un-normalized one (`helicoid` `0049` decision 4). Three more differences are accepted and
   pinned: `slerp` extrapolates off `[0, 1]` (the native series collapses there); `slerp` of
   identical inputs returns `qa` to a few ulp, where native returns the bit; and `log_so3`/`slerp`
   take `helicoid`'s unit domain as a `debug_assert!`, so a NaN or an off-unit input panics a
   *debug* build and is garbage-in-garbage-out in release. The native-contract tests are
   `cfg(not(feature = "helicoid"))`; the feature arm has its own. `Quat::{dot, norm, norm_squared,
   normalize}` and `Iso3::normalized` stay this crate's permanently (`0049`).
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
