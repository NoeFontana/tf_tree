//! Output layouts — `docs/PHASE4.md` §3.5.
//!
//! **Quaternion order:** `Eigen::Quaterniond` stores `(x, y, z, w)`, so a `memcpy` from
//! [`TFT_LAYOUT_QVEC7_WXYZ`] is a different rotation; [`TFT_LAYOUT_QVEC7_XYZW`] exists for it.
//! **Matrix major order:** row- vs column-major is a transpose, a rotation's inverse; the enum is
//! always explicit.

use tf_tree::{Iso3, Twist};

/// How to write a transform into caller memory.
pub type tft_layout = u32;

/// `[qw qx qy qz tx ty tz]` `f64` — canonical, matches the arena.
pub const TFT_LAYOUT_QVEC7_WXYZ: tft_layout = 0;
/// `[qx qy qz qw tx ty tz]` `f64` — **Eigen/Sophus coefficient order**.
pub const TFT_LAYOUT_QVEC7_XYZW: tft_layout = 1;
/// 4×4 `f64` column-major — Eigen's `Isometry3d`.
pub const TFT_LAYOUT_MAT4_COL: tft_layout = 2;
/// 4×4 `f64` row-major — C and NumPy.
pub const TFT_LAYOUT_MAT4_ROW: tft_layout = 3;
/// 3×4 `f32` row-major — GPU upload.
pub const TFT_LAYOUT_AFFINE12_ROW_F32: tft_layout = 4;
/// `[qw qx qy qz tx ty tz | ωx ωy ωz vx vy vz]` `f64` — pose **and body twist**.
///
/// [`TFT_LAYOUT_QVEC7_WXYZ`] plus the `[ω, v]` twist `TFT_TWIST_BYTES` describes (`docs/API.md` §3.3).
/// Asking for it *is* asking for derivatives: an edge interpolating with `LerpSlerp` returns
/// `TFT_ERR_NO_DERIVATIVES` and writes nothing for that element or any after it. Not readable.
pub const TFT_LAYOUT_QVEC7_WXYZ_TWIST6: tft_layout = 5;

/// The **bytes** one transform occupies in `layout`, or `None` for an undefined discriminant.
#[must_use]
pub fn payload_bytes(layout: tft_layout) -> Option<usize> {
    Some(match layout {
        TFT_LAYOUT_QVEC7_WXYZ | TFT_LAYOUT_QVEC7_XYZW => 7 * 8,
        TFT_LAYOUT_MAT4_COL | TFT_LAYOUT_MAT4_ROW => 16 * 8,
        TFT_LAYOUT_AFFINE12_ROW_F32 => 12 * 4,
        TFT_LAYOUT_QVEC7_WXYZ_TWIST6 => 13 * 8,
        _ => return None,
    })
}

/// Whether `layout` includes a twist and so goes through [`write_twist6`]; tested once outside loops.
#[must_use]
pub(crate) fn carries_twist(layout: tft_layout) -> bool {
    layout == TFT_LAYOUT_QVEC7_WXYZ_TWIST6
}

#[inline]
fn rot3(t: &Iso3) -> [f64; 9] {
    let (w, x, y, z) = (t.q.w, t.q.x, t.q.y, t.q.z);
    let (xx, yy, zz) = (x * x, y * y, z * z);
    let (xy, xz, yz) = (x * y, x * z, y * z);
    let (wx, wy, wz) = (w * x, w * y, w * z);
    [
        1.0 - 2.0 * (yy + zz),
        2.0 * (xy - wz),
        2.0 * (xz + wy),
        2.0 * (xy + wz),
        1.0 - 2.0 * (xx + zz),
        2.0 * (yz - wx),
        2.0 * (xz - wy),
        2.0 * (yz + wx),
        1.0 - 2.0 * (xx + yy),
    ]
}

/// Write the pose `t` into `dst` in `layout`.
///
/// `dst` must be at least [`payload_bytes`] long (a short slice panics, mapped to `TFT_ERR_INTERNAL`).
/// A twist-carrying layout must go to [`write_twist6`]; the `debug_assert` guards the split.
pub(crate) fn write(t: &Iso3, layout: tft_layout, dst: &mut [u8]) {
    debug_assert!(
        !carries_twist(layout),
        "a twist-carrying layout must go through `write_twist6`"
    );
    match layout {
        TFT_LAYOUT_QVEC7_WXYZ => put_qvec7_wxyz(t, dst),
        TFT_LAYOUT_QVEC7_XYZW => {
            put_f64(dst, &[t.q.x, t.q.y, t.q.z, t.q.w, t.t.x, t.t.y, t.t.z]);
        }
        TFT_LAYOUT_MAT4_ROW => {
            let r = rot3(t);
            put_f64(
                dst,
                &[
                    r[0], r[1], r[2], t.t.x, //
                    r[3], r[4], r[5], t.t.y, //
                    r[6], r[7], r[8], t.t.z, //
                    0.0, 0.0, 0.0, 1.0,
                ],
            );
        }
        TFT_LAYOUT_MAT4_COL => {
            let r = rot3(t);
            put_f64(
                dst,
                &[
                    r[0], r[3], r[6], 0.0, //
                    r[1], r[4], r[7], 0.0, //
                    r[2], r[5], r[8], 0.0, //
                    t.t.x, t.t.y, t.t.z, 1.0,
                ],
            );
        }
        TFT_LAYOUT_AFFINE12_ROW_F32 => {
            let r = rot3(t);
            let v = [
                r[0], r[1], r[2], t.t.x, //
                r[3], r[4], r[5], t.t.y, //
                r[6], r[7], r[8], t.t.z,
            ];
            for (i, val) in v.iter().enumerate() {
                let b = (*val as f32).to_ne_bytes();
                dst[i * 4..i * 4 + 4].copy_from_slice(&b);
            }
        }
        // Unreachable: `payload_bytes` validated the discriminant.
        _ => {}
    }
}

/// Write `t` and `twist` into `dst` as [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] (at least 104 bytes): the
/// pose as [`TFT_LAYOUT_QVEC7_WXYZ`], then `[ω, v]`.
#[inline]
pub(crate) fn write_twist6(t: &Iso3, twist: &Twist, dst: &mut [u8]) {
    let (pose, tail) = dst.split_at_mut(7 * 8);
    put_qvec7_wxyz(t, pose);
    put_f64(
        tail,
        &[
            twist.omega.x,
            twist.omega.y,
            twist.omega.z,
            twist.v.x,
            twist.v.y,
            twist.v.z,
        ],
    );
}

#[inline]
fn put_qvec7_wxyz(t: &Iso3, dst: &mut [u8]) {
    put_f64(dst, &[t.q.w, t.q.x, t.q.y, t.q.z, t.t.x, t.t.y, t.t.z]);
}

#[inline]
pub(crate) fn put_f64(dst: &mut [u8], vals: &[f64]) {
    for (i, v) in vals.iter().enumerate() {
        dst[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
    }
}

/// Why a caller's transform was refused; `Copy`, each maps to its own status code.
// The `Not*` prefix reads as the failing property.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadError {
    /// Some component was NaN or infinite.
    NotFinite,
    /// A quaternion too far from unit norm, including the all-zeros buffer.
    NotAUnitQuaternion,
    /// `|det R − 1| > DET_TOL`; see [`read`].
    NotARotationMatrix,
}

/// How far `det R` may stray from `+1`: loose for honest drift, tight enough to refuse a 0.001 % scale.
const DET_TOL: f64 = 1e-6;

/// How far `|q|` may stray from 1, applied to `‖q‖²` as [`NORM2_TOL`] (saves a `sqrt`).
const NORM_TOL: f64 = 1e-6;

/// `|‖q‖² − 1|` above which a quaternion is refused. See [`NORM_TOL`].
const NORM2_TOL: f64 = 2.0 * NORM_TOL + NORM_TOL * NORM_TOL;

/// `|‖q‖² − 1|` below which renormalizing is skipped (inside `Quat`'s `1e-12` invariant band).
const RENORM_SKIP_TOL: f64 = 2e-13;

#[inline]
fn normalize_if_needed(q: tf_tree::Quat) -> tf_tree::Quat {
    if (q.norm_squared() - 1.0).abs() <= RENORM_SKIP_TOL {
        q
    } else {
        q.normalize()
    }
}

/// Read a transform **out of caller memory** in `layout`; `src` must be at least [`payload_bytes`] long.
///
/// One check, `|det R − 1| ≤ 1e-6`, refuses a left-handed matrix and a scaled one; honest drift is
/// normalized. `AFFINE12_ROW_F32` is **not readable** (`docs/PROJECT.md` §5): returns `None`, which
/// the caller turns into `TFT_ERR_BAD_ENUM`.
pub(crate) fn read(layout: tft_layout, src: &[u8]) -> Option<Result<Iso3, ReadError>> {
    Some(match layout {
        TFT_LAYOUT_QVEC7_WXYZ => read_quat7(src, [0, 1, 2, 3]),
        TFT_LAYOUT_QVEC7_XYZW => read_quat7(src, [3, 0, 1, 2]),
        TFT_LAYOUT_MAT4_ROW => read_mat4(src, false),
        TFT_LAYOUT_MAT4_COL => read_mat4(src, true),
        TFT_LAYOUT_AFFINE12_ROW_F32 => return None,
        _ => return None,
    })
}

fn read_quat7(src: &[u8], wxyz: [usize; 4]) -> Result<Iso3, ReadError> {
    quat7(get_f64s::<7>(src), wxyz)
}

/// Validate and convert seven `f64`s already in `[qw qx qy qz tx ty tz]`, with [`read`]'s tolerances.
#[cfg(feature = "bridge")]
pub(crate) fn from_wxyz_pose(v: [f64; 7]) -> Result<Iso3, ReadError> {
    quat7(v, [0, 1, 2, 3])
}

/// A one-line description of a rejected transform.
#[cfg(feature = "bridge")]
pub(crate) fn read_error_text(e: ReadError) -> &'static str {
    match e {
        ReadError::NotFinite => "the transform contains NaN or infinity",
        ReadError::NotAUnitQuaternion => "the rotation is not a unit quaternion",
        ReadError::NotARotationMatrix => "the rotation matrix is reflected or scaled",
    }
}

fn quat7(v: [f64; 7], wxyz: [usize; 4]) -> Result<Iso3, ReadError> {
    if !v.iter().all(|x| x.is_finite()) {
        return Err(ReadError::NotFinite);
    }
    let q = tf_tree::Quat::new(v[wxyz[0]], v[wxyz[1]], v[wxyz[2]], v[wxyz[3]]);
    if (q.norm_squared() - 1.0).abs() > NORM2_TOL {
        return Err(ReadError::NotAUnitQuaternion);
    }
    Ok(Iso3::new(
        normalize_if_needed(q),
        tf_tree::Vec3::new(v[4], v[5], v[6]),
    ))
}

fn read_mat4(src: &[u8], column_major: bool) -> Result<Iso3, ReadError> {
    let m = get_f64s::<16>(src);
    if !m.iter().all(|x| x.is_finite()) {
        return Err(ReadError::NotFinite);
    }
    // `at(row, col)` undoes the storage order.
    let at = |r: usize, c: usize| {
        if column_major {
            m[c * 4 + r]
        } else {
            m[r * 4 + c]
        }
    };

    let r = [
        at(0, 0),
        at(0, 1),
        at(0, 2),
        at(1, 0),
        at(1, 1),
        at(1, 2),
        at(2, 0),
        at(2, 1),
        at(2, 2),
    ];
    let det = r[0] * (r[4] * r[8] - r[5] * r[7]) - r[1] * (r[3] * r[8] - r[5] * r[6])
        + r[2] * (r[3] * r[7] - r[4] * r[6]);
    if (det - 1.0).abs() > DET_TOL {
        return Err(ReadError::NotARotationMatrix);
    }
    let t = tf_tree::Vec3::new(at(0, 3), at(1, 3), at(2, 3));
    Ok(Iso3::new(
        normalize_if_needed(tf_tree::quat_from_rot3(&r)),
        t,
    ))
}

/// The first `N` `f64` of `src`, native-endian; a short `src` yields zeros (callers size it first).
#[inline]
fn get_f64s<const N: usize>(src: &[u8]) -> [f64; N] {
    let mut out = [0.0f64; N];
    let Some(head) = src.get(..N * 8) else {
        return out;
    };
    for (slot, chunk) in out.iter_mut().zip(head.chunks_exact(8)) {
        *slot = f64::from_ne_bytes(chunk.try_into().unwrap_or([0; 8]));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tf_tree::{Quat, Vec3};

    /// A 90° rotation about +z; every matrix entry is exactly 0 or ±1.
    fn rz90() -> Iso3 {
        let h = core::f64::consts::FRAC_PI_4; // half of 90°
        Iso3::new(
            Quat::new(h.cos(), 0.0, 0.0, h.sin()),
            Vec3::new(1.0, 2.0, 3.0),
        )
    }

    fn read_f64(dst: &[u8], i: usize) -> f64 {
        f64::from_ne_bytes(dst[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]))
    }

    fn read_f32(dst: &[u8], i: usize) -> f32 {
        f32::from_ne_bytes(dst[i * 4..i * 4 + 4].try_into().unwrap_or([0; 4]))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-15
    }

    #[test]
    fn qvec7_wxyz_is_w_first() {
        let t = rz90();
        let mut d = [0u8; 56];
        write(&t, TFT_LAYOUT_QVEC7_WXYZ, &mut d);
        let c = core::f64::consts::FRAC_1_SQRT_2;
        assert!(close(read_f64(&d, 0), c), "slot 0 must be qw");
        assert!(close(read_f64(&d, 1), 0.0));
        assert!(close(read_f64(&d, 2), 0.0));
        assert!(close(read_f64(&d, 3), c), "slot 3 must be qz");
        assert!(close(read_f64(&d, 4), 1.0));
        assert!(close(read_f64(&d, 5), 2.0));
        assert!(close(read_f64(&d, 6), 3.0));
    }

    #[test]
    fn qvec7_xyzw_is_w_last_and_differs_from_wxyz() {
        let t = rz90();
        let (mut a, mut b) = ([0u8; 56], [0u8; 56]);
        write(&t, TFT_LAYOUT_QVEC7_WXYZ, &mut a);
        write(&t, TFT_LAYOUT_QVEC7_XYZW, &mut b);
        let c = core::f64::consts::FRAC_1_SQRT_2;
        assert!(close(read_f64(&b, 0), 0.0), "slot 0 must be qx");
        assert!(close(read_f64(&b, 1), 0.0));
        assert!(close(read_f64(&b, 2), c), "slot 2 must be qz");
        assert!(close(read_f64(&b, 3), c), "slot 3 must be qw");
        assert_eq!(&a[32..], &b[32..]);
        assert_ne!(&a[..32], &b[..32], "XYZW is not distinct from WXYZ");
    }

    /// Row-major puts the translation in the last column, against a hand-computed pattern.
    #[test]
    fn mat4_row_matches_a_hand_computed_pattern() {
        let mut d = [0u8; 128];
        write(&rz90(), TFT_LAYOUT_MAT4_ROW, &mut d);
        let want = [
            0.0, -1.0, 0.0, 1.0, //
            1.0, 0.0, 0.0, 2.0, //
            0.0, 0.0, 1.0, 3.0, //
            0.0, 0.0, 0.0, 1.0,
        ];
        for (i, w) in want.iter().enumerate() {
            assert!(
                close(read_f64(&d, i), *w),
                "element {i}: got {} want {w}",
                read_f64(&d, i)
            );
        }
    }

    #[test]
    fn mat4_col_is_the_transpose_and_moves_the_translation() {
        let (mut r, mut c) = ([0u8; 128], [0u8; 128]);
        write(&rz90(), TFT_LAYOUT_MAT4_ROW, &mut r);
        write(&rz90(), TFT_LAYOUT_MAT4_COL, &mut c);
        let want = [
            0.0, 1.0, 0.0, 0.0, //
            -1.0, 0.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            1.0, 2.0, 3.0, 1.0,
        ];
        for (i, w) in want.iter().enumerate() {
            assert!(
                close(read_f64(&c, i), *w),
                "element {i}: got {} want {w}",
                read_f64(&c, i)
            );
        }
        for row in 0..4 {
            for col in 0..4 {
                assert!(close(
                    read_f64(&r, row * 4 + col),
                    read_f64(&c, col * 4 + row)
                ));
            }
        }
        assert_ne!(r, c, "the two matrix layouts must differ");
    }

    #[test]
    fn affine12_is_f32_row_major_without_the_bottom_row() {
        let mut d = [0u8; 48];
        write(&rz90(), TFT_LAYOUT_AFFINE12_ROW_F32, &mut d);
        let want: [f32; 12] = [
            0.0, -1.0, 0.0, 1.0, //
            1.0, 0.0, 0.0, 2.0, //
            0.0, 0.0, 1.0, 3.0,
        ];
        for (i, w) in want.iter().enumerate() {
            assert!(
                (read_f32(&d, i) - w).abs() < 1e-6,
                "element {i}: got {} want {w}",
                read_f32(&d, i)
            );
        }
    }

    #[test]
    fn payload_sizes_are_defined_exactly_for_known_layouts() {
        assert_eq!(payload_bytes(TFT_LAYOUT_QVEC7_WXYZ), Some(56));
        assert_eq!(payload_bytes(TFT_LAYOUT_QVEC7_XYZW), Some(56));
        assert_eq!(payload_bytes(TFT_LAYOUT_MAT4_COL), Some(128));
        assert_eq!(payload_bytes(TFT_LAYOUT_MAT4_ROW), Some(128));
        assert_eq!(payload_bytes(TFT_LAYOUT_AFFINE12_ROW_F32), Some(48));
        assert_eq!(payload_bytes(TFT_LAYOUT_QVEC7_WXYZ_TWIST6), Some(104));
        assert_eq!(payload_bytes(6), None);
        assert_eq!(payload_bytes(u32::MAX), None);
    }

    /// The twist layout is `QVEC7_WXYZ` (byte for byte) plus `[ω, v]`.
    #[test]
    fn the_twist_layout_extends_qvec7_wxyz_and_appends_omega_then_v() {
        let t = rz90();
        let v = tf_tree::Twist::new(Vec3::new(0.11, -0.22, 0.33), Vec3::new(0.44, -0.55, 0.66));

        let mut pose_only = [0u8; 56];
        write(&t, TFT_LAYOUT_QVEC7_WXYZ, &mut pose_only);
        let mut d = [0u8; 104];
        write_twist6(&t, &v, &mut d);

        assert_eq!(
            &d[..56],
            &pose_only[..],
            "the pose half is not byte-identical to QVEC7_WXYZ"
        );
        for (i, want) in [v.omega.x, v.omega.y, v.omega.z, v.v.x, v.v.y, v.v.z]
            .iter()
            .enumerate()
        {
            assert!(
                close(read_f64(&d, 7 + i), *want),
                "twist slot {i}: got {} want {want}",
                read_f64(&d, 7 + i)
            );
        }
    }

    #[test]
    fn a_pose_write_stays_inside_its_own_payload() {
        const SENTINEL: u8 = 0xAA;
        let mut d = [SENTINEL; 104];
        write(&rz90(), TFT_LAYOUT_QVEC7_WXYZ, &mut d);
        assert_ne!(
            &d[..56],
            &[SENTINEL; 56][..],
            "the pose half was not written"
        );
        assert!(
            d[56..].iter().all(|b| *b == SENTINEL),
            "a 56-byte layout wrote past its payload"
        );
    }

    /// `write` refuses a twist-carrying layout (debug builds, where the `debug_assert` exists).
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "must go through `write_twist6`")]
    fn the_pose_writer_refuses_a_twist_layout() {
        let mut d = [0u8; 104];
        write(&rz90(), TFT_LAYOUT_QVEC7_WXYZ_TWIST6, &mut d);
    }

    #[test]
    fn qvec7_wxyz_round_trips_bit_for_bit() {
        let t = tf_tree::exp_se3([0.3, -0.7, 0.2, 1.1, -0.5, 3.7]);
        let mut d = [0u8; 56];
        write(&t, TFT_LAYOUT_QVEC7_WXYZ, &mut d);
        assert_eq!(read_f64(&d, 0).to_bits(), t.q.w.to_bits());
        assert_eq!(read_f64(&d, 3).to_bits(), t.q.z.to_bits());
        assert_eq!(read_f64(&d, 6).to_bits(), t.t.z.to_bits());
    }

    /// A general rotation: `Rz(90°)` zeroes seven of `rot3`'s nine products. The oracle is Rodrigues.
    #[test]
    fn rot3_matches_an_independent_construction_for_a_general_rotation() {
        let w = Vec3::new(0.62, -0.51, 0.74);
        let t = Iso3::new(tf_tree::exp_so3(w), Vec3::new(1.0, 2.0, 3.0));

        let th = (w.x * w.x + w.y * w.y + w.z * w.z).sqrt();
        let (a, b) = (th.sin() / th, (1.0 - th.cos()) / (th * th));
        let k = [[0.0, -w.z, w.y], [w.z, 0.0, -w.x], [-w.y, w.x, 0.0]];
        let mut kk = [[0.0f64; 3]; 3];
        for (r, row) in kk.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                *cell = (0..3).map(|m| k[r][m] * k[m][c]).sum();
            }
        }
        let mut want = [[0.0f64; 3]; 3];
        for r in 0..3 {
            for c in 0..3 {
                want[r][c] = f64::from(u8::from(r == c)) + a * k[r][c] + b * kk[r][c];
            }
        }

        // Every product in `rot3` must be live.
        assert!(
            t.q.x.abs() > 0.15 && t.q.y.abs() > 0.15 && t.q.z.abs() > 0.15 && t.q.w.abs() > 0.15,
            "the fixture is not a general rotation: {:?}",
            t.q
        );

        let mut d = [0u8; 128];
        write(&t, TFT_LAYOUT_MAT4_ROW, &mut d);
        for (r, row) in want.iter().enumerate() {
            for (c, expect) in row.iter().enumerate() {
                let got = read_f64(&d, r * 4 + c);
                assert!(
                    (got - expect).abs() < 1e-12,
                    "R[{r}][{c}]: got {got} want {expect}"
                );
            }
        }
        let mut c4 = [0u8; 128];
        write(&t, TFT_LAYOUT_MAT4_COL, &mut c4);
        for r in 0..4 {
            for c in 0..4 {
                assert!(close(read_f64(&d, r * 4 + c), read_f64(&c4, c * 4 + r)));
            }
        }
    }

    fn iso_err(a: &Iso3, b: &Iso3) -> f64 {
        let mut dq = a.q * b.q.conjugate();
        if dq.w < 0.0 {
            dq = tf_tree::Quat::new(-dq.w, -dq.x, -dq.y, -dq.z);
        }
        let nv = (dq.x * dq.x + dq.y * dq.y + dq.z * dq.z).sqrt();
        (2.0 * nv.atan2(dq.w.abs())) + a.t.sub(b.t).norm()
    }

    #[test]
    fn every_readable_layout_round_trips() {
        let t = Iso3::new(
            tf_tree::exp_so3(Vec3::new(0.62, -0.51, 0.74)),
            Vec3::new(1.5, -2.25, 3.125),
        );
        for layout in [
            TFT_LAYOUT_QVEC7_WXYZ,
            TFT_LAYOUT_QVEC7_XYZW,
            TFT_LAYOUT_MAT4_ROW,
            TFT_LAYOUT_MAT4_COL,
        ] {
            let n = payload_bytes(layout).unwrap();
            let mut buf = vec![0u8; n];
            write(&t, layout, &mut buf);
            let got = read(layout, &buf)
                .expect("layout is readable")
                .expect("a transform we just wrote must be accepted");
            assert!(
                iso_err(&t, &got) < 1e-14,
                "layout {layout}: err {}",
                iso_err(&t, &got)
            );
        }
    }

    #[test]
    fn reading_wxyz_bytes_as_xyzw_is_a_different_rotation() {
        let t = Iso3::new(
            tf_tree::exp_so3(Vec3::new(0.62, -0.51, 0.74)),
            Vec3::new(1.0, 2.0, 3.0),
        );
        let mut buf = [0u8; 56];
        write(&t, TFT_LAYOUT_QVEC7_WXYZ, &mut buf);
        let wrong = read(TFT_LAYOUT_QVEC7_XYZW, &buf).unwrap().unwrap();
        assert!((wrong.q.norm() - 1.0).abs() < 1e-12);
        assert!(
            iso_err(&t, &wrong) > 0.1,
            "the fixture does not actually distinguish the two orders"
        );
    }

    #[test]
    fn a_reflection_is_refused() {
        let t = Iso3::new(
            tf_tree::exp_so3(Vec3::new(0.62, -0.51, 0.74)),
            Vec3::new(1.0, 2.0, 3.0),
        );
        let mut buf = [0u8; 128];
        write(&t, TFT_LAYOUT_MAT4_ROW, &mut buf);
        for i in [0usize, 4, 8] {
            let v = -read_f64(&buf, i);
            buf[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
        }
        assert_eq!(
            read(TFT_LAYOUT_MAT4_ROW, &buf).unwrap(),
            Err(ReadError::NotARotationMatrix)
        );
    }

    #[test]
    fn a_scaled_matrix_is_refused() {
        let t = Iso3::new(
            tf_tree::exp_so3(Vec3::new(0.62, -0.51, 0.74)),
            Vec3::new(1.0, 2.0, 3.0),
        );
        let mut buf = [0u8; 128];
        write(&t, TFT_LAYOUT_MAT4_ROW, &mut buf);
        for r in 0..3 {
            for c in 0..3 {
                let i = r * 4 + c;
                let v = read_f64(&buf, i) * 1.01;
                buf[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
            }
        }
        assert_eq!(
            read(TFT_LAYOUT_MAT4_ROW, &buf).unwrap(),
            Err(ReadError::NotARotationMatrix)
        );
    }

    /// Honest drift is accepted; `DET_TOL` is calibrated for it.
    #[test]
    fn accumulated_float_drift_is_still_accepted() {
        let step = tf_tree::exp_so3(Vec3::new(0.013, -0.021, 0.007));
        let mut q = tf_tree::Quat::IDENTITY;
        for _ in 0..2000 {
            q = q * step; // never renormalized
        }
        let t = Iso3::new(q, Vec3::new(1.0, 2.0, 3.0));
        let mut buf = [0u8; 128];
        write(&t, TFT_LAYOUT_MAT4_ROW, &mut buf);
        assert!(
            read(TFT_LAYOUT_MAT4_ROW, &buf).unwrap().is_ok(),
            "2000 unnormalized compositions must not trip the determinant check"
        );
    }

    #[test]
    fn an_uninitialized_quaternion_buffer_is_refused() {
        assert_eq!(
            read(TFT_LAYOUT_QVEC7_WXYZ, &[0u8; 56]).unwrap(),
            Err(ReadError::NotAUnitQuaternion)
        );
        assert_eq!(
            read(TFT_LAYOUT_MAT4_ROW, &[0u8; 128]).unwrap(),
            Err(ReadError::NotARotationMatrix)
        );
    }

    #[test]
    fn non_finite_input_is_refused() {
        let mut buf = [0u8; 56];
        write(&rz90(), TFT_LAYOUT_QVEC7_WXYZ, &mut buf);
        buf[4 * 8..5 * 8].copy_from_slice(&f64::NAN.to_ne_bytes());
        assert_eq!(
            read(TFT_LAYOUT_QVEC7_WXYZ, &buf).unwrap(),
            Err(ReadError::NotFinite)
        );
        let mut m = [0u8; 128];
        write(&rz90(), TFT_LAYOUT_MAT4_ROW, &mut m);
        m[3 * 8..4 * 8].copy_from_slice(&f64::INFINITY.to_ne_bytes());
        assert_eq!(
            read(TFT_LAYOUT_MAT4_ROW, &m).unwrap(),
            Err(ReadError::NotFinite)
        );
    }

    #[test]
    fn the_f32_layout_cannot_be_published_from() {
        assert!(read(TFT_LAYOUT_AFFINE12_ROW_F32, &[0u8; 48]).is_none());
        assert!(read(9999, &[0u8; 48]).is_none());
    }

    #[test]
    fn the_twist_layout_cannot_be_published_from() {
        assert!(read(TFT_LAYOUT_QVEC7_WXYZ_TWIST6, &[0u8; 104]).is_none());
    }
}
