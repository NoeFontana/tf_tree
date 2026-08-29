# 0043: `owner_lost` is a question about the owner

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`Tree::owner_lost` polls one thing:

```rust
Some(Attachment::Joined { socket, .. }) =>
    tf_tree_ipc::peer_hung_up(socket.as_fd()).unwrap_or(false),
```

That is a question about **this process's socket**, and it is documented and used
as a question about **the arena's owner**. The two agree exactly once: the first
time an owner dies. They disagree forever after.

`docs/PHASE2.md` §3.5 pairs it with `inherit_ownership` in the loop an integrator
is told to write:

```rust
if tree.owner_lost() {
    let _ = tree.inherit_ownership()?;
}
```

**Run that loop on a robot with four read-write consumers and one of them wins.**
The owner dies, all four sockets hang up, all four call `inherit_ownership`, one
takes byte 0 and becomes `Inheritance::Inherited`. The other three get
`Inheritance::Contended` — correct, documented, and *the end of the useful part*.
Their sockets still point at the corpse and stay hung up for the life of the
process, so `owner_lost()` goes on answering `true`, and the recommended loop
re-attempts an `F_OFD_SETLK` on byte 0 **every control cycle, forever**. At 1 kHz
that is a `fcntl` per millisecond per survivor, in the loop this library exists to
keep quiet, to be told each time that somebody else is the owner.

`Inheritance::Contended`'s own doc comment states the defect and hands it
forward: *"A loser should stop calling, and the reason it cannot simply latch is
that if the new owner also dies it ought to be able to inherit then. Reattaching a
survivor to a new owner is a protocol addition, so it is a decision record rather
than a patch."* `docs/PHASE2.md` §0.0's §3.5 row carries it as owed. This is that
record, and **the protocol addition turns out not to be needed** — see below.

## Decision

**`owner_lost` returns `true` when there is no owner, and a hung-up socket is
evidence rather than the answer.**

```rust
pub fn owner_lost(&self) -> bool {
    match &self.attachment {
        Some(Attachment::Joined { socket, session, .. }) => {
            // Cheap and false in the healthy case: one zero-timeout poll.
            if !tf_tree_ipc::peer_hung_up(socket.as_fd()).unwrap_or(false) {
                return false;
            }
            // Hung up. That says our channel is dead; it does not say the role
            // is vacant. The kernel knows which.
            !session.ownership_held().unwrap_or(false)
        }
        _ => false,
    }
}
```

The second question is `F_OFD_GETLK` on byte 0 of the lock file the session
already holds open, exposed as `Session::ownership_held`. `LockProbe::held`
reports *conflicting* locks only — a lock held by the querying description is
invisible to it — so a survivor probing its own session sees the winner's byte
and never its own.

### The three states this separates, which the socket alone cannot

| State | socket | byte 0 | `owner_lost` |
|---|---|---|---|
| owner alive and serving | up | held | `false` — one poll, no second syscall |
| owner dead, nobody has taken over | hung up | free | **`true`** — inherit |
| owner dead, a survivor took over or is mid-bind | hung up | held | **`false`** — somebody else has it |

Row three is the one that did not exist. It is also, on a fleet of *N*
read-write survivors, the state *N−1* of them are in.

### And the loser stays eligible

If the new owner dies too, the kernel releases byte 0 with no cooperation, and the
loser's next `owner_lost()` — same two syscalls, same order — answers `true`
again. Inheritance chains without anybody latching a flag, retrying a connect, or
holding a timer. **That is the whole property §3.5's "retry connect with backoff"
was reaching for**, and it is reached without a reconnect.

## Rationale

**Why not the reconnect §3.5's pseudo-code names.** `contended -> KEEP OUR SLOT,
retry connect with backoff` is what the spec says, and implementing it literally
means a *new wire message*: the handshake in §3.7 assigns a participant slot, and
§3.5 requirement 2 forbids an heir — and equally a re-joiner — from registering a
second time, because A3 encodes claim ownership as `participant_slot + 1` and a
second registration arranges for the survivor's own live claims to be reaped. So
the reconnect needs a "I am already slot *n*, give me a liveness channel and
nothing else" request, an owner-side arm that verifies the claim, `HelloRequest`
growing a field, and a compatibility story for an owner that does not understand
it. That is four moving parts and a wire change to answer a question the kernel
already answers in one `fcntl`.

**What the reconnect would buy that this does not, stated rather than elided.**
D17 makes liveness the socket, and the new owner never learns the loser exists —
so if the loser dies, the owner's **hangup callback** will not reclaim its
participant slot. That reclamation is not lost, because it was never the only
route: the loser holds its participant **byte**, the kernel releases it on death
with no cooperation, and both remaining collectors key off the byte rather than a
socket — the slot assigner at the next grant, and any survivor's
`Tree::reap_participants` sweep, which reads its verdict from `ofd_probe` and not
from a connection. So a dead loser's slot is reclaimed a grant or a sweep later
instead of immediately. **That is a latency degradation in reclamation, not a
correctness gap, and it is the state a loser is in today** — this record does not
introduce it, and a reconnect is what would remove it. If slot reclamation
latency ever becomes the binding constraint, the reconnect is the fix and this
record is not in its way.

**Why the byte and not the identity records.** `ParticipantTable` carries pids and
start times, and a survivor could scan them for a live owner. It would be reading
a *cache* of a kernel fact through a `/proc` heuristic that
[`0033`](./0033-the-identity-record-cannot-name-a-namespace.md) has already shown
cannot name a namespace, to answer a question `F_OFD_GETLK` answers exactly. The
byte is the authority everywhere else in this protocol (§5.1, §6.1,
[`0029`](./0029-the-topology-lock-is-a-kernel-lock.md)); it is the authority here.

**Why the poll stays first.** It is the healthy case, it is one `poll` with a zero
timeout on a fd this process already holds, and it is `false` for the entire life
of a normal deployment. Probing the byte first would put an `fcntl` on every call
in the case that never needs one. Ordering them poll-then-probe keeps the healthy
path exactly as cheap as it is today and pays the second syscall only after the
owner has actually died.

**Why not make `inherit_ownership` do this internally instead.** Because then the
survivor still calls it every cycle and still pays, and `owner_lost()` still
returns a value that is not true. The defect is that a predicate lies; moving who
consults it does not stop it lying.

## Consequences

- `owner_lost()` means what its name says on every path, so §3.5's recommended
  loop terminates its own retrying without an integrator writing a latch, a
  backoff, or a flag. The `Contended` arm becomes an ordinary transient rather
  than an absorbing state.
- One new `pub fn` on `tf_tree_ipc::Session` (`ownership_held`), which is
  `probe_ownership()?.held` and is the same call `Open::open`'s error path
  already makes.
- `owner_lost()` gains a syscall **only** when the socket is already hung up.
  The healthy path — every call in a deployment whose owner is alive — is
  unchanged: one `poll`, timeout 0.
- **A false `false` is now reachable and is the safe direction.** If the byte is
  held by a survivor that then dies before binding, this answers `false` for one
  cycle and `true` on the next, because the kernel frees the byte. The opposite
  error — a false `true` — is the one that sends two processes at one role, and
  this record removes rather than adds paths to it.
- **A survivor that does not inherit now has *three* correct outcomes where it
  had one, and the third was found by `ubuntu-24.04-arm` after this record was
  written.** `inherit_ownership` re-evaluates `owner_lost` internally, so a
  caller that polls and then inherits takes **two** observations at two instants,
  and a takeover fits between them:

  | poll | inherit | when |
  |---|---|---|
  | `true` | `Contended` | byte free at both instants; attempted and lost the race |
  | `false` | `OwnerAlive` | byte already held at the poll; never attempted |
  | `true` | `OwnerAlive` | byte free at the poll, taken before the attempt |

  Before this record only the first was reachable, because the socket poll gave
  the same answer twice. All three are the survivor correctly describing what it
  found, and none of them costs an `F_OFD_SETLK` after the first cycle.
  `two_survivors_race_and_exactly_one_inherits` had been amended to accept the
  first two and to require the poll and the outcome to *agree* — which is not an
  invariant, since they are not one observation. The x86-64 host never produced
  the third; the aarch64 runner did on the first run.
- **`Inheritance::Contended` gets rarer, and this was found by a test failing
  rather than by writing it down here first.** A survivor that evaluates
  `owner_lost` *after* the winner has taken byte 0 now sees a held byte, answers
  `false`, and never attempts the lock — so it reports `OwnerAlive` where it used
  to report `Contended`. Both are the survivor correctly describing what it
  found, and the new one is strictly cheaper (no `F_OFD_SETLK` at all), but it is
  an observable change in what a *correct* deployment sees, and
  `two_survivors_race_and_exactly_one_inherits` asserted the old shape —
  including a comment, "both saw the hangup — the trigger is a kernel fact, not
  a race", that this record makes false. `Contended` remains reachable for a
  genuine tie, where both poll before either takes the byte, and that test is
  amended to accept either correct answer rather than to pin the timing.
- §3.5's `retry connect with backoff` is **not** implemented, and its
  pseudo-code is amended to say what is: the loser keeps its slot and re-probes,
  which is what the backoff was for. The wire is unchanged.
- `docs/PHASE2.md` §0.0's §3.5 row stops carrying loser-reattach as owed and
  carries the reclamation-latency note instead, which is the honest residue.

## Implementation plan

1. `Session::ownership_held(&self) -> Result<bool, IpcError>` in
   `tf_tree_ipc::open`, documented as "does *anyone else* hold byte 0". Verified
   by a unit test in that module: a fresh session reports `false`, a second
   `LockFile` taking byte 0 makes it report `true`, and releasing makes it
   `false` again — the third assertion being what shows the probe is live rather
   than latched.
2. `Tree::owner_lost` consults it on the hung-up path. Verified by a rendezvous
   test: two read-write survivors, owner killed, the first is poked **and read**
   so it has certainly inherited, and the second's `owner_lost()` is then
   `false` — which is `true` before this change and is the defect stated as a
   test. Serialised on purpose: a test that poked both and hoped would be
   asserting the scheduler.
3. The chain holds: kill the heir too, and the survivor's `owner_lost()` answers
   `true` again and its `inherit_ownership()` returns `Inherited`. Verified by
   extending step 2's test rather than adding a second one, because the property
   is about the *sequence*. Reaching a third poke needs the test harness's
   `Kid::poke` to stop `take`-ing the child's stdin — it closed the pipe as a
   side effect of nudging, which was invisible while every helper answered once
   and parked.
3b. `two_survivors_race_and_exactly_one_inherits` is amended for the
   `OwnerAlive`/`Contended` split in *Consequences*. Verified by that test, which
   fails against the shipped change until it is.
4. `Inheritance::Contended`'s doc comment stops describing a permanent state and
   describes a transient one; `docs/PHASE2.md` §3.5's pseudo-code and §0.0's row
   are amended. Verified by `just lint` and by reading the amended pseudo-code
   against the shipped code.
5. `docs/RUNBOOK.md`'s owner-death guidance drops whatever latch it advises an
   operator to write, if it advises one. Verified by grep.

## Open questions

None. Two were resolved while writing:

- *Poll first or probe first?* Poll — it is the healthy case and it is one
  syscall on a fd already held.
- *Does the loser need a live socket to the new owner at all?* Not for
  inheritance, which is the only thing `owner_lost` gates. It needs one for the
  owner's hangup callback to reclaim its slot promptly, and the two byte-keyed
  collectors cover that more slowly. Recorded as the cost above rather than
  closed.
