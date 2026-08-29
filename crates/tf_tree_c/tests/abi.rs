//! The C ABI's misuse surface — `docs/PHASE4.md` §6.1.
//!
//! These drive the `extern "C"` entry points directly, exactly as a C caller
//! would, rather than through any Rust convenience. Everything here is about what
//! happens when the caller gets it **wrong**, because that is the half a Rust
//! consumer never exercises and a C consumer meets on day one.
//!
//! Everything here works with **no handle at all**. The misuse that only becomes
//! reachable once you hold one -- freeing in the wrong order, using a freed
//! handle, striding a batch -- is in `live.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::ptr;

use tf_tree_c::*;

/// Read this thread's error detail. Returns `None` if the call itself failed.
#[cfg(feature = "test-hooks")]
fn last_error() -> Option<tft_error> {
    let mut e: tft_error = unsafe { core::mem::zeroed() };
    e.struct_size = core::mem::size_of::<tft_error>() as u32;
    // SAFETY: `e` is a live, aligned `tft_error` with `struct_size` set.
    let rc = unsafe { tft_last_error(&mut e) };
    (rc == TFT_OK).then_some(e)
}

#[cfg(feature = "test-hooks")]
// `c_char` is `i8` on x86_64 and `u8` on aarch64, so this cast is necessary
// on one target and a no-op on the other; see `src/error.rs` for the full
// note. The allow is the fix — deleting the cast breaks x86_64.
#[allow(clippy::unnecessary_cast)]
fn message(e: &tft_error) -> String {
    let bytes: Vec<u8> = e
        .message
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// **The ABI version is the first thing a C++ header checks** (§3.6), so it must
/// answer before anything is opened and must be self-consistent.
#[test]
fn abi_version_is_reportable_without_any_handle() {
    assert_eq!(tft_abi_version_major(), TFT_ABI_VERSION_MAJOR);
    assert_eq!(tft_abi_version_minor(), TFT_ABI_VERSION_MINOR);
}

/// **Every entry point must reject NULL rather than dereference it**, and say so
/// with a status a caller can branch on.
///
/// Mutant: drop the `out.is_null()` check in `tft_plan_at` ⇒ segfault, which is
/// how this test fails rather than by assertion.
#[test]
fn null_arguments_are_refused_not_dereferenced() {
    let mut buf = [0u8; 128];
    // SAFETY: every pointer is either NULL (deliberately) or a live local.
    unsafe {
        assert_eq!(
            tft_plan_at(ptr::null(), 0, TFT_LAYOUT_MAT4_ROW, buf.as_mut_ptr().cast()),
            TFT_ERR_BAD_HANDLE,
            "a NULL plan must be a bad handle"
        );
        assert_eq!(
            tft_plan_at_many(
                ptr::null(),
                ptr::null(),
                4,
                TFT_LAYOUT_MAT4_ROW,
                ptr::null_mut(),
                0
            ),
            TFT_ERR_BAD_HANDLE
        );
        assert_eq!(
            tft_plan_create(ptr::null(), ptr::null(), ptr::null(), ptr::null_mut()),
            TFT_ERR_BAD_HANDLE
        );
        // Freeing NULL is explicitly a no-op, not a crash.
        tft_tree_free(ptr::null_mut());
        tft_plan_free(ptr::null_mut());
    }
}

/// **Handle-type confusion is rejected** — passing a `tft_plan*` where a
/// `tft_tree*` is expected, and the reverse.
///
/// This is what the magic word actually buys, and it is the mistake a C caller
/// really makes. Both pointers here are live, aligned, correctly sized handles;
/// only the *type* is wrong, so the read is well-defined and the magic differs.
///
/// **What this deliberately does not test:** an arbitrary foreign pointer. An
/// earlier version passed a `vec![0u8; 512]`, and Miri correctly reported the
/// resulting 8-byte read as Undefined Behaviour — *the test asserting that
/// foreign pointers are safely rejected was itself the UB reproducer.* No check
/// can make an out-of-bounds read well-defined; the module docs now say so
/// rather than promising otherwise.
#[cfg(feature = "test-hooks")]
#[test]
fn handle_type_confusion_is_rejected() {
    use core::ptr;
    let mut tree: *mut tft_tree = ptr::null_mut();
    // SAFETY: `tree` is a live local.
    assert_eq!(unsafe { tft_test_tree_create(&mut tree) }, TFT_OK);
    let a = std::ffi::CString::new("map").unwrap();
    let b = std::ffi::CString::new("base").unwrap();
    let mut plan: *mut tft_plan = ptr::null_mut();
    // SAFETY: live handle, NUL-terminated names.
    assert_eq!(
        unsafe { tft_plan_create(tree, a.as_ptr(), b.as_ptr(), &mut plan) },
        TFT_OK
    );

    // A tree where a plan is expected: live, aligned, big enough — and wrong.
    let mut out = [0u8; 128];
    // SAFETY: `tree` is a live handle, so the magic read is in bounds; it simply
    // holds the wrong value.
    let rc = unsafe {
        tft_plan_at(
            tree.cast::<tft_plan>(),
            0,
            TFT_LAYOUT_MAT4_ROW,
            out.as_mut_ptr().cast(),
        )
    };
    assert_eq!(rc, TFT_ERR_BAD_HANDLE, "a tree is not a plan");

    // ...and a plan where a tree is expected.
    let mut p2: *mut tft_plan = ptr::null_mut();
    // SAFETY: `plan` is a live handle; only its type is wrong here.
    let rc = unsafe { tft_plan_create(plan.cast::<tft_tree>(), a.as_ptr(), b.as_ptr(), &mut p2) };
    assert_eq!(rc, TFT_ERR_BAD_HANDLE, "a plan is not a tree");

    let e = last_error().expect("detail must be available");
    assert_eq!(e.code, TFT_ERR_BAD_HANDLE);
    assert!(message(&e).contains("handle"), "{:?}", message(&e));

    // SAFETY: each freed exactly once.
    unsafe {
        tft_plan_free(plan);
        tft_tree_free(tree);
    }
}

/// **An unknown layout is an error, never a silent default.**
///
/// A newer header handing this build a layout it does not implement must fail
/// loudly. Defaulting to `QVEC7_WXYZ` would write 56 bytes of a different format
/// into a buffer the caller sized for something else.
#[test]
fn an_unknown_layout_is_refused() {
    assert_eq!(tft_layout_size(TFT_LAYOUT_QVEC7_WXYZ), 56);
    assert_eq!(tft_layout_size(TFT_LAYOUT_MAT4_ROW), 128);
    assert_eq!(tft_layout_size(TFT_LAYOUT_AFFINE12_ROW_F32), 48);
    assert_eq!(tft_layout_size(TFT_LAYOUT_QVEC7_WXYZ_TWIST6), 104);
    // Not a layout this build defines.
    assert_eq!(tft_layout_size(9999), 0);
}

/// **Appending the layout was a *minor* bump, not a major one.**
///
/// `the_abi_check_implements_the_rule_rather_than_restating_it` above already
/// covers the rule itself — an older minor links, a newer one does not — for
/// whatever the current version happens to be. What it cannot cover is *which
/// number moved* when the enumerator was added, and that is the entire
/// compatibility argument for shipping a layout instead of a function: bump the
/// major and every existing C caller has to be rebuilt for a value they never
/// name.
///
/// Read through the exported functions rather than the constants, because a
/// constant comparison is a tautology the compiler folds away and these are
/// what a linked caller actually observes.
///
/// Mutant: bump `TFT_ABI_VERSION_MAJOR` alongside the enumerator ⇒ fails here
/// and nowhere else.
#[test]
fn the_layout_addition_moved_the_minor_and_not_the_major() {
    assert_eq!(
        tft_abi_version_major(),
        0,
        "no major bump for an appended enum"
    );
    assert!(
        tft_abi_version_minor() >= 3,
        "the appended layout is a minor bump"
    );
}

/// **`tft_last_error` validates `struct_size`** (§3.6's Vulkan rule), so a
/// caller compiled against a header with a *different* `tft_error` is told so
/// rather than having a struct of the wrong length written over its stack.
#[test]
fn last_error_rejects_a_struct_size_it_does_not_know() {
    let mut e: tft_error = unsafe { core::mem::zeroed() };
    e.struct_size = 8; // far too small
                       // SAFETY: `e` is a live, aligned `tft_error`.
    assert_eq!(unsafe { tft_last_error(&mut e) }, TFT_ERR_BAD_STRUCT_SIZE);
    // NULL is refused too, and without consulting the error machinery.
    // SAFETY: passing NULL is the case under test.
    assert_eq!(unsafe { tft_last_error(ptr::null_mut()) }, TFT_ERR_NULL_ARG);
}

/// **A Rust panic inside an `extern "C"` body must not abort the process** —
/// §3.4, and the reason `guard` exists.
///
/// Since Rust 1.81 an escaping panic aborts, which for a library linked into
/// somebody's robot means killing the host process over a bug in us. That this
/// test *returns at all* is the assertion; the status and payload are the detail.
///
/// Mutant: remove `catch_unwind` from `guard` ⇒ the test binary aborts and the
/// whole suite fails, which is exactly the failure mode being prevented.
#[cfg(feature = "test-hooks")]
#[test]
fn a_panic_becomes_a_status_and_the_process_survives() {
    let rc = tft_test_panic();
    assert_eq!(rc, TFT_ERR_INTERNAL);
    let e = last_error().expect("the panic payload must be reportable");
    assert_eq!(e.code, TFT_ERR_INTERNAL);
    assert!(
        message(&e).contains("deliberate panic"),
        "the panic payload should reach the caller: {:?}",
        message(&e)
    );
    // And the process is still usable afterwards, which is the real claim.
    assert_eq!(tft_abi_version_major(), TFT_ABI_VERSION_MAJOR);
}

/// `tft_error` must stay `#[repr(C)]` and its size must match what the header
/// declares, or `struct_size` validation is meaningless.
#[test]
fn tft_error_layout_is_what_the_header_promises() {
    assert_eq!(
        core::mem::size_of::<tft_error>() % 8,
        0,
        "no tail padding surprises"
    );
    assert_eq!(TFT_MESSAGE_LEN, 256);
    let e: tft_error = unsafe { core::mem::zeroed() };
    let base = core::ptr::addr_of!(e) as usize;
    assert_eq!(
        core::ptr::addr_of!(e.struct_size) as usize - base,
        0,
        "struct_size must be first — every size check reads it before anything else"
    );
}

/// `tft_extrapolated` gets [`tft_error`]'s treatment, for [`tft_error`]'s
/// reason: `struct_size` at offset 0, or the check
/// [`tft_plan_at_extrapolating`] makes before it writes is reading some other
/// field's bytes.
///
/// It is a *callee-filled* struct like `tft_error`, so the size check is what
/// makes §3.6's append mechanism real here — a future field can only be added
/// safely if this build can tell an older caller's struct from its own.
#[test]
fn tft_extrapolated_layout_is_what_the_header_promises() {
    assert_eq!(
        core::mem::size_of::<tft_extrapolated>() % 8,
        0,
        "no tail padding surprises"
    );
    // SAFETY: three integers with no niche and no validity invariant, so an
    // all-zero bit pattern is a valid value of this type.
    let e: tft_extrapolated = unsafe { core::mem::zeroed() };
    let base = core::ptr::addr_of!(e) as usize;
    assert_eq!(
        core::ptr::addr_of!(e.struct_size) as usize - base,
        0,
        "struct_size must be first — the size check reads it before anything else"
    );
    // The three policy discriminants are on the wire: a caller compares against
    // a literal it compiled against, so moving one is a major ABI break rather
    // than a rename.
    assert_eq!(
        (TFT_EXTRAP_ERROR, TFT_EXTRAP_HOLD, TFT_EXTRAP_CONSTANT_TWIST),
        (0, 1, 2)
    );
}

/// **§3.6's rule is enforced, not merely reportable.**
///
/// "Major must match exactly; the runtime minor may be ≥ the compiled-against
/// minor." Until `tft_check_abi` existed the library shipped two getters and
/// left every caller to implement that sentence — and the caller who does not
/// is exactly the one who needs it.
///
/// Mutant: relax the comparison to `compiled_minor != TFT_ABI_VERSION_MINOR` ⇒
/// the forwards-compatible case below starts failing, which is the whole point
/// of a minor version.
#[test]
fn the_abi_check_implements_the_rule_rather_than_restating_it() {
    // The honest case: a caller compiled against this exact header.
    assert_eq!(
        tft_check_abi(TFT_ABI_VERSION_MAJOR, TFT_ABI_VERSION_MINOR),
        TFT_OK
    );
    // Forwards compatible: an older header against this newer library.
    if TFT_ABI_VERSION_MINOR > 0 {
        assert_eq!(tft_check_abi(TFT_ABI_VERSION_MAJOR, 0), TFT_OK);
    }
    // A newer header than the library — the caller expects functions that are
    // not here.
    assert_eq!(
        tft_check_abi(TFT_ABI_VERSION_MAJOR, TFT_ABI_VERSION_MINOR + 1),
        TFT_ERR_ABI_MISMATCH
    );
    // A different major is never compatible, in either direction.
    assert_eq!(
        tft_check_abi(TFT_ABI_VERSION_MAJOR + 1, 0),
        TFT_ERR_ABI_MISMATCH
    );
    assert_eq!(
        tft_check_abi(TFT_ABI_VERSION_MAJOR.wrapping_sub(1), 0),
        TFT_ERR_ABI_MISMATCH
    );
}

/// **The mismatch report names all four numbers.** A version error that says
/// "incompatible" and stops is the debugging session §3.6 is trying to prevent.
#[cfg(feature = "test-hooks")]
#[test]
fn an_abi_mismatch_names_both_versions() {
    assert_eq!(tft_check_abi(7, 3), TFT_ERR_ABI_MISMATCH);
    let e = last_error().expect("detail must be available");
    assert_eq!(e.frame_a, 7, "the caller's major");
    assert_eq!(e.frame_b, 3, "the caller's minor");
    assert_eq!(e.plan_generation, u64::from(TFT_ABI_VERSION_MAJOR));
    assert_eq!(e.current_generation, u64::from(TFT_ABI_VERSION_MINOR));
    let m = message(&e);
    assert!(m.contains("7.3"), "the caller's version must appear: {m}");
    assert!(
        m.contains(&format!("{TFT_ABI_VERSION_MAJOR}.{TFT_ABI_VERSION_MINOR}")),
        "the library's version must appear: {m}"
    );
}

// ---------------------------------------------------------------------------
// Stamps — `docs/API.md` §5.1
// ---------------------------------------------------------------------------

/// The table both bindings are checked against.
///
/// `(sec, nanos, expected)`, where `None` means **refused**. Its twin lives in
/// `tests/python/test_api.py::PARTS_TABLE` and the two must stay identical:
/// a converter that agrees with Rust on the successes and disagrees at the
/// edges is the bug this row of `docs/API.md` §6 exists to prevent, and it is
/// invisible to any test that only checks the middle.
const PARTS_TABLE: &[(i64, u32, Option<i64>)] = &[
    (0, 0, Some(0)),
    (1_700_000_000, 123_456_789, Some(1_700_000_000_123_456_789)),
    // A negative second with a positive nanosecond field is how a stamp just
    // before the epoch is spelled, and the sum is *not* `sec * 1e9`.
    (-1, 999_999_999, Some(-1)),
    (-1, 0, Some(-1_000_000_000)),
    // Exactly `i64::MIN`, and the reason the range check is on the sum rather
    // than staged: `-9_223_372_037 * 1e9` alone is below `i64::MIN`, so a
    // `checked_mul` then `checked_add` refuses this *representable* stamp.
    (-9_223_372_037, 145_224_192, Some(i64::MIN)),
    (-9_223_372_037, 145_224_191, None),
    // Exactly `i64::MAX`, and one past it.
    (9_223_372_036, 854_775_807, Some(i64::MAX)),
    (9_223_372_036, 854_775_808, None),
    // Out-of-range nanoseconds are refused, not normalised into the next
    // second — which is the whole point of the converter existing.
    (0, 1_000_000_000, None),
    (0, u32::MAX, None),
];

/// **`tft_stamp_from_parts` agrees with `Stamp::from_parts` on every row**,
/// including the refusals.
///
/// Mutant: normalise instead of refusing (`nanos % 1e9`, carrying into `sec`)
/// ⇒ the last two rows return `TFT_OK`. Mutant: stage the range check as
/// `checked_mul` then `checked_add` ⇒ row 5 is refused, and that row is a
/// stamp `int64_t` can hold.
#[test]
fn the_stamp_converter_refuses_what_rust_refuses() {
    for &(sec, nanos, want) in PARTS_TABLE {
        let mut out: i64 = 0x5A5A_5A5A_5A5A_5A5A;
        // SAFETY: `out` is a live local.
        let rc = unsafe { tft_stamp_from_parts(sec, nanos, &mut out) };
        match want {
            Some(ns) => {
                assert_eq!(rc, TFT_OK, "({sec}, {nanos}) should convert");
                assert_eq!(out, ns, "({sec}, {nanos})");
            }
            None => {
                assert_eq!(rc, TFT_ERR_BAD_STAMP, "({sec}, {nanos}) should refuse");
                assert_eq!(
                    out, 0x5A5A_5A5A_5A5A_5A5A,
                    "a refused conversion must not write *out"
                );
            }
        }
    }
}

/// `tft_stamp_from_timespec` is `from_parts` plus one refusal: POSIX permits a
/// negative `tv_nsec` only in a *relative* interval, so a negative one here
/// means an interval is being converted as an instant.
///
/// Mutant: cast `tv_nsec` to `u32` without the sign check ⇒ `(0, -1)` becomes
/// `4294967295` nanoseconds, which the range check then refuses anyway — so the
/// assertion that carries the load is `i64::MIN`, whose low 32 bits are zero
/// and which such a cast turns into a wrong *answer* rather than a refusal.
#[test]
fn a_negative_tv_nsec_is_an_interval_not_an_instant() {
    let mut out: i64 = 0;
    // SAFETY: `out` is a live local, here and below.
    assert_eq!(
        unsafe { tft_stamp_from_timespec(1_700_000_000, 123_456_789, &mut out) },
        TFT_OK
    );
    assert_eq!(out, 1_700_000_000_123_456_789);
    for bad in [-1i64, -999_999_999, i64::MIN, 1_000_000_000] {
        assert_eq!(
            unsafe { tft_stamp_from_timespec(0, bad, &mut out) },
            TFT_ERR_BAD_STAMP,
            "tv_nsec = {bad}"
        );
    }
}

/// A NULL `out` is a NULL-argument error, not a crash and not a silent success.
#[test]
fn a_stamp_converter_rejects_a_null_out() {
    // SAFETY: passing NULL is exactly what is under test; both entry points
    // check it before any write.
    unsafe {
        assert_eq!(
            tft_stamp_from_parts(0, 0, ptr::null_mut()),
            TFT_ERR_NULL_ARG
        );
        assert_eq!(
            tft_stamp_from_timespec(0, 0, ptr::null_mut()),
            TFT_ERR_NULL_ARG
        );
    }
}

/// The refusal carries the offending pair, so an operator reading
/// `TFT_ERR_BAD_STAMP` in a log knows which message was malformed.
///
/// Mutant: drop the `set_error` detail closure (`|_| {}`) ⇒ both fields stay 0
/// and both assertions fail.
#[cfg(feature = "test-hooks")]
#[test]
fn a_refused_stamp_names_the_pair_that_was_refused() {
    let mut out: i64 = 0;
    // SAFETY: `out` is a live local.
    assert_eq!(
        unsafe { tft_stamp_from_parts(42, 1_500_000_000, &mut out) },
        TFT_ERR_BAD_STAMP
    );
    let e = last_error().expect("detail must be available");
    assert_eq!(e.requested, 42, "seconds");
    assert_eq!(e.newest, 1_500_000_000, "nanoseconds");
}
