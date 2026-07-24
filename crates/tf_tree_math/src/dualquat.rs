//! Fast SE(3) screw interpolation via unit dual-quaternion powers.
//!
//! The reference geodesic interpolation is `a · exp_se3(s · log_se3(a⁻¹·b))`
//! (see [`crate::reference::sclerp`]). That route spends two transcendental
//! pairs (an `atan2`+more in `log`, a `sin_cos` in `exp`) and the full `V`/`V⁻¹`
//! series. The screw form here computes the identical result raising the
//! relative transform's unit dual quaternion to the power `s`, spending exactly
//! **one `atan2` and one `sin_cos`** total.
//!
//! A unit dual quaternion `q̂ = q_r + ε q_d` of a rigid transform `(q, t)` has
//! `q_r = q` and `q_d = ½·(0,t)⊗q`. Its screw parameters are the angle `θ`,
//! pitch translation `d`, axis direction `l`, and moment `m`; the power `q̂ˢ`
//! scales `θ` and `d` by `s` and leaves `l`, `m` fixed.

use crate::iso3::{exp_se3, log_se3, Iso3, Vec3};
use crate::quat::Quat;

/// Below this rotation half-angle (`sin(θ/2) = ‖q_v‖`) the screw axis and moment
/// are ill-defined (division by `sin(θ/2)`), so the degenerate case routes
/// through the exact `exp_se3(s·log_se3(rel))` fallback instead.
const SCREW_SMALL: f64 = 1e-8;

/// Raise the rigid transform `rel` to the real power `s` along its screw axis.
///
/// Equivalent to `exp_se3(s · log_se3(rel))` but computed through the unit
/// dual-quaternion screw decomposition. For `s = 1` this returns `rel`; for
/// `s = 0`, the identity.
#[inline]
#[must_use]
pub fn screw_pow(rel: &Iso3, s: f64) -> Iso3 {
    // Canonicalize the real quaternion to the w ≥ 0 hemisphere so θ/2 ∈ [0, π/2]
    // lands in the principal branch, matching the reference log/exp route.
    let q = if rel.q.w < 0.0 { rel.q.neg() } else { rel.q };
    let t = rel.t;

    let sin_half = q.vector().norm(); // sin(θ/2) = ‖q_v‖ (unit quaternion)
    let cos_half = q.w; // cos(θ/2) ≥ 0

    if sin_half < SCREW_SMALL {
        // Near-identity rotation: the screw axis and moment are ill-defined
        // (they divide by sin(θ/2) → 0). A crude t' = s·t here would drop the
        // O(θ·‖t‖) rotation/translation screw coupling — large enough to blow
        // past 1e-14 for meter-scale translations. Route through the exact
        // (and, at this θ, fully accurate) log/exp form instead. This branch is
        // rare, so its cost is irrelevant.
        let rel = Iso3::new(q, t);
        let xi = log_se3(rel);
        return exp_se3([
            s * xi[0],
            s * xi[1],
            s * xi[2],
            s * xi[3],
            s * xi[4],
            s * xi[5],
        ]);
    }

    let inv_sin_half = 1.0 / sin_half;
    let l = q.vector().scale(inv_sin_half); // unit screw axis

    // Dual part q_d = ½·(0,t)⊗q, from which the screw pitch and moment follow.
    let q_d = (Quat::from_pure(t) * q).scale(0.5);
    let d = -2.0 * q_d.w * inv_sin_half; // pitch translation ( = l·t )
                                         // Moment m = (q_d_v − (d/2)·cos(θ/2)·l) / sin(θ/2).
    let m = q_d
        .vector()
        .sub(l.scale(0.5 * d * cos_half))
        .scale(inv_sin_half);

    // Scale the screw: θ' = s·θ, d' = s·d. θ/2 = atan2(sin_half, cos_half).
    let half_ang = s * libm::atan2(sin_half, cos_half);
    let (sin_sh, cos_sh) = libm::sincos(half_ang);
    let half_sd = 0.5 * s * d;

    // Recompose the scaled unit dual quaternion.
    let q_r2 = Quat::new(cos_sh, sin_sh * l.x, sin_sh * l.y, sin_sh * l.z);
    let q_d2_v = m.scale(sin_sh).add(l.scale(half_sd * cos_sh));
    let q_d2 = Quat::new(-half_sd * sin_sh, q_d2_v.x, q_d2_v.y, q_d2_v.z);

    // Recover translation: (0, t') = 2·q_d'⊗q_r'*.
    let t2q = q_d2 * q_r2.conjugate();
    Iso3::new(q_r2, Vec3::new(2.0 * t2q.x, 2.0 * t2q.y, 2.0 * t2q.z))
}
