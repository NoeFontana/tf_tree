//! **The unstable tier. Nothing in this module is covered by semver.**
//!
//! `docs/API.md` §2.6 is the specification; this mirrors the C ABI's two-header
//! split (`docs/PHASE4.md` §3.1).
//!
//! # The feature flag is the waiver
//!
//! ```toml
//! tf_tree = { version = "0.2", features = ["unstable"] }
//! ```
//!
//! **A type here may change shape, change meaning, or disappear in a patch
//! release**, with no deprecation cycle and no MSRV or platform promise.
//!
//! Membership test: **"does its shape follow the arena layout"** (which
//! `docs/PHASE5.md` §1 changes on purpose). [`Plan`], [`Guard`] and [`Stamp`]
//! are stable.
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

/// Raw, read-only access to the arena's own tables. **Unstable.** For
/// `tf_tree doctor` and `tf_tree top`; [`crate::Tree::arena_view`] is the door.
pub use tf_tree_core::arena_view::ArenaView;

/// Whether an edge is dynamic, static or tombstoned.
///
/// **Unstable.** The decode of `EdgeRecord::kind`, reached through an
/// [`ArenaView`] read.
pub use tf_tree_core::edge::EdgeKind;

/// Static metadata about an edge, an input to `tf_tree_core::compile`.
///
/// **Unstable.** This facade does not re-export `compile`; its `kind` is an
/// [`EdgeKind`].
pub use tf_tree_core::plan::EdgeMeta;
