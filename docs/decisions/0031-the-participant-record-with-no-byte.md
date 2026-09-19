# 0031: the participant record with no byte

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** steps 2, 3 and 4 landed with the promotion (#358); step 1
landed 2026-09-19. **Decided 2026-09-18, on the owner's delegation**: question 2
is answered *out of contract*, which selects the small branch this record
predicted for that answer, and questions 3 and 4 are answered with it.

**Frozen.** Corrections go in [`README.md`](./README.md)'s row for this record,
not in place — and this record is one that will attract them, because its
evidence is a census a reader can re-run and the workspace moves under it.

## Context

[`0028`](./0028-the-slot-a-killed-participant-keeps.md) made the participant's
OFD lock byte the **whole** liveness predicate — its open question 1, answered by
the owner on 2026-08-18, collapsed the two-fact predicate to the byte alone on
the grounds that *"a writer joins through the rendezvous and every participant
that can hold a slot holds a byte."*

That sentence is false of one call, and the call is `pub`:
`TreeBuilder::build_shared` registers a `LIVE` participant record
(`register_participant`, `crates/tf_tree/src/tree.rs`) and takes **no lock
byte**, because such an arena has no lock file at all — the fd is the capability
(`docs/PHASE2.md` §3.1 — *this cited §3.2 until 2026-09-18; that section is
"Identity and defaults", the phrase "the fd is the capability" appears nowhere in
`PHASE2.md`, and §3.1 is the section stating the sharing boundary this arena sits
outside*). So the arena can contain a `LIVE` record over a
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

`reclamation_verdict` (`crates/tf_tree/src/open.rs`) reads the state word,
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

**The discriminator is the lock file, not the server** — and a first version of
this section got that wrong, which is worth stating because the counterexample is
the library. There are **19** `.build_shared(` call sites in the workspace —
`rg -n '\.build_shared\(' crates/` gives 20 hits, one of which
(`crates/tf_tree_c/tests/bridge_shared.rs:324`) is a `///` comment describing a
mutant and calls nothing. *The path matters: run at the repo root the same
pattern gives 27, the extra seven being prose in `docs/decisions/`, this record
among them.* One of the 19 is **`Open::open`'s `Created` arm** in
`crates/tf_tree/src/open.rs`, which calls `build_shared` and whose
`spawn_owner_server` then binds an `OwnerServer` over the result. Writing "the only composition of `build_shared` with
`OwnerServer` is the test" was therefore false, and a reader re-running the check
gets a different answer than the record — the failure this project keeps having
with enumerations.

*No line numbers in this section except the one naming a `///` comment by
position — `bridge_shared.rs:324` above, which is a citation of prose rather than
of code and moves only if that file's comments do. The first version had them, and **this PR's own
rustdoc edit to `open.rs` moved every one by seven lines** — a citation invalidated
by the commit that wrote it. The round-4 sweep that says so **left two**, and a
round-5 edit to a test's doc comment moved both: the announcement of the fix was
itself incomplete, which is why this note now names that too.*

It is also the wrong question. That arm holds the ownership byte and participant
byte 0 **before** it builds — `register_creator` takes `CREATOR_SLOT` during
`Open::open` — then calls `build_shared`, then installs `use_ofd_liveness` and
`use_claim_leases` on the tree it got back, and only then binds. It is
`build_shared` **plus** the OFD-liveness half, which is precisely the supported
composition `tf_tree_c`'s bridge names. *This said the leases were installed
before the build; they are installed after it and before the bind, which the
source says in a comment two lines up. The composition is unchanged; the order
stated was not the order.* What this record is about is the
composition that omits that half.

**So, stated as the discriminator actually is:** of the 19 call sites, exactly
one stands up a rendezvous *with* a lock file (`Open::open`'s `Created` arm, the
supported path); **two** stand one up *without* one —
`a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes` and the
`byteless_served_arena()` helper, both in `crates/tf_tree/tests/rendezvous.rs`,
feeding the three tests that stage this record's measurement on purpose; and the remaining **sixteen** stand up
no rendezvous at all — ten passing the fd to a child, and **six**
(`backing.rs`, `workload.rs`, `control_loop`, `hugepage_grant`,
`replay_bit_identity` ×2) never calling `shared_fd` at all, using the arena in
one process (`backing.rs`, `workload.rs`,
`cache.rs`, `tree.rs`, `mp_bench`, `attach_bench`, `shm_scaling`,
`hugepage_grant`, `heap_vs_shared`, `control_loop`, `tests/population.rs` ×3,
`tests/multiprocess.rs`, `replay_bit_identity.rs` ×2). **No shipped path composes
the byte-less served shape**, and that is the claim this answer rests on.

*A first version of this paragraph said 20 and seventeen, and listed
`bridge_shared.rs` among the callers — in the paragraph whose subject is a census
a reader can re-run.*

**And it reaches neither binding — question 3, answered by checking.**
`tf_tree_py` exposes exactly one shared path, `open_arena`, which is
`tf_tree::Open`; `build_shared` appears nowhere in `crates/tf_tree_py/src/`. In
`crates/tf_tree_c/src/` it appears in two doc comments only — `bridge.rs`'s
*Why `tf_tree::Open` and not `TreeBuilder::build_shared`*, quoted above, and
`unstable.rs`'s note about a byte-less participant. So the shape is reachable
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
can find by name. Unserved — sixteen of the nineteen call sites — there is no
rendezvous, no lock file, and therefore no observer holding a probe: the record
is byte-less and *unobserved*, which is not a defect but the design.

**The shipped picture is sharper than "sixteen".** All sixteen unserved sites are
tests, benches or examples — including `cache.rs` and `tree.rs`, both inside
`#[cfg(test)] mod tests`. **The only `build_shared` call in shipped library code
is `Open::open`'s `Created` arm**, which is the served, lock-file-holding,
supported composition. So no shipping call site is unserved, and the one shipping
call site is served. *Two earlier spellings — "every composition in this
workspace", then "every one that ships" — were the universal part 1 retracts,
reintroduced; the fact that replaces them is stronger than either.*

What makes the wrong opinion available is binding a rendezvous over that arena
afterwards, and that is a **second call by the same caller**, not a property of
`build_shared`.

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
   gates everything — but it removes a reason that was on the page. *(Question 2
   was answered on 2026-09-18 and gates nothing now; this paragraph is retained
   draft text, and what it still says correctly is the cost of option 2 should
   the answer ever be reopened.)*

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
   `let Some(lock) = self.lock_file.as_ref() else { return Ok(None) }` — *this
   record said `self.claim_lock`, a field `0029` renamed; a stale symbol misleads
   exactly as a stale line number does, and this PR de-numbered the citations
   without re-reading the quoted code* — so a
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
   severity.** `reap_dead` and `reap_participants` are explicit calls: nothing in
   the library invokes either on its own.

   **The "no production caller" half of this is stale and is corrected rather
   than deleted.** It read: *"`git grep` finds `shm_torture.rs` and
   `rendezvous_child.rs`, a bench and a test binary."* That was true when this
   record was drafted and false since
   [`0044`](./0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)
   (2026-08-29), which gave the sweep to both bindings: `tft_tree_reap_dead` in
   the C ABI and `Tree.reap_dead()` in Python both call
   `reap_dead() + reap_participants()`. **The direction is against this record's
   comfort, so it is stated plainly** — the sweeping peer is now an ordinary C or
   Python consumer rather than a bench, which makes that half of the composition
   *easier* to reach, not harder. The conclusion is unchanged because the other
   half, a hand-served `build_shared` arena, is what this record puts out of
   contract, and neither binding can produce one (question 3).
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
   `backing.rs`, `workload.rs`. *(Retained draft text, and that universal is
   **refuted** by this record's own Decision part 1: `Open::open`'s `Created` arm
   composes `build_shared` with a rendezvous, holding a lock file, and is the
   supported path. The framing is kept because a reopening needs it; the claim in
   it is not the record's.)* If serving one by hand is out of contract, this
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

**Steps 2, 3 and 4 landed with the promotion (#358), because each is a
consequence of the *status change itself* rather than of the boundary being
written; step 1 was the one PR after it and landed 2026-09-19.** The answer
changes no code path, so all that step shipped is where the boundary is written.

*Step 2 moved across that line during review, and the criterion is what moved
it.* The three tests carried messages reading "`0031` has been answered — invert
it"; the answer changes no behaviour, so an engineer following that instruction
inverts a passing assertion and breaks the suite. That trap is created by the
status flipping, not by a rustdoc being written, so it belongs with the
promotion by this record's own partition.

1. **Say it where the call is. DONE 2026-09-19.**
   `TreeBuilder::build_shared`'s rustdoc gains the
   boundary: this creates an arena whose fd is the capability, and **binding a
   rendezvous over it is out of contract**, with the reason (the byte-less record
   no observer can judge) and a pointer here. `PHASE2.md` **§3.1** — *The sharing
   boundary is the runtime directory*, NORMATIVE — gains the same sentence,
   because a `build_shared` arena is precisely one that sits outside that
   boundary, and serving it is reaching back across. *An earlier revision of this
   step said §3.2; that section is env-var defaults.*
   - **Three shipped sites describe the shape and must be reconciled, found by
     review rather than by the plan**, which is why the step names them instead
     of a `rg` that would have to find them again. *A fourth,
     `crates/tf_tree/src/open.rs`'s `reclamation_verdict` rustdoc, said the
     question was "being decided", and a **fifth**, `Tree::participant_slot`'s public
     rustdoc, described the byte-less record's fate with no mention that the
     serving composition is out of contract. Both were corrected with the
     promotion, because a status change is what falsified them. Writing "three"
     as a closed count was the enumeration failure this record spends a paragraph
     on — and the correction to "four" repeated it one round later.*
     - `crates/tf_tree_cli/src/checks.rs` — *"`TreeBuilder::build_shared` called
       directly still registers without a byte **and is still supported**"*. The
       sentence is true of the **call** and this answer does not change it; what
       it must not be read as is support for serving the result. It gains that
       clause.
     - `crates/tf_tree_c/src/unstable.rs`, `tft_tree_reap_dead`'s doc — names "a
       `TreeBuilder::build_shared` participant with no socket" as one of two
       producers of a stale claim with no hangup, "and this is their only
       collector". That producer exists **only** in the composition this answer
       puts out of contract: the creator's claim can go stale to another process
       only if that process has the arena read-write, which is either
       `attach_shared(ReadWrite)` — refused by `0028` step 0b — or the
       rendezvous. The other producer, a dead owner, is in contract and unchanged.
       So the doc keeps both and says which is which.
     - `docs/RUNBOOK.md` carries the same pair to operators and gains the same
       distinction — and, because it is the operator-facing one, what to do:
       **do not sweep**, since `Tree::reap_dead`, `tft_tree_reap_dead` and
       `Tree.reap_dead()` will take the claims of publishers that are running.
       **No `tf_tree` subcommand sweeps** — checked rather than assumed, because
       a first draft of that sentence named `doctor` as one of them.
     - **A second rustdoc in `checks.rs` carries the pair too**, on `TFT014`'s
       claim-half bullet, and step 1 found it where the plan named only the
       "still supported" sentence in the same file. It is reconciled with the
       rest. This is the closed count this record spends a paragraph on,
       arriving once more in the step written to fix it — so the list above is
       what step 1 touched and is not a claim about what exists.
   - **Reconciled with the promotion, not by this step** — `PHASE2.md` §0.0, §3.9
     and §5.1, `PHASE3.md`, `CHANGELOG.md`, `CLAUDE.md`, and the three shipped
     rustdocs and comments above. Each was falsified by the status flipping rather than by the
     boundary being written, which is this record's partition; they are listed so
     an engineer running step 1 knows what is already done.
   - **Verified by** `just doc` and `just lint`, and by
     `rg 'build_shared' docs/ crates/ CLAUDE.md` over the result. **The repo root
     is in that command because it was not**, and `CLAUDE.md`'s Status section
     carries the exact sentence this step reconciles — a check scoped to two
     directories could not have found it.
2. **Keep it executed — three tests execute the boundary, two carried prose that
   had to change. DONE with the promotion (#358).**
   `a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes` says
   of itself *"It pins the defect, not the fix. When `0031` is answered this test
   flips, and each `PIN:` message says which way."* This is the answer, so it
   flips: from a defect pinned pending a decision to a **characterisation of an
   unsupported composition**, asserting the same observable facts with the `PIN:`
   messages restated as what the boundary costs. The assertions do not change —
   the behaviour does not change — only what the test claims about it.

   **And so does a second, which a first version of this step missed.**
   `a_byteless_publisher_is_evicted_from_the_edge_it_is_publishing_to` carries
   the same pending-decision framing in its doc comment and an in-test message
   reading *"If this is now 0, 0031 has been answered — invert it"*. **The third,
   `a_leased_publisher_keeps_its_edge_against_a_sweeper`, is the control and
   needed nothing** — it shares only the `byteless_served_arena()` helper and its
   doc carries no framing about this decision. Left alone the second would tell a
   reader this record is still open, which is the defect `0055` step 7 spent four review
   rounds on: a claim corrected everywhere except where somebody reads it.
   - **Verified by** all three still passing unmodified in substance, and by the
     mutant the first already documents (`reap_participants` counting the verdict
     without calling `reclaim`) still failing it.
   - **Not a rename.** The names describe what is executed and stay; what changes
     is the prose around them. A test named for a defect is the right name for a
     characterisation of the same behaviour.
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

## What review changed, because the record offers its evidence as re-runnable

**The ruling never moved; the facts under it were corrected one after another.**
Recorded because this record's whole claim on a reader is that its measurements
can be repeated — and no count of rounds or of the list below is given, because
those went stale too, in this very section, in the round that added a bullet to
it.

- **The discriminator was wrong.** "The only composition of `build_shared` with
  `OwnerServer` is the test" is false: `Open::open`'s `Created` arm is one, and
  it is the *supported* composition. What separates the shapes is the **lock
  file**, not the server — and the corrected fact is stronger than the false one.
- **The universal came back twice more** after being retracted, as "every
  composition in this workspace" and "every one that ships". What replaced it is
  sharper than any of the three: **the only `build_shared` call in shipped
  library code is that `Created` arm**, and every other call site is a test,
  bench or example.
- **The census was 20, then 19.** One `rg` hit is a `///` comment describing a
  mutant, and the number is reproducible only under `crates/` — at the repo root
  the same pattern gives 27.
- **The shipped sites were three, then four, then five, then six.** `checks.rs`,
  `unstable.rs`, `RUNBOOK.md`, then `reclamation_verdict`'s rustdoc, then
  `Tree::participant_slot`'s, then the comment inside `Tree::reap_participants`.
  Each correction restated a closed count and the next round found one more; this
  sentence is the fourth restatement and is not offered as the last.
- **Line citations were invalidated by the commit that wrote them.** A rustdoc
  edit in this branch moved `open.rs`'s by seven lines (`git diff --numstat main...HEAD`); the sweep that de-numbered
  them left two, which a later edit moved as well. The section cites symbols now
  — **and a symbol goes stale the same way**: the record quoted
  `self.claim_lock`, a field `0029` renamed to `lock_file`, and the de-numbering
  passes did not re-read the code they were citing.
- **An ordering was stated from memory.** The `Created` arm was described as
  installing claim leases *before* it builds; it installs them after the build
  and before the bind, which the source says in a comment two lines above the
  call. The composition the ruling rests on is unchanged; the sequence written
  down was not the sequence.

- **This branch broke line citations *outside* the files it edited, and they are
  not fixed here.** The two rustdoc corrections add 7 lines to
  `crates/tf_tree/src/open.rs` and **22** to `tree.rs`, and `docs/` carries **36
  unique `path.rs:N` citations into those two files** across 13 documents.
  **Twenty-two of the 36 sit past an edit point and are now off by the shift** —
  21 into `open.rs`, 1 into `tree.rs`; the other 14 are above both edits and are
  untouched. Four already pointed at a blank or bare-`///` line on `main`.
  Verified broken on two,
  [`0055`](./0055-the-recovery-capacity-a-fleet-cannot-add-later.md)'s
  `open.rs:1183` (`.take_attached()`) and
  [`0030`](./0030-the-atfork-handler-and-inherited-descriptors.md)'s `:1093`.

  **Not swept, and the reason is structural rather than effort.** 23 of the **42**
  citation sites are in `implemented` records, which this project freezes:
  corrections to them go in `decisions/README.md`'s errata, and twenty-three
  line-number errata would bury that file's real ones. **This is a repository
  problem that this branch exposed rather than created** — any edit to `open.rs`
  breaks up to 22 citations, nothing gates it, and the convention that produced
  them is still in force. The fix is a convention plus a gate: cite symbols, and
  have `artifact-versions` refuse *new* `crates/**.rs:N` citations in `docs/`
  while grandfathering the existing ones. **That is owed work and is recorded
  here rather than done in a record-move PR.**
- **A claim about the *world* went stale while the record sat in `draft`.**
  Question 1's severity argument said `reap_dead` has "no production caller — a
  bench and a test binary". `0044` gave the sweep to both bindings a month
  before this promotion, and the direction is against the record's comfort: the
  sweeping peer is now an ordinary C or Python consumer. The conclusion survives
  because the *other* half of the composition is what is out of contract, but a
  `draft` promoted to `ready` inherits every fact it stopped checking.
- **Fixes reported as landed had not been written — five times, from one
  scripting pattern.** Round 4's commit message says the nested emphasis in the
  §3.1 erratum was closed; it was not, and the nesting survived four more rounds.
  Round 11's claimed four more — `PHASE2.md`'s producer-pair qualification, the
  reconciled-sites list, step 2's test count, and the shipped-site enumeration —
  and **none of the four reached the file**.

  The cause is mechanical and worth naming because it is invisible from the
  output: each edit accumulated replacements into one string, asserted on each
  pattern as it went, and wrote **once at the end**. A single failed assertion
  discards every replacement before it, while the `print()` calls that already
  ran make the run look partly successful. The fix is to write and **re-read**
  each edit independently, which is what round 12 does. **A commit message is not
  a gate, and neither is a script's stdout.**
- **And a correction inverted the thing it corrected.** `Tree::participant_slot`'s
  rustdoc carries a guard naming the symbols that must *not* become intra-doc
  links, because linking a `shm`-gated name reddens `just stable-tier-check` on
  the tier a published consumer reads. Rewriting that guard added
  `participant_alive` — which carries no `cfg` and is *already* a working link
  four paragraphs above — and dropped `TreeBuilder::build_shared`, which is
  gated. Exactly inverted, in a sentence whose only job is to say which names are
  unsafe.

The pattern under all of them is one thing: **a claim written at the site being
edited, from what was already believed, rather than from the check re-run at that
moment.** It is the same failure the record's own subject is an instance of —
`0028` reasoning about a probe from the observer's side instead of the subject's.

## Not in this record

**#201 itself.** The byte/record divergence is a different defect with a
different fix — make the two indices one number by construction — and it stays on
its own issue. This record borrowed its reproduction harness and nothing else.
