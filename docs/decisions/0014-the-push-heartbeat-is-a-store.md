# 0014: The push heartbeat is a store, not a locked read-modify-write

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** #114

## Context

`SampleRing::push` ended with:

```rust
self.head.store(h + 1, Ordering::Release);
self.heartbeat.fetch_add(1, Ordering::Relaxed);
```

`docs/PHASE1.md` §6.2 is a **NORMATIVE** listing of that protocol and shows the
same `fetch_add`. Changing it is therefore a change to a normative
publish-protocol section, which `CLAUDE.md` routes here rather than to a PR —
even though, as below, the change is behaviour-identical and weakens no ordering.
This record exists so the edit is ratified rather than assumed.

The same five-lens audit that produced this record also found that the §6.2
listing had *never* been updated for amendment A5 (it still showed
`s.wrapping_add(1)` for the odd flip, which A5 replaced with `s | 1` before Phase
1 froze). Both lines were corrected in the same commit.

## Decision

*(draft — recommendation, not yet ratified)*

```rust
self.head.store(h + 1, Ordering::Release);
self.heartbeat.store(h + 1, Ordering::Relaxed);
```

and `docs/PHASE1.md` §6.2's listing amended to match, with an inline box
recording both corrections.

## Rationale

**The stored value is identical.** `heartbeat` is zero-initialised and bumped
exactly once per push, so it always equals the post-push `head`, i.e. `h + 1`.
Verified by grep over the whole workspace rather than assumed:

- `head` is stored in exactly one place — `buffer.rs`, this line's neighbour —
  and is never reset. No reap, relocation, re-claim or takeover path touches it.
- `ClaimRecord::heartbeat` is written by exactly one line, this one.
  (`participant.rs`'s `heartbeat.store(0)` is `ParticipantRecord`, a different
  struct.)

So `heartbeat == head` at every quiescent point, including across reap and
re-claim, and `store(h + 1)` is bit-identical to `fetch_add(1)`.

**No ordering is weakened.** `Relaxed` before, `Relaxed` after. What goes is the
atomicity, and the atomicity bought nothing: the ring is single-writer by
construction (D7), which is the *same* guarantee the plain `head.store`
immediately above already rests on. A protocol where one of those two is a
locked RMW and the other is a plain store was never coherent.

**The cost is real and the project already argued against it.** On x86 `fetch_add`
lowers to a `lock`-prefixed instruction whose implicit full barrier drains the
store buffer directly behind the eight relaxed payload stores above it. Measured
on `push/single_writer`, both sides on the same host: **8.66 → 4.65 ns/push, −46%**.
`counters.rs`'s own module doc is the argument, written for a different feature:
"a relaxed `fetch_add` on the push path costs ~5–10 ns … to store something the
arena already holds", and it names `EdgeRecord::head` as the thing that already
holds it. This applies it to the last such `fetch_add` left on the push path.

**Alternatives considered.** Leave it: costs 4 ns on the hottest write path in the
project for no invariant. Drop the heartbeat entirely: rejected — PHASE2 §6.4
requires it be bumped on every push, and it is a diagnostic an operator reads even
though it is never a reaping trigger (D17).

## Consequences

- `heartbeat` is now *defined* as "equal to `head`", not merely "advances once per
  push". Anything that resets `head` without resetting `heartbeat`, or introduces
  a second writer to either, breaks the equality. Both are single-writer today and
  D7 is what keeps them so; a future multi-writer edge type would have to revisit
  this line and `head.store` together. **This is enforced, not merely stated** —
  see the `debug_assert_eq!` in `push` and resolution 1 below.
- No consumer is affected: nothing in the workspace reads `ClaimRecord::heartbeat`
  except `ArenaView::ring_of`'s wiring and one core test asserting it equals 3
  after 3 pushes, which still passes.

## Implementation plan

1. `buffer.rs`: `fetch_add(1)` → `store(h + 1)` with the rationale comment —
   verified by `cargo nextest run -p tf_tree_core`, `just loom` (15/15), `just
   miri`, `just tsan`, all green.
2. `docs/PHASE1.md` §6.2: amend the listing for both this and A5 — verified by
   reading it against `SampleRing::push` line for line.
3. Re-baseline `just bench` — deliberately deferred to
   [`0013`](./0013-the-benchmark-gate-never-interpolated.md), so the push and
   lookup numbers are re-baselined exactly once rather than twice.

4. Assert the equality in `push` — see resolution 1 below. Verified by `just
   test`, `just loom` and `just miri`, all of which build with debug assertions
   on and exercise `push` directly.

All four steps have landed in #114.

## Resolved questions

Both were open while this record was `draft`; recorded here rather than deleted,
because the reasoning is the part worth keeping.

**1 — Assert the equality, don't just argue it. Resolved: yes, `debug_assert_eq!`
in `push`.**

The whole safety argument for this change is "`heartbeat == head` at every
quiescent point", and that is a property of *the rest of the codebase*, not of
this function — it holds because nothing else writes either field. A future
reset path, or a second writer, breaks it silently. `fetch_add` tolerated that
divergence (it would keep counting, just from the wrong base); a store cannot.

So the invariant is asserted immediately before the store. It costs nothing in
release, and it runs under `just loom`, `just miri` and the entire debug test
suite — which is precisely where a new writer would first appear. Checked before
adding it that no such path exists today: `head` is written only by `push` and
zeroed by both `EdgeRecord` constructors, and nothing in `tf_tree_ingest`, the
facade or the frozen-arena path writes it, so the assertion cannot fire
spuriously on any current path.

**2 — Is the inline amendment box the pattern for PHASE1? Resolved: no. `PHASE2.md`
§1 stays the single index; PHASE1 listings are corrected in place with a box
saying what changed.**

The two are not alternatives, and conflating them is what produced the drift this
record documents. `PHASE2.md` §1 is the *index of amendments* — it says what A1–A8
are and argues for them, and it must stay the one place to look. What it cannot do
is stop a reader of `PHASE1.md` §6.2 from copying a stale listing, which is exactly
what happened: A5 shipped in the code and §6.2 kept showing `s.wrapping_add(1)` for
the entire life of the project, and nobody noticed until a lens went looking for
comments that lie.

So a normative PHASE1 listing is **corrected in place** to match shipped code, with
a short box recording what changed and pointing at the amendment that caused it.
The listing is the thing people read and copy; it has to be right. The box exists
so the correction is not mistaken for the original.

Applying this retroactively to A1–A4 and A6–A8 is *not* part of this record — it is
a separate documentation sweep, and doing it here would bury a protocol change
inside a doc refactor. Worth scheduling; the §6.2 box is the template.
