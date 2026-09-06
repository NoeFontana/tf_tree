# 0051: the licence travels with the artifact, not with the file

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** the `docs/PHASE5.md` §10 bullet and §0.0 row that cite this
record. **No source file changes, and that is the decision.**

## Context

`docs/PHASE5.md` §10 lists the open-source readiness items and one of them is
three words long:

> Apache-2.0 / MIT dual (D30), license headers, `NOTICE`, SBOM per release.

Three of those four are done. The fourth has never been started, and this record
exists because *not started* and *declined* are indistinguishable in a checklist
— which is how the §0.0 row that enumerates "what remains" came to omit it
entirely while the item was neither.

**What is true today**, read off the repository rather than remembered. **No
source file carries a marker**, and the pathspec is load-bearing rather than
tidy: this record quotes the literal, so a repository-wide search of it answers
about this paragraph and not about the source tree — the failure mode a
transcript in a document has that a command does not.

```
$ git grep -lI 'SPDX-License-Identifier' -- '*.rs' '*.py' '*.h' '*.hpp' '*.sh' '*.c' '*.cpp'
$ git ls-files '*.rs' '*.py' '*.h' '*.hpp' '*.sh' '*.c' '*.cpp' | wc -l
```

The first prints nothing; the second prints the size of the pass. So a header
pass is not an edit, it is a rewrite of every source file in the repository, and
there is no partial state to build on.

**Nothing else in the project asks for headers.** `D30` maps to `PROJECT.md`'s
**D20**, which is two sentences and decides the dual licence — "The Rust
ecosystem norm and the only choice compatible with industrial adoption. Not GPL,
not BSL." It says nothing about per-file markers. The §10 bullet is the only
place the phrase appears as a *requirement*:

```
$ grep -rIn -iE 'licen[cs]e header|per-file header|SPDX' docs/ CONTRIBUTING.md NOTICE README.md
```

No transcript is pasted under that one, because this record lives inside its
search space and every hit it grows is one of these paragraphs. Read the output:
one line is §10's bullet, and every other line is this record or a §0.0/§13 row
citing it — i.e. the bullet, and the argument about whether to honour it.

**What the licences actually require is already met, and asserted.** Apache-2.0
§4(a) requires a copy of the License to travel with the distribution and §4(d)
requires the `NOTICE` content to be carried; MIT requires its notice in all
copies or substantial portions. Neither requires a per-file marker — the
Apache appendix's boilerplate is offered as a convenience ("we recommend"), not
as a condition. Each publishable crate carries `LICENSE-MIT`, `LICENSE-APACHE`
and `NOTICE` as symlinks to the root copies, `pyproject.toml`'s
`[tool.maturin] include` carries them into the Python distribution, and three
separate checks assert they arrive:

* `release.yml` refuses if any of the five crates.io tarballs is missing one,
  and additionally asserts `LICENSE-MIT`'s **byte count**, because the source
  tree's licences are symlinks and a checkout without `core.symlinks` would
  ship a path fragment that lists correctly;
* `just release-archive` checks the same set into each binary archive;
* `wheels.yml` compares every `License-File:` header in `PKG-INFO` against the
  sdist's contents — and as of 2026-09-05 refuses when there are none, which
  it used to treat as nothing to check.

## Decision

**No per-file licence headers. The obligation is on the artifact and it is
asserted where the artifact is built.**

`docs/PHASE5.md` §10's bullet and its §0.0 row cite this record, so the
checklist item is closed as *declined with an argument* rather than left in the
state that made it invisible.

## Rationale

**A header nothing checks drifts, and checking it costs a gate on every file
forever.** This repository's own experience is the argument: `just msrv` exists
because a hand-written floor disagreed with the manifest, `just
artifact-versions` exists because four crates.io front pages said `0.0.1` for
three releases, and this wave found a lockfile four releases stale. A header is
the same shape — a hand-copied string, in more places than any of those, with
nothing compiling it. So the honest choice is between *headers plus a gate* and
*no headers*, and the middle option is the one that decays.

*Headers plus a gate* is a real option and it loses on cost against value. The
cost is a check on every new source file, in perpetuity, plus a 289-file commit
that touches every blame line in the repository. The value is a file-level
licence marker for a consumer who copies one file out of a dual-licensed
project whose root, every crate directory, and every published artifact already
carry both licence texts. Nobody has asked for it.

**REUSE is the shape this would take if it is ever wanted**, and it is a
different decision from the one §10's three words imply — a specification with a
tool (`reuse lint`), `.reuse/dep5` for files that cannot carry a comment, and a
`LICENSES/` directory with the exact SPDX texts. If a downstream compliance
scanner ever requires per-file provenance, that is the record to write, and it
supersedes this one. Writing hand-rolled headers now would be the second
spelling that record would have to delete.

**Headers on a subset is the worst option** — public crates only, or new files
only. A partially marked tree makes the absence of a header on the remaining
files look like a statement about those files.

## Consequences

* A tool that reads licences per file finds nothing here and must read the
  manifests, which is where `cargo`, crates.io, docs.rs and PyPI already read
  them from (`[workspace.package] license = "MIT OR Apache-2.0"`).
* Somebody who copies a single file out of this repository copies it with no
  marker on it. That is the accepted cost, and the mitigation is the one that
  already exists: the licence files are at the root, in every crate directory
  and in every artifact.
* If headers ever land they land **all at once and under a check**, with the
  expression derived from `[workspace.package] license` rather than typed. A
  hand-typed dual-licence expression in 289 files is a rename waiting to
  disagree with the manifest.
* §10's checklist gains a shape it did not have: an item may be *declined*, and
  a declined item names the record that declined it. `PHASE7.md` already works
  this way for a whole phase.

## Implementation plan

1. This record — verified by `docs/PHASE5.md` §10's bullet and §0.0 row citing
   it, and by `just artifact-versions`, whose relative-link scan resolves the
   citation.
2. Nothing else. There is deliberately no step that touches a source file.

## Open questions

None. What would reopen this is named in *Rationale*: a downstream requirement
for file-level licence provenance, answered by a REUSE record that supersedes
this one.
