# 0011: The online bridge's clock guard, and what a static conflict does about authority

**Status:** ready — **its clock half is superseded by
[`0012`](./0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md)**
**Owner:** @NoeFontana
**Implementation:** **step 6 has landed** —
`tft_bridge_close_startup_window`, `TFT_BRIDGE_REASON_STARTUP_CONFLICTS = 9`,
`TFT_ABI_VERSION_MINOR` 7 → 8. Steps 1–4 landed in `336bc27`, and their clock
half was then **partly deleted** by
[`0012`](./0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md)
(`cd5295d`): read the scoping note below before treating any of steps 1–3 as a
description of the code. Step 5 landed in `336bc27` **except**
`StaticStore::conflicts_by_edge()`, which it names and which did not arrive until
#314 — so for the whole of that interval §5.4's *"enumerates **every** recorded
edge with both of its publishers"* clause was unmet on both halves, the static
one for want of the accessor and the authority one for want of step 6's caller.
**Steps 7–9 are outstanding**, and steps 7 and 8 plus two rows of step 9 are
`ros/` or container-only: nothing on the development host can run `just ros-test`
or `just tf2-check`.

> **Scoping note.** Only §*Decision* 1 (the clock guard) is retired, superseded by
> `0012`. §*Decision* 2 and 3 **stand**.

## Decision

### The crate's one notion of time: the transform ordinal

**`BridgeStats::transforms` is the bridge's clock.** Any window in
`tf_tree_bridge` is a count of transforms, or needs a decision record.

### 1. One `ClockGuard` per edge — superseded by `0012`

### 2. A real startup window; `Strict` accumulates inside it and halts at its close

`Ingest`'s startup window opens at construction and closes at the first of
`stats.transforms >= STARTUP_WINDOW_TRANSFORMS` (private, **4096**, a backstop) or
`Ingest::close_startup_window(&mut self) -> Option<Action>`.

**Inside the window** conflicts are recorded, not halted on, and the sample is
disposed of as `FirstWriterWins` would:

- `Authority::admit`'s `Strict` arm still returns `Verdict::Fatal` but also does
  the `reported`/`dropped` bookkeeping, without mutating `owners`. `Ingest` maps
  `Fatal` inside the window to `Action::AuthorityConflict { .., first_time }`.
- The `/tf_static` conflict arm returns `Action::StaticConflict` as before.

At close the window reads `Authority::conflicts()` and
`StaticStore::conflicts_by_edge()`; there is no separate ledger.

**At close**, under `Strict` only, if either is non-empty:
`HaltReason::StartupConflicts { authority: u32, statics: u32 }`, and the C seam's
`detail` enumerates **every** recorded edge with both publishers (§5.4). **Outside
the window**, `Strict` is `FirstWriterWins` plus counters, by design.

The close halt is checked **at the top of `offer`, before `stats.transforms += 1`**,
and increments no counter, or `balanced()` breaks.

### 3. `Action::Drop` keeps its shape — resolved as **no**

No enum change. The `rclcpp` side throttles at **three distinct call sites**, one
per reason; `BAD_POSE` reaches the same tail *with* a `detail` and must keep it.

## Consequences

- **A window-close halt charges no bucket**; step 5's tests pin the ledger.
- **The transform ordinal is a poor proxy for a duration.** The primary mechanism
  is `close_startup_window()` from a one-shot **steady** timer (not
  `node_->get_clock()`, which is `/clock` under `use_sim_time`).

## Implementation plan

1-5. **Landed.** Steps 2-4 are (1); 5-7 are (2); 8 is (3).
6. **Landed.** (2) across the C seam: `TFT_BRIDGE_REASON_STARTUP_CONFLICTS = 9`
   (**must** be appended to `UNSTABLE` in `xtask/src/headers.rs`) and
   `tft_bridge_close_startup_window(b, out)`, latching `stopped` as the `Halt`
   arm does. The stable header must carry no *declaration* of either symbol:

   ```sh
   grep -E '^#define TFT_BRIDGE_REASON_STARTUP|tft_bridge_close_startup_window *\(' \
       crates/tf_tree_c/include/tf_tree.h
   ```

7. **Outstanding. (2) in ROS.** A `startup_window_sec` parameter (default 5.0) and
   a one-shot `RCL_STEADY_TIME` timer calling the new entry point, routing its
   outcome through `report()`; not a `tft_bridge_options` field. A gtest that two
   conflicting publishers under `Strict` produce one `RCLCPP_FATAL` naming both.
   Cautions:
   - *The timer must be created on `group_`*: in the default group it fires on
     another thread and §3.2's affinity assertion fires.
   - *5.0 s is a coverage decision* (`/tf_static` latched samples can arrive
     seconds late, §5.4); whether the close should wait for a `/tf_static` is for
     this step to answer.
8. **Outstanding. (3) in ROS.** Split the `TFT_BRIDGE_DROPPED` tail into three
   `RCLCPP_WARN_THROTTLE` call sites and add `reason_name()`. Verified by
   `just ros-test`.
9. **Outstanding. Full gate:** `just lint`, `just test`, `just c-abi-check`,
   `just c-header-check`, `just tf2-check`, `just ros-test`.

## Open questions

None.
