# 0006: The eight-phase roadmap, and the decision numbers the Phase 4/5 specs assume

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** this record, plus the `docs/PROJECT.md` §4 rewrite it authorises

## Context

`docs/PHASE4.md` and `docs/PHASE5.md` landed as normative specifications that
re-cut `PROJECT.md` §4's six phases into eight:

| Phase | `PROJECT.md` §4 (old) | New specs |
|---|---|---|
| 4 | C ABI, C++ wrapper, `tf2_ros::Buffer` shim, bidirectional `/tf` bridge | C ABI, C++ wrapper, ROS 2 **ingress-only** bridge, `sample_with_derivatives` |
| 5 | Covariance, CoW branches, B-splines, MCAP record/replay, URDF, Rerun/Foxglove output | Frozen `.tft` arena, bag ingestion, diagnostic counters, diagnostics catalogue, `tf_tree top`, **no visualization** |
| 6 | Inter-host replication | Covariance, splines, CoW branches |
| 7 | — | `tf2_ros::Buffer` shim **and** arena → `/tf` egress |
| 8 | — | Inter-host replication |

The shim and egress move to Phase 7 **gated on evidence**; visualization becomes
"deliberately not built" (`PHASE5.md` §8 supersedes "Rerun and Foxglove output");
replication moves to Phase 8, invalidating D19's parenthetical and the
cross-domain-alignment note in `PROJECT.md`. The specs also cite decisions this
repository lacks (the log ends at D20): **D28**, **D29**, **D30**, **D34**, of
which D30 is this repository's D20.

## Decision

**The eight-phase roadmap in `PHASE4.md` and `PHASE5.md` is authoritative.**
`docs/PROJECT.md` §4 is rewritten to match and the dangling phase references are
corrected in the same commit. **D21 and D22 are added**, numbered in this
repository's sequence, with an alias table:

| Cited in the specs | This repository |
|---|---|
| D28, D29 | **D21** — the compatibility layer is Phase 7 and is gated on evidence |
| D30 | **D20** — Apache-2.0 / MIT dual license (already present, unchanged) |
| D34 | **D22** — a disabled feature never forks the layout hash |

**D21 — The compatibility layer is Phase 7, gated on evidence, not scheduled.**
`tf2_ros::Buffer` compatibility and arena → `/tf` egress do not begin until Phases
4 and 5 have produced operating experience: a real node on real hardware
(`PHASE4.md` §1) and offline/observability users who adopted nothing (`PHASE5.md`
§0). The shim is a hundred small judgements about ambiguous `tf2` behaviour, each
of which made without experience ships as a compatibility promise. Ingress-only in
Phase 4 removes every loopback, echo and authority-cycle question.

**D22 — A disabled feature never forks the layout hash.** When a cargo feature is
compiled out, the arena *regions* it would use stay declared and counted by
`layout_hash`; only the code that reads and writes them disappears. Sizing per
feature set would make `layout_hash` a function of the build, so two
correctly-built participants of one version would refuse to attach with a layout
mismatch naming no actionable cause. `PHASE5.md` §5.5 (`counters`) and §1.2 (the
Phase 6 regions) are its consumers.

## Rationale

The specs win over `PROJECT.md`: newer, more detailed, mutually consistent, and the
re-cut is *argued* (`PHASE5.md` §8.1). A record, because `CLAUDE.md` names
`PROJECT.md` and its §5 log as the contract. D21/D22 rather than backfilling to D34,
which would put content in the log that nobody wrote.

## Consequences

- `docs/PROJECT.md` §4 no longer says "six-phase roadmap" (`docs/README.md`,
  `CLAUDE.md`, `docs/RUNBOOK.md` checked), and stale phase references now read
  Phase 6 (B-splines, uncertainty) and Phase 8 (alignment, D19).
- A Phase 6 spec is owed the reserved regions from `PHASE5.md` §1.2 and the Phase 4
  surprise log.
- D22 binds every future cargo feature touching the arena; omitting a region to save
  space needs a `FORMAT_VERSION` bump.
- The alias table is load-bearing.

## Implementation plan

1. This record — listed `ready` in `docs/decisions/README.md`.
2. `docs/PROJECT.md` §4 rewritten; §5 gains D21 and D22; the dangling phase
   references corrected — `grep -rn 'six-phase\|Phase 5.*[Bb]-spline\|Phase
   6.*replication' docs/ CLAUDE.md` finds nothing contradicting this record.
3. `docs/README.md` and `CLAUDE.md` list `PHASE4.md` and `PHASE5.md` in the
   canonical-document table.

## Open questions

None. The content of the source document's D21–D27 is unknown here; the alias
table makes their absence visible.
