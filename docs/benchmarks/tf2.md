# tf_tree vs ROS 2 `tf2` — validation and benchmarks

This document records what has actually been measured against ROS 2's `tf2`, how,
and what the numbers do and do not support. Numbers here are **reproducible** —
every row names the command that produced it.

Reproduce everything with:

```bash
just tf2-differential   # correctness, synthetic fixture
just tf2-replay         # correctness, real recorded /tf stream
just tf2-bench          # performance, head-to-head
```

All three run in a container (`docker/tf2/`), so no ROS install is needed on the
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

### Steady-state lookup

1024 queries per iteration, stamps swept across the history window so the bracket
search does real work rather than repeatedly hitting one cached pair.

| Load | tf_tree | tf2 | Ratio |
|---|---|---|---|
| Fixture, depth 6 | 179 ns/lookup | 414 ns/lookup | **2.3x** |
| Recorded stream, depth 3 | 93 ns/lookup | 311 ns/lookup | **3.3x** |

### Where the win comes from

tf_tree's structural claim (decision `0003` / D3) is that it compiles the
topology walk **once** into a `Plan` and thereafter only samples, whereas tf2
walks the tree on every `lookupTransform`. Equalising that away would benchmark
an engine nobody would ship — but it is worth isolating, so the suite also
measures tf_tree recompiling a fresh plan for *every single query*:

| Load | tf_tree (plan reused) | tf_tree (replanned every query) | tf2 |
|---|---|---|---|
| Fixture, depth 6 | 179 ns | 355 ns | 414 ns |
| Recorded stream | 93 ns | 238 ns | 311 ns |

So plan reuse is worth about 2x — and tf_tree is still faster than tf2 *even when
throwing the plan away every time*. The advantage is therefore not solely
architectural bookkeeping; the sample path itself is faster.

### Scaling with tree size and depth

Deepest-pair lookup (leaf sensor back to root), 256 queries per iteration.

| Shape | tf_tree | tf2 | Ratio |
|---|---|---|---|
| 12 frames, depth 4 | 262 ns | 1367 ns | **5.2x** |
| 35 frames, depth 7 | 520 ns | 3189 ns | **6.1x** |
| 117 frames, depth 13 | 1056 ns | 6812 ns | **6.5x** |
| 375 frames, depth 15 | 1232 ns | 7086 ns | **5.8x** |

Both engines scale primarily with **depth** rather than frame count: going from
117 to 375 frames (3.2x the tree) while adding only two levels costs tf_tree 17%
and tf2 4%. The ratio holds between roughly 5x and 6.5x across the whole range.

### Publish

| | tf_tree | tf2 | Ratio |
|---|---|---|---|
| One sample onto one edge | **8.6 ns** | 142 ns | **16.4x** |

Both sides are bounded, or this would degenerate into a memory-growth benchmark
— but they are bounded *differently*, and that difference is part of what is
being compared. tf_tree's ring is count-bounded: a fixed power-of-two slot count,
overwritten in place, never allocating (this is invariant 8, enforced by the
zero-allocation gate). tf2's cache is time-bounded and prunes on insert. The
benchmark uses a 10 s tf2 cache, the realistic ROS default.

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
* **Read scaling under concurrent writers.** The `read_scaling` bench has no
  concurrent writer, and this host plateaus past 4 threads for reasons unrelated
  to the code.
* **tf2 under multi-threaded read load.** `BufferCore` takes an internal mutex
  per lookup; tf_tree's readers are lock-free. That is likely the largest
  remaining difference and is the obvious next measurement — on hardware where
  the result would mean something.

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
