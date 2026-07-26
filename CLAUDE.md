# CLAUDE.md — agent guidance for tf_tree

This file is for AI coding agents working in this repository. Humans, see
[`CONTRIBUTING.md`](./CONTRIBUTING.md).

## What this is

`tf_tree` is a transform tree engine (a faster, more scalable alternative to ROS
`tf2`). It is being built along a fixed six-phase roadmap. **The canonical
documents in [`docs/`](./docs/) are the contract** — read
[`docs/PROJECT.md`](./docs/PROJECT.md) (project overview, architecture, roadmap,
and the decision log D1–D20 in its §5) **then**
[`docs/PHASE1.md`](./docs/PHASE1.md) (the full, normative Phase 1 spec) before
touching code. When these documents do not answer a question, **stop and ask** —
do not invent an answer, especially in the concurrency or arena-layout sections.

[`docs/PHASE2.md`](./docs/PHASE2.md) is the Phase 2 spec (shared memory). Its §1
holds **Phase 1 amendments A1–A8, which are now all applied** (`FORMAT_VERSION =
2`) — read them before changing any concurrency protocol, because they are the
reason several orderings look the way they do. §0.0 is the live status table for
what remains.

What remains is the **lifecycle**, scoped by decision
[`0005`](./docs/decisions/0005-the-shared-memory-seam.md): §3.7 fd passing, so
`tf_tree::open()` does not exist yet and only a child can attach; `F_OFD_GETLK`
liveness; and reaping. `0005` is `ready`, so its *Implementation plan* is the
work breakdown — implement it as stated, and if you find an open question, stop
and ask rather than inventing an answer.

| Document | Role |
| --- | --- |
| [`docs/PROJECT.md`](./docs/PROJECT.md) | Overview, architecture, roadmap, decision log D1–D20 (§5). Supersedes `docs/decisions/0002`. |
| [`docs/PHASE1.md`](./docs/PHASE1.md) | Normative Phase 1 spec: layouts, atomic orderings, test plan (§10), benchmark gate (§11). Supersedes `docs/decisions/0003`. |
| [`docs/PHASE2.md`](./docs/PHASE2.md) | Normative Phase 2 spec; §1 holds Phase 1 amendments A1–A8 (all applied), §0.0 is the status table. |
| [`docs/PHASE3.md`](./docs/PHASE3.md) | Normative Phase 3 spec (Python bindings). Blocked on `0005`; not started. |
| [`docs/decisions/`](./docs/decisions/) | Decision-record process, retained for *future* decisions. `0002`–`0003` are superseded; [`0004`](./docs/decisions/0004-builder-time-edge-declaration.md) (builder-time edge declaration) and [`0005`](./docs/decisions/0005-the-shared-memory-seam.md) (the shared-memory seam) are authoritative. |

## Project shape (Phase 1 — pure Rust)

```
crates/tf_tree_math/    no_std; SE(3)/SO(3), quats, dual quats; #![forbid(unsafe_code)]
crates/tf_tree_arena/   no_std+alloc; pointer-free arena + layout math (unsafe allowed)
crates/tf_tree_core/    no_std+alloc; the engine; unsafe only in buffer.rs / arena_view.rs
crates/tf_tree/         std facade; #![forbid(unsafe_code)]
crates/tf_tree_bench/   criterion + tf2 differential harness
crates/tf_tree_cli/     binary `tf_tree` (alias `tft`)
xtask/                  loom / miri / bench-gate runners
```

Python bindings are **Phase 3**, not now. Phase 1 is pure Rust.

## Hard rules (from `docs/PROJECT.md` and `docs/PHASE1.md` — do not relitigate)

- **Dependency budget:** `tf_tree_core` = `libm` + `bytemuck` + `blake3` (no_std)
  and nothing else. `tf_tree_math` = `libm` + `bytemuck`. blake3 is the one
  addition, resolving a spec conflict over frame-name hashing.
- **Unsafe budget:** `#![forbid(unsafe_code)]` on `tf_tree_math`, `tf_tree`,
  `tf_tree_cli`. `unsafe` is permitted only in `tf_tree_arena` and in
  `tf_tree_core::{buffer, arena_view}`, each with a module `// SAFETY:` block and
  a per-block `// SAFETY:` comment naming the invariant relied on.
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
