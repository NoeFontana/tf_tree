# 0014: The push heartbeat is a store, not a locked read-modify-write

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** #114

## Context

`SampleRing::push` ended with `heartbeat.fetch_add(1, Relaxed)` after the `head` store; `docs/PHASE1.md` §6.2 is a **NORMATIVE** listing of it, so the change is ratified here.

## Decision

```rust
self.head.store(h + 1, Ordering::Release);
self.heartbeat.store(h + 1, Ordering::Relaxed);
```

`docs/PHASE1.md` §6.2's listing is amended to match, with an inline box recording
the change (and the A5 `s | 1` correction the listing had also missed).

## Rationale

- **The stored value is identical.** `heartbeat` is zero-initialised and bumped
  once per push, so it always equals the post-push `head`. `head` is stored in
  one place and never reset; `ClaimRecord::heartbeat` is written by this line
  only (`ParticipantRecord`'s is a different struct).
- **No ordering is weakened.** `Relaxed` before and after. The atomicity bought
  nothing: the ring is single-writer (D7), the guarantee the plain `head.store`
  above already rests on.
- **The cost was real:** `push/single_writer` went 8.66 → 4.65 ns/push.
- The heartbeat stays: PHASE2 §6.4 requires it bumped on every push (D17).

## Consequences

- `heartbeat` is now *defined* as equal to `head`. A reset of one without the
  other, or a second writer, breaks it; a multi-writer edge type must revisit this
  line and `head.store` together. Enforced by the `debug_assert_eq!` in `push`
  (resolution 1).

## Implementation plan

1. `buffer.rs`: `fetch_add(1)` → `store(h + 1)`.
2. `docs/PHASE1.md` §6.2 amended for this and A5.
3. Re-baseline `just bench` — deferred to
   [`0013`](./0013-the-benchmark-gate-never-interpolated.md).
4. Assert the equality in `push` (resolution 1).

All four landed in #114.

## Resolved questions

**1 — Assert the equality. Resolved: yes, `debug_assert_eq!` in `push`.** The
safety argument is a property of the rest of the codebase: `fetch_add` tolerated
a diverged base, a store cannot. The assertion is free in release and runs under
loom, miri and the debug suite, where a new writer would first appear. No current
path writes `head` other than `push` and the two `EdgeRecord` constructors (zero).

**2 — Is the inline amendment box the pattern for PHASE1? Resolved: no.**
`PHASE2.md` §1 stays the single index of amendments; a normative PHASE1 listing is
corrected in place to match shipped code, with a short box naming the amendment.
Applying that to A1–A4 and A6–A8 is a separate documentation sweep.
