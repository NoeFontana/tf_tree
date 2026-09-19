#![no_std]
#![deny(unsafe_code)]
// `unsafe` boundary: raw arena memory, in `buffer` and `arena_view` only.
// See `docs/decisions/0007`.
#![deny(unsafe_op_in_unsafe_fn)]
// `PHASE1.md` §13.
#![deny(missing_docs)]
//! `no_std + alloc` single-process transform tree engine.
//!
//! The engine: frame interning, topology, edge records, seqlock sample buffers
//! ([`buffer`]), bracket search ([`sample`]), plans ([`plan`]), the participant
//! table ([`participant`]) and counters ([`counters`]). The builder and
//! convenience API live in `tf_tree`.
//!
//! # Stability — this crate's `pub` surface is not the project's API
//!
//! Only what `tf_tree` re-exports is promised (`docs/API.md` §2.6); the rest is
//! shaped by the arena (`docs/PHASE5.md` §1) and may change.
//!
//! ## The `#[non_exhaustive]` rule this crate applies
//!
//! It goes on a type the engine *produces* and a caller only reads; **not** on
//! one a caller must *dispatch on* ([`plan::InterpPolicy`], [`plan::Step`],
//! [`edge::EdgeKind`], [`topology::TopoLockError`]), where the forced `_ =>` arm
//! hides a wrong answer. `#[repr(C)]` arena records never carry it: a new field
//! is a `FORMAT_VERSION` / `layout_hash` event.
//!
//! # Load-bearing invariants
//!
//! Append-only ids, no pointers in the arena, fixed capacity, one writer per
//! edge, monotone head, non-decreasing stamps, little-endian fields, and all
//! allocation at construction: `docs/PHASE1.md` §2.
//!
//! # Unsafe
//!
//! `unsafe` is re-enabled only in [`buffer`] and [`arena_view`], each with a
//! module-level `// SAFETY:` block.
//!
//! # Concurrency abstraction (`loom`)
//!
//! Atomics come from `crate::sync`: `core` atomics, or `loom` under `--cfg loom`.

extern crate alloc;

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

// Not loom-modelable: loom atomics are not `repr(C)`.
#[cfg(not(loom))]
pub mod arena_view;
pub mod participant;
#[cfg(not(loom))]
pub mod plan;
// Default-off; `docs/API.md` §2.3 item 3.
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

/// Maximum length of a **compiled** plan: the [`plan::Step`] slots a [`plan::Plan`]
/// carries by value, counted after constant folding; not the walk's bound
/// ([`MAX_PATH_EDGES`]). Set by
/// [`0034`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0034-the-depth-bound-priced-two-slots-the-same.md).
pub const MAX_DEPTH: usize = 32;

/// Maximum number of **raw** path edges [`plan::compile`] walks, across both sides
/// of the lowest common ancestor. Exceeding it, or folding past [`MAX_DEPTH`], is
/// [`LookupError::TreeTooDeep`]; `depth` tells them apart (`docs/PHASE1.md` §7.1).
pub const MAX_PATH_EDGES: usize = 64;

#[cfg(all(test, loom))]
mod loom_tests;

#[cfg(all(test, not(loom)))]
mod tests;

// §11.3: abort tests need `crash-points`; recovery assertions run in every build.
#[cfg(all(test, not(loom)))]
mod crash_tests;
