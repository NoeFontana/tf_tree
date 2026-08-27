# 0037: a takeover is not a second `open()`

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none, and the change that prompted this record **removed**
code rather than adding any. `tf_tree_ipc::Open::already_attached` and the
takeover arm it reached are deleted (#275); `OpenOutcome::TookOver` survives with
no producer. This record is what a future §3.5 has to answer first.

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

## What a §3.5 takeover would have to be

**A method on the `Session` the heir already holds**, not a new `Open`. The
description that holds the participant byte is the one that must continue to hold
it, and only that object can assert so without asking the kernel a question it
cannot answer. Sketch, not a decision:

```rust
impl<A> Session<A> {
    /// Inherit the owner role, keeping this session's slot, byte and arena.
    fn take_over_ownership(&mut self) -> Result<(), IpcError>;
}
```

It would `try_take_ownership()` on the description it already has, unlink the
stale socket, and hand the caller its *existing* fd to serve. No registration, no
second slot, nothing to verify — the invariant holds by construction, which is
the property `0028` question 3's answer was reaching for.

## Open questions

1. **Where does the heir's arena come from?** `0029` notes this is still unbuilt:
   the facade's `TookOver` arm cannot adopt, because "nothing in scope at it names
   the arena this process already holds — no fd, no socket path, no `Rendezvous`
   of the session's own". A `Session` method has all three, which is a further
   argument for the shape above, but the facade seam still has to be designed.
2. **Who wins when two survivors race?** Both call the method; one gets the
   ownership byte. The loser must remain a plain participant with its slot
   intact — which the shape above gives for free and the deleted arm did not.
3. **Does `OpenOutcome::TookOver` survive?** It is the protocol's vocabulary and
   currently has no producer. If takeover becomes a `Session` method it may not
   need to be an *outcome* at all.
4. **What happens to `docs/PHASE2.md` §3.5 and `0005` step 5**, both of which
   describe the second-`open()` protocol that will not be built? `0028` question
   3 already flagged them; nothing has amended them. §3.4's NORMATIVE
   pseudo-code *has* been amended by #275, because it mandated the heir taking a
   participant byte.
5. **What is `Session::release_ownership` for now?** It is public, shipped, and
   §3.5's "give up the owner role while staying attached" — and with the takeover
   arm gone there is no route by which any survivor becomes owner. A fresh
   `open()` takes ownership at step 2, meets step 4 against the survivors' held
   bytes, releases, and times out into `ArenaHeldButUnreachable` for as long as
   any survivor lives. **That was already true of every caller not using the
   unsound arm**, so deletion did not cause it — it removed the last thing
   obscuring it. The rendezvous is ownerless until every participant leaves.
   Whether that makes `release_ownership` a footgun to remove, or the correct
   half of a pair whose other half is the `Session` method sketched above, is
   this record's question and not #275's.

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
