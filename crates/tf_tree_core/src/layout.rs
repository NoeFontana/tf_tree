//! Output layouts — folding a plan straight into the caller's buffer.
//!
//! [`crate::Plan::at_many`] writes `Iso3`; these kernels fold **directly into the
//! caller's buffer** (`docs/PHASE3.md` §5.2), with no intermediate buffer. The
//! layout is matched once outside the loop; emitters are `#[inline]`, branch-free.
//!
//! `Affine32` is an output *encoding* for GPU upload; `f64`-only arithmetic
//! (`docs/PROJECT.md` §5 D6) still holds.

use tf_tree_math::{Iso3, Twist};

/// How a transform is written into a caller's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Layout {
    /// Row-major 4x4 homogeneous matrix, `f64`. 16 elements.
    Mat4,
    /// `[qw, qx, qy, qz, tx, ty, tz]`, `f64`. 7 elements.
    ///
    /// Byte-for-byte the engine's own `Iso3` ([`0042`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0042-the-cacheline-the-arena-never-asked-for.md)).
    Quat,
    /// Row-major 3x4 affine, `f32`. 12 elements.
    ///
    /// Omits the constant `[0 0 0 1]` row. GPU-facing.
    Affine32,
    /// `[qw qx qy qz tx ty tz | ωx ωy ωz vx vy vz]`, `f64`. 13 elements.
    ///
    /// [`Layout::Quat`] plus the body twist, angular first (`[ω, v]`, as
    /// [`tf_tree_math::twist`]); carries `at_with_derivatives` to batch callers
    /// (`docs/API.md` §3.3, `docs/PHASE5.md` §4.4). The one layout whose emission
    /// can fail: `LerpSlerp` edges yield [`LookupError::DerivativesUnavailable`].
    ///
    /// [`LookupError::DerivativesUnavailable`]: crate::LookupError::DerivativesUnavailable
    QuatTwist,
}

impl Layout {
    /// Elements one transform occupies.
    #[inline]
    #[must_use]
    pub const fn elems(self) -> usize {
        match self {
            Layout::Mat4 => 16,
            Layout::Quat => 7,
            Layout::Affine32 => 12,
            Layout::QuatTwist => 13,
        }
    }

    /// Whether this layout is emitted as `f32` rather than `f64`.
    #[inline]
    #[must_use]
    pub const fn is_f32(self) -> bool {
        matches!(self, Layout::Affine32)
    }
}

/// Write `iso` as a row-major 4x4 `f64` matrix.
///
/// Public so a binding can emit a single transform (`docs/PHASE3.md` §4.2).
#[inline]
pub fn write_mat4(iso: &Iso3, out: &mut [f64]) {
    let q = iso.q;
    let (w, x, y, z) = (q.w, q.x, q.y, q.z);
    let (xx, yy, zz) = (x * x, y * y, z * z);
    let (xy, xz, yz) = (x * y, x * z, y * z);
    let (wx, wy, wz) = (w * x, w * y, w * z);

    out[0] = 1.0 - 2.0 * (yy + zz);
    out[1] = 2.0 * (xy - wz);
    out[2] = 2.0 * (xz + wy);
    out[3] = iso.t.x;

    out[4] = 2.0 * (xy + wz);
    out[5] = 1.0 - 2.0 * (xx + zz);
    out[6] = 2.0 * (yz - wx);
    out[7] = iso.t.y;

    out[8] = 2.0 * (xz - wy);
    out[9] = 2.0 * (yz + wx);
    out[10] = 1.0 - 2.0 * (xx + yy);
    out[11] = iso.t.z;

    // Written, not assumed: the buffer may be reused.
    out[12] = 0.0;
    out[13] = 0.0;
    out[14] = 0.0;
    out[15] = 1.0;
}

/// Write `iso` as `[qw qx qy qz tx ty tz]`.
#[inline]
pub fn write_quat(iso: &Iso3, out: &mut [f64]) {
    out[0] = iso.q.w;
    out[1] = iso.q.x;
    out[2] = iso.q.y;
    out[3] = iso.q.z;
    out[4] = iso.t.x;
    out[5] = iso.t.y;
    out[6] = iso.t.z;
}

/// Write `iso` and `twist` as `[qw qx qy qz tx ty tz | ωx ωy ωz vx vy vz]`.
///
/// The first seven elements are [`write_quat`]'s; the tail is `[ω, v]`, **angular
/// first**, body-frame in the plan's **source** frame (`Plan::at_with_derivatives`).
#[inline]
pub fn write_quat_twist(iso: &Iso3, twist: &Twist, out: &mut [f64]) {
    write_quat(iso, out);
    out[7] = twist.omega.x;
    out[8] = twist.omega.y;
    out[9] = twist.omega.z;
    out[10] = twist.v.x;
    out[11] = twist.v.y;
    out[12] = twist.v.z;
}

/// Write `iso` as a row-major 3x4 `f32` affine.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn write_affine32(iso: &Iso3, out: &mut [f32]) {
    let q = iso.q;
    let (w, x, y, z) = (q.w, q.x, q.y, q.z);
    let (xx, yy, zz) = (x * x, y * y, z * z);
    let (xy, xz, yz) = (x * y, x * z, y * z);
    let (wx, wy, wz) = (w * x, w * y, w * z);

    // Computed in f64, narrowed once at the store.
    out[0] = (1.0 - 2.0 * (yy + zz)) as f32;
    out[1] = (2.0 * (xy - wz)) as f32;
    out[2] = (2.0 * (xz + wy)) as f32;
    out[3] = iso.t.x as f32;

    out[4] = (2.0 * (xy + wz)) as f32;
    out[5] = (1.0 - 2.0 * (xx + zz)) as f32;
    out[6] = (2.0 * (yz - wx)) as f32;
    out[7] = iso.t.y as f32;

    out[8] = (2.0 * (xz - wy)) as f32;
    out[9] = (2.0 * (yz + wx)) as f32;
    out[10] = (1.0 - 2.0 * (xx + yy)) as f32;
    out[11] = iso.t.z as f32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_tree_math::exp_se3;

    fn sample() -> Iso3 {
        exp_se3([0.3, -0.2, 0.15, 1.5, -2.5, 0.75])
    }

    /// The rotation block is orthonormal (a transpose passes; see the next test).
    #[test]
    fn mat4_rotation_block_is_orthonormal() {
        let mut m = [0.0f64; 16];
        write_mat4(&sample(), &mut m);
        for (i, j) in [(0, 1), (0, 2), (1, 2)] {
            let dot: f64 = (0..3).map(|k| m[i * 4 + k] * m[j * 4 + k]).sum();
            assert!(dot.abs() < 1e-12, "rows {i},{j} not orthogonal: {dot}");
        }
        for i in 0..3 {
            let n: f64 = (0..3).map(|k| m[i * 4 + k] * m[i * 4 + k]).sum();
            assert!((n - 1.0).abs() < 1e-12, "row {i} not unit: {n}");
        }
        assert_eq!(&m[12..16], &[0.0, 0.0, 0.0, 1.0]);
    }

    /// `M * v` equals the engine's own `Iso3` application; catches a transpose.
    #[test]
    fn mat4_acts_on_a_point_exactly_as_the_iso_does() {
        let iso = sample();
        let mut m = [0.0f64; 16];
        write_mat4(&iso, &mut m);

        let v = [0.7, -1.3, 2.1];
        let by_matrix = [
            m[0] * v[0] + m[1] * v[1] + m[2] * v[2] + m[3],
            m[4] * v[0] + m[5] * v[1] + m[6] * v[2] + m[7],
            m[8] * v[0] + m[9] * v[1] + m[10] * v[2] + m[11],
        ];
        let p = tf_tree_math::Vec3 {
            x: v[0],
            y: v[1],
            z: v[2],
        };
        let r = iso.q.rotate(p);
        let by_iso = tf_tree_math::Vec3 {
            x: r.x + iso.t.x,
            y: r.y + iso.t.y,
            z: r.z + iso.t.z,
        };
        for (got, want) in by_matrix.iter().zip([by_iso.x, by_iso.y, by_iso.z]) {
            assert!((got - want).abs() < 1e-12, "{got} != {want}");
        }
    }

    /// The quat layout is the engine's own order, and must not reorder it.
    #[test]
    fn quat_layout_is_w_first_then_translation() {
        let iso = sample();
        let mut q = [0.0f64; 7];
        write_quat(&iso, &mut q);
        assert_eq!(q[0], iso.q.w);
        assert_eq!(q[1], iso.q.x);
        assert_eq!(q[2], iso.q.y);
        assert_eq!(q[3], iso.q.z);
        assert_eq!(q[4], iso.t.x);
        assert_eq!(q[5], iso.t.y);
        assert_eq!(q[6], iso.t.z);
    }

    /// The affine is the 4x4's top three rows, narrowed — nothing else.
    #[test]
    fn affine32_is_the_first_three_rows_of_the_mat4() {
        let iso = sample();
        let mut m = [0.0f64; 16];
        write_mat4(&iso, &mut m);
        let mut a = [0.0f32; 12];
        write_affine32(&iso, &mut a);

        for row in 0..3 {
            for col in 0..4 {
                let want = m[row * 4 + col] as f32;
                let got = a[row * 4 + col];
                assert!(
                    (got - want).abs() <= f32::EPSILON * 4.0,
                    "row {row} col {col}: {got} != {want}"
                );
            }
        }
    }

    /// `QuatTwist`'s first seven elements are bit-for-bit `Quat`'s.
    #[test]
    fn quat_twist_opens_with_exactly_the_quat_layout() {
        let iso = sample();
        let twist = Twist::new(
            tf_tree_math::Vec3::new(0.11, -0.22, 0.33),
            tf_tree_math::Vec3::new(-1.5, 2.25, 0.125),
        );
        let mut q = [0.0f64; 7];
        write_quat(&iso, &mut q);
        let mut qt = [0.0f64; 13];
        write_quat_twist(&iso, &twist, &mut qt);

        for (i, want) in q.iter().enumerate() {
            assert_eq!(
                qt[i].to_bits(),
                want.to_bits(),
                "element {i} differs from the Quat layout"
            );
        }
    }

    /// The tail is `[ω, v]`, angular first; fixture uses same-magnitude components.
    #[test]
    fn quat_twist_tail_is_omega_then_v() {
        let twist = Twist::new(
            tf_tree_math::Vec3::new(0.11, -0.22, 0.33),
            tf_tree_math::Vec3::new(0.44, -0.55, 0.66),
        );
        let mut qt = [0.0f64; 13];
        write_quat_twist(&sample(), &twist, &mut qt);
        assert_eq!(qt[7], twist.omega.x);
        assert_eq!(qt[8], twist.omega.y);
        assert_eq!(qt[9], twist.omega.z);
        assert_eq!(qt[10], twist.v.x);
        assert_eq!(qt[11], twist.v.y);
        assert_eq!(qt[12], twist.v.z);
    }

    #[test]
    fn element_counts_match_the_emitters() {
        assert_eq!(Layout::Mat4.elems(), 16);
        assert_eq!(Layout::Quat.elems(), 7);
        assert_eq!(Layout::Affine32.elems(), 12);
        assert_eq!(Layout::QuatTwist.elems(), 13);
        assert!(Layout::Affine32.is_f32());
        assert!(!Layout::Mat4.is_f32());
        assert!(!Layout::Quat.is_f32());
        // `is_f32` routes `QuatTwist` to `at_many_into`, not `_f32`.
        assert!(!Layout::QuatTwist.is_f32());
    }
}
