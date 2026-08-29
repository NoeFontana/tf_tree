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
├── tf_tree_ipc/           rendezvous, lock file, fd passing
├── tf_tree_bridge/        ROS-independent half of the /tf ingest bridge
├── tf_tree_ingest/        MCAP bag ingestion
├── tf_tree_py/            PyO3 bindings; excluded from the cargo workspace
├── tf_tree_c/             C ABI + header-only C++ wrapper
├── tf_tree_tf2_sys/       tf2 side of the differential harness; excluded
├── tf_tree_bench/         criterion benches + tf2 differential harness
└── tf_tree_cli/           binary `tf_tree` (alias `tft`)
ros/                       ament_cmake packages; not cargo crates (`just ros-build`)
xtask/                     loom / miri / bench-gate runners
docs/PROJECT.md            overview, roadmap, decision log D1–D22 (§5)
docs/PHASE1.md             normative Phase 1 spec (implemented whole)
docs/PHASE2.md             normative Phase 2 spec; §1 = Phase 1 amendments A1–A8
docs/decisions/            architectural decision records (process, kept for new decisions)
```

`CLAUDE.md`'s *Project shape* is the same tree annotated with each crate's
unsafe and dependency budget. Five crates publish (`tf_tree`, `tf_tree_core`,
`tf_tree_math`, `tf_tree_arena`, `tf_tree_ipc`); the rest carry
`publish = false` with the reason in their manifest.

`docs/PROJECT.md` and `docs/PHASE1.md` are the contract — read them in that
order before proposing a change. `docs/PHASE2.md` §1 lists amendments A1–A8 to
Phase 1, **all of which are now applied**; read them before altering a
concurrency protocol, because they are why several orderings look the way they
do. §0.0 is the live status table and outranks this file.

This section used to say that
[`docs/decisions/0005`](./docs/decisions/0005-the-shared-memory-seam.md) scoped
"what is left (fd passing, liveness, reaping)". All three landed under `0005`.
What §0.0 still carries open is the daemon and tooling surface (§9, §10), the
long-running fault harness (§11.3, §11.4), and **ownership migration (§3.5),
which is not implemented and has no path today** — the takeover half was deleted
in #275 and [`0037`](./docs/decisions/0037-a-takeover-is-not-a-second-open.md)
records why a second `open()` cannot be it. Read §0.0's row, not this list.

`tf_tree_core` is the source of truth. `tf_tree_math` and `tf_tree_arena` are
separately publishable and separately testable — keeping the math crate free of
`unsafe` and of the arena is what lets its property tests run under Miri in
seconds. Phase 3 is implemented: the Python bindings live in `crates/tf_tree_py`
and are *excluded* from the cargo workspace, because they link libpython — so
`cargo build --workspace` never sees them and `just py-test` / `just py-lint`
are their gate.

## Prerequisites

- Rust (stable, pinned by `rust-toolchain.toml`) via `rustup`; a `nightly`
  toolchain with the `miri` component for `just miri`.
- [`just`](https://github.com/casey/just) for the task surface.
- `cargo-nextest` (`cargo install cargo-nextest --locked`).
- `cargo-deny` (`cargo install cargo-deny --locked`).

## Quickstart

```sh
just build            # cargo build --workspace --all-targets
just test             # cargo nextest + doctests + ingest-check
just lint             # fmt --check + eight clippy -D warnings passes, behind five gates
just audit            # cargo deny check (NOT part of `just lint`)
just fmt              # auto-format + clippy --fix
just loom             # concurrency model checking
just miri             # UB checking (arena + core + the facade's one unsafe)
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

## Changelog entries

`CHANGELOG.md`'s `[Unreleased]` section says **what changed, whether it breaks
anything, the PR number, and where the argument lives** — a decision record, a
spec section, or the module the reasoning is written on. It does not reproduce
that argument, restate a record's measured numbers, or re-litigate a trade-off
that already has a home; every argument keeps exactly one. A fact with no other
home stays in the entry, or moves to the document that should own it.

## Pull-request checklist

- [ ] `just lint` is clean.
- [ ] `just test` passes (and `just loom` / `just miri` if you touched the
      concurrency or arena code).
- [ ] `just audit` is clean (or the failure is explained in the PR).
- [ ] The `CHANGELOG.md` entry links its argument rather than repeating it.
- [ ] Public Rust items have doc comments (`missing_docs` is a warn lint, and CI
      builds docs with `-D warnings`).
- [ ] Every `unsafe` block has a `// SAFETY:` comment; `unsafe` stays within its
      budgeted crates/modules.
- [ ] If the change is architectural, it cites the `docs/PHASE1.md` /
      `docs/PHASE2.md` section it implements, or a linked decision that is
      `ready` or `implemented`.

## Releasing

`release.yml` and `wheels.yml` both fire on a `v*` tag and publish irreversibly —
crates.io and PyPI each refuse a re-upload of a version. The gates that matter
run on the tag, but two things are the maintainer's:

**Signed tags.** `docs/PHASE5.md` §10 lists this as the one open
release-automation item. The workflow checks whether the tag object carries a
signature and, by default, **warns**: the gate exists before a key does, and
making it a refusal today would block a release on something only you can create.
One-time setup:

```sh
# SSH signing needs no keyserver and reuses a key you already have.
git config --global gpg.format ssh
git config --global user.signingkey ~/.ssh/id_ed25519.pub
git config --global tag.gpgSign true       # sign every annotated tag
```

Then add the same public key to your GitHub account **as a signing key** (it is a
separate list from authentication keys), and set the repository variable
`REQUIRE_SIGNED_TAGS` to `true`. From that tag onward an unsigned tag is refused
rather than warned about. `git verify-tag v0.0.6` checks locally; GitHub's
"Verified" badge is what checks the signature against the key on the account,
which a runner cannot do because it does not have your public key.

**The SBOM** is generated for you — `scripts/sbom.py` from `cargo metadata`,
attached to the release and covered by `SHA256SUMS`. `just sbom <version>` writes
the same file locally. It walks the graph from the crates that actually ship over
`normal` edges only, so a dev-dependency never appears in a bill of materials for
something that does not contain it.

## License

Contributions are dual-licensed under [Apache-2.0](./LICENSE-APACHE) and
[MIT](./LICENSE-MIT) at the contributor's option.
