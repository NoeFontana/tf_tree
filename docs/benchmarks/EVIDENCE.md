# Evidence register: which artifact produced which number, and how to re-run it

Every performance claim in `docs/` comes from something you can execute. This
file indexes those things, and `just evidence-audit` enforces it.

| Kind | Meaning | Obligation |
|---|---|---|
| **gate** | a document states a pass/fail criterion from it | a named recipe; `evidence-audit` fails otherwise |
| **probe** | a one-off diagnostic; its finding is a record of a measurement taken | a documented command; citing prose reads as past tense |

## Gates — a recipe runs these

| Artifact | Recipe | Criterion |
|---|---|---|
| `tf_tree_c/examples/abi_cost.rs` | `just abi-cost` | PHASE4 §7 gate 1, re-cut by `0023`: four quotients on one interleaved ladder at `[profile.embedder]` — R1 the ABI < 1.10 (**1.025–1.038**), R2 the panic guard < 1.05, R3 the per-call guard < 1.25, and a control at 1 ± 0.02. **All PASS**; exits non-zero otherwise. The `release` arm is a contrast, not a gate (`lto = "thin"` inlines `tft_plan_at` away); the binary checks `argv[1]`'s profile against its target directory |
| `tf_tree_c/tests/cpp/bench.cpp` | `just cpp-bench` | PHASE4 §7 gate 2: C++ wrapper < 1.02× the raw C ABI |
| `tf_tree_bench/src/bin/bench_report.rs` | `just bench-check` / `just tf2-bench-check` | PHASE5 §10 regression gate against the committed baselines |
| `tf_tree_bench/src/bin/embed_cost.rs` | `just embed-cost-check` | PHASE5 §9.2 embedding cost |
| `xtask bench-gate` | `just bench` | PHASE1 §11.3 criteria |
| `tf_tree_bench/src/bin/abi_attached.rs` | `just abi-attached` | Whether the C ABI's cost is the boundary or the language: the boundary — Rust and C++ agree to 4 ns at `lto = false`. Also decomposes the per-call `Guard` (`0022` amendment 4): `fork::generation` +0.2, `Tree::view` +3.7, `Guard::new` +4.8, fork-safety half +6.7, cold cursor ~4.8, ~16 unattributed. Reports; does not gate. `--boundary-real` is checked against `build.rs`'s `OUT_DIR` profile and the manifest's `lto` |
| `tf_tree_bench/src/bin/attach_bench.rs` | `just attach-bench` | PHASE2 §12's attach rows. Reports; does not gate |
| `tf_tree_bench/src/bin/ingest_throughput.rs` | `just gate5` | PHASE5 §12 gate 5: ingest throughput >= 10x real time. **PASSES** by more than an order of magnitude on the generated 50-edge x 100 Hz x 32 s zstd corpus, worst of 3 rounds, `--release`; the four questions behind the number are in [`0050`](../decisions/0050-what-ten-times-real-time-divides.md). The grouped arm gates (`0050` Q1, Q2, Q4); a 40x denser corpus reads 2.6x (release) and exits non-zero. `crates/tf_tree_bench/tests/ingest_throughput.rs` runs per-PR in `just test` and deliberately does not assert the green direction (debug reads ~17x, a margin a loaded runner could lose). No interval is quoted; `just gate5` prints the run's numbers |
| `tf_tree_bench/src/bin/frozen_open.rs` | `just gate2` | PHASE5 §12 gate 2: `.tft` open under 10 ms for a 233 MB index. **PASSES**; `Tree::open_frozen` does no work proportional to the index, so this is a regression guard. Two cache states: the **resident** open at 338 MiB is more than two orders under budget; the **evicted** open is about two orders slower than resident. Only the resident arm gates, on the budget and a scale-invariance ratio inside its 4× bound ([`PHASE5.md`](../PHASE5.md#12-gate) §12 criterion 2). It publishes an absolute duration on a host failing `Fitness::probe` under [`PHASE5.md` §9.3](../PHASE5.md#93-honesty-requirements--normative)'s one-sided-budget amendment. A run it cannot evaluate REFUSES (eviction did not take, fixture under 233 MB, fixtures too close in size, `--budget-ms` above the criterion's). `crates/tf_tree_bench/tests/gate2.rs` drives each through the shipped binary per-PR from `just shm-check` |
| `tf_tree_bench/src/bin/frozen_workers.rs` | `just gate4` / `just gate4-python` | PHASE5 §12 gate 4: 16 workers on one `.tft`, total Pss ≤ 1.2× one worker. **PASSES at 1.024× with a *Rust* worker** (`p` = 0.36 MiB); the criterion is `S ≥ 74p`, so the verdict is a function of the worker. `gate4-python` runs the same driver with a CPython worker (`crates/tf_tree_bench/python/gate4_worker.py`): **1.804–1.806× FAIL** (`p` = 13.84–13.86 MiB) on CPython 3.14.3. Cite the row with the worker's language. The Python arm reports and exits 0; a second gated arm needs a record. The Rust arm exits non-zero on FAIL; `--gate` is passed by the caller and `--gate --python` is refused. `crates/tf_tree_bench/tests/gate4.rs` pins both directions and `gate_arithmetic_is_not_vacuous` pins the arithmetic |
| `tf_tree_bench/src/bin/owner_migration.rs` | `just owner-migration` | PHASE2 §12.3 gate 4b and §12.2's `owner kill -> new owner serving` and `lookup latency across an ownership migration` rows. Five processes, five migrations, the owner killed each round. Measured: **kill -> a fresh process can join again 0.6-1.2 ms p50, 1.1-2.0 ms p99**; **zero failed lookups** in every run. The p99.9 quotient is weakly evaluable (0.976-1.093 at 5 migrations, 1.000 at `--repeat 15`) because the 250 ms window is dominated by steady samples; the stall count (lookups past 10x the steady p99.9, per million) carries the weight: 510-542 steady against 517-531 during. Not wired into CI: a criterion that fails one run in five for tail noise gets ignored. Exits non-zero on FAIL and on **INVALID** (a starved writer). `gate_arithmetic_is_not_vacuous` asserts the verdict can flip; it runs in CI through `just shm-check`'s `--bins` |
| `docker/tf2/native_footprint.cpp` | `just tf2-native-footprint` | The memory comparison with no binding on either side. Not a gate; refuses a quotient if the arms stored different sample counts |
| `tf_tree_bench/src/backing.rs` (guard rows) | `just guard-cost` | `0022` question 1's 2x2: {release, embedder} x {counters on, off}, on writable arenas |
| `tf_tree_bench/src/backing.rs` | `just abi-split` | not a gate — the ABI ladder; PHASE4 §7 cites its guard rows |
| `tf_tree_bench/benches/push_sampler.rs` | `just push-sampler-cost` | not a gate — what [`0036`](../decisions/0036-the-receipt-time-the-format-already-reserved.md)'s clock-offset sampler costs `EdgeWriter::push`, as a **paired delta in one process** against `Publisher::push`: **+1.0–1.1 ns, ~21–23%** on the §11.1 fixture (5.87–5.88 against 4.85–4.87). Separate from `benches/push.rs` because an unpaired before/after across two `cargo bench` runs read +47% (drift). `SystemTime::now()` is 38.4 ns, 0.04 ns amortised at the 1024-push default; at a declared 10 Hz it is 78% of a ~4.9 ns overhead. PHASE1 §11.2 tabulates the range. The record's alternative (sampling off `heartbeat` with a mask) reads +1.4 ns |

## Probes — findings recorded, re-runnable on demand

Each is `cargo run --release -p <pkg> --example <name>` unless noted. None is gated.

| Artifact | What it established | Where cited |
|---|---|---|
| `guard_cost` | `Tree::guard()` per call vs hoisted, heap tree, one stamp | tf2.md, PHASE3 |
| `cost_model` | per-step cost of a dynamic chain in-process (217 ns) | tf2.md ×2 |
| `heap_vs_shared` | heap vs `MAP_SHARED` vs attached RO (51.1/51.3/51.2 ns); its stamp is an exact grid hit; superseded for the mapping question by `just abi-split` | tf2.md ×2 |
| `step_cost` | per-edge fold cost | tf2.md ×2 |
| `interp_cost` | interpolation policy costs | (uncited) |
| MCAP record-length survey (not committed) | real top-level record sizes against `IngestOptions::max_record_bytes`; `0010` question 2's owed measurement. 41 published SLAM recordings (`DapengFeng/MCAP`), ~100 GiB; full walk of three (27 974 records): **largest 1.2 MiB, always a `Chunk`, 0.47% of the 256 MiB ceiling**; **41 of 41 carry zero attachments**. One provenance: evidence the ceiling is not tight, not proof | `docs/decisions/0010` |
| `interp_accuracy` | what `ScLerp` buys over `LerpSlerp` by publish rate (position only). D5's owed measurement. `just interp-accuracy` | `docs/PROJECT.md` §5 D5 |
| `adjoint_cost` | adjoint chain cost for derivatives | PHASE4 |
| `deriv_cost` | `sample_with_derivatives` cost | PHASE4 |
| `autovec_probe` | SLP/autovectorisation of the fold | 0016 |
| `bracket_mix` | which interpolation region a real `/tf` stream lands in; `0060` step 0a. On `indoor_atelier.tfstream` four of five dynamic edges never move (202 of 202 intervals bit-identical); the fifth is **99.2% series per bracket, 86.3% under a 100 Hz sweep** (two publication gaps, 1.20 s and 5.30 s). Both policies share one series bound, `θ ≤ 0.15 rad`, so this edge is series above **1.23 Hz**. Checks every swept stamp against `Plan::at` bit-identically | `docs/decisions/0060` §9 |
| `at_many_small` / `at_many_recorded` (bench groups, `cargo bench -p tf_tree_bench --bench at_many`) | `0060` step 1's owed rows: N = 1..63 at both pose entry points, and two plans over `indoor_atelier.tfstream`. The two-phase fold's win is the phase buffering; 16 lanes beat 64; **+82% at N = 1**, crossover between N = 2 and 3; **−19.7% to −35.7%** on the recorded stream | `docs/decisions/0060` §10 |
| `at_many_shapes` (bench group, same bench) | `0060` step 2's plan and data shapes: one dynamic step, stamps on and off the publication grid, moving and stationary edge, both policies. Decision A is −18.6% to −28.3% across the six rows at `[profile.bench]` | `docs/decisions/0060` §11.1 |
| `search_seed` | ring search seeding strategy | PHASE1 |
| `contended_search` | search under a concurrent writer | PHASE1 |
| `hugepage_grant` | whether THP is granted to the memfd mapping. Needs `shm` | PHASE2 |
| `crates/tf_tree_bench/python/thread_scaling.py` (`just py-thread-scaling` free-threaded, `just py-thread-scaling-gil`; not a cargo target, registered here because **this row is the only copy of these numbers**) | **PHASE3 §12.2 criterion 4 and §7.3's scaling test.** 1/2/4/8 threads calling `plan.at` on one depth-3 plan, batch 64, 2 s window after a 0.5 s warm-up, `--release`, 4 physical / 8 logical cores. **Free-threaded (`python3.14t` 3.14.2, five runs):** 1→2 **1.967–1.981×**, 1→4 **3.809–3.934×**, 1→8 **5.979–6.237×**, straddling the 6× floor (four of five above). **GIL build (three runs):** 1→8 **5.707–5.896×**, below. **Control (`--serialize`):** 1→8 **0.834–0.846×**. The margin is inside this instrument's spread, so **this host cannot settle criterion 4**; eight physical cores would. The GIL shortfall is about half a GIL-held allocation and half unattributed: `--call at_into` (caller-owned buffer) was higher at 1→8 in **6 of 6** interleaved pairs by **+0.22 to +1.69** yet topped out at **5.64**, still below the floor. `--gate --call at_into` is refused (the criterion names `plan.at`). `mp::QUIET_ENOUGH` (0.10) is too loose for this harness, so the pairing, not absolute levels, is the finding. The single-thread arm prints ns/sample beside `tree.rs`'s documented **328 ns/elem**, so a release run reads ~0.9× and a `develop` run ~5.8×. Verdicts: `PASS`; `INVALID` on a shortfall where cores < threads or no physical core count is derivable; `FAIL` where the host has a core per thread (`--gate` exits 1); `--gate --serialize`, `--batch 0`, `--seconds 0` refused; a zero-call 1-thread arm exits 2. Core counting is per-processor over `sched_getaffinity` and floored by cgroup `cpu.max` | PHASE3 §12.2 criterion 4, PHASE3 §7.3 |
| `mlock_probe` | what `mlock` does to an arena-shaped mapping — [`0049`](../decisions/0049-the-flag-that-prefaults-the-arena.md)'s executor and `docs/API.md` §8.3's. Six arms plus two organic-pressure arms needing a memory cgroup (`systemd-run --user --scope -p MemoryMax=96M`; the file-backed arm is expected to be OOM-killed). No recipe: every answer is a property of the kernel, cgroup and swap. Linux only | `0049`, API.md §8.3, PHASE2 §7.4 |
| owner-death hangup timing (not committed; the program is inlined in [`0057`](../decisions/0057-an-owner-is-not-dead-until-its-files-close.md)'s *Reproduction*, run as `python3 hup_min.py N K,A,AN [BALLAST_MIB]`) | when a survivor could see an owner's death (6.8 host, apport pipe `core_pattern`, 2026-09-13): the attach socket's hangup and byte 0's OFD lock are released together at the end of the owner's exit, after any core dump and teardown. The `owner_lost()` figures in `PHASE2.md` §3.7 step 9 came from an uncommitted four-role probe; this program agrees with it within ~0.2 ms at ballast 0 and 3–9 ms at 1 GiB. No recipe (property of the host). Linux only | `PHASE2.md` §3.7 step 9, `0057` |
| `counter_cost` (bin) | what §5's diagnostic counters cost. Needs `--features counters` both ways. `docs/PHASE4.md` §7 records that counters roughly double the per-call `Guard` (+35.4 ns against +16.8 ns); `0022` question 1 closed by withdrawal (amendment 5), so it is not owed a gate | PHASE5 §5 |
| `py_parity` (bench) | Python binding parity. Needs the uv toolchain, `just py-*` | PHASE3 |
| `query_mix` (bench) | mixed query workload | (uncited) |
| `at_many` (bench) | batch-fold throughput | (uncited) |
| `read_scaling` (bench) | PHASE1 §11.3's read-scaling curve | xtask bench-gate names it as a manual follow-up |
| `lookup` (bench) | PHASE1 §11.3's depth-3 hot p50 rows; the fold is measured **inlined** (§11.3 NORMATIVE). `docs/PHASE3.md` quotes its `lookup/depth3/sclerp` row and `0013` re-cut both ceilings against it | xtask bench-gate; PHASE1 §11.3, PHASE3, `0013` |
| `push` (bench) | PHASE1 §11.2's ns/push table; `push_sampler` is the separate paired artifact | (PHASE1 §11.2) |
| `control_loop` (`tf_tree`) | the runtime path as a node writes it, and a tail reading for `docs/API.md` §8: two queries under one guard at 1 kHz against a 200 Hz estimate, under a concurrent writer. Host-specific, unpinned, cited as a claim nowhere. `just control-loop` | API.md §8.4 |
| `gen_zstd_conformance` | generates the ingest conformance corpus | (uncited, generator) |
| `soak` under cachegrind | which line of the lookup path mispredicts: `bracket`'s mask-select compiles to a conditional branch — **57.2%** of the process's mispredicts at a 37.3% rate, the loop back-edge 18.1%. Table and arms in [`0053`](../decisions/0053-the-branchless-bracket-that-branches.md). Not a recipe (needs the container and a per-arm rebuild); cachegrind records host source paths, so mount the tree at its own path: `docker run --rm -v "$PWD:$PWD" -w "$PWD" -u "$(id -u):$(id -g)" -e ROS_HOME=/tmp/.ros tf_tree/tf2-bench bash -lc 'valgrind --tool=cachegrind --branch-sim=yes --cache-sim=no --cachegrind-out-file=/tmp/cg.out target/profiling/soak --workload robot --duration 3s --interval 2s >/dev/null 2>&1; cg_annotate --show=Ir,Bc,Bcm --sort=Bcm --auto=yes /tmp/cg.out'`. Only per-lookup ratios compare across arms | `0053`, sample.rs's `bracket` doc block, tf2.md ×2, fast-path.md |
| `gen_domain_fixture` | generates `testdata/frozen/sensor_domain.tft`, the only non-zero-domain arena Python can reach ([`0038`](../decisions/0038-the-domain-a-binding-cannot-name.md) step 4) | (generator; fixture held by `crates/tf_tree/tests/frozen.rs`) |

Most criterion benches are compiled and executed by no recipe: `cargo xtask
bench-gate` runs `cargo bench -p tf_tree_bench --no-run`, which links the suite
and does not run it. A criterion bench cannot gate — `criterion` owns the exit
status — which is why the §12 criteria that acquired gates acquired `[[bin]]`s.
`read_scaling`'s criterion is mid-recut (`0013`).

## What `just evidence-audit` checks

For every `example`/`bin`/`bench` target in the workspace: if it is **not**
executed by any recipe, workflow, test-runner script or `CARGO_BIN_EXE_*` spawn,
it must be the first cell of a table row above. A new artifact that a document
starts citing, with no recipe and no row here, fails the audit. It does **not**
check that a probe's recorded number is still true.
