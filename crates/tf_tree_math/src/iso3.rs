//! `Vec3`, the `Iso3` rigid transform, and the SE(3) `exp`/`log` maps.

use crate::quat::{exp_so3, log_so3, Quat};
use bytemuck::{Pod, Zeroable};
use core::ops::Mul;

/// Angle threshold below which the `V`/`V⁻¹` coefficients switch to their Taylor
/// series. NORMATIVE (decision `0003`): `0.1`, not the `1e-8` most libraries
/// use — the closed forms lose 4–11 digits to cancellation well before that.
const THETA_SMALL: f64 = 0.1;

/// Series coefficients of `c1 = (1 − cos θ)/θ²`, powers of `θ²` (Horner order).
const C1: [f64; 4] = [1.0 / 2.0, -1.0 / 24.0, 1.0 / 720.0, -1.0 / 40320.0];
/// Series coefficients of `c2 = (θ − sin θ)/θ³`, powers of `θ²`.
const C2: [f64; 4] = [1.0 / 6.0, -1.0 / 120.0, 1.0 / 5040.0, -1.0 / 362880.0];
/// Series coefficients of `c3 = 1/θ² − (1 + cos θ)/(2θ sin θ)`, powers of `θ²`.
const C3: [f64; 4] = [1.0 / 12.0, 1.0 / 720.0, 1.0 / 30240.0, 1.0 / 1209600.0];

/// A point or vector in R³.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Vec3 {
    /// x component.
    pub x: f64,
    /// y component.
    pub y: f64,
    /// z component.
    pub z: f64,
}

impl Vec3 {
    /// The zero vector.
    pub const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    /// Construct from components.
    #[inline]
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    /// Component-wise sum.
    #[inline]
    #[must_use]
    pub const fn add(self, rhs: Self) -> Self {
        Self {
            x: self.x + rhs.x,
            y: self.y + rhs.y,
            z: self.z + rhs.z,
        }
    }

    /// Component-wise difference.
    #[inline]
    #[must_use]
    pub const fn sub(self, rhs: Self) -> Self {
        Self {
            x: self.x - rhs.x,
            y: self.y - rhs.y,
            z: self.z - rhs.z,
        }
    }

    /// Scale by a scalar.
    #[inline]
    #[must_use]
    pub const fn scale(self, k: f64) -> Self {
        Self {
            x: self.x * k,
            y: self.y * k,
            z: self.z * k,
        }
    }

    /// Dot product.
    #[inline]
    #[must_use]
    pub fn dot(self, rhs: Self) -> f64 {
        self.x * rhs.x + self.y * rhs.y + self.z * rhs.z
    }

    /// Cross product `self × rhs`.
    #[inline]
    #[must_use]
    pub fn cross(self, rhs: Self) -> Self {
        Self {
            x: self.y * rhs.z - self.z * rhs.y,
            y: self.z * rhs.x - self.x * rhs.z,
            z: self.x * rhs.y - self.y * rhs.x,
        }
    }

    /// Euclidean norm.
    #[inline]
    #[must_use]
    pub fn norm(self) -> f64 {
        libm::sqrt(self.dot(self))
    }
}

/// A rigid-body transform in SE(3): a rotation `q` followed by a translation
/// `t`. `T_parent_child` — applying it to a point in `child` yields the point in
/// `parent`.
///
/// Laid out as exactly one 64-byte cacheline (`align(64)`) with an 8-byte pad so
/// the Phase 2 shared-memory arena can store slots without re-deriving layout.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Iso3 {
    /// Rotation part (unit quaternion).
    pub q: Quat,
    /// Translation part.
    pub t: Vec3,
    /// Padding to a full cacheline. Always zero.
    _pad: [u8; 8],
}

impl Iso3 {
    /// The identity transform.
    pub const IDENTITY: Self = Self {
        q: Quat::IDENTITY,
        t: Vec3::ZERO,
        _pad: [0; 8],
    };

    /// Construct from a rotation and translation. No normalization is performed;
    /// callers guarantee `q` is unit.
    #[inline]
    #[must_use]
    pub const fn new(q: Quat, t: Vec3) -> Self {
        Self { q, t, _pad: [0; 8] }
    }

    /// The inverse transform. For `T = (q, t)`, `T⁻¹ = (q*, −q*·t)`.
    #[inline]
    #[must_use]
    pub fn inverse(&self) -> Self {
        let qi = self.q.conjugate();
        let ti = qi.rotate(self.t).scale(-1.0);
        Self::new(qi, ti)
    }

    /// Compute `self · rhs⁻¹` directly, without materializing `rhs⁻¹`.
    ///
    /// `a · b⁻¹` has rotation `q_a · q_b*` and translation
    /// `t_a − (q_a·q_b*)·t_b`, saving a negation pass and one rotation over
    /// `self * rhs.inverse()`.
    #[inline]
    #[must_use]
    pub fn mul_inv(&self, rhs: &Self) -> Self {
        let q = self.q * rhs.q.conjugate();
        let t = self.t.sub(q.rotate(rhs.t));
        Self::new(q, t)
    }

    /// The 7 `f64` bit patterns in canonical slot order
    /// `[qw, qx, qy, qz, tx, ty, tz]`.
    #[inline]
    #[must_use]
    pub fn to_bits(&self) -> [u64; 7] {
        [
            self.q.w.to_bits(),
            self.q.x.to_bits(),
            self.q.y.to_bits(),
            self.q.z.to_bits(),
            self.t.x.to_bits(),
            self.t.y.to_bits(),
            self.t.z.to_bits(),
        ]
    }

    /// Reconstruct from the canonical `[qw, qx, qy, qz, tx, ty, tz]` bit
    /// patterns produced by [`Iso3::to_bits`].
    #[inline]
    #[must_use]
    pub fn from_bits(bits: &[u64; 7]) -> Self {
        Self::new(
            Quat::new(
                f64::from_bits(bits[0]),
                f64::from_bits(bits[1]),
                f64::from_bits(bits[2]),
                f64::from_bits(bits[3]),
            ),
            Vec3::new(
                f64::from_bits(bits[4]),
                f64::from_bits(bits[5]),
                f64::from_bits(bits[6]),
            ),
        )
    }
}

impl Mul for Iso3 {
    type Output = Iso3;

    /// Compose two transforms: `a * b` = `T_a_x · T_x_b`.
    #[inline]
    fn mul(self, rhs: Iso3) -> Iso3 {
        let q = self.q * rhs.q;
        let t = self.t.add(self.q.rotate(rhs.t));
        Iso3::new(q, t)
    }
}

/// Evaluate a 4-term series in `θ²` by Horner's method.
#[inline]
fn horner(coeffs: &[f64; 4], theta2: f64) -> f64 {
    coeffs[0] + theta2 * (coeffs[1] + theta2 * (coeffs[2] + theta2 * coeffs[3]))
}

/// The three left-Jacobian coefficients `c1, c2, c3` for angle `θ`.
///
/// Below [`THETA_SMALL`] each uses its four-term Taylor series; above it the
/// closed form. `c3` is only needed by [`log_se3`], but computing all three
/// together keeps the branch in one place.
#[inline]
fn v_coeffs(theta: f64) -> (f64, f64, f64) {
    let theta2 = theta * theta;
    if theta < THETA_SMALL {
        (
            horner(&C1, theta2),
            horner(&C2, theta2),
            horner(&C3, theta2),
        )
    } else {
        let (sin, cos) = libm::sincos(theta);
        let c1 = (1.0 - cos) / theta2;
        let c2 = (theta - sin) / (theta2 * theta);
        let c3 = 1.0 / theta2 - (1.0 + cos) / (2.0 * theta * sin);
        (c1, c2, c3)
    }
}

/// SE(3) exponential: map a twist `ξ = [ω, v]` to a rigid transform.
///
/// `R = exp_so3(ω)` and `t = V(ω)·v` with `V(ω) = I + c1·[ω]× + c2·[ω]×²`.
#[inline]
#[must_use]
pub fn exp_se3(xi: [f64; 6]) -> Iso3 {
    let w = Vec3::new(xi[0], xi[1], xi[2]);
    let v = Vec3::new(xi[3], xi[4], xi[5]);
    let theta = w.norm();
    let (c1, c2, _c3) = v_coeffs(theta);
    // V·v = v + c1·(ω × v) + c2·(ω × (ω × v)).
    let wxv = w.cross(v);
    let wxwxv = w.cross(wxv);
    let t = v.add(wxv.scale(c1)).add(wxwxv.scale(c2));
    Iso3::new(exp_so3(w), t)
}

/// SE(3) logarithm: map a rigid transform to its twist `ξ = [ω, v]`.
///
/// `ω = log_so3(R)` and `v = V⁻¹(ω)·t` with
/// `V⁻¹(ω) = I − ½·[ω]× + c3·[ω]×²`.
#[inline]
#[must_use]
pub fn log_se3(t: Iso3) -> [f64; 6] {
    let w = log_so3(t.q);
    let theta = w.norm();
    let (_c1, _c2, c3) = v_coeffs(theta);
    // V⁻¹·t = t − ½·(ω × t) + c3·(ω × (ω × t)).
    let wxt = w.cross(t.t);
    let wxwxt = w.cross(wxt);
    let v = t.t.sub(wxt.scale(0.5)).add(wxwxt.scale(c3));
    [w.x, w.y, w.z, v.x, v.y, v.z]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::quat::Quat;
    use core::mem::{align_of, size_of};

    // --- Layout (NORMATIVE) -------------------------------------------------

    #[test]
    fn layout_sizes_and_aligns() {
        assert_eq!(size_of::<Vec3>(), 24);
        assert_eq!(align_of::<Vec3>(), 8);
        assert_eq!(size_of::<Quat>(), 32);
        assert_eq!(align_of::<Quat>(), 8);
        assert_eq!(size_of::<Iso3>(), 64);
        assert_eq!(align_of::<Iso3>(), 64);
    }

    #[test]
    fn pod_zeroable_roundtrips() {
        let iso = Iso3::new(Quat::new(0.5, 0.5, 0.5, 0.5), Vec3::new(1.0, -2.0, 3.0));
        // Pod cast to bytes and back is identity.
        let bytes: &[u8] = bytemuck::bytes_of(&iso);
        assert_eq!(bytes.len(), 64);
        let back: Iso3 = *bytemuck::from_bytes::<Iso3>(bytes);
        assert_eq!(back, iso);
        // Zeroable identity.
        let z: Iso3 = bytemuck::Zeroable::zeroed();
        assert_eq!(z.q, Quat::new(0.0, 0.0, 0.0, 0.0));
        assert_eq!(z.t, Vec3::ZERO);
    }

    #[test]
    fn to_bits_from_bits_roundtrip() {
        let iso = Iso3::new(Quat::new(0.1, 0.2, 0.3, 0.4), Vec3::new(-5.0, 6.5, 7.25));
        let bits = iso.to_bits();
        assert_eq!(bits[0], 0.1f64.to_bits());
        assert_eq!(bits[6], 7.25f64.to_bits());
        assert_eq!(Iso3::from_bits(&bits), iso);
    }

    // --- theta-sweep against a high-precision reference table ---------------
    //
    // Reference c1/c2/c3 computed with Python `decimal` at 80 significant
    // digits (scratchpad/refgen.py). Columns: (theta, c1, c2, c3).
    //
    // DEVIATION (documented): decision 0003 asks for rel err < 1e-14 across the
    // whole sweep and a branch-boundary discontinuity < 1e-15. With the
    // NORMATIVE 4-term series + the NORMATIVE naive closed forms, that is
    // provably unreachable for `c3` (and marginally `c2`) in the band
    // ~[0.1, 0.4]: the closed form `1/θ² − (1+cosθ)/(2θ sinθ)` loses ~1e-13 to
    // cancellation there — the spec's own table lists `c3` closed = 9.1e-14 at
    // θ=0.1 and 2.4e-14 at θ=0.3. The 4-term series is only good to ~0.1, so no
    // single threshold covers the gap. We therefore keep the mandated constants
    // and forms verbatim and assert the honest, measured bounds: a tight 1e-14
    // in the series branch (θ < 0.1, where the spec's claim holds) and the
    // achievable bound in the closed branch. The property that actually matters
    // downstream — `exp_se3(log_se3(T)) ≈ T` — is unaffected (rel err ~8e-16),
    // because `c3` multiplies an O(θ²) term; see proptest #3.
    const REF: [(f64, f64, f64, f64); 30] = [
        (1e-12, 0.5, 0.16666666666666666, 0.08333333333333333),
        (
            3.3167640539592015e-12,
            0.5,
            0.16666666666666666,
            0.08333333333333333,
        ),
        (
            1.1000923789635879e-11,
            0.5,
            0.16666666666666666,
            0.08333333333333333,
        ),
        (
            3.6487468585808916e-11,
            0.5,
            0.16666666666666666,
            0.08333333333333333,
        ),
        (
            1.210203242253766e-10,
            0.5,
            0.16666666666666666,
            0.08333333333333333,
        ),
        (
            4.013958611892171e-10,
            0.5,
            0.16666666666666666,
            0.08333333333333333,
        ),
        (
            1.3313353638003871e-09,
            0.5,
            0.16666666666666666,
            0.08333333333333333,
        ),
        (
            4.415725278417821e-09,
            0.5,
            0.16666666666666666,
            0.08333333333333333,
        ),
        (
            1.4645918875615246e-08,
            0.5,
            0.16666666666666666,
            0.08333333333333333,
        ),
        (
            4.857705726384321e-08,
            0.4999999999999999,
            0.16666666666666666,
            0.08333333333333334,
        ),
        (
            1.611186373798329e-07,
            0.49999999999999895,
            0.16666666666666646,
            0.08333333333333337,
        ),
        (
            5.343925048843172e-07,
            0.4999999999999881,
            0.1666666666666643,
            0.08333333333333373,
        ),
        (
            1.772453850905513e-06,
            0.4999999999998691,
            0.16666666666664048,
            0.0833333333333377,
        ),
        (
            5.878811219984968e-06,
            0.49999999999856,
            0.16666666666637867,
            0.08333333333338133,
        ),
        (
            1.9498629734458182e-05,
            0.49999999998415845,
            0.16666666666349836,
            0.08333333333386138,
        ),
        (
            6.467235420471095e-05,
            0.4999999998257286,
            0.1666666666318124,
            0.08333333333914238,
        ),
        (
            0.00021450293971110295,
            0.4999999980828537,
            0.1666666662832374,
            0.08333333339723821,
        ),
        (
            0.000711455639902364,
            0.49999997890962006,
            0.16666666244859066,
            0.08333333403634602,
        ),
        (
            0.0023597304924147028,
            0.4999997679863765,
            0.16666662026393952,
            0.08333334106712324,
        ),
        (
            0.007826669274272533,
            0.49999744764054793,
            0.16666615619447844,
            0.08333341841227955,
        ),
        (
            0.025959215311134088,
            0.4999719222615595,
            0.16666105108293833,
            0.08333426929398892,
        ),
        (
            0.08610059221295688,
            0.49969118832019055,
            0.16660489996992076,
            0.08334363141767584,
        ),
        (
            0.09,
            0.4996625911118206,
            0.1665991796830594,
            0.08334458550341563,
        ),
        (
            0.0999999,
            0.4995834730302009,
            0.16658335333843494,
            0.08334722550213645,
        ),
        (
            0.1,
            0.4995834721974234,
            0.1665833531718477,
            0.08334722552992746,
        ),
        (
            0.1000001,
            0.49958347136464504,
            0.16658335300526028,
            0.08334722555771848,
        ),
        (
            0.11,
            0.49949603663662395,
            0.1665658623780551,
            0.08335014373195444,
        ),
        (
            0.28557534927653494,
            0.49661117063474863,
            0.16598837413588508,
            0.08344682216568886,
        ),
        (
            0.9471860531772549,
            0.4637184564386937,
            0.15934804976842942,
            0.08460661871135167,
        ),
        (
            3.1415926535897825,
            0.20264236728467694,
            0.10132118364233812,
            0.10132118364233761,
        ),
    ];

    #[test]
    fn theta_sweep_matches_reference() {
        for &(theta, r1, r2, r3) in REF.iter() {
            let (c1, c2, c3) = v_coeffs(theta);
            let e1 = ((c1 - r1) / r1).abs();
            let e2 = ((c2 - r2) / r2).abs();
            let e3 = ((c3 - r3) / r3).abs();
            if theta < THETA_SMALL {
                // Series branch: the spec's verified regime. Tight bound.
                assert!(
                    e1 < 1e-14 && e2 < 1e-14 && e3 < 1e-14,
                    "series branch theta={theta}: e1={e1:e} e2={e2:e} e3={e3:e}"
                );
            } else {
                // Closed branch: honest achievable bound with the naive forms.
                assert!(
                    e1 < 2e-14 && e2 < 5e-14 && e3 < 2e-13,
                    "closed branch theta={theta}: e1={e1:e} e2={e2:e} e3={e3:e}"
                );
            }
        }
    }

    #[test]
    fn branch_boundary_value_is_continuous() {
        // Value jump between the two formulas evaluated at THETA_SMALL. With the
        // mandated 4-term series this is bounded below by the series truncation
        // error at 0.1 (~5.6e-15 for c1), so the spec's literal 1e-15 target is
        // unreachable; assert the measured ~7e-15 jump stays under 1e-14.
        let th = THETA_SMALL;
        let t2 = th * th;
        let series = (horner(&C1, t2), horner(&C2, t2), horner(&C3, t2));
        let (sin, cos) = libm::sincos(th);
        let closed = (
            (1.0 - cos) / t2,
            (th - sin) / (t2 * th),
            1.0 / t2 - (1.0 + cos) / (2.0 * th * sin),
        );
        let j1 = (series.0 - closed.0).abs();
        let j2 = (series.1 - closed.1).abs();
        let j3 = (series.2 - closed.2).abs();
        assert!(
            j1 < 1e-14 && j2 < 1e-14 && j3 < 1e-14,
            "boundary jump: j1={j1:e} j2={j2:e} j3={j3:e}"
        );
    }
}
