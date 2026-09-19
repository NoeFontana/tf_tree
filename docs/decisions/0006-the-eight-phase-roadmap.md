# 0006: The eight-phase roadmap, and the decision numbers the Phase 4/5 specs assume

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** this record, plus the `docs/PROJECT.md` §4 rewrite it authorises

## Context

`docs/PHASE4.md` and `docs/PHASE5.md` re-cut `PROJECT.md` §4's six phases into
eight and cite decisions this repository lacks (the log ended at D20).

## Decision

**The eight-phase roadmap in `PHASE4.md` and `PHASE5.md` is authoritative.**
`docs/PROJECT.md` §4 is rewritten to match. **D21 and D22 are added**, with an
alias table:

| Cited in the specs | This repository |
|---|---|
| D28, D29 | **D21** — the compatibility layer is Phase 7 and is gated on evidence |
| D30 | **D20** — Apache-2.0 / MIT dual license (unchanged) |
| D34 | **D22** — a disabled feature never forks the layout hash |

**D21 — The compatibility layer is Phase 7, gated on evidence, not scheduled.**
`tf2_ros::Buffer` compatibility and arena → `/tf` egress do not begin until Phases
4 and 5 have produced operating experience (`PHASE4.md` §1, `PHASE5.md` §0).

**D22 — A disabled feature never forks the layout hash.** When a cargo feature is
compiled out, the arena *regions* it would use stay declared and counted by
`layout_hash`; only the code that uses them disappears. Sizing per feature set
would make two correctly-built participants of one version refuse to attach.
`PHASE5.md` §5.5 (`counters`) and §1.2 (the Phase 6 regions) are its consumers.

## Consequences

D22 binds every future cargo feature touching the arena; omitting a region to save
space needs a `FORMAT_VERSION` bump. The alias table is load-bearing.

## Implementation plan

1. This record, listed in `docs/decisions/README.md`.
2. `docs/PROJECT.md` §4 rewritten; §5 gains D21 and D22.
3. `docs/README.md` and `CLAUDE.md` list `PHASE4.md` and `PHASE5.md`.

## Open questions

None.
