# Contributing

Thanks for your interest in contributing to tf_tree. This document is the
human-facing guide; agents should also read [`CLAUDE.md`](./CLAUDE.md).

## Repository layout

```
Cargo.toml                 workspace root: shared metadata, lints, deps
rust-toolchain.toml        stable channel, pinned
justfile                   single task surface
deny.toml                  cargo-deny configuration
crates/
├── tf_tree_math/          no_std SE(3)/SO(3) + dual quaternions; forbid(unsafe_code)
├── tf_tree_arena/         no_std+alloc pointer-free arena + layout math
├── tf_tree_core/          no_std+alloc engine: interning, topology, buffers, plans
├── tf_tree/               std facade: builder, plan-cached lookup, Display errors
├── tf_tree_bench/         criterion benches + tf2 differential harness
└── tf_tree_cli/           binary `tf_tree` (alias `tft`)
xtask/                     loom / miri / bench-gate runners
docs/PROJECT.md            overview, roadmap, decision log D1–D20 (§5)
docs/PHASE1.md             normative Phase 1 spec (the contract for current work)
docs/PHASE2.md             normative Phase 2 spec; §1 = Phase 1 amendments A1–A8
docs/decisions/            architectural decision records (process, kept for new decisions)
```

`docs/PROJECT.md` and `docs/PHASE1.md` are the contract — read them in that
order before proposing a change. `docs/PHASE2.md` §1 lists amendments A1–A8 to
Phase 1 that are agreed but **not yet applied**; check them before altering a
concurrency protocol so a Phase 1 change does not contradict one.

`tf_tree_core` is the source of truth. `tf_tree_math` and `tf_tree_arena` are
separately publishable and separately testable — keeping the math crate free of
`unsafe` and of the arena is what lets its property tests run under Miri in
seconds. Python bindings are Phase 3 and are not in this workspace yet.

## Prerequisites

- Rust (stable, pinned by `rust-toolchain.toml`) via `rustup`; a `nightly`
  toolchain with the `miri` component for `just miri`.
- [`just`](https://github.com/casey/just) for the task surface.
- `cargo-nextest` (`cargo install cargo-nextest --locked`).
- `cargo-deny` (`cargo install cargo-deny --locked`).

## Quickstart

```sh
just build            # cargo build --workspace
just test             # cargo nextest + doctests
just lint             # cargo fmt --check + clippy -D warnings + cargo-deny
just fmt              # auto-format + clippy --fix
just loom             # concurrency model checking
just miri             # UB checking (arena + core)
just bench            # benchmark suite + go/no-go gate
```

## Workflow for significant changes

Work already scoped by `docs/PHASE1.md` (or, later, `docs/PHASE2.md`) is
implemented directly against that spec — cite the section number in the PR.

Anything *not* covered there that touches the public API, crate boundaries, build
system, or release process starts as a **decision document** in
`docs/decisions/`. The process below is retained for exactly those cases:

1. Copy [`docs/decisions/template.md`](./docs/decisions/template.md) to
   `docs/decisions/NNNN-kebab-case-title.md` (next sequential number).
2. Fill it in; status starts as `draft`.
3. Open a PR with **just the decision document**; address review until the open
   questions are resolved, then flip the status to `ready` — the decision is now
   the implementation contract.
4. Implement under PRs that link the decision number; each PR maps to one step of
   the *Implementation plan*.
5. When all PRs merge, flip the status to `implemented` and list the PR numbers.
   This is the immutability lock.

Bug fixes, behavior-preserving refactors, and dependency bumps do not need a
decision document — just open a PR.

## Pull-request checklist

- [ ] `just lint` is clean.
- [ ] `just test` passes (and `just loom` / `just miri` if you touched the
      concurrency or arena code).
- [ ] `just audit` is clean (or the failure is explained in the PR).
- [ ] Public Rust items have doc comments (`missing_docs` is a warn lint, and CI
      builds docs with `-D warnings`).
- [ ] Every `unsafe` block has a `// SAFETY:` comment; `unsafe` stays within its
      budgeted crates/modules.
- [ ] If the change is architectural, it cites the `docs/PHASE1.md` /
      `docs/PHASE2.md` section it implements, or a linked decision that is
      `ready` or `implemented`.

## License

Contributions are dual-licensed under [Apache-2.0](./LICENSE-APACHE) and
[MIT](./LICENSE-MIT) at the contributor's option.
