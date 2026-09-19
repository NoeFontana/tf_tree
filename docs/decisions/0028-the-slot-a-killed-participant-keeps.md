# 0028: the slot a killed participant keeps

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** complete, in the plan's stated order — which is a safety
property and not a preference, because piece 2 is the first code that acts
destructively on a byte-derived verdict and steps 0b and 0c are what make that
verdict trustworthy.

| Step | Landed as |
|---|---|
| 0 — `PHASE2.md` §5, §5.1, §11.3's row | `356e746` (#218) |
| 0b — `attach_shared` / `attach_shared_at` refuse `ReadWrite` | `3636e21` (#227) |
| 0c — the byte/record correspondence asserted | `d25557f` (#228) |
| 1 — `ParticipantTable::reclaim` | `b352fdc` (#230) |
| 2 — the predicate, once | `f65343f` (#231) |
| 3 — the assigner decides from the byte | `78d6315` (#232) |
| 4 — the hangup fast path, rebased onto `reclaim` | `adeb158` (#191), rebased in `78d6315` (#232) |
| 5 — `Tree::reap_participants` | `001a1ad` (#233) |
| 6 — `TFT014`'s participant half | `4a91d6f` (#210), completed in `91f45dc` (#234) |
| 7 — the atfork handler | **not here** — [`0030`](./0030-the-atfork-handler-and-inherited-descriptors.md) |
| 8 — `--force-new` | #189, by taking neither branch as written |
| 9 — the `TookOver` arm refuses | `1b29208` (#229) |

## Context

Issue #184. `PHASE2.md` §5.1 is NORMATIVE: liveness comes from the OFD lock byte,
never from `state`. The assigner decided from `state` and `release` ran only from
`Drop`, so 64 `SIGKILL`ed read-write participants wedged the arena. The byte is
taken before the arena record on every path that has one.

## Decision

**Reclaim at the point of decision, from the participant's OFD lock byte; keep
the hangup callback as the O(1) fast path; and move the assigner's authority off
`state`.** No arena field is added and `FORMAT_VERSION` is not bumped.

**1. `ParticipantTable::reclaim(slot, observed: u32) -> bool`** in
`tf_tree_core` — a single `compare_exchange(observed, FREE, AcqRel, Acquire)`
against the *word the caller observed*, so any state change between observation
and CAS aborts it. It needs no incarnation because the observed word carries
it. `release` stays for the clean-detach path. It accepts **any** observed word,
`RESERVED` included (question 6) — safe only *given* steps 0b and 0c.

**2. One reclamation predicate — the lock byte, and nothing else.** A slot whose
record is not `FREE` is reclaimable iff its byte reads free via `F_OFD_GETLK`
(`LivenessProbe::is_held`); `None` means *not reclaimable* (§6.2). It applies to a
tree carrying a probe (from `Open::attempt`), where every participant holds a byte.
It is not built on `record_is_alive` (which tests `state`); it observes the state
word **before** probing the byte; it skips this process's own slot; and the byte
and record index must be the same integer, asserted at the single `hold_ownership`
call in `tf_tree`'s `open.rs` (step 0c).

**3. The assigner decides from the byte.** The `identity(slot).is_some()` skip is
replaced by the byte probe; on byte free + record not `FREE` it runs the
predicate and `reclaim`s before granting.

**4. Reaping is not owner-only.** A public `Tree::reap_participants() -> usize`
on a read-write tree sweeps all slots with the same predicate (`PROJECT.md` §6.3:
"Reaping must not be owner-only"). It is not a second spelling of `reap_dead`,
which reclaims *claims* (`CLAIM_BASE + edge`), not participant records.

The hangup callback is kept and calls `reclaim` with the observed word
(`0029` P5).

**Cost.** Nothing on the hot path (`API.md` R2). `FORMAT_VERSION`: no bump.

## Rationale

**Candidate B** (hangup-driven owner reap, #191) is kept as the fast path, not the
guarantee: it is owner-only (`PROJECT.md` §6.3) and misses fork-inherited
connections, `RESERVED` records, the owner's own slot, and `attach_shared`. A grace
period is forbidden by §3.4 and §6.4. A forked child keeps the byte held (§6.2), so
the predicate refuses; `TFT014` reports it and `0030` closes it. `RESERVED` carries
no incarnation, so step 0b makes the byte cover the `fill_slot` window.

## Crash matrix — §11.3

Every row is repaired by the byte-keyed sweep, the hangup, or the kernel
freeing the byte, except `fork.parent_killed_while_child_holds_description`
(byte held; not reclaimable by §6.2; `TFT014`; `0030`). `hangup.after_probe_before_cas`
is one CAS with no torn state. `reclaim.probe_then_reoccupied` fails the CAS on a
different word. `takeover.before_reaping_predecessor_clients` is swept by the
heir's first assign or any read-write survivor's `reap_participants()`.
`crash-points` does not exist (`PHASE2.md` §0.0): these are specifications.

- One implementation of the predicate; `ParticipantRecord::state` has a second
  writer, so every observable word must carry an incarnation.
- The owner's serving thread needs the lock file (`lock_probe`); `reap_participants()`
  is refused on a read-only tree (R6). A `SIGTERM` with no handler leaks a slot.

## Implementation plan

0. **Correct `PHASE2.md` §5, §5.1 and §11.3's row**: the joiner writes the arena
   record, after taking its byte; `--force-new` does not exist.
0b. **`Tree::attach_shared` and `attach_shared_at` refuse `ReadWrite`**, so the
   predicate is total. *Verified by:* unit tests for both entry points;
   `concurrent_reparents_from_separate_attachments_are_serialized`
   (`crates/tf_tree_bench/tests/multiprocess.rs`) and `load_child.rs`'s writer mode
   moved onto a path that takes a byte, still on separate slots.
0c. **The byte/record correspondence asserted** at the single `hold_ownership`
   call in `tf_tree`'s `open.rs`. #201 (`Session::release_ownership` keeps
   participant byte 0, so a forced create lands on byte 1 against record 0) is the
   falsifier. *Verified by:*
   `defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`
   (`crates/tf_tree/tests/rendezvous.rs`).
1. **`ParticipantTable::reclaim`**, any observed word. *Verified by:* loom case
   `reclaim_races_register` (`crates/tf_tree_core/src/loom_tests.rs`) with its
   reversed-probe-order failing control; unit tests for `live_word(inc)`,
   `RESERVED`, and a changed word. Listed in `PHASE1.md` §10.2.
2. **The predicate, once**, returning `Reclaimable | Live | Unknown`. *Verified by:*
   multiprocess tests in `crates/tf_tree/tests/`: `SIGSTOP`ped target (`Live`),
   `SIGKILL`ed (`Reclaimable`), the sweeper's own slot (`Live`).
3. **The assigner decides from the byte.** *Verified by*, in
   `crates/tf_tree/tests/rendezvous.rs`:
   `the_assigner_reclaims_a_stale_record_no_hangup_will_ever_clear`,
   `the_assigner_collects_a_record_left_reserved_by_a_killed_registrant`,
   `slot_recycling_under_abnormal_exit` (§11.2 scenario 2b).
4. **The hangup fast path** rebased onto `reclaim`, keeping the incarnation guard.
   *Verified by:* `the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live`,
   `the_hangup_collects_a_record_left_reserved_by_a_killed_registrant`.
5. **`Tree::reap_participants()`**, refused on a read-only tree. *Verified by:* kill
   the owner; a surviving read-write participant reaps its slot to `FREE`. An
   owner-kill mode must not assert on new joiners (§3.5 is unwired).
6. **`TFT014` gets its participant half** via `Tree::participant_alive`: a non-`FREE`
   record with byte free and pid gone, and the fork case with a distinct message.
   No new `TFT` id. *Verified by:* a `doctor --json` stale-`LIVE` fixture test and
   a healthy-arena negative test.
7. **The fork handler closes inherited descriptors** — moved to
   [`0030`](./0030-the-atfork-handler-and-inherited-descriptors.md).
8. **`--force-new`** — done (#189): no flag; `RUNBOOK.md` names
   `CreatePolicy::Always`.
9. **The `Created | TookOver` arm stops building an arena** (#220): the `TookOver`
   half refuses instead of `build_shared`-ing a fresh segment. *Verified by:* a unit
   test that it returns an error.

## Open questions

All resolved before `ready`.

1. **Is `attach_shared(ReadWrite)` supported? No** (owner, 2026-08-18); step 0b.
2. **Does `reclaim` clear the lock file's identity record? No.**
3. **Takeover:** the heir keeps its slot, byte and arena record; takeover is byte 0
   plus a `bind` (a second slot would reap its own claims); step 9.
4. **Sweep cost:** measured; no bound.
5. **The atfork handler** is `0030`.
6. **What collects `RESERVED`? The byte**: `reclaim` accepts any observed word,
   conditional on steps 0b and 0c.
