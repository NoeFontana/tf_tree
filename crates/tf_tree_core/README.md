# tf_tree_core

The `no_std + alloc` engine underneath
[`tf_tree`](https://crates.io/crates/tf_tree): frame interning, topology, edge
records and the claim table, the seqlock sample buffers, bracket search, and
plan compilation and evaluation.

**Most people want [`tf_tree`](https://crates.io/crates/tf_tree).** It re-exports
what is stable here and adds the allocating conveniences — the builder, the
plan-cached `lookup`, `Display` errors. Depend on this crate directly only if
you are `no_std`, or if you are building your own facade.

## This crate's `pub` surface is not the project's API

Rust has one visibility tier, so everything `pub` reads as a semver promise
whether it was meant as one or not. The facade answers that with a
`tf_tree::unstable` module behind a feature. This crate cannot — it is a
dependency of the facade and has to be published for the facade to be — so it
answers with a statement, which is the honest form of the same thing:

* **What `tf_tree` re-exports is the promise.** `Plan`, `Guard`, `Stamp`,
  `Query`, the error types, `Layout`. Their shape is the engine's contract.
* **Everything else here is shaped by the arena**, and the arena is scheduled to
  change. `arena_view`, `buffer`, `frame`, `edge`'s records, `participant`,
  `counters` and `topology` move with `FORMAT_VERSION`. Depend on them and
  expect to be rebuilt.

## What the engine guarantees

Eight invariants hold the concurrency design up. The four a caller can observe:

1. **Append-only identity.** `FrameId` and `EdgeId` are never reused; removal is
   tombstoning. A stale `Plan` can index a valid record but can never go out of
   bounds.
2. **Single writer per edge**, enforced by the claim table rather than by
   convention.
3. **Stamps are non-decreasing per edge**, and they are integer nanoseconds
   carrying a time domain in the type.
4. **Every heap allocation happens at construction.** Capacity is fixed; there
   is no growth and no realloc. Lookups do not allocate.

Errors are `Copy` identifiers naming the offending edge — never a `String`, and
never formatted on the failure path. Prose is a separate layer
(`tf_tree::Described`).

## Concurrency, and how it is checked

Every atomic is imported from `crate::sync`, which is `core::sync::atomic`
normally and `loom::sync::atomic` under `--cfg loom`. The publish, read, claim
and intern algorithms compile unchanged in both modes, so the model checker
exercises the same code the engine runs. Orderings are not relaxed because a
test passed on x86-64; that is what the loom suite is for.

## Features

| Feature | Default | What it does |
|---|---|---|
| `counters` | **on** | The `TFT` diagnostic counters. Turning it off removes the fields, the increments and the `Guard` destructor — what executes is then provably nothing. The arena *regions* stay either way, so the layout hash does not fork and the two builds still attach to each other. |
| `miri-soft-float` | off | Routes `libm` through its soft-float paths. Needed only under Miri, whose interpreter cannot execute the inline `sqrt` asm `libm` emits by default. Never enable it for anything you intend to measure. |
| `bench-probe` | off | One `#[inline(never)]` wrapper around `Plan::at`, compiled *in this crate* so the repository's cross-crate inlining measurement has an in-crate control. No shipped crate enables it. |

## Version

**`0.0.x` promises nothing.** Cargo treats every `0.0.x` release as
incompatible with every other, which is the intended signal: pin exactly, and
expect a later release to break. The number is deliberately not repeated here —
this line read `0.0.1` for three releases, because nothing gates a version in
prose. The reasoning is written out in the
repository's [`Cargo.toml`](https://github.com/NoeFontana/tf_tree/blob/main/Cargo.toml)
under `[workspace.package] version`, and the release notes are in
[`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md).

MSRV is **1.87**; see
[`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md).

## Where the rest of it is

[`docs/PROJECT.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PROJECT.md)
for the architecture and the decision log;
[`docs/PHASE1.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PHASE1.md)
for the normative layouts, the atomic orderings and the test plan;
[`docs/API.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/API.md) for
the six rules every binding obeys.

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option. See
[`NOTICE`](NOTICE).
