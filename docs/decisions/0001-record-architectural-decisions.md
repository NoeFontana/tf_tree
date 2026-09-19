# 0001: Record architectural decisions in `docs/decisions/`

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** initial commit

## Decision

Architectural Decision Records rooted at `docs/decisions/`:

- One document type, skeleton in [`template.md`](./template.md).
- Four statuses on the document itself, no folder moves: `draft → ready → implemented → superseded by NNNN`.
- Two gates: `draft → ready` (open questions resolved, plan concrete) and an `implemented` immutability lock (revise by superseding).
- Sequential four-digit numbering, append-only.
- [`README.md`](./README.md) states the lifecycle, the gates and the agent handoff bar.

Anything touching the public API, the crate boundary, the build system or the release process starts as a `draft` here; routine work does not. A `ready → implemented` transition requires the linked PRs merged and listed in *Implementation*.
