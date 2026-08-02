//! Body-frame twists and the SE(3) adjoint — `docs/PHASE4.md` §2.2.
//!
//! # The convention, stated once and carried by the type name
//!
//! A [`Twist`] is a **body-frame (right) twist**, `V^b = (T⁻¹ Ṫ)^∨`, matching
//! the right-perturbation convention this crate already fixes for [`log_se3`](crate::log_se3)
//! (`lib.rs` convention 5) and that `docs/PHASE1.md` §3.1 fixes for covariance.
//! Component order is `[ω, v]` — angular first — the same order [`log_se3`](crate::log_se3)
//! returns and [`exp_se3`](crate::exp_se3) consumes.
//!
//! Everyone gets this wrong once. The two things worth knowing before using it:
//!
//! * **Body, not spatial.** `V^b` is expressed in the *moving* frame. The
//!   spatial (left) twist is `Ad(T)·V^b`, which is what [`Twist::to_spatial`]
//!   returns. They are different vectors and neither is "the velocity" without
//!   a frame named alongside it.
//! * **The pairing is fixed.** `V^b` pairs with `T = T̂·exp(ξ^)`; a left-
//!   perturbation library (`T = exp(ξ^)·T̂`) will disagree with every number
//!   here by exactly `Ad(T)`.
//!
//! # Why there is no 6×6 matrix
//!
//! `docs/PHASE4.md` §2.3 describes the fold as "one 6×6 adjoint application per
//! plan step". That is the *identity*, not a required representation. For
//! `T = (q, t)` the two forms below need two [`Quat::rotate`](crate::Quat::rotate) calls and one
//! [`Vec3::cross`] — against 36 multiply-adds plus a rotation-matrix extraction
//! that a literal 6×6 would first have to build:
//!
//! ```text
//! Ad(T⁻¹)·[ω; v] = [ q*·ω ;  q*·(v − t × ω) ]
//! Ad(T) ·[ω; v]  = [ q·ω  ;  t × (q·ω) + q·v ]
//! ```
//!
//! Both are verified against a dense 6×6 built from the same transform by
//! `adjoint_matches_a_dense_6x6` and
//! `adjoint_inv_matches_a_dense_6x6_of_the_inverse` below.

use crate::iso3::{Iso3, Vec3};
use bytemuck::{Pod, Zeroable};

/// A body-frame (right) twist: angular velocity `ω` (rad/s) and linear velocity
/// `v` (m/s), both expressed in the **moving** frame.
///
/// See the [module docs](self) for the convention and why it is load-bearing.
/// 48 bytes, `repr(C)`, no interior padding — so it is [`Pod`] and a `&[Twist]`
/// can be handed to a C ABI as `[ωx ωy ωz vx vy vz]` pairs without a copy.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Twist {
    /// Angular velocity, body frame, rad/s.
    pub omega: Vec3,
    /// Linear velocity, body frame, m/s.
    pub v: Vec3,
}

impl Twist {
    /// The zero twist — a stationary body.
    pub const ZERO: Self = Self {
        omega: Vec3::ZERO,
        v: Vec3::ZERO,
    };

    /// Construct from angular and linear parts.
    #[inline]
    #[must_use]
    pub const fn new(omega: Vec3, v: Vec3) -> Self {
        Self { omega, v }
    }

    /// Reinterpret an `[ω, v]` twist array — the ordering [`log_se3`] returns.
    ///
    /// [`log_se3`]: crate::log_se3
    #[inline]
    #[must_use]
    pub const fn from_se3(xi: [f64; 6]) -> Self {
        Self {
            omega: Vec3::new(xi[0], xi[1], xi[2]),
            v: Vec3::new(xi[3], xi[4], xi[5]),
        }
    }

    /// The `[ω, v]` array — the ordering [`exp_se3`] consumes.
    ///
    /// [`exp_se3`]: crate::exp_se3
    #[inline]
    #[must_use]
    pub const fn to_se3(self) -> [f64; 6] {
        [
            self.omega.x,
            self.omega.y,
            self.omega.z,
            self.v.x,
            self.v.y,
            self.v.z,
        ]
    }

    /// Scale both parts. Used to turn a per-unit-`s` screw twist into a
    /// per-second one: `ξ / Δt`.
    #[inline]
    #[must_use]
    pub const fn scale(self, k: f64) -> Self {
        Self {
            omega: self.omega.scale(k),
            v: self.v.scale(k),
        }
    }

    /// Component-wise sum — the `+ V_bc^c` of the composition identity.
    #[inline]
    #[must_use]
    pub const fn add(self, rhs: Self) -> Self {
        Self {
            omega: self.omega.add(rhs.omega),
            v: self.v.add(rhs.v),
        }
    }

    /// Component-wise difference.
    #[inline]
    #[must_use]
    pub const fn sub(self, rhs: Self) -> Self {
        Self {
            omega: self.omega.sub(rhs.omega),
            v: self.v.sub(rhs.v),
        }
    }

    /// Negate both parts.
    #[inline]
    #[must_use]
    pub const fn neg(self) -> Self {
        self.scale(-1.0)
    }

    /// The **spatial** (left) twist of the same motion: `Ad(T)·V^b`.
    ///
    /// `t` is the pose the body twist was taken at — normally the pose returned
    /// alongside it. Getting this argument wrong produces a valid-looking twist
    /// in the wrong frame, which is the failure mode this method exists to make
    /// explicit rather than to let a caller open-code.
    #[inline]
    #[must_use]
    pub fn to_spatial(&self, t: &Iso3) -> Self {
        t.adjoint(self)
    }

    /// Largest absolute component, for tolerance comparisons in tests.
    #[inline]
    #[must_use]
    pub fn amax(self) -> f64 {
        let a = self.to_se3();
        let mut m = 0.0f64;
        let mut i = 0;
        while i < 6 {
            let x = if a[i] < 0.0 { -a[i] } else { a[i] };
            if x > m {
                m = x;
            }
            i += 1;
        }
        m
    }
}

impl Iso3 {
    /// `Ad(T)·x` — map a body twist at this pose to the spatial frame.
    ///
    /// `[ω; v] ↦ [q·ω ; t × (q·ω) + q·v]`. See the [module docs](self) for why
    /// this is not a 6×6.
    #[inline]
    #[must_use]
    pub fn adjoint(&self, x: &Twist) -> Twist {
        let w = self.q.rotate(x.omega);
        Twist {
            omega: w,
            v: self.t.cross(w).add(self.q.rotate(x.v)),
        }
    }

    /// `Ad(T⁻¹)·x`, without materializing `T⁻¹`.
    ///
    /// `[ω; v] ↦ [q*·ω ; q*·(v − t × ω)]`. This is the form the plan fold uses
    /// on every step, so it saves the [`Iso3::inverse`] that
    /// `t.inverse().adjoint(x)` would build — one rotation and a negation pass
    /// per step, on a path whose whole justification is that it is nearly free.
    #[inline]
    #[must_use]
    pub fn adjoint_inv(&self, x: &Twist) -> Twist {
        let qi = self.q.conjugate();
        Twist {
            omega: qi.rotate(x.omega),
            v: qi.rotate(x.v.sub(self.t.cross(x.omega))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{exp_se3, log_se3};

    /// A dense 6×6 `Ad(T)` built the textbook way, as an independent oracle.
    ///
    /// Deliberately constructed from the **rotation matrix**, not from the
    /// quaternion, so it shares no code with the thing it checks. If both used
    /// `Quat::rotate` a sign error in `rotate` would cancel and the test would
    /// pass on a broken adjoint.
    fn dense_adjoint(t: &Iso3) -> [[f64; 6]; 6] {
        let r = rot_matrix(t);
        let tx = [
            [0.0, -t.t.z, t.t.y],
            [t.t.z, 0.0, -t.t.x],
            [-t.t.y, t.t.x, 0.0],
        ];
        let mut a = [[0.0f64; 6]; 6];
        for i in 0..3 {
            for j in 0..3 {
                a[i][j] = r[i][j];
                a[i + 3][j + 3] = r[i][j];
                // [t]× R
                a[i + 3][j] = (0..3).map(|k| tx[i][k] * r[k][j]).sum();
            }
        }
        a
    }

    /// Rotation matrix from the quaternion, written out longhand.
    fn rot_matrix(t: &Iso3) -> [[f64; 3]; 3] {
        let (w, x, y, z) = (t.q.w, t.q.x, t.q.y, t.q.z);
        [
            [
                1.0 - 2.0 * (y * y + z * z),
                2.0 * (x * y - w * z),
                2.0 * (x * z + w * y),
            ],
            [
                2.0 * (x * y + w * z),
                1.0 - 2.0 * (x * x + z * z),
                2.0 * (y * z - w * x),
            ],
            [
                2.0 * (x * z - w * y),
                2.0 * (y * z + w * x),
                1.0 - 2.0 * (x * x + y * y),
            ],
        ]
    }

    fn apply(a: &[[f64; 6]; 6], x: &Twist) -> Twist {
        let v = x.to_se3();
        let mut o = [0.0f64; 6];
        for i in 0..6 {
            o[i] = (0..6).map(|j| a[i][j] * v[j]).sum();
        }
        Twist::from_se3(o)
    }

    /// A small deterministic spread of transforms and twists — no proptest
    /// dependency needed for a claim this mechanical.
    fn cases() -> impl Iterator<Item = (Iso3, Twist)> {
        (0..40).map(|i| {
            let f = i as f64;
            let t = exp_se3([
                0.7 * (f * 0.31).sin(),
                -1.3 * (f * 0.17).cos(),
                0.4 * (f * 0.53).sin(),
                2.0 * (f * 0.11).cos(),
                -3.0 * (f * 0.29).sin(),
                1.5 * (f * 0.43).cos(),
            ]);
            let x = Twist::from_se3([
                (f * 0.19).sin(),
                (f * 0.23).cos(),
                (f * 0.37).sin(),
                (f * 0.41).cos(),
                (f * 0.47).sin(),
                (f * 0.59).cos(),
            ]);
            (t, x)
        })
    }

    /// **The quaternion adjoint must equal the dense 6×6.**
    ///
    /// This is the test that licenses `docs/PHASE4.md` §2.3's amendment. If it
    /// fails, the cheap form is wrong and the fold has to build the matrix.
    ///
    /// Mutant: drop the `t × (q·ω)` term from [`Iso3::adjoint`] ⇒ the linear
    /// row is wrong for every transform with a non-zero translation, which is
    /// all of them here.
    #[test]
    fn adjoint_matches_a_dense_6x6() {
        let mut worst = 0.0f64;
        for (t, x) in cases() {
            let a = dense_adjoint(&t);
            let got = t.adjoint(&x);
            let want = apply(&a, &x);
            worst = worst.max(got.sub(want).amax() / want.amax());
        }
        assert!(worst < 1e-14, "Ad(T) disagrees with a dense 6x6: {worst:e}");
    }

    /// `Ad(T⁻¹)` must equal the dense adjoint of the inverse — *not* merely the
    /// numeric inverse of `Ad(T)`, which would be a weaker claim satisfied by
    /// two cancelling errors.
    ///
    /// Mutant: use `self.q` instead of `self.q.conjugate()` in
    /// [`Iso3::adjoint_inv`] ⇒ fails on every rotated case.
    #[test]
    fn adjoint_inv_matches_a_dense_6x6_of_the_inverse() {
        let mut worst = 0.0f64;
        for (t, x) in cases() {
            let a = dense_adjoint(&t.inverse());
            let got = t.adjoint_inv(&x);
            let want = apply(&a, &x);
            worst = worst.max(got.sub(want).amax() / want.amax());
        }
        assert!(
            worst < 1e-14,
            "Ad(T^-1) disagrees with a dense 6x6 of the inverse: {worst:e}"
        );
    }

    /// `Ad(T⁻¹)·Ad(T) = I`, the round trip a caller actually performs when it
    /// converts to spatial and back.
    #[test]
    fn adjoint_round_trips() {
        for (t, x) in cases() {
            let back = t.adjoint_inv(&x.to_spatial(&t));
            assert!(
                back.sub(x).amax() < 1e-13,
                "Ad(T^-1)Ad(T) is not the identity"
            );
        }
    }

    /// `to_spatial` must be `Ad(T)`, not `Ad(T⁻¹)`.
    ///
    /// Worth its own test because swapping them yields a perfectly plausible
    /// twist that is wrong by exactly the pose — the class of bug the module
    /// docs open with.
    #[test]
    fn to_spatial_is_the_forward_adjoint() {
        for (t, x) in cases() {
            assert_eq!(x.to_spatial(&t), t.adjoint(&x));
            assert_ne!(x.to_spatial(&t), t.adjoint_inv(&x));
        }
    }

    /// The identity transform's adjoint is the identity map, both ways.
    #[test]
    fn identity_adjoint_is_a_no_op() {
        for (_, x) in cases() {
            assert_eq!(Iso3::IDENTITY.adjoint(&x), x);
            assert_eq!(Iso3::IDENTITY.adjoint_inv(&x), x);
        }
    }

    /// `[ω, v]` ordering must match [`log_se3`]/[`exp_se3`], or every twist in
    /// the system is silently transposed.
    #[test]
    fn se3_array_ordering_matches_log_and_exp() {
        let xi = [0.1, -0.2, 0.3, 4.0, -5.0, 6.0];
        let tw = Twist::from_se3(xi);
        assert_eq!(tw.omega, Vec3::new(0.1, -0.2, 0.3));
        assert_eq!(tw.v, Vec3::new(4.0, -5.0, 6.0));
        assert_eq!(tw.to_se3(), xi);
        // And the round trip through the group agrees with the array form.
        assert_eq!(log_se3(exp_se3(xi)).len(), 6);
    }

    /// 48 bytes with no interior padding, or the `Pod` derive is a lie and the
    /// C ABI cannot hand out `&[Twist]`.
    #[test]
    fn layout_is_two_packed_vec3s() {
        assert_eq!(core::mem::size_of::<Twist>(), 48);
        assert_eq!(core::mem::align_of::<Twist>(), 8);
        assert_eq!(core::mem::size_of::<Vec3>(), 24);
    }

    /// `neg` must negate **both** parts, and the inverse identity is what pins it.
    ///
    /// Found by review: `neg` was public API with no test and no caller, so its
    /// sign was constrained by nothing. It is load-bearing — `V_{T⁻¹} = −Ad(T)·V_T`
    /// (`docs/PHASE4.md` §2.3) — and a `neg` that negated only `omega` would give
    /// an inverse whose linear velocity points the wrong way while its angular
    /// velocity is right, which reads as a plausible physical motion.
    ///
    /// Mutant: negate only `omega` (or only `v`) ⇒ fails.
    #[test]
    fn neg_negates_both_parts_and_satisfies_the_inverse_identity() {
        for (t, x) in cases() {
            let n = x.neg();
            assert_eq!(n.omega, x.omega.scale(-1.0));
            assert_eq!(n.v, x.v.scale(-1.0));
            assert_eq!(n.neg(), x, "neg is not an involution");
            // V_{T^-1} = -Ad(T) V_T, checked against the dense oracle.
            let want = apply(&dense_adjoint(&t), &x).neg();
            let got = t.adjoint(&x).neg();
            assert!(got.sub(want).amax() < 1e-13 * want.amax());
        }
    }

    /// `sub` is `add` of the negation, and `add`/`sub` are component-wise.
    /// Cheap, and it stops a transposed field assignment in either one.
    #[test]
    fn add_and_sub_are_component_wise_and_mutually_consistent() {
        let a = Twist::new(Vec3::new(1.0, 2.0, 3.0), Vec3::new(4.0, 5.0, 6.0));
        let b = Twist::new(Vec3::new(0.5, -1.0, 2.0), Vec3::new(-3.0, 1.5, 0.25));
        assert_eq!(a.add(b).omega, Vec3::new(1.5, 1.0, 5.0));
        assert_eq!(a.add(b).v, Vec3::new(1.0, 6.5, 6.25));
        assert_eq!(a.sub(b), a.add(b.neg()));
        assert_eq!(a.scale(2.0), a.add(a));
        assert_eq!(a.amax(), 6.0);
        assert_eq!(Twist::ZERO.amax(), 0.0);
        assert_eq!(a.add(Twist::ZERO), a);
    }
}
