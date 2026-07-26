# tf_tree — Phase 4 Implementation Specification: Dogfooding Integration

> **Companions:** `docs/PROJECT.md` (decision log), `docs/PHASE1.md`–`PHASE3.md` (implemented). Phase 5 is `docs/PHASE5.md`.

**Deliverable:** enough C, C++, and ROS 2 surface that a *new* perception node in a real stack reads from `tf_tree` instead of `tf2`, running on real hardware for a sustained period.

**This is not the `tf2_ros::Buffer` shim.** Per D28/D29, the compatibility layer is Phase 7 and is gated on evidence. Phase 4 exists to produce that evidence and, more importantly, to produce the operating experience without which the shim's hundred small semantic judgments cannot be made well.

**Exit criterion is operational, not a feature list.** See §1.

---

## 0.0 Implementation status

**Not started.** This section is the live status table, in the style of
`PHASE2.md` §0.0, and is updated as work lands.

| Area | Status |
|---|---|
| §2 `at_with_derivatives` | Not implemented |
| §3 C ABI — `tf_tree.h`, `tf_tree_unstable.h` | Not implemented |
| §4 C++ wrapper, CMake package | Not implemented |
| §5 ROS 2 ingest bridge | **Blocked — see below** |
| §6 test plan | Follows its section |
| §7 benchmarks and gate | Follows its section |
| §1 operational criteria | **Blocked — see below** |

### What this development environment can and cannot gate

Stated plainly here rather than discovered later, because two of this phase's
deliverables cannot be honestly claimed from this machine:

- **No ROS 2 installation** (`/opt/ros` is absent; no `rclcpp`, no `rosbag2`,
  no DDS). §5's bridge cannot be built, and its QoS regression test — the one
  test in this phase most worth having, because it catches the single most
  common ROS 2 tf integration bug — cannot run at all. A mock DDS proves
  nothing about QoS negotiation, which is the entire subject.
- **No robot and no two-week window.** §1's operational criteria are not
  satisfiable by any amount of code. They gate Phase 7, and they stay open.

The honest split is therefore: **§5 is designed and its ROS-independent half is
implemented and tested; its ROS-coupled half is not written.** Concretely, the
authority policy (§5.4), frame-name normalization (§5.6), static-transform
semantics (§5.7), and clock-reset handling (§5.5) are pure functions of a
`TransformStamped`-shaped input and are implemented and unit-tested against
synthesised input. The subscription, QoS, executor, and `publisher_gid`
attribution are not. Anything not implemented is marked as such in this table
rather than being quietly counted as done.

---

## 0. Scope

### In scope

| | |
|---|---|
| `sample_with_derivatives` | pulled forward from Phase 6 — §2 shows why it is nearly free |
| C ABI (`tf_tree.h`) | small, stable, semver'd; `cbindgen`-generated, hand-reviewed |
| C ABI (`tf_tree_unstable.h`) | everything else, no guarantees, opt-in by macro |
| C++ wrapper (`tf_tree.hpp`) | header-only, RAII, Eigen and Sophus interop |
| CMake package | `find_package(tf_tree)`, plus a `corrosion` source-build path |
| ROS 2 ingest bridge | one-way `/tf` + `/tf_static` → arena, with publisher attribution |
| Deployment forms | standalone node, `rclcpp` component, in-process library handle |

### Out of scope — NORMATIVE

| Excluded | Why |
|---|---|
| `tf2_ros::Buffer` API compatibility | Phase 7, gated (D28) |
| Publishing arena → `/tf` (egress) | Phase 7. Ingress-only removes all loopback questions from this phase. |
| ROS 1 | EOL. A bridge only, if ever. |
| Python changes | Phase 3 is done and the C ABI does not touch it (D: PyO3 binds Rust directly) |
| Covariance, splines, CoW branches | Phase 6 |
| A C++ *implementation* of anything | The wrapper is a header over the C ABI. No logic in C++ — logic lives in Rust where it is tested. |

---

## 1. What Phase 4 must prove

Feature completion is not the gate. The gate is:

1. **A real node, on real hardware, for ≥ 2 weeks continuous**, consuming transforms from `tf_tree` via the bridge, with no correctness incident.
2. **A written log of every surprise** — API friction, missing operations, confusing errors, deployment papercuts. This log *is* the design input for Phase 7 and is more valuable than the code.
3. **At least one pre-existing bug found in the host system** by the bridge's multi-publisher detection (§5.4). This is a falsifiable prediction: real ROS systems routinely have two nodes publishing one edge, and `tf2` averages them silently. If two weeks of running finds nothing, either the detection is broken or the host stack is unusually clean — determine which.

Do not proceed to Phase 5 having only met criterion 1.

---

## 2. `sample_with_derivatives` — pulled forward

### 2.1 Why now

D-level rationale: it is the clearest signal that `tf_tree` is a different primitive rather than a faster `tf2`, greenfield users (a new VIO or SLAM) pay no migration cost to adopt it, and — measured below — **it costs zero additional transcendental calls under ScLerp.**

### 2.2 API

```rust
pub struct Twist { pub omega: Vec3, pub v: Vec3 }   // body frame (right), rad/s and m/s

pub struct Sample {
    pub pose:  Iso3,
    pub twist: Twist,
    pub accel: Option<Twist>,   // None for ScLerp/LerpSlerp; Some(_) for splines (Phase 6)
}

impl Plan {
    pub fn at_with_derivatives(&self, g: &Guard, t: Stamp) -> Result<Sample, LookupError>;
}
```

**Convention — NORMATIVE:** twists are **body-frame (right) twists**, `V^b = (T⁻¹ Ṫ)^∨`, matching the right-perturbation convention already fixed in Phase 1 §3.1 for covariance. Provide `Twist::to_spatial(&self, t: &Iso3)` = `Ad(T) V^b` and document the pairing. Everyone gets this wrong once; make the type name and the docstring carry the convention.

### 2.3 ScLerp gives an exact, constant body twist for free

For `T(s) = a · exp(s · ξ^)` with `ξ = log(a⁻¹b)` and `s = (t − t₀)/Δt`:

```
V^b(t) = ξ / Δt        — constant across the whole segment
```

Verified numerically against central differences: **max relative error 2.4 × 10⁻⁷** (limited by the finite-difference step, not the formula), and constant across `s ∈ {0, 0.17, 0.5, 0.83, 1}`.

`ξ` is *already computed* by the ScLerp evaluation. **The first derivative costs one scalar multiply.**

Composition along a plan, both verified to ~5 × 10⁻¹¹:

```
T_ac = T_ab · T_bc   ⇒   V_ac^c = Ad(T_bc⁻¹) · V_ab^b + V_bc^c
S = T⁻¹              ⇒   V_S^b   = −Ad(T) · V_T^b
```

So `at_with_derivatives` accumulates one 6×6 adjoint application per plan step — roughly 2× a plain lookup, opt-in, no transcendentals. Add these two identities as proptests alongside the Phase 1 set.

### 2.4 The finding worth putting in the docs

LerpSlerp and ScLerp produce the **same** angular velocity (SLERP is constant-ω in body frame). They differ in the linear part, and the difference is not small. Measured on one random segment, body-frame linear velocity across `s = 0.05 → 0.95`:

```
LerpSlerp   v = [ 34.31, -0.75, 18.49] → [ 15.73, -35.43, -4.08]   spread 34.7
ScLerp      v = [ 25.997, -27.519, 20.879] → identical              spread 1.5e-10
```

Note the trap: LerpSlerp's `|v|` is constant to 4 × 10⁻¹¹ while the *vector* swings by 34.7. **A magnitude check will not catch this.** Physically, LerpSlerp's world-frame linear velocity is constant, so the body-frame velocity rotates through the segment — a spurious apparent lateral acceleration that is purely an artifact of the interpolant.

Anyone differentiating transforms (velocity estimation, IMU preintegration consistency checks, velocity-aware deskewing) gets that artifact from `tf2` today and has no way to see it. This is a concrete, demonstrable reason ScLerp is the default, and it belongs in the README with the numbers.

**NORMATIVE:** `at_with_derivatives` returns `Err(DerivativesUnavailable { edge, interp })` for `LerpSlerp` edges rather than returning the misleading value. The compatibility interpolator exists for bit-matching `tf2`, and silently handing back a derivative it does not really have would be worse than refusing.

---

## 3. The C ABI

### 3.1 Two tiers — NORMATIVE

This is the first ABI freeze in the project and the surface is permanent, so keep it small on purpose.

| Header | Contents | Guarantee |
|---|---|---|
| `tf_tree.h` | open/close, plan, at, at_many, publisher, declare, errors, version | **semver, frozen at 1.0** |
| `tf_tree_unstable.h` | everything else — introspection, telemetry, adaptive, derivatives | none; requires `#define TFT_ENABLE_UNSTABLE` |

**Do not mirror the Rust API into C.** The stable header should be ~30 functions. Anything a C++ user does not need in the hot path belongs in the unstable header until Phase 7 has told us what is actually used.

### 3.2 Handles and ownership

```c
typedef struct tft_tree      tft_tree;       // Send + Sync   -> shareable across threads
typedef struct tft_plan      tft_plan;       // Send + Sync   -> shareable, immutable
typedef struct tft_publisher tft_publisher;  // Send + !Sync  -> ONE THREAD AT A TIME
```

Every `tft_*_create`/`_open` pairs with exactly one `tft_*_free`. Freeing NULL is a no-op. Double-free is undefined and detected in debug builds by a magic word in the handle header.

**`tft_publisher` thread affinity — NORMATIVE.** C cannot express `!Sync`. Record the creating thread id in the handle and, in debug builds, `abort()` with a clear message on use from another thread. C users will hit this; a loud abort in debug beats silent corruption in release.

### 3.3 Error model

Status code plus a thread-local structured detail, because Phase 1's typed errors carry data that Python already exposes and C++ must not lose.

```c
typedef int32_t tft_status;   /* 0 = ok; negative = error */

typedef struct {
    uint32_t struct_size;          /* = sizeof(tft_error) at compile time */
    tft_status code;
    uint32_t edge;                 /* TFT_INVALID_ID when N/A */
    uint32_t frame_a, frame_b;
    int64_t  requested, oldest, newest;
    uint64_t plan_generation, current_generation;
    char     message[256];         /* NUL-terminated, names resolved */
} tft_error;

const tft_error* tft_last_error(void);   /* thread-local; valid until the next tf_tree call
                                            on THIS thread; never NULL */
```

Message formatting happens only on the error path. Document the thread-local lifetime in the header itself, not just the manual — this is the single most common C-API misuse.

### 3.4 Panic safety — NORMATIVE

Since Rust 1.81 a panic escaping an `extern "C"` function aborts the process. For a library embedded in someone's robot, killing the host process on an internal bug is unacceptable.

**Every `extern "C"` entry point wraps its body in `std::panic::catch_unwind`**, converting a panic into `TFT_ERR_INTERNAL` with the panic payload copied into `message`. `catch_unwind` is zero-cost on the non-panicking path (landing pads only), so this does not affect the hot-path budget — but add a benchmark row proving it, because it is the kind of claim that gets doubted.

### 3.5 Layouts — and the quaternion order trap

```c
typedef enum {
    TFT_LAYOUT_QVEC7_WXYZ = 0,   /* [qw qx qy qz tx ty tz] f64 — canonical, matches the arena */
    TFT_LAYOUT_QVEC7_XYZW = 1,   /* [qx qy qz qw tx ty tz] f64 — Eigen/Sophus coefficient order */
    TFT_LAYOUT_MAT4_COL   = 2,   /* 4x4 f64 column-major — Eigen                               */
    TFT_LAYOUT_MAT4_ROW   = 3,   /* 4x4 f64 row-major — C, NumPy                               */
    TFT_LAYOUT_AFFINE12_ROW_F32 = 4,  /* 3x4 f32 row-major — GPU                               */
} tft_layout;
```

Two traps, both of which produce plausible-looking wrong answers rather than crashes:

**Quaternion component order.** Our canonical form is `w`-first. `Eigen::Quaterniond`'s *internal storage* is `(x, y, z, w)` even though its constructor takes `(w, x, y, z)`. A `memcpy` from `QVEC7_WXYZ` into an `Eigen::Quaterniond` or a `Sophus::SE3d` is silently wrong — it yields a rotation that is usually still a valid unit quaternion, so nothing complains. `TFT_LAYOUT_QVEC7_XYZW` exists solely to make the correct thing the easy thing. Phase 1 §3.1 flagged this; Phase 4 is where it must actually be handled.

**Matrix major order.** Row-major and column-major differ by a transpose, which for a rotation is its inverse — again a valid transform, pointing the wrong way. Never infer major order from context; the enum is explicit and there is no default.

**NORMATIVE test:** for every layout, round-trip a known transform through the C ABI and assert against a hand-computed expected byte pattern, not against another tf_tree call. A self-consistent pair of bugs would otherwise pass.

### 3.6 ABI versioning

```c
uint32_t tft_abi_version_major(void);
uint32_t tft_abi_version_minor(void);
```

Rules: **major must match exactly**; the runtime minor may be ≥ the compiled-against minor. Every struct passed by pointer begins with `uint32_t struct_size`, so fields can be appended without a major bump (the Vulkan approach); the callee validates `struct_size` and rejects unknown sizes.

The C++ header performs this check in a static initializer and throws / aborts on mismatch with both versions named. A silently mismatched ABI is a debugging session nobody deserves.

### 3.7 The hot path

```c
tft_status tft_plan_at(const tft_plan*, int64_t stamp, tft_layout, void* out);
tft_status tft_plan_at_many(const tft_plan*, const int64_t* stamps, size_t n,
                            tft_layout, void* out, size_t out_stride_bytes);
```

`out_stride_bytes` allows writing directly into an array of user structs whose element size exceeds the payload (§4.3 — this is exactly what `Sophus::SE3d` needs). Zero means tightly packed.

No allocation, no locking, no `catch_unwind` cost measurable at this granularity. Gate: **within 5% of the native Rust benchmark for the same query.**

---

## 4. The C++ wrapper

### 4.1 Shape

Header-only, C++17, `namespace tf_tree`. RAII handles with deleted copy and defaulted move. Two error modes selected at include time:

- Default: throws `tf_tree::Error` carrying the full `tft_error` as accessors.
- `#define TF_TREE_NO_EXCEPTIONS`: every call returns `tf_tree::expected<T, Error>` (a minimal internal `expected`, or `std::expected` when C++23 is available). Robotics shops that compile with `-fno-exceptions` are common enough that this is not optional.

No logic. Every function is a thin inline over the C ABI. If a behaviour needs a branch, it belongs in Rust.

### 4.2 Eigen interop

```cpp
Eigen::Isometry3d T = plan.at<Eigen::Isometry3d>(stamp);

std::vector<Eigen::Isometry3d> out(n);
plan.at_many(stamps, out);            // writes straight into out.data()
```

`Eigen::Isometry3d` stores a 4×4 column-major `Matrix4d`: 128 bytes, alignment 16 (or 32 under AVX), and 128 is a multiple of both, so **an array of `Isometry3d` is tightly packed and `TFT_LAYOUT_MAT4_COL` writes into it with no copy and no stride.** Assert it rather than assume:

```cpp
static_assert(sizeof(Eigen::Isometry3d) == 128, "unexpected Eigen Transform layout");
```

C++17's over-aligned `new` makes `std::vector<Eigen::Isometry3d>` correct without `Eigen::aligned_allocator`; under C++14 the allocator is still required, so document it and `static_assert(__cplusplus >= 201703L)` on the convenience overload.

### 4.3 Sophus interop and the alignment hazard

`Sophus::SE3d` holds an `Eigen::Quaterniond` (32 B, alignment 16 or 32 depending on vectorization flags) followed by a `Vector3d` (24 B). The payload is 56 bytes, but the type's alignment rounds `sizeof` up — commonly to 64. **An array of `SE3d` is therefore usually *not* tightly packed**, and a `memcpy` of `n × 56` bytes into it corrupts every element after the first.

This is what `out_stride_bytes` is for:

```cpp
plan.at_many(stamps, TFT_LAYOUT_QVEC7_XYZW, out.data(), sizeof(Sophus::SE3d));
```

Because `sizeof` depends on the user's Eigen build flags, **it must be read at compile time from their build, never assumed**, and the wrapper must `static_assert` that the quaternion precedes the translation with no interior padding before enabling the direct path. If the assert fails, fall back to an element-wise loop — the operation is memory-bound and the loop costs almost nothing.

### 4.4 Packaging

- `find_package(tf_tree CONFIG)` exporting `tf_tree::tf_tree` (shared) and `tf_tree::tf_tree_static`, with `INTERFACE_COMPILE_FEATURES cxx_std_17`.
- Source builds via **`corrosion`**, so a `colcon build` works without the user hand-managing cargo.
- **Requiring a Rust toolchain is real adoption friction.** Ship prebuilt static libraries for `x86_64` and `aarch64` `linux-gnu` alongside the source path, and make the CMake config prefer a prebuilt when the target matches. Revisit a proper ROS binary package (bloom/rosdep) in Phase 7.

---

## 5. The ROS 2 ingest bridge

### 5.1 Ingress only — NORMATIVE

The bridge subscribes `/tf` and `/tf_static` and writes into the arena. It does **not** publish. One direction eliminates every loopback, echo, and authority-cycle question from this phase, and it is all that dogfooding needs: new nodes read from `tf_tree`, existing nodes keep publishing to `/tf` unchanged.

### 5.2 QoS — get this wrong and you silently receive nothing

**NORMATIVE**, matching `tf2_ros::TransformListener`:

| Topic | QoS |
|---|---|
| `/tf` | `KeepLast(100)`, **reliable**, **volatile** |
| `/tf_static` | `KeepLast(100)`, **reliable**, **transient_local** |

`/tf_static` is latched: each static broadcaster keeps its own transforms alive for late joiners, and depth 100 with `transient_local` is what collects all of them on subscribe. A `volatile` subscription to `/tf_static` receives nothing from publishers that started earlier and never publish again — which is most of them. This is the single most common ROS 2 tf integration bug and it presents as "my static transforms are missing" with no error anywhere.

Log the negotiated QoS at startup and warn on any incompatibility event.

### 5.3 Publisher attribution — the bridge is a bug detector

`TFMessage` carries no publisher identity, but the middleware does. `rclcpp::MessageInfo` exposes `rmw_message_info_t::publisher_gid`, and `Node::get_publishers_info_by_topic("/tf")` returns `TopicEndpointInfo` records carrying `node_name()`, `node_namespace()`, and `endpoint_gid()`. Match one against the other, cached and refreshed on graph-change events.

**Degrade gracefully:** GID reporting varies across RMW implementations. On failure, attribute to `"<unknown publisher>"` and keep running — attribution is diagnostic value, never a correctness dependency.

### 5.4 Authority policy — NORMATIVE

ROS permits any number of publishers per edge; `tf_tree` permits exactly one (D7). The bridge is where those meet.

| Policy | Behaviour |
|---|---|
| `FirstWriterWins` (**default**) | The first attributed publisher of an edge owns it. Later publishers' samples are dropped and counted, with a diagnostic naming **both** nodes and the edge. |
| `LastWriterWins` | Reclaim on each new publisher. Available, documented as chaotic, never the default. |
| `Strict` | Refuse to start if a conflict is detected within a startup window. For CI. |

The diagnostic must be loud, rate-limited, and surfaced in `tf_tree doctor`, because **this is the feature that finds pre-existing bugs in the host system** (§1, criterion 3). `tf2` interleaves competing publishers by timestamp and produces a transform that is a nonsensical blend of two authorities, silently. Being able to say "your `/ekf` and `/odom_node` have both been publishing `odom→base_link` for eight months" is a better sales pitch than any latency number.

### 5.5 Time domains and simulation

If `use_sim_time` is true, the bridge tags every edge it declares with the `SimTime` domain and drives its clock from `/clock`; otherwise `SystemTime`. Phase 1's typed domains then do the rest: a consumer querying with the wrong domain gets `TimeDomainMismatch` instead of a plausible wrong answer.

**NORMATIVE:** the bridge refuses to write to an edge whose declared domain differs from its own, and fails at startup rather than at first message. Sim and real transforms in one arena is a class of bug worth making impossible.

Also handle: `/clock` jumps backwards (bag loop, sim reset). On a detected backward jump beyond a threshold, the bridge **stops and reports** rather than pushing non-monotonic stamps that Phase 1 will reject one at a time. Offer `--on-clock-reset={halt,recreate}` where `recreate` builds a fresh arena instance.

### 5.6 Frame names

Strip a single leading `/` (ROS 1 legacy) and warn once per distinct frame. Reject empty names. Otherwise pass UTF-8 through unchanged — do not normalize case or Unicode, because frame names are identifiers and two frames differing only by case are two frames.

Apply `tf_prefix` remapping if configured, and log the resulting mapping table at startup. A silent remap is worse than no remap.

### 5.7 Static transform semantics

`/tf_static` messages arriving for an edge already declared static:

- **Identical value** (bitwise, or within 1e-12): idempotent, ignore silently. This is normal — late joiners re-receive latched messages.
- **Different value**: diagnostic naming both publishers and both values, then apply the authority policy. Two `robot_state_publisher` instances with different URDFs is a real and common misconfiguration.

A transform arriving on `/tf_static` for an edge already declared *dynamic* (or vice versa) is a hard error: the edge kind cannot change.

### 5.8 Deployment forms

Three, in increasing order of integration:

1. **Standalone node** `tf_tree_bridge`. Zero code changes to the host system.
2. **`rclcpp` component.** Composable into an existing container — and if composed alongside the tf broadcasters, intra-process communication means the bridge sees `TFMessage` without serialization at all.
3. **In-process library handle.** `tf_tree::ros::BridgeHandle handle(node, opts);` attached to an existing node. Lowest friction for a team that already has a node they can edit — which, for dogfooding, is us.

All three share one implementation; only the lifecycle wrapper differs.

### 5.9 Executor and backpressure

Dedicated `SingleThreadedExecutor` on its own thread, with optional CPU affinity and `SCHED_FIFO` priority (both off by default; document the `ulimit`/capability requirements). The bridge is the one component that still pays `tf2`'s deserialization cost, so it should be measured and isolated, not spread across a shared executor where it will be blamed for someone else's latency.

Track and expose: messages received, transforms applied, dropped by authority, dropped by non-monotonic stamp, and subscription queue depth. If the queue is persistently full, the bridge is the bottleneck and the operator needs to know that, not guess.

---

## 6. Test plan

### 6.1 C ABI

- Every function called with NULL handles, NULL out-pointers, zero-length arrays, and mismatched `struct_size` → correct status, no crash, no UB under ASan/UBSan.
- Double-free and use-after-free → debug-build abort with a named message.
- `tft_publisher` used from a second thread → debug abort.
- A forced panic in Rust (test-only hook) → `TFT_ERR_INTERNAL`, process survives, `tft_last_error()` carries the payload.
- ABI major/minor mismatch → rejected at init with both versions named.
- Byte-pattern assertions for all five layouts (§3.5).

### 6.2 C++ wrapper

- Compiles clean under `-Wall -Wextra -Wpedantic`, with and without `-fno-exceptions`, C++17 and C++20, GCC and Clang.
- `static_assert` suite for Eigen and Sophus layouts; the Sophus stride fallback exercised by forcing the assert off.
- Eigen batch write verified to touch exactly `n * sizeof(Isometry3d)` bytes (guard pages either side).
- ASan/UBSan across the whole wrapper test suite.

### 6.3 Bridge

- **QoS regression test:** a static broadcaster that publishes once and exits, with the bridge starting afterwards. The bridge must receive the transform. This test exists specifically to catch a `volatile` regression on `/tf_static`.
- Two publishers on one edge → `FirstWriterWins` applied, diagnostic names both nodes, counter increments.
- Sim time: `/clock` driven, domains tagged, cross-domain query rejected.
- Clock reset backwards → halt or recreate per policy, never non-monotonic pushes.
- Static conflict with differing values → diagnostic; identical values → silent.
- Frame name with leading `/` → stripped, warned once, not once per message.
- Bag replay at 10× real time → no drops, queue depth bounded.
- All three deployment forms produce byte-identical arena contents from the same bag.

### 6.4 End-to-end

Replay a recorded bag through the bridge, then compare `tf_tree` lookups against `tf2` lookups over the same query set using `LerpSlerp`, asserting agreement to 1e-12 — the Phase 1 differential test, now driven through the real ROS path. Any disagreement here is a bridge bug, since the core was already proven.

---

## 7. Benchmarks and the gate

| Benchmark | Report |
|---|---|
| `tft_plan_at` vs native Rust, depth 3 | ratio (target < 1.05) |
| `catch_unwind` overhead, happy path | ns delta |
| C++ `at<Eigen::Isometry3d>` vs C ABI | ns delta |
| Eigen batch write, n = 4096 | ns/sample vs native |
| Sophus strided write vs packed | ns/sample, both |
| Bridge CPU at 1 kHz × 20 edges | %CPU, and as a fraction of one `tf2` consumer |
| `at_with_derivatives` vs `at` | ratio (expect ~2×) |

**Gate:**

1. C ABI within **5%** of native for depth-3 lookup.
2. C++ wrapper within **2%** of the C ABI (it is inline code; anything more means it is not).
3. Bridge steady-state CPU **below one `tf2` consumer's** — the whole architectural claim is that this cost is paid once instead of N times.
4. Zero ASan/UBSan findings across the C and C++ suites.
5. §6.4 differential test passes to 1e-12.
6. §1's operational criteria met, including the surprise log.

---

## 8. Phase 5 handoff

- **Phase 5 requires a `FORMAT_VERSION` bump** (`PHASE5.md` §1). Do not add arena fields opportunistically during Phase 4 — collect anything you wish existed into the Phase 5 bump so the break happens exactly once.
- The bridge's authority conflicts, drop counters, and queue depth are Phase 5 `doctor` inputs. Emit them in a structured form now rather than as log lines.
- Carry the surprise log (§1.2) directly into `docs/PHASE7.md` as the shim's requirements list.

---

## 9. Definition of done

- [ ] `sample_with_derivatives` shipped, with the composition-identity proptests and the `DerivativesUnavailable` refusal for LerpSlerp
- [ ] `tf_tree.h` frozen and reviewed by hand — not merely `cbindgen` output — with every entry justified
- [ ] `tf_tree_unstable.h` gated behind `TFT_ENABLE_UNSTABLE`
- [ ] `catch_unwind` on every `extern "C"` boundary, tested by a forced panic
- [ ] All five layouts byte-pattern tested; `QVEC7_XYZW` documented as the Eigen/Sophus path
- [ ] `static_assert` guards for Eigen and Sophus with a working fallback
- [ ] `find_package(tf_tree)` works from a clean `colcon` workspace with no manual cargo step
- [ ] Prebuilt static libs for `x86_64` and `aarch64` published alongside the source path
- [ ] Bridge QoS regression test present and passing
- [ ] Multi-publisher detection verified against a deliberately broken launch file
- [ ] §7 gate met, or a written explanation of which criterion failed and by how much
- [ ] §1 operational criteria met, including the surprise log committed to the repository
