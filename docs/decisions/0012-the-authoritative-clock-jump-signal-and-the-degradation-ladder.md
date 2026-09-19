# 0012: The authoritative clock-jump signal, common-mode inference, and the degradation ladder

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

**Supersedes the clock half of
[`0011`](./0011-the-bridge-clock-guard-and-the-static-conflict-disposition.md)** —
its §*Decision* 1 (`ResetQuorum`, `QUORUM_EDGES`, `DEFAULT_CORRELATION_WINDOW`, the
`Authority::distinct_owners()` floor). `0011`'s §*Decision* 2 and 3 stand, and its
per-edge `ClockGuard` survives verbatim.

## Context

The bridge infers "`/clock` was reset" from `/tf` stamps. A global guard halts a
correct robot when `transform_tolerance` dates one edge ahead of another
(`ingest::tests::two_publishers_a_transform_tolerance_apart_never_halt`); a
publisher quorum floored by `distinct_owners()` halts on the first regression at
boot and on any RMW without endpoint GIDs. The root cause is inferring a property of
the time source from the signal under suspicion. **ROS 2 publishes clock jumps.**

## Decision

Four layers: L1 authoritative, L2 inference fallback, L3 the ladder.

### The five principles

| | Principle |
|---|---|
| **P1** | Prefer the authoritative signal to inference. |
| **P2** | A detector's reference clock must be **independent** of the clock under test. |
| **P3** | Windows are **physical time**, never event counts. |
| **P4** | Time is **injected**, never read ambiently inside `tf_tree_bridge`. |
| **P5** | A diagnostic may never become a correctness dependency (§5.3). |

### L0 — inject a steady receipt clock

```rust
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SteadyNanos(pub i64);
```

In `clock.rs`, re-exported at the crate root; never derived from `/clock` or a
publisher. `SteadyNanos::UNKNOWN == SteadyNanos(0)` is the sentinel and `Default`; L2
is skipped for that sample. `Sample` gains `pub received: SteadyNanos` (online
`RCL_STEADY_TIME` read **once per message**; offline the recording's log time);
`Sample::identity` leaves it `UNKNOWN`; `#[must_use] Sample::received_at(self,
SteadyNanos)` sets it. **No arithmetic may mix `SteadyNanos` with `stamp_nanos`
except the single offset subtraction in L2.**

### L1 — the authoritative path

```rust
pub enum JumpKind { ClockTypeChanged, Backward, Forward }

impl Ingest {
    pub fn note_time_jump(&mut self, delta_nanos: i64, kind: JumpKind) -> Action;
}
```

No threshold, no quorum: it applies `OnClockReset` directly (`Action::Halt {
HaltReason::ClockReset { .. } }` or `Action::RecreateArena { .. }`). `delta_nanos`
follows `rcl_time_jump_t::delta`: new time minus the last time before the jump, so a
rewind is **negative**. It charges no ledger bucket and increments `clock_resets`,
which is not a term in `BridgeStats::balanced()`.

### L2 — inference as common-mode rejection

Fallback for non-ROS C callers and system-clock steps. Per **publisher**,
keyed by `ingest::owner_key`, track `offset = sample.stamp_nanos - sample.received.0`;
a publisher's `transform_tolerance` is this offset, so once subtracted it is not a
jump.

- Baseline: integer EWMA, alpha = 1/8 (`BASELINE_DIVISOR = 8`).
- A **step** is `|offset - baseline| > reset_threshold_nanos`; the baseline then
  **snaps** to the new offset.
- **Common mode:** at least **two distinct publishers** stepped within
  `correlation_window_nanos` in receipt time, **and** their deltas agree:
  `|d_a - d_b| <= max(common_mode_tolerance_floor_nanos, ratio * max(|d_a|, |d_b|))`.
  This also detects **forward** jumps.

The table is a `BTreeMap<String, Offset>` probed with `get_mut(&str)`, never
`entry()` with an owned key, so refusal allocates nothing; capped at
`MAX_TRACKED_PUBLISHERS = 64`. `note_time_jump` resets it under `Recreate` only
(argued on `Ingest::apply_clock_reset`). The jump callback does not run on the
ingest thread, which is why the clock is injected.

### L3 — the degradation ladder

| Evidence | Disposition |
|---|---|
| authoritative jump signal (L1) | `Halt` / `RecreateArena` |
| common-mode step, ≥ 2 agreeing publishers (L2) | `Halt` / `RecreateArena` |
| single-source regression, at **any** magnitude | `Drop`, count, diagnose. **NEVER HALT.** |

In
`Ingest::offer`, `ClockVerdict::Jitter` and `ClockVerdict::Reset` share **one** arm:
drop, charge `dropped_non_monotonic`, diagnose. Promotion happens only through
`apply_clock_reset`, from the common-mode arm or `note_time_jump`.

### Configuration

`ClockPolicy { reset_threshold_nanos` (`DEFAULT_RESET_THRESHOLD_NANOS`, 100 ms),
`correlation_window_nanos` (1 s), `common_mode_tolerance_ratio` (0.25),
`common_mode_tolerance_floor_nanos` (50 ms), `on_reset: OnClockReset }`.
`Ingest::with(..) delegates to `Ingest::with_policies(config, authority,
ClockPolicy, tf_prefix)`.

### The `HaltReason` payload

`HaltReason::ClockReset { delta_nanos: i64, evidence: ClockEvidence }`,
`Action::RecreateArena { delta_nanos: i64 }`, and the `Copy` enum `ClockEvidence {
Reported { kind: JumpKind }, CommonMode { publishers: u32 } }`, replacing
`correlated_edges: u32`.

### Deleted and kept

Deleted: `ResetQuorum`, `QuorumVerdict`, `QUORUM_EDGES`, `DEFAULT_CORRELATION_WINDOW`,
`MAX_TRACKED_EDGES`, `Regression`, `Authority::distinct_owners()`. The per-edge
`ClockGuard` stays (drop only, no promotion).

## Consequences

- **The bridge never halts on one witness**, and no code path may make the halt
  decision a function of publisher identity.
- **`tf_tree_bridge` reads no clock**; adding `std::time` needs its own record.
- **`delta_nanos` is signed, `rcl`-convention**, in every consumer.
- **`dropped_non_monotonic` widens** to "transforms the clock rules refused"; the
  name is kept because renaming it crosses the C ABI.
- **The single-source refusal path allocates nothing**
  (`tests/steady_state_alloc.rs`, budget 2).
- **`clock_resets` counts promotions, not regressions.**
- **Only `just ros-test` covers `ros/tf_tree_ros/`**;
  `crates/tf_tree_c/tests/bridge.rs` is behind the `bridge` feature.

Known limitations: EWMA warm-up of about 8 samples (L1 has none); non-ROS C callers
get only L2 and L3; the offline half (`tf_tree_ingest`) still halts on the first
`ClockVerdict::Reset`.

## Implementation plan

Each step lands as one PR.
1. **This record, `0011`'s status line, `docs/decisions/README.md`,
   `docs/PHASE4.md` §5.3/§5.5.**
2. **L0–L3 in `tf_tree_bridge`**: everything above, the deletions, and a third
   `steady_state_alloc.rs` scenario (2000 offers of a stuck publisher, all `Drop`,
   budget 2). Every new test doc names the mutant it kills, and the mutant is
   **applied**.
3. **The C seam.** Append `int64_t received_steady_nanos` to `tft_bridge_sample` in
   **both** twins (`tf_tree_unstable.h`, `bridge.rs`; 88 → 96 bytes) with a
   size/offset parity assertion; accept a legacy `struct_size` as a **prefix**
   (bounded copy); add `tft_bridge_note_time_jump` and
   `TFT_BRIDGE_JUMP_{CLOCK_TYPE_CHANGED,BACKWARD,FORWARD}`, classified in
   `xtask/src/headers.rs`'s `UNSTABLE` list **in the same commit**; bump
   `TFT_ABI_VERSION_MINOR` 1 → 2; re-aim the three `tf_tree_c` tests that reached a
   halt with one publisher. Verified by `just c-header-check`, `just c-abi-check`.
4. **`rclcpp`.** One `rclcpp::Clock steady_{RCL_STEADY_TIME}` on `BridgeHandle`,
   read **once** per `TFMessage` in `ingest()` and passed into `offer_one`. Register
   a jump **post**-callback on `node_->get_clock()` with `{on_clock_change = true,
   min_forward = {1}, min_backward = {-1}}` (zero **disables** a direction). The
   callback **only records** `{delta, clock_change}` into a slot the ingest thread
   drains in `ingest()` and once per `run()` iteration; it must not call any
   `tft_bridge_*` entry point or throw. Verified by `just ros-test`.
5. **The CLI and offline prose.** The five bare `Sample` literals in
   `crates/tf_tree_cli/` move onto `Sample::identity(..)` with
   `SteadyNanos::UNKNOWN`; fix the stale intra-doc links in `tf_tree_ingest` and
   `tf_tree_bridge`.
6. **Full gate:** `just lint`, `just test`, `just c-abi-check`,
   `just c-header-check`, `just tf2-check`, `just ros-test`.

## Open questions

None.
