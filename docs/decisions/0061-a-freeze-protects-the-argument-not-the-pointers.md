# 0061: A freeze protects the argument, not the pointers

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** —

## Context

An `implemented` record is frozen: drift is fixed by a record that supersedes
it, never by editing it. That rule protects what a record *decided*, and it
currently also protects its `path.rs:NNN` citations — which decide nothing.

Those citations rot without anyone touching the record. Rustdoc added above a
cited item moves every citation below it, so a one-function doc edit can
invalidate a dozen sites in five frozen records at once. A sweep on 2026-09-19
found prefixed citations in tracked Markdown landing on blank lines, bare `///`
markers and closing braces; several predate any recorded shift.

Two costs follow, and the second is the reason this is a record and not a
cleanup:

1. `docs/decisions/README.md` is the only place a correction to a frozen record
   can live, so every shift adds to one shared erratum. That erratum grew to
   fifty-four lines, embedded three corrections of its own earlier revisions,
   and ended by telling the reader the citations are *unreliable* rather than
   repairable — which is the honest summary and also an admission that the
   enumeration was worth nothing.
2. **The gate cannot be made to check what it counts.**
   `scripts/line-citation-budget.txt` ratchets citation *counts* per file. It
   cannot fail on a citation that points at the wrong line, because the repair
   for a frozen record's broken pointer does not exist. So the one check that
   would catch this class is unavailable for as long as the freeze covers
   pointers.

## Decision

A frozen record may be edited to **convert a line citation into a symbol
citation**, and for nothing else. A citation naming a line in
`crates/tf_tree/src/open.rs` becomes `reclamation_verdict` in that file. The surrounding sentence
is not rewritten, no claim is added, retracted or rephrased, and the commit
touches no other word of the record.

`docs/decisions/README.md` states the exception where it states the freeze.

## Rationale

A citation is a pointer to evidence, not a claim about it. The sentence around
it already names what it is pointing at — every repair made in #361 found the
symbol in the record's own adjacent prose — so converting it changes what the
record asserts by nothing, and changes what a reader can *follow* from "wrong
line" to "the item".

The alternative is the status quo: a permanently growing erratum, and a gate
that can never validate its own corpus. Superseding a record to fix a pointer
is the other alternative and is worse than both — it spends a record number on
a typo and leaves the frozen text still pointing at a closing brace.

The risk is that "mechanical repair" becomes a door for substantive edits. The
mitigation is the diff shape, which is checkable: a conversion deletes a
`:NNN` and adds a backticked identifier on the same line.

## Consequences

- The shared erratum in `README.md` shrinks to the general statement and stops
  growing. Per-shift enumerations are deleted, not maintained.
- `just lint` can be extended to **validate** citation targets rather than only
  count them, because every failure it reports now has a legal repair. That is
  the check this project has been missing, and it is what makes trimming an
  over-long doc comment safe.
- Reviewers gain an obligation: a diff touching a frozen record is only
  acceptable if it is a citation conversion.
- Nothing is claimed about Rust doc comments, which carry the same citations
  and which no gate has ever read.

## Implementation plan

1. Amend the freeze rule in `docs/decisions/README.md` — verified by the rule
   and its exception being stated in one place.
2. Convert the broken prefixed citations in frozen records to symbols — verified
   by the validator in step 3 passing over tracked Markdown.
3. Extend `scripts/artifact-versions.py` to fail on a prefixed citation whose
   target line is blank, a bare comment marker, a closing brace, or past EOF —
   verified by two mutants: a citation moved onto a blank line must fail, and a
   correct one must pass.

## Open questions

1. Does the validator extend to the **bare** `tree.rs:123` spelling in the same
   step, or is that a second pass? The bare form is the larger corpus and has
   never been gated at all.
2. Rust doc comments carry these citations too and nothing reads them. Does the
   validator scan `crates/**/*.rs`, or does that wait for its own record?
3. Should a converted citation keep the line number in parentheses as a hint,
   or does that reintroduce exactly the rot being removed? (Leaning: no hint.)
