# 0013: The benchmark gate never interpolated, and what §11.3's numbers should be

**Status:** ready — items 1 and 2 of the *Decision* have landed (see the process
note below); all four open questions are resolved in *Resolution* at the end of
this record, and item 3 (the §11.3 thresholds) is the remaining work
**Owner:** @NoeFontana
**Implementation:** `crates/tf_tree_bench` (`fixture::QUERY_NS`, both call sites,
one test), `crates/tf_tree_py` (`NS_PER_STEP_ESTIMATE` 55 → 64, per `API.md`
§3.4), `docs/PHASE3.md` §6.1, `docs/API.md` §2.3/§3.1/§3.4/§6 row 10

> **Process note — this record was implemented in part while still `draft`, and
> that is stated here rather than left for a reader to notice.**
> [`README.md`](./README.md) reserves implementation for a `ready` record. Items
> 1 and 2 of the *Implementation plan* below — the stamp fix at both call sites,
> and the re-baseline measurement — were nonetheless written and landed against
> this record while it was `draft`. **On whose authority:** the measurement and
> the `API.md` §3.4 re-derivation of `NS_PER_STEP_ESTIMATE` were directed by the
> owner directly, outside this record; the *plan items* were not separately
> authorised, and were implemented ahead of the `draft` → `ready` gate. The code
> is correct and is not being backed out for process reasons, but the sequence
> was wrong and the record should not read as though the gate had been passed.
>
> Item 3 — `PHASE1.md` §11.3's thresholds — is untouched, and the *Open
> questions* are open. The record stays `draft`.

## Context

A five-lens audit of `tf_tree_core`, `tf_tree_arena` and `tf_tree_ipc` — the same
exercise that produced [`0011`](./0011-the-bridge-clock-guard-and-the-static-conflict-disposition.md)
for the Phase 4/5 surface — found that the lookup benchmark and the go/no-go gate
report have never measured interpolation.

`crates/tf_tree_bench/src/fixture.rs:28` defines the query stamp:

```rust
pub const NOW_NS: i64 = 9_900_000_000;
```

The fixture's four dynamic edges publish at 50, 200, 1000 and 10 Hz
(`fixture.rs:198-201`). `NOW_NS` is an exact multiple of **all four** periods:

| Edge rate | Period (ns) | `NOW_NS % period` |
| --- | --- | --- |
| 50 Hz | 20 000 000 | **0** |
| 200 Hz | 5 000 000 | **0** |
| 1000 Hz | 1 000 000 | **0** |
| 10 Hz | 100 000 000 | **0** |

Every dynamic edge therefore takes the exact-hit branch in
`SampleRing::sample` (`crates/tf_tree_core/src/sample.rs:100`):

```rust
let result = if t_i == t {
    // Exact hit — no interpolation.
    self.read_slot((i & self.mask) as usize)?
} else {
    ...
    I::eval(&a, &b, s)
};
```

`I::eval` never runs. The benchmark that `docs/PHASE1.md` §11.3 gates on has been
timing `bracket` plus `read_slot` — the seqlock read — and nothing else. Both the
criterion suite (`crates/tf_tree_bench/benches/lookup.rs:25`) and the gate report
(`crates/tf_tree_bench/src/report.rs:1472`) build their stamp the same way, from
the same constant, so this is one root cause at two call sites.

**The tell was already in the published data.** ScLerp and LerpSlerp come out
within ~1% of each other, while §11.3 budgets them 150 ns and 100 ns — a 50%
spread. Two interpolators cannot be indistinguishable unless neither is running.

### Measured (draft reading — superseded by *Re-baseline* below)

`cargo bench -p tf_tree_bench --bench lookup -- --quick`, this host, on-grid
(shipped) against off-grid (`NOW_NS - 500_000`, which is off-grid for all four
periods):

| Benchmark | On-grid (shipped) | Off-grid | Ratio |
| --- | --- | --- | --- |
| `depth1/sclerp` | 28.6 ns | 106.7 ns | 3.7× |
| `depth3/sclerp` | 69.9 ns | 290.1 ns | 4.1× |
| `depth3/lerpslerp` | 69.1 ns | 221.8 ns | 3.2× |
| `depth6/sclerp` | 52.0 ns | 202.6 ns | 3.9× |

Off-grid, the ScLerp/LerpSlerp gap becomes 290.1 / 221.8 = **1.31**, which is the
spread the design predicts. On-grid it is 1.01.

> **These absolute numbers are 46–71 % high, and `--quick` is why.** (Row by
> row against *Re-baseline* below: depth1 on-grid +70 %, depth3 on-grid +71 %,
> depth1 off-grid +53 %, depth3 ScLerp off-grid +51 %, depth3 LerpSlerp off-grid
> +46 %, depth6 off-grid +51 %. An earlier revision wrote "~25–70 %"; no row is
> 25 % high and the lower end of the real range is 46 %.) The
> finding they were taken to demonstrate — that the shipped stamp never
> interpolates — is unaffected and is confirmed below. The *magnitudes* are not:
> re-run without the flag, today's on-grid binary reports 40.8 ns at depth 3
> rather than 69.9, and the off-grid one 192.7 rather than 290.1. Everything a
> reader should quote is in *Re-baseline*; this table is kept because the record's
> argument was built on it and deleting it would hide what the argument was.
>
> The attribution is measured, not inferred. Today's on-grid binary, `--quick`
> against default sampling:
>
> | row | `--quick` today | this table | default sampling today |
> | --- | --- | --- | --- |
> | `depth1/sclerp` | 28.7 ns | 28.6 ns | 16.8 ns |
> | `depth3/sclerp` | 69.5 ns | 69.9 ns | 40.8 ns |
>
> The flag reproduces this table to within 1 % — across the `#[inline]` and
> galloping-cursor work that landed in between — and dropping it moves the number
> by 1.7×.
>
> **What the flag does**, re-read out of criterion 0.5.1's `src/routine.rs`
> (`Routine::sample`, the `if config.quick_mode` branch), because an earlier
> revision of this note stated the mechanism backwards:
>
> - It **never calls `warm_up`**. The default path calls it and *discards* the
>   result, using it only to size the sample.
> - It times `n = 1` iteration, then loops: time `2n`, fit, double. Each pass
>   carries the previous pass's `2n` timing forward as the new `n` point, so one
>   new `bench` call happens per doubling.
> - It stops when the residual of a two-point least-squares fit falls below
>   `significance_level` (default 5 %) **and** 100 ms have elapsed, or when
>   `measurement_time` (default 5 s) runs out — and returns **those two points**,
>   `n` and `2n`. The reported estimate is the slope through them.
> - The fit is `t = (t_prev + 2·t_now) / 5`, which weights the **larger, later**
>   sample 4:1 over the smaller, earlier one. So the estimate is *not* "dominated
>   by the first, cold sample" — that sentence stood here, and it is backwards.
>
> The default path instead discards a 3 s warm-up and fits 100 samples over a 5 s
> window (criterion 0.5.1's `Criterion::default`: `warm_up_time` 3 s,
> `measurement_time` 5 s, `sample_size` 100).
>
> **Which of those differences produces the 46–71 % is not isolated here**, and
> this note no longer claims it is. What the source establishes is only that no
> warm-up is discarded and that the estimate rests on two samples rather than a
> hundred. **`--quick` is therefore not the mode to re-baseline in**, and the
> numbers below are taken without it.

**Nothing here is a performance regression.** No code got slower. The engine has
always cost this much to interpolate; the benchmark simply never asked it to.

### What forces the decision now

Fixing the stamp is four characters. The problem is what the honest number does
to the gate: `docs/PHASE1.md` §11.3 budgets **150 ns p50 at depth 3**, and the
real measurement is over it. So the choice is not "fix the benchmark or not", it
is "what should §11.3's numbers be, now that we can measure the thing they were
written about". That is a change to a normative document, which `CLAUDE.md`
routes here rather than to a PR.

## Re-baseline

*The stamp fix and the measurement have landed (ahead of the `ready` gate — see
the process note above); §11.3 has not been touched and all four questions below
are still open.*

### What was run, and on what

`crates/tf_tree_bench/benches/lookup.rs` now queries `fixture::QUERY_NS`
(`NOW_NS − 500 µs`, off-grid on all four dynamic periods); `fixture::NOW_NS` is
unchanged and still on-grid, which is what the history window wants. Both stamps'
grid relationships are asserted by
`fixture::tests::the_latency_query_stamp_is_off_every_dynamic_grid`, so neither
can be tidied away silently.

Protocol, stated because `docs/PHASE5.md` §9.3 requires it:

- **Binaries**: two builds of the same bench target — the shipped on-grid one and
  the off-grid one — kept side by side and run **alternately**, so host drift
  lands on both columns rather than on one.
- **Profile**: `[profile.bench]` (`lto = "thin"`, `codegen-units = 1`), the
  workspace's own. An embedder's profile is `docs/PHASE5.md` §9.2's
  `embedding_cross_crate` row, not this one.
- **Sampling**: criterion 0.5.1 defaults, no `--quick`, for the reason recorded
  above. Those defaults are a **3 s warm-up, discarded**, then 100 samples over a
  **5 s** window; a re-run on 2026-08-02 prints `Warming up for 3.0000 s` and
  `Collecting 100 samples in estimated 5.000x s`, at 20–73 M iterations per row
  for the off-grid binary. (An earlier revision of this line said "2 s warm-up …
  4 s window … 21–236 M iterations". Those are neither criterion's defaults nor
  what a run of this bench prints, and the line called them defaults, so it is
  corrected rather than reconciled.)
- **Warmed**: the tree is built and 10 s of history published before the timed
  loop; the plan is compiled and the `Guard` created outside it. The guard's
  per-step bracket cursor is therefore warm and the stamp is constant, which is
  the *best* case for the cursor — see *What this number is not*.
- **Pinning**: `taskset -c 2`, single-threaded, one bench process at a time.
- **Host**: the development VM — AMD EPYC-Milan, 4 physical cores, SMT on,
  governor unreadable, THP `madvise`, kernel 6.8.0-136. It **fails
  `Fitness::probe`** and it is shared with other tenants; load average during the
  runs was 1.3–2.5. Nothing here is a claim in §9.3's sense.
- **Runs**: six alternated rounds of all four rows, plus three more rounds of the
  two depth-3 rows. Nothing was discarded.

### The rows

Criterion's point estimate per run; `n` runs; min / median / max across runs.

| Row | on-grid (superseded) | **off-grid (the honest cost)** | n | ratio (median) |
| --- | --- | --- | --- | --- |
| `depth1/sclerp` | 16.8 (16.8–17.0) | **69.6** (68.2–74.6) | 6 | 4.1× |
| `depth3/sclerp` | 40.8 (40.3–47.5) | **192.7** (190.4–268.9) | 9 | 4.7× |
| `depth3/lerpslerp` | 40.8 (40.4–41.4) | **151.8** (146.2–190.4) | 9 | 3.7× |
| `depth6/sclerp` | 31.4 (31.1–34.2) | **134.5** (132.9–165.3) | 6 | 4.3× |

All figures in nanoseconds. Depth 3 is three *dynamic* steps (`map ← imu_link`,
1 kHz / 200 Hz / 50 Hz); depth 1 is one; **depth 6 is six edges of which four are
static and fold**, leaving two dynamic steps, which is why it comes out *cheaper*
than depth 3. §11.3's NORMATIVE "every reported latency row must state its
dynamic-step count" exists for exactly this row. The compiled shapes those
counts describe are asserted, not read off the fixture by eye, by
`fixture::tests::the_benched_paths_have_the_step_counts_the_baseline_assumes`.

**Reproduced.** Three further alternating rounds on 2026-08-02, same protocol,
after everything on this branch had landed: `depth1/sclerp` 68.6 / 96.1 / 68.7,
`depth3/sclerp` 192.1 / 212.3 / 190.0 / 245.8, `depth3/lerpslerp` 147.6 / 146.6 /
147.8 / 147.5, `depth6/sclerp` 176.9 / 133.7 / 134.5. Every median lands inside
the band above; the outliers are the same host, and the second `depth1` run
(96.1 against a 68.6 median) is the clearest single example of why nothing here
resolves 10 %.

**An illustrative decomposition — and it is not a check.** The three rows'
compiled shapes are one dynamic step, two dynamic steps plus one folded constant,
and three dynamic steps, which is three equations in three unknowns:

| row | shape | measured |
| --- | --- | --- |
| `depth1` | `i + d` | 69.6 |
| `depth6` | `i + 2d + c` | 134.5 |
| `depth3` | `i + 3d` | 192.7 |

Solving: a **dynamic step d = 61.6 ns**, a folded-constant step **c = 3.4 ns**,
and a per-call intercept **i = 8.0 ns**. Each is the right order of magnitude for
what it names — an interpolation plus a bracket search, one `Iso3` multiply, and
a call with a generation check — and that plausibility is the whole of what the
exercise buys.

**It buys nothing more, and an earlier revision of this paragraph claimed it
did.** The system is *exactly* determined: three equations, three unknowns, zero
degrees of freedom. It therefore has a zero residual for any three medians
whatsoever, including wrong ones, and cannot fail. Calling it "a coherence check"
was a category error — there is no coherence left to check once the unknowns are
solved for. **d = 61.6 ns/dynamic-step is a derived quantity, not a validated
one**, and nothing downstream should quote it as a measurement; the measurements
are the four rows in the table. Making it a check would need a fourth row with a
shape the other three do not span — a depth-2 or depth-4 dynamic path — and no
such row exists in this fixture.

The constant the design actually needs is not `d`. `PHASE3.md` §6.1's
`NS_PER_STEP_ESTIMATE` is *cost per compiled step including the intercept*, which
is one measured median divided by one asserted step count — 192.7 / 3 = 64.2 at
the depth the design is anchored to — so **64** is what the constant was
re-derived to in this commit (`API.md` §6 row 10, NORMATIVE that it moves here).
**`PHASE3.md` §6.1's amendment is the single account of that constant** — what it
moved, what it replaces, and why one element is the whole consequence — and this
record deliberately does not restate the arithmetic.

**The interpolator now runs, and the tell 0013 opened with is gone.** Off-grid,
the ScLerp/LerpSlerp gap is 192.7 / 151.8 = **1.27**, against 1.00 on-grid — the
~1.3× the design predicts, which two interpolators cannot show unless both are
executing.

### What the spread can and cannot resolve

The upper tails are this host's other tenants, not the engine: six of the nine
`depth3/sclerp` runs land in 190–194 ns and the other three at 199.3, 210.1 and
268.9. **A difference smaller than ~10 % is not resolvable here**, and two things
were measured and are reported as unresolved rather than as numbers:

- **The moving-stamp contrast.** A third binary sweeping 977 off-grid stamps
  (rather than repeating one) was alternated with the fixed-stamp binary for
  three rounds: `depth3/sclerp` 189 / 238 / 214 ns sweeping against 199 / 269 /
  193 fixed. The within-binary range covers the between-binary difference
  entirely, so that round offered no figure.

  **A later, quieter round does resolve the sweep — and it resolves it to
  nothing.** The *Corroboration* table below swaps the fixed stamp for the
  `embed` probe's own 1024-stamp sweep on `depth3/lerpslerp` and measures
  +0.9 % (one run, against a four-run baseline reproducing to ~1 %). That is **not** the same as measuring a cold cursor: the probe's sweep
  steps 9 631 ns, which is under one hundredth of the 1 kHz edge's period, so
  over the whole sweep the cursor walks about 9.9 slots on the 1 kHz edge, 2.0 on
  the 200 Hz and 0.5 on the 50 Hz — roughly one cursor move per 104 calls. So:
  **the sweep is not what separates the two harnesses, and how much of the 192.7
  is the warm cursor is still not measured**, because no benchmark here moves the
  cursor hard enough to price it.
- **Anything at the 5 % scale.** For comparison, `docs/PHASE5.md` §9.2's
  `embedding_cross_crate` row only resolves 5 % by pairing its two columns
  *inside* a round; two unpaired criterion estimates on this host do not.

### Corroboration from a second harness, and the 31 % it took to get it

`just embed-cost`'s probe measures the same depth-3 fold with a different loop.
Both harnesses were re-run for this section on 2026-08-02, same host, `taskset
-c 2`, at the same codegen settings: the workspace manifest declares
`lto = "thin"`, `codegen-units = 1` for **both** `[profile.bench]` (which
`cargo bench` uses) and `[profile.release]` (which `embed_cost`'s control column
is built at), so the two harnesses are compared at identical codegen. The
`[profile.embedder]` column is *not* used for this comparison, and is the one
`PHASE5.md` §9.2 gates.

**The pair an earlier revision of this section published was not like-for-like,
and it hid a disagreement.** It set the probe's `LerpSlerp` column beside
`depth3/`**`sclerp`** and called ~190 ns agreement. Against its own
interpolator the probe does not agree:

| harness | interpolator | ns | n |
| --- | --- | --- | --- |
| criterion `lookup/depth3/lerpslerp` | LerpSlerp | **147.6** (146.6–147.8) | 4 runs |
| — *the same row in* The rows *above, earlier round* | LerpSlerp | 151.8 (146.2–190.4) | 9 runs |
| `embed_cost` **in**-crate, `[profile.release]` | LerpSlerp | **194.0** (round spread 0.26 %) | 9 rounds |
| `embed_cost` **out**-of-crate, `[profile.release]` | LerpSlerp | **193.0** (round spread 0.54 %) | 9 rounds |

194.0 / 147.6 = **1.31**; against the earlier round's 151.8 it is 1.28. Either
way, publishing that as corroboration would have been exactly the failure this
record exists to name. (The two criterion medians differ by 2.8 % across rounds
weeks apart, which is what *What the spread can and cannot resolve* means by not
resolving 10 % — the 31 % is well outside it.) Note also that the two
`embed_cost` columns are within 0.6 % of *each other*, so the gap is not about
which crate the body sits in.

**The 31 % is the `#[inline(never)]`, and that is measured rather than argued.**
Three differences between the harnesses were introduced into
`benches/lookup.rs` one at a time, `depth3/lerpslerp` re-run after each, and the
file reverted:

| `benches/lookup.rs`, changed to… | ns | runs | vs shipped |
| --- | --- | --- | --- |
| as shipped | 147.6 | 4 | — |
| …+ `black_box` on `plan` and `guard`, not only the stamp | 148.5 | 2 | +0.6 % |
| …+ the probe's 1024-stamp off-grid sweep instead of one repeated stamp | 148.8 | 1 | +0.9 % |
| …+ the call behind `#[inline(never)]`, as the probe's `one` is | **200.3** (198.7–200.3) | 3 | **+36 %** |

Each row is cumulative on the one above it. The first two are the differences
this record had been worrying about — a hoisted plan, a warm cursor — and
together they are worth under 1 %. The third is the whole gap. At 200.3 ns the
criterion row and the probe's out-of-crate column (193.0 ns — same crate, same
profile, same attribute) agree to **3.8 %**, and *that* is the like-for-like
corroboration: two harnesses that share only the engine agree to within 4 % once
their call shapes match. The ScLerp row cannot be corroborated at all here —
`embed::measure_with` hard-codes `InterpPolicy::LerpSlerp`, so the probe has no
ScLerp column to set beside `depth3/sclerp`.

**What this does to the gate, and it is a new question rather than an answer.**
The non-inlined call is worth ~51.5 ns (200.3 − 148.8) — **26 % of the 200 ns
figure, 35 % on top of the 148 ns one**. `PHASE1.md` §11.3 does not say which
call site its budget is written against, and the two readings differ by more
than any headroom under discussion in *Open questions* 2. It is added there as
question 3.

### What this number is not

- **Not a p50 under load.** It is a single-threaded, hot-cache, warm-cursor,
  repeated-stamp loop on a quiescent tree. §11.3's gate speaks of a p50, and the
  tail — the thing `PHASE2.md` §7.1 calls "a 150 ns p50, with a p99.9 that
  matters more" — is not measured by this row at all.
- **Not a claim.** The host fails `Fitness::probe`; `bench_report`'s
  `lookup_latency` row is `unavailable` on it before and after this change, so
  the committed baseline's status is unmoved.
- **Not a regression.** No code got slower; the benchmark started asking for work
  it had never asked for.

## Decision

*(draft — this is the recommendation, not yet ratified)*

1. **Fix the stamp at both call sites.** ✅ **Done.** `report.rs`'s
   `measure_lookup_latency` and `benches/lookup.rs` take `fixture::QUERY_NS`
   (`NOW_NS − 500 µs`); `NOW_NS`'s own doc now records that it is deliberately
   on-grid for all four rates, and a test asserts both properties so the offset
   cannot be tidied away.
2. **Re-baseline once**, after the other measurement-moving changes have landed.
   ✅ **Done** — the `push` heartbeat store ([`0014`](./0014-the-push-heartbeat-is-a-store.md)),
   the galloping cursor and the `#[inline]` placements are all in; the numbers are
   in *Re-baseline* above. The committed `results.json` is unmoved because
   `lookup_latency` is `unavailable` on this host either way.
3. **Amend `docs/PHASE1.md` §11.3** to budgets justified by the measurement, with
   the on-grid history recorded inline so the loosening is not mistaken for a
   concession to a regression. ⛔ **Not done, and deliberately not done by the
   re-baseline commit** — it is *Open questions* 1–3 below, and §11.3 is
   normative.

Item 3 is the part that needs a decision. Two defensible readings, restated
against the measured numbers rather than the draft ones:

- **(a) Re-cut the budget to the measured cost.** Depth-3 p50 ≤ 250 ns ScLerp /
  ≤ 200 ns LerpSlerp would be ~1.3× headroom over the 192.7 / 151.8 measured
  here. Honest, and the gate starts gating.
- **(b) Keep 150 ns as a *target*, and gate on regression-from-baseline instead
  of an absolute.** The absolute number was picked before anyone had measured
  interpolation, so treating it as a standing goal rather than a pass/fail line
  is arguably closer to what it always meant.

### What each reading would do to the existing rows

Nothing in this table has been changed; it is what the ratification would change,
priced.

| Consumer of §11.3's numbers | under (a) | under (b) |
| --- | --- | --- |
| `docs/PHASE1.md` §11.3 first bullet | two new absolutes, with the measurement and the on-grid history beside them | unchanged text, re-labelled *target*, plus a sentence naming the baseline as the gate |
| `xtask/src/main.rs`'s printed gate line (`< 150 ns … / < 100 ns …`) | new numbers; still `UNAVAILABLE` on any host failing `Fitness::probe` | replaced by "regression against the committed baseline", which the same binary can actually evaluate |
| `docs/PHASE1.md` §13 checklist item "the gate in §11.3 is met, **or** a written explanation of which criterion failed and by how much" | met on a fit host, presumably | never "met" as an absolute; the explanation clause becomes permanent |
| `bench_report`'s `lookup_latency` row | unchanged — it already gates `p50_ns` against the committed baseline with a per-percentile slack, which *is* reading (b) | unchanged, and (b) makes §11.3 agree with what the tool already does |
| `PHASE3.md` §12.2 criterion 1 (scalar `plan.at` < 250 ns ≈ 31 ns call + 150 ns work) | its arithmetic needs the new work term: 31 + 193 ≈ 224 ns, so 250 ns is now *tight* rather than generous | same, and it is worth noting either way |
| `PHASE3.md` §2's budget table row "Phase 1 target: native depth-3 lookup, 150 ns" | re-cut | re-labelled |
| `docs/design/fast-path.md` §9/§11 ("fails by ~2×") | on this fixture it is 1.28× (ScLerp) / 1.52× (LerpSlerp) over §11.3; that document measured a different one (capacity 4096) and its own numbers are not re-taken here — worth a note, not a rewrite | unchanged |

**Two things are true under both readings and are worth separating from the
choice.** The gate cannot be *evaluated* on this host under either — `xtask`
already prints `UNAVAILABLE` and says why. And whatever number is chosen, the row
it gates is a warm-cursor repeated-stamp best case (*What this number is not*),
so a gate cut to within a few percent of it would be measuring the loop shape as
much as the engine.

## Rationale

**Why not just leave the on-grid stamp and note the caveat.** It preserves the
same misleading number in the file everyone quotes. The gate's purpose is to fail
when a change makes lookup slower; a gate that exercises neither interpolator
cannot fail for the most likely cause of a slowdown, and the ScLerp/LerpSlerp
rows actively misinform — they invite the conclusion that the interpolator choice
does not matter, which is the opposite of D5.

**Why not add an off-grid benchmark alongside the on-grid one.** Considered, and
it is a reasonable addition later. It does not resolve this record, because §11.3
still has to say which number is the gate.

**Why not treat the exact-hit path as the realistic case.** It is not. A consumer
queries at a sensor stamp or a control-loop tick; landing exactly on a publisher's
grid is the coincidence, not the norm. The on-grid case is worth keeping as a
separate labelled row precisely because it is the best case.

## Consequences

- The gate becomes able to fail, and will need a real baseline.
- §11.3's numbers stop being comparable to any figure published before this
  change. Anything quoting "150 ns at depth 3" — README, docs, talks — needs the
  same amendment, or it becomes the next stale claim of the kind this audit spent
  its time finding.
- The existing `p50 > 0 && p50 < 1 ms` assertion still passes, so no test had to
  change to land item 1 — confirmed by running it. (It lives in `report.rs`'s
  `the_lookup_measurement_reports_every_sample_and_ordered_percentiles`, not in
  `tests/bench_report_cli.rs` as this line first said; that file asserts the
  CLI's argument handling and never reaches a percentile.)
- **`PHASE3.md` §6.1's `NS_PER_STEP_ESTIMATE` moved with the re-baseline**, 55 →
  64 ns/step, because `API.md` §3.4 is NORMATIVE that it moves in this commit.
  That is a *consequence of the measurement*, not of the threshold choice, which
  is why it did not wait for the questions below. **That amendment is now the
  single account of the constant**: `API.md` §3.4 is the NORMATIVE instruction
  and points at it, `PHASE5.md` §4.4's status block was a third write-up and has
  been reduced to a pointer, and this record does not restate the arithmetic
  either. Three copies of one number is how two of them went stale.
- **The `lookup_latency` row's published note now states its dynamic-step count
  and its stamp regime**, which `PHASE1.md` §11.3 requires NORMATIVELY of "every
  reported latency row" and which the shipped note — the only prose a reader of
  `results.json` gets — did not say. It is `report.rs`'s `LOOKUP_NOTE`, and
  `the_lookup_row_note_states_what_phase1_requires` checks the count against the
  plan the measurement actually compiles and the off-grid property against the
  stamp it actually queries, so neither claim can go stale silently. The
  committed `baseline/results.json` still carries the older prose; the baseline
  comparison ignores `note` by design (`baseline.rs`'s module table), so that is
  cosmetic and is not worth a `bench-baseline-update` on a host where the row is
  `unavailable`.

## Implementation plan

1. Fix the stamp at both call sites; document `NOW_NS` — ✅ done, verified by the
   ScLerp/LerpSlerp gap opening from 1.00 to 1.27 and by
   `the_latency_query_stamp_is_off_every_dynamic_grid`.
2. Re-baseline once — ✅ done (*Re-baseline* above). `just bench-baseline-update`
   was **not** run and the committed `results.json` is untouched:
   `lookup_latency` is `unavailable` on this host in the committed baseline *and*
   in a fresh report taken after the change (verified, not assumed), so the only
   diff it could produce is provenance churn. `bench_report --check-baseline`
   passes — *"PASS — 1 directional metric held"* — run without `--embed-cost`,
   which both sides of the gate normally pass and which was skipped here only
   because that recipe builds a third 166 MiB target tree and this host is at
   98 % disk. **On a host that passes `Fitness::probe` this step is real and is
   still owed**, and it is the step that turns these numbers into a baseline
   rather than a record.
3. ✅ **Amend `docs/PHASE1.md` §11.3** per *Resolution*: the two absolute ceilings
   with the on-grid history beside them, the regression clause, the NORMATIVE
   call-shape sentence, and the re-cut third criterion — verified by reading it
   against *Resolution* and by `xtask`'s gate line no longer naming 150/100.
4. ✅ **Update `xtask/src/main.rs`'s printed gate line** to the new numbers and to
   the 1→4 scaling criterion, keeping `UNAVAILABLE` where the host cannot decide
   a row — verified by `cargo xtask bench-gate` output.
5. ✅ **Add the `depth3/sclerp/exact_hit` row** to `benches/lookup.rs`, labelled as
   the on-grid best case — verified by `cargo bench -p tf_tree_bench --bench
   lookup` listing it, and by its ~4.7× gap to the off-grid row.
6. ⛔ **Re-baseline on a host that passes `Fitness::probe`.** Still owed, and still
   not this host; see step 2's note.

## Resolution

All four questions are answered here from the numbers already in this record.
Nothing below rests on a measurement that has not been taken.

### Q3 first, because it decides what the other numbers mean: **the budget is
written against the inlined call shape.**

Question 2 cannot be answered before this one, and this one turns out to have a
better answer than the choice it was posed as.

*Corroboration from a second harness* measures the same depth-3 `LerpSlerp` fold
at **147.6 ns** inlined and **200.3 ns** behind `#[inline(never)]` — the call
alone is ~51.5 ns, which is larger than the headroom question 2 is about. Two
reasons settle it for the inlined shape:

1. **It is what §11.1's harness already measures.** The primary re-baseline gives
   `depth3/lerpslerp` a median of 151.8 ns, against the second harness's 147.6 ns
   inlined and 200.3 ns not — so the fixture rows this gate is stated over are
   the inlined shape. Choosing the other one would mean every number in the
   *Re-baseline* table is the wrong baseline for the gate written from it, and a
   re-measurement nobody has taken. **A threshold set against a number that does
   not exist yet is how this record's original 150 ns came about.**
2. **The boundary already has its own gate, and it is a better one.**
   `PHASE5.md` §9.2's `embedding_cross_crate` row measures exactly the
   out-of-crate, non-inlined cost and gates it at **5 %** — currently reporting
   1.250–1.254× and therefore *failing*. Folding that same cost into §11.3 would
   put two independent quantities behind one number, so a movement could not be
   attributed to either: the engine got slower, or the boundary did, and the gate
   could not say which. **§11.3 gates the engine; §9.2 gates the boundary.**

This is a change from the direction the plan for this work first proposed, which
was `#[inline(never)]` on the argument that it is the shape an embedder gets. It
is the shape an embedder gets — and that is §9.2's row, measured there, against
a criterion written for it.

**NORMATIVE:** every latency row §11.3 gates is measured with the fold inlined
into its caller, as `benches/lookup.rs` measures it today. A row measured behind
a non-inlinable call is a `PHASE5.md` §9.2 row and is stated there.

### Q1: **both (a) and (b)**, with different jobs

They were posed as alternatives and they are not. Each covers the other's blind
spot:

- **(a) an absolute ceiling** — what the engine may cost, ever. It catches a
  catastrophic regression that (b) would absorb if a baseline were regenerated
  carelessly, and it is the number a reader wants when asking "is this fast?".
- **(b) regression-from-baseline** — the gate that actually bites, at 25 %
  per percentile. `bench_report`'s `lookup_latency` row already implements
  exactly this (`LATENCY_SLACK = 0.25`), so (b) is not new machinery; ratifying
  it makes §11.3 agree with what the tool has been doing.

### Q2: **≤ 300 ns ScLerp, ≤ 220 ns LerpSlerp, stated per interpolator**

The medians are 192.7 and 151.8, but **the ceiling has to clear the band, not the
median**, and the bands are 190.4–268.9 and 146.2–190.4 over nine runs each. A
ceiling at the 250/200 the *Decision* sketched would sit **below** ScLerp's
observed maximum of 268.9 — it would fail roughly one run in nine on an
unchanged engine, and a gate that flaps is a gate people learn to ignore
(`justfile`'s performance-suite header makes the same argument for the same
reason).

So the ceiling is set ~1.12× above each observed maximum: **300 ns** over 268.9,
**220 ns** over 190.4. Per interpolator, because they are not the same
measurement — the ScLerp/LerpSlerp gap is 1.27× off-grid and was 1.00× on-grid,
and one number for the slower one would leave `LerpSlerp` ungated in practice.

**This is not a loosening of a gate that was being met.** The 150/100 figures
were chosen before anything had measured interpolation, and the row they were
checked against took the exact-hit branch on all four dynamic edges — it timed
`bracket` plus a seqlock read. The old number was never about this work, so this
is the first setting of the threshold, not a concession.

### Q4: **yes — keep `depth3/sclerp/exact_hit` as a labelled row**

The objection recorded against it is real: a benchmark row that exists is a row
the baseline gate then has to carry. It loses to what the row buys. The 4.7×
between the on-grid and off-grid regimes is the exact property that hid this
defect for the life of the gate, and a labelled row makes it a documented
characteristic instead of a trap waiting for the next person who picks a round
stamp. It is gated like any other row; if it moves, that is worth knowing too.

### And the third criterion, folded in here because it is the same gate

`PHASE1.md` §11.3's third criterion — *"read throughput scales at least 6× from
1 to 8 threads"* — **cannot be evaluated on any host this project has.** Eight
threads on four physical cores can only pass 6× through SMT, which is why the
measured 5.35–5.62× (criterion benches) and 5.73× / 5.20× (`contended_scaling`,
pinned, with four writers) are neither a pass nor a fail. It has never been a
gate; it has been an `UNAVAILABLE` line.

It is re-cut to what a four-core host can decide, in two parts:

1. **tf_tree scales ≥ 2.5× from 1 to 4 threads** on ≥ 4 physical cores. Measured
   2.79× (recorded stream) and 3.09× (fixture), so it passes with margin and a
   regression to 2× fails it. Perfect scaling here is 4×, and the remaining core
   is running the writers and the OS.
2. **tf_tree's 1→4 scaling factor is ≥ 5× tf2's** over the same sweep. Measured
   2.79 / 0.36 = **7.75×**. This is a `Sensitivity::Ratio` row.

Part 2 is the one that carries the argument. §11.3's own prose says the criterion
exists because *"if tf_tree scales cleanly, the value proposition is your
perception nodes stop contending"* — and the decisive fact is not where tf_tree
lands against an absolute, it is that **tf2 goes backwards** (0.31× at 8 threads,
reproduced by a pure C++ control with our binding deleted). A ratio states that
and an absolute cannot.

The 8-thread ≥ 6× figure is **retained as informational**, with its measured
5.35–5.62× and an explicit note that it is unmeasurable below 8 physical cores.
It is not deleted: it is the number to re-take if this project ever gets that
host.

### Verification of the ratified numbers

Run at ratification, `cargo bench -p tf_tree_bench --bench lookup`, this host,
short measurement window (1 s warm-up / 3 s), criterion's median column:

| Row | Measured | Record's earlier figure | Ceiling | |
| --- | --- | --- | --- | --- |
| `lookup/depth1/sclerp` | 68.17 ns | 69.6 (68.2–74.6) | — | — |
| `lookup/depth3/sclerp` | **193.76 ns** | 192.7 (190.4–268.9) | ≤ 300 ns | **PASS** |
| `lookup/depth3/lerpslerp` | **146.97 ns** | 151.8 (146.2–190.4) | ≤ 220 ns | **PASS** |
| `lookup/depth6/sclerp` | 133.10 ns | 134.5 (132.9–165.3) | — | — |
| `lookup/depth3/sclerp/exact_hit` | **40.11 ns** | 40.8 on-grid | not a gate row | — |

Every row reproduces the *Re-baseline* table it was set from, and the new
`exact_hit` row reproduces the on-grid figure this record was written about:
193.76 / 40.11 = **4.83×** between the two regimes, against the 4.7× recorded
earlier.

**The re-cut ceilings therefore pass on this host**, which is the point of
re-cutting them — the gate is now one that can be met and can fail, rather than
one that had never measured the work it named. These numbers remain *indicative*:
the host fails `Fitness::probe` (4 physical cores, SMT on, unreadable governor),
so they are not claims, and step 6 below is still owed.

## Open questions

**None. All four are answered in *Resolution* above**, and the record is `ready`.
For the reader who arrives at this heading first:

1. *(a) or (b)?* — **both**, with different jobs: an absolute ceiling for what
   the engine may ever cost, and regression-from-baseline as the gate that bites.
2. *What headroom?* — **≤ 300 ns ScLerp / ≤ 220 ns LerpSlerp**, per interpolator,
   set ~1.12× above each *observed maximum* (268.9 and 190.4) rather than above
   the median, so it cannot flap on an unchanged engine.
3. *Which call shape?* — **inlined**, because that is what §11.1's harness
   already measures and because `PHASE5.md` §9.2's `embedding_cross_crate` gates
   the non-inlined boundary separately and better.
4. *Keep the on-grid row?* — **yes**, as a labelled `depth3/sclerp/exact_hit`.
