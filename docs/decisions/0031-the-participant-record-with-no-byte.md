# 0031: the participant record with no byte

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none. This record authorises nothing.

## Context

[`0028`](./0028-the-slot-a-killed-participant-keeps.md) made the participant's
OFD lock byte the **whole** liveness predicate — its open question 1, answered by
the owner on 2026-08-18, collapsed the two-fact predicate to the byte alone on
the grounds that *"a writer joins through the rendezvous and every participant
that can hold a slot holds a byte."*

That sentence is false of one call, and the call is `pub`:
`TreeBuilder::build_shared` registers a `LIVE` participant record
(`register_participant`, `crates/tf_tree/src/tree.rs:3155`) and takes **no lock
byte**, because such an arena has no lock file at all — the fd is the capability
(`docs/PHASE2.md` §3.2). So the arena can contain a `LIVE` record over a
permanently free byte, and every reclaimer `0028` shipped reads that as *dead*.

This is not #201. #201 is a byte and a record with **different indices**;
[`0028`](./0028-the-slot-a-killed-participant-keeps.md) plan step 0c refused that
pair at the facade and it stays open below it. What this record is about needs no
divergence, no `tf_tree_ipc` call and no second index — only a record with
nothing to pair against.

## What was measured

At `7739805`, in `crates/tf_tree/tests/rendezvous.rs`, pinned as
`a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`. A
`TreeBuilder::build_shared` creator, published through
`tf_tree_ipc::OwnerServer::bind_at`/`serve` so that facade peers can join it,
holding a claim and pushing samples:

```
C: NO SESSION AT ALL. record 0, pid 2544764. lock file exists: false
C: FACADE JOINER ON RECORD 0: slot 0 state live word 0x6 pid 2544764 alive false
C: RESCUER slot 2 participant_alive(0) = false
C: REAPED 2; record 0 0x6 -> 0x0
C: CREATOR STILL PUBLISHING AFTER THE SWEEP: true
```

**And the control that says the divergence is not the cause.** The same harness,
staging #201's byte/record divergence with the byte still held:

```
D: DIVERGED byte 1 record 0, survivor STILL holding byte 0. alive(0) = true
D: REAPED 0; record 0 word 0x6
```

The divergence alone reclaims nothing. What produces the false verdict is the
**absent byte**, and #201's divergence produces one only because it leaves byte 0
to be released by somebody else.

**Mutant, run rather than asserted.** `Tree::reap_participants` counting the
verdict without calling `ParticipantTable::reclaim` fails the pin at *left: 6,
right: 0* — so the post-sweep word carries the claim, not the count beside it.

## Why every reclaimer inherits it

`reclamation_verdict` (`crates/tf_tree/src/open.rs:299`) reads the state word,
then asks `probe.is_held(slot)`, and **never reads the record's own pid**. A free
byte gives `Some(false)`, which is `Reclaimable`. All three of `0028`'s
collectors share that one predicate, so all three inherit it: the owner's slot
assigner on the next grant past the slot, the owner's socket-hangup callback, and
`Tree::reap_participants` from any read-write peer.

**The probe belongs to the observer, not to the subject.** `0028` argued at plan
step 0b (`:1112-1119`, repeated at `:2230-2233`) that the byte-less class was
"narrowed, not eliminated" but harmless, because such a tree "has **no lock file
and therefore no probe** … so the byte predicate never runs on a tree that has no
bytes to run on". That is the error, and it is the record's own reasoning rather
than a rotted citation: the subject needs no probe. It needs only to be *looked
at* by a peer that has one. `0028` is `implemented` and therefore frozen, so the
correction lives in `decisions/README.md`'s row for it and in `PHASE2.md` §0.0.

## The argument this record must answer, because it is already on the page

`0028` plan step 0b refused `Tree::attach_shared(ReadWrite)` and
`attach_shared_at(ReadWrite, slot)` — a **breaking** change, shipped — and its
reason (`0028:298-299`) applies word for word to `build_shared`:

> In `ReadWrite` mode that produces a `LIVE` record with a permanently free byte
> — indistinguishable, by the byte alone, from the leak.

Step 0b closed two of the three byte-less entry points and left the third,
knowingly (`0028:1110-1119`). Whatever this record decides has to say why, or
close it too.

## Decision

**None yet.** `draft`. The two shapes on the table, neither costed:

1. **Give the record a byte.** `build_shared` acquires a lock file and a byte, or
   registers no record until something does. Keeps the predicate as `0028` left
   it — one fact, the kernel's — and is consistent with step 0b. The cost is that
   `build_shared`'s whole point is that it needs no runtime directory: the fd is
   the capability, and a lock file reintroduces a filesystem dependency into the
   one path that had none.
2. **Restore a second fact for byte-less records only.** A record whose recorded
   `(pid, start_time)` is demonstrably live is not reclaimed however its byte
   reads. This is exactly the `/proc` conjunct that `0028`'s answer to its
   question 1 deliberately deleted on 2026-08-18, so it reopens a decision that
   was taken with its own argument — and [`0029`](./0029-the-topology-lock-is-a-kernel-lock.md)
   is a whole record about *not* having a second spelling of liveness.

A third option — refuse `build_shared` on an arena that will be served — is not
obviously expressible: nothing in `build_shared` knows whether an `OwnerServer`
will later be bound over it.

## Open questions

1. **RESOLVED 2026-08-22 by measurement: no — the failure is availability, not
   integrity. D7 is never violated. But the eviction is unbounded.**
   ~~Does the false-dead verdict lead on to a claim-level loss? The measurement
   above erases the *participant record*. Whether a second writer can then take
   the edge — the outcome D7 and `record_is_alive`'s own doc comment call
   corruption — was measured by nobody, and it is what decides whether this is a
   defect to fix before the next release or a documented limitation. **This is
   the question to answer first**; the shape of the fix depends on the answer's
   severity.~~

   **The claim goes.** `take_claim_lease` opens with
   `let Some(lock) = self.claim_lock.as_ref() else { return Ok(None) }`, so a
   byte-less publisher holds **no lease byte either** — not just no participant
   byte. `Tree::reap_inner`'s guard is
   `if lock.probe_claim(edge).map_or(true, |p| p.held) { continue; }`, which
   declines only when the byte is *held* (and fails safe to held on a probe
   error). An unheld byte is indistinguishable from a dead holder's, so an
   ordinary peer's `reap_dead()` takes the claim of a publisher that is running:

   ```
   A: byte-less creator, record 0, two samples pushed
      reader(A) @1500 = -1.500
   B: joined at slot 1
   B: reap_dead() reaped 1 CLAIM(s) from a LIVE publisher
   B: claim -> Ok, B now owns the edge A is still writing to
   A: push @3000 -> Err(ClaimRevoked { edge: EdgeId(1) })
   B: push @3000 -> Ok(())
      reader(B) @1500 = -1.500   [A's old samples survive]
      reader(B) @3000 = -99.000  [B's]

   VERDICT two-writers-on-one-edge: false
   VERDICT victim-silently-stops:   true
   ```

   **D7 holds and no data is corrupted, which is the part that decides the
   severity.** `edge::reap` bumps the epoch before clearing the owner, so the
   victim's very next `push` is refused with `ClaimRevoked` rather than
   interleaving with the new writer. The ring is untouched — `reap` writes only
   `epoch` and `owner` — so samples already published stay readable, and the new
   writer's land normally.

   **The control discriminates.** Identical harness, publisher joined through the
   rendezvous so it holds a lease: `B: reap_dead() reaped 0 CLAIM(s)` and
   `B: claim -> Err(AlreadyClaimed(EdgeAlreadyClaimed { owner_slot: 1 }))`. So the
   reaper is not simply taking everything; it is the missing byte that decides.

   **What makes it worse than a one-off: the victim cannot keep the edge.**
   `ClaimRevoked`'s documented remedy is to re-claim, and re-claiming succeeds —
   into the same byte-less state, immediately re-reapable. Four rounds, holding
   the writer across them:

   ```
   round 1: B reap_dead() -> 1;  A push -> Err(ClaimRevoked { edge: EdgeId(1) })
   round 1: A re-claimed OK — and is byte-less again
   round 2: B reap_dead() -> 1;  A push -> Err(ClaimRevoked { edge: EdgeId(1) })
   ...
   round 4: B reap_dead() -> 1;  A push -> Err(ClaimRevoked { edge: EdgeId(1) })
   ```

   Every sample between eviction and re-claim is lost, and by
   `PHASE2.md`'s own point about a stale ring, **a consumer cannot tell**: the
   ring keeps answering every lookup off samples nobody is refreshing. The victim
   knows; the fleet does not.

   **Nothing does this automatically, and that is the other half of the
   severity.** `reap_dead` and `reap_participant` are explicit calls with no
   production caller — `git grep` finds `crates/tf_tree_bench/src/bin/shm_torture.rs`
   and `crates/tf_tree/src/bin/rendezvous_child.rs`, a bench and a test binary.
   The owner's socket-hangup callback reclaims **only the participant record**
   (`table.reclaim`), never a claim. So reaching this needs a hand-served
   `build_shared` arena *and* a peer that sweeps.

   **A compounding effect, through the participant half that did ship in
   0.0.4.** Once `reap_participants()` frees the byte-less record, the slot is
   grantable, and the next joiner gets the index the live claim still names:

   ```
   B: reap_participants() freed 1 RECORD(s) — A's among them
   C: joined at slot 0 (A's old record index)
   C: claim -> Err(AlreadyClaimed(EdgeAlreadyClaimed { owner_slot: 0 }))
   C: reap_dead() -> 0  [reap_inner skips owner_slot == own_slot]
   A: push after all this -> Ok(())
   ```

   C is told it already owns an edge it never claimed, and **cannot reap it,
   because the guard that stops a process reaping its own live claim now shields
   A's**. Two live processes on one slot index — which is exactly the failure
   `0028`'s review pass worked out for `RESERVED` ("the worked interleaving ends
   with **two live processes on one slot index**, which is the uniqueness A3's
   claims and A2's topology lock both rest on"), reached by a different route.

   **So: a documented limitation, not a fix-before-the-next-release.** The
   integrity properties hold. What does not hold is that a byte-less publisher
   can keep an edge in the presence of a sweeper, and that the participant table
   uniquely identifies a process. Both belong in whichever option this record
   takes; neither forces the timing.

   **Two instrument failures in this measurement, recorded because both produced
   a confident wrong reading first.** The reader helper built its transform with
   `exp_se3([x, 0, 0, 0, 0, 0])` — `xi[0..3]` is ω, so that is a pure *rotation*
   and every translation read back `0.000`, three stages running, including one
   whose answer was known. And the first recovery harness let the `EdgeWriter`
   drop each round, which *releases* the claim, so rounds 2–4 reaped 0 and the
   run read as "safe after the first eviction". Both were caught only by having a
   stage with a known-good expected value in it.
2. **Is a served `build_shared` arena a shape this project supports at all?**
   `0028:1113` calls `build_shared` "a supported shape — it is how an arena gets
   created", but every composition of it in this workspace
   passes the fd directly and stands up no rendezvous — `mp_bench`, `attach_bench`,
   `backing.rs`, `workload.rs`. If serving one by hand is out of contract, this
   record's answer is a refusal plus a sentence in the docs, and it is small. If
   it is in contract, it is option 1 or 2 above. **Nothing currently says which**,
   and that ambiguity is the reason this is a record rather than a patch.
3. **Does the same hole reach `tf_tree_c` or `tf_tree_py`?** Both bind the Rust
   core directly. Whether either exposes a byte-less read-write registration was
   not checked.
4. **What happens to `#201` if option 1 is taken?** Giving every registration a
   byte by construction would make the two indices one number and close #201 as a
   side effect, which is an argument for option 1 that has nothing to do with this
   record's own defect. It should not be double-counted: #201 also has its own
   narrower fix.

## What would make this `ready`

- Question 1 answered by a measurement, not by reading the claim path.
- Question 2 answered by the owner, because it is a scope decision rather than an
  engineering one, and it selects between two very different sizes of fix.
- Whichever option is chosen, the §11.2 or §11.3 walk that D15 makes it owe.

## Not in this record

**#201 itself.** The byte/record divergence is a different defect with a
different fix — make the two indices one number by construction — and it stays on
its own issue. This record borrowed its reproduction harness and nothing else.
