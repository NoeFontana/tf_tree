# 0031: the participant record with no byte

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** *Implementation plan* below — one PR. **Decided 2026-09-18,
on the owner's delegation**: question 2 is answered *out of contract*, which
selects the small branch this record predicted for that answer, and questions 3
and 4 are answered with it.

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

**A served `build_shared` arena is out of contract.** Question 2 is the one this
record said had to be answered by the owner because it selects between two very
different sizes of fix; answered that way, the fix is the small one this record
already described — *"a refusal plus a sentence in the docs"* — with one
correction to that phrasing, in part 2.

### 1. Why out of contract, on evidence rather than preference

**The composition is a hand-assembled half of a path that already exists.**
`tf_tree_c`'s bridge states the whole argument in code, and it was written
without this record in view (`crates/tf_tree_c/src/bridge.rs`, *Why
`tf_tree::Open` and not `TreeBuilder::build_shared`*): `build_shared` "publishes
no rendezvous … the fd is the capability", and **the path that publishes is
`Open::open`'s `Created` arm, which is `build_shared` *plus* OFD liveness, claim
leases, the owner server and ownership**. Serving a `build_shared` arena by hand
is that composition with the OFD-liveness half left out — and every consequence
question 1 measured is a consequence of leaving it out. The project already had
its answer; it was in a rustdoc rather than a record.

**Nothing composes it, measured now rather than quoted from the draft.** `rg`
over every `build_shared` call in the workspace: `tf_tree_bench`'s `backing.rs`,
`workload.rs`, `mp_bench`, `attach_bench`, `shm_scaling`, `tests/population.rs`,
`tests/multiprocess.rs`, and `tf_tree_cli`'s `replay_bit_identity.rs` — all pass
the fd directly and stand up no rendezvous. The **only** composition of
`build_shared` with `OwnerServer` in the tree is
`a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`, the
test this record's measurement is written from, which stages the shape on
purpose.

**And it reaches neither binding — question 3, answered by checking.**
`tf_tree_py` exposes exactly one shared path, `open_arena`, which is
`tf_tree::Open`; `build_shared` appears nowhere in `crates/tf_tree_py/src/`. In
`crates/tf_tree_c/src/` it appears only in the two doc comments quoted above and
in `unstable.rs`'s note about a byte-less participant. So the shape is reachable
only by a Rust caller who composes two public APIs that no shipped path composes,
and the answer costs no binding a capability it has.

### 2. What that selects, and the correction to "a refusal plus a sentence"

**The refusal cannot be mechanical, because there is nowhere to put it.** This
record's own third option — *refuse `build_shared` on an arena that will be
served* — is unexpressible at `build_shared`, which cannot know whether a server
will later be bound over its fd; that much the draft already says. What the draft
did not check is the other end: **`tf_tree_ipc` cannot express it either.**
`OwnerServer::bind_at` takes a `SegmentDescriptor` — `format_version`,
`layout_hash`, `arena_size`, `instance_uuid`, `boot_id` — and `serve` takes a
`BorrowedFd`, and the crate's dependencies are `rustix` and `libc` and nothing
else, so neither can map a participant table to see the absent byte. A refusal
there would need `tf_tree_arena` on a crate that deliberately has no arena
dependency, which is a larger change than the defect.

So the answer is **prose plus a characterisation test**, and the second half is
what keeps it from being only prose: the shape stays *executed*, with its
consequences asserted, so the boundary cannot rot into a sentence nobody runs.
That is the same reason `PHASE2.md` §0.0's false claim survived three days —
this record's own measurement section says it: *"what let it survive is that
nobody executed it."*

### 3. Why step 0b closed two byte-less entry points and this answer closes the third differently

This record requires that of whatever it decides, and the answer is that the
three are not the same shape.

`attach_shared(ReadWrite)` and `attach_shared_at(ReadWrite, slot)` attach to an
arena **somebody else created and may be serving**. A byte-less `LIVE` record
appears in a table that probe-carrying observers are already looking at, so the
wrong opinion is available the moment the record exists, and the only way to stop
it is to refuse the call — which is what step 0b did, breaking the API to do it.

`build_shared` creates an arena whose **fd is the capability** and which nothing
can find by name. Unserved — every composition in this workspace — there is no
rendezvous, no lock file, and therefore no observer holding a probe: the record
is byte-less and *unobserved*, which is not a defect but the design
(`PHASE2.md` §3.2). What makes the opinion available is binding a rendezvous over
it afterwards, and that is a **second call by the same caller**, not a property
of `build_shared`.

So step 0b refused the calls whose defect is intrinsic, and this answer refuses
the **composition** whose defect is not in either call. That is also why the
refusal is not mechanical: the defect belongs to a pair of calls, and neither
member can see the other.

### 4. What is *not* decided, and must not be read into this

Options 1 and 2 are **not taken and not refused** — they are moot, because both
buy a way to judge a byte-less record and this answer says the arena that
produces one is not a shape to support. If question 2 is ever reopened by a
measured field need, both are still on the table at the cost the draft records.

**Question 4 does not count for anything here.** Option 1 would have closed #201
as a side effect; option 1 is not taken, so #201 is untouched and keeps its own
narrower fix. The draft warned against double-counting that argument and this is
where the warning applies.

### The two shapes that are now moot, kept for a reopening

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
   was taken with its own argument.

   **The second half of this objection is retracted (#269).** It read: *"and
   [`0029`](./0029-the-topology-lock-is-a-kernel-lock.md) is a whole record about
   *not* having a second spelling of liveness."* That was true of the record's
   `draft`; it is false of the record that landed. `0029` was re-scoped, and its
   plan step 2 — *delete `participant_is_alive` as a second spelling* — was
   **reversed**: after that change the two functions answer different questions,
   one *"is participant `s` running"* and the other *"may this topology word be
   stolen"*, and the record says deleting the second would have routed a steal
   through a predicate measured to be wrong. So `0029` is now a record about
   liveness predicates being indexed by **what they authorise**, which is an
   argument *for* option 2's shape rather than against it. This does not decide
   the option — the cost of a `/proc` conjunct is unchanged and question 2 still
   gates everything — but it removes a reason that was on the page.

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
2. **ANSWERED 2026-09-18: no — out of contract. *Decision* parts 1 and 3.**
   The question is kept in full below rather than collapsed, because a
   reopening needs the framing that made it a scope decision.

   **Is a served `build_shared` arena a shape this project supports at all?**
   `0028:1113` calls `build_shared` "a supported shape — it is how an arena gets
   created", but every composition of it in this workspace
   passes the fd directly and stands up no rendezvous — `mp_bench`, `attach_bench`,
   `backing.rs`, `workload.rs`. If serving one by hand is out of contract, this
   record's answer is a refusal plus a sentence in the docs, and it is small. If
   it is in contract, it is option 1 or 2 above. **Nothing currently says which**,
   and that ambiguity is the reason this is a record rather than a patch.
3. **ANSWERED 2026-09-18 by checking: no.** ~~Does the same hole reach
   `tf_tree_c` or `tf_tree_py`? Both bind the Rust core directly. Whether either
   exposes a byte-less read-write registration was not checked.~~

   `tf_tree_py` reaches a shared arena through exactly one function,
   `open_arena`, which is `tf_tree::Open`; `build_shared` appears nowhere in
   `crates/tf_tree_py/src/`. In `crates/tf_tree_c/src/` it appears only in
   `bridge.rs`'s *Why `tf_tree::Open` and not `TreeBuilder::build_shared`* and
   in `unstable.rs`'s note about a byte-less participant — no entry point
   registers one. **This is load-bearing for the answer above**: had either
   binding exposed the shape, "out of contract" would have been a much harder
   position, because it would have meant withdrawing a capability a shipped
   binding already had.
4. **MOOT 2026-09-18: option 1 is not taken.** *Decision* part 4 says why this
   must not be counted as an argument for anything.

   **What happens to `#201` if option 1 is taken?** Giving every registration a
   byte by construction would make the two indices one number and close #201 as a
   side effect, which is an argument for option 1 that has nothing to do with this
   record's own defect. It should not be double-counted: #201 also has its own
   narrower fix.

## What made this `ready`

- ~~Question 1 answered by a measurement, not by reading the claim path.~~
  **Done 2026-08-22**, and the measurement is what sets the severity: D7 holds
  and no data is corrupted, but a byte-less publisher cannot keep an edge in the
  presence of a sweeper, and the participant table stops uniquely identifying a
  process.
- ~~Question 2 answered by the owner.~~ **Done 2026-09-18, on delegation:** out
  of contract.
- ~~Whichever option is chosen, the §11.2 or §11.3 walk that D15 makes it owe.~~
  **Not owed by this answer.** D15 makes a *mutation protocol* walk the crash
  matrix; this answer adds no protocol and changes no code path. What it owes
  instead is that the shape stay executed, which is step 2 of the plan.

## Implementation plan

**One PR.** The answer changes no code path, so what it ships is where the
boundary is written and what keeps it measured.

1. **Say it where the call is.** `TreeBuilder::build_shared`'s rustdoc gains the
   boundary: this creates an arena whose fd is the capability, and **binding a
   rendezvous over it is out of contract**, with the reason (the byte-less record
   no observer can judge) and a pointer here. `PHASE2.md` §3.2 — where *the fd is
   the capability* is stated — gains the same sentence, because that is the
   section a reader consults for this property rather than a rustdoc.
   - **Verified by** `just doc`, and by `rg 'build_shared' docs/ crates/` finding
     no other site that describes the shape as supported.
2. **Keep it executed.** `a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`
   says of itself *"It pins the defect, not the fix. When `0031` is answered this
   test flips, and each `PIN:` message says which way."* This is the answer, so
   it flips: from a defect pinned pending a decision to a **characterisation of
   an unsupported composition**, asserting the same observable facts with the
   `PIN:` messages restated as what the boundary costs. The assertions do not
   change — the behaviour does not change — only what the test claims about it.
   - **Verified by** the test still passing unmodified in substance, and by the
     mutant the test already documents (`reap_participants` counting the verdict
     without calling `reclaim`) still failing it.
3. **Remove the ledger row.** `PROJECT.md` §5.1 carries *"`0031`'s question |
   `0031` (`draft`) | Queued only if `0031` is answered by giving a byte-less
   participant record something to be judged by"*. It is not, so the row goes —
   and by that ledger's own rule, *"Adding a row is not a decision; removing one
   is"*, which is why the removal belongs to this record and not to a tidy-up.
   - **Verified by** `just lint`'s `artifact-versions`, which holds every table
     row to its header's cell count.
4. **Update `decisions/README.md`'s row** to say what was decided, per that
   file's rule that it records the decision and never restates a status.

**Stop point:** if step 1 cannot state the boundary without also describing a
mechanism that does not exist, the answer is being written as though it were
enforced, and the wording is wrong rather than the decision.

## Not in this record

**#201 itself.** The byte/record divergence is a different defect with a
different fix — make the two indices one number by construction — and it stays on
its own issue. This record borrowed its reproduction harness and nothing else.
