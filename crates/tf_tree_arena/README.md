# tf_tree_arena

[![crates.io](https://img.shields.io/crates/v/tf_tree_arena.svg?logo=rust)](https://crates.io/crates/tf_tree_arena)
[![docs.rs](https://img.shields.io/docsrs/tf_tree_arena?logo=docsdotrs)](https://docs.rs/tf_tree_arena)
[![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue.svg)](#licence)

The pointer-free arena and its layout math, for the
[`tf_tree`](https://crates.io/crates/tf_tree) transform engine. `no_std + alloc`.
**To look up transforms, depend on [`tf_tree`](https://crates.io/crates/tf_tree)
instead**; this surface is shaped by a layout that is scheduled to change.

## What it is

One flat allocation holds every record and ring buffer and contains **no
pointers**: every reference is a `u32` element index or a byte offset from the
arena base, so it is relocatable by `memcpy` and opening a frozen `.tft` is an
`mmap`. Two backings implement the `Arena` trait: `HeapArena` and `MappedArena`
(`shm` feature, Linux only).

## Identity

`ArenaHeader` carries `FORMAT_VERSION` (currently **3**) and a `layout_hash`; a
participant with a different layout is refused at attach, so appending a field to
a `#[repr(C)]` arena record is a `FORMAT_VERSION` event.

## Features

| Feature | Default | What it adds |
|---|---|---|
| `shm` | off | `MappedArena`, the frozen `.tft` reader/writer, a `rustix` dependency. **Linux only**, kernel ≥ 3.17 |

## `unsafe`, version, docs

`unsafe` is permitted here for raw arena memory
([`docs/decisions/0007`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md)).

**`0.0.x` promises nothing**: pin exactly
([`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md)).
MSRV is **1.87**
([`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md)).
Layout and orderings: `docs/PHASE1.md`; shared memory: `docs/PHASE2.md` §2.

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option; see [`NOTICE`](NOTICE).
