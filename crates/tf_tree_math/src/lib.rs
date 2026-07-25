#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
//! `no_std` SE(3)/SO(3) and dual-quaternion math for the `tf_tree` engine.
//!
//! # Conventions (lock these in — every downstream bug traces back to one)
//!
//! 1. **Hamilton** quaternions (not JPL).
//! 2. **`w` first** storage order (`[w, x, y, z]`) — differs from Eigen; the
//!    Phase 4 C++ wrapper must transpose.
//! 3. **Active** rotations. Applying a [`Quat`] to a vector rotates the vector
//!    within a fixed frame; applying an [`Iso3`] `T_parent_child` to a point
//!    expressed in `child` yields that point expressed in `parent`.
//! 4. [`Iso3`] composition `a * b` means `T_a_x * T_x_b` (`T_parent_child`
//!    chaining): the right operand's parent must be the left operand's child.
//! 5. Adjoint convention is **right-perturbation**: `T = T̂ · exp(ξ^)`, so
//!    [`log_se3`] returns the twist `ξ = [ω, v]` of the right-multiplied
//!    increment and [`exp_se3`] consumes the same ordering.
//!
//! This crate is `#![forbid(unsafe_code)]`: its property tests run under Miri in
//! seconds precisely because it holds no `unsafe` and no arena.
//!
//! # Numerics
//!
//! The two findings that drive the implementation (verified against a 50-digit
//! reference in `docs/PHASE1.md` §3.3):
//!
//! * [`log_so3`] goes through the quaternion (`2·atan2(‖q_v‖, q_w)`), never
//!   through `acos((tr − 1)/2)`, which loses nine digits near `θ = π`.
//! * The small-angle series threshold for the `V`/`V⁻¹` coefficients is
//!   `θ < 0.1` with four series terms, not the `1e-8` most libraries use.

pub mod dualquat;
pub mod interp;
pub mod iso3;
pub mod quat;
pub mod reference;

pub use interp::{Interp, LerpSlerp, ScLerp};
pub use iso3::{exp_se3, log_se3, Iso3, Vec3};
pub use quat::{exp_so3, log_so3, Quat};
