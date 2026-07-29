# CLAUDE.md — agent guidance for tf_tree

This file is for AI coding agents working in this repository. Humans, see
[`CONTRIBUTING.md`](./CONTRIBUTING.md).

## What this is

`tf_tree` is a transform tree engine (a faster, more scalable alternative to ROS
`tf2`). It is being built along a fixed **eight-phase** roadmap (re-cut from six
by [`0006`](./docs/decisions/0006-the-eight-phase-roadmap.md)). **The canonical
documents in [`docs/`](./docs/) are the contract** — read
[`docs/PROJECT.md`](./docs/PROJECT.md) (project overview, architecture, roadmap,
and the decision log D1–D22 in its §5) **then** the spec for the phase you are
touching, before touching code. When these documents do not answer a question,
**stop and ask** — do not invent an answer, especially in the concurrency or
arena-layout sections.

**Phases 1–3 are implemented.** `FORMAT_VERSION = 2`; `tf_tree::open()` exists;
the rendezvous, fd passing, ownership migration, claims-as-leases, reaping, fork
poisoning and the Python bindings all shipped. `docs/PHASE2.md` §0.0 and
`docs/PHASE3.md` are the authoritative status tables.

[`docs/PHASE2.md`](./docs/PHASE2.md) §1 holds **Phase 1 amendments A1–A8, all
applied** — read them before changing any concurrency protocol, because they are
the reason several orderings look the way they do.

**Current work is Phases 4 and 5.** Two cautions specific to them:

- `docs/PHASE5.md` §1 bumps `FORMAT_VERSION` to **3**. That is a real break —
  every participant must be rebuilt and restarted together. §1.2 lists what goes
  in, *including regions Phase 6 will fill*, so the break happens exactly once.
  **Do not add arena fields opportunistically outside that commit.**
- `docs/PHASE5.md` §8 is a section about **not** building something. Visualization
  is deliberately absent, with the argument recorded. Do not propose a viewer
  integration without refuting §8.1 first.

| Document | Role |
| --- | --- |
| [`docs/PROJECT.md`](./docs/PROJECT.md) | Overview, architecture, roadmap, decision log D1–D22 (§5). Supersedes `docs/decisions/0002`. |
| [`docs/PHASE1.md`](./docs/PHASE1.md) | Normative Phase 1 spec: layouts, atomic orderings, test plan (§10), benchmark gate (§11). Supersedes `docs/decisions/0003`. **Implemented.** |
| [`docs/PHASE2.md`](./docs/PHASE2.md) | Normative Phase 2 spec; §1 holds Phase 1 amendments A1–A8 (all applied), §0.0 is the status table. **Engine half implemented.** |
| [`docs/PHASE3.md`](./docs/PHASE3.md) | Normative Phase 3 spec (Python bindings). **Implemented.** |
| [`docs/PHASE4.md`](./docs/PHASE4.md) | Normative Phase 4 spec (C ABI, C++ wrapper, ROS 2 ingest bridge, `sample_with_derivatives`). §0.0 records what this environment cannot gate. **ROS 2 *is* available, in `docker/tf2`** — an earlier revision of this line said otherwise and was wrong. What is missing is a second RMW, clang in that image, and a robot. |
| [`docs/PHASE5.md`](./docs/PHASE5.md) | Normative Phase 5 spec (frozen `.tft` arena, bag ingestion, `FORMAT_VERSION = 3`, diagnostic counters, `TFT001`–`TFT016`, `tf_tree top`). |
| [`docs/decisions/`](./docs/decisions/) | Decision-record process, retained for *future* decisions. `0002`–`0003` are superseded; [`0004`](./docs/decisions/0004-builder-time-edge-declaration.md), [`0005`](./docs/decisions/0005-the-shared-memory-seam.md) and [`0006`](./docs/decisions/0006-the-eight-phase-roadmap.md) are authoritative. |

## Project shape (Phase 1 — pure Rust)

```
crates/tf_tree_math/    no_std; SE(3)/SO(3), quats, dual quats; #![forbid(unsafe_code)]
crates/tf_tree_arena/   no_std+alloc; pointer-free arena + layout math (unsafe allowed)
crates/tf_tree_core/    no_std+alloc; the engine; unsafe only in buffer.rs / arena_view.rs
crates/tf_tree/         std facade; #![forbid(unsafe_code)]
crates/tf_tree_ipc/     std; rendezvous, lock file, fd passing (unsafe: one atfork shim)
crates/tf_tree_py/      PyO3 bindings; binds the Rust core directly, not the C ABI
crates/tf_tree_bench/   criterion + tf2 differential harness
crates/tf_tree_tf2_sys/ the tf2 side of the differential harness
crates/tf_tree_cli/     binary `tf_tree` (alias `tft`)
ros/tf_tree_ros/        ament_cmake package: the §5 ingest bridge. NOT a cargo crate.
xtask/                  loom / miri / bench-gate runners
```

`ros/tf_tree_ros/` needs `rclcpp`, which exists only inside `docker/tf2`, so it
is outside the cargo workspace and outside every host recipe — `cargo fmt`,
`clippy` and `nextest` cannot see it, exactly like `crates/tf_tree_tf2_sys`.
**`just ros-build` and `just ros-test` are its entire gate**; run them after
touching anything under `ros/`. It reaches the engine only through
`find_package(tf_tree CONFIG)` and `tf_tree_c`'s default-off `bridge` feature.

Phase 4 adds a C ABI crate and a header-only C++ wrapper. **The Python binding
does not go through it** — PyO3 binds Rust directly, and `docs/PHASE3.md` §0
records why (typed errors and zero-copy buffers do not survive a C boundary).

## Hard rules (from `docs/PROJECT.md` and `docs/PHASE1.md` — do not relitigate)

- **Dependency budget:** `tf_tree_core` = `libm` + `bytemuck` + `blake3` (no_std)
  and nothing else. `tf_tree_math` = `libm` + `bytemuck`. blake3 is the one
  addition, resolving a spec conflict over frame-name hashing.
- **Unsafe budget** (restated by [`0007`](./docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md);
  the old enumeration had been stale since Phase 2): `unsafe` is permitted **only
  at a boundary the compiler cannot see across**, and there are four —
  the arena's raw memory (`tf_tree_arena`, `tf_tree_core::{buffer, arena_view}`),
  the OS (`tf_tree_ipc`), a foreign runtime (`tf_tree_py`), and a foreign caller
  (`tf_tree_c`). A fifth kind needs a decision record.
  `#![forbid(unsafe_code)]` stays on `tf_tree_math`, `tf_tree` and `tf_tree_cli`
  — **the facade does not move**, because its provable safety is what lets a
  reader trust the C ABI's `unsafe` is confined to argument validation.
  Every `unsafe` block carries a `// SAFETY:` comment naming the invariant it
  relies on; every crate with `unsafe` carries a module `// SAFETY:` block and
  `#![deny(unsafe_op_in_unsafe_fn)]`.
- **No pointers in the arena; fixed capacity; no growth/realloc; `#[repr(C)]`
  everywhere; append-only `FrameId`/`EdgeId` (tombstone, never recycle).**
- **`ArcSwap`/`Arc`/`Box`/`Vec` inside an arena structure is forbidden**
  (`docs/PROJECT.md` §5 D4).
- **Do not weaken an atomic ordering** because a test passes on x86-64 — the loom
  tests and the aarch64 CI target exist for exactly that.
- No `String` in any error type or hot path; errors are `Copy` and name the
  offending edge. No `async`/runtime. No GPU/point-cloud/`deskew`. `f64` only.
- `LerpSlerp`'s right-invariance test is **supposed to fail** — do not "fix" it
  (`docs/PROJECT.md` §5 D5; `docs/PHASE1.md` §3.4).

## Commands

Everything goes through `just`; CI mirrors it 1:1.

| Recipe | What it does |
| --- | --- |
| `just build` | `cargo build --workspace --all-targets` |
| `just test` | `cargo nextest run --workspace` + doctests |
| `just lint` | `cargo fmt --check`, `clippy -D warnings`, `cargo deny check` |
| `just fmt` | auto-format + clippy `--fix` |
| `just loom` | concurrency model checking (`cargo xtask loom`) |
| `just miri` | UB checking on arena + core |
| `just bench` | benchmark suite + go/no-go gate |
| `just ros-build` / `just ros-test` | `ros/tf_tree_ros` in the container — nothing on the host can |
| `just tf2-check` | the container-only crates: `tf_tree_tf2_sys`, and `tf_tree_c --features bridge` |

Single test — Rust: `cargo nextest run -p tf_tree_math -- exp_log_roundtrip`.

## Decision-document workflow

The phase specs (`docs/PROJECT.md`, `docs/PHASE1.md`, `docs/PHASE2.md`) are the
contract for work already scoped. A change that those documents do not cover —
new public API, new crate boundary, a different concurrency protocol — still
starts as a `draft` decision record in [`docs/decisions/`](./docs/decisions/),
not as a PR. Read [`docs/decisions/README.md`](./docs/decisions/README.md) for
the lifecycle. When a decision is `ready`, implement it as stated; its
*Implementation plan* is the per-PR work breakdown.

Records `0002` and `0003` are **superseded** by `docs/PROJECT.md` and
`docs/PHASE1.md` and are kept only as history — cite the new documents, never
them. `0004` is still authoritative for the builder-time edge declaration API.
