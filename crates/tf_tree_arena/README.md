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
pointers** — every reference is a `u32` element index or a byte offset from the
arena base. So it is relocatable by `memcpy`, maps into another process at a
different address with no fixups, and written to a file *is* the file: opening a
frozen `.tft` is an `mmap`. Two backings implement the `Arena` trait: `HeapArena`
(everywhere) and `MappedArena` (`memfd_create` + `mmap` + seals; `shm` feature,
Linux only).

## Identity

`ArenaHeader` carries `FORMAT_VERSION` (currently **3**) and a `layout_hash` of
the region table; a participant with a different layout is refused at attach, so
appending a field to a `#[repr(C)]` arena record is a `FORMAT_VERSION` event.

## Features

| Feature | Default | What it adds |
|---|---|---|
| `shm` | off | `MappedArena` (`memfd` + `mmap` + `F_ADD_SEALS`), the frozen `.tft` reader/writer, a `rustix` dependency. **Linux only**, kernel ≥ 3.17 |

With `shm` off the crate has one dependency (`bytemuck`) and no syscalls.

## `unsafe`, version, docs

`unsafe` is permitted here (raw arena memory is one of the boundaries in
[`docs/decisions/0007`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md));
every block carries a `// SAFETY:` comment and the crate is
`#![deny(unsafe_op_in_unsafe_fn)]`.

**`0.0.x` promises nothing**: pin exactly and expect a later release to break —
sharper here, since the arena layout is what the version is about
([`docs/PHASE5.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PHASE5.md)
§1.2). MSRV is **1.87**
([`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md));
release notes in
[`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md).
Layout and orderings: `docs/PHASE1.md`; shared memory: `docs/PHASE2.md` §2.

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option. See
[`NOTICE`](NOTICE).
