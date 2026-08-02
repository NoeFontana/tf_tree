#![forbid(unsafe_code)]
//! `std` facade for the `tf_tree` transform engine.
//!
//! Re-exports the [`tf_tree_core`] engine and adds the ergonomic, allocating
//! conveniences that do not belong in the `no_std` core: the [`TreeBuilder`] and
//! the [`Tree`] that owns a `HeapArena`, the plan-cached [`Tree::lookup`], and
//! [`Described`] — a `Display` wrapper that resolves error ids to frame names by
//! consulting the arena (the error type itself stays `Copy` and `no_std`).
//!
//! Most users depend on this crate, not on `tf_tree_core` directly.
//!
//! ```
//! use tf_tree::{TreeBuilder, InterpPolicy, Stamp, Iso3};
//!
//! // Topology is declared on the builder; `build()` sizes the arena from exactly
//! // these edges (static edges reserve no ring slots).
//! let tree = TreeBuilder::new()
//!     .static_edge("map", "odom", &Iso3::IDENTITY)
//!     .build()
//!     .expect("layout");
//!
//! // map -> odom is a static identity, so the lookup is identity at any time.
//! // A typed binding pins the default `SystemDomain` (method-call inference does
//! // not apply a type parameter's default, so annotate the stamp once).
//! let now: Stamp = Stamp::from_nanos(0);
//! let t = tree.lookup("map", "odom", now).unwrap();
//! assert_eq!(t, Iso3::IDENTITY);
//! # let _ = InterpPolicy::ScLerp;
//! ```
//!
//! # `no_std` / `std` split
//!
//! Everything arena-generic — [`Plan`], [`Step`], [`Guard`], [`Stamp`],
//! [`Domain`], [`Query`], the compile/evaluate engine — lives in the `no_std`
//! [`tf_tree_core`]. This crate adds only what needs `std`: the concrete [`Tree`]
//! owning a heap arena, the per-thread plan cache behind [`Tree::lookup`]
//! (`thread_local!`), and [`Described`]'s `Display`.
//!
//! # [`Tree`] is not `Clone`, and `Arc<Tree>` is the embedding idiom
//!
//! [`Tree`] is `Send + Sync`, so a shared reference is all a reader needs — but
//! it is deliberately not `Clone`, and the reason is that a `Tree` is not just a
//! handle. It owns its arena backing *and* holds a registered slot in the
//! arena's participant table — a fixed-size table (`DEFAULT_MAX_PARTICIPANTS`,
//! 64) sized when the arena is created, not an unbounded pool. A derived
//! `Clone` would have to pick one of two wrong answers: register a second slot,
//! and burn a scarce resource every time somebody passed a tree by value; or
//! share the first one, and report two participants as one to the reaper that
//! decides whether a slot's owner is still alive.
//!
//! So share it with an `Arc`:
//!
//! ```
//! use std::sync::Arc;
//! use tf_tree::{Iso3, Stamp, TreeBuilder};
//!
//! let tree = Arc::new(
//!     TreeBuilder::new()
//!         .static_edge("map", "odom", &Iso3::IDENTITY)
//!         .build()
//!         .expect("layout"),
//! );
//! let reader = Arc::clone(&tree);
//! let joined = std::thread::spawn(move || {
//!     let now: Stamp = Stamp::from_nanos(0);
//!     reader.lookup("map", "odom", now)
//! })
//! .join()
//! .expect("reader thread");
//! assert_eq!(joined.unwrap(), Iso3::IDENTITY);
//! ```
//!
//! This is not new advice, which is the point of writing it down: `tests/tsan.rs`
//! shares a tree between threads this way, `tf_tree_c` hands out
//! `Arc<TreeShare>` (a one-field wrapper around a `Tree`, so the refcount is on
//! the wrapper rather than on the `Tree` itself), and PyO3's `Py<PyTree>` is the
//! same refcount spelled in CPython's allocator. Three surfaces arrived here
//! independently and none of them said so where an embedder would look
//! (`docs/API.md` §2.2).

mod cache;
mod tree;

/// The `.tft` manifest's encoder (`docs/PHASE5.md` §2.3).
#[cfg(all(feature = "shm", target_os = "linux"))]
mod cbor;
/// The frozen `.tft` arena (`docs/PHASE5.md` §2).
#[cfg(all(feature = "shm", target_os = "linux"))]
mod frozen;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use frozen::FrozenFileError;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use tf_tree_arena::{FrozenError, FrozenHeader, ARENA_FILE_ALIGN};

pub use tree::{
    BuildError, Capacity, ClaimApiError, Described, EdgeCfg, EdgeWriter, ReparentError, Tree,
    TreeBuilder,
};

/// Test scaffolding for `docs/decisions/0005` §5's CAS-to-lease window. Absent
/// unless `--features test-hooks`; see [`tree::CLAIM_WINDOW_HOOK`].
#[cfg(all(feature = "test-hooks", feature = "shm", target_os = "linux"))]
#[doc(hidden)]
pub use tree::CLAIM_WINDOW_HOOK;

/// Shared-memory attachment surface (Phase 2). Linux-only, behind `--features shm`.
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use tf_tree_arena::{AttachMode, ShmError};

/// This build's arena format version (`docs/PHASE5.md` §1).
///
/// Re-exported as a function rather than the constant so the facade keeps its
/// `#![forbid(unsafe_code)]` promise of exposing no arena internals: a caller
/// gets the number it needs for a diagnostic without a path into
/// `tf_tree_arena`.
#[must_use]
pub fn arena_format_version() -> u32 {
    tf_tree_arena::FORMAT_VERSION
}

/// This build's arena layout hash — the *geometry*, as distinct from the
/// *format version*'s set of fields. Both are checked on attach.
#[must_use]
pub fn arena_layout_hash() -> u32 {
    tf_tree_arena::layout_hash()
}

/// Whether this build compiled `docs/PHASE5.md` §5's diagnostic counters in.
///
/// A diagnostic that reads `EdgeCounters` cannot otherwise tell "nothing
/// failed" from "nothing was counted", and those two answers call for opposite
/// actions. It has to be evaluated *here*, inside the crate that owns the
/// feature: cargo unifies features across a workspace, so a `cfg!` in a
/// downstream crate reports what that crate asked for rather than what the
/// engine was built with.
#[must_use]
pub fn counters_compiled_in() -> bool {
    cfg!(feature = "counters")
}

/// Zero-config rendezvous (`docs/PHASE2.md` §3.2, `docs/decisions/0005`).
#[cfg(all(feature = "shm", target_os = "linux"))]
mod open;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use open::{open, CreatePolicy, Open, OpenError};

// Re-export the core engine surface so downstream code depends only on `tf_tree`.

/// Raw, read-only access to the arena's own tables.
///
/// # Stability
///
/// **This export is CLI-facing, and it moves behind an `unstable` feature
/// before any published tag** (`docs/API.md` §2.6). C has two headers and the
/// split *is* the promise; Rust has one visibility tier, so everything `pub`
/// here reads as a stability commitment whether it was meant as one or not.
/// This one was not: it exists so `tf_tree doctor` and `tf_tree top` can render
/// what is in a segment without depending on `tf_tree_core` directly, and its
/// shape follows the arena layout — which `docs/PHASE5.md` §1 is about to
/// change on purpose.
///
/// The `tf_tree::unstable::*` mirror itself is **deferred while the crate is
/// private** and is not to be built ahead of a reason to. The heading is not
/// deferred, because it costs a comment and buys the difference between a move
/// that executes a documented plan and a move that breaks somebody who had no
/// way to know.
pub use tf_tree_core::arena_view::ArenaView;

/// Whether an edge is dynamic, static or tombstoned.
///
/// # Stability
///
/// **CLI-facing, and it moves behind `unstable` with [`ArenaView`]** — read
/// that item's note for the argument. It is here for the same consumer and is
/// reachable only through the same door: the value comes from an `EdgeRecord`,
/// which is an [`ArenaView`] read. An embedder declares an edge's kind by
/// calling [`TreeBuilder::static_edge`] or [`TreeBuilder::dynamic_edge`] and
/// never names this type. ([`EdgeMeta`] carries one, but its only consumer is
/// `tf_tree_core::compile`, which this facade does not re-export.)
pub use tf_tree_core::edge::EdgeKind;

pub use tf_tree_core::edge::Publisher;
pub use tf_tree_core::layout::{write_affine32, write_mat4, write_quat, write_quat_twist, Layout};
pub use tf_tree_core::plan::{
    AdaptiveScratch, Domain, EdgeMeta, ErrBound, Guard, InterpPolicy, Plan, Query, Sample,
    SensorDomain, SimDomain, Stamp, SteadyDomain, Step, SystemDomain, MAX_ADAPTIVE_DEPTH,
    MAX_KNOTS,
};
pub use tf_tree_core::{
    ClaimError, EdgeId, FrameError, FrameId, LookupError, PushError, MAX_DEPTH,
};

pub use tf_tree_math::{
    exp_se3, exp_so3, log_se3, log_so3, quat_from_rot3, Interp, Iso3, LerpSlerp, Quat, ScLerp,
    Twist, Vec3,
};
