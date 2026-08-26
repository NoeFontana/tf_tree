# 0035: the creator's slot is taken, not found

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** this PR, in one change — see *Why the lifecycle is compressed*.

## Context

Filed as issue #201. A creator ends up holding lock byte *i* and arena record *j*
with `i != j`, and every liveness predicate in the codebase assumes they match:
`LivenessProbe::is_held(slot)` probes the byte while `ParticipantTable` indexes
the record, with one integer.

The correspondence was *established* rather than lucky — §3.4 step 4 refuses to
create while any participant byte is held, so a creator runs against an empty
lock file and takes byte 0, while `build_shared` hands it an arena whose first
`FREE` record is 0 — but **step 4's scan and step 5's acquire are two separate
passes over the same bytes**, and nothing holds the file still between them.

The gap is not one instruction. `any_participant_held` probes byte 0 **first**
and then up to 63 more before returning, so byte 0 can change hands for the rest
of that scan. Measured with a second open file description toggling byte 0, 4000
iterations of exactly the two calls steps 4 and 5 make:

```
                  byte 0    diverged   yielded
scan + take-any     1395        2242       363
```

Control, racer off: 4000 / 0 / 0.

**Reachability, stated precisely because the issue overstated it.** No in-tree
production path takes a chosen participant byte concurrently with a creator: the
only non-test caller of `try_take_participant(slot)` is the joiner taking an
owner-assigned slot, and a live owner holds byte 0 itself. What makes this worth
fixing anyway is that **`LockFile::try_take_participant` is public API on a
published crate.** A downstream consumer can occupy that window whether or not
anything here does, and "no caller in *our* tree does this" is not an invariant a
library can offer.

Since `0028` step 0c the facade *detects* the divergence and refuses with
`OpenError::ParticipantSlotDiverged`. So there is no silent corruption today —
there is a hard error on a transient condition, and `is_retryable` does not list
it. End to end through `tf_tree::Open` with the same racer: 190 `Ok`, 210
`ParticipantSlotDiverged`; racer off, 400 `Ok`.

## Decision

**A creator takes participant slot 0, and the acquire *is* the check.**

Step 5 calls `try_take_participant(0)` — one `F_OFD_SETLK`, atomic by the kernel
— instead of `take_any_participant()`'s scan. `Contended` is not a new failure
mode: it is step 4's condition arriving late, so it takes step 4's branch —
release the ownership byte, back off, loop.

`register_any` stays exactly as it is for the **takeover** arm, whose correct
slot is a different question (see *Not in this record*).

## Rationale

Three candidates. The chosen one is the smallest and the only one that removes
the window rather than coping with it.

**Rejected: make `ParticipantSlotDiverged` retryable.** The one-line fix, and it
is not a fix.

* It repairs half the API. `is_retryable` is consulted only by `await_open`;
  `.open()` — which `tf_tree::open()` and most callers use — still returns the
  terminal error.
* Every lost race would build and discard a whole arena. The guard fires at the
  facade *after* memfd create and seal, `build_shared` over the full layout, and
  the liveness and claim-lease installs. Retrying turns a visible failure into an
  invisible cost, which is worse, because nothing will ever measure it again.
* It leaves the invariant asserted where it is **consumed** rather than where it
  is created — the one thing #201 asks to change.

**Rejected: take any byte, then release it if it is not 0.** Detect-and-undo.
Strictly better than today and still a compensating action for a state that
should not be constructible.

**Chosen: make the state unrepresentable.** Check and take are one operation, so
there is no window to close because there is no gap. It is also *cheaper* than
what it replaces — one `SETLK` against a scan — so the uncontended path, which is
every real path, gets faster.

**On layering.** The objection is that "a creator's byte must be 0" is a `tf_tree`
arena invariant imposed inside `tf_tree_ipc`, which publishes standalone and has
no `tf_tree` dependency. It lands against the framing, not the change. §3.4 step
4 already refuses to create while any participant byte is held, so **a creator is
by definition the first participant, and the first participant takes the first
slot.** That is a self-contained property of the rendezvous. The arena side then
relies on it, rather than the ipc side importing anything.

**On `--force-new`, and a correction to this record's first draft.**
`CreatePolicy::Always` skips step 4, so a contended byte 0 there means a *live*
participant holds it, and the create yields rather than diverging. One behaviour
for every policy, and no new error variant on a published crate.

This record first justified that with "the wedged arena `--force-new` is written
for has **dead** participants, and the kernel released their bytes when they
died, so byte 0 is free in exactly the case the flag is for". **That is wrong for
the case §3.4 actually names.** §3.4 offers `--force-new` as the escape hatch for
a participant that is `SIGSTOP`ped — alive, holding its byte, never taking over —
and calls it "an explicit, loud escape hatch that abandons the existing arena".
Against a live holder of byte 0, it does not abandon anything.

**It did not before this change either**, and that is the whole of the defence.
`defect_201_a_forced_creators_record_reads_dead_while_it_is_publishing` records
three revisions of the same line: `Always` *did* create over a stranded
participant (byte 1 against record 0 — #201); `0028` step 0c made that
`ParticipantSlotDiverged`; this record makes it `ArenaHeldButUnreachable` with
`first_slot: Some(0)`. None of the three delivers the documented escape hatch,
and only the third tells the operator which slot to look at.

So: the gap is pre-existing, this change makes the failure legible rather than
cryptic, and closing it is a separate question — it means deciding what
`--force-new` may do to a lock file whose bytes a live process holds, which is
`#189`'s territory and not a slot-assignment question. Filed rather than folded
in.

## Consequences

* **`ParticipantSlotDiverged` becomes unreachable from the create path.** The
  `0028` step 0c guard stays exactly where it is, now as an assertion rather than
  a filter — it still covers the takeover arm and hand-rolled
  `tf_tree_ipc::Open` + `TreeBuilder::build_shared` construction.
* **`is_retryable` is deliberately not changed.** Its remaining producers are the
  takeover arm and hand-rolled construction, and whether a retry there is *safe*
  depends on the takeover arm's correct slot, which is undecided. Adding
  retryability to a path nobody has analysed is a guess; not doing so leaves
  today's behaviour on those paths exactly as it is.
* **§3.4's normative algorithm changes**, so `docs/PHASE2.md` moves with the code.
* The uncontended create path does one `F_OFD_SETLK` instead of a 1–64 probe scan.

### D15 — the §11.3 crash-matrix walk

Four rows are in scope. None of their arguments changes, and the reason is the
same in each: **the kernel releases an OFD lock when its holder dies**, which is
the property those rows already rest on.

* `open.after_ownership_lock_before_bind` — the acquire moves inside this window
  but does not extend it. A creator killed here held byte 0; the kernel frees it;
  the next `open()` sees nothing held and proceeds. Unchanged.
* `open.after_create_before_bind` — its bound is "no participant byte held", and
  that still holds after death for the same reason. Unchanged.
* `takeover.after_ownership_lock_before_bind` — the takeover arm still uses
  `register_any`. Untouched.
* `attach.after_slot_assigned_before_publish` — the arena side. Untouched, and
  this change makes the byte/record correspondence it discusses hold by
  construction on the create path rather than by assumption.

One crash point is genuinely new in position and not in kind: between
`try_take_participant(0)` succeeding and `write_identity(0)` completing, byte 0
is held with no identity record. `register_any`'s own doc already covers that
window — "lock, then write … the window where a slot is held with a stale
record" — and it is the same window on a fixed slot rather than a found one. A
process that dies inside it holds nothing, because the kernel released the byte.

## Verification

* `a_creator_takes_slot_zero_or_does_not_create` — 400 opens against a racer on
  byte 0, asserting **every** create returns slot 0, with a positive control that
  at least one create succeeded so it cannot pass vacuously.
* **The mutant.** Putting `register_any` back in step 5 — the code before this
  change — fails that test on **iteration 1**: *"a creator took byte 1 while the
  arena would register record 0"*. Not statistically; immediately.
* `just test` (878 tests), `just shm-check`, `just lint`.

## Why the lifecycle is compressed

`decisions/README.md` runs `draft → ready → implemented`, and this record was
written and implemented in one change at the owner's explicit instruction. The
measurements that would have gated each step are all here: the window measured
both ways with a control, the three candidates costed, the layering objection
answered, the crash matrix walked, and a mutant proving the test discriminates.
Recorded so the compression is visible rather than assumed.

## Not in this record

**The takeover arm.** `Open::already_attached(true)` reaches `register_any` and
can still produce the divergence (executed: `TookOver`, lock byte 1, arena record
0). It is public, non-`cfg(test)` API on a published crate. Its correct slot is
[`0029`](./0029-the-topology-lock-is-a-kernel-lock.md) question 3 — whether §3.5's
heir reuses its slot — and `0029` is `draft` and itself blocked on
[`0031`](./0031-the-participant-record-with-no-byte.md). Closing it here would be
answering that question by accident.

The `0028` guard is what stands between that arm and a diverged tree, and it is
why this record does not remove it.
