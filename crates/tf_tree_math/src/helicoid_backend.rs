//! The SO(3) bodies delegated to `helicoid` under the `helicoid` feature (`docs/decisions/0063`).
//!
//! Each function converts at the boundary and calls one `helicoid` routine; the arithmetic is
//! `helicoid`'s. `Quat` and `Vec3` keep their layouts, so nothing here crosses a storage contract.
//!
//! A quaternion enters on `helicoid`'s *carried* path, a struct literal moved into
//! `SO3::from_quat_unchecked`, never through the vouching `Quat::from_wxyz_unchecked`. `Iso3`
//! composition never normalizes, so this crate's quaternions drift by design. The carried path
//! asserts nothing, propagates NaN, and states each operation's error up to
//! `|‖q‖² − 1| ≤ 2^-26.29` (`helicoid`'s `NUMERICS.md` §12).

use crate::iso3::Vec3;
use crate::quat::Quat;
use helicoid::{LieGroup, Quat as HQuat, SO3Tangent, SO3};
use helicoid_linalg::{Matrix, Vector};

#[inline]
fn so3(q: Quat) -> SO3<f64> {
    SO3::from_quat_unchecked(HQuat {
        w: q.w,
        x: q.x,
        y: q.y,
        z: q.z,
    })
}

#[inline]
fn quat(r: SO3<f64>) -> Quat {
    let q = r.quat();
    Quat::new(q.w, q.x, q.y, q.z)
}

#[inline]
pub(crate) fn exp_so3(w: Vec3) -> Quat {
    quat(SO3::exp(&SO3Tangent {
        phi: Vector([w.x, w.y, w.z]),
    }))
}

#[inline]
pub(crate) fn log_so3(q: Quat) -> Vec3 {
    let [x, y, z] = so3(q).log().phi.0;
    Vec3::new(x, y, z)
}

/// Returns the **normalized** quaternion `SO3::from_matrix` does; the native body does not
/// normalize (`docs/decisions/0063` decision 3).
#[inline]
pub(crate) fn quat_from_rot3(r: &[f64; 9]) -> Quat {
    let row = |i: usize| Vector([r[3 * i], r[3 * i + 1], r[3 * i + 2]]);
    quat(SO3::from_matrix(&Matrix::from_rows([
        row(0),
        row(1),
        row(2),
    ])))
}

#[inline]
pub(crate) fn slerp(qa: Quat, qb: Quat, s: f64) -> Quat {
    quat(SO3::geodesic(&so3(qa), &so3(qb), s))
}

/// Bit-identical to the native sandwich, drifted or not: the same products, in the same order.
#[inline]
pub(crate) fn rotate(q: Quat, v: Vec3) -> Vec3 {
    let [x, y, z] = so3(q).act(Vector([v.x, v.y, v.z])).0;
    Vec3::new(x, y, z)
}

/// The adapter twins (`docs/decisions/0063` decision 5): each delegated function against the body
/// that ran before delegation, on a deterministic sweep. Tolerances are measured, not derived: the
/// worst observed distance is asserted as a band, so an unrelated change does not fail them.
#[cfg(test)]
mod twin_tests {
    use super::*;
    use crate::interp::slerp_native;
    use crate::quat::{exp_so3_native, log_so3_native, quat_from_rot3_native};

    /// Rotation-vector magnitudes: both sides of `EXP_SO3_SMALL`, the series switches, and `π`.
    const THETAS: [f64; 14] = [
        0.0,
        1e-12,
        1e-9,
        1e-8,
        1e-6,
        1e-3,
        0.05,
        0.15,
        0.5,
        1.0,
        2.0,
        3.0,
        3.1,
        core::f64::consts::PI - 1e-9,
    ];

    /// A unit direction, varied across `k` (golden-ratio steps; no `rand`).
    fn axis(k: usize) -> Vec3 {
        let a = 0.618_033_988_749_894_9 * (k as f64 + 1.0);
        let (u, v) = (
            a.fract() * 2.0 - 1.0,
            (a * 7.0).fract() * core::f64::consts::TAU,
        );
        let r = (1.0 - u * u).sqrt();
        Vec3::new(r * v.cos(), r * v.sin(), u)
    }

    fn rotation_vectors() -> impl Iterator<Item = Vec3> {
        THETAS
            .iter()
            .enumerate()
            .flat_map(|(i, &t)| (0..8).map(move |k| axis(7 * i + k).scale(t)))
    }

    fn dist(a: Quat, b: Quat) -> f64 {
        a.sub(b).norm().min(a.add(b).norm())
    }

    /// `q` as the row-major `[f64; 9]` `quat_from_rot3` consumes.
    fn rot3(q: Quat) -> [f64; 9] {
        let (w, x, y, z) = (q.w, q.x, q.y, q.z);
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - w * z),
            2.0 * (x * z + w * y),
            2.0 * (x * y + w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - w * x),
            2.0 * (x * z - w * y),
            2.0 * (y * z + w * x),
            1.0 - 2.0 * (x * x + y * y),
        ]
    }

    #[test]
    fn exp_so3_agrees_with_the_native_twin() {
        let worst = rotation_vectors()
            .map(|w| exp_so3(w).sub(exp_so3_native(w)).norm())
            .fold(0.0, f64::max);
        assert!(worst < 1e-15, "exp_so3 differs by {worst:e}");
    }

    #[test]
    fn log_so3_agrees_with_the_native_twin() {
        let worst = rotation_vectors()
            .map(|w| {
                let q = exp_so3_native(w);
                log_so3(q).sub(log_so3_native(q)).norm()
            })
            .fold(0.0, f64::max);
        assert!(worst < 1e-14, "log_so3 differs by {worst:e}");
    }

    /// The recorded behaviour change (`0063` decision 3): the native result is not normalized, so
    /// the twin is compared after `normalize`, and up to the sign of the quaternion.
    #[test]
    fn quat_from_rot3_agrees_with_the_normalized_native_twin() {
        let worst = rotation_vectors()
            .map(|w| {
                let r = rot3(exp_so3_native(w));
                dist(quat_from_rot3(&r), quat_from_rot3_native(&r).normalize())
            })
            .fold(0.0, f64::max);
        assert!(worst < 1e-14, "quat_from_rot3 differs by {worst:e}");
    }

    #[test]
    fn slerp_agrees_with_the_native_twin() {
        let qs: [Quat; 4] =
            core::array::from_fn(|i| exp_so3_native(axis(i).scale(0.7 * i as f64 + 0.01)));
        let mut worst = 0.0f64;
        for (i, &qa) in qs.iter().enumerate() {
            for &qb in &qs[i + 1..] {
                for s in [0.0, 0.1, 0.25, 0.5, 0.9, 1.0] {
                    worst = worst.max(dist(slerp(qa, qb, s), slerp_native(qa, qb, s)));
                }
            }
        }
        assert!(worst < 1e-14, "slerp differs by {worst:e}");
    }

    #[test]
    fn slerp_endpoints_agree_with_the_native_twin() {
        let qa = exp_so3_native(axis(3).scale(0.4));
        for theta in [1e-8, 1e-3, 0.9] {
            let qb = qa * exp_so3_native(axis(5).scale(theta));
            assert!(
                dist(slerp(qa, qb, 0.0), qa) < 4e-16,
                "s = 0, theta {theta:e}"
            );
            assert!(
                dist(slerp(qa, qb, 1.0), qb) < 4e-16,
                "s = 1, theta {theta:e}"
            );
        }
    }

    #[test]
    fn slerp_of_identical_inputs_is_qa_to_a_few_ulp() {
        for k in 0..16 {
            let qa = exp_so3_native(axis(k).scale(0.3 * k as f64 + 0.05));
            for j in 0..=10 {
                let d = dist(slerp(qa, qa, j as f64 / 10.0), qa);
                assert!(d < 1e-15, "k={k} j={j}: {d:e}");
            }
        }
    }

    /// The native series collapses outside `[0, 1]`; `helicoid`'s geodesic is `x0 ⊕ s d`, so it holds.
    #[test]
    fn slerp_extrapolates_off_the_segment() {
        let mut worst = 0.0f64;
        for k in 0..16 {
            let qa = exp_so3_native(axis(k).scale(0.3 * k as f64 + 0.05));
            let ax = axis(k + 40);
            for theta in [0.02, 0.1, 0.5] {
                let qb = qa * exp_so3_native(ax.scale(theta));
                for s in [-20.0, -5.0, 2.0, 5.0, 20.0] {
                    let want = qa * exp_so3_native(ax.scale(s * theta));
                    worst = worst.max(dist(slerp(qa, qb, s), want));
                }
            }
        }
        assert!(worst < 1e-13, "extrapolation differs by {worst:e}");
    }

    /// `q` scaled so that `‖q‖² − 1 = eta`: the drift `Iso3` composition carries.
    fn drifted(q: Quat, eta: f64) -> Quat {
        q.scale((1.0 + eta).sqrt())
    }

    /// Unit, at `helicoid`'s vouched bound `2^-40`, and out to the edge of its drift band.
    const DRIFTS: [f64; 5] = [0.0, 9.1e-13, -9.1e-13, 1.2e-8, -1.2e-8];

    #[test]
    fn rotate_is_the_native_twin_to_the_bit_drifted_or_not() {
        let v = [
            Vec3::new(1.0, -2.0, 0.5),
            Vec3::new(-3e-9, 7.0, 1e6),
            Vec3::new(0.0, 0.0, 1.0),
        ];
        let bits = |a: Vec3| [a.x, a.y, a.z].map(f64::to_bits);
        for w in rotation_vectors() {
            for eta in DRIFTS {
                let q = drifted(exp_so3_native(w), eta);
                for &v in &v {
                    assert_eq!(
                        bits(rotate(q, v)),
                        bits(q.rotate_native(v)),
                        "{w:?} {eta:e}"
                    );
                }
            }
        }
    }

    /// A drifted quaternion reaches every delegated function without a debug panic, and `log_so3`
    /// stays the native twin's: both are scale-invariant.
    #[test]
    fn a_drifted_quaternion_is_carried_through_every_delegated_function() {
        let mut worst = 0.0f64;
        for w in rotation_vectors() {
            for eta in DRIFTS {
                let q = drifted(exp_so3_native(w), eta);
                worst = worst.max(log_so3(q).sub(log_so3_native(q)).norm());
                let other = drifted(exp_so3_native(w.scale(0.5)), -eta);
                let _ = slerp(q, other, 0.3);
            }
        }
        assert!(worst < 1e-14, "log_so3 of a drifted q differs by {worst:e}");
    }

    #[test]
    fn nan_propagates_through_every_delegated_function() {
        let q = Quat::new(f64::NAN, 0.1, 0.2, 0.3);
        let one = exp_so3_native(Vec3::new(0.3, -0.2, 0.1));
        let log = log_so3(q);
        assert!(log.x.is_nan() && log.y.is_nan() && log.z.is_nan());
        assert!(rotate(q, Vec3::new(1.0, 2.0, 3.0)).x.is_nan());
        assert!(slerp(q, one, 0.5).w.is_nan());
        assert!(slerp(one, q, 0.5).w.is_nan());
        assert!(slerp(one, one, f64::NAN).w.is_nan());
    }
}
