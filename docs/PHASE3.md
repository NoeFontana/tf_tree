# tf_tree — Phase 3 Implementation Specification: Python Bindings

> **Companion documents:** `docs/PROJECT.md` (roadmap, decision log), `docs/PHASE1.md` (core), `docs/PHASE2.md` (shared memory).

**Deliverable:** `import tf_tree; tree = tf_tree.open()` joins the robot's transform tree, batch lookups run at near-native speed with no intermediate copies, and the module is correct and parallel on free-threaded CPython. Every design choice is anchored to a measured number (Appendix A). Sections marked **NORMATIVE** are requirements.

## 0. Scope

In scope: PyO3 bindings direct to the Rust core, vectorized NumPy lookup, caller-owned `at_into` output, GIL discipline, free-threading, DLPack device classification (§5.5), fork poisoning, hand-written stubs, abi3 + free-threaded wheels.

**Out of scope — NORMATIVE:** `asyncio` integration (a lookup is ~200 ns); ROS 2 / `rclpy` (Phase 4, `tf_tree.ros`, lazy import); going through the C ABI (Python binds Rust directly, or it loses typed errors, zero-copy buffers and `Drop` ordering); any view into the arena (§5.1); a CUDA / CuPy dependency (D8: the user allocates device memory, we write into it); reimplementing logic in Python (anything with a branch belongs in Rust); `pickle` of `Tree`, `Plan`, `Publisher` (raise `TypeError` pointing at `open()`).

## 1. Free-threading and the build matrix

### 1.1 Free-threaded CPython is supported, and abi3 does not cover it

- Python 3.14 supports the free-threaded build (PEP 779) as `python3.14t`. **An `abi3` wheel is rejected by a free-threaded interpreter.**
- **PEP 803 (`abi3t`)** is valid on both builds from **Python 3.15**.
- **maturin emits at most one stable-ABI family per invocation** (PyO3/maturin#3226), so §10 needs **two maturin invocations per platform**.
- **PyO3 0.29 refuses to build for `3.13t`**; no `cp313t` wheel, deliberately.

### 1.2 The declaration that matters more — NORMATIVE

**If an extension module does not declare itself free-threading-safe, importing it silently re-enables the GIL for the whole process.** PyO3 0.29 (§10.1) treats an absent `gil_used` as safe, so omitting it risks a data race in an unaudited module. Keep `gil_used = false` explicit. The CI assertion below cannot test the flag; it catches any *other* import-time effect that re-enables the GIL. What makes the claim true is §7.1's `Send + Sync` audit, the concurrent-evaluation test, and TSan.

```rust
#[pymodule(gil_used = false)]      // -> Py_mod_gil = Py_MOD_GIL_NOT_USED
fn tf_tree(m: &Bound<'_, PyModule>) -> PyResult<()> { ... }
```

**CI must assert this**, not review it. On a free-threaded interpreter: `import sys, tf_tree; assert not sys._is_gil_enabled()`.

## 2. Measured budgets — the numbers that drive the API

CPython 3.12, x86-64, `-O2` (Appendix A).

| Operation | Cost |
|---|---|
| One `i64` arg via `PyArg_ParseTuple` | **60.1 ns** |
| One `i64` arg via `METH_FASTCALL` | **31.2 ns** |
| GIL release + reacquire around nothing | **+40.4 ns** |
| Batch call fixed overhead / marginal cost | ~220 ns / 1.7–2.5 ns per sample |
| `np.empty((n,4,4))` | ~270 ns, **flat** to n = 65 536 |
| `np.zeros((4096,4,4))` | 11 344 ns — never use |

Hence positional-only hot-path arguments (§4.2), no GIL release for scalar lookups (§6), and `at_into` (§5.2).

## 3. Time is integer nanoseconds — NORMATIVE

**`float` timestamps are rejected. There is no conversion, no convenience overload, and no "seconds" keyword.** At a 2026 Unix epoch (~1.75 × 10¹⁸ ns) the ULP of `float64` seconds is **238 ns**.

Accepted stamp types: Python `int`, `np.int64` scalar, and C-contiguous `np.int64` arrays. The only path from wall-clock types is `tf_tree.from_sec(x)` (lossy above ~10^7 s), `tf_tree.from_datetime(dt)` (exact; tz-aware only) and `tf_tree.now(domain="steady")` (`CLOCK_MONOTONIC`, int ns).

A scalar `float` (including `np.float16/32/64`) raises `TypeError` naming `from_sec` and stating the ULP (at the fixed 2026 epoch), at every stamp entry point (`Plan.at`, `at_into`, `adaptive`, `Tree.lookup`, `Publisher.push`, `tf_tree.push`). A float stamps *array* and a `list` of stamps are refused with numpy's or PyO3's own message.

## 4. API

### 4.1 Discovery mirrors Phase 2 exactly

```python
tree = tf_tree.open()                                    # join, zero config
tree = tf_tree.open(name="robot", domain=7, mode="ro")   # explicit; also a context manager
tree = tf_tree.open(mode="rw", create=[("map", "base"), ("base", "cam")])
```

**NORMATIVE defaults:** `mode="ro"` and no creation, deliberately unlike the Rust defaults: a Python consumer must be incapable of corrupting a tree (Phase 2 §8), and a notebook started before the robot must fail loudly rather than create an arena the real publisher then refuses to join.

**`create` is an edge list, not `"if_absent"`:** decision `0004` sizes an arena from its declared edges. Creating **requires `mode="rw"`**. `tf_tree.has_shared_memory()` reports whether the build includes the IPC layer (§10).

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

**NORMATIVE:** `at`, `at_into`, `latest`, and `push` take **positional-only** arguments (`def at(self, stamps, /)`) for the 29 ns `METH_FASTCALL` difference; verify PyO3 emits `METH_FASTCALL` for them. `layout=` is a keyword only on the non-hot overload. Keywords are fine elsewhere.

### 4.3 Publishing

```python
with tree.publisher("base_link", "odom") as pub:      # (child, parent) — claims on enter
    pub.push(stamp_ns, T)                             # T: (4,4) or (7,)
    pub.push_many(stamps, poses)                      # vectorized
```

Argument order is **(child, parent)**, matching `Tree::claim`. **There is no `declare_static` / `declare_dynamic` — NORMATIVE.** Decision [`0004`](./decisions/0004-builder-time-edge-declaration.md): topology is declared at builder time and the arena is sized from the declared edges; a post-build declaration would require the growth D4 forbids. A Python process that defines topology creates the arena with a layout (`layout_if_creating`): `open(mode="rw", create="if_absent", layout=[tf_tree.static_edge(child, parent, T), tf_tree.dynamic_edge(child, parent, capacity=8192, interp="sclerp")])`. `Tree.reparent` is the only runtime topology mutation.

### 4.4 Errors

Rust's typed errors map to an exception hierarchy with **structured attributes**. This block is [`0058`](./decisions/0058-the-fields-a-python-exception-only-printed.md)'s and **it is what ships**: fifteen classes, each a direct subclass of `TfTreeError` (`0058` §6); `ClaimRevokedError` is deliberately absent.

```python
class TfTreeError(Exception): ...    # base of all below; attributes in parentheses
# ExtrapolationError(.edge .requested .oldest .newest .domain)  DisconnectedError(.target .source .cut_at)
# NoDataError(.edge)  TopologyChangedError(.plan_generation .current_generation)  FrameNotDeclaredError(.name)
# DerivativesUnavailableError(.edge)  NoSegmentError(.edge)  BufferError  ChildProcessDetachedError (§8.1)
# TimeDomainMismatchError(.expected .got)  EdgeAlreadyClaimedError(.edge .owner_slot)
# NonMonotonicStampError(.edge .last .got)  ArenaHeldButUnreachableError(.holder_slots .ownership_held)
# ArenaAbsentError
```

- Every class is registered on every platform with `__module__ == "tf_tree"`, so it pickles. An attribute is set on every instance the library raises, and on none a caller constructs; it lives in `__dict__` and `args` stays `(message,)`.
- **Ids reach Python as the arena's stored names, or `None`**: `.edge` is the stored `(parent, child)` pair; `.target`, `.source`, `.cut_at` and `FrameNotDeclaredError.name` (the caller's typed name) are names. No attribute is an integer id or a pid (`0033`).
- `TimeDomainMismatchError.expected` is the path's or plan's tag, `.got` the caller's. `EdgeAlreadyClaimedError.owner_slot` is `int | None`, `None` exactly when the claim word was mid-claim. `ArenaHeldButUnreachableError.holder_slots` is an ascending `tuple[int, ...]`.
- `PushError::ClaimRevoked` reaches Python as base `TfTreeError` until a Python-reachable trigger exists ([`0031`](./decisions/0031-the-participant-record-with-no-byte.md)). `BufferError` and `tf_tree.open` stay out of `__all__` so `from tf_tree import *` shadows no builtin.

`str(e)` text is not a compatibility promise (`docs/API.md` R5). `TopologyChangedError` must document that the response is to re-`plan`.

## 5. Zero-copy — precisely what it does and does not mean

### 5.1 There are no views into the arena — NORMATIVE

**The library never hands Python a buffer that aliases arena memory**, read-only or not: a sample ring is overwritten by another process and correct reads go through the Phase 1 seqlock, so an aliasing NumPy array is a data race by construction. "Zero-copy" means **no intermediate allocation and no copy between the interpolation kernel and the caller's destination buffer.** State this in the README in those words.

### 5.2 Allocation tiers

- `plan.at(stamps)`: one allocation (~270 ns); one-shot and large batches.
- `plan.at_into(stamps, out)`: zero allocations; steady-state loops. `out` may be pinned host memory the caller allocated (`torch.empty(..., pin_memory=True)`, `cupyx.empty_pinned`, `numba.cuda.pinned_array`); a CPU store to `cudaMalloc` memory is undefined (§5.5).

### 5.3 Output buffer validation — NORMATIVE

Validate **fully, before writing anything**: dtype matches the layout, shape matches `(N, ...)` with `N` the stamp count, and the array is **C-contiguous** and writable. **Non-contiguous `out` is rejected, never silently copied.**

### 5.5 Accepting `out`: DLPack classifies, the buffer protocol carries writability — NORMATIVE

Outputs are plain `numpy.ndarray`; **do not hand-roll a DLPack exporter**, and do not build Arrow or put DLPack in the Phase 4 C ABI. An interop CI job consumes an `adaptive()` result from torch, JAX, and CuPy.

`__dlpack_device__()` returns `(device_type, device_id)` without a CUDA runtime (D8). **Accept** `kDLCPU` (1), `kDLCUDAHost` (3), `kDLCUDAManaged` (13), `kDLROCMHost` (11); **reject** everything else, naming the device type and suggesting a pinned allocator.

**NORMATIVE acquisition order for `out`:**

1. If the object exposes `__dlpack_device__`, apply the whitelist; reject non-host memory here.
2. Acquire the pointer through the **buffer protocol** (`PyBUF_WRITABLE | PyBUF_C_CONTIGUOUS`), which validates writability, contiguity and itemsize and keeps the buffer alive across the GIL release (§6.2).
3. Only if there is no buffer protocol but a versioned DLPack capsule, fall back to `np.from_dlpack()` — **never hand-parse the capsule.**
4. Otherwise raise, naming both protocols.

> **Implementation status: steps 2–4 are NOT implemented.** `at_into` casts to `numpy.ndarray` (subclasses included), so a `memoryview` or non-NumPy pinned allocation is **refused whatever its layout**, and the error suggests `np.asarray(...)`. Step 1 is implemented; the cast path checks `NPY_ARRAY_WRITEABLE` explicitly.

Drop `__cuda_array_interface__`; stream synchronization is the caller's responsibility (`stream=None`).

## 6. GIL discipline

### 6.1 The threshold is computed, not constant — NORMATIVE

**Never release for a scalar lookup; always release when holding the GIL would stall other threads.** The rule is expressed in work, because depth varies:

```rust
const GIL_RELEASE_THRESHOLD_NS: u64 = 1_000;
const NS_PER_STEP_ESTIMATE: u64 = 64;

let est = n as u64 * plan.depth() as u64 * NS_PER_STEP_ESTIMATE;
if est >= GIL_RELEASE_THRESHOLD_NS { py.allow_threads(|| kernel()) } else { kernel() }
```

**`NS_PER_STEP_ESTIMATE` is 64, and this block is the single account of it** (cited by [`API.md`](./API.md) §3.4 and `tf_tree_py::tree`'s `release_the_gil`): median 192.7 ns on `benches/lookup.rs` row `lookup/depth3/sclerp` at `fixture::QUERY_NS`, / 3 steps; [`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)'s *Re-baseline* holds the protocol. The error runs safe: the GIL is released later, never sooner.

The crossover is the smallest `n` with `n · depth · NS ≥ 1000`: depth 1 → n = 16, **depth 3 → n = 6**, depth 6 → n = 3. A `const` assertion in `tf_tree_py::tree` pins `n = 6`. **NORMATIVE ([`API.md`](./API.md) §3.4):** `NS_PER_STEP_ESTIMATE` is re-derived from `0013`'s re-baseline in the same commit, and this section names the measurement.

### 6.2 Rules while the GIL is released — NORMATIVE

1. **Touch no Python object**; extract raw pointers and lengths *before* `allow_threads`.
2. **Hold the buffer view across the release** (`PyReadwriteArray` or the raw buffer protocol). NumPy refuses to resize an exported array, which keeps the pointer valid, so **test that the resize actually fails.**

## 7. Free-threaded correctness

### 7.1 Every `#[pyclass]` must be `Send + Sync` — NORMATIVE

`Tree` and `Plan` (`Copy`, `#[pyclass(frozen)]`) are `Send + Sync` and exposed directly; `Guard` is never exposed. `Publisher` is **`Send + !Sync`** by design (single writer) and wrapped as `PyPublisher(Mutex<Publisher>)`, so two Python threads pushing to one edge serialize on the mutex.

### 7.2 No global mutable state — NORMATIVE

No `static mut`, no `once_cell` singletons holding Python objects, no process-global caches. **Type objects are not the singletons this forbids** ([`0058`](./decisions/0058-the-fields-a-python-exception-only-printed.md) *Context*). Sub-interpreter (PEP 734) support is best-effort. The plan cache behind `tree.lookup` must be `thread_local!`, not a shared map behind a lock.

### 7.3 Required CI

- The full test suite on both `3.14` and `3.14t` (`just py-test-freethreaded`), and the `sys._is_gil_enabled()` assertion from §1.2.
- A scaling test: 1/2/4/8 threads calling `plan.at` on a shared `Tree`, asserting near-linear throughput on `3.14t`. **Not met as CI**: `just py-thread-scaling` (registered in [`docs/benchmarks/EVIDENCE.md`](./benchmarks/EVIDENCE.md)) is a reporting recipe no workflow runs; `tests/python/test_freethreading.py` asserts only *correctness* over eight threads.
- **ThreadSanitizer**, via `just tsan` — eight readers against a live writer over the **Rust** layer (CPython is not TSan-instrumented); `-Zbuild-std` is required.

## 8. Process and interpreter lifecycle

### 8.1 Fork — NORMATIVE

Phase 2 applies `MADV_DONTFORK` to the arena, so **a forked child has no mapping and `SIGSEGV`s on any use of an inherited handle**, including `Tree::drop`. Claims are in-arena CAS words on `ClaimRecord` (A3, A4); the OFD locks in `crates/tf_tree_ipc/` cover the rendezvous lock file only, and [`0005`](./decisions/0005-the-shared-memory-seam.md) adds a lock lease alongside the CAS.

**Poisoning must suppress the destructor, not only the API surface.** `0005` step 9 implements the Rust half (a process-global fork generation bumped by `pthread_atfork`, checked in `Drop`); the Python hook is belt-and-braces:

```python
os.register_at_fork(after_in_child=_poison_all_handles)
```

Poisoning marks every `Tree`, `Plan`, and `Publisher` in the child dead; use raises `ChildProcessDetachedError` saying to call `tf_tree.open()` in the child. Test with `pytest-forked` under all three start methods.

In a fork child every method that reads or writes the arena raises it. `is_shared`, `is_writable`, `Plan.depth`, `Tree.source`, `Publisher.release` answer from handle state; `owner_lost` answers `False`, `reap_dead` `0`, `inherit_ownership` `"NotApplicable"`. §14's poisoning box stays unticked: the tests use bare `os.fork()`.

An `atexit` hook detaches any handle still open at interpreter shutdown.

## 9. Typing, docs, ergonomics

- `py.typed` plus **hand-written `.pyi` stubs** (generated stubs cannot express the scalar-vs-array overloads), with `@overload` for `at(int)` vs `at(NDArray[int64])` and `Literal["mat4","quat","affine32"]` for `layout`.
- **Also generate stubs (maturin 1.14, PyO3/maturin#3211), and diff**: CI asserts every public generated symbol appears in the hand-written `.pyi`.
- `mypy --strict` and `pyright --strict` in CI over the stubs and the example code; doctests in CI.

## 10. Build and distribution matrix

Targets: `manylinux_2_28` x86-64 and aarch64 (Jetson), `musllinux_1_2` x86-64 and aarch64, macOS arm64 / x86-64, Windows x86-64. Every row builds `abi3-py39` and `cp314t`. **macOS and Windows have no shared memory**: their wheels ship without the IPC layer and `open()` gives an in-process `HeapArena` tree.

Each row is **two maturin invocations** (§1.1): a GIL interpreter ≤ 3.14 (`--features pyo3/abi3-py39`) producing the `abi3` wheel, and `python3.14t` producing `cp314-cp314t`. A third, `abi3.abi3t` against 3.15+, *replaces* both once 3.15 ships; keep that job written and skipped. Publish PEP 740 attestations (from the *upload* step under Trusted Publishing), an SBOM, and reproducible builds.

### 10.1 Toolchain floors — NORMATIVE

| Tool | Floor | Why |
|---|---|---|
| PyO3 | `0.29` | `abi3t` / `abi3t-py315` features (§1.1) |
| maturin | `1.14.1` | builds abi3t (PyO3/maturin#3113) and gets the abi3/abi3t interaction right (PyO3/maturin#3226) |
| pytest | `9.1` | see below |
| ruff | `0.16` | see below |
| pyright | `1.1.411` | `--strict` behaviour §9 is written against |

- ruff 0.16 formats Python code blocks inside Markdown: set `[tool.ruff.format] exclude` over `**/*.md`, and keep `[tool.ruff.lint] select` explicit.
- pytest 9.1 can run an inline module-scoped autouse fixture twice under `--doctest-modules`: keep them in `conftest.py`; `parametrize` `argvalues` must be a `Collection`.

## 11. Test plan

### 11.1 Correctness across the boundary

- **Correctness:** Hypothesis tests (`at(t)` equals `at([t])[0]` bit-exactly; every `layout` the same transform; `at_into` equals `at`); differential test against the Rust CLI over a recorded MCAP session (bit-identical `f64`).
- **Errors:** every error type raised at least once with attributes asserted, except what `tests/python` cannot raise ([`0058`](./decisions/0058-the-fields-a-python-exception-only-printed.md)): the `None` arms of resolved-id attributes and `owner_slot`, and `ClaimRevokedError`.
- **Buffer safety:** resizing an exported array fails (§6.2); a bad `out` (dtype, shape, non-contiguous, read-only, mismatched `N`) raises **before any element is written**; CUDA-device `out` raises and is never written; pinned host buffers from torch, CuPy and Numba are accepted; leak tests over 10⁶ calls; a stamp array mutated from another thread during a released-GIL batch never crashes.
- **Concurrency:** §7.3 in full; two threads pushing to one `Publisher` serialize; two processes share an arena.
- **Lifecycle:** `pytest-forked` across `fork`, `forkserver`, `spawn`; `SIGKILL` while holding a claim lets another process claim the edge; `open()` / `close()` cycled 10⁴ times leaks no participant slots.

## 12. Benchmarks and the gate

### 12.1 Measurements

Report: scalar `plan.at` depth-3 p50/p99; `plan.at(stamps)` ns/sample at n = 1…65536; `at_into` vs `at`; the §6.1 GIL crossover; thread scaling 1→8 on GIL and `3.14t`; **the ratio against `tf2_ros` Python on the same tree and queries**; import time and wheel size.

### 12.2 The gate — NORMATIVE

1. **Scalar `plan.at` p50 under 250 ns.**
2. **`at_many` at n = 4096 within 1.3× of native per-sample cost** — the central claim; put it in the README.
3. **`at_into` eliminates the ~270 ns allocation**, visible at n = 64.
4. **Thread scaling ≥ 6× from 1 to 8 threads on `3.14t`**, and ≥ 6× on the GIL build for batches above the release threshold. `just py-thread-scaling` (`crates/tf_tree_bench/python/thread_scaling.py`) measures it; **this host cannot settle it**, and [`docs/benchmarks/EVIDENCE.md`](./benchmarks/EVIDENCE.md)'s probe row is the only copy of the readings. **≥ 8 physical cores would settle it**; a miss there is a genuine `FAIL`. [`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)'s *Resolution* re-cut PHASE1 §11.3's same criterion to ≥ 2.5× from 1 to 4 threads; whether criterion 4 takes that shape is a decision, not an edit to this line.
5. **`import tf_tree` does not re-enable the GIL**, asserted in CI.
6. **TSan clean** on the free-threaded build.
7. Zero leaks over 10⁶ calls.

## 13. Phase 4 handoff

Do not route Python through the C ABI. The `tf_tree.ros` submodule (lazy `rclpy` import, `Time` and `TransformStamped` conversion, `tf2_ros.Buffer`-shaped adapter) is Phase 4's, but **it must convert through integer nanoseconds** (§3), never `Time.to_sec()`. Carry §12's `tf2_ros` ratio into Phase 4's docs.

## 14. Definition of done

- [ ] `import tf_tree; tf_tree.open()` works with zero arguments, with and without a running arena
- [x] `#[pymodule(gil_used = false)]` set and asserted on `3.14t` (§1.2); every `#[pyclass]` is `Send + Sync`
- [~] No `float` stamp accepted anywhere; `TypeError` names `from_sec` and states the ULP — holds for every **scalar** float stamp, not for a float stamps **array** (§3)
- [ ] No API returns a view into the arena (grep-able review item, documented in the README)
- [x] `at_into` validates fully before writing; `out` classified via `__dlpack_device__`, CUDA device memory rejected; no hand-written DLPack capsule parsing anywhere
- [ ] `os.register_at_fork` poisoning tested under all three start methods
- [ ] Hand-written stubs; `mypy --strict` and `pyright --strict` clean over stubs and examples
- [x] CI asserts no generated-stub symbol is missing from the hand-written ones (§9)
- [x] Wheels for every row of §10 — two invocations each — with the `abi3.abi3t` job present and skipped (`.github/workflows/wheels.yml`)
- [ ] Toolchain floors of §10.1 pinned in `pyproject.toml`; `[tool.ruff.format] exclude` covers `**/*.md`; `.github/dependabot.yml` has its `uv` entry
- [~] **PEP 740 attestations are published; the SBOM is not** (`PHASE5.md` §10's *Not done* list)
- [ ] §12.2 gate met, or a written explanation of which criterion failed and by how much: **met**, **failed by a stated margin**, or **not producible on this host** (`docs/PHASE4.md` §9's equivalent box). Criterion 4 is the third; six of the seven criteria are still unmeasured
- [ ] `docs/PHASE4.md` written, carrying §13 forward with the measured numbers

## Appendix B — what is built, and what is unproven

Implemented and gated locally (`just py-test`, `py-test-freethreaded`, `py-lint`, `tsan`): everything in §4–§9 except the items below and §5.5 steps 2–4. Wheels: `cp314`, `cp314t`, and an `abi3-py39` wheel verified on 3.14.

Not implemented: `adaptive_into` (the Rust side returns slices borrowed from internal scratch, so it would copy and break §5.2's tier-2 "no copies"), and `tf_tree.ros` (Phase 4).

## Appendix A — measurements

CPython 3.12.3, x86-64, `-O2`; reproduce with `python xtask/pybench.py` (probe module under `bench/probe/`). Figures are §2's table plus the batch kernel curve: n=1 233.6 ns/sample; n=64 4.6; n=4096 1.7; n=65536 2.5.
