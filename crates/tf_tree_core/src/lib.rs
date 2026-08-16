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
//! # Stability — this crate's `pub` surface is not the project's API
//!
//! **`tf_tree` is the stable surface; this is the engine underneath it.** The
//! split is `docs/API.md` §2.6 applied one crate down: that section's problem is
//! that Rust has a single visibility tier, so everything `pub` reads as a
//! semver promise whether it was meant as one or not. The facade answers it with
//! a `tf_tree::unstable` module behind a feature. This crate cannot — it is a
//! dependency of the facade and has to be published for the facade to be — so it
//! answers with a statement instead, which is the honest form of the same thing:
//!
//! * **What `tf_tree` re-exports is the promise.** [`plan`]'s `Plan`, `Guard`,
//!   `Stamp`, `Query`, the [`error`] types, [`layout`] — those are the API, and
//!   they are stable because their shape is the *engine's* contract.
//! * **Everything else here is shaped by the arena**, and the arena is scheduled
//!   to change: `docs/PHASE5.md` §1 bumps `FORMAT_VERSION` to 3 and adds regions
//!   Phase 6 fills. [`arena_view`], [`buffer`], [`frame`], [`edge`]'s records,
//!   [`participant`], [`counters`] and [`topology`] move with it. Depend on them
//!   and expect to be rebuilt; that is what `tf_tree::unstable` says out loud
//!   for the two of them the facade used to re-export.
//!
//! ## The `#[non_exhaustive]` rule this crate applies
//!
//! Stated once, because a pre-tag audit went type by type and the *decisions*
//! are worth less than the rule that produced them. `#[non_exhaustive]` is free
//! to add before a published tag and a major bump after, so the default is to
//! add it — but not everywhere, because it is not free of consequence:
//!
//! > **It goes on a type the engine *produces* and a caller only *reads*, or one
//! > a caller *builds through a constructor*. It does not go on a type a caller
//! > must *dispatch on*.**
//!
//! The reason is what the forced `_ =>` arm does. On a produced type there is no
//! arm, so growth costs a downstream crate nothing. On a dispatched type the arm
//! has to have a body, and every honest body is a lie about a variant that did
//! not exist when it was written — so the attribute converts a compile error
//! saying "teach me the new case" into a silent wrong answer. A major version
//! bump is the cheaper of those two.
//!
//! Carrying it: every error enum a caller sees, [`plan::Query`],
//! [`layout::Layout`], [`plan::Sample`], [`plan::ErrBound`] (with
//! [`plan::ErrBound::new`]), [`sample::ExtrapPolicy`]. Deliberately without it,
//! each with the argument at the type: [`plan::InterpPolicy`], [`plan::Step`],
//! [`edge::EdgeKind`], [`topology::TopoLockError`].
//!
//! **The `#[repr(C)]` arena records are deliberately not `#[non_exhaustive]`** —
//! [`edge::EdgeRecord`], [`edge::ClaimRecord`], [`frame::FrameRecord`],
//! [`participant::ParticipantRecord`], [`buffer::PoseSlot`],
//! [`counters::EdgeCounters`], [`counters::ParticipantCounters`]. A field
//! appended to one of those is not a source-compatibility event, it is a
//! `FORMAT_VERSION` / `layout_hash` event, and that is already checked on every
//! attach by a mechanism stronger than the type system. Marking them would claim
//! a growth path the arena does not have while blocking the literal construction
//! the builders use.
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

// **The crates.io front page, wired to the doctest harness.** `README.md` has
// no `rust` fence today — it is a page about *not* depending on this crate
// unless you are `no_std` — but nothing parses a README, so an example added
// there later would be the one piece of published documentation no recipe
// compiles. This makes the first one a doctest. `cfg(doctest)` keeps it out of
// `cargo doc`, which renders the module docs above.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

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
// **Default-off, and no shipped configuration turns it on.** `docs/API.md` §2.3
// item 3's gated row compares the facade path called from a separate crate
// against the in-crate path, and the in-crate half has to be compiled here —
// see the module's own docs for the measurement that rules out putting it in
// the facade instead. Same pattern as `tf_tree_c`'s `test-hooks`.
#[cfg(all(feature = "bench-probe", not(loom)))]
pub mod bench_probe;
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
