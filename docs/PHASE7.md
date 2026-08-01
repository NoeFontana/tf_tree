# tf_tree — Phase 7 Specification: the `tf2`-shaped compatibility surface

> **Companions:** [`PROJECT.md`](./PROJECT.md) (D21 gates this phase),
> [`API.md`](./API.md) (the contract every surface obeys),
> [`PHASE4.md`](./PHASE4.md) §5 (the ingest bridge this sits on),
> [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md) (the wait).

**Deliverable:** a `tf2_ros::Buffer`-shaped C++ and Python surface over
`tf_tree`, so an existing ROS 2 node adopts the engine with a header swap rather
than a rewrite — and so that having adopted it, the node can graduate to the
native API one hot loop at a time.

---

## 0.0 Status — GATED, NOT SCHEDULED

**D21: the compatibility layer is Phase 7 and is gated on evidence, not
scheduled.** This document is **not** an implementation authorization. It is the
requirements artifact D21 asks for: a written statement of the semantic
judgments the shim must make, so that Phase 4 §1.2's surprise log has something
concrete to be graded against.

| Gate | State |
|---|---|
| `PHASE4.md` §1.1 — a real node, real hardware, ≥ 2 weeks continuous, no correctness incident | **not met** |
| `PHASE4.md` §1.2 — the written surprise log | **not started** |
| `PHASE4.md` §1.3 — ≥ 1 pre-existing host-system bug found by multi-publisher detection | **not met** |
| `PHASE5.md` §0 — offline / observability users who adopted nothing | **not met** |

**Work on §3–§6 does not begin until all four are met.** What §4 is *for* before
then is the opposite of implementation: every row is a question to be answered
by operating experience, and a row answered from this document instead of from
the log is a guess shipped as a compatibility promise — which is the exact
failure D21 exists to prevent.

**The one thing to do now** is §4's discipline: as the surprise log accumulates,
each entry is filed against a J-row or opens a new one. A log that produces no
new rows means either the log or this table is not being read.

Note that Phase 6 (continuous-time interpolation,
[`0009`](./decisions/0009-descoping-phase-6.md)) is *not* a gate on this phase.
The phases are ordered by what constrains what, and nothing in the shim depends
on splines.

---

## 1. What Phase 7 is for

`tf2`'s API shape is the reason people do not migrate, not its performance. A
node with forty `lookupTransform` call sites does not evaluate an engine on
nanoseconds; it evaluates it on how many of those forty lines have to change.

So the shim's job is **to be a ramp, not a destination**:

1. A header swap and a `Buffer` construction change get an existing node running
   on `tf_tree`, with `tf2`'s semantics preserved wherever preserving them is
   honest.
2. `Buffer::native_plan()` (§5) lets that node convert one hot loop at a time to
   the compiled-plan API, with no restructuring.
3. Where `tf_tree` deliberately refuses something `tf2` does — averaging two
   publishers, mixing time domains — the shim **refuses loudly and says why**.
   These are the incompatibilities that are the product.

**Non-goal: bit-compatibility with `tf2` in the cases where `tf2` is wrong.**
See §4 J4 and J9.

---

## 2. Scope

### In scope

| | |
|---|---|
| `Buffer` | `lookupTransform`, `canTransform` (both with and without timeout), `allFramesAsYAML`, `allFramesAsString` |
| `TransformListener` | a no-op-shaped adapter that guarantees the Phase 4 ingest bridge is running |
| C++ package | `tf_tree_tf2`, header-only over the C ABI, same two error modes as `tf_tree.hpp` |
| Python module | `tf_tree.tf2`, lazy `rclpy` import |
| `native_plan` | the escape hatch to tier 2 (`API.md` §1 R1) |
| Arena → `/tf` egress | the other half of D21; specified separately in §8 |

### Out of scope — NORMATIVE

| Excluded | Why |
|---|---|
| `doTransform` and every message-type conversion | §3.3. The shim returns `geometry_msgs::msg::TransformStamped`, so upstream `tf2_geometry_msgs`, `tf2_sensor_msgs` and users' own overloads work **unmodified**. This removes most of the surface area and all of the message-type dependency graph. |
| Anything defined in `namespace tf2_ros` | §3.2. ODR. |
| `tf2::BufferCore`'s internal API (`_frameExists`, `_getFrameStrings`, …) | Underscore-prefixed and used only by `tf2`'s own tools. If a real node needs one, the surprise log will say so. |
| `MessageFilter` | Large, subtle, and its use case is better served by the bridge. Revisit only from the log. |
| ROS 1 | EOL. |
| Blocking primitives in the arena | [`0018`](./decisions/0018-blocking-waits-belong-in-the-shim.md). |

---

## 3. Shape

### 3.1 It is a header over the C ABI, and contains no logic

Same rule as `tf_tree.hpp` (`PHASE4.md` §4.1), same reason: inline code in the
user's translation unit is invisible to the Rust test suite, to Miri and to
ASan-instrumented Rust. Anything that needs a branch belongs in Rust.

**The wait loop from `0018` is the one exception, and it therefore lives in
Rust**, behind `tft_wait_until_covered()` in `tf_tree_unstable.h`, with the C++
side calling it. A retry loop with a deadline and a backoff is exactly the kind
of logic this rule exists to keep out of the header.

`ros/tf_tree_tf2/` is an `ament_cmake` package and therefore sits outside the
cargo workspace, exactly as `ros/tf_tree_ros/` does — `cargo fmt`, `clippy` and
`nextest` cannot see it, and a `just ros-build` / `just ros-test` pair is its
entire gate. That is a second, independent reason the logic belongs in Rust: the
half of this phase written in C++ is the half the workspace's own tooling cannot
check.

### 3.2 Naming — NORMATIVE, and irreversible later

| | |
|---|---|
| C++ package | `tf_tree_tf2` |
| C++ header | `tf_tree/tf2_compat.hpp` |
| C++ namespace | `tf_tree::tf2_compat` |
| Python module | `tf_tree.tf2` |

**Nothing is ever defined in `namespace tf2_ros` or `namespace tf2`.** A shim
that squats the upstream namespace is an ODR violation the moment one
translation unit sees both — a link error at best, and at worst a silently
selected wrong definition producing a wrong transform. This is not a
hypothetical failure mode; it is the standard one for drop-in replacements.

Migrators get an **opt-in** alias header:

```cpp
#define TF_TREE_TF2_ALIAS
#include <tf_tree/tf2_compat.hpp>   // pulls tf_tree::tf2_compat::Buffer into scope
```

One documented line in their code, versus a silent hijack. The alias header is
also the natural place to `static_assert` that upstream `tf2_ros/buffer.h` was
not also included.

### 3.3 `doTransform` is not reimplemented

`Buffer::lookupTransform` returns `geometry_msgs::msg::TransformStamped` — the
same type `tf2_ros::Buffer` returns. Therefore every existing
`tf2::doTransform(...)` overload, in `tf2_geometry_msgs`, `tf2_sensor_msgs`,
`tf2_eigen` and in the user's own code, keeps working with no change and no
dependency on us.

This is the single largest scope decision in the phase. Reimplementing the
conversions would mean owning a matrix of message types × geometry libraries
that upstream already owns, tests and ships.

### 3.4 Cost, stated up front

The shim re-resolves by name per call. That is inherent to `tf2`'s API shape,
not a defect we can engineer away: `lookupTransform(target, source, t)` has no
place to hold a compiled plan.

Mitigation is the per-thread plan cache that `Tree::lookup` already uses, keyed
on `(FrameId, FrameId, generation)`. Residual cost per call: two name hashes, a
cache probe, and the `TransformStamped` construction.

**NORMATIVE (`API.md` §7.8):** the benchmark table carries a row where the shim
is **slower than native `tf_tree`**, beside the row where it is faster than
`tf2`. A comparison that only reports wins is the kind this project has been
explicit about not shipping — `PHASE5.md` §9.3's honesty requirements apply to
this table verbatim.

---

## 4. The semantic judgments

D21's claim is that the shim is a hundred small semantic judgments about what
`tf2` does when asked something ambiguous. Here are the ones known before
operating experience. **Each row needs an answer, a differential test against
`tf2::BufferCore`, and a line in the log that justifies it.** The "proposed"
column is a starting position, not a decision.

| # | Question | Proposed | Evidence needed |
|---|---|---|---|
| **J1** | `Time(0)` semantics | The upper bound of `Plan::span` — the largest stamp every dynamic edge on the path can answer for | `tf2`'s is computed per-pair and subtly different. **Differential-test, do not reason about it.** Note `span` returns `None` for an all-static plan, which answers at any stamp; `Time(0)` there is not a shortfall. |
| **J2** | `canTransform`/`lookupTransform` with a timeout | The predicted-sleep loop, `0018` §*Decision*. Documented granularity: scheduler quantum + one publish period | Whether any real node's startup is sensitive to a ~1 ms overshoot. Nothing so far suggests one is. |
| **J3** | `tf2` accepts unknown frames at runtime; we are builder-time ([`0004`](./decisions/0004-builder-time-edge-declaration.md)) | The shim owns arena creation and declares from first-seen messages against `frame_headroom`/`edge_headroom`. Exhaustion is a typed error **naming the knob**. | What headroom a real stack needs, and whether frame churn (e.g. per-detection frames) makes this untenable. **A growable arena is not an option** — D4. |
| **J4** | `tf2` interleaves competing publishers and silently blends two authorities | We reject and attribute. **An intentional incompatibility, documented as a fix.** | This is `PHASE4.md` §1.3's falsifiable prediction. If two weeks of running finds no such conflict, determine whether detection is broken or the stack is unusually clean *before* designing the shim's policy around it. |
| **J5** | Exception mapping | 1:1 onto `LookupException`, `ConnectivityException`, `ExtrapolationException`, `InvalidArgumentException`, `TimeoutException`. Types are a promise; **message text is not** (`API.md` §1 R5) | Whether any real node matches on `what()` text. If one does, document the incompatibility; do not reproduce the text. |
| **J6** | `cache_time` (`tf2` defaults to 10 s) | `Capacity::history(rate, seconds)` per edge. **State what it costs in bytes** — 10 s at 1 kHz is 10 000 slots × 64 B = 640 KiB for one edge | What rate to assume for an edge that has not declared one, and whether a measured-rate fallback is needed. `PHASE5.md` §6's `TFT007` amendment is the precedent to read first: a *measured* rate and a *declared* rate mean opposite things, and the shim would be inferring the first while the arena reads it as the second. |
| **J7** | Extrapolation | Typed error → `ExtrapolationException`, **except** on the `Time(0)` path, where `tf2` does not throw | Confirm the exception is the only difference and that `tf2` does not extrapolate silently in any configuration. |
| **J8** | `Buffer` shared across executors and callback groups | `Buffer` is `Sync`; no mutex on the read path (readers are wait-free). The plan cache is per-thread | Whether a many-callback-group node thrashes a 16-entry direct-mapped per-thread cache. Measure before resizing. |
| **J9** | Which time domain does a query use? | Derived from the `rcl_clock_type_t` of the clock the `Buffer` was constructed with: `RCL_ROS_TIME` → `SimTime` under `use_sim_time`, else `SystemTime`; `RCL_STEADY_TIME` → `SteadyDomain`. Mismatch is `TimeDomainMismatch`, not a plausible wrong answer | **This is the read-side counterpart of `PHASE4.md` §5.5**, which already does it for the write side and fails at startup rather than at first message. It catches the `use_sim_time` bug class — a node mixing `/clock` sim time with a driver's steady time — that `tf2` cannot detect at all. Confirm `rclcpp::Buffer` users always have a clock to derive from. See `API.md` §5.2 for why the offset is unrecoverable after the fact. **`SimTime` and `SteadyDomain` do not exist yet** — the built-ins are `SystemDomain` (tag 0) and `SensorDomain` (tag 1), and the bridge carries a domain as a bare `u8`. Declaring them, and settling the tag mapping with `PHASE4.md` §5.5's write side in the same change, is a prerequisite of this row and not part of it. |
| **J10** | Leading-`/` frame names | Stripped, warned **once**, not once per message | Already handled by the bridge (`PHASE4.md` §5.6); the shim inherits it and must not warn a second time. |
| **J11** | `setTransform(msg, authority, is_static)` | Accepted only on an `rw` arena, routed through a claim keyed on the authority string | Whether nodes that both publish and consume through one `Buffer` are common. If they are, this is not a corner. The claim is stored, not scoped, so it is an `OwnedWriter` ([`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md)) — this row is the third consumer that record predicts. |

**Rows will be added.** A surprise-log entry that fits no row opens one; that is
the mechanism by which this table becomes trustworthy.

---

## 5. The adoption ladder

`Buffer` carries exactly one non-`tf2` method:

```cpp
tf_tree::Plan Buffer::native_plan(const std::string& target,
                                  const std::string& source);
```

A migrating node keeps its forty `lookupTransform` call sites and converts its
one hot loop:

```cpp
// before
auto T = buffer.lookupTransform("map", "camera_optical", stamp);

// after — same Buffer, same node, tier 2
auto plan = buffer.native_plan("map", "camera_optical");   // once
Eigen::Isometry3d T = plan.at<Eigen::Isometry3d>(stamp.nanoseconds());  // in the loop
```

**NORMATIVE:** this method is documented in the shim's *first* page, not its
last. A compatibility layer whose escape hatch is undiscoverable becomes the
destination it was built to avoid — and then the project owns `tf2`'s API shape
forever.

The Python module carries the same method on `tf_tree.tf2.Buffer`, returning a
`tf_tree.Plan`.

---

## 6. Test plan

1. **Differential against `tf2::BufferCore`** over the same recorded `/tf`
   stream, asserting agreement to 1e-12 — the Phase 1 differential harness
   (`just tf2-differential`, `just tf2-replay`), now driven through the shim's
   API rather than the engine's. Every J-row that claims `tf2` parity gets a
   case here; every row that claims a deliberate divergence gets a case
   asserting the divergence.
2. **Timeout behaviour** (J2): a publisher started N ms after the waiter, for
   N across the interesting range, asserting the wait returns within
   `N + quantum + period` and never before the data exists.
3. **Headroom exhaustion** (J3): declare beyond `frame_headroom`, assert the
   error names the knob. **Mutant:** return a generic `LookupException` ⇒ fails.
4. **Authority conflict** (J4): two publishers on one edge, assert the shim
   refuses and attributes rather than blending. Compare against `tf2` on the
   same input to demonstrate the blend it produces.
5. **Domain mismatch** (J9): a `Buffer` on `RCL_STEADY_TIME` querying an arena
   the bridge tagged `SimTime`, asserting `TimeDomainMismatch` at construction,
   not at first lookup.
6. **ODR** (§3.2): a translation unit including both `tf2_ros/buffer.h` and
   `tf_tree/tf2_compat.hpp` compiles and links, and with `TF_TREE_TF2_ALIAS`
   defined it fails with our `static_assert`'s message.
7. **The `-fno-exceptions` matrix**, inherited from `PHASE4.md` §6.2: {gcc,
   clang} × {C++17, C++20} × {exceptions, `-fno-exceptions`}, all built **and
   run**, plus an ASan/UBSan build.

All of it runs under `just ros-test` in `docker/tf2`, for the reason §3.1 gives:
nothing on the host can build an `ament_cmake` package. `PHASE4.md` §0.0's note
about what this development environment cannot gate — a second RMW, clang in
that image, a robot — applies unchanged, and items 5 and 7 are the ones it bites.

---

## 7. `API.md` §7 conformance

The check every surface passes, applied to this one.

| # | Check | Answer |
|---|---|---|
| 1 | Three tiers reachable; way down documented | §5, on the first page |
| 2 | Hot tier allocates? | **Yes** — `TransformStamped` per call, and a name resolution. Inherent to `tf2`'s shape; stated in §3.4 and measured |
| 3 | Integer nanoseconds end to end; domain derived, not remembered | `rclcpp::Time::nanoseconds()`, never `seconds()`; J9 derives the domain from the clock |
| 4 | Layout explicit | Not applicable at the `Buffer` surface (`TransformStamped` is a fixed message); applies to `native_plan`, which inherits `layout_of<T>` |
| 5 | Errors typed, prose separate | J5 |
| 6 | Read-only default | `Buffer` opens `ro`; `setTransform` requires an explicit `rw` construction (J11) |
| 7 | No stored type carries a lifetime | `Buffer` holds `Arc<Tree>`; `native_plan` returns a `'static` `Plan`; J11's claim is an `OwnedWriter` |
| 8 | A row where this surface loses | §3.4 — required, not optional |

Check 2 is the one this surface fails, and it fails it for a reason that cannot
be engineered away. That is exactly why §5 exists and why it is on the first
page.

---

## 8. Egress (arena → `/tf`), deferred within this phase

The other half of D21. It is specified separately and later because it
reintroduces every loopback, echo and authority-cycle question that Phase 4's
ingress-only decision removed — and those questions are much easier to answer
once the shim has revealed how nodes actually use a `Buffer` that both reads and
writes (J11).

Minimum content when written: which edges are republished and on whose
authority; how a bridged edge is prevented from being re-ingested as a new
sample; what happens when two hosts each run a bridge; and what `tf_tree top`
shows so an operator can see a cycle before it becomes a support ticket.

Egress is also what `PHASE5.md` §8.4 defers the "tf_tree is the source of truth
and the viewer cannot see it" problem to. That section's argument — publish back
to `/tf` and every existing viewer works, with no viewer-specific code anywhere —
is a requirement on this section, not an independent idea.

---

## 9. Phase 8 handoff

- Inter-host replication assumes an interest declaration (`(target, source)` at
  a rate and precision, D19). The shim's per-call name resolution is the
  **opposite** of an interest declaration; `native_plan` is what makes one
  derivable. If the shim becomes the dominant surface, Phase 8 loses its input —
  one more reason §5 is a first-page feature.
- Carry the answered J-table into Phase 8 as the semantics replication must
  preserve across a link.
- J9's domain derivation is the local half of what Phase 8 does across hosts.
  Phase 8 aligns two domains and reports an uncertainty; this phase only refuses
  to mix them. Do not let the shim acquire an alignment of its own.
