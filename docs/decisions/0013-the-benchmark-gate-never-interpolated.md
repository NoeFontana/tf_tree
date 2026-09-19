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

The lookup benchmark and gate never measured interpolation. `fixture::NOW_NS` is
an exact multiple of all four dynamic edge periods, so every edge took the
exact-hit branch in `SampleRing::sample` and `I::eval` never ran. No code got
slower; the honest number exceeds `docs/PHASE1.md` §11.3's 150 ns depth-3 budget,
so what §11.3's numbers should be is a normative change and belongs here.

## Re-baseline

`benches/lookup.rs` queries `fixture::QUERY_NS` (`NOW_NS − 500 µs`, off-grid on
all four periods); `NOW_NS` stays on-grid for the history window.
`fixture::tests::the_latency_query_stamp_is_off_every_dynamic_grid` asserts both.

Protocol: criterion 0.5.1 defaults, `[profile.bench]` thin LTO, `taskset -c 2`, on
a host that fails `Fitness::probe`, so nothing here is a claim in `PHASE5.md`
§9.3's sense.

Criterion point estimate, ns; median (min–max) over `n` alternated runs:

| Row | on-grid (superseded) | **off-grid (the honest cost)** | n | ratio |
| --- | --- | --- | --- | --- |
| `depth1/sclerp` | 16.8 (16.8–17.0) | **69.6** (68.2–74.6) | 6 | 4.1× |
| `depth3/sclerp` | 40.8 (40.3–47.5) | **192.7** (190.4–268.9) | 9 | 4.7× |
| `depth3/lerpslerp` | 40.8 (40.4–41.4) | **151.8** (146.2–190.4) | 9 | 3.7× |
| `depth6/sclerp` | 31.4 (31.1–34.2) | **134.5** (132.9–165.3) | 6 | 4.3× |

Depth 3 is three dynamic steps; depth 6 has four static edges that fold, leaving
two. §11.3's NORMATIVE "every reported latency row must state its dynamic-step
count" exists for this; the shapes are asserted by
`fixture::tests::the_benched_paths_have_the_step_counts_the_baseline_assumes`.

`PHASE3.md` §6.1's `NS_PER_STEP_ESTIMATE` is one measured median over one
asserted step count (192.7 / 3 = 64.2), re-derived 55 → **64**; that amendment is
the single account of the constant.

## Decision

Items 1 and 2 landed; item 3 is answered in [*Resolution*](#resolution), which is
what `PHASE1.md` §11.3 carries.

1. **Fix the stamp at both call sites.** ✅ **Done.** `report.rs`'s
   `measure_lookup_latency` and `benches/lookup.rs` take `fixture::QUERY_NS`.
2. **Re-baseline once.** ✅ **Done** (*Re-baseline*). The committed `results.json`
   is unmoved because `lookup_latency` is `unavailable` on this host either way.
3. **Amend `docs/PHASE1.md` §11.3** to budgets justified by the measurement.
   ✅ **Done at ratification**: ≤ 300 ns ScLerp / ≤ 220 ns LerpSlerp with the 25 %
   baseline clause, the NORMATIVE inlined-call-shape sentence, and the re-cut 1→4
   scaling criterion.

## Consequences

- **`PHASE3.md` §6.1's `NS_PER_STEP_ESTIMATE` moved with the re-baseline**, 55 →
  64 ns/step, because `API.md` §3.4 is NORMATIVE that it moves in this commit.
- **`LOOKUP_NOTE`** (`report.rs`) states the row's dynamic-step count and stamp
  regime, checked by `the_lookup_row_note_states_what_phase1_requires`.

## Implementation plan

1. ✅ Fix the stamp at both call sites; document `NOW_NS`.
2. ✅ Re-baseline once. `just bench-baseline-update` was **not** run:
   `lookup_latency` is `unavailable` here. **On a host whose `fair_for_timing` is
   true this step is real and still owed.**
3. ✅ Amend `docs/PHASE1.md` §11.3 per *Resolution*.
4. ✅ Update `xtask/src/main.rs`'s printed gate line to the new numbers and the
   1→4 scaling criterion, keeping `UNAVAILABLE` where the host cannot decide.
5. ✅ Add the `depth3/sclerp/exact_hit` row to `benches/lookup.rs`.
6. ⛔ **Re-baseline on a host whose `fair_for_timing` is true.** Still owed: this
   host has SMT on and an unreadable governor. A fit host changes two of the nine
   `unavailable` baseline rows, `lookup_latency` and `embedding_cross_crate`.

   **Step 7 is a proposal, not ratified.**

7. **The §11.3 ceilings are one-sided budgets and can be gated on this host
   today.** "`0013` item 3" in `PHASE1.md` means the re-baseline, step 6; the
   thresholds themselves landed. `PHASE5.md` §9.3's *one-sided-budget* amendment
   licenses gating a ceiling on an unfit host. It is **not** a `bench_report` row
   (that is two-sided against a baseline; step 6 stands). `cargo xtask bench-gate`
   (`just bench`) already is §11.3's gate and prints both ceilings as
   `UNAVAILABLE`, so the proposal is to **change that row** to print the worst
   reading against the budget plus the fitness verdict, not to add a second
   holder (`docs/PROJECT.md` §6).

## Resolution

### Q3 first: **the budget is written against the inlined call shape.**

1. §11.1's harness already measures the inlined shape, and every number in
   *Re-baseline* is against it.
2. The boundary has its own gate: `PHASE5.md` §9.2's `embedding_cross_crate`
   measures the out-of-crate, non-inlined cost at **5 %**. **§11.3 gates the
   engine; §9.2 gates the boundary.**

**NORMATIVE:** every latency row §11.3 gates is measured with the fold inlined
into its caller, as `benches/lookup.rs` measures it. A row measured behind a
non-inlinable call is a `PHASE5.md` §9.2 row.

### Q1: **both (a) and (b)**, with different jobs

- **(a) an absolute ceiling** — what the engine may ever cost.
- **(b) regression-from-baseline** — the gate that bites, 25 % per percentile;
  `bench_report`'s `lookup_latency` row implements it (`LATENCY_SLACK = 0.25`).

### Q2: **≤ 300 ns ScLerp, ≤ 220 ns LerpSlerp, stated per interpolator**

The ceiling must clear the observed band, not the median: 190.4–268.9 and
146.2–190.4 over nine runs each, so ~1.12× above each observed maximum. Per
interpolator because the gap is 1.27× off-grid.

### Q4: **yes — keep `depth3/sclerp/exact_hit` as a labelled row**

The 4.7× between the regimes is the property that hid this defect.

### The third criterion

`PHASE1.md` §11.3's "read throughput scales at least 6× from 1 to 8 threads"
cannot be evaluated on any host this project has. It is re-cut in two parts:

1. **tf_tree scales ≥ 2.5× from 1 to 4 threads** on ≥ 4 physical cores.
2. **tf_tree's 1→4 scaling factor is ≥ 5× tf2's** over the same sweep. A
   `Sensitivity::Ratio` row; tf2 goes backwards under threads, which a ratio
   states where an absolute cannot.

The 8-thread ≥ 6× figure is retained as informational and unmeasurable below 8
physical cores.

## Open questions

**None. All four are answered in *Resolution* above.**
