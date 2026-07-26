//! The publish surface — `docs/PHASE4.md` §3.2's `tft_publisher`.
//!
//! # `Send + !Sync`, in a language with no such notion
//!
//! Phase 1 makes single-writer-per-edge a *type-level* property: `Publisher` is
//! `!Sync`, so two Rust threads cannot hold references to one and the compiler
//! says so. C has no way to express that, and §3.2 is NORMATIVE about what to do
//! instead — **record the creating thread in the handle and `abort()` in debug
//! builds on use from another thread.**
//!
//! That is implemented here, plus one thing §3.2 does not require: in *release*
//! builds the same check returns [`TFT_ERR_WRONG_THREAD`] rather than
//! proceeding. §3.2's argument is "a loud abort in debug beats silent corruption
//! in release"; it does, but a status code beats both, and the check is one
//! thread-local load and a compare that the branch predictor gets right every
//! time. Costed in `examples/abi_cost.rs` rather than assumed.
//!
//! **Why not a mutex, as the Python binding does?** `tf_tree_py` wraps the
//! writer in a `Mutex` because Python threads legitimately share objects and the
//! GIL-free build makes that concurrent. A C caller who shares a `tft_publisher`
//! between threads has made a mistake, not a design choice: serializing it would
//! hide the mistake and hand them a publisher whose stamps interleave between
//! two threads' clocks. Refusing is the more useful answer.
//!
//! # The lifetime
//!
//! `EdgeWriter<'a>` borrows the `Tree`. A C handle cannot carry a lifetime, so
//! the borrow is extended to `'static` and its validity moved to a runtime
//! guarantee: the handle holds an `Arc<TreeShare>`, so the arena outlives the
//! claim for certain. This is the same trade `tf_tree_py` makes and it is
//! spelled with a refcount rather than a comment *because* the comment version
//! was a use-after-free there.

use core::ffi::{c_char, c_void};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tf_tree::{ClaimApiError, ClaimError, EdgeWriter, PushError};

use crate::error::set_error;
use crate::layout;
use crate::{bad_enum, bad_handle, null_arg, TreeShare};
use crate::{
    tft_status, tft_tree, TFT_ERR_BUFFER_TOO_SMALL, TFT_ERR_RELEASED, TFT_ERR_UNKNOWN_FRAME, TFT_OK,
};

const MAGIC_PUBLISHER: u64 = 0x7446_5F50_5542_3031;

// ---------------------------------------------------------------------------
// Thread identity
// ---------------------------------------------------------------------------

/// Monotonic source of thread tokens. Never recycled, so a token cannot be
/// reused by a thread created after the publisher's owner exited — which
/// `gettid` on Linux emphatically can, and which would turn the affinity check
/// into a coin flip on a long-running process that churns threads.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// This thread's token, assigned on first use.
    ///
    /// A `Cell<u64>` rather than a `OnceCell` so the fast path is a plain load
    /// and a compare against zero — no `Option` discriminant, no `Drop` glue,
    /// and therefore no lazy-destructor registration on the publish path.
    static TOKEN: Cell<u64> = const { Cell::new(0) };
}

/// This thread's token, stable for the thread's lifetime and never reused.
#[inline]
fn thread_token() -> u64 {
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

// ---------------------------------------------------------------------------
// The handle
// ---------------------------------------------------------------------------

/// An exclusive claim on one edge, and the only way to publish through the C
/// ABI. `Send`, but **one thread at a time** — see the module docs.
///
/// `#[repr(C)]` for the same reason as `tft_tree`: the magic check reads a named
/// field, and `repr(Rust)` promises nothing about where that field lands.
///
/// **The generated header declares this as an incomplete type.** §3.2 says these
/// are opaque handles, and a C caller who can see the fields can dereference
/// them. `cbindgen`'s `cbindgen:opaque` annotation does not take effect on this
/// shape, so `xtask headers` excludes the type and emits the forward
/// declaration itself — which also satisfies §3.1's requirement that the stable
/// header be reviewed by hand rather than merely generated.
#[repr(C)]
pub struct tft_publisher {
    magic: u64,
    /// The token of the thread that called `tft_tree_claim`.
    owner: u64,
    /// `None` after [`tft_publisher_release`], so a use-after-release is a clear
    /// status rather than a claim silently held past its intended scope.
    ///
    /// Releasing explicitly matters more here than in Rust: a C caller that
    /// leaks the handle leaks the *claim*, and no other process can take the
    /// edge until this one exits.
    writer: Option<EdgeWriter<'static>>,
    /// Keeps the arena alive for at least as long as the claim points into it.
    _share: Arc<TreeShare>,
}

/// # Safety
///
/// `p` must be NULL or point to a live handle — see `crate`'s `magic_check!`,
/// whose contract this shares.
#[inline]
unsafe fn check_publisher(p: *const tft_publisher) -> bool {
    if p.is_null() {
        return false;
    }
    // SAFETY: non-null, and the caller contracts eight readable bytes at the
    // magic field's offset. `read_unaligned` for the same reason as elsewhere.
    unsafe { core::ptr::addr_of!((*p).magic).read_unaligned() == MAGIC_PUBLISHER }
}

/// Check thread affinity, per §3.2.
///
/// Returns `TFT_OK` when the calling thread owns `h`. Otherwise:
///
/// * **debug builds abort**, loudly and by name, because §3.2 says so and
///   because a C programmer who has done this wants to find out at the moment it
///   happens rather than three frames of transform history later;
/// * **release builds return [`TFT_ERR_WRONG_THREAD`]**, which is strictly more
///   useful than proceeding and is why that code is not dead.
#[inline]
fn check_thread(h: &tft_publisher) -> tft_status {
    if h.owner == thread_token() {
        return TFT_OK;
    }
    #[cfg(debug_assertions)]
    {
        // Not `panic!`: the guard would convert it into a status, and §3.2 asks
        // for an abort specifically so this is impossible to ignore.
        // `eprintln!` rather than a log facade: this runs microseconds before
        // `abort()`, in a library with no logging dependency, and the message
        // has to survive a caller who has redirected nothing. The workspace
        // lint denies it everywhere else for good reason and this is the
        // exception §3.2 asks for.
        #[allow(clippy::print_stderr)]
        {
            eprintln!(
                "tf_tree: FATAL — a tft_publisher created on one thread was used on \
             another. tft_publisher is Send but not Sync (docs/PHASE4.md §3.2): \
             exactly one thread may use it at a time. Claim a separate publisher \
             per thread, or hand this one over with a handoff the ABI cannot see."
            );
        }
        std::process::abort();
    }
    #[cfg(not(debug_assertions))]
    {
        set_error(
            crate::TFT_ERR_WRONG_THREAD,
            "tft_publisher is Send but not Sync: it was created on another thread",
            |_| {},
        );
        crate::TFT_ERR_WRONG_THREAD
    }
}

/// Borrow the live writer, or report why not.
///
/// Written once so the affinity check cannot be forgotten on a new entry point:
/// there is no way to reach the writer that does not go through here.
fn writer_of(h: &tft_publisher) -> Result<&EdgeWriter<'static>, tft_status> {
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

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Claim exclusive write access to the edge attaching `child` to `parent`.
///
/// Exactly one participant may hold an edge (D7), across the whole machine when
/// the arena is shared. The claim is released by [`tft_publisher_release`] or
/// [`tft_publisher_free`]; a leaked handle is a leaked claim.
///
/// The thread that calls this **owns** the resulting publisher — see §3.2 and
/// this module's documentation.
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
        let writer = match h.share.tree.claim(cf, pf) {
            Ok(w) => w,
            Err(e) => return map::claim(&e),
        };
        // SAFETY: `EdgeWriter<'a>` borrows the `Tree`; the `Arc<TreeShare>`
        // stored alongside is a strong reference to that same `Tree`, so it
        // outlives this writer for certain. The writer is never handed out —
        // only borrowed under `writer_of` — so no caller can outlive it either.
        // This is the pattern `tf_tree_py` uses, for the same reason.
        let writer = unsafe { extend_to_static(writer) };
        let handle = Box::new(tft_publisher {
            magic: MAGIC_PUBLISHER,
            owner: thread_token(),
            writer: Some(writer),
            _share: Arc::clone(&h.share),
        });
        // SAFETY: `out` is non-null and the caller contracts it writable.
        unsafe { core::ptr::write(out, Box::into_raw(handle)) };
        TFT_OK
    })
}

/// Extend an `EdgeWriter`'s borrow to `'static`.
///
/// The signature pins both types so **only the lifetime can differ** — written
/// inline as `transmute::<EdgeWriter, EdgeWriter>` it would compile across a
/// type change as long as the sizes happened to match, which is precisely the
/// bug that shipped in `tf_tree_py` (see `PyPublisher::inner`).
///
/// # Safety
///
/// The caller must keep the borrowed `Tree` alive for as long as the returned
/// writer exists.
unsafe fn extend_to_static(w: EdgeWriter<'_>) -> EdgeWriter<'static> {
    // SAFETY: the caller's obligation above is exactly what the lifetime
    // parameter encodes; nothing else about the type changes.
    unsafe { core::mem::transmute::<EdgeWriter<'_>, EdgeWriter<'static>>(w) }
}

/// Publish one transform at `stamp`, read from `src` in `layout`.
///
/// `src` must hold at least `tft_layout_size(layout)` bytes.
///
/// `TFT_LAYOUT_AFFINE12_ROW_F32` is **not accepted**: it is an `f32` output
/// encoding for GPU upload, and publishing through it would silently halve the
/// precision of everything downstream. It returns `TFT_ERR_BAD_ENUM`.
///
/// Matrix layouts are validated — a left-handed or scaled matrix is refused
/// rather than converted into a plausible wrong rotation. See
/// [`crate::layout::read`].
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

/// Publish `n` transforms, reading each `src_stride_bytes` apart.
///
/// `src_stride_bytes == 0` means tightly packed. The stride exists for the same
/// reason it does on `tft_plan_at_many`: an array of `Sophus::SE3d` is usually
/// *not* tightly packed (§4.3).
///
/// **Stops at the first rejected element**, leaving the earlier ones published.
/// That is the opposite of `tft_plan_at_many`'s all-or-nothing rule and it is
/// deliberate: a publication is not a buffer to be filled, it is a sequence of
/// independent release-stores that readers may already have observed. There is
/// no unpublishing. The failing index is reported in the error detail's
/// `frame_b` so the caller knows exactly where the stream stopped.
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
        // Zero elements is a no-op, before the NULL checks: a caller looping
        // over an empty set legitimately passes NULL for both pointers.
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
        // The last element occupies `payload` bytes at offset `(n-1)*stride`,
        // so the required extent is that — not `n*stride`. Checked, because `n`
        // and `stride` are both caller-controlled.
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
        // SAFETY: the caller contracts `n` readable `i64` at `stamps` and
        // `span` readable bytes at `src`.
        let ts = unsafe { core::slice::from_raw_parts(stamps, n) };
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

/// Record which element of a batch failed, without discarding the detail the
/// error mapper just wrote. `frame_b` is unused by every publish error, so it is
/// free to carry the index.
fn blame_index(i: usize, stamp: i64) {
    crate::error::amend_error(|d| {
        d.frame_b = u32::try_from(i).unwrap_or(crate::TFT_INVALID_ID);
        if d.requested == 0 {
            d.requested = stamp;
        }
    });
}

/// Release the claim now, leaving the handle valid but unusable for publishing.
///
/// The claim is *also* released by [`tft_publisher_free`]. This exists because a
/// C caller frequently wants to give the edge back at a known point — the end of
/// a calibration pass, say — while the handle's lifetime is managed elsewhere.
/// Calling it twice is a no-op, not an error.
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
        // SAFETY: `check_publisher` confirmed the magic word, and this is the
        // one entry point that needs `&mut`: it drops the writer.
        let h = unsafe { &mut *pubh };
        let rc = check_thread(h);
        if rc != TFT_OK {
            return rc;
        }
        // Dropping the writer is what releases the arena record and the lease.
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
    // The affinity check applies to `free` too, and for a sharper reason than
    // to `push`: `EdgeWriter`'s destructor releases the claim *and* the OFD
    // lease, and doing that from a thread that does not own the writer is the
    // corruption §3.2 exists to prevent, not merely a misuse.
    //
    // SAFETY: `check_publisher` confirmed the magic word.
    if check_thread(unsafe { &*pubh }) != TFT_OK {
        return;
    }
    // Zero the magic before dropping, so a racing or repeated free sees a dead
    // handle rather than following a freed `Arc`.
    // SAFETY: `check_publisher` confirmed this is a live `tft_publisher`.
    unsafe { core::ptr::write(pubh.cast::<u64>(), 0) };
    // SAFETY: produced by `Box::into_raw` in `tft_tree_claim`.
    drop(unsafe { Box::from_raw(pubh) });
}

/// `tft_publisher_push` with the panic guard removed, and nothing else changed.
///
/// **Measurement scaffolding, not a shipped entry point.** `examples/abi_cost.rs`
/// subtracts this from the real one to attribute the publish path's boundary
/// cost, because two rounds of guessing at it were both wrong: the redundant
/// `sqrt` turned out to be noise, and so did the pose decode.
///
/// It is not exported outside `--features test-hooks` and never appears in a
/// header. Publishing through it would be unsound in exactly the way §3.4
/// describes — a panic would abort the caller's process.
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

// ---------------------------------------------------------------------------
// Error mapping for the publish direction
// ---------------------------------------------------------------------------

/// The claim and push error families, kept next to the code that raises them.
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
            C::AlreadyClaimed(inner) => {
                set_error(
                    TFT_ERR_ALREADY_CLAIMED,
                    "another participant already holds this edge (one writer per edge)",
                    |d| {
                        // The owner is a participant *slot*, not a pid — A3 made
                        // the claim word an indirection into the participant
                        // table. `tf_tree doctor` resolves it to a process.
                        //
                        // `ClaimError` is `#[non_exhaustive]`, so this is a
                        // `match` and not a `let`: a variant added upstream must
                        // leave the slot at its sentinel rather than fail to
                        // compile a boundary crate.
                        if let ClaimError::EdgeAlreadyClaimed { owner_slot } = inner {
                            d.frame_a = *owner_slot;
                        }
                    },
                );
                TFT_ERR_ALREADY_CLAIMED
            }
            // Both of these mean "somebody else was mid-protocol; go again".
            // Reporting them as one code is not a loss of information: the
            // caller's only correct response to either is to retry.
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
                    TFT_ERR_UNKNOWN_FRAME,
                    "no edge attaches this child frame to any parent",
                    |d| d.frame_a = child.get(),
                );
                TFT_ERR_UNKNOWN_FRAME
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
                    TFT_ERR_UNKNOWN_FRAME,
                    "that child frame is attached to a different parent",
                    |d| {
                        d.frame_a = child.get();
                        d.frame_b = *actual;
                        d.plan_generation = u64::from(*expected);
                    },
                );
                TFT_ERR_UNKNOWN_FRAME
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
            PushError::NonMonotonicStamp { last, got } => {
                set_error(
                    TFT_ERR_NON_MONOTONIC,
                    "stamp predates this edge's newest sample; stamps are non-decreasing",
                    |d| {
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
