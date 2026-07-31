# 0011: The online bridge's clock guard, and what a static conflict does about authority

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

A five-lens audit of the Phase 4/5 surface surfaced three defects in
`crates/tf_tree_bridge/src/ingest.rs` that are behaviour changes rather than
repairs, and therefore belong here rather than in a PR. All three are about the
same function, `Ingest::offer`, which is §5's decision pipeline and is shared by
**both** the live `rclcpp` bridge and the offline `tf_tree_ingest` path.

### 1. The clock guard is one guard for the whole stream

`Ingest` holds a single `ClockGuard` and feeds it `sample.stamp_nanos` from every
accepted transform on every edge. `ClockGuard::observe` classifies any stamp more
than `DEFAULT_RESET_THRESHOLD_NANOS` (100 ms) below that shared high-water mark as
`ClockVerdict::Reset`, which under the default `OnClockReset::Halt` becomes
`Action::Halt` — latched by `tft_bridge_offer`, stopping the bridge permanently.

But `/tf` is not one publisher. AMCL and `robot_localization` publish
`map -> odom` dated their `transform_tolerance` (0.1–1.0 s by default) **into the
future**, while `odom -> base_link` from the wheel driver is stamped at publish
time. A SLAM node dates `map -> odom` at its last keyframe, hundreds of
milliseconds **behind**. Either direction is a steady inter-edge offset larger
than 100 ms on a correctly configured robot, and the lagging edge's first message
then reads as a backward jump.

**This project has already litigated this, and decided the other way — for the
offline half.** `crates/tf_tree_ingest/src/ingest.rs` carries a module-doc section
titled *"And the guard is per **edge**, not per stream"* making precisely this
argument, and keeps a `Vec<ClockGuard>` indexed by edge slot. Its conclusion:

> A single guard over the merged stream therefore reports a *reset* — the whole
> ingest halting, at the default, on an ordinary recording — for something that
> is not a clock reset at all but two publishers.

The online half has the identical exposure and the opposite implementation.

**What the evidence does and does not show.** `crates/tf_tree_cli/tests/topology.rs`
tolerates `Action::Drop { NonMonotonic }` on the real corpus and calls the global
guard *"global by design (it tracks the bridge's notion of now)"* — so there is a
written intent, not merely an oversight. Measured against
`testdata/tfstream/indoor_atelier.tfstream`, however, the guard currently drops
**nothing**: 1066 of 1066 samples published, `dropped_non_monotonic == 0`,
`clock_resets == 0`. That corpus's publishers are stamp-aligned, so it neither
demonstrates the defect nor refutes it. The case rests on the mechanism and on
the offline half's recorded argument, not on an in-tree reproduction.

### 2. A static conflict never reaches the authority policy

§5.7 specifies the order as: on a differing static value, "a diagnostic naming
both publishers and both values, **then** apply the authority policy". The
`StaticVerdict::Conflict` arm returns `Action::StaticConflict` directly, and every
arm of the `/tf_static` block returns, so `self.authority.admit(...)` is
unreachable for a static sample. The comment above it claimed the policy "decides
the disposition"; that comment has been corrected to describe what the code does,
and this record carries the question of what it *should* do.

`AuthorityPolicy::Strict` is defined by §5.4's table as *"Refuse to start if a
conflict is detected within a startup window. For CI."* — and **there is no
startup window in this crate**, so "refuse to start" has no implementation to
hang off either. That is why this is a decision and not a bug fix.

### 3. `Action::Drop` has no `first_time`, so its ROS log arm is unreachable

Every other diagnostic action carries `first_time` as a rate limiter. `Action::Drop`
does not, so the `rclcpp` side cannot gate its log, and three fault classes
(`BadName`, `KindChange`, `NonMonotonic`) either spam at message rate or are
silenced entirely. Adding the field changes a public enum in `tf_tree_bridge`.

## Decision

**Deferred — this document is `draft` and the three questions below are open.**
No behaviour has been changed. What *has* landed is the corrected comment on the
static-conflict arm, because a comment that contradicts its code is a defect
under any resolution of this record.

The shape being proposed, for review:

1. **Per-edge clock guards.** Replace `clock: ClockGuard` with
   `clocks: ByEdge<ClockGuard>` plus the `OnClockReset` each new guard is built
   with. `insert`/`lookup_mut`/`ByEdge` are already imported. `OnClockReset::Recreate`
   clears *every* guard, since the arena is rebuilt whole. A real bag loop or sim
   reset still halts, because `/clock` moves every edge at once and the first edge
   observed halts.
2. **Static conflict consults authority.** Add an `Authority::policy()` accessor
   and, under `Strict`, return `Action::Halt { HaltReason::AuthorityConflict }`
   instead of `Action::StaticConflict` — or, if the startup window is what §5.4
   actually requires, implement that window first and gate on it.
3. **`Action::Drop { reason, first_time }`**, with the rate-limit table keyed the
   way `undeclared` already is.

## Rationale

Taking these as a decision rather than as three PRs, because each changes what a
running robot does, and (1) additionally contradicts an explicit in-tree "by
design" statement. Alternatives:

- **Leave all three.** Defensible only for (2), and only until someone runs
  `Strict` in CI and finds it does nothing.
- **Fix (1) by raising the threshold** instead of going per-edge. Rejected: the
  offset is a publisher's `transform_tolerance`, which is configurable up to
  seconds, so no fixed threshold is correct; and a threshold large enough to
  absorb it is too large to catch a real reset.
- **Fix (1) in the C ABI instead**, by feeding the guard per edge from
  `tft_bridge_offer`. Rejected: it would put a §5.5 decision on the wrong side of
  the seam, and `tf_tree_bridge` is the crate whose whole purpose is that both
  callers classify a stream the same way.

## Consequences

- Two comments become false if (1) lands and must move with it:
  `crates/tf_tree_c/src/bridge.rs`'s "dominated by the global clock guard …
  `newest` is the maximum over every accepted sample", and
  `crates/tf_tree_cli/tests/topology.rs`'s "§5.5's clock guard is global by
  design".
- (1) makes the online and offline halves agree, which is the property
  `tf_tree_bridge` exists to provide and currently does not.
- (1) costs one `ClockGuard` and two `String` keys per edge, allocated once on an
  edge's first sample. `tests/steady_state_alloc.rs` is the gate that this stays
  off the steady-state path.
- (3) is a breaking change to a public enum for anyone matching `Action::Drop`
  exhaustively; `tf_tree_c`'s bridge and the CLI are the only in-tree matchers.

## Implementation plan

Not written; the document is `draft`. Each numbered item above lands as one PR
with a test, and (1)'s test is the one that does not exist today: two edges whose
publishers' stamps differ by more than 100 ms in a steady state, asserting the
bridge does **not** halt.

## Open questions

1. **Is the global guard load-bearing for something the per-edge one loses?**
   `topology.rs` says it "tracks the bridge's notion of now". Nothing reads a
   bridge-wide now today, but if `--on-clock-reset=recreate` is meant to key off a
   stream-wide notion, per-edge changes when a recreate fires.
2. **Does `Strict` halt on a static conflict, or does §5.4's startup window need
   implementing first?** These are different features and only one is a one-line
   change.
3. **Is `Action::Drop`'s missing `first_time` worth a breaking enum change**, or
   should the `rclcpp` side throttle instead, the way the `TFT_BRIDGE_REJECTED`
   arm now does?
