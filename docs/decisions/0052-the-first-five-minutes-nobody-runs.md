# 0052: the site, and the first five minutes it is supposed to open with

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none. This record exists so the site is not built before its
first question is answered.

## Context

`docs/PHASE5.md` §10 asks for an mdBook site whose first-five-minutes path is `pip install transform_tree`, three lines, a real result. The site is unstarted; the path is unmet: the README quickstart is a from-source build, and `pip install transform_tree` is executed nowhere (`wheels.yml` runs only on dispatch and `v*` tags and tests no wheel).

## The question this record has to answer first

**Where would a `pip install transform_tree` smoke run, such that it is exercised before a release rather than during one?**

1. **A step in `wheels.yml`** installing the built wheel into a clean venv and running `scripts/quickstart_smoke.py`. Tag and dispatch only.
2. **A step in `ci.yml`** installing from PyPI. Per-PR, but tests the previous release and adds a network fetch.
3. **Neither:** record the from-source quickstart as the substitute and rewrite §10's bullet.

## Decided already

`scripts/quickstart_smoke.py` stays the single executor and the README the single source of the snippet; the site copies nothing (`docs/PROJECT.md` §6), its chapters `{{#include}}` maintained documents.

## Implementation plan

None until the open questions close. Then: the path decision; `book.toml` + `SUMMARY.md` + a `just book` recipe; a Pages workflow verified by a published URL.

## Open questions

1. **Where does a `pip install` smoke run**: `wheels.yml`, `ci.yml` against PyPI, or nowhere?
2. **Is a second published rendering wanted at all?** `docs.rs` has the API reference.
3. **Who enables Pages**, if 2 is yes. Maintainer-only.
