# 0055: the recovery capacity a fleet cannot add later

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** **every step has landed** — 2 in #353 (`0411adb`), 1/3/4 in
#355 (`ea98e56`), 6 in #356 (`d121589`), 7 after it. Step 5 is **struck**: open
question 1 answered *no mechanism*, so there was nothing to build.


## Context

An arena reaches a state from which no process outside it can ever attach again. It takes three conditions at once:

1. the **ownership byte is free** — nobody serves the rendezvous;
2. **at least one participant byte is held** — some process still has the segment mapped;
3. **no *eligible* participant calls `Tree::inherit_ownership`** — eligible means attached **and** read-write **and** actually polling. **Eligibility, not mode, is the axis.**

(1) and (2) refuse every new **rendezvous** attach: nothing serves, so §3.7's join cannot start, and the create path is refused by §3.4's split-brain check, which yields to any held participant byte with no window to tune (`crates/tf_tree_ipc/src/open.rs:365-372`). The one non-rendezvous path, `Tree::attach_shared` over a passed descriptor, cannot supply an heir either:

- `refuse_a_byteless_writer` (`crates/tf_tree/src/tree.rs:2897`) turns `AttachMode::ReadWrite` away with `ShmError::ReadWriteNeedsRendezvous` before mapping ([`0028`](./0028-the-slot-a-killed-participant-keeps.md) step 0b).
- `Tree::attach_joined_at` (`crates/tf_tree/src/tree.rs:2947`) is `pub(crate)` and called only from `Open`'s `Joined` arm (`crates/tf_tree/src/open.rs:1183`), which condition (1) makes unreachable.
- `Tree::inherit_ownership` answers `Inheritance::NotApplicable` unless `is_joined()` (`crates/tf_tree/src/open.rs:599-601`).

A read-only `attach_shared` over a passed fd still succeeds; it holds no byte, so it is neither a candidate nor condition (2). "Holder" means *byte*-holder. **The set of processes that can satisfy (3) is fixed at the instant (1) becomes true and can only shrink.** A read-only consumer holds a participant byte (`crates/tf_tree_ipc/src/open.rs:328-345`) and attaches read-only by D18 (`crates/tf_tree/src/open.rs:605-606`).

The wedge is pinned mechanically by `a_live_participant_prevents_a_second_arena` (`crates/tf_tree_ipc/tests/multiprocess.rs:230`, §11.2 scenario 9, 128 iterations) and `scenario_3_an_owner_dying_leaves_readers_working_and_joins_refused` (`crates/tf_tree/tests/rendezvous.rs:4577`). `tf_tree participants` shows only the `mode` column (`crates/tf_tree_cli/src/lib.rs:1660-1661`): *attached at the vacancy instant* and *polling* are observable by nobody afterwards (D17).

## Decision

**Decided 2026-09-18: no new surface.** This record authorises documentation and tests, not a protocol.

**1. Documentation of an existing consequence.** [`PHASE2.md`](../PHASE2.md) §3.5 and [`RUNBOOK.md`](../RUNBOOK.md) state:

> Recovery capacity is whatever was **attached and eligible at the instant the ownership role fell vacant**. It cannot be added afterwards: an ownerless arena with a held participant byte admits no new **rendezvous** attachment, and the rendezvous is the only door a would-be heir can come through — the descriptor-passing path refuses `AttachMode::ReadWrite` outright — so the set of processes that could inherit can only shrink from that instant on. A fleet that intends to survive owner death holds that capacity **before** the owner dies. **Eligibility, not mode, is what is held**: a read-write attachment that never polls `owner_lost()` is not recovery capacity, and neither is a read-write node that was not attached at that instant.

together with the three ways to be ineligible (read-only; built against a release with no such call; never polls), and that RUNBOOK's "open one process read-write" remedy is necessary and not sufficient.

**2. Question 1: the NORMATIVE fleet requirement, and no mechanism.** A standby-heir helper polls, so it is a thread or process; [`PHASE2.md`](../PHASE2.md) §3.5 is NORMATIVE that there is "no background thread, no daemon, no watcher a user must run", and [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) holds that every required process is where adoption dies. "A thread per attachment is the library-shaped version of the same cost" (`crates/tf_tree/src/open.rs:565`). An optional helper names the gap and closes it for nobody who did not already know. The requirement is advice with a normative label; whether anyone *calls* is unobservable from outside. **Reopen only on** a measured field incident in which a fleet that had read the requirement wedged anyway.

**3. Question 2: no adopting an ownerless arena.** Each route supersedes a standing decision: a named segment runs at [`PROJECT.md`](../PROJECT.md) §6 and §3.9 "no stale segments, ever"; an fd depot is a daemon (`0019`); a lock-file handoff runs at §3.4's deleted step 3 and the five unsound states of [`0037`](./0037-a-takeover-is-not-a-second-open.md) ([`0035`](./0035-the-creators-slot-is-taken-not-found.md) is state 1). A proposal starts by superseding that decision.

**4. Question 3: the message states facts plus one pointer; the remedy lives in RUNBOOK alone.** `Display` for `ArenaHeldButUnreachable` sees which lock bytes are held and cannot tell whether one process holds two of them, nor name the ownership byte's holder; three successive remedies out of that arm were wrong in a reachable state. It prints the bytes held, the first slot, its pid, and where to read what to do. The prose stays in the message layer ([`API.md`](../API.md) R5).

**5. Struck.** No mechanism, so nothing to build.

## Rationale

The property is a consequence of §3.4's split-brain check, §3.7's join and `attach_shared`'s refusal, all true since the rendezvous shipped; stating it costs nothing. Recovery cannot be "requested" later: the segment is an unnamed `memfd` ([`PHASE2.md`](../PHASE2.md) §3.6 step 1) handed over only by a serving owner. Making read-only survivors eligible is refused by D18 ([`PROJECT.md`](../PROJECT.md) §5): a `PROT_READ` mapping cannot write the participant table, and the MMU boundary is the design's only security boundary.

## Consequences

- The NORMATIVE line has nothing gating it beyond the `mode` column (one of its three conditions).
- `shm_torture` needs a candidate whose attachment brackets every owner kill; its `--no-inherit` control already distinguishes "nothing inherited" from "broken". This record does not authorise that change.
- Nothing here reopens anything `0009` cut.

## Implementation plan

1. **State the property. DONE, 2026-09-18** — [`PHASE2.md`](../PHASE2.md) §3.5, and RUNBOOK's *The arena's owner died* and *`ArenaHeldButUnreachable`*.
2. **The message defect. DONE, #353.** The message recommended a recovery that failed when followed verbatim; `the_escape_hatch_creates_over_a_stranded_participant` is extended.
3. **Pin the eligibility half. DONE.** `scenario_3`'s `join-sweep` child is the read-write survivor that never polls: the same `open-uuid` is refused while it holds its byte and succeeds once it exits. the byte-level sequence is `a_live_participant_prevents_a_second_arena` (`crates/tf_tree_ipc/tests/multiprocess.rs:230`), not respelled. `byte_0_and_ownership_held_by_two_different_holders_is_refused_without_naming_a_topology` pins the two-holder `Display` arm.
4. **[`PHASE2.md`](../PHASE2.md) §0.0's *Ownership migration (§3.5)* row records the precondition. DONE** in the checked spelling *"settled by `0055`"* (`DECISION_SETTLED_VERB`), which is what makes `just artifact-versions` gate it.
5. ~~The mechanism.~~ **Struck** (*Decision* part 2).
6. **Reduce what the `ArenaHeldButUnreachable` remedy claims. DONE.** The message is facts only (116-164 bytes, ASCII; convention (e)'s 120 bytes is not claimed); the remedy is `docs/RUNBOOK.md`'s eight-row table indexed by the printed mask, lowest slot and pid, and ownership byte. The negative rule, that no arm states a count of holders or a procedure, is asserted over every renderable state by `every_unreachable_state_reports_the_facts_and_prescribes_nothing`. Messages end `(ArenaHeldButUnreachable)`, convention (g).
7. **The same reduction for `HandshakeRejected`. DONE.** The message states the status and the owner's two numbers, 115-124 bytes, ASCII (`rejection_advice` is deleted); the seven remedies live in `docs/RUNBOOK.md`'s `HandshakeRejected` section. Negative rule: no rendering names a status it did not get (`both_rejection_arms_name_only_the_status_they_carry`, 140-byte budget). `crates/tf_tree_cli/tests/runbook.rs` requires a table row per refusal status, a remedy-cell length floor, the forbidden words across the union of remedy cells, and a worked example equal to `Display`'s output; it lives in `tf_tree_cli` because `cargo package` ships no `docs/` for `tf_tree_ipc`. `status_is_a_refusal`, a total `match`, is the compile error (`cargo check -p tf_tree_ipc --all-targets`) that says a row is owed.

## Open questions

All three are answered in *Decision* parts 2, 3 and 4. Question 1 (a supported way to hold recovery capacity): no mechanism. Question 2 (should a fresh process adopt an ownerless arena): no. Question 3 (is the operator guidance wrong): yes, in one measurable place, fixed by steps 6-7.
