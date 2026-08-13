# Evidence register: which artifact produced which number, and how to re-run it

Every performance claim in `docs/` comes from something you can execute. This
file is the index of those things, and `just evidence-audit` enforces it.

## Why this file exists

`docs/PHASE4.md` recorded §7 gate criterion 1 as **"1.020×, PASS"** for months
while the benchmark that produces it — `crates/tf_tree_c/examples/abi_cost.rs` —
printed **FAIL** at 1.34–1.46×. Nothing caught it, because *the example was
executed by no recipe and no workflow*. It was named in one `justfile` comment.

That is not a benchmark bug. It is a **class** of bug: a document cites a number,
the artifact that produced it drifts or regresses, and nobody finds out because
no command re-derives it. An audit of the workspace found the same shape in
roughly a dozen other places.

So each row below declares what an artifact is **for**, which decides what is
owed:

| Kind | Meaning | Obligation |
|---|---|---|
| **gate** | a document states a pass/fail criterion from it | must have a named recipe; `evidence-audit` fails otherwise |
| **probe** | a one-off diagnostic; its finding is history, not a live claim | must have a documented command, and the citing prose must read as past tense |

A probe is not second-class. Most of the sharpest findings in
`docs/benchmarks/tf2.md` came from one — the point is only that a probe's number
is a *record of a measurement taken*, not a *claim being maintained*, and prose
must not blur the two.

## Gates — a recipe runs these

| Artifact | Recipe | Criterion |
|---|---|---|
| `tf_tree_c/examples/abi_cost.rs` | `just abi-cost` | PHASE4 §7 gate 1: `tft_plan_at` < 1.05× native. **Currently FAILING at 1.34–1.46×** — see PHASE4 §7 |
| `tf_tree_c/tests/cpp/bench.cpp` | `just cpp-bench` | PHASE4 §7 gate 2: C++ wrapper < 1.02× the raw C ABI |
| `tf_tree_bench/src/bin/bench_report.rs` | `just bench-check` / `just tf2-bench-check` | PHASE5 §10 regression gate against the committed baselines |
| `tf_tree_bench/src/bin/embed_cost.rs` | `just embed-cost-check` | PHASE5 §9.2 embedding cost |
| `xtask bench-gate` | `just bench` | PHASE1 §11.3 criteria |
| `tf_tree_bench/src/bin/frozen_workers.rs` | `just gate4` | PHASE5 §12 gate 4: 16 workers on one `.tft`, total Pss ≤ 1.2× one worker. **PASSES at 1.024×** |
| `docker/tf2/native_footprint.cpp` | `just tf2-native-footprint` | The memory comparison with no binding on either side. Not a pass/fail gate — it refuses to print a quotient if the two arms stored different sample counts |
| `tf_tree_bench/src/backing.rs` | `just abi-split` | not a gate — reports the ABI ladder; listed here because PHASE4 §7 cites its guard rows |

## Probes — findings recorded, re-runnable on demand

Each is `cargo run --release -p <pkg> --example <name>` unless noted. None is
gated; each backs prose that should read as "was measured", not "is".

| Artifact | What it established | Where cited |
|---|---|---|
| `guard_cost` | `Tree::guard()` per call vs hoisted, heap tree, one stamp | tf2.md, PHASE3 |
| `cost_model` | per-step cost of a dynamic chain in-process (217 ns) | tf2.md ×2 |
| `heap_vs_shared` | heap vs `MAP_SHARED` vs attached RO (51.1/51.3/51.2 ns). **Its stamp is an exact grid hit — see the file header; superseded for the mapping question by `just abi-split`** | tf2.md ×2 |
| `step_cost` | per-edge fold cost | tf2.md ×2 |
| `interp_cost` | interpolation policy costs | (uncited) |
| `adjoint_cost` | adjoint chain cost for derivatives | PHASE4 |
| `deriv_cost` | `sample_with_derivatives` cost | PHASE4 |
| `autovec_probe` | SLP/autovectorisation of the fold — `0016`'s amendment | 0016 |
| `search_seed` | ring search seeding strategy | PHASE1 |
| `contended_search` | search under a concurrent writer | PHASE1 |
| `hugepage_grant` | whether THP is granted to the memfd mapping. Needs `shm` | PHASE2 |
| `counter_cost` (bin) | what §5's diagnostic counters cost. Needs `--features counters` both ways | PHASE5 §5 |
| `py_parity` (bench) | Python binding parity. Needs the uv toolchain, `just py-*` | PHASE3 |
| `query_mix` (bench) | mixed query workload | (uncited) |
| `at_many` (bench) | batch-fold throughput | (uncited; the *method* `Plan::at_many` is cited widely — different thing) |
| `read_scaling` (bench) | PHASE1 §11.3's read-scaling curve | xtask bench-gate names it as a manual follow-up |
| `gen_zstd_conformance` | generates the ingest conformance corpus | (uncited, generator) |

**The criterion benches are compiled but never executed by anything.**
`cargo xtask bench-gate` runs `cargo bench -p tf_tree_bench --no-run` — it links
them, which stops them rotting into non-compiling code, and then does not run
them. `read_scaling` in particular backs PHASE1 §11.3's read-scaling criterion,
and `xtask` prints *"measure indicatively with `cargo bench --bench
read_scaling`"* — an instruction to a human, not an execution. That is the
`abi_cost` shape one step short of the same failure, and it is recorded here
rather than fixed because the §11.3 criterion is itself mid-recut (`0013`).

**`counter_cost` deserves a promotion and has not had one.** `docs/PHASE4.md` §7
now records that the diagnostic counters roughly double the per-call `Guard`
(+35.4 ns against +16.8 ns with them off). That is a live claim about a shipped
default, not history, and `0022` question 1 turns on it. It should become a gate
when `0022` moves to `ready`.

## What `just evidence-audit` checks

For every `example`/`bin`/`bench` target in the workspace: if it is **not**
executed by any recipe, workflow, test-runner script or `CARGO_BIN_EXE_*` spawn,
then it must appear in a table above. A new artifact that a document starts
citing, with no recipe and no row here, fails the audit.

The audit does **not** check that a probe's recorded number is still true — that
is what makes it a probe. It checks only that somebody can find out.
