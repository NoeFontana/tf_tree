# Evidence register: which artifact produced which number, and how to re-run it

Every performance claim in `docs/` comes from something you can execute. This
file indexes those things, and `just evidence-audit` enforces it.

| Kind | Meaning | Obligation |
|---|---|---|
| **gate** | a document states a pass/fail criterion from it | a named recipe; `evidence-audit` fails otherwise |
| **probe** | a one-off diagnostic; its finding is a record of a measurement taken | a documented command |

## Gates — a recipe runs these

| Artifact | Recipe | Criterion |
|---|---|---|
| `tf_tree_c/examples/abi_cost.rs` | `just abi-cost` | PHASE4 §7 gate 1, re-cut by `0023`: four quotients on one interleaved ladder at `[profile.embedder]` — R1 the ABI < 1.10 (**1.025–1.038**), R2 the panic guard < 1.05, R3 the per-call guard < 1.25, and a control at 1 ± 0.02. **All PASS**; exits non-zero otherwise. The `release` arm is a contrast, not a gate |
| `tf_tree_c/tests/cpp/bench.cpp` | `just cpp-bench` | PHASE4 §7 gate 2: C++ wrapper < 1.02× the raw C ABI |
| `tf_tree_bench/src/bin/bench_report.rs` | `just bench-check` / `just tf2-bench-check` | PHASE5 §10 regression gate against the committed baselines |
| `tf_tree_bench/src/bin/embed_cost.rs` | `just embed-cost-check` | PHASE5 §9.2 embedding cost |
| `xtask bench-gate` | `just bench` | PHASE1 §11.3 criteria |
| `tf_tree_bench/src/bin/abi_attached.rs` | `just abi-attached` | Whether the C ABI's cost is the boundary or the language: the boundary (Rust and C++ agree to 4 ns at `lto = false`). Decomposes the per-call `Guard` (`0022` amendment 4). Reports; does not gate |
| `tf_tree_bench/src/bin/attach_bench.rs` | `just attach-bench` | PHASE2 §12's attach rows. Reports; does not gate |
| `tf_tree_bench/src/bin/ingest_throughput.rs` | `just gate5` | PHASE5 §12 gate 5: ingest throughput >= 10x real time. **PASSES** by more than an order of magnitude on the generated 50-edge x 100 Hz x 32 s zstd corpus, worst of 3 rounds, `--release`; the questions behind the number are in [`0050`](../decisions/0050-what-ten-times-real-time-divides.md). `crates/tf_tree_bench/tests/ingest_throughput.rs` runs per-PR in `just test` and does not assert the green direction |
| `tf_tree_bench/src/bin/frozen_open.rs` | `just gate2` | PHASE5 §12 gate 2: `.tft` open under 10 ms for a 233 MB index. **PASSES**; the **resident** open is more than two orders under budget; only the resident arm gates, on the budget and a scale-invariance ratio inside its 4× bound ([`PHASE5.md`](../PHASE5.md#12-gate) §12 criterion 2). A run it cannot evaluate REFUSES. `crates/tf_tree_bench/tests/gate2.rs` drives each arm through the shipped binary per-PR from `just shm-check` |
| `tf_tree_bench/src/bin/frozen_workers.rs` | `just gate4` / `just gate4-python` | PHASE5 §12 gate 4: 16 workers on one `.tft`, total Pss ≤ 1.2× one worker. **PASSES at 1.024× with a *Rust* worker**; the criterion is `S ≥ 74p`, so the verdict is a function of the worker. `gate4-python` (`crates/tf_tree_bench/python/gate4_worker.py`): **1.804–1.806× FAIL** on CPython 3.14.3; it reports and exits 0. Cite the row with the worker's language. `crates/tf_tree_bench/tests/gate4.rs` pins both directions and `gate_arithmetic_is_not_vacuous` pins the arithmetic |
| `tf_tree_bench/src/bin/owner_migration.rs` | `just owner-migration` | PHASE2 §12.3 gate 4b and §12.2's `owner kill -> new owner serving` and `lookup latency across an ownership migration` rows. Measured: **kill -> a fresh process can join again 0.6-1.2 ms p50, 1.1-2.0 ms p99**; **zero failed lookups**. Not wired into CI. Exits non-zero on FAIL and on **INVALID** (a starved writer); `gate_arithmetic_is_not_vacuous` asserts the verdict can flip |
| `docker/tf2/native_footprint.cpp` | `just tf2-native-footprint` | The memory comparison with no binding on either side. Not a gate |
| `tf_tree_bench/src/backing.rs` (guard rows) | `just guard-cost` | `0022` question 1's 2x2: {release, embedder} x {counters on, off}, on writable arenas |
| `tf_tree_bench/src/backing.rs` | `just abi-split` | not a gate; the ABI ladder PHASE4 §7 cites |
| `tf_tree_bench/benches/push_sampler.rs` | `just push-sampler-cost` | not a gate: what [`0036`](../decisions/0036-the-receipt-time-the-format-already-reserved.md)'s clock-offset sampler costs `EdgeWriter::push`, as a **paired delta in one process**: **+1.0–1.1 ns, ~21–23%** on the §11.1 fixture. PHASE1 §11.2 tabulates the range |

## Probes — findings recorded, re-runnable on demand

Each is `cargo run --release -p <pkg> --example <name>` unless noted. None is gated.

| Artifact | What it established | Where cited |
|---|---|---|
| `guard_cost` | `Tree::guard()` per call vs hoisted, heap tree, one stamp | tf2.md, PHASE3 |
| `cost_model` | per-step cost of a dynamic chain in-process (217 ns) | tf2.md ×2 |
| `heap_vs_shared` | heap vs `MAP_SHARED` vs attached RO (51.1/51.3/51.2 ns); superseded for the mapping question by `just abi-split` | tf2.md ×2 |
| `step_cost` | per-edge fold cost | tf2.md ×2 |
| `interp_cost` | interpolation policy costs | (uncited) |
| MCAP record-length survey (not committed) | real top-level record sizes against `IngestOptions::max_record_bytes`: **largest 1.2 MiB, always a `Chunk`, 0.47% of the 256 MiB ceiling**; 41 of 41 recordings carry zero attachments | `docs/decisions/0010` |
| `interp_accuracy` | what `ScLerp` buys over `LerpSlerp` by publish rate (position only); `just interp-accuracy` | `docs/PROJECT.md` §5 D5 |
| `adjoint_cost` | adjoint chain cost for derivatives | PHASE4 |
| `deriv_cost` | `sample_with_derivatives` cost | PHASE4 |
| `autovec_probe` | SLP/autovectorisation of the fold | 0016 |
| `bracket_mix` | which interpolation region a real `/tf` stream lands in; `0060` step 0a. On `indoor_atelier.tfstream` the moving edge is **99.2% series per bracket, 86.3% under a 100 Hz sweep**; series above **1.23 Hz**. Checks every swept stamp against `Plan::at` bit-identically | `docs/decisions/0060` §9 |
| `at_many_small` / `at_many_recorded` (bench groups, `cargo bench -p tf_tree_bench --bench at_many`) | `0060` step 1's rows: N = 1..63 at both pose entry points, and two plans over `indoor_atelier.tfstream`. 16 lanes beat 64; **+82% at N = 1**, crossover between N = 2 and 3; **−19.7% to −35.7%** on the recorded stream | `docs/decisions/0060` §10 |
| `at_many_shapes` (bench group, same bench) | `0060` step 2's plan and data shapes; Decision A is −18.6% to −28.3% across the six rows at `[profile.bench]` | `docs/decisions/0060` §11.1 |
| `search_seed` | ring search seeding strategy | PHASE1 |
| `contended_search` | search under a concurrent writer | PHASE1 |
| `hugepage_grant` | whether THP is granted to the memfd mapping. Needs `shm` | PHASE2 |
| `crates/tf_tree_bench/python/thread_scaling.py` (`just py-thread-scaling` free-threaded, `just py-thread-scaling-gil`; not a cargo target, registered here because **this row is the only copy of these numbers**) | **PHASE3 §12.2 criterion 4 and §7.3's scaling test.** 1/2/4/8 threads on one depth-3 plan, batch 64, `--release`, 4 physical / 8 logical cores. **Free-threaded (`python3.14t` 3.14.2, five runs):** 1→8 **5.979–6.237×**, straddling the 6× floor. **GIL build (three runs):** 1→8 **5.707–5.896×**. **Control (`--serialize`):** 1→8 **0.834–0.846×**. **This host cannot settle criterion 4**; eight physical cores would. Verdicts: `PASS`; `INVALID` on a shortfall where cores < threads; `FAIL` where the host has a core per thread (`--gate` exits 1). The single-thread arm prints ns/sample beside `tree.rs`'s documented **328 ns/elem** | PHASE3 §12.2 criterion 4, PHASE3 §7.3 |
| `mlock_probe` | what `mlock` does to an arena-shaped mapping ([`0049`](../decisions/0049-the-flag-that-prefaults-the-arena.md)'s executor). Six arms plus two organic-pressure arms needing a memory cgroup (`systemd-run --user --scope -p MemoryMax=96M`). No recipe. Linux only | `0049`, API.md §8.3, PHASE2 §7.4 |
| owner-death hangup timing (not committed; the program is inlined in [`0057`](../decisions/0057-an-owner-is-not-dead-until-its-files-close.md)'s *Reproduction*) | when a survivor could see an owner's death: the attach socket's hangup and byte 0's OFD lock are released together at the end of the owner's exit, after any core dump and teardown. No recipe. Linux only | `PHASE2.md` §3.7 step 9, `0057` |
| `counter_cost` (bin) | what §5's diagnostic counters cost. Needs `--features counters` both ways. Counters roughly double the per-call `Guard` (+35.4 ns against +16.8 ns) | PHASE5 §5 |
| `py_parity` (bench) | Python binding parity. Needs the uv toolchain, `just py-*` | PHASE3 |
| `query_mix` (bench) | mixed query workload | (uncited) |
| `at_many` (bench) | batch-fold throughput | (uncited) |
| `read_scaling` (bench) | PHASE1 §11.3's read-scaling curve | xtask bench-gate |
| `lookup` (bench) | PHASE1 §11.3's depth-3 hot p50 rows; the fold is measured **inlined** (§11.3 NORMATIVE) | xtask bench-gate; PHASE1 §11.3, PHASE3, `0013` |
| `push` (bench) | PHASE1 §11.2's ns/push table | (PHASE1 §11.2) |
| `control_loop` (`tf_tree`) | the runtime path as a node writes it: two queries under one guard at 1 kHz against a 200 Hz estimate, under a concurrent writer. Host-specific, unpinned. `just control-loop` | API.md §8.4 |
| `gen_zstd_conformance` | generates the ingest conformance corpus | (uncited, generator) |
| `soak` under cachegrind | which line of the lookup path mispredicts: `bracket`'s mask-select compiles to a conditional branch, **57.2%** of the process's mispredicts ([`0053`](../decisions/0053-the-branchless-bracket-that-branches.md)). Not a recipe (needs the container and a per-arm rebuild): `docker run --rm -v "$PWD:$PWD" -w "$PWD" -u "$(id -u):$(id -g)" -e ROS_HOME=/tmp/.ros tf_tree/tf2-bench bash -lc 'valgrind --tool=cachegrind --branch-sim=yes --cache-sim=no --cachegrind-out-file=/tmp/cg.out target/profiling/soak --workload robot --duration 3s --interval 2s >/dev/null 2>&1; cg_annotate --show=Ir,Bc,Bcm --sort=Bcm --auto=yes /tmp/cg.out'` | `0053`, sample.rs's `bracket` doc block, tf2.md ×2, fast-path.md |
| `gen_domain_fixture` | generates `testdata/frozen/sensor_domain.tft` ([`0038`](../decisions/0038-the-domain-a-binding-cannot-name.md) step 4) | (generator; fixture held by `crates/tf_tree/tests/frozen.rs`) |

Most criterion benches are executed by no recipe: `cargo xtask bench-gate` only
links the suite (`--no-run`). A criterion bench cannot gate, since `criterion`
owns the exit status; the §12 criteria that acquired gates acquired `[[bin]]`s.

## What `just evidence-audit` checks

For every `example`/`bin`/`bench` target in the workspace: if it is **not**
executed by any recipe, workflow, test-runner script or `CARGO_BIN_EXE_*` spawn,
it must be the first cell of a table row above. It does **not** check that a
probe's recorded number is still true.
