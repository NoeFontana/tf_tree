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

// The test harness and `proptest` require `std`; the crate itself stays
// `no_std + alloc`.
#[cfg(test)]
extern crate std;

pub mod header;
pub mod heap;
pub mod layout;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod mapped;

pub use header::{
    pack_topo, unpack_topo, ArenaHeader, TopoLock, FORMAT_VERSION, TF_TREE_MAGIC, TOPO_BLOCKS,
};
pub use heap::{Arena, HeapArena};
pub use layout::{layout_hash, ArenaLayout, LayoutError, Region, DEFAULT_MAX_PARTICIPANTS};
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use mapped::{AttachMode, MappedArena, ShmError};
