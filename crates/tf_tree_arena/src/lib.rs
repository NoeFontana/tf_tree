#![no_std]
// `unsafe` boundary: raw arena memory. See `docs/decisions/0007`.
#![deny(unsafe_op_in_unsafe_fn)]
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

// The test harness and `proptest` require `std`; the crate itself stays
// `no_std + alloc`.
#[cfg(test)]
extern crate std;

// **The crates.io front page, wired to the doctest harness.** `README.md` has
// no `rust` fence today and should not grow one lightly — it spends its length
// telling a reader to depend on `tf_tree` instead. This is here so that the
// first example anyone does add is *run*: no recipe parses a README, so a front
// page can go stale against the API it demonstrates with every gate green.
// `cfg(doctest)` keeps it out of `cargo doc`, which renders the docs above.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

// Private: it exports exactly one public item, and that item is re-exported at
// the crate root. A `pub mod` here would render in rustdoc as a module whose
// only content is a type readers already found as `tf_tree_arena::ShmError`.
#[cfg(all(feature = "shm", target_os = "linux"))]
mod check;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod frozen;
pub mod header;
pub mod heap;
pub mod layout;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod mapped;

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
