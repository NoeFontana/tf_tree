# 0012: The authoritative clock-jump signal, common-mode inference, and the degradation ladder

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

**Supersedes the clock half of
[`0011`](./0011-the-bridge-clock-guard-and-the-static-conflict-disposition.md)** —
its §*Decision* 1 (the promotion rule, `ResetQuorum`, `QUORUM_EDGES`,
`DEFAULT_CORRELATION_WINDOW`, the `Authority::distinct_owners()` floor). `0011`'s
§*Decision* 2 (`AuthorityPolicy::Strict` startup window) and §*Decision* 3
(`Action::Drop` keeps its shape) stand, and its per-edge `ClockGuard` survives
verbatim.

## Context

The online bridge infers "`/clock` was reset" from the `/tf` stamps it receives.
Three inference rules were tried; each failed on a concrete reproduction.

### The three rules, and what killed each

**Rule 1 — one global `ClockGuard` for the whole stream** (`Reset` at 100 ms behind
a merged high-water mark). AMCL and `robot_localization` date `map -> odom` by
`transform_tolerance` (0.1–1.0 s into the future), so the lagging edge regresses
off the leading edge's mark and a correct robot halts at boot. Pinned by
`ingest::tests::two_publishers_a_transform_tolerance_apart_never_halt`. No threshold
separates a relative offset from a reset: `transform_tolerance` has no ceiling.

**Rule 2 — a guard per edge, quorum over distinct *edges*.** One node owning two
dynamic edges restarts and both regress at once: a quorum from a single publisher.

**Rule 3 — quorum over distinct publishers, floored by
`Authority::distinct_owners()`**, `needed = QUORUM_EDGES.min(corroborators.max(1))`.

*(a) The boot race.* `distinct_owners()` counts publishers that have established
ownership, not those that exist. AMCL and `robot_localization` publish late, so
for the first seconds the wheel driver is the only owner, the floor is 1, and its
first regression latches a permanent halt.

*(b) Attribution became a correctness dependency.* `Publisher::UnknownGid` and
`Publisher::Unattributed` are **unit** variants, so on an RMW without endpoint GIDs
every publisher compares equal, the floor is permanently 1, and every single-edge
regression halts. §5.3: *attribution is diagnostic value, never a correctness
dependency.* Two changes each safe alone composed into the defect.

### The root cause

**Inferring a property of the time source from observations of the signal under
suspicion, anchored on proxies that are not physical time** (the transform
ordinal, a publisher count from the authority table), both moved by the traffic
being judged. The fix is not a fourth rule: **ROS 2 publishes clock jumps.**

## Decision

Four layers, L0–L3. L1 is the authoritative path; L2 is inference kept as a
fallback; L3 is the ladder that makes attribution quality change *diagnosis*
quality and never correctness.

### The five principles

Normative for any later change to this area.

| | Principle |
|---|---|
| **P1** | Prefer the authoritative signal to inference. |
| **P2** | A detector's reference clock must be **independent** of the clock under test. |
| **P3** | Windows are **physical time**, never event counts. |
| **P4** | Time is **injected**, never read ambiently inside `tf_tree_bridge`. |
| **P5** | A diagnostic may never become a correctness dependency (§5.3). |

### L0 — inject a steady receipt clock

```rust
/// A reading of a local **steady** (monotonic) clock, in nanoseconds.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SteadyNanos(pub i64);
```

In `clock.rs`, re-exported at the crate root. A distinct type from a publisher's
stamp, never derived from `/clock` or a publisher. `SteadyNanos::UNKNOWN ==
SteadyNanos(0)` is the "no receipt clock" sentinel and `Default`; L2 is skipped
for that sample.

`Sample` gains `pub received: SteadyNanos` (online `RCL_STEADY_TIME` read **once per
message**; offline the recording's log time). `Sample::identity` leaves it
`UNKNOWN`; `#[must_use] Sample::received_at(self, SteadyNanos)` sets it.

**No arithmetic may mix `SteadyNanos` with `stamp_nanos` except the single
offset subtraction in L2.**

### L1 — the authoritative path

```rust
pub enum JumpKind { ClockTypeChanged, Backward, Forward }

impl Ingest {
    pub fn note_time_jump(&mut self, delta_nanos: i64, kind: JumpKind) -> Action;
}
```

No threshold, no inference, no quorum. It applies `OnClockReset` directly —
`Action::Halt { HaltReason::ClockReset { .. } }` or `Action::RecreateArena { .. }`.
`delta_nanos` follows `rcl_time_jump_t::delta`: **new time minus the last time
before the jump, so a rewind is NEGATIVE** (a deliberate sign flip against
`0011`'s `by_nanos`). It charges no ledger bucket, so `BridgeStats::balanced()` is
untouched; it increments `clock_resets`, which is not a term in `balanced()`.

### L2 — inference as common-mode rejection

The fallback, for non-ROS C callers, system-clock steps, and defence in depth.

Per **publisher**, keyed by `ingest::owner_key`, track `offset = sample.stamp_nanos
- sample.received.0`. **A publisher's `transform_tolerance` is this offset**;
measured and subtracted it stops looking like a jump, which dissolves rule 1.

- Smoothed baseline per publisher: integer EWMA, **alpha = 1/8**
  (`BASELINE_DIVISOR = 8`). Steady-state lag under per-sample drift `d` is `7d`.
- A **step** is `|offset - baseline| > reset_threshold_nanos`. On a step the
  baseline **snaps** to the new offset, so a broken publisher costs one step per
  bout.
- **Common mode:** at least **two distinct publishers** stepped within
  `correlation_window_nanos` of each other in receipt time (`SteadyNanos`), **and**
  their step deltas agree: `|d_a - d_b| <= max(common_mode_tolerance_floor_nanos,
  ratio * max(|d_a|, |d_b|))`.

A real `/clock` step moves everyone by the same amount; independent restarts do
not. This also detects **forward** jumps.

The table is a `BTreeMap<String, Offset>` probed with `get_mut(&str)`, never
`entry()` with an owned key, so refusal allocates nothing after a publisher's first
sample. Capped at `MAX_TRACKED_PUBLISHERS = 64`; refusing a row can only make a
halt harder to reach.

### L3 — the degradation ladder

| Evidence | Disposition |
|---|---|
| authoritative jump signal (L1) | `Halt` / `RecreateArena` — exact |
| common-mode step, ≥ 2 agreeing publishers (L2) | `Halt` / `RecreateArena` |
| single-source regression, at **any** magnitude | `Drop`, count, diagnose. **NEVER HALT.** |

The bridge never halts on one witness, so there is no floor to get wrong. Without
endpoint introspection every publisher collapses to one sentinel, common mode never
reaches two, and every regression degrades to `Drop`. Phase 1 rejects a
non-monotonic stamp regardless, so the arena is protected.

In `Ingest::offer`, `ClockVerdict::Jitter` and `ClockVerdict::Reset` collapse into
**one** arm: drop, charge `dropped_non_monotonic`, diagnose. Promotion happens
only through `apply_clock_reset`, reached from the common-mode arm or
`note_time_jump`.

### Configuration

```rust
pub struct ClockPolicy {
    pub reset_threshold_nanos: i64,              // DEFAULT_RESET_THRESHOLD_NANOS (100 ms)
    pub correlation_window_nanos: i64,           // 1_000_000_000
    pub common_mode_tolerance_ratio: f64,        // 0.25
    pub common_mode_tolerance_floor_nanos: i64,  // 50_000_000
    pub on_reset: OnClockReset,
}
```

`reset_threshold_nanos` cites `DEFAULT_RESET_THRESHOLD_NANOS`, which the offline
half also consumes. The ratio covers a large step whose last pre-jump messages were
up to a period apart; the floor covers a small step where a ratio is smaller than
the jitter. `Ingest::with(..)` delegates to `Ingest::with_policies(config,
authority, ClockPolicy, tf_prefix)`.

### The `HaltReason` payload

```rust
HaltReason::ClockReset { delta_nanos: i64, evidence: ClockEvidence }
Action::RecreateArena { delta_nanos: i64 }

pub enum ClockEvidence { Reported { kind: JumpKind }, CommonMode { publishers: u32 } }
```

`ClockEvidence` is `Copy` and replaces `correlated_edges: u32`.

### Deleted and kept

Deleted: `ResetQuorum`, `QuorumVerdict`, `QUORUM_EDGES`,
`DEFAULT_CORRELATION_WINDOW`, `MAX_TRACKED_EDGES`, `Regression`,
`Authority::distinct_owners()`, and `owner_key`'s quorum role (it survives as L2's
key). `DEFAULT_RESET_THRESHOLD_NANOS` stays. The per-edge `ClockGuard` stays
verbatim: it makes the per-edge **drop** decision Phase 1 invariant 6 requires and
no longer promotes to a halt.

## Rationale

### The five principles and their prior art

**P1 — `rcl` jump callbacks.** `rcl/time.h` declares `rcl_time_jump_t {
clock_change; delta }` and `rcl_clock_add_jump_callback(..)`;
`rclcpp::Clock::create_jump_callback(pre, post, threshold)` wraps them (verified in
`docker/tf2`, ROS 2 *lyrical*).

**P2 — the Linux clocksource watchdog** compares a candidate against an
*independent* clock over a fixed interval. Rules 1–3 validated `/clock` against
quantities `/clock` moves; `RCL_STEADY_TIME` is monotonic and unaffected by
`use_sim_time`.

**P3 — NTP step thresholds** are durations (128 ms step, 1000 s panic). A
transform-count window makes its meaning a function of the traffic under suspicion.

**P4 — `rcl` has no ambient `now()`**; the caller passes the clock. Also, the jump
callback **does not run on the ingest thread** (`NodeOptions::use_clock_thread`
defaults to `true`) and every `tft_bridge_*` entry point is thread-affine (debug
aborts; `--release` returns `TFT_ERR_WRONG_THREAD`); injection makes the hand-off
reviewable.

**P5 — GNSS common-mode receiver-clock jump detection, and §5.3.** A receiver
tracking fewer satellites loses sensitivity but never declares a jump that did not
happen. Rule 3 degraded the other way.

### Why common-mode *agreement* beats coincidence

The quorum asked "did two things regress near each other?", which a supervisor
respawning two nodes produces. Agreement asks whether they moved by the **same
amount**: a `/clock` step changes every offset by exactly the step, whereas
independent restarts share no mechanism equalising their regressions. It also
detects **forward** jumps, and its tolerance `max(floor, ratio · max(|d_a|, |d_b|))`
is scale-free (a 5 s loop is allowed 1.25 s of disagreement, a 200 ms step 50 ms).
The offset `stamp - received` is constant for a healthy publisher, so a
`transform_tolerance` is subtracted, not tolerated.

### Alternatives rejected

- **A fourth inference rule.** The three failures were one category error.
- **Subscribe to `/clock` directly.** `rcl`'s jump callbacks are the same evidence
  already computed.
- **Halt on a single witness when the deployment "provably" has one publisher.**
  The bridge knows only how many it has *seen*.
- **A second `BridgeStats` bucket for clock refusals.** It grows the versioned
  `tft_bridge_stats` and `balanced()`; `dropped_non_monotonic` widens instead.
- **`received` defaulting to `stamp_nanos` for old C callers.** It forces `offset
  ≡ 0`, re-enabling rule 1's defect. `UNKNOWN` is the honest degradation.

### Implementation notes

The per-edge guard's threshold is unobservable through `Ingest` (`Jitter` and
`Reset` share a disposition; the mutant passes), so `reset_threshold_nanos` is
observable only through the step detector. `note_time_jump` resets the offset table
under `Recreate` only: under `Halt`, forgetting the per-edge marks would let the
next post-rewind sample read as forward motion and come back `Action::Publish`
(argued on `Ingest::apply_clock_reset`).

## Consequences

### Committed to
- **The bridge never halts on one witness.** Any promotion of a per-edge fact to a
  global judgment carries an authoritative signal or two independent, *agreeing*
  sources.
- **`tf_tree_bridge` reads no clock**; adding `std::time` needs its own record.
  Every clock window is nanoseconds of a steady clock.
- **`delta_nanos` is signed, `rcl`-convention**; every consumer (the C seam's
  `detail` string, the `rclcpp` `report()` arms) must agree.
- **`dropped_non_monotonic` widens** to "transforms the clock rules refused",
  because a forward common-mode jump refuses a monotone sample. The name is kept
  because renaming it crosses the C ABI.
- **The single-source refusal path allocates nothing**, pinned by a third scenario
  in `tests/steady_state_alloc.rs` (budget 2).
- **`clock_resets` counts promotions, not regressions**, and is not in `balanced()`.
- **Only `just ros-test` covers `ros/tf_tree_ros/`** (outside the cargo workspace);
  `crates/tf_tree_c/tests/bridge.rs` is behind the `bridge` feature.
- **No code path may make the halt decision a function of publisher identity.**

### Known limitations
- **The EWMA needs warm-up**: a clock step in a publisher's first ~8 samples is
  likely absorbed. L1 has none.
- **A fast-drifting publisher can mask a step** smaller than
  `reset_threshold_nanos - 7d`; raising α trades this for tolerating less jitter.
- **Non-ROS C callers get only L2 and L3**; without `tft_bridge_note_time_jump` and
  with `SteadyNanos::UNKNOWN` there is no detector and `clock_resets` stays 0. A C
  caller predating the appended `tft_bridge_sample` field gets `UNKNOWN` and loses L2.
- **Two publishers restarting at the same instant by the same amount are reported
  as a clock reset**; `ClockEvidence::CommonMode { publishers }` names it. A forward
  common-mode jump refuses a monotone sample into a counter whose name does not
  describe it.
- **The offline half (`tf_tree_ingest`) still halts on the first
  `ClockVerdict::Reset`**: a bag is a finished artifact; a false halt online takes
  down a running robot.

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
   (bounded copy, not a whole-struct `read_unaligned`); add
   `tft_bridge_note_time_jump` and `TFT_BRIDGE_JUMP_{CLOCK_TYPE_CHANGED,BACKWARD,
   FORWARD}`, classified in `xtask/src/headers.rs`'s `UNSTABLE` list **in the same
   commit** (an unclassified `pub const` leaks into the frozen `tf_tree.h`); bump
   `TFT_ABI_VERSION_MINOR` 1 → 2; re-aim the three `tf_tree_c` tests that reached a
   halt with one publisher. Verified by `just c-header-check`, `just c-abi-check`.
4. **`rclcpp`.** One `rclcpp::Clock steady_{RCL_STEADY_TIME}` on `BridgeHandle`,
   read **once** per `TFMessage` in `ingest()` and passed into `offer_one`. Register
   a jump **post**-callback on `node_->get_clock()` with `{on_clock_change = true,
   min_forward = {1}, min_backward = {-1}}` (zero **disables** a direction) and keep
   the `JumpHandler::SharedPtr`. The callback **only records** `{delta,
   clock_change}` into a slot the ingest thread drains in `ingest()` and once per
   `run()` iteration; it must not call any `tft_bridge_*` entry point or throw.
   Verified by `just ros-test`.
5. **The CLI and offline prose.** The five bare `Sample` literals in
   `crates/tf_tree_cli/` move onto `Sample::identity(..)` with
   `SteadyNanos::UNKNOWN` (the `.tfstream` grammar has no log-time column); fix the
   stale intra-doc links in `tf_tree_ingest` and `tf_tree_bridge`.
6. **Full gate:** `just lint`, `just test`, `just c-abi-check`,
   `just c-header-check`, `just tf2-check`, `just ros-test`.

## Open questions

None.
