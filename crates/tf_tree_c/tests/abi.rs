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

/// **The appended enumerator is a minor bump, and `tft_check_abi` says so.**
///
/// §3.6's rule is that the major must match exactly and the runtime's minor may
/// exceed the compiled-against one. A caller built against the header that
/// predates `TFT_LAYOUT_QVEC7_WXYZ_TWIST6` therefore still links: it passes
/// `(0, 2)` and this library is `(0, 3)`. That is the entire compatibility
/// argument for adding a layout rather than a function, so it is asserted
/// rather than asserted-in-prose.
///
/// Mutant: bump `TFT_ABI_VERSION_MAJOR` instead ⇒ the first assertion fails and
/// every existing C caller has to be rebuilt for a value they never name.
#[test]
fn an_older_caller_still_links_across_the_layout_addition() {
    // Read through the exported functions rather than the constants: these are
    // what a linked caller actually observes, and a constant comparison is a
    // tautology the compiler folds away.
    assert_eq!(
        tft_abi_version_major(),
        0,
        "no major bump for an appended enum"
    );
    assert!(
        tft_abi_version_minor() >= 3,
        "the appended layout is a minor bump"
    );
    // The caller that compiled before the enumerator existed.
    assert_eq!(tft_check_abi(0, 2), TFT_OK);
    // ...and one from the future is still refused, because it may name a
    // discriminant this build would reject.
    assert_ne!(tft_check_abi(0, TFT_ABI_VERSION_MINOR + 1), TFT_OK);
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
