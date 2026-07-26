//! Status codes, the thread-local error detail, and the panic guard —
//! `docs/PHASE4.md` §3.3 and §3.4.
//!
//! # Two things a C caller cannot do for itself
//!
//! **Carry a typed error.** Phase 1's errors are `Copy` structs that name the
//! offending edge, the requested stamp and the window that failed (D11), and
//! Python already exposes all of it. Collapsing that to an `int` would make the
//! C++ wrapper strictly worse than the Python binding at reporting what went
//! wrong. So the status code is accompanied by a thread-local [`tft_error`]
//! carrying the same fields, formatted **only on the error path**.
//!
//! **Survive a Rust panic.** Since Rust 1.81 a panic escaping an `extern "C"`
//! function aborts the process. For a library linked into somebody's robot,
//! killing the host process because of a bug in *us* is not acceptable, so every
//! entry point wraps its body in [`catch_unwind`](std::panic::catch_unwind).
//!
//! Both are handled by one helper, [`guard`], because they happen at the same
//! place: wrapping the body *is* the boundary. Two separate requirements
//! remembered at thirty call sites would be two requirements to forget.

use core::cell::RefCell;
use core::ffi::c_char;

use tf_tree::LookupError;

/// `0` on success; negative on failure.
pub type tft_status = i32;

/// Success.
pub const TFT_OK: tft_status = 0;

// Status codes. Stable, and **append-only** — a C consumer may compare against
// a literal it compiled against years ago.
/// A required pointer argument was NULL.
pub const TFT_ERR_NULL_ARG: tft_status = -1;
/// A handle's magic word did not match: freed, corrupted, or not ours.
pub const TFT_ERR_BAD_HANDLE: tft_status = -2;
/// A `struct_size` field named a size this build does not know.
pub const TFT_ERR_BAD_STRUCT_SIZE: tft_status = -3;
/// An enum argument was outside its defined range.
pub const TFT_ERR_BAD_ENUM: tft_status = -4;
/// The caller's output buffer is too small for the request.
pub const TFT_ERR_BUFFER_TOO_SMALL: tft_status = -5;
/// A frame name that this tree never interned.
pub const TFT_ERR_UNKNOWN_FRAME: tft_status = -10;
/// Target and source are in different connected components.
pub const TFT_ERR_DISCONNECTED: tft_status = -11;
/// The edge has no published samples yet.
pub const TFT_ERR_NO_DATA: tft_status = -12;
/// The requested stamp lies outside the edge's retained history.
pub const TFT_ERR_EXTRAPOLATION: tft_status = -13;
/// The topology changed since the plan was compiled; re-plan.
pub const TFT_ERR_TOPOLOGY_CHANGED: tft_status = -14;
/// The query's time domain does not match the plan's.
pub const TFT_ERR_TIME_DOMAIN: tft_status = -15;
/// The ring lapped the reader mid-read. Retryable.
pub const TFT_ERR_SLOT_RECYCLED: tft_status = -16;
/// A slot stayed mid-write longer than the retry limit. Retryable.
pub const TFT_ERR_SLOT_CONTENDED: tft_status = -17;
/// This handle was created before a `fork()` and is being used in the child.
pub const TFT_ERR_CHILD_DETACHED: tft_status = -18;
/// The edge's interpolation policy has no reportable derivative.
pub const TFT_ERR_NO_DERIVATIVES: tft_status = -19;
/// There is a pose at this stamp but no segment to differentiate.
pub const TFT_ERR_NO_SEGMENT: tft_status = -20;
/// A `tft_publisher` was used from a thread other than its creator's.
pub const TFT_ERR_WRONG_THREAD: tft_status = -30;
/// The path between the two frames is deeper than `TFT_MAX_DEPTH`.
pub const TFT_ERR_TREE_TOO_DEEP: tft_status = -21;
/// The compiled-against ABI version is incompatible with this library (§3.6).
pub const TFT_ERR_ABI_MISMATCH: tft_status = -6;
/// A published transform contained NaN or infinity.
pub const TFT_ERR_NOT_FINITE: tft_status = -7;
/// A published rotation is not one: a non-unit quaternion, or a matrix whose
/// determinant is not `+1` (reflected, or carrying scale).
pub const TFT_ERR_NOT_A_ROTATION: tft_status = -8;
/// Another participant already holds this edge. One writer per edge (D7).
pub const TFT_ERR_ALREADY_CLAIMED: tft_status = -31;
/// A published stamp predates the edge's newest sample.
pub const TFT_ERR_NON_MONOTONIC: tft_status = -32;
/// A reaper judged this writer dead and took the edge away. Re-claim.
pub const TFT_ERR_CLAIM_REVOKED: tft_status = -33;
/// The edge is static or tombstoned; there is nothing to publish to it.
pub const TFT_ERR_NOT_DYNAMIC: tft_status = -34;
/// The arena is mapped read-only, so nothing can be claimed for writing.
pub const TFT_ERR_READ_ONLY: tft_status = -35;
/// The operation raced another participant's protocol. Retryable, and the
/// caller's only correct response is to try again.
pub const TFT_ERR_RETRY: tft_status = -36;
/// The publisher's claim was released; claim the edge again to publish.
pub const TFT_ERR_RELEASED: tft_status = -37;
/// Something the library did not anticipate — including a caught Rust panic.
pub const TFT_ERR_INTERNAL: tft_status = -99;

/// Sentinel for an id field that does not apply to this error.
pub const TFT_INVALID_ID: u32 = u32::MAX;

/// Length of [`tft_error::message`], including the NUL.
pub const TFT_MESSAGE_LEN: usize = 256;

/// Structured detail for the most recent failure **on this thread**.
///
/// Every field that does not apply to a given error is `TFT_INVALID_ID` (ids) or
/// `0` (stamps and generations), so a caller can print the whole struct without
/// checking which variant produced it.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_camel_case_types)]
pub struct tft_error {
    /// `sizeof(tft_error)` at the time this build was compiled — the Vulkan
    /// approach to appending fields without a major version bump (§3.6).
    pub struct_size: u32,
    /// The status code this detail belongs to.
    pub code: tft_status,
    /// The offending edge, or [`TFT_INVALID_ID`].
    pub edge: u32,
    /// First frame involved, or [`TFT_INVALID_ID`].
    pub frame_a: u32,
    /// Second frame involved, or [`TFT_INVALID_ID`].
    pub frame_b: u32,
    /// The requested stamp, in nanoseconds.
    pub requested: i64,
    /// Oldest retained stamp on the offending edge.
    pub oldest: i64,
    /// Newest published stamp on the offending edge.
    pub newest: i64,
    /// Topology generation the plan was compiled against.
    pub plan_generation: u64,
    /// Current topology generation.
    pub current_generation: u64,
    /// NUL-terminated human-readable detail. Never contains a partial UTF-8
    /// sequence: it is written from ASCII only.
    pub message: [c_char; TFT_MESSAGE_LEN],
}

impl tft_error {
    const fn blank() -> tft_error {
        tft_error {
            struct_size: core::mem::size_of::<tft_error>() as u32,
            code: TFT_OK,
            edge: TFT_INVALID_ID,
            frame_a: TFT_INVALID_ID,
            frame_b: TFT_INVALID_ID,
            requested: 0,
            oldest: 0,
            newest: 0,
            plan_generation: 0,
            current_generation: 0,
            message: [0; TFT_MESSAGE_LEN],
        }
    }

    /// Copy `text` into `message`, truncating at the buffer and always leaving a
    /// NUL. ASCII only, so truncation cannot split a multi-byte sequence.
    fn set_message(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let n = bytes.len().min(TFT_MESSAGE_LEN - 1);
        for (dst, &b) in self.message.iter_mut().zip(&bytes[..n]) {
            // Non-ASCII would risk a truncated code point; substitute instead.
            *dst = if b.is_ascii() {
                b as c_char
            } else {
                b'?' as c_char
            };
        }
        self.message[n] = 0;
    }
}

thread_local! {
    /// Thread-local by design (§3.3). A process-global would need a lock on the
    /// error path and would report another thread's failure to this one, which
    /// is worse than reporting nothing.
    static LAST_ERROR: RefCell<tft_error> = const { RefCell::new(tft_error::blank()) };
}

/// Record `code` and `detail` as this thread's most recent error.
pub(crate) fn set_error(code: tft_status, message: &str, fill: impl FnOnce(&mut tft_error)) {
    LAST_ERROR.with(|slot| {
        if let Ok(mut e) = slot.try_borrow_mut() {
            *e = tft_error::blank();
            e.code = code;
            fill(&mut e);
            e.set_message(message);
        }
    });
}

/// Add detail to this thread's error **without** discarding what is already
/// there.
///
/// [`set_error`] deliberately blanks first, so a fresh error cannot inherit a
/// stale field. That makes it the wrong tool for layering: `tft_plan_at_many`
/// used it after `record_lookup` and wiped the edge id and the retained window
/// the caller needs — reported by review, and the reason this exists.
pub(crate) fn amend_error(fill: impl FnOnce(&mut tft_error)) {
    LAST_ERROR.with(|slot| {
        if let Ok(mut e) = slot.try_borrow_mut() {
            fill(&mut e);
        }
    });
}

/// Clear this thread's error slot. Called on every successful entry point so a
/// stale detail cannot be mistaken for a fresh one.
pub(crate) fn clear_error() {
    LAST_ERROR.with(|slot| {
        if let Ok(mut e) = slot.try_borrow_mut() {
            e.code = TFT_OK;
            e.message[0] = 0;
        }
    });
}

/// Copy this thread's most recent error into `out`.
///
/// # Errors
///
/// [`TFT_ERR_NULL_ARG`] if `out` is NULL, [`TFT_ERR_BAD_STRUCT_SIZE`] if
/// `out->struct_size` is not a size this build recognises.
///
/// # Safety
///
/// `out` must be NULL or point to a writable, correctly aligned `tft_error`
/// whose `struct_size` field has been initialised.
#[no_mangle]
pub unsafe extern "C" fn tft_last_error(out: *mut tft_error) -> tft_status {
    // Deliberately NOT wrapped in `guard`: it must stay callable from an error
    // path, and re-entering the error machinery to report a failure to report an
    // error is how a diagnostic surface becomes the bug.
    if out.is_null() {
        return TFT_ERR_NULL_ARG;
    }
    // SAFETY: `out` is non-null by the check above; the caller's contract
    // requires it to be writable, aligned, and to have `struct_size` set.
    let declared = unsafe { core::ptr::addr_of!((*out).struct_size).read() };
    if declared as usize != core::mem::size_of::<tft_error>() {
        return TFT_ERR_BAD_STRUCT_SIZE;
    }
    LAST_ERROR.with(|slot| match slot.try_borrow() {
        Ok(e) => {
            // SAFETY: as above, plus `tft_error` is `Copy` with no padding
            // invariants, so a bitwise write is a complete initialisation.
            unsafe { core::ptr::write(out, *e) };
            TFT_OK
        }
        Err(_) => TFT_ERR_INTERNAL,
    })
}

/// Map a `tf_tree` lookup error onto a status code, recording its detail.
pub(crate) fn record_lookup(err: LookupError) -> tft_status {
    use LookupError as L;
    match err {
        L::UnknownFrame { .. } => {
            set_error(TFT_ERR_UNKNOWN_FRAME, "no such frame in this tree", |_| {});
            TFT_ERR_UNKNOWN_FRAME
        }
        L::Disconnected { target, source, .. } => {
            set_error(
                TFT_ERR_DISCONNECTED,
                "target and source are in different connected components",
                |e| {
                    e.frame_a = target.get();
                    e.frame_b = source.get();
                },
            );
            TFT_ERR_DISCONNECTED
        }
        L::NoData { edge } => {
            set_error(TFT_ERR_NO_DATA, "the edge has no published samples", |e| {
                e.edge = edge.get();
            });
            TFT_ERR_NO_DATA
        }
        L::Extrapolation {
            edge,
            requested,
            oldest,
            newest,
        } => {
            set_error(
                TFT_ERR_EXTRAPOLATION,
                "the requested stamp is outside the edge's retained history",
                |e| {
                    e.edge = edge.get();
                    e.requested = requested;
                    e.oldest = oldest;
                    e.newest = newest;
                },
            );
            TFT_ERR_EXTRAPOLATION
        }
        L::TopologyChanged { plan, current } => {
            set_error(
                TFT_ERR_TOPOLOGY_CHANGED,
                "the topology changed since this plan was compiled; re-plan",
                |e| {
                    e.plan_generation = plan;
                    e.current_generation = current;
                },
            );
            TFT_ERR_TOPOLOGY_CHANGED
        }
        L::TimeDomainMismatch { .. } | L::MixedTimeDomains { .. } => {
            set_error(TFT_ERR_TIME_DOMAIN, "time domain mismatch", |_| {});
            TFT_ERR_TIME_DOMAIN
        }
        L::SlotRecycled { edge } => {
            set_error(
                TFT_ERR_SLOT_RECYCLED,
                "the ring lapped the reader mid-read; retry",
                |e| e.edge = edge.get(),
            );
            TFT_ERR_SLOT_RECYCLED
        }
        L::SlotContended { edge } => {
            set_error(
                TFT_ERR_SLOT_CONTENDED,
                "a slot stayed mid-write past the retry limit; retry",
                |e| e.edge = edge.get(),
            );
            TFT_ERR_SLOT_CONTENDED
        }
        L::ChildDetached => {
            set_error(
                TFT_ERR_CHILD_DETACHED,
                "this handle belongs to the parent of a fork(); open a new tree",
                |_| {},
            );
            TFT_ERR_CHILD_DETACHED
        }
        L::DerivativesUnavailable { edge, .. } => {
            set_error(
                TFT_ERR_NO_DERIVATIVES,
                "this edge's interpolation policy has no reportable derivative \
                 (declare it ScLerp)",
                |e| e.edge = edge.get(),
            );
            TFT_ERR_NO_DERIVATIVES
        }
        L::NoSegment { edge } => {
            set_error(
                TFT_ERR_NO_SEGMENT,
                "a pose exists at this stamp but no segment to differentiate",
                |e| e.edge = edge.get(),
            );
            TFT_ERR_NO_SEGMENT
        }
        L::TreeTooDeep { depth } => {
            set_error(
                TFT_ERR_TREE_TOO_DEEP,
                "the path between these frames is deeper than the engine's fixed limit",
                |e| e.requested = i64::from(depth),
            );
            TFT_ERR_TREE_TOO_DEEP
        }
        L::UnknownEdge { edge } => {
            set_error(TFT_ERR_BAD_HANDLE, "edge id names no usable edge", |e| {
                e.edge = edge.get();
            });
            TFT_ERR_BAD_HANDLE
        }
        L::FrameOutOfRange { frame } => {
            set_error(
                TFT_ERR_UNKNOWN_FRAME,
                "frame id is out of range for this arena",
                |e| {
                    e.frame_a = frame.get();
                },
            );
            TFT_ERR_UNKNOWN_FRAME
        }
        L::MissingEdge { child } => {
            set_error(
                TFT_ERR_NO_DATA,
                "the topology records a parent for this frame but no edge for the link",
                |e| e.frame_a = child.get(),
            );
            TFT_ERR_NO_DATA
        }
        L::BufferTooSmall { .. } => {
            set_error(TFT_ERR_BUFFER_TOO_SMALL, "output buffer too small", |_| {});
            TFT_ERR_BUFFER_TOO_SMALL
        }
        other => {
            set_error(TFT_ERR_INTERNAL, "unhandled lookup error", |_| {});
            let _ = other;
            TFT_ERR_INTERNAL
        }
    }
}

/// Run `body` with a panic guard, translating a panic into
/// [`TFT_ERR_INTERNAL`] instead of aborting the process (§3.4).
///
/// Clears the thread's error slot first, so a success leaves no stale detail
/// behind and a failure's detail is unambiguously the one just recorded.
///
/// `catch_unwind` is zero-cost on the non-panicking path — it emits landing pads,
/// not a runtime check — which is why §3.7's hot path can be wrapped without a
/// measurable cost. `bench/abi_cost` is the row that proves it rather than
/// asserting it.
#[inline]
pub(crate) fn guard(body: impl FnOnce() -> tft_status) -> tft_status {
    clear_error();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(payload) => {
            // The payload is whatever `panic!` was given. `&str` and `String`
            // cover every panic the standard library and this crate produce;
            // anything else still yields a status rather than an abort.
            let text = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("panic with a non-string payload");
            set_error(TFT_ERR_INTERNAL, text, |_| {});
            TFT_ERR_INTERNAL
        }
    }
}
