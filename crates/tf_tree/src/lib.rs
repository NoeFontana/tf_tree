#![deny(unsafe_code)]
// `unsafe` boundary: **one lifetime extension, in [`OwnedWriter`], and nothing
// else** (`docs/decisions/0017`). It is the only one in the workspace; `deny`
// rather than `forbid` so `rg 'allow\(unsafe_code\)' crates/tf_tree/src` finds
// it. A second site needs its own record (`docs/decisions/0007`).
#![deny(unsafe_op_in_unsafe_fn)]
// `PHASE1.md` §13. Binds this repository's builds only: cargo caps lints on
// registry dependencies.
#![deny(missing_docs)]
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
//! # Minimum supported Rust version
//!
//! **1.87**, from `[workspace.package] rust-version`. `just msrv` fails if this
//! line, `README.md`, `SUPPORT.md` or any `rust-version` disagrees. `SUPPORT.md`
//! is the bump policy.
//!
//! # Two stability tiers
//!
//! Everything at this crate's root is **stable**. `tf_tree::unstable`, behind the
//! default-off `unstable` feature, is not; enabling the feature is the waiver
//! (`docs/API.md` §2.6). It holds what the *arena layout* shapes
//! (`docs/PHASE5.md` §1). To ask *what is in this tree?* on the stable tier use
//! [`Tree::frames`] and [`Tree::edges`] (`docs/API.md` §3.2).
//!
//! These pin that `ArenaView`, `EdgeKind` and `EdgeMeta` moved (fully only with
//! the feature on; `just test-doc-error-codes` is the real gate for `E0432`).
//! Three blocks, because one block passes when any one of the three is absent:
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
//! [`EdgeWriter`] still carries a lifetime (`docs/API.md` §2.1 names this);
//! [`OwnedWriter`] is the storable shape.
//!
//! # `no_std` / `std` split
//!
//! Everything arena-generic — [`Plan`], [`Step`], [`Guard`], [`Stamp`],
//! [`Domain`], [`Query`], the compile/evaluate engine — lives in the `no_std`
//! [`tf_tree_core`]. This crate adds only what needs `std`: the heap arena,
//! [`Tree::lookup`]'s per-thread plan cache (`thread_local!`), and
//! [`Described`]'s `Display`.
//!
//! [`Tree`] is `Send + Sync` but deliberately not `Clone`: it owns its arena
//! backing *and* a slot in the fixed-size participant table
//! (`DEFAULT_MAX_PARTICIPANTS`, 64). A clone would either burn a slot or report
//! two participants as one to the reaper.
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
//! `tf_tree_c` and PyO3 do the same (`docs/API.md` §2.2).
//!
//! # Set `lto = "thin"` and `codegen-units = 1` in your release profile
//!
//! ```toml
//! [profile.release]
//! lto = "thin"
//! codegen-units = 1
//! ```
//!
//! Worth about 25% of a depth-3 lookup: [`Plan::at`] and the fold beneath it sit
//! across crate boundaries from the consumer, and every published latency is
//! taken under whole-program optimisation. Measurements and `just embed-cost`:
//! `docs/API.md` §2.3. The cost is build time; how the 25% splits between the
//! two settings is not measured.

// The crates.io front page, compiled: `README.md`'s `rust` fence is gated for
// API, not output.
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

/// Test scaffolding for `docs/decisions/0005` §5's CAS-to-lease window. Absent
/// unless `--features test-hooks`; see [`tree::CLAIM_WINDOW_HOOK`].
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

/// This build's arena layout hash — the *geometry*, as distinct from the
/// *format version*'s set of fields. Both are checked on attach.
#[must_use]
pub fn arena_layout_hash() -> u32 {
    tf_tree_arena::layout_hash()
}

/// Whether this build compiled `docs/PHASE5.md` §5's diagnostic counters in.
///
/// Evaluated here because cargo unifies features across a workspace, so a
/// downstream `cfg!` would report the wrong crate's features.
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
// The payload of `OpenError::Rendezvous` and every type its variants carry, so a
// caller can dispatch without depending on `tf_tree_ipc`. Left out: `rustix`'s
// `Errno`. A `//` comment, because rustdoc would prepend a `///` to each item.
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use tf_tree_ipc::{
    EnvVar, HelloStatus, IpcError, LockRole, NameProblem, ProcError, ProcParseError,
    RuntimeDirSource, WireError,
};

/// Test scaffolding for `docs/decisions/0028` plan step 2's private reclamation
/// predicate. Absent unless `--features test-hooks`; see
/// [`open::reclamation_verdict_for_test`].
#[cfg(all(feature = "test-hooks", feature = "shm", target_os = "linux"))]
#[doc(hidden)]
pub use open::reclamation_verdict_for_test;

/// **The unstable tier — `docs/API.md` §2.6.** Enabling the `unstable` feature
/// is the waiver; read the module's own documentation for what it waives.
#[cfg(feature = "unstable")]
pub mod unstable;

// Re-export the core engine surface so downstream code depends only on
// `tf_tree`.

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
// The payload of every public error variant is nameable from here; it is on the
// stable tier because a stable variant already hands the type out
// (`docs/API.md` §2.6).
pub use tf_tree_arena::LayoutError;

// The math surface, including both interpolation kernels, so a consumer does not
// keep two direct dependencies in lockstep on a `0.0.x` line. The module is
// re-exported rather than a bare `screw_pow`, which would be a second spelling.
// `tests/math_reexports.rs` pins the list.
pub use tf_tree_math::dualquat;
pub use tf_tree_math::{
    exp_se3, exp_so3, log_se3, log_so3, quat_from_rot3, slerp, Interp, Iso3, LerpSlerp, Quat,
    ScLerp, Twist, Vec3,
};
