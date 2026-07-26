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

mod cache;
mod tree;

pub use tree::{
    BuildError, Capacity, ClaimApiError, Described, EdgeCfg, EdgeWriter, ReparentError, Tree,
    TreeBuilder,
};

/// Shared-memory attachment surface (Phase 2). Linux-only, behind `--features shm`.
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use tf_tree_arena::{AttachMode, ShmError};

/// Zero-config rendezvous (`docs/PHASE2.md` §3.2, `docs/decisions/0005`).
#[cfg(all(feature = "shm", target_os = "linux"))]
mod open;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use open::{open, CreatePolicy, Open, OpenError};

// Re-export the core engine surface so downstream code depends only on `tf_tree`.
pub use tf_tree_core::arena_view::ArenaView;
pub use tf_tree_core::edge::{EdgeKind, Publisher};
pub use tf_tree_core::layout::{write_affine32, write_mat4, write_quat, Layout};
pub use tf_tree_core::plan::{
    AdaptiveScratch, Domain, EdgeMeta, ErrBound, Guard, InterpPolicy, Plan, Query, SensorDomain,
    Stamp, Step, SystemDomain, MAX_ADAPTIVE_DEPTH, MAX_KNOTS,
};
pub use tf_tree_core::{
    ClaimError, EdgeId, FrameError, FrameId, LookupError, PushError, MAX_DEPTH,
};

pub use tf_tree_math::{
    exp_se3, exp_so3, log_se3, log_so3, Interp, Iso3, LerpSlerp, Quat, ScLerp, Vec3,
};
