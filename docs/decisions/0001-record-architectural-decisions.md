# 0001: Record architectural decisions in `docs/decisions/`

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** initial commit

## Context

A library has architectural decisions whose *why* outlives any single PR.
Without a durable home, those decisions get lost in commit messages, Slack
threads, and the heads of whoever was around at the time. New contributors —
human or agent — then re-litigate them or, worse, silently violate them.

This project is small today but is structured as a long-lived template: a
two-crate Rust workspace plus a Python wrapper, designed to be handed to
agents for implementation work. That handoff is the immediate forcing
function: an agent needs a stable, citable spec to implement against.

## Decision

Adopt a lightweight Architectural Decision Record (ADR) practice rooted at
`docs/decisions/`:

- A single document type, with the skeleton in [`template.md`](./template.md).
- Four statuses, on the document itself, no folder moves:
  `draft → ready → implemented → superseded by NNNN`.
- Two gates: a `draft → ready` review (open questions resolved, plan
  concrete) and an `implemented` immutability lock (revise by superseding,
  not editing).
- Sequential four-digit numbering, append-only.
- [`README.md`](./README.md) is the contract: it states the lifecycle, the
  gates, and the agent handoff bar.

Significant changes — anything that touches the public API, the crate
boundary, the build system, or the release process — start as a `draft`
here. Routine work (bugfixes, refactors that preserve behavior, dependency
bumps) does not need a decision.

## Rationale

- **Why ADRs at all.** They make architecture searchable, citable, and
  reviewable as a thing separate from code. The PR description is the wrong
  place for the *why* of a decision; it disappears into the merge queue.
- **Why a single document type.** Multiple types (RFC, ADR, design doc)
  create overhead and ambiguity about which to write. One type, four
  statuses, scales.
- **Why no folder moves.** Statuses are properties of the document, not the
  filesystem. Moving files breaks links and obscures history.
- **Why supersede instead of edit.** An `implemented` document is a
  historical record. Editing it makes the past mutable and erodes trust in
  what was decided.

Alternatives considered:

- **No ADRs, rely on commit messages and code comments.** Loses the *why*
  almost immediately; agent handoff becomes guesswork.
- **MADR / heavier ADR frameworks.** More structure than this project
  needs; we keep the lifecycle simple and the template short.
- **Design docs in `docs/design/` with no lifecycle.** Tracks the current
  state of a subsystem but not the discrete decisions that shaped it. Kept
  as an opt-in extension for later.

## Consequences

- Every architectural change starts as a draft here; no decision, no PR.
- Agents can be handed a `ready` doc and proceed without re-design.
- The folder grows append-only; we accept that some early decisions will
  be superseded later, and that's fine — the history is the point.
- A `ready → implemented` transition requires the linked PRs to be merged
  and the *Implementation* field to list them.

## Implementation plan

1. Create `docs/decisions/{README,template,0001-record-architectural-decisions}.md`
   — verified by these files existing in the initial commit.

## Open questions

None.
