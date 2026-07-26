//! Output layouts — folding a plan straight into the caller's buffer.
//!
//! # Why this exists
//!
//! [`crate::Plan::at_many`] writes `Iso3`, which is `#[repr(C, align(64))]` and
//! therefore **64 bytes with 8 of padding**. Every layout a consumer actually
//! wants has a different stride: a 4x4 `f64` matrix is 128, a `[qw qx qy qz tx
//! ty tz]` row is 56, a 3x4 `f32` affine is 48. So `&mut [Iso3]` cannot alias
//! any of them, and a caller wanting one has to allocate an `Iso3` buffer,
//! evaluate into it, and then convert — two passes over the data and one
//! allocation that exists only because the shapes disagree.
//!
//! These kernels fold **directly into the destination**. `docs/PHASE3.md` §5.2's
//! "zero copies" is not a figure of speech: there is no intermediate buffer,
//! and for `at_many_into` no allocation at all.
//!
//! # Shape of the hot loop
//!
//! The layout is matched **once**, outside the loop, and each arm runs its own
//! monotone-cursor loop. That is deliberate and not duplication for its own
//! sake: a `match` on the layout *inside* the loop would put an unpredictable
//! branch between every element and the next, and the whole point of a batch
//! API is that the per-element cost is a handful of nanoseconds. Emitters are
//! `#[inline]` and branch-free — straight-line stores from an `Iso3` already in
//! registers.
//!
//! # Why `f32` is only the affine layout
//!
//! `docs/PROJECT.md` §5 says `f64` only, and that holds for everything the
//! engine *computes*. `Affine32` is an output *encoding* for consumers that are
//! going to upload the result to a GPU, where `f32` is the native format and
//! the conversion would otherwise happen anyway — one pass later, over a buffer
//! that had to exist. Nothing reads it back.

use tf_tree_math::Iso3;

/// How a transform is written into a caller's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Layout {
    /// Row-major 4x4 homogeneous matrix, `f64`. 16 elements.
    ///
    /// What NumPy users mean by "a transform": `(N, 4, 4)`.
    Mat4,
    /// `[qw, qx, qy, qz, tx, ty, tz]`, `f64`. 7 elements.
    ///
    /// The engine's own representation minus the padding, so this is the
    /// cheapest layout to emit and the one to prefer when the consumer does not
    /// specifically need a matrix.
    Quat,
    /// Row-major 3x4 affine, `f32`. 12 elements.
    ///
    /// The bottom row of a rigid transform is always `[0 0 0 1]`, so storing it
    /// wastes a quarter of the buffer and all of the bandwidth that goes with
    /// it. GPU-facing.
    Affine32,
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
/// The rotation is expanded from the quaternion directly rather than through a
/// `Mat3` type: the products below are shared between entries, so doing it in
/// one place lets the compiler keep every one of them in a register.
#[inline]
pub(crate) fn write_mat4(iso: &Iso3, out: &mut [f64]) {
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

    // The bottom row is constant for a rigid transform. Written rather than
    // assumed: the caller's buffer may be reused across calls, and leaving it
    // to whatever was there before would make a stale row look like a valid
    // projective transform.
    out[12] = 0.0;
    out[13] = 0.0;
    out[14] = 0.0;
    out[15] = 1.0;
}

/// Write `iso` as `[qw qx qy qz tx ty tz]`.
#[inline]
pub(crate) fn write_quat(iso: &Iso3, out: &mut [f64]) {
    out[0] = iso.q.w;
    out[1] = iso.q.x;
    out[2] = iso.q.y;
    out[3] = iso.q.z;
    out[4] = iso.t.x;
    out[5] = iso.t.y;
    out[6] = iso.t.z;
}

/// Write `iso` as a row-major 3x4 `f32` affine.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn write_affine32(iso: &Iso3, out: &mut [f32]) {
    let q = iso.q;
    let (w, x, y, z) = (q.w, q.x, q.y, q.z);
    let (xx, yy, zz) = (x * x, y * y, z * z);
    let (xy, xz, yz) = (x * y, x * z, y * z);
    let (wx, wy, wz) = (w * x, w * y, w * z);

    // Computed in f64 and narrowed once at the store. Doing the algebra in f32
    // would lose bits the engine went to trouble to keep, for no bandwidth
    // saving — the buffer is the same size either way.
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

    /// A 4x4 emitted from a rotation must be orthonormal with det = +1.
    ///
    /// Transposing the rotation block — the classic row/column-major slip — is
    /// still orthonormal, so this is checked against the *action* on a vector
    /// below, not by itself.
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

    /// **The orientation check.** `M * v` must equal what the engine's own
    /// `Iso3` application gives — which is what catches a transposed rotation
    /// block, the one error orthonormality cannot see.
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
        // The independent reference: the engine's own quaternion rotation
        // plus the translation. Nothing here shares code with `write_mat4`.
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

    #[test]
    fn element_counts_match_the_emitters() {
        assert_eq!(Layout::Mat4.elems(), 16);
        assert_eq!(Layout::Quat.elems(), 7);
        assert_eq!(Layout::Affine32.elems(), 12);
        assert!(Layout::Affine32.is_f32());
        assert!(!Layout::Mat4.is_f32());
        assert!(!Layout::Quat.is_f32());
    }
}
