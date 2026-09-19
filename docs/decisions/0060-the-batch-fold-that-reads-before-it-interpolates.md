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

Steps: **0a done** (§9), **1 done** (§10), **2 done** (§11). **0b needs the
vanished prototype's kernels and has not run**, so Decision B is not taken.
**4 (aarch64) has not run.**

## Context

`Plan::at_many`, `at_many_into` and `at_many_into_f32` folded the whole plan once
per stamp: per dynamic step, one seqlocked ring read and one `Interp::eval`.
`Plan::fold_batch_with_twist` is the `QuatTwist` sibling and is untouched.
[`0016`](./0016-portable-simd-and-the-dependency-budget.md) (`withdrawn`) found a
branch-free interpolation loop vectorises but is unreachable until every bracket
is read first and interpolated second. That restructure runs across stamps within
one step; it is not [`fast-path.md`](../design/fast-path.md) §14's Lever 2
(splitting across *steps*).

**Constraints.** No `unsafe` in `tf_tree_math` and none added to `tf_tree_core`
([`0007`](./0007-the-unsafe-budget-and-the-c-abi.md),
[`0048`](./0048-a-kind-is-not-a-crate-name.md)); no intrinsics, no
`-C target-cpu`, no NLERP, no new dependency, `f64` only, D5's `LerpSlerp`
unchanged. **Batch results are bit-identical (`to_bits`) to `Plan::at`.**

## The stop rule

- A lever that wins under ~5% stops.
- A kernel must clear 1.3x against `Interp::eval` in a loop (§2) before an engine
  prototype is worth building. `v1` cleared it; **`v2`'s LerpSlerp kernel fails it
  at 1.268x**, its ScLerp kernel clears it at 1.796x.
- Applied per lever against its own control: the restructure against `681e601`,
  the kernel against the restructure and against its own
  `-C no-vectorize-loops` build.

## What was measured

Host: AMD EPYC-Milan, KVM guest, `[profile.bench]` (thin LTO,
`codegen-units = 1`, SSE2), `taskset -c 2`; arms interleaved, deltas paired per
rep. §1-§8 ran against `681e601` plus an uncommitted prototype. Arms: `base` (the
`681e601` engine), `scal` (two-phase chunked fold, scalar `Interp::eval`;
isolates the restructure), `v1`/`v2` (`scal` plus an SoA kernel; `v2` answers
`s == 0`/`s == 1` by bitwise select), `v2nlv` (`v2` with
`-C no-vectorize-loops`). A failing read at element `i` lowers the chunk's limit
to `i`, so the error and rows written are the per-stamp fold's.

### 1. Baseline

`monotone_1024` on `681e601` is 263 ns/stamp. The non-monotone path is faster than
the monotone one; recorded, not investigated.

### 2. The kernel's ceiling, isolated

`crates/tf_tree_bench/examples/autovec_probe.rs`: LerpSlerp SoA kernel 1.676x,
ScLerp 2.008x over `eval` in a loop, nearly all of it the loop vectoriser (0.962x
and 1.004x without). `v2`'s endpoint selects drop LerpSlerp to 1.268x and ScLerp
to 1.796x.

### 3. Evidence the widening is across stamps

A per-symbol census of packed `f64` arithmetic must collapse under
`-C no-vectorize-loops` or it is vacuous: `Plan::at_many` 744 to 139,
`sclerp_soa::<64>` 391 to 98, `lerp_slerp_soa::<64>` 166 to 37; no `%ymm`.

### 4. End to end

ns/stamp, N = 1024: 3dyn ScLerp mono 261.53 (`base`) / 192.35 (`scal`) / 124.88
(`v2`), i.e. `v2` -52.2% and `scal` -26.4%; `into_quat_twist_1024` (negative
control) -0.6%. The restructure alone is 38-54% of `v2`'s saving under ScLerp and
82-87% under LerpSlerp. Build-level noise is about 1-3%.

### 5. Where it loses: stationary edges

One pose every 5 ms, 1024 monotone stamps: every element falls outside the
kernel's safe region (LerpSlerp `h == 0`; ScLerp `sh2 = 0` for this quaternion,
§9.3). `v2` vs `scal`: LerpSlerp +80.0%, ScLerp +115.5%; vs `base` +0.9% and
+5.9%. The kernel erases the restructure's ~2x win on this data.

### 6. Knots: why `v1` and `v2` differ

An on-grid batch starts on a knot (`s == 0`); under `v1` that sends the whole
chunk through a `#[cold]` fix-up. `v2`'s selects remove it and pay off-grid. A
cheaper design, copying phase 1's exact hits over the kernel output afterwards
(`copy`), was not built; step 0b builds it.

### 7. Independent re-measurement

A second agent reproduced every headline within ~1%. On `at_many` the kernel's
gain over `scal` is the loop vectoriser within noise. The "87% kernel" claim on
`into_mat4` is withdrawn; §10.2 finds the weakness does not reproduce. Rebuild
arms rather than reusing saved binaries.

### 8. Bit identity and mutants

Bit identity passes at three levels: a kernel pin in `tf_tree_math` (every
element the classifier calls safe matches `eval` by `to_bits`; every out-of-region
band classifies unsafe), an engine test over crafted branch regions and random
series steps (every stamp alone and paired with a safe stamp in lanes 0 and 1, and
an error grid), and all rows of the benchmarked matrix.

**The test-design finding:** a classifier defect is masked whenever another
unsafe element shares its chunk, because the fix-up re-derives the predicate. Only
per-stamp isolated batches, in both lanes, caught the LerpSlerp classifier
admitting the fallback band (M3) and the ScLerp classifier admitting `s == 1.0`
(M4). **M5** (an error does not lower the chunk limit) is caught. **M6** (`v2`'s
`s == 0` select takes `b`) survives the engine test because phase 1 stores every
exact hit as `(p, p, 0.0)`, and is caught by the kernel pin.

### 9. Step 0a: what the recorded mix is

A measurement of data, nothing timed. `crates/tf_tree_bench/examples/bracket_mix.rs`
classifies every bracket an `at_many` sweep would read into the five arms of
`slerp` and `screw_parts` / `ScrewParts::pow`, under three sweeps (100 Hz
off-grid `rate`, one per sample interval `interval`, one per knot `ongrid`). Every
swept stamp also goes through `Plan::at` and must agree bit-identically. A bracket
whose `s` rounds to `1.0` counts as an exact hit.

```sh
cargo run --release -p tf_tree_bench --example bracket_mix
cargo run --release -p tf_tree_bench --example bracket_mix -- <stream> [sweep_hz]
```

The recording is `testdata/tfstream/indoor_atelier.tfstream`: 5 dynamic edges,
1 066 samples, 47.3 s. Under the 100 Hz sweep the four wheel edges are 100.0%
stationary and `odom_combined→base_footprint` is 86.3% series, 13.7% large arc;
all edges 18.9% series, 78.1% stationary. `ongrid` is 100% exact hits.

#### 9.1 Four of the five edges never move

All 202 intervals of each wheel/front-link edge have bit-identical rotations. That
is the regime §5 measured the prototype losing 80% (LerpSlerp) and 115% (ScLerp)
on, and it is 4 of 5 edges of the only real recording in the tree.

#### 9.2 The one moving edge is 99.2% series, and its 13.7% is gaps, not motion

Two publication gaps (1.20 s and 5.30 s) are large-arc because they are long.
**Both policies share one series bound, a statement about angle:** `θ ≤ 0.15 rad`
between consecutive samples. This edge would be series at any rate above
1.23 Hz. `TFT009` (`PHASE5.md`) reports the gaps.

#### 9.3 `ScLerp` reads a motionless edge as degenerate only by luck

`conj(q) ⊗ q`'s vector part cancels exactly only for a quaternion with a zero in
the right place (the wheel quaternion has `w = z = 0`); otherwise `sh2 ≈ 5e-36`
lands in `ScLerp`'s series region on rounding noise, bit-identically to
`ScLerp::eval`. So "not moving" equals "takes the fallback arm" under `LerpSlerp`
but not `ScLerp`, and §5's all-fallback loss is a `LerpSlerp` regime.

#### 9.4 The chunk fraction is near-bimodal: a bail-out has no threshold to tune

Under 100 Hz the wheel edges give 66 of 66 chunks at 0.00 series fraction;
`odom_combined→base_footprint` gives 61 of 74 at 1.00. What a bail-out separates
here is edges, not chunks.

#### 9.5 An on-grid consumer is 100% exact hits

A consumer looking up at a message's own stamp lands on a knot: §6's regime, why
`copy` and `v2` are separate arms in step 0b.

#### 9.6 The controls

The synthetic `fixture` is 100% series; `repeat/axis` 100% stationary;
`repeat/generic` 100% stationary under `LerpSlerp`, 100% series under `ScLerp`;
`jitter` (~1e-7 rad) 100% LERP fallback. Nothing sits near a class boundary.

#### 9.7 What step 0a does and does not settle

**Settled:** the mix is measured with controls (open question 1); the bail-out
threshold is not a tuning problem.

**Not settled:** this is one 47-second indoor run whose fastest edge publishes at
19.8 Hz, biased toward shrinking B's regime. **What generalises is §9.2's bound,
not §9's percentages.** Across all five edges at 100 Hz, 81.1% of brackets fall
back against a ~27% linear break-even between unlike datasets, which is not a
threshold; B's stop rule is still step 0b's interleaved timings, and the bail-out
is no longer optional.

### 10. Step 1: what A's win actually is, and what it costs at small N

Measured on `main` at `ac5d4d3` (fold byte-identical to §1-§8's); arms are an
independent reimplementation. Arms: `base`, `hoist` (samplers resolved once per
batch), `accbuf` (only the accumulator buffered), `scal` (64 lanes), `scal16`.

#### 10.1 The win is the phase buffering, and nothing else comes close

`monotone_1024`: `hoist` +0.71%, `accbuf` -3.43%, `scal` -25.10%. Deferring the
`eval` carries the whole 23-25%; the buffering is what A is.

#### 10.2 A is uniform across entry points, so it does not need to bind per one

`scal` vs `base`: `monotone_1024` -24.50%, `into_mat4_1024` -23.81%,
`into_quat_1024` -25.37%, control -1.00%. §7's `into_mat4` weakness does not
reproduce.

#### 10.3 Sixteen lanes

Sixteen lanes equal 64 at N = 63 and 1024 and are ahead below (N = 8: -21.60% vs
-12.96%), because they initialise a quarter as much. Stack reservation: `at_many`
1 256 B at `base`, 17 720 B at 64 lanes, 6 600 B at 16.

#### 10.4 The cost nobody had measured: a batch smaller than a chunk

One stamp is 3.1x slower at 64 lanes and 1.8x at 16, since the lane arrays
initialise on every call. N = 2 is +14.82% and N = 3 is -11.10%, so a batch of
fewer than **three** stamps takes the per-stamp fold; no `unsafe`. Both sides are
committed rows (`at_many_small/*`).

#### 10.5 On the recorded stream, which is what A's stop rule is applied to

1024 monotone off-grid stamps over `indoor_atelier.tfstream`, `scal16` vs `base`:
`laser → odom_combined` -26.16% (`at_many`) / -23.44% (`at_many_into(Mat4)`);
`left_wheel_link → odom_combined` -35.72% / -19.69%. A clears its ~5% floor four to
seven times over at every entry point.

### 11. Step 2: A as landed, and the one thing it costs

Landed 2026-09-18 against parent `2524667`: one fold body, sixteen lanes,
per-stamp fold below three stamps. `plan.rs` carries `FOLD_LANES = 16`,
`FOLD_MIN_BATCH = 3` and a `const` assertion pinning both, because
`crates/tf_tree/tests/batch_phases.rs` restates them.

#### 11.1 The numbers, `[profile.bench]`

Interleaved paired runs against `2524667`, 7 reps:

| row | `2524667` | A | delta |
| --- | ---: | ---: | ---: |
| `at_many/monotone_1024` | 273 811 ns | 220 115 ns | **-19.19%** |
| `at_many/into_mat4_1024` | 270 537 | 221 967 | -18.08% |
| `at_many/into_quat_1024` | 272 144 | 220 264 | -19.06% |
| `at_many/into_affine32_1024` | 280 733 | 223 481 | -20.38% |
| `at_many/into_quat_twist_1024` **(control)** | 340 851 | 340 862 | **+0.00%** |
| `at_many_recorded/mixed_at_many_1024` | 206 754 | 141 119 | **-31.35%** |
| `at_many_recorded/mixed_into_mat4_1024` | 204 648 | 174 914 | -14.17% |
| `at_many_small/at_many_1` | 104 | 101 | -2.18% |
| `at_many_small/at_many_2` | 261 | 260 | +2.34% |
| `at_many_small/at_many_3` | 651 | 577 | -11.52% |
| `at_many_small/at_many_16` | 3 723 | 3 116 | -16.31% |
| `at_many_shapes/*` (six rows) | | | -18.58% to -28.30% |

A's per-entry-point stop rule passed everywhere; no batch size loses
(`at_many_2`'s +2.34% is inside noise).

#### 11.2 The frame the record predicted, and what actually happened

The chunked pass is a separate `#[inline(never)]` function (`fold_chunked`), so
the entry frames do not grow: `at_many_into_tagged` stays 888 B and
`at_many_into_f32_tagged` 344 B; the lanes are reserved one call in, in a frame a
batch under three stamps never enters.

#### 11.3 The cost of one read body, which is real and is on `Plan::at`

`read_from` returning a 128-byte `Bracket` through a `Result` regressed the scalar
path (`lookup/depth3/sclerp/exact_hit` +86.4%). As landed, `read_from` is generic
over a `FromBracket` trait: the batch instantiates it at `Bracket`, the scalar
path at `Interpolated<I>`. At `[profile.bench]` every `lookup/*` row is within
1.4% of `2524667`. **At `[profile.embedder]` (`lto = false`,
`codegen-units = 16`) the scalar path still costs +7.7% to +11.0%**, caused by the
read split alone. It lands anyway: the workspace's baseline profile is
unchanged, `docs/API.md` §2.3 already records that `lto = "thin"` erases this
class of cost, two read bodies would trade a structural correctness invariant
(the trailing revalidation in `sample.rs`) for 10% on one profile, and
`just bench-check`'s `lookup_latency` and `embedding_cross_crate` rows are
UNAVAILABLE on this host. **Reopen** if a quiet, non-SMT, fixed-governor host
reproduces the embedder column, or if `just embed-cost`'s gated row resolves there
and moves.

#### 11.4 A lever found and not taken

`#[inline(always)]` on `SampleRing::sample_from` is -27.6% to -38.0% at
`[profile.bench]` and +13.5% to +20.2% at `[profile.embedder]`: the same trade as
11.3. It belongs to `docs/API.md` §2.3's inline-placement question.

#### 11.5 What holds it

- **`crates/tf_tree/tests/batch_phases.rs`, 6 tests**, reproducing §8's engine
  test over every batch entry point against `Plan::at` by `to_bits`, including the
  error grid; `the_fixture_reaches_the_branches_it_names` asserts the inputs.
- **Loom model** `read_from_validates_the_bracket_it_hands_back` at
  `LOOM_MAX_PREEMPTIONS = 3`; deleting `read_from`'s trailing `head - i > retained`
  check fails it. The lap check does not move on the scalar path.
- **Mutants:** M5 is caught by the error grid alone; `inverted` ignored, phase 1
  reading `chunk[0]` for every lane, and the accumulator not reset per chunk are
  each caught by 3 tests; `cursors` reset per chunk survives as equivalent;
  `FOLD_MIN_BATCH = 1` is caught by the `const` pin (equivalent without it: the
  bypass is a performance boundary held by `at_many_small/at_many_2` and `_3`).

#### 11.6 What step 2 does not close

- §1's monotone-slower-than-non-monotone inversion; the non-monotone batch keeps
  the per-stamp `fold_at` loop.
- `[profile.embedder]`'s scalar column (11.3).
- **aarch64**, plan step 4.

## Decision (A taken and landed; B proposed, not taken)

1. **Decision A: the restructure, with no kernel. LANDED 2026-09-18 (§11).**
   `Plan::fold_batch` is a chunked two-phase fold: per chunk and per step, phase 1
   reads every bracket through **the one** seqlocked read path and phase 2 calls
   the scalar `Interp::eval`. Amended by §10: **sixteen lanes** (§10.3); **a batch
   of fewer than three stamps takes the per-stamp fold** (§10.4); **one fold body,
   not one per entry point** (§10.2). `sample_from` is the bracket read plus
   `eval`, not a copy beside it.
2. **Decision B: an SoA kernel for ScLerp, on top of A, only if step 0 says the
   data wants it.** It applies only to chunks the classifier admits, with a
   chunk-level bail-out; step 0b must answer whether paying the classification at
   all beats not having the kernel. Its knot handling is whichever of `v2`'s
   selects and `copy` clears the 1.3x bar and is faster on the recorded mix.
   **B proposes no LerpSlerp kernel**: `v2`'s is 1.268x in the probe (§2). One
   re-enters only if a variant clears the bar with knot handling included and then
   clears the stop rule against A on the recorded mix.

**The stop rules bind each separately.** **A** binds per entry point: it lands
where the arm wins at least ~5% against `681e601` on step 0a's recorded stream; an
entry point where it does not keeps the per-stamp fold. **B** is abandoned if it
wins under ~5% against **A** on the recorded mix, a loss included, or under ~5%
against its own `-C no-vectorize-loops` build.

## Rationale

- **Adopt `v2` as prototyped: no.** Its -52% headline was measured on synthetic
  data in the series region; on stationary data it is at parity or worse than A,
  its LerpSlerp kernel fails the probe bar, and its kernels are second bodies of
  `slerp`'s series arm and of `screw_parts` / `ScrewParts::pow`.
- **Adopt A and B together: no.** They are separable; A carries most of the
  saving, and bundling lets B's unmeasured sign ride in on A's measured one.
- **`pulp`:** excluded by D14's budget; it cannot vectorise a seqlock loop.
- **NLERP or any approximation:** breaks bit-identity with `Plan::at`.
- **Only plans with exactly one dynamic step:** the 3-step cells win as much. That
  shape survives only as a bench and test fixture;
  `crates/tf_tree_bench/benches/py_parity.rs` has it and is all `h == 0` under
  LerpSlerp (§5's all-fallback regime).

## Consequences

- **`at_many` stops being a per-stamp fold.** Under a live writer the gap between
  two steps' reads for one stamp grows to up to one chunk; bit-identity to
  `Plan::at` is a property of a quiescent ring.
- **On a failing batch, phase 1 does bounded extra work** (up to the chunk's end);
  results, counters, the error and rows written are the per-stamp fold's.
- **The entry frames do not grow (§11.2).**
- **If B lands, two arithmetic bodies must stay bit-identical forever.** That is
  D13's shape (`docs/PROJECT.md` §5): `ScLerp::eval` is the reference, and any edit
  to `screw_parts` or `ScrewParts::pow`'s series arm must be mirrored in the
  kernel, with the private thresholds reaching the classifier by value.
- **PHASE3 §12.2 criterion 2's denominator moves.** `py_parity`'s
  `native_mat4_4096` reaches the same `at_many_into_tagged`; whichever lands
  retakes that criterion's reading in the same change.
- **No `just` gate sees the batch lever** (open question 4).
- **`fold_batch`'s doc says the cursor logic "must not be duplicated"**; the
  prototype duplicated it. A must not.

## Implementation plan

0. **Measure the recorded `/tf` mix, then time the arms on it.**
   - **0a. Classification. DONE, 2026-09-16 (§9).**
   - **0b. Timing, on the uncommitted prototype.** Build `copy` (`v1`'s kernel
     with `v2`'s monomorphic fix-up, then a pass copying `a` over each recorded
     exact hit) and the chunk-level bail-out. Then, interleaved:
     1. **Knot handling.** Time `v2` and `copy` on 0a's stream and on the matrix's
        on-grid and off-grid cells, and `copy`'s kernels in §2's probe against the
        1.3x bar; the faster on 0a's stream that clears it is B's knot handling.
     2. **B against A.** Time `base`, `scal`, the kept arm with and without the
        bail-out, and the kept arm with `-C no-vectorize-loops`, on 0a's stream and
        `py_parity`'s one-dynamic-step `at_many_into` rows.
     - **Stop points:** **B closes here** if its faster arm wins under ~5% against
       `scal` on 0a's stream or against its own `-C no-vectorize-loops` build; this
       record then says what would reopen it. The LerpSlerp kernel stays out unless
       `copy`'s clears 1.3x in the probe and ~5% against `scal` on 0a's LerpSlerp
       edges.
1. **Attribute A's mechanism, size its chunk, apply A's stop rule. DONE,
   2026-09-17 (§10).**
2. **Land A. DONE, 2026-09-18 (§11).** One read body over `FromBracket`; the
   bypass tested either side by `at_many_small/*` and pinned by the `const`;
   loom model `read_from_validates_the_bracket_it_hands_back`;
   `batch_phases.rs`; `at_many_shapes` rows; M5 and five more mutants; gates at
   `[profile.bench]` and `[profile.embedder]`. The embedder stop point was not
   triggered (batch rows win 10.4-24.2%); the scalar loss is disclosed (§11.3).
3. **Only if step 0b left B open, land B on A.**
   - The kernel (ScLerp only unless 0b re-admitted LerpSlerp) with 0b's knot
     handling and bail-out, and a kernel pin in the crate it lands in (open
     question 5).
   - **A D13 proptest**: pose pairs, `s` and mixed safe/unsafe chunk fills,
     comparing kernel plus fix-up with `ScLerp::eval` by `to_bits`. The fixed tests
     stay, since a generated distribution will not land on `s == 1.0`, the `1e-290`
     degenerate band or the series threshold.
   - Every §8 mutant that applies, each with its catching assertion.
   - The per-symbol census (*Reproduction*) on the shipped bench binary: packed
     arithmetic must collapse under `-C no-vectorize-loops`, zero `%ymm`.
   - The B-versus-A delta on the matrix, stationary rows, `py_parity` and the
     recorded stream, at both profiles.
   - **Stop point:** B does not land if, at either profile, it wins under ~5%
     against A on the recorded stream or against its own
     `-C no-vectorize-loops` build, or if the census does not collapse.
4. **Re-time on a second architecture** (`ubuntu-24.04-arm` or any aarch64 host)
   for A and, if it landed, B. **B's claim is x86-64 SSE2 only until this runs.**

## Open questions

1. ~~**Which recorded stream is the mix?**~~ **ANSWERED, 2026-09-16** (§9):
   `testdata/tfstream/indoor_atelier.tfstream`, alone. **Reopen criterion:** if a
   second permissively-licensed recording with a genuinely high-rate moving edge
   is added to `testdata/tfstream/`, re-run step 0a on it and re-apply B's stop
   rule before B closes or lands.
2. ~~**Chunk size and stack.**~~ **ANSWERED, 2026-09-17 (§10.3, §10.4):** sixteen
   lanes with a bypass below three stamps. **Still open:** whether a 6.6 kB
   batch-entry frame (five times `base`'s) is acceptable for `no_std`, small-stack
   embedders and `API.md` §8's real-time envelope.
3. ~~**One read body: is the lap check's move acceptable on the scalar path?**~~
   **ANSWERED by step 2 (§11.3, §11.5):** the lap check does not move on the
   scalar path; the cost is codegen at `[profile.embedder]`. Two read bodies were
   not taken.
4. **What gates the batch lever?** `bench-check` and `embed-cost` cannot see it.
   Options: a batch row in `bench_report` (needing `Fitness::probe`); the
   per-symbol census as a recipe; both. **B without one ships a vectorisation
   nothing would notice losing.**
5. **Where do the kernels live, and at what visibility?** The prototype put them
   in `tf_tree_math` as a `#[doc(hidden)] pub mod`, which is public surface on a
   published crate; the alternative is `tf_tree_core`, which needs
   `SIN_HALF_THETA_SMALL_SQ` and `SCREW_DEGENERATE_SQ` exported. Not
   decision-affecting for A.

## Reproduction

The fold is `Plan::fold_batch` / `Plan::fold_chunked`, the read is
`SampleRing::read_from`, the test is `crates/tf_tree/tests/batch_phases.rs`, the
model is `loom_tests::read_from_validates_the_bracket_it_hands_back`, the rows are
the `at_many_shapes` group; §9's tables are the `bracket_mix` command. The
prototype is gone; a reader re-running §1-§8 rebuilds it on `681e601`.

**The per-symbol census** (`objdump -d -C --no-show-raw-insn BINARY`): per function
matching a pattern, count instructions by class (packed arithmetic
`(add|sub|mul|div|sqrt|min|max)pd`, scalar `...sd`, shuffles, `call`) and
`%ymm`/`%zmm` operands, on the as-built binary and on the
`-C no-vectorize-loops` binary. The claim holds only if the packed arithmetic in
the kernel symbols collapses in the second (§3).
