# tf_tree vs ROS 2 `tf2` — validation and benchmarks

Every row names the command that produced it; all run in a container
(`docker/tf2/`). `just tf2-check` covers `tf_tree_tf2_sys`, which the workspace
excludes.

```bash
just tf2-differential   # correctness, synthetic fixture
just tf2-replay         # correctness, real recorded /tf stream
just tf2-bench          # performance, head-to-head, single-threaded
just tf2-scaling        # concurrent read scaling, 1/2/4/8 threads
just tf2-native-control # pure C++ tf2, no Rust and no FFI
just footprint          # memory + instructions per lookup
just shm-test           # multi-process gate: another process, bit-identical
just shm-scaling        # N reader PROCESSES on one shared arena
just mp-bench           # N node-shaped consumers at a fixed rate
just mp-bench-tf2       # the same, both engines, in the ROS container
just py-vs-tf2          # tf_tree's Python API vs tf2_ros's (PHASE3 §12.1)
just profile-lookup     # where the lookup spends itself, by file
```

## Setup

| | |
|---|---|
| **tf2 version** | ROS 2 Lyrical Luth |
| **Platform** | Ubuntu 26.04 "Resolute", g++ 15.2 |
| **Bridge** | `tf_tree_tf2_sys` — `extern "C"` shim over `tf2::BufferCore` |
| **Interpolation** | `LerpSlerp` on both sides (tf2's policy) |

Both engines are in-process libraries driven from the same loop; no middleware is
in any measurement except the transport section.

## Correctness

Identical topology and sample stream, then the same random queries. Disagreement
is `max(rotation-angle error in radians, translation error in metres)`.

| Load | Queries scored | Max disagreement | Bound |
|---|---|---|---|
| Synthetic fixture (24 frames, depth 6) | 95,909 | **2.876e-15** | 1e-12 |
| Recorded `/tf` stream (10 frames, depth 3) | 50,000 | **6.665e-15** | 1e-12 |

Bound is [`PHASE1.md`](../PHASE1.md) §10.5. `DiffReport::passed()` requires
`compared > 0`.

## Performance

Host: 8 logical CPUs / **4 physical cores** (2-way SMT), AMD EPYC-Milan at
2445 MHz, no governor exposed, idle. This is **not**
[`PHASE1.md`](../PHASE1.md) §11.3's go/no-go gate, which needs core-pinned
hardware.

### Measurement biases

| # | Bias | Cost charged to tf2 | Found by |
|---|---|---|---|
| 1 | `CString::new` x2 per call in the Rust binding | **63-65 ns (14-18%)** | code review |
| 2 | `const char*` -> two `std::string` temporaries at the C++ call site | **~17 ns (7%)** | native C++ control |
| 3 | Residual FFI boundary (cross-TU, no inlining, extra copy) | **45.3 ns (10%)** | native C++ control |
| 4 | `setTransform` authority passed as a string *literal* | **~8 ns + 1 malloc/free per publish** | `just footprint` |

Biases 1-3 favoured tf_tree; bias 4 favoured tf2. Biases 1, 2 and 4 are fixed;
bias 3 is irreducible for any FFI comparison, so **the single-threaded ratio is
reported against native C++**.

**Where the 45.3 ns comes from.** tf2 at **498.2 ns through `tf_tree_tf2_sys`**
(Rust harness row below) against **452.9 ns called natively** (C++ harness row):
an unpaired difference, documentary, not what `ratio.rs`'s floor rests on.

### Steady-state lookup

1024 queries per iteration, stamps swept across the history window.

| Load | tf_tree | tf2 (via bridge) | tf2 (native C++) | Ratio vs native |
|---|---|---|---|---|
| Fixture, depth 6 | 178 ns | 379 ns | - | 2.1x |
| Recorded stream, depth 3 | 94 ns | 292 ns | **253 ns** | **2.7x** |

The headline is **~2.7x**. Replanning every query costs tf_tree 361 ns (fixture)
and 243 ns (recorded): plan reuse is worth ~2x. Deepest-pair lookup, 12 frames /
depth 4: 262 ns against 1377 ns (**5.3x**); 375 frames / depth 15: 1193 ns against
7337 ns (**6.2x**). Publish: **9.4 ns** against 114 ns (**12.1x**); tf_tree's
publish is allocation-free (invariant 8, enforced by the zero-allocation gate).

### The binding cuts both ways: a bracket

`docker/tf2/native_ratio.sh` runs **both engines in one C++ process**, tf2 called
natively, `tf_tree` through its C ABI, served by an arena owner (`native_arena`).
Both engines agree on all 256 stamps to **2.05e-15** before either is timed.

| Harness | tf_tree profile | tf_tree | tf2 | Ratio | Who pays the boundary |
|---|---|---|---|---|---|
| Rust (`ratio.rs`) | `release`, `lto = "thin"` | 201.5 ns (native Rust) | 498.2 ns (via binding) | **2.47×** | tf2, +10% |
| C++ (`native_ratio.cpp`) | `release`, `lto = "thin"` | 306.7 ns (via C ABI) | 452.9 ns (native) | **1.48×** | tf_tree, +52% |
| Neither, unpaired | `release`, `lto = "thin"` | 201.5 ns | 452.9 ns | **2.25×** | nobody |
| Neither, unpaired | `embedder`, `lto = false` | 244.2 ns | 439.2 ns | **1.80×** | nobody |

`ratio.rs` gates on `FLOOR = 2.0`, bounded by `UNBIASED_ESTIMATE = 2.25` (row 3,
no binding on either arm). Row 1 carries the binding price and is never gated.

### The bracket's second axis: the consumer's profile

A consumer's `--release` build has no LTO; `[profile.embedder]` writes those
defaults out (`docs/API.md` §2.3 item 3). `just tf2-ratio-profiles`
(`taskset -c 2`, one session):

| build | `lto` | tf_tree | tf2 (via binding) | **paired ratio** | band |
|---|---|---|---|---|---|
| `[profile.release]` — what the gate is measured in | `"thin"` | 201.6 ns | 504.4 ns | **2.490×** | 2.452–2.547 |
| `[profile.profiling]` — control, inherits `release`, only debuginfo differs | `"thin"` | 200.4 ns | 494.7 ns | 2.468× | 2.408–2.485 |
| `[profile.embedder]` — cargo's release defaults, i.e. a consumer | `false` | 244.2 ns | 506.1 ns | **2.075×** | 2.063–2.080 |

At `[profile.embedder]` the unbiased estimate is 439.2 / 244.2 = **1.80×**, below
the floor, which was not lowered: the gate is a regression detector for the
`--release` workspace build
([`0025`](../decisions/0025-what-build-the-tf2-ratio-gate-speaks-for.md)). The
consumer-facing headline stays ~2.7×.

### The C ABI's 52%

> **What gates today** is [`0023`](../decisions/0023-the-gate-that-could-not-gate.md)'s
> re-cut, run by `just abi-cost` and recorded in [`EVIDENCE.md`](./EVIDENCE.md)'s
> `abi_cost` row: R1 the ABI < 1.10 (measured **1.025–1.038**), R2 the panic
> guard < 1.05, R3 the per-call guard < 1.25, and a control at 1 ± 0.02. The
> 52% below is a C++ caller against a *shared* arena, a different configuration.

#### Where the 52% actually goes: the C ABI's per-call work

§11.1 fixture, off-grid sweep, `imu_link ← map`; link mode is not the cause
(245.4 against 244.4 ns, static against shared):

| Rung | API | Arena | ns/lookup |
|---|---|---|---|
| H | native Rust | heap, in-process | 200.7 |
| A | native Rust | memfd, **read-only, cross-process** | **202.5** |
| C | **C ABI** (`tft_plan_at`) | same arena as A | **302.0** |
| C′ | **C ABI** (`tft_plan_at_many`) | same arena as A | **261.0** |

The shared mapping costs **≤ 9.6 ns**; the C ABI **+99.5 ns (+49%)** on the
identical arena. The cause is per-call work the Rust API lets you hoist:
`tft_plan_at` builds a `Guard` on every call inside a `catch_unwind`;
`tft_plan_at_many` pays it once per batch and recovers **41 ns**. The `Guard`
constructor allocates nothing and takes no lock, so [`API.md`](../API.md) §1 R2
holds ([`0022`](../decisions/0022-the-per-call-guard-and-the-unwatched-gate.md)
*Decision* item 4), and the `tft_guard` handle is declined. C++ embedders: use
`tft_plan_at_many` on any hot path.

### Concurrent read scaling — 1 / 2 / 4 / 8 threads

One shared tree and buffer; 101 rounds per point, engines interleaved. Overrides
go *inside* the quoted string (`run.sh` does not forward the host environment;
bare `just tf2-scaling` runs 51 rounds / 50,000 samples): `./docker/tf2/run.sh 'TF2_ROUNDS=101 TF2_LATENCY_SAMPLES=100000 cargo run -p tf_tree_bench --features tf2 --release --bin tf2_scaling'`.

Million lookups/s, recorded stream; `spread` is `(best - median)/best`:

| Threads | tf_tree | spread | tf2 | spread | Ratio | tf_tree vs 1thr | tf2 vs 1thr |
|---|---|---|---|---|---|---|---|
| 1 | 12.70 | 1.9% | 3.61 | 1.0% | 3.5x | 1.00x | 1.00x |
| 4 | 35.44 | 24.0% | 1.31 | 49.9% | 27.0x | 2.79x | **0.36x** |
| 8 | 68.02 | 29.1% | 1.13 | 4.5% | **60.3x** | 5.35x | **0.31x** |

tf2 anti-scales (every `lookupTransform` takes `BufferCore`'s frame mutex). The
pure C++ control (`docker/tf2/native_scaling.sh`) shows this is tf2, not the
binding: native 3.80 -> 1.12 M/s (**0.30x**) at 8 threads against the bridge's
3.66 -> 1.12 (**0.31x**). At 8 threads p99.9 is 331 ns against 83 us.

**The scaling gate.** [`PHASE1.md`](../PHASE1.md) §11.3's third criterion is
"read throughput scales at least **6x** from 1 to 8 threads". **Measured
5.35x-5.62x: not met as written**, and not fairly evaluable on 4 physical cores
(tf_tree reached 2.79x-3.09x at 4 threads); only a host with >= 8 physical cores
settles it.

## Memory and computation

`just footprint`: `cachegrind` and `memcheck` simulate, so counts are exact and
need no idle machine. Each engine runs in its own process. Memory is
`mallinfo2`'s `uordblks + hblkhd` (not RSS).

### Memory — identical topology, 10 s of history (12,600 samples)

| | tf_tree | tf2 |
|---|---|---|
| Heap held | 1,397,616 B | 1,421,392 B |
| Bytes per stored sample | 110.3 | **112.8** |
| Bytes per *declared slot* | **72.9** | n/a |
| Allocations to build | **108** | 88,459 |
| Allocations per published transform | **0** | 1.00 |
| Allocations per lookup | **0** | **0** |

tf_tree is not meaningfully smaller — within 2.4%. The real difference is the
write path: tf2 allocates and frees once per published transform; tf_tree's
lifetime allocations all precede the first lookup.

### Computation — per lookup, three dynamic steps, 100 ms query window

Baseline-subtracted (mode `N=0` performs setup and no lookups).

| Per lookup | tf_tree `LerpSlerp` | tf_tree `ScLerp` | tf2 |
|---|---|---|---|
| Instructions | **2,072** | 2,863 | 4,083 |
| L1-D misses | **0.002** | 0.002 | **15.3** |
| Branch mispredicts | **7.70** | 7.70 | 14.00 |
| — of which *indirect* | **0.00002** | 0.00002 | **6.00** |

Against tf2 on the comparable policy: **1.97x fewer instructions, ~8,000x fewer
L1-D misses, ~zero indirect mispredicts against six** (tf2's virtual dispatch
through `TimeCacheInterface`; a compiled `Plan` is a flat `[Step; 16]`).
Cachegrind's predictor is a simple two-level model: compare engines under it, not
against a real CPU's count.

### Where the lookup actually spends itself

`just profile-lookup`, cachegrind over 60 000 depth-3 lookups, by file, under the
`profiling` profile (release codegen, debuginfo kept):

| file | instructions | branch mispredicts |
|---|---:|---:|
| `tf_tree_core/src/sample.rs` (inlined into `fold_at`) | 27.1% | **76.8%** |
| `tf_tree_math/src/quat.rs` | ~19% | ~0% |
| `tf_tree_core/src/buffer.rs` | ~13% | ~0% |

Essentially every mispredict is in the sampling path, and the bracket search is
**not** branchless: its index select compiles to a conditional branch
([`0053`](../decisions/0053-the-branchless-bracket-that-branches.md)).

## Multi-process: the comparison tf2 cannot enter

`just shm-scaling` (Linux, `--features shm`). `tf2::BufferCore` has no
shared-memory mode: each process runs its own `tf2_ros::TransformListener`;
tf_tree maps one arena N times.

| Processes | Aggregate M/s | ns/lookup | vs 1 proc | Unique resident | tf2 history would be *(arithmetic)* |
|---|---|---|---|---|---|
| 1 | 4.66 | 213 | 1.00x | 3.5 MiB | 1.4 MiB |
| 4 | 15.43 | 257 | 3.31x | 9.9 MiB | 5.4 MiB |
| 8 | 18.17 | 431 | 3.90x | 18.7 MiB | 10.8 MiB |

Scaling is bounded by cores: 4 processes × 213 ns is an 18.8 M/s roofline and the
8-process row measures 18.2. The tf2 column is `n × 1 421 392 B`; no tf2 process
ran behind it. `Unique resident` is Pss-derived, not RSS, despite the `rss` metric
ids in `scale_sweep.rs` and `soak.rs`, which are frozen join keys for `bench_ab`
and the baseline. The 213 ns figure against `cost_model`'s 217 ns is unpaired;
the paired shared-mapping measurement is `just abi-split`.

### The multi-process *node* evaluation

`just mp-bench` measures N nodes at 100 Hz, open loop (latency from the
*intended* tick), with a publisher running throughout, `service` (work-start to
done) and `cycle` (intended-tick to done) clocks, per-consumer CPU and PSS. **The
tf2 column is a floor**: `mp-bench-tf2` runs N private `BufferCore`s fed the same
stream, with no `TransformListener` or DDS fan-out. Rows above 10% foreign load
are `NOISY`.

`just mp-bench-tf2`, 2026-07-26; 100 Hz × 6 s per point, 8 lookups per tick,
depth-3 chain, `taskset -c 0-7`, foreign load 1–7%. Microseconds; one run per
point, so differences under ~10% are noise:

| engine | nodes | svc p50 | svc p99 | svc p99.9 | cyc p50 | CPU %/node | PSS MiB |
|---|---:|---:|---:|---:|---:|---:|---:|
| tf_tree | 1 | 3.22 | 7.04 | 11.65 | 67.6 | 0.139 | 4.00 |
| tf_tree | 16 | 1.93 | 4.19 | 15.68 | 64.5 | 0.108 | 16.96 |
| tf2 (floor) | 1 | 15.94 | 34.05 | 38.66 | 79.9 | 0.254 | 5.25 |
| tf2 (floor) | 16 | 14.72 | 29.18 | 64.26 | 76.8 | 0.229 | 41.12 |

Service latency is 5.0× better at one node and 7.6× at sixteen; CPU per node 2.1×
at sixteen (tf_tree's falls with consumers, `PHASE2.md` §12.4); memory 2.4×, **2.8×**
per marginal node.

### Python: `tf_tree` against `tf2_ros`

`just py-vs-tf2`, 2026-07-26. One dynamic edge, 2000 samples at 1 ms;
`tf2_ros.Buffer` fed directly, no DDS. Release builds with LTO.

| | tf_tree | `tf2_ros` | ratio |
|---|---:|---:|---:|
| scalar lookup | **188.7 ns** | 12 182.3 ns | **64.5×** |
| batch, n = 4096 (per sample) | **34.9 ns** | 12 607.5 ns | **361.7×** |

tf2 has no batch API; the scalar row is the like-for-like number. Against
`docs/PHASE3.md` §12.2's gate (`benches/py_parity.rs`, native 36.3 ns/sample at
n = 4096):

| gate | result | |
|---|---|---|
| 1. scalar `plan.at` p50 under 250 ns | 228.8 ns | **pass** |
| 2. `at_many` at n = 4096 within 1.3× of native | 0.93× | **pass** |
| 3. `at_into` eliminates the allocation | 8.1 µs/call saved at n = 4096 | **pass** |

`just py-mp-bench` (2026-07-26, 8 Python nodes at 100 Hz, depth-3 chain; 8-node svc p50 3.1–3.4 µs against 233–374 µs, **75–110×**):

| marginal, per node | tf_tree | `tf2_ros` | ratio |
|---|---:|---:|---:|
| memory (PSS) | **14.9 MiB** | 37.7–38.0 MiB | 2.5× |
| CPU | **0.16–0.17 %** | 2.9–3.8 % | **18–22×** |
| time to first usable lookup | **0–1 ms** | 64–121 ms | ~70× |

The CPU row is `docs/PHASE2.md` §12.4's "O(1) in the number of consumers"
measured. `just shm-test` is the multi-process gate: a separate process, after
`exec`, maps the same sealed `memfd` and answers **bit-identically** over 512
queries, read-only and after a peer attached ([`PHASE2.md`](../PHASE2.md) §4).

## The performance suite: contention, scale, duration, and the transport

Every number below is **indicative**: this host fails `tf_tree_bench`'s
`Fitness::probe` and every harness says so.

### Read scaling with concurrent writers — `just contended-scaling`

[`PHASE1.md`](../PHASE1.md) §11.2 specifies "1/2/4/8/16 reader threads, **4
concurrent writers**, cores pinned". `contended_scaling` runs N reader and M
writer *processes* on one shared arena, each on its own core by `taskset`.
24 frames, 3 dynamic steps, 3 s per point, 8 logical CPUs:

| readers | writers | Mlookup/s | scale | svc p50 | svc p99 | svc p99.9 |
|---|---|---|---|---|---|---|
| 1 | 0 | 4.64 | 1.00x | 250 ns | 300 ns | 360 ns |
| 8 | 0 | 26.55 | **5.73x** | 330 ns | 410 ns | 520 ns |
| 8 | 4 | 24.09 | **5.20x** | 360 ns | 430 ns | 480 ns |

Four concurrent writers cost about 9% of aggregate throughput at 8 readers; the
tail is flat. The §11.3 scaling gate still fails because 8 threads over 4 cores
exceed 4x only via SMT.

### What a writer costs a reader — `TF2_WRITERS=N just tf2-scaling`

Writers publish to edges the query path does **not** traverse; `BufferCore` takes
one mutex for the whole buffer, tf_tree's rings are per edge. Two writers,
`fixture_depth6`, 15 rounds, engines interleaved:

| | tf_tree M/s | tf2 M/s | ratio |
|---|---|---|---|
| 1 thread, 0 writers | 6.66 | 3.00 | 2.2x |
| 1 thread, 2 writers | 6.63 | **0.62** | **10.7x** |
| 4 threads, 2 writers | 20.65 | **0.81** | **25.6x** |

Two writers cost tf_tree's readers 0.5% of throughput; they cost tf2's readers 79%
of throughput, 5.9x at p50 and 31x at p99.9.

### Scale — `just scale-sweep`

Lookup cost against tree **width** at fixed dynamic-step count:

| workload | frames | edges | dyn steps | at p50 | latest_common p50 | plan compile | build |
|---|---|---|---|---|---|---|---|
| `robot` | 24 | 23 | 3 | 251 ns | 100 ns | 144 ns | 2.7 ms |
| `av` | 375 | 374 | 14 | 1012 ns | 231 ns | 171 ns | 2.1 ms |
| `extreme_wide` | 12289 | 12288 | 4 | 320 ns | 111 ns | 244 ns | 364 ms |

Width is free; depth costs ~66 ns per additional dynamic step
([`PHASE1.md`](../PHASE1.md) §11.3). Ring depth, one edge: `at` p50 is 90 ns at 8
slots and 120 ns at 1 048 576 slots (72 MiB), p99.9 101 -> 700 ns. Publish, one
thread over N edges: 6.84 ns/push at 1 edge, 10.33 at 256 (the working set;
`EdgeCounters` is padded to 128 bytes).

**Limits:** **59 651 678 sample slots (~4.00 GiB) in one arena**, past which
`LayoutError::ArenaTooLarge` (every region offset is a `u32`); 32 compiled plan
steps (`MAX_DEPTH`) and 64 raw path edges (`MAX_PATH_EDGES`), below.

### Duration — `just soak`

40 s, 24-frame fixture (10 s retained), 2 reader and 4 writer threads, snapshots
every 10 s: Mlookup/s 5.04–5.24, p50 280 ns, p99.9 460–470 ns, Pss 2636 -> 2656
KiB, `declined` ~130 ppm throughout. The harness **asserts** the rings lapped.
**Publish-to-visible is ~190 ns at p50** ([`PHASE5.md`](../PHASE5.md) §9.2's
required row).

### The transport — `just dds-bench`

One publisher, real DDS, the container's RMW, [`PHASE4.md`](../PHASE4.md) §5.2's
QoS, 4 consumers, 100 Hz, 100 ms query lag, 3 s warm-up discarded, **15 s
measured** (`SECONDS_MEASURED`), both arms on stock defaults.

| arm | procs | consumers | svc p50 | svc p99 | svc p99.9 | CPU %/consumer | PSS |
|---|---|---|---|---|---|---|---|
| `tf2.processes` | 4 | 4 | 2.59 µs | 8.64 µs | 12.16 µs | 2.968% | 63.15 MiB |
| `tf2.composed` | 1 | 4 | 1.43 µs | 6.21 µs | 10.50 µs | 0.644% | **24.04 MiB** |
| `tf_tree.composed` | 1 | 4 | **0.77 µs** | **3.62 µs** | **6.18 µs** | 0.656% | 24.76 MiB |
| `tf_tree.processes` | 5 | 4 | 0.90 µs | 8.96 µs | 16.90 µs | **0.725%** | 69.51 MiB |

`procs` counts the bridge. Against `tf2.processes`: **3.4x on p50** for the
composed arm and **4.1x on CPU** for the multi-process one (the lowest of four
`tf2.processes` samples, 2.968–5.163, so the floor of the estimate).
`tf_tree.composed` leads `tf2.composed`, tf2's best case, at every percentile.
`mp::tests::cpu_time_survives_a_thread_exiting` pins the CPU measurement.

#### The fourth arm — `tf_tree.processes`

[`0015`](../decisions/0015-the-bridge-fills-a-shared-arena.md): one
`bench_consumer --mode tf_tree_bridge` process publishes its arena under
`$TF_TREE_NAME`, four `--mode tf_tree_attach` processes join it read-only. The
bridge's cost is inside the row. Over a 15 s window:

| | fixed | per consumer | 4 consumers | 16 consumers (extrapolated) |
|---|---|---|---|---|
| `tf2.processes` CPU | — | 0.445 s | 1.78 s | 7.12 s |
| `tf_tree.processes` CPU | 0.362 s (bridge) | 0.0186 s | 0.436 s | 0.66 s |

A marginal tf_tree consumer costs about 24x less CPU than a marginal tf2 one;
break-even is roughly one consumer. **Where it is worse, at N = 4:** memory (69.51
against 63.15 MiB; each of five processes carries an rclcpp node and a DDS
participant) and tail latency, **p99 (8.96 µs against 8.64)** and **p99.9 (16.90
against 12.16)** ([`PHASE5.md`](../PHASE5.md) §9.3 requires this in the table);
both `.processes` `svc` columns are wake-from-idle-dominated on this host and need
pinned hardware before quoting.

### The memory curve

`CONSUMERS=8`, `12`, `16`, one run each:

| N | `tf2.processes` | `tf_tree.processes` | delta | tf2 per consumer | tf_tree per consumer |
|---|---|---|---|---|---|
| 4 | 63.15 MiB | 69.51 MiB | **+6.36** | 15.79 | 17.38 |
| 8 | 113.96 MiB | 113.80 MiB | −0.16 | 14.25 | 14.23 |
| 16 | 226.59 MiB | **219.06 MiB** | **−7.54** | 14.16 | 13.69 |

The stacks are indistinguishable from N = 8 to 12; N = 16's 7.54 MiB (**3.3%**) is
the first difference outside single-run spread. **None of this is an arena
difference:** the `composed` arms put both stacks in one process, and there
tf_tree is worse by +1.04, +0.80, +0.75 MiB at N = 8, 12, 16 — that ~1 MiB is the
arena; the rest is rclcpp and DDS (`bridge_supervision` names the error of
reporting it otherwise).

## A real difference: maximum chain depth

tf_tree bounds a path twice; **tf2 bounds it not at all.**

* `tf_tree_core::MAX_PATH_EDGES` (**64**) caps the *raw walk* — every edge
  between the two frames, up to their common ancestor and back down.
* `tf_tree_core::MAX_DEPTH` (**32**) caps the *compiled plan*, counted **after**
  adjacent rigid links fold into one step.

Either overrun is `LookupError::TreeTooDeep`; its `depth` is `MAX_PATH_EDGES + 1`
for the walk, else the exact folded step count.
[`0034`](../decisions/0034-the-depth-bound-priced-two-slots-the-same.md) sized
both ([`PHASE1.md` §7.1](../PHASE1.md#71-step-representation)). Migrating from
tf2, compare your worst frame pair's diameter, not root-to-leaf depth.

## Data provenance

The recorded stream is derived from a mobile-robot dataset under **CC BY 4.0**
(DOI [10.5281/zenodo.19894190](https://doi.org/10.5281/zenodo.19894190)); see
[`testdata/tfstream/ATTRIBUTION.md`](../../testdata/tfstream/ATTRIBUTION.md).

## Runbook for pinned hardware

```bash
# isolcpus=2-7 nohz_full=2-7 rcu_nocbs=2-7 at boot; cpupower -g performance; boost off
echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid
taskset -c 2 ./docker/tf2/run.sh \
  'cargo bench -p tf_tree_bench --features tf2 --bench tf2_compare'
```

Report p50/p99/p99.9, not means ([`PHASE1.md`](../PHASE1.md) §11.2). The
performance-suite harnesses (`just contended-scaling`, `scale-sweep`, `soak-long`,
`dds-bench`) refuse on a busy machine; `just bench-run` / `bench-ab` compare two
builds.
