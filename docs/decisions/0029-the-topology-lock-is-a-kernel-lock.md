# 0029: the topology lock is a kernel lock

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** this PR, in one change — see *Why the lifecycle is
compressed*.

| Step | What landed |
|---|---|
| 1 — byte 1 in the lock file | `tf_tree_ipc::LockFile::{try_take_topology, release_topology}`, `LockRole::Topology` |
| 2 — `reparent` takes it before the word | `Tree::reparent`, `TopologyLease`, `ReparentError::TopologyLease` |
| 3 — the §11.3 `topo.holding_lock` walk | `docs/PHASE2.md` §11.3, and §5.1's probe-vs-acquire amendment |
| 4 — the discriminating test | `a_live_holder_that_proc_calls_dead_keeps_the_topology_lock`, plus `a_killed_topology_lock_holder_releases_its_byte_to_the_kernel` across a process boundary |

## This record was re-scoped, and the re-scope is the decision

It was titled **"one liveness predicate per tree"** and proposed that
`Tree::reparent` swap the `(pid, start_time, boot_id)` triple for `self.liveness`
— the `F_OFD_GETLK` probe. Everything below the *Decision* heading is new; the
*Context* is kept, because the enumeration and the measurements in it are what
made the swap look wrong.

**The old title contained the error.** It presupposed that the answer to #213 is
a better predicate. It is not. A predicate is what you write when you cannot ask
the kernel, and on the path #213 is about, you can.

## Context

Filed as issue #213. `Tree::reparent` is the only path that takes A2's in-arena
topology lock, and it decides whether the current holder is alive from the
`(pid, start_time, boot_id)` triple — **even on a rendezvous tree that is holding
an `F_OFD_GETLK` probe**. `docs/PHASE2.md` §5.1 is NORMATIVE and says liveness is
the lock byte; §0.0 records this as the third of three places where the triple is
still on a correctness path (#205, out of #194).

A false "dead" here **steals A2's topology lock from a live mutator**, which is
the direction §6.2 forbids and the direction `record_is_alive`'s own doc comment
calls corruption. A false "alive" only delays recovery.

`TopoLockView::new` has exactly one call site in the whole facade, so `reparent`
really is the only topology-lock path — this is not one case among several. The
in-arena `topo_lock` has exactly one non-test consumer for the same reason, and
`acquired_at_nanos` has **none**: it is stored on every acquire and read by
nothing, despite a doc comment offering it to `doctor`.

## Why the swap this record used to propose is not the fix

**Neither available fact is sound for death.** Both can say "dead" about a
running process, and both have been measured doing it:

| fact | says "dead" about a live process when | measured |
|---|---|---|
| the `/proc` triple | the holder is in another PID namespace (`Known(st) != stored` against an unrelated local pid — *not* `ENOENT`-shaped, so no "cannot prove death" bias catches it); or is same-user but **non-dumpable** under `hidepid` | the appendix below, and [`0033`](./0033-the-identity-record-cannot-name-a-namespace.md) |
| the participant OFD byte | the holder never had one — a directly-called `TreeBuilder::build_shared` creator holds a `LIVE` record over a permanently free byte | [`0031`](./0031-the-participant-record-with-no-byte.md), pinned as `a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes` |

So the swap trades one unsound predicate for another. That is not a deduction:
this record's earlier revision said the adapter "should *be* P3 rather than
resemble it", and written and run on 2026-08-22 that **steals A2's topology lock
from a live, still-publishing process** — `Err(LockContended { owner_slot: 0 })`
at that `HEAD`, `Ok(())` with the adapter. The control that isolates the cause is
a holder that *does* hold its byte, under the same adapter: refused.

That is what blocked this record on `0031`'s question 2 — a **scope** question
("is a served `build_shared` arena a shape this project supports at all?") that
this record has no standing to answer and no way to route around, because the
probe API collapses "byte released by a dead holder" and "holder never had a
byte" into one `Some(false)` and no adapter can separate them.

**The dependency was real, and it was a consequence of the shape, not of the
problem.** It exists only because the proposed fix keys a *destructive* act on
the *absence* of a byte. Nothing about #213 requires that.

## The facade has five answers to "is slot *s* running?", and they disagree

Enumerated from the code rather than from the spec, because §0.0's row names
three and there are five. [`0030`](./0030-the-atfork-handler-and-inherited-descriptors.md)
depends on this table and it is unchanged by the decision below, except that P4
now reaches its predicate in strictly fewer cases.

| # | site | the fact it consults | which trees |
|---|---|---|---|
| P1 | `liveness_for` (`tree.rs`) | `record_is_alive` — the `/proc` triple, entire. Plus a boot-id arm that returns **`false` for every slot** when the arena's boot id differs from the host's | every tree with no probe: heap, `TreeBuilder::build_shared` called directly, `Tree::attach_shared` |
| P2 | `use_ofd_liveness` | the OFD byte, with an explicit "never report ourselves dead" guard, falling back to `record_is_alive` when `F_OFD_GETLK` declines to answer | both arms of `Open::attempt`, and nothing else |
| P3 | `Tree::participant_alive` | the state word `Acquire`, **then** `self.liveness` — the order is right, and it is right by `&&`'s short-circuit rather than by statement | whichever of P1/P2 the tree carries |
| P4 | `Tree::reparent` | `participant_is_alive`, i.e. the triple. **This is #213** | every tree |
| P5 | the owner server's socket-hangup callback (`crates/tf_tree/src/open.rs`) | neither the byte nor the triple — only the socket (D17) | the owner's serving thread |

**P5 is the one that matters most and is missing from every existing
enumeration**, including §0.0's row and this record's first draft of this table.
It is the *only* facade path that **mutates** the participant table on a liveness
verdict — `rec.state.load(Acquire)`, then `table.reclaim(slot, observed)`,
driving the word to `FREE`. Everything else merely reports. Any statement of the
form "the facade decides liveness from X" has to account for a path that decides
it from neither X nor Y. **A sixth predicate must not be added here without
reading `0028` plan step 4**, because this path now shares `reclaim` with the
slot assigner, which *does* decide from the byte.

Downstream, two shipped consumers already override P1/P2 rather than trusting
them:

- `tf_tree top` (`crates/tf_tree_cli/src/top.rs`) assigns `existing.alive =
  *held` from the lock-file row, commented *"The kernel's answer wins over the
  arena record's"*. So `top` already reports a byte-less live participant dead.
- `tf_tree doctor` (`crates/tf_tree_cli/src/doctor.rs`) records
  `alive: alive || before != after`, bracketing the probe with two `Acquire`
  reads of the state word, so any occupancy change forces *alive*. Strictly more
  fail-safe than P3.

## The ordering constraint, and why the decision below satisfies it structurally

Under `loom`, a predicate that composes an arena word with a lock-byte **probe**
must observe the word first. Reverse the two reads and a published record is
erased in 0.00 s: the reclaimer probes byte *s* free before a registrant takes
it, then observes `live_word(1)`, and CASes a live byte-holder's record to
`FREE`. Under word-then-byte the `Acquire` load of a `live_word`
**synchronises-with** the publishing `Release` store, so a byte probe sequenced
after it must see the byte held. Two independently built models reached this
separately; it is stated once, in `PHASE2.md` §5.1, and `0028` open question 6 is
where it was argued.

**The constraint is about a probe and does not bind an acquire.** A probe is an
advisory read of somebody else's byte and races every subsequent take. An
`F_OFD_SETLK` *excludes* every subsequent take for as long as it is held. The
decision below acquires; nothing it reads afterwards can be invalidated by a
taker, because there cannot be one. That is why the order here is
**byte-then-word** rather than word-then-byte, and why that is not a
contradiction of §5.1 — §5.1 constrains `reclamation_verdict`, which probes.

Stated as the invariants the code now maintains:

- **T1.** On a tree that carries a lock file, the arena topology word is CASed to
  a non-zero value only while this process holds topology byte 1, and the byte is
  released only *after* the word is.
- **T2.** Therefore, if this process holds byte 1 and observes a non-zero
  topology word, the holder is either **dead** or **a writer with no lock file**.
- **T3.** The residual `/proc` predicate decides only T2's second disjunct, and
  only in the safe direction: it authorises a steal solely where it can prove
  death, and it can only ever *withhold* one.

## `just loom` gives this change zero coverage, and its own models say why

`crates/tf_tree_core/src/loom_tests.rs` is the only file in the workspace
containing `loom::model`; A2's lock appears there as `TopoModel`, a documented
reimplementation whose `acquire` takes the predicate as a parameter, "injected
exactly as in the real code".

**Every predicate any model passes is correct by construction** — `|_| true`
three times and `|slot| slot != DEAD_SLOT` once. So the model's liveness is an
**oracle**, and A2's exclusion is a theorem of the oracle rather than of the
code.

**Measured, and the obvious experiment does not work.** Flipping the two-mutator
model's predicate from `|_| true` to `|_| false`, so the model is maximally wrong
about two live holders, leaves `two_mutators_race_the_lock_and_a_reader_sees_no_mix`
**green** (6.42 s). That model never reaches the code under test: `acquire` only
considers stealing after `MODEL_SPIN_LIMIT` (= 3) CAS attempts fail, and there
the lock is always released inside that budget. A `panic!` planted in the steal
branch proves it directly:

```
test loom_tests::a_dead_lock_holder_is_stolen_from_and_leaves_no_trace ... FAILED
        panicked at loom_tests.rs:458: PROBE: steal branch reached
test loom_tests::two_mutators_race_the_lock_and_a_reader_sees_no_mix ... ok
```

**One model steals, and its victim is inert by construction.** In
`a_dead_lock_holder_is_stolen_from_and_leaves_no_trace` the holder is
`core::mem::forget`ed — "the crash: no release, no `Drop`" — so it executes no
further instruction, ever. There is no live holder anywhere in the suite for a
wrong predicate to steal *from*. That test says as much itself: its first draft
ran the dying participant as a loom thread and the authors removed it, because
*"it is **not this one**: it is the false-negative case, where liveness wrongly
declares a live-but-stalled participant dead and it later resumes"*.

**This bore on the swap and does not bear on the decision taken.** The swap
entered exactly that excluded class, so it owed a new model — a live holder and a
fallible predicate — before it could land. The decision below does not enter it:
on a lock-file tree the live-holder case is settled by an `F_OFD_SETLK` that
`loom` does not model and cannot, because it is a syscall and not a memory
operation. What `loom` still covers is unchanged and still correct: the word
protocol, which is untouched. **So this change owes no new `loom` model**, and
that is a claim about scope rather than a green run — `just loom` was green
before and after, and would have been either way.

## Decision

**`Tree::reparent` acquires an OFD lock on the lock file's byte 1 before it
touches the arena word.** Where the tree has no lock file, nothing changes at
all.

1. **Byte 1 of the lock file is the topology mutation lock.** §3.3 reserves bytes
   1–15 and byte 1 was free. `LockFile` gains `try_take_topology` and
   `release_topology`; `LockRole` gains `Topology`.
2. **`reparent` takes it first, releases it last.** The lease guard is declared
   *before* the `TopoGuard`, so Rust's reverse-declaration drop order releases
   the word and only then the byte — T1's second half, enforced by scope rather
   than by a comment.
3. **Byte contention is refused, not resolved.** `try_take_topology ==
   Contended` returns `ReparentError::LockContended` naming whatever the word
   says, and **naming nothing where the word is still zero** — the holder has the
   byte and has not yet CASed. `owner_slot` becomes `Option<u32>` for that, which
   is a breaking change to a public error and is the reason it is listed here
   rather than left to the diff: `tf_tree_core`'s `TopoLockError` reports that
   case as `u32::MAX`, and passing the sentinel through rendered *"the topology
   lock is held by live participant slot 4294967295"* to an operator.
   `docs/API.md` R5 makes the field the contract and the message a diagnostic,
   which is only worth anything if the field is true. The sentinel stays in the
   core, whose callers are engine code that reads its doc comment, and is
   translated once at `tf_tree`'s `From<TopoLockError>`. **This was a
   pre-existing defect** — `TopoLockView::finish` has always produced `u32::MAX`
   for a lock freed between its load and its CAS — that this change would have
   made much easier to hit.
4. **An `fcntl` failure that is not contention is its own variant**,
   `ReparentError::TopologyLease { raw_os_error }`. It is not folded into
   `LockContended`: refusing is the safe direction, but a refusal that names a
   live peer when the real cause is `EBADF` is the shape of wrong diagnosis this
   repository has shipped twice.
5. **The `/proc` predicate stays, unchanged, as the residual.** It is what
   decides T2's second disjunct. `participant_is_alive` is **not** deleted — see
   *Rationale*.

**What this is not.** It is not a second liveness predicate, and it is not a
liveness predicate at all: byte 1 is not asked *whether a participant is alive*,
it is asked *whether anyone is in A2's critical section*. Those are different
questions, and only the second one has to be answered here. The first is what
drags in the participant table, the slot indirection, `0028`'s reclaimers and
`0031`'s byte-less class; none of them are reachable from this path any more.

### The exposure, before and after

One row per class, and **no row moves in the widening direction**:

| holder | before | after |
|---|---|---|
| live, other PID namespace (`Known(st) != stored`) | **stolen from** — #213 | byte held ⇒ refused |
| live, same-user but non-dumpable under `hidepid` | **stolen from** | byte held ⇒ refused |
| live, `SIGSTOP`ped | refused | byte held ⇒ refused |
| dead, held a byte | stolen from | byte free, triple says dead ⇒ stolen from |
| dead, unreaped zombie with a live parent | refused (triple says alive) | unchanged — refused |
| live, no lock file (`0031`'s class) | refused (triple says alive) | unchanged — refused |
| dead, no lock file | stolen from | unchanged — stolen from |

The two rows that move are the two #213 was filed about. **Nothing acquires a
new way to be stolen from**, which is what makes this landable without `0031`.

## Rationale

**Why the byte rather than a better predicate.** §6.1 already made this exact
move for claims, and its own table is the argument: *"`F_OFD_GETLK` says free ⇒
definitively dead… no window, no timeout to tune, and no heuristic that can be
wrong."* §5.1 made it for participant records. A2's topology lock is the last
in-arena lock in the system whose holder's liveness is *inferred*, and #213 is
what that costs. The fix is not a fourth bespoke inference; it is the move the
other two already made.

**Why `participant_is_alive` is kept, when this record used to delete it.** The
deletion was right under the swap: two functions answering the same question from
different facts is the second spelling `CLAUDE.md` forbids. It is wrong here.
After this change the two functions do not answer the same question —
`Tree::participant_alive` answers *"is participant *s* running"* and
`participant_is_alive` answers *"may this topology word be stolen"*, which T2
makes a strictly narrower question with a different sound answer. Deleting it
would mean routing the steal through P3, which is the measured regression this
record opened with. What the deletion was really for is the *duplication of the
`/proc` seam*, and that is addressed instead by `participant_is_alive` carrying,
for the first time, a doc comment naming T2 as the whole of what it decides.

**Why not the conjunction written as a closure.** `steal ⟺ byte free ∧ triple
says dead` is the same rule, and it is what the code computes — but as a compound
predicate it would have had to probe *the holder's participant byte*, which
re-introduces the slot indirection, `0031`'s class and §5.1's word-before-byte
constraint, and it would need a new `loom` model to discharge them. Acquiring the
topology byte computes the same conjunction structurally: the `∧` is the syscall
that already returned, and the residual is exactly the disjunct the kernel could
not settle.

**Why byte 1 and not an arena field.** `FORMAT_VERSION = 3` already happened and
Phase 6's region table is still owed
([`0032`](./0032-the-region-table-was-not-part-of-the-purchase.md)); `CLAUDE.md`
forbids adding an arena field opportunistically. The lock file has fifteen
reserved bytes and no format version to bump. This change touches no arena byte.

**On layering.** The objection is that "topology mutation" is a `tf_tree` concept
placed inside `tf_tree_ipc`, which publishes standalone and has no `tf_tree`
dependency. It is the objection `0035` answered and the answer is the same: the
lock file is a byte map of *roles in one rendezvous*, ownership and claims
included, and neither of those is an ipc concept either. The arena side relies on
the byte; the ipc side imports nothing.

**Why `TopoLockView::acquire` keeps its `Fn(u32) -> bool` parameter.** The
alternative — teach `tf_tree_core` about the lock file — was rejected in this
record's first revision on the dependency budget (D14: `libm` + `bytemuck` +
`blake3`, and a probe is a syscall) and that rejection stands. The core keeps a
predicate-shaped hole and the facade decides what fills it. The predicate is now
a residual rather than the whole answer, which is a fact about the *caller*, and
the caller is where it is stated.

### Alternatives considered

- **Do nothing and document it.** A real option in this project — `PHASE5.md` §8
  is a whole section about not building something. Rejected because §0.0 has
  documented it since #205 and the documentation has not stopped the code being
  wrong.
- **Swap the fact (this record's first proposal).** Blocked on `0031`, widens the
  byte-less class, owes a new `loom` model. Superseded above.
- **Refuse `reparent` outright on a lock-file-less shared tree.** Would let T3's
  residual go away entirely, since every writer would then hold a byte. It is a
  breaking change on a public path that needs `0031`'s question 2 answered, and
  it buys nothing this change does not already have. Left as the follow-on below.
- **Make the topology byte per-participant** (`TOPO_BASE + slot`) rather than one
  byte. Then a holder could be *named* by probing. Rejected: it re-creates the
  slot indirection for a diagnostic benefit, and `F_OFD_GETLK` cannot name a
  holder anyway (`l_pid = -1`, §3.3) — the identity records are what name one.

## Consequences

- A2's exclusion, on every tree obtained from `tf_tree::Open`, is a kernel fact.
  Two of the three paths §0.0's #205 row names are unaffected; **this one shrinks
  the row from three to two.**
- **One syscall per `reparent`, and one more on the way out.** D3 keeps `reparent`
  off the query path, so this is not a budget question. It is not free either, and
  the number is two `fcntl`s, not one.
- **The residual is stated rather than removed.** A writer with no lock file —
  heap (single-process, where `self.decl` is the whole exclusion and always was),
  a directly-called `build_shared`, `attach_shared` — still decides from the
  triple, and is still decided about by the triple. That is `0031`'s class and it
  is unchanged in both directions.
- **`self.decl` becomes marginally more load-bearing and is still belt and
  braces.** Two threads of one `Tree` share a lock-file description, so byte 1
  does not arbitrate between them; the `Mutex` does, and behind it
  `TopoLockView::acquire`'s `owner_slot == participant_slot` guard still refuses.
  Two `Tree`s in one process hold two descriptions and *do* conflict —
  `two_descriptions_in_one_process_still_conflict` is the existing pin for that
  property, and `concurrent_reparents_from_separate_attachments_are_serialized`
  now exercises it 128 times.
- **A new failure mode that did not exist, and it is far smaller than the
  obvious comparison makes it look.** If byte 1 is held by a forked child
  ([`0030`](./0030-the-atfork-handler-and-inherited-descriptors.md)'s hole,
  §6.2), `reparent` refuses for as long as that child lives, where before the
  triple might have authorised a steal. That is an **availability** loss in
  exchange for the corruption the same inheritance would otherwise permit, and
  `doctor`'s `TFT014` reports the inheritance.

  **It is not "the identical trade §6.1 took for claims", which is what an
  earlier revision of this bullet said.** A claim byte is held for the life of a
  `Publisher` — indefinitely, and by design — so a `fork` anywhere in a
  publishing process inherits a held byte, which is why §6.2 and §7.3 treat it as
  a first-class hazard there. The topology byte is held for the two `fcntl`s and
  one block copy of a single `reparent`. To inherit a *held* one, a process must
  `fork` from another thread while this one is inside that window **and** then
  die without leaving it. The window is microseconds against a call that
  `PROJECT.md` §5 D3 keeps off the query path entirely, so the two exposures
  differ by orders of magnitude and equating them overstates this one. The
  mechanism is real and is recorded; the risk is not comparable.
- `topo_lock.owner` is now a *diagnostic* on a lock-file tree — the byte is the
  lock and the word records who holds it, which is §6.1's "the record may lag;
  the lock never does" applied one lock over. It remains the *only* exclusion on
  a lock-file-less tree, so it is not demoted, and no code may read it as
  authoritative on either.

## Implementation plan, as landed

1. **`tf_tree_ipc`: byte 1.** `TOPOLOGY_OFFSET`, `LockRole::Topology`,
   `try_take_topology`, `release_topology`, and the §3.3 byte table in both the
   module doc and `PHASE2.md`. *Verified by:*
   `the_byte_layout_is_the_one_the_spec_tabulates` extended to assert the new
   offset is disjoint from ownership, every participant byte and the claim
   region — a collision there would be one integer meaning two things, which is
   `0035`'s failure one region over;
   `two_descriptions_contend_for_the_topology_byte_and_a_release_hands_it_over`;
   and `closing_a_description_releases_the_topology_byte`, which is the property
   the whole design rests on and is asserted for this byte rather than inherited
   from `dropping_the_file_releases_every_lock`.
   **No `probe_topology`**: nothing reads it yet, and an unused `pub fn` on a
   published crate is surface with no consumer.
2. **`tf_tree`: `TopologyLease` and the acquire order.** The guard is declared
   before the `TopoGuard` so the release order is the reverse, and it carries no
   `fork_gen` — unlike `ClaimLease` — because it is created and dropped inside
   one call on one thread. `Tree`'s `claim_lock` field is **renamed
   `lock_file`**: one description now carries both the claim leases and this
   byte, and a field spelled for one of its two roles is how a future change puts
   the topology byte on a second description without noticing that it must not.
   *Verified by:* the two tests in step 4, both with the mutant run.

   **The release *order* is not covered by a test, and that is stated rather
   than glossed.** It is not observable from outside the call: releasing
   byte-then-word leaves a window whose only symptom is a peer spinning out its
   budget and then asking `/proc` about a process that is merely finishing —
   a **spurious refusal**, not a safety failure, and indistinguishable from
   ordinary contention from any vantage point a test has. Catching it would need
   a `test-hooks` seam of the `CLAIM_WINDOW_HOOK` kind, and that machinery is not
   worth a liveness-only defect. What holds the order is scope: reverse the two
   `let`s and the drop order reverses with them.
3. **The §11.3 walk**, plus §5.1. `topo.holding_lock`'s row is rewritten for the
   three holder classes T2 leaves plus the fork inheritor, with P5's class named
   and why it needs no row of its own. §5.1 gains the NORMATIVE sentence that its
   word-before-byte rule binds a *probe* and not an *acquire*, and states T1/T2
   once — because a reader who has only §5.1 would otherwise read this change as
   a violation of it. D15 makes this the gate rather than a deliverable.
4. **The tests.** `a_live_holder_that_proc_calls_dead_keeps_the_topology_lock`
   stages the `Known(st) != stored` collision directly — a live joiner, byte 1
   held from a second description, and a `start_time` in its participant record
   that no longer matches the process — and asserts `reparent` refuses *and that
   the word did not change hands*, since a steal with a cosmetic error would
   otherwise pass. **The control is the same test with the byte released**, which
   steals, so the byte is the one variable. Its doc comment states what it is and
   is not: it stages the same *predicate input* a PID-namespace mismatch
   produces, by a different route, and it does not stage a PID namespace —
   `0033` is where that lives.

   `a_killed_topology_lock_holder_releases_its_byte_to_the_kernel` is the same
   property across a real process boundary, which a thread cannot stage: a helper
   (`tf_tree_rendezvous_child hold-topo`) takes the byte and is `SIGKILL`ed, and
   the mutation that was refused while it lived succeeds once it is gone, with
   nothing run on its behalf. It also pins the unnamed-holder path
   (`owner_slot: None`) — the holder has the byte and has published no slot —
   which is the state every mutator passes through between its two acquires.
   `a_contended_topology_lock_never_renders_a_sentinel_slot`
   (`crates/tf_tree/tests/behavior.rs`, and so in `just test` rather than only
   under `shm`) pins the message half: the named case reaches the reader, the
   unnamed case reaches them with **no digit in it at all**. Mutant run —
   restoring the sentinel to the `None` arm fails it.

   **Mutants run, not asserted.** Replacing the lease acquire with `None`:
   `a live holder was stolen from: Ok(())` and `a live holder of the topology
   byte did not refuse this mutation: Ok(())`, one per test.

### Why the lifecycle is compressed

`CLAUDE.md` sends a change the specs do not cover through a `draft` record first,
and this record has been `draft` since #213 was filed. What is compressed is
`ready` → `implemented`: the record went `ready` and landed in one change,
because the four steps above are not separable. Steps 1 and 2 without step 3
would ship a mutation-protocol change with no crash-matrix walk (D15), and step 4
is the only thing in the workspace that tells the old behaviour from the new.

## What this leaves open, and to whom

- **`0031` is untouched and still blocks its own question.** This record no longer
  depends on it. The dependency ran one way and now runs no way; `0031` needs
  nothing from here.
- **The follow-on that removes T3's residual** is a capability question, narrower
  than `0031`'s: *may a writer with no lock file mutate topology on a shared
  arena?* Answer it "no" and `participant_is_alive` really can be deleted,
  because T2's second disjunct becomes empty. `0028` plan step 0b already took the
  identical decision one call over — it refused `attach_shared(ReadWrite)` on
  exactly the "no lock file ⇒ no byte" ground, as a shipped breaking change — so
  the precedent is one record old. It is not taken here because nothing about
  #213 requires it and it is a breaking change on a public path.
- **P5 is still in place and this record still does not touch it.** The
  socket-hangup callback decides liveness from neither fact. It is correct under
  D17 and its incarnation guard bounds the damage. It is out of scope here for a
  reason that is now sharper than "D17 makes the socket authoritative for its own
  class": P5 reclaims a *participant record*, and after this change the topology
  lock does not consult participant records at all.
- **A `loom` model with a live holder a wrong predicate can steal from** was this
  record's third unmet gate item and is no longer owed by *this* change, per the
  `loom` section. It is still owed by anything that changes the *word* protocol,
  and the gap it names is real: no model in the suite has a live victim.

## Corrections this change makes to other documents

- **Three citations of "`0029` question 3" mean `0028` question 3.** They cite
  this record for *"whether the §3.5 heir should reuse its slot"*, which is
  `0028`'s question 3 and has been **RESOLVED since 2026-08-20** (the heir keeps
  its existing slot, byte and arena); two of the three additionally call it
  `draft`. It matters now because this record's questions are gone, so the
  citations would otherwise dangle into nothing. **Two of the three are fixed
  and the third deliberately is not**: `crates/tf_tree/src/open.rs` and
  `CHANGELOG.md` are corrected in place, and
  [`0035`](./0035-the-creators-slot-is-taken-not-found.md) is `implemented` and
  therefore frozen — the correction for it lives here and in
  `decisions/README.md`'s row, the same way `0035`'s own retraction lives in
  `PHASE2.md` §0.0 rather than in `0035`.
- **`concurrent_reparents_from_separate_attachments_are_serialized`'s doc comment
  said "only A2's in-arena lock can stop them colliding"**, which this change
  makes false. Updated to name both locks and which one does what.
- `0029` question 1's paragraph on `doctor` misdiagnosing a namespaced
  participant as a fork inheritor asked for the run to be retaken before it was
  cited as a measurement. That is superseded:
  [`0033`](./0033-the-identity-record-cannot-name-a-namespace.md) shipped
  `Identity::pid_ns_inode` and two `TFT014` guards, so the misdiagnosis is fixed
  and the request is moot. The measurement itself — the collision — survives in
  `0033`.

## Two things this deliberately does not build, for the operator's sake

Both were considered because the audience for this library is an engineer
running a multi-process robot stack, not a reader of this record. Both are
refused, and the reasons are here so the next person does not have to re-derive
them.

**No `TFT0xx` for the topology lock, and no `probe_topology` to feed one.** The
check would be cheap now — one `F_OFD_GETLK` on byte 1, where before this change
"is A2's lock held" was unanswerable without a `/proc` guess — and it is still
not worth having, because every state it could report is one an operator must not
act on. *Byte free, word non-zero* is a mutator that crashed mid-`reparent`, and
it **self-heals**: the next `reparent` spins out its budget and steals, and until
one happens the stale word affects nothing, because no lookup reads it. *Byte
held, word zero* is a healthy mutator two instructions into its own call.
*Byte held, word set* is a healthy mutator. The one genuinely actionable state —
a fork inheritor holding the byte for a dead parent — is already `TFT014`'s, has
the same remedy text, and needs the microsecond coincidence described in
*Consequences*. A check whose every verdict is "this is fine, or is somebody
else's check" is noise in a tool an operator reaches for when something is
already wrong. `PHASE5.md` §8 is the precedent for writing that down instead of
building it.

**No blocking or retrying `reparent`, and this one is a real gap that belongs to
another issue.** `ReparentError::LockContended` puts the retry loop on the
caller, and `concurrent_reparents_from_separate_attachments_are_serialized`
contains the loop every caller has to write — `match … Ok => break, LockContended
=> spin_loop(), other => panic`. A naive `reparent(…).unwrap()` therefore panics
in production under contention that is not a fault, and **that will reach a user
far more often than #213 ever did.** It is not fixed here because it is a new
public surface rather than a defect in this one — [`0018`](./0018-blocking-waits-belong-in-the-shim.md)
bears on where a wait may live, D17 on why a timeout is not the answer, and a
blocking acquire against a wedged holder would hang a robot where an error lets
it decide. That is a decision record, and this record naming it is not the same
as taking it.

## Appendix: what the `/proc` triple was measured doing

Kept from this record's open question 1, because the *Decision* rests on it and
because `0033` and `decisions/README.md` both cite it. Measured on
`dev-box-2026-01` (Ubuntu 24.04, kernel 6.8.0-136, util-linux 2.39.3,
`apparmor_restrict_unprivileged_userns=1`), unprivileged unless a line says
otherwise. **Not measured on a CI runner**, and every capability below is distro
policy rather than a kernel guarantee.

**A PID-namespace mismatch, staged with four words.** Bare `unshare --fork --pid`
is refused (`Operation not permitted`), and so is `unshare -Ur`. **`unshare -U
--fork --pid` succeeds**: creating the user namespace in the same call supplies
the capability the pid-namespace check wants, and the task keeps its host kuid
for permission checks, so `F_OFD_SETLK`, `shm_open`, `mmap(MAP_SHARED)` and
`connect()` against 0600 objects all work.

**It is a real participant, not a mimic.** `tf_tree_rendezvous_child` built at
`7739805` with `--features shm,unstable`: an owner and a control joiner in the
host namespace, a third joiner under `unshare -U --fork --kill-child --pid`. The
namespaced one completed the whole rendezvous — `SCM_RIGHTS`, slot assignment,
mapping — byte-identically to the control. Then, on that live arena:

```
tf_tree --attach top       ->  slot 2   pid 1   rw   live   yes
F_OFD_GETLK(byte 16 + 2)   ->  F_WRLCK                      # byte says ALIVE
tf_tree peer-alive 2       ->  alive true
read_start_time(1)         ->  Known(12)  vs stored 287088242
alive_given(...)           ->  false                        # triple says DEAD
```

`/proc/1` is `systemd`. That is #205's predicted **collision**, `Known(st) !=
stored` — not `ENOENT`-shaped, so the bias that makes everything else survivable
does not fire.

**Two preconditions, or the staging silently fails.** (a) `TF_TREE_RUNTIME_DIR`
must be named explicitly: the default is `/tmp/tf_tree-<getuid()>` and inside the
empty-map userns `getuid()` is 65534, so the joiner creates
`/tmp/tf_tree-65534` beside the owner's `/tmp/tf_tree-1000`. (b) The staging
passes `runtime_dir.rs`'s `meta.uid() != uid` gate only because an **empty**
`uid_map` collapses both `getuid()` and `stat()`'s uid to the overflow uid — so
"improving" the staging into a *mapped* namespace breaks it.

**`hidepid=2` is narrower than an earlier revision of this record said, and not
zero.** Same-user is not sufficient: a same-user but **non-dumpable** target is
hidden, because `has_pid_permissions` falls through to `ptrace_may_access`, which
fails on `dumpable == 0`. In a throwaway container with its own procfs:

```
/proc mounted hidepid=invisible
dumpable target:     pid=13 uid=1000 dumpable=1   -> VISIBLE
NON-dumpable target: pid=15 uid=1000 dumpable=0   -> HIDDEN
remount hidepid=off:  NON-dumpable same-user      -> VISIBLE
```

So a participant that drops privileges without a re-exec, or hardens itself with
`PR_SET_DUMPABLE(0)`, is hidden from a same-user reader. **Dumpability is a third
dependency, alongside §3.10's same-user rule and a shared PID namespace, and it
is stated nowhere.** `hidepid` cannot be staged unprivileged on this host in any
case: every `mount`/`remount` returns `EACCES` from AppArmor, including a fresh
procfs at a new mountpoint.

**A third staging needing nothing: an unreaped zombie.** `/proc/<pid>` survives
with `state=Z` and its start time intact, so the triple reads `Known(287080225)
== stored` → **alive** while the kernel has released the byte → `F_UNLCK` →
**dead**. Its **reaping parent must stay alive** — with the parent gone the
subreaper collects instantly and the discriminator vanishes. It is **not** a case
of `/proc` lying (`state=Z` is accurate, and `read_start_time` never reads field
3), which is why it is a weak argument that the byte is authoritative even though
it is a usable test fixture. **Pid reuse stays out**: `pid_max` is 4194304, a wrap
is ~630 s of fork storm and racy besides, and `ns_last_pid` needs `CAP_SYS_ADMIN`.

**Why the shipped test does not use any of these.** The staging that discriminates
old code from new is a *predicate input*, not a namespace: what makes the triple
say "dead" about a live holder is `Known(st) != stored`, and plan step 4 produces
that input directly by storing a stale `start_time` into the holder's participant
record. That runs on any host with no capability, no container and no subprocess,
and it is the same input the namespace collision above produces by a longer
route. The test's doc comment says exactly that, so nothing here is cited as
coverage of a PID namespace — `0033` is where that lives.

## Not in this record: #220

**#220 is not record material and should not become a second draft.** The
`Created | TookOver` arm called `builder.build_shared(...)`, so a taker-over
would have built a *new* arena rather than inherit the one it already has.
Checked at that call site, there was no fd, no socket path, no `Rendezvous` and
no way to take the session's arena in scope — **so the arm could not be fixed in
place under any answer to `0028` question 3.**

**Landed as `0028` plan step 9.** The arm is split: `OpenOutcome::Created` keeps
the `build_shared` and `OpenOutcome::TookOver` refuses with
`OpenError::TakeoverUnsupported` — a refusal rather than an adoption because
nothing at the match names the arena this process already holds. Restructuring
where the heir's arena comes from is still unbuilt.

`OpenOutcome::TookOver` **remains unconstructible through `tf_tree::Open`'s
public surface** — the builder has no `already_attached` setter, under no
feature, and step 9 deliberately did not add one. It is reachable from exactly
one place, a `#[cfg(test)]` field of the same name on that builder, which only
`tf_tree` compiled as its own test target can set;
`open::tests::a_takeover_refuses_rather_than_building_a_second_arena` is its only
user.
