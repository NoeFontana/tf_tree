# 0039: extrapolation you cannot fail to notice

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #278, #283

## Context

`ExtrapPolicy::{Error, Hold, ConstantTwist}` worked in `tf_tree_core::sample` but no consumer could select one: every fold site in `plan.rs` passed `Error` and `tf_tree` did not re-export the type (only `crates/tf_tree_bench/examples/step_cost.rs:264` passed `Hold`).

## Decision

**Extrapolation is selected per query, and its result cannot be read without also reading how far it was extrapolated.** A `Hold` right for a 10 Hz edge is wrong for a 1 kHz edge on the same route, so the caller who bears the consequence chooses.

### 1. `Extrapolated`

`Extrapolated { pose: Iso3, by_ns: i64, edge: EdgeId }` is `Copy`. `by_ns` is nanoseconds past the newest stamp every dynamic edge has data for; `0` means every edge bracketed the query. `edge` is the edge that ran out first (meaningless at `by_ns == 0`). There is no accessor yielding the pose alone; a future one deletes the property this record is for.

### 2. Methods

`Plan::at_extrapolating<D: Domain>(&self, g, t: Stamp<D>, policy)` and `Plan::at_extrapolating_tagged(&self, g, nanos, domain, policy)` return `Result<Extrapolated, LookupError>`. `Plan::at` is untouched and still passes `Error`. `ExtrapPolicy` and `Extrapolated` are re-exported from `tf_tree`.

### 3. `by_ns` is measured against `latest_common`, before the fold

`by_ns = max(0, t - min over dynamic edges of newest_stamp)`, with the argmin edge, via `newest_common` factored out of `fold_latest_common`. The walk runs **before** the fold: `newest_stamp` is non-decreasing, so a concurrent `push` can only make `by_ns` over-report a bracketed query, never report `0` for a pose the fold invented. It is not `note`d (a second `note` would double `lookups_ok`, which `TFT010`/`TFT011` divide by). Test: `by_ns_zero_is_never_claimed_for_a_pose_the_fold_invented`.

### 4. The hot path does not pay

The distance comes from `d` `newest_stamp` loads on the extrapolating path only, not threaded through `fold_at`, `sample_from` or the seqlock read; `Plan::at`'s code is unchanged (`just bench-check`).

## Consequences

- `Error` remains the way to make extrapolation a failure; extrapolation under an explicit policy is a requested outcome, not an error.
- Step 3 test: a query 5 ms past the newest sample gives `Err(Extrapolation)` under `Error`, the newest pose with `by_ns == 5_000_000` under `Hold`, and a different pose with the same `by_ns` under `ConstantTwist`.
