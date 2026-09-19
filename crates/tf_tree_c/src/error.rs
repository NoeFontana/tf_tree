//! Status codes, the thread-local error detail, and the panic guard — `docs/PHASE4.md` §3.3 and
//! §3.4.
//!
//! A status code alone would make the C++ wrapper worse than Python at reporting what went wrong
//! (D11), so it is accompanied by a thread-local [`tft_error`], formatted only on the error path. A
//! Rust panic escaping an `extern "C"` function aborts the host process, so every entry point wraps
//! its body in `guard`.

use core::cell::RefCell;
use core::ffi::c_char;

use tf_tree::LookupError;

/// `0` on success; negative on failure.
pub type tft_status = i32;

/// Success.
pub const TFT_OK: tft_status = 0;

// Status codes: stable and **append-only**.
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
///
/// From plan compilation it can also mean something else; see
/// `tft_plan_create`'s *Errors*.
pub const TFT_ERR_UNKNOWN_FRAME: tft_status = -10;
/// Target and source are in different connected components.
pub const TFT_ERR_DISCONNECTED: tft_status = -11;
/// The edge has no published samples yet.
///
/// From plan compilation it can also mean something else; see
/// `tft_plan_create`'s *Errors*.
pub const TFT_ERR_NO_DATA: tft_status = -12;
/// The requested stamp lies outside the edge's retained history.
pub const TFT_ERR_EXTRAPOLATION: tft_status = -13;
/// The topology changed since the plan was compiled; re-plan.
pub const TFT_ERR_TOPOLOGY_CHANGED: tft_status = -14;
/// The query's time domain does not match the plan's.
///
/// From plan compilation it can also mean something else; see
/// `tft_plan_create`'s *Errors*.
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
/// The path between the two frames is too long: more raw edges than a lookup will walk, or more
/// steps than a compiled plan holds once adjacent rigid links fold into one.
///
/// Two engine bounds share one status because this table is frozen (`0034`); neither is exported as
/// a macro.
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
/// Both frame names are known, but the child is attached to a **different** parent than the one
/// named. The detail carries `frame_a` = the child, `frame_b` = its actual parent.
pub const TFT_ERR_PARENT_MISMATCH: tft_status = -38;
/// The named child frame has no incoming edge at all — it is a root, or was never attached.
pub const TFT_ERR_NO_EDGE: tft_status = -39;
/// A configuration text could not be turned into a topology: it does not parse,
/// declares a cycle, or describes a tree the engine will not build. The
/// message names the line or the frame.
pub const TFT_ERR_BAD_CONFIG: tft_status = -40;
/// A `(sec, nanos)` pair is not a representable stamp: `nanos` is outside `[0, 1e9)`, or the total
/// does not fit `int64_t`.
///
/// Returned only by `tft_stamp_from_parts` and `tft_stamp_from_timespec` (a minor bump under
/// `docs/PHASE4.md` §3.6). The detail carries `requested` = seconds, `newest` = nanoseconds. Not
/// `TFT_ERR_BAD_ENUM`: this is an arithmetic refusal.
pub const TFT_ERR_BAD_STAMP: tft_status = -41;
/// A **shared** arena was asked for and could not be had: the rendezvous name is held by a live
/// arena, the runtime directory is unusable, the segment could not be created or mapped, or this
/// library was built without `--features shm`. The message says which (`docs/decisions/0015`).
///
/// Returned only by `tft_bridge_create` with a non-NULL `tft_bridge_options::arena_name` (a minor
/// bump under `docs/PHASE4.md` §3.6). There is **no fallback to a private heap arena**.
pub const TFT_ERR_ARENA_UNAVAILABLE: tft_status = -42;
/// Something the library did not anticipate — including a caught Rust panic.
pub const TFT_ERR_INTERNAL: tft_status = -99;

/// Sentinel for an id field that does not apply to this error.
pub const TFT_INVALID_ID: u32 = u32::MAX;

/// Length of [`tft_error::message`], including the NUL.
pub const TFT_MESSAGE_LEN: usize = 256;

/// Structured detail for the most recent failure **on this thread**. Fields that do not apply are
/// `TFT_INVALID_ID` (ids) or `0`.
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
    /// A well-formed all-clear `tft_error`, with `struct_size` already set. Public so callers need
    /// not `mem::zeroed()` (`docs/decisions/0048`).
    pub const fn blank() -> tft_error {
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
    // `c_char` is `i8` on x86_64 and `u8` on aarch64, so exactly one cast below is a
    // no-op per target; the allow is the fix, not a suppression.
    #[allow(clippy::unnecessary_cast)]
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
    /// Thread-local by design (§3.3): a process-global would need a lock and report another
    /// thread's failure.
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

/// Add detail to this thread's error **without** discarding what is there; [`set_error`] blanks
/// first, which is wrong for layering.
pub(crate) fn amend_error(fill: impl FnOnce(&mut tft_error)) {
    LAST_ERROR.with(|slot| {
        if let Ok(mut e) = slot.try_borrow_mut() {
            fill(&mut e);
        }
    });
}

/// This thread's most recent error message, so a caller inside this crate can quote the engine
/// rather than invent a second wording. Allocates; failure paths only.
#[cfg(feature = "bridge")]
// `c_char` signedness: see `set_message`.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn last_message() -> String {
    LAST_ERROR.with(|slot| {
        slot.try_borrow().map_or_else(
            |_| String::new(),
            |e| {
                let bytes: Vec<u8> = e
                    .message
                    .iter()
                    .take_while(|&&c| c != 0)
                    .map(|&c| c as u8)
                    .collect();
                String::from_utf8(bytes).unwrap_or_default()
            },
        )
    })
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
    // Deliberately not wrapped in `guard`: it must stay callable from an error path.
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
        // Split: only `MixedTimeDomains` knows an edge (D11). Static messages: this is the
        // `tft_plan_at` path, which does not allocate.
        L::TimeDomainMismatch { .. } => {
            set_error(TFT_ERR_TIME_DOMAIN, "time domain mismatch", |_| {});
            TFT_ERR_TIME_DOMAIN
        }
        L::MixedTimeDomains { edge, .. } => {
            set_error(
                TFT_ERR_TIME_DOMAIN,
                "the path's dynamic edges publish in different time domains",
                |e| e.edge = edge.get(),
            );
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
            // `requested` carries `depth`, the count that overran one of two bounds (`0034`); see
            // `TFT_ERR_TREE_TOO_DEEP`.
            set_error(
                TFT_ERR_TREE_TOO_DEEP,
                "the path between these frames overruns one of the engine's two length bounds",
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

/// Run `body` with a panic guard, returning `fallback` if it panics.
///
/// For the `extern "C"` entry points that return a count, a size or nothing, which [`guard`] does
/// not fit (§6's checklist). No error is recorded: there is no status to carry it. `fallback` reads
/// as "nothing to report".
#[inline]
pub(crate) fn guard_value<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or(fallback)
}

/// Run `body` with a panic guard, translating a panic into [`TFT_ERR_INTERNAL`] instead of aborting
/// the process (§3.4). Clears the thread's error slot first.
///
/// `catch_unwind` is zero-cost on the non-panicking path; `bench/abi_cost` measures it.
#[inline]
pub(crate) fn guard(body: impl FnOnce() -> tft_status) -> tft_status {
    clear_error();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(payload) => {
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

#[cfg(test)]
mod tests {
    use super::{
        record_lookup, tft_error, LAST_ERROR, TFT_ERR_TIME_DOMAIN, TFT_INVALID_ID, TFT_MESSAGE_LEN,
    };
    use tf_tree::{EdgeId, LookupError};

    fn last() -> tft_error {
        LAST_ERROR.with(|slot| *slot.borrow())
    }

    /// A path whose dynamic edges disagree names the edge that disagreed; a query in the wrong
    /// domain has no edge to name and reports `TFT_INVALID_ID`.
    ///
    /// **Mutant:** delete `|e| e.edge = edge.get()` from the `MixedTimeDomains` arm; the test
    /// fails.
    ///
    /// **Mutant:** in the `TimeDomainMismatch` arm, `set_error(..)` → `amend_error(|e| e.code =
    /// TFT_ERR_TIME_DOMAIN)`; the last assertion fails.
    #[test]
    fn mixed_time_domains_names_its_edge() {
        let rc = record_lookup(LookupError::MixedTimeDomains {
            edge: EdgeId(7),
            expected: 1,
            got: 0,
        });
        assert_eq!(rc, TFT_ERR_TIME_DOMAIN);
        let e = last();
        assert_eq!(e.code, TFT_ERR_TIME_DOMAIN);
        assert_eq!(e.edge, 7);

        record_lookup(LookupError::TimeDomainMismatch {
            expected: 1,
            got: 0,
        });
        assert_eq!(last().edge, TFT_INVALID_ID);
    }

    /// The buffer `tf_tree_ipc`'s message budget is derived from (`0055` step 6).
    ///
    /// That crate's `MESSAGE_BUDGET` is 255 usable bytes minus the longest fixed text a C path
    /// puts before a rendering (`bridge::generic_failure_message`'s prefix), and cannot see these
    /// constants. Pinned here: the buffer size, the truncation bound and the `?` substitution
    /// (one em-dash costs three bytes).
    // `c_char` signedness again: `c as u8` is a no-op on exactly one target.
    #[allow(clippy::unnecessary_cast)]
    #[test]
    fn the_message_buffer_is_the_size_this_crates_budget_assumes() {
        assert_eq!(
            TFT_MESSAGE_LEN, 256,
            "tf_tree_ipc's MESSAGE_BUDGET is derived from this; move both together"
        );

        let mut long = last();
        long.set_message(&"x".repeat(300));
        let kept = long.message.iter().take_while(|&&c| c != 0).count();
        assert_eq!(
            kept,
            TFT_MESSAGE_LEN - 1,
            "a 300-byte message must keep exactly {} bytes and then NUL; \
             tf_tree_ipc's budget subtracts the wrapper from this number",
            TFT_MESSAGE_LEN - 1
        );

        let mut e = last();
        e.set_message("a\u{2014}b");
        let rendered: Vec<u8> = e
            .message
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        assert_eq!(
            String::from_utf8_lossy(&rendered),
            "a???b",
            "one em-dash must cost three bytes and render as ???, which is what the \
             ASCII half of tf_tree_ipc's gate is for"
        );
    }
}
