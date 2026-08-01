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
O(`len`) walk of a 1 KiB `[Step; MAX_DEPTH]` array — 28 steps of scanning on a
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
plausible mechanism (28 iterations! 1 KiB!) reasoned about instead of measured.
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

That split is what makes Lever 2 the right next move rather than another
arithmetic win. It attacks both halves at once: restructuring locate /
interpolate / compose replaces the per-step walk with three tighter loops *and*
changes the inlining context that the other half lives in.
