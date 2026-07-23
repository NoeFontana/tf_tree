# rust-python-template

Rust-core + PyO3 + Python-wrapper monorepo template. `maturin` + `uv` build,
`just` task surface, abi3 wheels (one per OS/arch covers Python 3.10–3.14),
dual MIT / Apache-2.0.

## Setup

```sh
# Create from template on GitHub, then clone, then:
./setup.sh          # prompt for project name / author / owner, rename, commit, self-delete
just bootstrap      # uv sync + maturin develop --release
```

## Commands

```sh
just test           # cargo nextest + pytest
just lint           # fmt + clippy + ruff + pyright
just build          # release wheel to target/wheels/
just audit          # cargo-deny
```

`just --list` for everything.

## Layout

```
crates/<name>-core/    pure Rust, #![forbid(unsafe_code)], source of truth
crates/<name>-ffi/     PyO3 bindings, publish = false, conversion only
python/<name>/         Python wrapper + .pyi stubs
tests/{rust,python}/   integration tests at each boundary
docs/decisions/        ADRs with draft → ready → implemented lifecycle
```

[`CONTRIBUTING.md`](./CONTRIBUTING.md) for the workflow,
[`CLAUDE.md`](./CLAUDE.md) for agent guidance.
