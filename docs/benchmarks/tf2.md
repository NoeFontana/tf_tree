# tf_tree vs ROS 2 `tf2` — validation and benchmarks

This document records what has actually been measured against ROS 2's `tf2`, how,
and what the numbers do and do not support. Numbers here are **reproducible** —
every row names the command that produced it.

Reproduce everything with:

```bash
just tf2-differential   # correctness, synthetic fixture
just tf2-replay         # correctness, real recorded /tf stream
just tf2-bench          # performance, head-to-head, single-threaded
just tf2-scaling        # concurrent read scaling, 1/2/4/8 threads
just tf2-native-control # pure C++ tf2, no Rust and no FFI — the bias control
just footprint           # memory + instructions per lookup (no idle machine needed)
just shm-test            # multi-process gate: another process, bit-identical
just shm-scaling         # N reader PROCESSES on one shared arena (roofline)
just mp-bench            # N node-shaped consumers at a fixed rate (deployment)
just mp-bench-tf2        # the same, both engines, in the ROS container
just py-vs-tf2           # tf_tree's Python API vs tf2_ros's (PHASE3 §12.1)
just profile-lookup      # where the lookup spends itself, by file
```

All of them run in a container (`docker/tf2/`), so no ROS install is needed on
the host.

The bridge all five go through has its own check, `just tf2-check`: fmt, clippy
and the unit tests of `tf_tree_tf2_sys` and of `tf_tree_bench --features tf2`.
It exists because that crate is excluded from the workspace (it only builds
where ROS 2 does), so `just lint` and `just test` cannot see it.

## Setup

| | |
|---|---|
| **tf2 version** | ROS 2 Lyrical Luth (May 2026, LTS to May 2031) |
| **Platform** | Ubuntu 26.04 "Resolute", g++ 15.2 |
| **Bridge** | `tf_tree_tf2_sys` — `extern "C"` shim over `tf2::BufferCore` |
| **Interpolation** | `LerpSlerp` on both sides (tf2's policy) |

`tf2::BufferCore` links against `-ltf2` alone: no rclcpp, no DDS, no ROS graph.
Both engines are therefore plain in-process libraries driven from the same loop
with the same inputs. **No middleware is in any measurement.** A comparison that
included DDS would measure the transport, not the transform engine, and would
flatter tf_tree for the wrong reason.

## Correctness

The claim that matters for migration is that code moving from tf2 to tf_tree sees
the *same transform*. Both engines are given an identical topology and an
identical sample stream, then asked the same random queries.

| Load | Queries scored | Max disagreement | Bound |
|---|---|---|---|
| Synthetic fixture (24 frames, depth 6) | 95,909 | **2.876e-15** | 1e-12 |
| Recorded `/tf` stream (10 frames, depth 3) | 50,000 | **6.665e-15** | 1e-12 |

Both are ~150-350x tighter than [`PHASE1.md`](../PHASE1.md) §10.5's 1e-12 gate, and are at the
level of `f64` round-off for the composition depths involved.

On the recorded stream the two engines also declined **exactly the same queries**
(`tf_tree-only 0, tf2-only 0`): they agree on *which* lookups are answerable, not
merely on the values of the ones they both answer. That is the stronger property
and the one a migration actually depends on.

Disagreement is measured as `max(rotation-angle error in radians, translation
error in metres)` — not a component-wise comparison, which would let a
quaternion sign flip pass.

### Guarding against a vacuous pass

A differential can "pass" by comparing nothing. Two safeguards:

* `DiffReport::passed()` requires `compared > 0`, so an oracle that declined
  every query reports failure rather than a `max_error` of `0.0`.
* The scored count is reported alongside the error, so a run that only compared
  a handful of queries is visible rather than silently reassuring.

## Performance

All figures below were taken on an **idle** machine (load < 0.6, nothing running
but the harness), 8 logical CPUs / **4 physical cores** (2-way SMT), AMD
EPYC-Milan pinned at 2445 MHz with no frequency governor exposed. Criterion
confidence intervals were within ±0.2%.

These are still **not** the [`PHASE1.md`](../PHASE1.md) §11.3 go/no-go gate, which calls for
dedicated core-pinned hardware; a shared-tenancy VM with SMT is closer than
before but is not that.

### Measuring the measurement: four biases found and removed

Three of the four favoured tf_tree; the fourth favoured tf2. They are listed
here, with what each cost, because a benchmark whose author only checked in one
direction is not evidence — and checking in both directions is what turned up
number 4.

| # | Bias | Cost charged to tf2 | Found by |
|---|---|---|---|
| 1 | `CString::new` x2 per call in the Rust binding | **63-65 ns (14-18%)** | code review |
| 2 | `const char*` -> two `std::string` temporaries at the C++ call site | **~17 ns (7%)** | native C++ control |
| 3 | Residual FFI boundary (cross-TU, no inlining, extra copy) | ~21 ns (8%) | native C++ control |
| 4 | `setTransform` authority passed as a string *literal* | **~8 ns + 1 malloc/free per publish** | `just footprint` |

Bias 4 was found last and is the only one so far that ran the *other* way — it
was charged to tf2 and made tf_tree look better. `tf2::BufferCore::setTransform`
takes `const std::string&`, so a literal constructs a temporary per call, and at
20 characters `"tf_tree_differential"` is past libstdc++'s 15-byte SSO buffer:
one heap allocation on every publish. A real broadcaster stores its authority
once. Fixing it (a `static const std::string`) moved tf2's publish from 123 ns to
114 ns and **the published push ratio from 14.1x to 12.1x**. It was found only
because the allocation-count measurement below asked a question timing does not:
*how many times did each engine call the allocator?*

Bias 2 is the instructive one. The in-tree `shim_overhead` probe reported only
11 ns and **missed it entirely**, because the probe measured the Rust side while
the cost was incurred inside C++ at the `lookupTransform` call site. No amount of
staring at the Rust code would have found it. What found it was
`docker/tf2/native_scaling.cpp` — the same load and the same queries with the
binding deleted outright.

Fixes: `FrameName` now owns a heap `std::string` on the C++ side and
`tft2_lookup_pre` passes it by `const&`, which is byte-for-byte the call a native
C++ user makes. Two controls run on every benchmark invocation so this cannot
silently regress — `tf2/lookupTransform_alloc` re-measures the naive binding, and
`tf2/shim_overhead` isolates what the bridge still costs.

Bias 3 is irreducible for any FFI comparison, so **the single-threaded ratio is
reported against the native C++ figure**, which has no binding in it at all.

### Steady-state lookup

1024 queries per iteration, stamps swept across the history window.

| Load | tf_tree | tf2 (via bridge) | tf2 (native C++) | Ratio vs native |
|---|---|---|---|---|
| Fixture, depth 6 | 178 ns | 379 ns | - | 2.1x |
| Recorded stream, depth 3 | 94 ns | 292 ns | **253 ns** | **2.7x** |

The honest headline is therefore **~2.7x**, not the 3.3x first reported.

### Where the win comes from

tf_tree compiles the topology walk **once** into a `Plan` and thereafter only
samples ([`PROJECT.md`](../PROJECT.md) §5 D3); tf2 walks per call. Benchmarking only that would
be self-serving, so the suite also measures tf_tree recompiling a fresh plan for
*every single query*:

| Load | tf_tree (plan reused) | tf_tree (replanned every query) | tf2 |
|---|---|---|---|
| Fixture, depth 6 | 178 ns | 361 ns | 379 ns |
| Recorded stream | 94 ns | 243 ns | 292 ns |

Plan reuse is worth about 2x — and tf_tree stays ahead of tf2 *even when
throwing the plan away every time*, so the sample path is faster on its own
merits, not only the bookkeeping.

### Scaling with tree size and depth

Deepest-pair lookup, 256 queries per iteration.

| Shape | tf_tree | tf2 | Ratio |
|---|---|---|---|
| 12 frames, depth 4 | 262 ns | 1377 ns | **5.3x** |
| 35 frames, depth 7 | 514 ns | 2592 ns | **5.0x** |
| 117 frames, depth 13 | 1031 ns | 5948 ns | **5.8x** |
| 375 frames, depth 15 | 1193 ns | 7337 ns | **6.2x** |

Both scale primarily with **depth**, not frame count: 117 -> 375 frames (3.2x the
tree, two more levels) costs tf_tree 16% and tf2 23%.

### Publish

| | tf_tree | tf2 | Ratio |
|---|---|---|---|
| One sample onto one edge | **9.4 ns** | 114 ns | **12.1x** |

tf_tree's publish path is allocation-free; **tf2's is not** — it calls the
allocator exactly once per stored transform (measured below). The earlier claim
that both were allocation-free was wrong, and the earlier 14.1x ratio included a
second allocation that was the shim's fault, not tf2's (bias 4 above).

tf_tree's `push` takes no strings; the
tf2 row hands the shim the `std::string`s a `FrameName` already owns and assigns
them into the message, which is what a native C++ publisher does. The earlier
139 ns figure was measured through a path that built a NUL-terminated copy of
each name per call — two heap allocations charged to tf2 that a C++ caller never
pays. The `push/tf2_alloc` row keeps that naive binding as a control: **187 ns**,
so the marshalling was worth ~15 ns and a fully naive binding ~64 ns.

Both caches are bounded, differently, and that difference is part of the comparison:
tf_tree's ring is count-bounded (fixed power-of-two slots, overwritten in place,
never allocating — invariant 8, enforced by the zero-allocation gate); tf2's
cache is time-bounded and prunes on insert, here at the realistic 10 s default.

### Concurrent read scaling — 1 / 2 / 4 / 8 threads

**tf_tree's readers take no lock**; every `tf2::lookupTransform` acquires
`BufferCore`'s internal frame mutex. One shared tree and one shared buffer, as
both engines are meant to be used — per-thread buffers would erase the
contention being studied. 101 rounds per point, engines **interleaved within
every round** so drift lands on both equally.

The tables below were **not** taken with a bare `just tf2-scaling`: the harness
defaults are 51 rounds and 50,000 latency samples, and both were raised for this
run. The exact command was

```bash
./docker/tf2/run.sh 'TF2_ROUNDS=101 TF2_LATENCY_SAMPLES=100000 \
  cargo run -p tf_tree_bench --features tf2 --release --bin tf2_scaling'
```

(the overrides go *inside* the quoted command — `run.sh` does not forward the
host environment into the container). `just tf2-scaling` runs the same harness
at its defaults, which is faster and noisier.

Throughput, million lookups/s, recorded stream. `spread` is
`(best - median)/best` for that cell — small means the machine was quiet, and it
is quoted because it is what makes the 4-thread row interpretable:

| Threads | tf_tree | spread | tf2 | spread | Ratio | tf_tree vs 1thr | tf2 vs 1thr |
|---|---|---|---|---|---|---|---|
| 1 | 12.70 | 1.9% | 3.61 | 1.0% | 3.5x | 1.00x | 1.00x |
| 2 | 23.42 | 8.8% | 1.82 | 1.8% | 12.9x | 1.84x | **0.50x** |
| 4 | 35.44 | 24.0% | 1.31 | 49.9% | 27.0x | 2.79x | **0.36x** |
| 8 | 68.02 | 29.1% | 1.13 | 4.5% | **60.3x** | 5.35x | **0.31x** |

tf_tree scales; **tf2 anti-scales** — more threads make it slower than one
thread, the signature of a contended global mutex.

tf_tree's one-thread figure moved from 10.64 to 12.70 M/s against the previous
edition of this table; that is the interpolation work in
[`docs/design/fast-path.md`](../design/fast-path.md) §11, not a change of method.

**The 4-thread row is the noisiest point in the whole suite, and now it is
labelled as such.** A previous edition of this document carried an unexplained
discrepancy there (1.56 vs 1.39 M/s for tf2 across two back-to-back runs) and
flagged it as unresolved. Two fresh runs on an idle host reproduce the
instability rather than the value — tf_tree 38.20 then 35.44 M/s, tf2 spread
reaching 49.9% — while every other row repeats to within 1% (8 threads: 68.09
then 68.02). The explanation is the host: with 4 physical cores, 4 threads is the
point where the runnable set exactly matches the core count, so which SMT sibling
each thread lands on decides the result and nothing pins it. It is not a
measurement to be re-taken until it settles; it is a row this hardware cannot
measure precisely.

**This is tf2's behaviour, not an artifact of our binding.** The pure C++ control
(`docker/tf2/native_scaling.sh`, no Rust, no FFI, same stream, same queries)
reproduces it. Both were re-run back-to-back on the same host so the columns are
directly comparable (which is why the bridge's 4-thread figure here is 1.39
rather than the noisier 1.56 of the run tabulated above), and the control sweeps **exactly** the Rust harness's
`common_window` (max of the per-edge first stamps to min of the per-edge last
stamps — it previously ran to the global last stamp, so ~11% of its queries were
past the end of the shortest edge and answered by tf2's throw path rather than
its lookup path):

| Threads | native C++ tf2 M/s | via our bridge M/s | native vs 1thr | bridge vs 1thr |
|---|---|---|---|---|
| 1 | 3.80 | 3.66 | 1.00x | 1.00x |
| 2 | 1.84 | 1.83 | 0.48x | 0.50x |
| 4 | 1.38 | 1.39 | 0.36x | 0.38x |
| 8 | 1.12 | 1.12 | **0.30x** | **0.31x** |

The bridge is within 4% of native at one thread and within 1% at two, four and
eight — it costs a little on an uncontended call and nothing once the mutex
dominates. The collapse is tf2's.

The tail is starker than the throughput. Per-lookup latency, recorded stream:

| Threads | Engine | p50 | p99 | p99.9 | p99.99 |
|---|---|---|---|---|---|
| 1 | tf_tree | 110 ns | 141 ns | 179 ns | 6.5 us |
| 1 | tf2 | 291 ns | 852 ns | 1.2 us | 7.7 us |
| 8 | tf_tree | 151 ns | 220 ns | **331 ns** | 7.1 us |
| 8 | tf2 | 3.4 us | 47 us | **83 us** | 204 us |

At 8 threads tf_tree's p99.9 is 331 ns against tf2's 83 us — a factor of **252**.
For a control loop that is the difference between a bounded and an unbounded
worst case, and it is the strongest result in this document: unlike the
throughput ratio it does not depend on core count, and unlike the single-threaded
ratio it is not sensitive to any FFI residue.

Note the shape, not just the size. tf_tree's p50 rises 110 -> 151 ns from 1 to 8
threads and its p99.9 rises 179 -> 331 ns: both grow slightly and stay bounded.
tf2's p50 rises 291 ns -> 3.4 us and its p99.9 rises 1.2 us -> 83 us — the tail
degrades ~14x faster than the median, which is what a convoy looks like.

**Caveat on the scaling factor, and the gate.** This host has **4 physical
cores**; tf_tree's 5.35x (recorded) / 5.62x (fixture) at 8 threads is SMT-assisted
and is not a clean scaling number. The harness prints the physical core count for
exactly this reason.

[`PHASE1.md`](../PHASE1.md) §11.3's third gate criterion is "read throughput
scales at least **6x** from 1 to 8 threads". **Measured 5.35x-5.62x, so the
criterion is not met as written** — but it cannot be fairly evaluated on this
machine, because 8 threads on 4 cores can only exceed 4x through SMT at all. The
honest reading is that tf_tree reached 2.79x-3.09x at 4 threads on 4 cores and
then gained a further ~1.8x from hyperthreading. Re-running on a host with >= 8
physical cores is the only way to settle it; §11.3 asks for dedicated pinned
hardware for precisely this reason.

What the criterion was actually protecting is not in doubt. Its stated purpose is
that "if tf_tree scales cleanly, the value proposition is your perception nodes
stop contending". Against an engine that goes *backwards* — 0.31x at 8 threads —
a 5.4x that is core-count-limited rather than contention-limited settles that
question regardless of where it lands against 6.

Two internal cross-checks that the harness measures what it claims: for each
engine, p50 minus the ~20 ns timer overhead matches the figure implied by its
independently batch-timed throughput.

## Memory and computation

Reproduce with `just footprint`. Unlike every timing row above, **these numbers
do not need an idle machine**: `cachegrind` and `memcheck` simulate, so the
counts are exact and reproducible under load. They are also the only rows here
that survive a change of CPU.

Each engine is measured in its **own process**. Building both in one would let
the first engine's freed chunks satisfy the second's requests, making whichever
ran second look cheaper by an amount nobody can bound.

Memory is `mallinfo2`'s `uordblks + hblkhd`, not RSS. RSS is page-granular and
includes text and stacks; `mallinfo2` is glibc's own accounting, and since C++
`operator new` bottoms out in `malloc` it measures the C++ and the Rust engine on
identical terms. The `hblkhd` term is not optional: tf_tree's arena is a single
allocation above glibc's 128 KiB mmap threshold, so `uordblks` alone would report
it as using almost nothing.

### Memory — identical topology, identical 10 s of history (12,600 samples)

| | tf_tree | tf2 |
|---|---|---|
| Heap held | 1,397,616 B | 1,421,392 B |
| Bytes per stored sample | 110.3 | **112.8** |
| Bytes per *declared slot* | **72.9** | n/a |
| Allocations to build | **108** | 88,459 |
| Allocations per published transform | **0** | 1.00 |
| Allocations per lookup | **0** | **0** |

**tf_tree is not meaningfully smaller — it is within 2.4%.** That is worth
stating plainly, because the arena design invites the assumption that it would
win here, and it does not.

The two per-sample figures differ because tf_tree's rings are sized by *declared
capacity*, not by what is stored: `Capacity::history` rounds each ring up to a
power of two, so a 1 kHz edge over 10 s asks for 10,000 slots and reserves
16,384. At 72.9 B/slot (a 64 B cacheline-padded `Iso3` plus an 8 B stamp)
tf_tree is 1.56x denser than tf2 per unit of capacity, and this fixture's
rounding hands almost all of that back. Fixed capacity that never reallocates is
the point of the design; the rounding is what it costs.

**The real difference is on the write path, not the read path.** Both engines
turn out to be allocation-free per lookup — that is a genuine tf2 result, and the
naive expectation that a C++ `std::map` engine must be allocating on reads is
simply wrong when the caller passes prebuilt string handles. But tf2 allocates
and frees **once per published transform**, forever. A robot publishing ten
dynamic edges at 1 kHz puts 10,000 malloc/free pairs per second through the
allocator; tf_tree puts through zero, and its 96 lifetime allocations all happen
before the first lookup.

That figure was 2.00 before bias 4 was found and fixed — the extra one was the
shim's, not tf2's. The remaining one is genuine: tf2 stores each transform in a
per-frame node it must allocate, which is the direct cost of a container that
grows to fit what it is given rather than reserving a fixed ring.

### Computation — per lookup, three dynamic steps, 100 ms query window

Baseline-subtracted: mode `N=0` performs the full setup and no lookups, so
subtracting it removes construction, teardown and process start exactly.

| Per lookup | tf_tree `LerpSlerp` | tf_tree `ScLerp` | tf2 |
|---|---|---|---|
| Instructions | **2,072** | 2,863 | 4,083 |
| L1-D misses | **0.002** | 0.002 | **15.3** |
| LL-D misses | 0.00002 | 0.00003 | 0.0003 |
| Branch mispredicts | **7.70** | 7.70 | 14.00 |
| — of which *indirect* | **0.00002** | 0.00002 | **6.00** |

Against tf2 on the comparable policy: **1.97x fewer instructions, ~8,000x fewer
L1-D misses, and effectively zero indirect branch mispredicts against six.**

Re-measured 2026-07-26 after the layout-kernel work; the previous run read
2,105 instructions, 18.5 L1-D misses and 8.16 mispredicts for tf_tree. The
change is small and in the right direction, and it is recorded rather than
quietly overwritten so that a later regression has something to be a regression
*from*. These are simulated counts, so unlike every timing table here they are
exact and reproducible under load — which is why they are the numbers to watch.

The six indirect mispredicts are virtual dispatch — tf2 reaches its per-frame
caches through `TimeCacheInterface`, and the target is unpredictable because a
walk visits a different frame each step. tf_tree's compiled `Plan` is a flat
`[Step; 16]` with no dynamic dispatch anywhere on the path, so the indirect
predictor is never consulted.

**Two caveats that cut against the headline.** First, LL-D misses are ~0 for
*both* engines: this fixture's whole working set is 1.4 MB and fits in L3, so
tf2's 15.3 L1 misses per lookup are being served by L2/L3, not DRAM. On a tree
large enough to fall out of L3 those become memory accesses and the gap widens —
but that is a prediction, and it is not measured here. Second, the instruction
ratio (1.97x) is *smaller* than the measured wall-clock ratio (2.7x), so roughly
a third of the observed speed advantage is not explained by executing fewer
instructions. The mispredict and cache columns are the likely remainder, along
with tf2's per-lookup mutex, but attributing it precisely needs cycle counters
this host does not permit (`perf_event_paranoid=4`).

A third caveat applies to the mispredict column specifically: **cachegrind's
branch predictor is a simple two-level model, not a Zen 3 TAGE.** Comparing two
engines under the same model is sound, and that is all this table does; reading
"7.70" as the count a real CPU incurs is not. Acting on that distinction matters
— that figure was first read as the bracket search's data-dependent branch,
and an early branchless rewrite recovered only 0.46 of it, which was written up
here as "LLVM had already emitted a `cmov`". **That explanation was wrong**, and
per-line profiling (`just profile-lookup`, once it worked) shows why: 99.97% of
`sample.rs`'s mispredicts really are in the bracket loop —

    248,173  base += half * cmp;
    131,112  while len > 1 {

— so the search was the right suspect all along. `half * cmp` reads as
branchless and is not; the backend emits a branch for it. Replacing the
multiply with a mask (`half & (0 - cmp)`, an AND the backend cannot turn back
into control flow) moves simulated mispredicts 7.70 -> 7.32 per lookup and
wall-clock by **-2.8% at depth 3 / sclerp, -1.5% at depth 3 / lerpslerp, -1.2%
at depth 6**, with no change at depth 1 and no change in instruction count.
Confirmed over two runs; the first run's apparent +3.3% at depth 1 did not
reproduce (p = 0.09).

The lesson stands, just not the one originally drawn: the simple predictor
model was not the problem, the *absence of line-level data* was

The corresponding tf2 number, 6.00 *indirect* mispredicts, is on firmer ground:
indirect targets are a structural property of virtual dispatch, not an artifact
of predictor modelling, and a compiled `Plan` has no indirect branches at all.

## Multi-process: the comparison tf2 cannot enter

Reproduce with `just shm-scaling` (Linux, `--features shm`). Two back-to-back
runs on an idle host; every row below repeated to within 1% except where noted.

Robot software is deployed as *separate executables* — perception, planning,
control — not as threads in one process. So the thread-scaling table above,
where tf_tree already wins, is not actually the deployment shape. This is.

**`tf2::BufferCore` has no shared-memory mode.** Every process needing
transforms runs its own `tf2_ros::TransformListener`, which subscribes to `/tf`,
deserializes every message, and maintains a private, complete copy of the
history. N consumers therefore cost N buffers, N deserialization pipelines and
N-way DDS fan-out — and the copies drift apart in time, because each is updated
by its own callback thread. tf_tree maps one arena N times.

| Processes | Aggregate M/s | ns/lookup | vs 1 proc | Unique resident | tf2 history would be |
|---|---|---|---|---|---|
| 1 | 4.66 | 213 | 1.00x | 3.5 MiB | 1.4 MiB |
| 2 | 9.04 | 219 | 1.94x | 5.7 MiB | 2.7 MiB |
| 4 | 15.43 | 257 | 3.31x | 9.9 MiB | 5.4 MiB |
| 8 | 18.17 | 431 | 3.90x | 18.7 MiB | 10.8 MiB |

**Scaling is bounded by cores, not by the design.** This host has 4 physical
cores, and 4 processes x 213 ns/lookup is a 18.8 M/s roofline; the 8-process row
measures 18.2, i.e. **the readers saturate the machine**. The 8-process row
oversubscribes 2:1, which is why its per-lookup latency doubles while aggregate
throughput stays flat — the correct and expected shape, not contention. There is
no lock for the processes to contend on.

That last claim is checkable rather than rhetorical. Eight processes sharing four
cores each get 50% of a core, so a lookup that takes 213 ns at full speed should
take 426 ns. **Measured: 431 ns.** The 8-process row is pure oversubscription,
with ~1% left over for anything else — there is no hidden cross-process cost to
go looking for.

One inference, flagged as such because this host's `perf_event_paranoid` forbids
the counters that would confirm it: because the arena is *shared*, N processes
touch the **same cache lines**, so the cache footprint of transform data is
independent of consumer count. tf2's N private buffers would be N x 1.4 MB of
distinct lines — 5.6 MB at four consumers, past many L3s. This is consistent with
the 4-process row costing only 21% more per lookup than one process, but it is
not measured.

Two things worth noting against the thread table above. Multi-**process** scaling
at 4 (3.31x) is slightly *better* than multi-thread scaling at 4 (2.79-3.09x):
separate address spaces share no allocator, no TLS and no false-shared cache
lines. And per-lookup cost at one process is 213 ns against the 217 ns
`cost_model` measures in-process for the same three-dynamic-step chain — **there
is no penalty for the arena being shared.** That is the single most important
number here: `MAP_SHARED` costs nothing per lookup.

The memory columns need care, and getting them wrong would have flattered the
design in the wrong place. Summing each process's RSS **double-counts** the
arena, because every mapper's `/proc/self/statm` includes pages that are
physically resident once. "Unique resident" subtracts the `(n-1)` redundant
copies. The remaining per-process growth (~2.2 MiB) is executable, stack and
libc — not transform data.

The last column is **arithmetic, not a measurement**, and is labelled as such
here and in the tool's own output: `n` x the 1,421,392 B `just footprint`
measured for one `BufferCore` on this fixture. Timing a real `tf2_ros` listener
would drag DDS into the comparison, which every other row in this document is
careful to exclude — so the structural cost is stated as arithmetic rather than
measured badly.

### The multi-process *node* evaluation — methodology

`just shm-scaling` above answers a roofline question: *how many lookups can N
processes extract from one arena in total?* That is a property of the machine.
The question an integrator actually has is different — **N nodes each need
transforms at their own rate; what does each experience, and what does it
cost?** — and `just mp-bench` is the harness for it. Five things had to change,
and each was a defect in the old one:

1. **Open loop, not a tight loop.** A tight loop is a *closed-loop* generator:
   the next request starts when the last finishes, so a slow response reduces
   the offered load and every recorded sample still looks fast. That is
   **coordinated omission**, and it is why a saturating loop reports a beautiful
   p99.9 for a system that is visibly stuttering. The new harness fixes the
   schedule in advance — tick `i` is due at `t0 + i/rate` — and measures from the
   *intended* time, so a node that falls behind reports the backlog.
2. **A publisher runs throughout.** The old harness read a quiescent tree:
   nothing exercised the seqlock retry path, nothing invalidated the readers'
   cache lines, and — decisively for the comparison — nothing ever held
   `tf2::BufferCore`'s mutex.
3. **Two clocks, reported separately.** `service` is work-start to done: what
   the engine costs. `cycle` is intended-tick to done: what the node
   experiences. At 100 Hz the second is ~95% OS wakeup latency, so reporting
   only it would make every engine look identical and excellent; reporting only
   the first would hide that a node's real latency is mostly not up to the
   engine at all.
4. **CPU per consumer.** §12.4's industrial claim is that tf_tree is *O(1) in
   the number of consumers* where `/tf` is O(consumers × edges × rate). Nothing
   measured it. A flat CPU column is that claim holding.
5. **PSS, not summed RSS.** Summed RSS counts a shared page once per mapper.
   The earlier table corrected for it by subtracting a known arena size, which
   only works because we know it; PSS is the kernel's own answer and is equally
   correct for tf2's private per-process buffers.

**The tf2 column will be a floor, and must be labelled as one.** Every other row
in this document excludes middleware, because for a single-process
library-vs-library comparison DDS would measure the transport rather than the
engine. That reasoning does not survive this question: across processes the
transport **is** tf2's mechanism — there is no other way for a second process to
obtain the tree. So `mp-bench-tf2` measures N private `BufferCore`s fed the
identical stream, which shows the duplication that having no shared arena forces
and nothing else. A deployed consumer additionally pays a `TransformListener`
and its DDS fan-out, and the write-up must say so rather than let the floor be
read as the cost.

**The harness refuses to run on a busy machine**, naming the top CPU consumers.
That guard exists because the first run of it was taken on a host carrying an
unrelated 600%-CPU job and nothing in the output said so. Every row also carries
its own measured foreign-load percentage and is flagged `NOISY` above 10%, so a
contaminated row cannot be published by accident.

### The multi-process node evaluation — results

`just mp-bench-tf2`, 2026-07-26. Both engines in the same container on the same
host, back to back: AMD EPYC-Milan, **4 physical cores / 8 SMT threads**,
`taskset -c 0-7`, 100 Hz × 6 s per point, 8 lookups per tick, depth-3 chain, a
publisher running throughout. Foreign load 1–7% on every row; none flagged
`NOISY`.

**tf_tree**

| nodes | svc p50 | svc p99 | svc p99.9 | cyc p50 | cyc p99 | cyc p99.9 | CPU %/node | PSS MiB |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 3.22 | 7.04 | 11.65 | 67.6 | 87.0 | 198.7 | 0.139 | 4.00 |
| 2 | 2.53 | 6.27 | 122.37 | 66.0 | 93.2 | 432.1 | 0.120 | 4.82 |
| 4 | 2.30 | 5.50 | 23.30 | 66.0 | 87.6 | 331.8 | 0.119 | 6.36 |
| 8 | 2.11 | 4.86 | 16.64 | 64.8 | 88.1 | 2801.7 | 0.112 | 11.83 |
| 16 | 1.93 | 4.19 | 15.68 | 64.5 | 79.4 | 630.8 | 0.108 | 16.96 |

**tf2** (floor — see above)

| nodes | svc p50 | svc p99 | svc p99.9 | cyc p50 | cyc p99 | cyc p99.9 | CPU %/node | PSS MiB |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 15.94 | 34.05 | 38.66 | 79.9 | 105.5 | 2129.9 | 0.254 | 5.25 |
| 2 | 14.78 | 28.29 | 34.05 | 78.3 | 108.5 | 655.4 | 0.232 | 7.26 |
| 4 | 14.91 | 27.39 | 46.08 | 77.8 | 106.0 | 1237.0 | 0.234 | 11.31 |
| 8 | 14.91 | 29.70 | 53.25 | 76.3 | 102.9 | 1097.7 | 0.226 | 18.57 |
| 16 | 14.72 | 29.18 | 64.26 | 76.8 | 101.9 | 704.5 | 0.229 | 41.12 |

Times are microseconds. What the two tables say:

- **Service latency: 5.0× at one node, 7.6× at sixteen** (15.94 → 3.22 µs;
  14.72 → 1.93 µs), and 7.0× at p99. The gap *widens* with node count because
  tf_tree's median falls as consumers are added while tf2's is flat — the shared
  arena stays warm across readers, whereas each `BufferCore` must warm its own.
  This is not a claim that consumers make tf_tree faster; it is a claim that they
  do not make it slower, which is the deployment question.
- **CPU per node: 2.1× (0.229% vs 0.108% at sixteen nodes)**, and — the actual
  `PHASE2.md` §12.4 claim — tf_tree's *falls* from 0.139% to 0.108% across a 16×
  increase in consumers while tf2's stays flat at ~0.23%. Both are O(1) in
  consumers here; only tf_tree is O(1) *and* cheap. tf2 being flat is a property
  of the floor: with a real `TransformListener` each consumer would deserialize
  the full `/tf` stream itself, which is where the O(consumers) term enters.
- **Memory: 2.4× at sixteen nodes (41.12 vs 16.96 MiB PSS)**, or per *marginal*
  node, 2.39 MiB against 0.86 MiB — **2.8×**. tf_tree's marginal megabyte is
  process overhead (binary, stacks, allocator), not tree data; the arena is
  counted once no matter how many map it, which is the point of PSS here.
- **Cycle latency is not an engine measurement** and is reported to show that.
  Both sit near the 100 Hz OS wakeup (65 µs vs 77 µs p50, the ~12 µs gap tracking
  the service difference), and the p99.9 column is the scheduler on a 4-core host
  running up to 16 processes. Rows at 8 and 16 nodes oversubscribe the physical
  cores 2:1 and 4:1; read their tails as a property of this host, not of either
  library.

**Caveats that belong with these numbers.** One run per point, no repeats, so
treat single-row differences under ~10% as noise — the trends across five rows
are what carry weight, not any one cell. The tf2 column is a floor with no
transport, as set out above. And this is a 4-core cloud instance: the absolute
microseconds will differ on the pinned hardware in the runbook below, though the
ratios should not.

**The CPU column was unresolvable before 2026-07-26.** It read `0.0` for every
row of both engines, which looks exactly like the O(1) claim holding. `ProcStats`
took CPU time from `/proc/self/stat`'s `utime + stime`, in 10 ms clock ticks —
and a consumer here spends about 4 ms of CPU per 6-second window, less than one
tick, so the counter read zero. It now reads `/proc/<pid>/schedstat`, which is
nanoseconds. Any CPU-per-node figure quoted from an earlier run is meaningless.

### Where the lookup actually spends itself

`just profile-lookup`, 2026-07-26. Cachegrind over 60 000 depth-3 lookups,
attributed by file.

| file | instructions | branch mispredicts |
|---|---:|---:|
| `tf_tree_core/src/sample.rs` (inlined into `fold_at`) | 27.1% | **76.8%** |
| `tf_tree_math/src/quat.rs` | ~19% | ~0% |
| `tf_tree_math/src/iso3.rs` | ~16% | ~0% |
| `tf_tree_core/src/buffer.rs` | ~13% | ~0% |
| `tf_tree_core/src/plan.rs` | ~9% | ~0% |

Two things follow, and they point at different optimisations:

- **Essentially every mispredict is in the sampling path**, not in the maths.
  The bracket search is already branchless (a `cmp` folded into the index), so
  what remains is the bounds and seqlock structure around it.
- **The maths is ~35% of instructions and mispredicts nothing.** That is the
  shape that rewards SIMD and does not reward branch work — the opposite of
  where intuition sends you after reading the mispredict column.

**This profile was not obtainable before.** `[profile.release]` sets
`strip = "debuginfo"`, so cachegrind, perf and callgrind all fall back to
function-level attribution — and `fold_at` inlines the entire sampling chain
into itself, so the answer was "96.6% of mispredicts are in `fold_at`". True,
and useless, because `fold_at` *is* the hot path. The `profiling` profile
(release codegen, debuginfo kept) is what makes the table above possible.

**Per-*line* attribution works, and the paragraph that used to stand here saying
it did not was wrong.** It read: *"`cg_annotate --auto=yes` in this image emits
no annotated source even with the debuginfo present. That is the next thing to
fix before any of the optimisations above should be attempted."* Re-run
2026-08-04 in the same image, `just profile-lookup`'s exact command at
`n = 20000`: 2547 lines of output, of which 2318 are annotated source, covering
`plan.rs`, `sample.rs`, `buffer.rs`, `arena_view.rs`, `interp.rs`, `iso3.rs` and
`quat.rs` with per-line `Ir` / `Bcm` / `D1mr`. `cg_annotate` is 3.26.0 here and
reports `Annotation: on` in its own metadata block.

**The document already contradicted the claim two paragraphs later** — *"Optimisations
tried and rejected"* below opens by citing per-line evidence (*"per-line profiling
puts two lines of `interp.rs` at ~9% of all instructions"*), which is not a
sentence anybody could write without the annotation working. That contradiction
is what makes this a stale line rather than a disputed measurement; whichever run
produced the original observation, it does not describe this image now.

One caveat that is not stale: `--auto=yes` is **deprecated** in `cg_annotate`
3.26 (`--annotate` is the current spelling, and the two are documented as
identical). It still works, and it is not what the original paragraph was about.

The genuinely broken recipe is the *other* one. `just profile-cachegrind` runs on
the **host**, and the host has no `valgrind` — the recipe's own
`command -v valgrind` guard catches it and says so. `just profile-lookup` and
`just footprint` go through `docker/tf2/run.sh`, and the image installs
`valgrind` for exactly this reason (`docker/tf2/Dockerfile:15-24`). So the
per-line path that works is the containerised one, and a host-side
`profile-cachegrind` needs `valgrind` installed before it can be run at all.

### Optimisations tried and rejected

Kept because a negative result nobody records gets retried.

**Constant divisions in `slerp_weight` → reciprocal multiplies. No effect.**
Per-line profiling puts two lines of `interp.rs` at ~9% of all instructions,
and `slerp_weight` contains four divisions by non-power-of-two constants —
which LLVM may *not* rewrite as reciprocal multiplies, because that changes
rounding and Rust grants no fast-math permission. Four per call, two calls per
slerp, three slerps in a depth-3 lookup: **24 real `divsd` per lookup**, on the
one arithmetic instruction that is not pipelined at one per cycle. It looks
like an obvious win.

It is not one. Moving the reciprocals to `const` and multiplying measured
`p = 0.74` at depth 3 / sclerp, `p = 0.91` at depth 3 / lerpslerp and
`p = 0.67` at depth 6 — no change at any depth. The divisions are *independent
of each other and of the Horner chain*, so they issue in parallel with work
that has to happen anyway; the critical path is the dependent chain of
multiply-adds, not division throughput. Instruction counts mislead here
precisely because they do not model that.

The numerics were checked before the timing, and passed:
`slerp_series_matches_exact_below_threshold` holds at 1e-15 across the whole
threshold range with the reciprocals in place. So the change was safe, and
still not worth making.

**A note on `depth1/sclerp` on this host.** It has now twice shown a ~3%
"regression" (p < 0.05) that did not reproduce on a second run. Treat
single-run depth-1 results here as layout noise unless they repeat; the depth 3
and 6 rows have been stable across every comparison in this document.

### Python: `tf_tree` against `tf2_ros`

`just py-vs-tf2`, 2026-07-26, in the same container. One dynamic edge, 2000
samples at 1 ms, queried inside the retained window so neither engine is
extrapolating. **tf2 is given every advantage available in-process**:
`tf2_ros.Buffer` wraps the same `BufferCore` a real node uses, fed directly —
no DDS, no serialisation, no `TransformListener`. A deployed consumer pays more
than this; nothing here pays less.

| | tf_tree | `tf2_ros` | ratio |
|---|---:|---:|---:|
| scalar lookup | **188.7 ns** | 12 182.3 ns | **64.5×** |
| batch, n = 64 (per sample) | **42.7 ns** | 12 477.5 ns | **292.0×** |
| batch, n = 4096 (per sample) | **34.9 ns** | 12 607.5 ns | **361.7×** |

Re-measured after Phase 2 completed. The earlier run read 178.6 ns scalar
against 11 708.8 ns for tf2; **both** columns moved by about the same 4%, which
is the container, not the code. What is *not* noise is recorded below.

The batch rows deserve a word, because they are the easiest to misread. **tf2
has no batch API**, so its per-sample figure is simply its scalar figure — the
comparison is a vectorised call against the Python loop a user would otherwise
write. That is the honest framing: the absence of a vectorised path *is* the
cost, and reporting only the scalar row would understate what a user
experiences. The scalar row, 65.6×, is the like-for-like number.

Both figures are release builds with LTO. Before `tf_tree_py` gained a
`[profile.release]` — it is excluded from the workspace, so it inherited none —
the same measurements read 253.9 ns scalar and 51.7 ns/sample at n = 4096. The
hot path is a call from the extension into `tf_tree_core`'s batch kernel, which
only inlines across the crate boundary with LTO on.

Against `docs/PHASE3.md` §12.2's gate, measured on the *identical* fixture with
`benches/py_parity.rs` (native: 36.3 ns/sample at n = 4096):

| gate | result | |
|---|---|---|
| 1. scalar `plan.at` p50 under 250 ns | 228.8 ns | **pass** |
| 2. `at_many` at n = 4096 within 1.3× of native | 0.93× | **pass** |
| 3. `at_into` eliminates the allocation | 8.1 µs/call saved at n = 4096 | **pass** |

Criterion 2's parity bench exists because `benches/at_many.rs` uses the deep
mobile-robot fixture; dividing a Python figure by *its* ns/sample would have
produced a meaningless ratio in either direction.

#### A 32-byte struct field cost 4%, and the benchmark is how it was found

Phase 2's fork poisoning needed a `Guard` that refuses every evaluation without
reading the arena. The first version carried an `Option<LookupError>`:

| | `Guard` | Python scalar | native depth-3 |
|---|---:|---:|---:|
| before | 48 B | 178.6 ns | 64.1 ns |
| `Option<LookupError>` field | **80 B** | 196.3 ns | 64.6 ns |
| generation sentinel | 48 B | **188.7 ns** | **62.0 ns** |

`LookupError` is 32 bytes, and `Option` of it is niche-packed to the same 32 —
so the field grew `Guard` by two thirds, on a struct built **once per `at()`
call** on the Python path. Encoding "detached" as a `generation` of `u64::MAX`
instead costs nothing at all: `check_generation` already loads `generation`, and
the poison check folds into the comparison it was already making, on the cold
side of it.

The native row barely moved either way, which is why this needed the Python
benchmark to see: the extension's per-call overhead is where a fatter `Guard`
shows up. Sentinels earn their keep by being unreachable, so
`a_generation_mismatch_is_never_mistaken_for_a_detached_guard` builds the exact
collision — a plan from one arena's generation evaluated against another arena
at generation 0 — and fails against `DETACHED = 0` or `1`.

### Python, multi-process: N nodes on one arena against N private buffers

`just py-mp-bench`, 2026-07-26, 8 cpus, in the same container. Two corrections
landed after the first publication of these numbers, both found in review: the
publisher was running at **50 Hz where `PUB_HZ` says 100** (its tick index was
both derived from elapsed time *and* incremented, so it slept two periods per
push) while the tf2 consumer filled its buffer at the full 100 Hz — so the two
engines were not seeing "the same stream" as claimed; and CPU was sampled by the
coordinator over a window that overlapped tf2's startup. Both are fixed and the
table is re-measured. Eight Python
consumer nodes at 100 Hz over a depth-3 chain, open-loop, with a live publisher
for tf_tree. **This is where the shared arena earns its keep, and it is the row
the single-process comparison cannot show**: a Python `tf2_ros` node
materialises the whole history privately, and every node pays for it again.

The methodology is `crates/tf_tree_bench/src/mp.rs`'s — open loop against
*intended* tick times so a stall shows up as latency rather than as fewer
samples, per-consumer tails rather than one mean, and PSS rather than summed
RSS, which would count the shared arena once per consumer and flatter tf_tree
by exactly the amount being claimed.

| nodes | tf_tree svc p50 | `tf2_ros` svc p50 | ratio | tf_tree PSS | `tf2_ros` PSS |
|---:|---:|---:|---:|---:|---:|
| 1 | **2.2–2.9 µs** | 167–260 µs | 76–89× | 23.6 MiB | 54.2 MiB |
| 2 | **2.2–2.9 µs** | 155–226 µs | 69–78× | 40.1 MiB | 94.3 MiB |
| 4 | **2.6–3.0 µs** | 188–233 µs | 74–77× | 70.4 MiB | 170.5 MiB |
| 8 | **3.1–3.4 µs** | 233–374 µs | **75–110×** | 128.3 MiB | 320.9 MiB |

Latency is given as the range across two runs because **this machine was not
idle** — it had been running benchmarks for hours — and the absolute figures
moved by up to 1.5× between them. The *ratios* and the memory columns did not,
and those are what the claims rest on. On a dedicated machine, report point
values; here, reporting one would be picking a number.

**The slope is the claim, not the totals.** Both engines pay identically for the
Python interpreter and numpy, which dominate the absolute figures. What the
shared arena changes is what each *additional* node costs:

| marginal, per node | tf_tree | `tf2_ros` | ratio |
|---|---:|---:|---:|
| memory (PSS) | **14.9 MiB** | 37.7–38.0 MiB | 2.5× |
| CPU | **0.16–0.17 %** | 2.9–3.8 % | **18–22×** |
| time to first usable lookup | **0–1 ms** | 64–121 ms | ~70× |

**The CPU row previously read 64×, and that was an artifact.** The coordinator
sampled `schedstat` between two of its own sleeps, a window that for tf2
overlapped the consumer's import and fill rather than only its measured loop.
Each consumer now measures its own CPU across its own loop and reports it —
nothing else knows when that loop starts and ends. The corrected 18–22× is
still the O(1)-versus-O(consumers × edges × rate) shape, and it is still the
right column to look at; it is simply not 64×.

The CPU row is `docs/PHASE2.md` §12.4's "O(1) in the number of consumers"
measured rather than asserted: tf_tree's per-node CPU is flat across the sweep
and tf2's rises, which is the shape the claim predicts. The gap grows with the
fleet.

"time to first usable lookup" is the startup cost, and it is structural rather
than incidental. tf_tree joins an arena somebody else is already publishing
into: a handshake, a mapping, and the pages §7.1 populates. tf2 has nothing to
join, so each node fills its own buffer before it can answer anything — which
is also why its p50 rises with node count while tf_tree's does not.

**tf2 is given every advantage again.** Its consumers are fed directly, with no
DDS, no serialisation and no `TransformListener`; a deployed node pays more than
this and nothing pays less. The one asymmetry that favours tf_tree is
architectural rather than a harness choice: a compiled `Plan` resolves the chain
once, and tf2 re-walks it per lookup because it has no equivalent concept. That
*is* the difference §12.1 exists to report.

Two honest caveats. The machine has 8 cpus, so the 8-node row is at the edge of
where scheduling starts to dominate — `cycle p99.9` is mostly OS wakeup at
100 Hz and is reported but not compared. And tf_tree's service p50 is flat
across the sweep to within noise, which is the expected shape but is measured on
one machine, not proven.

#### The consumer loop was 1.5x slower than it needed to be, and the profile said why

The first run of this benchmark used `plan.at(t)` and reported **1 923 ns** at
eight nodes. Attributing that, on a depth-3 chain, release build, in-process:

| | ns |
|---|---:|
| empty Python loop | 5.6 |
| `plan.depth()` — PyO3 dispatch and nothing else | 15.6 |
| **native Rust `plan.at(&g, t)`** | **114** |
| `plan.latest()` | 118 |
| `plan.at(t)` | 211 |
| `plan.at_into(t, buf)`, as first written | **265** |

Three things fell out, two of which killed a hypothesis:

* **`Tree::guard()` costs 1.1 ns**, measured directly in Rust
  (`examples/guard_cost.rs`) after suspecting it. Building a guard per call is
  free; hoisting it out of a loop buys nothing.
* **The output allocation is not the cost.** `np.empty((4,4))` is ~90 ns *from
  Python*, but from Rust it is ~25 ns — the 90 is mostly the Python call.
  Replacing `zeros` with an uninitialized `new` measured **no change** and was
  reverted rather than kept for the story.
* **`at_into` was slower than `at`**, which is the result that pointed at the
  real cost. It called `reject_device_memory` first, which does
  `getattr("__dlpack_device__")` and then **calls** it — a full Python method
  call, ~120 ns, on every invocation. NumPy has had `__dlpack_device__` since
  1.22, so an ordinary `np.empty((4,4))` paid all of it.

A successful cast to `numpy.ndarray` proves the buffer is host memory, because
CuPy and torch arrays are not numpy subclasses. Skipping the probe on that path
leaves §5.5's guarantee intact — the objects that can actually trip it still pay
— and takes `at_into` from 265 ns to **173 ns**, which is now 1.3x faster than
`at` *and* allocates nothing.

Switching the consumer loop to it took the multi-process p50 from 1 923 ns to
**1 321 ns** at eight nodes, and p99.9 from 29.3 us to 15.3 us — both measured
with the same harness on both sides, so the **1.46x improvement stands** even
though the absolute figures above were re-measured afterwards on a busier
machine and against a corrected publisher rate. A node does one lookup per tick
and cannot batch, so this is the path a real deployment takes.

### What is implemented, and what is not

`just shm-test` is the gate: a **separate process**, after `exec`, maps the same
sealed `memfd` and answers **bit-identically** over 512 queries, plus a
read-only (`PROT_READ`) attachment and a check that samples published *after* a
peer attached are visible to it. The reader in that child is the unmodified
Phase 1 reader — [`PHASE2.md`](../PHASE2.md) §4's "zero lines in the read path",
tested rather than asserted.

**Crash consistency: amendments A1–A8 are applied** (`FORMAT_VERSION = 2`). The
arena has a participant table (A6), claims name a participant *slot* rather than
a PID so a writer killed mid-claim leaves no unreclaimable edge (A3), `push`
re-checks the claim epoch so a revoked writer cannot resurrect (A4), the
topology generation and active block publish in a single atomic word (A1) under
an in-arena reapable lock (A2), the sample writer forces slot parity rather than
incrementing it (A5), the header carries a full 16-byte boot id (A7), and the
interning spin is bounded with takeover of a provably-dead claimant (A8).

What is **still** missing is the *lifecycle*, listed in
[`PHASE2.md`](../PHASE2.md) §0.0 and scoped by decision
[`0005`](../decisions/0005-the-shared-memory-seam.md):

- Segments are handed over by **fd inheritance**; the §3.7 `SOCK_SEQPACKET` +
  `SCM_RIGHTS` handshake is not implemented, so a process that is not a child
  cannot attach at all and `tf_tree::open()` does not exist yet.
- **Liveness** comes from a `/proc` heuristic that fails safe (unknown ⇒ alive),
  not from `F_OFD_GETLK` (§5.1).
- **Nothing reaps** (§6.3): `edge::reap` exists and is called only by tests, so a
  participant that dies holding a claim leaks that edge until the arena does.

Those are crash-*recovery* properties rather than
correctness-under-normal-operation properties, and none of the numbers above
depend on them — but they must be in place before this is production-safe.

## The performance suite: contention, scale, duration, and the transport

Everything above this line is measured on one 24-frame fixture, in windows of a
few seconds, against a **quiescent** tree, with `tf2::BufferCore` fed
in-process. Four sections follow, each closing one of those.

Every number here is **indicative**: this host has 4 physical cores with SMT and
an unreadable frequency governor, so it fails `tf_tree_bench`'s own
`Fitness::probe` and every harness in the suite says so in its output. What is
*not* host-dependent is the shape of each curve and the ratios between rows
taken minutes apart on the same machine, and that is what these sections are
for.

### Read scaling with concurrent writers — `just contended-scaling`

[`PHASE1.md`](../PHASE1.md) §11.2 specifies "1/2/4/8/16 reader threads, **4
concurrent writers**, cores pinned". Until now the writers and the pinning were
both in this document's own "not measured" list. `contended_scaling` runs N
reader *processes* and M writer *processes* on one shared arena, each placed on
its own core by `taskset` — processes rather than threads because per-thread
placement needs `sched_setaffinity`, and `CLAUDE.md`'s unsafe budget routes a
new kind of `unsafe` to a decision record.

24 frames, 3 dynamic steps, 3 s per point, 8 logical CPUs:

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

**Four concurrent writers cost about 9%** — of aggregate throughput at 8 readers
(26.55 → 24.09 Mlookup/s) and of p50 (330 → 360 ns). The tail is *flat*: p99.9
does not move, and at 8 readers it is marginally lower under load than without.

`err_slot_recycled + err_slot_contended` was **0** on every row. That is worth
stating precisely, because it is easy to over-read: those counters record reads
that **failed**, not reads that retried. A successful seqlock retry is invisible
to the arena by design, and what it costs shows up in the ~9% above.

The scaling column is the §11.3 gate's, and it still fails on this host for the
reason it always has — 8 threads over 4 physical cores can only exceed 4x via
SMT. What is new is that it now fails by a *measured* margin **under the load the
gate actually specifies** (5.20x) rather than only on an empty road (5.73x).

`svc` is the per-lookup service distribution, taken in a dense loop with one
clock pair per lookup and the clock's own cost measured and reported alongside
(28–31 ns on this host). The first revision of this harness reported latency
only from an open-loop schedule and printed a p50 of **61 µs** for an operation
costing ~300 ns, because at any achievable tick rate the dominant term is the OS
deciding to run you. Both distributions are now reported, separately.

Two cross-checks, because a new harness that disagrees with the old ones is
measuring something else:

* The 1-reader/0-writer row (250 ns p50) lands on `scale_sweep`'s independent
  `robot` measurement (251 ns p50) — different binary, different loop, same
  workload.
* `benches/read_scaling` grew a `read_scaling_writers` group so the portable
  criterion path also stops measuring an empty road. Its writers publish at the
  fixture's **nominal** rates, and at 8 threads it reports 925.9 µs against the
  quiescent group's 933.4 µs — no difference. That is the expected answer and it
  is why `contended_scaling` exists: at 50–1000 Hz a writer is invisible against
  millions of lookups a second, so the pressure has to be applied by a writer per
  core. An earlier revision of that group ran its writers flat out and starved
  the readers so badly the bench could not complete a row in ten minutes; the
  file records it.

### What a writer costs a reader — `TF2_WRITERS=N just tf2-scaling`

The section above is tf_tree alone, because a second process cannot reach a
`tf2::BufferCore` at all. This is the in-process head-to-head, and it is the row
that separates the two designs most sharply.

The writers publish to dynamic edges the query path does **not** traverse. That
is the measurement rather than a courtesy:

* `tf2::BufferCore` takes **one mutex for the whole buffer**, so a write to any
  edge excludes every reader of every other edge.
* tf_tree's rings are per edge with a seqlock per slot, so a write to an edge a
  reader is not reading costs that reader nothing.

Two writers, `fixture_depth6`, 15 rounds, engines interleaved within every round:

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
cost tf2's readers 79% of throughput, 5.9x at p50 and 31x at p99.9** — for
writes to edges those readers never touch.

The recorded stream says the same, more so: 16.6x at one thread and 43.0x at
four.

Two honest notes. tf2's `worst` round ratio falls to 0.54 at one thread, so its
contended single-thread row is soft — the mutex makes it bursty, which is itself
the finding, but the median is what to quote and not the best. And tf2's
throughput *rises* from 1 to 4 threads under writers (0.62 → 0.81 M/s), which is
not scaling: at one thread its single reader loses the mutex to the writers more
often than four readers collectively do.

An earlier revision of this harness gave each writer thread its own stamp counter
starting from the same base. Both engines' state outlives a pass while the
threads do not, so the second pass republished stamps the first had already
written — tf_tree rejected them silently, and tf2 rejected them *with a
`TF_OLD_DATA` warning per sample*, filling the measured window with stderr I/O
and reporting tf2 at 0.36 M/s with a 50% spread. That number would have been
published as tf2's cost under contention. The counter is now shared across every
writer and every pass.

### Scale — `just scale-sweep`

Lookup cost against tree **width**, at a fixed dynamic-step count, which is the
only way to separate size from depth.

| workload | frames | edges | dyn steps | at p50 | latest_common p50 | plan compile | build |
|---|---|---|---|---|---|---|---|
| `recorded` | 10 | 9 | 2 | 80 ns | 60 ns | 116 ns | 0.7 ms |
| `robot` | 24 | 23 | 3 | 251 ns | 100 ns | 144 ns | 2.7 ms |
| `humanoid` | 117 | 116 | 12 | 880 ns | 210 ns | 161 ns | 0.9 ms |
| `av` | 375 | 374 | 14 | 1012 ns | 231 ns | 171 ns | 2.1 ms |
| `fleet_16` | 385 | 384 | 4 | 330 ns | 120 ns | 238 ns | 44 ms |
| `fleet_64` | 1537 | 1536 | 4 | 330 ns | 120 ns | 245 ns | 120 ms |
| `extreme_wide` | 12289 | 12288 | 4 | 320 ns | 111 ns | 244 ns | 364 ms |

**Width is free.** The last three rows hold the dynamic-step count at 4 while the
tree grows 32x, from 385 to 12 289 frames, and `at p50` does not move — it goes
*down* by 10 ns, which is noise. The earlier four-point row above topped out at
375 frames and could only say "primarily depth"; this says it at two orders more.

**Depth is what costs**: `humanoid` (12 steps) to `av` (14 steps) is
880 → 1012 ns, about 66 ns per additional dynamic step, which is the
interpolation cost [`PHASE1.md`](../PHASE1.md) §11.3 predicts.

**Plan compilation is nearly flat too** — 116 ns at 10 frames, 244 ns at 12 289,
and the last three rows (32x the tree) are within 3% of each other. It walks to
the root, so it scales with depth rather than with the tree. `build` does scale,
linearly in samples, and 364 ms for a 12 289-frame arena is a startup cost worth
knowing rather than a surprise.

Ring depth, one edge, stamps swept across the whole ring:

| slots | retained | MiB | at p50 | at p99.9 |
|---|---|---|---|---|
| 8 | 7 | 0.0 | 90 ns | 101 ns |
| 1 024 | 1 023 | 0.1 | 90 ns | 150 ns |
| 16 384 | 16 383 | 1.1 | 91 ns | 160 ns |
| 262 144 | 262 143 | 18.0 | 119 ns | 450 ns |
| 1 048 576 | 1 048 575 | 72.0 | 120 ns | 700 ns |

The binary search behaves: a 131 072x deeper ring costs 30 ns at p50, and 20 of
those 30 arrive in one step — between 16 K and 256 K slots, i.e. between a ring
that fits in cache and one that does not. The **tail** is where that shows
plainly: p99.9 goes 101 → 700 ns. It is a property of the machine as much as of
the engine, and no previous benchmark varied this axis at all.

Publish, one thread round-robin over N edges: 6.84 ns/push at 1 edge, 7.01 at 16,
7.41 at 64, 10.33 at 256. Per-edge isolation holds through 64; the step at 256 is
the working set (256 rings is 18 MiB of first-touched pages), not false sharing —
`EdgeCounters` is padded to 128 bytes precisely so two edges never share a line.

**The limits, printed by the engine rather than copied from a header:**

* **59 651 678 sample slots (~4.00 GiB) in one arena**, past which
  `LayoutError::ArenaTooLarge` — every region offset in the header is a `u32`.
  This was undocumented, and it is the one that binds first on any populated
  tree; `TooManyFrames`/`TooManyEdges` are `u32` counts nothing reaches.
* 16 compiled plan steps (`MAX_DEPTH`), as the section below already records.

### Duration — `just soak`

40 s, 24-frame fixture (10 s of retained history), 2 reader threads, 4 writer
threads, snapshots every 10 s:

| interval | Mlookup/s | p50 | p99.9 | publish→visible p50 | ring laps | RSS | declined |
|---|---|---|---|---|---|---|---|
| 0 | 5.18 | 280 ns | 470 ns | 191 ns | 1.0 | 2636 KiB | 131 ppm |
| 1 | 5.22 | 280 ns | 460 ns | 191 ns | 1.0 | 2636 KiB | 130 ppm |
| 2 | 5.04 | 280 ns | 470 ns | 200 ns | 1.0 | 2656 KiB | 137 ppm |
| 3 | 5.24 | 280 ns | 470 ns | 191 ns | 1.0 | 2656 KiB | 130 ppm |

No drift: p99.9 ends at 1.00x its first interval, RSS grows 20 KiB, and the rings
lapped 3.9 times — which the harness *asserts*, because a soak that never lapped
a ring did not exercise the path it exists for and must fail rather than print a
clean table. Laps are `interval / retained`, both read from the arena, so the
assertion holds for a workload with a different history too.

The `declined` column is the harness's own, not the engine's, and it is reported
as a rate precisely so that is checkable: the readers re-probe the retained
window every few thousand lookups while the writers slide it, so queries aimed at
the oldest end occasionally land just below it. **Stable at ~130 ppm across every
interval** is the expected shape; a rate that *grew* would mean the window was
sliding faster than the readers could follow.

**Publish-to-visible is ~190 ns at p50** and this is the first time it has been
measured. It is [`PHASE5.md`](../PHASE5.md) §9.2's required row: not lookup
latency, but how long after a writer's `push` returns that a *different thread*
can read the sample. A probe writer records when `push` returned and a probe
reader spins until the arena reports that stamp. Its p99.9 is milliseconds and is
not a claim about the engine — the probe reader is one of six runnable threads on
four cores, and what that tail measures is the scheduler descheduling it.

### The transport — `just dds-bench`

Every tf2 comparison above this section feeds `tf2::BufferCore` in-process.
That is deliberately generous to tf2 and is **not** what a deployed node pays:
`mp_bench` says so in its own output ("this tf2 column is a FLOOR ... but no
transport"). This is the run that pays it — one publisher, real DDS, the
container's RMW, [`PHASE4.md`](../PHASE4.md) §5.2's QoS, 4 consumers, 100 Hz,
100 ms query lag, 3 s warm-up discarded, **15 s measured** (`SECONDS_MEASURED`;
every `.out` in the run records `measured_s 15.0`), both arms on stock defaults.

| arm | procs | consumers | svc p50 | svc p99 | svc p99.9 | CPU %/consumer | PSS |
|---|---|---|---|---|---|---|---|
| `tf2.processes` | 4 | 4 | 2.59 µs | 8.64 µs | 12.16 µs | 2.968% | 63.15 MiB |
| `tf2.composed` | 1 | 4 | 1.43 µs | 6.21 µs | 10.50 µs | 0.644% | **24.04 MiB** |
| `tf_tree.composed` | 1 | 4 | **0.77 µs** | **3.62 µs** | **6.18 µs** | 0.656% | 24.76 MiB |
| `tf_tree.processes` | 5 | 4 | 0.90 µs | 8.96 µs | 16.90 µs | **0.725%** | 69.51 MiB |

`procs` is the count the tool prints, so `tf_tree.processes` reads **5** and not
`4+1`: the bridge is a process an operator supervises and it is counted like
one.

Against the ordinary ROS deployment (`tf2.processes`, one listener per node):
**3.4x on p50** for the composed arm (2.59 / 0.77) and **4.1x on CPU** for the
multi-process one (2.968 / 0.725). The multi-process arm's own p50 ratio is
2.9x (2.59 / 0.90), and it is the weaker number for the reason two sections
below: on this unpinned host that arm's p50 is wake-from-idle-dominated.

**The 4.1x is the conservative pairing and is quoted deliberately.** It divides
the *lowest* of the four `tf2.processes` CPU samples by the tf_tree row, so the
arm being argued against gets its best run. The medians of the same four samples
give 4.7x (3.384 / 0.7245). Take 4.1x as the floor of a one-host, four-run
estimate rather than as the measurement.

One run of four, all on this host within the hour; the row-to-row spread is in
the CPU column and it is one-sided. `tf2.processes` measured
2.968 / 3.064 / 5.163 / 3.703 %/consumer across the four — a 74 % spread;
`tf_tree.processes` measured 0.725 / 0.724 / 0.728 / 0.710, a 2.5 % one. The tf2
arm's variance is its four listener threads competing for four cores with the
publisher; the tf_tree arm has one thread doing that work and it shows.

**Two corrections to the previous version of this table, both mine and both
changing what it says.**

*The CPU column was measuring a sleeping thread.* `measure.hpp` read
`/proc/self/schedstat`, which is the **main thread's** file — `mp.rs`'s
`self_cpu_ns` says so in its own doc comment and sums `/proc/self/task/*`
instead, and this header, which describes itself as a mirror of `mp.rs`, did
not. Every arm here does its work on other threads. Measured on this host, two
threads burning 4.004 s of CPU over a 2.003 s window moved
`/proc/self/schedstat` by 0.000336 s. That is where "CPU per consumer is at the
resolution floor for all three arms — 0.003–0.012%" came from: not a floor, an
instrument pointed at the wrong thread. The withdrawal of the old "4.7x on CPU"
reading is itself withdrawn; the column now reads 0.64–2.97% and 4.1x is
measured.

The replacement is `CLOCK_PROCESS_CPUTIME_ID`, not `mp.rs`'s task sum, because
the task sum is a sum over **live** tasks and every consumer here reads its
second sample after joining its query threads — so their CPU is subtracted and
the `uint64_t` difference underflows. That was found in the field, in an attach
process that printed `cpu_ns 18446744073701835266`. The two agree to 0.2 ms
while the threads are alive (1.9481 s against 1.9479 s) and diverge completely
once one exits (8.4117 s against 0.0004 s). **`mp.rs` carries the same latent
hazard and has not been touched**; whether it is reachable there is a question
for that harness.

*The fourth row exists.* See below.

`tf2.composed` is in the table because without it the comparison is a strawman:
it is tf2's *best* case, one listener shared by four threads in one process.
Against it `tf_tree.composed` leads at every percentile in the table:
**1.9x at p50** (1.43 / 0.77), **1.7x at p99** (6.21 / 3.62) and **1.7x at
p99.9** (10.50 / 6.18), at comparable memory.

> **This paragraph used to say tf_tree "trails it at p99, 8.70 µs against 7.90"
> and led "1.5x at p99.9", and neither number is in the table above it.** They
> were the previous revision's row, kept when the row was replaced — a
> comparison stated against measurements the document had already deleted. All
> three ratios above are recomputed from the two rows as printed. Where
> `tf_tree.processes` does lose is stated below; it is not this.

Both arms are the same executable with a different `--mode`, so the schedule, the
query set, the warm-up window and the measurement code are literally the same
code. The publisher plan, the bridge's topology config and the query set are all
*generated* from one workload entry, so §9.3's "identical data" is structural
rather than promised.

#### The fourth arm — `tf_tree.processes`

Every version of this document before this one said there was no multi-process
tf_tree arm, because `tft_bridge_create` built a **heap** arena no second process
could attach to, and `dds_report` printed that gap above its own table on every
run. [`0015`](../decisions/0015-the-bridge-fills-a-shared-arena.md) closed it:
one `bench_consumer --mode tf_tree_bridge` process publishes its arena under
`$TF_TREE_NAME`, four `--mode tf_tree_attach` processes join it read-only with
`tft_tree_open()`, and none of them subscribes to `/tf`. It is §9.1's actual
sentence — *"one bridge plus N `tf_tree` consumers"* — and it is the arm this
project's central claim is about.

**The bridge's cost is inside the row, not beside it.** The bridge process emits
the same stats block as every other process in the arm with `consumers 0`, and
the aggregator sums CPU and PSS across an arm and divides by the summed consumer
count. So the 0.725% above is the whole arm, bridge included, amortized over the
four consumers it serves. The breakdown over a 15 s window:

| | fixed | per consumer | 4 consumers | 16 consumers (extrapolated) |
|---|---|---|---|---|
| `tf2.processes` CPU | — | 0.445 s | 1.78 s | 7.12 s |
| `tf_tree.processes` CPU | 0.362 s (bridge) | 0.0186 s | 0.436 s | 0.66 s |

**A marginal tf_tree consumer costs about 24x less CPU than a marginal tf2 one**
(0.0186 s against 0.445 s), and the bridge's 0.362 s is paid once whatever N is.
That is `PROJECT.md`'s O(1)-in-consumers argument, measured end to end over a
real DDS for the first time. Break-even against tf2 is at roughly one consumer:
the bridge costs less than a single tf2 listener does.

**Every figure in this section is one run of four processes on one host**, and
both means above are means of four. Two significant figures is what that
supports; the ratio is "about 24x", not 23.9x, and the break-even is "about one
consumer", not a number with a decimal point in it. The spread that justifies
the caution is in the CPU column above.

**Where it is worse, at N = 4.** Two places. The previous revision of this
section named only the first, while its own table showed both:

*Memory.* 69.51 MiB against 63.15. The arena is 1.3 MiB and shared; what
dominates is that each of the five processes carries an rclcpp node and a DDS
participant.

> **A per-consumer PSS figure does not compare across these two arms, and this
> paragraph used to be built out of one.** It divided each arm's total by its
> consumer count and subtracted — "13.66 MiB against tf2's 15.79, a 2.13 MiB
> saving" — which is confounded in the flattering direction by construction.
> PSS divides each shared page by the number of processes mapping it, and these
> arms map from a different number of processes: 4 for `tf2.processes`, 5 for
> `tf_tree.processes`. The identical rclcpp text is therefore charged at S/4 to
> a tf2 consumer and S/5 to a tf_tree one **before any architectural difference
> exists**, and the quotient credits tf_tree with the difference. The totals are
> exact and fair — PSS sums correctly across processes, which is the whole
> reason this suite reports it — so the extrapolation has to come from them.

Fit `total(P) = P·private + shared` to each stack's two arms, `P` being the
process count, and read the marginal consumer off `private`:

| | composed, P = 1 | processes | `private` | `shared` |
|---|---|---|---|---|
| `tf2` | 24.04 MiB | 63.15 MiB, P = 4 | (63.15 − 24.04)/3 = **13.04 MiB** | 24.04 − 13.04 = 11.00 MiB |
| `tf_tree` | 24.76 MiB | 69.51 MiB, P = 5 | (69.51 − 24.76)/4 = **11.19 MiB** | 24.76 − 11.19 = 13.57 MiB |

A marginal consumer therefore saves **≈1.85 MiB**, not 2.13, and the crossover —
where `N·13.04 + 11.00` meets `(N+1)·11.19 + 13.57` — is at
`(24.76 − 11.00)/1.85` ≈ **7.4 consumers**, not 7. This row is on the wrong side
of it either way. **Treat both as approximate and single-run**: the fit has two
points per stack and its `private` term is not purely per-process, because a
composed process hosts four query threads where a `.processes` one hosts a
single consumer, so per-consumer thread cost leaks into it. What the fit is good
for is the direction and the order of magnitude, and both say the same thing the
quotient did — this workload does not reach the crossover — with the thumb taken
off the scale.

The saving per consumer is small here because `robot` has 23 edges, so the tf2
`Buffer` this replaces is itself small; a tree with thousands of edges of history
moves that number and this one does not measure it.

*Tail latency, on this host.* `tf_tree.processes` also loses to `tf2.processes`
at **p99 (8.96 µs against 8.64)** and at **p99.9 (16.90 against 12.16)** in the
table above. [`PHASE5.md`](../PHASE5.md) §9.3 requires that to be in the same
table and not a footnote, so it is stated here rather than left for a reader to
derive: memory is not the only column this arm is behind in. The section below
argues those two numbers are the unpinned host's idle behaviour rather than the
engine — and that argument is *why they need pinned hardware before anyone
quotes them*, in either direction.

The attach consumers also hold a full rclcpp node **on purpose**, subscribed to
nothing. Dropping it would take ~14 MiB per process out of the row and measure
"no rclcpp" rather than "no `/tf`", which is not the claim.

#### The `svc` column of both `.processes` arms is wake-from-idle-dominated

`tf_tree.processes` p50 measured **5.89 µs** on the first run of the day and
**0.90, 0.95, 1.02 µs** on three consecutive ones. That is not the engine. The
slow run's distribution is bimodal — p10 0.79 µs, 18.1% of samples under 1 µs,
p50 5.89, p75 10.05 — which is the shape of a thread waking a cold core, not of
a slower lookup.

It was isolated rather than assumed. Same bridge, same **shared** arena, same
rate, same queries, but the four query threads in **one** attach process instead
of four — so the process is never idle — measured **1.00 µs p50** against
`tf_tree.composed`'s 0.78 on a private heap arena. That rules out the memfd
mapping, the page size and the attach path, and leaves the host's idle
behaviour. The irony is exact: **the arm is penalised for the consumer doing so
little that its core has time to go to sleep.** `tf2.processes` swings the same
way and less far (2.59 → 11.07 µs across these runs) because a tf2 listener
process is never idle — it is deserializing `/tf` the whole time.

Both `.processes` rows' latency percentiles therefore need the pinned-hardware
runbook below before they are quoted. **The `tf_tree.processes` CPU and PSS
columns are steady across all four runs — 0.710–0.728 %/consumer, a 2.5 %
spread — and are what this arm is for.** The `tf2.processes` CPU column is
*not*: 2.968–5.163 across the same four, a 74 % spread, which is why the 4.1x
above is quoted from the pairing that flatters tf2 and labelled conservative.
This sentence used to say both columns were steady across all four runs, and
the table two sections up refutes it.

#### A bridge defect this harness found — and fixed

The first run of the tf_tree arm reported **10 070 transforms received, 187
applied, 9 864 dropped as authority conflicts, and 100% of lookups failing** —
against a single publisher and a correctly declared topology.

`tf_tree_bridge::Publisher` was keyed on the **resolved node name**, not on the
GID. `rmw_fastrtps` reports `_NODE_NAME_UNKNOWN_` for an endpoint discovered
before its participant's node information arrives and corrects it on a later
graph walk, so the same publisher was attributed twice under two names. Under the
default `first_writer_wins` the placeholder became the edge's owner and the
corrected name was a *different* publisher, rejected permanently — that policy
never re-inserts.

**Fixed.** [`PHASE4.md`](../PHASE4.md) §5.3 already says the GID is the identity
("match one against the other"); the implementation used the name. `Publisher`
now carries the GID as its identity with the node name as presentation, and
`PartialEq`/`Ord`/`Hash` read the identity alone — hand-written rather than
derived, precisely so a later field cannot silently rejoin the key.

The same change closes the *opposite* defect, which §5.3's own amendment had
already named without fixing: `Publisher::UnknownGid` was a **unit** variant, so
on an RMW that reports GIDs but resolves no names every publisher compared equal
and §5.4's conflict detection was silently off — in exactly the deployment least
able to diagnose it. A GID with no name is now a distinct publisher, and prints
its GID so a diagnostic can tell two of them apart. A publisher with **no GID at
all** stays the unit `Unattributed`, because `0012`'s ladder requires that less
attribution mean less detection and never more stopping.

Two regression tests gate it (`crates/tf_tree_c/tests/bridge.rs`), and both were
checked against mutants: putting the name back in the identity fails the rename
test, and collapsing uncached GIDs to a sentinel fails the two-publishers test.
This same arm now runs at **0 dropped of 16 373 transforms under the default
policy**, which is the end-to-end evidence.

It was found only because the aggregator flags a row whose lookups mostly failed
instead of printing its (excellent) latencies as a result.

## A real difference: maximum chain depth

tf_tree caps a compiled plan at `tf_tree_core::MAX_DEPTH` (**16** steps); a
deeper path is rejected with `LookupError::TreeTooDeep`. **tf2 has no such
limit.**

This is a deliberate design choice — the fixed `[Step; MAX_DEPTH]` array is what
makes `Plan` `Copy`, heap-free and allocation-free — and [`PHASE1.md`](../PHASE1.md) §7.1 argues
16 is generous when real trees are 4-8 deep. It was hit while building the
scaling row above (a 24-deep spine is refused outright), so it is recorded here
rather than discovered by a user.

**If you are migrating from tf2 and your tree is deeper than 16, tf_tree will
refuse the lookup.** Check before adopting.

## Data provenance

The recorded stream is derived from an indoor/outdoor mobile-robot dataset
released under **CC BY 4.0** (DOI [10.5281/zenodo.19894190](https://doi.org/10.5281/zenodo.19894190)).
See [`testdata/tfstream/ATTRIBUTION.md`](../../testdata/tfstream/ATTRIBUTION.md)
for the full attribution and the list of changes made.

Licensing was the binding constraint on dataset choice. Several widely-used
robotics datasets (KITTI, nuScenes, Newer College, Boreas) are CC BY-**NC**-SA,
whose non-commercial clause makes them unusable in a permissively-licensed
repository; Autoware's datasets and TUM RGB-D state no clear license at all.

## What is still not measured

* **The go/no-go latency gate** (depth-3 p50 < 150 ns ScLerp / < 100 ns
  LerpSlerp). Needs dedicated, core-pinned hardware. The indicative numbers above
  are in the right territory, but a mean over a loop on a shared VM is not a p50
  on isolated cores, and must not be reported as one.
* ~~**Concurrent read scaling on an idle machine.**~~ **Done** — the scaling
  tables above are from two runs on an idle host (load < 1.0, nothing but the
  harness), 101 rounds and 100,000 latency samples per point, engines interleaved
  within each round. Every row except the 4-thread one repeats to within 1%
  across the two runs; the 4-thread row is discussed above and this hardware
  cannot measure it precisely.
* **The 6x scaling gate, on >= 8 physical cores.** Measured 5.35x-5.62x on a
  4-core host, where 8 threads can only exceed 4x via SMT. Not a fair test of the
  criterion either way.
* ~~**Read scaling under concurrent writers.**~~ **Done** — `just
  contended-scaling`, above. Four concurrent writers cost ~9% of aggregate
  throughput and nothing at p99.9, with zero slot-failure counters.
* ~~**Per-thread core pinning.**~~ **Done for the multi-process harness** —
  `contended_scaling` places each reader and each writer on its own core with
  `taskset`, one process per core. The criterion benches still pin nothing and
  say so; per-*thread* placement needs `sched_setaffinity`, which `CLAUDE.md`'s
  unsafe budget routes to a decision record.
* **A non-SMT machine.** 4 physical cores cap what an 8-thread row can show.
* ~~**tf2 under concurrent writers.**~~ **Done** — `TF2_WRITERS=N just
  tf2-scaling`, in the section above. It separates the engines more sharply than
  any other row here.
* **tf2 under writers on the *queried* edges.** The sweep above writes edges the
  query path does not traverse, which is the architectural comparison. Writing on
  path additionally slides the queried window under a fixed stamp sweep, so it
  needs the moving-window handling `contended_scaling` has and `tf2_scaling` does
  not. Expect it to hurt both engines; the question is by how much each.
* ~~**tf_tree across processes over DDS.**~~ **Done** — `just dds-bench`'s
  fourth arm, above: one bridge process publishing a shared arena and four
  processes attached to it, 0% failures, with the bridge's CPU and PSS inside
  the row. What is *not* done is its latency on pinned cores; both
  `.processes` arms' `svc` percentiles are wake-from-idle-dominated on this
  host, and the section above measures why rather than asserting it.
* **The memory crossover, measured rather than extrapolated.** At N = 4
  `tf_tree.processes` costs more PSS than `tf2.processes`, and the two-parameter
  fit above puts the crossover near N = 7.4 — approximate, single-run, and from
  two points per stack. Nothing has run it at N = 16, which is the count
  [`PHASE5.md`](../PHASE5.md) §12 criterion 4 is stated at, and running it is
  the only thing that settles the number.
* **A second RMW.** `docker/tf2` carries one, so the DDS numbers' sensitivity to
  the middleware vendor is unmeasured. [`PHASE4.md`](../PHASE4.md) §0.0 already
  records the missing second RMW.
* **An ingest-throughput benchmark.** [`PHASE5.md`](../PHASE5.md) §12 gate 5 is
  still held by nobody. It is an offline path, so this suite deliberately did
  not fold it in.

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

Report p50/p99/p99.9, not means — [`PHASE1.md`](../PHASE1.md) §11.2 is explicit that the tail is
what a control loop cares about.

The performance suite above is run the same way, and its harnesses **refuse** on
a busy machine rather than producing a number that describes somebody else's
workload:

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

`bench_ab` reads the direction and the tolerance from the file rather than
inferring either from a key name, and exits non-zero on a regression, so it drops
into a bisect script unwrapped. Two runs of the same build must report every row
as `noise` — that property is the reason to trust it, and it is checked.
