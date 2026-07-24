#![forbid(unsafe_code)]
//! `std` facade for the `tf_tree` transform engine.
//!
//! Re-exports the [`tf_tree_core`] engine and adds the ergonomic, allocating
//! conveniences that do not belong in the `no_std` core: the `TreeBuilder`,
//! the plan-cached `lookup`, and `Display` for errors (which resolves IDs to
//! names by consulting the arena).
//!
//! Most users depend on this crate, not on `tf_tree_core` directly.

// Re-exports and the ergonomic surface are added by the Phase 1 public-API PR.
