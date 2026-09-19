# 0061: A freeze protects the argument, not the pointers

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** —

## Context

An `implemented` record is frozen: drift is fixed by a record that supersedes it.
That also freezes its `path.rs:NNN` citations, which rot whenever doc is added
above a cited item. `docs/decisions/README.md` is the only place to correct them,
so its erratum grows, and `scripts/line-citation-budget.txt` ratchets citation
*counts* but cannot fail a wrong-line citation, because no repair exists.

## Decision

A frozen record may be edited to **convert a line citation into a symbol
citation**, and for nothing else: a citation naming a line in
`crates/tf_tree/src/open.rs` becomes `reclamation_verdict` in that file. No other
word of the record is touched. The diff shape is checkable: a `:NNN` deleted and a
backticked identifier added on the same line. `docs/decisions/README.md` states
the exception where it states the freeze and its erratum shrinks to the general
statement.

## Consequences

- `just lint` can **validate** citation targets rather than only count them,
  because every failure now has a legal repair.
- A diff touching a frozen record is acceptable only as a citation conversion.

## Implementation plan

1. Amend the freeze rule in `docs/decisions/README.md`.
2. Convert the broken prefixed citations in frozen records to symbols.
3. Extend `scripts/artifact-versions.py` to fail on a prefixed citation whose
   target line is blank, a bare comment marker, a closing brace, or past EOF —
   verified by two mutants: a citation moved onto a blank line must fail, a
   correct one must pass.

## Open questions

1. Does the validator cover the **bare** `tree.rs:123` spelling in the same step?
2. Does it scan `crates/**/*.rs` doc comments, or wait for its own record?
3. Should a converted citation keep the line number as a hint? (Leaning: no.)
