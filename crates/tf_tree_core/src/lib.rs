#![no_std]
#![deny(unsafe_code)]
// `unsafe` boundary: raw arena memory, in `buffer` and `arena_view` only.
// See `docs/decisions/0007`.
#![deny(unsafe_op_in_unsafe_fn)]
//! `no_std + alloc` single-process transform tree engine.
//!
//! This is the source of truth: frame interning, topology, edge records, the
//! seqlock sample buffers, plan compilation, and typed errors. It is the
//! shared-memory design (Phase 1) backed by a heap allocation — every layout
//! decision exists so Phase 2's `MappedArena` is a one-line swap.
//!
//! The whole of `docs/PHASE1.md`'s implementation order lives here: the
//! concurrency core — frame interning ([`frame`]), topology ([`topology`]),
//! edge records and the claim table ([`edge`]), the seqlock sample buffer
//! ([`buffer`]), bracket search ([`sample`]) — and, above it, plan compilation
//! and evaluation ([`plan`]: [`plan::Plan`], [`plan::Guard`], `at`/`at_many`).
//! Phase 2's participant table ([`participant`]) and Phase 5's diagnostic
//! counters ([`counters`]) sit alongside them. `TreeBuilder` and the public
//! convenience API live in the `tf_tree` facade, which drives the primitives
//! here.
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
/// Consumer-side diagnostic counters (`docs/PHASE5.md` §5).
pub mod counters;
pub mod edge;
pub mod error;
pub mod frame;
/// Output layouts for folding results straight into a caller's buffer.
pub mod layout;
pub mod sample;

pub(crate) mod sync;

// The arena-byte views and the `AtomicU16`-backed topology block cannot be
// modeled by loom (loom atomics are not `repr(C)` and loom does not provide the
// narrow-width atomics the topology depth array uses). They compile only in the
// production configuration; the loom tests reimplement the protocols they need.
#[cfg(not(loom))]
pub mod arena_view;
// Builds on `crate::sync` and nothing arena-shaped, so unlike its neighbours it
// *is* model-checkable: `loom_tests` drives the real `register`/`release` to
// check the slot-handover race, which no single-threaded test can reach.
pub mod participant;
// Plan compilation, typed time, and evaluation. Depends on `arena_view`/
// `topology` (production-only), so it is `not(loom)`; the loom suite exercises
// the concurrency core beneath it, not the plan layer.
#[cfg(not(loom))]
pub mod plan;
#[cfg(not(loom))]
pub mod topology;

pub use error::{ClaimError, EdgeId, FrameError, FrameId, LookupError, PushError, TopologyError};
pub use participant::{ParticipantError, ParticipantRecord, ParticipantTable};

#[cfg(not(loom))]
pub use plan::{
    compile, AdaptiveScratch, Domain, EdgeMeta, ErrBound, Guard, InterpPolicy, Plan, Query, Sample,
    SensorDomain, SimDomain, Stamp, SteadyDomain, Step, SystemDomain, MAX_ADAPTIVE_DEPTH,
    MAX_KNOTS,
};

/// Maximum combined path depth of a compiled plan (used by the next PR). Real
/// trees are 4–8; 16 is generous. Declared here so the fixed step array and the
/// [`LookupError::TreeTooDeep`] bound share one constant.
pub const MAX_DEPTH: usize = 16;

#[cfg(all(test, loom))]
mod loom_tests;

#[cfg(all(test, not(loom)))]
mod tests;
