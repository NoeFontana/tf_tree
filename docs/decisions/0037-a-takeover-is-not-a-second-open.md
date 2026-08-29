# 0037: a takeover is not a second `open()`

**Status:** implemented
**Owner:** @NoeFontana
**Implementation:** #275 (the deletion) and the §3.5 commit on
`feat/sota-runtime-hardening` (the replacement). The change that prompted this
record **removed** code rather than adding any — `tf_tree_ipc::Open::already_attached`
and the takeover arm it reached are deleted — and this record is what the
replacement was built from.

## Context

Issue #201: on two paths a process held lock byte *i* and arena participant
record *j* with `i != j`, and every liveness predicate indexes the two with one
integer (`docs/PHASE2.md` §5.1). A live participant then reads as dead, which
§6.2 calls the corrupting direction.

[`0035`](./0035-the-creators-slot-is-taken-not-found.md) closed the creator path
and deferred the other, believing the arm's correct slot was an open question. It
was not — [`0028`](./0028-the-slot-a-killed-participant-keeps.md) question 3 had
resolved on 2026-08-20 — and `0035` cited it as "`0029` question 3", a
misdirection `0029` corrected on 2026-08-25 while `0035` was already frozen.

So the arm was approached as a bug with a known answer. **It is not. It is a
protocol that cannot be expressed as an `Open::open` call**, which `0028`
question 3 says in as many words:

> The cost is real and is accepted: §3.5 cannot be wired as "call `Open::open`
> again with `already_attached(true)`" … the existing takeover arm ends with no
> caller

That sentence was read as *"nothing calls it yet"* and is really *"nothing can"*.

## Why it cannot be an `Open::open` call

**A new file description cannot verify a claim about the caller's own locks.**

The arm's contract was *"I already hold the arena at slot n"*, and every attempt
to make it safe needed to check that. `Open::open` builds its own `LockFile`, and
from a fresh description `F_OFD_GETLK` answers *"does anyone **else** hold this
byte"* — `crates/tf_tree_ipc/src/lockfile.rs`'s module doc states it twice, the
second time calling it "a trap for any future code that tries to read back its
own state".

So the probe cannot distinguish:

| | |
|---|---|
| the caller holds byte *n* on another description | **the declaration is true** |
| a live **peer** holds byte *n* | the declaration is a lie, and accepting it hands the caller a session naming the peer's slot |

Both return `held = true`. The check that looks like it verifies the
precondition verifies something else.

## What two rounds of repair produced

Recorded because the list is the argument, and each was **executed** rather than
derived:

1. `register_any` handed back the first *free* byte — `outcome=TookOver
   session slot=0` against a caller whose arena record was 5. The original #201
   defect.
2. Returning the caller's declared slot with no check — `already_attached_at(0)`
   from a process holding nothing gave `TookOver slot=0` over a **free** byte,
   which §0.0 calls the class that reads dead to every probe-carrying observer.
3. No range check — `already_attached_at(u32::MAX)` returned
   `Ok(4294967295)` through public API, to a caller that indexes a 64-record
   table with it.
4. A **serving** owner overrode the declaration — `already_attached_at(5)`
   against a bound owner returned `Joined slot=1`, reproducing #201 on the join
   path, in the §3.5 race the arm exists for.
5. Honouring the declaration on the join path stranded the owner's grant: the
   assigner's `granted` bitmask is cleared only on hangup, so a declared join
   holds a slot reserved-but-unusable for the life of the connection. Sixty-four
   of them wedge an arena with 64 free bytes and 64 `FREE` records.

Four of the five were introduced *while fixing* the one before it. That is the
evidence for this record existing rather than a sixth patch.

## Decision: a takeover is a method on the session that already holds the byte

**A method on the `Session` the heir already holds**, not a new `Open`. The
description that holds the participant byte is the one that must continue to hold
it, and only that object can assert so without asking the kernel a question it
cannot answer. This was written as a sketch and is now what shipped:

```rust
impl<A> Session<A> {
    /// Inherit the owner role, keeping this session's slot, byte and arena.
    fn take_over_ownership(&mut self) -> Result<(), IpcError>;
}
```

It `try_take_ownership()`s on the description it already has and hands the caller
its *existing* fd to serve. No registration, no second slot, nothing to verify —
the invariant holds by construction, which is the property `0028` question 3's
answer was reaching for. (The stale socket needs no separate unlink:
`OwnerServer::bind_at` already binds a pid-suffixed temporary and `rename`s it
into place, so a leftover path from a dead owner is replaced atomically and a
client sees either the old socket or a fully-listening new one.)

**§3.5's own pseudo-code was right the whole time, and is worth saying plainly
because it reframes what #275 deleted.** Its algorithm — *"try `F_OFD_SETLK`(byte
0); acquired → unlink stale sock; bind; listen; serve OUR existing fd"* — is
exactly what the shipped method and its facade seam do. What was wrong was never
the protocol; it was reaching the protocol through a second `open()`, which is
the one route that cannot establish the precondition the protocol assumes. The
five unsound states above are all consequences of the plumbing, not of the
algorithm.

### The half nobody had noticed was missing

§3.5 opens *"When a participant's client socket reports `HUP`"*. **Nothing
watched that socket.** So even while a takeover arm existed, no participant ever
*called* it — the arm had no caller, which was read as "not wired up yet" and was
really "there is no trigger". `tf_tree_ipc::peer_hung_up` and
`tf_tree::Tree::owner_lost` are that trigger, and they are the reason this record
ships a working §3.5 rather than a better-shaped unreachable one.

The trigger is **caller-driven, deliberately**: a survivor evaluates
`owner_lost()` in its own loop, and there is no background thread and no daemon.
`0019` holds that every process a user is *required* to run is a place adoption
dies, and a thread per attachment is the library-shaped version of that cost. The
consequence is stated rather than hidden: **a fleet whose survivors never call it
stays ownerless**, exactly as it does today. What changes is that a survivor which
*does* call it can now rescue the arena, where before nothing could.

## Open questions

**None. All five are answered, and three of them were answered by building it.**
They are kept with their answers rather than deleted, because two of the five had
a *different* answer in the sketch than they turned out to have in the code.

1. **Where does the heir's arena come from?** — *Answered by the facade seam.*
   `0029` recorded that the old `TookOver` arm "names the arena this process
   already holds — no fd, no socket path, no `Rendezvous` of the session's own".
   Two of the three were already in reach of a joined `Tree`: the mapping and its
   segment fd (`Tree::shared_fd`). **The third genuinely was missing** —
   `Attachment::Joined` parked the session and the socket and dropped the
   `Rendezvous` on the floor — so `Tree::inherit_ownership` could not have bound
   anything. It retains it now, which is the whole of what this question was
   asking for.

2. **Who wins when two survivors race?** — *Answered as sketched, and it cost
   nothing.* Both call the method; one wins the uncontended `F_OFD_SETLK`. The
   loser gets `Ok(false)` → `Inheritance::Contended` **with its slot intact**,
   because taking the lock on the description it already holds cannot move the
   slot. There is no arbitration, no retry protocol and no shared state between
   the candidates.

3. **Does `OpenOutcome::TookOver` survive?** — **No.** The sketch left this open
   ("it may not need to be an *outcome* at all") and building it settles it:
   takeover is not an outcome of `open()`, because `open()` is not how a takeover
   happens. The variant had no producer before this change and has none after it,
   and keeping vocabulary for a state the protocol cannot reach is the kind of
   half-truth that made `0035` cite a resolved question as open. It goes, along
   with `tf_tree::OpenError::TakeoverUnsupported` and the refusal arm that
   returned it — an arm whose own doc comment explained at length what would
   happen in a case that could not arise.

4. **What happens to `docs/PHASE2.md` §3.5 and `0005` step 5?** — *§3.5 is
   amended to describe what ships.* The checked note below this list found the
   asymmetry: §3.4's pseudo-code had been amended by #275 and §3.5's had not, so
   a reader of the section met a deleted protocol. The resolution is not the one
   the question anticipated — §3.5's **algorithm was correct** (see the Decision
   above); what it lacked was an implementation and a trigger. So it is amended
   for its plumbing and its trigger, and its strongest claim is restated
   unchanged, because it is still true: **lookups do not stop, slow down, or
   observe anything during a takeover.**

5. **What is `Session::release_ownership` for now?** — *It is the correct half of
   a pair, and now it has the other half.* The question offered two readings: a
   footgun to remove, or half of a pair whose other half is the sketched method.
   The second. `release_ownership` gives up the role while staying attached;
   `take_over_ownership` inherits it while staying attached. Neither moves a slot.
   The state the question described as the reason for doubt — "the rendezvous is
   ownerless until every participant leaves" — was never caused by
   `release_ownership`; it was caused by there being no way back, and there is one
   now.

## The `0028` step 9 obligation, and why it is not triggered

The section below records that `0028` plan step 9's verification — *"a unit test
that the `TookOver` arm returns an error rather than a `Tree`"* — was retired
with the arm, and that it **becomes owed again the moment this record gives
`TookOver` a producer**. It does not. Answer 3 removes the variant.

The hazard step 9 guarded is real and outlives the variant, so it is worth
stating what now prevents it. The danger was an heir that reached
`build_shared` and stood up a **new, empty segment** under the rendezvous name
every survivor was still mapped through — a forked arena, with the survivors
reading one set of bytes and every later joiner another. In the shipped shape
that is not reachable, and not because anything checks for it: the heir never
constructs an arena at all. It keeps the mapping it already had, and
`Tree::inherit_ownership` hands `spawn_owner_server` the very `Tree` it was
called on. There is no code path from inheriting to creating, so there is no
state for a guard to guard.

That is the difference between a refusal and a structural impossibility, and it
is the same difference the Decision above draws about the slot.

## What the deletion costs, recorded rather than absorbed

**[`0028`](./0028-the-slot-a-killed-participant-keeps.md) plan step 9's
verification is retired.** That step split the `Created | TookOver` arm so the
`TookOver` half refuses rather than `build_shared`-ing a fresh segment — an heir
on that path would hold a new, empty arena under the rendezvous name every
survivor is still mapped through — and required *"a unit test that the `TookOver`
arm returns an error rather than a `Tree`"*, reached "through `tf_tree_ipc` or
through a `#[cfg(test)]` seam, and must say which".

It said which: the seam. #275 deleted the only producer of `TookOver`, so the
seam had nothing to set and went with it. **The refusal remains and can no longer
be exercised.**

That is a real loss of coverage on a guard `0028` argued for at length, and it is
accepted for one reason: the state it guards against is now unreachable, so the
test would assert about a state the code cannot enter. **It becomes owed again
the moment this record gives `TookOver` a producer** — and whoever does that owes
it before the producer lands, not after, because the refusal is what stands
between an heir and a forked tree.

**Two more verifications went with it, and both are recorded here rather than
noticed later.** [`0029`](./0029-the-topology-lock-is-a-kernel-lock.md) states
that `TookOver` "is reachable from exactly one place, a `#[cfg(test)]` field" and
names that unit test as "its only user" — both gone, and `0029` is `implemented`
and frozen, so this is the correction. And
`crates/tf_tree_ipc/tests/multiprocess.rs`'s `#201` stress test records its
mutant as *"replace `register_creator` with `register_any` in step 5"*;
`register_any` is deleted, so that recipe cannot be run. The test still guards
`0035`'s fix; what is lost is the documented way to show it is not vacuous.

The arm itself is deliberately *not* replaced with `unreachable!`: `TookOver` is
a `pub` variant of a published crate's enum, and a panic is the wrong shape for
the day it comes back.

## Not in this record

**Any claim that §3.5 is scheduled.** It is not, and this record does not
schedule it. What it does is stop the next person from re-deriving five unsound
states before reaching the same conclusion.
