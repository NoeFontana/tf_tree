# 0060: the batch fold that reads before it interpolates

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none. The prototype this record measures was built in a
detached worktree at `681e601`, was never committed and is not coming in as it
stands; *Implementation plan* step 0 is a measurement, not code.

## Context

`Plan::at_many`, `at_many_into` and `at_many_into_f32` evaluate a batch by
folding the **whole plan once per stamp**: for each stamp, for each dynamic step,
one seqlocked ring read (`SampleRing::sample_from`) and one `Interp::eval`,
composed as it goes (`Plan::fold_batch`, and `Plan::at_many`'s own copy of the
same loop). `Plan::fold_batch_with_twist` is the `QuatTwist` sibling and is not
touched by anything here.

[`0016`](./0016-portable-simd-and-the-dependency-budget.md) is `withdrawn`, and
two of its findings are the starting point:

- **Its Amendment §2 names why that loop cannot be vectorised across stamps.**
  Four of the five blockers belong to the engine, not to the arithmetic: the
  `head` atomic loads, `read_slot`'s seqlock and `fence`, the `?` early exit and
  the data-dependent bracket search. The dated note #337 added (*Line numbers,
  2026-09-14*) says to read each blocker by its symbol, because every cited line
  has since moved. The list itself still holds.
- **Its §3 measured a branch-free interpolation loop that does vectorise,
  1.67× on LerpSlerp quaternions alone,** and said it was *"unreachable from where
  the engine stands"*. Getting there means splitting the fold so every bracket is
  read first and interpolated second: *"That restructure is not in this record,
  is not costed by it."* Its Withdrawal then measured `-C target-cpu=x86-64-v3`
  as 8–14% **slower** on `at_many`: wider lanes lose in this fold.

[`fast-path.md`](../design/fast-path.md) §14 **falsified Lever 2**, which split
the fold across *steps* so a plan's *d* dependent load chains could overlap. It
kept SIMD across *stamps* as the half that survives. **The restructure measured
here runs across stamps within one step. It is not Lever 2 reopened.** A chunk of
stamps reads one step's ring, then that step is interpolated, then composed.
The *d* steps still run in plan order and still never overlap.

A skeptic-verified plan asked for this lever to start as a measurement on the
real fold rather than as an implementation. It asked that the stop rule be
stated before any number, and that the gates be named. This record carries that
measurement and an independent re-measurement of it.

**Constraints every arm held, and any implementation must hold.** No `unsafe`
in `tf_tree_math` or `tf_tree_core` outside the files
[`0007`](./0007-the-unsafe-budget-and-the-c-abi.md) permits (the prototype adds
none), no intrinsics, no `-C target-cpu`, no NLERP, no new dependency, D5's
`LerpSlerp` semantics unchanged, `f64` only. **Batch results must be
bit-identical (`to_bits`) to `Plan::at` for the same stamp.**

## The stop rule, stated before the numbers

**A lever that wins under ~5% stops.** 0016 §3 set that floor as its own
measurement's noise, with a single run moving by up to +50% on a shared host.
The rule is applied **per lever, against that lever's own control**:

- the restructure (two-phase fold, scalar `Interp::eval`) against `681e601`;
- the kernel against the restructure. Because the restructure's arm turned out to
  be a weak control on one entry point, the kernel is also checked against the
  same kernel built with `-C no-vectorize-loops`, which isolates what is actually
  SIMD.

## What was measured

### Host and method

- **CPU:** AMD EPYC-Milan (Zen 3), 4 cores / 8 threads with SMT, KVM guest. L1d
  is 32 KiB per core. There is no cpufreq interface, so the governor cannot be
  read.
- **Toolchain:** Linux 6.8.0-138, rustc 1.97.1 (LLVM 22.1.6).
- **Build:** every timed binary is `cargo bench --no-run`, i.e. `[profile.bench]`
  (thin LTO, `codegen-units = 1`), with no `-C target-cpu`. That is the x86-64
  SSE2 baseline: two `f64` lanes. `CARGO_TARGET_DIR` was unset.
- **Pinning:** everything ran under `taskset -c 2`. The SMT sibling, cpu3, was
  not isolated. There are no hardware counters (`perf_event_paranoid=4`, no
  `perf`, no `valgrind`), so **no mechanism claim below rests on a counter.**
- **Date:** all campaigns ran on 2026-09-14, against `681e601` plus the
  prototype.
- **Interleaving:** arms alternate within each cell, and the arm order rotates
  every rep. Deltas are paired per rep.
- **Cell names:** `1dyn` is `odom → map`, one 50 Hz dynamic step. `3dyn` is
  `imu_link → map`, the plan behind the existing `at_many` rows, with three
  dynamic steps. `mono` is the existing stamps, `lo + span·i/n` over the last
  100 ms. `nonmono` is the same stamps permuted by `i → (i·389) mod n`.
  `offgrid` is `mono` shifted by +1 ns, so no stamp lands on a knot.

**The arms:**

| arm | what it is |
|---|---|
| `base` | the `681e601` engine; with the prototype feature off, its batch functions are mnemonic-identical to a pristine `681e601` build |
| `scal` | the two-phase chunked fold, phase 2 calling scalar `Interp::eval`, which **isolates the restructure** |
| `v1` | `scal` with the SoA branch-free kernel in phase 2, plus a scalar fix-up of every element the classifier rejects |
| `v2` | `v1`, but the kernel itself answers `s == 0` and `s == 1` by bitwise select, and the fix-up is monomorphic and not cold |
| `v2nlv` | `v2` rebuilt with `RUSTFLAGS='-C no-vectorize-loops'`; re-measurement only |

The fold, all arms: chunks of up to 64 stamps. **Phase 1**, per dynamic step,
reads each stamp's bracket `(a, b, s)` into 64-lane stack buffers. The read is a
crate-private, line-for-line copy of `sample_from` with the seqlock and the lap
check kept. **Phase 2** interpolates the chunk, then composes. A failing read at
element `i` lowers the chunk's limit to `i`, so the error returned is the one the
per-stamp fold returns. `note_ok` fires per element in order, then `note_err`.
Nothing past `i` is written, which is `at_many_into`'s documented partial-write
contract. `at_many` goes through the same fold. The code is under
*Reproduction*.

### 1. Baseline, before anything changed

A matrix was added to the `at_many` bench (plan × policy × order × N). It was run
against the pristine `681e601` engine, 3 full criterion passes (warm-up 1 s,
measurement 3 s). Figures are ns/stamp, median of the passes.

| cell | ScLerp | LerpSlerp |
|---|---|---|
| 1dyn mono/64 | 92.52 | 71.83 |
| 1dyn nonmono/64 | 82.99 | 61.65 |
| 1dyn mono/1024 | 94.87 | 72.26 |
| 1dyn nonmono/1024 | 83.87 | 62.66 |
| 3dyn mono/64 | 256.61 | 196.03 |
| 3dyn nonmono/64 | 242.27 | 183.38 |
| 3dyn mono/1024 | 263.91 | 201.78 |
| 3dyn nonmono/1024 | 251.26 | 189.50 |

The existing `monotone_1024` row reads 263.00 ns/stamp, which is 269.3 µs per
batch against 0016's 271.9–275.9 µs. So the instrument reproduces.

**A baseline fact, recorded and not investigated:** in every cell the
non-monotone path (`fold_at` plus `sample_hinted`) is **9.5–14.3 ns/stamp
faster** than the monotone path, which calls the out-of-line `fold_at_cursors`.
The cursor exists to make the monotone path the fast one.

### 2. The kernel's ceiling, isolated

`crates/tf_tree_bench/examples/autovec_probe.rs` was extended with four
full-pose variants. Each one runs a kernel plus its safe-region classifier over
1024 pose pairs one 200 Hz tick apart, chunked by 64 the way the engine chunks.
All four are `to_bits`-identical to `eval` on every element, and every element
classifies safe:

| variant | policy | layout |
|---|---|---|
| E | LerpSlerp | array-of-structs |
| F | LerpSlerp | structure-of-arrays |
| G | ScLerp | array-of-structs |
| H | ScLerp | structure-of-arrays |

The method: 15 interleaved round-robin reps inside each process, ~120 ms per
sample. Three builds (as built, `-C no-vectorize-loops`, `-C no-vectorize-slp`)
were interleaved over 5 rotations. Figures are ns/element, the median of the 5
run-medians.

| variant | as built | no loop vectoriser | no SLP |
|---|---|---|---|
| A `LerpSlerp::eval` in a loop | 17.993 | 18.019 | 19.211 |
| A' `ScLerp::eval` in a loop | 47.840 | 47.947 | 46.600 |
| F LerpSlerp SoA kernel | **10.732** | 18.717 | 10.713 |
| H ScLerp SoA kernel | **23.833** | 47.737 | 23.841 |
| E LerpSlerp AoS kernel | 11.867 | 16.959 | 11.858 |
| G ScLerp AoS kernel | 25.195 | 45.526 | 25.305 |

- **Ceiling, paired per run:** A/F **1.676×**, A'/H **2.008×**. Both clear the
  1.3× bar the plan set before an engine prototype was worth building.
- **Nearly all of that is the loop vectoriser.** Without it, A/F is 0.962× and
  A'/H is 1.004×. Removing the branches and inlining buys nothing on its own in
  the SoA shape.
- **SLP is irrelevant to the kernels:** 0.997–1.005×.
- **0016's variants reproduce.** Without the loop vectoriser, B is 1.459×
  slower and C 2.019× (0016 recorded 1.49× and 1.89×). D is 19.314 ns and does
  not vectorise at all.
- **v2's endpoint selects cost ceiling**, in a separate 5-rotation run: F is
  14.168 ns, so A/F drops to **1.268×**, below the 1.3× bar for LerpSlerp. H is
  26.579 ns, so A'/H drops to **1.796×**. Compared across the two runs, the
  selects cost ~3.4 ns (F) and ~2.8 ns (H) per element on data with no
  endpoints. The loop vectoriser is still worth 1.102× on F and 1.687× on H.

### 3. Evidence the widening is across stamps

**0016's `%ymm` guard checks nothing here.** At the SSE2 baseline the engine
has no `ymm` at all, and SLP already packs within one `eval`. So the census
counts, **per symbol**, packed `f64` arithmetic (`addpd`, `subpd`, `mulpd`,
`divpd`, `sqrtpd`, `minpd`, `maxpd`) against its `-sd` scalar forms. Only the
named function bodies are counted, so blake3's runtime-dispatched AVX2/AVX-512
kernels cannot pollute the result. The criterion that makes the census
non-vacuous is that **the count must collapse under `-C no-vectorize-loops`**.

Counts are packed / scalar arithmetic:

| symbol | as built | no loop vectoriser | no SLP |
|---|---|---|---|
| `variant_f` (LerpSlerp SoA) | 129 / 0 | 0 / 129 | 129 / 0 |
| `variant_h` (ScLerp SoA) | 293 / 0 | 3 / 287 | 293 / 0 |
| `ScLerp::eval`, out of line | 144 / 179 | 144 / 179 | 0 / 423 |
| `LerpSlerp::eval`, out of line | 47 / 81 | 47 / 81 | 0 / 171 |

In the engine bench binaries, packed arithmetic / packed compares:

| symbol | `base` | `scal` | `v1` | `v2` | `v2nlv` |
|---|---|---|---|---|---|
| `Plan::at_many::<SystemDomain>` | 310 / 0 | 322 / 0 | 612 / 11 | 744 / 15 | 139 / 0 |
| `sclerp_soa::<64>`, out of line | — | — | 296 | 391 | 98 |
| `lerp_slerp_soa::<64>`, out of line | — | — | 129 | 166 | 37 |

- **There is no `%ymm`/`%zmm` in any counted symbol.**
- **Stack frames** (the prologue's reservation):

  | function | `681e601` | prototype |
  |---|---|---|
  | `at_many` | 1256 B | 15 944 B |
  | `at_many_into_tagged` | 888 B | 16 040 B |
  | `at_many_into_f32_tagged` | 344 B | 15 960 B |

  The `scal` arm reserves 16 040–16 168 B. The five zero-initialised 64-lane
  buffers account for 14 848 B of that.
- **`base` is the real `681e601`.** With the feature off, the mnemonic sequences
  of `at_many`, `at_many_into_tagged`, `at_many_into_f32_tagged`, `fold_at` and
  `fold_at_cursors` are identical to a pristine `681e601` build.

### 4. End to end

The final campaign: 29 cells × 4 arms (`base`, `v1`, `v2`, `scal`) × 5 reps, i.e.
580 criterion runs (warm-up 1 s, measurement 2 s, `--exact` per cell). Across all
of them, criterion's 95% CI half-width had a median of 0.08% and a maximum of
1.93%. Figures are ns/stamp medians over reps, and deltas are paired per rep.

| cell (N = 1024 unless marked) | base | scal | v1 | v2 | v2 vs base | scal vs base | v2 vs scal |
|---|---|---|---|---|---|---|---|
| 3dyn ScLerp mono (**flagship**) | 261.53 | 192.35 | 127.88 | 124.88 | **−52.2%** | −26.4% | −35.2% |
| 3dyn ScLerp nonmono | 250.04 | 203.92 | 141.90 | 138.30 | −44.6% | −18.5% | −32.1% |
| 3dyn ScLerp mono/64 | 255.31 | 189.06 | 189.67 | 127.38 | −50.1% | −25.9% | −32.7% |
| 1dyn ScLerp mono | 92.41 | 65.90 | 41.10 | 43.05 | −53.3% | −28.8% | −34.5% |
| 1dyn ScLerp nonmono/64 | 81.73 | 68.30 | 67.12 | 46.57 | −43.2% | −16.8% | −31.8% |
| 1dyn LerpSlerp mono | 72.25 | 36.11 | 28.18 | 30.61 | −57.5% | −49.7% | −15.4% |
| 3dyn LerpSlerp mono | 199.97 | 104.23 | 91.26 | 87.07 | −56.5% | −48.0% | −16.3% |
| 3dyn LerpSlerp nonmono/64 | 180.58 | 109.52 | 170.69 | 96.54 | −46.6% | −39.2% | −12.1% |
| `into_mat4_1024` (3dyn ScLerp mono) | 262.70 | 243.82 | 128.43 | 122.36 | −53.4% | −6.9% | −49.9% |
| `into_quat_twist_1024` (**negative control**) | 328.63 | 325.23 | 326.51 | 327.00 | −0.6% | −1.0% | +0.5% |

Across all 24 matrix cells (both policies, 1dyn/3dyn, mono/nonmono/offgrid, N 64
and 1024):

- **`v2` against `base`:** −43.2% to −57.9%.
- **`scal` against `base`:** −16.8% to −50.1%. **`scal` is faster than `base` in
  every cell whose fold it touches, the stationary ones included.**
- **`v2` against `scal`:** −12.1% to −35.2%. ScLerp cells read −31.0% to −35.2%;
  LerpSlerp cells read −12.1% to −16.8%.
- **The restructure's share of `v2`'s saving** over `base`, computed as
  (base − scal) / (base − v2): **38–54% under ScLerp**, with non-monotone cells
  the low end (38–42%), and **82–87% under LerpSlerp**. On `into_mat4` it is
  13%, and §7 withdraws that figure's attribution.

**The negative control is flat.** `into_quat_twist_1024` goes through a fold no
arm touches, and every pairwise delta has a median inside ±1.2% and a range
inside [−3.0, +2.7]%. So **build-level noise here is about ±1–3%**, and a cell
delta under ~3% is not read as an effect.

**Stop-rule reading of this campaign.** The restructure clears 5% in every
matrix cell, by 3.4× at its weakest (−16.8%). **On `into_mat4` it only just
clears it:** −6.9% here and −6.2% in §7's re-measurement, close enough to the
±1–3% noise band that this entry point alone would not carry it. The kernel
clears 5% against `scal` in every series-region cell, by 2.4× at its weakest
(−12.1%).

### 5. Where it loses: stationary edges

One pose was published every 5 ms, and the bench queries 1024 monotone stamps.
**Under the prototype classifier every element falls outside the safe region.**
For LerpSlerp, identical quaternions give `h == 0`. For ScLerp, `conj(q)·q` has a
vector part that is exactly zero for the benchmarked generic rotation, so
`sh2 = 0`, which is below `SCREW_DEGENERATE_SQ`. That was measured, not argued
(`conj(q)*q = (1.0000000000000002, 0, 0, 0)`, and `sclerp_safe = false` at
s = 0.25, 0.5 and 0.9). The prototype bench's own comment had predicted the
series arm for a generic constant rotation, and it was wrong.

| cell | base | scal | v1 | v2 | v2 vs base | v2 vs scal |
|---|---|---|---|---|---|---|
| LerpSlerp generic | 38.77 | 21.77 | 60.54 | 39.23 | +0.9% | **+80.0%** |
| ScLerp generic | 107.33 | 53.08 | 130.64 | 113.96 | +5.9% | **+115.5%** |
| ScLerp identity | 108.01 | 53.09 | 130.61 | 113.62 | +5.7% | **+115.2%** |

**On the only non-series data measured, the kernel turns the restructure's ~2×
win into a loss against `681e601`.** The restructure alone is −43.8% to −50.8%
there.

### 6. Knots: why `v1` and `v2` differ

Every on-grid 64-stamp batch starts on a knot (`s == 0`). Under `v1`, a knot
sends the whole chunk through a `#[cold]` fix-up that takes its predicate by
function pointer.

**That hypothesis was measured with the off-grid arm, not inferred:**

- **ScLerp 1dyn:** `v1` reads 41.95 ns off-grid against 66.59 on-grid at N = 64.
  Against `scal` it is −38.3% off-grid and −1.4% on-grid.
- **LerpSlerp 1dyn/64:** `v1` against `scal` is **+49.4%** on-grid and −24.8%
  off-grid.
- **All LerpSlerp mono/nonmono N = 64 cells:** `v1` is +49.4% to +56.0% against
  `scal`.

**`v2` removes the penalty,** running 31.0–45.2% faster than `v1` on every
on-grid (mono and nonmono) N = 64 cell, and pays for it elsewhere:

| cells | `v2` vs `v1` |
|---|---|
| off-grid (no knots) | +8.6% to +16.3% |
| 1dyn mono/nonmono, N = 1024 | +3.4% to +8.6% |
| 3dyn mono/nonmono, N = 1024 | −2.3% to −4.8% |

On-grid minus off-grid under `v1` at N = 64 is 1.5–1.8 µs per chunk per
dynamic step, derived from the table and assuming every step's first stamp is a
knot. The off-grid arm attributes that cost to knots, and nothing attributes it
further. **A cheaper design was not built:** copy phase 1's recorded exact
hits over the kernel's output afterwards, with no select in the loop.

### 7. Independent re-measurement

A second agent rebuilt every arm from the worktree and added **`v2nlv`**. It ran
8 cells × 4 arms (`base`, `v2`, `scal`, `v2nlv`) × 6 reps, i.e. 192 criterion
runs (warm-up 1 s, measurement 3 s, `--exact`), interleaved and rotated under
`taskset -c 2`. **It measured per-run `/proc/stat` busy time on every other CPU,
the SMT sibling included, and the maximum was 2%.**

| cell (N = 1024) | base | scal | v2nlv | v2 | v2 vs base | v2 vs scal | v2nlv vs scal | loop-vectoriser share of saving |
|---|---|---|---|---|---|---|---|---|
| 1dyn ScLerp mono | 92.79 | 65.69 | 65.03 | 43.05 | −53.5% | −34.4% | −1.0% | 44% |
| 1dyn LerpSlerp mono | 72.04 | 36.06 | 35.85 | 30.49 | −57.9% | −15.5% | −0.4% | 13% |
| 3dyn ScLerp mono | 261.75 | 192.47 | 189.19 | 125.15 | −52.3% | −34.9% | −1.6% | 47% |
| 3dyn LerpSlerp mono | 199.73 | 104.13 | 101.04 | 86.89 | −56.6% | −16.3% | −2.6% | 13% |
| 3dyn ScLerp nonmono | 249.56 | 205.47 | 202.98 | 138.14 | −44.6% | −32.6% | −1.2% | 58% |
| `into_mat4_1024` | 261.49 | 245.53 | 186.35 | 122.22 | −53.1% | −50.0% | **−23.8%** | 46% |
| stationary ScLerp generic | 108.66 | 52.84 | 135.60 | 114.05 | +5.0% | +115.3% | +155.9% | — |
| `into_quat_twist_1024` | 325.22 | 325.44 | 325.65 | 325.20 | −0.0% | −0.2% | −0.1% | — |

The last column is (v2nlv − v2) / (base − v2): the part of the total saving that
disappears when only the loop vectoriser is switched off.

**Every headline figure reproduces within ~1%.** The re-measurement agreed with
the verdict. It corrected the first campaign in four places, and **where the two
disagree, this record takes the conservative reading**:

1. **On `at_many`, the kernel's gain over `scal` is entirely the loop
   vectoriser.** `v2nlv` sits within 0.4–2.6% of `scal`, which confirms §2's
   probe on the real engine, where the first campaign had never run that control.
2. **On `into_mat4`, "87% kernel, −49.9%" is withdrawn.** `v2nlv` is still
   −23.8% against `scal` there, so about half of the "kernel alone" gain is not
   SIMD. `scal` is a weak control on that entry point:
   - its `at_many_into_tagged` inlines scalar `eval` and grows to 6 851
     instructions, against 2 543 at `base`;
   - it reads 245.5 ns/stamp on `into_mat4` against 192.5 on `at_many`, for the
     same plan and stamps.

   The SIMD figure for `into_mat4` is `v2` against `v2nlv`, **−34.2%**.
3. **The restructure dominates more than the first report weighted it.**
   Measured by the loop vectoriser, SIMD is **12.5–58.2% of the saving** in the
   six series-region cells, and 13% on LerpSlerp.
4. **The first campaign's host-state instrument could not support its claim.**
   It took `%CPU` from `ps -eo pcpu`, a per-process lifetime average that hides
   bursts, so its "0 busy samples" is not evidence of a quiet host. What
   supports the first campaign's figures instead is that a second campaign, on a
   host whose `/proc/stat` deltas were measured quiet, reproduces them within
   ~1%.

One process note. The first campaign's saved `base` and `scal` binaries differ by
sha256 from fresh rebuilds, most likely because bench cells were added after
those binaries were built. The five engine batch functions are mnemonic-identical
between the two builds, so no figure moves. **Rebuild rather than reuse the
saved binaries.**

### 8. Bit identity and mutants

**Bit identity: PASS, at three levels, in every arm.**

1. **Kernel pin** (`tf_tree_math`, release and debug).
   - Every element the classifier calls safe matches `LerpSlerp::eval` /
     `ScLerp::eval` by `to_bits`. The test asserts more than 8 000 safe elements
     per policy.
   - Every out-of-region band classifies unsafe: large arc, the LERP-fallback
     band, `h == 0`, endpoints including signed zeros, the ScLerp degenerate band
     straddling `1e-290`, and a NaN rotation.
   - Under `v2`, the SoA output is exactly `a` at `s == 0` and exactly `b` at
     `s == 1`.
2. **Engine test** (`crates/tf_tree/tests/soa_proto.rs`, 3 tests). It compares
   `at_many`, `at_many_into` (Quat and Mat4) and `at_many_into_f32` (Affine32)
   against `Plan::at` on a fresh `Guard`.
   - **Inputs:** crafted branch regions repeated 4×, plus 200 random series
     steps, for 2 350 stamps per plan. The crafted regions are knots; `t ==
     t_new`; `s` rounding to exactly 1.0 over a 2⁶⁰ ns segment; identical
     rotations; LERP fallback at `1e-7` rad; 0.2998 and 0.3002 rad either side of
     the series threshold; large arc; far hemisphere; exact identity; signed
     zeros; NaN and ±inf translations.
   - **Plans and policies:** ScLerp, LerpSlerp and a mixed plan. One dynamic
     step, and dyn/static/dyn in both directions, so both `inverted` flags are
     exercised. Identity and all-static plans.
   - **Batches:** the full batch; slices of 1, 2, 63, 64, 65 and 129 at three
     offsets; **every stamp alone, and paired with a known-safe stamp in lane 0
     and in lane 1**.
   - **Error contract:** 5 lengths × 6 positions × 3 failure kinds. It asserts
     the error, the rows before it, a sentinel in every row from it on, and the
     per-edge counters.
   - Passes with the feature off, on `v1`, `v2` and `scal`, in release and
     debug.
3. **Benchmarked data.** All 16 128 rows of the 27 benchmarked matrix, off-grid
   and stationary cells are bit-identical to `Plan::at`, in `base`, `v1`, `v2` and
   `scal`. **No speedup here is fast because it is wrong.**

**Mutants.** Each was applied with an exact-count replacement, the test was run,
the file was restored by copy plus `utime`, and the sha256 was checked. The
outcomes come from the logs.

| mutant | crafted-only fixture | + random series | + isolated stamps (`v1` code) | `v2` code |
|---|---|---|---|---|
| M1 ScLerp kernel 1-ulp reassociation | **survived** | caught | caught | caught |
| M1b gross ScLerp kernel error | caught | caught | caught | caught |
| M2 LerpSlerp translation as `a + (b − a)s` | caught | caught | caught | caught |
| M3 LerpSlerp classifier admits the fallback band | caught | **survived** | caught | caught |
| M4 ScLerp classifier admits `s == 1.0` | **survived** | **survived** | caught | survives: equivalent, `v2`'s select overrides it |
| M5 an error does not lower the chunk limit | caught | caught | caught | caught |
| M6 `v2`'s `s == 0` select takes `b` | — | — | — | survives the engine test; **caught by the kernel pin** |
| M6b `v2` kernel drops its `s == 1` select | — | — | — | caught |

**The test-design finding matters more than any single row.** A
kernel-classifier defect is **masked whenever another unsafe element shares its
chunk**, because the fix-up re-derives the predicate from a separate function.
The first two fixture versions missed two real mutants for that reason. Only
per-stamp isolated batches, in both lanes, caught them.

M6 is unobservable through the public API: phase 1 stores every exact hit as
`(p, p, 0.0)`, so `a == b` there. The re-measurement added two more mutants, and
no log of them was kept:

- **Dropping `h > 0.0` from both LerpSlerp predicates survives,** and is
  equivalent: `h == 0` forces `theta_sq == 0`, which already fails the fallback
  bound.
- **Dropping ScLerp's `sh2` upper bound is caught,** by the isolated-stamp
  shapes.

### MEASURED and INFERRED

| claim | standing |
|---|---|
| the restructure wins 17–50% on `at_many` and 7% on `into_mat4`, synthetic data, this host | MEASURED |
| the restructure wins 44–51% on stationary data | MEASURED (3 cells) |
| the kernel's gain over the restructure on `at_many` is the loop vectoriser | MEASURED (`v2nlv`, 5 cells) |
| the kernel loses 80–115% to the restructure on all-fallback data | MEASURED |
| batch output is bit-identical to `Plan::at` in every arm | MEASURED (three levels, mutants) |
| **why** the restructure is faster (per-chunk sampler and policy resolution hoisted, the out-of-line `fold_at_cursors` call gone, loop layout) | INFERRED: hypotheses, none isolated |
| why `scal` is weak on `into_mat4` (scalar `eval` inlined into a 6 851-instruction `at_many_into_tagged`) | INFERRED from the census, not isolated |
| the real `/tf` mix of series, stationary, large-arc and knot segments | **UNMEASURED**, and it decides the kernel's sign |
| a rough break-even for `v2` over `scal`: ~27% of elements falling back under ScLerp, ~24% under LerpSlerp | INFERRED, see below |
| the 16 kB frame's per-call cost (zeroing 14.8 kB) at small N | UNMEASURED; N < 64 was not benchmarked |
| aarch64/NEON, `[profile.embedder]` (`lto = false`, `codegen-units = 16`), AVX hosts | UNMEASURED |
| `read_bracket_from`'s earlier lap check is as sound as `sample_from`'s | ARGUED (below) and audited by the re-measurement; **no loom model** |

**The break-even line is a linear interpolation between two unlike datasets, and
must not be used as a threshold.** On 1dyn ScLerp mono/1024, `v2` saves 22.85
ns/stamp against `scal` on series data. On stationary data it costs 60.88. If the
cost mixed linearly per element, `v2` would win only while the fallback fraction
stays under 22.85 / (22.85 + 60.88) ≈ 27%. The same arithmetic on LerpSlerp (5.50
saved, 17.46 lost) gives ≈ 24%. The mixing is not linear: fix-up runs per chunk
and re-classifies all 64 elements, and a bail-out would change the shape. So
this only says **the fraction that matters is tens of percent, not a tail.**

**Why the moved lap check is not a weaker check.** `sample_from` reads its slots
with `read_slot`'s seqlock, calls `eval` on the copies, then re-loads `head` and
refuses if the bracket was lapped. `read_bracket_from` makes the same re-load and
the same comparison over the same indices, right after the slot reads, and defers
`eval`. `eval` works on copies, so nothing it reads can be recycled in between.
The check therefore still runs after the last shared-memory read it protects.
`head` is monotone, so the earlier check can only refuse **less often** than the
later one, **and only for reads that were already complete and validated.** That
is an argument, and 0016 and `fast-path.md` §14 are why this record does not
accept an argument in place of a model. The three `loom_tests.rs` lap models
(`sample_from_with_a_stale_cursor_across_a_lap`,
`sample_from_hold_revalidates_across_a_lap`,
`sample_from_exact_newest_revalidates_across_a_lap`) do not cover the copy.

## Decision (proposed, not taken)

**Verdict: PROCEED, to two separable decisions taken in order, not to the
prototype as built.** The two campaigns agree on the verdict. Where their
attributions differ, the conservative reading above is the one carried forward.

1. **Decision A — the restructure, with no kernel.** Rewrite `Plan::fold_batch`,
   and `at_many`'s loop through it, as a chunked two-phase fold. Per chunk and per
   step, phase 1 reads every bracket through **the one** seqlocked read path and
   phase 2 calls the scalar `Interp::eval`. It is proposed first because:
   - it is the only lever that wins in **every** cell whose fold it touches,
     stationary data included (−6.9% to −50.8%), though `into_mat4`'s −6.9% is
     barely past the rule;
   - it needs no second body of any arithmetic;
   - it is the prerequisite for Decision B whatever B becomes.

   Before it lands, its mechanism is measured (step 1). Its chunk size and stack
   frame are decided (open question 2). Its read path is **one body**:
   `sample_from` becomes the bracket read plus `eval`, not a copy beside it
   (open question 3).
2. **Decision B — the SoA kernel, on top of A, only if step 0 says the data
   wants it.** It applies only to chunks the classifier admits, with a
   chunk-level bail-out: classify first, and skip the kernel when the chunk is
   mostly fallback. Its knot handling is chosen by measurement between `v2`'s
   selects and a post-kernel copy of phase 1's exact hits.

**The stop rule binds each separately.** A is abandoned if, after step 1, it
wins under ~5% against `681e601` on the recorded mix. B is abandoned if it wins
under ~5% against **A**, measured on the recorded mix. The SIMD part of that
claim must hold against a `-C no-vectorize-loops` build of B, not only against A.
**B is also abandoned if it loses to A by more than noise on the mix,** because
its synthetic win does not transfer.

## Rationale

- **Adopt `v2` as prototyped.** Its headline (−52% flagship) was measured on
  synthetic data that keeps nearly every element in the series region. On the only
  other data shape measured, it is slower than `681e601` and more than 2× slower
  than A. Its kernels are term-for-term second bodies of `slerp`'s series arm and
  of `screw_parts` / `ScrewParts::pow` / `inv_mul`. The only thing keeping them in
  step with the scalar arms is a bit-identity test. Adopting that maintenance
  cost before knowing which data it serves is backwards.
- **Adopt A and B together.** They were measured as separable, and A carries
  82–87% of the saving under LerpSlerp and 38–54% under ScLerp. Bundling them
  would let B's unmeasured sign ride in on A's measured one.
- **Stop.** The stop rule does not permit it. A clears ~5% by 3.4× in its
  weakest matrix cell (and by only 1.4× on `into_mat4`). B clears it against A by
  2.4× in the series region, and the negative control is flat.
- **`-C target-cpu=x86-64-v3`, or `pulp`.** Excluded by the constraints, and
  measured slower by 0016. This record changes nothing about either.
- **NLERP or any other approximation in the kernel.** Excluded. It breaks
  bit-identity with `Plan::at`, and bit-identity is what makes batch a layout
  rather than a second answer.
- **Only plans with exactly one dynamic step.** The audited plan first scoped
  it that way. The prototype handles every plan shape, both inversion
  flags and static steps, and the 3-step cells win as much as the 1-step ones.
  **The one-dynamic-step scope survives only as a bench and test fixture,
  because nothing existing reaches that shape:**
  - `batch.rs`'s `Chain` has two dynamic edges;
  - every existing `at_many` row uses the 3-step plan.

## Consequences

- **If A lands, `at_many` stops being a per-stamp fold.** Under a live writer the
  gap between two steps' reads for one stamp grows from one fold to up to one
  chunk. A batch was never a snapshot and still is not. Bit-identity to
  `Plan::at` is a property of a quiescent ring, and the tests assert it only
  there.
- **On a failing batch, phase 1 does bounded extra work.** It reads, and updates
  guard hint cells for, stamps after the first failure in earlier steps, up to the
  end of the chunk. Results and counters are unchanged, because hints never
  change a result. The error returned and the rows written are exactly the
  per-stamp fold's.
- **The stack frame of every batch entry grows** from 344–1 256 B to ~16 kB at a
  64-stamp chunk. That matters to `no_std` and small-stack embedders, and to
  `API.md` §8.3's page-fault residual, since a first call can touch up to ~4
  more stack pages (inferred from the frame size, not measured). It cannot be
  avoided without `MaybeUninit`, which safe Rust in `tf_tree_core` forbids. It
  can be shrunk by shrinking the chunk (open question 2).
- **If B lands, two arithmetic bodies must stay bit-identical forever.** Any
  edit to `slerp`'s series arm or to `ScrewParts::pow`'s must be mirrored in the
  kernel, and the private thresholds must reach the classifier by value. The
  kernel pin and the isolated-stamp engine test are what hold that, and a mutant
  run is what proves they still can fail.
- **Neither decision has a gate in `just` today that can see it.**
  - `just bench-check`'s baseline has **no batch row**: its ids are latency,
    memory, attach, scaling and differential rows.
  - `just embed-cost` measures `Plan::at` across the crate boundary, not
    `at_many`.

  Both remain regression gates for what A could break on the scalar path, since
  A touches `sample_from`. **Neither is evidence for the lever,** and a toolchain
  that stopped vectorising B's loop would pass both. Open question 4 asks what the
  gate becomes.
- **`fold_batch`'s doc comment says the cursor logic "must not be duplicated,
  because it is where the galloping search and the seqlock retry live".** The
  prototype duplicated it. A must not.

## Implementation plan

0. **Measure the recorded `/tf` mix. No engine code.** For each dynamic edge of a
   recorded stream (PHASE5 §3's MCAP ingest, or the soak's `robot` workload),
   classify every bracket an `at_many` sweep would read into five classes: series
   region, stationary / `h == 0` / degenerate, LERP fallback, large arc, and exact
   hit.
   - Report the fractions per policy, per edge and per 64-stamp chunk. The
     chunk-level fraction is what a bail-out sees.
   - **Positive controls:** the matrix fixture must read ~100% series, and the
     stationary fixture 100% fallback. Without both, a classifier reading
     everything as series passes.
   - Time A, `v2`, and `v2` with a bail-out on that stream, interleaved.
   - **Verified by** the fraction table, the two controls, and the interleaved
     timings recorded in this record.
   - **Stop point:** if B loses to A on the recorded mix, B is closed here and
     this record says what would reopen it.
1. **Attribute A's mechanism before adopting it.** Build arms that separately:
   - hoist `view.sampler(edge)` and the policy dispatch per chunk into
     `681e601`'s fused loop;
   - inline, or remove the call to, `fold_at_cursors`;
   - change only the loop order.

   Also explain `scal`'s weakness on `into_mat4`, and the baseline's
   monotone-slower-than-non-monotone inversion (§1).
   - **Verified by** each arm's interleaved delta against `base` and against
     `scal`, with the flat `into_quat_twist_1024` control.
   - **Stop point:** if one sub-change carries A's saving without phase
     buffering, land that and not A.
2. **Land A.**
   - **One read body.** `sample_from` expressed through the bracket read, so the
     scalar path runs the same lap check in the same position.
   - **A loom model** for that read, mirroring the three `sample_from_*` lap
     models. It must carry a disabled-check control that fails, and run at
     `LOOM_MAX_PREEMPTIONS >= 3`.
   - **A one-dynamic-step fixture** in `crates/tf_tree/tests/` under both
     policies. It must reproduce §8's engine test in full: branch regions,
     isolated stamps in lanes 0 and 1, the error-contract grid, counters.
   - **A one-dynamic-step `at_many` bench row,** plus the off-grid and stationary
     rows.
   - **A run mutant:** M5 (an error does not lower the chunk limit) applied,
     observed failing, and reverted with a `touch`.
   - **Verified by** `just test`, `just shm-check`, `just loom`, `just miri`,
     `just tsan`, `just lint`, `just doc`, `just bench-check` and
     `just embed-cost-check`. Also by the new rows timed interleaved against the
     parent commit, at `[profile.bench]` **and** `[profile.embedder]`.
   - **Stop point:** if the embedder profile loses what the bench profile wins,
     A does not land as measured.
3. **Only if step 0 left B open, land B on A.**
   - **The kernel pin** in `tf_tree_math`, including `v2`-style endpoint
     assertions if selects are kept.
   - **All of §8's mutants plus M6 and M6b,** each run and recorded with its
     catching assertion. Survivors must be argued equivalent.
   - **The chunk bail-out.**
   - **The per-symbol census** under *Reproduction*, run on the shipped bench
     binary: the kernel symbols' packed arithmetic must collapse under
     `-C no-vectorize-loops`, with zero `%ymm`.
   - **The B-versus-A delta,** on the matrix, the stationary rows and step 0's
     recorded stream, at both profiles and against `v2nlv`.
   - **Verified by** everything step 2 names, plus that census and those deltas.
4. **Re-time on a second architecture.** Run the `ubuntu-24.04-arm` CI hardware,
   or any aarch64 host, for A and, if it landed, B. NEON's lanes and shuffle costs
   are not x86's. Verified by the same interleaved rows. **B's claim is x86-64
   SSE2 only until this runs.**

## Open questions

1. **Which recorded stream is the mix?** The MCAP corpus, the soak's `robot`
   workload, or both. Step 0 cannot start without an answer, and it decides B.
2. **Chunk size and stack.** 64 lanes cost ~16 kB of frame. 16 lanes would cost
   ~4 kB and give the loop vectoriser a shorter trip count. Nobody has measured
   the trade, or N < 64 at all. Is a batch-entry frame of that size acceptable
   for `no_std` and small-stack embedders, and for the real-time envelope
   `API.md` §8 states?
3. **One read body: is the lap check's move acceptable on the scalar path?**
   Decision A requires `sample_from` to become "read the bracket, then `eval`",
   which moves `Plan::at`'s lap check before `eval` too. The argument is above
   and the loom model is step 2. But `Plan::at` is `API.md` §2.3's measured hot
   path, and re-spelling it may move its codegen. The alternative keeps two read
   bodies, and `fold_batch`'s own doc comment refuses that.
4. **What gates the batch lever?** `bench-check` and `embed-cost` cannot see it.
   Options:
   - add a batch row to `bench_report`, which is a gated latency row and needs
     `Fitness::probe`;
   - script the per-symbol census as a recipe (a structural gate that runs on
     any host);
   - both.

   **B without one of these ships a vectorisation nothing would notice losing.**
5. **Where do the kernels live, and at what visibility?** The prototype put them
   in `tf_tree_math` as a `#[doc(hidden)] pub mod`. `tf_tree_math` publishes, so
   that is public surface. `API.md` §2.7 makes a kernel the engine runs public on
   its own terms, and a batch kernel with a *"recompute what I reject"* contract
   is not a shape §2.7 was written for. The alternative is to move the kernel into
   `tf_tree_core`, which needs `SIN_HALF_THETA_SMALL_SQ` and
   `SCREW_DEGENERATE_SQ` exported from `tf_tree_math`. **Not decision-affecting
   for A.**

## Reproduction

**Nothing below is added to the repository as a file.** The prototype diff,
test, logs and binaries stayed in the session's scratch directory (see the
bullet list below). What a reader can rerun is the code and commands below,
applied to `681e601`.

**Commands:**

```text
# worktree, CARGO_TARGET_DIR unset
git worktree add --detach <dir> 681e601

# arms (each binary copied aside after building)
cargo bench -p tf_tree_bench --bench at_many --no-run                              # base
cargo bench -p tf_tree_bench --bench at_many --no-run --features soa-proto         # v1; v2 is the same build after the v2 kernel edit
cargo bench -p tf_tree_bench --bench at_many --no-run --features soa-proto-scalar  # scal
RUSTFLAGS='-C no-vectorize-loops' \
cargo bench -p tf_tree_bench --bench at_many --no-run --features soa-proto         # v2nlv

# one cell of one rep; the driver loops cells x reps and rotates arm order per rep
CRITERION_HOME=<per-arm dir> taskset -c 2 <arm binary> --bench --noplot \
    --warm-up-time 1 --measurement-time 2 --exact at_many_matrix/3dyn/sclerp/mono/1024

# probe, three builds, interleaved over 5 rotations
PROBE_BUILD=normal cargo build --release -p tf_tree_bench --example autovec_probe --features soa-proto
RUSTFLAGS='-C no-vectorize-loops' cargo build --release -p tf_tree_bench --example autovec_probe --features soa-proto
RUSTFLAGS='-C no-vectorize-slp'   cargo build --release -p tf_tree_bench --example autovec_probe --features soa-proto

# bit identity and mutants
cargo test --release -p tf_tree --features soa-proto,unstable --test soa_proto   # also without soa-proto, with soa-proto-scalar, and in debug
cargo test --release -p tf_tree_math --features soa-proto --lib batch_proto
cargo nextest run --release -p tf_tree --features soa-proto,unstable
```

**The kernels, as measured (`v2`).** They sit behind a `soa-proto` feature in
`tf_tree_math::interp`. `slerp_weight` and `theta_sq_from_chord` are the crate's
existing private helpers. `PROTO_*` are `dualquat.rs`'s two private thresholds,
re-exported by value under the feature. Every expression is a term-for-term copy
of the scalar arm it replaces, so the only thing removed is control flow.

```rust
pub const CHUNK: usize = 64;
pub struct Lanes<const N: usize> { pub qw: [f64; N], pub qx: [f64; N], pub qy: [f64; N],
    pub qz: [f64; N], pub tx: [f64; N], pub ty: [f64; N], pub tz: [f64; N] }
// Lanes::get(i) -> Iso3 and Lanes::set(i, &Iso3) copy the seven components.

/// Bitwise per-component select; exact whatever the operands hold.
#[inline(always)]
pub fn select(c: bool, x: &Iso3, y: &Iso3) -> Iso3 {
    Iso3::new(
        Quat::new(if c { x.q.w } else { y.q.w }, if c { x.q.x } else { y.q.x },
                  if c { x.q.y } else { y.q.y }, if c { x.q.z } else { y.q.z }),
        Vec3::new(if c { x.t.x } else { y.t.x }, if c { x.t.y } else { y.t.y },
                  if c { x.t.z } else { y.t.z }))
}

/// LerpSlerp::eval past its endpoint shortcuts, slerp's series arm unconditionally.
#[inline(always)]
pub fn lerp_slerp_elem(a: &Iso3, b: &Iso3, s: f64) -> (Iso3, bool) {
    let t = a.t.scale(1.0 - s).add(b.t.scale(s));
    let qa = a.q;
    let dot = qa.dot(b.q);
    let qb = if dot < 0.0 { b.q.neg() } else { b.q };
    let h = 0.5 * qa.sub(qb).norm_squared();
    let theta_sq = theta_sq_from_chord(h);
    let wa = slerp_weight(1.0 - s, theta_sq);
    let wb = slerp_weight(s, theta_sq);
    let q = qa.scale(wa).add(qb.scale(wb));
    let safe = (s != 0.0) & (s != 1.0) & (h > 0.0)
        & (theta_sq >= SLERP_LERP_FALLBACK * SLERP_LERP_FALLBACK)
        & (theta_sq <= THETA_SLERP_SMALL * THETA_SLERP_SMALL);
    (Iso3::new(q, t), safe)
}

/// ScLerp::eval past its endpoint shortcuts: a * screw_pow(a^-1 b, s), with
/// screw_parts' degenerate return removed and ScrewParts::pow's series arm taken.
#[inline(always)]
pub fn sclerp_elem(a: &Iso3, b: &Iso3, s: f64) -> (Iso3, bool) {
    let rel = a.inv_mul(b);
    let q = if rel.q.w < 0.0 { rel.q.neg() } else { rel.q };
    let t = rel.t;
    let q_v = q.vector();
    let sh2 = q_v.norm_squared();
    let ch = q.w;
    let q_d = Quat::new(
        -(t.x * q.x) - t.y * q.y - t.z * q.z,
        t.x * q.w + t.y * q.z - t.z * q.y,
        -(t.x * q.z) + t.y * q.w + t.z * q.x,
        t.x * q.y - t.y * q.x + t.z * q.w,
    ).scale(0.5);
    let k = q_d.w / sh2;
    let m_sh = q_d.vector().add(q_v.scale(k * ch));
    let phi_sq = theta_sq_from_chord(sh2 / (1.0 + ch));
    let wa = slerp_weight(1.0 - s, phi_sq);
    let wb = slerp_weight(s, phi_sq);
    let (cos_sh, w) = (wa + wb * ch, wb);
    let q_r2 = Quat::new(cos_sh, w * q_v.x, w * q_v.y, w * q_v.z);
    let q_d2_v = m_sh.scale(w).add(q_v.scale(-s * k * cos_sh));
    let q_d2 = Quat::new(s * q_d.w * w, q_d2_v.x, q_d2_v.y, q_d2_v.z);
    let t2q = q_d2 * q_r2.conjugate();
    let pow = Iso3::new(q_r2, Vec3::new(2.0 * t2q.x, 2.0 * t2q.y, 2.0 * t2q.z));
    let safe = (s != 0.0) & (s != 1.0)
        & (sh2 >= PROTO_SCREW_DEGENERATE_SQ) & (sh2 <= PROTO_SIN_HALF_THETA_SMALL_SQ);
    (*a * pow, safe)
}

// lerp_slerp_safe / sclerp_safe: the same predicate expressions alone (sclerp_safe
// computes rel_q as a.q.conjugate() * b.q, which is inv_mul's rotation part).

/// SoA loop over the first n lanes (sclerp_soa is identical with sclerp_elem).
/// Returns true when no element needs the scalar fix-up.
#[inline]
pub fn lerp_slerp_soa<const N: usize>(a: &Lanes<N>, b: &Lanes<N>, s: &[f64; N],
                                      n: usize, out: &mut Lanes<N>) -> bool {
    let n = n.min(N);
    let mut bad = false;
    for i in 0..n {
        let (pa, pb, si) = (a.get(i), b.get(i), s[i]);
        let (p, safe) = lerp_slerp_elem(&pa, &pb, si);
        let (at0, at1) = (si == 0.0, si == 1.0);          // v2 only; v1 wrote p
        out.set(i, &select(at0, &pa, &select(at1, &pb, &p)));
        bad |= !(safe | at0 | at1);                        // v1: bad |= !safe
    }
    !bad
}
```

**Phase 2 and the fix-up** (`tf_tree_core::plan`, `v2`). `scal` replaces the
kernel call with a plain `for i in 0..n { p.set(i, &I::eval(..)) }`:

```rust
match policy {
    InterpPolicy::LerpSlerp => if !bp::lerp_slerp_soa(a, b, s, n, p) {
        fixup::<LerpSlerp, _>(bp::lerp_slerp_safe, a, b, s, n, p) },
    InterpPolicy::ScLerp => if !bp::sclerp_soa(a, b, s, n, p) {
        fixup::<ScLerp, _>(bp::sclerp_safe, a, b, s, n, p) },
}

#[inline(never)]
fn fixup<I: Interp, F: Fn(&Iso3, &Iso3, f64) -> bool>(safe: F, a: &Lanes<CHUNK>,
        b: &Lanes<CHUNK>, s: &[f64; CHUNK], n: usize, p: &mut Lanes<CHUNK>) {
    for i in 0..n.min(CHUNK) {
        let (pa, pb) = (a.get(i), b.get(i));
        if !(safe(&pa, &pb, s[i]) | (s[i] == 0.0) | (s[i] == 1.0)) {
            p.set(i, &I::eval(&pa, &pb, s[i]));
        }
    }
}
```

**The chunk loop** (`Plan::fold_batch_soa`). `read_brackets` walks
`chunk[..limit]` through `SampleRing::read_bracket_from`. On the monotone path it
uses the step's resumable cursor; otherwise it reads and writes the guard's hint
cell exactly as `sample_hinted` does. It returns `(interp, elements_read,
Option<error>)`. **This listing is condensed:** the prototype keeps the
`inverted` test outside the compose loop, as two loops, and that is the form
that was timed.

```rust
for chunk in stamps.chunks(CHUNK) {
    let mut limit = chunk.len();
    for i in 0..limit { acc.set(i, &Iso3::IDENTITY); }
    let mut first_err = None;
    for (k, step) in steps.iter().enumerate() {
        if limit == 0 { break; }
        match step {
            Step::Static(m) => for i in 0..limit { let x = acc.get(i) * *m; acc.set(i, &x); },
            Step::Dyn { edge: e, inverted } => {
                let (interp, got, err) = g.read_brackets(*e, k, &chunk[..limit], &nanos,
                    monotone, &mut cursors[k], (&mut la, &mut lb, &mut ls));
                if let Some(err) = err { limit = got; first_err = Some(err); }
                interp_lanes(InterpPolicy::from_u8(interp), &la, &lb, &ls, limit, &mut lp);
                for i in 0..limit {
                    let x = if *inverted { acc.get(i).mul_inv(&lp.get(i)) } else { acc.get(i) * lp.get(i) };
                    acc.set(i, &x);
                }
            }
        }
    }
    for i in 0..limit {
        g.note_ok(edge);
        let Some(d) = dst.next() else { return Ok(()) };
        write(&acc.get(i), d);
    }
    if let Some(err) = first_err { g.note_err(&err); return Err(err); }
}
```

`read_bracket_from` is `sample_from` under `ExtrapPolicy::Error`, line for line,
with three changes and no others:

- it returns `(a, b, s)` instead of `I::eval(a, b, s)`;
- an exact hit (`t == t_new`, or `t_i == t`) returns `(p, p, 0.0)`;
- the trailing `head - i > retained` lap check runs before the deferred `eval`
  rather than after it.

`ExtrapPolicy::Error` is the only policy the batch path passes at `681e601`.

**The isolated-stamp shape the engine test needs** (§8's test-design finding):

```rust
for (j, &t) in stamps.iter().enumerate() {
    check(&[t]);                  // alone
    check(&[safe_stamp, t]);      // lane 1, beside a known-safe element
    check(&[t, safe_stamp]);      // lane 0
}
// check(batch): at_many / at_many_into / at_many_into_f32 rows vs Plan::at on a
// fresh Guard, by to_bits, every component
```

**The per-symbol census** (`objdump -d -C`; only named bodies are counted). It is
a condensed form of the script that produced §3's tables. This form was run
against the `v2` and `v2nlv` bench binaries before being quoted, and it
reproduces §3's `at_many` (744 → 139), `sclerp_soa` (391 → 98) and
`lerp_slerp_soa` (166 → 37) packed-arithmetic counts:

```python
# disasm_counts.py BINARY PATTERN [PATTERN...]
import re, subprocess, sys, collections
CATS = {
  'packed_arith': r'^(v?)(add|sub|mul|div|sqrt|min|max)pd$',
  'packed_cmp':   r'^(v?)cmp\w*pd$',
  'packed_logic': r'^(v?)(and|andn|or|xor)pd$',
  'scalar_arith': r'^(v?)(add|sub|mul|div|sqrt|min|max)sd$',
  'scalar_cmp':   r'^(v?)(u?comisd|cmp\w*sd)$',
  'shuffle':      r'^(v?)(unpcklpd|unpckhpd|shufpd|movlhps|movhlps|pshufd|movddup|unpcklps|unpckhps|shufps)$',
  'packed_move':  r'^(v?)(movapd|movupd|movaps|movups)$',
  'call':         r'^call',
}
binary, pats = sys.argv[1], [re.compile(p) for p in sys.argv[2:]]
dis = subprocess.run(['objdump', '-d', '-C', '--no-show-raw-insn', binary],
                     capture_output=True, text=True, check=True).stdout
funcs, cur = collections.OrderedDict(), None
for ln in dis.splitlines():
    m = re.match(r'^[0-9a-f]+ <(.*)>:$', ln)
    if m: cur = m.group(1); funcs.setdefault(cur, []); continue
    m = cur and re.match(r'^\s+[0-9a-f]+:\s+(\S+)\s*(.*)$', ln)
    if m: funcs[cur].append((m.group(1), m.group(2)))
for name, ins in funcs.items():
    if not any(p.search(name) for p in pats): continue
    c = collections.Counter(insns=len(ins))
    for mn, ops in ins:
        c['ymm'] += ('%ymm' in ops or '%zmm' in ops)
        for cat, rx in CATS.items(): c[cat] += bool(re.match(rx, mn))
    print(name[:70], dict(c))
```

Run it as `python3 disasm_counts.py <bin> 'Plan>::at_many::' 'batch_proto::(sclerp|lerp_slerp)_soa'`
on the as-built binary and on the `-C no-vectorize-loops` binary. The claim
holds only if the packed arithmetic in the kernel symbols collapses in the
second.

The scratch locations at the time of writing, which are not durable:

- the first campaign's logs, summaries, disassembly census, full prototype diff
  and every binary are under the session scratchpad's `soa-results/`;
- the re-measurement's driver log and binaries are under its `skeptic/`;
- the prototype worktree is `wt-soa-proto`, detached at `681e601`.
