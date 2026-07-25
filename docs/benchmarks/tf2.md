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
