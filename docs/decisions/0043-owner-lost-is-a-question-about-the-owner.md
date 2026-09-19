# 0043: `owner_lost` is a question about the owner

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #281

## Decision

**`owner_lost` returns `true` when there is no owner, and a hung-up socket is evidence rather than the answer.** Polling only the socket answered a question about this process's channel: after the first owner death every losing survivor stayed hung up forever. When the socket is hung up, `Tree::owner_lost` consults `Session::ownership_held`, an `F_OFD_GETLK` on byte 0 of the lock file the session already holds (it reports only *conflicting* locks).

| State | socket | byte 0 | `owner_lost` |
|---|---|---|---|
| owner alive and serving | up | held | `false` (one poll, no second syscall) |
| owner dead, nobody has taken over | hung up | free | **`true`** — inherit |
| owner dead, a survivor took over or is mid-bind | hung up | held | **`false`** |

The loser stays eligible: if the new owner dies too, byte 0 is released and its next `owner_lost()` answers `true` again, with no latch, reconnect (§3.5's "retry connect with backoff" would need a new wire message) or timer. The byte, not the identity records, is the authority: `ParticipantTable` pids are a `/proc` heuristic (`0033`; §5.1, §6.1, `0029`).

## Consequences

A false `true` is the dangerous direction and is not added. Because `inherit_ownership` re-evaluates `owner_lost`, a survivor has three correct outcomes: (`true`, `Contended`) lost the race; (`false`, `OwnerAlive`) byte held at the poll; (`true`, `OwnerAlive`) byte taken between poll and attempt. `two_survivors_race_and_exactly_one_inherits` accepts all three and must not pin timing.

## Implementation plan

1. `Session::ownership_held` in `tf_tree_ipc::open`; `Tree::owner_lost` consults it on the hung-up path; rendezvous tests with two read-write survivors and a killed heir.
2. `docs/PHASE2.md` §3.5 and §0.0, and `docs/RUNBOOK.md`'s owner-death guidance, amended.

## Open questions

None.
