# tf_tree — Phase 7 Specification: the `tf2`-shaped compatibility surface

> **Companions:** [`PROJECT.md`](./PROJECT.md) (D21 gates this phase),
> [`API.md`](./API.md), [`PHASE4.md`](./PHASE4.md) §5 (the ingest bridge this
> sits on), [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) (the wait).

**Deliverable:** a `tf2_ros::Buffer`-shaped C++ and Python surface over
`tf_tree`, so an existing ROS 2 node adopts the engine with a header swap and can
then graduate to the native API one hot loop at a time.

## 0.0 Status — GATED, NOT SCHEDULED

**D21: Phase 7 is gated on evidence, not scheduled.** This document is not an
implementation authorization; it is the requirements artifact D21 asks for, so
Phase 4 §1.2's surprise log has something concrete to be graded against.

| Gate | State |
|---|---|
| `PHASE4.md` §1.1 — a real node, real hardware, ≥ 2 weeks continuous, no correctness incident | **not met** |
| `PHASE4.md` §1.2 — the written surprise log | **not started** |
| `PHASE4.md` §1.3 — ≥ 1 pre-existing host-system bug found by multi-publisher detection | **not met** |
| `PHASE5.md` §0 — offline / observability users who adopted nothing | **not met** |

**Work on §3–§6 does not begin until all four are met.** Until then every §4 row
is a question for operating experience; a row answered from this document rather
than from the log is a guess shipped as a compatibility promise. Each surprise-log
entry is filed against a J-row or opens a new one.

Phase 6 ([`0009`](./decisions/0009-descoping-phase-6.md)) is not a gate on this phase.

## 1. What Phase 7 is for

`tf2`'s API shape, not its performance, is why people do not migrate: a node with
forty `lookupTransform` call sites cares how many lines change. The shim is **a
ramp, not a destination**: a header swap runs an existing node on `tf_tree`;
`Buffer::native_plan()` (§5) converts one hot loop at a time; and where `tf_tree`
deliberately refuses something `tf2` does (averaging two publishers, mixing time
domains) the shim **refuses loudly and says why**. **Non-goal:** bit-compatibility
with `tf2` where `tf2` is wrong (§4 J4, J9).

## 2. Scope

In scope: `Buffer` (`lookupTransform`, `canTransform` with and without timeout,
`allFramesAsYAML`, `allFramesAsString`); a `TransformListener` adapter that
guarantees the Phase 4 ingest bridge is running; the C++ package `tf_tree_tf2`
(header-only over the C ABI, same two error modes as `tf_tree.hpp`); the Python
module `tf_tree.tf2` (lazy `rclpy` import); `native_plan` (`API.md` §1 R1); and
arena → `/tf` egress, §8.

### Out of scope — NORMATIVE

| Excluded | Why |
|---|---|
| `doTransform` and every message-type conversion | §3.3. The shim returns `geometry_msgs::msg::TransformStamped`, so upstream `tf2_geometry_msgs`, `tf2_sensor_msgs` and users' overloads work unmodified |
| Anything defined in `namespace tf2_ros` | §3.2. ODR |
| `tf2::BufferCore`'s internal API (`_frameExists`, …) | Underscore-prefixed; used only by `tf2`'s own tools |
| `MessageFilter` | Large, subtle; better served by the bridge |
| ROS 1 | EOL |
| Blocking primitives in the arena | [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) |

## 3. Shape

### 3.1 A header over the C ABI, with no logic

As `tf_tree.hpp` (`PHASE4.md` §4.1): inline code in the user's translation unit
is invisible to Rust tests, Miri and ASan. **The wait loop from `0018` is the one
exception and lives in Rust**, behind `tft_wait_until_covered()` in
`tf_tree_unstable.h`. `ros/tf_tree_tf2/` is an `ament_cmake` package outside the
cargo workspace; `just ros-build` / `just ros-test` are its entire gate.

### 3.2 Naming — NORMATIVE, and irreversible later

C++ package `tf_tree_tf2`, header `tf_tree/tf2_compat.hpp`, namespace
`tf_tree::tf2_compat`, Python module `tf_tree.tf2`.

**Nothing is ever defined in `namespace tf2_ros` or `namespace tf2`** (ODR).
Migrators get an opt-in alias header, where to `static_assert` that upstream
`tf2_ros/buffer.h` was not also included:

```cpp
#define TF_TREE_TF2_ALIAS
#include <tf_tree/tf2_compat.hpp>   // pulls tf_tree::tf2_compat::Buffer into scope
```

### 3.3 `doTransform` is not reimplemented

`lookupTransform` returns `geometry_msgs::msg::TransformStamped`, so existing
`tf2::doTransform` overloads keep working.

### 3.4 Cost, stated up front

The shim re-resolves by name per call, inherent to `tf2`'s API shape.
Mitigation is `Tree::lookup`'s per-thread plan cache keyed on
`(arena, FrameId, FrameId, generation)`; residual cost is two name hashes, a
cache probe and the `TransformStamped` construction.

**NORMATIVE (`API.md` §7.8):** the benchmark table carries a row where the shim
is **slower than native `tf_tree`**, beside the row where it beats `tf2`;
`PHASE5.md` §9.3's honesty requirements apply verbatim.

## 4. The semantic judgments

Each row needs an answer, a differential test against `tf2::BufferCore`, and a
log line that justifies it. "Proposed" is a starting position, not a decision.

| # | Question | Proposed | Evidence needed |
|---|---|---|---|
| **J1** | `Time(0)` semantics | The upper bound of `Plan::span` — the largest stamp every dynamic edge on the path can answer for | `tf2`'s is computed per-pair and subtly different. **Differential-test, do not reason about it.** `span` is `None` for an all-static plan, which answers at any stamp |
| **J2** | `canTransform`/`lookupTransform` with a timeout | The predicted-sleep loop, `0018` §*Decision*. Granularity: scheduler quantum + one publish period | Whether any real node's startup is sensitive to a ~1 ms overshoot |
| **J3** | `tf2` accepts unknown frames at runtime; we are builder-time ([`0004`](./decisions/0004-builder-time-edge-declaration.md)) | The shim owns arena creation and declares from first-seen messages against `frame_headroom`/`edge_headroom`. Exhaustion is a typed error **naming the knob** | What headroom a real stack needs, and whether frame churn makes this untenable. **A growable arena is not an option** — D4 |
| **J4** | `tf2` interleaves competing publishers and silently blends two authorities | We reject and attribute. **An intentional incompatibility, documented as a fix** | `PHASE4.md` §1.3's falsifiable prediction. If two weeks find no conflict, determine whether detection is broken or the stack is clean *before* designing policy around it |
| **J5** | Exception mapping | 1:1 onto `LookupException`, `ConnectivityException`, `ExtrapolationException`, `InvalidArgumentException`, `TimeoutException`. Types are a promise; **message text is not** (`API.md` §1 R5) | Whether any real node matches on `what()` text; if so, document the incompatibility |
| **J6** | `cache_time` (`tf2` defaults to 10 s) | `Capacity::history(rate, seconds)` per edge. **State the bytes** — 10 s at 1 kHz is 10 000 slots × 64 B = 640 KiB per edge | What rate to assume for an undeclared edge. `PHASE5.md` §6's `TFT007` amendment is the precedent: a *measured* and a *declared* rate mean opposite things |
| **J7** | Extrapolation | Typed error → `ExtrapolationException`, **except** on the `Time(0)` path, where `tf2` does not throw | Confirm `tf2` does not extrapolate silently in any configuration |
| **J8** | `Buffer` shared across executors and callback groups | `Buffer` is `Sync`; no mutex on the read path. The plan cache is per-thread | Whether a many-callback-group node thrashes a 16-entry direct-mapped cache. Measure before resizing |
| **J9** | Which time domain does a query use? | Derived from the `rcl_clock_type_t` of the `Buffer`'s clock: `RCL_ROS_TIME` → `SimDomain` under `use_sim_time`, else `SystemDomain`; `RCL_STEADY_TIME` → `SteadyDomain`. Mismatch is `TimeDomainMismatch` | The read-side counterpart of `PHASE4.md` §5.5; catches the `use_sim_time` bug class `tf2` cannot detect. Confirm `rclcpp::Buffer` users always have a clock. `API.md` §5.2 explains why the offset is unrecoverable afterwards. **The prerequisite is met**: `SimDomain` is tag 2, `SteadyDomain` tag 3, permanently (`API.md` §2.5), and `tf_tree_bridge::config::parse_domain` maps all four names. The open bridge-side clause is tracked in `PHASE4.md` §5.5. **This row is still gated by §0.0** |
| **J10** | Leading-`/` frame names | Stripped, warned **once**, not once per message | Handled by the bridge (`PHASE4.md` §5.6); the shim must not warn a second time |
| **J11** | `setTransform(msg, authority, is_static)` | Accepted only on an `rw` arena, routed through a claim keyed on the authority string | Whether nodes that both publish and consume through one `Buffer` are common. The claim is an `OwnedWriter` ([`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md)) |

**Rows will be added.** A surprise-log entry that fits no row opens one.

## 5. The adoption ladder

`Buffer` carries exactly one non-`tf2` method, documented on the shim's *first*
page (NORMATIVE); `tf_tree.tf2.Buffer` carries it too, returning a `tf_tree.Plan`:

```cpp
tf_tree::Plan Buffer::native_plan(const std::string& target,
                                  const std::string& source);
auto plan = buffer.native_plan("map", "camera_optical");                // once
Eigen::Isometry3d T = plan.at<Eigen::Isometry3d>(stamp.nanoseconds());  // in the loop
```

## 6. Test plan

Differential against `tf2::BufferCore` over one recorded `/tf` stream to 1e-12
(`just tf2-differential`, `just tf2-replay`), through the shim's API, with a case
for every J-row claiming parity or divergence; timeout bounds `N + quantum +
period` and never early (J2); headroom exhaustion names the knob (J3, **mutant:**
a generic `LookupException` ⇒ fails); authority conflict refused and attributed
(J4); `TimeDomainMismatch` at construction (J9); ODR (§3.2): a unit including both
headers links, and with `TF_TREE_TF2_ALIAS` fails with our `static_assert`; the
`-fno-exceptions` matrix (`PHASE4.md` §6.2) built **and run**, plus ASan/UBSan.
All of it runs under `just ros-test` in `docker/tf2`; `PHASE4.md` §0.0's note on
what this environment cannot gate applies.

## 7. `API.md` §7 conformance

| # | Check | Answer |
|---|---|---|
| 1 | Three tiers reachable; way down documented | §5, on the first page |
| 2 | Hot tier allocates? | **Yes** — `TransformStamped` per call and a name resolution; inherent to `tf2`'s shape, §3.4 |
| 3 | Integer nanoseconds; domain derived | `rclcpp::Time::nanoseconds()`, never `seconds()`; J9 |
| 4 | Layout explicit | Not applicable at `Buffer`; `native_plan` inherits `layout_of<T>` |
| 5 | Errors typed, prose separate | J5 |
| 6 | Read-only default | `Buffer` opens `ro`; `setTransform` requires explicit `rw` (J11) |
| 7 | No stored type carries a lifetime | `Buffer` holds `Arc<Tree>`; `native_plan` returns a `'static` `Plan`; J11's claim is an `OwnedWriter` |
| 8 | A row where this surface loses | §3.4 — required |

## 8. Egress (arena → `/tf`), deferred within this phase

The other half of D21, specified later because it reintroduces the loopback, echo
and authority-cycle questions Phase 4's ingress-only decision removed. When
written it must say: which edges are republished and on whose authority; how a
bridged edge avoids re-ingestion; what happens when two hosts each run a bridge;
what `tf_tree top` shows for a cycle. `PHASE5.md` §8.4 defers the viewer problem
here: publishing back to `/tf` is a requirement on this section.

## 9. Phase 8 handoff

Inter-host replication assumes an interest declaration (D19); the shim's per-call
name resolution is the opposite, and `native_plan` makes one derivable. The
answered J-table is the semantics replication preserves. J9 is the local half of
Phase 8's cross-host alignment: the shim only refuses to mix domains and must not
acquire an alignment of its own.
