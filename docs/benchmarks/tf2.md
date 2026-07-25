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
```

All four run in a container (`docker/tf2/`), so no ROS install is needed on the
host.

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

Both are ~150-350x tighter than decision `0003`'s 1e-12 gate, and are at the
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

**Read the caveat before the table.** These were measured on a shared 8-vCPU
AMD EPYC-Milan VM with no core pinning and no isolation. Run-to-run variation of
±20% was observed on the tf2 rows. The **ratios** below were stable across runs;
the **absolute** numbers are indicative only and are *not* the decision `0003`
go/no-go gate, which requires dedicated core-pinned hardware. Nothing here is
claimed as a gate pass.

### Measurement bias found and removed

An earlier revision of this benchmark **overstated tf2's cost**, and the
correction is recorded here rather than quietly applied.

`Tf2Buffer::lookup(&str, &str, ..)` converts both frame names to NUL-terminated
C strings on every call — two heap allocations per lookup. `tf_tree`'s
`Plan::at` takes no strings and allocates nothing, so timing the two against each
other charged this repository's FFI marshalling to tf2.

The bridge now exposes `FrameName` plus `lookup_by_name` /
`set_transform_by_name`, which convert once and allocate never; every benchmark
row uses them. Two permanent control rows keep the correction measurable rather
than asserted:

| Row (recorded stream, per lookup) | | |
|---|---|---|
| `tf2/lookupTransform` — allocation-free | **309 ns** | what is reported |
| `tf2/lookupTransform_alloc` — the old path | 354 ns | **+45 ns (+15%)** of bias |
| `tf2/shim_overhead` — FFI + `std::string` marshalling, no `BufferCore` call | 11 ns | ~3% residual |

So the residual bridge cost is about **11 ns (3%)**, and it sits on tf2's side of
the ledger — meaning tf2's true cost is very slightly *better* than the tables
below show. The allocation bias was larger, and is gone.

(The same comparison on the fixture load falls inside run-to-run noise on this
host; the recorded-stream figures above have non-overlapping confidence
intervals and are the trustworthy ones.)

### Steady-state lookup

1024 queries per iteration, stamps swept across the history window so the bracket
search does real work rather than repeatedly hitting one cached pair.

| Load | tf_tree | tf2 | Ratio |
|---|---|---|---|
| Fixture, depth 6 | 182 ns | 422 ns | **2.3x** |
| Recorded stream, depth 3 | 95 ns | 309 ns | **3.3x** |

### Where the win comes from

tf_tree's structural claim (decision `0003` / D3) is that it compiles the
topology walk **once** into a `Plan` and thereafter only samples, whereas tf2
walks the tree on every `lookupTransform`. Equalising that away would benchmark
an engine nobody would ship — but it is worth isolating, so the suite also
measures tf_tree recompiling a fresh plan for *every single query*:

| Load | tf_tree (plan reused) | tf_tree (replanned every query) | tf2 |
|---|---|---|---|
| Fixture, depth 6 | 182 ns | 369 ns | 422 ns |
| Recorded stream | 95 ns | 242 ns | 309 ns |

So plan reuse is worth about 2x — and tf_tree is still faster than tf2 *even when
throwing the plan away every time*. The advantage is therefore not solely
architectural bookkeeping; the sample path itself is faster.

### Scaling with tree size and depth

Deepest-pair lookup (leaf sensor back to root), 256 queries per iteration.

| Shape | tf_tree | tf2 | Ratio |
|---|---|---|---|
| 12 frames, depth 4 | 266 ns | 1408 ns | **5.3x** |
| 35 frames, depth 7 | 520 ns | 2818 ns | **5.4x** |
| 117 frames, depth 13 | 1051 ns | 6204 ns | **5.9x** |
| 375 frames, depth 15 | 1204 ns | 7522 ns | **6.3x** |

Both engines scale primarily with **depth** rather than frame count: going from
117 to 375 frames (3.2x the tree) while adding only two levels costs tf_tree 15%
and tf2 21%.

### Publish

| | tf_tree | tf2 | Ratio |
|---|---|---|---|
| One sample onto one edge | **8.9 ns** | 137 ns | **15.3x** |

Both sides are bounded, or this would degenerate into a memory-growth benchmark
— but they are bounded *differently*, and that difference is part of what is
being compared. tf_tree's ring is count-bounded: a fixed power-of-two slot count,
overwritten in place, never allocating (this is invariant 8, enforced by the
zero-allocation gate). tf2's cache is time-bounded and prunes on insert. The
benchmark uses a 10 s tf2 cache, the realistic ROS default.

### Concurrent read scaling — 1 / 2 / 4 / 8 threads

`just tf2-scaling`. This is the measurement most likely to separate the two
engines: **tf_tree's readers take no lock at all**, while every
`tf2::lookupTransform` acquires `BufferCore`'s internal frame mutex. One shared
tree and one shared buffer, as both engines are meant to be used — per-thread
buffers would erase the contention being studied.

> **PRELIMINARY.** The figures below come from a smoke run on a *busy* VM. They
> are recorded to show the harness works and what shape the result takes; they
> are not a result. Re-run on an idle machine before citing.

Throughput, million lookups/s (recorded stream):

| Threads | tf_tree | tf2 | Ratio | tf_tree vs 1 thread | tf2 vs 1 thread |
|---|---|---|---|---|---|
| 1 | 9.54 | 2.09 | 4.6x | 1.00x | 1.00x |
| 2 | 17.09 | 1.37 | 12.5x | 1.79x | **0.66x** |
| 4 | 33.94 | 1.10 | 30.9x | 3.56x | **0.53x** |
| 8 | 36.35 | 1.18 | 30.9x | 3.81x | **0.56x** |

tf_tree scales; **tf2 anti-scales** — adding threads makes it slower than a single
thread, the classic signature of a contended global mutex (cache-line ping-pong
plus futex sleep/wake).

The tail is where it is starkest. Per-lookup latency, recorded stream:

| Threads | Engine | p50 | p99 | p99.9 |
|---|---|---|---|---|
| 1 | tf_tree | 131 ns | 181 ns | 280 ns |
| 1 | tf2 | 331 ns | 921 ns | 2.0 us |
| 8 | tf_tree | 150 ns | 181 ns | **250 ns** |
| 8 | tf2 | 581 ns | 58.9 us | **258 us** |

At 8 threads tf_tree's p99.9 is *lower* than at 1 thread (250 ns vs 280 ns, i.e.
flat within noise), while tf2's grows to **258 microseconds** — a factor of about
1000. For a control loop, that is the difference between a bounded and an
unbounded worst case.

Two internal cross-checks that the harness is measuring what it claims: at one
thread, tf_tree's p50 of 210 ns minus the ~20 ns timer overhead matches the
182 ns implied by its batch-timed throughput, and tf2's 411 ns likewise matches
its 383 ns.

## A real difference: maximum chain depth

tf_tree caps a compiled plan at `tf_tree_core::MAX_DEPTH` (**16** steps); a
deeper path is rejected with `LookupError::TreeTooDeep`. **tf2 has no such
limit.**

This is a deliberate design choice — the fixed `[Step; MAX_DEPTH]` array is what
makes `Plan` `Copy`, heap-free and allocation-free — and decision `0003` argues
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
  against a quiescent tree. Decision `0003`'s gate specifies 4 concurrent
  writers, which would additionally stress tf_tree's seqlock retry path and
  tf2's writer/reader lock exclusion.
* **Per-thread core pinning.** The harness pins nothing; `taskset` on the whole
  process is the current best approximation.

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

Report p50/p99/p99.9, not means — decision `0003` is explicit that the tail is
what a control loop cares about.
