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
| `tf_tree_c/examples/abi_cost.rs` | `just abi-cost` | PHASE4 §7 gate 1, re-cut by `0023` (**draft**): four quotients on one interleaved ladder at `[profile.embedder]` — R1 the ABI < 1.10 (**1.025–1.038**), R2 the panic guard < 1.05, R3 the per-call guard < 1.25, and a control at 1 ± 0.02. **All PASS**, and the recipe now exits non-zero when they do not. The `release` arm it also runs is a contrast, not a gate: `lto = "thin"` inlines `tft_plan_at` away. **Which arm gates is no longer decided by the recipe**: `argv[1]`'s profile claim is checked against the target directory the binary is running out of, so swapping the recipe's two lines aborts instead of moving the gate onto the LTO-erased arm |
| `tf_tree_c/tests/cpp/bench.cpp` | `just cpp-bench` | PHASE4 §7 gate 2: C++ wrapper < 1.02× the raw C ABI |
| `tf_tree_bench/src/bin/bench_report.rs` | `just bench-check` / `just tf2-bench-check` | PHASE5 §10 regression gate against the committed baselines |
| `tf_tree_bench/src/bin/embed_cost.rs` | `just embed-cost-check` | PHASE5 §9.2 embedding cost |
| `xtask bench-gate` | `just bench` | PHASE1 §11.3 criteria |
| `tf_tree_bench/src/bin/abi_attached.rs` | `just abi-attached` | Whether the C ABI's cost is the boundary or the language. Answer: the boundary — Rust and C++ agree to 4 ns at `lto = false`. **Also decomposes the per-call `Guard`** (`0022` amendment 4): `fork::generation` +0.2, `Tree::view` +3.7, `Guard::new` +4.8, the fork-safety half +6.7, the cold cursor ~4.8, ~16 unattributed. Reports; does not gate. **Its profile is measured, not passed in**: `--boundary-real` is now a claim checked against `build.rs`'s `OUT_DIR` profile directory and the `lto` the manifest declares for it, and a mismatch is a non-zero exit |
| `tf_tree_bench/src/bin/attach_bench.rs` | `just attach-bench` | PHASE2 §12's attach rows. Reports; does not gate. Supplies `attach_latency`'s figure |
| `tf_tree_bench/src/bin/frozen_workers.rs` | `just gate4` / `just gate4-python` | PHASE5 §12 gate 4: 16 workers on one `.tft`, total Pss ≤ 1.2× one worker. **PASSES at 1.024× — with a *Rust* worker**, whose private cost is `p` = 0.36 MiB. The criterion is `S ≥ 74p`, so the verdict is a function of the worker, and the **second recipe is the same driver with a spawned CPython worker** (`crates/tf_tree_bench/python/gate4_worker.py`): on the same 338 MiB file, deleted and re-frozen by both, it reads **1.804–1.806× — FAIL** across runs (`p` = 13.84–13.86 MiB, wanting ~1 025 MiB of arena) on CPython 3.14.3, against the **1.785×** §12 gate 4's amendment recorded by hand on 3.13.12. Cite the row with the worker's language attached — both recipes print it in their verdict line. **The Python arm reports and does not gate**: criterion 4 is stated over the Rust arm, `gate4-python` exits 0 on the FAIL it prints, and giving the gate a second *gated* arm is a decision that needs a record |
| `docker/tf2/native_footprint.cpp` | `just tf2-native-footprint` | The memory comparison with no binding on either side. Not a pass/fail gate — it refuses to print a quotient if the two arms stored different sample counts |
| `tf_tree_bench/src/backing.rs` (guard rows) | `just guard-cost` | 0022 question 1's 2x2: {release, embedder} x {counters on, off}, on writable arenas |
| `tf_tree_bench/src/backing.rs` | `just abi-split` | not a gate — reports the ABI ladder; listed here because PHASE4 §7 cites its guard rows |
| `tf_tree_bench/benches/push_sampler.rs` | `just push-sampler-cost` | not a gate — reports what [`0036`](../decisions/0036-the-receipt-time-the-format-already-reserved.md)'s clock-offset sampler costs `EdgeWriter::push`, as a **paired delta in one process** against `Publisher::push`. Measured **+1.0–1.1 ns, ~21–23%** on the §11.1 fixture: 5.9–6.1 ns against 4.8–5.0 ns over five sittings before [`0036`](../decisions/0036-the-receipt-time-the-format-already-reserved.md)'s amendment, and **re-derived after it** — the sampler changed shape, gaining a parameter, a domain gate and two cold-path guards — at 5.87–5.88 against 4.85–4.87. The hot path is the same countdown either way. It is a separate artifact from `benches/push.rs` because this host fails `bench_report`'s fitness probe: an unpaired before/after across two `cargo bench` runs read **+47%**, and the difference between the two answers was drift, not the sampler. **Almost none of the cost is the clock *at the interval measured*** — `SystemTime::now()` is 38.4 ns here, which at the 1024-push default is 0.04 ns amortised, 3% of the delta; the rest is the per-push counter, which `0036` priced at nothing. The edge benchmarked declares no rate, so 1024 is the *high* end of `sample_every` and the low end of the clock's share: at a declared 10 Hz it is 78% of a ~4.9 ns overhead. PHASE1 §11.2 tabulates the range and marks which row is measured. **The record's own alternative was built and is slower** — sampling off the arena's `heartbeat` with a mask reads +1.4 ns against this +1.1 ns |

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
| `control_loop` (`tf_tree`) | the runtime path as a node writes it, and a tail reading for `docs/API.md` §8: two queries under one guard at 1 kHz against a 200 Hz estimate, under a concurrent writer. **Host-specific and cited as a claim nowhere** — unpinned, no RT scheduler, and two clock reads around a sub-microsecond operation, so it reads high and says so. Run with `just control-loop` | API.md §8.4, as the thing that stopped being absent |
| `gen_zstd_conformance` | generates the ingest conformance corpus | (uncited, generator) |
| `gen_domain_fixture` | generates `testdata/frozen/sensor_domain.tft`, the only non-zero-domain arena Python can reach — [`0038`](../decisions/0038-the-domain-a-binding-cannot-name.md) step 4's verification is unwritable without it | (uncited, generator; the fixture it writes is held to account by `crates/tf_tree/tests/frozen.rs`) |

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

**Update: `0022` is now `ready`, and the trigger above fired without the
promotion happening — deliberately.** Question 1 was closed by *withdrawal*
(amendment 5): the flush is only reached by a per-call guard on a *writable*
arena, and no shipped configuration is both, so the ~16 ns it would recover is
paid by nobody. `just guard-cost` now measures the 2×2 that establishes that, and
it is registered in the table above. What `counter_cost` still carries is the
§5.7 claim that counters cost a *hoisted* guard nothing at any thread count at or
below the CPU count — worth a gate on its own terms, and unblocked, but no longer
owed by `0022`. Recording the non-promotion here rather than deleting the
paragraph, because "a trigger fired and nothing happened" is exactly the shape of
failure this file exists to catch.

## What `just evidence-audit` checks

For every `example`/`bin`/`bench` target in the workspace: if it is **not**
executed by any recipe, workflow, test-runner script or `CARGO_BIN_EXE_*` spawn,
then it must appear in a table above. A new artifact that a document starts
citing, with no recipe and no row here, fails the audit.

The audit does **not** check that a probe's recorded number is still true — that
is what makes it a probe. It checks only that somebody can find out.
