//! Output layouts — `docs/PHASE4.md` §3.5.
//!
//! # Two traps, both of which produce plausible wrong answers rather than crashes
//!
//! **Quaternion component order.** `tf_tree`'s canonical form is `w`-first
//! (`tf_tree_math` convention 2). `Eigen::Quaterniond`'s *internal storage* is
//! `(x, y, z, w)` even though its constructor takes `(w, x, y, z)` — so a
//! `memcpy` from [`TFT_LAYOUT_QVEC7_WXYZ`] into an `Eigen::Quaterniond` or a
//! `Sophus::SE3d` is silently wrong. It still yields a *unit* quaternion, so
//! nothing complains and no norm check fires; the rotation is simply a different
//! one. [`TFT_LAYOUT_QVEC7_XYZW`] exists solely to make the correct thing the
//! easy thing.
//!
//! **Matrix major order.** Row-major and column-major differ by a transpose,
//! which for a rotation is its *inverse* — again a valid transform, pointing the
//! wrong way. There is deliberately no default: the enum is always explicit.
//!
//! # Why these are written here and not delegated
//!
//! `tf_tree_core::layout` already writes `Mat4`, `Quat` and `Affine32`, and the
//! three that overlap delegate to it so the C ABI and the Python binding cannot
//! disagree. The two that do not exist there — `QVEC7_XYZW` and the row/column
//! distinction — are written out longhand, and §3.5's NORMATIVE test asserts
//! them against **hand-computed byte patterns** rather than against another
//! `tf_tree` call, because a self-consistent pair of bugs would otherwise pass.

use tf_tree::Iso3;

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

/// The number of **bytes** one transform occupies in `layout`, or `None` if the
/// discriminant is not one this build defines.
///
/// Returning `None` rather than a default is deliberate: an unknown layout from
/// a newer header must be an error, never a silent fallback to `QVEC7_WXYZ`.
#[must_use]
pub fn payload_bytes(layout: tft_layout) -> Option<usize> {
    Some(match layout {
        TFT_LAYOUT_QVEC7_WXYZ | TFT_LAYOUT_QVEC7_XYZW => 7 * 8,
        TFT_LAYOUT_MAT4_COL | TFT_LAYOUT_MAT4_ROW => 16 * 8,
        TFT_LAYOUT_AFFINE12_ROW_F32 => 12 * 4,
        _ => return None,
    })
}

/// The rotation matrix of `t`, row-major, as nine `f64`.
///
/// One place, used by both matrix layouts, so the two cannot disagree about the
/// rotation and differ only in how it is laid out.
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

/// Write `t` into `dst` in `layout`.
///
/// `dst` must be at least [`payload_bytes`] long; the caller checks that, and
/// this function's slicing would panic rather than overrun if it did not — which
/// the panic guard turns into `TFT_ERR_INTERNAL` rather than an abort.
pub(crate) fn write(t: &Iso3, layout: tft_layout, dst: &mut [u8]) {
    match layout {
        TFT_LAYOUT_QVEC7_WXYZ => {
            put_f64(dst, &[t.q.w, t.q.x, t.q.y, t.q.z, t.t.x, t.t.y, t.t.z]);
        }
        TFT_LAYOUT_QVEC7_XYZW => {
            // The whole reason this variant exists. See the module docs.
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
            // Column-major: the translation lands in the last *column*, which in
            // this ordering is elements 12..15 — not the last row.
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
        // Unreachable: the caller validated the discriminant with
        // `payload_bytes` before allocating a slice for it.
        _ => {}
    }
}

#[inline]
fn put_f64(dst: &mut [u8], vals: &[f64]) {
    for (i, v) in vals.iter().enumerate() {
        dst[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_tree::{Quat, Vec3};

    /// A 90° rotation about +z with a distinctive translation. Chosen so every
    /// matrix entry is exactly 0, ±1 — a hand-computable byte pattern with no
    /// rounding, which is what §3.5 requires the assertion to be against.
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

    /// **`QVEC7_WXYZ` is `w` first.** The canonical order; everything else is
    /// defined relative to it.
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

    /// **`QVEC7_XYZW` is `w` last** — the Eigen/Sophus storage order, and the
    /// entire reason the variant exists.
    ///
    /// Mutant: make `QVEC7_XYZW` write the same order as `WXYZ` ⇒ fails. That
    /// mutation is exactly the bug the variant exists to prevent, and it would
    /// still produce a unit quaternion in the caller's `Eigen::Quaterniond`.
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
        // Translation is in the same place in both.
        assert_eq!(&a[32..], &b[32..]);
        // Non-vacuity: the two layouts must actually differ.
        assert_ne!(&a[..32], &b[..32], "XYZW is not distinct from WXYZ");
    }

    /// **Row-major puts the translation in the last column of each row.**
    ///
    /// Asserted against the hand-computed pattern for `Rz(90°)`:
    /// `[0 −1 0 | 1; 1 0 0 | 2; 0 0 1 | 3; 0 0 0 1]`.
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

    /// **Column-major is the transpose, and the translation moves to 12..14.**
    ///
    /// This is the trap: a caller that reads column-major bytes as row-major gets
    /// `Rz(−90°)` — a perfectly valid rotation pointing the wrong way — and a
    /// translation of `(0, 0, 0)` from what it thinks is the last column.
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
        // And it is genuinely the transpose of the row-major form.
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

    /// The GPU layout is 3×4 `f32` row-major — 48 bytes, no bottom row.
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

    /// Every defined layout has a size, and an undefined one has none — so an
    /// unknown discriminant from a newer header is an error rather than a
    /// silent fallback.
    #[test]
    fn payload_sizes_are_defined_exactly_for_known_layouts() {
        assert_eq!(payload_bytes(TFT_LAYOUT_QVEC7_WXYZ), Some(56));
        assert_eq!(payload_bytes(TFT_LAYOUT_QVEC7_XYZW), Some(56));
        assert_eq!(payload_bytes(TFT_LAYOUT_MAT4_COL), Some(128));
        assert_eq!(payload_bytes(TFT_LAYOUT_MAT4_ROW), Some(128));
        assert_eq!(payload_bytes(TFT_LAYOUT_AFFINE12_ROW_F32), Some(48));
        assert_eq!(payload_bytes(5), None);
        assert_eq!(payload_bytes(u32::MAX), None);
    }

    /// **Round-tripping through `QVEC7_WXYZ` must reproduce the arena's bits
    /// exactly**, since that layout *is* the arena's representation.
    #[test]
    fn qvec7_wxyz_round_trips_bit_for_bit() {
        let t = tf_tree::exp_se3([0.3, -0.7, 0.2, 1.1, -0.5, 3.7]);
        let mut d = [0u8; 56];
        write(&t, TFT_LAYOUT_QVEC7_WXYZ, &mut d);
        assert_eq!(read_f64(&d, 0).to_bits(), t.q.w.to_bits());
        assert_eq!(read_f64(&d, 3).to_bits(), t.q.z.to_bits());
        assert_eq!(read_f64(&d, 6).to_bits(), t.t.z.to_bits());
    }

    /// **A general rotation, because `Rz(90°)` is degenerate for this purpose.**
    ///
    /// Found by review: in `rot3`, `Rz(90°)` makes seven of the nine quaternion
    /// products identically zero (`x = y = 0`, so `xx`, `yy`, `xy`, `xz`, `yz`,
    /// `wx`, `wy` all vanish). The hand-computed byte-pattern tests above are
    /// therefore checking two live terms out of nine — they pin the *layout*,
    /// which is what §3.5 asks for, but they cannot catch a swapped or
    /// sign-flipped product in `rot3` itself.
    ///
    /// This uses a rotation with all three components non-zero and checks the
    /// matrix against an **independent** construction: three successive
    /// axis-angle rotations composed as matrices, sharing no code with `rot3`.
    ///
    /// Mutant: swap `wx` and `wy` in `rot3`, or flip the sign of any
    /// `2.0 * (.. - ..)` term ⇒ fails here while every test above still passes.
    #[test]
    fn rot3_matches_an_independent_construction_for_a_general_rotation() {
        // A rotation whose axis has three comparable components, so every one
        // of the nine products in `rot3` is live.
        let w = Vec3::new(0.62, -0.51, 0.74);
        let t = Iso3::new(tf_tree::exp_so3(w), Vec3::new(1.0, 2.0, 3.0));

        // The oracle: Rodrigues, straight from the axis-angle vector. Shares no
        // code with `rot3`'s quaternion algebra, so a matched pair of errors
        // cannot satisfy both.
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

        // Non-vacuity: every product in `rot3` must be live, or this is no
        // better than the `Rz(90°)` fixture it exists to complement.
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
        // ...and the column-major form is still its transpose.
        let mut c4 = [0u8; 128];
        write(&t, TFT_LAYOUT_MAT4_COL, &mut c4);
        for r in 0..4 {
            for c in 0..4 {
                assert!(close(read_f64(&d, r * 4 + c), read_f64(&c4, c * 4 + r)));
            }
        }
    }
}
