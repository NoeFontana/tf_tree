#![no_std]
#![deny(unsafe_code)]
// `unsafe` boundary: raw arena memory, in `buffer` and `arena_view` only.
// See `docs/decisions/0007`.
#![deny(unsafe_op_in_unsafe_fn)]
// `PHASE1.md` §13. Binds this repository and its path dependents only: cargo
// caps lints on registry dependencies.
#![deny(missing_docs)]
//! `no_std + alloc` single-process transform tree engine.
//!
//! The engine: frame interning, topology, edge records, seqlock sample buffers
//! ([`buffer`]), bracket search ([`sample`]), plan compilation and evaluation
//! ([`plan`]), the participant table ([`participant`]) and diagnostic counters
//! ([`counters`]), backed by a heap allocation so Phase 2's `MappedArena` is a
//! one-line swap. `TreeBuilder` and the convenience API live in the `tf_tree`
//! facade.
//!
//! # Stability — this crate's `pub` surface is not the project's API
//!
//! **`tf_tree` is the stable surface** (`docs/API.md` §2.6, applied one crate
//! down).
//!
//! * **What `tf_tree` re-exports is the promise**: [`plan`]'s `Plan`, `Guard`,
//!   `Stamp`, `Query`, the [`error`] types, [`layout`], and [`ParticipantError`]
//!   (the payload of `tf_tree::BuildError::Participant`).
//! * **Everything else is shaped by the arena**, which changes (`docs/PHASE5.md`
//!   §1): [`arena_view`], [`buffer`], [`frame`], [`edge`]'s records,
//!   [`participant`], [`counters`], [`topology`]. Depend on them and expect to be
//!   rebuilt.
//!
//! ## The `#[non_exhaustive]` rule this crate applies
//!
//! `#[non_exhaustive]` goes on a type the engine *produces* and a caller only
//! *reads*, or *builds through a constructor*; **not** on a type a caller must
//! *dispatch on*, where the forced `_ =>` arm turns a compile error into a silent
//! wrong answer.
//!
//! Carrying it: every error enum a caller sees, [`plan::Query`],
//! [`layout::Layout`], [`plan::Sample`], [`plan::ErrBound`] (with
//! [`plan::ErrBound::new`]), [`sample::ExtrapPolicy`]. Deliberately without it,
//! each with the argument at the type: [`plan::InterpPolicy`], [`plan::Step`],
//! [`edge::EdgeKind`], [`topology::TopoLockError`].
//!
//! The `#[repr(C)]` arena records ([`edge::EdgeRecord`], [`edge::ClaimRecord`],
//! [`frame::FrameRecord`], [`participant::ParticipantRecord`],
//! [`buffer::PoseSlot`], [`counters::EdgeCounters`],
//! [`counters::ParticipantCounters`]) are deliberately not: a new field is a
//! `FORMAT_VERSION` / `layout_hash` event, checked on every attach.
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
//! Atomics come from `crate::sync`: `core::sync::atomic`, or `loom::sync::atomic`
//! under `--cfg loom`. The publish/read/claim/intern algorithms compile in both;
//! the arena-byte views are `#[cfg(not(loom))]` and loom tests use heap instances.

extern crate alloc;

// proptest, loom and `crash`'s env read (`docs/PHASE2.md` §11.3) need `std`, via
// `extern crate` under a `cfg`, not a feature (features unify across the graph).
#[cfg(any(test, feature = "crash-points"))]
extern crate std;

// Compiles any `rust` fence in `README.md` as a doctest.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

pub mod buffer;
/// Consumer-side diagnostic counters (`docs/PHASE5.md` §5).
pub mod counters;
pub mod crash;
pub mod edge;
pub mod error;
pub mod frame;
/// Output layouts for folding results straight into a caller's buffer.
pub mod layout;
pub mod sample;

pub(crate) mod sync;

// Not loom-modelable (loom atomics are not `repr(C)`; no narrow-width atomics).
#[cfg(not(loom))]
pub mod arena_view;
// Model-checkable: `loom_tests` drives the real `register`/`release`.
pub mod participant;
// Depends on `arena_view`/`topology`, so `not(loom)`.
#[cfg(not(loom))]
pub mod plan;
// Default-off; `docs/API.md` §2.3 item 3's gated row needs the in-crate path
// compiled here (see the module docs). Like `tf_tree_c`'s `test-hooks`.
#[cfg(all(feature = "bench-probe", not(loom)))]
pub mod bench_probe;
#[cfg(not(loom))]
pub mod topology;

pub use error::{ClaimError, EdgeId, FrameError, FrameId, LookupError, PushError, TopologyError};
pub use participant::{ParticipantError, ParticipantRecord, ParticipantTable};

#[cfg(not(loom))]
pub use plan::{
    compile, AdaptiveScratch, Domain, EdgeMeta, ErrBound, Extrapolated, Guard, InterpPolicy, Plan,
    Query, Sample, SensorDomain, SimDomain, Stamp, SteadyDomain, Step, SystemDomain,
    MAX_ADAPTIVE_DEPTH, MAX_KNOTS,
};
pub use sample::ExtrapPolicy;

/// Maximum length of a **compiled** plan: the number of [`plan::Step`] slots a
/// [`plan::Plan`] carries, counted *after* constant folding.
///
/// A slot is 64 bytes (`size_of::<Step>()`, `0042`); every `Plan` carries
/// `MAX_DEPTH` of them by value, which is why it is not the walk's bound
/// ([`MAX_PATH_EDGES`]). Set to 32 by
/// [`0034`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0034-the-depth-bound-priced-two-slots-the-same.md);
/// survey in `docs/PHASE1.md` §7.1, *Two bounds, and they price different slots*.
pub const MAX_DEPTH: usize = 32;

/// Maximum number of **raw** path edges [`plan::compile`] will walk, counted
/// across both sides of the lowest common ancestor before folding.
///
/// A slot is a `u32` edge id in `compile`'s stack frame (`docs/PHASE1.md` §7.1).
///
/// Exceeding it, or folding to more than [`MAX_DEPTH`] steps, is
/// [`LookupError::TreeTooDeep`]; `depth` tells them apart. 64 is ~1.9x the
/// surveyed floor and not 256, which would set the worst accepted compile latency
/// ~4x higher (refused pairs are not cached).
pub const MAX_PATH_EDGES: usize = 64;

#[cfg(all(test, loom))]
mod loom_tests;

#[cfg(all(test, not(loom)))]
mod tests;

// §11.3: abort tests need `crash-points`; recovery assertions run in every build.
#[cfg(all(test, not(loom)))]
mod crash_tests;
