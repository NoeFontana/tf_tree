# 0014: The push heartbeat is a store, not a locked read-modify-write

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** #114

## Decision

`SampleRing::push` ends with `self.head.store(h + 1, Release)` then `self.heartbeat.store(h + 1, Relaxed)`. `docs/PHASE1.md` §6.2's NORMATIVE listing is amended to match.

## Rationale

- The stored value is identical: `heartbeat` is bumped once per push, so it always equals the post-push `head`.
- No ordering is weakened; the ring is single-writer (D7).
- The heartbeat stays: PHASE2 §6.4 requires it bumped on every push (D17).

## Consequences

- `heartbeat` is *defined* as equal to `head`; a reset of one without the other, or a second writer, breaks it. Enforced by the `debug_assert_eq!` in `push` (resolution 1).

## Implementation plan

1. `buffer.rs`: `fetch_add(1)` to `store(h + 1)`.
2. `docs/PHASE1.md` §6.2 amended.
3. Re-baseline `just bench`, deferred to [`0013`](./0013-the-benchmark-gate-never-interpolated.md).
4. Assert the equality in `push` (resolution 1).

All four landed in #114.

## Resolved questions

**1 — Assert the equality. Resolved: yes, `debug_assert_eq!` in `push`.** Free in release; runs under loom, miri and the debug suite.

**2 — Is the inline amendment box the pattern for PHASE1? Resolved: no.** `PHASE2.md` §1 stays the single index of amendments.
