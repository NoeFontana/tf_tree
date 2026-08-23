#![deny(unsafe_code)]
// `unsafe` boundary: **one lifetime extension, in [`OwnedWriter`], and nothing
// else.** `EdgeWriter<'a>` borrows the `Tree`; `OwnedWriter` stores an
// `Arc<Tree>` beside it and extends that borrow to `'static`, with the strong
// reference — not a comment — as the thing that keeps the arena alive.
// See `docs/decisions/0017`, which records why the facade takes the block
// rather than each binding hand-rolling it (two did; one of them leaked a claim
// lease and bypassed the fork guard for the life of every Python publisher).
// **Those two hand-rolled helpers are gone**: that record's steps 6–7 deleted
// them, and both bindings now claim through `Tree::claim_owned`. So this is the
// only lifetime extension in the workspace — not merely in this crate — and a
// second one anywhere is a new decision record rather than a patch.
//
// This is `deny` rather than `forbid` so that the one site can `#[allow]`
// itself and be *visible* — `rg 'allow\(unsafe_code\)' crates/tf_tree/src`
// returns it and should return nothing else. A second site is a new kind of
// boundary and needs its own record (`docs/decisions/0007`).
#![deny(unsafe_op_in_unsafe_fn)]
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
//! **1.87**. It is declared in `[workspace.package] rust-version` and repeated
//! here because a manifest is not somewhere a user reads: the person deciding
//! whether they can adopt this crate opens the docs, and `cargo` refusing to
//! build is a worse way to find out. `just msrv` builds `--locked` on exactly
//! that toolchain and fails if this line, `README.md`, `SUPPORT.md` or any
//! hand-written `rust-version` disagrees with the manifest.
//!
//! An MSRV bump is a minor-version bump pre-1.0 and a breaking change after —
//! `SUPPORT.md` is the policy, including why each of the two steps so far was
//! forced by a dependency rather than chosen.
//!
//! # Two stability tiers
//!
//! Everything at this crate's root is the **stable** surface: at a published tag
//! each `pub` item is a semver promise. The `tf_tree::unstable` module — behind
//! the default-off `unstable` feature, so it is absent from these docs unless
//! that feature is on — is not, and enabling the feature is the waiver
//! (`docs/API.md` §2.6). It mirrors the C ABI's `tf_tree.h` /
//! `tf_tree_unstable.h` split, which is the same promise spelled as two headers.
//!
//! What lives there is what the *arena layout* shapes, because that layout is
//! scheduled to change (`docs/PHASE5.md` §1). If you are reading transforms, you
//! will never need it.
//!
//! **Gating a door is not the same as removing a room.** The question the gated
//! `Tree::arena_view` used to be the only Rust answer to — *what is in this
//! tree?* — is answered on the stable tier by [`Tree::frames`] and
//! [`Tree::edges`], which mirror Python's `tree.frames()` / `tree.edges()`
//! (`docs/API.md` §3.2). Names only: the statistics half is `docs/PHASE5.md`
//! §4.2's and is held back on every surface until §3's counting pass. Enabling
//! `unstable` buys the arena-shaped *spelling* of that answer — record fields,
//! capacities, counters — not the answer itself.
//!
//! The three items that moved do not answer at the crate root any more, and this
//! is what pins that — **but only when the feature is on**, and where that holds
//! moved in 0.0.1. It used to be every `cargo test` here, because the crate
//! dev-depended on itself to enable `unstable`; that line did not survive
//! `cargo package` and is gone. Today the assertion means "moved to
//! `tf_tree::unstable`" under `cargo test --doc --workspace`, which unifies the
//! feature in from the four consumers that declare it, and degrades to the
//! weaker "absent from the crate root" under a bare `-p tf_tree`. Both readings
//! are true; `just test` runs the strong one:
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
//! Three blocks and not one `use tf_tree::{ArenaView, EdgeKind, EdgeMeta};`,
//! because a single block passes as soon as *any* one of the three is absent —
//! it would go on passing after a refactor put two of them back.
//!
//! `E0432` and not a bare `compile_fail`: an unpinned one passes when the
//! snippet fails for *any* reason, and stable rustdoc ignores the code, so
//! `just test-doc-error-codes` is this line's real gate (`justfile`).
//!
//! ## What the pre-tag audit left alone, and why
//!
//! The sweep behind the split asked `docs/API.md` §7 of every `pub` item here.
//! Three moved; the rest stay, and two answers are worth recording because they
//! look like omissions:
//!
//! * **[`EdgeWriter`] still carries a lifetime**, which §2.1 calls a violation.
//!   It is a *known* one and it is not the bug: [`OwnedWriter`] is the storable
//!   shape, and a scoped claim whose scope the borrow checker enforces is better
//!   when it fits. §2.1 says so in terms.
//! * **[`Described`]'s two fields became private.** They promised that the
//!   `Display` wrapper is exactly `(error, tree)` forever, for no caller — the
//!   only construction site in the workspace is [`Tree::describe`].
//!
//! `tf_tree_core`'s crate docs carry the rule the audit applied to
//! `#[non_exhaustive]`, and the per-type arguments sit on the types.
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
//!
//! # Set `lto = "thin"` and `codegen-units = 1` in your release profile
//!
//! ```toml
//! [profile.release]
//! lto = "thin"
//! codegen-units = 1
//! ```
//!
//! **This is worth about 25% of a depth-3 lookup, and it is not cargo-cult
//! advice — it is a property of where this engine's code lives.** [`Plan::at`]
//! sits across a crate boundary from every consumer, and it and the fold beneath
//! it live one crate further down still, in `tf_tree_core`. Five functions on
//! the evaluate path carry `#[inline]` for exactly that reason (`Plan::at`, the
//! scalar fold, and the three [`Guard`] sampling entry points), but what an
//! attribute buys depends on **your** profile, not on ours: cargo's
//! `--release` defaults are `lto = false, codegen-units = 16`, and this
//! workspace's are not, so every latency number this project publishes is taken
//! under whole-program optimisation and your node's is not.
//!
//! Measured rather than asserted, because the last claim made here about this
//! mechanism was wrong in a way only a probe could show. One program — a
//! depth-3 `map <- imu_link` lookup, `LerpSlerp`, off-grid stamps so the
//! interpolation runs, one lookup per non-inlinable call — built twice and
//! pinned to one core, nine rounds each, three consecutive runs:
//!
//! | downstream profile | ns/lookup |
//! | --- | --- |
//! | `lto = false`, `codegen-units = 16` (cargo's `--release` default) | 240 |
//! | `lto = "thin"`, `codegen-units = 1` | 193–195 |
//!
//! On a 4-physical-core AMD EPYC-Milan VM under moderate load, 2026-08-02, so
//! read it as "about a quarter", not as three digits — the ratio itself moved
//! between 1.19× and 1.24× across those runs.
//!
//! **The same runs also say *why*, which is the part that makes this advice
//! rather than folklore.** They time a second, identical body compiled *inside*
//! `tf_tree_core`, and compare it against the one outside:
//!
//! | downstream profile | from outside the engine | from inside it |
//! | --- | --- | --- |
//! | `lto = false`, `codegen-units = 16` | 240 ns | 191 ns |
//! | `lto = "thin"`, `codegen-units = 1` | 193 ns | 194 ns |
//!
//! At cargo's defaults the crate boundary costs about a quarter of the lookup;
//! with thin LTO it costs nothing measurable, because the boundary is gone at
//! link time. `just embed-cost` in this repository re-measures both, and
//! `docs/PHASE5.md` §9.2 makes the second one a standing, gated benchmark row so
//! the next change to those attributes moves a number somebody sees.
//!
//! The cost of taking this advice is build time: thin LTO adds a link-time
//! optimisation pass, and `codegen-units = 1` gives up intra-crate build
//! parallelism. Both are compile-time costs and neither changes what the shipped
//! binary computes. How the 25% splits between the two settings has **not** been
//! measured here, so if your release builds are slow enough that you want to
//! take only one of them, measure your own case rather than trusting a guess
//! from this paragraph.

// **The crates.io front page, compiled.** `README.md`'s `rust` fence is the
// example a stranger reads first, and no recipe parses a README — the next
// signature change to `claim`, `plan`, `Capacity::history` or the `Described`
// wording would break the published page with every gate green. `cfg(doctest)`
// keeps it out of `cargo doc`, which already renders the module docs above, and
// off the crate root, whose `//!` block carries intra-doc links a README cannot.
//
// It gates the *API*, not the *output*: the fence's `// -> x = 0.5` and its
// two-line extrapolation message are comments, and a doctest does not read
// stdout. Turning them into asserts would gate those too, at some cost to how
// the front page reads; that trade has not been made.
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
///
/// Re-exported as a function rather than the constant so the facade keeps its
/// promise of exposing no arena internals: a caller gets the number it needs
/// for a diagnostic without a path into `tf_tree_arena`. That promise is
/// unchanged by `docs/decisions/0017` moving the crate from
/// `#![forbid(unsafe_code)]` to `deny` with one exception — the exception is a
/// lifetime extension, not a widening of what this surface hands out.
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

/// Test scaffolding for `docs/decisions/0028` plan step 2's reclamation
/// predicate, which is private. Its two production callers are the owner's
/// slot assigner (that record's step 3) and [`Tree::reap_participants`] (step
/// 5); both act on the verdict without reporting one — the assigner stops at
/// the first grantable slot, the sweep reports a count — and neither a grant
/// nor a count can separate the two verdicts that collect nothing. Absent
/// unless `--features test-hooks`; see
/// [`open::reclamation_verdict_for_test`].
#[cfg(all(feature = "test-hooks", feature = "shm", target_os = "linux"))]
#[doc(hidden)]
pub use open::reclamation_verdict_for_test;

/// **The unstable tier — `docs/API.md` §2.6.** Enabling the `unstable` feature
/// is the waiver; read the module's own documentation for what it waives.
#[cfg(feature = "unstable")]
pub mod unstable;

// Re-export the core engine surface so downstream code depends only on `tf_tree`.
//
// Everything below is the **stable** tier: at a published tag each line is a
// semver promise. `ArenaView`, `EdgeKind` and `EdgeMeta` used to be here and are
// now in the `unstable` module — see it for the test that separates them, which
// is "does its shape follow the arena layout", not "is it low-level".

pub use tf_tree_core::edge::Publisher;
pub use tf_tree_core::layout::{write_affine32, write_mat4, write_quat, write_quat_twist, Layout};
pub use tf_tree_core::plan::{
    AdaptiveScratch, Domain, ErrBound, Guard, InterpPolicy, Plan, Query, Sample, SensorDomain,
    SimDomain, Stamp, SteadyDomain, Step, SystemDomain, MAX_ADAPTIVE_DEPTH, MAX_KNOTS,
};
pub use tf_tree_core::{
    ClaimError, EdgeId, FrameError, FrameId, LookupError, PushError, MAX_DEPTH, MAX_PATH_EDGES,
};

// **The math surface, including both interpolation kernels.** `slerp` is here
// for the reason the rest of this block exists: a consumer who reaches
// `LerpSlerp` through this facade and its kernel through `tf_tree_math` has two
// direct dependencies to keep in lockstep on a `0.0.x` line where every release
// breaks every other — which is a worse position than the `Iso3` round trip
// `docs/API.md` §2.7 told them to abandon. **`ScLerp`'s kernel is here on the
// same argument**, and it took a review pass to see that leaving it out
// reproduced the asymmetry one layer up: exporting `LerpSlerp` + `slerp` but
// `ScLerp` with no route to `screw_pow` puts an `ScLerp` consumer in exactly the
// two-dependency position this block exists to prevent. What is *not* done is a
// bare `screw_pow` at this root, which would be a second spelling
// (`PROJECT.md` §6) of `tf_tree_math::dualquat::screw_pow`. Re-exporting the
// module is the *same* spelling, so `tf_tree::dualquat::screw_pow` and
// `tf_tree_math::dualquat::screw_pow` are one path with one prefix swapped.
// `tests/math_reexports.rs` is what says this list and `tf_tree_math`'s are one
// set of items rather than two.
pub use tf_tree_math::dualquat;
pub use tf_tree_math::{
    exp_se3, exp_so3, log_se3, log_so3, quat_from_rot3, slerp, Interp, Iso3, LerpSlerp, Quat,
    ScLerp, Twist, Vec3,
};
