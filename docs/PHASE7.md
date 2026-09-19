# tf_tree — Phase 7 Specification: the `tf2`-shaped compatibility surface

> **Companions:** [`PROJECT.md`](./PROJECT.md) (D21), [`API.md`](./API.md),
> [`PHASE4.md`](./PHASE4.md) §5, [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md).

## 0.0 Status — GATED, NOT SCHEDULED

**D21: gated on evidence, not scheduled.** A `tf2_ros::Buffer`-shaped C++ and
Python surface over `tf_tree`; this document is not an implementation authorization.

| Gate | State |
|---|---|
| `PHASE4.md` §1.1 — a real node, real hardware, ≥ 2 weeks continuous, no correctness incident | **not met** |
| `PHASE4.md` §1.2 — the written surprise log | **not started** |
| `PHASE4.md` §1.3 — ≥ 1 pre-existing host-system bug found by multi-publisher detection | **not met** |
| `PHASE5.md` §0 — offline / observability users who adopted nothing | **not met** |

**Work on §3–§6 does not begin until all four are met.** A §4 row answered from
this document rather than from the log is a guess. Each surprise-log entry is
filed against a J-row or opens a new one.

## 1. What Phase 7 is for

`tf2`'s API shape is why people do not migrate. The shim is **a ramp, not a destination**: a header swap, then `Buffer::native_plan()` (§5)
per hot loop. Where `tf_tree` refuses what `tf2` does (blending publishers,
mixing time domains) the shim **refuses loudly and says why**. **Non-goal:**
bit-compatibility where `tf2` is wrong (§4 J4, J9).

## 2. Scope

In scope: `Buffer` (`lookupTransform`, `canTransform` with and without timeout,
`allFramesAsYAML`, `allFramesAsString`); a `TransformListener` adapter that
guarantees the ingest bridge runs; C++ package `tf_tree_tf2` and Python module
`tf_tree.tf2` (lazy `rclpy`); `native_plan`; arena → `/tf` egress, §8.

### Out of scope — NORMATIVE

`doTransform` and every message-type conversion (`lookupTransform` returns
`geometry_msgs::msg::TransformStamped`, so upstream overloads work); anything in
`namespace tf2_ros` (§3.2, ODR); `tf2::BufferCore` internals; `MessageFilter`;
ROS 1; blocking primitives in the arena ([`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md)).

## 3. Shape

- **3.1 Header over the C ABI, no logic.** As `tf_tree.hpp` (`PHASE4.md` §4.1).
  The `0018` wait loop is the one exception and lives in Rust, behind
  `tft_wait_until_covered()`. `ros/tf_tree_tf2/` is an `ament_cmake` package outside
  the cargo workspace; `just ros-build` / `just ros-test` are its gate.
- **3.2 Naming — NORMATIVE.** C++ package `tf_tree_tf2`, header
  `tf_tree/tf2_compat.hpp`, namespace `tf_tree::tf2_compat`, Python module
  `tf_tree.tf2`. Nothing is ever defined in `namespace tf2_ros` or `tf2` (ODR).
  An opt-in alias (`#define TF_TREE_TF2_ALIAS`) `static_assert`s upstream
  `tf2_ros/buffer.h` was not also included.
- **3.4 Cost — NORMATIVE (`API.md` §7.8).** The shim re-resolves by name per
  call. The benchmark table carries a row where the shim is **slower than native
  `tf_tree`**, beside the row where it beats `tf2`; `PHASE5.md` §9.3 applies verbatim.

## 4. The semantic judgments

Each row needs an answer, a differential test against `tf2::BufferCore`, and a
log line that justifies it. "Proposed" is not a decision.

| # | Question | Proposed | Evidence needed |
|---|---|---|---|
| **J1** | `Time(0)` semantics | The upper bound of `Plan::span` — the largest stamp every dynamic edge on the path can answer for | `tf2`'s is computed per-pair and subtly different. **Differential-test, do not reason about it.** `span` is `None` for an all-static plan, which answers at any stamp |
| **J2** | `canTransform`/`lookupTransform` with a timeout | The predicted-sleep loop, `0018` §*Decision*. Granularity: scheduler quantum + one publish period | Whether any real node's startup is sensitive to a ~1 ms overshoot |
| **J3** | `tf2` accepts unknown frames at runtime; we are builder-time ([`0004`](./decisions/0004-builder-time-edge-declaration.md)) | The shim owns arena creation and declares from first-seen messages against `frame_headroom`/`edge_headroom`. Exhaustion is a typed error **naming the knob** | What headroom a real stack needs, and whether frame churn makes this untenable. **A growable arena is not an option** — D4 |
| **J4** | `tf2` interleaves competing publishers and silently blends two authorities | We reject and attribute. **An intentional incompatibility, documented as a fix** | `PHASE4.md` §1.3's falsifiable prediction. If two weeks find no conflict, determine whether detection is broken or the stack is clean *before* designing policy around it |
| **J5** | Exception mapping | 1:1 onto `LookupException`, `ConnectivityException`, `ExtrapolationException`, `InvalidArgumentException`, `TimeoutException`. Types are a promise; **message text is not** (`API.md` §1 R5) | Whether any real node matches on `what()` text |
| **J6** | `cache_time` (`tf2` defaults to 10 s) | `Capacity::history(rate, seconds)` per edge. **State the bytes** — 10 s at 1 kHz is 10 000 slots × 64 B = 640 KiB per edge | What rate to assume for an undeclared edge |
| **J7** | Extrapolation | Typed error → `ExtrapolationException`, **except** on the `Time(0)` path, where `tf2` does not throw | Confirm `tf2` does not extrapolate silently in any configuration |
| **J8** | `Buffer` shared across executors and callback groups | `Buffer` is `Sync`; no mutex on the read path. The plan cache is per-thread | Whether a many-callback-group node thrashes a 16-entry direct-mapped cache |
| **J9** | Which time domain does a query use? | Derived from the `rcl_clock_type_t` of the `Buffer`'s clock: `RCL_ROS_TIME` → `SimDomain` under `use_sim_time`, else `SystemDomain`; `RCL_STEADY_TIME` → `SteadyDomain`. Mismatch is `TimeDomainMismatch` | The read-side counterpart of `PHASE4.md` §5.5; `API.md` §5.2 explains why the offset is unrecoverable afterwards. `SimDomain` is tag 2, `SteadyDomain` tag 3, permanently (`API.md` §2.5). J9 is the local half of Phase 8's cross-host alignment: the shim refuses to mix domains and must not acquire an alignment of its own |
| **J10** | Leading-`/` frame names | Stripped, warned **once**, not once per message | Handled by the bridge (`PHASE4.md` §5.6); the shim must not warn a second time |
| **J11** | `setTransform(msg, authority, is_static)` | Accepted only on an `rw` arena, routed through a claim keyed on the authority string | Whether nodes that both publish and consume through one `Buffer` are common. The claim is an `OwnedWriter` ([`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md)) |

## 5. The adoption ladder
`Buffer` carries exactly one non-`tf2` method, documented on the shim's *first*
page (NORMATIVE): `Buffer::native_plan(target, source)` returns a `tf_tree::Plan`
(`tf_tree.Plan` in Python), used as `plan.at<T>(stamp.nanoseconds())` in the loop.

## 6. Test plan

Differential against `tf2::BufferCore` over one recorded `/tf` stream to 1e-12
(`just tf2-differential`, `just tf2-replay`), one case per J-row; timeout bounds
`N + quantum + period`, never early (J2); headroom exhaustion names the knob (J3,
**mutant:** a generic `LookupException` ⇒ fails); authority conflict attributed
(J4); `TimeDomainMismatch` at construction (J9); ODR (§3.2); the
`-fno-exceptions` matrix (`PHASE4.md` §6.2) built **and run**, plus ASan/UBSan,
under `just ros-test`.

## 7. `API.md` §7 conformance

Tiers: §5. Hot tier allocates (**yes**: `TransformStamped` and a name resolution
per call, §3.4). Integer nanoseconds via `rclcpp::Time::nanoseconds()`, domain
derived (J9). Layout: `native_plan` inherits `layout_of<T>`. Errors typed (J5).
Read-only default: `Buffer` opens `ro`, `setTransform` requires `rw` (J11). No
stored lifetime: `Buffer` holds `Arc<Tree>`, `native_plan` returns a `'static`
`Plan`, J11's claim is an `OwnedWriter`. The row where this surface loses: §3.4.

## 8. Egress (arena → `/tf`), deferred within this phase

The other half of D21. When written it must say: which edges are republished and
on whose authority; how a bridged edge avoids re-ingestion; what happens when two
hosts each run a bridge; what `tf_tree top` shows for a cycle (`PHASE5.md` §8.4).
