//! The C ABI against a real tree — `docs/PHASE4.md` §6.1, the half that needs a
//! live handle.
//!
//! `abi.rs` covers misuse with no handle at all. This covers the working path and
//! the misuse that only becomes reachable *once you have* a handle: freeing in
//! the wrong order, using a freed handle, striding a batch write.
#![cfg(feature = "test-hooks")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::ffi::c_char;
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

/// **The four `f64` pose layouts must agree about the transform**, differing
/// only in how it is written. A disagreement here means one of them is
/// transposing or reordering something.
///
/// Four, not six, and the name says four because it used to say five and was
/// wrong on both counts. `TFT_LAYOUT_AFFINE12_ROW_F32` is narrowed to `f32`, so
/// it cannot be compared bit for bit against these; it is pinned instead against
/// a hand-written pattern, in
/// `layout::tests::affine12_is_f32_row_major_without_the_bottom_row`.
/// `TFT_LAYOUT_QVEC7_WXYZ_TWIST6` is a pose *and a twist*, and
/// its pose half is asserted equal to `QVEC7_WXYZ` — byte for byte, from the
/// same call — in `the_twist_layout_writes_the_pose_and_the_twist_contiguously`.
#[test]
fn the_four_f64_pose_layouts_describe_the_same_transform() {
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

// **There is deliberately no "a freed handle is rejected" test.**
//
// One was written, and it was wrong in a way worth recording: reading a freed
// handle's magic word *is* a heap-use-after-free, so the test made
// `docs/PHASE4.md` §7 gate criterion 4 ("zero ASan findings") fail on unmutated
// code. ASan was right and the test was not.
//
// The magic-zeroing in `tft_tree_free`/`tft_plan_free` is still worth doing —
// it turns the common double-free into a no-op while the allocation is
// untouched — but that is a best-effort mitigation, not a checkable contract,
// and the ABI documents double-free as undefined. A test cannot assert
// undefined behaviour behaves; it can only commit the UB.

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

/// **A batch failure must carry the same detail a single call would**, plus the
/// index of the element that failed.
///
/// Found by review: `at_many` called `set_error` after `record_lookup`, and
/// `set_error` blanks the slot first — so a batch caller got `edge = INVALID` and
/// `oldest = newest = 0` where the equivalent single call reported the edge and
/// the retained window it needs to clamp its next query. That is exactly the
/// information loss §3.3 exists to prevent.
///
/// Mutant: use `set_error` instead of `amend_error` ⇒ fails on the window.
#[test]
fn a_batch_failure_keeps_the_detail_a_single_call_would_give() {
    let tree = Tree::new();
    let plan = tree.plan("map", "base");
    let bad = i64::MAX / 2;

    // What the single-shot path reports.
    let mut one = [0u8; 56];
    // SAFETY: live plan, correctly sized buffer.
    let rc1 = unsafe { tft_plan_at(plan.0, bad, TFT_LAYOUT_QVEC7_WXYZ, one.as_mut_ptr().cast()) };
    assert_eq!(rc1, TFT_ERR_EXTRAPOLATION);
    let single = fetch_error();
    assert_ne!(
        single.edge, TFT_INVALID_ID,
        "the single path names the edge"
    );
    assert_ne!(single.newest, 0, "the single path reports the window");

    // The same failure as the second element of a batch.
    let stamps = [250_000_000i64, bad];
    let mut buf = [0u8; 2 * 56];
    // SAFETY: live plan, 2 stamps, 112 bytes.
    let rc2 = unsafe {
        tft_plan_at_many(
            plan.0,
            stamps.as_ptr(),
            2,
            TFT_LAYOUT_QVEC7_WXYZ,
            buf.as_mut_ptr().cast(),
            0,
        )
    };
    assert_eq!(rc2, rc1, "the same failure must give the same status");
    let batch = fetch_error();
    assert_eq!(batch.edge, single.edge, "the batch path lost the edge id");
    assert_eq!(batch.oldest, single.oldest, "the batch path lost `oldest`");
    assert_eq!(batch.newest, single.newest, "the batch path lost `newest`");
    // ...and it says which element failed, which the single path cannot.
    assert_eq!(batch.frame_b, 1, "the failing index must be reported");
}

/// A plan whose compilation fails must return the failure, not a handle.
///
/// Found by review: this arm was untested, and returning `TFT_OK` there survived
/// the whole suite — which would hand the caller an uninitialised `out` pointer
/// it believes is a handle.
///
/// Mutant: return `TFT_OK` from `tft_plan_create`'s `Err` arm ⇒ fails.
#[test]
fn a_plan_that_cannot_compile_returns_no_handle() {
    let tree = Tree::new();
    // `sensor` and `map` are connected, so force the other failure the compiler
    // can produce: a frame that exists in neither direction.
    let a = std::ffi::CString::new("sensor").unwrap();
    let b = std::ffi::CString::new("").unwrap();
    let mut p: *mut tft_plan = ptr::null_mut();
    // SAFETY: live handle, NUL-terminated names, live out-pointer.
    let rc = unsafe { tft_plan_create(tree.0, a.as_ptr(), b.as_ptr(), &mut p) };
    assert_ne!(rc, TFT_OK, "an empty frame name must not compile");
    assert!(p.is_null(), "no handle may be produced on failure");
}

/// Read this thread's error detail.
fn fetch_error() -> tft_error {
    let mut e: tft_error = unsafe { core::mem::zeroed() };
    e.struct_size = core::mem::size_of::<tft_error>() as u32;
    // SAFETY: `e` is live, aligned, and its struct_size is set.
    assert_eq!(unsafe { tft_last_error(&mut e) }, TFT_OK);
    e
}

// ---------------------------------------------------------------------------
// The unstable tier — `tf_tree_unstable.h`, §3.1
// ---------------------------------------------------------------------------

/// **Derivatives cross the boundary, and the twist is the one §2 defines.**
///
/// The fixture's two dynamic edges are ScLerp with a constant screw per edge, so
/// the composed twist is checked against a **central difference of the pose**
/// rather than against another `tf_tree` call — the same oracle the Rust-side
/// tests use, and one that shares no code with the adjoint fold.
///
/// Asserted as a *convergence order*, not an absolute error: the stencil's own
/// truncation is O(h²), so halving `h` must quarter the error. An absolute
/// tolerance here would either be loose enough to pass a wrong answer or tight
/// enough to fail on the stencil rather than on the code.
#[test]
fn derivatives_match_a_central_difference_of_the_pose() {
    let t = Tree::new();
    let p = t.plan("map", "sensor");
    let at = 300_000_000i64;

    let mut pose = [0u8; 56];
    let mut twist = [0.0f64; 6];
    // SAFETY: live plan; both buffers are exactly the documented sizes.
    let rc = unsafe {
        tft_plan_at_with_derivatives(
            p.0,
            at,
            TFT_LAYOUT_QVEC7_WXYZ,
            pose.as_mut_ptr().cast(),
            twist.as_mut_ptr(),
        )
    };
    assert_eq!(rc, TFT_OK);

    // The oracle: (T(t)⁻¹ · T(t+h))'s log, over 2h. `log_se3` orders the result
    // [ω, v], matching the twist buffer.
    let err_at = |h_ns: i64| {
        let sample = |s: i64| {
            let mut b = [0u8; 56];
            // SAFETY: live plan, correctly sized buffer.
            assert_eq!(
                unsafe { tft_plan_at(p.0, s, TFT_LAYOUT_QVEC7_WXYZ, b.as_mut_ptr().cast()) },
                TFT_OK
            );
            tf_tree::Iso3::new(
                tf_tree::Quat::new(
                    read_f64(&b, 0),
                    read_f64(&b, 1),
                    read_f64(&b, 2),
                    read_f64(&b, 3),
                ),
                tf_tree::Vec3::new(read_f64(&b, 4), read_f64(&b, 5), read_f64(&b, 6)),
            )
        };
        let h = h_ns as f64 * 1e-9;
        let d = tf_tree::log_se3(sample(at - h_ns).inverse() * sample(at + h_ns));
        (0..6)
            .map(|i| (d[i] / (2.0 * h) - twist[i]).abs())
            .fold(0.0f64, f64::max)
    };

    let coarse = err_at(2_000_000);
    let fine = err_at(1_000_000);
    assert!(
        fine < coarse / 3.0 || fine < 1e-9,
        "halving h must roughly quarter the error: {coarse:e} -> {fine:e}"
    );
}

/// **`TFT_LAYOUT_QVEC7_WXYZ_TWIST6` is the same numbers in one buffer** —
/// `docs/API.md` §3.3's `(N, 13)` row, appended as a **minor** ABI bump
/// (`docs/PHASE4.md` §3.6).
///
/// The whole claim of a layout — rather than a fourth entry point — is that it
/// is a *re-encoding* and never a second computation. So this asserts the 104
/// bytes against the 56-byte pose write and the 6-element twist buffer taken
/// from the very same call, bit for bit. A tolerance would let a second
/// implementation hide inside it.
///
/// Mutants, applied to the source, run and reverted. Both name `write_twist6`,
/// which is where this write lives — `layout::write` has been pose-only since
/// the round that split the two, so a mutant naming *its* twist arm could not be
/// applied at all:
///
/// * `write_twist6` writes the pose half inline in `xyzw` order rather than
///   delegating to `put_qvec7_wxyz` ⇒ fails, "the pose half is not QVEC7_WXYZ
///   byte for byte". Delegation is the whole guard: an inline copy that happens
///   to be right today is one edit from being the trap this module opens with.
/// * `write_twist6` emits `v` before `ω` ⇒ fails, "twist slot 0 differs between
///   out_pose's tail and out_twist", `4620749313291464668` against
///   `4608340743733235298`.
#[test]
fn the_twist_layout_writes_the_pose_and_the_twist_contiguously() {
    let t = Tree::new();
    let p = t.plan("map", "sensor");
    let at = 300_000_000i64;

    assert_eq!(
        tft_layout_size(TFT_LAYOUT_QVEC7_WXYZ_TWIST6),
        104,
        "13 f64 — the size a C caller allocates from"
    );

    let mut row = [0u8; 104];
    let mut twist = [0.0f64; 6];
    // SAFETY: live plan; `row` is exactly `tft_layout_size` bytes and `twist`
    // exactly `TFT_TWIST_BYTES`.
    assert_eq!(
        unsafe {
            tft_plan_at_with_derivatives(
                p.0,
                at,
                TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                row.as_mut_ptr().cast(),
                twist.as_mut_ptr(),
            )
        },
        TFT_OK
    );

    // The pose half against the layout it extends, from an independent call.
    let mut pose = [0u8; 56];
    // SAFETY: live plan, correctly sized buffer.
    assert_eq!(
        unsafe { tft_plan_at(p.0, at, TFT_LAYOUT_QVEC7_WXYZ, pose.as_mut_ptr().cast()) },
        TFT_OK
    );
    assert_eq!(
        &row[..56],
        &pose[..],
        "the pose half is not QVEC7_WXYZ byte for byte"
    );

    // The tail against `out_twist` from the same call — the two spellings of
    // the same six numbers must not diverge.
    for (i, v) in twist.iter().enumerate() {
        assert_eq!(
            read_f64(&row, 7 + i).to_bits(),
            v.to_bits(),
            "twist slot {i} differs between out_pose's tail and out_twist"
        );
    }
    // Non-vacuity: the fixture must actually be moving, or six zeros would
    // satisfy every assertion above.
    assert!(
        twist.iter().any(|v| v.abs() > 1e-9),
        "the fixture's twist is zero; this test would pass against a stub"
    );
}

/// **The stable batch entry points serve the twist layout, and every row they
/// write is bit-identical to the scalar derivative call** — `docs/PHASE5.md`
/// §4.4 item 1, which is NORMATIVE and says derivatives reach C.
///
/// This is the assertion the whole layout rests on. `tft_plan_at_many` is a
/// different loop from `tft_plan_at_with_derivatives` — it strides, it batches,
/// and it evaluates through `Plan::at_many_into` rather than one scalar call per
/// element — so "the same numbers" is a claim about two independent code paths
/// and not a tautology. Compared with `to_bits`, because a tolerance is exactly
/// where a second implementation of a velocity would hide.
///
/// **Both of `twist_batch`'s shapes are exercised**, and they are genuinely
/// different code: a tightly packed buffer is handed to `at_many_into` as the
/// output slice itself, while a strided one is evaluated a chunk at a time and
/// scattered. §4.3's whole reason for the stride parameter is writing into an
/// array of caller structs, and a 13-element row is the widest payload the ABI
/// has, so the offset arithmetic has the most room to be wrong.
///
/// Mutants, all applied to the source, run and reverted:
///
/// * `panic!()` as the first statement of
///   `Plan::fold_at_with_derivatives_cursors` ⇒ **fails**, in
///   `tft_plan_at_many`'s panic guard: `left: -99, right: 0`, i.e.
///   `TFT_ERR_INTERNAL` where `TFT_OK` was expected. This is the mutant worth
///   reading. With the same injection *and* `twist_batch` disabled — which is
///   what this entry point did before this round, a scalar
///   `at_with_derivatives` per element — the test passes. It is the only
///   assertion available that the C batch reaches the cursor fold at all: a
///   cursor is a hint and cannot change an answer, so nothing about the values
///   below can distinguish the two.
/// * drop the `stride == payload` test in `twist_batch`, so the strided call
///   takes the packed arm ⇒ fails at element 1, "the strided batch row differs
///   from the scalar call" — the packed arm writes rows 104 bytes apart into a
///   buffer whose rows are 128 apart.
/// * `layout::write_twist6` emits `v` before `ω` ⇒ fails at element 0. The batch
///   rows come from `tf_tree_core::layout::write_quat_twist` and the reference
///   from `write_twist6`, so the two orders are exactly what this compares.
#[test]
fn the_batch_twist_layout_is_bit_identical_to_the_scalar_derivative_call() {
    let t = Tree::new();
    let p = t.plan("map", "sensor");

    // Off-grid and ascending: the interpolant and its derivative both run, and
    // the batch takes its monotone-cursor branch.
    let stamps: Vec<i64> = (0..48).map(|k| 200_000_000 + k * 7_300_000).collect();

    const STRIDE: usize = 128; // > 104, so the rows are not tightly packed
    let mut rows = vec![0u8; stamps.len() * STRIDE];
    // SAFETY: live plan; `stamps` is `len` readable i64 and `rows` holds
    // `(len - 1) * STRIDE + 104` bytes, which is what the call touches.
    assert_eq!(
        unsafe {
            tft_plan_at_many(
                p.0,
                stamps.as_ptr(),
                stamps.len(),
                TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                rows.as_mut_ptr().cast(),
                STRIDE,
            )
        },
        TFT_OK
    );

    // The same batch, tightly packed — the `Quat7Twist6[]` the C++ wrapper
    // passes, and the only shape that reaches the zero-copy arm.
    let mut packed = vec![0u8; stamps.len() * 104];
    // SAFETY: live plan; `stamps` is `len` readable i64 and `packed` is exactly
    // `len * 104` bytes, which is what a zero stride makes the call touch.
    assert_eq!(
        unsafe {
            tft_plan_at_many(
                p.0,
                stamps.as_ptr(),
                stamps.len(),
                TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                packed.as_mut_ptr().cast(),
                0,
            )
        },
        TFT_OK
    );

    let mut moving = 0usize;
    for (i, &s) in stamps.iter().enumerate() {
        // The reference: the unstable scalar call, into its own buffers.
        let mut pose = [0u8; 104];
        let mut twist = [0.0f64; 6];
        // SAFETY: live plan; both buffers are exactly the documented sizes.
        assert_eq!(
            unsafe {
                tft_plan_at_with_derivatives(
                    p.0,
                    s,
                    TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                    pose.as_mut_ptr().cast(),
                    twist.as_mut_ptr(),
                )
            },
            TFT_OK
        );
        let row = &rows[i * STRIDE..i * STRIDE + 104];
        assert_eq!(
            row,
            &pose[..],
            "element {i}: the strided batch row differs from the scalar call"
        );
        assert_eq!(
            &packed[i * 104..(i + 1) * 104],
            &pose[..],
            "element {i}: the packed batch row differs from the scalar call"
        );
        // ...and the tail really is the twist, not merely equal to itself.
        for (k, v) in twist.iter().enumerate() {
            assert_eq!(
                read_f64(row, 7 + k).to_bits(),
                v.to_bits(),
                "element {i} twist slot {k}"
            );
        }
        if twist.iter().any(|v| v.abs() > 1e-9) {
            moving += 1;
        }
    }
    // Non-vacuity: a stationary fixture would satisfy all of the above against
    // a layout that wrote six zeros.
    assert_eq!(moving, stamps.len(), "the fixture is not moving");

    // The scalar stable entry point serves it too, with the same bytes.
    let mut one = [0u8; 104];
    // SAFETY: live plan, 104-byte buffer for a 104-byte layout.
    assert_eq!(
        unsafe {
            tft_plan_at(
                p.0,
                stamps[3],
                TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                one.as_mut_ptr().cast(),
            )
        },
        TFT_OK
    );
    assert_eq!(
        &one[..],
        &rows[3 * STRIDE..3 * STRIDE + 104],
        "tft_plan_at and tft_plan_at_many disagree about the twist layout"
    );
}

/// **A twist batch that fails part-way still names the element and keeps the
/// rows before it** — the §4.3 contract, across the two-phase evaluation.
///
/// `tft_plan_at_many` serves this layout by handing the whole batch to
/// `Plan::at_many_into`, which reports *which error* and never *which element*.
/// So a failure falls back to the scalar loop, which reproduces it and reports
/// the index. Everything a caller can observe has to come out the same as it
/// would from a single-phase loop, and that is what this asserts: the status,
/// `frame_b`, the live prefix, and the untouched tail.
///
/// Both shapes are run, because they fail in different places — the packed arm
/// fails after `at_many_into` has already written rows into the caller's buffer,
/// the strided one after it has written them into a stack chunk that is then
/// discarded. The observable result must not be able to tell.
///
/// Mutants, applied to the source, run and reverted:
///
/// * `twist_batch`'s packed arm ignores the result and returns `true` ⇒ fails
///   at "stride 0" with `left: 0, right: -13` — `TFT_OK` reported for a batch
///   that did not evaluate.
/// * `note_batch_failure` is passed `0` instead of `i` ⇒ fails at "stride 0: the
///   failing index" with `left: 0, right: 2`.
/// * delete the `if twist_batch(..) { return TFT_OK; }` early return, leaving
///   only the scalar loop ⇒ **passes**, and that is the point: this test pins
///   the contract the fast path must not change, not the fast path itself. The
///   fast path is pinned by
///   `the_batch_twist_layout_is_bit_identical_to_the_scalar_derivative_call`.
#[test]
fn a_twist_batch_that_fails_part_way_reports_the_element_and_keeps_the_prefix() {
    let t = Tree::new();
    let p = t.plan("map", "sensor");
    // Two stamps inside the window, then one far outside it.
    let stamps = [200_000_000i64, 207_300_000, i64::MAX / 2];

    for stride in [0usize, 128] {
        let step = if stride == 0 { 104 } else { stride };
        let mut buf = vec![0xAAu8; stamps.len() * step];
        // SAFETY: live plan; three readable stamps and `3 * step` writable
        // bytes, which covers `(3-1) * step + 104` for either stride.
        let rc = unsafe {
            tft_plan_at_many(
                p.0,
                stamps.as_ptr(),
                stamps.len(),
                TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                buf.as_mut_ptr().cast(),
                stride,
            )
        };
        assert_eq!(rc, TFT_ERR_EXTRAPOLATION, "stride {stride}");
        let e = fetch_error();
        assert_eq!(e.frame_b, 2, "stride {stride}: the failing index");
        assert_ne!(
            e.edge, TFT_INVALID_ID,
            "stride {stride}: the edge must survive the fallback"
        );

        // The two elements before the failure are live, and equal to what the
        // scalar call gives at the same stamps.
        for (i, &s) in stamps[..2].iter().enumerate() {
            let mut one = [0u8; 104];
            // SAFETY: live plan, 104-byte buffer for a 104-byte layout.
            assert_eq!(
                unsafe {
                    tft_plan_at(
                        p.0,
                        s,
                        TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                        one.as_mut_ptr().cast(),
                    )
                },
                TFT_OK
            );
            assert_eq!(
                &buf[i * step..i * step + 104],
                &one[..],
                "stride {stride}: element {i} should be live"
            );
        }
        // ...and the failing element was not written at all.
        assert_eq!(
            &buf[2 * step..2 * step + 104],
            &[0xAAu8; 104][..],
            "stride {stride}: the failing element must not be written"
        );
    }
}

/// **A tightly packed but `f64`-misaligned `out` is still correct**, and is the
/// reason `twist_batch`'s zero-copy arm tests alignment as well as stride.
///
/// A C caller's `void*` carries no alignment promise. `layout::write_twist6`
/// never needed one — it stores through `f64::to_ne_bytes` into a byte slice —
/// but the packed arm builds a `&mut [f64]` over the caller's memory, and a
/// misaligned reference is Undefined Behaviour in Rust *even on a target whose
/// loads would have worked*. So the alignment test is not defensive style; it is
/// what decides which arm runs, and this is a caller that must take the other
/// one.
///
/// The buffer is deliberately skewed to `addr % 8 == 4`, which is reachable in C
/// from a `char` buffer, a packed struct, or an arena allocator.
///
/// Mutant, run: delete the `align_offset` test from `twist_batch` so this call
/// takes the packed arm. Two distinct failures, and both were observed:
///
/// * Under `just c-abi-check`'s Miri pass — "Undefined Behavior: constructing
///   invalid value of type `&mut [f64]`: encountered an unaligned reference
///   (required 8 byte alignment but found 4)".
/// * Under a plain `cargo nextest run` (the debug profile `just test` uses) —
///   `SIGABRT`, "unsafe precondition(s) violated: `slice::from_raw_parts_mut`
///   requires the pointer to be aligned and non-null". The standard library's
///   own debug-assertion catches it before Miri is needed.
///
/// It survives **only** under `--release`, where `debug_assertions` is off and
/// the UB goes unobserved on a target whose loads happen to work. That is the
/// configuration Miri exists for here, and it is why the claim is stated against
/// the arm that runs rather than against the values written.
#[test]
fn an_unaligned_packed_twist_batch_is_written_correctly() {
    let t = Tree::new();
    let p = t.plan("map", "sensor");
    let stamps: Vec<i64> = (0..8).map(|k| 200_000_000 + k * 7_300_000).collect();

    // Sixteen bytes of headroom, not eight: `skew` is at most 11 (up to 7 to
    // reach an 8-boundary, then 4 past it), and a shorter tail is a three-byte
    // overrun the allocator hides on most runs. Miri found exactly that.
    let mut raw = vec![0xAAu8; stamps.len() * 104 + 16];
    let base = raw.as_mut_ptr();
    let skew = (8 - (base as usize % 8)) % 8 + 4;
    // SAFETY: `skew <= 11` and `raw` has sixteen bytes of headroom past the payload.
    let out = unsafe { base.add(skew) };
    assert_eq!(
        out as usize % 8,
        4,
        "the buffer must actually be misaligned"
    );

    // SAFETY: live plan; `stamps.len()` readable i64, and `out` has
    // `stamps.len() * 104` writable bytes.
    let rc = unsafe {
        tft_plan_at_many(
            p.0,
            stamps.as_ptr(),
            stamps.len(),
            TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
            out.cast(),
            0,
        )
    };
    assert_eq!(rc, TFT_OK);

    for (i, &s) in stamps.iter().enumerate() {
        let mut one = [0u8; 104];
        // SAFETY: live plan, 104-byte buffer for a 104-byte layout.
        assert_eq!(
            unsafe {
                tft_plan_at(
                    p.0,
                    s,
                    TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                    one.as_mut_ptr().cast(),
                )
            },
            TFT_OK
        );
        assert_eq!(
            &raw[skew + i * 104..skew + (i + 1) * 104],
            &one[..],
            "element {i} of a misaligned packed batch"
        );
    }
    // Non-vacuity: the rows must not all be the sentinel, and the twist tail
    // must be live — six zeros would satisfy the comparison above against a
    // layout that wrote nothing but a pose.
    assert!(
        (0..stamps.len())
            .any(|i| (7..13).any(|k| read_f64(&raw[skew + i * 104..], k).abs() > 1e-9)),
        "the fixture's twist is zero everywhere"
    );
}

/// **A `LerpSlerp` edge refuses the twist layout with a *typed* status** —
/// `TFT_ERR_NO_DERIVATIVES`, naming the edge, not `TFT_ERR_BAD_ENUM`.
///
/// `LerpSlerp`'s body twist is an artifact of the interpolant rather than of
/// the motion, so it is refused rather than reported (`docs/PHASE4.md` §2.4).
/// The *status* is the load-bearing part: `TFT_ERR_BAD_ENUM` would tell a
/// caller their layout argument was invalid, sending them to fix a call that is
/// correct, when the real answer is "declare this edge `ScLerp`". That
/// distinction is the entire content of a typed error space (R5), and the two
/// are one `record_lookup`-versus-`bad_enum` away from each other at every call
/// site in the crate.
///
/// The buffer is checked untouched as well. `DerivativesUnavailable` is a
/// property of the *edge*, so it fires on the first element and nothing is
/// written — unlike the stamp-dependent errors, which can stop a batch part-way
/// through.
///
/// Mutant, run: in `tft_plan_at`'s twist arm, replace `Err(e) => record_lookup(e)`
/// with `Err(_) => bad_enum("layout")` ⇒ fails on the status assertion. The
/// pose-layout call below is the non-vacuity guard: without it, a fixture whose
/// plan simply did not resolve would satisfy every refusal assertion here.
#[test]
fn a_lerpslerp_edge_refuses_the_twist_layout_with_a_typed_status() {
    let mut raw: *mut tft_tree = ptr::null_mut();
    // SAFETY: `raw` is a live local.
    assert_eq!(unsafe { tft_test_lerpslerp_tree_create(&mut raw) }, TFT_OK);
    let t = Tree(raw);
    let p = t.plan("map", "base");
    let at = 155_000_000i64;

    // Non-vacuity: the pose layouts work over this plan and this stamp, so the
    // refusals below are about the derivative and not about the fixture.
    let mut pose = [0u8; 56];
    // SAFETY: live plan, 56-byte buffer for a 56-byte layout.
    assert_eq!(
        unsafe { tft_plan_at(p.0, at, TFT_LAYOUT_QVEC7_WXYZ, pose.as_mut_ptr().cast()) },
        TFT_OK
    );

    const SENTINEL: u8 = 0xAA;
    let mut row = [SENTINEL; 104];
    // SAFETY: live plan; the buffer is the layout's full size, so a write that
    // wrongly went ahead would be in bounds and visible rather than UB.
    assert_eq!(
        unsafe {
            tft_plan_at(
                p.0,
                at,
                TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                row.as_mut_ptr().cast(),
            )
        },
        TFT_ERR_NO_DERIVATIVES,
        "a LerpSlerp edge must be a typed refusal, not a bad enum"
    );
    assert!(
        row.iter().all(|b| *b == SENTINEL),
        "a refused tft_plan_at wrote into the caller's buffer"
    );
    // The error names the offending edge, which is what turns the status into
    // an action: it is the edge whose `InterpPolicy` has to change.
    let e = fetch_error();
    assert_eq!(e.code, TFT_ERR_NO_DERIVATIVES);
    assert_ne!(e.edge, TFT_INVALID_ID, "the refusal must name the edge");

    let stamps = [at, at + 10_000_000];
    let mut rows = [SENTINEL; 208];
    // SAFETY: live plan, two readable i64, 2 x 104 writable bytes.
    assert_eq!(
        unsafe {
            tft_plan_at_many(
                p.0,
                stamps.as_ptr(),
                stamps.len(),
                TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                rows.as_mut_ptr().cast(),
                0,
            )
        },
        TFT_ERR_NO_DERIVATIVES
    );
    assert!(
        rows.iter().all(|b| *b == SENTINEL),
        "a refused batch wrote into the caller's buffer"
    );
}

/// **Either output may be NULL.** Asking for only the twist is a real request,
/// and asking for neither is the caller's mistake, not a silent no-op.
#[test]
fn derivatives_write_only_what_was_asked_for() {
    let t = Tree::new();
    let p = t.plan("map", "sensor");
    let mut twist = [0.0f64; 6];
    // SAFETY: live plan; `out_pose` NULL is the case under test.
    assert_eq!(
        unsafe {
            tft_plan_at_with_derivatives(
                p.0,
                300_000_000,
                TFT_LAYOUT_QVEC7_WXYZ,
                core::ptr::null_mut(),
                twist.as_mut_ptr(),
            )
        },
        TFT_OK
    );
    assert!(twist.iter().any(|v| v.abs() > 1e-12), "a twist was written");

    let mut pose = [0u8; 56];
    // SAFETY: live plan; `out_twist` NULL is the case under test.
    assert_eq!(
        unsafe {
            tft_plan_at_with_derivatives(
                p.0,
                300_000_000,
                TFT_LAYOUT_QVEC7_WXYZ,
                pose.as_mut_ptr().cast(),
                core::ptr::null_mut(),
            )
        },
        TFT_OK
    );
    // Both NULL is refused rather than treated as a very fast success.
    // SAFETY: both output pointers NULL is the case under test.
    assert_eq!(
        unsafe {
            tft_plan_at_with_derivatives(
                p.0,
                300_000_000,
                TFT_LAYOUT_QVEC7_WXYZ,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        },
        TFT_ERR_NULL_ARG
    );
}

/// **Introspection reports the fixture's actual shape**, and a frame name that
/// does not fit is refused rather than truncated.
///
/// Mutant: write `buf_len` bytes and NUL-terminate at the end ⇒ `"sen"` comes
/// back for `"sensor"`, which is a *different plausible frame name* — the exact
/// failure mode this library exists to argue against.
///
/// This is also where both id conventions are pinned. They were established by
/// probing a built tree, not by reading the docs, because the docs disagree:
/// `error.rs` says edge index 0 is an ordinary slot while `TreeBuilder` reserves
/// it and `doctor` skips it. What is true is that **ids of both kinds run
/// `1 ..= count`**, and this is the test that keeps that true.
#[test]
fn introspection_reports_the_tree_and_refuses_to_truncate() {
    let t = Tree::new();
    // Four frames, three edges, both counted the same way.
    // SAFETY: `t.0` is a live handle.
    assert_eq!(
        unsafe { tft_tree_frame_count(t.0) },
        4,
        "map/odom/base/sensor"
    );
    // SAFETY: as above. The arena header holds 4 here — `declared + 1` — and
    // this asserting 3 is what caught the raw field leaking through.
    assert_eq!(unsafe { tft_tree_edge_count(t.0) }, 3);

    let mut buf: [c_char; 64] = [0; 64];
    let name_of = |id: u32, buf: &mut [c_char; 64]| -> tft_status {
        // SAFETY: live handle; `buf` has 64 writable bytes.
        unsafe { tft_tree_frame_name(t.0, id, buf.as_mut_ptr(), buf.len()) }
    };
    // Frame 0 is the root sentinel, not the first frame.
    assert_eq!(name_of(0, &mut buf), TFT_ERR_UNKNOWN_FRAME);

    // `c_char` is `i8` on x86_64 and `u8` on aarch64, so this cast is necessary
    // on one target and a no-op on the other; see `src/error.rs` for the full
    // note. The allow is the fix — deleting the cast breaks x86_64.
    #[allow(clippy::unnecessary_cast)]
    let read_name = |buf: &[c_char; 64]| -> String {
        buf.iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8 as char)
            .collect()
    };
    assert_eq!(name_of(1, &mut buf), TFT_OK);
    assert_eq!(read_name(&buf), "map", "ids start at 1");
    assert_eq!(name_of(4, &mut buf), TFT_OK);
    assert_eq!(read_name(&buf), "sensor", "...and run through the count");

    // "sensor" needs 7 bytes with its NUL; 6 is one short.
    let mut small: [c_char; 6] = [0; 6];
    // SAFETY: live handle; `small` has 6 writable bytes, which is the point.
    let rc = unsafe { tft_tree_frame_name(t.0, 4, small.as_mut_ptr(), small.len()) };
    assert_eq!(rc, TFT_ERR_BUFFER_TOO_SMALL);
    assert!(
        small.iter().all(|&c| c == 0),
        "nothing may be written when it does not fit"
    );
    // ...and the caller is told how much it needed.
    let mut e: tft_error = unsafe { core::mem::zeroed() };
    e.struct_size = core::mem::size_of::<tft_error>() as u32;
    // SAFETY: `e` is a live, aligned `tft_error` with `struct_size` set.
    assert_eq!(unsafe { tft_last_error(&mut e) }, TFT_OK);
    assert_eq!(e.requested, 7, "the required length, NUL included");

    // An id past the end is an error, not a read of whatever is there.
    assert_eq!(name_of(9999, &mut buf), TFT_ERR_UNKNOWN_FRAME);
}

/// **A private arena reports that it has no instance UUID, rather than zeros.**
///
/// The UUID is written only when a *shared* arena is created (`docs/PHASE2.md`
/// §1, A1); a heap arena leaves the field zero. This test was written to assert
/// that two trees differ, and found that two unrelated heap trees both reported
/// sixteen zero bytes and therefore compared **equal** — so a caller asking "are
/// we on the same arena?" would have got `yes` for two processes that had never
/// met.
///
/// Mutant: drop the `is_shared` check ⇒ `TFT_OK` and a zero buffer come back.
#[test]
fn a_private_arena_reports_no_instance_uuid_rather_than_zeros() {
    let t = Tree::new();
    let mut a = [0xAAu8; 16];
    // SAFETY: live handle; a 16-byte buffer.
    assert_eq!(
        unsafe { tft_tree_instance_uuid(t.0, a.as_mut_ptr()) },
        TFT_ERR_NO_DATA
    );
    assert_eq!(
        a, [0xAA; 16],
        "nothing may be written when there is no uuid"
    );
}

// ---------------------------------------------------------------------------
// Time domains — `docs/decisions/0038`
// ---------------------------------------------------------------------------

/// A fixture whose dynamic edge publishes in a caller-chosen time domain:
/// `map -> odom` dynamic, `odom -> sensor` static.
struct DomainTree(*mut tft_tree);

impl DomainTree {
    fn new(domain: u8) -> DomainTree {
        let mut p: *mut tft_tree = ptr::null_mut();
        // SAFETY: `p` is a live local.
        let rc = unsafe { tft_test_domain_tree_create(domain, &mut p) };
        assert_eq!(rc, TFT_OK);
        assert!(!p.is_null());
        DomainTree(p)
    }
}

impl Drop for DomainTree {
    fn drop(&mut self) {
        // SAFETY: created above, freed exactly once.
        unsafe { tft_tree_free(self.0) };
    }
}

/// Plan `target <- source` in `domain`, returning the status and the handle.
///
/// The handle is returned even on failure so the caller can assert it was left
/// alone: a refused plan must not hand back something a C caller will later
/// free.
fn plan_in(
    tree: *mut tft_tree,
    target: &str,
    source: &str,
    domain: u8,
) -> (tft_status, *mut tft_plan) {
    let t = std::ffi::CString::new(target).unwrap();
    let s = std::ffi::CString::new(source).unwrap();
    let mut p: *mut tft_plan = ptr::null_mut();
    // SAFETY: live handle, both strings NUL-terminated, `p` a live local.
    let rc = unsafe { tft_plan_create_in_domain(tree, t.as_ptr(), s.as_ptr(), domain, &mut p) };
    (rc, p)
}

/// **The arena the project tells operators to build is readable from C.**
///
/// `ros/tf_tree_ros/src/bridge_node.cpp` warns an operator running
/// `use_sim_time` to give the simulated tree its own domain (`docs/PHASE4.md`
/// §5.5). Before `docs/decisions/0038` following that advice made the arena
/// unreadable from C, C++ and Python by construction: every query site
/// constructed a `Stamp<SystemDomain>`, so `Plan::check_domain` compared tag `0`
/// against the arena's and refused — permanently, with no argument the caller
/// could pass.
///
/// Both halves are the test. Domain 1 reads a transform; domain 0 — which is
/// what `tft_plan_create` means and all a C caller *could* say before — is
/// refused with the code §5.5 already defined.
///
/// Mutant: route `tft_plan_at` back through the typed `Plan::at` (hard-coding
/// `SystemDomain::TAG`) ⇒ the domain-1 lookup below returns
/// `TFT_ERR_TIME_DOMAIN`. Confirmed by running it, not by reading it.
#[test]
fn a_plan_in_a_non_default_domain_reads_a_transform() {
    let tree = DomainTree::new(1);

    // What a caller could say before this decision, and what it gets.
    let (rc, p) = plan_in(tree.0, "map", "odom", 0);
    assert_eq!(
        rc, TFT_ERR_TIME_DOMAIN,
        "domain 0 cannot read a tag-1 arena"
    );
    assert!(p.is_null(), "a refused plan hands back no handle to free");
    let e = fetch_error();
    assert_eq!(e.code, TFT_ERR_TIME_DOMAIN);
    let msg = String::from_utf8_lossy(
        &e.message
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect::<Vec<u8>>(),
    )
    .into_owned();
    // The reason the check is at plan time and not in the hot loop: the frame
    // names are still in hand here and are gone by the first lookup.
    assert!(msg.contains("map"), "the message names the route: {msg}");
    assert!(msg.contains("odom"), "the message names the route: {msg}");

    // The same refusal through the frozen spelling, which `0038` defines as
    // this call with `domain = 0`.
    let t = std::ffi::CString::new("map").unwrap();
    let s = std::ffi::CString::new("odom").unwrap();
    let mut legacy: *mut tft_plan = ptr::null_mut();
    // SAFETY: live handle, NUL-terminated names, `legacy` a live local.
    let rc = unsafe { tft_plan_create(tree.0, t.as_ptr(), s.as_ptr(), &mut legacy) };
    assert_eq!(rc, TFT_ERR_TIME_DOMAIN, "tft_plan_create is domain 0");
    assert!(legacy.is_null());

    // And what the decision adds: the tag the publisher actually configured.
    let (rc, raw) = plan_in(tree.0, "map", "odom", 1);
    assert_eq!(rc, TFT_OK, "domain 1 is what this arena publishes in");
    let plan = Plan(raw);

    let mut out = [0u8; 128];
    // SAFETY: live plan; `out` is 128 bytes and MAT4_ROW needs exactly that.
    let rc = unsafe {
        tft_plan_at(
            plan.0,
            150_000_000,
            TFT_LAYOUT_MAT4_ROW,
            out.as_mut_ptr().cast(),
        )
    };
    assert_eq!(rc, TFT_OK, "a tagged plan evaluates");
    // A real transform, not arbitrary bytes: bottom row [0 0 0 1], and the
    // translation is the fixture's at t = 0.15 s (15 samples of 0.05 m each,
    // interpolated: x is between the 15th and 16th knot).
    assert_eq!(read_f64(&out, 12), 0.0);
    assert_eq!(read_f64(&out, 13), 0.0);
    assert_eq!(read_f64(&out, 14), 0.0);
    assert_eq!(read_f64(&out, 15), 1.0);
    assert!(
        read_f64(&out, 3) > 0.7 && read_f64(&out, 3) < 0.8,
        "x at t=0.15s is between the 15th and 16th knot, got {}",
        read_f64(&out, 3)
    );

    // The batch and derivative paths carry the same tag: three entry points
    // route through the handle, and a test that only drove the scalar one would
    // leave two of them able to regress silently.
    let stamps = [100_000_000i64, 150_000_000, 200_000_000];
    let mut rows = [0f64; 3 * 13];
    // SAFETY: live plan; three stamps; `rows` is 3 x 13 f64, tightly packed.
    let rc = unsafe {
        tft_plan_at_many(
            plan.0,
            stamps.as_ptr(),
            3,
            TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
            rows.as_mut_ptr().cast(),
            0,
        )
    };
    assert_eq!(rc, TFT_OK, "the batch fold takes the handle's tag too");

    let mut row = [0f64; 13];
    let mut twist = [0f64; 6];
    // SAFETY: live plan; 13 f64 of pose+twist and 6 f64 of twist.
    let rc = unsafe {
        tft_plan_at_with_derivatives(
            plan.0,
            150_000_000,
            TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
            row.as_mut_ptr().cast(),
            twist.as_mut_ptr(),
        )
    };
    assert_eq!(rc, TFT_OK, "so does the unstable derivatives entry point");
    assert_eq!(
        row[7..],
        twist[..],
        "the tail of the row is the twist it wrote separately"
    );
}

/// **A static route accepts any domain**, because the engine accepts it too.
///
/// `Plan::check_domain_tag` fires only when the plan has a dynamic edge, so a
/// lookup over `odom -> sensor` succeeds whatever tag it is asked in. Refusing
/// it at plan time would be a refusal the lookup would never have made — and a
/// caller holding one domain for a whole arena cannot know which of its routes
/// happen to be static.
///
/// Mutant: replace the plan-time predicate with a bare
/// `plan.domain() != domain` ⇒ the `1` and `7` arms below return
/// `TFT_ERR_TIME_DOMAIN`. Confirmed by running it.
#[test]
fn a_static_route_is_readable_from_any_domain() {
    let tree = DomainTree::new(1);
    for domain in [0u8, 1, 7] {
        let (rc, raw) = plan_in(tree.0, "odom", "sensor", domain);
        assert_eq!(rc, TFT_OK, "a static route has no domain to disagree with");
        let plan = Plan(raw);
        let mut out = [0u8; 128];
        // SAFETY: live plan; `out` is 128 bytes, MAT4_ROW's exact payload.
        let rc = unsafe {
            tft_plan_at(
                plan.0,
                150_000_000,
                TFT_LAYOUT_MAT4_ROW,
                out.as_mut_ptr().cast(),
            )
        };
        assert_eq!(rc, TFT_OK, "and it evaluates in domain {domain}");
    }
}

/// **A tag-0 dynamic route is refused at plan time too, not only at lookup.**
///
/// The other half of `a_static_route_is_readable_from_any_domain`, and the one
/// a `plan.domain()`-only predicate gets wrong. `Plan::domain` reports `0` for
/// both "no dynamic edge" and "dynamic edge in domain 0", so a check written
/// against it alone has to let this case through and leave the engine to refuse
/// every evaluate call — with `TimeDomainMismatch { expected: 0, got: 7 }` and
/// no frame names, which is the diagnostic `docs/decisions/0038` moved the
/// check to recover. Asking `Plan::steps` for a `Step::Dyn` separates the two.
///
/// Mutant: drop the `has_dynamic &&` conjunct's *other* direction — i.e. gate
/// the refusal on `plan.domain() != 0` instead — ⇒ `TFT_OK` here, and the
/// message assertions below never run.
#[test]
fn a_tag_zero_dynamic_route_is_refused_before_the_first_lookup() {
    // `Tree::new()`'s fixture publishes in domain 0 and `map <- sensor` crosses
    // its dynamic edges, so this is a genuine mismatch the engine would refuse.
    let tree = Tree::new();
    let (rc, p) = plan_in(tree.0, "map", "sensor", 7);
    assert_eq!(
        rc, TFT_ERR_TIME_DOMAIN,
        "a dynamic tag-0 route cannot be read in domain 7"
    );
    assert!(p.is_null(), "a refused plan hands back no handle to free");

    let e = fetch_error();
    assert_eq!(e.code, TFT_ERR_TIME_DOMAIN);
    let msg = String::from_utf8_lossy(
        &e.message
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect::<Vec<u8>>(),
    )
    .into_owned();
    assert!(msg.contains("map"), "the message names the route: {msg}");
    assert!(msg.contains("sensor"), "the message names the route: {msg}");

    // And the refusal is the engine's, not a stricter one invented here: the
    // same fixture, the same route, in the domain it actually publishes in.
    let (rc, raw) = plan_in(tree.0, "map", "sensor", 0);
    assert_eq!(rc, TFT_OK);
    drop(Plan(raw));
}

/// **The tag-0 arenas everything already used are untouched.**
///
/// `tft_plan_create` is `tft_plan_create_in_domain` with `domain = 0`, so the
/// fixture every other test in this file drives must behave identically through
/// either spelling — same status, same bytes. This is the half of the ABI
/// promise a new parameter is most likely to break.
#[test]
fn the_default_domain_is_what_tft_plan_create_always_meant() {
    let tree = Tree::new();
    let old = tree.plan("map", "sensor");
    let (rc, raw) = plan_in(tree.0, "map", "sensor", 0);
    assert_eq!(rc, TFT_OK);
    let new = Plan(raw);

    let (mut a, mut b) = ([0u8; 128], [0u8; 128]);
    // SAFETY: two live plans over the same route; each buffer is MAT4_ROW-sized.
    unsafe {
        assert_eq!(
            tft_plan_at(
                old.0,
                250_000_000,
                TFT_LAYOUT_MAT4_ROW,
                a.as_mut_ptr().cast()
            ),
            TFT_OK
        );
        assert_eq!(
            tft_plan_at(
                new.0,
                250_000_000,
                TFT_LAYOUT_MAT4_ROW,
                b.as_mut_ptr().cast()
            ),
            TFT_OK
        );
    }
    assert_eq!(a, b, "the two spellings are one entry point");
}
