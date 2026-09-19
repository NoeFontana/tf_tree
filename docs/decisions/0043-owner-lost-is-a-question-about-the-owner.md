# 0043: `owner_lost` is a question about the owner

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #281

## Decision

**`owner_lost` returns `true` when there is no owner, and a hung-up socket is evidence rather than the answer.** Polling only the socket answered a question about this process's channel: after the first owner death every losing survivor stayed hung up forever, so `docs/PHASE2.md` §3.5's loop re-attempted `F_OFD_SETLK` on byte 0 every cycle.

```rust
pub fn owner_lost(&self) -> bool {
    match &self.attachment {
        Some(Attachment::Joined { socket, session, .. }) => {
            if !tf_tree_ipc::peer_hung_up(socket.as_fd()).unwrap_or(false) {
                return false;
            }
            !session.ownership_held().unwrap_or(false)
        }
        _ => false,
    }
}
```

`Session::ownership_held` is `F_OFD_GETLK` on byte 0 of the lock file the session already holds; it reports *conflicting* locks only ("does anyone else hold byte 0").

### The three states this separates, which the socket alone cannot

| State | socket | byte 0 | `owner_lost` |
|---|---|---|---|
| owner alive and serving | up | held | `false` — one poll, no second syscall |
| owner dead, nobody has taken over | hung up | free | **`true`** — inherit |
| owner dead, a survivor took over or is mid-bind | hung up | held | **`false`** — somebody else has it |

### And the loser stays eligible

If the new owner dies too, the kernel releases byte 0 and the loser's next `owner_lost()` answers `true` again. Inheritance chains without a latch, a reconnect or a timer.

## Rationale

- **No reconnect.** §3.5's "retry connect with backoff" would need a new wire message (an "I am already slot *n*" request, since a second registration reaps the survivor's own claims under A3), a verifying owner-side arm and a `HelloRequest` field. A dead loser's slot is reclaimed a grant or a `Tree::reap_participants` sweep later (both key off the byte): a latency degradation, not a correctness gap.
- **The byte, not the identity records**: `ParticipantTable` pids are a `/proc` heuristic that `0033` shows cannot name a namespace; the byte is the authority (§5.1, §6.1, `0029`).
- **The poll stays first**: the healthy path stays one zero-timeout `poll`.
- Moving the check inside `inherit_ownership` would leave the predicate lying.

## Consequences

- `owner_lost()` gains a syscall only when the socket is already hung up. A false `false` (byte held by a survivor that dies before binding) lasts one cycle; a false `true` is the dangerous direction and is not added.
- A survivor has three correct outcomes, because `inherit_ownership` re-evaluates `owner_lost` and a takeover fits between the two observations:

  | poll | inherit | when |
  |---|---|---|
  | `true` | `Contended` | byte free at both instants; attempted and lost the race |
  | `false` | `OwnerAlive` | byte already held at the poll; never attempted |
  | `true` | `OwnerAlive` | byte free at the poll, taken before the attempt |

  `two_survivors_race_and_exactly_one_inherits` accepts these and must not pin timing.
- §3.5's pseudo-code and §0.0's row say the loser keeps its slot and re-probes; the wire is unchanged.

## Implementation plan

1. `Session::ownership_held(&self) -> Result<bool, IpcError>` in `tf_tree_ipc::open`, unit-tested fresh/held/released.
2. `Tree::owner_lost` consults it on the hung-up path; rendezvous test with two read-write survivors, serialised.
3. The chain holds: kill the heir and the survivor inherits again (extends step 2's test).
3b. `two_survivors_race_and_exactly_one_inherits` amended for the `OwnerAlive`/`Contended` split.
4. `Inheritance::Contended`'s doc describes a transient state; `docs/PHASE2.md` §3.5 and §0.0 amended.
5. `docs/RUNBOOK.md`'s owner-death guidance drops any latch it advises.

## Open questions

None.
