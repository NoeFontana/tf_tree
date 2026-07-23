# Contributing

Thanks for your interest in contributing to rust-python-template. This
document is the human-facing guide; agents should also read
[`CLAUDE.md`](./CLAUDE.md).

## Repository layout

```
Cargo.toml                       workspace root: shared metadata, lints, deps
pyproject.toml                   maturin build backend + uv dev deps
rust-toolchain.toml              stable channel, pinned
justfile                         single task surface
deny.toml                        cargo-deny configuration
crates/
├── rust-python-template-core/   pure-Rust core; #![forbid(unsafe_code)]
└── rust-python-template-ffi/    PyO3 bindings, publish = false
python/rust_python_template/     Python wrapper + .pyi type stubs
tests/
├── rust/                        cargo integration tests against -core
└── python/                      pytest against the FFI boundary
docs/decisions/                  architectural decision records
```

The pure-Rust core lives in `crates/rust-python-template-core` and is the
source of truth for the library's behavior. The FFI crate is a thin shell
around it that handles Python ↔ Rust conversion. Keep business logic out
of `-ffi`.

## Prerequisites

- Rust (stable, pinned by `rust-toolchain.toml`) via `rustup`.
- [`uv`](https://docs.astral.sh/uv/) for Python + venv.
- [`just`](https://github.com/casey/just) for the task surface.
- `cargo-nextest` (`cargo install cargo-nextest --locked`).
- `cargo-deny` (`cargo install cargo-deny --locked`).

## Quickstart

```sh
just bootstrap        # sync venv + build FFI extension in release mode
just test             # cargo nextest + pytest
just lint             # cargo fmt --check + clippy + ruff + pyright
just fmt              # auto-fix: cargo fmt + ruff format + ruff --fix
just audit            # cargo-deny: advisories, licenses, bans, sources
just build            # produce a release wheel in target/wheels/
```

Day-to-day iteration: `just develop` (debug build of the FFI extension) is
faster than rebuilding in release mode. Re-run `just develop` after any
Rust change.

## Workflow for significant changes

Anything that touches the public API, crate boundaries, build system, or
release process starts as a **decision document**:

1. Copy [`docs/decisions/template.md`](./docs/decisions/template.md) to
   `docs/decisions/NNNN-kebab-case-title.md` (next sequential number).
2. Fill in *Context*, *Decision*, *Rationale*, *Consequences*,
   *Implementation plan*. Status starts as `draft`.
3. Open a PR with **just the decision document**. Address review until the
   open questions are resolved.
4. Flip the status to `ready` in a follow-up commit. The decision is now
   the implementation contract.
5. Implement under PRs that link the decision number. Each PR maps to one
   step from the *Implementation plan*.
6. When all PRs are merged, flip the status to `implemented` and list the
   PR numbers in the *Implementation* field. This is the immutability
   lock — the document is now frozen.

Bug fixes, behavior-preserving refactors, and dependency bumps do not need
a decision document — just open a PR.

To revise an `implemented` decision, write a new decision that supersedes
it. See [`docs/decisions/README.md`](./docs/decisions/README.md).

## Pull-request checklist

Before opening a PR:

- [ ] `just lint` is clean.
- [ ] `just test` passes.
- [ ] `just audit` is clean (or the failure is explained in the PR).
- [ ] Public Rust items in `-core` have doc comments (`missing_docs` is a
      warn lint).
- [ ] If the change is architectural, the linked decision document is at
      `ready` or `implemented`.

## License

Contributions are dual-licensed under [Apache-2.0](./LICENSE-APACHE) and
[MIT](./LICENSE-MIT) at the contributor's option. By submitting a
contribution you agree to this dual-license terms.
