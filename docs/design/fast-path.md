# Design: the next-generation lookup hot path

**Status:** partly implemented, partly **falsified by measurement**. Levers 1, 1b and the scalar cursor (§16) are in; Levers 2 and 3 are rejected; 4 and 5 are open. §11 is the projected-versus-measured table; §12 is the per-term decomposition and ranks the bracket search first.
**Measured on:** AMD EPYC-Milan, 4 physical cores, 2445 MHz fixed, idle
**Reproduce:** `cargo run --release -p tf_tree_bench --example cost_model`

---

## 1. Measure first

Cost is linear in dynamic steps: about 5 ns fixed + 86 ns per dynamic step (depth 3, capacity 4096: 263.5 ns; capacity 64 to 65 536: 256.7 to 308.3 ns). Exact-stamp hit versus between-samples: 119.6 vs 269.9 ns, so interpolation is 50.1 ns per step. `ScLerp` costs a further +15 ns/step over `LerpSlerp`.

Initial per-step model (superseded by §12): interpolation ~50 ns, bracket search ~21 ns, reads/composition/bounds ~15 ns.

---

## 3. Lever 1 — transcendental-free slerp weights (in)

With `a` either `s` or `1 − s`, and `u = θ²`:

```
sin(aθ)/sin(θ) = a·[ 1 + (1−a²)·u/6
                       + (1−a²)(7−3a²)·u²/360
                       + (1−a²)(31−42a²+11a⁴)·u³/15120 + … ]
```

Horner in `u`: no transcendental, divide or branch. `u` comes from the chord, not `acos`: for unit quaternions after the sign fix, `h = |q1 − q0|²/2` from component differences, and `θ² = 2h·(1 + h/6 + 3h²/40 + …)`.

Threshold rules (`docs/PHASE1.md` §3.3):

1. Sweep θ on a log grid 1e-12 to π against the exact path and a high-precision reference.
2. Pick `THETA_SLERP_SMALL` where the error curves cross; record the table in the source.
3. Keep the exact path above the threshold.
4. Reference/fast differential proptest (D13), 10⁵ pairs including near-identity and near-π.

The tf2 differential stays far below 1e-12; a material move means the series is wrong.

---

## 4. Lever 2 — restructure `fold_at` into locate / interpolate / compose (rejected, §14)

Proposal: (1) locate all *d* bracket pairs so the dependent load chains overlap; (2) interpolate all *d* lanes at once, by autovectorisation only (`tf_tree_math` is `#![forbid(unsafe_code)]`); (3) compose serially, with a fixed `[_; MAX_DEPTH]` stack scratch.

---

## 5. Lever 3 — interpolation-seeded bracket search — **FALSIFIED, not implemented**

Proposal: `guess = lo + (t − t_lo) · (hi − lo) / (t_hi − t_lo)`, then a few branchless correction steps, falling back to binary search. §10 made it conditional on the seed-error distribution on real data (`cargo run --release -p tf_tree_bench --example search_seed`):

| Stream | Jitter (CV) | Seed error p50 | p99 | Within ±2 |
|---|---|---|---|---|
| Synthetic fixture (isochronous) | 0.00 | 0 | 0 | 100% |
| Recorded, `base_link->*_link` | 1.44 | **11** | 42 | 16% |
| Recorded, `odom_combined->base_footprint` | 2.85 | **48** | 90 | 8% |

A real robot publishes intermittently (29–44 gaps over 3x the median), so a linear index-vs-time seed lands 11–48 indices off where a binary search needs ~8 probes. The intent survives in `sample_from`'s galloping cursor (§16).

`bracket` is not branchless: its mask-select compiles to a conditional branch. See [`0053`](../decisions/0053-the-branchless-bracket-that-branches.md).

---

## 6. Lever 4 — skip the per-slot seqlock for all but the newest slot (open)

`push` only writes slot `head & mask`. A reader of logical index `i` with `head − i ≥ 2` reads a slot no writer touches until the ring laps, and the trailing `head − i > retained` revalidation detects lapping. Saves about 5 ns/step.

**The most dangerous item here**: it resembles the "weakened an ordering because x86 passed" smell in `docs/PROJECT.md` §6. It does not land without:

- a loom test for reader-vs-writer at `head − 1` and `head − 2`,
- a loom test across a lapping writer,
- the existing wrapped-ring proptest passing,
- Miri clean.

---

## 7. Lever 5 — SIMD across stamps for `at_many` / `at_adaptive` (open)

Stamps in a batch are independent, so SIMD across stamps has no ordering constraint.

---

## 9. A finding this exposed: the gate may not be met

`docs/PHASE1.md` §11.3 requires depth-3 hot lookup p50 under 150 ns (ScLerp) and 100 ns (LerpSlerp). A depth-3 chain of **three dynamic edges** at capacity 4096 measured 264 ns LerpSlerp, 312 ns ScLerp; a recorded stream with mostly static edges measured 94 ns. The spec does not say which the gate means, so a benchmark row should state the number of *dynamic* steps, not just the depth.

---

## 10. Order of work, and what would falsify each step

1. **Lever 1**, with the threshold sweep. Falsified if the series cannot reach 1e-15 below a threshold covering real inter-sample arcs, or the tf2 differential moves materially.
2. **Lever 3.** Falsified if real streams are jittery enough that the seeded guess misses often.
3. **Lever 5.** Lowest risk.
4. **Lever 2**, only after 1 and 3, and only if the asm shows vectorisation.
5. **Lever 4**, last, only with the loom coverage in §6.

---

## 11. Results — what each lever actually returned

| Lever | Projected | Measured | Verdict |
|---|---|---|---|
| **1** — transcendental-free `slerp` | 50 → ~10 ns/step | 50.1 → 27.0 ns/step; depth-3 263.5 → 197.7 ns | **In** |
| **1b** — transcendental-free `screw_pow` | *not proposed* | ScLerp 51.6 → 43.6 ns/eval (−15%) | **In** |
| **3** — interpolation-seeded search | 21 → ~4 ns/step | seed misses by 11–48 indices on real data | **Rejected** (§5) |
| **3b** — mask-select `bracket` | *not proposed* | −1.2% instructions; 237.0 → 231.2 ns @ cap 16384 | **In**; still a branch, see [`0053`](../decisions/0053-the-branchless-bracket-that-branches.md) |
| **2** | — | 1.0–1.15x, no ILP to recover | **Rejected** (§14) |
| **4**, **5** | — | not attempted | Open |

The explanation for a cost needs its own measurement, separate from the measurement of the cost.

Depth-3, three dynamic steps, capacity 4096, pinned: 217 ns `LerpSlerp`, 296 ns `ScLerp`. Against §11.3's gate this still fails by ~2x; §9's flag stands.

---

## 12. The residual, measured — and the search is the largest term

**Reproduce:** `taskset -c 2 cargo run --release -p tf_tree_bench --example step_cost`
**Measured on:** AMD EPYC-Milan, 4 physical cores, L1d 32 KiB, L2 512 KiB, idle

Per dynamic step, capacity 4096, `LerpSlerp`, stamps landing between samples. Marginal from the depth sweep: **72.5 ns/step**.

| Term | ns/step | Share | How measured |
|---|---|---|---|
| **Bracket search** | **24.3** | **34%** | `sample(exact) − sample(Hold)` |
| **Interpolation** | **22.2** | **31%** | `sample(between) − sample(exact) − read_slot` |
| Fold overhead | ~11.5 | 16% | residual against the depth sweep |
| `Iso3` composition | 6.8 | 9% | direct, chained |
| `read_slot` ×2 | 6.6 | 9% | direct |
| `ArenaView::sampler` | 1.9 | 3% | direct |
| Ring preamble | 1.7 | 2% | `sample(Hold) − read_slot` |
| Interp-policy dispatch | ~0 | 0% | `guard_sample − sampler − sample(between)` |
| **sum** | **75.0** | | vs 72.5 measured |

### The search is a cache cliff, not a probe curve

Capacity sweep at depth 1, with `Hold` (one pose slot, no search) as the control:

| capacity | stamps | poses | `sample(exact)` | `sample(Hold)` | Δ/log2 |
|---|---|---|---|---|---|
| 64 | 0.5 KiB | 4 KiB | 12.78 | 5.01 | — |
| 256 | 2 KiB | 16 KiB | 13.45 | 4.97 | 0.34 |
| 1 024 | 8 KiB | 64 KiB | 14.37 | 4.97 | 0.46 |
| 4 096 | **32 KiB** | 256 KiB | 32.53 | 4.97 | **9.08** |
| 16 384 | 128 KiB | 1 024 KiB | 43.31 | 5.03 | 5.39 |

`Hold` is flat across a 256x range of pose-array size, so shrinking `PoseSlot` buys nothing. The whole cliff is the stamp array: flat while it fits L1d, then ~9 ns per doubling. Probe cost is ~1 ns while resident and ~2.3–2.7 ns after; it is a step function of `capacity × 8 bytes` against L1d.

Real edges are sized by `Capacity::history(rate_hz, HISTORY_SECS)`: a 10 Hz edge (128 slots, 1 KiB) and 200 Hz edge (2 048 slots, 16 KiB) stay in L1; a 1 kHz IMU edge (16 384 slots, 128 KiB) sits at the far end and pays ~3x the search cost (43 ns against 14). `HISTORY_SECS` is a deployment note for `docs/RUNBOOK.md`, not an engine change.

### What this changes about the levers

- Probe count was never the lever; the stamp footprint the search touches is.
- **A compact stamp summary** (search a 1-in-16 summary array first, then finish within two cache lines) attacks the largest term. It needs a new arena region, so a `FORMAT_VERSION` break and its own decision record.
- The interp-policy dispatch is free, so resolving it at plan-compile time buys nothing.

---

## 13. The two O(depth) scans were not the residual

**Verdict: falsified.** `Plan::at` computed `has_dynamic` and `first_dynamic_edge` on every call; `fold_into` now derives both once (`dyn_count`, `first_dyn`). `bench_ab` over depths 1–6 showed noise. It is kept because a test pins one derivation against a fresh scan, and `dyn_count` is the dynamic-step count `docs/PHASE1.md` §11.3 needs a row to state.

`step_cost`'s **fold replica** (a harness copy of `fold_at`) shows about half the ~11.5 ns/step residual is the step-array walk and half is codegen context (inlining, register pressure) no harness rearrangement reproduces.

---

## 14. Lever 2 is falsified — there is no ILP to recover

**Verdict: rejected, without implementing it.** Its thesis (§4) is that *d* dependent chains overlap. `t_guard_sample` samples *d* edges with no accumulator and no `?` between them, the shape the "locate" phase would create. If chains overlap, per-sample cost falls as *d* rises.

| *d* | *d* different edges | **one ring, *d* stamps** |
|---|---|---|
| 1 | 56.80 (1.00×) | 55.56 (1.00×) |
| 2 | 63.06 (1.11×) | 62.40 (1.12×) |
| 3 | 59.60 (1.05×) | 63.82 (1.15×) |
| 4 | 58.29 (1.03×) | 61.19 (1.10×) |
| 6 | 57.24 (1.01×) | 60.56 (1.09×) |

Per-sample cost rises; the right column holds the footprint fixed as the control. The search is memory-bound, not latency-bound (§12): independent work multiplies the misses competing for the same L1.

### What survives

- **Lever 5 survives, and only that half.** Interpolation (22.2 ns, 31%) is arithmetic-bound, so a wider ALU can still attack it across batch elements.
- **Footprint, not parallelism, is the lever on the search**: a compact stamp summary (§12), a shorter `HISTORY_SECS`, anything that keeps the probed array in L1.

---

## 15. Shrinking `Step` — falsified

**Verdict: rejected, without implementing it.** `Step` is **128 bytes** (`Step::Static` carries an `Iso3`, `#[repr(C, align(64))]`). `Step` is a `Pod` type the C ABI and Python zero-copy buffers see, so the win was bounded first: `step_cost`'s **compact walk** drives the same fold from one `u32` per step. A 32x smaller walked array was worth nothing (depth 6: 409.1 vs 412.4 ns): a sequential scan is hidden by the prefetcher behind ~64 ns of sampling per step, whereas the search's stamp probes are random.

---

## 16. The scalar cursor — the first lever that returned anything

**Verdict: in.** ~9% off a depth-3 lookup, and it flattens the capacity cliff.

`sample_from`'s galloping cursor was already used by the batch path (`fold_at_cursors`, `at_many`/`at_adaptive`); scalar `Plan::at` restarted the bracket search at the window midpoint every call. The cursor makes access local, so probes land in a line the previous query pulled in.

| | capacity 4096 | capacity 16384 |
|---|---|---|
| `sample` (fresh search) | 54.58 | 58.54 |
| `sample_from` (cursor) | **40.71** (0.75×) | **41.37** (0.71×) |

`Plan::at` depth 3, pinned and idle: 228.6 → 207.0 ns (−9.4%).

### What shipped

`Guard` gained one packed `u64` per step, `(edge << 32) | index`. `Guard` is `!Sync` and built per batch on one thread, so a plain `Cell` is sound (same argument as `docs/PHASE5.md` §5.4's `ok` counter). The edge tag self-invalidates: one guard can evaluate several plans. `sample_from` is documented and tested to return exactly what `sample` returns; a stale, wrong or absent hint costs time, never accuracy.

### What it costs

`Guard::new` goes from 1.4 ns to 8.5 ns (128 bytes of cursor stores). A guard hoisted across a batch (§5.4) improves 116.8 → 107.5 ns; a guard rebuilt per lookup breaks even (122.3 → 123.3).

---

## 17. The cliff under a writer — the contention hypothesis is falsified

`contended_search`: one dynamic edge, one reader at a fixed lag behind the newest stamp, zero or one writer at the edge's nominal 1 kHz. It asserts that pushes landed, none were refused, and no query fell outside the retained window.

- **A writer costs the search essentially nothing** (0.89x–1.06x, noise): the reader out-issues the writer roughly 125 000 to one. The result depends on the rate ratio, not this host's caches.
- **The cursor's benefit survives contention** (1.8x–2.4x for a consumer polling near the newest stamp) and makes the search capacity-independent (5.34–5.52 ns across a 16x range), confirming §12.

Not covered: one writer on the **same edge** as the reader, and publish rates above 1 kHz. `docs/PHASE1.md` §11.2's four concurrent writers sit on four different edges; `read_scaling`'s `+writers` group covers them for throughput.
