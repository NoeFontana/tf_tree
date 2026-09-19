//! The `tf_tree` C ABI — `docs/PHASE4.md` §3.
//!
//! # SAFETY (module-level, per `docs/decisions/0007`)
//!
//! This crate is an `unsafe` boundary: **a foreign caller**. `unsafe` turns caller pointers into
//! references once, at the entry point; past that check every body is safe code.
//!
//! 1. **Handles carry a magic word**, written by the constructor and zeroed by the destructor (§3.2).
//!    It rejects NULL and handle-type confusion; it does **not** validate an arbitrary foreign
//!    pointer (the read is `read_unaligned`), so the caller's contract is NULL or a handle.
//!    Use-after-free is best-effort.
//! 2. **`#![deny(unsafe_op_in_unsafe_fn)]`**: each dereference carries its own `// SAFETY:`.
//! 3. **This crate depends on `tf_tree`, not `tf_tree_core`**, so it cannot reach an invariant the
//!    safe facade protects.
//!
//! # Panics cannot escape
//!
//! Every entry point wraps its body in `error::guard`, which converts a panic into
//! `TFT_ERR_INTERNAL`; [`tft_last_error`] is the exception, staying callable from an error path.
//! callable from an error path.
//!
//! `#[allow(non_camel_case_types)]` below: these names appear verbatim in
//! `tf_tree.h`.

// `unsafe` boundary: a foreign caller. See `docs/decisions/0007`.
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(non_camel_case_types)]

#[cfg(feature = "bridge")]
pub mod bridge;
pub mod error;
pub mod layout;
pub mod publisher;
pub mod unstable;

use core::ffi::{c_char, c_void};
use std::sync::Arc;

use tf_tree::{Stamp, SystemDomain, Tree};

pub use error::{
    tft_error, tft_last_error, tft_status, TFT_ERR_ABI_MISMATCH, TFT_ERR_ALREADY_CLAIMED,
    TFT_ERR_ARENA_UNAVAILABLE, TFT_ERR_BAD_CONFIG, TFT_ERR_BAD_ENUM, TFT_ERR_BAD_HANDLE,
    TFT_ERR_BAD_STAMP, TFT_ERR_BAD_STRUCT_SIZE, TFT_ERR_BUFFER_TOO_SMALL, TFT_ERR_CHILD_DETACHED,
    TFT_ERR_CLAIM_REVOKED, TFT_ERR_DISCONNECTED, TFT_ERR_EXTRAPOLATION, TFT_ERR_INTERNAL,
    TFT_ERR_NON_MONOTONIC, TFT_ERR_NOT_A_ROTATION, TFT_ERR_NOT_DYNAMIC, TFT_ERR_NOT_FINITE,
    TFT_ERR_NO_DATA, TFT_ERR_NO_DERIVATIVES, TFT_ERR_NO_EDGE, TFT_ERR_NO_SEGMENT, TFT_ERR_NULL_ARG,
    TFT_ERR_PARENT_MISMATCH, TFT_ERR_READ_ONLY, TFT_ERR_RELEASED, TFT_ERR_RETRY,
    TFT_ERR_SLOT_CONTENDED, TFT_ERR_SLOT_RECYCLED, TFT_ERR_TIME_DOMAIN, TFT_ERR_TOPOLOGY_CHANGED,
    TFT_ERR_TREE_TOO_DEEP, TFT_ERR_UNKNOWN_FRAME, TFT_ERR_WRONG_THREAD, TFT_INVALID_ID,
    TFT_MESSAGE_LEN, TFT_OK,
};
pub use layout::{
    tft_layout, TFT_LAYOUT_AFFINE12_ROW_F32, TFT_LAYOUT_MAT4_COL, TFT_LAYOUT_MAT4_ROW,
    TFT_LAYOUT_QVEC7_WXYZ, TFT_LAYOUT_QVEC7_WXYZ_TWIST6, TFT_LAYOUT_QVEC7_XYZW,
};
#[cfg(feature = "test-hooks")]
pub use publisher::tft_test_push_unguarded;
pub use publisher::{
    tft_publisher, tft_publisher_free, tft_publisher_push, tft_publisher_push_many,
    tft_publisher_release, tft_tree_claim,
};
pub use unstable::{
    tft_plan_at_with_derivatives, tft_tree_edge_count, tft_tree_frame_count, tft_tree_frame_name,
    tft_tree_instance_uuid, TFT_TWIST_BYTES,
};
#[cfg(all(feature = "shm", target_os = "linux"))]
pub use unstable::{
    tft_tree_inherit_ownership, tft_tree_open_named, tft_tree_owner_lost, tft_tree_reap_dead,
    TFT_CONTENDED, TFT_INHERITED, TFT_NOT_APPLICABLE, TFT_OWNER_ALIVE, TFT_READ_ONLY,
};

use error::{amend_error, guard, record_lookup, set_error};

/// Major ABI version; **must match exactly** between the compiled-against header and the linked library.
pub const TFT_ABI_VERSION_MAJOR: u32 = 0;
/// Minor ABI version; the runtime's may be **≥** the compiled-against value (§3.6). Every bump is an
/// append.
///
/// * `1` → `2`: `tft_bridge_note_time_jump`; fields appended to `tft_bridge_sample` and
///   `tft_bridge_outcome`.
/// * `2` → `3`: [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] (`docs/API.md` §3.3).
/// * `3` → `4`: [`tft_stamp_from_parts`], [`tft_stamp_from_timespec`], [`TFT_ERR_BAD_STAMP`]
///   (`docs/API.md` §5.1).
/// * `4` → `5`: `tft_bridge_options::arena_name`, [`TFT_ERR_ARENA_UNAVAILABLE`]
///   (`docs/decisions/0015`).
/// * `5` → `6`: [`tft_plan_create_in_domain`] (`docs/decisions/0038`).
/// * `6` → `7`: [`tft_plan_at_extrapolating`], [`tft_extrap_policy`], [`tft_extrapolated`]
///   (`docs/decisions/0039`).
/// * `7` → `8`: `tft_bridge_close_startup_window`, `TFT_BRIDGE_REASON_STARTUP_CONFLICTS`
///   (`docs/decisions/0011` step 6).
pub const TFT_ABI_VERSION_MINOR: u32 = 8;

/// The library's major ABI version.
#[no_mangle]
pub extern "C" fn tft_abi_version_major() -> u32 {
    TFT_ABI_VERSION_MAJOR
}

/// The library's minor ABI version.
#[no_mangle]
pub extern "C" fn tft_abi_version_minor() -> u32 {
    TFT_ABI_VERSION_MINOR
}

/// Check the compiled-against header against the linked library: major must match exactly; the
/// runtime minor may be ≥ (§3.6). Call once at startup with the header's constants (the C++ wrapper
/// does).
///
/// # Errors
///
/// [`TFT_ERR_ABI_MISMATCH`]; `frame_a`/`frame_b` carry the caller's major/minor,
/// `plan_generation` and `current_generation` the library's.
#[no_mangle]
pub extern "C" fn tft_check_abi(compiled_major: u32, compiled_minor: u32) -> tft_status {
    guard(|| {
        if compiled_major == TFT_ABI_VERSION_MAJOR && compiled_minor <= TFT_ABI_VERSION_MINOR {
            return TFT_OK;
        }
        let msg = format!(
            "ABI mismatch: compiled against {compiled_major}.{compiled_minor}, \
             linked {TFT_ABI_VERSION_MAJOR}.{TFT_ABI_VERSION_MINOR} (major must \
             match exactly; the library's minor must be at least the header's)"
        );
        set_error(TFT_ERR_ABI_MISMATCH, &msg, |d| {
            d.frame_a = compiled_major;
            d.frame_b = compiled_minor;
            d.plan_generation = u64::from(TFT_ABI_VERSION_MAJOR);
            d.current_generation = u64::from(TFT_ABI_VERSION_MINOR);
        });
        TFT_ERR_ABI_MISMATCH
    })
}

/// Assemble a stamp from a `(sec, nanos)` pair, exactly (`docs/API.md` §5.1); the C spelling of
/// `Stamp::from_parts`. No float on any surface (R3).
///
/// It returns a status because out-of-range `nanos` or a wrapping sum would each yield a plausible
/// stamp and no `int64_t` sentinel exists.
///
/// # Errors
///
/// [`TFT_ERR_NULL_ARG`] if `out` is NULL. [`TFT_ERR_BAD_STAMP`] if `nanos` is
/// outside `[0, 1e9)` or the sum does not fit `int64_t`; `*out` is not written.
///
/// # Safety
///
/// `out` must be NULL or point to a writable `int64_t`.
#[no_mangle]
pub unsafe extern "C" fn tft_stamp_from_parts(sec: i64, nanos: u32, out: *mut i64) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        let Some(s) = Stamp::<SystemDomain>::from_parts(sec, nanos) else {
            return bad_stamp(sec, i64::from(nanos));
        };
        // SAFETY: `out` is non-null and caller-writable; written only after success, so a refusal leaves it as it was.
        unsafe { core::ptr::write(out, s.nanos()) };
        TFT_OK
    })
}

/// Assemble a stamp from a POSIX `struct timespec`'s fields.
///
/// # Errors
///
/// Everything [`tft_stamp_from_parts`] refuses, plus a negative `tv_nsec` (an interval passed as an
/// instant).
///
/// # Safety
///
/// `out` must be NULL or point to a writable `int64_t`.
#[no_mangle]
pub unsafe extern "C" fn tft_stamp_from_timespec(
    tv_sec: i64,
    tv_nsec: i64,
    out: *mut i64,
) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        let Some(s) = Stamp::<SystemDomain>::from_timespec(tv_sec, tv_nsec) else {
            return bad_stamp(tv_sec, tv_nsec);
        };
        // SAFETY: as `tft_stamp_from_parts`.
        unsafe { core::ptr::write(out, s.nanos()) };
        TFT_OK
    })
}

/// The refusal both stamp converters raise; `requested` carries seconds, `newest` nanoseconds.
fn bad_stamp(sec: i64, nanos: i64) -> tft_status {
    set_error(
        TFT_ERR_BAD_STAMP,
        "not a representable stamp: nanos must be in [0, 1000000000) and the \
         total must fit int64. Both are refused rather than normalised or \
         wrapped, because either would look like a valid time",
        |d| {
            d.requested = sec;
            d.newest = nanos;
        },
    );
    TFT_ERR_BAD_STAMP
}

const MAGIC_TREE: u64 = 0x7446_5F54_5245_4531; // "tFT_TREE1"-ish
const MAGIC_PLAN: u64 = 0x7446_5F50_4C41_4E31;

/// An opaque handle to a transform tree. `Send + Sync`. `#[repr(C)]` because `check_tree` reads the
/// magic through a field projection; the header declares it incomplete.
#[repr(C)]
pub struct tft_tree {
    magic: u64,
    share: Arc<TreeShare>,
}

/// An opaque handle to a compiled plan. `Send + Sync`, immutable; incomplete in the header.
#[repr(C)]
pub struct tft_plan {
    magic: u64,
    plan: tf_tree::Plan,
    /// The plan owns a share of the tree, so freeing the tree first must not dangle.
    share: Arc<TreeShare>,
    /// The time domain every evaluate call asks the engine about (`docs/decisions/0038`).
    domain: u8,
}

/// The tree, shared between its handle and every plan compiled from it (`docs/decisions/0017`).
pub(crate) struct TreeShare {
    pub(crate) tree: Arc<Tree>,
}

/// Generate a magic-word validator reading the field by name via `read_unaligned`; not a validator
/// for arbitrary memory.
macro_rules! magic_check {
    ($name:ident, $ty:ty, $magic:expr) => {
        /// # Safety
        ///
        /// `p` must be NULL, or point to at least eight readable bytes at the handle's `magic` offset
        /// (any live handle type satisfies that).
        #[inline]
        pub(crate) unsafe fn $name(p: *const $ty) -> bool {
            if p.is_null() {
                return false;
            }
            // SAFETY: non-null, with eight readable bytes at the magic offset per the contract;
            // `read_unaligned` and a field projection add no further UB.
            unsafe { core::ptr::addr_of!((*p).magic).read_unaligned() == $magic }
        }
    };
}

magic_check!(check_tree, tft_tree, MAGIC_TREE);
magic_check!(check_plan, tft_plan, MAGIC_PLAN);

/// Wrap a share of a tree in a fresh, independently owned handle.
#[cfg(feature = "bridge")]
pub(crate) fn tree_handle(share: Arc<TreeShare>) -> Box<tft_tree> {
    Box::new(tft_tree {
        magic: MAGIC_TREE,
        share,
    })
}

/// Join the running arena named by the environment, read-only (D18), as `tf_tree::open()` does
/// (`$TF_TREE_DOMAIN`, `$TF_TREE_NAME`, `$TF_TREE_RUNTIME_DIR`). Pass `*out` to [`tft_tree_free`]
/// exactly once.
/// # Safety
///
/// `out` must be NULL or point to a writable `*mut tft_tree`.
#[cfg(feature = "shm")]
#[no_mangle]
pub unsafe extern "C" fn tft_tree_open(out: *mut *mut tft_tree) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        match tf_tree::open() {
            Ok(tree) => {
                let h = Box::new(tft_tree {
                    magic: MAGIC_TREE,
                    share: Arc::new(TreeShare {
                        tree: Arc::new(tree),
                    }),
                });
                // SAFETY: `out` is non-null and caller-writable.
                unsafe { core::ptr::write(out, Box::into_raw(h)) };
                TFT_OK
            }
            Err(_) => {
                set_error(
                    TFT_ERR_INTERNAL,
                    "could not join an arena; check $TF_TREE_DOMAIN, $TF_TREE_NAME \
                     and that a publisher is running",
                    |_| {},
                );
                TFT_ERR_INTERNAL
            }
        }
    })
}

/// Release a tree handle; freeing NULL is a no-op. Plans compiled from it stay valid.
///
/// # Safety
///
/// `tree` must be NULL or a live handle not already freed; the magic word catches a double-free only
/// while the allocation is intact.
#[no_mangle]
pub unsafe extern "C" fn tft_tree_free(tree: *mut tft_tree) {
    if tree.is_null() {
        return;
    }
    // SAFETY: validated before the box is reconstituted.
    if !unsafe { check_tree(tree) } {
        return;
    }
    // Zero the magic first, so a repeated free sees a dead handle.
    // SAFETY: `check` confirmed this is a live `tft_tree`.
    unsafe { core::ptr::write(tree.cast::<u64>(), 0) };
    // SAFETY: the handle was produced by `Box::into_raw` in a constructor above.
    drop(unsafe { Box::from_raw(tree) });
}

/// Compile a plan for `target <- source`, by frame name; compile once, evaluate many times (D3).
///
/// This is [`tft_plan_create_in_domain`] with `domain = 0`; on an arena whose dynamic edges carry
/// another tag (`docs/PHASE4.md` §5.5) it returns [`TFT_ERR_TIME_DOMAIN`] (`docs/decisions/0038`).
///
/// # Errors
///
/// `*out` is not written on any failure. Three codes carry extra meaning here:
///
/// * [`TFT_ERR_UNKNOWN_FRAME`] — a name is not UTF-8 or does not resolve (read-only: undeclared, a
///   permanent hash-slot collision, or being interned right now, retry; writable: a full frame
///   table). With `frame_a` set: no consistent topology snapshot within the retry limit
///   (transient), or a parent index outside the frame table (corrupt arena).
/// * [`TFT_ERR_NO_DATA`] — the topology records a parent for `frame_a` but no edge (corrupt arena).
///   An edge with no samples yet compiles.
/// * [`TFT_ERR_TIME_DOMAIN`] — the route's dynamic edges publish in a tag other than `domain`, or
///   disagree among themselves (`edge` names the one that did).
///
/// # Safety
///
/// `tree` must be a live handle. `target` and `source` must be NUL-terminated UTF-8. `out` must be
/// NULL or point to a writable `*mut tft_plan`.
#[no_mangle]
pub unsafe extern "C" fn tft_plan_create(
    tree: *const tft_tree,
    target: *const c_char,
    source: *const c_char,
    out: *mut *mut tft_plan,
) -> tft_status {
    // SAFETY: every pointer is forwarded under the identical contract.
    unsafe { tft_plan_create_in_domain(tree, target, source, 0, out) }
}

/// Compile a plan for `target <- source` queried in time domain `domain` (`docs/decisions/0038`).
///
/// A foreign caller carries the tag (`0`–`3` built-in, `4`+ driver-declared; `docs/API.md` §2.5) as
/// data; `0` is [`tft_plan_create`]. A mismatch is reported once, at plan time; every evaluate still
/// passes the tag to the engine.
///
/// # Errors
///
/// Everything [`tft_plan_create`] returns, plus [`TFT_ERR_TIME_DOMAIN`] when the route has a dynamic
/// edge whose tag is not `domain`; `*out` is not written. A bare `domain != plan.domain()` would
/// wrongly refuse a static route, so this asks [`tf_tree::Plan::steps`] whether any
/// [`tf_tree::Step::Dyn`] is present.
///
/// # Safety
///
/// As [`tft_plan_create`]: `tree` must be a live handle, `target` and `source`
/// NUL-terminated UTF-8, and `out` NULL or a writable `*mut tft_plan`.
#[no_mangle]
pub unsafe extern "C" fn tft_plan_create_in_domain(
    tree: *const tft_tree,
    target: *const c_char,
    source: *const c_char,
    domain: u8,
    out: *mut *mut tft_plan,
) -> tft_status {
    guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { check_tree(tree) } {
            return bad_handle("tft_tree");
        }
        if target.is_null() || source.is_null() || out.is_null() {
            return null_arg("target/source/out");
        }
        // SAFETY: the caller contracts both are NUL-terminated C strings.
        let (t, s) = unsafe {
            (
                core::ffi::CStr::from_ptr(target),
                core::ffi::CStr::from_ptr(source),
            )
        };
        let (Ok(t), Ok(s)) = (t.to_str(), s.to_str()) else {
            set_error(
                TFT_ERR_UNKNOWN_FRAME,
                "frame name is not valid UTF-8",
                |_| {},
            );
            return TFT_ERR_UNKNOWN_FRAME;
        };
        // SAFETY: `check` confirmed the magic word.
        let h = unsafe { &*tree };
        let (Ok(tf), Ok(sf)) = (h.share.tree.frame(t), h.share.tree.frame(s)) else {
            set_error(TFT_ERR_UNKNOWN_FRAME, "no such frame in this tree", |_| {});
            return TFT_ERR_UNKNOWN_FRAME;
        };
        match h.share.tree.plan(tf, sf) {
            Ok(plan) => {
                // `Plan::has_dynamic` is private; this must match its predicate (see *Errors*).
                let has_dynamic = plan
                    .steps()
                    .iter()
                    .any(|s| matches!(s, tf_tree::Step::Dyn { .. }));
                if has_dynamic && plan.domain() != domain {
                    set_error(
                        TFT_ERR_TIME_DOMAIN,
                        &format!(
                            "plan {t} <- {s} runs over edges in time domain \
                             {} and was asked for domain {domain}; a lookup \
                             would refuse every stamp. Pass the publisher's \
                             domain to tft_plan_create_in_domain (docs/PHASE4.md 5.5)",
                            plan.domain()
                        ),
                        |_| {},
                    );
                    return TFT_ERR_TIME_DOMAIN;
                }
                let p = Box::new(tft_plan {
                    magic: MAGIC_PLAN,
                    plan,
                    share: Arc::clone(&h.share),
                    domain,
                });
                // SAFETY: `out` is non-null and the caller contracts it writable.
                unsafe { core::ptr::write(out, Box::into_raw(p)) };
                TFT_OK
            }
            Err(e) => record_lookup(e),
        }
    })
}

///
/// # Safety
///
/// `plan` must be NULL or a handle from [`tft_plan_create`] not already freed.
#[no_mangle]
pub unsafe extern "C" fn tft_plan_free(plan: *mut tft_plan) {
    if plan.is_null() {
        return;
    }
    // SAFETY: validated before the box is reconstituted.
    if !unsafe { check_plan(plan) } {
        return;
    }
    // SAFETY: `check` confirmed this is a live `tft_plan`.
    unsafe { core::ptr::write(plan.cast::<u64>(), 0) };
    // SAFETY: produced by `Box::into_raw` in `tft_plan_create`.
    drop(unsafe { Box::from_raw(plan) });
}

/// Evaluate `plan` at `stamp`, writing the result into `out` in `layout` (at least
/// `tft_layout_size(layout)` bytes).
///
/// **On a hot path, prefer [`tft_plan_at_many`]**: this builds a `Guard` per lookup
/// (`docs/decisions/0022`). The plan is evaluated in the domain it was compiled for.
///
/// [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] asks for derivatives: thirteen `f64` are written, failing with
/// `TFT_ERR_NO_DERIVATIVES` or `TFT_ERR_NO_SEGMENT` and writing nothing.
/// # Safety
///
/// `plan` must be a handle from `tft_plan_create` that has not been freed.
/// `out` must point to at least `tft_layout_size(layout)` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn tft_plan_at(
    plan: *const tft_plan,
    stamp: i64,
    layout: tft_layout,
    out: *mut c_void,
) -> tft_status {
    guard(|| {
        // SAFETY: validated below before any field access.
        if !unsafe { check_plan(plan) } {
            return bad_handle("tft_plan");
        }
        if out.is_null() {
            return null_arg("out");
        }
        let Some(n) = layout::payload_bytes(layout) else {
            return bad_enum("layout");
        };
        // SAFETY: `check` confirmed the magic word, so this points at a live
        // `tft_plan` constructed by `tft_plan_create`.
        let h = unsafe { &*plan };
        // SAFETY: the caller contracts that `out` has `n` writable bytes, and
        // `n` is exactly what `tft_layout_size` reports for this layout.
        let dst = unsafe { core::slice::from_raw_parts_mut(out.cast::<u8>(), n) };

        let g = h.share.tree.guard();
        // `_tagged`, with the handle's tag (`docs/decisions/0038`); the layout picks the evaluation once,
        // so a pose layout never pays for the adjoint chain.
        if layout::carries_twist(layout) {
            match h.plan.at_with_derivatives_tagged(&g, stamp, h.domain) {
                Ok(s) => {
                    layout::write_twist6(&s.pose, &s.twist, dst);
                    TFT_OK
                }
                Err(e) => record_lookup(e),
            }
        } else {
            match h.plan.at_tagged(&g, stamp, h.domain) {
                Ok(iso) => {
                    layout::write(&iso, layout, dst);
                    TFT_OK
                }
                Err(e) => record_lookup(e),
            }
        }
    })
}

/// [`tft_plan_at`] without the panic guard, to price `catch_unwind` (`docs/decisions/0022`); `test-hooks`.
/// # Safety
///
/// As [`tft_plan_at`]; a panic crossing this boundary is undefined behaviour.
#[cfg(feature = "test-hooks")]
#[no_mangle]
pub unsafe extern "C" fn tft_test_plan_at_unguarded(
    plan: *const tft_plan,
    stamp: i64,
    layout: tft_layout,
    out: *mut c_void,
) -> tft_status {
    // SAFETY: validated below before any field access.
    if !unsafe { check_plan(plan) } {
        return bad_handle("tft_plan");
    }
    if out.is_null() {
        return null_arg("out");
    }
    let Some(n) = layout::payload_bytes(layout) else {
        return bad_enum("layout");
    };
    // SAFETY: `check_plan` confirmed the magic word, so this points at a live
    // `tft_plan` constructed by `tft_plan_create`.
    let h = unsafe { &*plan };
    // SAFETY: the caller contracts that `out` has `n` writable bytes, and `n` is
    // exactly what `tft_layout_size` reports for this layout.
    let dst = unsafe { core::slice::from_raw_parts_mut(out.cast::<u8>(), n) };

    let g = h.share.tree.guard();
    if layout::carries_twist(layout) {
        match h.plan.at_with_derivatives_tagged(&g, stamp, h.domain) {
            Ok(s) => {
                layout::write_twist6(&s.pose, &s.twist, dst);
                TFT_OK
            }
            Err(e) => record_lookup(e),
        }
    } else {
        match h.plan.at_tagged(&g, stamp, h.domain) {
            Ok(iso) => {
                layout::write(&iso, layout, dst);
                TFT_OK
            }
            Err(e) => record_lookup(e),
        }
    }
}

/// Evaluate `plan` at `n` stamps, writing each result `out_stride_bytes` apart (0 = packed; §4.3).
///
/// # Partial writes
///
/// Evaluation stops at the first failing stamp and earlier elements stay written; `frame_b` carries
/// the failing index. Only the argument checks are all-or-nothing.
///
/// # `TFT_LAYOUT_QVEC7_WXYZ_TWIST6`
///
/// Accepted as by [`tft_plan_at`], per element. `TFT_ERR_NO_DERIVATIVES` fires on the first element
/// with the buffer untouched; `TFT_ERR_NO_SEGMENT` can fire part-way. Sort your stamps:
/// non-decreasing stamps ride a resumable cursor.
///
/// `plan` must be a live handle. `stamps` must point to `n` readable `int64_t`. `out` must point to at
/// least `n * stride` writable bytes (`stride` is `out_stride_bytes`, or the payload size if zero).
#[no_mangle]
pub unsafe extern "C" fn tft_plan_at_many(
    plan: *const tft_plan,
    stamps: *const i64,
    n: usize,
    layout: tft_layout,
    out: *mut c_void,
    out_stride_bytes: usize,
) -> tft_status {
    guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { check_plan(plan) } {
            return bad_handle("tft_plan");
        }
        let Some(payload) = layout::payload_bytes(layout) else {
            return bad_enum("layout");
        };
        if n == 0 {
            return TFT_OK;
        }
        if stamps.is_null() {
            return null_arg("stamps");
        }
        if out.is_null() {
            return null_arg("out");
        }
        let stride = if out_stride_bytes == 0 {
            payload
        } else {
            out_stride_bytes
        };
        if stride < payload {
            set_error(
                TFT_ERR_BUFFER_TOO_SMALL,
                "out_stride_bytes is smaller than the layout's payload",
                |_| {},
            );
            return TFT_ERR_BUFFER_TOO_SMALL;
        }
        let Some(span) = (n - 1)
            .checked_mul(stride)
            .and_then(|x| x.checked_add(payload))
        else {
            set_error(TFT_ERR_BUFFER_TOO_SMALL, "n * stride overflows", |_| {});
            return TFT_ERR_BUFFER_TOO_SMALL;
        };

        // SAFETY: `check` confirmed the magic word.
        let h = unsafe { &*plan };
        // SAFETY: the caller contracts `n` readable `i64` at `stamps`.
        let ts = unsafe { core::slice::from_raw_parts(stamps, n) };
        // SAFETY: the caller contracts `span` writable bytes at `out`, which is
        // exactly the extent the loop below touches.
        let dst = unsafe { core::slice::from_raw_parts_mut(out.cast::<u8>(), span) };

        let g = h.share.tree.guard();
        // Two loops chosen once from the layout, not a compare or closure per element (`examples/abi_cost.rs`).
        if layout::carries_twist(layout) {
            // `at_many_into` reports only success, so on failure the scalar loop re-runs to find the
            // index for `frame_b` (§4.3).
            if twist_batch(&h.plan, &g, ts, h.domain, dst, stride, payload) {
                return TFT_OK;
            }
            for (i, &t) in ts.iter().enumerate() {
                match h.plan.at_with_derivatives_tagged(&g, t, h.domain) {
                    Ok(s) => {
                        let off = i * stride;
                        layout::write_twist6(&s.pose, &s.twist, &mut dst[off..off + payload]);
                    }
                    Err(e) => return note_batch_failure(i, t, e),
                }
            }
        } else {
            for (i, &t) in ts.iter().enumerate() {
                match h.plan.at_tagged(&g, t, h.domain) {
                    Ok(iso) => {
                        let off = i * stride;
                        layout::write(&iso, layout, &mut dst[off..off + payload]);
                    }
                    Err(e) => return note_batch_failure(i, t, e),
                }
            }
        }
        TFT_OK
    })
}

/// Evaluate `stamps` as [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] via [`tf_tree::Plan::at_many_into`]; `false` if
/// any element failed. Non-packed strides go through a stack buffer (`docs/API.md` §3.3).
fn twist_batch(
    plan: &tf_tree::Plan,
    g: &tf_tree::Guard<'_>,
    stamps: &[i64],
    domain: u8,
    dst: &mut [u8],
    stride: usize,
    payload: usize,
) -> bool {
    const ROW: usize = 13;
    const CHUNK: usize = 32;
    debug_assert_eq!(payload, ROW * 8, "the twist layout is thirteen f64");

    if stride == payload && dst.as_ptr().align_offset(core::mem::align_of::<f64>()) == 0 {
        // SAFETY: `dst` is uniquely borrowed, `f64`-aligned (tested above) and `n * payload` long, a
        // multiple of eight; `f64` has no invalid bit patterns.
        let rows = unsafe {
            core::slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<f64>(), dst.len() / 8)
        };
        return plan
            .at_many_into_tagged(g, stamps, domain, tf_tree::Layout::QuatTwist, rows)
            .is_ok();
    }

    let mut scratch = [0.0f64; CHUNK * ROW];
    for (c, part) in stamps.chunks(CHUNK).enumerate() {
        let rows = &mut scratch[..part.len() * ROW];
        if plan
            .at_many_into_tagged(g, part, domain, tf_tree::Layout::QuatTwist, rows)
            .is_err()
        {
            return false;
        }
        for (j, row) in rows.chunks_exact(ROW).enumerate() {
            let off = (c * CHUNK + j) * stride;
            layout::put_f64(&mut dst[off..off + payload], row);
        }
    }
    true
}

/// Record the failure of batch element `i` at stamp `t` with `amend_error`, keeping the detail
/// `record_lookup` set (§3.3); the index goes in `frame_b`.
#[cold]
fn note_batch_failure(i: usize, t: i64, e: tf_tree::LookupError) -> tft_status {
    let status = record_lookup(e);
    amend_error(|d| {
        d.frame_b = u32::try_from(i).unwrap_or(TFT_INVALID_ID);
        if d.requested == 0 {
            d.requested = t;
        }
    });
    status
}

/// The bytes one transform occupies in `layout`, or `0` for an undefined discriminant.
#[no_mangle]
pub extern "C" fn tft_layout_size(layout: tft_layout) -> usize {
    layout::payload_bytes(layout).unwrap_or(0)
}

/// What to do when the stamp is newer than every sample on the route; an undefined value is
/// [`TFT_ERR_BAD_ENUM`].
pub type tft_extrap_policy = u32;

/// Refuse: [`TFT_ERR_EXTRAPOLATION`], nothing written. `0`, the default, and what [`tft_plan_at`] does.
pub const TFT_EXTRAP_ERROR: tft_extrap_policy = 0;
/// Hold the newest sample constant; [`tft_extrapolated::by_ns`] comes back in the same call.
pub const TFT_EXTRAP_HOLD: tft_extrap_policy = 1;
/// Extend the constant screw twist of the two newest samples (`docs/decisions/0039`); falls back to
/// [`TFT_EXTRAP_HOLD`] on a single-sample edge.
pub const TFT_EXTRAP_CONSTANT_TWIST: tft_extrap_policy = 2;

fn extrap_policy(policy: tft_extrap_policy) -> Option<tf_tree::ExtrapPolicy> {
    Some(match policy {
        TFT_EXTRAP_ERROR => tf_tree::ExtrapPolicy::Error,
        TFT_EXTRAP_HOLD => tf_tree::ExtrapPolicy::Hold,
        TFT_EXTRAP_CONSTANT_TWIST => tf_tree::ExtrapPolicy::ConstantTwist,
        _ => return None,
    })
}

/// How far past the route's newest common sample an answer was extrapolated (`docs/decisions/0039`
/// §1). Required: a NULL `info` is [`TFT_ERR_NULL_ARG`]; `struct_size` must be
/// `sizeof(tft_extrapolated)` or the call returns [`TFT_ERR_BAD_STRUCT_SIZE`] (§3.6).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tft_extrapolated {
    /// `sizeof(tft_extrapolated)` (§3.6).
    pub struct_size: u32,
    /// Nanoseconds past the newest stamp every dynamic edge has data for; `0` means every edge
    /// bracketed the query (`docs/decisions/0039` §3).
    pub by_ns: i64,
    /// The dynamic edge whose newest stamp is `by_ns` behind the query, or [`TFT_INVALID_ID`] when
    /// `by_ns` is `0`.
    pub edge: u32,
}

impl tft_extrapolated {
    /// A "not extrapolated" value with `struct_size` set and `edge` the sentinel (`docs/decisions/0048`).
    #[must_use]
    pub const fn blank() -> tft_extrapolated {
        tft_extrapolated {
            struct_size: core::mem::size_of::<tft_extrapolated>() as u32,
            by_ns: 0,
            edge: TFT_INVALID_ID,
        }
    }
}

/// [`tft_plan_at`], permitting extrapolation under `policy` and reporting how far
/// (`docs/decisions/0039`).
///
/// `info` is required (NULL is [`TFT_ERR_NULL_ARG`], nothing written). [`tft_plan_at`] still refuses
/// and is what a caller that must not act on invented data should call.
///
/// [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] is refused with [`TFT_ERR_BAD_ENUM`].
///
/// # Errors
///
/// Everything [`tft_plan_at`] returns. Under [`TFT_EXTRAP_ERROR`] a stamp past the newest sample is
/// [`TFT_ERR_EXTRAPOLATION`]; otherwise `info->by_ns` says how far. [`TFT_ERR_BAD_STRUCT_SIZE`] if
/// `info->struct_size` is not `sizeof(tft_extrapolated)`.
///
/// # Safety
///
/// `plan` must be a live handle. `out` must point to at least `tft_layout_size(layout)` writable
/// bytes. `info` must point to a writable `tft_extrapolated` with `struct_size` set.
#[no_mangle]
pub unsafe extern "C" fn tft_plan_at_extrapolating(
    plan: *const tft_plan,
    stamp: i64,
    policy: tft_extrap_policy,
    layout: tft_layout,
    out: *mut c_void,
    info: *mut tft_extrapolated,
) -> tft_status {
    guard(|| {
        // SAFETY: validated below before any field access.
        if !unsafe { check_plan(plan) } {
            return bad_handle("tft_plan");
        }
        if out.is_null() {
            return null_arg("out");
        }
        if info.is_null() {
            return null_arg("info");
        }
        let Some(policy) = extrap_policy(policy) else {
            return bad_enum("policy");
        };
        let Some(n) = layout::payload_bytes(layout) else {
            return bad_enum("layout");
        };
        if layout::carries_twist(layout) {
            set_error(
                TFT_ERR_BAD_ENUM,
                "TFT_LAYOUT_QVEC7_WXYZ_TWIST6 has no extrapolating form: the \
                 engine returns a pose here, not a pose and a twist",
                |_| {},
            );
            return TFT_ERR_BAD_ENUM;
        }
        // SAFETY: `info` is non-null and the caller contracts a `tft_extrapolated` with `struct_size`
        // set; `read_unaligned` as `magic_check!`.
        let declared = unsafe { core::ptr::addr_of!((*info).struct_size).read_unaligned() };
        if declared as usize != core::mem::size_of::<tft_extrapolated>() {
            set_error(
                TFT_ERR_BAD_STRUCT_SIZE,
                "info->struct_size is not sizeof(tft_extrapolated)",
                |_| {},
            );
            return TFT_ERR_BAD_STRUCT_SIZE;
        }
        // SAFETY: `check_plan` confirmed the magic word, so this points at a
        // live `tft_plan` constructed by `tft_plan_create`.
        let h = unsafe { &*plan };
        // SAFETY: the caller contracts that `out` has `n` writable bytes, and
        // `n` is exactly what `tft_layout_size` reports for this layout.
        let dst = unsafe { core::slice::from_raw_parts_mut(out.cast::<u8>(), n) };

        let g = h.share.tree.guard();
        // `_tagged`, with the handle's tag (`docs/decisions/0038`).
        match h.plan.at_extrapolating_tagged(&g, stamp, h.domain, policy) {
            Ok(x) => {
                layout::write(&x.pose, layout, dst);
                // Written only on success; `by_ns == 0` reports the sentinel edge.
                let e = tft_extrapolated {
                    struct_size: core::mem::size_of::<tft_extrapolated>() as u32,
                    by_ns: x.by_ns,
                    edge: if x.by_ns == 0 {
                        TFT_INVALID_ID
                    } else {
                        x.edge.get()
                    },
                };
                // SAFETY: `info` is non-null, caller-writable, and its `struct_size` matched exactly, so the
                // whole struct is inside the caller's allocation.
                unsafe { core::ptr::write(info, e) };
                TFT_OK
            }
            Err(e) => record_lookup(e),
        }
    })
}

/// A guarded no-op, to price `guard`.
#[cfg(feature = "test-hooks")]
#[no_mangle]
pub extern "C" fn tft_guarded_noop(x: i32) -> tft_status {
    guard(|| x)
}

/// Force a panic inside an `extern "C"` body, to prove the guard converts it into a status.
///
/// # Safety
///
/// Takes no pointers.
#[cfg(feature = "test-hooks")]
#[no_mangle]
pub extern "C" fn tft_test_panic() -> tft_status {
    guard(|| {
        #[allow(clippy::panic)]
        {
            panic!("deliberate panic from tft_test_panic");
        }
    })
}

/// The same for an entry point with no status (a count or size): the caller sees the fallback.
///
/// # Safety
///
/// Takes no pointers.
#[cfg(feature = "test-hooks")]
#[no_mangle]
pub extern "C" fn tft_test_panic_value() -> u32 {
    crate::error::guard_value(u32::MAX, || {
        #[allow(clippy::panic)]
        {
            panic!("deliberate panic from tft_test_panic_value");
        }
    })
}

/// Build a fixture: `map -> odom -> base` (dynamic ScLerp, 64 samples) plus a static `base -> sensor`.
///
/// # Safety
///
/// `out` must be NULL or point to a writable `*mut tft_tree`.
#[cfg(feature = "test-hooks")]
#[no_mangle]
pub unsafe extern "C" fn tft_test_tree_create(out: *mut *mut tft_tree) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        let cfg = tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(256));
        let mount = tf_tree::exp_se3([0.3, -0.7, 0.2, 0.11, -0.05, 0.37]);
        let Ok(tree) = tf_tree::TreeBuilder::new()
            .dynamic_edge("map", "odom", cfg)
            .dynamic_edge("odom", "base", cfg)
            .static_edge("base", "sensor", &mount)
            .build()
        else {
            return TFT_ERR_INTERNAL;
        };
        for (parent, child, k) in [("map", "odom", 1.0f64), ("odom", "base", 2.0)] {
            let (Ok(p), Ok(c)) = (tree.frame(parent), tree.frame(child)) else {
                return TFT_ERR_INTERNAL;
            };
            let Ok(w) = tree.claim(c, p) else {
                return TFT_ERR_INTERNAL;
            };
            for i in 0..64i64 {
                let f = i as f64;
                if w.push(
                    i * 10_000_000,
                    &tf_tree::exp_se3([
                        0.004 * k * f,
                        -0.003 * f,
                        0.002 * k * f,
                        0.05 * f,
                        -0.02 * k * f,
                        0.01 * f,
                    ]),
                )
                .is_err()
                {
                    return TFT_ERR_INTERNAL;
                }
            }
            core::mem::forget(w);
        }
        let h = Box::new(tft_tree {
            magic: MAGIC_TREE,
            share: Arc::new(TreeShare {
                tree: Arc::new(tree),
            }),
        });
        // SAFETY: `out` is non-null and the caller contracts it writable.
        unsafe { core::ptr::write(out, Box::into_raw(h)) };
        TFT_OK
    })
}

/// Build a fixture whose dynamic edge uses `LerpSlerp` (`map -> base`), for `TFT_ERR_NO_DERIVATIVES`.
///
/// # Safety
///
/// `out` must be NULL or point to a writable `*mut tft_tree`.
#[cfg(feature = "test-hooks")]
#[no_mangle]
pub unsafe extern "C" fn tft_test_lerpslerp_tree_create(out: *mut *mut tft_tree) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        let cfg = tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(64));
        let Ok(tree) = tf_tree::TreeBuilder::new()
            .default_interp(tf_tree::InterpPolicy::LerpSlerp)
            .dynamic_edge("map", "base", cfg)
            .build()
        else {
            return TFT_ERR_INTERNAL;
        };
        let (Ok(p), Ok(c)) = (tree.frame("map"), tree.frame("base")) else {
            return TFT_ERR_INTERNAL;
        };
        let Ok(w) = tree.claim(c, p) else {
            return TFT_ERR_INTERNAL;
        };
        for i in 0..32i64 {
            let f = i as f64;
            if w.push(
                i * 10_000_000,
                &tf_tree::exp_se3([
                    0.004 * f,
                    -0.003 * f,
                    0.002 * f,
                    0.05 * f,
                    -0.02 * f,
                    0.01 * f,
                ]),
            )
            .is_err()
            {
                return TFT_ERR_INTERNAL;
            }
        }
        // Held for the life of the tree.
        core::mem::forget(w);
        let h = Box::new(tft_tree {
            magic: MAGIC_TREE,
            share: Arc::new(TreeShare {
                tree: Arc::new(tree),
            }),
        });
        // SAFETY: `out` is non-null and the caller contracts it writable.
        unsafe { core::ptr::write(out, Box::into_raw(h)) };
        TFT_OK
    })
}

/// Build a fixture whose dynamic edge publishes in `domain`, plus a static edge (`docs/decisions/0038`).
///
/// # Safety
///
/// `out` must be NULL or point to a writable `*mut tft_tree`.
#[cfg(feature = "test-hooks")]
#[no_mangle]
pub unsafe extern "C" fn tft_test_domain_tree_create(
    domain: u8,
    out: *mut *mut tft_tree,
) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        let cfg = tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(64)).domain(domain);
        let mount = tf_tree::exp_se3([0.2, -0.1, 0.4, -0.3, 0.15, 0.05]);
        let Ok(tree) = tf_tree::TreeBuilder::new()
            .dynamic_edge("map", "odom", cfg)
            .static_edge("odom", "sensor", &mount)
            .build()
        else {
            return TFT_ERR_INTERNAL;
        };
        let (Ok(p), Ok(c)) = (tree.frame("map"), tree.frame("odom")) else {
            return TFT_ERR_INTERNAL;
        };
        let Ok(w) = tree.claim(c, p) else {
            return TFT_ERR_INTERNAL;
        };
        for i in 0..32i64 {
            let f = i as f64;
            if w.push(
                i * 10_000_000,
                &tf_tree::exp_se3([
                    0.004 * f,
                    -0.003 * f,
                    0.002 * f,
                    0.05 * f,
                    -0.02 * f,
                    0.01 * f,
                ]),
            )
            .is_err()
            {
                return TFT_ERR_INTERNAL;
            }
        }
        core::mem::forget(w);
        let h = Box::new(tft_tree {
            magic: MAGIC_TREE,
            share: Arc::new(TreeShare {
                tree: Arc::new(tree),
            }),
        });
        // SAFETY: `out` is non-null and the caller contracts it writable.
        unsafe { core::ptr::write(out, Box::into_raw(h)) };
        TFT_OK
    })
}

/// Build a fixture with a claimable dynamic edge `world -> robot` and a static `robot -> tool`.
///
/// # Safety
///
/// `out` must be NULL or point to a writable `*mut tft_tree`.
#[cfg(feature = "test-hooks")]
#[no_mangle]
pub unsafe extern "C" fn tft_test_publishable_tree_create(out: *mut *mut tft_tree) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        let cfg = tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(64));
        let mount = tf_tree::exp_se3([0.1, 0.2, -0.3, 0.4, -0.5, 0.6]);
        let Ok(tree) = tf_tree::TreeBuilder::new()
            .dynamic_edge("world", "robot", cfg)
            .static_edge("robot", "tool", &mount)
            .frame_headroom(4)
            .edge_headroom(2)
            .build()
        else {
            return TFT_ERR_INTERNAL;
        };
        let h = Box::new(tft_tree {
            magic: MAGIC_TREE,
            share: Arc::new(TreeShare {
                tree: Arc::new(tree),
            }),
        });
        // SAFETY: `out` is non-null and the caller contracts it writable.
        unsafe { core::ptr::write(out, Box::into_raw(h)) };
        TFT_OK
    })
}

pub(crate) fn bad_handle(what: &str) -> tft_status {
    set_error(
        TFT_ERR_BAD_HANDLE,
        "handle is NULL, freed, or not a tf_tree handle",
        |_| {},
    );
    let _ = what;
    TFT_ERR_BAD_HANDLE
}

pub(crate) fn null_arg(what: &str) -> tft_status {
    set_error(TFT_ERR_NULL_ARG, "a required argument was NULL", |_| {});
    let _ = what;
    TFT_ERR_NULL_ARG
}

pub(crate) fn bad_enum(what: &str) -> tft_status {
    set_error(
        TFT_ERR_BAD_ENUM,
        "an enum argument is outside the range this build defines",
        |_| {},
    );
    let _ = what;
    TFT_ERR_BAD_ENUM
}
