#![no_std]
//! `no_std + alloc` pointer-free arena abstraction and layout math for `tf_tree`.
//!
//! The arena is a single flat allocation holding every record and ring buffer.
//! It contains **no pointers** — only `u32` element indices and byte offsets
//! relative to the arena base — so it is relocatable by `memcpy` and, in
//! Phase 2, mappable into another process unchanged.
//!
//! Phase 1 backs the arena with `HeapArena` (an aligned heap allocation).
//! Phase 2 adds `MappedArena` (`memfd` + `mmap`) as the *only* change required
//! to move to shared memory.
//!
//! # Unsafe
//!
//! `unsafe` is permitted in this crate (raw arena access). Every `unsafe` block
//! carries a `// SAFETY:` comment naming the invariant it relies on.

extern crate alloc;

// Modules are added by the Phase 1 `tf_tree_arena` implementation PR:
//   header, layout, heap.
