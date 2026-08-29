# 0039: extrapolation you cannot fail to notice

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`ExtrapPolicy` has three variants and all three work.
`crates/tf_tree_core/src/sample.rs` implements `Error`, `Hold` and `ConstantTwist`
in `sample`, `sample_from`, `sample_with_twist` and `sample_with_twist_from`; the
`ConstantTwist` arm has its own helper (`constant_twist`), its own documented
four-case table for the bracket-less outcomes, and tests.

**No consumer can select any of them.** All five fold sites in
`crates/tf_tree_core/src/plan.rs` pass the `Error` literal, `crates/tf_tree/src/`
contains zero occurrences of the type, and the facade does not re-export it — so
the name cannot even be written by a caller of `tf_tree`. The only code that ever
passes `Hold` is a benchmark example reaching past the facade into
`tf_tree_core::sample` (`crates/tf_tree_bench/examples/step_cost.rs:264`).

So the engine carries a tested capability that is dead from every shipped surface,
and the question is whether to delete it or to reach it. This record reaches it,
because the capability is one a control loop actually wants: a controller running
at 1 kHz against a 100 Hz state estimate is *always* asking for a stamp past the
newest sample, and the honest answer is a bounded prediction with its bound
attached — not a refusal, and not a silent stale pose.

## Decision

**Extrapolation is selected per query, and its result cannot be read without also
reading how far it was extrapolated.**

### 1. One new return type, whose shape is the safety property

```rust
/// A pose, and how far past the plan's newest common sample it was extrapolated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extrapolated {
    /// The pose.
    pub pose: Iso3,
    /// Nanoseconds past the newest stamp every dynamic edge on this plan has
    /// data for. `0` means every edge bracketed the query — the answer is
    /// interpolated, not invented.
    pub by_ns: i64,
    /// The dynamic edge that ran out of data first: the one whose newest stamp
    /// is `by_ns` behind the query. Meaningless when `by_ns == 0`.
    pub edge: EdgeId,
}
```

There is no accessor that yields the pose alone. That is the whole design: a
caller who wants extrapolation is handed the distance in the same value, so
"forgot to check the staleness" is not reachable by omission — it takes a
deliberate `.pose`.

### 2. Two new methods, and the default does not move

```rust
impl Plan {
    pub fn at_extrapolating<D: Domain>(&self, g: &Guard, t: Stamp<D>,
        policy: ExtrapPolicy) -> Result<Extrapolated, LookupError>;
    pub fn at_extrapolating_tagged(&self, g: &Guard, nanos: i64, domain: u8,
        policy: ExtrapPolicy) -> Result<Extrapolated, LookupError>;
}
```

The tagged sibling exists for [`0038`](./0038-the-domain-a-binding-cannot-name.md)'s
reason and is what the bindings call. `Plan::at` is untouched, still passes
`Error`, and remains what the README's hot loop shows. `ExtrapPolicy` and
`Extrapolated` are re-exported from `tf_tree` so the facade can name them.

### 3. `by_ns` is measured against `latest_common`, not per edge

The distance reported is `max(0, t - min over dynamic edges of newest_stamp)` —
the *worst* edge on the route, which is the number that bounds the answer's
invention. `Plan::latest_common` already folds exactly that minimum
(`fold_latest_common`); this reuses the same walk and additionally keeps the
argmin edge, so nothing new is computed and nothing is estimated.

### 4. The hot path does not pay for this

The distance is derived **after** the fold, from `d` `newest_stamp` loads on the
extrapolating path only. It is not threaded through `fold_at`, `sample_from` or
the seqlock read, so `Plan::at`'s generated code is unchanged — verified by the
existing benchmark gate rather than asserted. Threading an out-parameter through
the sampler was the obvious alternative and is rejected for exactly this: the
policy is a runtime enum, so an unused out-parameter would not be optimised away
on the default path, and the default path is every existing caller.

## Rationale

**Why per query rather than per edge?** A `Hold` that is right for a 10 Hz map
edge is wrong for the 1 kHz IMU edge on the same route, so per-edge configuration
makes the property of an *answer* depend on a declaration made by whoever
published, not by whoever is about to act on it. Per query, the caller who bears
the consequence makes the choice.

**Why not delete the two variants instead?** That was the smaller change and the
one this record's alternative proposed: cut `Hold`, `ConstantTwist`,
`constant_twist` and the `policy` parameter from four sampler signatures, ~130
lines. It loses a capability the engine already computes correctly and that the
project's own headline use case needs — and "refuse, the caller can hold the last
pose themselves" is worse, because a caller holding the last pose *outside* the
engine does not know how stale it is without asking a second question that the
API also did not offer.

**Why is `Hold` kept, given that a silently stale pose is the dangerous one?**
Because it is not silent here. The danger in `Hold` is a pose that looks fresh;
`Extrapolated` makes freshness a field the caller is handed. Under that shape
`Hold` is the honest primitive for a consumer that genuinely wants
zero-order hold (a latched static-ish edge, a display), and `ConstantTwist` is the
one a controller wants. Both are reported identically.

**Why not report per-edge staleness for every edge?** It would be a slice, which
is an allocation or a caller-supplied buffer on a path that has neither, and the
minimum is what bounds the composed answer. A caller who needs the breakdown has
`Plan::span` and the diagnostics catalogue.

## Consequences

- The public surface grows by one struct, two methods and two re-exports. The
  engine loses no code, and ~130 lines that were dead from every shipped surface
  become reachable.
- `docs/RUNBOOK.md`'s extrapolation guidance and `docs/PHASE1.md` §3.x can stop
  describing a policy nobody could select.
- `Extrapolated` is `Copy` and allocation-free, so R2 and R5 hold. It is not an
  error type and does not need to be: extrapolation under an explicit policy is a
  requested outcome, not a failure, and `ExtrapPolicy::Error` remains the way to
  make it a failure.
- A future accessor returning only the pose would delete the property this record
  is for. It is a design smell for `docs/PROJECT.md` §6, and is listed there.

## Implementation plan

1. Re-export `ExtrapPolicy` from `tf_tree`; add `Extrapolated` to
   `tf_tree_core::plan` and re-export it. Verified by a doctest naming both types
   through the facade.
2. A private `newest_common(&self, g) -> Result<Option<(i64, EdgeId)>, LookupError>`
   on `Plan`, factored out of `fold_latest_common` so the minimum and its argmin
   edge are computed once and in one place. Verified by `latest_common`'s existing
   tests continuing to pass unchanged.
3. `at_extrapolating` and `at_extrapolating_tagged`. Verified by a test that a
   query 5 ms past the newest sample returns `Err(Extrapolation)` under `Error`,
   the newest pose with `by_ns == 5_000_000` under `Hold`, and a *different* pose
   with the same `by_ns` under `ConstantTwist` — the third assertion being what
   distinguishes the two policies rather than just exercising them.
4. `Plan::at`'s generated code is unchanged: verified by `just bench-check`
   against the committed baseline, and reported in the PR rather than assumed.
5. `docs/API.md` §2 records the new surface against the six rules;
   `docs/PROJECT.md` §6 gains the smell in step 4's *Consequences*.

## Open questions

None. Two were resolved while writing:

- *Per-edge or per-query?* Per query — the caller who bears the consequence
  chooses, and a route mixes rates.
- *Should the pose be reachable without the distance?* No, and that is the point
  of the type rather than an incidental property of it.
