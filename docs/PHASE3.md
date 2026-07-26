# tf_tree — Phase 3 Implementation Specification: Python Bindings

> **Companion documents:** `docs/PROJECT.md` (vision, roadmap, decision log), `docs/PHASE1.md` (core), `docs/PHASE2.md` (shared memory). §14 of `PHASE2.md` listed three handoff constraints; **§1 below corrects one of them** and adds the constraint that turned out to matter most.

**Deliverable:** a Python package where `import tf_tree; tree = tf_tree.open()` joins the robot's transform tree, batch lookups run at near-native speed with no intermediate copies, and the module is correct and parallel on free-threaded CPython.

**Framing.** Python is where this library's performance argument is most visible — `tf2_ros`'s Python path costs tens of microseconds per lookup and the ML and perception people writing Python are the ones who will advocate for adoption. It is also where the binding can most easily throw the performance away: a per-call overhead of 60 ns against a 150 ns lookup is a 40% tax, and a single missing declaration can silently disable free-threading for a user's entire process. **This phase is a budget-management exercise, and every design choice below is anchored to a measured number** (Appendix A).

Sections marked **NORMATIVE** are requirements.

---

## 0. Scope

### In scope

| | |
|---|---|
| PyO3 bindings | direct to the Rust core, not through the Phase 4 C ABI |
| Vectorized lookup | NumPy in, NumPy out, no intermediate allocation |
| Caller-owned output | `at_into` writing directly into user memory, including pinned/device memory |
| GIL discipline | measured, threshold computed from plan depth × batch size |
| Free-threading | `Py_MOD_GIL_NOT_USED`, `Sync` pyclasses, TSan CI |
| Interop | DLPack for device classification, buffer protocol for acquisition (§5.5) |
| Lifecycle | fork poisoning, interpreter shutdown, context managers |
| Typing | hand-written `.pyi`, `py.typed`, strict mypy and pyright |
| Distribution | manylinux/musllinux/macOS/Windows × abi3 + free-threaded wheels |

### Out of scope — NORMATIVE

| Excluded | Why |
|---|---|
| `asyncio` integration | A lookup is ~200 ns. There is nothing to await. Adding it would imply the operation is slow. |
| ROS 2 / `rclpy` | Phase 4. A `tf_tree.ros` submodule with lazy import is Phase 4's deliverable, not this one. |
| Going through the C ABI | Phase 4 freezes a C ABI for C++. Python binds Rust directly — through C we would lose typed errors, zero-copy buffers, and `Drop` ordering. Two FFI boundaries is the correct cost. |
| Any view into the arena | §5.1. Non-negotiable and worth understanding before writing code. |
| CUDA / CuPy dependency | D8. The user allocates device memory; we write into it. |
| Reimplementing logic in Python | The binding is a thin shell. Anything with a branch in it belongs in Rust where it is tested. |
| `pickle` support for `Tree`, `Plan`, `Publisher` | A live mapping cannot be serialized. Raise `TypeError` with a message pointing at `open()`. |

---

## 1. What 2026 forces — and a correction to the Phase 2 handoff

`PHASE2.md` §14 said "abi3 wheels via maturin." **That is now incomplete in a way that would have shipped a package unusable by a growing fraction of users**, and the constraint it missed is more dangerous than the one it stated.

### 1.1 Free-threaded CPython is supported, and abi3 does not cover it

- Python 3.13 shipped the free-threaded build as experimental; **Python 3.14 (October 2025) promoted it to officially supported** under PEP 779 — phase II: supported, still opt-in, shipped as a separate `python3.14t` binary. The single-threaded penalty is now roughly 5–10%.
- **`abi3` does not work on free-threaded builds.** PyO3 prints a warning and ignores the setting; an `abi3` wheel is rejected by a free-threaded interpreter outright.
- **PEP 803 ("abi3t") was approved by the Steering Council in 2026**, defining a stable ABI valid on both GIL-enabled and free-threaded builds from **Python 3.15 onward**. As of 2026-07 the toolchain has caught up with the PEP: **PyO3 0.29.0** ships the `abi3t` and `abi3t-py315` features (plus a new `abi3-py315`; `abi3-py37` is gone), and **maturin 1.14.0** builds them, with 1.14.1 fixing the abi3/abi3t interaction. The only thing still missing is CPython 3.15 itself.
- **maturin emits at most one stable-ABI family per invocation** (PyO3/maturin#3226). Selection happens *after* interpreter resolution: `abi3t` when a CPython ≥ 3.15 interpreter is present, otherwise `abi3`; an interpreter that does not support the chosen family falls back to a version-specific wheel. `abi3` and `abi3.abi3t` therefore cannot come out of the same build. §10 needs **two maturin invocations per platform**, not one job with an extra flag — and building the `abi3.abi3t` wheel is structurally impossible before 3.15, so there is no way to get it wrong quietly.
- **PyO3 0.29 refuses to build for `3.13t`**, manylinux has dropped 3.13t from its images, and maturin's `--find-interpreters` deliberately picks up 3.14t and newer only (PyO3/maturin#3206). A `cp313t` wheel is not buildable on the toolchain this phase pins. That is the right outcome and not a loss: 3.13t was PEP 703's experimental phase, and free-threading only became *supported* in 3.14 under PEP 779.
- Phase III (free-threaded as the *default*) has no PEP and no timeline.

**Consequence for the build matrix (§10):** ship `abi3` for GIL builds *plus* version-specific `cp314t` wheels, from **two separate maturin invocations**, and add the third invocation the week 3.15 ships — everything below CPython is already ready for it. Do not plan around a single wheel, or a single build, per platform.

### 1.2 The declaration that matters more — NORMATIVE

**If an extension module does not declare itself free-threading-safe, importing it silently re-enables the GIL for the entire process.** No error, no warning to the application, no failed import — the user's threads keep running and simply stop running in parallel.

For this library that failure is particularly bad: a perception application on free-threaded Python imports `tf_tree` to go faster and instead loses all parallelism everywhere, with the regression attributable to nothing.

```rust
#[pymodule(gil_used = false)]      // -> Py_mod_gil = Py_MOD_GIL_NOT_USED
fn tf_tree(m: &Bound<'_, PyModule>) -> PyResult<()> { ... }
```

**CI must assert this**, not review it:

```python
# runs only on a free-threaded interpreter
import sys, tf_tree
assert not sys._is_gil_enabled(), "importing tf_tree re-enabled the GIL"
```

Declaring `gil_used = false` is a promise, and §7 is what makes the promise true.

---

## 2. Measured budgets — the numbers that drive the API

Measured on CPython 3.12, x86-64, `-O2` (Appendix A has the reproduction). Everything in §3–§6 follows from this table.

| Operation | Cost |
|---|---|
| Pure-Python function call | 20.8 ns |
| Bare C call, `METH_VARARGS`, no args | 22.6 ns |
| One `i64` arg via `PyArg_ParseTuple` | **60.1 ns** |
| One `i64` arg via `METH_FASTCALL` | **31.2 ns** |
| GIL release + reacquire around nothing | **+40.4 ns** |
| Batch call fixed overhead (2 buffers acquired) | ~220 ns |
| Batch marginal cost | 1.7–2.5 ns/sample |
| `np.empty((n,4,4))` | ~270 ns, **flat** to n = 65 536 |
| `np.zeros((4096,4,4))` | 11 344 ns — never use |
| Phase 1 target: native depth-3 lookup | 150 ns |

Four conclusions, each of which becomes a NORMATIVE rule:

1. **Argument parsing costs as much as the lookup.** `PyArg_ParseTuple` is 29 ns more expensive than `METH_FASTCALL` — roughly 20% of a depth-3 budget for parsing one integer. Hot-path methods take **positional-only arguments** (§4.2).
2. **Releasing the GIL costs 40 ns**, which is 27% of a single lookup. Do not release for scalar lookups; do release for batches (§6).
3. **Allocation is a flat ~270 ns.** Irrelevant for a 65 536-sample batch, but ~50% of the call for a 64-sample control loop. That, precisely, is what `at_into` is for (§5.2) — not the big-batch case people assume.
4. **A Python scalar lookup floors at ~180–200 ns** (31 ns call + ~150 ns work). Against `tf2_ros`'s Python path this is a two-to-three-order-of-magnitude difference, and it is the number to publish.

---

## 3. Time is integer nanoseconds — NORMATIVE

**`float` timestamps are rejected. There is no conversion, no convenience overload, and no "seconds" keyword.**

This will be the most-questioned decision in the API, because `rospy` and much of `rclpy` hand people float seconds. Answer it with the measurement, which is in the docstring of the exception:

At a 2026 Unix epoch (~1.75 × 10¹⁸ ns), the ULP of `float64` seconds is **238 ns**. Converting a 1 kHz stamp sequence to float seconds and back:

```
epoch ns            : 1753400000123456789
via float seconds   : 1753400000123456768        error: 21 ns
ulp at that magnitude                            238 ns
1 kHz stamps with the wrong spacing after a round trip:  1999 / 1999
```

**Every single consecutive-sample interval in a 1 kHz stream is wrong after a float round trip.** For a library whose entire purpose is sub-millisecond temporal accuracy, silently accepting that input would produce interpolation errors users would blame on the interpolator.

Accepted stamp types: Python `int`, `np.int64` scalar, and C-contiguous `np.int64` arrays. Explicit converters exist and are the only path from wall-clock types:

```python
tf_tree.from_sec(1753400000.5)          # -> int ns, documented as lossy above ~10^7 s
tf_tree.from_datetime(dt)               # exact; requires tz-aware
tf_tree.now(domain="steady")            # CLOCK_MONOTONIC, int ns
```

Passing a `float` raises `TypeError` naming `from_sec` and stating the ULP at the caller's magnitude.

---

## 4. API

### 4.1 Discovery mirrors Phase 2 exactly

```python
import tf_tree

tree = tf_tree.open()                                 # join or create, zero config
tree = tf_tree.open(name="robot", domain=7,
                    mode="ro", create="never")        # explicit
with tf_tree.open() as tree: ...                      # context manager, explicit detach
```

**NORMATIVE defaults:** `mode="ro"` and `create="never"`.

Both differ from the Rust defaults, deliberately. Most Python consumers are notebooks, analysis scripts, and visualization tools; they must be incapable of corrupting a robot's transform tree (Phase 2 §8), and a notebook started before the robot must fail loudly rather than create an empty arena that the real publisher then refuses to join. A Python process that wants to publish says so:

```python
tree = tf_tree.open(mode="rw", create="if_absent")
```

`tf_tree.has_shared_memory()` reports whether the platform build includes the IPC layer (§10).

### 4.2 Lookup — vectorized first, positional-only

```python
plan = tree.plan("map", "camera_optical")     # compile once

T  = plan.at(stamp_ns)                        # int   -> (4,4)   f64
Ts = plan.at(stamps)                          # (N,)  -> (N,4,4) f64
plan.at_into(stamps, out)                     # writes in place, allocates nothing

Ts = plan.at(stamps, layout="quat")           # (N,7)  f64  [qw qx qy qz tx ty tz]
Ts = plan.at(stamps, layout="affine32")       # (N,12) f32  row-major 3x4, GPU-ready

T = plan.latest()
T = plan.latest_common()

knots, poses = plan.adaptive(t0, t1, lin=1e-3, ang=1e-4)
plan.adaptive_into(t0, t1, out_knots, out_poses, lin=1e-3, ang=1e-4)

T = tree.lookup("map", "camera_optical", stamp_ns)    # plan-cached convenience
```

**NORMATIVE:** `at`, `at_into`, `latest`, and `push` take **positional-only** arguments (`def at(self, stamps, /)`), because the measured `METH_FASTCALL` difference is 29 ns — 20% of the depth-3 budget. `layout=` is accepted as a keyword only on the non-hot overload; if that measurably costs, add `at_quat` / `at_affine32` as separate positional-only methods. **Verify that PyO3 actually emits `METH_FASTCALL` for these signatures** rather than assuming it; if it does not, that is 29 ns and worth a hand-written shim.

Keyword arguments are fine everywhere else — `open`, `plan`, `declare_*`, `adaptive` — all of which run at startup.

Scalar-vs-array dispatch on the same method is the NumPy idiom and is what makes the vectorized path the *obvious* path. That matters more than any single optimization here: a user who writes a Python loop over `tree.lookup` gets ~200 ns per iteration; the same work through `plan.at(stamps)` amortizes to near-native. **Make the fast path the one that reads naturally.**

### 4.3 Publishing

```python
with tree.publisher("odom", "base_link") as pub:      # claims on enter, releases on exit
    pub.push(stamp_ns, T)                             # T: (4,4) or (7,)
    pub.push_many(stamps, poses)                      # vectorized

tree.declare_static("base_link", "camera_mount", T)
tree.declare_dynamic("odom", "base_link", capacity=8192, interp="sclerp")
```

The context manager is the documented form. A `Publisher` that is garbage-collected without `__exit__` still releases its claim via `Drop`, but Python finalization order is not guaranteed and the explicit form is what the docs show.

### 4.4 Errors

Rust's typed errors map to an exception hierarchy carrying **structured attributes**, not just messages, so users can program against them:

```python
class TfTreeError(Exception): ...
class ExtrapolationError(TfTreeError):      # .edge, .requested, .oldest, .newest
class DisconnectedError(TfTreeError):       # .target, .source, .cut_at
class NoDataError(TfTreeError):             # .edge
class TopologyChangedError(TfTreeError):    # .plan_generation, .current_generation
class TimeDomainMismatchError(TfTreeError): # .expected, .got
class EdgeAlreadyClaimedError(TfTreeError): # .edge, .owner_pid
class ClaimRevokedError(TfTreeError):       # .edge
class ArenaHeldButUnreachableError(TfTreeError):   # .holders  -> [(pid, name), ...]
class ChildProcessDetachedError(TfTreeError):      # §8.1
class FrameNotDeclaredError(TfTreeError, KeyError)
```

`str(e)` uses the Rust `Described` wrapper so frame and edge IDs appear as names. `TopologyChangedError` must document that the correct response is to re-`plan`, since it is the one error a correct program routinely hits.

---

## 5. Zero-copy — precisely what it does and does not mean

### 5.1 There are no views into the arena — NORMATIVE

**The library never hands Python a buffer that aliases arena memory.** Not for poses, not for stamps, not with a read-only flag, not for debugging.

The reason is structural, not conservative: an edge's sample storage is a ring buffer being overwritten by another process, and correct reads go through the Phase 1 seqlock protocol. A NumPy array pointing into it would bypass that protocol entirely — a data race by construction, producing torn poses that appear as occasional impossible transforms with no way to detect them. It would also pin the arena mapping for the array's lifetime, which in a notebook is forever.

So "zero-copy" here means: **no intermediate allocation and no copy between the interpolation kernel and the caller's destination buffer.** Results are computed by interpolation — there is no pre-existing array to view — and they are written exactly once, directly into their final home. State this in the README in those words; users who have read the phrase elsewhere will otherwise assume they can get a window onto shared memory, and be right to be annoyed when they cannot.

### 5.2 Three tiers

| Tier | Allocations | Copies | When |
|---|---|---|---|
| `plan.at(stamps)` | 1 (~270 ns) | 0 | one-shot, exploratory, large batches |
| `plan.at_into(stamps, out)` | 0 | 0 | steady-state loops — the 270 ns is ~50% of a 64-sample call |
| `plan.at_into(stamps, device_out)` | 0 | 0 | `out` is pinned or device memory the caller allocated |

Tier 3 needs no CUDA in our dependency tree: the caller allocates pinned host memory (`torch.empty(..., pin_memory=True)`, `cupyx.empty_pinned`, `numba.cuda.pinned_array`) and we write into it, after which their async copy or kernel reads it at DMA bandwidth.

**Tier 3 is a convenience, not a performance necessity, and §5.5 explains why.** An earlier draft of this section said `out` could be "pinned or device memory" — that is wrong for discrete GPUs, where a CPU store to `cudaMalloc` memory is undefined. §5.5 is the corrected rule.

### 5.3 Output buffer validation — NORMATIVE

Validate **fully, before writing anything**. A half-written output array on a validation failure is the worst possible outcome, because the user sees plausible garbage.

Required checks: dtype exactly matches the layout, shape matches `(N, ...)` for the given layout, the array is **C-contiguous** and writable, and `N` matches the stamp count.

**Non-contiguous or strided `out` is rejected, never silently copied.** A silent copy would defeat the entire purpose of the method while appearing to work — the user would ship it and wonder why their profile did not change.

### 5.4 Export: implement nothing — NORMATIVE

Outputs are plain `numpy.ndarray`, so `__dlpack__`, `__dlpack_device__`, `__array_interface__`, and the buffer protocol arrive for free (verified: `np.from_dlpack` round-trips sharing memory, and NumPy 2.4 correctly propagates the read-only flag through the versioned capsule).

**Do not hand-roll a DLPack exporter.** The capsule ownership protocol — renaming `"dltensor"` to `"used_dltensor"` so the producer's destructor does not double-free — is subtle, it is `unsafe`, and NumPy already implements it correctly. Writing our own would add unsafe code and a class of double-free bugs in exchange for nothing.

Add an interop CI job that consumes an `adaptive()` result from torch, JAX, and CuPy. That job's purpose is catching a dtype or stride regression, not proving DLPack works.

### 5.5 Accepting `out`: DLPack classifies, the buffer protocol carries writability — NORMATIVE

The `out` parameter is the only place an interchange protocol earns its keep, and the two properties that decide the design are **device placement** and **mutability**.

**Device placement — DLPack is the only portable source of it.** A CPU kernel cannot write to discrete-GPU device memory; a store to a `cudaMalloc` pointer is undefined. We need to know what kind of memory we were handed, and `__dlpack_device__()` returns `(device_type, device_id)` cheaply, without consuming the object and without a CUDA runtime. Nothing else gives us that portably — `cudaPointerGetAttributes` would need a CUDA dependency, which D8 forbids.

```
accept:  kDLCPU (1), kDLCUDAHost (3), kDLCUDAManaged (13), kDLROCMHost (11)
reject:  everything else, naming the device type and suggesting a pinned allocator
```

In practice almost every host-accessible buffer reports `kDLCPU` — PyTorch treats pinned tensors as `device='cpu'`, and `cupyx.empty_pinned` returns a NumPy array. **So this check's real job is producing a good error instead of a segfault**, not enabling an exotic path. That is worth the twenty lines on its own.

**Mutability — the buffer protocol is the reliable carrier.** Measured on NumPy 2.4: a read-only array exported and re-imported through DLPack stays read-only, and the versioned capsule (`dltensor_versioned`, requested via `__dlpack__(max_version=(1,0))`) carries the flag. But that is a property of *this producer's* DLPack version. Older DLPack had no read-only bit, so a producer could hand us a tensor that looks writable and is not. The buffer protocol has conveyed writability since PEP 3118 and cannot get this wrong.

**NORMATIVE acquisition order for `out`:**

1. If the object exposes `__dlpack_device__`, call it and apply the whitelist above. Reject non-host memory here, with the good message.
2. Acquire the pointer through the **buffer protocol** (`PyBUF_WRITABLE | PyBUF_C_CONTIGUOUS`). This validates writability, contiguity, and itemsize in one call, and it is the same mechanism that keeps the buffer alive across the GIL release (§6.2).
3. Only if the object has no buffer protocol support but does offer a versioned DLPack capsule, fall back to `np.from_dlpack()` and take the pointer from the resulting array — **never hand-parse the capsule.**
4. Otherwise raise, naming both protocols.

**Drop `__cuda_array_interface__` entirely.** An earlier draft listed it. Since device memory is rejected anyway, its only remaining role would be pinned buffers that expose CAI but not DLPack — a set that is empty in practice. It is CUDA-only, gives strictly less information than DLPack, and would be dead code.

**Stream synchronization is the caller's responsibility.** We accept only host-accessible memory and we have no CUDA runtime to synchronize against. Pass `stream=None`; document that a caller handing us a buffer a GPU kernel recently touched must synchronize first.

### 5.6 Why the device story is small — and why that is the design working

By D8, the engine's product is a *bounded-error knot array*: an adaptive sample of a 100 ms sweep at 1 cm / 1e-4 rad tolerance is tens of knots, roughly 1 KB, about 6 µs over PCIe. Pinned memory saves microseconds on a transfer that is already negligible.

State this plainly in the docs rather than marketing a zero-copy-to-GPU pipeline. **If the outputs were large enough for the device path to be a performance necessity, that would be evidence D8 was wrong and we were emitting far too much data.** The smallness is the point. Tier 3 exists so that a pipeline already living on the GPU does not have to bounce through an extra host buffer — an ergonomic win, not a throughput one.

### 5.7 Not Arrow, and not in the C ABI

**Arrow** solves columnar data with nullability, variable-length types, and schema negotiation. Our payload is dense, fixed-shape, non-nullable `f64`/`f32`. All of Arrow's machinery would be overhead, and no consumer in this domain expects it.

**DLPack does not belong in the Phase 4 C ABI.** It is a Python-ecosystem convention. A C++ robotics user wants a pointer, a count, and a layout enum — or an `Eigen::Isometry3d`. Putting `DLManagedTensor*` in a C robotics API would be surprising, would drag in a header dependency, and would buy nothing on a boundary where both sides already agree on memory ownership. Carry this into `docs/PHASE4.md`.

---

## 6. GIL discipline

### 6.1 The threshold is computed, not constant — NORMATIVE

Releasing the GIL costs a measured 40 ns. A depth-3 lookup costs ~150 ns. So:

- **Never release for a scalar lookup.** It would add 27% for parallelism nobody can use in a 150 ns window.
- **Always release when the work is long enough that holding it would stall other threads.**

The rule is expressed in work, not in element count, because depth varies:

```rust
const GIL_RELEASE_THRESHOLD_NS: u64 = 1_000;
const NS_PER_STEP_ESTIMATE: u64 = 55;

let est = n as u64 * plan.depth() as u64 * NS_PER_STEP_ESTIMATE;
if est >= GIL_RELEASE_THRESHOLD_NS { py.allow_threads(|| kernel()) } else { kernel() }
```

For depth 3 this releases from about `n = 6`. The worst case where we do *not* release is under 1 µs of GIL retention — far below CPython's 5 ms switch interval, so no other thread notices. The worst case where we do release is a 4% overhead. Both sides of the threshold are cheap, which is why the exact constant does not need tuning; **what matters is that neither branch is ever badly wrong.**

Publish the constants and add a benchmark row proving the crossover behaves as predicted.

### 6.2 Rules while the GIL is released — NORMATIVE

1. **Touch no Python object.** Extract raw pointers and lengths *before* `allow_threads` and use only plain Rust data inside.
2. **Hold the buffer view across the release.** Acquire the `Py_buffer` (via `numpy`'s `PyReadwriteArray` or the raw buffer protocol) before releasing and keep it alive for the whole call. NumPy refuses to resize an array while a buffer is exported; that refusal is what makes the pointer valid, so **add a test that asserts the resize actually fails** rather than trusting it.
3. On free-threaded builds `allow_threads` is cheaper but not free, and the same rules apply — the thread state still detaches.

---

## 7. Free-threaded correctness

This is what makes §1.2's declaration honest.

### 7.1 Every `#[pyclass]` must be `Send + Sync` — NORMATIVE

| Type | Rust | Python wrapper |
|---|---|---|
| `Tree` | `Send + Sync` | direct |
| `Plan` | `Send + Sync + Copy` | direct, `#[pyclass(frozen)]` |
| `Guard` | borrows `Tree` | not exposed; taken internally per call |
| `Publisher` | **`Send + !Sync`** | `PyPublisher(Mutex<Publisher>)` |

`Publisher` is `!Sync` by design (Phase 1: single-writer as a type-level property), so it cannot be a pyclass directly. Wrap it in a mutex: an uncontended lock is ~15 ns against a `push` that already costs more than that, and the semantics are exactly right — two Python threads pushing to one edge serialize, which is the same guarantee the type system gives in Rust, enforced at a different level.

### 7.2 No global mutable state — NORMATIVE

No `static mut`, no `once_cell` singletons holding Python objects, no process-global caches. Everything lives in module state or in the `Tree`. This is required for free-threading and it is also what makes PEP 734 sub-interpreters possible later; support for those is best-effort in this phase but the constraint costs nothing to honour now and is expensive to retrofit.

The per-thread plan cache behind `tree.lookup` must be genuinely per-thread (`thread_local!`), not a shared map behind a lock — a shared cache would turn the convenience API into a contention point on exactly the workload free-threading exists to serve.

### 7.3 Required CI

- The full test suite on both `3.14` and `3.14t`.
- The `sys._is_gil_enabled()` assertion from §1.2.
- A scaling test: 1/2/4/8 threads calling `plan.at` on a shared `Tree`, asserting near-linear aggregate throughput on `3.14t` — this is the claim the phase exists to make.
- **ThreadSanitizer** on the free-threaded build. TSan finding nothing is the evidence behind `gil_used = false`; without it that declaration is an assertion.

---

## 8. Process and interpreter lifecycle

### 8.1 Fork — NORMATIVE

Phase 2 applies `MADV_DONTFORK` to the arena, so **a forked child has no mapping and any inherited handle is a segfault waiting to happen.** Phase 2 also holds claims as OFD locks, which *are* inherited across fork — so without intervention a child both appears to hold the claim and has no memory to write to.

Python 3.14 changed the default `multiprocessing` start method on POSIX from `fork` to `forkserver`, and `fork` is no longer the default on any platform — which reduces the hazard but does not remove it: 3.9–3.13 still default to `fork`, and `mp.get_context("fork")` and bare `os.fork()` remain entirely ordinary things to write.

```python
os.register_at_fork(after_in_child=_poison_all_handles)
```

Poisoning marks every `Tree`, `Plan`, and `Publisher` in the child dead; any use raises `ChildProcessDetachedError` with a message saying to call `tf_tree.open()` in the child. **A clear Python exception instead of a segfault** is the whole deliverable here, and it must be tested with `pytest-forked` under all three start methods.

### 8.2 Interpreter shutdown

Explicit `close()` and context-manager support are the documented path. An `atexit` hook detaches anything still open.

Then state the reassuring part plainly, because it is a genuine payoff from Phase 2: **Python's finalization is unreliable, and it does not matter.** If the interpreter is killed, hangs in `atexit`, or leaks the handle entirely, the kernel releases the OFD locks and closes the socket, and the arena's crash-consistency handles the rest. Python is the least trustworthy participant in the system and the design already accounts for it — which is exactly why the arena was built the way it was.

---

## 9. Typing, docs, ergonomics

- `py.typed` marker plus **hand-written `.pyi` stubs**. Generated stubs cannot express the scalar-vs-array return overloads, which are the most important thing for users to see.
- **But generate them too, and diff.** maturin 1.14 generates stubs for mixed PyO3 projects (PyO3/maturin#3211), which this is. The standing hazard with hand-written stubs is not that they are wrong on day one — it is that a method added in Rust never reaches them. So make CI generate the stubs and assert that every public symbol in the generated set appears in the hand-written `.pyi`. Signatures are ours; *existence* is checkable, and that is the half that rots.
- `@overload` for `at(int) -> NDArray[(4,4)]` versus `at(NDArray[int64]) -> NDArray[(N,4,4)]`, and `Literal["mat4","quat","affine32"]` for `layout`.
- `mypy --strict` and `pyright --strict` in CI over the stubs *and* over the example code — examples that do not typecheck are a documentation bug.
- Docstrings carry the measured numbers where they explain a design (the float-seconds ULP, the GIL threshold). Users argue with rules and accept measurements.
- `__repr__` on `Tree` shows domain, name, mode, participant count, and `instance_uuid`; on `Plan`, the frame names and folded depth.
- Doctests in CI.

---

## 10. Build and distribution matrix

| Platform | Shared memory | ABI targets |
|---|---|---|
| `manylinux_2_28` x86-64 | yes | `abi3-py39`, `cp314t` |
| `manylinux_2_28` aarch64 (Jetson) | yes | same |
| `musllinux_1_2` x86-64, aarch64 | yes | same |
| macOS arm64 / x86-64 | **no** — in-process only | same |
| Windows x86-64 | **no** — in-process only | same |

Each row is **two maturin invocations**, for the reason in §1.1: one against a GIL interpreter ≤ 3.14 (`--features pyo3/abi3-py39`) producing the `abi3` wheel, one against `python3.14t` producing `cp314-cp314t`. A third — `abi3.abi3t`, against a 3.15+ interpreter — is added when 3.15 ships and then *replaces* both on interpreters that can use it. `cp313t` is absent deliberately (§1.1), not by omission.

Shipping macOS and Windows wheels without the IPC layer is deliberate: developer laptops are where adoption starts, and a library that cannot be imported on a Mac will not be evaluated. `tf_tree.has_shared_memory()` reports the truth at runtime, and `open()` on those platforms transparently gives an in-process `HeapArena` tree with a documented one-process limitation.

Additional requirements:

- **maturin** with `cibuildwheel` or `maturin-action`. Cross-compile aarch64; do not require a Jetson in CI.
- **Add the `abi3.abi3t` job now, skipped**, with a comment referencing PEP 803. Its only unmet precondition is a 3.15 interpreter — PyO3 0.29 and maturin 1.14.1 already do their halves — so having the job written is the difference between a same-week and a same-quarter response.
- **Let `--find-interpreters` discover `3.14t`** rather than hand-listing interpreter paths; before maturin 1.14 it missed free-threaded interpreters on Windows (PyO3/maturin#3206), which is exactly the row where a hand-written path is most likely to be wrong.
- PEP 740 attestations, an SBOM, and reproducible builds. These are table stakes for an industrial integrator's supply-chain review and are cheap to set up before the first release, expensive after. Attestations are produced by the *upload* step under Trusted Publishing, not by `maturin build` — maturin 1.14.1 adopting them for its own releases is precedent for the workflow shape, not a feature we inherit.
- The Rust `shm` feature gates the entire IPC layer so the macOS and Windows builds compile it out rather than stubbing it at runtime.

### 10.1 Toolchain floors — NORMATIVE

| Tool | Floor | Why this floor and not an earlier one |
|---|---|---|
| PyO3 | `0.29` | `abi3t` / `abi3t-py315` features; the free-threaded ABI story (§1.1) does not exist below it |
| maturin | `1.14.1` | builds abi3t (PyO3/maturin#3113) and gets the abi3/abi3t interaction right (PyO3/maturin#3226); 1.14.0 also fixed `maturin develop` truncating the editable ELF via stale hardlinks (PyO3/maturin#3199), a dev-loop footgun rather than a release one |
| pytest | `9.1` | see the doctest interaction below |
| ruff | `0.16` | see the Markdown interaction below |
| pyright | `1.1.411` | current; `--strict` behaviour is what §9 is written against |

Two upgrades in this set change *default* behaviour, and both intersect something this spec already asks for:

- **ruff 0.16 formats Python code blocks inside Markdown, by default.** `docs/` is this project's contract, and its fenced blocks are written to be read — line-broken to make an argument, sometimes elided with `...`. Set `[tool.ruff.format] exclude` over `**/*.md` before running the formatter for the first time. The same release raised the default rule set from 59 rules to 413; that one is a non-event *provided* `[tool.ruff.lint] select` stays explicit, which it must.
- **pytest 9.1 changed `--doctest-modules` fixture visibility**: a module-, package-, or session-scoped autouse fixture defined inline in a test module can now run twice, because the `Module` and the `DoctestModule` register fixtures independently. §9 requires doctests in CI, so put every autouse fixture in `conftest.py` from the start. Also: `parametrize` `argvalues` must be a `Collection` — a generator is deprecated and silently yields skipped tests on a second collection, which the Hypothesis and sweep tests in §11 are otherwise well-placed to hit.

---

## 11. Test plan

### 11.1 Correctness across the boundary

- Hypothesis property tests mirroring the Rust proptests where the boundary can break them: `at(t)` scalar equals `at([t])[0]` **bit-exactly**; every `layout` yields the same transform; `at_into` equals `at`; endpoint stamps return stored poses exactly.
- Differential test against the Rust CLI over a recorded MCAP session (Phase 2 §10), asserting bit-identical `f64`.
- Every error type raised at least once with its attributes asserted.

### 11.2 Buffer safety

- Resize an array while it is exported → must fail (this is what §6.2 relies on).
- `out` with wrong dtype, wrong shape, non-contiguous, read-only, or mismatched `N` → raises **before any element is written** (assert the array is unmodified).
- `out` on a CUDA device (`torch.empty(..., device="cuda")`, `cupy.empty(...)`) → raises naming the device type and suggesting a pinned allocator. **Never** attempt the write.
- `out` as a pinned host buffer from torch, CuPy, and Numba → accepted, written correctly, and the device reads back the expected values.
- A read-only array round-tripped through DLPack must still be rejected as `out`.
- Reference-count and `tracemalloc` leak tests over 10⁶ calls.
- A stamp array mutated from another thread during a released-GIL batch: results must be *some* valid transforms, never a crash.

### 11.3 Concurrency

- §7.3 in full: both interpreters, GIL assertion, thread scaling, TSan.
- Two threads pushing to one `Publisher`: serialized, no corruption.
- Two Python processes sharing an arena via Phase 2, one publishing and one reading.

### 11.4 Lifecycle

- `pytest-forked` across `fork`, `forkserver`, and `spawn`; the `fork` child must raise `ChildProcessDetachedError`, never segfault.
- Interpreter killed with `SIGKILL` while holding a claim → another process can claim the edge (this exercises Phase 2's reaping from the least trustworthy participant).
- `open()` / `close()` cycled 10⁴ times without leaking participant slots.

---

## 12. Benchmarks and the gate

### 12.1 Measurements

| Benchmark | Report |
|---|---|
| scalar `plan.at(t)`, depth 3 | p50, p99 |
| `plan.at(stamps)` at n = 1, 8, 64, 512, 4096, 65536 | ns/sample |
| `at_into` vs `at` at the same n | delta (expect ~270 ns fixed) |
| GIL crossover sweep around the §6.1 threshold | ns/sample either side |
| thread scaling 1→8, GIL build and `3.14t` | aggregate throughput |
| `tree.lookup` (plan-cached) vs explicit `plan.at` | ratio |
| **vs `tf2_ros` Python, same tree, same queries** | ratio |
| import time, wheel size | ms, MB |

### 12.2 The gate — NORMATIVE

1. **Scalar `plan.at` p50 under 250 ns** (≈ 31 ns call + 150 ns work + margin).
2. **`at_many` at n = 4096 within 1.3× of native per-sample cost.** This is the central claim: batch work through Python is essentially free.
3. **`at_into` eliminates the full ~270 ns allocation**, visible at n = 64.
4. **Thread scaling ≥ 6× from 1 to 8 threads on `3.14t`**, and ≥ 6× on the GIL build for batches above the release threshold.
5. **`import tf_tree` does not re-enable the GIL**, asserted in CI.
6. **TSan clean** on the free-threaded build.
7. Zero leaks over 10⁶ calls.

Criteria 4–6 are the ones that make this a 2026 binding rather than a 2019 one. Criterion 2 is the one to put in the README.

---

## 13. Phase 4 handoff

- **Do not route Python through the C ABI** when Phase 4 builds it. Two FFI surfaces is the correct cost; collapsing them would cost typed errors, zero-copy buffers, and `Drop` ordering.
- The `tf_tree.ros` submodule (lazy `rclpy` import, `builtin_interfaces.msg.Time` and `TransformStamped` conversion, `tf2_ros.Buffer`-shaped adapter) is Phase 4's, but design its stamp conversion against §3 now — **it must convert through integer nanoseconds**, never through `Time.to_sec()`.
- Carry §12's measured `tf2_ros` ratio into Phase 4's docs. The Python comparison is the most dramatic number this project will produce, and Phase 4 is where ROS users encounter it.

---

## 14. Definition of done

- [ ] `import tf_tree; tf_tree.open()` works with zero arguments on a machine with a running arena, and in a bare notebook with none
- [ ] `#[pymodule(gil_used = false)]` set and asserted by CI on `3.14t`
- [ ] Every `#[pyclass]` is `Send + Sync`; `Publisher` wrapped
- [ ] No `float` stamp accepted anywhere; `TypeError` names `from_sec` and states the ULP
- [ ] No API returns a view into the arena (grep-able review item, documented in the README)
- [ ] `at_into` validates fully before writing; non-contiguous `out` rejected
- [ ] `out` device classification via `__dlpack_device__`; CUDA device memory rejected with an actionable message
- [ ] No hand-written DLPack capsule parsing anywhere in the codebase
- [ ] `os.register_at_fork` poisoning tested under all three start methods
- [ ] Hand-written stubs; `mypy --strict` and `pyright --strict` clean over stubs and examples
- [ ] CI asserts no public symbol exists in the generated stubs but not the hand-written ones (§9)
- [ ] Wheels for every row of §10 — two invocations each — with the `abi3.abi3t` job present and skipped
- [ ] Toolchain floors of §10.1 pinned in `pyproject.toml`; `[tool.ruff.format] exclude` covers `**/*.md`
- [ ] `.github/dependabot.yml` regains its `uv` entry when `pyproject.toml` lands (the Phase 1 scaffold removed both)
- [ ] PEP 740 attestations and SBOM published with the first release
- [ ] §12.2 gate met, or a written explanation of which criterion failed and by how much
- [ ] `docs/PHASE4.md` written, carrying §13 forward with the measured numbers

---

## Appendix A — measurements

CPython 3.12.3, x86-64, GCC 13.3, `-O2`. Reproduce with `python xtask/pybench.py`; the C probe module is checked in under `bench/probe/`.

```
pure-python function call                    20.8 ns
bare C call (METH_VARARGS, no args)          22.6 ns
  + GIL release/reacquire around nothing     63.0 ns   (delta +40.4)
one i64 arg, PyArg_ParseTuple                60.1 ns
one i64 arg, METH_FASTCALL                   31.2 ns   (delta -28.9)

batch kernel, n=1        233.6 ns total    233.6 ns/sample
batch kernel, n=8        227.0 ns total     28.4 ns/sample
batch kernel, n=64       291.2 ns total      4.6 ns/sample
batch kernel, n=512      919.5 ns total      1.8 ns/sample
batch kernel, n=4096    6894.1 ns total      1.7 ns/sample
batch kernel, n=65536 166617.0 ns total      2.5 ns/sample

np.empty((n,4,4))  ~270 ns flat from n=1 to n=65536
np.zeros((4096,4,4))                      11344 ns    -- never use

float64 seconds at 2026 epoch: ULP = 238 ns
1 kHz stamp intervals corrupted by a float round trip: 1999 / 1999
```
