# Design: the next-generation lookup hot path

**Status:** partly implemented, partly **falsified by measurement**. Levers 1
and 1b are in; Lever 3 is rejected on real data. See §11 for what each lever
actually returned versus what this document projected, and **§12 for the
per-term measurement that replaces §11's residual row — it inverts the ranking:
the bracket search, not interpolation, is the largest term, and it is a cache
cliff rather than a probe curve.**
**Measured on:** AMD EPYC-Milan, 4 physical cores, 2445 MHz fixed, idle
**Reproduce:** `cargo run --release -p tf_tree_bench --example cost_model`

---

## 1. Measure first — and the measurement overturned the hypothesis

The obvious hypothesis is that the **bracket search** dominates: it is a binary
search over a ring of up to 16 384 samples, which is ~14 *serially dependent*
loads, and dependent cache accesses are usually what kills a latency budget.

That hypothesis is wrong, and it is worth showing why before proposing anything.

**Capacity sweep** — depth 3, ring capacity 64 → 65 536 (6 → 16 probes):

| capacity | log2 | ns/lookup | ns/step |
|---|---|---|---|
| 64 | 6 | 256.7 | 85.6 |
| 4 096 | 12 | 264.7 | 88.2 |
| 65 536 | 16 | 308.3 | 102.8 |

A 1024× larger ring — **ten extra dependent probes per step** — costs only
17.2 ns/step. That is **1.72 ns per probe**, and it puts total search cost at
capacity 4096 at roughly **21 ns of an 86 ns step**. The ring is small and hot;
the probes hit L1/L2, and the hardware overlaps them better than the dependency
chain suggests.

**Depth sweep** — capacity 4096:

| depth | ns/lookup | marginal |
|---|---|---|
| 1 | 91.5 | — |
| 3 | 263.5 | 85.4 |
| 6 | 516.7 | 80.8 |

Cost is almost exactly linear: **≈ 5 ns fixed + 86 ns per dynamic step.** So the
entire optimisation problem is "make one step cheaper"; there is no
super-linear term to attack.

**The decisive measurement** — exact-stamp hit (no interpolation performed)
versus a stamp that falls between samples, depth 3, capacity 4096:

| | ns/lookup |
|---|---|
| exact hit — bracket found, one slot read, no interp | 119.6 |
| interpolated — same search, two slot reads, interp | 269.9 |
| **difference** | **150.3 (50.1 ns per step)** |

### The resulting cost model, per dynamic step

| Term | ns | Share |
|---|---|---|
| **Interpolation math** | **~50** | **58%** |
| Bracket search (12 probes × 1.72) | ~21 | 24% |
| Slot reads, composition, bounds checks | ~15 | 18% |

`ScLerp` costs a further +15 ns/step over `LerpSlerp` (103.9 vs 88.7).
Cache locality is a minor term: sweeping the whole window versus pinning one
stamp differs by only 19.6 ns of 281 at capacity 16 384.

**Interpolation is the target.** Everything below follows from that.

---

## 2. Why interpolation costs 50 ns

`slerp` (`tf_tree_math/src/interp.rs`) is, per step:

```rust
let angle    = libm::acos(dot);            // ~20-25 ns
let sin_angle = libm::sin(angle);          // ~15-20 ns
let wa = libm::sin((1.0 - s) * angle) / sin_angle;   // ~15-20 ns + div
let wb = libm::sin(s * angle) / sin_angle;           // reuses sin_angle
```

One `acos`, three `sin`, one divide. That is the 50 ns, and it is spent on
**transcendentals evaluated over a tiny arc**.

That last point is the whole opportunity. In a real `/tf` stream the two
bracketing samples are *adjacent in time*. At 1 kHz, a body rotating at a brisk
180 °/s moves **3 milliradians** between samples. Even a 10 Hz edge at that rate
moves 0.3 rad. `slerp` exists to give constant angular velocity across a
**large** arc; across 3 mrad it is doing full-precision transcendental work to
compute something a short polynomial reproduces exactly.

This is the same shape of finding as `docs/PHASE1.md` §3.3 (D12): the closed form
is the wrong tool in the regime the code actually runs in, and the fix is a
measured-threshold series.

---

## 3. Lever 1 — transcendental-free slerp weights (target: 50 ns → ~10 ns/step)

### The series

With `a` standing for either `s` or `1 − s`, and `u = θ²`:

```
sin(aθ)/sin(θ) = a·[ 1 + (1−a²)·u/6
                       + (1−a²)(7−3a²)·u²/360
                       + (1−a²)(31−42a²+11a⁴)·u³/15120 + … ]
```

Every term is multiply–add. Evaluated by Horner in `u`, both weights cost a
handful of FMAs and **no transcendental, no divide, no branch**.

### Getting `u = θ²` without `acos`, and without cancellation

The naive route is `θ = acos(dot)`, which is exactly what we are trying to
delete — and it is also *numerically worst* precisely where we operate: for
`dot → 1`, `acos` loses half the significant digits, and `1 − dot` cancels
catastrophically.

Use the chord instead. For unit quaternions after the sign fix:

```
|q1 − q0|² = 2 − 2·dot = 2h        so   h = |q1 − q0|²/2
```

computed **from the component differences**, which involves no cancellation at
all. Then

```
θ² = 2h·(1 + h/6 + 3h²/40 + …)
```

So the fast path never forms `1 − dot` and never calls `acos`. It is both
faster *and* more accurate than the current code in the regime that matters —
which is the good kind of optimisation.

### Threshold and validation — non-negotiable

`docs/PHASE1.md` §3.3 is explicit that a threshold must be *measured*, not
guessed, and that three terms versus four changes the answer. The same applies
here:

1. Sweep θ on a log grid from 1e-12 to π, comparing the series against the
   existing `acos`/`sin` path and against a high-precision reference.
2. Pick `THETA_SLERP_SMALL` from where the two error curves cross, exactly as
   `THETA_SMALL = 0.1` was chosen — and record the table in the source.
3. Keep the exact path as the fallback above the threshold. It is not dead code;
   a 10 Hz edge on a fast-rotating body will reach it.
4. Add the reference/fast differential proptest (D13), 10⁵ pairs including
   near-identity and near-π.

**Risk to watch:** the tf2 differential currently reads 2.876e-15 (fixture) and
6.665e-15 (recorded). A more accurate slerp will *move* those numbers, because
tf2 keeps using `acos`. They should stay far below 1e-12 — but this must be
**re-measured, not assumed**, and if the number moves materially the series is
wrong somewhere.

---

## 4. Lever 2 — restructure `fold_at` into locate / interpolate / compose

Today the fold is:

```
for step in steps:  acc = acc ⊗ sample(step, t)
```

which serialises three things that need not be serial: each `sample()` is a
dependent load chain, and `acc` carries a dependency across every step.

Restructure into three phases over the plan's fixed `[Step; MAX_DEPTH]`:

1. **Locate** — for all *d* dynamic steps, find the bracket index pair. These are
   **mutually independent**, so the *d* dependent load chains overlap in the
   out-of-order window instead of running end to end. Pure ILP; no SIMD needed
   and no unsafe.
2. **Interpolate** — with Lever 1 the interpolation is a pure FMA polynomial, so
   *d* quaternion slerps are 4 (AVX2) or 8 (AVX-512) lanes wide. Lay the *d*
   bracket pairs out SoA and interpolate every lane at once.
3. **Compose** — *d−1* quaternion multiplies. Inherently serial, but that is
   ~5 ns each and it is the only genuinely serial part.

Scratch is a fixed `[_; MAX_DEPTH]` on the stack — invariant 8 (no allocation
after construction) is preserved.

**The unsafe-budget constraint is real here.** `tf_tree_math` is
`#![forbid(unsafe_code)]`, and explicit SIMD intrinsics are `unsafe`.
`core::simd` is nightly. So phase 2 must rely on **autovectorisation**, which
means writing the interpolation over `[f64; 4]` in a shape LLVM will actually
vectorise, and then *verifying it did* by reading the asm and re-running the
cost model — not by hoping. If it does not vectorise, phase 1's ILP win stands
on its own and phase 2 is dropped rather than smuggled in via an unsafe island.

---

## 5. Lever 3 — interpolation-seeded bracket search — **FALSIFIED, not implemented**

> **Rejected.** §10 made this lever conditional on measuring the correction-step
> distribution on real recorded data first. That measurement
> (`cargo run --release -p tf_tree_bench --example search_seed`) rejects it:
>
> | Stream | Jitter (CV) | Seed error p50 | p99 | Within ±2 |
> |---|---|---|---|---|
> | Synthetic fixture (isochronous) | 0.00 | 0 | 0 | 100% |
> | Recorded, `base_link->*_link` | 1.44 | **11** | 42 | 16% |
> | Recorded, `odom_combined->base_footprint` | 2.85 | **48** | 90 | 8% |
>
> The seed lands a median of 11–48 indices from the answer, where a binary
> search over the same 203–254 samples needs ~8 probes *in total*. The seeded
> search would be slower, not faster.
>
> **The reason is not jitter in the usual sense.** The recording's median period
> is a clean 100.0 ms — the nominal rate, dead on. What breaks the seed is that
> a real robot publishes *intermittently*: 29–44 gaps longer than 3x the median,
> together covering **50–71% of the timeline**. Excluding gaps the CV drops from
> 1.44 to 0.43. A linear index-vs-time seed assumes uniform sample density, and
> that is precisely what a gap destroys — which no amount of tuning the seed
> repairs, because the information simply is not in the two window endpoints.
>
> This is exactly the falsification §10 asked for, and it is why the harness
> exists. Had this been implemented on the strength of the synthetic fixture —
> where the seed is *perfect*, 0 error at every rate — it would have shipped a
> regression on real workloads and the fixture would never have caught it.
>
> The intent survives elsewhere: `sample_from`'s galloping cursor already
> exploits query-to-query locality, which is the form of seeding that *is* robust
> to gaps, and `bracket` is now branchless so its cost no longer depends on the
> stamp distribution at all.

### The original proposal, retained for the record

Stamps on a real edge are near-isochronous, so the index is *predictable*:

```
guess = lo + (t − t_lo) · (hi − lo) / (t_hi − t_lo)
```

followed by a **fixed, small number of branchless correction steps** (`cmov`,
no data-dependent branches), falling back to the existing binary search when the
correction budget is exhausted — which is what jitter, gaps, and bursty
publishers will do.

The direct win is modest (~17 ns/step). The *structural* win is larger: it
collapses a log-depth serial dependent load chain into ~2 loads, which is what
lets Lever 2's phase 1 actually overlap across steps.

---

## 6. Lever 4 — skip the per-slot seqlock for all but the newest slot

**The claim:** `push` only ever writes slot `head & mask`. A reader reading
logical index `i` with `head − i ≥ 2` is reading a slot **no writer will touch**
until the ring laps — and the trailing `head − i > retained` revalidation
already detects lapping.

The `head` Acquire load at entry synchronises-with the writer's `head` Release
store, so all seven pose words of every sample older than `head − 1` are already
visible. The per-slot seqlock is therefore **redundant for exactly the case that
dominates**: querying the past.

Saves 2 loads + 2 fences per endpoint, 4 per step — perhaps 5 ns/step.

**This is the single most dangerous item in this document.** It looks exactly
like the "weakened an ordering because x86 passed" smell that `docs/PROJECT.md`
§6 lists. The argument is different in kind — it is about *which slots a writer
can touch*, not about how strong an ordering needs to be — but that distinction
is worth precisely nothing without proof. It does not land without:

- a loom test for reader-vs-writer at `head − 1` and `head − 2`,
- a loom test across a lapping writer,
- the existing wrapped-ring proptest still passing,
- Miri clean.

If any of those is awkward to write, **drop this lever.** It is 5 ns.

---

## 7. Lever 5 — SIMD across stamps for `at_many` / `at_adaptive`

For batch queries the natural SIMD axis is the **stamp**, and stamps are
independent. 8 stamps × depth *d* is embarrassingly parallel and should run at
close to memory bandwidth rather than latency. This is the easiest large win and
the one with no correctness risk, because a batch already has no ordering
constraint between its elements.

---

## 8. Projected result

Per dynamic step, capacity 4096, `LerpSlerp`:

| Term | Now | After L1+L3+L4 | After L2 |
|---|---|---|---|
| Interpolation | 50 | 10 | ~4 (amortised across lanes) |
| Search | 21 | 4 | 4 (overlapped) |
| Reads / compose / checks | 15 | 10 | 10 |
| **per step** | **86** | **~24** | **~18** |
| **depth-3 lookup** | **264** | **~77** | **~60** |

Roughly **3.4× on the levers that carry no correctness risk**, and ~4.4× with
the SIMD restructure — *if* autovectorisation cooperates.

Every number in the right-hand columns is a projection from the measured left
column. They are targets to falsify, not results.

---

## 9. A finding this exposed: the gate may not be met

`docs/PHASE1.md` §11.3 requires **depth-3 hot lookup p50 under 150 ns (ScLerp)
and under 100 ns (LerpSlerp)**.

Measured here on a depth-3 chain of **three dynamic edges** at capacity 4096:
**264 ns LerpSlerp, 312 ns ScLerp** — roughly 2.6× and 2.1× over the gate.

The 94 ns figure previously reported for the recorded stream is *also* "depth 3",
but most of its chain is static edges that constant-fold away, so it is a much
cheaper query. Which of the two the gate means is not stated.

I am not claiming the gate fails — it depends on a definition the spec does not
give. I am flagging that **the answer differs by 2.8× depending on the reading**,
which makes it worth pinning down before anyone reports a pass. The honest
version of the benchmark row should state the number of *dynamic* steps, not
just the depth.

---

## 10. Order of work, and what would falsify each step

1. **Lever 1**, with the §3.3-style threshold sweep. Falsified if the series
   cannot hit 1e-15 below a threshold that covers real inter-sample arcs, or if
   the tf2 differential moves materially.
2. **Lever 3.** Falsified if real recorded streams are jittery enough that the
   seeded guess misses often — measure the correction-step distribution on
   `indoor_atelier.tfstream` before committing.
3. **Lever 5** (`at_many`). Lowest risk, independent of the others.
4. **Lever 2**, only after 1 and 3, and only if the asm shows vectorisation.
5. **Lever 4**, last, and only with the loom coverage above. Drop it otherwise.

Re-run `cargo run --release -p tf_tree_bench --example cost_model` after each
step; the table in §1 is the regression baseline.

---

## 11. Results — what each lever actually returned

Every projection in §8 was labelled "a target to falsify, not a result". This is
the falsification. Three of the five held up; one was rejected outright; one
returned a tenth of its estimate.

| Lever | Projected | Measured | Verdict |
|---|---|---|---|
| **1** — transcendental-free `slerp` | 50 → ~10 ns/step | 50.1 → 27.0 ns/step; depth-3 263.5 → 197.7 ns | **In.** Half the projected gain |
| **1b** — transcendental-free `screw_pow` | *not proposed* | ScLerp 51.6 → 43.6 ns/eval (−15%) | **In.** Not in this document at all |
| **3** — interpolation-seeded search | 21 → ~4 ns/step | seed misses by 11–48 indices on real data | **Rejected** (§5) |
| **3b** — branchless `bracket` | *not proposed* | −1.2% instructions; 237.0 → 231.2 ns @ cap 16384 | **In.** A tenth of the expected gain |
| **2**, **4**, **5** | — | not attempted | Open |

### What this document got wrong, and the pattern in it

**§1's cost model was right; §5's premise was not.** The measurement-first
discipline correctly identified interpolation as the dominant term and correctly
ranked search second. Where it went wrong was in reasoning about *why* the
second term costs what it does — twice, in the same direction:

* §5 assumed real `/tf` stamps are near-isochronous, so an interpolated index
  guess would land close. Real stamps have a clean 100.0 ms median period and
  **gaps covering 50–71% of the timeline**. The nominal rate was right and the
  inference from it was wrong.
* The branchless rewrite assumed the search's data-dependent `if` was costing
  most of the fixture's 8.16 mispredicts per lookup. LLVM had already turned it
  into a `cmov`, so there was almost nothing there to win.

Both errors share a shape: a plausible mechanism was reasoned about instead of
measured, in a document whose own §1 opens by warning against exactly that. The
lesson is not "measure the bottom line" — §1 did — but that **the explanation for
a cost needs its own measurement**, separate from the measurement of the cost.

Lever 1b exists for the same reason and points the other way: it was found by
noticing that `ScLerp` is the *default* policy and had never been profiled
separately, not by following this document's plan.

### Where the remaining time goes

Depth-3, three dynamic steps, capacity 4096, pinned: **217 ns `LerpSlerp`**,
296 ns `ScLerp`. Against §11.3's 100 ns / 150 ns gate this still fails by ~2x,
and §9's flag stands. The per-step budget was estimated as:

| Term | ns/step | Note |
|---|---|---|
| Interpolation | ~27 | was 50; Lever 1 |
| Bracket search | ~20 | latency-bound on a serial dependent-load chain |
| Slot reads, composition, bounds checks | ~25 | a **residual**, not a measurement |

**§12 replaces this table with a measured one, and the ranking inverts.**

---

## 12. The residual, measured — and the search is the largest term

**Reproduce:** `taskset -c 2 cargo run --release -p tf_tree_bench --example step_cost`
**Measured on:** AMD EPYC-Milan, 4 physical cores, L1d 32 KiB, L2 512 KiB, idle

§11's third row was a subtraction. That is the very thing §11's own post-mortem
says not to do — *"the explanation for a cost needs its own measurement"* — so
`step_cost` measures each term directly and then **checks that they add up**.

### The measured decomposition

Per dynamic step, capacity 4096, `LerpSlerp`, stamps swept across the window and
landing between samples. Measured marginal from the depth sweep: **72.5 ns/step**.

| Term | ns/step | Share | How measured |
|---|---|---|---|
| **Bracket search** | **24.3** | **34%** | `sample(exact) − sample(Hold)` |
| **Interpolation** | **22.2** | **31%** | `sample(between) − sample(exact) − read_slot` |
| Fold overhead + the two O(depth) scans | ~11.5 | 16% | residual against the depth sweep |
| `Iso3` composition | 6.8 | 9% | direct, chained |
| `read_slot` ×2 | 6.6 | 9% | direct |
| `ArenaView::sampler` | 1.9 | 3% | direct |
| Ring preamble | 1.7 | 2% | `sample(Hold) − read_slot` |
| Interp-policy dispatch | ~0 | 0% | `guard_sample − sampler − sample(between)` |
| **sum** | **75.0** | | vs 72.5 measured — **3% closure** |

Per call, once: `Plan::at` on an identity plan is **3.8 ns**.

**Two of §11's three rows were wrong in the same direction.** Search is not 20 ns
and interpolation is not 27; search is the *larger* of the two, and the residual
row was ~25 when the genuinely unexplained part is ~11.5. Lever 1 over-delivered
on interpolation relative to what §11 credited it, and the search was
under-counted.

### The search is a cache cliff, not a probe curve

The reason the search costs what it does is not the probe count. Sweeping ring
capacity at depth 1, with `Hold` — which reads one pose slot and runs **no
search** — as the control:

| capacity | stamps | poses | `sample(exact)` | `sample(Hold)` | Δ/log2 |
|---|---|---|---|---|---|
| 64 | 0.5 KiB | 4 KiB | 12.78 | 5.01 | — |
| 256 | 2 KiB | 16 KiB | 13.45 | 4.97 | 0.34 |
| 1 024 | 8 KiB | 64 KiB | 14.37 | 4.97 | 0.46 |
| 4 096 | **32 KiB** | 256 KiB | 32.53 | 4.97 | **9.08** |
| 16 384 | 128 KiB | 1 024 KiB | 43.31 | 5.03 | 5.39 |

Two things fall out, and the control is what makes them conclusive:

1. **`Hold` is flat to within 1% across a 256× range of pose-array size.**
   Reading one pose slot costs ~5.0 ns whether the pose array is 4 KiB or 1 MiB.
   The pose array's size costs nothing, so shrinking `PoseSlot` would buy nothing
   here.
2. **The whole cliff is the stamp array.** Cost is flat — 0.3–0.5 ns per doubling
   — while the stamps fit comfortably in L1, then steps by ~9 ns per doubling at
   exactly the capacity whose stamp array is **32 KiB, this host's L1d size**.
   Effective per-probe cost goes from ~1 ns (L1-resident) to ~2.3–2.7 ns (not).

§1's model — *"the ring is small and hot; the probes hit L1/L2"* at **1.72 ns per
probe** — holds only while the stamp array fits L1. It was measured at depth 3,
where three rings share the cache, and generalised into a per-probe constant. It
is not a constant; it is a step function of `capacity × 8 bytes` against L1d.

**And the reference capacity understates it.** Real edges are not sized in slots;
they are sized by `Capacity::history(rate_hz, HISTORY_SECS)`, and the benchmark
fixture keeps 10 s. So the capacity an edge gets is a function of how fast it
publishes:

| fixture edge | rate | slots | stamp array | where that lands |
|---|---|---|---|---|
| `map → odom` | 10 Hz | 128 | 1 KiB | flat, L1 |
| `odom → base_link` | 50 Hz | 512 | 4 KiB | flat, L1 |
| `base_link → laser` | 200 Hz | 2 048 | 16 KiB | flat, L1 |
| `base_link → imu_link` | **1 kHz** | **16 384** | **128 KiB** | **the far end of the cliff** |

A 1 kHz IMU edge pays roughly **3× the search cost** of a 10 Hz map edge — 43 ns
against 14 — for no reason other than that its stamp array does not fit L1. That
is the realistic case, not the pathological one, and it is why the reference
table above at capacity 4096 reads as the middle of the curve rather than the
end of it.

It also names a knob nobody has been told about: `HISTORY_SECS` is what sets
that array's size, and a consumer that only ever queries the last second is
paying full search cost for ten. That is a deployment note for
`docs/RUNBOOK.md`, not an engine change.

### What this changes about the levers

- **Lever 3's rejection stands, and for a better reason than §5 gave.** §5 killed
  interpolation-seeding because the seed lands 11–48 indices off on real data.
  The stronger reason is that reducing the *probe count* was never the lever:
  probes are ~1 ns each when the stamps are resident. The lever is reducing the
  *stamp footprint the search touches*.
- **A compact stamp summary is a new lever this measurement suggests**, and it is
  not in §1–§7: search a 1-in-16 summary array first (256 entries = 2 KiB at
  capacity 4096, permanently L1-resident), then finish within a range that spans
  two cache lines. It needs a new arena region, so it is a `FORMAT_VERSION` break
  and needs its own decision record — but it attacks the largest measured term,
  which nothing currently proposed does.
- **Lever 2's restructure is still worth doing** and its case is *stronger*: with
  the search at 34% and latency-bound on a serial dependent-load chain, the
  overlap it buys applies to the largest term rather than the second.
- **The interp-policy dispatch is free** (0 ns within noise), so resolving it at
  plan-compile time would buy nothing. Measured so nobody spends a day on it.

### The residual that is left

~11.5 ns/step, roughly flat per step across depths 1–6 (spread 2.95 ns). It is
per-*step*, not per-call, so the identity-plan floor cannot contain it. The two
O(depth) scans in `Plan::at` — `check_domain` → `has_dynamic`, and
`first_dynamic_edge` — have exactly that shape, and both are pure functions of
the compiled plan. §13 records what removing them actually returned.

---

## 13. The two O(depth) scans were not the residual

**Verdict: falsified.** Removing both scans moved nothing.

`Plan::at` computed `has_dynamic` and `first_dynamic_edge` on every call, each an
O(`len`) walk of a `[Step; MAX_DEPTH]` array — **2048 bytes** at the
`MAX_DEPTH = 16` these numbers were taken at, 4096 since `0034`; see §15 — 28 steps of scanning on a
depth-14 lookup, before folding anything. `first_dynamic_edge`'s own doc comment
called it "an O(plan length) scan … loop-invariant" and hoisted it for the batch
path while the scalar path kept paying it. Both are functions of the compiled
steps, so `Plan::new` now derives them once (`dyn_count`, `first_dyn`).

`bench_ab` over the depth sweep, pinned, idle host:

| depth | before | after | verdict |
|---|---|---|---|
| 1 | 77.6 | 75.6 | noise |
| 2 | 155.1 | 155.4 | noise |
| 3 | 228.5 | 228.5 | noise |
| 4 | 299.2 | 298.6 | noise |
| 6 | 440.1 | 435.7 | noise |

**Every row is noise.** The reasoning was the same shape §11 diagnoses — a
plausible mechanism (28 iterations! 2 KiB!) reasoned about instead of measured.
An out-of-order core hides a pair of predictable, non-faulting scans completely
behind memory-bound work that is already in flight.

**It is kept anyway, and the reason is not performance.** It is the same amount
of code, it puts the derivation in one place where a test can pin it against a
fresh scan, and `dyn_count` is the *dynamic-step count* that `docs/PHASE1.md`
§11.3 needs a row to state. Recorded here so nobody re-derives the expectation.

### So what *is* the residual?

Measured rather than guessed this time. `step_cost` gained a **fold replica**: a
harness-side copy of `fold_at` that walks the same step array through the same
`match` and the same `?`, calling the same primitives. Where it lands separates
the two candidates.

| depth | predicted | fold replica | measured | walk (replica − pred.) | context (meas. − replica) |
|---|---|---|---|---|---|
| 1 | 67.2 | 68.1 | 77.0 | 0.9 | 8.9 |
| 2 | 130.7 | 146.0 | 155.0 | 15.4 | 9.0 |
| 3 | 194.2 | 211.6 | 229.3 | 17.4 | 17.7 |
| 4 | 257.6 | 275.3 | 299.3 | 17.7 | 24.0 |
| 6 | 384.6 | 407.3 | 434.1 | 22.7 | 26.8 |

Roughly **half the residual is the step-array walk itself** — reproducible in the
harness, so it is a property of the loop and not of `tf_tree_core` — and **half is
codegen context** that no rearrangement of the harness reproduces: inlining
decisions and register pressure inside the real fold.

That split looked like it made Lever 2 the right next move. §14 measured whether
Lever 2 has anything to win before writing it, and the answer is no.

---

## 14. Lever 2 is falsified — there is no ILP to recover

**Verdict: rejected, without implementing it.** §11 called Lever 2 *"the one
remaining lever with a plausible 2x in it"*. It has none.

Lever 2's entire thesis (§4) is that the fold serialises work that need not be
serial: each `sample()` is its own dependent load chain, and `acc` carries a
dependency across every step, so the *d* chains run end to end instead of
overlapping in the out-of-order window. Restructuring into locate / interpolate /
compose would let them overlap.

That thesis is testable **without the rewrite**. `t_guard_sample` already samples
*d* edges with no accumulator chaining them and no `?` between them — precisely
the shape the "locate" phase would create. If the chains overlap when nothing
stops them, per-sample cost must fall as *d* rises.

| *d* | *d* different edges | **one ring, *d* stamps** |
|---|---|---|
| 1 | 56.80 (1.00×) | 55.56 (1.00×) |
| 2 | 63.06 (1.11×) | 62.40 (1.12×) |
| 3 | 59.60 (1.05×) | 63.82 (1.15×) |
| 4 | 58.29 (1.03×) | 61.19 (1.10×) |
| 6 | 57.24 (1.01×) | 60.56 (1.09×) |

**Per-sample cost does not fall. It rises.** The right-hand column is the control
that makes this conclusive: a depth-*d* chain has *d* different rings, so raising
*d* also multiplies the working set — six rings at capacity 4096 is 1.7 MiB
against one ring's 288 KiB, past this host's 512 KiB L2 — and a real win could
have been hidden inside that. Repeating the measurement against **one** ring
sampled at *d* different stamps holds the footprint fixed and the chains just as
independent. It is flat-to-worse there too.

### Why, and what it means for everything else

Because §12 already established what the dominant term is: the search is
**memory-bound**, not latency-bound. Adding independent work to a computation
that is waiting on cache does not hide the wait — it multiplies the misses
competing for the same L1. Six searches in flight over a 32 KiB stamp array
evict each other.

This is the third time this document has recorded the same failure shape, and it
is worth naming as a rule rather than an anecdote:

> §5 assumed real stamps were isochronous. The branchless rewrite assumed the
> search's `if` was mispredicting. Lever 2 assumed *d* dependent chains would
> overlap. Each was a plausible mechanism, reasoned about rather than measured,
> and each was wrong. **The cheap test of a structural lever is usually available
> before the structure is built** — here it was two harness loops against code
> that already existed.

### What survives

- **SIMD across *stamps* (Lever 5) survives, and only that half.** §4's phase 2
  and any cross-*step* vectorisation die with Lever 2 — the steps do not overlap,
  so there is nothing to widen. Batch elements are a different axis, and
  interpolation (22.2 ns, 31%) is genuinely arithmetic-bound rather than
  memory-bound, so it is the one term a wider ALU can still attack.
- **Footprint, not parallelism, is the lever on the search.** A compact stamp
  summary (§12), a shorter `HISTORY_SECS`, or anything else that keeps the probed
  array in L1. Not more chains in flight.
- **Huge pages stop being a side quest.** If the hot path is memory-bound at the
  cache level, its behaviour at the *TLB* level is no longer a footnote — and the
  arena asks for `MADV_HUGEPAGE` without anything checking whether the kernel
  granted it.

---

## 15. Shrinking `Step` — falsified, and it cost one harness function

**Verdict: rejected, without implementing it.** §13 split the residual and found
half of it reproducible by a *fold replica* that walks the plan's step array.
That looked like a lever, and a measurement of the array's real size made it look
like a big one:

`Step` is **128 bytes**, not the 64 it appears to be. `Step::Static` carries an
`Iso3`, and `Iso3` is `#[repr(C, align(64))]` with an explicit 8-byte pad — so
the enum discriminant rounds the whole variant to two cache lines and
`[Step; MAX_DEPTH]` is **2048 bytes**. A depth-6 fold walks 768 bytes, twelve
cache lines, to read six discriminants and six edge ids. (§13 and `plan.rs` both
said "1 KiB"; both understated it by 2× and are corrected.)

> **`MAX_DEPTH` was 16 when every number on this page was taken; `0034` moved it
> to 32, so the array is 4096 bytes now.** Nothing measured here moves with it:
> the fold is O(`len`) and the "depth-6 fold walks 768 bytes" figure is a
> function of the plan's real length, not of the array's declared size. The
> array size appears in this document only as the thing §13's rejected lever
> wanted to shrink. `size_of::<Step>()` is still 128.

Better still, the fix looked unconstrained: `Plan` is a value type, **not an
arena structure**, so there is no `FORMAT_VERSION` to break. And `Iso3`'s
`align(64)` justifies itself as *"so the Phase 2 shared-memory arena can store
slots without re-deriving layout"* — which is not true: the arena stores
`[AtomicU64; 7]` in `PoseSlot` and `[u64; 7]` in `EdgeRecord::static_pose`. It
never stores a typed `Iso3` at all.

So: bound the win before changing a `Pod` type that the C ABI and the Python
zero-copy buffers both see. `step_cost` gained a **compact walk** — the same
fold, the same samples, the same compose order, driven from one `u32` per step
(tag + inverted + edge or static index), so sixteen steps fit in a single cache
line instead of thirty-two.

| depth | fold replica (2048 B array) | compact walk (64 B array) |
|---|---|---|
| 1 | 68.6 | 68.2 |
| 2 | 143.1 | 142.6 |
| 3 | 209.3 | 209.5 |
| 4 | 276.6 | 276.5 |
| 6 | 409.1 | 412.4 |

**A 32× smaller walked array is worth nothing.** At depth 6 the compact form is
marginally *slower*.

The reason is the same one §14 turns on, seen from the other side: the step array
is walked **sequentially**, and a linear scan of a few hundred bytes is exactly
what a hardware prefetcher exists for — those loads are already in flight behind
the ~64 ns of sampling work per step. The stamp array is **randomly probed** by a
binary search whose next address depends on the last comparison, and no
prefetcher can help with that.

> Sequential footprint is free. Random footprint is not. Four of this
> document's levers died because they attacked code shape or code size; the one
> that worked (§16) attacked the *access pattern*.

---

## 16. The scalar cursor — the first lever that returned anything

**Verdict: in.** ~9% off a depth-3 lookup, and it flattens the capacity cliff.

`sample_from`'s galloping cursor already existed and was already wired into the
**batch** path (`fold_at_cursors`, used by `at_many`/`at_adaptive`). Scalar
`Plan::at` did not use it: every call restarted the bracket search at the window
midpoint, discarding everything the previous call learned.

Given §12 — the search is 34% of a step, and its cost is *whether the probed
stamp array fits L1* — a cursor is the one lever that changes the thing that
actually costs: it does not shrink the array, it makes the access **local**, so
probes land in a line the previous query already pulled in.

Bounded in the harness first, on a monotone sweep, before touching the engine:

| | capacity 4096 (32 KiB stamps) | capacity 16384 (128 KiB — a 1 kHz edge, 10 s) |
|---|---|---|
| `sample` (fresh search) | 54.58 | 58.54 |
| `sample_from` (cursor) | **40.71** (0.75×) | **41.37** (0.71×) |

It also **nearly flattens the cliff**: a fresh search costs +7% going 4096 →
16384; the cursor costs +1.6%. Exactly what cache locality predicts, and a second
independent confirmation of §12's mechanism.

### What shipped

`Guard` gained one packed `u64` per step — `(edge << 32) | index`. `Guard` is
`!Sync` by construction and built per batch on one thread, so a plain `Cell` is
sound; that is the same argument `docs/PHASE5.md` §5.4 already makes for the
`ok` counter. The edge tag is self-invalidation: one guard can evaluate several
plans, and step `k` of one is a different edge from step `k` of another.

**Nothing here can change a result.** `sample_from` is documented and tested to
return exactly what `sample` returns for the same `t`; only the search path
differs. A stale, wrong or absent hint costs time and never accuracy — which is
what makes a cache that nothing invalidates safe.

`Plan::at`, end to end, pinned and idle:

| depth | before | after | |
|---|---|---|---|
| 1 | 76.9 | 72.8 | −5.4% |
| 2 | 156.7 | 142.6 | −9.0% |
| 3 | **228.6** | **207.0** | **−9.4%** |
| 4 | 298.4 | 274.6 | −8.0% |
| 6 | 438.1 | 411.1 | −6.2% |

Consistent across three settled runs. `bench_ab` labels these `noise` because its
tolerance is 10%, set to catch *regressions*; a real 6–9% win sits under it, and
that is a property of the gate rather than of the change.

### What it costs, stated rather than buried

`Guard::new` goes from **1.4 ns to 8.5 ns** — the cursor array must be
initialised, and 128 bytes of stores is what that costs. Packing two arrays into
one and switching to an inline-`const` initialiser were both tried and neither
moved it.

So the trade depends entirely on how a consumer holds its guard:

| pattern | before | after | |
|---|---|---|---|
| guard hoisted across a batch (the intended shape, §5.4) | 116.8 | **107.5** | −8.0% |
| guard rebuilt per lookup | 122.3 | 123.3 | +0.8%, within noise |

Batched consumers win; per-lookup consumers break even. `guard_cost`'s own header
notes the Python scalar path is the per-call one, so it is the one that gains
nothing here. If that ever needs fixing, a smaller direct-mapped cursor array
(4 entries, `k % 4`, tag-checked) would cut the initialisation and keep the win
for shallow plans, at the cost of collisions on deep ones.

---

## 17. The cliff under a writer — the contention hypothesis is falsified, and §16 gets stronger

§12 and §16 were both measured on a **quiescent** tree, which is not the deployed
condition, and that left two open worries pointing opposite ways:

* the cliff might get **worse** under a writer, because every push writes the
  stamp cache lines a reader is probing — which would raise the value of every
  footprint lever;
* the **cursor might stop helping**, because its whole benefit is resuming into a
  line that is still warm, and a writer is exactly what makes it cold.

`contended_search` measures both. One dynamic edge, one reader asking at a fixed
lag behind the newest stamp — what a consumer actually does, and what keeps the
query inside a window the writer is sliding — and either zero or one writer
publishing at the edge's nominal 1 kHz.

Three things are asserted rather than assumed, because each would make the result
a measurement of nothing: that pushes **landed**, that none were **refused**, and
that no query fell **outside the retained window**. All three hold in every cell.

Median ns/query, pinned to two cores:

| capacity | stamps | fresh, quiet | fresh, +writer | writer cost | cursor, quiet | cursor, +writer | writer cost |
|---|---|---|---|---|---|---|---|
| 1 024 | 8 KiB | 9.84 | 9.82 | 1.00× | 5.40 | 5.53 | 1.02× |
| 4 096 | 32 KiB | 10.94 | 10.93 | 1.00× | 5.44 | 5.52 | 1.01× |
| 16 384 | 128 KiB | 12.60 | 12.15 | 0.96× | 5.47 | 5.56 | 1.02× |

The writer achieved **918–939 Hz** — below its nominal 1 kHz, because `sleep`
overshoots — and that works out to **~8 pushes per million reader queries**.

### Both worries are answered, and neither the way I expected

**A writer costs the search essentially nothing.** The writer's multiplier is
0.89× – 1.06× on every column — noise in both directions. The invalidation
mechanism is real in principle and is simply not what a 1 kHz publisher does to a
reader: one store per millisecond against a reader issuing tens of thousands of
probes in the same interval.

**The cursor's benefit survives contention entirely** — and in this access
pattern it is *larger* than §16 measured, because §16 swept the whole window
while a real consumer polls near the newest stamp. Here the cursor is worth
**1.8× – 2.4×**.

More striking than the ratio: **the cursor makes the search capacity-independent.**
5.34 – 5.52 ns across a 16× range of ring capacity, quiescent or contended, while
the fresh search climbs with it. The cliff is not merely reduced; for the pattern
a consumer actually uses, it is gone.

That is the strongest confirmation yet of §12's mechanism. If the cost were probe
*count*, the cursor could not flatten it — a bigger ring is more probes either
way. Only a locality explanation predicts a flat line.

### The ratio is the whole explanation

**~8 pushes per million queries.** The reader out-issues the writer by roughly
125 000 to one. A cache line can only be contended if two parties touch it in
overlapping windows, and at four orders of magnitude apart they essentially never
do. The invalidation mechanism is real in principle; a `/tf` publisher is simply
nowhere near fast enough to exercise it against a reader in a tight loop.

That number is also what makes the result *portable*: it does not depend on this
host's cache sizes, only on the rate ratio, and no realistic publisher changes
the ratio by four orders of magnitude.

### A methodological defect this found in itself

The first version of this harness ran 4096 queries a round — about 40 µs — so a
whole 41-round loop finished in under two milliseconds and the writer landed
**four pushes** during the entire contended measurement. The contended columns
were very nearly a measurement of *no writer*.

It surfaced through an impossible number rather than through review: the derived
publish rate was quantised by a ±1-push error to ±250 Hz, and printed **1182 Hz
for a loop that sleeps 1 ms**. A rate above the reciprocal of its own sleep
period cannot happen, which is what made it worth chasing.

The rounds are now sized so each loop runs ~200 ms and the writer lands ~200
pushes. The conclusion did not change — but before the fix it was not supported,
and a reader had no way to tell.

### What this does not cover

One writer, on the **same edge** as the reader. That is deliberately the sharpest
test of the invalidation hypothesis, because it is the only configuration where
the writer touches the exact lines the reader probes. `docs/PHASE1.md` §11.2
specifies *four* concurrent writers, but four writers necessarily sit on four
different edges — a weaker test of this mechanism, and one `read_scaling`'s
`+writers` group already covers for aggregate throughput.

Higher publish rates are also unmeasured. A 1 kHz writer is one store per
millisecond; a pathological publisher is not, and nothing here bounds where that
starts to bite.

