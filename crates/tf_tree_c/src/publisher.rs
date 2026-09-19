//! The publish surface — `docs/PHASE4.md` §3.2's `tft_publisher`.
//!
//! `Publisher` is `!Sync`: the handle records its creating thread and `abort()`s in debug builds on
//! use from another (release returns `TFT_ERR_WRONG_THREAD`), with no mutex (§3.2, NORMATIVE). It
//! holds an [`OwnedWriter`] (`docs/decisions/0017`).

use core::ffi::{c_char, c_void};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use tf_tree::{ClaimApiError, ClaimError, OwnedWriter, PushError};

use crate::error::set_error;
use crate::layout;
use crate::{bad_enum, bad_handle, null_arg};
use crate::{
    tft_status, tft_tree, TFT_ERR_BUFFER_TOO_SMALL, TFT_ERR_RELEASED, TFT_ERR_UNKNOWN_FRAME, TFT_OK,
};

const MAGIC_PUBLISHER: u64 = 0x7446_5F50_5542_3031;

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static TOKEN: Cell<u64> = const { Cell::new(0) };
}

/// This thread's token, stable for its lifetime and never reused.
#[inline]
pub(crate) fn thread_token() -> u64 {
    TOKEN.with(|t| {
        let v = t.get();
        if v != 0 {
            return v;
        }
        let fresh = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        t.set(fresh);
        fresh
    })
}

/// An exclusive claim on one edge, and the only way to publish through the C ABI. `Send`, but one
/// thread at a time (see the module docs). `#[repr(C)]` because the magic check reads a named field.
#[repr(C)]
pub struct tft_publisher {
    magic: u64,
    owner: u64,
    /// `None` after [`tft_publisher_release`]. A leaked handle leaks the claim.
    writer: Option<OwnedWriter>,
}

/// # Safety
///
/// `p` must be NULL or point to a live handle (see `magic_check!`).
#[inline]
unsafe fn check_publisher(p: *const tft_publisher) -> bool {
    if p.is_null() {
        return false;
    }
    // SAFETY: non-null, and the caller contracts eight readable bytes at the magic offset.
    unsafe { core::ptr::addr_of!((*p).magic).read_unaligned() == MAGIC_PUBLISHER }
}

/// Check thread affinity (§3.2): `TFT_OK` on the owning thread, else abort (debug) or error.
#[inline]
fn check_thread(h: &tft_publisher) -> tft_status {
    check_thread_token(h.owner, "tft_publisher")
}

/// [`check_thread`] over a bare token, shared with the bridge; `what` names the handle type.
#[inline]
pub(crate) fn check_thread_token(owner: u64, what: &str) -> tft_status {
    if owner == thread_token() {
        return TFT_OK;
    }
    #[cfg(debug_assertions)]
    {
        // Not `panic!`: the guard would turn it into a status, and §3.2 asks for an abort.
        #[allow(clippy::print_stderr)]
        {
            eprintln!(
                "tf_tree: FATAL — {}. Claim a separate handle per thread, or \
                 hand this one over with a handoff the ABI cannot see.",
                wrong_thread_message(what)
            );
        }
        std::process::abort();
    }
    #[cfg(not(debug_assertions))]
    {
        set_error(
            crate::TFT_ERR_WRONG_THREAD,
            &wrong_thread_message(what),
            |_| {},
        );
        crate::TFT_ERR_WRONG_THREAD
    }
}

/// The sentence both profiles print; ASCII only (`set_message` substitutes `?`).
fn wrong_thread_message(what: &str) -> String {
    format!(
        "{what} is Send but not Sync (docs/PHASE4.md 3.2): it was created on \
         another thread, and exactly one thread may use it at a time"
    )
}

fn writer_of(h: &tft_publisher) -> Result<&OwnedWriter, tft_status> {
    let rc = check_thread(h);
    if rc != TFT_OK {
        return Err(rc);
    }
    h.writer.as_ref().ok_or_else(|| {
        set_error(
            TFT_ERR_RELEASED,
            "this publisher was released; claim the edge again to publish",
            |_| {},
        );
        TFT_ERR_RELEASED
    })
}

/// Claim exclusive write access to the edge attaching `child` to `parent` (one participant per edge,
/// D7). Released by [`tft_publisher_release`] or [`tft_publisher_free`]; a leaked handle leaks the
/// claim. The calling thread owns the publisher. An unseen frame name is interned, so a mistyped
/// `child` fails with `TFT_ERR_NO_EDGE`.
///
/// # Safety
///
/// `tree` must be a live handle. `child` and `parent` must be NUL-terminated
/// UTF-8. `out` must be NULL or point to a writable `*mut tft_publisher`.
#[no_mangle]
pub unsafe extern "C" fn tft_tree_claim(
    tree: *const tft_tree,
    child: *const c_char,
    parent: *const c_char,
    out: *mut *mut tft_publisher,
) -> tft_status {
    crate::error::guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { crate::check_tree(tree) } {
            return bad_handle("tft_tree");
        }
        if child.is_null() || parent.is_null() || out.is_null() {
            return null_arg("child/parent/out");
        }
        // SAFETY: the caller contracts both are NUL-terminated C strings.
        let (c, p) = unsafe {
            (
                core::ffi::CStr::from_ptr(child),
                core::ffi::CStr::from_ptr(parent),
            )
        };
        let (Ok(c), Ok(p)) = (c.to_str(), p.to_str()) else {
            set_error(
                TFT_ERR_UNKNOWN_FRAME,
                "frame name is not valid UTF-8",
                |_| {},
            );
            return TFT_ERR_UNKNOWN_FRAME;
        };
        // SAFETY: `check_tree` confirmed the magic word.
        let h = unsafe { &*tree };
        let (Ok(cf), Ok(pf)) = (h.share.tree.frame(c), h.share.tree.frame(p)) else {
            set_error(TFT_ERR_UNKNOWN_FRAME, "no such frame in this tree", |_| {});
            return TFT_ERR_UNKNOWN_FRAME;
        };
        // `claim_owned`: the writer carries its own `Arc<Tree>` (`docs/decisions/0017`).
        let writer = match h.share.tree.claim_owned(cf, pf) {
            Ok(w) => w,
            Err(e) => return map::claim(&e),
        };
        let handle = Box::new(tft_publisher {
            magic: MAGIC_PUBLISHER,
            owner: thread_token(),
            writer: Some(writer),
        });
        // SAFETY: `out` is non-null and the caller contracts it writable.
        unsafe { core::ptr::write(out, Box::into_raw(handle)) };
        TFT_OK
    })
}

/// Publish one transform at `stamp`, read from `src` in `layout`.
///
/// `src` must hold at least `tft_layout_size(layout)` bytes. `AFFINE12_ROW_F32` is refused.
///
/// # Safety
///
/// `pubh` must be a live handle used from the thread that created it. `src` must
/// point to at least `tft_layout_size(layout)` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn tft_publisher_push(
    pubh: *mut tft_publisher,
    stamp: i64,
    layout: crate::tft_layout,
    src: *const c_void,
) -> tft_status {
    crate::error::guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { check_publisher(pubh) } {
            return bad_handle("tft_publisher");
        }
        if src.is_null() {
            return null_arg("src");
        }
        let Some(n) = layout::payload_bytes(layout) else {
            return bad_enum("layout");
        };
        // SAFETY: `check_publisher` confirmed the magic word.
        let h = unsafe { &*pubh };
        let w = match writer_of(h) {
            Ok(w) => w,
            Err(rc) => return rc,
        };
        // SAFETY: the caller contracts `n` readable bytes at `src`.
        let bytes = unsafe { core::slice::from_raw_parts(src.cast::<u8>(), n) };
        let iso = match layout::read(layout, bytes) {
            None => return bad_enum("layout is write-only"),
            Some(Err(e)) => return map::read(e),
            Some(Ok(iso)) => iso,
        };
        match w.push(stamp, &iso) {
            Ok(()) => TFT_OK,
            Err(e) => map::push(&e),
        }
    })
}

/// Publish `n` transforms, reading each `src_stride_bytes` apart (0 means tightly packed; §4.3).
///
/// Stops at the first rejected element, leaving earlier ones published (no unpublishing); the failing
/// index is in the error detail's `frame_b`.
///
/// # Safety
///
/// `pubh` must be a live handle used from its creating thread. `stamps` must
/// point to `n` readable `int64_t`, and `src` to `n` strided payloads.
#[no_mangle]
pub unsafe extern "C" fn tft_publisher_push_many(
    pubh: *mut tft_publisher,
    stamps: *const i64,
    n: usize,
    layout: crate::tft_layout,
    src: *const c_void,
    src_stride_bytes: usize,
) -> tft_status {
    crate::error::guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { check_publisher(pubh) } {
            return bad_handle("tft_publisher");
        }
        let Some(payload) = layout::payload_bytes(layout) else {
            return bad_enum("layout");
        };
        if n == 0 {
            return TFT_OK;
        }
        if stamps.is_null() || src.is_null() {
            return null_arg("stamps/src");
        }
        let stride = if src_stride_bytes == 0 {
            payload
        } else {
            src_stride_bytes
        };
        if stride < payload {
            set_error(
                TFT_ERR_BUFFER_TOO_SMALL,
                "src_stride_bytes is smaller than the layout's payload",
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

        // SAFETY: `check_publisher` confirmed the magic word.
        let h = unsafe { &*pubh };
        let w = match writer_of(h) {
            Ok(w) => w,
            Err(rc) => return rc,
        };
        // SAFETY: `stamps` is non-NULL and the caller contracts `n` readable, aligned `i64`.
        let ts = unsafe { core::slice::from_raw_parts(stamps, n) };
        // SAFETY: `src` is non-NULL and the caller contracts `span` readable bytes, the overflow-checked extent.
        let bytes = unsafe { core::slice::from_raw_parts(src.cast::<u8>(), span) };

        for (i, &t) in ts.iter().enumerate() {
            let off = i * stride;
            let iso = match layout::read(layout, &bytes[off..off + payload]) {
                None => return bad_enum("layout is write-only"),
                Some(Err(e)) => {
                    let rc = map::read(e);
                    blame_index(i, t);
                    return rc;
                }
                Some(Ok(iso)) => iso,
            };
            if let Err(e) = w.push(t, &iso) {
                let rc = map::push(&e);
                blame_index(i, t);
                return rc;
            }
        }
        TFT_OK
    })
}

fn blame_index(i: usize, stamp: i64) {
    crate::error::amend_error(|d| {
        d.frame_b = u32::try_from(i).unwrap_or(crate::TFT_INVALID_ID);
        if d.requested == 0 {
            d.requested = stamp;
        }
    });
}

/// Release the claim now, leaving the handle valid but unusable. Calling it twice is a no-op.
///
/// # Safety
///
/// `pubh` must be a live handle used from the thread that created it.
#[no_mangle]
pub unsafe extern "C" fn tft_publisher_release(pubh: *mut tft_publisher) -> tft_status {
    crate::error::guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { check_publisher(pubh) } {
            return bad_handle("tft_publisher");
        }
        // SAFETY: `check_publisher` confirmed the magic word; this is the one entry point needing `&mut`.
        let h = unsafe { &mut *pubh };
        let rc = check_thread(h);
        if rc != TFT_OK {
            return rc;
        }
        h.writer = None;
        TFT_OK
    })
}

/// Release the claim and the handle. Freeing NULL is a no-op.
///
/// # Safety
///
/// `pubh` must be NULL or a handle from [`tft_tree_claim`] not already freed,
/// and must be freed from the thread that created it.
#[no_mangle]
pub unsafe extern "C" fn tft_publisher_free(pubh: *mut tft_publisher) {
    if pubh.is_null() {
        return;
    }
    // SAFETY: validated before the box is reconstituted.
    if !unsafe { check_publisher(pubh) } {
        return;
    }
    // Affinity applies to `free`: dropping the writer off-thread is the corruption §3.2 prevents.
    // SAFETY: `check_publisher` confirmed the magic word.
    if check_thread(unsafe { &*pubh }) != TFT_OK {
        return;
    }
    // SAFETY: `check_publisher` confirmed this is a live `tft_publisher`.
    unsafe { core::ptr::write(pubh.cast::<u64>(), 0) };
    // SAFETY: produced by `Box::into_raw` in `tft_tree_claim`.
    drop(unsafe { Box::from_raw(pubh) });
}

/// `tft_publisher_push` without the panic guard: measurement scaffolding for `examples/abi_cost.rs`
/// under `--features test-hooks`, never in a header.
///
/// # Safety
///
/// As [`tft_publisher_push`].
#[cfg(feature = "test-hooks")]
#[no_mangle]
pub unsafe extern "C" fn tft_test_push_unguarded(
    pubh: *mut tft_publisher,
    stamp: i64,
    layout: crate::tft_layout,
    src: *const c_void,
) -> tft_status {
    // SAFETY: validated before any field access.
    if !unsafe { check_publisher(pubh) } {
        return bad_handle("tft_publisher");
    }
    if src.is_null() {
        return null_arg("src");
    }
    let Some(n) = layout::payload_bytes(layout) else {
        return bad_enum("layout");
    };
    // SAFETY: `check_publisher` confirmed the magic word.
    let h = unsafe { &*pubh };
    let w = match writer_of(h) {
        Ok(w) => w,
        Err(rc) => return rc,
    };
    // SAFETY: the caller contracts `n` readable bytes at `src`.
    let bytes = unsafe { core::slice::from_raw_parts(src.cast::<u8>(), n) };
    let iso = match layout::read(layout, bytes) {
        None => return bad_enum("layout is write-only"),
        Some(Err(e)) => return map::read(e),
        Some(Ok(iso)) => iso,
    };
    match w.push(stamp, &iso) {
        Ok(()) => TFT_OK,
        Err(e) => map::push(&e),
    }
}

/// The claim and push error families.
pub(crate) mod map {
    use super::{ClaimApiError, ClaimError, PushError};
    use crate::error::set_error;
    use crate::layout::ReadError;
    use crate::*;

    pub(crate) fn claim(e: &ClaimApiError) -> tft_status {
        use ClaimApiError as C;
        match e {
            C::ChildDetached => {
                set_error(
                    TFT_ERR_CHILD_DETACHED,
                    "this handle belongs to the parent of a fork(); open a new tree",
                    |_| {},
                );
                TFT_ERR_CHILD_DETACHED
            }
            C::AlreadyClaimed { edge, cause } => {
                set_error(
                    TFT_ERR_ALREADY_CLAIMED,
                    "another participant already holds this edge (one writer per edge)",
                    |d| {
                        d.edge = edge.get();
                        if let ClaimError::EdgeAlreadyClaimed { owner_slot } = cause {
                            d.frame_a = *owner_slot;
                        }
                    },
                );
                TFT_ERR_ALREADY_CLAIMED
            }
            C::LeaseContended { edge } | C::ReapedDuringClaim { edge } => {
                set_error(
                    TFT_ERR_RETRY,
                    "the claim raced another participant's protocol; retry",
                    |d| d.edge = edge.get(),
                );
                TFT_ERR_RETRY
            }
            C::LeaseUnavailable { edge } => {
                set_error(
                    TFT_ERR_INTERNAL,
                    "the lock file could not be asked about this edge's lease",
                    |d| d.edge = edge.get(),
                );
                TFT_ERR_INTERNAL
            }
            C::UnknownFrame { child } => {
                set_error(
                    TFT_ERR_UNKNOWN_FRAME,
                    "the child frame is not a frame of this tree",
                    |d| d.frame_a = child.get(),
                );
                TFT_ERR_UNKNOWN_FRAME
            }
            C::NoEdge { child } => {
                set_error(
                    TFT_ERR_NO_EDGE,
                    "no edge attaches this child frame to any parent",
                    |d| d.frame_a = child.get(),
                );
                TFT_ERR_NO_EDGE
            }
            C::NotDynamic { child, edge } => {
                set_error(
                    TFT_ERR_NOT_DYNAMIC,
                    "this edge is static or tombstoned; there is nothing to publish to",
                    |d| {
                        d.frame_a = child.get();
                        d.edge = edge.get();
                    },
                );
                TFT_ERR_NOT_DYNAMIC
            }
            C::ParentMismatch {
                child,
                expected,
                actual,
            } => {
                set_error(
                    TFT_ERR_PARENT_MISMATCH,
                    "that child frame is attached to a different parent",
                    |d| {
                        d.frame_a = child.get();
                        d.frame_b = *actual;
                        d.plan_generation = u64::from(*expected);
                    },
                );
                TFT_ERR_PARENT_MISMATCH
            }
            C::ReadOnly => {
                set_error(
                    TFT_ERR_READ_ONLY,
                    "this arena is mapped read-only; open it with write access to publish",
                    |_| {},
                );
                TFT_ERR_READ_ONLY
            }
            other => {
                set_error(TFT_ERR_INTERNAL, "unhandled claim error", |_| {});
                let _ = other;
                TFT_ERR_INTERNAL
            }
        }
    }

    pub(crate) fn push(e: &PushError) -> tft_status {
        match e {
            PushError::NonMonotonicStamp { edge, last, got } => {
                set_error(
                    TFT_ERR_NON_MONOTONIC,
                    "stamp predates this edge's newest sample; stamps are non-decreasing",
                    |d| {
                        d.edge = edge.get();
                        d.requested = *got;
                        d.newest = *last;
                    },
                );
                TFT_ERR_NON_MONOTONIC
            }
            PushError::ClaimRevoked { edge } => {
                set_error(
                    TFT_ERR_CLAIM_REVOKED,
                    "a reaper judged this writer dead and took the edge; stop and re-claim",
                    |d| d.edge = edge.get(),
                );
                TFT_ERR_CLAIM_REVOKED
            }
            PushError::ChildDetached => {
                set_error(
                    TFT_ERR_CHILD_DETACHED,
                    "this handle belongs to the parent of a fork(); open a new tree",
                    |_| {},
                );
                TFT_ERR_CHILD_DETACHED
            }
            other => {
                set_error(TFT_ERR_INTERNAL, "unhandled push error", |_| {});
                let _ = other;
                TFT_ERR_INTERNAL
            }
        }
    }

    pub(crate) fn read(e: ReadError) -> tft_status {
        match e {
            ReadError::NotFinite => {
                set_error(
                    TFT_ERR_NOT_FINITE,
                    "the transform contains NaN or infinity",
                    |_| {},
                );
                TFT_ERR_NOT_FINITE
            }
            ReadError::NotAUnitQuaternion => {
                set_error(
                    TFT_ERR_NOT_A_ROTATION,
                    "the quaternion is not unit-norm (an all-zero buffer looks like this)",
                    |_| {},
                );
                TFT_ERR_NOT_A_ROTATION
            }
            ReadError::NotARotationMatrix => {
                set_error(
                    TFT_ERR_NOT_A_ROTATION,
                    "det R is not +1: the matrix is reflected or carries scale",
                    |_| {},
                );
                TFT_ERR_NOT_A_ROTATION
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::wrong_thread_message;

    #[test]
    fn the_wrong_thread_message_names_the_handle_that_moved() {
        for what in ["tft_publisher", "tft_bridge"] {
            let m = wrong_thread_message(what);
            assert!(m.starts_with(what), "message was {m:?}");
            assert!(
                m.contains("Send but not Sync"),
                "the phrase `publish.rs` asserts on stderr: {m:?}"
            );
            assert!(m.is_ascii(), "message was {m:?}");
        }
    }
}
