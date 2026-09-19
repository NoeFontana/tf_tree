# 0055: the recovery capacity a fleet cannot add later

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** **every step has landed** — 2 in #353 (`0411adb`), 1/3/4 in
#355 (`ea98e56`), 6 in #356 (`d121589`), 7 after it. Step 5 is **struck**: open
question 1 answered *no mechanism*, so there was nothing to build.

## Context
An arena can reach a state from which no process outside it can attach again: the ownership byte is free, at least one participant byte is held, and no *eligible* participant (attached, read-write and polling `owner_lost()`) calls `Tree::inherit_ownership`. Nothing serves the rendezvous, §3.4's split-brain check refuses a create, and `Tree::attach_shared` over a passed descriptor refuses `AttachMode::ReadWrite` ([`0028`](./0028-the-slot-a-killed-participant-keeps.md) step 0b), so no heir can arrive. `a_live_participant_prevents_a_second_arena` and `scenario_3_an_owner_dying_leaves_readers_working_and_joins_refused` pin the wedge.

## Decision
**Decided 2026-09-18: no new surface**; documentation and tests, not a protocol.
- **1. Document the consequence.** [`PHASE2.md`](../PHASE2.md) §3.5 and [`RUNBOOK.md`](../RUNBOOK.md) state that recovery capacity is whatever was attached and eligible when the ownership role fell vacant, and cannot be added afterwards. **Eligibility, not mode, is what is held**: a read-write attachment that never polls is not capacity.
- **2. Question 1: a NORMATIVE fleet requirement, and no mechanism.** A standby-heir helper is a thread or daemon, which [`PHASE2.md`](../PHASE2.md) §3.5 and [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) refuse. Reopen only on a field incident in which a fleet that had read the requirement wedged anyway.
- **3. Question 2: no adopting an ownerless arena.** Each route supersedes a standing decision (`PROJECT.md` §6 and §3.9; `0019`; [`0037`](./0037-a-takeover-is-not-a-second-open.md)). Making read-only survivors eligible is refused by D18.
- **4. Question 3: the message states facts plus one pointer; the remedy lives in RUNBOOK alone.** `ArenaHeldButUnreachable` cannot tell whether one process holds two of the bytes, so it prints the bytes held, the first slot, its pid and where to read what to do ([`API.md`](../API.md) R5).

## Implementation plan
Steps 1, 2 and 4 are done (4: `PHASE2.md` §0.0's row, *"settled by `0055`"*, `DECISION_SETTLED_VERB`); step 5 is struck.
3. **DONE.** `scenario_3`'s `join-sweep` child is the read-write survivor that never polls; `byte_0_and_ownership_held_by_two_different_holders_is_refused_without_naming_a_topology` pins the two-holder arm.
6. **DONE.** `ArenaHeldButUnreachable` states facts only, ASCII, 116-164 bytes; the remedy is `docs/RUNBOOK.md`'s eight-row table. `every_unreachable_state_reports_the_facts_and_prescribes_nothing` asserts it.
7. **DONE.** `HandshakeRejected` states the status and the owner's two numbers (`both_rejection_arms_name_only_the_status_they_carry`); `crates/tf_tree_cli/tests/runbook.rs` requires a RUNBOOK row per refusal status, and `status_is_a_refusal`, a total `match`, is the compile error that says a row is owed.

## Open questions
All answered in *Decision* parts 2, 3 and 4.
