# 0018: Blocking waits live in the caller, not in the arena

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** step 1 landed as
`Plan::slowest_nominal_rate_mhz` in `crates/tf_tree_core/src/plan.rs`, beside
`Plan::span`, with the signature *Decision* prints:
`Result<Option<u32>, LookupError>`. Steps 2–5 are open; step 2's runnable
doc test wants a `Tree` and a second thread, neither of which exists in
`tf_tree_core`, so it belongs on the facade.

## Decision

**No blocking primitive in the arena, and none in `tf_tree_core`. The wait lives
in the caller** ([`PHASE7.md`](../PHASE7.md) shim).

**`Plan::span(&Guard) -> Result<Option<(i64, i64)>, LookupError>`** already gives
the coverage interval. `Ok(None)` means every step is static; `t0 > t1` is an empty
intersection; `NoData` names the first edge never published. **No `Plan::coverage`
is added.**

**Core gains one read-only addition:**

```rust
impl Plan {
    /// The slowest declared nominal rate among this plan's dynamic edges, in
    /// millihertz (`EdgeRecord::nominal_rate_mhz`). `Ok(None)` if none declared one.
    pub fn slowest_nominal_rate_mhz(&self, g: &Guard)
        -> Result<Option<u32>, LookupError>;
}
```

It is fallible so a waiter sees `TopologyChanged`, `ChildDetached` and `UnknownEdge`.

**The wait, which is the shim's and is written once there:**

```
loop {
    let g = tree.guard();
    match plan.span(&g)? {
        None => return Ok(plan.at(&g, wanted)?),                    // all static
        Some((_, newest)) if newest >= wanted => return Ok(plan.at(&g, wanted)?),
        Some((_, newest)) => sleep(min(remaining, (wanted - newest) + one_period)),
    }
    if now >= deadline { return Err(Timeout) }
}
```

`NoData` takes the shortfall branch. The sleep is a prediction; geometric backoff
applies only when it was wrong. `canTransform(timeout)`'s granularity is scheduler
granularity plus one publish period.

**Escalation path.** Below ~100 µs: the **Phase 2 owner server** (one polling
thread, an `eventfd` per waiter). **The arena futex is nailed shut:** `PROT_READ`
consumers (D18) cannot register as waiters, and a check would tax every push.

## Implementation plan

1. **Done.** `Plan::slowest_nominal_rate_mhz`; an edge declaring `0` is skipped.
2. Document both with the loop above as a facade doc test.
3. Re-export through the facade and `tf_tree_unstable.h` — `just c-header-check`.
4. An exploratory benchmark row for `span`; no `bench-check` verdict.
5. The shim's wait and `tft_wait_until_covered()` (`PHASE7.md` §4 J2), gated by D21.

## Open questions

None.
