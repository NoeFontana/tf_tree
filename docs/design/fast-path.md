# Design: the next-generation lookup hot path

**Status:** partly implemented, partly **falsified by measurement**. Levers 1, 1b and the scalar cursor (§16) are in; Levers 2 and 3 are rejected; 4 and 5 are open. §11 is the projected-versus-measured table; §12 is the per-term decomposition and ranks the bracket search first.
**Measured on:** AMD EPYC-Milan, 4 physical cores, 2445 MHz fixed, idle
**Reproduce:** `cargo run --release -p tf_tree_bench --example cost_model`

---

## 1. Measure first

Hypothesis: the bracket search (binary search over up to 16 384 samples, ~14 serially dependent loads) dominates. The first measurement said no; §12 refines that.

Capacity sweep, depth 3:

| capacity | log2 | ns/lookup | ns/step |
|---|---|---|---|
| 64 | 6 | 256.7 | 85.6 |
| 4 096 | 12 | 264.7 | 88.2 |
| 65 536 | 16 | 308.3 | 102.8 |

Depth sweep, capacity 4096:

| depth | ns/lookup | marginal |
|---|---|---|
| 1 | 91.5 | — |
| 3 | 263.5 | 85.4 |
| 6 | 516.7 | 80.8 |

Cost is linear: about 5 ns fixed + 86 ns per dynamic step. Exact-stamp hit (no interpolation) versus between-samples, depth 3, capacity 4096: 119.6 vs 269.9 ns, so interpolation is 50.1 ns per step.

Initial cost model per dynamic step (superseded by §12):

| Term | ns | Share |
|---|---|---|
| Interpolation math | ~50 | 58% |
| Bracket search (12 probes × 1.72) | ~21 | 24% |
| Slot reads, composition, bounds checks | ~15 | 18% |

`ScLerp` costs a further +15 ns/step over `LerpSlerp`.

---

## 2. Why interpolation cost 50 ns

`slerp` (`tf_tree_math/src/interp.rs`) did one `acos`, three `sin` and a divide per step, evaluated over a tiny arc: adjacent `/tf` samples at 1 kHz on a body turning 180 °/s differ by 3 mrad. A short series reproduces the result. Same shape as `docs/PHASE1.md` §3.3 (D12): a measured-threshold series beats the closed form in the regime the code runs in.

---

## 3. Lever 1 — transcendental-free slerp weights (in)

With `a` either `s` or `1 − s`, and `u = θ²`:

```
sin(aθ)/sin(θ) = a·[ 1 + (1−a²)·u/6
                       + (1−a²)(7−3a²)·u²/360
                       + (1−a²)(31−42a²+11a⁴)·u³/15120 + … ]
```

Horner in `u`: no transcendental, divide or branch. `u` comes from the chord, not `acos`: for unit quaternions after the sign fix, `h = |q1 − q0|²/2` from component differences (no cancellation), and `θ² = 2h·(1 + h/6 + 3h²/40 + …)`. That is faster and more accurate than `acos` near identity.

Threshold rules (`docs/PHASE1.md` §3.3):

1. Sweep θ on a log grid 1e-12 to π against the exact path and a high-precision reference.
2. Pick `THETA_SLERP_SMALL` where the error curves cross; record the table in the source.
3. Keep the exact path above the threshold; a 10 Hz edge on a fast-rotating body reaches it.
4. Reference/fast differential proptest (D13), 10⁵ pairs including near-identity and near-π.

The tf2 differential must be re-measured, not assumed: it should stay far below 1e-12; a material move means the series is wrong.

---

## 4. Lever 2 — restructure `fold_at` into locate / interpolate / compose (rejected, §14)

Proposal: (1) locate all *d* bracket pairs, mutually independent so the dependent load chains overlap; (2) interpolate all *d* lanes at once, SoA, by autovectorisation only (`tf_tree_math` is `#![forbid(unsafe_code)]`; `core::simd` is nightly); (3) compose serially. Scratch is a fixed `[_; MAX_DEPTH]` on the stack (invariant 8). If the asm does not vectorise, phase 2 is dropped rather than smuggled in via an unsafe island.

---

## 5. Lever 3 — interpolation-seeded bracket search — **FALSIFIED, not implemented**

Proposal: `guess = lo + (t − t_lo) · (hi − lo) / (t_hi − t_lo)`, then a fixed small number of branchless correction steps, falling back to binary search.

§10 made it conditional on measuring the seed-error distribution on real data (`cargo run --release -p tf_tree_bench --example search_seed`):

| Stream | Jitter (CV) | Seed error p50 | p99 | Within ±2 |
|---|---|---|---|---|
| Synthetic fixture (isochronous) | 0.00 | 0 | 0 | 100% |
| Recorded, `base_link->*_link` | 1.44 | **11** | 42 | 16% |
| Recorded, `odom_combined->base_footprint` | 2.85 | **48** | 90 | 8% |

The seed lands 11–48 indices off where a binary search over the same 203–254 samples needs ~8 probes in total. The median period is a clean 100.0 ms, but a real robot publishes intermittently: 29–44 gaps over 3x the median, covering 50–71% of the timeline. A linear index-vs-time seed assumes uniform density, and the two window endpoints do not carry the information to repair that. The fixture, where the seed is perfect, would never have caught it.

The intent survives in `sample_from`'s galloping cursor (§16), which exploits query-to-query locality and is robust to gaps.

`bracket` is not branchless: its mask-select compiles to a conditional branch, so its cost depends on the stamp distribution. See [`0053`](../decisions/0053-the-branchless-bracket-that-branches.md).

---

## 6. Lever 4 — skip the per-slot seqlock for all but the newest slot (open)

`push` only writes slot `head & mask`. A reader of logical index `i` with `head − i ≥ 2` reads a slot no writer touches until the ring laps, and the trailing `head − i > retained` revalidation detects lapping. The `head` Acquire load synchronises-with the writer's Release store, so the seqlock is redundant for querying the past. Saves about 5 ns/step.

**The most dangerous item here**: it resembles the "weakened an ordering because x86 passed" smell in `docs/PROJECT.md` §6. It does not land without:

- a loom test for reader-vs-writer at `head − 1` and `head − 2`,
- a loom test across a lapping writer,
- the existing wrapped-ring proptest passing,
- Miri clean.

If any is awkward, drop the lever.

---

## 7. Lever 5 — SIMD across stamps for `at_many` / `at_adaptive` (open)

Stamps in a batch are independent, so SIMD across stamps has no ordering constraint and no correctness risk.

---

## 8. Projected result

Per dynamic step, capacity 4096, `LerpSlerp` (projections to falsify, not results):

| Term | Now | After L1+L3+L4 | After L2 |
|---|---|---|---|
| Interpolation | 50 | 10 | ~4 |
| Search | 21 | 4 | 4 |
| Reads / compose / checks | 15 | 10 | 10 |
| **per step** | **86** | **~24** | **~18** |
| **depth-3 lookup** | **264** | **~77** | **~60** |

---

## 9. A finding this exposed: the gate may not be met

`docs/PHASE1.md` §11.3 requires depth-3 hot lookup p50 under 150 ns (ScLerp) and under 100 ns (LerpSlerp). Measured on a depth-3 chain of **three dynamic edges** at capacity 4096: 264 ns LerpSlerp, 312 ns ScLerp. The 94 ns figure for the recorded stream is also "depth 3" but mostly static edges that constant-fold away. The spec does not say which the gate means, so the answer differs by 2.8x by reading. A benchmark row should state the number of *dynamic* steps, not just the depth.

---

## 10. Order of work, and what would falsify each step

1. **Lever 1**, with the threshold sweep. Falsified if the series cannot reach 1e-15 below a threshold covering real inter-sample arcs, or the tf2 differential moves materially.
2. **Lever 3.** Falsified if real streams are jittery enough that the seeded guess misses often (measure on `indoor_atelier.tfstream`).
3. **Lever 5.** Lowest risk.
4. **Lever 2**, only after 1 and 3, and only if the asm shows vectorisation.
5. **Lever 4**, last, only with the loom coverage in §6.

Re-run `cost_model` after each step; §1's table is the regression baseline.

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

Lesson: two errors (§5's isochronous-stamps premise; the mask rewrite's "LLVM already emitted a `cmov`", refuted by disassembly in `0053`) were plausible mechanisms reasoned about instead of measured. The explanation for a cost needs its own measurement, separate from the measurement of the cost.

Depth-3, three dynamic steps, capacity 4096, pinned: 217 ns `LerpSlerp`, 296 ns `ScLerp`. Against §11.3's 100/150 ns gate this still fails by ~2x; §9's flag stands. §12 replaces the earlier per-step estimate.

| Term | ns/step | How it is known |
|---|---|---|
| Interpolation | ~27 | measured (`interp_cost`) |
| Bracket search | ~20 | measured (`cost_model` capacity sweep) |
| **Slot reads, composition, bounds checks** | **~25** | **a residual** |

---

## 12. The residual, measured — and the search is the largest term

**Reproduce:** `taskset -c 2 cargo run --release -p tf_tree_bench --example step_cost`
**Measured on:** AMD EPYC-Milan, 4 physical cores, L1d 32 KiB, L2 512 KiB, idle

`step_cost` measures each term directly and checks that they add up.

### The measured decomposition

Per dynamic step, capacity 4096, `LerpSlerp`, stamps swept across the window and landing between samples. Marginal from the depth sweep: **72.5 ns/step**.

| Term | ns/step | Share | How measured |
|---|---|---|---|
| **Bracket search** | **24.3** | **34%** | `sample(exact) − sample(Hold)` |
| **Interpolation** | **22.2** | **31%** | `sample(between) − sample(exact) − read_slot` |
| Fold overhead + the two O(depth) scans | ~11.5 | 16% | residual against the depth sweep |
| `Iso3` composition | 6.8 | 9% | direct, chained |
| `read_slot` ×2 | 6.6 | 9% | direct |
| `ArenaView::sampler` | 1.9 | 3% | direct (not re-run since `SampleRing` dropped its stored `mask`) |
| Ring preamble | 1.7 | 2% | `sample(Hold) − read_slot` |
| Interp-policy dispatch | ~0 | 0% | `guard_sample − sampler − sample(between)` |
| **sum** | **75.0** | | vs 72.5 measured, 3% closure |

`Plan::at` on an identity plan is 3.8 ns per call.

### The search is a cache cliff, not a probe curve

Capacity sweep at depth 1, with `Hold` (one pose slot, no search) as the control:

| capacity | stamps | poses | `sample(exact)` | `sample(Hold)` | Δ/log2 |
|---|---|---|---|---|---|
| 64 | 0.5 KiB | 4 KiB | 12.78 | 5.01 | — |
| 256 | 2 KiB | 16 KiB | 13.45 | 4.97 | 0.34 |
| 1 024 | 8 KiB | 64 KiB | 14.37 | 4.97 | 0.46 |
| 4 096 | **32 KiB** | 256 KiB | 32.53 | 4.97 | **9.08** |
| 16 384 | 128 KiB | 1 024 KiB | 43.31 | 5.03 | 5.39 |

1. `Hold` is flat within 1% across a 256x range of pose-array size, so shrinking `PoseSlot` would buy nothing.
2. The whole cliff is the stamp array: flat (0.3–0.5 ns per doubling) while the stamps fit L1, then ~9 ns per doubling at the capacity whose stamp array is 32 KiB, this host's L1d. Per-probe cost goes from ~1 ns to ~2.3–2.7 ns.

§1's 1.72 ns/probe holds only while the stamp array fits L1; it is a step function of `capacity × 8 bytes` against L1d.

Real edges are sized by `Capacity::history(rate_hz, HISTORY_SECS)` (the fixture keeps 10 s):

| fixture edge | rate | slots | stamp array | where that lands |
|---|---|---|---|---|
| `map → odom` | 10 Hz | 128 | 1 KiB | flat, L1 |
| `odom → base_link` | 50 Hz | 512 | 4 KiB | flat, L1 |
| `base_link → laser` | 200 Hz | 2 048 | 16 KiB | flat, L1 |
| `base_link → imu_link` | **1 kHz** | **16 384** | **128 KiB** | **far end of the cliff** |

A 1 kHz IMU edge pays ~3x the search cost of a 10 Hz edge (43 ns against 14). `HISTORY_SECS` sets that array's size; a deployment note for `docs/RUNBOOK.md`, not an engine change.

### What this changes about the levers

- Lever 3's rejection stands, for a better reason: probe count was never the lever (~1 ns per probe when resident); the stamp footprint the search touches is.
- **A compact stamp summary** (search a 1-in-16 summary array first, 2 KiB at capacity 4096, then finish within two cache lines) attacks the largest term. It needs a new arena region, so a `FORMAT_VERSION` break and its own decision record.
- Lever 2's case looked stronger here; §14 falsified it.
- The interp-policy dispatch is free, so resolving it at plan-compile time buys nothing.

### The residual that is left

~11.5 ns/step, flat per step across depths 1–6 (spread 2.95 ns), so per-step, not per-call. §13 records that the two O(depth) scans in `Plan::at` were not it.

---

## 13. The two O(depth) scans were not the residual

**Verdict: falsified.** `Plan::at` computed `has_dynamic` and `first_dynamic_edge` on every call, each an O(`len`) walk of `[Step; MAX_DEPTH]`. `fold_into` now derives both once (`dyn_count`, `first_dyn`). `bench_ab` over the depth sweep, pinned, idle:

| depth | before | after | verdict |
|---|---|---|---|
| 1 | 77.6 | 75.6 | noise |
| 2 | 155.1 | 155.4 | noise |
| 3 | 228.5 | 228.5 | noise |
| 4 | 299.2 | 298.6 | noise |
| 6 | 440.1 | 435.7 | noise |

An out-of-order core hides predictable, non-faulting scans behind memory-bound work. It is kept for code placement: one derivation a test can pin against a fresh scan, and `dyn_count` is the dynamic-step count `docs/PHASE1.md` §11.3 needs a row to state.

### So what is the residual?

`step_cost` has a **fold replica**: a harness copy of `fold_at` walking the same step array with the same `match`, `?` and primitives.

| depth | predicted | fold replica | measured | walk (replica − pred.) | context (meas. − replica) |
|---|---|---|---|---|---|
| 1 | 67.2 | 68.1 | 77.0 | 0.9 | 8.9 |
| 2 | 130.7 | 146.0 | 155.0 | 15.4 | 9.0 |
| 3 | 194.2 | 211.6 | 229.3 | 17.4 | 17.7 |
| 4 | 257.6 | 275.3 | 299.3 | 17.7 | 24.0 |
| 6 | 384.6 | 407.3 | 434.1 | 22.7 | 26.8 |

About half the residual is the step-array walk (a property of the loop); half is codegen context (inlining, register pressure) no harness rearrangement reproduces.

---

## 14. Lever 2 is falsified — there is no ILP to recover

**Verdict: rejected, without implementing it.** Its thesis (§4) is that *d* dependent chains run end to end instead of overlapping. That is testable without the rewrite: `t_guard_sample` samples *d* edges with no accumulator chaining them and no `?` between them, the shape the "locate" phase would create. If chains overlap, per-sample cost must fall as *d* rises.

| *d* | *d* different edges | **one ring, *d* stamps** |
|---|---|---|
| 1 | 56.80 (1.00×) | 55.56 (1.00×) |
| 2 | 63.06 (1.11×) | 62.40 (1.12×) |
| 3 | 59.60 (1.05×) | 63.82 (1.15×) |
| 4 | 58.29 (1.03×) | 61.19 (1.10×) |
| 6 | 57.24 (1.01×) | 60.56 (1.09×) |

Per-sample cost rises. The right column is the control: *d* different rings multiply the working set (six rings at capacity 4096 is 1.7 MiB against a 512 KiB L2), so one ring sampled at *d* stamps holds the footprint fixed. It is flat-to-worse too.

The search is memory-bound, not latency-bound (§12): independent work multiplies the misses competing for the same L1.

**The cheap test of a structural lever is usually available before the structure is built.**

### What survives

- **Lever 5 survives, and only that half.** §4's phase 2 and cross-*step* vectorisation die with Lever 2. Interpolation (22.2 ns, 31%) is arithmetic-bound, so a wider ALU can still attack it across batch elements.
- **Footprint, not parallelism, is the lever on the search**: a compact stamp summary (§12), a shorter `HISTORY_SECS`, anything that keeps the probed array in L1.
- **Huge pages stop being a side quest.** The arena asks for `MADV_HUGEPAGE` without anything checking whether the kernel granted it.

---

## 15. Shrinking `Step` — falsified

**Verdict: rejected, without implementing it.** `Step` is **128 bytes**: `Step::Static` carries an `Iso3` (`#[repr(C, align(64))]`), so `[Step; MAX_DEPTH]` was 2048 bytes at `MAX_DEPTH = 16` (4096 since `0034` moved it to 32). Nothing measured here moves with it: the fold is O(`len`), so a depth-6 fold walks 768 bytes regardless of the declared array size.

`Plan` is not an arena structure, so no `FORMAT_VERSION` break; and `Iso3`'s `align(64)` rationale is untrue, since the arena stores `[AtomicU64; 7]` and `[u64; 7]`, never a typed `Iso3`. But `Step` is a `Pod` type the C ABI and Python zero-copy buffers see, so the win was bounded first: `step_cost`'s **compact walk** drives the same fold from one `u32` per step (16 steps per cache line).

| depth | fold replica (2048 B array) | compact walk (64 B array) |
|---|---|---|
| 1 | 68.6 | 68.2 |
| 2 | 143.1 | 142.6 |
| 3 | 209.3 | 209.5 |
| 4 | 276.6 | 276.5 |
| 6 | 409.1 | 412.4 |

A 32x smaller walked array is worth nothing: a sequential scan is what the prefetcher hides behind ~64 ns of sampling per step, whereas the binary search's stamp probes are random and unprefetchable. Sequential footprint is free; random footprint is not.

---

## 16. The scalar cursor — the first lever that returned anything

**Verdict: in.** ~9% off a depth-3 lookup, and it flattens the capacity cliff.

`sample_from`'s galloping cursor was already used by the batch path (`fold_at_cursors`, `at_many`/`at_adaptive`); scalar `Plan::at` restarted the bracket search at the window midpoint every call. Given §12, the cursor changes the thing that costs: it makes access local, so probes land in a line the previous query pulled in.

Harness bound, monotone sweep:

| | capacity 4096 (32 KiB stamps) | capacity 16384 (128 KiB) |
|---|---|---|
| `sample` (fresh search) | 54.58 | 58.54 |
| `sample_from` (cursor) | **40.71** (0.75×) | **41.37** (0.71×) |

A fresh search costs +7% from 4096 to 16384; the cursor +1.6%, a second confirmation of §12's mechanism.

### What shipped

`Guard` gained one packed `u64` per step, `(edge << 32) | index`. `Guard` is `!Sync` and built per batch on one thread, so a plain `Cell` is sound (same argument as `docs/PHASE5.md` §5.4's `ok` counter). The edge tag self-invalidates: one guard can evaluate several plans. `sample_from` is documented and tested to return exactly what `sample` returns; a stale, wrong or absent hint costs time, never accuracy.

`Plan::at`, pinned and idle:

| depth | before | after | |
|---|---|---|---|
| 1 | 76.9 | 72.8 | −5.4% |
| 2 | 156.7 | 142.6 | −9.0% |
| 3 | **228.6** | **207.0** | **−9.4%** |
| 4 | 298.4 | 274.6 | −8.0% |
| 6 | 438.1 | 411.1 | −6.2% |

`bench_ab` labels these `noise` because its 10% tolerance targets regressions.

### What it costs

`Guard::new` goes from 1.4 ns to 8.5 ns (128 bytes of cursor stores).

| pattern | before | after | |
|---|---|---|---|
| guard hoisted across a batch (§5.4) | 116.8 | **107.5** | −8.0% |
| guard rebuilt per lookup | 122.3 | 123.3 | +0.8%, noise |

Batched consumers win; per-lookup consumers (the Python scalar path, per `guard_cost`'s header) break even. A 4-entry direct-mapped cursor array (`k % 4`, tag-checked) would cut the initialisation for shallow plans at the cost of collisions on deep ones.

---

## 17. The cliff under a writer — the contention hypothesis is falsified

§12 and §16 were measured quiescent. Worries: a writer might worsen the cliff (each push writes the stamp lines a reader probes), or make the cursor stop helping (a writer makes its line cold).

`contended_search`: one dynamic edge, one reader at a fixed lag behind the newest stamp, zero or one writer at the edge's nominal 1 kHz. It asserts that pushes landed, none were refused, and no query fell outside the retained window; all hold in every cell.

Median ns/query, pinned to two cores:

| capacity | stamps | fresh, quiet | fresh, +writer | writer cost | cursor, quiet | cursor, +writer | writer cost |
|---|---|---|---|---|---|---|---|
| 1 024 | 8 KiB | 9.84 | 9.82 | 1.00× | 5.40 | 5.53 | 1.02× |
| 4 096 | 32 KiB | 10.94 | 10.93 | 1.00× | 5.44 | 5.52 | 1.01× |
| 16 384 | 128 KiB | 12.60 | 12.15 | 0.96× | 5.47 | 5.56 | 1.02× |

The writer achieved 918–939 Hz, about 8 pushes per million reader queries.

- **A writer costs the search essentially nothing** (0.89x–1.06x, noise): the reader out-issues the writer roughly 125 000 to one, so lines are essentially never contended. The result depends on the rate ratio, not this host's caches.
- **The cursor's benefit survives contention** and is 1.8x–2.4x for a consumer polling near the newest stamp.
- **The cursor makes the search capacity-independent**: 5.34–5.52 ns across a 16x capacity range. Only a locality explanation predicts a flat line, confirming §12.

### What this does not cover

One writer on the **same edge** as the reader, the sharpest test of invalidation. `docs/PHASE1.md` §11.2 specifies four concurrent writers, which necessarily sit on four different edges, a weaker test of this mechanism that `read_scaling`'s `+writers` group covers for throughput. Higher publish rates are unmeasured.
