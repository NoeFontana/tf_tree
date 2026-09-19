# 0060: the batch fold that reads before it interpolates

**Status:** ready — **Decision A has landed** (§11, 2026-09-18); Decision B is
still proposed and its step 0b has not run, so this record is not `implemented`.
**Owner:** @NoeFontana
**Implementation:** **Decision A is in the engine** — `Plan::fold_batch` is a
chunked two-phase fold at sixteen lanes with a per-stamp bypass below three
stamps, all four batch entry points share it, and `SampleRing::sample_from` and
the batch share one read body (`read_from`). §11 is what landed and what it
cost. **Decision B is not**, and the prototype this record measures its kernels
against was built in a detached worktree at `681e601`, was never committed and
is not coming in as it stands.

Steps: **0a done** (§9; harness `crates/tf_tree_bench/examples/bracket_mix.rs`;
answers open question 1). **1 done** (§10; bench rows `at_many_small/*` and
`at_many_recorded/*`; answers open question 2 and amends Decision A in three
places). **2 done** (§11; `at_many_shapes/*`,
`crates/tf_tree/tests/batch_phases.rs`, a loom model, six mutants; answers open
question 3). **0b needs the vanished prototype's kernels and has not run**, so
Decision B is not taken. **4 (aarch64) has not run.**

## Context

`Plan::at_many`, `at_many_into` and `at_many_into_f32` folded the **whole plan
once per stamp**: per dynamic step, one seqlocked ring read
(`SampleRing::sample_from`) and one `Interp::eval`. `Plan::fold_batch_with_twist`
is the `QuatTwist` sibling and is untouched.

[`0016`](./0016-portable-simd-and-the-dependency-budget.md) is `withdrawn`; its
findings are the starting point. Four of five blockers to vectorising across
stamps belong to the engine (the `head` loads, `read_slot`'s seqlock and
`fence`, the `?` early exit, the data-dependent bracket search), and a
branch-free interpolation loop vectorises (1.67× on LerpSlerp quaternions) but is
"unreachable from where the engine stands" until every bracket is read first and
interpolated second. `-C target-cpu=x86-64-v3` measured 8–14% *slower* on
`at_many`.

[`fast-path.md`](../design/fast-path.md) §14 falsified Lever 2 (splitting the
fold across *steps*). **This restructure runs across stamps within one step and
is not Lever 2 reopened**: a chunk of stamps reads one step's ring, that step is
interpolated, then composed; the *d* steps still run in plan order.

**Constraints every arm held, and any implementation must hold.** No `unsafe` in
`tf_tree_math` (`#![forbid(unsafe_code)]`) and none added to `tf_tree_core`
([`0007`](./0007-the-unsafe-budget-and-the-c-abi.md) rule 1, as amended by
[`0048`](./0048-a-kind-is-not-a-crate-name.md); no row added to
`scripts/unsafe-budget.txt`). No intrinsics, no `-C target-cpu`, no NLERP, no new
dependency, D5's `LerpSlerp` semantics unchanged, `f64` only. **Batch results
must be bit-identical (`to_bits`) to `Plan::at` for the same stamp.**

## The stop rule

- **A lever that wins under ~5% stops.** 0016 §3 set that floor as its own
  measurement's noise.
- **A kernel must clear 1.3× in the probe** (§2: the kernel against
  `Interp::eval` in a loop, same pose pairs) before an engine prototype is worth
  building. `v1` cleared it (1.676× LerpSlerp, 2.008× ScLerp). **`v2`'s LerpSlerp
  kernel fails it at 1.268×**; its ScLerp kernel clears it at 1.796×. Decision B
  applies the bar.
- **The floor is applied per lever, each against its own control:** the
  restructure (two-phase fold, scalar `Interp::eval`) against `681e601`, and the
  kernel against the restructure.
- **The kernel is also checked against itself built with
  `-C no-vectorize-loops`**, which isolates what is SIMD.

## What was measured

### Host and method

AMD EPYC-Milan (Zen 3), 4 cores / 8 threads SMT, KVM guest; rustc 1.97.1; every
timed binary is `cargo bench --no-run` (`[profile.bench]`: thin LTO,
`codegen-units = 1`, SSE2 baseline, two `f64` lanes); `taskset -c 2`,
`CARGO_TARGET_DIR` unset; no hardware counters, so **no mechanism claim rests on
a counter.** §1–§8 ran 2026-09-14 against `681e601` plus the prototype; §9 times
nothing. Arms alternate within each cell, the order rotates every rep, deltas are
paired per rep.

Cells: `1dyn` is `odom → map` (one 50 Hz dynamic step); `3dyn` is
`imu_link → map` (three dynamic steps, the existing `at_many` plan); `mono` is
`lo + span·i/n` over the last 100 ms; `nonmono` permutes it by
`i → (i·389) mod n`; `offgrid` is `mono` + 1 ns.

| arm | what it is |
|---|---|
| `base` | the `681e601` engine |
| `scal` | the two-phase chunked fold, phase 2 calling scalar `Interp::eval`; **isolates the restructure** |
| `v1` | `scal` with the SoA branch-free kernel, plus a scalar fix-up of every element the classifier rejects |
| `v2` | `v1` with the kernel answering `s == 0` / `s == 1` by bitwise select and a monomorphic, non-cold fix-up |
| `v2nlv` | `v2` built with `-C no-vectorize-loops`; re-measurement only |

The fold: chunks of up to 64 stamps. **Phase 1**, per dynamic step, reads each
stamp's bracket `(a, b, s)` into lane buffers through a copy of `sample_from`
with seqlock and lap check kept. **Phase 2** interpolates the chunk, then
composes. A failing read at element `i` lowers the chunk's limit to `i`, so the
error is the per-stamp fold's; `note_ok` fires per element in order, then
`note_err`; nothing past `i` is written (`at_many_into`'s partial-write contract).

### 1. Baseline

`monotone_1024` on `681e601` is 263 ns/stamp (269.3 µs per batch against 0016's
271.9–275.9 µs). The non-monotone path (`fold_at` + `sample_hinted`) is
9.5–14.3 ns/stamp *faster* than the monotone one that calls the out-of-line
`fold_at_cursors`, though the cursor exists to make monotone fast. **Recorded, not
investigated.**

### 2. The kernel's ceiling, isolated

`crates/tf_tree_bench/examples/autovec_probe.rs` runs each kernel plus its
safe-region classifier over 1024 pose pairs, chunked by 64, `to_bits`-identical
to `eval` on every element. ns/element, median of 5 rotations:

| variant | as built | no loop vectoriser |
|---|---|---|
| `LerpSlerp::eval` in a loop | 17.993 | 18.019 |
| `ScLerp::eval` in a loop | 47.840 | 47.947 |
| LerpSlerp SoA kernel | **10.732** | 18.717 |
| ScLerp SoA kernel | **23.833** | 47.737 |

- **Ceiling:** 1.676× (LerpSlerp), 2.008× (ScLerp). **Nearly all of it is the loop
  vectoriser**; without it 0.962× and 1.004×. SLP is irrelevant (0.997–1.005×).
- **`v2`'s endpoint selects cost ~3.4 ns (F) and ~2.7 ns (H) per element**: the
  LerpSlerp kernel drops to **1.268×** (below the bar), ScLerp to **1.796×**.

### 3. Evidence the widening is across stamps

At the SSE2 baseline there is no `ymm`, so the census counts, per symbol, packed
`f64` arithmetic (`addpd`, `subpd`, `mulpd`, `divpd`, `sqrtpd`, `minpd`, `maxpd`)
against its `-sd` forms; **the count must collapse under
`-C no-vectorize-loops`** or the census is vacuous. In the engine bench binaries
`Plan::at_many::<SystemDomain>` went 744 → 139, `sclerp_soa::<64>` 391 → 98 and
`lerp_slerp_soa::<64>` 166 → 37 (`v2` → `v2nlv`); no `%ymm`/`%zmm` anywhere.
Prototype entry frames grew from 344–1 256 B to ~16 kB (five zero-initialised
64-lane buffers are 14 848 B of it).

### 4. End to end

29 cells × 4 arms × 5 reps; criterion 95% CI half-width median 0.08%, max 1.93%.
ns/stamp, N = 1024:

| cell | base | scal | v2 | v2 vs base | scal vs base | v2 vs scal |
|---|---|---|---|---|---|---|
| 3dyn ScLerp mono (flagship) | 261.53 | 192.35 | 124.88 | **−52.2%** | −26.4% | −35.2% |
| 1dyn LerpSlerp mono | 72.25 | 36.11 | 30.61 | −57.5% | −49.7% | −15.4% |
| `into_mat4_1024` | 262.70 | 243.82 | 122.36 | −53.4% | −6.9% | −49.9% |
| `into_quat_twist_1024` (**negative control**) | 328.63 | 325.23 | 327.00 | −0.6% | −1.0% | +0.5% |

Across all 24 matrix cells `scal` is faster than `base` in every cell whose fold
it touches (−16.8% to −50.1%), stationary ones included; the restructure's share
of `v2`'s saving is 38–54% under ScLerp and 82–87% under LerpSlerp. **Build-level
noise is about ±1–3%**; a delta under ~3% is not read as an effect. The
restructure clears 5% everywhere; the kernel clears it against `scal` in every
series-region cell but does not outrank the failed 1.3× probe bar.

### 5. Where it loses: stationary edges

One pose published every 5 ms, 1024 monotone stamps: under the prototype
classifier every element falls outside the safe region (LerpSlerp: `h == 0`;
ScLerp: for this cell's quaternion `conj(q)·q = (1.0000000000000002, 0, 0, 0)`, so
`sh2 = 0`, below `SCREW_DEGENERATE_SQ`). Whether `conj(q)·q`'s vector part cancels
exactly is a property of the quaternion's **zero pattern**, not of constancy
(§9.3), so this all-fallback regime is what *this cell's* quaternion does, not what
a motionless ScLerp edge generally produces.

| cell | base | scal | v2 | v2 vs base | v2 vs scal |
|---|---|---|---|---|---|
| LerpSlerp | 38.77 | 21.77 | 39.23 | +0.9% | **+80.0%** |
| ScLerp | 107.33 | 53.08 | 113.96 | +5.9% | **+115.5%** |

On this data the kernel erases the restructure's ~2× win (parity under LerpSlerp,
+5–6% loss under ScLerp).

### 6. Knots: why `v1` and `v2` differ

Every on-grid 64-stamp batch starts on a knot (`s == 0`); under `v1` a knot sends
the whole chunk through a `#[cold]` fix-up (LerpSlerp 1dyn/64: `v1` vs `scal`
+49.4% on-grid, −24.8% off-grid). `v2`'s selects remove that (31–45% faster than
`v1` on-grid N = 64) and pay elsewhere (+8.6% to +16.3% off-grid). **A cheaper
design was not built:** copy phase 1's recorded exact hits over the kernel's
output afterwards, with no select in the loop. Step 0b builds it as `copy`.

### 7. Independent re-measurement

A second agent rebuilt every arm and added `v2nlv` (8 cells × 4 arms × 6 reps;
other-CPU busy time ≤ 2%). Every headline figure reproduced within ~1%. Where the
campaigns disagree this record takes the conservative reading:

1. On `at_many` the kernel's gain over `scal` is the loop vectoriser within
   ±1–3% noise; on 3dyn LerpSlerp about 18% of the gain is not.
2. **On `into_mat4`, "87% kernel" is withdrawn**: `v2nlv` is still −23.8% against
   `scal` there (SIMD share `v2` vs `v2nlv`: **−34.2%**), because `scal` was a
   weak control on that entry point. Why was UNEXPLAINED; §10.2 finds it does not
   reproduce.
3. The attribution moves on `into_mat4` only.
4. The first campaign's host-quiet instrument (`ps -eo pcpu`) could not support
   its claim; the reproduction on a `/proc/stat`-measured quiet host does.

The saved `base`/`scal` binaries differ by sha256 from their rebuilds for an
unknown reason. **Rebuild rather than reuse saved binaries.**

### 8. Bit identity and mutants

**Bit identity: PASS at three levels.**

1. **Kernel pin** (`tf_tree_math`): every element the classifier calls safe
   matches `LerpSlerp::eval` / `ScLerp::eval` by `to_bits` (>8 000 safe elements
   per policy); every out-of-region band classifies unsafe (large arc, LERP
   fallback, `h == 0`, endpoints incl. signed zeros, the ScLerp degenerate band
   straddling `1e-290`, NaN); under `v2` the output is exactly `a` at `s == 0` and
   `b` at `s == 1`.
2. **Engine test** (`crates/tf_tree/tests/soa_proto.rs`): `at_many`,
   `at_many_into` (Quat, Mat4) and `at_many_into_f32` against `Plan::at` on a fresh
   `Guard`, over crafted branch regions plus 200 random series steps, ScLerp /
   LerpSlerp / mixed plans, both `inverted` flags, batches of 1, 2, 63, 64, 65, 129
   at three offsets, **every stamp alone and paired with a known-safe stamp in lane 0
   and lane 1**, and an error grid of 5 lengths × 6 positions × 3 failure kinds
   (error, rows before it, sentinel from it on, per-edge counters). Every mutant
   below ran against its two-test version.
3. **Benchmarked data:** all 16 128 rows of the benchmarked matrix, off-grid and
   stationary cells are bit-identical to `Plan::at`. **No speedup here is fast
   because it is wrong.**

| mutant | outcome |
|---|---|
| M1 ScLerp kernel 1-ulp reassociation | survived a crafted-only fixture; caught once random series added |
| M2 LerpSlerp translation as `a + (b − a)s` | caught |
| M3 LerpSlerp classifier admits the fallback band | survived crafted + random; caught by isolated stamps |
| M4 ScLerp classifier admits `s == 1.0` | survived until isolated stamps; on `v2` equivalent (the select overrides it) |
| M5 an error does not lower the chunk limit | caught |
| M6 `v2`'s `s == 0` select takes `b` | survives the engine test (phase 1 stores every exact hit as `(p, p, 0.0)`); **caught by the kernel pin** |
| M6b `v2` kernel drops its `s == 1` select | caught |

**The test-design finding:** a kernel-classifier defect is **masked whenever
another unsafe element shares its chunk**, because the fix-up re-derives the
predicate separately. Only per-stamp isolated batches, in both lanes, caught M3
and M4. Dropping `h > 0.0` from both LerpSlerp predicates survives (equivalent:
`h == 0` forces `theta_sq == 0`, which already fails the fallback bound); dropping
ScLerp's `sh2` upper bound is caught by the isolated-stamp shapes.

### 9. Step 0a: what the recorded mix is

A measurement of *data*: no engine code, nothing timed.
`crates/tf_tree_bench/examples/bracket_mix.rs` is the instrument and can be
re-run:

```sh
cargo run --release -p tf_tree_bench --example bracket_mix
cargo run --release -p tf_tree_bench --example bracket_mix -- <stream> [sweep_hz]
```

It classifies every bracket an `at_many` sweep would read into the five arms of
`slerp` and `screw_parts` / `ScrewParts::pow`, per edge, per policy and per
64-stamp chunk, under three sweeps: a 100 Hz off-grid grid (`rate`), one query per
sample interval (`interval`), and one query at every knot (`ongrid`).

**The bracket search mirrors `SampleRing::sample_from` and is checked.** Every
swept stamp also goes through `Plan::at` on the same one-edge plan and the two
must agree **bit-identically**; the `checked` column is that count and equals `n`
in every row. **A bracket whose `s` rounds to `1.0` counts as an exact hit**, as
the kernel's predicate does (a 2e16 ns span queried 1 ns short of its end gives
exactly `1.0`); the other endpoint is unreachable. Nothing in this recording is
near either.

#### The recording

`testdata/tfstream/indoor_atelier.tfstream`: 5 dynamic edges, 1 066 samples,
47.3 s, CC BY 4.0 (`testdata/tfstream/ATTRIBUTION.md`). `LerpSlerp` and `ScLerp`
agree on every cell, so one table serves both (§9.3 says why that is luck).

| sweep | edge | n | series | stationary | lerp fb | large arc | exact hit |
|---|---|---|---|---|---|---|---|
| `rate` 100 Hz | the four `base_link` wheel edges (each) | 4 220 | 0.0% | **100.0%** | 0.0% | 0.0% | 0.0% |
| `rate` 100 Hz | `odom_combined→base_footprint` | 4 730 | **86.3%** | 0.0% | 0.0% | 13.7% | 0.0% |
| `rate` 100 Hz | all edges | 21 610 | 18.9% | 78.1% | 0.0% | 3.0% | 0.0% |
| `interval` | the four wheel edges (each) | 202 | 0.0% | **100.0%** | 0.0% | 0.0% | 0.0% |
| `interval` | `odom_combined→base_footprint` | 253 | **99.2%** | 0.0% | 0.0% | 0.8% | 0.0% |
| `interval` | all edges | 1 061 | 23.7% | 76.2% | 0.0% | 0.2% | 0.0% |
| `ongrid` | every edge | 1 066 | 0.0% | 0.0% | 0.0% | 0.0% | **100.0%** |

#### 9.1 Four of the five edges never move

All 202 intervals of each of `left_front_link`, `left_wheel_link`,
`right_front_link` and `right_wheel_link` have bit-identical rotations (10 Hz
dynamic `/tf`, a joint-state publisher's shape for a joint that does not turn).
That is the regime §5 measured the prototype **losing 80% (LerpSlerp) and 115%
(ScLerp)** on, and it is 4 of 5 edges of the only real recording in the tree.

#### 9.2 The one moving edge is 99.2% series, and its 13.7% is gaps, not motion

`odom_combined→base_footprint` publishes at 19.8 Hz by median interval and has
two large-arc intervals of 253 (1.20 s and **5.30 s**); a 100 Hz sweep asks 650 of
its 4 730 questions inside them, which is the whole 13.7%: duration weighting, not
fast motion. The gaps are large-arc because they are *long*.

**Both policies share one series bound, and it is a statement about angle:**
`LerpSlerp` is in its series arm while `θ² ≤ THETA_SLERP_SMALL²`, `ScLerp` while
`sin²(θ) ≤ SIN_HALF_THETA_SMALL_SQ`, and that constant *is* `sin(0.15)²`. Both
reduce to `θ ≤ 0.15 rad` between consecutive samples (body rotation 0.30 rad).
The rate at which each interval would enter the series region:

| edge | published (median interval) | series above, p50 / p90 / p99 / max |
|---|---|---|
| the four wheel edges | 10.0 Hz | — (no rotating interval) |
| `odom_combined→base_footprint` | 19.8 Hz | 0.010 / 1.135 / 1.192 / **1.226 Hz** |

Had the publisher not stopped, every interval of this edge would be series at any
rate above 1.23 Hz. That extrapolation assumes constant turn rate across the
interval, which is what the interpolant assumes. The gaps are what `TFT009`
(`PHASE5.md`) reports from the detector's side.

#### 9.3 `ScLerp` reads a motionless edge as degenerate only by luck

`conj(q) ⊗ q` is the identity in exact arithmetic; its vector components cancel
by different routes in `f64`. **`x` always cancels exactly.** `y` and `z` need
not: a quaternion with a zero in the right place (the wheel quaternion has
`w = z = 0`) makes every term vanish before it can round, so `sin²(θ/2)` is
exactly `0`, below `SCREW_DEGENERATE_SQ`: degenerate arm. A quaternion with four
non-zero components leaves `sh2 ≈ 5e-36`, **4.7e254 times**
`SCREW_DEGENERATE_SQ` (1e-290): the edge lands in `ScLerp`'s **series** region on
rounding noise, bit-identically to `ScLerp::eval`. That is not a defect
(`screw_pow_is_accurate_down_to_the_degenerate_threshold` sweeps θ to 1e-160). It
means:

1. "The robot is not moving" and "the interpolant takes its fallback arm" are the
   same statement under `LerpSlerp` and not under `ScLerp`.
2. A `ScLerp` kernel fires on motionless edges of most other shapes. §5's
   all-fallback loss is a `LerpSlerp` regime; under `ScLerp` it is a coincidence
   of this recording's frames.

#### 9.4 The chunk fraction is near-bimodal: a bail-out has no threshold to tune

Under the 100 Hz sweep the wheel edges give **66 of 66** chunks at 0.00 series
fraction, and `odom_combined→base_footprint` gives **61 of 74** at 1.00 and 9 at
0.00 (4 straddle a gap boundary). Any bail-out threshold strictly between 0 and 1
makes the same decision. **What a bail-out separates here is edges, not chunks.**

#### 9.5 An on-grid consumer is 100% exact hits

A ROS consumer looks a transform up *at the stamp of the message it is
processing*, and a publisher driven by the same sensor puts a knot there. That is
§6's regime: the worst case for `v1`, and the reason `copy` and `v2` exist as
separate arms in step 0b.

#### 9.6 The controls

Every one of the five classes is reached, so no column can only ever be zero:

| control | what it is | `LerpSlerp` | `ScLerp` |
|---|---|---|---|
| `fixture` | the synthetic 50–1000 Hz fixture the matrix bench uses | **100.0% series** (3 368 brackets) | **100.0% series** |
| `repeat/axis` | one pose repeated, the wheel edges' quaternion shape | **100.0% stationary** | **100.0% stationary** |
| `repeat/generic` | one pose repeated, four non-zero components | **100.0% stationary** | **100.0% series** (§9.3) |
| `jitter` | ~1e-7 rad a sample | **100.0% LERP fallback** | **100.0% series** |

`jitter` exists because the `lerp_fb` column would otherwise read 0.0% everywhere.
**Nothing sits near a class boundary**: the `near` column (within 1e-9 relative)
is 0 in every row, so reaching `θ²` by `2·asin(√(h/2))` rather than
`theta_sq_from_chord` cannot have moved a bucket.

#### 9.7 What step 0a does and does not settle

**Settled.** The mix is measured with its controls (open question 1); the
bail-out's threshold is not a tuning problem (§9.4).

**Not settled.** This is **one 47-second indoor run of one wheeled robot**, whose
fastest edge publishes at 19.8 Hz. It has **no high-rate edge**, and the series
region is exactly the high-rate regime, so the corpus is biased toward *shrinking*
B's regime (`fixture`'s 50–1000 Hz edges are 100% series). B's stop-rule verdict
is a verdict about this stream. **What generalises is §9.2's bound
(`θ ≤ 0.15 rad` between samples, both policies), not §9's percentages.**

**INFERRED, not the stop rule.** Across all five edges at 100 Hz, **81.1% of
brackets fall back**, against a ~27% break-even (below). Without a bail-out B
loses on this stream; with one it wins on at most the 61 all-series chunks of the
recording's 338. That is a prediction from two numbers on unlike data, so **B's
stop rule is still step 0b's interleaved timings**; it does mean the bail-out
stopped being optional. Under §9.3, B's feared all-fallback regime is
`LerpSlerp`'s everywhere and `ScLerp`'s only on frames shaped like these, and B
proposes no LerpSlerp kernel.

**The break-even is a linear interpolation between two unlike datasets and must
not be used as a threshold.** On 1dyn ScLerp mono/1024 `v2` saves 22.85 ns/stamp
against `scal` on series data and costs 60.88 on stationary data, so it wins only
while the fallback fraction stays under 22.85 / (22.85 + 60.88) ≈ 27% (≈ 24%
under LerpSlerp). The mixing is not linear (fix-up re-classifies all 64
elements); it only says the fraction that matters is tens of percent, not a tail.

### 10. Step 1: what A's win actually is, and what it costs at small N

Measured 2026-09-17 on `main` at `ac5d4d3`; `git diff 681e601..main` over
`plan.rs`, `sample.rs` and `interp.rs` is empty, so the fold is byte-identical to
§1–§8's. **These arms are an independent reimplementation from *Reproduction*.**
Paired medians over 4–5 reps; the whole suite (202 tests, incl. `tests/batch.rs`'s
`to_bits` check over 700 stamps) passes on every arm.

| arm | what it is |
|---|---|
| `base` | `main`, unmodified |
| `hoist` | each dynamic step's `(interp, ring)` resolved **once per batch**; no chunking, same loop order, same scalar `eval` |
| `accbuf` | steps outer / stamps inner over a 64-stamp chunk with **only the accumulator** buffered; sampling and `eval` stay fused |
| `scal` | the full two-phase chunked fold, 64 lanes |
| `scal16` | `scal` at 16 lanes |

#### 10.1 The win is the phase buffering, and nothing else comes close

| arm | `monotone_1024` | `into_mat4_1024` | `into_quat_twist_1024` |
|---|---|---|---|
| `hoist` | **+0.71%** | −1.59% | −0.12% |
| `accbuf` | **−3.43%** | **−1.29%** | −1.07% |
| `scal` | −25.10% | −22.67% | +0.52% |
| `scal16` | −24.29% | −22.95% | — |

The sampler and policy hoist buys nothing (3 072 arena lookups become 3;
+0.71%). Loop order and the dropped `fold_at_cursors` call are worth −1.3% to
−3.4%, inside the floor. **Deferring the `eval` carries the whole 23–25%.** So
step 1's escape hatch (a sub-change that carries A's saving without phase
buffering) does not open: the buffering is what A is.

#### 10.2 A is uniform across entry points, so it does not need to bind per one

`scal` vs `base`: `monotone_1024` −24.50%, `into_mat4_1024` −23.81%,
`into_quat_1024` −25.37%, control −1.00%. **§7's `into_mat4` weakness does not
reproduce** (three runs: −22.7%, −23.2%, −23.8% against the flagship's −24.5% to
−25.1%); what separates the campaigns is the prototype. **The conservative
reading is that the per-entry-point split is unnecessary, not that it is wrong.**

#### 10.3 Sixteen lanes

| N | `scal` (64 lanes) | `scal16` (16 lanes) |
|---|---|---|
| 1 | **+208.85%** | **+82.45%** |
| 2 | — | **+14.82%** |
| 3 | — | −11.10% |
| 8 | −12.96% | −21.60% |
| 63 | −23.82% | −23.87% |
| 1024 | −24.72% | −24.29% |

**Sixteen lanes is better everywhere it differs**: equal at 1024 and 63, ahead
below (−21.6% vs −13.0% at N = 8), because it initialises a quarter as much.
Stack reservation (`objdump -d -C`, §3's method): `at_many` 1 256 B at `base`,
17 720 B at 64 lanes, **6 600 B** at 16 (6 664 B and 6 504 B for the two `_into`
entries). *Consequences*' 4.8–5.0 kB derivation was optimistic by about 35%.

#### 10.4 The cost nobody had measured: a batch smaller than a chunk

**Both chunk sizes fail step 1's stop point as written** (N < 64 rows must not
lose to `base`): one stamp is 3.1× slower at 64 lanes and 1.8× at 16. `la`, `lb`,
`ls` and `lp` are `[Iso3::IDENTITY; CHUNK]`, initialised on every call whatever
the batch holds. **A bypass costs nothing and needs no `unsafe`**: N = 2 is
+14.82% and N = 3 is −11.10%, so a batch of fewer than **three** stamps takes the
per-stamp fold. Both sides are committed bench rows (`at_many_small/*`), so the
threshold can be re-derived on another host.

#### 10.5 On the recorded stream, which is what A's stop rule is applied to

§9's two plans over `indoor_atelier.tfstream`, 1024 monotone off-grid stamps,
`scal16` against `base`:

| plan | `at_many` | `at_many_into(Mat4)` |
|---|---|---|
| `laser → odom_combined` — one static step and the one **moving** dynamic edge | **−26.16%** | **−23.44%** |
| `left_wheel_link → odom_combined` — adds a dynamic step motionless all recording (§9.1) | **−35.72%** | **−19.69%** |

**A clears its ~5% floor by four to seven times at every entry point on real
data**, most on the plan that crosses a motionless edge.

### 11. Step 2: A as landed, and the one thing it costs

Landed 2026-09-18 against parent `2524667`: Decision A as step 1 amended it (one
fold body, sixteen lanes, per-stamp fold below three stamps, `sample_from` through
the bracket read). `plan.rs` carries `FOLD_LANES = 16`, `FOLD_MIN_BATCH = 3` and a
`const` assertion pinning both, because `crates/tf_tree/tests/batch_phases.rs`
restates them.

#### 11.1 The numbers, `[profile.bench]`

Interleaved paired runs against `2524667`, 7 reps, median of per-rep deltas:

| row | `2524667` | A | delta |
| --- | ---: | ---: | ---: |
| `at_many/monotone_1024` | 273 811 ns | 220 115 ns | **−19.19%** |
| `at_many/into_mat4_1024` | 270 537 | 221 967 | −18.08% |
| `at_many/into_quat_1024` | 272 144 | 220 264 | −19.06% |
| `at_many/into_affine32_1024` | 280 733 | 223 481 | −20.38% |
| `at_many/into_quat_twist_1024` **(control)** | 340 851 | 340 862 | **+0.00%** |
| `at_many_recorded/mixed_at_many_1024` | 206 754 | 141 119 | **−31.35%** |
| `at_many_recorded/mixed_into_mat4_1024` | 204 648 | 174 914 | −14.17% |
| `at_many_small/at_many_1` | 104 | 101 | −2.18% |
| `at_many_small/at_many_2` | 261 | 260 | +2.34% |
| `at_many_small/at_many_3` | 651 | 577 | −11.52% |
| `at_many_small/at_many_16` | 3 723 | 3 116 | −16.31% |
| `at_many_shapes/*` (six rows) | | | −18.58% to −28.30% |

The other `at_many_recorded/*` and `at_many_small/*` rows are −2.8% to −18.6%.
`into_quat_twist_1024` is the control: its fold is untouched, so a run that moved
it would be measuring the host.

**A's per-entry-point stop rule: PASSED everywhere**; on the recorded stream the
two plans clear the ~5% floor by 2.8–6.3×. **No batch size loses**: `at_many_2`'s
+2.34% is the only positive number and is inside noise.

#### 11.2 The frame the record predicted, and what actually happened

*Consequences* predicted the entry frames would grow to ~16 kB. **As landed they
do not grow at all**: the chunked pass is a separate `#[inline(never)]` function
(`fold_chunked`, four instantiations, 4 056–4 088 B), so the lanes are reserved one
call in, in a frame a batch under three stamps never enters. `at_many_into_tagged`
stays `sub $0x378,%rsp` (888 B) and `at_many_into_f32_tagged` `sub $0x158,%rsp`
(344 B). The `API.md` §8.3 page-fault residual therefore does not move for a
caller that never batches, and moves by one page for one that does. The split was
reached for on a wrong hypothesis (the small-N regression it was meant to fix was
11.3's); it is kept for the frame result above.

#### 11.3 The cost of one read body, which is real and is on `Plan::at`

Open question 3 asked for one read body. Having `read_from` return a `Bracket` and
let `sample_from` fold it **regresses the scalar path** (a `Bracket` is 128 bytes,
carried through a `Result` on every sample): `lookup/depth3/sclerp/exact_hit`
45 ns → 85 ns (**+86.4%**; `#[inline(always)]` +6.7%). **As landed the return type
is the parameter**: `read_from` is generic over a `FromBracket` trait, the batch
instantiates it at `Bracket` and the scalar path at `Interpolated<I>` (56 bytes,
never an enum). The search, the seqlocked slot reads and the trailing lap check
live in one function, and at `[profile.bench]` every `lookup/*` row is within
**±1.4%** of `2524667` across two 7-rep runs.

**At `[profile.embedder]` (`lto = false`, `codegen-units = 16`) it still costs**,
the one negative result in step 2: `depth1/sclerp` +10.13%, `depth3/sclerp`
+10.77%, `depth3/lerpslerp` +7.72%, `depth6/sclerp` +10.95%,
`depth3/sclerp/exact_hit` −3.37%. It is the read split and nothing else: this
change's `sample.rs` with `2524667`'s own `plan.rs` reproduces it within a point,
and `read_from` is fully inlined there. Nothing here pins the cause further.

**Why it lands anyway, as a principal decision on the owner's delegation:**

- `[profile.bench]` is the workspace's own profile, the one
  `crates/tf_tree_bench/baseline/results.json` was taken under; there the scalar
  path is unchanged and every batch row wins 14–31%.
- `docs/API.md` §2.3 already records that this path costs 25% at
  `lto = false, codegen-units = 16` for a cross-crate caller and that
  `lto = "thin"` erases it.
- Two read bodies would trade a structural correctness invariant for 10% on one
  profile: `sample.rs`'s module doc records that the trailing revalidation "has
  seven places that need it and six of them did not have it".
- `just bench-check`'s `lookup_latency` and `embedding_cross_crate` rows are
  **UNAVAILABLE on this host** (SMT, unreadable governor), so no committed gate
  sees this either way.

**Reopen it** if a quiet, non-SMT, fixed-governor host reproduces the embedder
column, or if `just embed-cost`'s gated row ever resolves on such a host and
moves.

#### 11.4 A lever found and not taken

`#[inline(always)]` on `SampleRing::sample_from` itself is −27.6% to −38.0% on
every interpolating `lookup/*` row at `[profile.bench]` and +13.5% to +20.2% at
`[profile.embedder]`: the same trade as 11.3, so not taken. It belongs to
`docs/API.md` §2.3's inline-placement question.

#### 11.5 What holds it

- **`crates/tf_tree/tests/batch_phases.rs`, 6 tests** (the sixth `unstable`-gated),
  reproducing §8's engine test: eleven crafted branch regions repeated 4× plus 200
  random series steps; six plan shapes over both policies, mixed policies and both
  `inverted` flags; every batch entry point against `Plan::at` by `to_bits`; every
  stamp alone, paired, and in lanes 0, 1 and 15 of a 17-stamp batch; the error grid;
  identity and all-static plans. `the_fixture_reaches_the_branches_it_names`
  asserts the *inputs*, because every other assertion is satisfied trivially by a
  fixture that reaches one arm.
- **A loom model**, `read_from_validates_the_bracket_it_hands_back`, at
  `LOOM_MAX_PREEMPTIONS = 3`. Deleting `read_from`'s trailing `head - i > retained`
  check **FAILS** it and necessarily also fails
  `sample_from_with_a_stale_cursor_across_a_lap` (there is one check now). The
  three older `sample_from_*` models' mutants were re-run at their moved sites and
  still fail. **The moved lap check is not weaker**: it makes the same re-load and
  comparison over the same indices right after the slot reads and defers `eval`,
  which works on copies, so it still runs after the last shared-memory read it
  protects; `head` is monotone, so it can only refuse less often, and only for
  reads already complete and validated. For the scalar path it did not move at all.
- **Mutants**, each applied once and reverted:

  | mutant | outcome |
  | --- | --- |
  | **M5** — an error does not lower the chunk limit | **caught**, by the error grid alone: the failed lane keeps being read, fails again at a later step, and the later error overwrites the earlier one |
  | phase 2 ignores `inverted` | caught, 3 tests |
  | phase 1 reads `chunk[0]` for every lane | caught, 3 tests |
  | the accumulator is not reset per chunk | caught, 3 tests |
  | `cursors` reset at the top of every chunk | **survives — equivalent** (a cold cursor is a valid cursor) |
  | `FOLD_MIN_BATCH = 1` | **caught by the `const` pin**; with the pin relaxed it **survives — equivalent**: the bypass is a performance boundary held by `at_many_small/at_many_2` and `at_many_3` |
- **Gates run:** `just test`, `shm-check`, `stable-tier-check`, `msrv`, `doc`,
  `lint`, `loom` (27 passed), `miri`, `tsan`, `py-test`, `py-lint`, and
  `bench-check` under CI's `EMBED_COST_KNOWN_COLLAPSED=1`.

#### 11.6 What step 2 does not close

- §1's monotone-slower-than-non-monotone inversion; the non-monotone batch keeps
  the per-stamp `fold_at` loop deliberately, since chunking it is unmeasured.
- `[profile.embedder]`'s scalar column (11.3), with its reopen criterion.
- **aarch64** — plan step 4.
- Two unpulled levers inside A: the accumulator is refilled with `Iso3::IDENTITY`
  once per chunk where the plan's first step could assign into it; and the
  non-monotone batch still folds per stamp. Neither is decision-affecting.

## Decision (A taken and landed; B proposed, not taken)

Two separable decisions, taken in order.

1. **Decision A — the restructure, with no kernel. LANDED 2026-09-18 (§11).**
   `Plan::fold_batch`, and `at_many`'s loop through it, is a chunked two-phase
   fold: per chunk and per step, phase 1 reads every bracket through **the one**
   seqlocked read path and phase 2 calls the scalar `Interp::eval`. It wins in
   every cell whose fold it touches, needs no second arithmetic body, and is the
   prerequisite for B whatever B becomes. **Amendments from §10:**
   - **Sixteen lanes, not sixty-four** (§10.3): equal at N ≥ 63, ahead below, at
     6 600 B of frame against 17 720 B.
   - **A batch of fewer than three stamps takes the per-stamp fold** (§10.4): the
     chunked path is +82% at N = 1 even at 16 lanes. It needs no `unsafe`.
   - **One fold body, not one per entry point** (§10.2). The stop rule below still
     binds per entry point, but the second fold body it was willing to buy is not
     needed.

   Its read path is **one body**: `sample_from` is the bracket read plus `eval`,
   not a copy beside it (open question 3).
2. **Decision B — an SoA kernel for ScLerp, on top of A, only if step 0 says the
   data wants it.** It applies only to chunks the classifier admits, with a
   chunk-level bail-out (classify first, skip the kernel when the chunk is mostly
   fallback). §9.4: what the bail-out separates is *edges*, so its threshold is
   not a tuning parameter, and step 0b must answer whether paying the
   classification at all beats not having the kernel. Its knot handling is
   whichever of `v2`'s selects and a post-kernel copy of phase 1's exact hits
   clears the 1.3× bar and is faster on the recorded mix; step 0b builds and times
   both.

   **B proposes no LerpSlerp kernel.** `v2`'s is 1.268× in the probe, under the
   1.3× bar fixed before any engine number (§2), whatever its end-to-end margin on
   synthetic data. One re-enters B only if a variant clears the bar with its knot
   handling included and then clears the stop rule against A on the recorded mix.
   Until then B's LerpSlerp edges take A's scalar phase 2.

**The stop rules bind each separately.**

- **A** binds **per entry point**: it lands for each batch entry point where the
  arm wins at least ~5% against `681e601` on step 0a's recorded stream, and an
  entry point where it does not keeps the per-stamp fold. A is abandoned only if
  no entry point clears it (principal ruling on the owner's delegation,
  2026-09-14).
- **B** is abandoned if it wins under ~5% against **A** on the recorded mix, a loss
  included, or under ~5% against its own `-C no-vectorize-loops` build. Step 0b
  applies both, and step 3 applies them again at both profiles.

## Rationale

- **Adopt `v2` as prototyped: no.** Its −52% headline was measured on synthetic
  data that keeps nearly every element in the series region. On stationary data it
  is at parity under LerpSlerp, 5–6% slower under ScLerp and 1.8–2.2× slower than
  A; its LerpSlerp kernel fails the probe bar; and its kernels are term-for-term
  second bodies of `slerp`'s series arm and of `screw_parts` /
  `ScrewParts::pow` / `inv_mul`, held only by a bit-identity test.
- **Adopt A and B together: no.** They are separable, and A carries 82–87% of the
  saving under LerpSlerp and 38–54% under ScLerp; bundling would let B's
  unmeasured sign ride in on A's measured one.
- **A LerpSlerp kernel in B: not proposed.** Both rules were fixed before any
  engine number, and the bar decided whether an engine kernel was worth building.
  LerpSlerp is also where the kernel carries least (13% of the saving) and it is
  `py_parity`'s all-fallback policy.
- **`-C target-cpu=x86-64-v3`:** every arm held the SSE2 baseline; nothing here.
- **`pulp`:** excluded by D14's budget and unmeasured; it cannot vectorise a
  seqlock loop either, so this restructure would be its prerequisite too.
- **NLERP or any approximation in the kernel:** excluded; it breaks bit-identity
  with `Plan::at`, which is what makes batch a layout rather than a second answer.
- **Only plans with exactly one dynamic step:** the prototype handles every plan
  shape and the 3-step cells win as much. The scope survives only as a bench and
  test fixture. `crates/tf_tree_bench/benches/py_parity.rs` has that shape (one
  dynamic edge, 2000 samples at 1 ms, `at_many_into(Layout::Mat4)`, PHASE3 §12.2
  criterion 2's native denominator) and is all `h == 0` under LerpSlerp: §5's
  all-fallback regime.

## Consequences

- **`at_many` stops being a per-stamp fold.** Under a live writer the gap between
  two steps' reads for one stamp grows to up to one chunk. A batch was never a
  snapshot; bit-identity to `Plan::at` is a property of a quiescent ring and the
  tests assert it only there.
- **On a failing batch, phase 1 does bounded extra work** (reads and updates hint
  cells for stamps after the first failure in earlier steps, up to the chunk's
  end). Results and counters are unchanged; the error and rows written are exactly
  the per-stamp fold's.
- ~~**The stack frame of every batch entry grows**~~ — **it does not (§11.2).** The
  lanes are 4 056–4 088 B and live in `fold_chunked`, one `#[inline(never)]` call
  in, which a batch under three stamps never enters; no `MaybeUninit`, no `unsafe`.
- **If B lands, two arithmetic bodies must stay bit-identical forever.** That is
  D13's shape (`docs/PROJECT.md` §5): `ScLerp::eval` is the reference and the
  kernel the fast version tested against it by proptest. Any edit to
  `screw_parts` or `ScrewParts::pow`'s series arm (and, if a LerpSlerp kernel ever
  re-enters, to `slerp`'s) must be mirrored in the kernel, and the private
  thresholds must reach the classifier by value. D13's proptest, the kernel pin and
  the isolated-stamp engine test hold that, and a mutant run proves they can fail.
- **PHASE3 §12.2 criterion 2's denominator moves.** `py_parity`'s
  `native_mat4_4096` reaches the same `at_many_into_tagged`; A should make it
  faster, and a saving that reaches both sides by the same absolute amount still
  raises the Python-to-native ratio (inferred, not measured). Whichever lands
  retakes that criterion's reading in the same change.
- **No `just` gate sees the batch lever.** `bench-check`'s baseline has no batch
  row and `embed-cost` measures `Plan::at`, not `at_many`; both remain regression
  gates for the scalar path A touches, but neither is evidence for the lever, and
  on this host their scalar rows are UNAVAILABLE (§11.3). Open question 4 asks what
  the gate becomes.
- **`fold_batch`'s doc comment says the cursor logic "must not be duplicated,
  because it is where the galloping search and the seqlock retry live".** The
  prototype duplicated it. A must not.

## Implementation plan

0. **Measure the recorded `/tf` mix, then time the arms on it.**
   - **0a. Classification. DONE, 2026-09-16 — §9.** `bracket_mix.rs`, registered
     in `docs/benchmarks/EVIDENCE.md`, with four controls, every swept stamp
     checked against `Plan::at`, and three sweeps.
   - **0b. Timing, on the uncommitted prototype.** Two arms never built are built
     on the prototype worktree:
     - `copy`: `v1`'s kernel (no select) with `v2`'s monomorphic fix-up. Phase 1
       records each exact hit, the fix-up predicate skips hits, and a pass after
       the kernel copies `a` over the output at each hit;
     - the chunk-level bail-out.

     Then, interleaved:
     1. **Knot handling.** Time `v2` and `copy` on 0a's stream and on the matrix's
        on-grid and off-grid cells, and `copy`'s kernels in §2's probe, hit copy
        included, against the 1.3× bar. Of the arms that clear it, the faster on
        0a's stream is B's knot handling.
     2. **B against A.** Put the bail-out on the kept arm. Time `base`, `scal`, the
        kept arm with and without the bail-out, and the kept arm rebuilt with
        `-C no-vectorize-loops`, on 0a's stream and on `py_parity`'s one-dynamic-step
        all-fallback `at_many_into` rows. The kept arm's LerpSlerp kernel is timed
        as a separate row.
     - **Verified by** the interleaved timings and probe ceilings, per policy,
       recorded in this record.
     - **Stop points:** **B closes here** if its faster arm, with or without the
       bail-out, wins under ~5% against `scal` on 0a's stream, or under ~5% against
       its own `-C no-vectorize-loops` build; this record then says what would
       reopen it. **The LerpSlerp kernel stays out of B** unless `copy`'s clears
       1.3× in the probe and ~5% against `scal` on 0a's LerpSlerp edges.
1. **Attribute A's mechanism, size its chunk, apply A's stop rule. DONE,
   2026-09-17 — §10.** The phase buffering is the lever; A is uniform across entry
   points; 16 lanes beats 64; a batch under three stamps must bypass; A clears its
   floor by 4–7× on the recorded stream. The small-N stop point failed as written at
   both chunk sizes, which is what the bypass is for.
2. **Land A**, as amended by §10: **16 lanes, one fold body, per-stamp fold for a
   batch under three stamps. DONE, 2026-09-18 — §11.** Items, all done:
   - **One read body**: `sample_from` through the bracket read, generic over
     `FromBracket`; `Plan::at`'s check did not move (§11.3 is what it cost).
   - **The bypass is a tested boundary**: `at_many_small/*` rows either side of it,
     and the `const` pin stops the test's lane shapes retargeting silently.
   - **A loom model** with a disabled-check control:
     `read_from_validates_the_bracket_it_hands_back`.
   - **A one-dynamic-step fixture** reproducing §8's engine test:
     `crates/tf_tree/tests/batch_phases.rs`, 6 tests.
   - **`at_many_shapes` bench rows**: one dynamic step, on-grid and off-grid,
     moving and stationary, both policies.
   - **Mutant M5** run, observed failing, reverted; five more beside it (§11.5).
   - **Gates** as §11.5 lists, at `[profile.bench]` **and** `[profile.embedder]`.
   - **Stop point** (embedder profile loses what bench wins): **NOT TRIGGERED**;
     batch rows win 10.4–24.2% there. The scalar `Plan::at` loses (§11.3),
     disclosed with a reopen criterion.
3. **Only if step 0b left B open, land B on A.**
   - **The kernel:** ScLerp only unless step 0b re-admitted LerpSlerp, with step
     0b's knot handling and the chunk bail-out if it kept it.
   - **The kernel pin,** in the crate the kernel lands in (open question 5),
     including endpoint assertions if selects are kept.
   - **A D13 proptest.** D13 (`docs/PROJECT.md` §5) requires the fast version to be
     "tested against it by proptest": generate pose pairs, `s` and chunk fills,
     including chunks that mix safe and unsafe elements, and compare the kernel plus
     fix-up with `ScLerp::eval` by `to_bits`. `proptest` is already a dev-dependency
     of `tf_tree_math` and `tf_tree_core`. The fixed tests stay, because a generated
     distribution cannot be relied on to land on `s == 1.0`, the `1e-290`
     degenerate band or either side of the series threshold.
   - **Every §8 mutant that applies to the kernel as landed** (M2 and M3 only with
     a LerpSlerp kernel, M6 and M6b only with selects), each with its catching
     assertion; survivors argued equivalent.
   - **The per-symbol census** under *Reproduction*, run on the shipped bench
     binary: the kernel symbols' packed arithmetic must collapse under
     `-C no-vectorize-loops`, with zero `%ymm`.
   - **The B-versus-A delta,** on the matrix, the stationary rows, `py_parity` and
     step 0's recorded stream, at both profiles and against B's own
     `-C no-vectorize-loops` build.
   - **Stop point:** B does not land if, at either profile, it wins under ~5%
     against A on step 0's recorded stream or against its own
     `-C no-vectorize-loops` build, or if the census does not collapse.
4. **Re-time on a second architecture.** Run the `ubuntu-24.04-arm` CI hardware, or
   any aarch64 host, for A and, if it landed, B; NEON's lanes and shuffle costs are
   not x86's. **B's claim is x86-64 SSE2 only until this runs.**

## Open questions

1. ~~**Which recorded stream is the mix?**~~ **ANSWERED, 2026-09-16** (principal
   ruling on the owner's delegation; §9): **`testdata/tfstream/indoor_atelier.tfstream`,
   alone.** It is the only real `/tf` recording in the tree, permissively licensed
   (CC BY 4.0), and replays with no network. A bag through PHASE5 §3's MCAP ingest
   would add no *mix* (the only bag is this recording's own source). What
   generalises is §9.2's bound, not §9's percentages.

   **Reopen criterion.** The recording has no edge above 19.8 Hz and none needing
   more than 1.23 Hz to stay in the series region (§9.2), so it is biased toward
   *shrinking* the kernel's regime. **If a second permissively-licensed recording
   with a genuinely high-rate moving edge is added to `testdata/tfstream/`, step 0a
   is re-run on it and B's stop rule re-applied before B closes or lands.**
2. ~~**Chunk size and stack.**~~ **ANSWERED, 2026-09-17 — §10.3 and §10.4:**
   sixteen lanes with a per-stamp bypass below three stamps. **Still open:** 6.6 kB
   is a batch-entry frame five times `base`'s 1 256 B, and whether that is
   acceptable for `no_std` and small-stack embedders and `API.md` §8's real-time
   envelope is not something these rows answer; the caller who cannot pay it (one
   or two stamps) no longer does.
3. ~~**One read body: is the lap check's move acceptable on the scalar path?**~~
   **ANSWERED by step 2 (§11.3, §11.5).** **The lap check does not move on the
   scalar path at all**: `read_from`'s scalar instantiation interpolates *as the
   bracket is built*, so `Plan::at` still checks after the `eval`. It moves only
   for the batch instantiation, which the loom model covers with a failing control.
   **What the question should have asked about was codegen**: one read body is
   ±1.4% on `lookup/*` at `[profile.bench]` and **+6% to +11% at
   `[profile.embedder]`** (§11.3 has the numbers, the reason it lands anyway and
   the reopen criterion). Two read bodies were not taken.
4. **What gates the batch lever?** `bench-check` and `embed-cost` cannot see it.
   Options: add a batch row to `bench_report` (a gated latency row needing
   `Fitness::probe`); script the per-symbol census as a recipe (a structural gate
   that runs on any host); both. **B without one of these ships a vectorisation
   nothing would notice losing.**
5. **Where do the kernels live, and at what visibility?** The prototype put them in
   `tf_tree_math` as a `#[doc(hidden)] pub mod`, which is public surface on a
   published crate; `API.md` §2.7 was not written for a kernel with a "recompute
   what I reject" contract. The alternative is `tf_tree_core`, which needs
   `SIN_HALF_THETA_SMALL_SQ` and `SCREW_DEGENERATE_SQ` exported from
   `tf_tree_math`. **Not decision-affecting for A.**

## Reproduction

**Step 0a's harness and step 2's artefacts are committed**: the fold is
`Plan::fold_batch` / `Plan::fold_chunked`, the read is `SampleRing::read_from`, the
test is `crates/tf_tree/tests/batch_phases.rs`, the model is
`loom_tests::read_from_validates_the_bracket_it_hands_back`, the rows are the
`at_many_shapes` group. §9's tables are
`cargo run --release -p tf_tree_bench --example bracket_mix` on `main`. The
prototype diff, logs and binaries are gone (a host reboot cleared `/tmp`); a reader
who wants to re-run an arm rebuilds it from this section, applied to `681e601`.

```text
# two arms of the same bench harness, differing only in the engine (§11)
git worktree add --detach <dir> <this commit>
cd <dir> && git checkout 2524667 -- crates/tf_tree_core/src/{plan,sample}.rs
cargo bench --bench at_many --no-run                     # and --bench lookup
cargo bench --bench at_many --no-run --profile embedder  # and --bench lookup

# one cell; the driver loops rows x arms x reps and rotates arm order per rep
CRITERION_HOME=<per-arm dir> taskset -c 2 <arm binary> --bench --noplot \
    --warm-up-time 1 --measurement-time 2 --exact at_many/monotone_1024

# §11.2's frames, off the shipped binaries
objdump -d --start-address=<sym> <binary> | grep -m1 'sub .*,%rsp'
nm -C <binary> | grep read_from     # empty iff `read_from` inlined everywhere

# §1–§8's prototype arms, on 681e601
cargo bench -p tf_tree_bench --bench at_many --no-run --features soa-proto         # v1/v2
cargo bench -p tf_tree_bench --bench at_many --no-run --features soa-proto-scalar  # scal
RUSTFLAGS='-C no-vectorize-loops' cargo bench -p tf_tree_bench --bench at_many --no-run --features soa-proto   # v2nlv
```

**The kernels, as measured (`v2`).** Term-for-term copies of the scalar arms they
replace, behind a `soa-proto` feature in `tf_tree_math::interp`, over
structure-of-arrays `Lanes<N>` (seven `[f64; N]` components); `slerp_weight` and
`theta_sq_from_chord` are the crate's existing helpers and `PROTO_*` are
`dualquat.rs`'s two private thresholds re-exported by value. The only thing
removed is control flow.

- **`lerp_slerp_elem`:** `LerpSlerp::eval` past its endpoint shortcuts, `slerp`'s
  series arm unconditionally. `safe = (s != 0) & (s != 1) & (h > 0) &
  (theta_sq >= SLERP_LERP_FALLBACK²) & (theta_sq <= THETA_SLERP_SMALL²)`, with
  `h = 0.5·|qa − qb|²` after the hemisphere flip.
- **`sclerp_elem`:** `a * screw_pow(a⁻¹ b, s)` with `screw_parts`' degenerate
  return removed and `ScrewParts::pow`'s series arm taken. `safe = (s != 0) &
  (s != 1) & (sh2 >= PROTO_SCREW_DEGENERATE_SQ) & (sh2 <=
  PROTO_SIN_HALF_THETA_SMALL_SQ)`; `sclerp_safe` computes the rotation as
  `a.q.conjugate() * b.q`, which is `inv_mul`'s rotation part.
- **`select(c, x, y)`:** bitwise per-component select, exact whatever the operands
  hold. `v2` writes `select(s == 0, a, select(s == 1, b, p))` and ORs
  `s == 0 | s == 1` into the safe mask; `v1` wrote `p` and used `bad |= !safe`.
- **Phase 2 and the fix-up:** the SoA loop returns `true` when no element needs the
  fix-up; otherwise an `#[inline(never)]` monomorphic
  `fixup::<I, _>(safe, a, b, s, n, p)` recomputes every element for which
  `!(safe | s == 0 | s == 1)` with `I::eval`. `scal` replaces the kernel call with
  a plain `I::eval` loop.
- **The chunk loop** (`Plan::fold_batch_soa`): per chunk, reset the accumulator to
  identity, then per step either multiply a static step in or read brackets for
  `chunk[..limit]`, interpolate the lanes, and compose (`inverted` test outside the
  compose loop, as two loops); a read error lowers `limit` to the elements read and
  records the first error; then `note_ok` per element, write the rows, and
  `note_err` on the first error. `read_bracket_from` is `sample_from` under
  `ExtrapPolicy::Error` line for line, with three changes: it returns `(a, b, s)`
  instead of `I::eval(a, b, s)`; an exact hit returns `(p, p, 0.0)`; and the
  trailing `head - i > retained` lap check runs before the deferred `eval`.
- **The isolated-stamp shape the engine test needs** (§8): for every stamp `t`,
  check `[t]`, `[safe_stamp, t]` and `[t, safe_stamp]` against `Plan::at` on a fresh
  `Guard`, every component by `to_bits`.

**The per-symbol census** (`objdump -d -C --no-show-raw-insn BINARY`): per function
whose name matches a pattern, count instructions by mnemonic class (packed
arithmetic `(add|sub|mul|div|sqrt|min|max)pd`, packed compares `cmp\w*pd`,
scalar `…sd`, shuffles, `call`) and `%ymm`/`%zmm` operands, on the as-built binary
and on the `-C no-vectorize-loops` binary. The claim holds only if the packed
arithmetic in the kernel symbols collapses in the second (§3: `at_many` 744 → 139,
`sclerp_soa` 391 → 98, `lerp_slerp_soa` 166 → 37).
