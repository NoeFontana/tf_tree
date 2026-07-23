#![no_std]
#![forbid(unsafe_code)]
//! `no_std` SE(3)/SO(3) and dual-quaternion math for the `tf_tree` engine.
//!
//! # Conventions (lock these in — every downstream bug traces back to one)
//!
//! 1. **Hamilton** quaternions (not JPL).
//! 2. **`w` first** storage order (`[w, x, y, z]`) — differs from Eigen; the
//!    Phase 4 C++ wrapper must transpose.
//! 3. **Active** rotations.
//! 4. `Iso3` composition `a * b` means `T_a_x * T_x_b` (`T_parent_child`).
//! 5. Adjoint convention is **right-perturbation**: `T = T̂ · exp(ξ^)`.
//!
//! This crate is `#![forbid(unsafe_code)]`: its property tests run under Miri in
//! seconds precisely because it holds no `unsafe` and no arena.

// Modules are added by the Phase 1 `tf_tree_math` implementation PR:
//   quat, iso3, dualquat, interp, reference.
