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
| Heap held | 1,388,352 B | 1,421,392 B |
| Bytes per stored sample | 109.6 | **112.8** |
| Bytes per *declared slot* | **72.4** | n/a |
| Allocations to build | **96** | 88,459 |
| Allocations per published transform | **0** | 1.00 |
| Allocations per lookup | **0** | **0** |

**tf_tree is not meaningfully smaller — it is within 2.4%.** That is worth
stating plainly, because the arena design invites the assumption that it would
win here, and it does not.

The two per-sample figures differ because tf_tree's rings are sized by *declared
capacity*, not by what is stored: `Capacity::history` rounds each ring up to a
power of two, so a 1 kHz edge over 10 s asks for 10,000 slots and reserves
16,384. At 72.4 B/slot (a 64 B cacheline-padded `Iso3` plus an 8 B stamp)
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
| Instructions | **2,105** | 2,900 | 4,085 |
| L1-D misses | **0.002** | 0.002 | **18.5** |
| LL-D misses | 0.00005 | 0.00005 | 0.0003 |
| Branch mispredicts | **8.16** | 8.19 | 14.00 |
| — of which *indirect* | **0.00002** | 0.00002 | **6.00** |

Against tf2 on the comparable policy: **1.94x fewer instructions, ~9,700x fewer
L1-D misses, and effectively zero indirect branch mispredicts against six.**

The six indirect mispredicts are virtual dispatch — tf2 reaches its per-frame
caches through `TimeCacheInterface`, and the target is unpredictable because a
walk visits a different frame each step. tf_tree's compiled `Plan` is a flat
`[Step; 16]` with no dynamic dispatch anywhere on the path, so the indirect
predictor is never consulted.

**Two caveats that cut against the headline.** First, LL-D misses are ~0 for
*both* engines: this fixture's whole working set is 1.4 MB and fits in L3, so
tf2's 18.5 L1 misses per lookup are being served by L2/L3, not DRAM. On a tree
large enough to fall out of L3 those become memory accesses and the gap widens —
but that is a prediction, and it is not measured here. Second, the instruction
ratio (1.94x) is *smaller* than the measured wall-clock ratio (2.7x), so roughly
a third of the observed speed advantage is not explained by executing fewer
instructions. The mispredict and cache columns are the likely remainder, along
with tf2's per-lookup mutex, but attributing it precisely needs cycle counters
this host does not permit (`perf_event_paranoid=4`).

A third caveat applies to the mispredict column specifically: **cachegrind's
branch predictor is a simple two-level model, not a Zen 3 TAGE.** Comparing two
engines under the same model is sound, and that is all this table does; reading
"8.16" as the count a real CPU incurs is not. Acting on that distinction matters
— the 8.16 figure was first read as the bracket search's data-dependent branch,
and rewriting that search branchlessly recovered only 0.46 of it, because LLVM
had already emitted a `cmov`. The mispredicts are spread across `fold_at`'s 124
conditional branches per lookup, not concentrated in the search.

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
publisher running throughout. Foreign load 2–5% on every row; none flagged
`NOISY`.

**tf_tree**

| nodes | svc p50 | svc p99 | svc p99.9 | cyc p50 | cyc p99 | cyc p99.9 | CPU %/node | PSS MiB |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2.94 | 5.22 | 6.94 | 67.1 | 95.2 | 970.8 | 0.125 | 3.93 |
| 2 | 2.64 | 5.50 | 16.00 | 67.6 | 117.8 | 888.8 | 0.130 | 4.88 |
| 4 | 2.34 | 4.58 | 6.88 | 67.1 | 89.6 | 909.3 | 0.120 | 6.20 |
| 8 | 2.18 | 4.80 | 30.72 | 65.0 | 95.2 | 1638.4 | 0.111 | 11.12 |
| 16 | 1.99 | 3.82 | 20.74 | 64.5 | 82.4 | 2211.8 | 0.106 | 18.88 |

**tf2** (floor — see above)

| nodes | svc p50 | svc p99 | svc p99.9 | cyc p50 | cyc p99 | cyc p99.9 | CPU %/node | PSS MiB |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 13.63 | 24.70 | 41.47 | 77.8 | 96.3 | 321.5 | 0.233 | 5.11 |
| 2 | 14.27 | 29.06 | 43.26 | 78.3 | 153.6 | 1007.6 | 0.227 | 7.28 |
| 4 | 14.91 | 28.16 | 159.74 | 78.8 | 115.2 | 1220.6 | 0.246 | 11.13 |
| 8 | 14.66 | 26.50 | 41.73 | 77.3 | 124.9 | 3571.7 | 0.230 | 18.30 |
| 16 | 14.59 | 28.16 | 60.67 | 77.3 | 105.0 | 3735.6 | 0.235 | 37.77 |

Times are microseconds. What the two tables say:

- **Service latency: 4.6× at one node, 7.3× at sixteen** (13.63 → 2.94 µs; 14.59
  → 1.99 µs), and the same ratio at p99. The gap *widens* with node count because
  tf_tree's median falls as consumers are added while tf2's is flat — the shared
  arena stays warm across readers, whereas each `BufferCore` must warm its own.
  This is not a claim that consumers make tf_tree faster; it is a claim that they
  do not make it slower, which is the deployment question.
- **CPU per node: 2.2× (0.235% vs 0.106% at sixteen nodes)**, and — the actual
  `PHASE2.md` §12.4 claim — tf_tree's *falls* from 0.125% to 0.106% across a 16×
  increase in consumers while tf2's stays flat at ~0.23%. Both are O(1) in
  consumers here; only tf_tree is O(1) *and* cheap. Note that tf2 being flat is a
  property of the floor: with a real `TransformListener` each consumer would
  deserialize the full `/tf` stream itself, which is where the O(consumers) term
  actually enters.
- **Memory: 2.0× at sixteen nodes (37.77 vs 18.88 MiB PSS)**, or per *marginal*
  node, 2.18 MiB against 1.00 MiB — 2.2×. tf_tree's marginal megabyte is process
  overhead (binary, stacks, allocator), not tree data; the arena is counted once
  no matter how many map it, which is the whole point of PSS here.
- **Cycle latency is not an engine measurement** and is reported to show that.
  Both sit near the 100 Hz OS wakeup (65 µs vs 78 µs p50, the ~12 µs gap tracking
  the service difference), and the p99.9 column — 0.9–3.7 ms for both — is the
  scheduler on a 4-core host running up to 16 processes. Rows at 8 and 16 nodes
  oversubscribe the physical cores 2:1 and 4:1; read their tails as a property of
  this host, not of either library.

**Caveats that belong with these numbers.** One run per point, no repeats, so
treat single-row differences under ~10% as noise — the trends across five rows
are what carry weight, not any one cell. The tf2 column is a floor with no
transport, as set out above. And this is a 4-core cloud instance: the absolute
microseconds will differ on the pinned hardware in the runbook below, though the
ratios should not.

**The CPU column was wrong until this run.** It read `0.0` for every row of both
engines, which looks exactly like the O(1) claim holding. `ProcStats` took CPU
time from `/proc/self/stat`'s `utime + stime`, in 10 ms clock ticks — and a
consumer here spends about 4 ms of CPU per 6-second window, less than one tick,
so the counter read zero. It now reads `/proc/<pid>/schedstat`, which is
nanoseconds. A test spins 3 ms and requires the reading to see it; against the
old code it fails with *"3 ms of spinning read as 0 ns of CPU"*. Any CPU-per-node
figure quoted from a run before 2026-07-26 is meaningless.

### What is implemented, and what is not

`just shm-test` is the gate: a **separate process**, after `exec`, maps the same
sealed `memfd` and answers **bit-identically** over 512 queries, plus a
read-only (`PROT_READ`) attachment and a check that samples published *after* a
peer attached are visible to it. The reader in that child is the unmodified
Phase 1 reader — [`PHASE2.md`](../PHASE2.md) §4's "zero lines in the read path",
tested rather than asserted.

What is **not** implemented, and what each would protect against, is listed in
[`PHASE2.md`](../PHASE2.md) §1 and §5-6. The short version: this is the mapping,
not yet the lifecycle. Segments are handed over by fd inheritance rather than the
`SOCK_SEQPACKET` + `SCM_RIGHTS` handshake, and there is no participant registry,
no liveness detection and no reaping — so a participant that **dies while holding
a claim** leaks that edge, and one that dies mid-topology-mutation could wedge
readers (amendments A1-A4). Those are crash-consistency properties, not
correctness-under-normal-operation properties, and the numbers above do not
depend on them. They must be in place before this is production-safe.

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
* **Read scaling under concurrent writers.** Every reader benchmark here runs
  against a quiescent tree. [`PHASE1.md`](../PHASE1.md) §11.2 specifies 4 concurrent
  writers, which would additionally stress tf_tree's seqlock retry path and
  tf2's writer/reader lock exclusion.
* **Per-thread core pinning.** The harness pins nothing; `taskset` on the whole
  process is the current best approximation.
* **A non-SMT machine.** 4 physical cores cap what an 8-thread row can show.

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
