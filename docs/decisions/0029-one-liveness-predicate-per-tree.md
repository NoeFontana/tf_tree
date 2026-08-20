# 0029: one liveness predicate per tree

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none — this record exists so that the adapter does not land
as a one-liner.

## Context

Filed as issue #213. `Tree::reparent` is the only path that takes A2's in-arena
topology lock, and it decides whether the current holder is alive from the
`(pid, start_time, boot_id)` triple — **even on a rendezvous tree that is holding
an `F_OFD_GETLK` probe**. `docs/PHASE2.md` §5.1 is NORMATIVE and says liveness is
the lock byte; §0.0 records this as the third of three places where the triple is
still on a correctness path (#205, out of #194).

The mechanical change is small. `crates/tf_tree/src/tree.rs:1735` builds the
predicate from the free function `participant_is_alive` (`:3007`) and hands it to
`TopoLockView::acquire` (`:1736`); `self.liveness`, where the probe lives, is
consumed at exactly two places and this is not one of them.

**It is filed as a record rather than a PR because the change is not the
adapter.** A liveness verdict here decides when a topology lock becomes
*stealable*, which is a mutation protocol, so D15 applies and it owes a §11.3
walk. And two things this repository learned in the last three days bear on it
directly, neither of which is written down anywhere a person changing that line
would look.

## The facade has five answers to "is slot *s* running?", and they disagree

Enumerated from the code rather than from the spec, because §0.0's row names
three and there are five.

| # | site | the fact it consults | which trees |
|---|---|---|---|
| P1 | `liveness_for` (`tree.rs:2999`) | `record_is_alive` (`:2952`) — the `/proc` triple, entire. Plus a boot-id arm that returns **`false` for every slot** when the arena's boot id differs from the host's | every tree with no probe: heap, `TreeBuilder::build_shared` called directly, `Tree::attach_shared` |
| P2 | `use_ofd_liveness` (`tree.rs:2413`) | the OFD byte, with an explicit "never report ourselves dead" guard, falling back to `record_is_alive` when `F_OFD_GETLK` declines to answer | both arms of `Open::attempt`, and nothing else |
| P3 | `Tree::participant_alive` (`tree.rs:2564`) | the state word `Acquire`, **then** `self.liveness` — the order is right, and it is right by `&&`'s short-circuit rather than by statement | whichever of P1/P2 the tree carries |
| P4 | `Tree::reparent` (`tree.rs:1735`) | `participant_is_alive`, i.e. the triple, **never** `self.liveness` | every tree, including those that paid for a probe. **This is #213** |
| P5 | the owner server's socket-hangup callback (`crates/tf_tree/src/open.rs:758-768`) | neither the byte nor the triple — only the socket (D17) | the owner's serving thread |

**P5 is the one that matters most and is missing from every existing
enumeration**, including §0.0's row and this record's first draft of this table.
It is the *only* facade path that **mutates** the participant table on a liveness
verdict: `table.identity(slot)` at `:763`, then `table.release(slot, incarnation)`
at `:764`, `LIVE -> FREE`. Everything else merely reports. Its incarnation guard
means a wrong verdict is a spurious free rather than a second occupant, which is
why it is survivable — but any statement of the form "the facade decides liveness
from X" has to account for a path that decides it from neither X nor Y.

Downstream, two shipped consumers already override P1/P2 rather than trusting
them, which is worth knowing before adding a sixth:

- `tf_tree top` (`crates/tf_tree_cli/src/top.rs:466`) assigns `existing.alive =
  *held` from the lock-file row, commented *"The kernel's answer wins over the
  arena record's"*. So `top` already reports a byte-less live participant dead.
- `tf_tree doctor` (`crates/tf_tree_cli/src/doctor.rs:497`) records
  `alive: alive || before != after`, bracketing the probe with two `Acquire`
  reads of the state word, so any occupancy change forces *alive*. Strictly more
  fail-safe than P3.

## The ordering constraint, which is new and which the adapter can break

Under `loom`, a predicate that composes an arena word with a lock-byte probe
**must observe the word first**. Reverse the two reads and a published record is
erased in 0.00 s: the reclaimer probes byte *s* free before a registrant takes
it, then observes `live_word(1)`, and CASes a live byte-holder's record to `FREE`.

The mechanism, not just the observation: under word-then-byte the `Acquire` load
of a `live_word` **synchronises-with** the publishing `Release` store, so a byte
probe sequenced after it must see the byte held. Two independently built models
reached this separately; it is recorded in `0028` open question 6.

P3 has the safe order. **An adapter for P4 need not**, and the natural way to
write one does not: `TopoLockView::acquire` wants `Fn(u32) -> bool` while
`BoxedLiveness` is `(u32, &ParticipantRecord)`, so the adapter has to fetch the
record — and `ParticipantTable::get` (`participant.rs:192`) performs no atomic
load at all, so *when* the word is read becomes the adapter author's choice
rather than the type's. Worse at the sweep scale: `LockFile::held_participants()`
returns all 64 bytes in one call, so anyone writing this from the mask takes
every probe before reading any word — the failing order, by construction.

## Decision — proposed, not taken

**`Tree::reparent` uses `self.liveness`, and `participant_is_alive` is deleted
rather than left as a second spelling.** Three pieces:

1. **An adapter that reads the word first.** It takes the slot, loads
   `rec.state` with `Acquire`, returns `false` if the word is not `LIVE`, and
   only then calls `self.liveness`. That is P3's body; the adapter should *be*
   P3 rather than resemble it, so there is one place where the order is stated.
2. **`participant_is_alive` is removed.** `grep` gives it three occurrences —
   the call at `tree.rs:1735`, a doc-link at `:2939`, and the definition at
   `:3007` — so no public item changes and no signature moves. What changes is
   `reparent`'s semantics, which belongs in `CHANGELOG.md` as a behaviour entry,
   not as a breaking-API entry.
3. **A §11.3 walk, because a topology lock is a mutation protocol.** §11.3 has
   exactly eleven rows (`docs/PHASE2.md:865-876`) and the one this touches is
   `topo.holding_lock` — *"lock stuck → stealable after liveness check (A2)"*.
   Its holder classes have to be re-walked with P5 in them: a holder whose slot
   the owner already released on `HUP` reads dead under P4 today via
   `identity() -> None`, and would read dead under the adapter too, by a
   different route. That is the same verdict for a different reason, which is
   exactly the kind of coincidence that stops being true later.

**This narrows the exposure; it does not remove it.** Trees with no probe — heap,
a directly-called `build_shared`, `attach_shared` — still have nothing but the
triple, so P1 remains the whole predicate for them and #205's row stays open.

## Rationale

**Why not leave it.** §5.1 is NORMATIVE and the current code contradicts it on
the one path where a false "dead" lets a second process mutate topology
concurrently with the first. The predicate has been biased against proving death
since #204, so this is not a live wrong answer on an ordinary host — but the
bias is what makes it survivable, not what makes it right, and `/proc` has two
documented ways to say "dead" about a running process (§0.0's row from #205:
`hidepid=2` without §3.10's same-user rule, and a PID-namespace mismatch, which
is not even `ENOENT`-shaped).

**Why not a one-line adapter in a PR.** Because of the ordering constraint above,
which is three days old, is recorded only inside a draft record's open question,
and is invisible at the call site. A reviewer looking at `is_alive = move |slot|
...` has nothing to check it against. Writing it down is most of the value of
this record.

**Why delete `participant_is_alive` rather than keep both.** `CLAUDE.md` forbids
a second spelling of an existing path. Two functions that answer the same
question from different facts is precisely that, and the drift is already
measurable: `tree.rs:2908-2936`'s seam documentation renders onto
`record_is_alive` while `participant_is_alive` at `:3007` carries no doc at all.

### Alternatives considered

- **Pass `self.liveness` directly, no adapter.** Fails on shape: the closure
  types differ, so something has to fetch the record, and that is where the
  ordering hazard lives. An adapter that exists but is unremarked is worse than
  one the record names.
- **Make `TopoLockView::acquire` take the record-shaped predicate.** Moves the
  problem into `tf_tree_core`, which has no `LivenessProbe` and must not gain
  one — `tf_tree_core` is `libm` + `bytemuck` + `blake3` (D14) and the probe is
  a syscall. Rejected on the dependency budget.
- **Do nothing and document it.** A real option in this project — `PHASE5.md` §8
  is a whole section about not building something. It is rejected here only
  because §0.0 already documents it and the documentation has not stopped the
  code from being wrong; the row has been true and unfixed since #205.

## Consequences

- One predicate per tree, stated in one place, with the ordering in it.
- `reparent` on a rendezvous tree stops stealing a topology lock from a live
  mutator that `/proc` cannot see (`hidepid`, PID namespace).
- `reparent` on a **probe-less** tree is unchanged, so the class §0.0's #205 row
  describes shrinks from three paths to two rather than closing.
- One more caller of `self.liveness`, which is `Box<dyn Fn>` — an indirect call
  on a path that already takes a lock and does I/O-free arena work. `reparent` is
  not a hot path (D3 keeps it off the query path), so this is not a budget
  question.

## Implementation plan

1. **The adapter, as P3's body**, with the ordering stated in a comment naming
   the `loom` result. *Verified by:* a unit test that the adapter returns `false`
   for a non-`LIVE` word without consulting the liveness closure at all —
   i.e. that the word is read first — using a counting fake.
2. **Delete `participant_is_alive`**, repoint the doc-link at `:2939`.
   *Verified by:* `rg participant_is_alive crates/` returning nothing.
3. **The §11.3 walk for `topo.holding_lock`**, with P5's holder class added.
   *Verified by:* the row's text naming which holder classes are stealable and
   why, reviewed against the five predicates above.
4. **A multiprocess test that the two predicates differ on a real tree.** Today
   no test in the tree distinguishes them on this path, which is part of the
   finding. *Verified by:* a `SIGSTOP`ped topology-lock holder — byte held,
   process not scheduled — which the byte calls alive and which `/proc` also
   calls alive, so the *discriminating* case is a holder whose `/proc` entry is
   unreadable; if that cannot be staged without `hidepid`, say so rather than
   claiming coverage.

## Open questions

1. **Can step 4's discriminating test be built on an ordinary host?** The two
   predicates agree except where `/proc` lies, and the two known ways to make it
   lie are `hidepid=2` and a PID-namespace mismatch. `unshare --fork --pid` was
   unavailable in the environment §0.0's #205 row was written in. If neither can
   be staged, this record ships a change with no test that distinguishes the old
   behaviour from the new, and that has to be stated rather than glossed.
2. **Does the adapter belong to this record or to `0028` piece 2?** `0028`'s
   piece 2 is "one reclamation predicate, named once", and this is "one liveness
   predicate, named once", one layer up. They are not the same object — piece 2
   decides whether to *reclaim*, this decides whether to *steal* — but if both
   land, two records will have specified the ordering constraint. **This gates
   `ready`**, because the wrong answer is the second spelling the decision is
   about avoiding.
3. **Is P5 in scope?** The socket-hangup callback decides liveness from neither
   fact. It is correct under D17 and its incarnation guard bounds the damage, so
   nothing here proposes changing it — but a record titled "one liveness
   predicate per tree" that leaves a fifth in place should say why, and "D17
   makes the socket authoritative for its own class" may or may not be the whole
   answer.

## What would make this `ready`

- Question 2 answered — it decides whether this record survives at all or is
  folded into `0028`'s plan as a step.
- Question 1 attempted, with the command, and its answer written down either way.
- The §11.3 `topo.holding_lock` walk drafted and agreed, since D15 makes it the
  gate rather than a deliverable.

## Not in this record: #220

**#220 is not record material and should not become a second draft.** The
`Created | TookOver` arm (`crates/tf_tree/src/open.rs:546`) calls
`builder.build_shared(...)`, so a taker-over would build a *new* arena rather
than inherit the one it already has. Checked at that call site, there is no fd,
no socket path, no `Rendezvous` and no way to take the session's arena in scope —
**so the arm cannot be fixed in place under any answer to `0028` question 3.**
Fixing it means restructuring where the heir's arena comes from, which is exactly
what question 3 is about, and `OpenOutcome::TookOver` is unconstructible from
`tf_tree::Open` today because the builder has no `already_attached` setter.

Its home is `0028`'s implementation plan, as a step that lands with §3.5's
trigger. Filing a second record would put the same question-3 dependency in two
places.
