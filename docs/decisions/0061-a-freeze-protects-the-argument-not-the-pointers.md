# 0061: A freeze protects the argument, not the pointers

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** —

## Context

An `implemented` record is frozen, which also freezes its line citations; `scripts/line-citation-budget.txt` ratchets their counts but cannot fail a wrong-line citation, because no repair exists.

## Decision

A frozen record may be edited to **convert a line citation into a symbol citation**, and for nothing else (a line in `crates/tf_tree/src/open.rs` becomes `reclamation_verdict`). The diff shape is checkable: a line number deleted and a backticked identifier added on the same line. `docs/decisions/README.md` states the exception where it states the freeze.

## Implementation plan

1. Amend the freeze rule in `docs/decisions/README.md`.
2. Convert the broken prefixed citations in frozen records to symbols.
3. Extend `scripts/artifact-versions.py` to fail on a prefixed citation whose target line is blank, a bare comment marker, a closing brace, or past EOF, verified by two mutants.

## Open questions

1. Does the validator cover the **bare** spelling (no directory prefix) in the same step?
2. Does it scan `crates/**/*.rs` doc comments, or wait for its own record?
3. Should a converted citation keep the line number as a hint? (Leaning: no.)
