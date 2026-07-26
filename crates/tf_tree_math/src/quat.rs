//! Hamilton quaternions (`w` first, active rotations) and SO(3) `exp`/`log`.

use crate::iso3::Vec3;
use bytemuck::{Pod, Zeroable};
use core::ops::Mul;

/// Angle (in radians) below which `exp_so3` switches to a Taylor series for the
/// `sin(θ/2)/θ` scale factor, avoiding a `0/0` at the identity.
const EXP_SO3_SMALL: f64 = 1e-8;

/// Unit quaternion, Hamilton convention, scalar (`w`) first.
///
/// Represents an active rotation. Unchecked constructors and arithmetic assume
/// (and preserve, up to rounding) unit norm.
///
/// INVARIANT: callers of unchecked constructors guarantee `|q| == 1` within
/// `1e-12`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Quat {
    /// Scalar part.
    pub w: f64,
    /// First vector component (`i`).
    pub x: f64,
    /// Second vector component (`j`).
    pub y: f64,
    /// Third vector component (`k`).
    pub z: f64,
}

impl Quat {
    /// The multiplicative identity (zero rotation).
    pub const IDENTITY: Self = Self {
        w: 1.0,
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    /// Construct from raw components (`w` first). No normalization is performed.
    #[inline]
    #[must_use]
    pub const fn new(w: f64, x: f64, y: f64, z: f64) -> Self {
        Self { w, x, y, z }
    }

    /// The quaternion conjugate `(w, −x, −y, −z)`. For a unit quaternion this is
    /// the inverse rotation.
    #[inline]
    #[must_use]
    pub const fn conjugate(self) -> Self {
        Self {
            w: self.w,
            x: -self.x,
            y: -self.y,
            z: -self.z,
        }
    }

    /// The squared Euclidean norm `w² + x² + y² + z²`.
    #[inline]
    #[must_use]
    pub fn norm_squared(self) -> f64 {
        self.w * self.w + self.x * self.x + self.y * self.y + self.z * self.z
    }

    /// The Euclidean norm `√(w² + x² + y² + z²)`.
    #[inline]
    #[must_use]
    pub fn norm(self) -> f64 {
        libm::sqrt(self.norm_squared())
    }

    /// Return this quaternion scaled to unit norm.
    ///
    /// A zero quaternion (norm below `1e-300`) is returned unchanged rather than
    /// producing infinities; unit-norm callers never hit that path.
    #[inline]
    #[must_use]
    pub fn normalize(self) -> Self {
        let n = self.norm();
        if n < 1e-300 {
            self
        } else {
            let inv = 1.0 / n;
            Self {
                w: self.w * inv,
                x: self.x * inv,
                y: self.y * inv,
                z: self.z * inv,
            }
        }
    }

    /// Dot product of the four components, viewed as vectors in R⁴.
    #[inline]
    #[must_use]
    pub fn dot(self, rhs: Self) -> f64 {
        self.w * rhs.w + self.x * rhs.x + self.y * rhs.y + self.z * rhs.z
    }

    /// Negate every component. `-q` is the same rotation as `q`.
    #[inline]
    #[must_use]
    pub const fn neg(self) -> Self {
        Self {
            w: -self.w,
            x: -self.x,
            y: -self.y,
            z: -self.z,
        }
    }

    /// Component-wise scale.
    #[inline]
    #[must_use]
    pub const fn scale(self, k: f64) -> Self {
        Self {
            w: self.w * k,
            x: self.x * k,
            y: self.y * k,
            z: self.z * k,
        }
    }

    /// Component-wise sum.
    #[inline]
    #[must_use]
    pub const fn add(self, rhs: Self) -> Self {
        Self {
            w: self.w + rhs.w,
            x: self.x + rhs.x,
            y: self.y + rhs.y,
            z: self.z + rhs.z,
        }
    }

    /// Component-wise difference.
    ///
    /// Exists for the chord form `|a − b|² = 2 − 2·(a·b)`, which is how `slerp`
    /// obtains the angle between two near-parallel quaternions **without**
    /// forming `1 − dot` — a subtraction that cancels catastrophically exactly
    /// when the two are close, which is the case that dominates.
    #[inline]
    #[must_use]
    pub const fn sub(self, rhs: Self) -> Self {
        Self {
            w: self.w - rhs.w,
            x: self.x - rhs.x,
            y: self.y - rhs.y,
            z: self.z - rhs.z,
        }
    }

    /// The vector (imaginary) part `(x, y, z)`.
    #[inline]
    #[must_use]
    pub const fn vector(self) -> Vec3 {
        Vec3 {
            x: self.x,
            y: self.y,
            z: self.z,
        }
    }

    /// Build a pure quaternion `(0, v)` from a vector.
    #[inline]
    #[must_use]
    pub const fn from_pure(v: Vec3) -> Self {
        Self {
            w: 0.0,
            x: v.x,
            y: v.y,
            z: v.z,
        }
    }

    /// Rotate a vector by this (unit) quaternion: `v' = q · v · q⁻¹`.
    ///
    /// Uses the two-cross-product form, which needs no explicit quaternion
    /// product and is numerically well behaved.
    #[inline]
    #[must_use]
    pub fn rotate(self, v: Vec3) -> Vec3 {
        let u = self.vector();
        let t = u.cross(v).scale(2.0);
        v.add(t.scale(self.w)).add(u.cross(t))
    }
}

impl Mul for Quat {
    type Output = Quat;

    /// Hamilton product `self ⊗ rhs`.
    #[inline]
    fn mul(self, rhs: Quat) -> Quat {
        let a = self;
        let b = rhs;
        Quat {
            w: a.w * b.w - a.x * b.x - a.y * b.y - a.z * b.z,
            x: a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y,
            y: a.w * b.y - a.x * b.z + a.y * b.w + a.z * b.x,
            z: a.w * b.z + a.x * b.y - a.y * b.x + a.z * b.w,
        }
    }
}

/// SO(3) exponential: map a rotation vector `ω` (axis × angle) to a unit
/// quaternion.
///
/// `q = (cos(θ/2), sinc(θ/2)·½·ω)` with `θ = |ω|`; the `sin(θ/2)/θ` scale uses a
/// Taylor series below `EXP_SO3_SMALL` (a private threshold) to stay finite at
/// `θ = 0`.
#[inline]
#[must_use]
pub fn exp_so3(w: Vec3) -> Quat {
    exp_so3_theta(w, w.norm())
}

/// [`exp_so3`] with the rotation magnitude `theta = ‖w‖` supplied by the caller.
///
/// `exp_se3` already computes `‖w‖` for the `V` coefficients; threading it here
/// avoids recomputing the `sqrt`. `theta` must equal `w.norm()`.
#[inline]
#[must_use]
pub(crate) fn exp_so3_theta(w: Vec3, theta: f64) -> Quat {
    let half = 0.5 * theta;
    // One sincos for both the scalar part and the sin(θ/2)/θ scale.
    let (sin_half, cos_half) = libm::sincos(half);
    // scale = sin(θ/2)/θ, so that q_v = scale · ω.
    let scale = if theta > EXP_SO3_SMALL {
        sin_half / theta
    } else {
        // sin(θ/2)/θ = 1/2 − θ²/48 + θ⁴/3840 − …
        let theta2 = theta * theta;
        0.5 - theta2 / 48.0 + theta2 * theta2 / 3840.0
    };
    Quat {
        w: cos_half,
        x: scale * w.x,
        y: scale * w.y,
        z: scale * w.z,
    }
}

/// SO(3) logarithm: map a unit quaternion to its rotation vector `ω` in the
/// principal branch `|ω| ∈ [0, π]`.
///
/// **Quaternion form, never the trace.** `θ = 2·atan2(‖q_v‖, q_w)` (after
/// canonicalizing `q_w ≥ 0`) is accurate to full precision even near `θ = π`,
/// where `acos((tr R − 1)/2)` loses nine digits.
#[inline]
#[must_use]
pub fn log_so3(q: Quat) -> Vec3 {
    // Canonicalize to the hemisphere w ≥ 0 so θ/2 = atan2(n, w) ∈ [0, π/2] and
    // the result lands in the principal branch |ω| ∈ [0, π]. q and −q are the
    // same rotation, so this is free.
    let (w, x, y, z) = if q.w < 0.0 {
        (-q.w, -q.x, -q.y, -q.z)
    } else {
        (q.w, q.x, q.y, q.z)
    };
    let n = libm::sqrt(x * x + y * y + z * z);
    if n == 0.0 {
        return Vec3::ZERO;
    }
    let theta = 2.0 * libm::atan2(n, w);
    // scale = θ/n maps the (unit) axis (x,y,z)/n up to magnitude θ. atan2 keeps
    // θ ≈ 2n for tiny n, so θ/n stays well conditioned down to n → 0.
    let scale = theta / n;
    Vec3 {
        x: scale * x,
        y: scale * y,
        z: scale * z,
    }
}

/// The unit quaternion of a **row-major 3×3 rotation matrix**.
///
/// `r` is `[r00 r01 r02, r10 r11 r12, r20 r21 r22]`, the same order
/// `tf_tree_c`'s `rot3` emits, so this is exactly its inverse.
///
/// # Why the four branches
///
/// The textbook one-liner `w = √(1 + tr R)/2` divides the vector part by `4w`,
/// and `w → 0` as the angle approaches π — precisely the rotations a robot's
/// `map → odom` yaw spends time near. At `θ = π` it is `0/0`. Shepperd's method
/// instead builds the quaternion around whichever of `w, x, y, z` is largest in
/// magnitude, so the divisor is never below `1/√2` of the largest component and
/// the result is accurate to full precision for every rotation, including the
/// half-turns.
///
/// # This does not validate `r`
///
/// A non-rotation input produces a quaternion rather than an error — a
/// reflection (`det R = −1`) yields a *different, valid* rotation, silently.
/// That check belongs where foreign input arrives, not in the kernel; see
/// `tf_tree_c::layout::read`, which rejects `|det R − 1| > 1e-6` before calling
/// this.
///
/// The result is not normalized either, for the same reason: a caller that
/// validated the determinant knows the norm is 1 to rounding, and one that did
/// not should not be silently rescued.
#[inline]
#[must_use]
pub fn quat_from_rot3(r: &[f64; 9]) -> Quat {
    let (r00, r11, r22) = (r[0], r[4], r[8]);
    let trace = r00 + r11 + r22;
    if trace > 0.0 {
        // s = 4w, and w ≥ 1/2 here, so s ≥ 2.
        let s = libm::sqrt(trace + 1.0) * 2.0;
        Quat::new(
            0.25 * s,
            (r[7] - r[5]) / s,
            (r[2] - r[6]) / s,
            (r[3] - r[1]) / s,
        )
    } else if r00 > r11 && r00 > r22 {
        // s = 4x.
        let s = libm::sqrt(1.0 + r00 - r11 - r22) * 2.0;
        Quat::new(
            (r[7] - r[5]) / s,
            0.25 * s,
            (r[1] + r[3]) / s,
            (r[2] + r[6]) / s,
        )
    } else if r11 > r22 {
        // s = 4y.
        let s = libm::sqrt(1.0 + r11 - r00 - r22) * 2.0;
        Quat::new(
            (r[2] - r[6]) / s,
            (r[1] + r[3]) / s,
            0.25 * s,
            (r[5] + r[7]) / s,
        )
    } else {
        // s = 4z.
        let s = libm::sqrt(1.0 + r22 - r00 - r11) * 2.0;
        Quat::new(
            (r[3] - r[1]) / s,
            (r[2] + r[6]) / s,
            (r[5] + r[7]) / s,
            0.25 * s,
        )
    }
}
