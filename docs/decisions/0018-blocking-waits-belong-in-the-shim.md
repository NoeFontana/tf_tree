# 0018: Blocking waits live in the caller, not in the arena

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** step 1 landed as
`Plan::slowest_nominal_rate_mhz` in `crates/tf_tree_core/src/plan.rs`, beside
`Plan::span`, with the signature *Decision* prints:
`Result<Option<u32>, LookupError>`. Steps 2–5 are open; step 2's runnable
doc test wants a `Tree` and a second thread, neither of which exists in
`tf_tree_core`, so it belongs on the facade.

## Context

`tf2_ros::Buffer::canTransform` and `lookupTransform` block until the transform is
available, and any `tf2`-shaped shim ([`PHASE7.md`](../PHASE7.md)) must offer it.
`tf_tree` has no blocking or notification primitive (reads are wait-free, a publish
is a seqlock store). The timeout is built **inside the arena** (a layout change and
a permanent push-path cost, D4) or **in the caller** (sleep-and-recheck).

## Decision

**No blocking primitive in the arena, and none in `tf_tree_core`. The wait lives
in the caller.**

**`Plan::span(&Guard) -> Result<Option<(i64, i64)>, LookupError>`** already
provides the coverage interval a waiter needs: the intersection of every dynamic
edge's retained window, generation-checked. `Ok(None)` means every step is static;
`t0 > t1` is a real empty intersection; `NoData` names the first edge that has
never published. **A `Plan::coverage` is not added — it would be `span` under a
second name.**

**Core gains one read-only addition:**

```rust
impl Plan {
    /// The slowest declared nominal rate among this plan's dynamic edges, in
    /// millihertz, as declared on `EdgeCfg` and stored in
    /// `EdgeRecord::nominal_rate_mhz`. `Ok(None)` if no edge on the plan
    /// declared one.
    ///
    /// A caller sleeping until data arrives should sleep about one period of
    /// *this* edge, not of the fastest one.
    pub fn slowest_nominal_rate_mhz(&self, g: &Guard)
        -> Result<Option<u32>, LookupError>;
}
```

It is fallible because it must carry `TopologyChanged`, `ChildDetached` and
`UnknownEdge`: a waiter that swallowed either of the first two would spin to its
deadline against a plan that can never be satisfied.

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

`NoData` takes the same branch as a shortfall: sleep one period and re-check
(the startup case). The sleep is a **prediction**, not a poll interval; geometric
backoff applies only when it was wrong.

**Escalation path.** If a requirement below ~100 µs ever appears, the answer is
the **Phase 2 owner server**: it polls on behalf of all waiters on its one thread
and signals each an `eventfd`, with no arena write from any consumer and the push
path untouched. **The arena futex is nailed shut.**

## Rationale

**The waiting party cannot write.** D18 makes consumers attach `PROT_READ`, and every
shared-memory blocking primitive needs the waiter to register by writing a shared
word (a `SIGSEGV` for a read-only consumer). An arena futex would cost read-write
attach for every waiter or a second writable side-channel with its own rendezvous
and reaping.

**A permanent tax on the write path**: a waiter-count check on every push, after
[`0014`](./0014-the-push-heartbeat-is-a-store.md) took `push` from 8.66 ns to 4.65 ns.

**The latency bought is small.** A futex wake is ~5–10 µs; a timed sleep overshoots
by ~50 µs (`PREEMPT_RT`) to ~1 ms (stock). The prediction matters more (one wake, not
100, on a 10 Hz edge). `nominal_rate_mhz` is populated from `topology.toml`'s
`rate_hz`; a rate from `--discover` costs one extra wake.

**`span` belongs in core** so no copy lives where `just test`, `just miri` and
`just loom` never build. **Rejected:** robust mutex / process-shared condvar,
`eventfd` per consumer signalled by the publisher, no wait at all.

## Consequences

- The shim's `canTransform(timeout)` has a **documented granularity**: scheduler
  granularity plus one publish period, stated as an incompatibility.
- `tf_tree_core` stays free of blocking primitives; the push path is untouched.
- `nominal_rate_mhz` is load-bearing for wait latency; an edge declaring none gets a
  conservative fallback period, which the shim reports once at startup.
- `PHASE7.md` §2 and §3.1 point here, so nobody reaches for a futex.

## Implementation plan

1. **Done.** `Plan::slowest_nominal_rate_mhz`, calling `check_generation` as `span`
   does; tested over 10/50/200/1000 Hz edges asserting the 10 Hz answer (**mutant:**
   return the fastest), an edge declaring `0` is skipped, no declaration is `Ok(None)`.
2. Document `span` and the new method with the loop above as a runnable doc test on
   the facade — `cargo test --doc`.
3. Re-export through the `tf_tree` facade and `tf_tree_unstable.h` — `just
   c-header-check`.
4. An exploratory benchmark row for `span` at depths 1/3/6; no `bench-check` verdict.
5. `PHASE7.md` §4 J2 cites this record; the shim's wait and
   `tft_wait_until_covered()` are implemented there, after `PHASE7.md` §0.0's gates.

Steps 1–4 are not gated by D21; step 5 is.

## Open questions

None.
