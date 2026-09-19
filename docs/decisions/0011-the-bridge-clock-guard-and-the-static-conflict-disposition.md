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

> **Scoping note.** Only §*Decision* 1 (the clock guard) is retired: it is
> **superseded by `0012`**, which keeps the per-edge `ClockGuard` and deletes the
> promotion rule (`ResetQuorum`, `QuorumVerdict`, `QUORUM_EDGES`,
> `DEFAULT_CORRELATION_WINDOW`, `MAX_TRACKED_EDGES`, `Authority::distinct_owners()`
> as a floor, `HaltReason::ClockReset { correlated_edges }`). Do not implement or
> cite it. §*Decision* 2 (the startup window) and §*Decision* 3 (`Action::Drop`
> keeps its shape) **stand**. The window keeps its transform ordinal by choice,
> not because the crate has no clock: `0012`'s `SteadyNanos` exists.

## Context

Three defects in `Ingest::offer` (`crates/tf_tree_bridge/src/ingest.rs`) are
behaviour changes, not repairs. They share a cause: the bridge derives a global or
temporal *judgment* (clock reset, misconfigured deployment) from a single
per-edge *fact*.

1. **One `ClockGuard` for the whole stream.** Publishers dating `map -> odom`
   tens to hundreds of milliseconds ahead of or behind `odom -> base_link` make
   the lagging edge read as a backward jump. The offline half
   (`crates/tf_tree_ingest/src/ingest.rs`) already keeps one guard per edge.
2. **`AuthorityPolicy::Strict` is unimplemented.** §5.4 defines it as *"refuse to
   start if a conflict is detected within a startup window"*; the crate has no
   window, and `Authority::admit`'s `Strict` arm halts on the second colliding
   message. Routing statics into it would be a per-message halt wearing the
   window's name.
3. **`Action::Drop` has no `first_time`**, so the `if (out.first_time != 0)` gate
   in `ros/tf_tree_ros/src/bridge_handle.cpp`'s `TFT_BRIDGE_DROPPED` tail is dead
   and `BadName`, `KindChange` and `NonMonotonic` are silent.

## Decision

Keep the primitive per edge and exact; make each promotion an explicit rule with
its own window.

### The crate's one notion of time: the transform ordinal

**`BridgeStats::transforms` is the bridge's clock.** It is incremented
unconditionally at the top of `offer`, before any early return, and a publisher's
stamp cannot move it. It is an ordinal, not a duration. Any future window in
`tf_tree_bridge` is a count of transforms, or needs a decision record.

### 1. One `ClockGuard` per edge — superseded by `0012`

### 2. A real startup window; `Strict` accumulates inside it and halts at its close

`Ingest` has a startup window, open from construction, closing at the first of:

- `stats.transforms >= STARTUP_WINDOW_TRANSFORMS` (private, **4096**), a backstop;
- `Ingest::close_startup_window(&mut self) -> Option<Action>`, the explicit close,
  which is how a caller that owns a real clock supplies a real duration.

**Inside the window** no conflict halts per message; both kinds are recorded and
the sample is disposed of as `FirstWriterWins` would:

- `Authority::admit`'s `Strict` arm still returns `Verdict::Fatal` but also does
  the `reported`/`dropped` bookkeeping, so the conflict is visible to
  `Authority::conflicts()` and `tf_tree doctor`. It does not mutate `owners`.
  `Ingest` maps `Fatal` inside the window to `Action::AuthorityConflict { .., first_time }`
  and `dropped_authority += 1`.
- The `/tf_static` conflict arm returns as before (`Action::StaticConflict`,
  `static_conflicts += 1`, `dropped_authority += 1`).

There is **no separate conflict ledger**: at close the window reads
`Authority::conflicts()` and `StaticStore::conflicts_by_edge()`.

**At close**, under `Strict` only, if either source is non-empty:
`HaltReason::StartupConflicts { authority: u32, statics: u32 }`, and the C seam's
`detail` enumerates **every** recorded edge with both publishers (§5.4).

**Outside the window**, `Strict` is `FirstWriterWins` plus counters, by design: a
bridge healthy for an hour must not be killed by a late-joining publisher.

The window-close halt is checked **at the top of `offer`, before
`stats.transforms += 1`**, and increments no counter: it is not an event about the
arriving transform, and charging a bucket would unbalance `balanced()`. Likewise
`close_startup_window()`.

Why a window rather than a per-message halt: `Strict` exists for CI, which wants
every misconfiguration in one run; and `/tf_static` is `transient_local`, so when a
conflict is *observed* is a DDS discovery artefact, not a fault time.

Rejected: making `Authority::admit` window-aware (it must stay a pure per-edge
fact table); a duration field in `tft_bridge_options` (changes `sizeof`, and every
validation site in `crates/tf_tree_c/src/bridge.rs` is an exact equality); a new
`BridgeStats` bucket (`dropped_authority` is already the bucket).

### 3. `Action::Drop` keeps its shape — resolved as **no**

No enum change. The `rclcpp` side throttles, as the `TFT_BRIDGE_REJECTED` arm
does, at **three distinct call sites**, one per reason (rcutils' throttle state is
a function-local `static` per macro expansion, so one call would let a kilohertz
`NonMonotonic` edge starve a once-ever `KindChange` line). The tail also needs a
`reason_name()`; `BAD_POSE` reaches the same tail *with* a `detail` and must keep
it. Why not `first_time`: `KindChange` is once per edge already; `NonMonotonic`'s
severity is its rate; `BadName`'s key is publisher-controlled and unbounded.
`BridgeStats` is the reporting surface; logs are a convenience.

## Consequences

- **A window-close halt charges no bucket**; step 5's tests pin the ledger
  invariant. `dropped_non_monotonic` must keep being incremented on the
  single-edge regression path or `balanced()` is false forever.
- **`Strict` outside the window is `FirstWriterWins` plus counters**: documented,
  not a degradation to fix later.
- **A transform ordinal is a poor proxy for a duration**, and (2) wants one. 4096
  transforms is ~2 s of a busy `/tf` and minutes of a sparse one. The primary
  mechanism is `close_startup_window()`, which the `rclcpp` node drives from a
  one-shot **steady** timer (not `node_->get_clock()`, which is `/clock` under
  `use_sim_time`); a caller that never calls it inherits the backstop.
- **A bridge that receives no traffic never closes its window**: no tick exists
  in the crate. The gap is narrow (a `/tf_static` conflict, then permanent
  silence).
- **`crates/tf_tree_c/tests/bridge.rs`** is behind the default-off `bridge`
  feature (covered by `just test`'s `-p tf_tree_c --features bridge` line) and
  `ros/tf_tree_ros/` is outside the workspace: **only `just ros-test`, in
  `docker/tf2`, covers it.**

## Implementation plan

Steps 2-4 are (1) and are landed and partly superseded by `0012`; 5-7 are (2);
8 is (3). Each is one PR.

1. **Landed.** This record and its `README.md` row.
2. **Landed** (`336bc27`; clock half then partly deleted by `0012`). (1) in `tf_tree_bridge`.
3. **Landed.** (1) across the C seam.
4. **Landed.** (1) in ROS, the CLI and the docs.
5. **Landed** (`336bc27`; `StaticStore::conflicts_by_edge()` in #314). (2) in
   `tf_tree_bridge`: the window, `STARTUP_WINDOW_TRANSFORMS = 4096`,
   `close_startup_window()`, the boundary check before `transforms += 1`,
   `Authority`'s `Strict` bookkeeping, `HaltReason::StartupConflicts`.
6. **Landed.** (2) across the C seam: `TFT_BRIDGE_REASON_STARTUP_CONFLICTS = 9`
   (**must** be appended to `UNSTABLE` in `xtask/src/headers.rs`, or it is
   silently emitted into the frozen `tf_tree.h`) and
   `tft_bridge_close_startup_window(b, out)`, latching `stopped` as the `Halt`
   arm does. The stable header must carry no *declaration* of either symbol:

   ```sh
   grep -E '^#define TFT_BRIDGE_REASON_STARTUP|tft_bridge_close_startup_window *\(' \
       crates/tf_tree_c/include/tf_tree.h
   ```

7. **Outstanding. (2) in ROS.** A `startup_window_sec` parameter (default 5.0) and
   a one-shot `RCL_STEADY_TIME` timer calling the new entry point, routing its
   outcome through `report()`. Not a `tft_bridge_options` field. A gtest that two
   conflicting publishers under `Strict` produce one `RCLCPP_FATAL` naming both.
   Verified by `just ros-test`. Two cautions:
   - *The timer must be created on `group_`.* `BridgeHandle`'s callback group is
     spun alone on the bridge's thread; a timer in the node's default group fires
     on another thread, so §3.2's affinity assertion `abort()`s a debug build and a
     release build gets `TFT_ERR_WRONG_THREAD` and never closes the window.
   - *5.0 s is a coverage decision, not a latency one.* `/tf_static` latched
     samples can arrive "seconds after either process started" (§5.4); a window
     that closes first lets the two-different-URDFs fault pass. Whether 5.0 is
     defensible, and whether the close should be deferred until a `/tf_static`
     has been seen, is for this step to answer.
8. **Outstanding. (3) in ROS.** Split the `TFT_BRIDGE_DROPPED` tail into three
   `RCLCPP_WARN_THROTTLE` call sites, one per reason, and add `reason_name()`.
   Preserve `BAD_POSE`'s `detail`. Verified by `just ros-test`.
9. **Outstanding. Full gate:** `just lint`, `just test`, `just c-abi-check`,
   `just c-header-check`, `just tf2-check`, `just ros-test`.

## Open questions

None.
