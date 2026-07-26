# 0006: The eight-phase roadmap, and the decision numbers the Phase 4/5 specs assume

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** this record, plus the `docs/PROJECT.md` §4 rewrite it authorises

## Context

`docs/PHASE4.md` and `docs/PHASE5.md` landed as normative specifications. Read
against `docs/PROJECT.md`, they are not consistent with it, and the
inconsistency is structural rather than cosmetic — it changes what phase a
reader believes a feature belongs to.

**The roadmap is re-cut from six phases to eight.** `PROJECT.md` §4 says:

| PROJECT.md §4 | Contents |
|---|---|
| Phase 4 | C ABI, C++ wrapper, **`tf2_ros::Buffer` shim, bidirectional `/tf` bridge** |
| Phase 5 | Covariance, CoW branches, B-splines, MCAP record/replay, URDF, **Rerun and Foxglove output** |
| Phase 6 | Inter-host replication |

The new specs say:

| New specs | Contents |
|---|---|
| Phase 4 | C ABI, C++ wrapper, ROS 2 **ingress-only** bridge, `sample_with_derivatives` |
| Phase 5 | Frozen `.tft` arena, bag ingestion, diagnostic counters, diagnostics catalogue, `tf_tree top`, **no visualization at all** |
| Phase 6 | Covariance, splines, CoW branches |
| Phase 7 | `tf2_ros::Buffer` shim **and** arena → `/tf` egress |
| Phase 8 | Inter-host replication |

Three specific collisions, each of which would mislead a reader of the older
document:

1. **The shim and the egress bridge move from Phase 4 to Phase 7**, and become
   *gated on evidence* rather than scheduled. `PHASE4.md` §0 and §1 are explicit
   that Phase 4 exists to produce that evidence.
2. **Visualization moves from "Phase 5 deliverable" to "deliberately not
   built."** `PHASE5.md` §8 is a five-subsection argument for building nothing,
   and it supersedes `PROJECT.md` §4's "Rerun and Foxglove output" outright.
3. **Replication moves from Phase 6 to Phase 8**, which invalidates the
   parenthetical in D19 and the cross-domain-alignment note at `PROJECT.md:99`.

**The specs also cite decision numbers this repository does not have.** The
decision log ends at D20. The new specs cite **D28**, **D29**, **D30** and
**D34**. One of these is a renumbering of an existing entry — `PHASE5.md` §10
cites "Apache-2.0 / MIT dual (D30)", which is this repository's **D20**. The
others carry content that is genuinely new and is currently asserted only inside
a phase spec, which is the wrong place for a project-level constraint.

Left alone, a future reader resolves the conflict by trusting `PROJECT.md` —
`CLAUDE.md` tells them to, and it is listed first in `docs/README.md`. They would
then plan a `tf2_ros::Buffer` shim into Phase 4 and a Rerun module into Phase 5,
both of which the newer specs explicitly reject.

## Decision

**The eight-phase roadmap in `PHASE4.md` and `PHASE5.md` is authoritative.**
`docs/PROJECT.md` §4 is rewritten to match, and the two dangling phase
references (D19's parenthetical, the cross-domain-alignment note) are corrected
in the same commit.

**Two new entries are added to the decision log, D21 and D22**, carrying the
content the new specs assume. They are numbered in this repository's sequence,
not the source document's. An alias table records the mapping so that a reader
meeting "D28" in a spec can find it:

| Cited in the specs | This repository |
|---|---|
| D28, D29 | **D21** — the compatibility layer is Phase 7 and is gated on evidence |
| D30 | **D20** — Apache-2.0 / MIT dual license (already present, unchanged) |
| D34 | **D22** — a disabled feature never forks the layout hash |

**D21 — The compatibility layer is Phase 7, and it is gated on evidence, not
scheduled.**
`tf2_ros::Buffer` API compatibility and arena → `/tf` egress are deferred to
Phase 7 and do not begin until Phases 4 and 5 have produced operating
experience: a real node on real hardware (`PHASE4.md` §1) and offline/observability
users who adopted nothing (`PHASE5.md` §0). The shim is a hundred small semantic
judgements about what `tf2` does when asked something ambiguous, and each one made
without operating experience is a guess that ships as a compatibility promise.
Ingress-only in Phase 4 is what buys this: one direction removes every loopback,
echo and authority-cycle question from the phase.

**D22 — A disabled feature never forks the layout hash.**
When a cargo feature is compiled out, the arena *regions* it would use remain
declared in the layout and continue to be counted by `layout_hash`. Only the
code that reads and writes them disappears. The alternative — sizing the arena
differently per feature set — makes `layout_hash` a function of the build
configuration, so two correctly-built participants of the same version would
refuse to attach to each other with a message about layout mismatch that names no
actionable cause. Wasting a region in a build that does not use it is cheap;
a version-skew diagnostic that lies is not.

`PHASE5.md` §5.5 is the first consumer of D22 (the `counters` feature), and §1.2
is the second (the Phase 6 covariance and spline regions, declared with offset
`0` meaning absent and filled in later without a further layout change).

## Rationale

**Why the specs win over `PROJECT.md`.** They are newer, they are far more
detailed, they are mutually consistent, and — decisively — the re-cut is
*argued* in them rather than merely asserted. `PHASE5.md` §8.1 does not simply
drop Rerun output; it explains that a user's bag already contains transforms
alongside the images and LiDAR, opens in Rerun today, and that anything we emit
is a re-encoding of data they can already see. That is a better argument than
the line it replaces, and reversing it would require refuting it.

**Why a decision record rather than editing `PROJECT.md` directly.**
`CLAUDE.md` names `PROJECT.md` as the contract and its §5 decision log as
hard constraints. Silently rewriting a roadmap and two decision entries in a
document described that way is exactly the move the decision process exists to
prevent. The edit is the *consequence* of this record, and it is traceable to it.

**Why D21 and D22 rather than backfilling D21–D34 to match the source
numbering.** Inventing twelve decisions to reach D34 would put content in the
log that nobody wrote. The alias table costs one lookup and asserts nothing
false. If the source document surfaces later with real D21–D27, they can be
added and this table amended.

**Alternative considered: leave `PROJECT.md` alone and mark the specs as
provisional.** Rejected — it inverts the actual state of knowledge. The specs
are the more thought-through documents, and marking them provisional would
mean a reader implements the older, worse plan.

## Consequences

- `docs/PROJECT.md` §4 no longer says "six-phase roadmap" anywhere, and
  `docs/README.md`, `CLAUDE.md` and `docs/RUNBOOK.md` are checked for the same
  phrase.
- **`PROJECT.md:13`** ("Uncertainty, when it arrives in Phase 5") and
  **`PROJECT.md:30`** ("cumulative B-splines (Phase 5)") now read Phase 6.
  **`PROJECT.md:99`** ("until Phase 6 supplies alignment") now reads Phase 8.
  **D19**'s "(Phase 6)" now reads "(Phase 8)".
- A Phase 6 spec does not exist yet and is now owed two things it did not
  previously owe: the reserved arena regions from `PHASE5.md` §1.2, and the
  Phase 4 surprise log.
- D22 constrains every future cargo feature that touches the arena. A feature
  that wants to *save space* by omitting a region cannot; it must justify a
  `FORMAT_VERSION` bump instead.
- The decision log is no longer contiguous with the source document the specs
  came from. The alias table is load-bearing and must be kept accurate.

## Implementation plan

1. This record — verified by `docs/decisions/README.md` listing it as `ready`.
2. `docs/PROJECT.md` §4 rewritten to the eight-phase roadmap; §5 gains D21 and
   D22; the three dangling phase references corrected — verified by
   `grep -rn 'six-phase\|Phase 5.*[Bb]-spline\|Phase 6.*replication' docs/ CLAUDE.md`
   returning nothing that contradicts this record.
3. `docs/README.md` and `CLAUDE.md` updated to list `PHASE4.md` and `PHASE5.md`
   in the canonical-document table — verified by both files naming every
   `docs/PHASE*.md` that exists.

## Open questions

None. The one thing this record deliberately does *not* resolve is the content
of the source document's D21–D27, which is unknown here; the alias table makes
their absence visible rather than papering over it.
