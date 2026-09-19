# 0035: the creator's slot is taken, not found

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** this PR, in one change — see *Why the lifecycle is compressed*.

## Decision

**A creator takes participant slot 0, and the acquire *is* the check.**

`docs/PHASE2.md` §3.4 step 5 calls `try_take_participant(0)` (one `F_OFD_SETLK`) instead of `take_any_participant()`'s scan, which left a creator able to hold lock byte *i* and arena record *j* with `i != j` (#201). `Contended` is step 4's condition arriving late: release the ownership byte, back off, loop. `register_any` stays for the takeover arm. Under `--force-new` (`CreatePolicy::Always`) a contended byte 0 means a live participant holds it (`ArenaHeldButUnreachable` with `first_slot: Some(0)`).

## Consequences

- `ParticipantSlotDiverged` is unreachable from the create path; the `0028` step 0c guard stays as an assertion. `is_retryable` is unchanged.

### D15 — the §11.3 crash-matrix walk

The window between `try_take_participant(0)` and `write_identity(0)` is `register_any`'s existing "slot held with a stale record" window on a fixed slot.

## Verification

`a_creator_takes_slot_zero_or_does_not_create`: 400 opens against a racer on byte 0, every create returns slot 0.

## Why the lifecycle is compressed

Written and implemented in one change at the owner's explicit instruction.
