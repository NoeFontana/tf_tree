# 0051: the licence travels with the artifact, not with the file

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** the `docs/PHASE5.md` §10 bullet and §0.0 row that cite this
record. **No source file changes, and that is the decision.**

## Context

`docs/PHASE5.md` §10 lists "license headers"; *not started* is indistinguishable
from *declined*. No source file carries an `SPDX-License-Identifier`, and D20 says
nothing about per-file markers. Apache-2.0 §4(a)/(d) and MIT require the text and `NOTICE` to
travel with the distribution, and that is met and asserted: each publishable crate
carries `LICENSE-MIT`, `LICENSE-APACHE` and `NOTICE` as symlinks to the root copies,
and `pyproject.toml`'s `[tool.maturin] include` carries them into the wheel;
`release.yml`, `just release-archive` and `wheels.yml` assert it.

## Decision

**No per-file licence headers. The obligation is on the artifact and it is
asserted where the artifact is built.** `docs/PHASE5.md` §10 closes the item as
*declined with an argument*.

## Rationale

A header nothing checks drifts, and checking it costs a gate on every file forever:
the choice is *headers plus a gate* or *no headers*; headers on a subset are worst,
since absence on the rest reads as a statement about them.

## Implementation plan

Nothing beyond `docs/PHASE5.md` §10's bullet and §0.0 row citing this record. If headers ever land they land **all at once and under a check**, derived from `[workspace.package] license`.

## Open questions
None.
