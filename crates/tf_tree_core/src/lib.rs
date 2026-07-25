#![no_std]
#![deny(unsafe_code)]
//! `no_std + alloc` single-process transform tree engine.
//!
//! This is the source of truth: frame interning, topology, edge records, the
//! seqlock sample buffers, plan compilation, and typed errors. It is the
//! shared-memory design (Phase 1) backed by a heap allocation — every layout
//! decision exists so Phase 2's `MappedArena` is a one-line swap.
//!
//! This PR implements steps 4 and 5 of `docs/PHASE1.md`'s implementation order
//! (see its appendix): the concurrency core —
//! frame interning ([`frame`]), topology ([`topology`]), edge records and the
//! claim table ([`edge`]), the seqlock sample buffer ([`buffer`]), and bracket
//! search ([`sample`]). Plan compilation, `Guard`, `at`/`at_many`, `TreeBuilder`
//! and the public convenience API are the next PR; the internal primitives they
//! call live here.
//!
//! # Load-bearing invariants
//!
//! 1. **Append-only identity.** `FrameId`/`EdgeId` are never reused; removal is
//!    tombstoning. A stale `Plan` may index a valid record but never go out of
//!    bounds.
//! 2. **No pointers in the arena.** Every intra-arena reference is a `u32`.
//! 3. **Fixed capacity.** Set at construction; ring capacities are powers of two.
//! 4. **Single writer per edge.** Enforced by the claim table.
//! 5. **Monotone head.** Never masked in storage, only at access.
//! 6. **Stamps non-decreasing per edge.**
//! 7. **Little-endian arena fields** (asserted at construction).
//! 8. **Every heap allocation happens at construction.**
//!
//! # Unsafe
//!
//! The crate is `#![deny(unsafe_code)]`; `unsafe` is re-enabled only in the two
//! modules that reinterpret arena bytes ([`buffer`] and [`arena_view`]), each
//! carrying a module-level `// SAFETY:` block. Everything else is safe.
//!
//! # Concurrency abstraction (`loom`)
//!
//! All atomics are imported from `crate::sync`, which is `core::sync::atomic`
//! normally and `loom::sync::atomic` under `--cfg loom`. The publish/read/claim/
//! intern algorithms compile unchanged in both modes; the arena-byte views are
//! `#[cfg(not(loom))]` (loom atomics cannot live in mapped bytes), and the loom
//! tests drive the shared algorithms over heap-allocated instances.

extern crate alloc;

// proptest and loom require `std`; the crate itself stays `no_std + alloc`.
#[cfg(test)]
extern crate std;

pub mod buffer;
pub mod edge;
pub mod error;
pub mod frame;
pub mod sample;

pub(crate) mod sync;

// The arena-byte views and the `AtomicU16`-backed topology block cannot be
// modeled by loom (loom atomics are not `repr(C)` and loom does not provide the
// narrow-width atomics the topology depth array uses). They compile only in the
// production configuration; the loom tests reimplement the protocols they need.
#[cfg(not(loom))]
pub mod arena_view;
// Plan compilation, typed time, and evaluation. Depends on `arena_view`/
// `topology` (production-only), so it is `not(loom)`; the loom suite exercises
// the concurrency core beneath it, not the plan layer.
#[cfg(not(loom))]
pub mod participant;
#[cfg(not(loom))]
pub mod plan;
#[cfg(not(loom))]
pub mod topology;

pub use error::{ClaimError, EdgeId, FrameError, FrameId, LookupError, PushError, TopologyError};
#[cfg(not(loom))]
pub use participant::{ParticipantError, ParticipantRecord, ParticipantTable};

#[cfg(not(loom))]
pub use plan::{
    compile, AdaptiveScratch, Domain, EdgeMeta, ErrBound, Guard, InterpPolicy, Plan, Query,
    SensorDomain, Stamp, Step, SystemDomain, MAX_ADAPTIVE_DEPTH, MAX_KNOTS,
};

/// Maximum combined path depth of a compiled plan (used by the next PR). Real
/// trees are 4–8; 16 is generous. Declared here so the fixed step array and the
/// [`LookupError::TreeTooDeep`] bound share one constant.
pub const MAX_DEPTH: usize = 16;

#[cfg(all(test, loom))]
mod loom_tests;

#[cfg(all(test, not(loom)))]
mod tests;
