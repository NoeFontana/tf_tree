# 0035: the creator's slot is taken, not found

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** this PR, in one change — see *Why the lifecycle is compressed*.

## Decision

**A creator takes participant slot 0, and the acquire *is* the check.**

Issue #201: `docs/PHASE2.md` §3.4 step 4's scan (refuse while any participant byte is held) and step 5's acquire were two passes over the same bytes, so a creator could hold lock byte *i* and arena record *j* with `i != j`, and every liveness predicate indexes both with one integer. `LockFile::try_take_participant` is public API on a published crate, so "no in-tree caller races a creator" is not an invariant a library can offer.

Step 5 calls `try_take_participant(0)` (one `F_OFD_SETLK`, atomic by the kernel) instead of `take_any_participant()`'s scan. `Contended` is step 4's condition arriving late and takes step 4's branch: release the ownership byte, back off, loop. `register_any` stays for the **takeover** arm.

## Rationale

- Rejected: making `ParticipantSlotDiverged` retryable (repairs half the API since only `await_open` consults `is_retryable`; every lost race builds and discards a whole arena; the invariant stays asserted where it is consumed).
- Rejected: take any byte, then release if not 0 (detect-and-undo).
- Layering: step 4 makes a creator by definition the first participant, and the first participant takes the first slot; a self-contained property of the rendezvous.
- `--force-new` (`CreatePolicy::Always`) skips step 4, so a contended byte 0 there means a *live* participant holds it and the create yields (`ArenaHeldButUnreachable` with `first_slot: Some(0)`). The `SIGSTOP`ped-participant escape hatch §3.4 describes was never delivered by any revision (`defect_201_a_forced_creators_record_reads_dead_while_it_is_publishing`); closing that is `#189`'s question, not slot assignment.

## Consequences

- `ParticipantSlotDiverged` is unreachable from the create path; the `0028` step 0c guard stays as an assertion covering the takeover arm and hand-rolled `tf_tree_ipc::Open` + `TreeBuilder::build_shared` construction.
- `is_retryable` is deliberately unchanged.
- §3.4's normative algorithm changes and `docs/PHASE2.md` moves with the code; the uncontended create does one `F_OFD_SETLK` instead of a scan.

### D15 — the §11.3 crash-matrix walk

The kernel releases an OFD lock when its holder dies, which every row already rests on. `open.after_ownership_lock_before_bind`, `open.after_create_before_bind`, `takeover.after_ownership_lock_before_bind` and `attach.after_slot_assigned_before_publish` are unchanged. The window between `try_take_participant(0)` and `write_identity(0)` is `register_any`'s existing "slot held with a stale record" window on a fixed slot; a process dying in it holds nothing.

## Verification

- `a_creator_takes_slot_zero_or_does_not_create`: 400 opens against a racer on byte 0, every create returns slot 0, with a positive control that at least one create succeeded. Putting `register_any` back in step 5 fails it on iteration 1.

## Why the lifecycle is compressed

Written and implemented in one change at the owner's explicit instruction; the measurements that would have gated each step are in this record's test.

## Not in this record

**The takeover arm.** `Open::already_attached(true)` reached `register_any` and could still diverge; its correct slot is `0028` question 3, and the `0028` guard is what stood between that arm and a diverged tree. The arm was later deleted (`0037`).
