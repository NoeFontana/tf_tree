# 0051: the licence travels with the artifact, not with the file

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** the `docs/PHASE5.md` §10 bullet and §0.0 row that cite this
record. **No source file changes, and that is the decision.**

## Context

`docs/PHASE5.md` §10 lists "Apache-2.0 / MIT dual (D30), license headers,
`NOTICE`, SBOM per release". Three are done; per-file headers were never started,
and *not started* is indistinguishable from *declined* in a checklist.

- No source file carries an `SPDX-License-Identifier`, so a header pass would
  rewrite every source file with no partial state to build on.
- Nothing else asks for headers: `PROJECT.md` D20 decides the dual licence and
  says nothing about per-file markers.
- What the licences require is already met and asserted. Apache-2.0 §4(a)/(d)
  and MIT require the text and `NOTICE` to travel with the distribution, not a
  per-file marker. Each publishable crate carries `LICENSE-MIT`, `LICENSE-APACHE`
  and `NOTICE` as symlinks to the root copies, and `pyproject.toml`'s
  `[tool.maturin] include` carries them into the wheel. Asserted by `release.yml`
  (all five crates.io tarballs, plus `LICENSE-MIT`'s byte count, since a checkout
  without `core.symlinks` ships a path fragment), `just release-archive`, and
  `wheels.yml` (every `License-File:` header in `PKG-INFO` against the sdist; none
  is a refusal).

## Decision

**No per-file licence headers. The obligation is on the artifact and it is
asserted where the artifact is built.**

`docs/PHASE5.md` §10's bullet and §0.0 row cite this record, so the item is
closed as *declined with an argument*.

## Rationale

**A header nothing checks drifts, and checking it costs a gate on every file
forever.** `just msrv` and `just artifact-versions` exist because hand-copied
strings drifted. Headers are the same shape in more places. The choice is
*headers plus a gate* or *no headers*; the middle option decays. The gate costs a
check on every new source file plus a commit touching every blame line, for a
file-level marker nobody has asked for, when the root, every crate directory and
every artifact already carry both texts.

**REUSE** (`reuse lint`, `.reuse/dep5`, `LICENSES/`) is the shape any future
headers take, in a record that supersedes this one. **Headers on a subset is the
worst option**: absence on the rest reads as a statement about them.

## Consequences

* Per-file licence tools must read the manifests (`[workspace.package] license`);
  a single copied file is unmarked, the accepted cost.
* If headers ever land they land **all at once and under a check**, the
  expression derived from `[workspace.package] license`, never hand-typed.

## Implementation plan

1. This record — verified by `docs/PHASE5.md` §10's bullet and §0.0 row citing it.
2. Nothing else. No step touches a source file.

## Open questions

None. What would reopen this: a downstream requirement for file-level licence
provenance, answered by a REUSE record that supersedes this one.
