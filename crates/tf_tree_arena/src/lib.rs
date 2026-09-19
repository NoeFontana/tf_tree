#![no_std]
// `unsafe` boundary: raw arena memory. See `docs/decisions/0007`.
#![deny(unsafe_op_in_unsafe_fn)]
// `PHASE1.md` §13. Binds builds of this repository only: cargo caps lints on registry dependencies.
#![deny(missing_docs)]
//! `no_std + alloc` pointer-free arena abstraction and layout math for `tf_tree`.
//!
//! A single flat allocation with **no pointers** — only `u32` indices and byte
//! offsets from the base — so it is relocatable by `memcpy` and mappable into
//! another process unchanged.
//!
//! Backends: `HeapArena` (aligned heap allocation) and `MappedArena` (`memfd` + `mmap`).
//!
//! # Unsafe
//!
//! `unsafe` is permitted (raw arena access); every block carries a `// SAFETY:` comment.

extern crate alloc;

// The test harness and `proptest` require `std`.
#[cfg(test)]
extern crate std;

// Runs any `rust` fence in `README.md` as a doctest; no recipe parses a README otherwise.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

// Private: its one public item is re-exported at the crate root.
#[cfg(all(feature = "shm", target_os = "linux"))]
mod check;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod frozen;
pub mod header;
pub mod heap;
pub mod layout;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod mapped;
// `docs/decisions/0059`'s structural checks on an error's `Display`.
#[cfg(test)]
mod render_test;

#[cfg(all(feature = "shm", target_os = "linux"))]
pub use check::ShmError;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use frozen::{
    write_frozen, FrozenArena, FrozenError, FrozenHeader, ARENA_FILE_ALIGN, FROZEN_HEADER_SIZE,
    FROZEN_MAGIC,
};
pub use header::{
    pack_topo, unpack_topo, ArenaHeader, TopoLock, FORMAT_VERSION, TF_TREE_MAGIC, TOPO_BLOCKS,
};
pub use heap::{Arena, HeapArena};
pub use layout::{
    layout_hash, ArenaLayout, LayoutError, Region, DEFAULT_MAX_PARTICIPANTS, FRAME_HASH_STRIDE,
};
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use mapped::{AttachMode, MappedArena};
