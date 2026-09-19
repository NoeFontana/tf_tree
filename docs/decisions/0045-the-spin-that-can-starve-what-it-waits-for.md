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

**`Tree::reparent` faced the same class and answered it differently — at the
facade, and only there.** A2's topology lock became a kernel lock at the
`Tree::reparent` layer ([`0029`](./0029-the-topology-lock-is-a-kernel-lock.md)):
it takes §3.3's byte 1 *before* the arena word, so a stopped holder blocks rather
than being inferred about. **The in-arena `TopoLockView::acquire` still spins
`TOPO_LOCK_SPIN_LIMIT` and then calls `is_alive(owner_slot)` and steals** — the
byte narrows what that inference may authorise, it does not remove it, and
`PHASE2.md` §0.0 records the path as closed only for a tree that *has* a lock
file. *This paragraph said A2 "stopped asking whether the holder is alive",
which is true of `reparent`'s acquisition order and false of the core's.* The
interning path has no byte at all, and still spins.

## Decision

**Two changes, and they are separable — the second is a protocol change and the
first is not.**

### 1. The spin yields (not a protocol change)

The waiter yields to the scheduler after a short pure-spin prefix. **How the
yield reaches `tf_tree_core` is a seam, not a feature**, and the draft's answer —
"the same shape `crash-points` already uses" — does not work: that shape is
sound only because nothing shipped enables `crash-points`. A yield behind a
default-off feature never reaches a `tf_tree` user; a yield behind a
default-**on** one puts `extern crate std` into every `tf_tree_core` in the graph
by unification, which is what `lib.rs`'s own comment argues against.

**So the facade installs it, the way it already installs liveness.**
`ArenaView::with_liveness` takes a predicate from `tf_tree`, which is `std`;
the yield travels the same seam. `tf_tree_core` keeps `#![no_std]`
unconditionally and pure-spins when no hook is installed — which is correct on a
bare-metal target, where there is no scheduler to yield to — and a shipped
`tf_tree` user gets the yield with no feature to enable and no `std` in the
core's dependency graph.

**Its cost, stated because step 3 costs the alternative on the same grounds.**
`InternTable`, `intern_core` and `find_core` are all `pub` in `tf_tree_core`,
which is one of the five publishing crates, so threading a hook to
`wait_for_publish` is **new public surface** and owes `API.md` §7's checklist and
a `0.0.x` break. Both routes cost surface; this one is chosen because the
feature route's cost is *unsoundness* — a yield no shipped user reaches, or
`extern crate std` unified into every `tf_tree_core` — while this one's is a
reviewable API change. *An earlier version presented the seam as the cheap
option and priced only the alternative.*

*And one of the three arguments against the feature route was wrong:* "compiled
by no gate" is false — `just test` runs
`cargo nextest run -p tf_tree_core --features crash-points` and `just lint`
carries two clippy passes over it. The `--workspace` parenthetical justifies one
command, not the recipe. The other two legs stand and are what decide it.

This changes nothing about *whether* the wait ends. It changes only whether the
waiter is holding the CPU the claimant needs. **A `no_std` build keeps the pure
spin**, which is correct there: a bare-metal target has no scheduler to yield to.

### 2. The wait is bounded (a protocol change — this is what needs deciding)

Both roles increment a round counter on **every** liveness round, not only the
`CLAIM_UNRECORDED`/`CLAIM_ANONYMOUS` ones, and return `Wait::Contended` past a
limit. `FrameError::InternContended` already exists and already reads *"another
interner holds the name's slot and cannot be judged"*, which is exactly the
state.

**This does not amend A8's text, and question 1 is where that is established.**
A8 constrains *takeover* — "a slow interner is never stolen from" — and a
refusal steals nothing; the "without limit" sentence is `INTERN_SPIN_LIMIT`'s
doc comment, not amendment text. What §1 owes is an **added note** that a
claimant which is neither running nor dead is a class A8 did not consider.
*This read "This amends A8's 'without limit', and that is why this is a record",
which question 1 now withdraws.* The
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

- **`Tree::await_frames` is the one shipped caller the bound actively breaks, and
  fixing it is what makes the bound coherent.** It maps *every* `find_frame`
  error to `AwaitError::Frame(e)` and returns immediately — "Terminal, not
  retried" — on the reasoning that such an error "will not change on its own".
  That is exactly false for the new producer, which resumes on `SIGCONT`. A
  consumer calling `await_frames(["odom"], 5s)` waits today and, after step 2,
  would be refused at ~3 ms with its own five-second budget discarded: a
  behaviour break on the stable tier of a published crate.

  **The resolution is the separation the bound exists to create.**
  `find_frame` stops waiting unboundedly and says *"someone holds this and I
  cannot say when"*; the layer that **owns a deadline** decides how long to keep
  asking. So `await_frames` retries `InternContended` within its own timeout and
  returns it only when that timeout expires. The core answers, the caller with a
  clock waits — which is the shape the record wanted and did not notice it was
  breaking.

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
- **A8's text does not change** (question 1): the bound constrains waiting and
  A8 constrains takeover. `docs/PHASE2.md` §1 gains a **note** recorded the way
  §3.5's amendment was — that a claimant neither running nor dead is a class A8
  did not consider — rather than an edit to A8. *This bullet read "A8's text
  changes".*
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
   none is installed. **The prefix is per liveness round, not per wait** —
   `wait_for_publish` resets its spin counter at every round, so the per-round
   reading re-spends the prefix at each boundary and enters the scheduler **less**
   often, by up to `INTERN_SPIN_LIMIT`×, than a per-wait prefix spent once after
   which every iteration yields. *An earlier version had that direction
   backwards, which would send an implementer chasing a `bench-check` regression
   toward up to 10 000× more scheduler entries on the name-resolution path.* Consequences names the prefix as the knob to turn
   on a `bench-check` regression, so which one it is has to be stated. `tf_tree` installs it. Both functions keep yielding under
   `cfg(loom)`, or the models stop scheduling the thread they wait on.
   - **Verified by** `just bench-check` against the committed baseline, reported
     rather than assumed; by `just loom`; and by the existing frame tests passing
     unchanged.
   - **Two different things, and an earlier version of this step conflated
     them.** *No hook installed* is a **runtime** state, and it is already
     exercised: every `tf_tree_core` unit test builds an `ArenaView` with no
     facade, and `just test` runs them. What has **no gate** is a `no_std`
     compile of `tf_tree_core` — `stable-tier-check` compiles `-p tf_tree` and runs `tf_tree_ingest` and `tf_tree_bridge`, none of them `tf_tree_core` `no_std`,
     the closest pass is
     `clippy -p tf_tree_core --no-default-features --features crash-points`
     which pulls `extern crate std` in through that feature, and there is no
     bare-metal cross-compile anywhere in the justfile. Step 1 owes that compile
     gate; it would not have caught the runtime case the earlier text named.
   - **A hosted direct consumer of `tf_tree_core` keeps the unbounded,
     non-yielding spin.** It is one of the five publishing crates, so this is a
     real population, and the seam gives them nothing unless they install a hook
     themselves. That is a consequence of choosing the seam over a feature and is
     recorded rather than hidden: the record's subject is a starvation the
     facade's users stop having.
   - **Two stop points, because the first is only half a check.** If
     `bench-check` moves on a path that does *not* reach `wait_for_publish`, the
     yield leaked into `spin`. And **a test must prove the hook is actually
     installed on a `tf_tree` tree** — a default-off feature or an uninstalled
     hook leaves the yield unreachable for every shipped user while every gate
     stays green, which is the failure mode `bench-check` structurally cannot
     see.
2. Both roles count every liveness round; past the limit, `Wait::Contended` →
   `FrameError::InternContended`. **Report the measured round cost — the spin *and*
   the probe syscalls — on both gated architectures, and check N = 8 against it.**

   *An earlier version said "derive N from question 2's rule", which is
   unfalsifiable as written*: the floor is a constant and the ceiling a
   control-loop rate, so no measurement outcome can move N. What the measurement
   *can* do is falsify the **premises** — if a round costs far more than ~0.4 ms
   because the `/proc` fallback dominates, the ceiling is breached at N = 8 and N
   must come down toward the floor; if the healthy-intern margin is under two
   orders of magnitude, the floor's justification is wrong. Those are the
   outcomes step 2 reports against.
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

     **And the timeout is not the only assertion the bound kills.** After it, the
     test hand-publishes on the claimant's behalf and asserts
     `rx.recv().unwrap().unwrap_err() == FrameHashCollision`. Under the bound the
     spawned interner has already returned `InternContended` and exited, so
     `recv_timeout` consumed the only message and nothing sends again: that `recv()`
     **blocks forever and the test hangs** until nextest's timeout kills it.
     *An earlier version said it panics on a disconnected channel; `tx` is bound
     in the enclosing function and the `s.spawn` closure is not `move`, so the
     sender outlives the scope and the channel never disconnects. A hang is
     harder to recognise than a panic, which makes the understatement worse.* The
     doc comment's claim that the unblocking "proves the waiter was still on the
     normal publish path" describes a property the bound makes unobservable.
     Fixing only the timeout leaves a panicking test.

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

   **Worse, the shipped Python message tells the caller to retry.**
   `unresolvable_name` renders *"is being interned right now by a participant
   this arena cannot identify … Retry"*, so a control loop following it retries
   forever against a stopped claimant — relocating the unbounded wait into the
   caller rather than ending it, and defeating the purpose of step 2. *An earlier
   version cited `detached_err`'s rustdoc naming the variant "in the retry loop"
   alongside `SlotContended` and `LeaseContended`; that site is `pub(crate)` and
   this step's own erratum says it reaches no wheel user, so the paragraph rested
   on the source it withdraws.* So step 3 covers the variant doc, the `Display`,
   `intern_core`'s `# Errors` (which does not mention the variant today) and the
   Python retry guidance.

   **The alternative, costed rather than taken:** a distinct `Copy` identifier
   for "the claimant will not progress", which is what D11 would suggest if the
   two causes need different handling. It is new public surface on a published
   crate and `API.md` §7's checklist; this record takes the cheaper route and
   names it so a reopening starts from the cost.

   **Eleven shipped sites — and "counted rather than sampled" was wrong twice
   before this number.** `FrameError::InternContended`'s
   variant doc and its `Display`; `LookupError::UnknownFrame`'s "transient";
   `find_core`'s `# Errors` and `ArenaView::find_frame`'s, both saying the
   claimant is *anonymous*; `Tree::lookup`'s `# Errors` ("— transient: retry"),
   which is the one a Rust caller actually reads; `intern_core`'s `# Errors`,
   which does not mention the variant at all; and **`tf_tree_py`'s
   `unresolvable_name` message**, which is the shipped Python text telling a
   caller *"cannot identify … Retry"*. **Plus three found in round 4:**
   `crates/tf_tree_c/include/tf_tree.h`'s `TFT_ERR_UNKNOWN_FRAME` — *"a name
   another participant is interning right now (transient — retry)"*, the shipped
   C ABI text, and the step's own first line already says "the C entry point";
   `AwaitError::Frame`'s doc, whose rationale is that such an error "will not
   change on its own"; and `Tree::frame`'s `# Errors`. The header is generated,
   so `cargo xtask headers` regenerates it and `just c-header-check` gates it.

   *An earlier version of this step listed
   four and cited `detached_err`'s rustdoc for the Python site — that one is
   `pub(crate)` and reaches no wheel user, so the fix would have landed on the
   invisible copy while the message that actually sends a control loop into an
   unbounded retry stayed put.*
4. A `loom` model reaches the bound, with a control that fails when the bound is
   unreachable — and, per question 2, a second control that fails when the
   reader's `CLAIM_UNRECORDED` abandon path is unreachable, which is what a
   shrunk N below `READER_UNRECORDED_ROUNDS` would cause.

5. **`Tree::await_frames` retries `InternContended` within its own timeout**
   rather than returning it terminally, and `AwaitError::Frame`'s doc stops
   saying the error "will not change on its own". Without this the bound turns a
   five-second await into a three-millisecond refusal.
   - **Verified by** a test that stages a contended name and asserts
     `await_frames` keeps waiting to *its* deadline, and by the existing await
     tests passing unchanged.

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
     N ≥ 1 is therefore unreachable by a claimant that is merely slow, so this
     side does not constrain the choice at all.
   - **Actionable by a control loop.** A robot loop runs at 100–1000 Hz. A
     refusal cannot always arrive inside a period — see the Ceiling bullet, which
     is where this constraint is actually stated — but it must arrive in a time a
     loop can absorb as a dropped cycle rather than a stall.

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
   - **Ceiling:** the refusal must arrive while a control loop can still act on
     it. **Not "inside one period"** — at 1 kHz that is 1 ms, which the floor
     alone (N ≥ 5, ~2 ms on x86) already exceeds, so the rule would be
     unsatisfiable at the top of its own rate range. What the bound buys at any N
     is that *one* cycle is lost instead of all of them; the ceiling is therefore
     single-digit milliseconds on x86, and a 1 kHz loop is told plainly that a
     refusal costs it more than one period.
   - **N = 8** is the value to implement: comfortably above the floor of 5, with
     room for `READER_UNRECORDED_ROUNDS` to grow before the two collide, at
     ~3.2 ms of spinning on x86. *An earlier version called it "the smallest
     multiple of the floor"; 8 is a multiple of 4, not of the floor's 5, so
     re-applying that derivation at a different `READER_UNRECORDED_ROUNDS` gives
     whichever of two numbers the reader guesses.*

   **Two costs the first two versions of this answer did not price.**

   - **The probes are syscalls and are not counted in N × `INTERN_SPIN_LIMIT`.**
     Each round also calls `claimant_alive`, which on a `tf_tree` tree is an
     `F_OFD_GETLK` and can fall back to reading `/proc/<pid>/stat`. At N = 8 that
     is eight syscall-backed probes on top of the spinning, and on a loaded host
     a `/proc` read runs to hundreds of microseconds. **Step 2 measures the round
     cost, not only the intern duration** — the record's own standard is that the
     number is not to be assumed, and the first two versions assumed the half
     that is cheap.
   - **`loom` cannot simply shrink N.** `INTERN_SPIN_LIMIT` is 2 under
     `cfg(loom)`; `READER_UNRECORDED_ROUNDS` carries **no `cfg`** and stays 4. A
     loom-shrunk N of 2 or 4 therefore sits at or below the floor, the reader's
     `CLAIM_UNRECORDED` abandon path becomes unreachable in the model, and the
     model stops covering the false negative that constant exists to prevent.
     **Both constants shrink together or neither does**, and step 4 owes a
     control that fails when the abandon path is unreachable.

   *An earlier version of this answer read "the smallest value whose product …
   exceeds the measured intern by two orders of magnitude and stays under 10 ms".
   That has a floor a single round already clears, so it yields **N = 1** — below
   `READER_UNRECORDED_ROUNDS`, which is the one value that must not be chosen. It
   stated the "N of about 8" conclusion beside a rule that contradicts it.*

   **Step 2 must still report the measured round cost**, on **both**
   architectures this project gates. `spin_loop()` is a `pause` of ~140 cycles on
   recent x86-64 and an `isb` of tens of cycles on aarch64, so the same iteration
   count is **several times shorter** there and the "~0.4 ms at 3 GHz" figure
   below is x86-specific.

   **Which constraint that pressures is the opposite of what an earlier version
   of this paragraph said.** A shorter round makes N × `INTERN_SPIN_LIMIT` a
   *smaller* duration, so the **ceiling gains** headroom on aarch64 — it is the
   **health margin** that erodes: "unreachable in health by orders of
   magnitude" goes from roughly three orders to two as a round falls from ~0.4 ms
   to ~0.05 ms. **The structural floor is untouched** — `READER_UNRECORDED_ROUNDS`
   and N both count *rounds*, not time, so no clock speed moves it, and calling
   the health margin "the floor's purpose" re-merged the two things this answer
   separates. *The earlier text said the ceiling lost headroom on arm, which
   would tell an implementer to lower N there — backwards for the constraint
   actually under pressure, and toward the floor this same answer says must not
   be crossed.*

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

   Four of the five are **bounded and return rather than waiting on a peer's
   scheduling** — three wait on a store a running peer is about to make, and
   A2's acquire spins `TOPO_LOCK_SPIN_LIMIT` and then decides, as the Context
   above is at pains to say. Yielding in any of them trades a sub-microsecond
   wait for a scheduler round trip. *An earlier version said all four wait on a
   store a running peer is about to make, which is the claim the Context
   paragraph exists to refute about A2.*
   `buffer::read_slot` is the hot read path, so that is not a theoretical cost.
   Exactly one waits on a peer whose scheduling is the thing in question.

   So `spin` stays pure for the four and a second function — calling the
   facade-installed yield hook of *Decision* §1, pure-spinning when none is
   installed — is `wait_for_publish`'s alone, with each call site naming which it
   wants. *This read "yielding under the std-backed arm", the mechanism §1 now
   rejects; the mechanism is stated in three places and this was the third.* **The `loom` arm is unaffected**: it already yields for all
   five, for interleaving rather than starvation, and that must stay true of both
   functions or the models stop scheduling the thread they wait on.

   ~~Does the yield belong in `sync::spin` for every caller, or only here?~~ The
   seqlock retry in `buffer::read_slot` and A2's acquire spin both wait on a peer
   that is a few instructions from a store; yielding there would trade a
   sub-microsecond wait for a scheduler round trip. If the answer is "only here",
   `spin` splits into two functions and each call site states which it wants.
