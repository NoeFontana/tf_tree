# 0052: the site, and the first five minutes it is supposed to open with

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none. This record exists so the site is not built before its
first question is answered.

## Context

`docs/PHASE5.md` §10 asks for "a documentation site (mdBook): a first-five-minutes
path that works — `pip install transform_tree`, three lines, a real result —
before any architecture prose." That is two things.

**The site half is unstarted**: no `book.toml`, `SUMMARY.md`, recipe or workflow,
and GitHub Pages is not enabled (a repository setting, not a commit).

**The path half is unmet in a way that reads as met.** `README.md`'s *Start with
no data at all* is executed on every PR (`scripts/quickstart_smoke.py`,
`ci.yml`'s `python` job), but it leads with `just quickstart` — a clone, `uv` and
`maturin develop`, a from-source build. `pip install transform_tree` appears once
in the README and is executed nowhere: `wheels.yml` says **"Nothing here tests a
wheel"** and runs only on `workflow_dispatch` and `v*` tags. So the audience §4
puts first, someone with a recording and no Rust toolchain, is offered a path
that needs one.

## The question this record has to answer first

**Where would a `pip install transform_tree` smoke run, such that it is exercised
before a release rather than during one?** (`scripts/sbom.py` is the cautionary
case: its only caller was a tag-gated job, so it produced nothing on any path.)

1. **A step in `wheels.yml`** installing the built wheel into a clean venv and
   running `scripts/quickstart_smoke.py`. Tests this release's artifact; tag and
   dispatch only, the SBOM shape.
2. **A step in `ci.yml`** installing from PyPI and running the same snippet.
   Per-PR, but tests the *previous* release and adds a network fetch to the PR
   path.
3. **Neither: record the from-source quickstart as the substitute** and rewrite
   §10's bullet. Cheapest, and closes the item as *decided*.

## Decided already

`scripts/quickstart_smoke.py` stays the single executor and the README the single
source of the snippet; the site copies nothing (`docs/PROJECT.md` §6), its
chapters `{{#include}}` maintained documents. §0.0's §10 row and §13's box 18
name the outstanding set: the site, a signing key, and the `pip install` path.

## Implementation plan

None until the open questions close. Then, in order:

1. The path decision — a green workflow step, or §10's bullet rewritten.
2. `book.toml` + `SUMMARY.md` + a `just book` recipe.
3. A Pages workflow — verified by a published URL (needs a repository setting).

## Open questions

1. **Where does a `pip install` smoke run** — `wheels.yml`, `ci.yml` against PyPI,
   or nowhere, with the from-source quickstart as the substitute?
2. **Is a second published rendering wanted at all?** `docs.rs` has the API reference.
3. **Who enables Pages**, if 2 is yes. Maintainer-only, like the signing key.
