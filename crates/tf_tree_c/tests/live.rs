//! The C ABI against a real tree — `docs/PHASE4.md` §6.1, the half that needs a
//! live handle.
//!
//! `abi.rs` covers misuse with no handle at all. This covers the working path and
//! the misuse that only becomes reachable *once you have* a handle: freeing in
//! the wrong order, using a freed handle, striding a batch write.
#![cfg(feature = "test-hooks")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::ptr;

use tf_tree_c::*;

/// A live fixture tree. `map -> odom -> base -> sensor`.
struct Tree(*mut tft_tree);

impl Tree {
    fn new() -> Tree {
        let mut p: *mut tft_tree = ptr::null_mut();
        // SAFETY: `p` is a live local.
        let rc = unsafe { tft_test_tree_create(&mut p) };
        assert_eq!(rc, TFT_OK);
        assert!(!p.is_null());
        Tree(p)
    }

    fn plan(&self, target: &str, source: &str) -> Plan {
        let t = std::ffi::CString::new(target).unwrap();
        let s = std::ffi::CString::new(source).unwrap();
        let mut p: *mut tft_plan = ptr::null_mut();
        // SAFETY: the handle is live, both strings are NUL-terminated, `p` local.
        let rc = unsafe { tft_plan_create(self.0, t.as_ptr(), s.as_ptr(), &mut p) };
        assert_eq!(rc, TFT_OK, "plan {target} <- {source}");
        Plan(p)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // SAFETY: created above, freed exactly once.
        unsafe { tft_tree_free(self.0) };
    }
}

struct Plan(*mut tft_plan);

impl Drop for Plan {
    fn drop(&mut self) {
        // SAFETY: created above, freed exactly once.
        unsafe { tft_plan_free(self.0) };
    }
}

fn read_f64(b: &[u8], i: usize) -> f64 {
    f64::from_ne_bytes(b[i * 8..i * 8 + 8].try_into().unwrap())
}

/// The working path: open, plan, evaluate, and get a transform that is actually
/// a transform.
#[test]
fn a_lookup_through_the_c_abi_returns_a_valid_rigid_transform() {
    let tree = Tree::new();
    let plan = tree.plan("map", "sensor");
    let mut out = [0u8; 128];
    // SAFETY: live plan, `out` is 128 bytes and MAT4_ROW needs exactly that.
    let rc = unsafe {
        tft_plan_at(
            plan.0,
            250_000_000,
            TFT_LAYOUT_MAT4_ROW,
            out.as_mut_ptr().cast(),
        )
    };
    assert_eq!(rc, TFT_OK);

    // Bottom row is [0 0 0 1] — it is a rigid transform, not arbitrary bytes.
    assert_eq!(read_f64(&out, 12), 0.0);
    assert_eq!(read_f64(&out, 13), 0.0);
    assert_eq!(read_f64(&out, 14), 0.0);
    assert_eq!(read_f64(&out, 15), 1.0);
    // The rotation block is orthonormal: each row is a unit vector.
    for r in 0..3 {
        let n = (0..3)
            .map(|c| read_f64(&out, r * 4 + c).powi(2))
            .sum::<f64>();
        assert!((n - 1.0).abs() < 1e-12, "row {r} is not unit: {n}");
    }
}

/// **Every layout must agree about the transform**, differing only in how it is
/// written. A disagreement here means one of the five is transposing or
/// reordering something.
#[test]
fn all_five_layouts_describe_the_same_transform() {
    let tree = Tree::new();
    let plan = tree.plan("map", "base");
    let stamp = 250_000_000;

    let mut q_wxyz = [0u8; 56];
    let mut q_xyzw = [0u8; 56];
    let mut m_row = [0u8; 128];
    let mut m_col = [0u8; 128];
    // SAFETY: live plan; each buffer is exactly `tft_layout_size` for its layout.
    unsafe {
        assert_eq!(
            tft_plan_at(
                plan.0,
                stamp,
                TFT_LAYOUT_QVEC7_WXYZ,
                q_wxyz.as_mut_ptr().cast()
            ),
            TFT_OK
        );
        assert_eq!(
            tft_plan_at(
                plan.0,
                stamp,
                TFT_LAYOUT_QVEC7_XYZW,
                q_xyzw.as_mut_ptr().cast()
            ),
            TFT_OK
        );
        assert_eq!(
            tft_plan_at(
                plan.0,
                stamp,
                TFT_LAYOUT_MAT4_ROW,
                m_row.as_mut_ptr().cast()
            ),
            TFT_OK
        );
        assert_eq!(
            tft_plan_at(
                plan.0,
                stamp,
                TFT_LAYOUT_MAT4_COL,
                m_col.as_mut_ptr().cast()
            ),
            TFT_OK
        );
    }

    // The two quaternion layouts carry the same four numbers, permuted.
    assert_eq!(read_f64(&q_wxyz, 0), read_f64(&q_xyzw, 3), "w");
    assert_eq!(read_f64(&q_wxyz, 1), read_f64(&q_xyzw, 0), "x");
    assert_eq!(read_f64(&q_wxyz, 2), read_f64(&q_xyzw, 1), "y");
    assert_eq!(read_f64(&q_wxyz, 3), read_f64(&q_xyzw, 2), "z");
    // ...and the same translation, in the same place.
    assert_eq!(&q_wxyz[32..], &q_xyzw[32..]);

    // The two matrix layouts are transposes of each other.
    for r in 0..4 {
        for c in 0..4 {
            assert_eq!(read_f64(&m_row, r * 4 + c), read_f64(&m_col, c * 4 + r));
        }
    }
    // The translation agrees between the quaternion and matrix forms.
    for k in 0..3 {
        assert!((read_f64(&q_wxyz, 4 + k) - read_f64(&m_row, k * 4 + 3)).abs() < 1e-15);
    }
}

/// **`out_stride_bytes` writes into an array of caller structs** — §4.3's reason
/// for the parameter, exercised here without needing Sophus.
///
/// The stride is deliberately larger than the payload, and the gap bytes are
/// pre-filled with a sentinel that must survive: if the ABI wrote tightly packed
/// and ignored the stride, it would overwrite them.
///
/// Mutant: ignore `out_stride_bytes` and use `payload` ⇒ the sentinel check fails.
#[test]
fn a_strided_batch_writes_into_caller_structs_without_touching_the_gaps() {
    let tree = Tree::new();
    let plan = tree.plan("map", "base");
    const N: usize = 16;
    const STRIDE: usize = 64; // 56-byte payload + 8 bytes of caller padding

    let stamps: Vec<i64> = (0..N as i64)
        .map(|i| 100_000_000 + i * 10_000_000)
        .collect();
    let mut buf = vec![0xAAu8; N * STRIDE];

    // SAFETY: live plan, `stamps` has N elements, `buf` has N*STRIDE bytes.
    let rc = unsafe {
        tft_plan_at_many(
            plan.0,
            stamps.as_ptr(),
            N,
            TFT_LAYOUT_QVEC7_WXYZ,
            buf.as_mut_ptr().cast(),
            STRIDE,
        )
    };
    assert_eq!(rc, TFT_OK);

    for i in 0..N {
        // The 8 bytes past each payload must still be the sentinel.
        let gap = &buf[i * STRIDE + 56..i * STRIDE + 64];
        assert_eq!(gap, &[0xAA; 8], "element {i}'s gap was overwritten");
        // And each payload is a unit quaternion, i.e. real data was written.
        let s: f64 = (0..4)
            .map(|k| read_f64(&buf[i * STRIDE..], k).powi(2))
            .sum();
        assert!(
            (s - 1.0).abs() < 1e-12,
            "element {i} is not a unit quaternion"
        );
    }

    // Each element must match the single-shot lookup at the same stamp.
    for i in [0usize, 7, N - 1] {
        let mut one = [0u8; 56];
        // SAFETY: as above.
        unsafe {
            assert_eq!(
                tft_plan_at(
                    plan.0,
                    stamps[i],
                    TFT_LAYOUT_QVEC7_WXYZ,
                    one.as_mut_ptr().cast()
                ),
                TFT_OK
            );
        }
        assert_eq!(&buf[i * STRIDE..i * STRIDE + 56], &one[..], "element {i}");
    }
}

/// A stride smaller than the payload is refused rather than overlapping writes.
#[test]
fn a_stride_below_the_payload_is_refused() {
    let tree = Tree::new();
    let plan = tree.plan("map", "base");
    let stamps = [100_000_000i64, 110_000_000];
    let mut buf = [0u8; 256];
    // SAFETY: live plan; the call is expected to fail before writing.
    let rc = unsafe {
        tft_plan_at_many(
            plan.0,
            stamps.as_ptr(),
            2,
            TFT_LAYOUT_QVEC7_WXYZ,
            buf.as_mut_ptr().cast(),
            32, // < 56
        )
    };
    assert_eq!(rc, TFT_ERR_BUFFER_TOO_SMALL);
    assert_eq!(buf, [0u8; 256], "nothing may be written on a rejected call");
}

/// **A zero-length batch really is a no-op** — the assertion `abi.rs` could not
/// make without a live handle.
#[test]
fn a_zero_length_batch_succeeds_and_writes_nothing() {
    let tree = Tree::new();
    let plan = tree.plan("map", "base");
    // SAFETY: live plan; n == 0 means neither pointer is dereferenced.
    let rc = unsafe {
        tft_plan_at_many(
            plan.0,
            ptr::null(),
            0,
            TFT_LAYOUT_MAT4_ROW,
            ptr::null_mut(),
            0,
        )
    };
    assert_eq!(rc, TFT_OK, "an empty batch is a no-op, not an error");
}

/// **A success clears the previous error.** The real version of `abi.rs`'s weak
/// attempt, which could only reach failing calls.
#[test]
fn a_successful_call_clears_the_error_slot() {
    let tree = Tree::new();
    let plan = tree.plan("map", "base");

    // Provoke a failure: a stamp far past the published window.
    let mut out = [0u8; 128];
    // SAFETY: live plan, correctly sized buffer.
    let rc = unsafe {
        tft_plan_at(
            plan.0,
            i64::MAX / 2,
            TFT_LAYOUT_MAT4_ROW,
            out.as_mut_ptr().cast(),
        )
    };
    assert_eq!(rc, TFT_ERR_EXTRAPOLATION);
    let mut e: tft_error = unsafe { core::mem::zeroed() };
    e.struct_size = core::mem::size_of::<tft_error>() as u32;
    // SAFETY: `e` is live and its struct_size is set.
    unsafe { tft_last_error(&mut e) };
    assert_eq!(e.code, TFT_ERR_EXTRAPOLATION);
    assert_ne!(e.newest, 0, "the window must be reported");

    // Now a success on the same thread.
    // SAFETY: as above, with a stamp inside the window.
    let rc = unsafe {
        tft_plan_at(
            plan.0,
            250_000_000,
            TFT_LAYOUT_MAT4_ROW,
            out.as_mut_ptr().cast(),
        )
    };
    assert_eq!(rc, TFT_OK);
    // SAFETY: as above.
    unsafe { tft_last_error(&mut e) };
    assert_eq!(e.code, TFT_OK, "a success must clear the stale detail");
    assert_eq!(e.message[0], 0, "and its message");
}

/// **Freeing the tree before its plans must not dangle** — the natural C order,
/// and the reason `tft_plan` holds a refcounted share rather than a pointer.
///
/// Mutant: make `tft_plan` hold `*const tft_tree` ⇒ use-after-free here, which
/// ASan catches and which a plain run may or may not.
#[test]
fn a_plan_outlives_the_tree_handle_it_was_compiled_from() {
    let mut t: *mut tft_tree = ptr::null_mut();
    // SAFETY: `t` is a live local.
    assert_eq!(unsafe { tft_test_tree_create(&mut t) }, TFT_OK);
    let name_a = std::ffi::CString::new("map").unwrap();
    let name_b = std::ffi::CString::new("base").unwrap();
    let mut p: *mut tft_plan = ptr::null_mut();
    // SAFETY: live handle, NUL-terminated names, live out-pointer.
    assert_eq!(
        unsafe { tft_plan_create(t, name_a.as_ptr(), name_b.as_ptr(), &mut p) },
        TFT_OK
    );

    // Free the tree first — the order a C programmer naturally writes.
    // SAFETY: created above, freed once.
    unsafe { tft_tree_free(t) };

    // The plan must still evaluate.
    let mut out = [0u8; 128];
    // SAFETY: the plan is still live; it holds its own share of the tree.
    let rc = unsafe { tft_plan_at(p, 250_000_000, TFT_LAYOUT_MAT4_ROW, out.as_mut_ptr().cast()) };
    assert_eq!(rc, TFT_OK, "the plan must survive its tree handle");
    assert_eq!(read_f64(&out, 15), 1.0);

    // SAFETY: freed exactly once.
    unsafe { tft_plan_free(p) };
}

/// **A freed handle is rejected, not followed.** The magic word is zeroed on
/// free, so the allocation-intact case is caught.
///
/// Mutant: remove the magic-zeroing write from `tft_plan_free` ⇒ the second call
/// dereferences a freed `Box`.
#[test]
fn a_freed_handle_is_rejected_by_the_magic_word() {
    let tree = Tree::new();
    let name_a = std::ffi::CString::new("map").unwrap();
    let name_b = std::ffi::CString::new("base").unwrap();
    let mut p: *mut tft_plan = ptr::null_mut();
    // SAFETY: live handle and names.
    assert_eq!(
        unsafe { tft_plan_create(tree.0, name_a.as_ptr(), name_b.as_ptr(), &mut p) },
        TFT_OK
    );
    // SAFETY: freed exactly once here.
    unsafe { tft_plan_free(p) };

    // The pointer is now dangling but the magic was zeroed before the drop, so a
    // second free is a no-op rather than a double-free.
    // SAFETY: this is the case under test; the allocation may be reused, which is
    // why the ABI documents double-free as undefined and this test only asserts
    // the common case where it has not been.
    unsafe { tft_plan_free(p) };
}

/// An unknown frame name is refused by name, not by crashing.
#[test]
fn an_unknown_frame_is_refused() {
    let tree = Tree::new();
    let a = std::ffi::CString::new("map").unwrap();
    let b = std::ffi::CString::new("no_such_frame").unwrap();
    let mut p: *mut tft_plan = ptr::null_mut();
    // SAFETY: live handle, NUL-terminated names.
    let rc = unsafe { tft_plan_create(tree.0, a.as_ptr(), b.as_ptr(), &mut p) };
    assert_eq!(rc, TFT_ERR_UNKNOWN_FRAME);
    assert!(p.is_null(), "no handle may be produced on failure");
}
