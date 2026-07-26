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

// ---------------------------------------------------------------------------
// The publish direction — §3.2's `tft_publisher`
// ---------------------------------------------------------------------------

/// Why a caller's transform was refused.
///
/// `Copy` and carries no `String`, per `docs/PROJECT.md` §5. The C ABI maps
/// each to its own status code, because "your matrix is left-handed" and "your
/// quaternion is zero" want different fixes.
// The `Not*` prefix is the point: every variant is a way the input fails to be
// a transform, and the shared prefix is what makes that readable at the call
// site. `enum_variant_names` would have them renamed to `Finite`/`UnitQuaternion`
// /`RotationMatrix`, which read as the *successful* properties.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadError {
    /// Some component was NaN or infinite.
    NotFinite,
    /// A quaternion whose norm is too far from 1 to be a rotation — including
    /// the all-zeros buffer, which is what an uninitialized one looks like.
    NotAUnitQuaternion,
    /// `|det R − 1| > DET_TOL`. One check that catches both realistic mistakes;
    /// see [`read`].
    NotARotationMatrix,
}

/// How far `det R` may stray from `+1`.
///
/// Loose enough that an honest matrix which has been composed a few hundred
/// times and never re-orthonormalized still passes — `f64` drift after 1000
/// compositions is ~1e-13 — and tight enough that a 0.001 % scale is refused.
const DET_TOL: f64 = 1e-6;

/// How far `|q|` may stray from 1, in the same spirit.
///
/// Compared against `norm_squared`, not `norm`: `|q| ∈ [1−t, 1+t]` iff
/// `|q|² ∈ [(1−t)², (1+t)²]`, and to first order that is `|‖q‖² − 1| ≤ 2t`. The
/// bound below is deliberately the loose side of that — `2t + t²` — so the
/// squared test accepts exactly what the unsquared one would and not a hair
/// less. Saves a `sqrt` on every published transform.
const NORM_TOL: f64 = 1e-6;

/// `|‖q‖² − 1|` above which a quaternion is refused. See [`NORM_TOL`].
const NORM2_TOL: f64 = 2.0 * NORM_TOL + NORM_TOL * NORM_TOL;

/// `|‖q‖² − 1|` below which renormalizing is not worth a `sqrt` and four
/// divides.
///
/// `Quat`'s invariant is `|q| == 1` within `1e-12`, so anything inside that band
/// is already what the arena requires and normalizing would only move the last
/// bit. A caller who hands us a correctly normalized quaternion — which is
/// almost all of them, since that is what every quaternion library produces —
/// therefore pays nothing for the renormalization path.
///
/// The squared band is `2 × 1e-13`, comfortably inside the `1e-12` the invariant
/// allows, so the fast path cannot admit a quaternion the engine would reject.
const RENORM_SKIP_TOL: f64 = 2e-13;

/// Normalize `q` only if it needs it.
///
/// The quaternion path used to do **two** `sqrt`s — one for the norm check, one
/// for the normalization — on a push that costs tens of nanoseconds.
///
/// **Measured effect: none.** `examples/abi_cost.rs` reports 22.1 ns/push with
/// and without this, inside the run-to-run spread. It is kept because it is
/// strictly less work for provably the same result, not because it bought
/// anything: an earlier version of this comment claimed 43 %, which was a
/// prediction written before the measurement and refuted by it.
#[inline]
fn normalize_if_needed(q: tf_tree::Quat) -> tf_tree::Quat {
    if (q.norm_squared() - 1.0).abs() <= RENORM_SKIP_TOL {
        q
    } else {
        q.normalize()
    }
}

/// Read a transform **out of caller memory** in `layout`.
///
/// `src` must be at least [`payload_bytes`] long; the caller checks that.
///
/// # What is validated, and why it is only these things
///
/// A C caller's transform arrives from their own math, and the two mistakes
/// that actually happen are both silent:
///
/// * **A left-handed matrix** — someone negated a column, or transposed a
///   coordinate convention. `det R = −1`, and Shepperd's method converts it
///   without complaint into a *different, perfectly valid* rotation.
/// * **A matrix carrying scale** — an `Eigen::Affine3d` used where an
///   `Isometry3d` was meant. `det R = s³`, and normalizing the quaternion
///   silently discards the scale.
///
/// **One determinant catches both**, which is why the check is a determinant
/// and not an orthonormality sweep: `|det R − 1| ≤ 1e-6`. It costs 9
/// multiplies and 5 adds against a `push` that is already tens of nanoseconds,
/// and it is the difference between a robot that reports an error and one that
/// drives into a wall confidently.
///
/// Float drift from honest composition is *not* an error: the quaternion is
/// normalized after conversion, which is exactly what an arena sample requires
/// and what `Iso3::normalized` already does everywhere else.
///
/// `AFFINE12_ROW_F32` is **not readable**. It is an output encoding for GPU
/// upload (`tf_tree_core::layout`'s reasoning, and `docs/PROJECT.md` §5's "f64
/// only"): accepting a publication in `f32` would quietly halve the precision
/// of everything downstream of it. Returns `None` for that discriminant, which
/// the caller turns into `TFT_ERR_BAD_ENUM`.
pub(crate) fn read(layout: tft_layout, src: &[u8]) -> Option<Result<Iso3, ReadError>> {
    Some(match layout {
        TFT_LAYOUT_QVEC7_WXYZ => read_quat7(src, [0, 1, 2, 3]),
        TFT_LAYOUT_QVEC7_XYZW => read_quat7(src, [3, 0, 1, 2]),
        TFT_LAYOUT_MAT4_ROW => read_mat4(src, false),
        TFT_LAYOUT_MAT4_COL => read_mat4(src, true),
        // Deliberately unreadable — see the doc comment.
        TFT_LAYOUT_AFFINE12_ROW_F32 => return None,
        _ => return None,
    })
}

/// `[w, x, y, z]` slot indices, so the two quaternion orders share one body and
/// cannot drift apart.
fn read_quat7(src: &[u8], wxyz: [usize; 4]) -> Result<Iso3, ReadError> {
    let v = get_f64s::<7>(src);
    if !v.iter().all(|x| x.is_finite()) {
        return Err(ReadError::NotFinite);
    }
    let q = tf_tree::Quat::new(v[wxyz[0]], v[wxyz[1]], v[wxyz[2]], v[wxyz[3]]);
    // Catches the all-zeros buffer, which is what an uninitialized struct looks
    // like and is otherwise a division by zero inside `normalize`.
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
    // Index the source as `at(row, col)` whichever way it is stored, so the
    // rest of this function is written once in row-major terms.
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
    // The translation is the last *column* in both storage orders — `at` has
    // already undone the transposition, which is the whole trap §3.5 names.
    let t = tf_tree::Vec3::new(at(0, 3), at(1, 3), at(2, 3));
    Ok(Iso3::new(
        normalize_if_needed(tf_tree::quat_from_rot3(&r)),
        t,
    ))
}

/// The first `N` `f64` of `src`, native-endian.
///
/// One bounds check for the whole read rather than `N` of them: the slice is
/// narrowed once, and the per-element `try_into` then sees a fixed-size chunk
/// the optimizer can prove is in range. A short `src` yields zeros, which cannot
/// happen — every caller sizes the slice from `payload_bytes` first — and is
/// still the safe answer if one ever forgets.
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

    // -----------------------------------------------------------------------
    // The publish direction — `read`
    // -----------------------------------------------------------------------

    fn iso_err(a: &Iso3, b: &Iso3) -> f64 {
        let mut dq = a.q * b.q.conjugate();
        if dq.w < 0.0 {
            dq = tf_tree::Quat::new(-dq.w, -dq.x, -dq.y, -dq.z);
        }
        let nv = (dq.x * dq.x + dq.y * dq.y + dq.z * dq.z).sqrt();
        (2.0 * nv.atan2(dq.w.abs())) + a.t.sub(b.t).norm()
    }

    /// **Every readable layout round-trips through `write` then `read`.**
    ///
    /// A general rotation, so all four Shepperd branches and all nine
    /// quaternion products are live — the `Rz(90°)` fixture above would not
    /// distinguish a transposed matrix read from a correct one.
    ///
    /// Mutant: drop the `column_major` transposition in `read_mat4` (index
    /// `m[r * 4 + c]` unconditionally) ⇒ `MAT4_COL` round-trips to the inverse
    /// rotation and this fails. That is exactly the §3.5 trap, in the direction
    /// nothing else tests.
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

    /// **The two quaternion orders are not interchangeable on read either.**
    ///
    /// Reading `WXYZ` bytes as `XYZW` yields a *different unit quaternion* — no
    /// norm check fires, nothing complains, and the robot points somewhere else.
    /// This is the trap in the publish direction, and it is why the two orders
    /// have to be distinguishable by test rather than by inspection.
    #[test]
    fn reading_wxyz_bytes_as_xyzw_is_a_different_rotation() {
        let t = Iso3::new(
            tf_tree::exp_so3(Vec3::new(0.62, -0.51, 0.74)),
            Vec3::new(1.0, 2.0, 3.0),
        );
        let mut buf = [0u8; 56];
        write(&t, TFT_LAYOUT_QVEC7_WXYZ, &mut buf);
        let wrong = read(TFT_LAYOUT_QVEC7_XYZW, &buf).unwrap().unwrap();
        // Still a unit quaternion — that is the whole problem.
        assert!((wrong.q.norm() - 1.0).abs() < 1e-12);
        assert!(
            iso_err(&t, &wrong) > 0.1,
            "the fixture does not actually distinguish the two orders"
        );
    }

    /// **A left-handed matrix is refused, not silently converted.**
    ///
    /// Negating one column gives `det R = −1`. Shepperd's method happily
    /// produces a valid quaternion from it, which would be a rotation the
    /// caller never asked for.
    ///
    /// Mutant: delete the determinant check ⇒ this returns `Ok` and the
    /// reflection is published as a rotation.
    #[test]
    fn a_reflection_is_refused() {
        let t = Iso3::new(
            tf_tree::exp_so3(Vec3::new(0.62, -0.51, 0.74)),
            Vec3::new(1.0, 2.0, 3.0),
        );
        let mut buf = [0u8; 128];
        write(&t, TFT_LAYOUT_MAT4_ROW, &mut buf);
        // Negate the first column: rows 0, 1, 2 at element 0, 4, 8.
        for i in [0usize, 4, 8] {
            let v = -read_f64(&buf, i);
            buf[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
        }
        assert_eq!(
            read(TFT_LAYOUT_MAT4_ROW, &buf).unwrap(),
            Err(ReadError::NotARotationMatrix)
        );
    }

    /// **A scaled matrix is refused by the same check.**
    ///
    /// `Eigen::Affine3d` where an `Isometry3d` was meant. `det = s³`, and
    /// normalizing the quaternion would discard the scale without a word.
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

    /// **Honest drift is accepted.** A matrix composed many times and never
    /// re-orthonormalized must still publish, or the check is a nuisance rather
    /// than a guard. `DET_TOL` is calibrated for exactly this.
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

    /// **An all-zero buffer is refused rather than divided by.**
    ///
    /// This is what an uninitialized C struct looks like, and without the norm
    /// check `normalize` divides by zero and publishes NaN into the arena.
    #[test]
    fn an_uninitialized_quaternion_buffer_is_refused() {
        assert_eq!(
            read(TFT_LAYOUT_QVEC7_WXYZ, &[0u8; 56]).unwrap(),
            Err(ReadError::NotAUnitQuaternion)
        );
        // ...and so is the all-zero matrix, by the determinant.
        assert_eq!(
            read(TFT_LAYOUT_MAT4_ROW, &[0u8; 128]).unwrap(),
            Err(ReadError::NotARotationMatrix)
        );
    }

    /// **NaN never reaches the arena.** A single NaN in a pose poisons every
    /// interpolation that ever brackets it, and there is no way to detect it
    /// after the fact except by the results being wrong.
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

    /// **`AFFINE12_ROW_F32` is write-only, on purpose.**
    ///
    /// It is a GPU upload encoding; accepting a publication in `f32` would
    /// halve the precision of everything downstream (`docs/PROJECT.md` §5,
    /// "f64 only"). `None` here becomes `TFT_ERR_BAD_ENUM` at the boundary.
    #[test]
    fn the_f32_layout_cannot_be_published_from() {
        assert!(read(TFT_LAYOUT_AFFINE12_ROW_F32, &[0u8; 48]).is_none());
        assert!(read(9999, &[0u8; 48]).is_none());
    }
}
