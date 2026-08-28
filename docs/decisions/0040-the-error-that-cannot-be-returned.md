# 0040: the error that cannot be returned

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

None of `tf_tree_core`'s five error enums — `LookupError`, `PushError`,
`ClaimError`, `FrameError`, `TopologyError` — implements `Display` or
`core::error::Error`. That is not an oversight anyone missed; it is written down
in the crate's own doc comment, as the reason the **first code a consumer
writes** is not compilable Rust:

> `crates/tf_tree/src/tree.rs`, on `Tree::await_frames`: *"`text`, not `rust`, and
> that is the record's own deliberate choice: the three calls yield `OpenError`,
> `AwaitError` and `LookupError`, and `LookupError` implements neither `Display`
> nor `Error`, so **no single `?`-chain unifies them — not even into
> `Box<dyn Error>`**."*

So [`0019`](./0019-one-binary-and-topology-you-can-wait-for.md) §2b's startup
sequence — attach, wait for frames, compile a plan — is published as a ```` ```text ````
block. A robotics integrator's first contact with this library is an example that
does not compile, and their first real function cannot be
`fn setup() -> anyhow::Result<Plan>`.

**This is a day-one cost paid by every Rust consumer, and it buys nothing.** The
constraint that produced it is real and stays: an error must be `Copy`,
`String`-free and `no_std`, because it is returned from the wait-free read path
(D11, `docs/API.md` R5). Neither `Display` nor `core::error::Error` requires any
of that to change.

## Decision

**1. Every error enum implements `Display`, and prints identifiers.**

`fmt::Display` writes into the caller's formatter. No allocation, no `String`
field, no change to layout or to `Copy`. It names what the error *carries* — an
`EdgeId`, a `FrameId`, a domain tag, a stamp — and it does not resolve anything
against an arena, because it has no arena to resolve against.

**2. Every error enum implements `core::error::Error`.**

`core::error::Error` — not `std::error::Error` — so `tf_tree_core` stays
`no_std`. It has been in `core` since Rust 1.81 and the MSRV here is 1.87. This
is what makes `?` work into `anyhow::Error` and `Box<dyn Error>`, which is the
whole point.

**3. `Described` stays the naming layer, and stops duplicating the rest.**

`Tree::describe(err) -> Described<'_>` holds a `&Tree` and resolves frame and
edge *names*; it says things a raw `Display` structurally cannot. It keeps every
arm that names something. Its fallback arm, today `other => write!(f,
"{other:?}")`, becomes `other => write!(f, "{other}")` — so the five variants it
does not special-case (`BufferTooSmall`, `WrongElementType`, `ChildDetached`,
`DerivativesUnavailable`, `NoSegment`) stop printing as `Debug` and start
printing the prose written once in `core`.

**4. Message text remains uncontracted, and the rustdoc says so.**

`docs/API.md` R5 is NORMATIVE that error *types* are a compatibility promise and
message *text* is not. Both `Display` impls are documented as diagnostics that may
change in any release, with no surface documenting text a caller could match on.

## Rationale

**Why this does not weaken R5.** R5 says two things: errors stay `Copy`,
`String`-free and `no_std`; and *"name resolution against the arena is
`Described`, a `Display` wrapper, not a field."* The first is untouched — a
`Display` impl adds no field and no allocation. The second is about **name
resolution**, and the raw impl does none: it prints `edge 3`, never
`odom -> base_link`. The rule forbids putting the arena's knowledge inside the
error, not forbidding the error from describing itself.

**Why not the obvious alternative — leave it, and tell people to use
`Described`.** Because `Described` needs a `&Tree`, and the errors that most need
returning are the ones from paths where the caller is mid-construction and has no
tree yet, or is propagating out of a function that never had one. It also cannot
be the answer for `PushError`, `ClaimError` or `TopologyError` at all —
`Described` wraps `LookupError` only.

**Why not `thiserror`.** The core's dependency budget is `libm` + `bytemuck` +
`blake3` and nothing else (D14). `thiserror` is also a `std`-oriented derive whose
generated `Error` impl is `std::error::Error`. Hand-written `core::fmt` is a few
hundred lines of the most mechanical code in the repository.

**Why this is not "a second spelling" (`docs/PROJECT.md` §6).** The two impls say
*different things*, because one of them knows the frame table and the other does
not. Where they would overlap — the five arena-independent variants — decision 3
deletes the overlap rather than creating it, and the message is written once.

## Consequences

- `fn setup() -> anyhow::Result<Plan>` becomes writable. That is the measure of
  whether this decision worked, and it is demonstrated by a compiling doctest on
  `LookupError` rather than by converting `0019` §2b's startup sequence — see
  implementation step 4 for why that example stays ```` ```text ````, on a reason
  that has nothing to do with errors.
- Five public types gain two trait impls each. Removing a trait impl is breaking,
  so this is a commitment; it is a cheap one, since neither trait constrains the
  representation.
- A caller who logs a raw error gets ids where they might have wanted names. The
  rustdoc on each `Display` points at `Tree::describe`, and `Described` remains
  strictly better wherever a tree is in hand.
- `tf_tree_core` still links no `std`. `core::error::Error` is the only reason
  that is true, and it is why the MSRV floor matters here: a drop below 1.81
  would break the crate rather than merely the convenience.

## Implementation plan

1. `Display` for all five enums in `crates/tf_tree_core/src/error.rs`, covering
   every variant with no catch-all — so a variant added later is a compile error
   here rather than a silently generic message. Verified by a test that formats
   one value of every variant and asserts each is non-empty and contains the
   identifier it carries.
2. `core::error::Error` for all five. Verified by a test that puts each into a
   `Box<dyn core::error::Error>`, and by one that `?`-chains a `LookupError` and
   an `OpenError` into a single `Box<dyn Error>` function — the thing the doc
   comment says cannot be done.
3. `Described`'s fallback arm delegates. Verified by asserting a `BufferTooSmall`
   through `Tree::describe` no longer renders as `Debug`.
4. The acceptance doctest lands on `LookupError` itself, and
   `Tree::await_frames`' example **stays** ```` ```text ````. This is a
   correction to the plan as written, made while implementing: `Open` and
   `OpenError` are `shm`-gated and `just test` runs doctests on *default*
   features, so that example as ```` ```rust ```` would not compile there — and
   as `no_run` behind a `cfg_attr` it would be a doctest no recipe executes,
   which is the failure class `docs/benchmarks/EVIDENCE.md` exists for. What the
   comment claimed was two reasons; one of them (no `?`-chain unifies them) is
   now false and is replaced by what changed, and the other (needs a live arena
   and a feature) is true and stays. Verified by `just test`, which compiles and
   runs the `LookupError` doctest on every configuration.
5. `docs/API.md` R5 records the split: type is the contract, raw `Display` is ids,
   `Described` is names, text is never a promise.

## Open questions

None. One was resolved while writing: whether `Display` should fall back to
`Debug` for variants carrying no identifier (`WrongElementType`, `ChildDetached`).
It should not — those are the two a user is *most* likely to meet from a binding,
where `Debug`'s type-name spelling is the least useful thing to print.
