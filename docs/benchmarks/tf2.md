# tf_tree vs ROS 2 `tf2` — validation and benchmarks

What has been measured against ROS 2's `tf2`, how, and what the numbers support.
Every row names the command that produced it.

```bash
just tf2-differential   # correctness, synthetic fixture
just tf2-replay         # correctness, real recorded /tf stream
just tf2-bench          # performance, head-to-head, single-threaded
just tf2-scaling        # concurrent read scaling, 1/2/4/8 threads
just tf2-native-control # pure C++ tf2, no Rust and no FFI — the bias control
just footprint          # memory + instructions per lookup (no idle machine needed)
just shm-test           # multi-process gate: another process, bit-identical
just shm-scaling        # N reader PROCESSES on one shared arena (roofline)
just mp-bench           # N node-shaped consumers at a fixed rate (deployment)
just mp-bench-tf2       # the same, both engines, in the ROS container
just py-vs-tf2          # tf_tree's Python API vs tf2_ros's (PHASE3 §12.1)
just profile-lookup     # where the lookup spends itself, by file
```

All run in a container (`docker/tf2/`); no ROS install is needed on the host.
`just tf2-check` (fmt, clippy, unit tests of `tf_tree_tf2_sys` and of
`tf_tree_bench --features tf2`) covers the crate `just lint` and `just test`
cannot see, because it is excluded from the workspace.

## Setup

| | |
|---|---|
| **tf2 version** | ROS 2 Lyrical Luth (May 2026, LTS to May 2031) |
| **Platform** | Ubuntu 26.04 "Resolute", g++ 15.2 |
| **Bridge** | `tf_tree_tf2_sys` — `extern "C"` shim over `tf2::BufferCore` |
| **Interpolation** | `LerpSlerp` on both sides (tf2's policy) |

`tf2::BufferCore` links against `-ltf2` alone: no rclcpp, no DDS. Both engines
are plain in-process libraries driven from the same loop with the same inputs.
**No middleware is in any measurement** except the transport section.

## Correctness

Both engines get an identical topology and sample stream, then the same random
queries.

| Load | Queries scored | Max disagreement | Bound |
|---|---|---|---|
| Synthetic fixture (24 frames, depth 6) | 95,909 | **2.876e-15** | 1e-12 |
| Recorded `/tf` stream (10 frames, depth 3) | 50,000 | **6.665e-15** | 1e-12 |

Bound is [`PHASE1.md`](../PHASE1.md) §10.5's 1e-12 gate. On the recorded stream
the engines also declined **exactly the same queries** (`tf_tree-only 0,
tf2-only 0`). Disagreement is `max(rotation-angle error in radians, translation
error in metres)`, so a quaternion sign flip cannot pass.

`DiffReport::passed()` requires `compared > 0`, so an oracle that declined every
query fails rather than reporting `max_error` 0.0; the scored count is printed.

## Performance

Idle machine (load < 0.6), 8 logical CPUs / **4 physical cores** (2-way SMT),
AMD EPYC-Milan at 2445 MHz, no governor exposed. Criterion intervals ±0.2%.
This is **not** [`PHASE1.md`](../PHASE1.md) §11.3's go/no-go gate, which needs
dedicated core-pinned hardware.

### Measurement biases

| # | Bias | Cost charged to tf2 | Found by |
|---|---|---|---|
| 1 | `CString::new` x2 per call in the Rust binding | **63-65 ns (14-18%)** | code review |
| 2 | `const char*` -> two `std::string` temporaries at the C++ call site | **~17 ns (7%)** | native C++ control |
| 3 | Residual FFI boundary (cross-TU, no inlining, extra copy) | **45.3 ns (10%)** | native C++ control |
| 4 | `setTransform` authority passed as a string *literal* | **~8 ns + 1 malloc/free per publish** | `just footprint` |

Biases 1-3 favoured tf_tree; bias 4 favoured tf2. `setTransform` takes
`const std::string&`, so a 20-character literal (past libstdc++'s 15-byte SSO)
heap-allocates on every publish; a real broadcaster stores its authority once.
Fixed with a `static const std::string`: tf2 publish 123 -> 114 ns, push ratio
14.1x -> 12.1x.

Fixes for 1-2: `FrameName` owns a heap `std::string` on the C++ side and
`tft2_lookup_pre` passes it by `const&`, the call a native C++ user makes.
`tf2/lookupTransform_alloc` (the naive binding) and `tf2/shim_overhead` run on
every benchmark invocation as controls. Bias 3 is irreducible for any FFI
comparison, so **the single-threaded ratio is reported against native C++**.

**Where the 45.3 ns comes from.** tf2 at **498.2 ns through `tf_tree_tf2_sys`**
(Rust harness row of the bracket table below) against **452.9 ns called
natively** (C++ harness row): 45.3 ns, 10.0% of native. It is an unpaired
difference between two processes and carries this host's run-to-run spread; it
is documentary and is not what `ratio.rs`'s floor rests on. Independent order-
of-magnitude support: [`0022`](../decisions/0022-the-per-call-guard-and-the-unwatched-gate.md)
amendment 3 prices a non-inlined call boundary on this fixture at ~55 ns (our
own C ABI, not tf2's). It would be falsified by a paired run of the two
harnesses in one process, or any tf2 row pair on this fixture whose
binding-versus-native difference is not ~45 ns.

### Steady-state lookup

1024 queries per iteration, stamps swept across the history window.

| Load | tf_tree | tf2 (via bridge) | tf2 (native C++) | Ratio vs native |
|---|---|---|---|---|
| Fixture, depth 6 | 178 ns | 379 ns | - | 2.1x |
| Recorded stream, depth 3 | 94 ns | 292 ns | **253 ns** | **2.7x** |

The headline is **~2.7x**.

### The binding cuts both ways: a bracket

Ratios that put tf2 behind `tf_tree_tf2_sys` pay bias 3. `docker/tf2/native_ratio.sh`
runs **both engines in one C++ process**, tf2 called natively, `tf_tree` through
its C ABI as a shared library. An arena owner (`native_arena`) serves the fixture
over the rendezvous and dumps the identical `.tfstream` (`tft_tree_open` attaches
and cannot create, D18); both engines agree on all 256 stamps to **2.05e-15**
before either is timed. Rust-side `tf_tree` numbers use this workspace's
`[profile.release]` (`lto = "thin"`).

| Harness | tf_tree profile | tf_tree | tf2 | Ratio | Who pays the boundary |
|---|---|---|---|---|---|
| Rust (`ratio.rs`) | `release`, `lto = "thin"` | 201.5 ns (native Rust) | 498.2 ns (via binding) | **2.47×** | tf2, +10% |
| C++ (`native_ratio.cpp`) | `release`, `lto = "thin"` | 306.7 ns (via C ABI) | 452.9 ns (native) | **1.48×** | tf_tree, +52% |
| Neither, unpaired | `release`, `lto = "thin"` | 201.5 ns | 452.9 ns | **2.25×** | nobody |
| Neither, unpaired | `embedder`, `lto = false` | 244.2 ns | 439.2 ns | **1.80×** | nobody |

The first two bracket the truth; the third is the best point estimate for a
build like this workspace's (unpaired, so not gate material) and lands near the
2.7× recorded-stream row. The fourth is the build a consumer actually gets, and
does not clear 2.0.

`crates/tf_tree_bench/src/ratio.rs` gates on `FLOOR = 2.0`, bounded by
`UNBIASED_ESTIMATE = 2.25` — row 3, with **no binding on either arm**. The
binding price therefore appears in neither half of that quotient; row 1, which
carries it, is reported and never gated.

### The bracket's second axis: the consumer's profile

Cargo applies the top-level package's profile to the whole dependency graph, so
`cargo add tf_tree` + `--release` compiles the engine with no LTO.
`[profile.embedder]` is those defaults written out; thin LTO inlines `Plan::at`
across the crate boundary and `lto = false` does not (`docs/API.md` §2.3 item 3).

`just tf2-ratio-profiles` (`docker/tf2`, `taskset -c 2`, one session,
2026-08-15):

| build | `lto` | tf_tree | tf2 (via binding) | **paired ratio** | band |
|---|---|---|---|---|---|
| `[profile.release]` — what the gate is measured in | `"thin"` | 201.6 ns | 504.4 ns | **2.490×** | 2.452–2.547 |
| `[profile.profiling]` — control, inherits `release`, only debuginfo differs | `"thin"` | 200.4 ns | 494.7 ns | 2.468× | 2.408–2.485 |
| `[profile.embedder]` — cargo's release defaults, i.e. a consumer | `false` | 244.2 ns | 506.1 ns | **2.075×** | 2.063–2.080 |

The tf2 column is invariant (504.4 -> 506.1 ns, +0.34%; it is an `extern "C"`
call no Rust LTO can inline into) and the tf_tree column moves +21.1%.
`profiling` lands on the LTO arm, so the number tracks `lto`, not the profile's
name.

At `[profile.embedder]` the unbiased estimate is 439.2 / 244.2 = **1.80×**,
below the floor; the row still passes there at a paired 2.075×, on binding bias.
**The floor was not lowered**: `just tf2-bench-check` builds `--release` in this
workspace, so the gate is a regression detector for that build and not a
consumer-facing guarantee (`FLOOR`'s doc comment;
[`0025`](../decisions/0025-what-build-the-tf2-ratio-gate-speaks-for.md)). The
consumer-facing headline stays ~2.7×. A second gated row at `[profile.embedder]`
would need a decision record; `runstore::BUILD_CRITICAL_FACTS` already refuses to
score two profiles against one baseline.

### The C ABI's 52%

[`PHASE4.md`](../PHASE4.md) §7 gate 1 once recorded `tft_plan_at` at 1.020×
native Rust; that was Rust calling the ABI inside one build, where the linker
sees across the call. A C++ caller against `libtf_tree_c.so` pays 52% on the same
host and fixture.

> **What gates today** is [`0023`](../decisions/0023-the-gate-that-could-not-gate.md)'s
> re-cut, run by `just abi-cost` and recorded in [`EVIDENCE.md`](./EVIDENCE.md)'s
> `abi_cost` row: four quotients on one interleaved ladder at
> `[profile.embedder]` — R1 the ABI < 1.10 (measured **1.025–1.038**), R2 the
> panic guard < 1.05, R3 the per-call guard < 1.25, and a control at 1 ± 0.02.
> The ABI's own cost is about **+6 ns**. `PHASE4.md` §0.0 records the old
> single-quotient §7 criterion as NOT EVALUABLE; neither 1.020× nor the
> 1.34–1.46× that briefly replaced it is a statement about the ABI. The 52%
> below is a C++ caller against a *shared* arena, a different configuration.

#### Where the 52% actually goes: the C ABI's per-call work

The same `tests/cpp/bench.cpp` against `libtf_tree_c.a` and `libtf_tree_c.so`
measures **245.4 ns against 244.4 ns**: link mode is not the cause. Full ladder,
§11.1 fixture, same off-grid sweep, `imu_link ← map`:

| Rung | API | Arena | ns/lookup |
|---|---|---|---|
| H | native Rust | heap, in-process | 200.7 |
| S | native Rust | `MAP_SHARED` memfd, in-process RW | 203.2 |
| A | native Rust | memfd, **read-only, cross-process** | **202.5** |
| C | **C ABI** (`tft_plan_at`) | same arena as A | **302.0** |
| C′ | **C ABI** (`tft_plan_at_many`) | same arena as A | **261.0** |

The shared mapping costs **≤ 9.6 ns** (paired, nine runs, median quotient
1.0066–1.0112×, ~1.8 ns typical); attaching read-only from another process
costs **−0.7 ns**; the C ABI costs **+99.5 ns (+49%)** on the identical arena.

The cause is per-call work the Rust API lets you hoist: `tft_plan_at` builds a
`Guard` on every call inside a `catch_unwind`, where the Rust arm acquires one
guard for all 10,240 lookups. `tft_plan_at_many` pays it once per batch and
recovers **41 ns** of the 99.5. The `is_shared()` fork check in `Tree::guard` is
noise (+2.1 ns counters off, −8.4 ns on); the per-call guard costs ~17 ns on both
backings and Phase 5's counters roughly double it. The `Guard` constructor
allocates nothing and takes no lock — a single acquire load — so
[`API.md`](../API.md) §1 R2 is not violated
([`0022`](../decisions/0022-the-per-call-guard-and-the-unwatched-gate.md)
*Decision* item 4), and the `tft_guard` handle is declined.

The mapping-is-free claim rests on a run that interpolates. The 213-vs-217 ns
row in the multi-process section compares two harnesses unpaired (spread ~4%),
and `examples/heap_vs_shared` queries one exact grid hit
([`0013`](../decisions/0013-the-benchmark-gate-never-interpolated.md)'s defect).

**What a C++ embedder should do, in order of leverage:**

1. **Use `tft_plan_at_many` on any hot path** — 41 ns of the 99.5 at a batch of
   256. Sort the stamps.
2. **Do not switch link mode expecting a win** (within 0.4%).
3. A private (non-`shm`) arena does not pay this: `0023`'s ladder prices the
   whole C ABI at R1 = 1.025–1.038×, **~6–9 ns on a ~245 ns lookup**.

The C++ arm reads as what a C++ embedder gets today, not what the engine costs.

### Where the win comes from

tf_tree compiles the topology walk **once** into a `Plan` and thereafter only
samples ([`PROJECT.md`](../PROJECT.md) §5 D3); tf2 walks per call. Replanning
for every query:

| Load | tf_tree (plan reused) | tf_tree (replanned every query) | tf2 |
|---|---|---|---|
| Fixture, depth 6 | 178 ns | 361 ns | 379 ns |
| Recorded stream | 94 ns | 243 ns | 292 ns |

Plan reuse is worth ~2x, and tf_tree stays ahead of tf2 even without it.

### Scaling with tree size and depth

Deepest-pair lookup, 256 queries per iteration.

| Shape | tf_tree | tf2 | Ratio |
|---|---|---|---|
| 12 frames, depth 4 | 262 ns | 1377 ns | **5.3x** |
| 35 frames, depth 7 | 514 ns | 2592 ns | **5.0x** |
| 117 frames, depth 13 | 1031 ns | 5948 ns | **5.8x** |
| 375 frames, depth 15 | 1193 ns | 7337 ns | **6.2x** |

Both scale primarily with depth: 117 -> 375 frames costs tf_tree 16% and tf2 23%.

### Publish

| | tf_tree | tf2 | Ratio |
|---|---|---|---|
| One sample onto one edge | **9.4 ns** | 114 ns | **12.1x** |

tf_tree's publish is allocation-free; tf2 calls the allocator exactly once per
stored transform. The `push/tf2_alloc` row keeps the naive binding (which builds
a NUL-terminated copy of each name per call) as a control: **187 ns**. tf_tree's
ring is count-bounded (power-of-two slots, overwritten in place, never
allocating — invariant 8, enforced by the zero-allocation gate); tf2's cache is
time-bounded and prunes on insert at its 10 s default.

### Concurrent read scaling — 1 / 2 / 4 / 8 threads

tf_tree's readers take no lock; every `tf2::lookupTransform` acquires
`BufferCore`'s frame mutex. One shared tree and one shared buffer; 101 rounds per
point, engines interleaved within every round. The command (overrides go
*inside* the quoted string — `run.sh` does not forward the host environment;
bare `just tf2-scaling` runs 51 rounds / 50,000 samples, faster and noisier):

```bash
./docker/tf2/run.sh 'TF2_ROUNDS=101 TF2_LATENCY_SAMPLES=100000 \
  cargo run -p tf_tree_bench --features tf2 --release --bin tf2_scaling'
```

Million lookups/s, recorded stream; `spread` is `(best - median)/best`:

| Threads | tf_tree | spread | tf2 | spread | Ratio | tf_tree vs 1thr | tf2 vs 1thr |
|---|---|---|---|---|---|---|---|
| 1 | 12.70 | 1.9% | 3.61 | 1.0% | 3.5x | 1.00x | 1.00x |
| 2 | 23.42 | 8.8% | 1.82 | 1.8% | 12.9x | 1.84x | **0.50x** |
| 4 | 35.44 | 24.0% | 1.31 | 49.9% | 27.0x | 2.79x | **0.36x** |
| 8 | 68.02 | 29.1% | 1.13 | 4.5% | **60.3x** | 5.35x | **0.31x** |

tf_tree scales; **tf2 anti-scales**, the signature of a contended global mutex.
The 4-thread row is the noisiest here (every other row repeats within 1%): with 4
physical cores, 4 threads exactly matches the core count, so which SMT sibling
each thread lands on decides the result and nothing pins it.

**This is tf2's behaviour, not our binding's.** The pure C++ control
(`docker/tf2/native_scaling.sh`, sweeping exactly the Rust harness's
`common_window`), re-run back-to-back:

| Threads | native C++ tf2 M/s | via our bridge M/s | native vs 1thr | bridge vs 1thr |
|---|---|---|---|---|
| 1 | 3.80 | 3.66 | 1.00x | 1.00x |
| 2 | 1.84 | 1.83 | 0.48x | 0.50x |
| 4 | 1.38 | 1.39 | 0.36x | 0.38x |
| 8 | 1.12 | 1.12 | **0.30x** | **0.31x** |

Per-lookup latency, recorded stream:

| Threads | Engine | p50 | p99 | p99.9 | p99.99 |
|---|---|---|---|---|---|
| 1 | tf_tree | 110 ns | 141 ns | 179 ns | 6.5 us |
| 1 | tf2 | 291 ns | 852 ns | 1.2 us | 7.7 us |
| 8 | tf_tree | 151 ns | 220 ns | **331 ns** | 7.1 us |
| 8 | tf2 | 3.4 us | 47 us | **83 us** | 204 us |

At 8 threads p99.9 is 331 ns against 83 us — a factor of **252**. It does not
depend on core count or FFI residue, and it is the strongest result here. tf_tree's
tail stays bounded; tf2's degrades ~14x faster than its median, which is what a
convoy looks like.

**The scaling gate.** [`PHASE1.md`](../PHASE1.md) §11.3's third criterion is
"read throughput scales at least **6x** from 1 to 8 threads". **Measured
5.35x-5.62x: not met as written**, but not fairly evaluable on 4 physical cores,
where 8 threads exceed 4x only through SMT (tf_tree reached 2.79x-3.09x at 4
threads). Only a host with >= 8 physical cores settles it. The criterion's
purpose — that perception nodes stop contending — is answered by 0.31x for tf2.

## Memory and computation

`just footprint`. Unlike every timing row, **these need no idle machine**:
`cachegrind` and `memcheck` simulate, so counts are exact and reproducible, and
survive a change of CPU. Each engine runs in its **own process**, so one's freed
chunks cannot satisfy the other's requests. Memory is `mallinfo2`'s `uordblks +
hblkhd` (not RSS); `hblkhd` matters because tf_tree's arena is one allocation
above glibc's 128 KiB mmap threshold.

### Memory — identical topology, 10 s of history (12,600 samples)

| | tf_tree | tf2 |
|---|---|---|
| Heap held | 1,397,616 B | 1,421,392 B |
| Bytes per stored sample | 110.3 | **112.8** |
| Bytes per *declared slot* | **72.9** | n/a |
| Allocations to build | **108** | 88,459 |
| Allocations per published transform | **0** | 1.00 |
| Allocations per lookup | **0** | **0** |

**tf_tree is not meaningfully smaller — within 2.4%.**

#### With no binding on either side

`just tf2-native-footprint` (`docker/tf2/native_footprint.cpp`, a C++ program
linking only `libtf2`) against `footprint`'s `mem-tf_tree` mode, two processes,
same `.tfstream`:

| | tf_tree (Rust) | tf2 (native C++) | ratio |
|---|---|---|---|
| `heap_bytes` | 1 411 136 | 1 419 792 | 1.006 |
| bytes per stored sample | 111.2 | 112.7 | 1.013 |
| **`pss_kib` delta** | **1 272** | 1 332 | **1.047** |
| bytes per *declared slot* | **73.5** | n/a | — |

The binding did not inflate tf2's memory. `mallinfo2` cannot see residency;
[`0021`](../decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md)
over-allocates at 16 and aligns to 64 by hand so `calloc` returns
demand-faulted pages, saving 464 KiB of Pss (predicted 466 KiB: 6 472
never-published slots × 72 B) with `heap_bytes` unchanged. `heap_bytes` is exact
and bit-identical across runs; `pss_kib` is page-quantised, stable to ~3%
(1704–1752 over five runs).

tf_tree's rings are sized by *declared capacity*: `Capacity::history` rounds
each ring up to a power of two (1 kHz over 10 s asks for 10,000 slots, reserves
16,384). At 72.9 B/slot — a 64 B `PoseSlot` plus an 8 B stamp; the arena stores
the atomics the seqlock requires and an `Iso3` never enters it
([`0042`](../decisions/0042-the-cacheline-the-arena-never-asked-for.md)) —
tf_tree is 1.56x denser per unit of capacity, and this fixture's rounding (19 072
slots for 12 600 samples) hands almost all of that back. Right-sized it would
hold 73.5 B/sample against tf2's 112.7 (**1.53×**).

**The real difference is the write path.** Both engines are allocation-free per
lookup (given prebuilt string handles), but tf2 allocates and frees once per
published transform, forever: ten dynamic edges at 1 kHz is 10,000 malloc/free
pairs a second. tf_tree's lifetime allocations all precede the first lookup —
totals are **identical at N = 0 and N = 10 000** (108 for `lookup-tf_tree`, 94 for
`push-tf_tree`, which builds a one-dynamic-edge tree). tf2's one allocation per
transform is a per-frame node in a container that grows to fit.

### Computation — per lookup, three dynamic steps, 100 ms query window

Baseline-subtracted (mode `N=0` performs setup and no lookups).

| Per lookup | tf_tree `LerpSlerp` | tf_tree `ScLerp` | tf2 |
|---|---|---|---|
| Instructions | **2,072** | 2,863 | 4,083 |
| L1-D misses | **0.002** | 0.002 | **15.3** |
| LL-D misses | 0.00002 | 0.00003 | 0.0003 |
| Branch mispredicts | **7.70** | 7.70 | 14.00 |
| — of which *indirect* | **0.00002** | 0.00002 | **6.00** |

Against tf2 on the comparable policy: **1.97x fewer instructions, ~8,000x fewer
L1-D misses, and effectively zero indirect mispredicts against six.** The six are
virtual dispatch through `TimeCacheInterface`; a compiled `Plan` is a flat
`[Step; 16]` with no dynamic dispatch. Indirect targets are a structural property,
not a predictor artifact.

Caveats. LL-D misses are ~0 for both because the 1.4 MB working set fits in L3;
a tree that falls out of L3 would widen the gap (a prediction, not measured). The
instruction ratio (1.97x) is smaller than the wall-clock ratio (2.7x); the
remainder is likely mispredicts, caches and tf2's mutex, but attributing it
needs cycle counters this host does not permit (`perf_event_paranoid=4`).
Cachegrind's predictor is a simple two-level model, not a Zen 3 TAGE: compare
engines under it, do not read "7.70" as a real CPU's count.

99.97% of `sample.rs`'s mispredicts are in the bracket loop (`base += half * cmp`
and `while len > 1`, per-line via `just profile-lookup`). Replacing the multiply
with a mask (`half & (0 - cmp)`) moves simulated mispredicts 7.70 -> 7.32 and
wall-clock by -2.8% (depth 3 / sclerp), -1.5% (depth 3 / lerpslerp), -1.2%
(depth 6). It is a cheaper loop body, not an erased branch: LLVM folds the mask
back into a `select` and cmov-conversion expands it to a branch in every inlined
copy. [`0053`](../decisions/0053-the-branchless-bracket-that-branches.md) carries
the disassembly and the four spellings.

## Multi-process: the comparison tf2 cannot enter

`just shm-scaling` (Linux, `--features shm`); two back-to-back idle runs, rows
repeat within 1%. Robot software deploys as separate executables, so this is the
deployment shape, not the thread table.

**`tf2::BufferCore` has no shared-memory mode.** Each process runs its own
`tf2_ros::TransformListener`: N buffers, N deserialization pipelines, N-way DDS
fan-out, and copies that drift apart. tf_tree maps one arena N times.

| Processes | Aggregate M/s | ns/lookup | vs 1 proc | Unique resident | tf2 history would be *(arithmetic)* |
|---|---|---|---|---|---|
| 1 | 4.66 | 213 | 1.00x | 3.5 MiB | 1.4 MiB |
| 2 | 9.04 | 219 | 1.94x | 5.7 MiB | 2.7 MiB |
| 4 | 15.43 | 257 | 3.31x | 9.9 MiB | 5.4 MiB |
| 8 | 18.17 | 431 | 3.90x | 18.7 MiB | 10.8 MiB |

**Scaling is bounded by cores, not by design.** 4 processes × 213 ns is an
18.8 M/s roofline and the 8-process row measures 18.2. Eight processes on four
cores each get 50% of a core, so 213 ns should become 426 ns; **measured 431 ns**
— ~1% left for any cross-process cost. There is no lock to contend on. Multi-
process scaling at 4 (3.31x) beats multi-thread (2.79-3.09x): separate address
spaces share no allocator, TLS or false-shared lines.

The `tf2 history would be` column is **arithmetic**, `n × 1 421 392 B` (the
`footprint` figure multiplied out); no tf2 process ran behind it, and timing a
real `tf2_ros` listener would drag DDS in. **`Unique resident` is Pss-derived,
not RSS**, despite the `rss` metric ids in `scale_sweep.rs` and `soak.rs`, which
are frozen join keys for `bench_ab` and the baseline differ. Summed RSS
double-counts the arena; the remaining ~2.2 MiB per-process growth is
executable, stack and libc.

One unmeasured inference (`perf_event_paranoid` forbids the counters): N
processes touch the same cache lines, so the transform data's cache footprint is
independent of consumer count, where tf2's N private buffers are N × 1.4 MB
(5.6 MB at four, past many L3s); the 4-process row costing only 21% more per
lookup is consistent with it. The 213 ns figure against `cost_model`'s 217 ns is
unpaired; the paired shared-mapping measurement is `just abi-split` (above).

### The multi-process *node* evaluation — methodology

`just shm-scaling` is a roofline question. `just mp-bench` asks what N nodes,
each needing transforms at its own rate, experience and cost:

1. **Open loop, not a tight loop.** Tick `i` is due at `t0 + i/rate` and latency
   is measured from the *intended* time, so a stalled node reports the backlog
   instead of hiding it (coordinated omission).
2. **A publisher runs throughout**, so the seqlock retry path and
   `tf2::BufferCore`'s mutex are actually exercised.
3. **Two clocks.** `service` is work-start to done (engine cost); `cycle` is
   intended-tick to done (node experience). At 100 Hz `cycle` is ~95% OS wakeup.
4. **CPU per consumer**, testing §12.4's claim that tf_tree is O(1) in consumers
   where `/tf` is O(consumers × edges × rate).
5. **PSS, not summed RSS.**

**The tf2 column is a floor and must be labelled one.** Across processes the
transport *is* tf2's mechanism, so `mp-bench-tf2` measures N private
`BufferCore`s fed the identical stream, showing only the duplication; a deployed
consumer also pays a `TransformListener` and DDS fan-out. The harness refuses to
run on a busy machine, naming the top CPU consumers; each row carries its
foreign-load percentage and is `NOISY` above 10%.

### Results

`just mp-bench-tf2`, 2026-07-26. Same container and host: AMD EPYC-Milan, 4
physical cores / 8 SMT threads, `taskset -c 0-7`, 100 Hz × 6 s per point, 8
lookups per tick, depth-3 chain, publisher running throughout. Foreign load
1–7%; none `NOISY`. Times in microseconds.

**tf_tree**

| nodes | svc p50 | svc p99 | svc p99.9 | cyc p50 | cyc p99 | cyc p99.9 | CPU %/node | PSS MiB |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 3.22 | 7.04 | 11.65 | 67.6 | 87.0 | 198.7 | 0.139 | 4.00 |
| 2 | 2.53 | 6.27 | 122.37 | 66.0 | 93.2 | 432.1 | 0.120 | 4.82 |
| 4 | 2.30 | 5.50 | 23.30 | 66.0 | 87.6 | 331.8 | 0.119 | 6.36 |
| 8 | 2.11 | 4.86 | 16.64 | 64.8 | 88.1 | 2801.7 | 0.112 | 11.83 |
| 16 | 1.93 | 4.19 | 15.68 | 64.5 | 79.4 | 630.8 | 0.108 | 16.96 |

**tf2** (floor)

| nodes | svc p50 | svc p99 | svc p99.9 | cyc p50 | cyc p99 | cyc p99.9 | CPU %/node | PSS MiB |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 15.94 | 34.05 | 38.66 | 79.9 | 105.5 | 2129.9 | 0.254 | 5.25 |
| 2 | 14.78 | 28.29 | 34.05 | 78.3 | 108.5 | 655.4 | 0.232 | 7.26 |
| 4 | 14.91 | 27.39 | 46.08 | 77.8 | 106.0 | 1237.0 | 0.234 | 11.31 |
| 8 | 14.91 | 29.70 | 53.25 | 76.3 | 102.9 | 1097.7 | 0.226 | 18.57 |
| 16 | 14.72 | 29.18 | 64.26 | 76.8 | 101.9 | 704.5 | 0.229 | 41.12 |

- **Service latency: 5.0× at one node, 7.6× at sixteen** (15.94 -> 3.22 µs;
  14.72 -> 1.93 µs), 7.0× at p99. tf_tree's median falls with consumers while
  tf2's is flat; consumers do not make tf_tree slower, which is the deployment
  question.
- **CPU per node: 2.1×** at sixteen (0.229% vs 0.108%). tf_tree's *falls* from
  0.139% to 0.108% across 16× the consumers (`PHASE2.md` §12.4); tf2's is flat
  at ~0.23% because the floor has no `TransformListener` deserializing `/tf`.
- **Memory: 2.4× at sixteen** (41.12 vs 16.96 MiB PSS); per marginal node 2.39 vs
  0.86 MiB, **2.8×**. tf_tree's marginal megabyte is process overhead, not tree
  data.
- **Cycle latency is not an engine measurement**: both sit near the 100 Hz OS
  wakeup (65 vs 77 µs p50), and p99.9 is the scheduler running up to 16 processes
  on 4 cores.

One run per point: treat single-row differences under ~10% as noise; the trends
carry the weight. Ratios, not absolutes, should transfer to pinned hardware.
CPU is read from `/proc/<pid>/schedstat` (nanoseconds); 10 ms `utime + stime`
ticks read zero for a consumer spending ~4 ms of CPU per window.

### Where the lookup actually spends itself

`just profile-lookup`, cachegrind over 60 000 depth-3 lookups, by file. The
`profiling` profile (release codegen, debuginfo kept) makes this possible;
`[profile.release]` strips debuginfo and `fold_at` inlines the whole chain. The
containerised `just profile-lookup` and `just footprint` work per-line; host-side
`just profile-cachegrind` needs `valgrind` installed. `cg_annotate --auto=yes` is
deprecated for `--annotate` but identical.

| file | instructions | branch mispredicts |
|---|---:|---:|
| `tf_tree_core/src/sample.rs` (inlined into `fold_at`) | 27.1% | **76.8%** |
| `tf_tree_math/src/quat.rs` | ~19% | ~0% |
| `tf_tree_math/src/iso3.rs` | ~16% | ~0% |
| `tf_tree_core/src/buffer.rs` | ~13% | ~0% |
| `tf_tree_core/src/plan.rs` | ~9% | ~0% |

- Essentially every mispredict is in the sampling path, and the bracket search
  is **not** branchless: its index select compiles to a conditional branch
  ([`0053`](../decisions/0053-the-branchless-bracket-that-branches.md)).
- The maths is ~35% of instructions and mispredicts nothing: the shape that
  rewards SIMD and not branch work.

### Optimisations tried and rejected

**Constant divisions in `slerp_weight` -> reciprocal multiplies. No effect.**
Four non-power-of-two divisions per call (LLVM may not rewrite them without
fast-math) is 24 `divsd` per depth-3 lookup. Moving the reciprocals to `const`
measured `p = 0.74` (depth 3 / sclerp), `0.91` (depth 3 / lerpslerp), `0.67`
(depth 6): the divisions are independent of each other and of the Horner chain,
so they issue in parallel; the critical path is the dependent multiply-add
chain. `slerp_series_matches_exact_below_threshold` held at 1e-15 with the
reciprocals in place.

`depth1/sclerp` has twice shown a ~3% "regression" (p < 0.05) that did not
reproduce; treat single-run depth-1 results here as layout noise.

### Python: `tf_tree` against `tf2_ros`

`just py-vs-tf2`, 2026-07-26, same container. One dynamic edge, 2000 samples at
1 ms, queried inside the retained window. tf2 gets every in-process advantage:
`tf2_ros.Buffer` wraps the same `BufferCore` a real node uses, fed directly, no
DDS or `TransformListener`.

| | tf_tree | `tf2_ros` | ratio |
|---|---:|---:|---:|
| scalar lookup | **188.7 ns** | 12 182.3 ns | **64.5×** |
| batch, n = 64 (per sample) | **42.7 ns** | 12 477.5 ns | **292.0×** |
| batch, n = 4096 (per sample) | **34.9 ns** | 12 607.5 ns | **361.7×** |

tf2 has no batch API, so its per-sample figure is its scalar figure; the scalar
row is the like-for-like number. Both are release builds with LTO
(`tf_tree_py` is excluded from the workspace and carries its own
`[profile.release]`; without it the same runs read 253.9 ns scalar, 51.7
ns/sample at n = 4096).

Against `docs/PHASE3.md` §12.2's gate, on the identical fixture with
`benches/py_parity.rs` (native 36.3 ns/sample at n = 4096):

| gate | result | |
|---|---|---|
| 1. scalar `plan.at` p50 under 250 ns | 228.8 ns | **pass** |
| 2. `at_many` at n = 4096 within 1.3× of native | 0.93× | **pass** |
| 3. `at_into` eliminates the allocation | 8.1 µs/call saved at n = 4096 | **pass** |

`benches/py_parity.rs` exists because `benches/at_many.rs` uses the deep
mobile-robot fixture.

#### A 32-byte struct field cost 4%

Fork poisoning needs a `Guard` that refuses every evaluation without reading the
arena. An `Option<LookupError>` field (32 B, niche-packed) grew `Guard` from 48 to
80 B, on a struct built once per `at()` on the Python path:

| | `Guard` | Python scalar | native depth-3 |
|---|---:|---:|---:|
| before | 48 B | 178.6 ns | 64.1 ns |
| `Option<LookupError>` field | **80 B** | 196.3 ns | 64.6 ns |
| generation sentinel | 48 B | **188.7 ns** | **62.0 ns** |

Encoding "detached" as `generation == u64::MAX` costs nothing: `check_generation`
already loads `generation`. `a_generation_mismatch_is_never_mistaken_for_a_detached_guard`
builds the exact collision and fails against `DETACHED = 0` or `1`.

### Python, multi-process: N nodes on one arena against N private buffers

`just py-mp-bench`, 2026-07-26, 8 cpus, same container. Eight Python consumer
nodes at 100 Hz over a depth-3 chain, open loop, live publisher for tf_tree, PSS
not summed RSS (methodology: `crates/tf_tree_bench/src/mp.rs`).

| nodes | tf_tree svc p50 | `tf2_ros` svc p50 | ratio | tf_tree PSS | `tf2_ros` PSS |
|---:|---:|---:|---:|---:|---:|
| 1 | **2.2–2.9 µs** | 167–260 µs | 76–89× | 23.6 MiB | 54.2 MiB |
| 2 | **2.2–2.9 µs** | 155–226 µs | 69–78× | 40.1 MiB | 94.3 MiB |
| 4 | **2.6–3.0 µs** | 188–233 µs | 74–77× | 70.4 MiB | 170.5 MiB |
| 8 | **3.1–3.4 µs** | 233–374 µs | **75–110×** | 128.3 MiB | 320.9 MiB |

Latency is a range across two runs because the machine was not idle; the ratios
and memory columns did not move, and those carry the claims. Both engines pay
identically for the interpreter and numpy; the claim is the slope:

| marginal, per node | tf_tree | `tf2_ros` | ratio |
|---|---:|---:|---:|
| memory (PSS) | **14.9 MiB** | 37.7–38.0 MiB | 2.5× |
| CPU | **0.16–0.17 %** | 2.9–3.8 % | **18–22×** |
| time to first usable lookup | **0–1 ms** | 64–121 ms | ~70× |

The CPU row is `docs/PHASE2.md` §12.4's "O(1) in the number of consumers"
measured: tf_tree's per-node CPU is flat and tf2's rises. Each consumer measures
its own CPU across its own loop. Time to first lookup is structural: tf_tree
joins an arena already being published into (handshake, mapping, the pages §7.1
populates), while each tf2 node fills its own buffer first. tf2 is fed directly
with no transport, and a compiled `Plan` resolves the chain once where tf2
re-walks it per lookup — the difference §12.1 exists to report. At 8 cpus the
8-node row is where scheduling starts to dominate; `cycle p99.9` is reported and
not compared.

#### The consumer loop: `at_into` and the DLPack probe

Depth-3 chain, release, in-process:

| | ns |
|---|---:|
| empty Python loop | 5.6 |
| `plan.depth()` — PyO3 dispatch and nothing else | 15.6 |
| **native Rust `plan.at(&g, t)`** | **114** |
| `plan.latest()` | 118 |
| `plan.at(t)` | 211 |
| `plan.at_into(t, buf)`, as first written | **265** |

`Tree::guard()` costs 1.1 ns (`examples/guard_cost.rs`); the output allocation is
not the cost (an uninitialized `new` measured no change). `at_into` was slower
than `at` because `reject_device_memory` did `getattr("__dlpack_device__")` and
**called** it, ~120 ns per invocation. A successful cast to `numpy.ndarray`
proves the buffer is host memory (CuPy and torch arrays are not numpy
subclasses), so skipping the probe on that path leaves §5.5's guarantee intact
and takes `at_into` to **173 ns**, 1.3x faster than `at` and allocation-free.
The consumer loop moved from 1 923 to **1 321 ns** p50 at eight nodes (p99.9
29.3 -> 15.3 us): a **1.46x improvement**, same harness both sides.

### What is implemented

`just shm-test` is the gate: a **separate process**, after `exec`, maps the same
sealed `memfd` and answers **bit-identically** over 512 queries, plus a read-only
(`PROT_READ`) attachment and a check that samples published after a peer attached
are visible. The reader in that child is the unmodified Phase 1 reader
([`PHASE2.md`](../PHASE2.md) §4's "zero lines in the read path").

**Crash consistency: amendments A1–A8 are applied** (`FORMAT_VERSION = 2`): a
participant table (A6); claims name a participant *slot* not a PID (A3); `push`
re-checks the claim epoch (A4); topology generation and active block publish in
one atomic word (A1) under an in-arena reapable lock (A2); the sample writer
forces slot parity (A5); a full 16-byte boot id in the header (A7); bounded
interning spin with takeover of a provably-dead claimant (A8).

The lifecycle is complete: the `SOCK_SEQPACKET` + `SCM_RIGHTS` transport
(`crates/tf_tree_ipc/src/lib.rs:55`), `tf_tree::open()`
(`crates/tf_tree/src/open.rs:227`), liveness by OFD file lock via `fcntl` (a
`SIGSTOP`ped publisher keeps its claims, a dead one is reclaimed; `rustix` 1.1
has no OFD locking), and `Tree::reap_dead` / `Tree::reap_participant`.
`PHASE2.md` §0.0 is the authoritative status table.

## The performance suite: contention, scale, duration, and the transport

Everything above uses one 24-frame fixture, windows of a few seconds, a
quiescent tree and in-process `tf2::BufferCore`. Every number below is
**indicative**: this host fails `tf_tree_bench`'s `Fitness::probe` and every
harness says so. Curve shapes and ratios between rows taken minutes apart are
what these sections are for.

### Read scaling with concurrent writers — `just contended-scaling`

[`PHASE1.md`](../PHASE1.md) §11.2 specifies "1/2/4/8/16 reader threads, **4
concurrent writers**, cores pinned". `contended_scaling` runs N reader and M
writer *processes* on one shared arena, each placed on its own core by `taskset`
(per-thread placement needs `sched_setaffinity`, which the unsafe budget routes
to a decision record). 24 frames, 3 dynamic steps, 3 s per point, 8 logical CPUs:

| readers | writers | Mlookup/s | scale | svc p50 | svc p99 | svc p99.9 |
|---|---|---|---|---|---|---|
| 1 | 0 | 4.64 | 1.00x | 250 ns | 300 ns | 360 ns |
| 2 | 0 | 6.70 | 1.45x | 330 ns | 410 ns | 502 ns |
| 4 | 0 | 13.43 | 2.90x | 330 ns | 410 ns | 500 ns |
| 8 | 0 | 26.55 | **5.73x** | 330 ns | 410 ns | 520 ns |
| 1 | 4 | 4.38 | 0.94x | 270 ns | 330 ns | 400 ns |
| 2 | 4 | 6.22 | 1.34x | 350 ns | 422 ns | 480 ns |
| 4 | 4 | 12.33 | 2.66x | 360 ns | 420 ns | 470 ns |
| 8 | 4 | 24.09 | **5.20x** | 360 ns | 430 ns | 480 ns |

**Four concurrent writers cost about 9%** of aggregate throughput at 8 readers
and of p50 (330 -> 360 ns); the tail is flat. `err_slot_recycled +
err_slot_contended` was **0** on every row — those count reads that *failed*, not
retried; a successful seqlock retry is invisible to the arena and shows up in
the ~9%. The §11.3 scaling gate still fails here because 8 threads over 4
physical cores exceed 4x only via SMT; it now fails by a measured margin (5.20x)
under the load the gate specifies.

`svc` is a dense loop with one clock pair per lookup (clock cost 28–31 ns, reported
alongside); an open-loop schedule reported 61 µs p50 for a ~300 ns operation
because the OS scheduling term dominates. Cross-checks: the 1-reader/0-writer row
(250 ns) matches `scale_sweep`'s independent `robot` p50 (251 ns); and
`benches/read_scaling`'s `read_scaling_writers` group at nominal writer rates
shows no difference (925.9 vs 933.4 µs at 8 threads), because at 50–1000 Hz a
writer is invisible — hence a writer per core here.

### What a writer costs a reader — `TF2_WRITERS=N just tf2-scaling`

The in-process head-to-head (a second process cannot reach a `BufferCore`).
Writers publish to edges the query path does **not** traverse: `tf2::BufferCore`
takes one mutex for the whole buffer, so a write to any edge excludes every
reader; tf_tree's rings are per edge with a seqlock per slot. Two writers,
`fixture_depth6`, 15 rounds, engines interleaved:

| | tf_tree M/s | tf2 M/s | ratio |
|---|---|---|---|
| 1 thread, 0 writers | 6.66 | 3.00 | 2.2x |
| 1 thread, 2 writers | 6.63 | **0.62** | **10.7x** |
| 4 threads, 0 writers | 20.77 | 1.16 | 17.9x |
| 4 threads, 2 writers | 20.65 | **0.81** | **25.6x** |

| p50 / p99.9 | 0 writers | 2 writers |
|---|---|---|
| tf_tree, 1 thread | 190 ns / 281 ns | 191 ns / 310 ns |
| tf2, 1 thread | 431 ns / 741 ns | **2554 ns / 23 044 ns** |

**Two writers cost tf_tree's readers 0.5% of throughput and nothing at p50. They
cost tf2's readers 79% of throughput, 5.9x at p50 and 31x at p99.9.** The
recorded stream gives 16.6x at one thread and 43.0x at four. tf2's `worst` round
ratio falls to 0.54 at one thread (bursty; quote the median), and its throughput
*rising* from 1 to 4 threads under writers is not scaling: a lone reader loses
the mutex to writers more often than four collectively. The writers share one
stamp counter across passes; per-writer counters republished old stamps and
filled the window with `TF_OLD_DATA` stderr I/O.

### Scale — `just scale-sweep`

Lookup cost against tree **width** at fixed dynamic-step count:

| workload | frames | edges | dyn steps | at p50 | latest_common p50 | plan compile | build |
|---|---|---|---|---|---|---|---|
| `recorded` | 10 | 9 | 2 | 80 ns | 60 ns | 116 ns | 0.7 ms |
| `robot` | 24 | 23 | 3 | 251 ns | 100 ns | 144 ns | 2.7 ms |
| `humanoid` | 117 | 116 | 12 | 880 ns | 210 ns | 161 ns | 0.9 ms |
| `av` | 375 | 374 | 14 | 1012 ns | 231 ns | 171 ns | 2.1 ms |
| `fleet_16` | 385 | 384 | 4 | 330 ns | 120 ns | 238 ns | 44 ms |
| `fleet_64` | 1537 | 1536 | 4 | 330 ns | 120 ns | 245 ns | 120 ms |
| `extreme_wide` | 12289 | 12288 | 4 | 320 ns | 111 ns | 244 ns | 364 ms |

**Width is free**: 32x the tree at 4 dynamic steps does not move `at p50`.
**Depth is what costs**: `humanoid` to `av` is 880 -> 1012 ns, ~66 ns per
additional dynamic step ([`PHASE1.md`](../PHASE1.md) §11.3). Plan compilation
walks to the root, so it scales with depth (116 -> 244 ns). `build` scales
linearly in samples; 364 ms for 12 289 frames is a startup cost.

Ring depth, one edge, stamps swept across the ring:

| slots | retained | MiB | at p50 | at p99.9 |
|---|---|---|---|---|
| 8 | 7 | 0.0 | 90 ns | 101 ns |
| 1 024 | 1 023 | 0.1 | 90 ns | 150 ns |
| 16 384 | 16 383 | 1.1 | 91 ns | 160 ns |
| 262 144 | 262 143 | 18.0 | 119 ns | 450 ns |
| 1 048 576 | 1 048 575 | 72.0 | 120 ns | 700 ns |

A 131 072x deeper ring costs 30 ns at p50, 20 of them between 16 K and 256 K
slots (cache-resident to not); the tail p99.9 goes 101 -> 700 ns.

Publish, one thread round-robin over N edges: 6.84 ns/push at 1 edge, 7.01 at 16,
7.41 at 64, 10.33 at 256. The step at 256 is the working set (18 MiB of
first-touched pages), not false sharing: `EdgeCounters` is padded to 128 bytes.

**Limits:** **59 651 678 sample slots (~4.00 GiB) in one arena**, past which
`LayoutError::ArenaTooLarge` (every region offset is a `u32`); it binds first on
any populated tree. 32 compiled plan steps (`MAX_DEPTH`) and 64 raw path edges
(`MAX_PATH_EDGES`), below.

### Duration — `just soak`

40 s, 24-frame fixture (10 s retained), 2 reader and 4 writer threads, snapshots
every 10 s:

| interval | Mlookup/s | p50 | p99.9 | publish→visible p50 | ring laps | Pss | declined |
|---|---|---|---|---|---|---|---|
| 0 | 5.18 | 280 ns | 470 ns | 191 ns | 1.0 | 2636 KiB | 131 ppm |
| 1 | 5.22 | 280 ns | 460 ns | 191 ns | 1.0 | 2636 KiB | 130 ppm |
| 2 | 5.04 | 280 ns | 470 ns | 200 ns | 1.0 | 2656 KiB | 137 ppm |
| 3 | 5.24 | 280 ns | 470 ns | 191 ns | 1.0 | 2656 KiB | 130 ppm |

No drift: p99.9 ends at 1.00x its first interval and Pss grows 20 KiB. The
harness **asserts** the rings lapped (laps are `interval / retained`, both read
from the arena), because a soak that never lapped a ring must fail rather than
print a clean table. `declined` is the harness's own rate (queries aimed at the
oldest end land just below the sliding window); stable ~130 ppm is the expected
shape.

**Publish-to-visible is ~190 ns at p50** ([`PHASE5.md`](../PHASE5.md) §9.2's
required row): how long after a writer's `push` returns a different thread can
read the sample. Its p99.9 (milliseconds) is the scheduler descheduling the probe
reader, one of six runnable threads on four cores, not a claim about the engine.

### The transport — `just dds-bench`

Every comparison above feeds `tf2::BufferCore` in-process, generous to tf2. This
run pays the transport: one publisher, real DDS, the container's RMW,
[`PHASE4.md`](../PHASE4.md) §5.2's QoS, 4 consumers, 100 Hz, 100 ms query lag,
3 s warm-up discarded, **15 s measured** (`SECONDS_MEASURED`), both arms on stock
defaults.

| arm | procs | consumers | svc p50 | svc p99 | svc p99.9 | CPU %/consumer | PSS |
|---|---|---|---|---|---|---|---|
| `tf2.processes` | 4 | 4 | 2.59 µs | 8.64 µs | 12.16 µs | 2.968% | 63.15 MiB |
| `tf2.composed` | 1 | 4 | 1.43 µs | 6.21 µs | 10.50 µs | 0.644% | **24.04 MiB** |
| `tf_tree.composed` | 1 | 4 | **0.77 µs** | **3.62 µs** | **6.18 µs** | 0.656% | 24.76 MiB |
| `tf_tree.processes` | 5 | 4 | 0.90 µs | 8.96 µs | 16.90 µs | **0.725%** | 69.51 MiB |

`procs` counts the bridge (an operator supervises it), so `tf_tree.processes`
reads 5. All arms are the same executable with a different `--mode`; the publisher
plan, bridge topology config and query set are *generated* from one workload
entry, so §9.3's "identical data" is structural.

Against the ordinary deployment (`tf2.processes`): **3.4x on p50** for the
composed arm (2.59 / 0.77) and **4.1x on CPU** for the multi-process one
(2.968 / 0.725). **The 4.1x is the conservative pairing**: it divides the
*lowest* of four `tf2.processes` CPU samples (2.968 / 3.064 / 5.163 / 3.703, a
74% spread) by the tf_tree row (0.725 / 0.724 / 0.728 / 0.710, a 2.5% spread);
the medians give 4.7x. Take 4.1x as the floor of a one-host, four-run estimate.
The multi-process arm's own p50 ratio is 2.9x (2.59 / 0.90), the weaker number
for the wake-from-idle reason below.

`tf2.composed` is tf2's *best* case (one listener shared by four threads) and is
there so the comparison is not a strawman. Against it `tf_tree.composed` leads at
every percentile: **1.9x at p50** (1.43 / 0.77), **1.7x at p99** (6.21 / 3.62),
**1.7x at p99.9** (10.50 / 6.18), at comparable memory.

CPU is `CLOCK_PROCESS_CPUTIME_ID` in `measure.hpp`. `/proc/self/schedstat` is the
**main thread's** file and reads a sleeping thread when work runs elsewhere;
`mp.rs`'s sum over `/proc/self/task/*` is exact only while every thread lives,
and collapses after `join()`; `/proc/self/stat` (`utime + stime`) is always
correct at 10 ms quantisation. `clock_gettime` needs `unsafe`, which
`tf_tree_bench` forbids, so `mp.rs` reads `stat` alongside the task sum and
returns `stat` when it exceeds the sum by more than two ticks.
`mp::tests::cpu_time_survives_a_thread_exiting` pins it.

#### The fourth arm — `tf_tree.processes`

[`0015`](../decisions/0015-the-bridge-fills-a-shared-arena.md): one
`bench_consumer --mode tf_tree_bridge` process publishes its arena under
`$TF_TREE_NAME`, four `--mode tf_tree_attach` processes join it read-only with
`tft_tree_open()`, and none subscribes to `/tf` — §9.1's "one bridge plus N
`tf_tree` consumers". **The bridge's cost is inside the row**: it emits the same
stats block with `consumers 0`, and the aggregator sums CPU and PSS across an arm
and divides by the summed consumer count. Over a 15 s window:

| | fixed | per consumer | 4 consumers | 16 consumers (extrapolated) |
|---|---|---|---|---|
| `tf2.processes` CPU | — | 0.445 s | 1.78 s | 7.12 s |
| `tf_tree.processes` CPU | 0.362 s (bridge) | 0.0186 s | 0.436 s | 0.66 s |

**A marginal tf_tree consumer costs about 24x less CPU than a marginal tf2 one**,
and the bridge's 0.362 s is paid once whatever N is; break-even against tf2 is
roughly one consumer. This is `PROJECT.md`'s O(1)-in-consumers argument measured
end to end over a real DDS. All figures are one run of four processes on one
host: two significant figures.

**Where it is worse, at N = 4.**

*Memory.* 69.51 MiB against 63.15: the arena is 1.3 MiB and shared; what
dominates is that each of five processes carries an rclcpp node and a DDS
participant. A per-consumer PSS quotient does not compare across these arms,
because PSS divides a shared page by the number of mapping processes (4 vs 5),
crediting tf_tree before any architectural difference exists. Totals are fair.

*Tail latency.* `tf_tree.processes` loses to `tf2.processes` at **p99 (8.96 µs
against 8.64)** and **p99.9 (16.90 against 12.16)**
([`PHASE5.md`](../PHASE5.md) §9.3 requires this in the table); the section below
argues both are unpinned-host idle behaviour, which is why they need pinned
hardware before anyone quotes them in either direction.

### The memory curve

`CONSUMERS=8`, `12`, `16`, one run each:

| N | `tf2.processes` | `tf_tree.processes` | delta | tf2 per consumer | tf_tree per consumer |
|---|---|---|---|---|---|
| 4 | 63.15 MiB | 69.51 MiB | **+6.36** | 15.79 | 17.38 |
| 8 | 113.96 MiB | 113.80 MiB | −0.16 | 14.25 | 14.23 |
| 12 | 167.41 MiB | 168.39 MiB | +0.97 | 13.95 | 14.03 |
| 16 | 226.59 MiB | **219.06 MiB** | **−7.54** | 14.16 | 13.69 |

The sign flips between N = 4 and N = 8. The stacks are **indistinguishable from
N = 8 to 12**, and the first difference clearly outside single-run spread is N =
16's 7.54 MiB (**3.3%**); the delta is not monotonic, so read the per-consumer
columns. tf_tree's marginal consumer falls (17.38 -> 13.69 MiB) while tf2's stays
~14.2: one fixed extra process (the bridge, with its own rclcpp node and DDS
participant) amortised, against a marginal consumer that holds no per-node
history. A fit `total(P) = P·private + shared` predicted a crossover at ~7.4
consumers; the location held, its implied magnitude did not. Repeat each N
before quoting the number.

**None of this is an arena difference.** The `composed` arms put both stacks in
one process, and there tf_tree is worse by **+1.04, +0.80, +0.75 MiB** at N = 8,
12, 16: that ~1 MiB is the arena. Everything else is rclcpp and DDS, paid
identically per process; the comparison is dominated by process count, and
reporting it as an arena result is the error `bridge_supervision` names. `robot`
has 23 edges, so the tf2 `Buffer` replaced is small; a tree with thousands of
edges of history is not measured. The attach consumers hold an rclcpp node on
purpose, subscribed to nothing: dropping it would remove ~14 MiB per process and
measure "no rclcpp", not "no `/tf`".

#### The `svc` column of both `.processes` arms is wake-from-idle-dominated

`tf_tree.processes` p50 measured **5.89 µs** on the first run of the day and
**0.90, 0.95, 1.02 µs** on three consecutive ones; the slow run is bimodal (p10
0.79 µs, 18.1% under 1 µs, p75 10.05), a thread waking a cold core. Same bridge,
same shared arena, same rate, but four query threads in **one** attach process
(never idle) measured **1.00 µs** against `tf_tree.composed`'s 0.78 on a private
heap arena, ruling out the memfd mapping, page size and attach path. The arm is
penalised for the consumer doing so little that its core sleeps; `tf2.processes`
swings less (2.59 -> 11.07 µs) because a listener is always deserializing `/tf`.

Both `.processes` latency percentiles need the pinned-hardware runbook before
quoting. **The `tf_tree.processes` CPU and PSS columns are steady** (0.710–0.728
%/consumer); the `tf2.processes` CPU column is not (2.968–5.163), which is why
4.1x is quoted from the pairing that flatters tf2.

#### A bridge defect this harness found — and fixed

The first tf_tree run reported 10 070 transforms received, 187 applied, 9 864
dropped as authority conflicts and 100% of lookups failing, against one
publisher and a correct topology. `tf_tree_bridge::Publisher` was keyed on the
resolved *node name*, and `rmw_fastrtps` reports `_NODE_NAME_UNKNOWN_` for an
endpoint discovered before its node information arrives, so one publisher was
attributed under two names and `first_writer_wins` rejected the real one
permanently. [`PHASE4.md`](../PHASE4.md) §5.3 already makes the GID the identity:
`Publisher` now carries the GID as identity with the node name as presentation,
and `PartialEq`/`Ord`/`Hash` are hand-written to read the identity alone.
`Publisher::UnknownGid` is no longer a unit variant (on an RMW that reports GIDs
but resolves no names every publisher compared equal and §5.4's conflict
detection was off); a publisher with **no GID at all** stays the unit
`Unattributed`, because `0012`'s ladder requires that less attribution mean less
detection and never more stopping. Two tests in
`crates/tf_tree_c/tests/bridge.rs` gate it (mutant-checked), and the arm now runs
**0 dropped of 16 373 transforms** under the default policy. The aggregator
flags a row whose lookups mostly failed instead of printing its latencies.

## A real difference: maximum chain depth

tf_tree bounds a path twice; **tf2 bounds it not at all.**

* `tf_tree_core::MAX_PATH_EDGES` (**64**) caps the *raw walk* — every edge
  between the two frames, up to their common ancestor and back down.
* `tf_tree_core::MAX_DEPTH` (**32**) caps the *compiled plan*, counted **after**
  adjacent rigid links fold into one step. A 40-link fixed chain is one step.

Either overrun is `LookupError::TreeTooDeep`; its `depth` is `MAX_PATH_EDGES + 1`
for the walk, else the exact folded step count. The fixed `[Step; MAX_DEPTH]`
array is what makes `Plan` `Copy`, heap-free and allocation-free.
[`0034`](../decisions/0034-the-depth-bound-priced-two-slots-the-same.md) split
the two and re-sized both; the survey behind the sizes is in
[`PHASE1.md` §7.1](../PHASE1.md#71-step-representation).

**If you are migrating from tf2, the number to compare is your worst frame
pair's diameter — up to the common ancestor and back down — not your tree's
root-to-leaf depth.** Past 64 edges, or past 32 steps once fixed joints are
declared static, tf_tree refuses the lookup.

## Data provenance

The recorded stream is derived from an indoor/outdoor mobile-robot dataset under
**CC BY 4.0** (DOI [10.5281/zenodo.19894190](https://doi.org/10.5281/zenodo.19894190));
see [`testdata/tfstream/ATTRIBUTION.md`](../../testdata/tfstream/ATTRIBUTION.md).
Licensing bound the choice: KITTI, nuScenes, Newer College and Boreas are CC
BY-**NC**-SA, and Autoware's and TUM RGB-D state no clear license.

## What is still not measured

* **The go/no-go latency gate** (depth-3 p50 < 150 ns ScLerp / < 100 ns
  LerpSlerp) needs dedicated, core-pinned hardware; a mean over a loop on a
  shared VM is not a p50 on isolated cores.
* **The 6x scaling gate on >= 8 physical cores.** Measured 5.35x-5.62x on 4.
* **A non-SMT machine.**
* **Per-thread core pinning.** `contended_scaling` pins one process per core; the
  criterion benches pin nothing, and per-thread placement needs
  `sched_setaffinity`, routed by the unsafe budget to a decision record.
* **tf2 under writers on the *queried* edges.** The sweep writes edges the query
  path does not traverse; writing on path also slides the queried window and
  needs the moving-window handling `contended_scaling` has and `tf2_scaling` does
  not.
* **tf_tree over DDS on pinned cores.** Both `.processes` arms' `svc`
  percentiles are wake-from-idle-dominated on this host.
* **The memory curve's spread.** Every point is one run and the delta is not
  monotonic; repeat each N before quoting N = 16's 7.54 MiB.
* **A second RMW.** `docker/tf2` carries one; [`PHASE4.md`](../PHASE4.md) §0.0
  records the gap.
* **An ingest-throughput benchmark.** [`PHASE5.md`](../PHASE5.md) §12 gate 5; an
  offline path, deliberately not folded into this suite.

## Runbook for pinned hardware

```bash
# Isolate cores at boot: isolcpus=2-7 nohz_full=2-7 rcu_nocbs=2-7
sudo cpupower frequency-set -g performance
echo 0 | sudo tee /sys/devices/system/cpu/cpufreq/boost
echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid

just tf2-bench          # or, pinned:
taskset -c 2 ./docker/tf2/run.sh \
  'cargo bench -p tf_tree_bench --features tf2 --bench tf2_compare'
```

Report p50/p99/p99.9, not means ([`PHASE1.md`](../PHASE1.md) §11.2). The
performance suite runs the same way, and its harnesses **refuse** on a busy
machine:

```bash
just contended-scaling --workload robot --seconds 8   # §11.2's row, pinned
just scale-sweep                                      # width, depth, ring, fan-out, limits
just soak-long                                        # 30 minutes, fails on drift
just dds-bench 'CONSUMERS=16 SECONDS_MEASURED=120'    # end to end, in the container
```

To find out whether a change to the core helped:

```bash
just bench-run robot          # writes target/bench-runs/<sha>/
# ... change the engine ...
just bench-run robot
just bench-ab target/bench-runs/<a>/contended_scaling.json \
               target/bench-runs/<b>/contended_scaling.json
```

`bench_ab` reads direction and tolerance from the file, and exits non-zero on a
regression. Two runs of the same build must report every row as `noise`; that
property is checked.
