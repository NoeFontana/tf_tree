# 0013: The benchmark gate never interpolated, and what §11.3's numbers should be

**Status:** ready — items 1 and 2 of the *Decision* have landed (see the process
note below) and all four open questions are resolved in *Resolution* at the end
of this record. **This line used to end *"and item 3 (the §11.3 thresholds) is
the remaining work"*, and that was true of no reading of "item 3"** — plan step 7
sets out the three meanings the phrase had acquired and names this line as the
stale one. *Decision* item 3 is marked ✅ Done at ratification and the process
note says §11.3 *is* amended; `docs/PHASE1.md` reads "`0013` item 3" as the
**re-baseline**, which is plan step 6. What actually remains is **step 6's
re-baseline**, and **step 7, which is a proposal and not yet ratified**: it would
change `cargo xtask bench-gate`'s §11.3 row from `UNAVAILABLE` to a one-sided
budget verdict. Correcting the line rather than leaving it is the point of the
record — its subject is exactly this drift
**Owner:** @NoeFontana
**Implementation:** `crates/tf_tree_bench` (`fixture::QUERY_NS`, both call sites,
one test), `crates/tf_tree_py` (`NS_PER_STEP_ESTIMATE` 55 → 64, per `API.md`
§3.4), `docs/PHASE3.md` §6.1, `docs/API.md` §2.3/§3.1/§3.4/§6 row 10

## Context

The lookup benchmark and go/no-go gate never measured interpolation.
`fixture::NOW_NS` (`9_900_000_000`) is an exact multiple of all four dynamic
edge periods (50, 200, 1000 and 10 Hz), so every edge took the exact-hit branch
in `SampleRing::sample` and `I::eval` never ran. The gate timed `bracket` plus
`read_slot`. Both `benches/lookup.rs` and `report.rs`'s `measure_lookup_latency`
built their stamp from that constant. The tell: ScLerp and LerpSlerp came out
within ~1% of each other while §11.3 budgets them 150 and 100 ns.

No code got slower; the benchmark never asked for the work. Fixing the stamp
makes the honest number exceed `docs/PHASE1.md` §11.3's 150 ns depth-3 budget,
so what §11.3's numbers should be is a normative change and belongs here.

## Re-baseline

`benches/lookup.rs` queries `fixture::QUERY_NS` (`NOW_NS − 500 µs`, off-grid on
all four periods); `NOW_NS` stays on-grid for the history window.
`fixture::tests::the_latency_query_stamp_is_off_every_dynamic_grid` asserts both.
`--quick` is not the mode to baseline in (it reads 46–71 % high); numbers below
use criterion 0.5.1 defaults (3 s discarded warm-up, 100 samples over 5 s).

Protocol: shipped on-grid and off-grid binaries alternated; `[profile.bench]`
(`lto = "thin"`, `codegen-units = 1`); tree warm, plan and `Guard` built outside
the loop (best case for the bracket cursor); `taskset -c 2`. Host: the dev VM,
AMD EPYC-Milan, 4 physical cores, SMT on, governor unreadable; it fails
`Fitness::probe`, so nothing here is a claim in `PHASE5.md` §9.3's sense.

Criterion point estimate, ns; median (min–max) over `n` alternated runs:

| Row | on-grid (superseded) | **off-grid (the honest cost)** | n | ratio |
| --- | --- | --- | --- | --- |
| `depth1/sclerp` | 16.8 (16.8–17.0) | **69.6** (68.2–74.6) | 6 | 4.1× |
| `depth3/sclerp` | 40.8 (40.3–47.5) | **192.7** (190.4–268.9) | 9 | 4.7× |
| `depth3/lerpslerp` | 40.8 (40.4–41.4) | **151.8** (146.2–190.4) | 9 | 3.7× |
| `depth6/sclerp` | 31.4 (31.1–34.2) | **134.5** (132.9–165.3) | 6 | 4.3× |

Depth 3 is three dynamic steps; depth 1 is one; depth 6 is six edges of which
four are static and fold, leaving two dynamic steps, which is why it is cheaper
than depth 3. §11.3's NORMATIVE "every reported latency row must state its
dynamic-step count" exists for this row; the shapes are asserted by
`fixture::tests::the_benched_paths_have_the_step_counts_the_baseline_assumes`.

Off-grid the ScLerp/LerpSlerp gap is 192.7 / 151.8 = **1.27**, against 1.00
on-grid.

`PHASE3.md` §6.1's `NS_PER_STEP_ESTIMATE` is one measured median over one
asserted step count (192.7 / 3 = 64.2), re-derived 55 → **64**; that amendment is
the single account of the constant. A three-row per-step decomposition is exactly
determined and cannot fail; do not quote it as a measurement.

**Spread.** A difference under ~10 % is not resolvable on this host, and a
moving-stamp contrast was measured and resolves to nothing. How much of the
192.7 is the warm cursor is not measured: no benchmark moves the cursor hard
enough to price it.

### Corroboration from a second harness, and the 31 % it took to get it

`just embed-cost`'s probe measures the same depth-3 fold with a different loop.
Both harnesses were compared at identical codegen (`lto = "thin"`,
`codegen-units = 1`).

| harness | interpolator | ns | n |
| --- | --- | --- | --- |
| criterion `lookup/depth3/lerpslerp` | LerpSlerp | **147.6** (146.6–147.8) | 4 runs |
| `embed_cost` **in**-crate, `[profile.release]` | LerpSlerp | **194.0** | 9 rounds |
| `embed_cost` **out**-of-crate, `[profile.release]` | LerpSlerp | **193.0** | 9 rounds |

The 31 % is the `#[inline(never)]`, measured by changing `benches/lookup.rs`
cumulatively, one difference at a time:

| `benches/lookup.rs`, changed to… | ns | runs | vs shipped |
| --- | --- | --- | --- |
| as shipped | 147.6 | 4 | — |
| …+ `black_box` on `plan` and `guard`, not only the stamp | 148.5 | 2 | +0.6 % |
| …+ the probe's 1024-stamp off-grid sweep instead of one repeated stamp | 148.8 | 1 | +0.9 % |
| …+ the call behind `#[inline(never)]`, as the probe's `one` is | **200.3** (198.7–200.3) | 3 | **+36 %** |

Once call shapes match, the two harnesses agree to 3.8 % (200.3 vs 193.0). The
non-inlined call is worth ~51.5 ns, more than any headroom under discussion; that
is *Resolution* Q3. `embed::measure_with` hard-codes `LerpSlerp`, so ScLerp
cannot be corroborated.

### What this number is not

- **Not a p50 under load**: single-threaded, hot-cache, warm-cursor,
  repeated-stamp, quiescent tree. The tail (`PHASE2.md` §7.1) is not measured.
- **Not a claim**: the host fails `Fitness::probe`; `bench_report`'s
  `lookup_latency` row is `unavailable` on it.
- **Not a regression.**

## Decision

Items 1 and 2 landed; item 3 is answered in [*Resolution*](#resolution), which is
what `PHASE1.md` §11.3 carries.

1. **Fix the stamp at both call sites.** ✅ **Done.** `report.rs`'s
   `measure_lookup_latency` and `benches/lookup.rs` take `fixture::QUERY_NS`;
   `NOW_NS`'s doc records that it is deliberately on-grid, and a test asserts both.
2. **Re-baseline once**, after the other measurement-moving changes landed.
   ✅ **Done** (*Re-baseline*). The committed `results.json` is unmoved because
   `lookup_latency` is `unavailable` on this host either way.
3. **Amend `docs/PHASE1.md` §11.3** to budgets justified by the measurement, with
   the on-grid history recorded inline. ✅ **Done at ratification**: ≤ 300 ns
   ScLerp / ≤ 220 ns LerpSlerp with the 25 % baseline clause, the NORMATIVE
   inlined-call-shape sentence, and the re-cut 1→4 scaling criterion. Two readings
   were posed: **(a)** re-cut the absolute budget to the measured cost;
   **(b)** keep 150 ns as a target and gate on regression-from-baseline.
   *Resolution* Q1 takes both.

## Rationale

- **Not leaving the on-grid stamp with a caveat**: a gate that exercises neither
  interpolator cannot fail for the likeliest cause of a slowdown, and the
  ScLerp/LerpSlerp rows invite the conclusion that the interpolator choice does
  not matter, the opposite of D5.
- **Not treating the exact-hit path as realistic**: a consumer queries at a
  sensor or control tick; landing on a publisher's grid is the coincidence. It is
  kept as a separate labelled best-case row.

## Consequences

- The gate can fail and needs a real baseline.
- §11.3's numbers are not comparable to any figure published before this change;
  anything quoting "150 ns at depth 3" needs the same amendment.
- **`PHASE3.md` §6.1's `NS_PER_STEP_ESTIMATE` moved with the re-baseline**, 55 →
  64 ns/step, because `API.md` §3.4 is NORMATIVE that it moves in this commit.
- **The `lookup_latency` row's note states its dynamic-step count and stamp
  regime**, as §11.3 requires. It is `report.rs`'s `LOOKUP_NOTE`;
  `the_lookup_row_note_states_what_phase1_requires` checks both claims against
  what the measurement compiles and queries. The baseline comparison ignores
  `note` (`baseline.rs`).

## Implementation plan

1. ✅ Fix the stamp at both call sites; document `NOW_NS`.
2. ✅ Re-baseline once. `just bench-baseline-update` was **not** run:
   `lookup_latency` is `unavailable` here in both the committed baseline and a
   fresh report. **On a host whose `fair_for_timing` is true this step is real
   and still owed.**
3. ✅ Amend `docs/PHASE1.md` §11.3 per *Resolution*.
4. ✅ Update `xtask/src/main.rs`'s printed gate line to the new numbers and the
   1→4 scaling criterion, keeping `UNAVAILABLE` where the host cannot decide.
5. ✅ Add the `depth3/sclerp/exact_hit` row to `benches/lookup.rs`.
6. ⛔ **Re-baseline on a host whose `fair_for_timing` is true.** Still owed.
   `Fitness::probe` is four axes:

   | axis | verdict here | what fails it |
   |---|---|---|
   | `fair_for_timing` | **false** | SMT on (8 logical over 4 physical); governor unreadable |
   | `fair_for_ratios` | **true** | nothing; `busy_fraction` 0.000–0.021 against `mp::QUIET_ENOUGH` = 0.10 |
   | `fair_for_memory` | **true** | nothing |
   | `enough_cores` | true at 1 consumer, false at 4 and 16 | 4 physical cores |

   The committed baseline records `fair_for_ratios: false` because the machine
   was 15 % busy at that moment, not because of the machine.

   Of the nine `unavailable` rows in the committed baseline a fit host changes
   two: `lookup_latency` and `embedding_cross_crate`. The rest are not measured
   in `bench_report`'s one process on any host (`cpu_per_consumer`,
   `publish_to_visible`, `scaling_curve`, `total_rss_n_consumers`,
   `tft_16_workers_rss`, `tft_open_vs_bag_parse`; `just gate2` / `just gate4`
   take the last two) or need ROS 2 (`lookup_ratio_vs_tf2`). The machine needed
   has **SMT disabled and a readable cpufreq governor**. Building with
   `--features shm` does not change the two `.tft` rows.

   **Step 7 is a proposal, not ratified.**

7. **The §11.3 ceilings are one-sided budgets and can be gated on this host
   today.** "`0013` item 3" in `PHASE1.md` means the re-baseline, step 6; the
   thresholds themselves landed. `PHASE5.md` §9.3's *one-sided-budget* amendment
   licenses gating a ceiling on an unfit host, since every failing check can only
   lengthen a duration. It is **not** a `bench_report` row (that is two-sided
   against a baseline; step 6 stands); it is a ceiling held in its own binary and
   recipe like `just gate2`, printing margin and fitness reasons. `cargo xtask
   bench-gate` (`just bench`) already is §11.3's gate and prints both ceilings as
   `UNAVAILABLE`, so the proposal is to **change that row** to print the worst
   reading against the budget plus the fitness verdict, not to add a second
   holder (`docs/PROJECT.md` §6).

## Resolution

### Q3 first: **the budget is written against the inlined call shape.**

The same fold measures 147.6 ns inlined and 200.3 ns behind `#[inline(never)]`;
the ~51.5 ns call is larger than the headroom Q2 is about.

1. §11.1's harness already measures the inlined shape, and every number in
   *Re-baseline* is against it. A threshold set against a number nobody has
   measured is how the original 150 ns came about.
2. The boundary has its own gate: `PHASE5.md` §9.2's `embedding_cross_crate`
   measures the out-of-crate, non-inlined cost at **5 %**. Folding it into §11.3
   would put two independent quantities behind one number. **§11.3 gates the
   engine; §9.2 gates the boundary.**

**NORMATIVE:** every latency row §11.3 gates is measured with the fold inlined
into its caller, as `benches/lookup.rs` measures it. A row measured behind a
non-inlinable call is a `PHASE5.md` §9.2 row.

### Q1: **both (a) and (b)**, with different jobs

- **(a) an absolute ceiling** — what the engine may ever cost; catches a
  regression a carelessly regenerated baseline would absorb.
- **(b) regression-from-baseline** — the gate that bites, 25 % per percentile;
  `bench_report`'s `lookup_latency` row already implements it
  (`LATENCY_SLACK = 0.25`).

### Q2: **≤ 300 ns ScLerp, ≤ 220 ns LerpSlerp, stated per interpolator**

The ceiling must clear the observed band, not the median: 190.4–268.9 and
146.2–190.4 over nine runs each. 250/200 would sit below ScLerp's observed
maximum and flap on an unchanged engine. So the ceiling is ~1.12× above each
observed maximum: **300 ns** over 268.9, **220 ns** over 190.4. Per interpolator
because the gap is 1.27× off-grid. This is the first setting of the threshold,
not a loosening of a gate that was being met.

### Q4: **yes — keep `depth3/sclerp/exact_hit` as a labelled row**

The 4.7× between the regimes is the property that hid this defect; a labelled row
makes it a documented characteristic. It is gated like any other row.

### The third criterion

`PHASE1.md` §11.3's "read throughput scales at least 6× from 1 to 8 threads"
cannot be evaluated on any host this project has (8 threads on 4 physical cores
passes only through SMT; measured 5.35–5.62× criterion, 5.73× / 5.20×
`contended_scaling`). It is re-cut in two parts:

1. **tf_tree scales ≥ 2.5× from 1 to 4 threads** on ≥ 4 physical cores. Measured
   2.79× (recorded stream) and 3.09× (fixture).
2. **tf_tree's 1→4 scaling factor is ≥ 5× tf2's** over the same sweep. Measured
   2.79 / 0.36 = **7.75×**. A `Sensitivity::Ratio` row. It carries the argument:
   tf2 goes backwards (0.31× at 8 threads, reproduced by a pure C++ control), and
   a ratio states that where an absolute cannot.

The 8-thread ≥ 6× figure is retained as informational and unmeasurable below 8
physical cores.

### Verification of the ratified numbers

`cargo bench -p tf_tree_bench --bench lookup`, this host, 1 s warm-up / 3 s,
median: `depth3/sclerp` **193.76 ns** (ceiling ≤ 300, PASS), `depth3/lerpslerp`
**146.97 ns** (≤ 220, PASS), `depth1/sclerp` 68.17, `depth6/sclerp` 133.10,
`depth3/sclerp/exact_hit` **40.11** (193.76 / 40.11 = 4.83×; not a gate row).
Indicative only: the host fails `Fitness::probe`.

## Open questions

**None. All four are answered in *Resolution* above.**

1. *(a) or (b)?* — **both**.
2. *What headroom?* — **≤ 300 ns ScLerp / ≤ 220 ns LerpSlerp**, ~1.12× above each
   observed maximum.
3. *Which call shape?* — **inlined**; `embedding_cross_crate` gates the boundary.
4. *Keep the on-grid row?* — **yes**, as `depth3/sclerp/exact_hit`.
