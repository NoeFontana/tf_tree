# 0052: the site, and the first five minutes it is supposed to open with

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none. This record exists so the site is not built before its
first question is answered.

## Context

`docs/PHASE5.md` §10 asks for one thing in one sentence and it is two things:

> Documentation site (mdBook): a first-five-minutes path that works —
> `pip install transform_tree`, three lines, a real result — before any
> architecture prose.

**The site half is unstarted**, and has no partial state to reason about: no
`book.toml`, no `SUMMARY.md`, no recipe, no workflow, and no publishing surface
at all — `grep -rn 'pages:' .github/workflows/*.yml` finds nothing, so GitHub
Pages is not enabled and enabling it is a repository setting rather than a
commit.

**The path half is the one worth a record, because it is not merely unstarted —
it is unmet in a way that reads as met.** §0.0's §10 row and §13's box 18 both
enumerated what remains and named only the site and a signing key, and this
repository does have an executed quickstart: `README.md`'s *Start with no data
at all* section, extracted and run by `scripts/quickstart_smoke.py` and executed
on every pull request by `ci.yml`'s `python` job. So the checklist looks
covered.

It is a different path from the one §10 names. The README section leads with

```sh
just quickstart        # uv-managed interpreter + venv, extension installed
```

which is a clone, `uv`, and `maturin develop` — a *from-source developer build*.
`pip install transform_tree` appears once in the whole README, in a table row,
and is executed nowhere: `wheels.yml` builds and publishes wheels, its own
header says **"Nothing here tests a wheel"**, and it triggers only on
`workflow_dispatch` and `v*` tags.

**So the audience §4 puts first — somebody holding a recording and no Rust
toolchain — is offered a path that requires a Rust toolchain, and the path that
does not require one has never been executed by anything.**

## The question this record has to answer first

Not "which mdBook theme". This:

**Where would a `pip install transform_tree` smoke run, such that it is
exercised before a release rather than during one?**

That question is sharp right now because this wave answered the same question
badly once already, elsewhere: `scripts/sbom.py`'s only caller was a tag-gated
release job, added after the last tag, so it had produced nothing on any path
and no release carries an SBOM. **Adding a wheel-install step to `wheels.yml`
would repeat that exactly** — tag-gated, unexercised, and green in the checklist
until the release that needs it.

The candidates, none free:

1. **A step in `wheels.yml`** that installs the built wheel into a clean venv
   and runs `scripts/quickstart_smoke.py` (which executes whatever interpreter
   runs it, so it needs no new script). Tests the artifact this release
   produces. Runs on a tag and on `workflow_dispatch` only — the SBOM shape.
2. **A step in `ci.yml`** that `pip install`s `transform_tree` **from PyPI** and
   runs the same snippet. Runs on every PR, and tests the *previous* release
   rather than this commit — a red run would mean "the last release is broken",
   which is worth knowing and is not what a PR gate is usually for. It also puts
   a network fetch in the pull-request path.
3. **Neither: record the from-source quickstart as the substitute** and rewrite
   §10's bullet to say so. Cheapest, honest, and closes the checklist item as
   *decided* rather than leaving it as *unstarted*.

## What is decided already, and is not open

* **The README's snippet is not copied anywhere.** Whatever the answer,
  `scripts/quickstart_smoke.py` stays the single executor and the README stays
  the single source of the snippet and of its expected output.
* **The site, if built, does not become a second copy of anything.** An mdBook
  chapter that restates a `§0.0` table is a second rendering of normative bytes,
  and `docs/PROJECT.md` §6 already names that shape. Chapters `{{#include}}` the
  maintained documents or they do not exist.
* **`{{#include ../README.md}}` for the quickstart chapter is not automatically
  right**, and that is the concrete way the two halves of §10's sentence
  interact: the README's quickstart leads with `just quickstart`, so including
  it wholesale gives the site a first-five-minutes chapter that opens with a
  clone. Whichever option above wins decides what that chapter includes.

## Consequences

* Until this is answered, §0.0's §10 row and §13's box 18 name the same
  outstanding set: the site, a signing key, and the `pip install` path. That is
  the correction this record's `Context` records — the `pip install` path was
  missing from both rows, which had it filed as done.
* A site built before the answer would publish the wrong first page, and the
  wrong first page is the one thing §10 says must come "before any architecture
  prose".

## Implementation plan

None until the open questions close. When they do, in order:

1. The path decision — verified by whichever of the three lands: a green step in
   the workflow it was put in, or the §10 bullet rewritten to name the
   from-source path as the substitute.
2. `book.toml` + `SUMMARY.md` + a `just book` recipe — verified by `just book`
   building locally and by `just artifact-versions`, which asserts every `just`
   recipe a document names exists.
3. A Pages workflow — verified by a published URL, which needs the repository
   setting a commit cannot make.

## Open questions

1. **Where does a `pip install` smoke run** — `wheels.yml` (tag-gated, the SBOM
   shape), `ci.yml` against PyPI (per-PR, tests the previous release), or
   nowhere, with the from-source quickstart recorded as the substitute?
2. **Is a second published rendering of these documents wanted at all?**
   `docs.rs` already publishes the API reference for all five published crates
   and the README badges it, so a book adds a narrative layer, not a missing
   reference — and the honest reason it does not exist is that nobody has asked
   for it.
3. **Who enables Pages**, if the answer to 2 is yes. Maintainer-only, in the
   same class as the signing key.
