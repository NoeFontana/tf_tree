# 0018: Blocking waits live in the caller, not in the arena

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** _(filled in as work lands)_

## Context

`tf2_ros::Buffer::canTransform` and `lookupTransform` take a timeout and
**block** until the transform is available or the timeout expires. Every ROS 2
node that waits for `map -> odom` at startup uses this, and any
`tf2`-shaped shim ([`PHASE7.md`](../PHASE7.md)) must offer it.

`tf_tree` has no blocking primitive and no notification mechanism at all. Reads
are wait-free by construction, publishes are a store to a seqlock slot, and
nothing anywhere signals a waiter. So the shim's timeout has to be built out of
something, and there are two places to build it: **inside the arena**, as a
futex or robust mutex that publishers wake; or **in the caller**, as a
sleep-and-recheck loop.

This has to be decided before the shim is designed rather than during, because
an arena-resident primitive is a layout change (D4: shared memory is a layout
constraint, not a transport) and a permanent cost on the push path.

## Decision

**No blocking primitive in the arena, and none in `tf_tree_core`. The wait lives
in the caller.**

**Most of what a caller's wait needs already exists**, which is a large part of
why this is affordable:

- **`Plan::span(&Guard) -> Result<Option<(i64, i64)>, LookupError>`** — shipped
  for `PHASE5.md` §4.2, and it is exactly the coverage interval a waiter needs:
  the intersection of every dynamic edge's retained window, `Guard`-scoped,
  generation-checked, and three-valued in the way that matters here. `Ok(None)`
  is "every step is static, so any stamp answers"; `t0 > t1` is a real empty
  intersection rather than an error; `NoData` names the first edge that has
  never published. Its upper bound is what `Query::LatestCommon` resolves to.
  **A `Plan::coverage` is not added — it would be `span` under a second name**,
  and this repository has already paid once for a second spelling of an existing
  path (`PHASE5.md` §4.2's `resample`).

**Core gains one read-only addition**, and nothing else:

```rust
impl Plan {
    /// The slowest declared nominal rate among this plan's dynamic edges, in
    /// millihertz, as declared on `EdgeCfg` and stored in
    /// `EdgeRecord::nominal_rate_mhz`. `None` if no edge on the plan declared
    /// one.
    ///
    /// A caller sleeping until data arrives should sleep about one period of
    /// *this* edge, not of the fastest one.
    pub fn slowest_nominal_rate_mhz(&self, g: &Guard) -> Option<u32>;
}
```

**The wait, which is the shim's and is written once there:**

```
loop {
    let g = tree.guard();
    match plan.span(&g)? {
        None                       => return Ok(plan.at(&g, wanted)?),  // all static
        Some((_, newest)) if newest >= wanted => return Ok(plan.at(&g, wanted)?),
        Some((_, newest))          => sleep(min(deadline_remaining,
                                                (wanted - newest) + one_period)),
    }
    if now >= deadline { return Err(Timeout) }
}
```

`NoData` — an edge that has never published at all — takes the same branch as a
shortfall: sleep one period and re-check, because "has not started yet" is the
startup case this whole loop exists for.

It is a **prediction**, not a poll interval: the shortfall is known and the
publish period is declared, so the typical wake count is one or two. Geometric
backoff applies only when the prediction was wrong (a publisher slower than its
declared rate, or stopped).

**The escalation path is recorded so that it is not reinvented as a futex.** If
a measured requirement below ~100 µs ever appears, the answer is the **Phase 2
owner server**, which already has a socket, fd passing, liveness and reaping: it
polls on behalf of all waiters on its one thread and signals each an `eventfd`.
Amortized across N waiters, no arena write from any consumer, and the push path
is untouched. That door stays open. **The arena futex is nailed shut**, for the
reasons below.

## Rationale

### The decisive argument: the waiting party cannot write

D18 makes consumers attach `PROT_READ`, and the read-only mapping is an
MMU-enforced safety boundary this project paid for deliberately — a diagnostic
or consumer process linked against the C ABI is *incapable* of corrupting a
robot's transform tree, and the hardware is what enforces that rather than our
own care.

Every shared-memory blocking primitive requires the waiter to **register** by
writing a word the waker can see: a futex needs the waiter's word in the shared
mapping, a robust mutex needs the waiter in its list, a condvar needs its
sequence bumped. A `PROT_READ` consumer physically cannot do any of it — the
store is a `SIGSEGV`, not an error.

So an arena futex costs one of:

- **every waiting consumer attaches read-write**, discarding D18 for a startup
  convenience; or
- **a second, writable side-channel** with its own rendezvous, liveness and
  reaping — Phase 2's hardest machinery, duplicated, to save a millisecond.

Neither is worth it. And note the shape of the trade: it spends a *safety*
property to buy a *latency* property, on the startup path, where latency is the
cheapest thing in the system.

### The secondary cost is a permanent tax on the write path

A wake requires the publisher to check a waiter count on every push. That is
affordable — this project has measured exactly that shape before, at +0.195 ns
for the fork guard's relaxed load and predictable branch — but the direction is
wrong. `push` was taken from 8.66 ns to 4.65 ns by
[`0014`](./0014-the-push-heartbeat-is-a-store.md) precisely by removing work
that served a diagnostic. Adding work back to the hot write path in service of a
*read*-path convenience reverses a decision made two records ago.

### The latency being bought back is small, and the sleep is not a spin

A futex wake is ~5–10 µs. A timed sleep overshoots by scheduler granularity:
~50 µs on a tuned `PREEMPT_RT` or `nohz_full` kernel, ~1 ms on a stock
`CONFIG_HZ=1000` desktop. So the arena futex buys back at most a millisecond, on
a path where `tf2` users currently wait tens of milliseconds and are content.

The prediction matters more than the primitive. A naive 1 ms poll for a 10 Hz
edge wakes 100 times; the predicted sleep wakes once. `nominal_rate_mhz` is
already in `EdgeRecord`, already declared on `EdgeCfg`, and — since
`PHASE5.md` §6's `TFT007` amendment — **actually populated**, from
`topology.toml`'s `rate_hz` through `TopologyConfig`. This decision simply makes
it reachable from a plan, which is why `slowest_nominal_rate_mhz` is part of the
decision rather than a follow-on.

That amendment also supplies the caveat: a rate produced by
`tf_tree topology --discover` is an *observation* being read as a *declaration*.
A wait tuned to a discovered rate inherits that ambiguity, and the consequence is
one extra wake — which is why the ambiguity is tolerable here and was not
tolerable for `TFT007`.

### Why `span` belongs in core even though the wait does not

It is already there, and the reasoning `PHASE5.md` §4.2's amendment gives for
having moved it out of `tf_tree_py` is the same reasoning that keeps the wait out
of the shim's C++ header: a copy of the retained-window intersection in a crate
`just test`, `just miri` and `just loom` never build is a copy that goes stale,
and walking `ArenaView` instead of a `Guard` answers where `Plan::at` refuses —
from a stale plan after a re-parent, and from a fork-poisoned child. A waiter
that misses `TopologyChanged` would spin until its deadline against a plan that
can never be satisfied.

Three consumers want the interval and none of them can compute it without
reaching into unstable internals: the shim's wait, `doctor`'s "which edge is
holding this lookup back", and any Rust embedder writing their own wait.

### Alternatives considered

**Futex in the arena.** Rejected above: incompatible with D18 without either
discarding the read-only boundary or duplicating Phase 2's rendezvous.

**Robust mutex / `PTHREAD_PROCESS_SHARED` condvar in the arena.** Same
registration problem, plus it puts a lock in a design whose central claim is
wait-free reads, plus owner-death recovery becomes a second reaping protocol
alongside the OFD-lock one that already exists.

**`eventfd` per consumer, signalled by the publisher directly.** Puts a syscall
on the push path and requires the publisher to know its consumers, which the
architecture deliberately does not model — consumers attach without the
publisher's knowledge, and that is the property that makes zero-config discovery
work.

**Owner server polls and fans out `eventfd`s.** Not rejected — this is the
recorded escalation path. Deferred because it is real work for a latency
improvement nobody has asked for, and because doing it later costs nothing that
doing it now would save.

**No wait at all; the shim returns immediately and the user loops.** Rejected:
it is the one piece of `tf2` semantics that every ROS 2 node depends on at
startup, and a shim that omits it is not a shim.

## Consequences

- The shim's `canTransform(timeout)` has a **documented granularity** — it will
  return later than the transform arrived, bounded by scheduler granularity plus
  one publish period. This must be in the shim's documentation as a stated
  incompatibility, not discovered.
- `tf_tree_core` stays free of any blocking or notification primitive, and
  therefore stays `no_std`-shaped at its seams and loom-testable as it is. A
  futex would need its own loom model.
- The push path is untouched. `0014`'s 4.65 ns stands.
- `nominal_rate_mhz` becomes load-bearing for wait latency, not merely
  advisory. An edge that declares no rate gets a conservative fallback period,
  and the shim should say so once at startup rather than silently.
- One new method on `Plan`, which is a `Copy` type whose surface we have kept
  small on purpose. It is a read; it does not touch the hot fold.
- A future contributor who meets a slow startup will reach for a futex. The
  escalation-path paragraph above, plus a pointer to this record from
  `PHASE7.md` §2 and §3.1, is what stops that.

## Implementation plan

1. `Plan::slowest_nominal_rate_mhz` in `crates/tf_tree_core/src/plan.rs`,
   beside `Plan::span`, reading `EdgeRecord::nominal_rate_mhz` through the
   `Guard` and calling `check_generation` as `span` does — verified by a test
   over a fixture with edges at 10/50/200/1000 Hz asserting the 10 Hz answer.
   **Mutant:** return the fastest ⇒ fails. A second test: an edge declaring `0`
   (undeclared, per `TFT007`'s amendment) is skipped rather than treated as
   0 Hz, and a plan where *no* edge declares returns `None`.
2. Document `span` and the new method together as the inputs to a caller-side
   wait, with the loop from §*Decision* in the module docs as a runnable doc
   test on a tree seeded from another thread — verified by
   `cargo test --doc -p tf_tree_core`.
3. Re-export both through the `tf_tree` facade and expose them in
   `tf_tree_unstable.h`, since the shim reaches core only through the C ABI —
   verified by `just c-header-check`.
4. A benchmark row: `span` at depths 1/3/6, to establish it is a read and not a
   walk anyone should worry about — verified by `just bench`. It is exploratory,
   not a gate row: it feeds no `bench-check` verdict.
5. `PHASE7.md` §4 J2 cites this record; the shim's wait — and
   `tft_wait_until_covered()` — is implemented there, not here, and not before
   `PHASE7.md` §0.0's gates are met.

Steps 1–4 are not gated by D21: they are core reads with three consumers, one of
which (`doctor`) exists today. Step 5 is.

## Open questions

None.
