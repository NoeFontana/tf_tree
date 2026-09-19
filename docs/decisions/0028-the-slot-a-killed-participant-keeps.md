# 0028: the slot a killed participant keeps

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** complete, in the plan's stated order — which is a safety
property and not a preference, because piece 2 is the first code that acts
destructively on a byte-derived verdict and steps 0b and 0c are what make that
verdict trustworthy.

## Decision

**Reclaim at the point of decision, from the participant's OFD lock byte; keep
the hangup callback as the O(1) fast path; move the assigner's authority off
`state`** (issue #184). No arena field; no `FORMAT_VERSION` bump.

**1. `ParticipantTable::reclaim(slot, observed: u32) -> bool`** — one
`compare_exchange(observed, FREE, AcqRel, Acquire)` against the observed word. It
accepts any word, `RESERVED` included, safe only given steps 0b and 0c.

**2. One predicate, the lock byte alone.** A non-`FREE` record is reclaimable iff
its byte reads free via `F_OFD_GETLK` (`LivenessProbe::is_held`); `None` is not
reclaimable (§6.2). The state word is observed **before** the byte is probed, this
process's own slot is skipped, and `record_is_alive` is not used.

**3. The assigner decides from the byte**, reclaiming before it grants.

**4. Reaping is not owner-only** (`PROJECT.md` §6.3): `Tree::reap_participants() ->
usize` sweeps all slots with the same predicate (refused read-only, R6); it is not
a second spelling of `reap_dead`, which reclaims claims.

## Implementation plan

0b. **`Tree::attach_shared` and `attach_shared_at` refuse `ReadWrite`**, so the
   predicate is total.
0c. **The byte and record index are the same integer**, asserted at the single
   `hold_ownership` call in `open.rs`. Verified by
   `defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`
   (`crates/tf_tree/tests/rendezvous.rs`).
1. **`ParticipantTable::reclaim`.** Verified by loom case `reclaim_races_register`
   (`crates/tf_tree_core/src/loom_tests.rs`); listed in `PHASE1.md` §10.2.
2. **The predicate, once**: `Reclaimable | Live | Unknown`.
3. **The assigner decides from the byte.** Verified in `rendezvous.rs` by
   `the_assigner_reclaims_a_stale_record_no_hangup_will_ever_clear`.
4. **The hangup fast path** rebased onto `reclaim`, keeping the incarnation guard.
5. **`Tree::reap_participants()`.**
6. **`TFT014`'s participant half** via `Tree::participant_alive`; no new id.
7. **The fork handler closes inherited descriptors** — moved to
   [`0030`](./0030-the-atfork-handler-and-inherited-descriptors.md).
8. **`--force-new`** — no flag; `RUNBOOK.md` names `CreatePolicy::Always`.
9. **The `TookOver` arm refuses** instead of building an arena.

## Open questions

All resolved.

3. **Takeover:** the heir keeps its slot, byte and record; takeover is byte 0 plus a
   `bind`; step 9.
6. **What collects `RESERVED`? The byte**, given steps 0b and 0c.
