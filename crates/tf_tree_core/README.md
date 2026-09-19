# tf_tree_core

[![crates.io](https://img.shields.io/crates/v/tf_tree_core.svg?logo=rust)](https://crates.io/crates/tf_tree_core)
[![docs.rs](https://img.shields.io/docsrs/tf_tree_core?logo=docsdotrs)](https://docs.rs/tf_tree_core)
[![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue.svg)](#licence)

The `no_std + alloc` engine underneath [`tf_tree`](https://crates.io/crates/tf_tree).
**Most people want `tf_tree`**; depend on this crate only if you are `no_std` or
building your own facade.

## This crate's `pub` surface is not the project's API

**What `tf_tree` re-exports is the promise** (`Plan`, `Guard`, `Stamp`, `Query`,
the error types, `Layout`). Everything else is shaped by the arena and moves with
`FORMAT_VERSION`.

## What the engine guarantees

1. **Append-only identity.** `FrameId` and `EdgeId` are never reused; a stale
   `Plan` can index a valid record but never go out of bounds.
2. **Single writer per edge**, enforced by the claim table.
3. **Stamps are non-decreasing per edge**, integer nanoseconds carrying a time
   domain in the type.
4. **Every heap allocation happens at construction.** Fixed capacity; lookups do
   not allocate.

Errors are `Copy` identifiers naming the offending edge; prose is
`tf_tree::Described`. Every atomic is imported from `crate::sync`, so the loom
suite checks the code the engine runs.

## Features

| Feature | Default | What it does |
|---|---|---|
| `counters` | **on** | Diagnostic counters; off does not fork the layout hash |
| `miri-soft-float` | off | Soft-float `libm` paths, for Miri only |
| `bench-probe` | off | An `#[inline(never)]` wrapper around `Plan::at` for benchmarks. No shipped crate enables it |

## Version and docs

**`0.0.x` promises nothing**: pin exactly
([`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md)).
MSRV is **1.87** ([`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md)).
Architecture and layouts: `docs/PROJECT.md`, `docs/PHASE1.md`, `docs/API.md`.

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option; see [`NOTICE`](NOTICE).
