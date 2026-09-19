# 0045: the spin that can starve what it waits for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** *Implementation plan* below — four steps. **Decided
2026-09-19, on the owner's delegation**: both halves are taken, the yield is
`wait_for_publish`'s alone, and A8's invariant is untouched by the bound.

## Context

`FrameTable::wait_for_publish` is the handshake a name resolution goes through
when another participant is mid-intern. It ends in an unconditional `spin()`:

```rust
        }
        spin();
    }
}
```

`sync::spin` is `core::hint::spin_loop()` and nothing else — no `sched_yield`
anywhere in the ladder. `INTERN_SPIN_LIMIT`'s doc states the bound deliberately:

> The bound is a *liveness-poll interval*, not a timeout: a claimant that the
> predicate reports alive is waited on again, **without limit**.

That is amendment **A8**, and it is right about the case it was written for. A
claimant that dies is proven dead and its entry is taken over. What A8 did not
consider is a claimant that is **neither running nor dead**: `SIGSTOP`, a
debugger attached, a frozen cgroup, or a container paused mid-checkpoint. Its
participant record reads `LIVE`, `/proc` reports it, the OFD byte is held — every
predicate says *alive* — and it will not publish until something resumes it.

**Two consequences, and the second is worse than the first.**

*The wait is unbounded.* Both roles fall through to `spin()`; neither increments
a round counter on that path. `Tree::lookup`, `tft_plan_create` and Python's
`tree.plan()` all reach it, and none of their docs mentions a wait at all.

*The spin does not yield, so it can prevent the resume it is waiting for.* On a
shared core — a Jetson-class part, a `cpuset` with two threads, an
`isolcpus`-pinned RT consumer — a spinning waiter at higher priority stops the
claimant from being scheduled. The waiter is then the reason its own wait does
not end. That is priority inversion, and this is the only place in the codebase
that can produce it: every other spin in the engine waits on a **store** that a
running peer is a few instructions from making, while this one waits on a peer
that may not be running at all.

**`Tree::reparent` faced the same class and answered it differently.** A2's
topology lock is a kernel lock ([`0029`](./0029-the-topology-lock-is-a-kernel-lock.md)):
it stopped asking whether the holder is alive and took an OFD byte, so a
stopped holder blocks rather than being inferred about. The interning path still
infers, and still spins.

## Decision

**Two changes, and they are separable — the second is a protocol change and the
first is not.**

### 1. The spin yields (not a protocol change)

The waiter yields to the scheduler after a short pure-spin prefix. **How the
yield reaches `tf_tree_core` is a seam, not a feature**, and the draft's answer —
"the same shape `crash-points` already uses" — does not work: that shape is
sound only because nothing shipped enables `crash-points`. A yield behind a
default-off feature never reaches a `tf_tree` user and is compiled by no gate
(`cargo nextest run --workspace` builds default features); a yield behind a
default-**on** one puts `extern crate std` into every `tf_tree_core` in the graph
by unification, which is what `lib.rs`'s own comment argues against.

**So the facade installs it, the way it already installs liveness.**
`ArenaView::with_liveness` takes a predicate from `tf_tree`, which is `std`;
the yield travels the same seam. `tf_tree_core` keeps `#![no_std]`
unconditionally and pure-spins when no hook is installed — which is correct on a
bare-metal target, where there is no scheduler to yield to — and a shipped
`tf_tree` user gets the yield with no feature to enable and no `std` in the
core's dependency graph.

This changes nothing about *whether* the wait ends. It changes only whether the
waiter is holding the CPU the claimant needs. **A `no_std` build keeps the pure
spin**, which is correct there: a bare-metal target has no scheduler to yield to.

### 2. The wait is bounded (a protocol change — this is what needs deciding)

Both roles increment a round counter on **every** liveness round, not only the
`CLAIM_UNRECORDED`/`CLAIM_ANONYMOUS` ones, and return `Wait::Contended` past a
limit. `FrameError::InternContended` already exists and already reads *"another
interner holds the name's slot and cannot be judged"*, which is exactly the
state.

**This amends A8's "without limit", and that is why this is a record.** The
trade it makes: a caller can now be told *"someone holds this name and I cannot
say when they will finish"* instead of waiting for them. A control loop can act
on that; it cannot act on a spin.

## Rationale

**Why not just make the spin yield and stop there.** A yield fixes the
starvation, not the unboundedness: a stopped claimant on a machine with spare
cores still never publishes, and the waiter still never returns. The two failures
are independent, which is why they are separate steps rather than one.

**Why not take a kernel lock, as `0029` did for the topology byte.** The lock
file is a §3.3 resource with a fixed byte layout, and interning is per *name* —
there is no byte to take, and inventing one means a byte per hash slot. `0029`
worked because A2's lock is a single arena-wide word.

**Why `Wait::Contended` rather than a longer spin.** A limit that is merely
larger moves the failure rather than reporting it. What a caller needs is the
difference between *"this is taking a while"* and *"this will not finish without
intervention"*, and only a typed refusal carries that.

**Why the limit cannot be a duration.** `tf_tree_core` is `no_std` and has no
clock; D14's dependency budget is `libm` + `bytemuck` + `blake3`. A round count
is what this layer can express, and its calibration is the same kind of number
`INTERN_SPIN_LIMIT` already is.

## Consequences

- `Tree::lookup`, `tft_plan_create` and `tree.plan()` gain a failure mode they
  did not have. Their docs must say so — a call that could not fail and now can
  is a breaking change in behaviour even where the signature is unchanged.
  **Note (2026-09-14):** on `Tree::lookup` that failure does not arrive as
  `FrameError::InternContended`. The facade's resolver (`find` in
  `crates/tf_tree/src/tree.rs`) maps every `find_frame` error onto
  `LookupError::UnknownFrame { hash }`, so the contention step 2 makes reachable
  reaches a Rust caller disguised as an undeclared name. `Tree::lookup`'s
  `# Errors` now says so and names a write-free way to tell the cases apart; a
  distinct variant or a cause field would be a record of its own.
- A8's text changes, so `docs/PHASE2.md` §1 needs an amendment recorded the way
  §3.5's was rather than an edit.
- The `loom` models that exercise interning gain a reachable `Contended` arm.
  `INTERN_SPIN_LIMIT` is already 2 under `loom` for interleaving reasons; the new
  bound needs the same treatment and its own control, because a model where the
  bound is never reached tests nothing.
- **A yielding spin is measurable and must be measured.** It is on the
  name-resolution path, which `Tree::lookup` takes on a cache miss.
  `just bench-check` is the gate, and a regression there is a reason to make the
  pure-spin prefix longer rather than to drop the yield.

## Implementation plan

1. **`spin` splits, and the yield arrives through the facade's seam** (question
   3 and *Decision* §1). `sync::spin` is unchanged and stays pure for the four
   store-waiters; a second function, used by `frame::wait_for_publish` alone,
   calls an installed yield hook after a pure-spin prefix and pure-spins when
   none is installed. `tf_tree` installs it. Both functions keep yielding under
   `cfg(loom)`, or the models stop scheduling the thread they wait on.
   - **Verified by** `just bench-check` against the committed baseline, reported
     rather than assumed; by `just loom`; by `just stable-tier-check` and the
     `no_std` build, which must still compile with no hook; and by the existing
     frame tests passing unchanged.
   - **Two stop points, because the first is only half a check.** If
     `bench-check` moves on a path that does *not* reach `wait_for_publish`, the
     yield leaked into `spin`. And **a test must prove the hook is actually
     installed on a `tf_tree` tree** — a default-off feature or an uninstalled
     hook leaves the yield unreachable for every shipped user while every gate
     stays green, which is the failure mode `bench-check` structurally cannot
     see.
2. Both roles count every liveness round; past the limit, `Wait::Contended` →
   `FrameError::InternContended`. **Report the measured worst-case intern
   duration and derive N from question 2's rule** — the number is not to be
   assumed.
   - **The existing control must be rewritten, and saying it "keeps passing" was
     wrong.** `a_claimant_that_cannot_be_proven_dead_is_never_stolen_from` stages
     exactly this shape and asserts the waiter is *still blocked after 250 ms*
     — `rx.recv_timeout(250ms).is_err()`. Step 2 makes that false by design at
     any N under the ceiling, and the assertion's message would then misdiagnose
     the refusal: *"an unproven claimant was stolen from"* is what it prints, and
     a `Contended` return steals nothing.

     **The property survives; the proxy does not.** "Never stolen from" must be
     asserted **structurally** — `claiming[i]` still names the original claimant
     and no id was published — rather than inferred from continued blocking.
     Rewritten that way it holds under any N, which is what lets N be actionable
     instead of being forced above 250 ms.

     *The draft reconciled the two with "a limit large enough not to fire", which
     is a real constraint (N ≳ 625) and contradicts this record's ceiling. This
     step deleted that clause and asserted the test would pass unchanged, which
     is false; the resolution is to fix the test's mechanism, not to inflate N.*
   - **Verified by** that rewritten control, plus a new test staging a claimant
     that reads alive and never publishes and asserting `InternContended` inside
     the bound.
3. `docs/PHASE2.md` §1's A8 gains the amendment; `Tree::lookup`, the C entry
   point and the Python method document the new refusal.

   **And `FrameError::InternContended`'s own prose is wrong for the new
   producer, in three places that ship.** Its variant doc says it is raised when
   the claimant is an **anonymous** view and is "actionable: identify the view";
   its `Display` says "another process may have died mid-intern";
   `LookupError::UnknownFrame`, which is what a Rust caller actually sees, is
   documented as **transient**. For a `SIGSTOP`ped claimant all three are false —
   it is identified, it is alive, and nothing about it is transient.

   **Worse, `tf_tree_py`'s error docs name this variant in the *retry loop***
   alongside `SlotContended` and `LeaseContended`. A control loop following the
   shipped guidance retries forever against a stopped claimant, which relocates
   the unbounded wait into the caller rather than ending it — defeating the
   purpose of step 2. So step 3 covers the variant doc, the `Display`,
   `intern_core`'s `# Errors` (which does not mention the variant today) and the
   Python retry guidance.

   **The alternative, costed rather than taken:** a distinct `Copy` identifier
   for "the claimant will not progress", which is what D11 would suggest if the
   two causes need different handling. It is new public surface on a published
   crate and `API.md` §7's checklist; this record takes the cheaper route and
   names it so a reopening starts from the cost.
4. A `loom` model reaches the bound, with a control that fails when the bound is
   unreachable.

## Open questions

**All three answered 2026-09-19, on the owner's delegation.** The questions are
kept in full below rather than collapsed, because a reopening needs the framing;
each answer says where the evidence is.

1. **ANSWERED: yes — and the amendment does not touch A8's invariant, because
   A8 constrains *takeover*, not *waiting*.**

   A8 is titled *"Interning must not spin forever on a **dead** claimant"*, and
   its body is entirely about a claimant that dies between the hash CAS and the
   id store — the crash point `intern.after_hash_cas_before_id_store`. Its fix is
   *record the claimant, bound the spin, take the entry over if the claimant is
   dead*, and the safety property it states is about `is_alive` failing safe:
   **"a slow interner is never stolen from."**

   Returning `Wait::Contended` to the caller **steals nothing**. The entry stays
   with its claimant, `claiming[i]` is not CASed, no record is written, and a
   slow interner is still never stolen from. So A8's invariant survives the bound
   exactly, and what changes is a caller-visible refusal A8 never spoke to.

   **The "without limit" sentence is not A8's.** It is `INTERN_SPIN_LIMIT`'s doc
   comment in `crates/tf_tree_core/src/frame.rs` — an implementation note
   describing the code A8 produced, not normative amendment text. This record's
   draft attributed it to A8 and then hedged that the reading was its own; the
   hedge is withdrawn because the distinction is checkable in the two documents.

   What §1 still owes is an **amendment recorded the way §3.5's was** — the note
   that a claimant which is neither running nor dead is a class A8 did not
   consider, and that the wait is now bounded while the takeover rule is not.
   That is step 3 and it is a docs change, not a re-decision.

   ~~May A8's "without limit" be amended at all, or is unbounded waiting load
   bearing for something this record has not found?~~ A8 is one of the eight
   Phase 1 amendments `docs/PHASE2.md` §1 holds, and `CLAUDE.md` names that
   section as the reason several orderings look odd. The argument here is that
   the unbounded case was written for a claimant that is *running*, and a stopped
   one is a different class — but that is this record's reading of A8, not A8's
   own statement.
2. **ANSWERED as a rule, not as a number — and step 2 owes the measurement the
   rule consumes.** Two constraints bound it from opposite sides, and between
   them the number is not delicate:

   - **Unreachable in health, by orders of magnitude.** A successful intern is a
     hash-slot CAS, a record write and a release store — sub-microsecond. One
     round of `INTERN_SPIN_LIMIT` is 10 000 pure spins, ~0.4 ms at 3 GHz, which
     already exceeds a healthy intern by roughly three orders of magnitude. Any
     N ≥ 2 is therefore unreachable by a claimant that is merely slow.
   - **Actionable by a control loop.** A robot loop runs at 100–1000 Hz, so a
     refusal has to arrive inside a period to be worth anything. That puts
     N × `INTERN_SPIN_LIMIT` in **single-digit milliseconds** — N of about 8.

   **The floor is structural, not a duration, and a first version of this answer
   missed it.** `READER_UNRECORDED_ROUNDS` is 4: a reader waits that many rounds
   on a `CLAIM_UNRECORDED` slot before concluding the name is absent, because one
   round made `find_frame` report a live in-flight name as missing. A global
   bound of N ≤ 4 fires **first** and answers `InternContended` where the correct
   answer is *not there* — reintroducing the false negative that constant exists
   to prevent. So **N > `READER_UNRECORDED_ROUNDS`**, and the two constants move
   together: whoever changes one must re-derive the other.

   **The rule, which is what this record fixes:**

   - **Floor:** N > `READER_UNRECORDED_ROUNDS`, so the reader's abandon path
     still fires before the global bound.
   - **Ceiling:** N × `INTERN_SPIN_LIMIT` inside one control-loop period.
   - Both hold at **N = 8**, which is the value to implement unless the
     measurement moves the ceiling.

   *An earlier version of this answer read "the smallest value whose product …
   exceeds the measured intern by two orders of magnitude and stays under 10 ms".
   That has a floor a single round already clears, so it yields **N = 1** — below
   `READER_UNRECORDED_ROUNDS`, which is the one value that must not be chosen. It
   stated the "N of about 8" conclusion beside a rule that contradicts it.*

   **Step 2 must still report the measured worst-case intern**, on **both**
   architectures this project gates — `spin_loop()` is a `pause` of ~140 cycles
   on recent x86-64 and an `isb` of tens of cycles on aarch64, so the same
   iteration count is several times shorter there and the "~0.4 ms at 3 GHz"
   figure below is x86-specific. A ceiling expressed in milliseconds and derived
   from an x86 count loses most of its headroom on the `ubuntu-24.04-arm` row and
   on a Jetson.

   ~~What is the limit?~~ `INTERN_SPIN_LIMIT` is 10 000 pure-spin iterations
   between liveness checks (~0.4 ms at 3 GHz). A round bound of *N* liveness
   checks is therefore *N* × that, and picking *N* means deciding how long a
   control loop should wait before being told it cannot proceed. It wants a
   measurement of how long a real intern takes, which nothing in the repository
   currently records.
3. **ANSWERED by counting the callers: only here, and `spin` splits.**
   `sync::spin` has five production call sites in `tf_tree_core`, and they fall
   into two classes with nothing in between:

   | call site | waits on |
   |---|---|
   | `buffer::read_slot` (seqlock retry) | a writer's release store, a few instructions away |
   | `plan.rs`'s two generation retries | a topology mutator's store |
   | `topology.rs`'s A2 acquire | the lock word, bounded |
   | **`frame::wait_for_publish`** | **a peer that may not be running at all** |

   Four of the five wait on a store a *running* peer is about to make, and
   yielding in those trades a sub-microsecond wait for a scheduler round trip.
   `buffer::read_slot` is the hot read path, so that is not a theoretical cost.
   Exactly one waits on a peer whose scheduling is the thing in question.

   So `spin` stays pure for the four and a second function — yielding under the
   std-backed arm — is `wait_for_publish`'s alone, with each call site naming
   which it wants. **The `loom` arm is unaffected**: it already yields for all
   five, for interleaving rather than starvation, and that must stay true of both
   functions or the models stop scheduling the thread they wait on.

   ~~Does the yield belong in `sync::spin` for every caller, or only here?~~ The
   seqlock retry in `buffer::read_slot` and A2's acquire spin both wait on a peer
   that is a few instructions from a store; yielding there would trade a
   sub-microsecond wait for a scheduler round trip. If the answer is "only here",
   `spin` splits into two functions and each call site states which it wants.
