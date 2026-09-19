# Contributing

Agents also read [`CLAUDE.md`](./CLAUDE.md); the crate tree is in
[`README.md`](./README.md#workspace).

`docs/PROJECT.md` and `docs/PHASE1.md` are the contract; read them in that order
before proposing a change. `docs/PHASE2.md` §1 (amendments A1–A8) before altering
a concurrency protocol. Each spec's §0.0 status table outranks this file.

`crates/tf_tree_py` is excluded from the cargo workspace (it links libpython);
`just py-test` / `just py-lint` are its gate.

## Prerequisites

`rustup` (stable per `rust-toolchain.toml`, plus `nightly` with `miri`),
[`just`](https://github.com/casey/just), `cargo-nextest`, `cargo-deny`.

## Workflow for significant changes

Work scoped by a phase spec cites its section in the PR. Anything else touching
the public API, crate boundaries, build system or release process starts as a
**decision document**:

1. Copy [`docs/decisions/template.md`](./docs/decisions/template.md) to
   `docs/decisions/NNNN-kebab-case-title.md` (next number); status starts `draft`.
2. Open a PR with just the record; once its open questions are resolved, flip the
   status to `ready`.
3. Implement under PRs that link the number, one per *Implementation plan* step.
4. When all merge, flip the status to `implemented` and list the PR numbers.

Bug fixes, behavior-preserving refactors and dependency bumps need no record.

## Pull-request checklist

- [ ] `just lint`, `just test` and `just audit` pass (plus `just loom` /
      `just miri` for concurrency or arena code).
- [ ] The `CHANGELOG.md` `[Unreleased]` entry says what changed and whether it
      breaks anything, and links the argument.
- [ ] Every `unsafe` block has a `// SAFETY:` comment naming its invariant, within
      its budgeted crate/module.
- [ ] An architectural change cites the spec section or `ready`/`implemented`
      decision it implements.

## Releasing

`release.yml` and `wheels.yml` fire on a `v*` tag and publish irreversibly.

**Signed tags** (`docs/PHASE5.md` §10). One-time setup:

```sh
git config --global gpg.format ssh
git config --global user.signingkey ~/.ssh/id_ed25519.pub
git config --global tag.gpgSign true       # sign every annotated tag
```

Add the same public key to GitHub **as a signing key** and set the repository
variable `REQUIRE_SIGNED_TAGS` to `true`; an unsigned tag is then refused.

**The SBOM** comes from `scripts/sbom.py` (`just sbom <version>`) and is attached
to the release.

## License

Contributions are dual-licensed [Apache-2.0](./LICENSE-APACHE) / [MIT](./LICENSE-MIT).
