# CLAUDE.md — agent guidance for tf_tree

For AI agents. Humans: [`CONTRIBUTING.md`](./CONTRIBUTING.md). `tf_tree` is a transform tree
engine built along a fixed eight-phase roadmap ([`0006`](./docs/decisions/0006-the-eight-phase-roadmap.md)).

**The documents in [`docs/`](./docs/) are the contract.** Read `docs/PROJECT.md`, then the
spec for the phase you touch, **before** touching code. When they do not answer a
question, **stop and ask**; never invent an answer in the concurrency or arena-layout
sections. Ignore `docs/changelog/`.

## Status

Each spec's **§0.0 status table is the source of truth**, over its own prose, the README,
and this file. Published on the `0.0.x` line (every release may break every other); read
`[workspace.package] version`. MSRV **1.87**.

## Read before changing X

| Doc | Role |
|---|---|
| [`PROJECT.md`](./docs/PROJECT.md) | Overview, roadmap, D1–D22 (§5), design smells (§6). |
| [`PHASE1.md`](./docs/PHASE1.md) | Layouts, atomic orderings, tests (§10), bench gate (§11). |
| [`PHASE2.md`](./docs/PHASE2.md) | Shared memory. §1 holds amendments A1–A8: read before changing any concurrency protocol. |
| [`PHASE3.md`](./docs/PHASE3.md) | Python bindings. |
| [`PHASE4.md`](./docs/PHASE4.md) | C ABI, C++ wrapper, ROS 2 ingest (`docker/tf2`), `sample_with_derivatives`. |
| [`PHASE5.md`](./docs/PHASE5.md) | Frozen `.tft`, bag ingestion, counters, `TFT001`–`TFT019`, `tf_tree top`. |
| [`PHASE7.md`](./docs/PHASE7.md) | The `tf2` shim. **GATED (D21), not scheduled**; §4 is questions, not answers. |
| [`API.md`](./docs/API.md) | API contract: six rules (§1), per-binding surface (§2–§5), delta table (§6), new-surface checklist (§7). |
| [`decisions/`](./docs/decisions/) | A record's own `**Status:**` line says whether it is authoritative: `grep -m1 -H '^\*\*Status:' docs/decisions/0*.md`. [`README.md`](./docs/decisions/README.md) indexes them. [`0028`](./docs/decisions/0028-the-slot-a-killed-participant-keeps.md) is implemented and frozen (fork half: [`0030`](./docs/decisions/0030-the-atfork-handler-and-inherited-descriptors.md)); [`0016`](./docs/decisions/0016-portable-simd-and-the-dependency-budget.md) is withdrawn. |

Cautions:

- **`FORMAT_VERSION = 3` already happened** (the header). The region-table break is still owed ([`0032`](./docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md)); **do not add arena fields opportunistically** — the queue is `PROJECT.md` §5.1.
- **`PHASE5.md` §8 is about *not* building visualization.** Do not propose a viewer without refuting §8.1.
- **`PHASE7.md` existing is not permission to build it** (§0.0 lists four unmet gates).
- `Tree::owner_lost` / `inherit_ownership` are caller-driven by design ([`0019`](./docs/decisions/0019-one-binary-and-topology-you-can-wait-for.md)): no background thread, no daemon.

## Project shape

Five crates publish: `tf_tree`, `tf_tree_core`, `tf_tree_math`, `tf_tree_arena`, `tf_tree_ipc`.
`tf_tree_py` and `tf_tree_tf2_sys` are excluded from the cargo workspace; `ros/*` is not
cargo, and `just ros-build` / `just ros-test` are its entire gate. `tf_tree_py` binds the
Rust core directly, not the C ABI. `tf_tree_math`, `tf_tree_arena` and `tf_tree_core` are
`no_std`.

## Hard rules — do not relitigate

- **Dependency budget:** `tf_tree_core` = `libm` + `bytemuck` + `blake3`; `tf_tree_math` = `libm` + `bytemuck`. No `serde`, `tokio`, `nalgebra`, or logging framework in the core (D14).
- **Unsafe budget** ([`0007`](./docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md), [`0048`](./docs/decisions/0048-a-kind-is-not-a-crate-name.md)): only at a boundary the compiler cannot see across (arena memory, the OS, a foreign runtime/library, a foreign caller, our own C ABI called from Rust, a trait a never-shipped target requires). A new kind needs a record; a new file needs a row in `scripts/unsafe-budget.txt`. The budget binds a crate **root**, not a package. Every `unsafe` block carries a `// SAFETY:` naming its invariant; every root with `unsafe` declares its posture, a module `// SAFETY:` block and `#![deny(unsafe_op_in_unsafe_fn)]`.
- **`tf_tree` has exactly one `#[allow(unsafe_code)]`** — `OwnedWriter` ([`0017`](./docs/decisions/0017-owned-handles-and-the-lifetime-rule.md)); `rg 'allow\(unsafe_code\)' crates/tf_tree/src` must return one line. Bindings claim through `Tree::claim_owned`.
- **Check API shape against [`API.md`](./docs/API.md) §1 before writing it:** R1 three tiers; R2 hot tier never allocates/locks/converts; R3 integer-nanosecond stamps carrying a domain; R4 layout stated, never inferred; R5 errors are `Copy` identifiers; R6 read-only by default. A question those do not answer is a decision record.
- **Arena:** no pointers, fixed capacity, no growth, `#[repr(C)]` everywhere, append-only `FrameId`/`EdgeId` (tombstone, never recycle — D10). No `ArcSwap`/`Arc`/`Box`/`Vec` inside an arena structure (D4).
- **Do not weaken an atomic ordering because a test passes on x86-64.** `just loom` is the argument; aarch64 CI is corroboration only.
- No `String` in any error type or hot path; errors are `Copy` and name the offending edge (D11). No `async`/runtime. No GPU/point-cloud/`deskew` (D8). `f64` only (D6).
- **`LerpSlerp`'s right-invariance test is supposed to fail** — do not fix it (D5; `PHASE1.md` §3.4).
- One writer per edge (D7). Read-only attach is the consumer default (D18). Liveness is the socket, never a heartbeat timeout (D17). Every mutation protocol walks the crash matrix (D15).
- No blocking wait, futex, or notification primitive in the arena ([`0018`](./docs/decisions/0018-blocking-waits-belong-in-the-shim.md)).
- Do not add a second spelling of an existing path (a `coverage` beside `span`). Document the one that exists.
- **Cite a symbol, never a line number**; `just lint` fails a new line citation.
- **A comment states the decision and its load-bearing evidence, then stops.** Rustdoc is the item's contract; provenance belongs in the owning record, cited by number. Prefer deleting prose to adding it.
- **Cut, not deferred** ([`0009`](./docs/decisions/0009-descoping-phase-6.md)): covariance, copy-on-write branches, multi-parent edges, URDF in the engine.

## Commands

Everything goes through `just`; CI invokes the recipes 1:1. Run them before pushing. Tests: **nextest, not `cargo test`** (libtest shares a pid and `TF_TREE_RUNTIME_DIR`). Single test: `cargo nextest run -p tf_tree_math -- exp_log_roundtrip`. Unset `CARGO_TARGET_DIR` for `bench-check` and the header check.

| Recipe | Purpose |
|---|---|
| `just build` / `test` | build; nextest, doctests, default features |
| `just lint` | fmt, clippy, audit gates; not `shm` targets or `scripts/*.py` |
| `just shm-check` | fmt/clippy/tests for the `shm` feature |
| `just stable-tier-check` | default tier with `unstable` off |
| `just artifact-versions` | version, link, table, changelog gates; `git add` new docs first |
| `just doc` | rustdoc, warnings denied; not in lint/test |
| `just py-test` / `py-lint` | Python tests / lint (also `scripts/`) |
| `just msrv` | floor build and stated floor |
| `just loom` / `miri` / `tsan` | model checking / UB / thread sanitizer |
| `just bench-check` | bench go/no-go against baseline |
| `just gate2` / `gate4` / `gate5` | PHASE5 §12 gates; figures in `docs/benchmarks/EVIDENCE.md` |
| `just ros-build` / `ros-test` | container-only |

## Decision workflow

A change the specs do not cover — new public API, new crate boundary, a different
concurrency protocol — starts as a **`draft` decision record** in
[`docs/decisions/`](./docs/decisions/), **not** as a PR (see its `README.md`). When a
record is `ready`, implement it as stated; its *Implementation plan* is the per-PR
breakdown. Cite `PROJECT.md`/`PHASE1.md`. `0004` remains authoritative for builder-time
edge declaration.
