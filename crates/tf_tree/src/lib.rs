#![deny(unsafe_code)]
// `unsafe` boundary: one lifetime extension, in [`OwnedWriter`] (`docs/decisions/0017`);
// `deny` not `forbid` so it is greppable. A second site needs a record (`0007`).
#![deny(unsafe_op_in_unsafe_fn)]
// `PHASE1.md` §13.
#![deny(missing_docs)]
//! `std` facade for the `tf_tree` transform engine.
//!
//! Re-exports the [`tf_tree_core`] engine and adds the allocating conveniences:
//! [`TreeBuilder`], the [`Tree`] that owns a `HeapArena`, the plan-cached
//! [`Tree::lookup`], and [`Described`], a `Display` wrapper that resolves error
//! ids to frame names.
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
//! # Minimum supported Rust version
//!
//! **1.87**, from `[workspace.package] rust-version`; `just msrv` checks this line
//! against `README.md`, `SUPPORT.md` and every `rust-version`.
//!
//! # Two stability tiers
//!
//! The crate root is **stable**. `tf_tree::unstable`, behind the default-off
//! `unstable` feature, holds what the arena layout shapes (`docs/API.md` §2.6,
//! `docs/PHASE5.md` §1). On the stable tier use [`Tree::frames`] and
//! [`Tree::edges`] (`docs/API.md` §3.2).
//!
//! These pin that `ArenaView`, `EdgeKind` and `EdgeMeta` moved (gated by
//! `just test-doc-error-codes`); one block each, so one absence cannot pass:
//!
//! ```compile_fail,E0432
//! use tf_tree::ArenaView;
//! ```
//! ```compile_fail,E0432
//! use tf_tree::EdgeKind;
//! ```
//! ```compile_fail,E0432
//! use tf_tree::EdgeMeta;
//! ```
//!
//! [`EdgeWriter`] carries a lifetime (`docs/API.md` §2.1); [`OwnedWriter`] is the
//! storable shape.
//!
//! # `no_std` / `std` split
//!
//! Arena-generic items ([`Plan`], [`Step`], [`Guard`], [`Stamp`], [`Domain`],
//! [`Query`]) live in the `no_std` [`tf_tree_core`]; this crate adds the heap
//! arena, [`Tree::lookup`]'s per-thread plan cache and [`Described`].
//!
//! [`Tree`] is `Send + Sync` but not `Clone`: it owns its arena backing and a
//! participant-table slot (`DEFAULT_MAX_PARTICIPANTS`). Share it with an `Arc`:
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
//! `docs/API.md` §2.2.
//!
//! # Set `lto = "thin"` and `codegen-units = 1` in your release profile
//!
//! ```toml
//! [profile.release]
//! lto = "thin"
//! codegen-units = 1
//! ```
//!
//! Worth about 25% of a depth-3 lookup, since [`Plan::at`] crosses a crate
//! boundary; see `docs/API.md` §2.3.

// The crates.io front page, compiled.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

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
    AwaitError, BuildError, Capacity, ClaimApiError, Described, EdgeCfg, EdgeWriter, OwnedWriter,
    ReparentError, Tree, TreeBuilder,
};

/// Test scaffolding for `docs/decisions/0005` §5's CAS-to-lease window
/// (`test-hooks`); see [`tree::CLAIM_WINDOW_HOOK`].
#[cfg(all(feature = "test-hooks", feature = "shm", target_os = "linux"))]
#[doc(hidden)]
pub use tree::CLAIM_WINDOW_HOOK;

/// Shared-memory attachment surface (Phase 2). Linux-only, behind `--features shm`.
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use tf_tree_arena::{AttachMode, ShmError};

/// This build's arena format version (`docs/PHASE5.md` §1).
#[must_use]
pub fn arena_format_version() -> u32 {
    tf_tree_arena::FORMAT_VERSION
}

/// This build's arena layout hash (geometry); checked on attach with the format version.
#[must_use]
pub fn arena_layout_hash() -> u32 {
    tf_tree_arena::layout_hash()
}

/// Whether this build compiled `docs/PHASE5.md` §5's diagnostic counters in.
///
/// Evaluated here because a downstream `cfg!` would see the wrong crate's features.
#[must_use]
pub fn counters_compiled_in() -> bool {
    cfg!(feature = "counters")
}

/// Zero-config rendezvous (`docs/PHASE2.md` §3.2, `docs/decisions/0005`).
#[cfg(all(feature = "shm", target_os = "linux"))]
mod open;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use open::Inheritance;
#[cfg(all(feature = "crash-points", feature = "shm", target_os = "linux"))]
pub use open::CRASH_SITES;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use open::{open, CreatePolicy, Open, OpenError};
// Payload of `OpenError::Rendezvous` and its variants' types (not `rustix`'s `Errno`).
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use tf_tree_ipc::{
    EnvVar, HelloStatus, IpcError, LockRole, NameProblem, ProcError, ProcParseError,
    RuntimeDirSource, WireError,
};

/// Test scaffolding for `docs/decisions/0028` step 2 (`test-hooks`); see
/// [`open::reclamation_verdict_for_test`].
#[cfg(all(feature = "test-hooks", feature = "shm", target_os = "linux"))]
#[doc(hidden)]
pub use open::reclamation_verdict_for_test;

/// **The unstable tier** (`docs/API.md` §2.6); enabling `unstable` is the waiver.
#[cfg(feature = "unstable")]
pub mod unstable;

pub use tf_tree_core::edge::Publisher;
pub use tf_tree_core::layout::{write_affine32, write_mat4, write_quat, write_quat_twist, Layout};
pub use tf_tree_core::plan::{
    AdaptiveScratch, Domain, ErrBound, Extrapolated, Guard, InterpPolicy, Plan, Query, Sample,
    SensorDomain, SimDomain, Stamp, SteadyDomain, Step, SystemDomain, MAX_ADAPTIVE_DEPTH,
    MAX_KNOTS,
};
// `ExtrapPolicy`: reached by `Plan::at_extrapolating` (`0039`).
pub use tf_tree_core::sample::ExtrapPolicy;
pub use tf_tree_core::{
    ClaimError, EdgeId, FrameError, FrameId, LookupError, ParticipantError, PushError,
    TopologyError, MAX_DEPTH, MAX_PATH_EDGES,
};
// Payload of a stable error variant (`docs/API.md` §2.6).
pub use tf_tree_arena::LayoutError;

// The math surface; `tests/math_reexports.rs` pins the list.
pub use tf_tree_math::dualquat;
pub use tf_tree_math::{
    exp_se3, exp_so3, log_se3, log_so3, quat_from_rot3, slerp, Interp, Iso3, LerpSlerp, Quat,
    ScLerp, Twist, Vec3,
};
