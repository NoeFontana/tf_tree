# CLAUDE.md — agent guidance for rust-python-template

This file is for AI coding agents working in this repository. Humans, see
[`CONTRIBUTING.md`](./CONTRIBUTING.md).

## Project shape

A mixed Rust/Python monorepo: a pure-Rust core, a thin PyO3 FFI layer, and a
Python wrapper. Built with `maturin` + `uv`, tasks run through `just`.

```
crates/
├── rust-python-template-core/   pure Rust; #![forbid(unsafe_code)]; source of truth
└── rust-python-template-ffi/    PyO3 → rust_python_template._core; publish = false;
                                 data conversion only, no business logic
python/rust_python_template/     user-facing Python wrapper + .pyi stubs
tests/rust/                      cargo integration tests against -core
tests/python/                    pytest against the FFI boundary
```

**Hard rules:**
- `-core` is `#![forbid(unsafe_code)]`. No unsafe, no exceptions. If you
  need unsafe interop, it goes in `-ffi` with a `// SAFETY:` comment that
  states the invariants relied on.
- `-core` has zero Python dependencies. It must compile and test without
  libpython.
- `-ffi` does conversion only. Business logic must live in `-core`.
- Stable Rust only. Channel is pinned by `rust-toolchain.toml`.
- The workspace `[lints]` table denies `unwrap_used`, `expect_used`,
  `panic`, `todo`, `unimplemented`, `dbg_macro`. If you reach for any of
  these, you are signaling that a real error type is missing.

## Commands

Every workflow goes through `just`. CI mirrors these recipes 1:1.

| Recipe | What it does |
| --- | --- |
| `just bootstrap` | `uv sync --all-extras` + `uv run maturin develop --release`. Run once after clone. |
| `just develop` | Rebuild the FFI extension in debug mode (fast iteration). |
| `just build` | Build a release wheel into `target/wheels/`. |
| `just test` | Full suite: `cargo nextest run --workspace` then `pytest`. |
| `just test-rust` | Rust tests only. |
| `just test-py` | Python tests only. |
| `just lint` | `cargo fmt --check`, `clippy -D warnings`, `ruff check`, `ruff format --check`, `pyright`. |
| `just fmt` | Auto-fix: `cargo fmt`, `ruff format`, `ruff check --fix`. |
| `just audit` | `cargo deny check` (advisories, licenses, bans, sources). |
| `just clean` | Remove `target/`, `.venv/`, built `_core*.so` artifacts. |
| `just versions` | Print toolchain versions. |

## Single-test invocations

- Rust: `cargo nextest run -p rust-python-template-core -- add_works`
- Python: `uv run pytest tests/python/test_add.py::test_add`

## Prerequisites

- `rustup` with the stable channel (`rust-toolchain.toml` enforces this).
- `uv` (Python + venv + dep management).
- `just`.
- `cargo-nextest` (`cargo install cargo-nextest --locked`).
- `cargo-deny` (`cargo install cargo-deny --locked`).

## Decision-document workflow

Architectural changes (public API, crate boundaries, build/release process)
start as a `draft` decision in [`docs/decisions/`](./docs/decisions/), not
as a PR. Read [`docs/decisions/README.md`](./docs/decisions/README.md) for
the lifecycle. When status is `ready`, the *Decision* is final; implement
it as stated — do not redesign mid-flight. If you find an unresolved
question while implementing a `ready` doc, stop and ask.

## Conventions you might not pick up from the code

- **Errors:** use `thiserror` in `-core` for typed errors. `-ffi` converts
  them to `PyErr` at the boundary.
- **Docs:** `missing_docs` is a warn-by-default lint. Every public item in
  `-core` should have a doc comment.
- **PyO3 version:** 0.28 with `abi3-py310`. One wheel covers Python
  3.10–3.14. Do not pin a specific Python version.
- **`unreachable_pub`** is a warn lint; prefer `pub(crate)` for items not in
  the public API.

## What's deliberately out of scope

CLI, fuzzing, benchmarks, reference-impl oracle, real-model integration
tests, mkdocs user docs, design docs, slow-tier CI workflow. See the
"Opt-in extensions" section of [`docs/decisions/README.md`](./docs/decisions/README.md).
These are documented recipes, not promised features.
