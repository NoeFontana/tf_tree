# CLAUDE.md — agent guidance for tf_tree

This file is for AI coding agents working in this repository. Humans, see
[`CONTRIBUTING.md`](./CONTRIBUTING.md).

## What this is

`tf_tree` is a transform tree engine (a faster, more scalable alternative to ROS
`tf2`). It is being built along a fixed six-phase roadmap. **The decisions in
[`docs/decisions/`](./docs/decisions/) are the contract** — read
[`0002`](./docs/decisions/0002-tf-tree-architecture.md) (architecture + decision
log D1–D16) and [`0003`](./docs/decisions/0003-phase-1-single-process-core.md)
(the full Phase 1 spec) before touching code. When a `ready` decision does not
answer a question, **stop and ask** — do not invent an answer, especially in the
concurrency or arena-layout sections.

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

## Hard rules (from the decisions — do not relitigate)

- **Dependency budget:** `tf_tree_core` = `libm` + `bytemuck` + `blake3` (no_std)
  and nothing else. `tf_tree_math` = `libm` + `bytemuck`. blake3 is the one
  addition, resolving a spec conflict over frame-name hashing.
- **Unsafe budget:** `#![forbid(unsafe_code)]` on `tf_tree_math`, `tf_tree`,
  `tf_tree_cli`. `unsafe` is permitted only in `tf_tree_arena` and in
  `tf_tree_core::{buffer, arena_view}`, each with a module `// SAFETY:` block and
  a per-block `// SAFETY:` comment naming the invariant relied on.
- **No pointers in the arena; fixed capacity; no growth/realloc; `#[repr(C)]`
  everywhere; append-only `FrameId`/`EdgeId` (tombstone, never recycle).**
- **`ArcSwap`/`Arc`/`Box`/`Vec` inside an arena structure is forbidden** (D4).
- **Do not weaken an atomic ordering** because a test passes on x86-64 — the loom
  tests and the aarch64 CI target exist for exactly that.
- No `String` in any error type or hot path; errors are `Copy` and name the
  offending edge. No `async`/runtime. No GPU/point-cloud/`deskew`. `f64` only.
- `LerpSlerp`'s right-invariance test is **supposed to fail** — do not "fix" it.

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

Architectural changes start as a `draft` decision in
[`docs/decisions/`](./docs/decisions/), not as a PR. Read
[`docs/decisions/README.md`](./docs/decisions/README.md) for the lifecycle. When
a decision is `ready`, implement it as stated; its *Implementation plan* is the
per-PR work breakdown.
