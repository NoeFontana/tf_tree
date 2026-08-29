# 0045: the spin that can starve what it waits for

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** —

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

`sync::spin` gains a std-backed arm that calls `std::thread::yield_now` after a
short pure-spin prefix, on the same shape `crash-points` already uses: the
feature pulls `std` in **for itself** via an `extern crate std` under its own
`cfg`, rather than through a `std` feature a default build could enable by
unification. `#![no_std]` on the crate root stays unconditional.

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

1. `sync::spin` yields under the std-backed arm, pure-spins otherwise. Verified
   by `just bench-check` against the committed baseline, reported rather than
   assumed, and by the existing frame tests passing unchanged.
2. Both roles count every liveness round; past the limit, `Wait::Contended` →
   `FrameError::InternContended`. Verified by a test that stages a claimant which
   reads alive and never publishes — the existing suite already stages exactly
   that shape in `a_claimant_that_cannot_be_proven_dead_is_never_stolen_from`,
   which currently asserts the *opposite* property and must keep passing with a
   limit large enough not to fire.
3. `docs/PHASE2.md` §1's A8 gains the amendment; `Tree::lookup`, the C entry
   point and the Python method document the new refusal.
4. A `loom` model reaches the bound, with a control that fails when the bound is
   unreachable.

## Open questions

1. **May A8's "without limit" be amended at all, or is unbounded waiting load
   bearing for something this record has not found?** A8 is one of the eight
   Phase 1 amendments `docs/PHASE2.md` §1 holds, and `CLAUDE.md` names that
   section as the reason several orderings look odd. The argument here is that
   the unbounded case was written for a claimant that is *running*, and a stopped
   one is a different class — but that is this record's reading of A8, not A8's
   own statement.
2. **What is the limit?** `INTERN_SPIN_LIMIT` is 10 000 pure-spin iterations
   between liveness checks (~0.4 ms at 3 GHz). A round bound of *N* liveness
   checks is therefore *N* × that, and picking *N* means deciding how long a
   control loop should wait before being told it cannot proceed. It wants a
   measurement of how long a real intern takes, which nothing in the repository
   currently records.
3. **Does the yield belong in `sync::spin` for every caller, or only here?** The
   seqlock retry in `buffer::read_slot` and A2's acquire spin both wait on a peer
   that is a few instructions from a store; yielding there would trade a
   sub-microsecond wait for a scheduler round trip. If the answer is "only here",
   `spin` splits into two functions and each call site states which it wants.
