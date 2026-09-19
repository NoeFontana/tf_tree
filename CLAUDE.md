# CLAUDE.md — agent guidance for tf_tree

For AI agents. Humans: [`CONTRIBUTING.md`](./CONTRIBUTING.md).

`tf_tree` is a transform tree engine — a faster, more scalable alternative to ROS
`tf2`. Built along a fixed **eight-phase** roadmap ([`0006`](./docs/decisions/0006-the-eight-phase-roadmap.md)).

**The documents in [`docs/`](./docs/) are the contract.** Read
[`docs/PROJECT.md`](./docs/PROJECT.md) (architecture, roadmap, decision log
D1–D22 in §5, design smells in §6), then the spec for the phase you touch,
**before** touching code. When they do not answer a question — **stop and ask.**
Never invent an answer in the concurrency or arena-layout sections.
Ignore `docs/changelog/` (released entries; history lives in git).

## Status

Each spec's **§0.0 status table is the source of truth**, over its own prose, the
README, and this file. Published on the `0.0.x` line (every release may break
every other); read `[workspace.package] version`. MSRV **1.87**.

## Read before changing X

| Doc | Role |
|---|---|
| [`PROJECT.md`](./docs/PROJECT.md) | Overview, roadmap, D1–D22 (§5), design smells (§6). |
| [`PHASE1.md`](./docs/PHASE1.md) | Layouts, atomic orderings, tests (§10), bench gate (§11). |
| [`PHASE2.md`](./docs/PHASE2.md) | Shared memory. §1 holds Phase 1 amendments A1–A8 — read before changing any concurrency protocol. |
| [`PHASE3.md`](./docs/PHASE3.md) | Python bindings. |
| [`PHASE4.md`](./docs/PHASE4.md) | C ABI, C++ wrapper, ROS 2 ingest, `sample_with_derivatives`. ROS 2 is available in `docker/tf2`. |
| [`PHASE5.md`](./docs/PHASE5.md) | Frozen `.tft`, bag ingestion, counters, `TFT001`–`TFT019`, `tf_tree top`. |
| [`PHASE7.md`](./docs/PHASE7.md) | The `tf2` shim. **GATED (D21), not scheduled.** Its §4 is questions, not answers. |
| [`API.md`](./docs/API.md) | Cross-cutting API contract: six rules (§1), per-binding surface (§2–§5), delta table (§6), new-surface checklist (§7). |
| [`decisions/`](./docs/decisions/) | A record's own `**Status:**` line says whether it is authoritative: `grep -m1 -H '^\*\*Status:' docs/decisions/0*.md` (keep `-H`). [`README.md`](./docs/decisions/README.md) indexes what each decided. [`0028`](./docs/decisions/0028-the-slot-a-killed-participant-keeps.md) is implemented; its fork half is [`0030`](./docs/decisions/0030-the-atfork-handler-and-inherited-descriptors.md). [`0016`](./docs/decisions/0016-portable-simd-and-the-dependency-budget.md) is withdrawn. |

Cautions:

- **`FORMAT_VERSION = 3` already happened** (the header). The region-table break is still owed
  ([`0032`](./docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md)); **do not add arena fields opportunistically** — the queue is `PROJECT.md` §5.1.
- **`PHASE5.md` §8 is about *not* building visualization.** Do not propose a viewer without refuting §8.1.
- **`PHASE7.md` existing is not permission to build it** (§0.0 lists four unmet gates).
- `Tree::owner_lost` / `inherit_ownership` are caller-driven by design ([`0019`](./docs/decisions/0019-one-binary-and-topology-you-can-wait-for.md)): no background thread, no daemon.

## Project shape

```
crates/tf_tree_math/    no_std; SE(3)/SO(3), quats, dual quats; #![forbid(unsafe_code)]
crates/tf_tree_arena/   no_std+alloc; pointer-free arena + layout math (unsafe allowed)
crates/tf_tree_core/    no_std+alloc; the engine; unsafe only in buffer.rs / arena_view.rs
crates/tf_tree/         std facade; #![deny(unsafe_code)] + one #[allow]: OwnedWriter (0017)
crates/tf_tree_ipc/     std; rendezvous, lock file, fd passing (unsafe: one atfork shim)
crates/tf_tree_bridge/  ROS-independent half of the /tf ingest bridge
crates/tf_tree_ingest/  MCAP bag ingestion (§3); not in core/arena, not in the CLI
crates/tf_tree_py/      PyO3; binds the Rust core directly, NOT the C ABI
crates/tf_tree_c/       C ABI + header-only C++ wrapper
crates/tf_tree_bench/   criterion + tf2 differential harness (register: scripts/unsafe-budget.txt, 0048)
crates/tf_tree_tf2_sys/ tf2 side of the differential harness; unsafe = 0007 kind 3
crates/tf_tree_cli/     binary `tf_tree` (alias `tft`)
ros/tf_tree_ros/        ament_cmake: the §5 ingest bridge. NOT a cargo crate.
ros/tf_tree_bench_ros/  ament_cmake: PHASE5 §9.1 DDS comparison. Benchmark-only.
xtask/                  loom / bench-gate / headers runners (Miri is `just miri`)
```

**Five crates publish:** `tf_tree`, `tf_tree_core`, `tf_tree_math`, `tf_tree_arena`,
`tf_tree_ipc`. `tf_tree_py` and `tf_tree_tf2_sys` are excluded from the cargo
workspace; `ros/*` is invisible to cargo — `just ros-build` and `just ros-test`
are its entire gate.

## Hard rules — do not relitigate

- **Dependency budget:** `tf_tree_core` = `libm` + `bytemuck` + `blake3`; `tf_tree_math` = `libm` + `bytemuck`. No `serde`, `tokio`, `nalgebra`, or logging framework in the core (D14).
- **Unsafe budget** ([`0007`](./docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md), [`0048`](./docs/decisions/0048-a-kind-is-not-a-crate-name.md)): only at a boundary the compiler cannot see across (arena memory, the OS, a foreign runtime/library, a foreign caller, our own C ABI called from Rust, a trait a never-shipped target requires). A new kind needs a record; a new file needs a row in `scripts/unsafe-budget.txt`. The budget binds a crate **root**, not a package. Every `unsafe` block carries a `// SAFETY:` naming its invariant; every root with `unsafe` declares its posture, carries a module `// SAFETY:` block and `#![deny(unsafe_op_in_unsafe_fn)]`.
- **`tf_tree` has exactly one `#[allow(unsafe_code)]`** — `OwnedWriter`'s lifetime extension ([`0017`](./docs/decisions/0017-owned-handles-and-the-lifetime-rule.md)); `rg 'allow\(unsafe_code\)' crates/tf_tree/src` must return one line. Bindings claim through `Tree::claim_owned`.
- **Check API shape against [`API.md`](./docs/API.md) §1 before writing it:** R1 three tiers; R2 hot tier never allocates/locks/converts; R3 integer-nanosecond stamps carrying a domain; R4 layout stated, never inferred; R5 errors are `Copy` identifiers; R6 read-only by default. A question those do not answer is a decision record.
- **Arena:** no pointers, fixed capacity, no growth, `#[repr(C)]` everywhere, append-only `FrameId`/`EdgeId` (tombstone, never recycle — D10). No `ArcSwap`/`Arc`/`Box`/`Vec` inside an arena structure (D4).
- **Do not weaken an atomic ordering because a test passes on x86-64.** `just loom` is the argument; aarch64 CI is corroboration only.
- No `String` in any error type or hot path; errors are `Copy` and name the offending edge (D11). No `async`/runtime. No GPU/point-cloud/`deskew` (D8). `f64` only (D6).
- **`LerpSlerp`'s right-invariance test is supposed to fail** — do not fix it (D5; `PHASE1.md` §3.4).
- One writer per edge (D7). Read-only attach is the consumer default (D18). Liveness is the socket, never a heartbeat timeout (D17). Every mutation protocol walks the crash matrix (D15).
- No blocking wait, futex, or notification primitive in the arena ([`0018`](./docs/decisions/0018-blocking-waits-belong-in-the-shim.md)).
- Do not add a second spelling of an existing path (a `coverage` beside `span`). Document the one that exists.
- **Cite a symbol, never a line number.** `just lint` ratchets existing prefixed citations (`scripts/line-citation-budget.txt`); a new one fails. Scope and measurements: `check_line_citations` in `scripts/artifact-versions.py`; the bare form (`tree.rs:123`) is also counted.
- **A comment states the decision and its load-bearing evidence, then stops.** Rustdoc is the item's contract: what it does, what the caller must guarantee, what they get, and warnings. Provenance (retracted measurements, earlier wording, rejected hardenings) belongs in the owning record, cited by number.
- **Cut, not deferred** ([`0009`](./docs/decisions/0009-descoping-phase-6.md)): covariance, copy-on-write branches, multi-parent edges, URDF in the engine. Do not reintroduce them.
- Records state the decision, not the narrative. Prose volume is a maintenance cost; prefer deleting to adding.

## Commands

Everything goes through `just`; CI invokes the recipes 1:1. Run them locally before pushing. Tests: **nextest, not `cargo test`** (tests share a pid and `TF_TREE_RUNTIME_DIR` under libtest). Single test: `cargo nextest run -p tf_tree_math -- exp_log_roundtrip`. Unset `CARGO_TARGET_DIR` for `bench-check` and the header check.

| Recipe | What it does |
|---|---|
| `just build` | `cargo build --workspace --all-targets` |
| `just test` | nextest workspace + doctests + ingest-check, default features (`shm`-gated code is compiled out) |
| `just lint` | fmt + clippy per feature configuration, plus the repo's audit gates (unsafe budget, evidence, versions, py-compile); not `cargo deny`; does not compile `shm` targets or lint `scripts/*.py` |
| `just shm-check` | fmt/clippy/tests for the `shm` feature; a new `shm`-only target goes on its list |
| `just stable-tier-check` | `tf_tree` default tier with `unstable` off |
| `just artifact-versions` | version-skew, doc-link, table-shape, recipe-reference and changelog gates; reads `git ls-files`, so `git add` new docs first |
| `just quickstart` | builds venvs, runs the README snippet and asserts its output |
| `just doc` | rustdoc, warnings denied; not covered by lint/test |
| `just py-cross-check` | `cargo check` of `tf_tree_py` at macOS/Windows targets with `pure-hash` |
| `just py-test` / `py-lint` | Python tests / lint (`py-lint` also covers `scripts/` and `tf_tree_py` rustdoc) |
| `just msrv` | floor build, manifests, and the floor as stated in prose |
| `just no-network` | asserts every library-test `socket(2)` is `AF_UNIX`, under `strace -f` |
| `just loom` / `miri` / `tsan` | concurrency model checking / UB / thread sanitizer |
| `just audit` | `cargo deny check` |
| `just bench` / `bench-check` / `bench-baseline-update` | suite and go/no-go gate against the committed baseline |
| `just embed-cost` / `embed-cost-check` | PHASE5 §9.2 embedding measurements |
| `just gate2` / `gate5` | PHASE5 §12 gates 2 and 5; exit non-zero on FAIL; figures live in `docs/benchmarks/EVIDENCE.md` |
| `just gate4` / `gate4-python` | PHASE5 §12 gate 4; `gate4` (Rust worker, nightly) gates, `gate4-python` only reports |
| `just test-doc-error-codes` | the `compile_fail,E0277` pins, on nightly |
| `just ros-build` / `ros-test` / `tf2-check` / `dds-bench` | container-only |
| `just contended-scaling` / `scale-sweep` / `soak` / `bench-run` / `bench-ab` | exploratory; emit JSON, do not feed `bench-check` |

## Decision workflow

A change the specs do not cover — new public API, new crate boundary, a different
concurrency protocol — starts as a **`draft` decision record** in
[`docs/decisions/`](./docs/decisions/), **not** as a PR (see its `README.md`).
When a record is `ready`, implement it as stated; its *Implementation plan* is the
per-PR breakdown. Cite `PROJECT.md`/`PHASE1.md`; records `0002`/`0003` were
consolidated into them and no longer exist. `0004` remains authoritative for
builder-time edge declaration.
