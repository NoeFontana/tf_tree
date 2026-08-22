# tf_tree_arena

The pointer-free arena and its layout math, for the
[`tf_tree`](https://crates.io/crates/tf_tree) transform engine. `no_std +
alloc`.

**If you want to look up transforms, depend on
[`tf_tree`](https://crates.io/crates/tf_tree) instead.** This crate is the
memory underneath it, and its surface is shaped by an on-disk/on-wire layout
that is scheduled to change.

## What it is

One flat allocation holds every record and every ring buffer, and it contains
**no pointers** — every internal reference is a `u32` element index or a byte
offset relative to the arena base. Three consequences, and they are the whole
reason the crate exists:

* It is relocatable by `memcpy`.
* It maps into another process unchanged, at a different address, with no
  fixups.
* Written to a file it *is* the file. Opening one is an `mmap`: no parsing, no
  deserialization, no pointer swizzling. That is what a frozen `.tft` index is.

Two backings implement the same `Arena` trait: `HeapArena` (an aligned heap
allocation, everywhere) and `MappedArena` (`memfd_create` + `mmap` + seals,
behind the default-off `shm` feature, Linux only).

## Identity, and why attaching is safe

`ArenaHeader` carries `FORMAT_VERSION` (currently **3**) and a `layout_hash`
computed from the region table. A participant that computes a different layout
is refused at attach rather than allowed to read a differently-shaped record —
which is why appending a field to a `#[repr(C)]` arena record is a
`FORMAT_VERSION` event and not a source-compatibility event. A version break
means every participant rebuilds and restarts together.

## Features

| Feature | Default | What it adds |
|---|---|---|
| `shm` | off | `MappedArena` (`memfd` + `mmap` + `F_ADD_SEALS`), the frozen `.tft` reader/writer, and a `rustix` dependency. **Linux only**, kernel ≥ 3.17. |

With `shm` off the crate is `no_std + alloc` with exactly one dependency
(`bytemuck`) and no syscalls.

## `unsafe`

Permitted here, deliberately: this is one of the four boundaries the compiler
cannot see across (raw arena memory). Every `unsafe` block carries a
`// SAFETY:` comment naming the invariant it relies on, and the crate is
`#![deny(unsafe_op_in_unsafe_fn)]`. The budget and its four boundaries are
[`docs/decisions/0007`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md).

## Version

**`0.0.x` promises nothing.** Cargo treats every `0.0.x` release as
incompatible with every other, which is the intended signal: pin exactly, and
expect a later release to break. The number is deliberately not repeated here —
this line read `0.0.1` for three releases, because nothing gates a version in
prose. The reasoning is written out in the
repository's [`Cargo.toml`](https://github.com/NoeFontana/tf_tree/blob/main/Cargo.toml)
under `[workspace.package] version`, and the release notes are in
[`CHANGELOG.md`](https://github.com/NoeFontana/tf_tree/blob/main/CHANGELOG.md).

That warning is sharper here than for most crates: the arena layout is the
thing the version is about. `FORMAT_VERSION` moved 2 → 3 during Phase 5, and
[`docs/PHASE5.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PHASE5.md)
§1.2 records that the regions Phase 6 will fill were added in that same break so
it happens once rather than twice.

MSRV is **1.87**; see
[`SUPPORT.md`](https://github.com/NoeFontana/tf_tree/blob/main/SUPPORT.md).

## Where the rest of it is

[`docs/PROJECT.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PROJECT.md)
for the architecture and the decision log;
[`docs/PHASE1.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PHASE1.md)
for the normative layout and atomic orderings;
[`docs/PHASE2.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/PHASE2.md)
§2 for the shared-memory design.

## Licence

Dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE), at your option. See
[`NOTICE`](NOTICE).
