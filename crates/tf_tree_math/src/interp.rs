//! Interpolation between two `Iso3` poses.
//!
//! Two policies share the [`Interp`] trait:
//!
//! * [`ScLerp`] — the SE(3) screw geodesic, **the default**. Left- and
//!   right-invariant. Computed with the fast dual-quaternion power
//!   ([`crate::dualquat::screw_pow`]), proptested against
//!   [`crate::reference::sclerp`].
//! * [`LerpSlerp`] — tf2-compatible: translation LERP, rotation shortest-arc
//!   SLERP. Left-invariant but **not** right-invariant; that asymmetry is why
//!   `ScLerp` is the default (decision `0003`).

use crate::dualquat::screw_pow;
use crate::iso3::Iso3;
use crate::quat::Quat;

/// Below this half-angle between the two quaternions, `slerp` falls back to a
/// normalized LERP to avoid dividing by `sin(angle) → 0`.
const SLERP_LERP_FALLBACK: f64 = 1e-6;

/// Interpolate between two poses `a` (at `s = 0`) and `b` (at `s = 1`).
pub trait Interp {
    /// Interpolate at parameter `s`. `s = 0` returns `a` exactly and `s = 1`
    /// returns `b` exactly; intermediate `s` follows the policy's path.
    fn eval(a: &Iso3, b: &Iso3, s: f64) -> Iso3;
}

/// SE(3) screw-geodesic interpolation — the default policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScLerp;

/// tf2-compatible interpolation: translation LERP + rotation SLERP.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LerpSlerp;

impl Interp for ScLerp {
    #[inline]
    fn eval(a: &Iso3, b: &Iso3, s: f64) -> Iso3 {
        // Endpoints are exact by construction (proptest #6).
        if s == 0.0 {
            return *a;
        }
        if s == 1.0 {
            return *b;
        }
        // a · (a⁻¹·b)ˢ — the fast screw power of the relative transform.
        let rel = a.inv_mul(b);
        *a * screw_pow(&rel, s)
    }
}

impl Interp for LerpSlerp {
    #[inline]
    fn eval(a: &Iso3, b: &Iso3, s: f64) -> Iso3 {
        if s == 0.0 {
            return *a;
        }
        if s == 1.0 {
            return *b;
        }
        let t = a.t.scale(1.0 - s).add(b.t.scale(s));
        let q = slerp(a.q, b.q, s);
        Iso3::new(q, t)
    }
}

impl Iso3 {
    /// `self⁻¹ · rhs`, the relative transform from `self` to `rhs`.
    ///
    /// Computed directly — rotation `q_self*·q_rhs`, translation
    /// `q_self*·(t_rhs − t_self)` — rather than materializing `self.inverse()`
    /// and composing, which saves a vector rotation and a negation pass on the
    /// ScLerp interpolation hot path.
    #[inline]
    #[must_use]
    fn inv_mul(&self, rhs: &Iso3) -> Iso3 {
        let qi = self.q.conjugate();
        let q = qi * rhs.q;
        let t = qi.rotate(rhs.t.sub(self.t));
        Iso3::new(q, t)
    }
}

/// Shortest-arc spherical linear interpolation of two unit quaternions.
///
/// Applies the standard sign fix (`if q_a·q_b < 0 { negate q_b }`) so the path
/// takes the short way round, and falls back to a normalized LERP when the
/// half-angle drops below [`SLERP_LERP_FALLBACK`].
#[inline]
#[must_use]
fn slerp(qa: Quat, qb: Quat, s: f64) -> Quat {
    let mut dot = qa.dot(qb);
    let qb = if dot < 0.0 {
        dot = -dot;
        qb.neg()
    } else {
        qb
    };

    // Clamp for the domain of acos against rounding past 1.0.
    let dot = if dot > 1.0 { 1.0 } else { dot };
    let angle = libm::acos(dot);

    if angle < SLERP_LERP_FALLBACK {
        // Near-parallel: LERP and renormalize.
        return lerp_norm(qa, qb, s);
    }

    let sin_angle = libm::sin(angle);
    let wa = libm::sin((1.0 - s) * angle) / sin_angle;
    let wb = libm::sin(s * angle) / sin_angle;
    qa.scale(wa).add(qb.scale(wb))
}

/// Normalized linear interpolation of two quaternions.
#[inline]
#[must_use]
fn lerp_norm(qa: Quat, qb: Quat, s: f64) -> Quat {
    qa.scale(1.0 - s).add(qb.scale(s)).normalize()
}
