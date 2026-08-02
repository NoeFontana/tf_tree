//! **The unstable tier. Nothing in this module is covered by semver.**
//!
//! `docs/API.md` §2.6 is the specification, and its deferral — "while the crate
//! is private" — expires at the first published tag. This module is the Rust
//! mirror of the C ABI's two-header split (`docs/PHASE4.md` §3.1): `tf_tree.h`
//! is semver'd, `tf_tree_unstable.h` needs `#define TFT_ENABLE_UNSTABLE` and
//! promises nothing. C got a macro because a header is text; Rust gets a
//! Cargo feature, because a feature is the only thing in the language a
//! *caller* has to write down.
//!
//! # The feature flag is the waiver
//!
//! ```toml
//! tf_tree = { version = "0.2", features = ["unstable"] }
//! ```
//!
//! Writing that line is the acknowledgement. Concretely, and this is not a
//! formality:
//!
//! * **A type here may change shape, change meaning, or disappear in a patch
//!   release.** No deprecation cycle is owed and none will be given.
//! * **It is not covered by the crate's MSRV or platform promises either.**
//! * A build that turns this on and then breaks on `cargo update` is working as
//!   designed. The stable surface — everything at the crate root — is where a
//!   compatibility bug report belongs.
//!
//! # Why these items and not others
//!
//! The test is not "is it low-level" but **"does its shape follow the arena
//! layout"**. `docs/PHASE5.md` §1 changes that layout on purpose, bumping
//! `FORMAT_VERSION` to 3 and adding regions Phase 6 will fill, so anything
//! shaped by it is scheduled to move by a document that already exists. That is
//! the difference between an unstable item and merely an advanced one: [`Plan`],
//! [`Guard`] and [`Stamp`] are as low-level as anything here and are *stable*,
//! because their shape is the engine's contract rather than the arena's.
//!
//! # What the waiver buys
//!
//! ```
//! use tf_tree::unstable::{ArenaView, EdgeKind};
//! use tf_tree::{Iso3, TreeBuilder};
//!
//! let tree = TreeBuilder::new()
//!     .static_edge("map", "odom", &Iso3::IDENTITY)
//!     .build()
//!     .expect("layout");
//!
//! // `Tree::arena_view` is gated on this feature too — it is the door.
//! let view: ArenaView<'_> = tree.arena_view();
//! let edge = view.edge(tf_tree::EdgeId(1)).expect("the declared edge");
//! assert_eq!(EdgeKind::from_u8(edge.kind), EdgeKind::Static);
//! ```
//!
//! [`Plan`]: crate::Plan
//! [`Guard`]: crate::Guard
//! [`Stamp`]: crate::Stamp

/// Raw, read-only access to the arena's own tables.
///
/// **Unstable.** Its accessors are the arena's regions — `header()`,
/// `frame_record()`, `edge_record()`, the participant table, the topology
/// seqlock — one method per region, so `docs/PHASE5.md` §1's layout change is
/// a change to this type by construction. It exists so `tf_tree doctor` and
/// `tf_tree top` can render what is in a segment without depending on
/// `tf_tree_core` directly; it was never an embedding surface, and the reason
/// it read as one was that Rust has a single visibility tier.
///
/// [`crate::Tree::arena_view`] is the door to it and is gated on the same
/// feature. Leaving that method on the stable surface would have handed out
/// every method below through type inference without a caller ever naming the
/// type — the split would then have been a spelling convention rather than a
/// promise.
pub use tf_tree_core::arena_view::ArenaView;

/// Whether an edge is dynamic, static or tombstoned.
///
/// **Unstable, and here for the same reason as [`ArenaView`] rather than by
/// association.** It is the decode of `EdgeRecord::kind`, a `u8` field in the
/// arena, and it is reachable only through the same door: the value comes from
/// an [`ArenaView`] read. An embedder declares an edge's kind by calling
/// [`crate::TreeBuilder::static_edge`] or [`crate::TreeBuilder::dynamic_edge`]
/// and never names this type. ([`EdgeMeta`] carries one, which is why that type
/// is in this module too.)
pub use tf_tree_core::edge::EdgeKind;

/// Static metadata about an edge, supplied to `tf_tree_core::compile` for
/// constant folding.
///
/// **Unstable, and it was already unusable from the stable tier** — which is
/// the audit finding that moved it rather than a judgement about its shape.
/// `EdgeMeta` is an *input* to `tf_tree_core::compile`, and this facade does not
/// re-export `compile`: `Tree::plan` builds the metadata internally from an
/// [`ArenaView`] and compiles for you. So the crate root promised a type with no
/// reachable callee, and a `pub` item nobody can call is the clearest possible
/// case of a promise that was never deliberate.
///
/// Its public `kind` field is an [`EdgeKind`], so it could not have stayed
/// beside a stable `EdgeKind` either.
pub use tf_tree_core::plan::EdgeMeta;
