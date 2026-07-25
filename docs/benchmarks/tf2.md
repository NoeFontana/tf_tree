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
```

All five run in a container (`docker/tf2/`), so no ROS install is needed on the
host.

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

### Measuring the measurement: three biases found and removed

Every bias found so far favoured tf_tree. They are listed here, with what each
cost, because a benchmark whose author only checked in one direction is not
evidence.

| # | Bias | Cost charged to tf2 | Found by |
|---|---|---|---|
| 1 | `CString::new` x2 per call in the Rust binding | **63-65 ns (14-18%)** | code review |
| 2 | `const char*` -> two `std::string` temporaries at the C++ call site | **~17 ns (7%)** | native C++ control |
| 3 | Residual FFI boundary (cross-TU, no inlining, extra copy) | ~21 ns (8%) | native C++ control |

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
| One sample onto one edge | **8.7 ns** | 123 ns | **14.1x** |

Both publish paths are allocation-free. tf_tree's `push` takes no strings; the
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

Throughput, million lookups/s, recorded stream:

| Threads | tf_tree | tf2 | Ratio | tf_tree vs 1thr | tf2 vs 1thr |
|---|---|---|---|---|---|
| 1 | 10.64 | 3.65 | 2.9x | 1.00x | 1.00x |
| 2 | 19.76 | 1.84 | 10.8x | 1.86x | **0.50x** |
| 4 | 37.80 | 1.56 | 24.3x | 3.55x | **0.43x** |
| 8 | 64.71 | 1.12 | 57.9x | 6.08x | **0.31x** |

tf_tree scales; **tf2 anti-scales** — more threads make it slower than one
thread, the signature of a contended global mutex.

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
| 1 | tf_tree | 130 ns | 180 ns | 290 ns | 5.2 us |
| 1 | tf2 | 291 ns | 821 ns | 1.4 us | 7.6 us |
| 8 | tf_tree | 160 ns | 230 ns | **350 ns** | 6.8 us |
| 8 | tf2 | 3.4 us | 48 us | **87 us** | 183 us |

At 8 threads tf_tree's p99.9 is 350 ns against tf2's 87 us — a factor of ~250.
For a control loop that is the difference between a bounded and an unbounded
worst case.

**Caveat on the scaling factor.** This host has 4 physical cores; tf_tree's 6.08x
at 8 threads is SMT-assisted and should not be quoted as a clean scaling number.
The 4-thread figure (3.55x on 4 cores) is the defensible one. The harness prints
the physical core count for exactly this reason.

Two internal cross-checks that the harness measures what it claims: for each
engine, p50 minus the ~20 ns timer overhead matches the figure implied by its
independently batch-timed throughput.

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
* **Concurrent read scaling on an idle machine.** The harness exists and works
  (`just tf2-scaling`); the numbers above are from a smoke run on a busy host and
  must be re-taken before they are cited.
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
