# tf_tree — Phase 3 Implementation Specification: Python Bindings

> **Companion documents:** `docs/PROJECT.md` (vision, roadmap, decision log), `docs/PHASE1.md` (core), `docs/PHASE2.md` (shared memory).

**Deliverable:** a Python package where `import tf_tree; tree = tf_tree.open()` joins the robot's transform tree, batch lookups run at near-native speed with no intermediate copies, and the module is correct and parallel on free-threaded CPython.

**Framing.** A per-call overhead of 60 ns against a 150 ns lookup is a 40% tax, and a single missing declaration can silently disable free-threading for a user's whole process. This phase is a budget-management exercise; every design choice is anchored to a measured number (Appendix A).

Sections marked **NORMATIVE** are requirements.

## 0. Scope

### In scope

PyO3 bindings direct to the Rust core; vectorized NumPy lookup; caller-owned `at_into` output (including pinned memory); GIL discipline; free-threading (`Py_MOD_GIL_NOT_USED`, `Sync` pyclasses, TSan); DLPack device classification (§5.5); fork poisoning and shutdown; hand-written stubs; abi3 + free-threaded wheels for manylinux/musllinux/macOS/Windows.

### Out of scope — NORMATIVE

| Excluded | Why |
|---|---|
| `asyncio` integration | A lookup is ~200 ns. There is nothing to await. |
| ROS 2 / `rclpy` | Phase 4 (`tf_tree.ros`, lazy import). |
| Going through the C ABI | Python binds Rust directly; through C we would lose typed errors, zero-copy buffers, and `Drop` ordering. |
| Any view into the arena | §5.1. |
| CUDA / CuPy dependency | D8. The user allocates device memory; we write into it. |
| Reimplementing logic in Python | The binding is a thin shell; anything with a branch in it belongs in Rust. |
| `pickle` of `Tree`, `Plan`, `Publisher` | A live mapping cannot be serialized. Raise `TypeError` pointing at `open()`. |

---

## 1. Free-threading and the build matrix

### 1.1 Free-threaded CPython is supported, and abi3 does not cover it

- Python 3.14 supports the free-threaded build (PEP 779) as a separate `python3.14t` binary.
- **`abi3` does not work on free-threaded builds**: an `abi3` wheel is rejected by a free-threaded interpreter.
- **PEP 803 (`abi3t`)** defines a stable ABI valid on both builds from **Python 3.15**. PyO3 0.29 ships the `abi3t` features and maturin 1.14 builds them; only CPython 3.15 is missing.
- **maturin emits at most one stable-ABI family per invocation** (PyO3/maturin#3226), so `abi3` and `abi3.abi3t` cannot come out of the same build. §10 needs **two maturin invocations per platform**.
- **PyO3 0.29 refuses to build for `3.13t`** and maturin's `--find-interpreters` picks up 3.14t and newer only (PyO3/maturin#3206). No `cp313t` wheel; that is deliberate.

### 1.2 The declaration that matters more — NORMATIVE

**If an extension module does not declare itself free-threading-safe, importing it silently re-enables the GIL for the entire process.**

> **Correction — PyO3 0.29 (which §10.1 pins) flipped the default.** An **absent** `gil_used` now declares the module free-threading-*safe*; removing `gil_used = false` from `tf_tree_py` leaves `sys._is_gil_enabled()` false on `3.14t`. The failure mode is now a data race in an unaudited module, not a slowdown.
>
> Keep writing `gil_used = false` explicitly. The CI assertion below cannot test the flag (its absence yields the same value); it still catches any *other* import-time effect that re-enables the GIL. What makes the claim true is §7.1's `Send + Sync` audit, the concurrent-evaluation test, and TSan.

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

---

## 2. Measured budgets — the numbers that drive the API

CPython 3.12, x86-64, `-O2` (Appendix A).

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

1. **Argument parsing costs as much as the lookup.** Hot-path methods take **positional-only arguments** (§4.2).
2. **Releasing the GIL costs 40 ns.** Do not release for scalar lookups; release for batches (§6).
3. **Allocation is a flat ~270 ns**, ~50% of a 64-sample call. That is what `at_into` is for (§5.2).

---

## 3. Time is integer nanoseconds — NORMATIVE

**`float` timestamps are rejected. There is no conversion, no convenience overload, and no "seconds" keyword.**

At a 2026 Unix epoch (~1.75 × 10¹⁸ ns), the ULP of `float64` seconds is **238 ns**:

```
epoch ns            : 1753400000123456789
via float seconds   : 1753400000123456768        error: 21 ns
ulp at that magnitude                            238 ns
1 kHz stamps with the wrong spacing after a round trip:  1999 / 1999
```

Accepted stamp types: Python `int`, `np.int64` scalar, and C-contiguous `np.int64` arrays. Explicit converters are the only path from wall-clock types:

```python
tf_tree.from_sec(1753400000.5)          # -> int ns, documented as lossy above ~10^7 s
tf_tree.from_datetime(dt)               # exact; requires tz-aware
tf_tree.now(domain="steady")            # CLOCK_MONOTONIC, int ns
```

Passing a scalar `float` (including `np.float16/32/64`) raises `TypeError` naming `from_sec` and stating the ULP; every stamp entry point (`Plan.at`, `at_into`, `adaptive`, `Tree.lookup`, `Publisher.push`, `tf_tree.push`) goes through the one refusal.

A float stamps *array*, `Publisher.push_many` with a float array, and a `list` of stamps are refused with numpy's or PyO3's own message, without the measurement. The message states the fixed 238 ns ULP of a 2026 epoch, not the caller's magnitude.

---

## 4. API

### 4.1 Discovery mirrors Phase 2 exactly

```python
import tf_tree

tree = tf_tree.open()                                 # join, zero config
tree = tf_tree.open(name="robot", domain=7, mode="ro")   # explicit
with tf_tree.open() as tree: ...                      # context manager, explicit detach
```

**NORMATIVE defaults:** `mode="ro"` and no creation. Both differ from the Rust defaults, deliberately: most Python consumers are notebooks and analysis tools that must be incapable of corrupting a robot's tree (Phase 2 §8), and a notebook started before the robot must fail loudly rather than create an empty arena the real publisher then refuses to join.

```python
tree = tf_tree.open(mode="rw", create=[("map", "base"), ("base", "cam")])
```

**`create` is an edge list, not `"if_absent"`:** decision `0004` sizes an arena from its declared edges, so an arena cannot be created without saying what is in it. Creating **requires `mode="rw"`** and is refused otherwise.

`tf_tree.has_shared_memory()` reports whether the platform build includes the IPC layer (§10).

### 4.2 Lookup — vectorized first, positional-only

```python
plan = tf_tree.open().plan("map", "camera_optical")     # compile once

T  = plan.at(stamp_ns)                        # int   -> (4,4)   f64
Ts = plan.at(stamps)                          # (N,)  -> (N,4,4) f64
plan.at_into(stamps, out)                     # writes in place, allocates nothing
Ts = plan.at(stamps, layout="quat")           # (N,7)  f64  [qw qx qy qz tx ty tz]
Ts = plan.at(stamps, layout="affine32")       # (N,12) f32  row-major 3x4
T = plan.latest(); T = plan.latest_common()

knots, poses = plan.adaptive(t0, t1, lin=1e-3, ang=1e-4)
plan.adaptive_into(t0, t1, out_knots, out_poses, lin=1e-3, ang=1e-4)

T = tree.lookup("map", "camera_optical", stamp_ns)    # plan-cached convenience
```

**NORMATIVE:** `at`, `at_into`, `latest`, and `push` take **positional-only** arguments (`def at(self, stamps, /)`), for the 29 ns `METH_FASTCALL` difference. `layout=` is accepted as a keyword only on the non-hot overload. Verify that PyO3 actually emits `METH_FASTCALL` for these signatures. Keyword arguments are fine elsewhere (`open`, `plan`, `adaptive`), which run at startup.

### 4.3 Publishing

```python
with tree.publisher("base_link", "odom") as pub:      # (child, parent) — claims on enter
    pub.push(stamp_ns, T)                             # T: (4,4) or (7,)
    pub.push_many(stamps, poses)                      # vectorized
```

The context manager is the documented form (`Drop` still releases the claim otherwise). Argument order is **(child, parent)**, matching `Tree::claim(child, parent)`.

**There is no `declare_static` / `declare_dynamic` — NORMATIVE.** Decision [`0004`](./decisions/0004-builder-time-edge-declaration.md): topology is declared at builder time and the arena is sized from the declared edges (`crates/tf_tree/src/tree.rs:5`: "there is no post-build `declare_*`"). A post-build declaration would require the growth D4 forbids. A Python process that needs to define topology creates the arena with a layout (`layout_if_creating`):

```python
tree = tf_tree.open(mode="rw", create="if_absent", layout=[
    tf_tree.static_edge("base_link", "camera_mount", T),
    tf_tree.dynamic_edge("odom", "base_link", capacity=8192, interp="sclerp"),
])
```

`Tree.reparent` remains the only runtime topology mutation.

### 4.4 Errors

Rust's typed errors map to an exception hierarchy carrying **structured attributes**. This block is [`0058`](./decisions/0058-the-fields-a-python-exception-only-printed.md)'s and **it is what ships**: fifteen classes. `ClaimRevokedError` is deliberately absent.

```python
class TfTreeError(Exception): ...
class ExtrapolationError(TfTreeError):      # .edge, .requested, .oldest, .newest, .domain
class DisconnectedError(TfTreeError):       # .target, .source, .cut_at
class NoDataError(TfTreeError):             # .edge
class TopologyChangedError(TfTreeError):    # .plan_generation, .current_generation
class FrameNotDeclaredError(TfTreeError):   # .name
class DerivativesUnavailableError(TfTreeError):   # .edge
class NoSegmentError(TfTreeError):          # .edge
class BufferError(TfTreeError): ...
class ChildProcessDetachedError(TfTreeError):     # §8.1
class TimeDomainMismatchError(TfTreeError): # .expected, .got
class EdgeAlreadyClaimedError(TfTreeError): # .edge, .owner_slot
class NonMonotonicStampError(TfTreeError):  # .edge, .last, .got
class ArenaHeldButUnreachableError(TfTreeError):  # .holder_slots, .ownership_held
class ArenaAbsentError(TfTreeError): ...
```

- **Every class is a direct subclass of `TfTreeError`.** No `KeyError` base on `FrameNotDeclaredError` (`0058` §6): it would widen every `except KeyError` / `except LookupError` around a tf_tree call. Every class is registered on every platform, and its `__module__` is `"tf_tree"` so it pickles across `multiprocessing`.
- **An attribute is set on every instance the library raises, and on none a caller constructs.** It lives in `__dict__`, `args` stays `(message,)`, so pickle, `copy` and `multiprocessing` keep it. Values are plain data: `int`, `str`, `bool`, `None`, tuples of those.
- **An id reaches Python as the arena's stored names, or `None`.** `.edge` is the stored `(parent, child)` pair, `tuple[str, str]`; a frame (`.target`, `.source`, `.cut_at`) is its stored name. No attribute is an integer id.
- `ExtrapolationError.domain` is the query's time-domain tag, an `int`. `TimeDomainMismatchError` covers both plan-time and per-query refusals; `.expected` is the path's or plan's tag, `.got` the caller's.
- **`EdgeAlreadyClaimedError.owner_slot`** is a participant slot, `int | None`, `None` exactly when the claim word was mid-claim (`CLAIMING`). It is not a pid.
- **`ArenaHeldButUnreachableError.holder_slots`** is the held slots, ascending, `tuple[int, ...]`; `.ownership_held` is a `bool`. No pid is carried (`0033`); `tf_tree doctor` turns a slot into a process.
- **`FrameNotDeclaredError.name`** is the name the caller typed, `str | None`, `None` only where no name survives.
- **`ClaimRevokedError` waits for a Python-reachable trigger.** No Python caller can make `PushError::ClaimRevoked` raise, so it reaches Python as base `TfTreeError`; [`0031`](./decisions/0031-the-participant-record-with-no-byte.md) does not create one.
- `BufferError` and `tf_tree.open` are kept out of `__all__` so `from tf_tree import *` shadows no builtin.

`str(e)` names ids as the arena's names, resolved by the binding (`edge_label_in` / `frame_label` in `crates/tf_tree_py/src/errors.rs`); its text is not a compatibility promise (`docs/API.md` R5). `TopologyChangedError` must document that the response is to re-`plan`.

---

## 5. Zero-copy — precisely what it does and does not mean

### 5.1 There are no views into the arena — NORMATIVE

**The library never hands Python a buffer that aliases arena memory** — not for poses, not for stamps, not read-only.

An edge's sample storage is a ring being overwritten by another process, and correct reads go through the Phase 1 seqlock. A NumPy array pointing into it bypasses the protocol: a data race by construction, producing torn poses that cannot be detected. It would also pin the arena mapping for the array's lifetime.

"Zero-copy" here means **no intermediate allocation and no copy between the interpolation kernel and the caller's destination buffer.** State this in the README in those words.

### 5.2 Three tiers

| Tier | Allocations | When |
|---|---|---|
| `plan.at(stamps)` | 1 (~270 ns) | one-shot, exploratory, large batches |
| `plan.at_into(stamps, out)` | 0 | steady-state loops |
| `plan.at_into(stamps, device_out)` | 0 | `out` is pinned host memory the caller allocated (`torch.empty(..., pin_memory=True)`, `cupyx.empty_pinned`, `numba.cuda.pinned_array`) |

All tiers copy nothing after the kernel. Tier 3 is a convenience, not a performance necessity; a CPU store to `cudaMalloc` memory is undefined (§5.5).

### 5.3 Output buffer validation — NORMATIVE

Validate **fully, before writing anything**: dtype exactly matches the layout, shape matches `(N, ...)`, the array is **C-contiguous** and writable, and `N` matches the stamp count. **Non-contiguous or strided `out` is rejected, never silently copied.**

### 5.4 Export: implement nothing — NORMATIVE

Outputs are plain `numpy.ndarray`, so `__dlpack__`, `__dlpack_device__`, `__array_interface__` and the buffer protocol come for free. **Do not hand-roll a DLPack exporter**: the capsule ownership protocol is `unsafe` and NumPy already implements it.

An interop CI job consumes an `adaptive()` result from torch, JAX, and CuPy.

### 5.5 Accepting `out`: DLPack classifies, the buffer protocol carries writability — NORMATIVE

`__dlpack_device__()` returns `(device_type, device_id)` without a CUDA runtime (`cudaPointerGetAttributes` would need one, which D8 forbids). Older DLPack had no read-only bit; the buffer protocol has conveyed writability since PEP 3118.

```
accept:  kDLCPU (1), kDLCUDAHost (3), kDLCUDAManaged (13), kDLROCMHost (11)
reject:  everything else, naming the device type and suggesting a pinned allocator
```

**NORMATIVE acquisition order for `out`:**

1. If the object exposes `__dlpack_device__`, apply the whitelist; reject non-host memory here.
2. Acquire the pointer through the **buffer protocol** (`PyBUF_WRITABLE | PyBUF_C_CONTIGUOUS`), which validates writability, contiguity and itemsize and keeps the buffer alive across the GIL release (§6.2).
3. Only if there is no buffer protocol but a versioned DLPack capsule, fall back to `np.from_dlpack()` — **never hand-parse the capsule.**
4. Otherwise raise, naming both protocols.

> **Implementation status: steps 2–4 are NOT implemented.** `at_into` casts to `numpy.ndarray` (subclasses included), so a `memoryview` or non-NumPy pinned allocation is **refused whatever its layout**, and the error suggests `np.asarray(...)`. Step 1 is implemented. The cast path checks `NPY_ARRAY_WRITEABLE` explicitly, since step 2 was where writability was to be checked.

**Drop `__cuda_array_interface__` entirely**; device memory is rejected anyway. **Stream synchronization is the caller's responsibility**: pass `stream=None`, and a caller handing us a buffer a GPU kernel recently touched must synchronize first.

### 5.6 Device story and non-goals

By D8 the product is a bounded-error knot array (~1 KB, ~6 µs over PCIe), so tier 3 is an ergonomic win, not a throughput one. **Not Arrow** (dense fixed-shape payload). **DLPack does not belong in the Phase 4 C ABI**; carry this into `docs/PHASE4.md`.

---

## 6. GIL discipline

### 6.1 The threshold is computed, not constant — NORMATIVE

Releasing the GIL costs a measured 40 ns; a depth-3 lookup costs ~193 ns.

- **Never release for a scalar lookup.**
- **Always release when holding the GIL would stall other threads.**

The rule is expressed in work, because depth varies:

```rust
const GIL_RELEASE_THRESHOLD_NS: u64 = 1_000;
const NS_PER_STEP_ESTIMATE: u64 = 64;

let est = n as u64 * plan.depth() as u64 * NS_PER_STEP_ESTIMATE;
if est >= GIL_RELEASE_THRESHOLD_NS { py.allow_threads(|| kernel()) } else { kernel() }
```

For depth 3 this releases from `n = 6`. Not releasing retains the GIL for under 1 µs, far below CPython's 5 ms switch interval; releasing costs at most 4%. Neither branch is ever badly wrong, so the exact constant needs no tuning. Publish the constants and keep a benchmark row proving the crossover.

> **Amendment — `NS_PER_STEP_ESTIMATE` is 64, and this block is the single account of it** (cited by [`API.md`](./API.md) §3.4 and `tf_tree_py::tree`'s `release_the_gil`).
>
> **Measurement.** `benches/lookup.rs`, row `lookup/depth3/sclerp`, off-grid stamp `fixture::QUERY_NS` (three dynamic steps, `ScLerp`), nine alternated runs, `taskset -c 2`, shared 4-core VM: min 190.4, **median 192.7**, max 268.9 ns. 192.7 / 3 → **64 ns/step**. [`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)'s *Re-baseline* holds the protocol. The batch path this threshold governs measures 328 ns/elem (pose) and 369 (twist) at depth 3, above `est`'s 192, so the error runs safe (the GIL is released later, never sooner).
>
> The crossover is the smallest `n` with `n · depth · NS ≥ 1000`: depth 1 → n = 16, **depth 3 → n = 6**, depth 6 → n = 3. A `const` assertion in `tf_tree_py::tree` pins `n = 6`.
>
> **NORMATIVE ([`API.md`](./API.md) §3.4):** `NS_PER_STEP_ESTIMATE` is re-derived from `0013`'s re-baseline in the same commit, and this section names the measurement.

### 6.2 Rules while the GIL is released — NORMATIVE

1. **Touch no Python object.** Extract raw pointers and lengths *before* `allow_threads`.
2. **Hold the buffer view across the release** (`numpy`'s `PyReadwriteArray` or the raw buffer protocol). NumPy refuses to resize an exported array; that is what makes the pointer valid, so **test that the resize actually fails.**
3. On free-threaded builds `allow_threads` is cheaper but not free; the same rules apply.

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

`Publisher` is `!Sync` by design (single writer), so it is wrapped in a mutex (~15 ns uncontended): two Python threads pushing to one edge serialize.

### 7.2 No global mutable state — NORMATIVE

No `static mut`, no `once_cell` singletons holding Python objects, no process-global caches. **Type objects are not the singletons this forbids**: every `#[pyclass]` and `create_exception!` class keeps its type object in a process-global static, so the rule is about instances and caches ([`0058`](./decisions/0058-the-fields-a-python-exception-only-printed.md) *Context*). Sub-interpreter (PEP 734) support is best-effort.

The plan cache behind `tree.lookup` must be genuinely per-thread (`thread_local!`), not a shared map behind a lock.

### 7.3 Required CI

- The full test suite on both `3.14` and `3.14t` (`just py-test-freethreaded`).
- The `sys._is_gil_enabled()` assertion from §1.2.
- A scaling test: 1/2/4/8 threads calling `plan.at` on a shared `Tree`, asserting near-linear aggregate throughput on `3.14t`. **Not met as CI.** `just py-thread-scaling` (registered in [`docs/benchmarks/EVIDENCE.md`](./benchmarks/EVIDENCE.md)) takes the measurement since 2026-09-11, but it is a reporting recipe and no workflow runs it. `tests/python/test_freethreading.py` asserts only *correctness* over eight threads.
- **ThreadSanitizer**, via `just tsan` — eight readers against a live writer, over the **Rust** layer, deliberately: CPython is not TSan-instrumented, and the Python layer calls straight through. It complements `just loom`. Verified non-vacuous; `-Zbuild-std` is required.

---

## 8. Process and interpreter lifecycle

### 8.1 Fork — NORMATIVE

Phase 2 applies `MADV_DONTFORK` to the arena (`MappedArena::advise`, `crates/tf_tree_arena/src/mapped.rs:328`), so **a forked child has no mapping and any inherited handle is a segfault waiting to happen.**

- **Claims are in-arena CAS words** on `ClaimRecord` (`crates/tf_tree_core/src/edge.rs:150`), owner = participant slot + 1 (A3), guarded by an epoch (A4). The OFD locks in `crates/tf_tree_ipc/` cover the rendezvous lock file only. Decision [`0005`](./decisions/0005-the-shared-memory-seam.md) adds a lock lease alongside the CAS, but the CAS remains the decision.
- The child's failure is **`SIGSEGV` on any use of the vanished mapping**, including `Tree::drop` (`impl Drop for Tree`), so a child that never touches the API still dies at exit.

**Poisoning must suppress the destructor, not only the API surface.** `0005` step 9 implements the Rust half (a process-global fork generation bumped by `pthread_atfork`, checked in `Drop`); the Python hook is belt-and-braces.

```python
os.register_at_fork(after_in_child=_poison_all_handles)
```

Poisoning marks every `Tree`, `Plan`, and `Publisher` in the child dead; use raises `ChildProcessDetachedError` saying to call `tf_tree.open()` in the child. It must be tested with `pytest-forked` under all three start methods.

> **What a fork child's call does (measured 2026-09-14).** Raises `ChildProcessDetachedError`: `lookup`, `plan`, `span`, `edges`, `frames`, `instance_uuid`, `publisher`, `Tree.freeze`, `Plan.at` / `at_into` / `at_extrapolating` / `at_extrapolating_into` / `adaptive` / `edges` / `latest`, `Publisher.push` / `push_many`, and module-level `push`. Answers from handle state without raising: `is_shared`, `is_writable`, `Plan.depth`, `Tree.source`, `Publisher.release`; `owner_lost` answers `False`, `reap_dead` `0`, `inherit_ownership` `"NotApplicable"`. The Rust `Tree::freeze_to` has no check of its own; the binding refuses before the call. §14's poisoning box stays unticked: the tests use bare `os.fork()`.

### 8.2 Interpreter shutdown

Explicit `close()` and context-manager support are the documented path; an `atexit` hook detaches anything still open. If the interpreter is killed or leaks the handle, the kernel releases the OFD locks and closes the socket and the arena's crash-consistency handles the rest: Python's finalization is unreliable, and it does not matter.

---

## 9. Typing, docs, ergonomics

- `py.typed` plus **hand-written `.pyi` stubs** (generated stubs cannot express the scalar-vs-array overloads), with `@overload` for `at(int)` vs `at(NDArray[int64])` and `Literal["mat4","quat","affine32"]` for `layout`.
- **Also generate stubs (maturin 1.14, PyO3/maturin#3211), and diff**: CI asserts every public generated symbol appears in the hand-written `.pyi`.
- `mypy --strict` and `pyright --strict` in CI over the stubs *and* the example code; doctests in CI.
- Docstrings carry the measured numbers behind a design. `__repr__` on `Tree` shows domain, name, mode, participant count, `instance_uuid`; on `Plan`, frame names and folded depth.

---

## 10. Build and distribution matrix

| Platform | Shared memory | ABI targets |
|---|---|---|
| `manylinux_2_28` x86-64 | yes | `abi3-py39`, `cp314t` |
| `manylinux_2_28` aarch64 (Jetson) | yes | same |
| `musllinux_1_2` x86-64, aarch64 | yes | same |
| macOS arm64 / x86-64 | **no** — in-process only | same |
| Windows x86-64 | **no** — in-process only | same |

Each row is **two maturin invocations** (§1.1): one against a GIL interpreter ≤ 3.14 (`--features pyo3/abi3-py39`) producing the `abi3` wheel, one against `python3.14t` producing `cp314-cp314t`. A third, `abi3.abi3t` against 3.15+, is added when 3.15 ships and then *replaces* both there. `cp313t` is absent deliberately.

macOS and Windows wheels ship without the IPC layer; `tf_tree.has_shared_memory()` reports the truth and `open()` there gives an in-process `HeapArena` tree. The Rust `shm` feature compiles the IPC layer out.

- Cross-compile aarch64 with `maturin-action`; keep the `abi3.abi3t` job written and skipped (PEP 803); let `--find-interpreters` discover `3.14t`.
- PEP 740 attestations (produced by the *upload* step under Trusted Publishing), an SBOM, and reproducible builds.

### 10.1 Toolchain floors — NORMATIVE

| Tool | Floor | Why |
|---|---|---|
| PyO3 | `0.29` | `abi3t` / `abi3t-py315` features (§1.1) |
| maturin | `1.14.1` | builds abi3t (PyO3/maturin#3113) and gets the abi3/abi3t interaction right (PyO3/maturin#3226) |
| pytest | `9.1` | see below |
| ruff | `0.16` | see below |
| pyright | `1.1.411` | `--strict` behaviour §9 is written against |

- **ruff 0.16 formats Python code blocks inside Markdown by default**: set `[tool.ruff.format] exclude` over `**/*.md`, and keep `[tool.ruff.lint] select` explicit.
- **pytest 9.1 can run an inline module-scoped autouse fixture twice under `--doctest-modules`**: keep them in `conftest.py`. `parametrize` `argvalues` must be a `Collection`.

---

## 11. Test plan

### 11.1 Correctness across the boundary

- **Correctness:** Hypothesis tests (`at(t)` equals `at([t])[0]` bit-exactly; every `layout` the same transform; `at_into` equals `at`; endpoints exact); differential test against the Rust CLI over a recorded MCAP session (bit-identical `f64`).
- **Errors:** every error type raised at least once with attributes asserted. **Met for every class `tests/python` can make raise; four things cannot** ([`0058`](./decisions/0058-the-fields-a-python-exception-only-printed.md)): the `None` arm of every resolved-id attribute (`.edge`, `.target`, `.source`, `.cut_at`); `FrameNotDeclaredError.name`'s `None` arm; `EdgeAlreadyClaimedError.owner_slot`'s `None` arm (a claim held in `CLAIMING`); and `ClaimRevokedError`, not built (reasons: 0058). `TopologyChangedError`'s generations are raised by `tests/python/test_shared.py` through `tf_tree_rendezvous_child join-reparent`.
- **Buffer safety:** resizing an exported array fails (§6.2); `out` with wrong dtype, shape, non-contiguity, read-only, or mismatched `N` raises **before any element is written**; CUDA-device `out` raises naming the device type and is never written; pinned host buffers from torch, CuPy and Numba are accepted; a read-only array round-tripped through DLPack is still rejected; refcount and `tracemalloc` leak tests over 10⁶ calls; a stamp array mutated from another thread during a released-GIL batch yields valid transforms, never a crash.
- **Concurrency:** §7.3 in full; two threads pushing to one `Publisher` serialize without corruption; two processes share an arena, one publishing, one reading.
- **Lifecycle:** `pytest-forked` across `fork`, `forkserver`, `spawn` (the `fork` child raises `ChildProcessDetachedError`, never segfaults); `SIGKILL` while holding a claim lets another process claim the edge; `open()` / `close()` cycled 10⁴ times leaks no participant slots.

---

## 12. Benchmarks and the gate

### 12.1 Measurements

Report: scalar `plan.at` depth-3 p50/p99; `plan.at(stamps)` ns/sample at n = 1…65536; `at_into` vs `at` delta (~270 ns); the GIL crossover around the §6.1 threshold; thread scaling 1→8 on GIL and `3.14t`; `tree.lookup` vs `plan.at`; **the ratio against `tf2_ros` Python on the same tree and queries**; import time and wheel size.

### 12.2 The gate — NORMATIVE

1. **Scalar `plan.at` p50 under 250 ns.**
2. **`at_many` at n = 4096 within 1.3× of native per-sample cost.** The central claim; put it in the README.
3. **`at_into` eliminates the full ~270 ns allocation**, visible at n = 64.
4. **Thread scaling ≥ 6× from 1 to 8 threads on `3.14t`**, and ≥ 6× on the GIL build for batches above the release threshold. **Measured 2026-09-11 by `just py-thread-scaling` (`crates/tf_tree_bench/python/thread_scaling.py`); this host cannot settle it.** [`docs/benchmarks/EVIDENCE.md`](./benchmarks/EVIDENCE.md)'s probe row is the only copy of the readings. The free-threaded half straddles the floor; the GIL half (`just py-thread-scaling-gil`) sits below it, roughly half of the gap being `Plan::at`'s GIL-held output allocation (`--call at_into` reads higher in six of six interleaved pairs; `--gate --call at_into` is refused because the criterion names `plan.at`) and the rest unattributed.

    A clearing run is a conservative pass, since every host unfairness to a scaling floor pushes the reading down (`docs/PHASE5.md` §9.3's one-sided-budget argument); it does not license calling the criterion met. **≥ 8 physical cores would settle it**; a miss on such a host is a genuine `FAIL`. [`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)'s *Resolution* re-cut PHASE1 §11.3's same criterion to ≥ 2.5× from 1 to 4 threads; whether criterion 4 takes that shape is a decision, not an edit to this line.
5. **`import tf_tree` does not re-enable the GIL**, asserted in CI.
6. **TSan clean** on the free-threaded build.
7. Zero leaks over 10⁶ calls.

---

## 13. Phase 4 handoff

- **Do not route Python through the C ABI.**
- The `tf_tree.ros` submodule (lazy `rclpy` import, `builtin_interfaces.msg.Time` and `TransformStamped` conversion, `tf2_ros.Buffer`-shaped adapter) is Phase 4's, but **it must convert through integer nanoseconds** (§3), never `Time.to_sec()`.
- Carry §12's measured `tf2_ros` ratio into Phase 4's docs.

---

## 14. Definition of done

- [ ] `import tf_tree; tf_tree.open()` works with zero arguments on a machine with a running arena, and in a bare notebook with none
- [x] `#[pymodule(gil_used = false)]` set; asserted on `3.14t` — but see §1.2's correction, the attribute is not what the assertion proves
- [x] Every `#[pyclass]` is `Send + Sync`; `Publisher` wrapped
- [~] No `float` stamp accepted anywhere; `TypeError` names `from_sec` and states the ULP — **split on 2026-09-14.** No float is accepted anywhere. The `TypeError` with the ULP holds for every **scalar** float stamp, not for a float stamps **array** (numpy's or PyO3's own message), and the ULP is stated at a fixed epoch (§3)
- [ ] No API returns a view into the arena (grep-able review item, documented in the README)
- [x] `at_into` validates fully before writing; non-contiguous `out` rejected
- [x] `out` device classification via `__dlpack_device__`; CUDA device memory rejected with an actionable message
- [x] No hand-written DLPack capsule parsing anywhere in the codebase
- [ ] `os.register_at_fork` poisoning tested under all three start methods
- [ ] Hand-written stubs; `mypy --strict` and `pyright --strict` clean over stubs and examples
- [x] CI asserts no public symbol exists in the generated stubs but not the hand-written ones (§9)
- [x] Wheels for every row of §10 — two invocations each — with the `abi3.abi3t` job present and skipped (`.github/workflows/wheels.yml`; executed and green on the `v0.0.3` and `v0.0.4` tags)
- [ ] Toolchain floors of §10.1 pinned in `pyproject.toml`; `[tool.ruff.format] exclude` covers `**/*.md`
- [ ] `.github/dependabot.yml` regains its `uv` entry when `pyproject.toml` lands
- [~] **PEP 740 attestations are published; the SBOM is not.** `wheels.yml`'s `publish` job carries `attestations: write` and `attestations: true` under Trusted Publishing and has run green on two tags (since 2026-08-19). The SBOM is absent (`PHASE5.md` §10's *Not done* list)
- [ ] §12.2 gate met, or a written explanation of which criterion failed and by how much. There are three states: **met**, **failed by a stated margin**, and **not producible on this host** (`docs/PHASE4.md` §9's equivalent box). Criterion 4 is the third (2026-09-11; readings in [`docs/benchmarks/EVIDENCE.md`](./benchmarks/EVIDENCE.md)). The box stays unticked because six of the seven criteria are still unmeasured
- [ ] `docs/PHASE4.md` written, carrying §13 forward with the measured numbers

---

## Appendix B — what is built, and what is unproven

Implemented and gated locally (`just py-test`, `py-test-freethreaded`, `py-lint`, `tsan`): `open()`/`build()`, `Plan.at`, `at_into` with DLPack classification, `adaptive`, `Publisher`, the fifteen exception classes of §4.4, stubs with a bidirectional drift check, `pyright --strict`, TSan over the read path. Wheels: `cp314`, `cp314t`, and an `abi3-py39` wheel verified on 3.14.

`wheels.yml` has run green on `v0.0.3` (2026-08-19) and `v0.0.4` (2026-08-22): every wheel row and `sdist` succeeded, `abi3.abi3t` was skipped as §14 specifies, and `publish` succeeded. The 0.0.2 PyPI wheels did not come from this workflow.

Not implemented: `adaptive_into` (the Rust side returns slices borrowed from internal scratch, so it would copy and break §5.2's tier-2 "no copies"), and `tf_tree.ros` (Phase 4).

## Appendix A — measurements

CPython 3.12.3, x86-64, `-O2`; reproduce with `python xtask/pybench.py` (probe module under `bench/probe/`). Figures are §2's table plus the batch curve:

```
batch kernel n=1: 233.6 ns/sample; n=64: 4.6; n=4096: 1.7; n=65536: 2.5
np.empty((n,4,4)) ~270 ns flat; np.zeros((4096,4,4)) 11344 ns
```
