# 0039: extrapolation you cannot fail to notice

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #278, #283

## Context

`ExtrapPolicy::{Error, Hold, ConstantTwist}` existed in `tf_tree_core::sample` but no consumer could select one: every fold site passed `Error` and `tf_tree` did not re-export the type.

## Decision

**Extrapolation is selected per query, and its result cannot be read without also reading how far it was extrapolated.**

### 1. `Extrapolated`

`Extrapolated { pose: Iso3, by_ns: i64, edge: EdgeId }` is `Copy`. `by_ns` is nanoseconds past the newest stamp every dynamic edge has data for (`0`: every edge bracketed the query); `edge` is the one that ran out first. There is no accessor yielding the pose alone.

### 2. Methods

`Plan::at_extrapolating<D: Domain>(&self, g, t, policy)` and `Plan::at_extrapolating_tagged(&self, g, nanos, domain, policy)` return `Result<Extrapolated, LookupError>`. `Plan::at` still passes `Error`. `ExtrapPolicy` and `Extrapolated` are re-exported from `tf_tree`.

### 3. `by_ns` is measured against `latest_common`, before the fold

`by_ns = max(0, t - min over dynamic edges of newest_stamp)`, via `newest_common`. The walk runs **before** the fold: `newest_stamp` is non-decreasing, so a concurrent `push` can only over-report, never report `0` for an invented pose. Not `note`d (`lookups_ok` would double). Test: `by_ns_zero_is_never_claimed_for_a_pose_the_fold_invented`.

### 4. The hot path does not pay

The distance costs `d` `newest_stamp` loads on the extrapolating path only; `Plan::at` is unchanged (`just bench-check`).

## Consequences

- Step 3 test: 5 ms past the newest sample gives `Err(Extrapolation)` under `Error`, the newest pose with `by_ns == 5_000_000` under `Hold`, a different pose with the same `by_ns` under `ConstantTwist`.
