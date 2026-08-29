//! The `tf_tree` C ABI — `docs/PHASE4.md` §3.
//!
//! # SAFETY (module-level, per `docs/decisions/0007`)
//!
//! This crate is one of the four boundaries where `unsafe` is permitted: **a
//! foreign caller**. Every `extern "C"` function receives pointers the compiler
//! cannot reason about, and the `unsafe` here does exactly one job — **turning
//! those pointers into Rust references, once, at the entry point.** Past that
//! check every body is safe code.
//!
//! Three rules make that auditable, and every `unsafe` block below relies on
//! them:
//!
//! 1. **Handles carry a magic word, and its guarantee is narrower than it
//!    looks.** Every handle begins with a magic word written by its constructor
//!    and zeroed by its destructor (§3.2). What that actually buys, stated
//!    precisely because an earlier version of this comment overpromised and the
//!    overpromise was itself Undefined Behaviour:
//!
//!    * **NULL is rejected.** Always, and before anything is read.
//!    * **Handle-type confusion is rejected.** Passing a `tft_plan*` where a
//!      `tft_tree*` is expected reads a live, aligned, sufficiently large
//!      allocation and finds the wrong magic. This is the common mistake and it
//!      is caught.
//!    * **An arbitrary foreign pointer is *not* reliably rejected, and cannot
//!      be.** Reading 8 bytes through a pointer about which nothing is known is
//!      UB in Rust regardless of what the bytes turn out to be — it may be
//!      misaligned, or 4 bytes from the end of a smaller object. The read is
//!      [`core::ptr::read_unaligned`] so alignment at least is not a hazard,
//!      but no check can make an out-of-bounds read well-defined. **The caller's
//!      contract is that the pointer is NULL or a handle**; the magic word is
//!      defence in depth against a plausible mistake, not a validator for
//!      arbitrary memory.
//!    * **Use-after-free is best-effort.** The magic is zeroed before the
//!      allocation is released, so a second free is a no-op *while the memory is
//!      untouched*. Once the allocator reuses it, nothing can help — and reading
//!      it at all is a use-after-free that ASan will (correctly) flag, which is
//!      why there is no test for it.
//! 2. **`#![deny(unsafe_op_in_unsafe_fn)]`.** An `unsafe fn` confers no
//!    permission on its body, so each dereference carries its own `// SAFETY:`
//!    naming what it relies on.
//! 3. **This crate depends on `tf_tree`, not `tf_tree_core`.** It goes through
//!    the same safe facade every other consumer does, so the C ABI cannot reach
//!    an invariant the Rust API protects — notably the fork-generation and
//!    detach checks a C caller is least equipped to reproduce.
//!
//! # Panics cannot escape
//!
//! Since Rust 1.81 a panic crossing an `extern "C"` boundary aborts the process.
//! Every entry point wraps its body in `error::guard`, which converts a panic
//! into `TFT_ERR_INTERNAL` with the payload in the thread-local detail. The one
//! deliberate exception is [`tft_last_error`], which must stay callable from an
//! error path.
//!
//! # Naming
//!
//! The `#[allow(non_camel_case_types)]` below is not laziness: these type names
//! appear verbatim in `tf_tree.h`, and `cbindgen` reproduces them as written. A
//! Rust-idiomatic name here would produce a C name no C programmer expects.

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

// ---------------------------------------------------------------------------
// ABI version — §3.6
// ---------------------------------------------------------------------------

/// Major ABI version. **Must match exactly** between the header a caller
/// compiled against and the library it links.
pub const TFT_ABI_VERSION_MAJOR: u32 = 0;
/// Minor ABI version. The runtime's may be **≥** the compiled-against value.
///
/// `1` → `2`: the §5 bridge seam gained an entry point
/// (`tft_bridge_note_time_jump`), a field appended to `tft_bridge_sample`
/// (`received_steady_nanos`), and three appended to `tft_bridge_outcome`
/// (`delta_nanos`, `clock_evidence`, `clock_evidence_detail`). **Every one of
/// them is an append**, which is exactly what a minor bump means under §3.6 —
/// no existing field moved, changed type, or changed meaning, so a caller built
/// against `0.1` reads the same bytes out of the same offsets.
///
/// The rule finally has an implementation behind it as well as a sentence:
/// `tft_bridge_offer` reads a caller's shorter `tft_bridge_sample` as the prefix
/// it is, instead of refusing it with `TFT_ERR_BAD_STRUCT_SIZE`. Until that
/// landed, appending a field would have locked every older caller out of every
/// call — which is the precise case §3.6 exists to prevent.
///
/// `2` → `3`: one appended `tft_layout` enumerator,
/// [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`], carrying `at_with_derivatives` as a layout
/// (`docs/API.md` §3.3) — accepted by [`tft_plan_at`] and [`tft_plan_at_many`],
/// which is why it is in the frozen header rather than the unstable one. §3.6
/// names this case explicitly: an older caller never spells the new value, and
/// every entry point that takes a `tft_layout` rejects a discriminant it does
/// not define rather than computing a size from it — so a `0.2` caller against
/// a `0.3` library is unchanged in every byte it can observe. No struct grew,
/// nothing moved, and no existing enumerator changed meaning.
///
/// `3` → `4`: two appended entry points, [`tft_stamp_from_parts`] and
/// [`tft_stamp_from_timespec`] (`docs/API.md` §5.1), and the one status code
/// they can return, [`TFT_ERR_BAD_STAMP`]. **This is the additive case §3.6's
/// rule is for, and it is worth stating why a new *function* is a minor bump
/// rather than no bump at all**: the minor is exactly the number a caller
/// compares to find out whether the symbols its header declares are present in
/// the library it linked. Adding a symbol without moving it would let a caller
/// compiled against this header link against a `0.3` library, pass
/// `tft_check_abi`, and then fail at the dynamic loader — or, on a static link,
/// not build. Nothing existing moved, changed type or changed meaning, so the
/// major does not move.
///
/// `TFT_ERR_BAD_STAMP` rides along for the reason its own documentation gives:
/// only the two new functions return it, so a `0.3` caller cannot receive a
/// code it cannot name.
///
/// `4` → `5`: one appended field on `tft_bridge_options`, `arena_name`
/// (`docs/decisions/0015`), and the one status code it can produce,
/// [`TFT_ERR_ARENA_UNAVAILABLE`]. The append is a minor bump on the same terms
/// as `1` → `2`'s: nothing moved, changed type or changed meaning, and
/// `tft_bridge_create` now reads a shorter `tft_bridge_options` as the prefix it
/// is instead of refusing it — the §3.6 rule that had, until this bump, an
/// implementation for `tft_bridge_sample` alone.
///
/// **The new status code's argument is tighter than `TFT_ERR_BAD_STAMP`'s.**
/// That one rests on an older caller never *calling* the two new functions;
/// this one rests on an older caller being unable to *express* the request. The
/// code is reachable only when `arena_name` is non-NULL, and a caller whose
/// `struct_size` names the `0.4` layout has no such field — its bytes end where
/// the field begins, and `read_options` zero-fills the rest. So a `0.4` caller
/// provably cannot receive it, rather than merely being expected not to.
///
/// `5` → `6`: one appended entry point, [`tft_plan_create_in_domain`]
/// (`docs/decisions/0038`). It is `3` → `4`'s case exactly — a new *symbol*, so
/// the minor has to move or a caller compiled against this header links a `0.5`
/// library, passes `tft_check_abi`, and then fails at the loader. Nothing
/// moved, changed type or changed meaning: [`tft_plan_create`] keeps its
/// signature and its meaning, which `0038` defines as this function with
/// `domain = 0`.
///
/// **What a `0.5` caller can observe is a refusal arriving earlier**, and only
/// on an arena where it was already receiving that refusal. On a tree whose
/// dynamic edges carry a non-zero domain, `tft_plan_create` used to return
/// `TFT_OK` and then answer [`TFT_ERR_TIME_DOMAIN`] to every lookup for the
/// life of the plan; it now returns that same code from the plan call. No
/// program that got an answer before stops getting one — there was no such
/// program, which is the defect `0038` exists to fix — and the alternative
/// (leaving the check to the hot loop) throws away the frame names.
///
/// `6` → `7`: one appended entry point, [`tft_plan_at_extrapolating`], with
/// the two values it needs — [`tft_extrap_policy`] and [`tft_extrapolated`]
/// (`docs/decisions/0039`). A new *symbol*, so `3` → `4`'s argument applies
/// unchanged: the minor is what a caller compares to find out whether the
/// symbols its header declares are present in the library it linked, and
/// without the bump a caller compiled against this header links a `0.6`
/// library, passes [`tft_check_abi`], and then fails at the loader.
///
/// **No existing declaration moves, and no status code is added.** The
/// refusal a caller can now ask *not* to receive, [`TFT_ERR_EXTRAPOLATION`],
/// has been in this header since 1.0 — which is what keeps this bump smaller
/// than `4` → `5`'s: there is no code an older caller could be handed and
/// could not name. [`tft_plan_at`] keeps its signature and its meaning; it
/// refuses, as it always has, and this function with [`TFT_EXTRAP_ERROR`] is
/// that same refusal with a distance attached on success.
///
/// **A `0.6` caller can observe nothing at all.** Unlike `5` → `6`, which
/// moved a refusal earlier on arenas that were already failing, nothing here
/// changes the behaviour of any call that existed before: the new policy is
/// reachable only through a symbol an older caller cannot name.
pub const TFT_ABI_VERSION_MINOR: u32 = 7;

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

/// Check the header a caller compiled against against the library they linked.
///
/// §3.6 states the rule — **major must match exactly; the runtime minor may be
/// ≥ the compiled-against minor** — and until this existed nothing enforced it.
/// Two getters let a caller *implement* the rule; only one of them will, and the
/// one who does not is the one who needs it.
///
/// Call it as `tft_check_abi(TFT_ABI_VERSION_MAJOR, TFT_ABI_VERSION_MINOR)`
/// using the constants **from the header**, so the arguments are baked in at the
/// caller's compile time and the comparison is genuinely between two builds. The
/// C++ wrapper does this in a static initializer (§3.6); a C caller should do it
/// once at startup.
///
/// # Errors
///
/// [`TFT_ERR_ABI_MISMATCH`], with both version pairs in the error detail:
/// `frame_a`/`frame_b` carry the caller's major/minor, `plan_generation` and
/// `current_generation` the library's. The message names all four, because a
/// silently mismatched ABI is a debugging session nobody deserves.
#[no_mangle]
pub extern "C" fn tft_check_abi(compiled_major: u32, compiled_minor: u32) -> tft_status {
    guard(|| {
        if compiled_major == TFT_ABI_VERSION_MAJOR && compiled_minor <= TFT_ABI_VERSION_MINOR {
            return TFT_OK;
        }
        // The message is built from a fixed set of small integers, so this
        // allocates only on the failure path — which ends the process anyway,
        // in every caller that uses this correctly.
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

// ---------------------------------------------------------------------------
// Stamps — `docs/API.md` §5.1
// ---------------------------------------------------------------------------

/// Assemble a stamp from a `(sec, nanos)` pair, exactly — `docs/API.md` §5.1.
///
/// The C spelling of `Stamp::from_parts`, and it refuses exactly what that
/// refuses. This is the shape a ROS 2 `builtin_interfaces/Time` already has
/// (`{int32 sec, uint32 nanosec}`), so the conversion users resent writing in
/// every node — `stamp.sec * 1000000000 + stamp.nanosec` — becomes one call
/// that cannot overflow silently.
///
/// **No float, on any surface** (R3). The ecosystem already agrees with int64
/// nanoseconds; accepting a double here would not recover precision a driver had
/// already destroyed, only move the blame.
///
/// # Why it returns a status and not the stamp
///
/// Because two inputs have no correct answer and both plausible alternatives are
/// silently wrong. Normalising an out-of-range `nanos` turns a malformed message
/// into a well-formed stamp; wrapping an out-of-range sum hands back a stamp on
/// the other side of the epoch that compares, interpolates and prints perfectly.
/// There is no sentinel `int64_t` to return instead — every value is a legal
/// stamp — so the refusal has to be the return value and the answer has to be an
/// out-parameter.
///
/// # Errors
///
/// [`TFT_ERR_NULL_ARG`] if `out` is NULL. [`TFT_ERR_BAD_STAMP`] if `nanos` is
/// outside `[0, 1e9)` or the sum does not fit `int64_t`; `*out` is not written
/// in either case.
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
        // SAFETY: `out` is non-null by the check above and the caller contracts
        // it writable. Written only after the conversion succeeded, so a refused
        // call leaves the caller's variable as it was.
        unsafe { core::ptr::write(out, s.nanos()) };
        TFT_OK
    })
}

/// Assemble a stamp from the two fields of a POSIX `struct timespec`.
///
/// `tft_stamp_from_timespec(ts.tv_sec, ts.tv_nsec, &out)` — the fields rather
/// than the struct, because `tf_tree_core`'s dependency budget has no `libc` in
/// it and declaring our own `#[repr(C)]` copy would be a type the caller then
/// has to convert *into*, which is the conversion this exists to remove.
/// `time_t` and `long` are both `int64_t` on every 64-bit target, so there is no
/// cast at the call site.
///
/// # Errors
///
/// Everything [`tft_stamp_from_parts`] refuses, plus a **negative `tv_nsec`**.
/// POSIX permits one only in a *relative* `timespec` — an interval handed to
/// `nanosleep` — so a negative field means an interval is being converted as an
/// instant, which is the mistake this refusal catches.
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

/// The one refusal both stamp converters raise, with the offending pair in the
/// detail.
///
/// `requested` carries the seconds and `newest` the nanoseconds — both are plain
/// `int64_t` detail fields with no other meaning on this path, and an operator
/// reading `TFT_ERR_BAD_STAMP` wants to know *which* pair was rejected far more
/// than they want a second message string.
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

// ---------------------------------------------------------------------------
// Handles — §3.2
// ---------------------------------------------------------------------------

/// Magic words. Distinct per type, so passing a `tft_plan*` where a `tft_tree*`
/// is expected is caught rather than followed into a type confusion.
const MAGIC_TREE: u64 = 0x7446_5F54_5245_4531; // "tFT_TREE1"-ish
const MAGIC_PLAN: u64 = 0x7446_5F50_4C41_4E31;

/// An opaque handle to a transform tree. `Send + Sync`: shareable across threads.
///
/// `#[repr(C)]` is load-bearing, not decoration: `check_tree` validates the
/// magic word through a field projection, and under the default `repr(Rust)` the
/// compiler is free to reorder fields — so "the magic is first" would be an
/// assumption that happens to hold today and could stop holding on a compiler
/// upgrade, silently turning every handle check into a read of an `Arc` pointer.
/// **The generated header declares this as an incomplete type.** §3.2 says these
/// are opaque handles, and a C caller who can see the fields can dereference
/// them. `cbindgen`'s `cbindgen:opaque` annotation does not take effect on this
/// shape, so `xtask headers` excludes the type and emits the forward
/// declaration itself — which also satisfies §3.1's requirement that the stable
/// header be reviewed by hand rather than merely generated.
#[repr(C)]
pub struct tft_tree {
    magic: u64,
    share: Arc<TreeShare>,
}

/// An opaque handle to a compiled plan. `Send + Sync`, immutable.
///
/// `#[repr(C)]` for the same reason as [`tft_tree`].
/// **The generated header declares this as an incomplete type.** §3.2 says these
/// are opaque handles, and a C caller who can see the fields can dereference
/// them. `cbindgen`'s `cbindgen:opaque` annotation does not take effect on this
/// shape, so `xtask headers` excludes the type and emits the forward
/// declaration itself — which also satisfies §3.1's requirement that the stable
/// header be reviewed by hand rather than merely generated.
#[repr(C)]
pub struct tft_plan {
    magic: u64,
    plan: tf_tree::Plan,
    /// **Why the plan owns a share of the tree rather than a pointer to it.**
    ///
    /// A C caller has no borrow checker, frees in whatever order it likes, and
    /// the *natural* order — free the tree, then its plans — is exactly the one
    /// that would dangle. A raw `*const tft_tree` here would turn a reasonable
    /// C idiom into a use-after-free, so the plan holds a refcounted share and
    /// `tft_tree_free` merely drops one reference.
    share: Arc<TreeShare>,
    /// The time domain every evaluate call on this handle asks the engine
    /// about — `docs/decisions/0038`.
    ///
    /// **On the handle rather than on each call**, for three reasons in that
    /// record's order: the ABI is frozen, so a per-call tag would mean a new
    /// spelling of three functions instead of one; a domain is a property of a
    /// route through the tree rather than of an instant, so it cannot
    /// legitimately vary between two queries on one plan; and plan time is
    /// where the *frame names* are still in hand, so the mismatch can name the
    /// route that disagreed. Set and validated by
    /// [`tft_plan_create_in_domain`]; [`tft_plan_create`] sets `0`.
    domain: u8,
}

/// The tree, shared between its own handle and every plan compiled from it.
///
/// # Why the `Tree` is itself behind an `Arc`
///
/// [`tf_tree::Tree::claim_owned`] takes `self: &Arc<Tree>` — the `Arc` *is* the
/// safety argument for the `'static` writer it returns
/// (`docs/decisions/0017`), so a claim cannot be taken from a `Tree` that is
/// merely a field of some other refcounted thing. This crate used to answer
/// that with its own `extend_to_static`, which `0017` step 7 deletes; holding
/// the `Arc` the facade asks for is what replaces it.
///
/// The two refcounts are not redundant. `Arc<TreeShare>` is the *handle*
/// refcount — a `tft_tree` and every `tft_plan` compiled from it share one, so
/// a C caller may free them in any order — and it will grow siblings of `tree`
/// as the handle acquires state. `Arc<Tree>` is the *arena* refcount, and it is
/// the one a publisher or a bridge writer holds after every handle is gone.
///
/// **The cost is one extra dependent load on `h.share.tree`**, which the read
/// path takes before `guard()`. It was not measured; it is a pointer chase into
/// an allocation the same call just touched, against a `tft_plan_at` that then
/// does a seqlock read and a fold.
pub(crate) struct TreeShare {
    pub(crate) tree: Arc<Tree>,
}

/// Generate a magic-word validator that reads the field **by name**.
///
/// Two deliberate choices, both of which were wrong in an earlier revision:
///
/// * **By name, not `cast::<u64>()`.** The projection is correct whatever the
///   layout turns out to be, and it means `magic` is genuinely *read* so
///   `dead_code` keeps watching it. The first version cast to `*const u64` and
///   assumed offset zero, which `repr(Rust)` does not promise — hence the
///   `#[repr(C)]` on the handle types as well.
/// * **[`core::ptr::read_unaligned`], not `read`.** A caller that passes a
///   pointer to something that is not a handle at all — the case the check
///   exists to catch — offers no alignment guarantee, and an aligned read there
///   is Undefined Behaviour *even when the value read would have failed the
///   comparison*. Miri caught exactly that on this crate's own test.
///
/// `addr_of!` is used rather than `&(*p).magic` so no reference to a
/// possibly-invalid handle is ever created.
///
/// **This is not a validator for arbitrary memory** — see the module docs. It
/// cannot be: an out-of-bounds read stays out of bounds however it is spelled.
macro_rules! magic_check {
    ($name:ident, $ty:ty, $magic:expr) => {
        /// # Safety
        ///
        /// `p` must be NULL, or point to at least `size_of::<u64>()` readable
        /// bytes at the offset of the handle's `magic` field. Every handle this
        /// crate hands out satisfies that, as does any other live handle type
        /// (which is what makes type-confusion detection work).
        ///
        /// Passing a pointer to an unrelated, smaller object is Undefined
        /// Behaviour and no amount of checking here can change that.
        #[inline]
        pub(crate) unsafe fn $name(p: *const $ty) -> bool {
            if p.is_null() {
                return false;
            }
            // SAFETY: `p` is non-null and, per the contract above, has at least
            // eight readable bytes at the magic field's offset. `read_unaligned`
            // so a caller's misaligned handle-shaped pointer is not additionally
            // UB; the projection reads only the magic field.
            unsafe { core::ptr::addr_of!((*p).magic).read_unaligned() == $magic }
        }
    };
}

magic_check!(check_tree, tft_tree, MAGIC_TREE);
magic_check!(check_plan, tft_plan, MAGIC_PLAN);

/// Wrap a share of an already-built tree in a fresh, independently owned handle.
///
/// The bridge builds the arena and then has to hand the node something it can
/// read through. Handing out a *pointer into* the bridge would make the reader's
/// lifetime the writer's problem in a language with no borrow checker; handing
/// out another refcounted handle makes free order irrelevant, which is the same
/// trade [`tft_plan::share`] already makes.
#[cfg(feature = "bridge")]
pub(crate) fn tree_handle(share: Arc<TreeShare>) -> Box<tft_tree> {
    Box::new(tft_tree {
        magic: MAGIC_TREE,
        share,
    })
}

// ---------------------------------------------------------------------------
// Lifecycle — §3.2
// ---------------------------------------------------------------------------

/// Join the running arena named by the environment, read-only.
///
/// Mirrors `tf_tree::open()`: `$TF_TREE_DOMAIN`, `$TF_TREE_NAME` and
/// `$TF_TREE_RUNTIME_DIR` select which arena, and the attach is **read-only**
/// (D18) — a diagnostic or consumer process linked against this ABI cannot
/// corrupt a robot's transform tree, and the MMU is what enforces that rather
/// than our own care.
///
/// On success `*out` receives a handle the caller must pass to
/// [`tft_tree_free`] exactly once.
///
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
                // SAFETY: `out` is non-null by the check above and the caller
                // contracts that it is writable.
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

/// Release a tree handle. Freeing NULL is a no-op.
///
/// Any plan compiled from this tree stays valid: the underlying tree is
/// refcounted and this drops one reference (see `tft_plan::share`).
///
/// # Safety
///
/// `tree` must be NULL or a handle from a `tft_tree_*` constructor that has not
/// already been freed. Double-free is undefined; the magic word catches it in
/// every case that leaves the allocation intact, but not after the allocator has
/// reused the memory.
#[no_mangle]
pub unsafe extern "C" fn tft_tree_free(tree: *mut tft_tree) {
    if tree.is_null() {
        return;
    }
    // SAFETY: validated before the box is reconstituted.
    if !unsafe { check_tree(tree) } {
        return;
    }
    // Zero the magic *before* dropping, so a racing or repeated free sees a dead
    // handle rather than following a freed `Arc`.
    // SAFETY: `check` confirmed this is a live `tft_tree`.
    unsafe { core::ptr::write(tree.cast::<u64>(), 0) };
    // SAFETY: the handle was produced by `Box::into_raw` in a constructor above.
    drop(unsafe { Box::from_raw(tree) });
}

/// Compile a plan for `target <- source`, by frame name.
///
/// Plan compilation walks the topology once; evaluating the result is the hot
/// path (D3). A C caller should compile once and evaluate many times, exactly as
/// a Rust one would.
///
/// **This is [`tft_plan_create_in_domain`] with `domain = 0`**, which is the tag
/// a real-time tree publishes in and therefore the right call for most arenas.
/// On an arena whose dynamic edges carry any other tag — a simulated tree, which
/// `docs/PHASE4.md` §5.5 tells an operator to configure — it now returns
/// [`TFT_ERR_TIME_DOMAIN`] here instead of on every lookup afterwards. That is
/// the same refusal moved earlier, not a new one: before `docs/decisions/0038`
/// such a plan compiled and then failed every single evaluate call, with no
/// argument a C caller could pass to say otherwise.
///
/// # Safety
///
/// `tree` must be a live handle. `target` and `source` must be NUL-terminated
/// UTF-8. `out` must be NULL or point to a writable `*mut tft_plan`.
#[no_mangle]
pub unsafe extern "C" fn tft_plan_create(
    tree: *const tft_tree,
    target: *const c_char,
    source: *const c_char,
    out: *mut *mut tft_plan,
) -> tft_status {
    // SAFETY: every pointer is forwarded unchanged under the identical
    // contract, and `0` is the domain this entry point has always meant.
    unsafe { tft_plan_create_in_domain(tree, target, source, 0, out) }
}

/// Compile a plan for `target <- source` that will be queried in time domain
/// `domain`.
///
/// The domain a binding could not name (`docs/decisions/0038`).
/// [`tf_tree::Domain`] is an **open trait** — `SystemDomain` through
/// `SteadyDomain` hold `0`–`3` and a driver
/// with a PTP-disciplined clock declares its own tag from `4` upwards
/// (`docs/API.md` §2.5) — so a foreign caller can neither enumerate the domains
/// it may be asked about nor name the type it would have to instantiate. It
/// carries the tag as data instead, and the engine's tagged entry points do the
/// comparison they always did.
///
/// Pass the integer the publisher configured. `0` is [`tft_plan_create`].
///
/// # The check is here, and it is not removed from the lookup
///
/// A mismatch is reported once, at plan time, while the frame *names* are still
/// in hand — instead of on every lookup in a hot loop, where the engine can
/// only say which two integers disagreed. Every evaluate entry point still
/// passes this handle's tag to the engine on every call and the engine still
/// compares it: there is no "already checked" fast path, which would be the
/// footgun `0038` exists to remove rather than a smaller version of it.
///
/// # Errors
///
/// Everything [`tft_plan_create`] returns, plus [`TFT_ERR_TIME_DOMAIN`] when
/// this route has a dynamic edge and that edge's tag is not `domain`. `*out` is
/// not written and no handle is created.
///
/// **That condition is the engine's, spelled the same way**, and the equality
/// matters more than it looks. `0038` §4 says the check moves rather than
/// changes: `Plan::check_domain_tag` fires on `has_dynamic() && domain !=
/// self.domain`, so anything refused here is refused by every lookup and
/// anything accepted here is accepted by every lookup. Neither direction is
/// free to drift.
///
/// * Refuse *more* than the engine and a **static** route becomes unreadable —
///   `tf_tree::Plan::domain` reports `0` for a route with no dynamic edge on
///   it, so a bare `domain != plan.domain()` would reject `base -> sensor` for
///   any caller holding one non-zero tag across a whole arena, a lookup the
///   engine serves and a route the caller cannot know is static in advance.
/// * Refuse *less* and the diagnostic silently degrades: a route whose dynamic
///   edges are tag `0`, asked about in domain `1`, would compile and then fail
///   every evaluate call with only two integers to show for it. `plan.domain()`
///   cannot tell "all static" from "dynamic, tag 0" on its own — this asks
///   [`tf_tree::Plan::steps`] whether any [`tf_tree::Step::Dyn`] is present,
///   which is the same question `has_dynamic` answers, and pays for it once per
///   plan rather than once per lookup.
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
                // The whole point of validating here rather than per lookup:
                // `t` and `s` are the names the caller typed, and after this
                // function returns nothing on either side of the ABI has them.
                //
                // `Plan::has_dynamic` is private, and `plan.domain()` alone
                // cannot stand in for it — it is `0` both for "all static" and
                // for "dynamic, tag 0". So reconstruct the engine's own
                // predicate from `steps()`, which is public and whose `Step`
                // is deliberately not `#[non_exhaustive]` for exactly this
                // ("which edges does this plan sample?"). Scanning at most
                // MAX_DEPTH steps once per plan is off the hot path; getting
                // the condition merely *close* is not, in either direction —
                // see this function's *Errors*.
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

/// Release a plan handle. Freeing NULL is a no-op.
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

// ---------------------------------------------------------------------------
// Hot path — §3.7
// ---------------------------------------------------------------------------

/// Evaluate `plan` at `stamp`, writing the result into `out` in `layout`.
///
/// `out` must have room for at least `tft_layout_size(layout)` bytes.
///
/// **On a hot path, prefer [`tft_plan_at_many`].** The C signature has nowhere
/// to keep a `Guard` between calls, so this one builds a fresh one per lookup
/// and the batch entry point pays it once per call instead: 261 ns/element
/// against 302 on the depth-3 fixture at n = 256 (`docs/decisions/0022`, whose
/// implementation plan asks for this pointer in both headers — batching is the
/// whole of the available win, and a reader who never finds `tft_plan_at_many`
/// is the only way that decision goes wrong).
///
/// The plan is evaluated in the domain it was compiled for
/// ([`tft_plan_create_in_domain`]); the tag is on the handle, not on this call.
///
/// # `TFT_LAYOUT_QVEC7_WXYZ_TWIST6`
///
/// Asking for that layout *is* asking for derivatives: the plan is evaluated
/// with them and thirteen `f64` are written, pose then body twist. It is
/// therefore the one layout this function can fail on for a reason the others
/// cannot — `TFT_ERR_NO_DERIVATIVES` when an edge on the path interpolates with
/// `LerpSlerp`, `TFT_ERR_NO_SEGMENT` when it has a pose at this stamp but no
/// segment to differentiate. Nothing is written in either case.
///
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
        // **`_tagged`, and the tag is the handle's** (`docs/decisions/0038`).
        // The typed form would hard-code `SystemDomain::TAG`, which is what made
        // every lookup on a simulated arena fail permanently: `Domain` is an
        // open trait, so there is no type this file can name that stands for
        // the caller's domain. The check itself is unchanged — same condition,
        // same `TimeDomainMismatch` — only where the tag comes from.
        // Two evaluations, chosen once from the layout. A twist layout cannot
        // be served by `plan.at`, and a pose layout must not pay for the
        // adjoint chain `at_with_derivatives` runs per plan step.
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

/// [`tft_plan_at`]'s body with the panic guard removed, and **nothing else
/// changed**.
///
/// The read-path twin of [`tft_test_push_unguarded`], and it exists for the same
/// reason: so `examples/abi_cost.rs` can price `catch_unwind` on a **real,
/// non-inlinable** call by subtraction rather than by inference.
/// `tft_guarded_noop` cannot do that job — its body is small enough for rustc to
/// inline across the crate boundary, so it measures inlined code and answers a
/// different question.
///
/// This matters now because `docs/PHASE4.md` §7 gate criterion 1 is **failing**
/// (1.34–1.46× against a 1.05 gate) and only about half the gap is attributed:
/// the per-call `Guard` explains ~35 ns of ~60. `docs/decisions/0022`'s
/// implementation plan said to price the remainder *before* designing a
/// `tft_guard` handle around it, and this is what made that possible. It was
/// priced (that record's amendment 3: no unexplained residue) and **the handle
/// was then declined** — the answer to the per-call guard is
/// [`tft_plan_at_many`], one guard per batch. This hook keeps its job: R2 in
/// `abi_cost.rs` is a standing gate, not a one-off investigation.
///
/// Behind `test-hooks`, which nothing that ships enables.
///
/// # Safety
///
/// Identical to [`tft_plan_at`]: `plan` must be a live handle from
/// `tft_plan_create`, and `out` must point to at least the layout's payload size
/// in writable bytes. **Unlike `tft_plan_at` it does not catch unwinds**, so a
/// panic crossing this boundary is undefined behaviour rather than
/// `TFT_ERR_INTERNAL`. That is the whole point, and it is why this is not a
/// shipped entry point.
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
    // Tagged, as `tft_plan_at` is: this body must stay that body with nothing
    // but the guard removed, or the subtraction it exists for measures two
    // differences instead of one.
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

/// Evaluate `plan` at `n` stamps, writing each result `out_stride_bytes` apart.
///
/// `out_stride_bytes == 0` means tightly packed. A stride larger than the
/// payload writes directly into an array of caller structs — §4.3 is why this
/// parameter exists at all (`Sophus::SE3d` is usually *not* tightly packed).
///
/// # Partial writes
///
/// Evaluation stops at the first stamp that fails, and the elements already
/// written stay written — a batch is not a transaction. `tft_last_error`'s
/// `frame_b` carries the index that failed, so a caller knows exactly how many
/// leading elements are live. Only the argument checks (NULL, stride, overflow,
/// an unknown layout) are all-or-nothing.
///
/// # `TFT_LAYOUT_QVEC7_WXYZ_TWIST6`
///
/// Accepted here as it is by [`tft_plan_at`], and with the same meaning: each
/// element is thirteen `f64`, pose then body twist, evaluated with derivatives.
/// `TFT_ERR_NO_DERIVATIVES` is a property of an *edge*, so it fires on the
/// first element and leaves the buffer untouched; `TFT_ERR_NO_SEGMENT` depends
/// on the stamp and can fire part-way through.
///
/// **Sort your stamps.** This layout is evaluated by the engine's batch fold,
/// which rides a resumable cursor per plan step when the stamps are
/// non-decreasing — an `O(1)` amortized bracket search instead of `O(log n)` per
/// stamp per step. Unsorted stamps get the same answers and pay the searches.
/// A tightly packed `out` (`out_stride_bytes` of `0` or 104, `f64`-aligned) is
/// written in place with no intermediate copy; any other stride is evaluated in
/// chunks and scattered, which restarts the cursor once per chunk.
///
/// # Safety
///
/// `plan` must be a live handle. `stamps` must point to `n` readable `int64_t`.
/// `out` must point to at least `n * stride` writable bytes, where `stride` is
/// `out_stride_bytes` or the layout's payload size when that is zero.
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
        // Zero elements is a no-op, not an error, and must be handled before the
        // NULL checks: a caller looping over an empty set legitimately passes
        // NULL for both pointers.
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
        // The last element occupies `payload` bytes at offset `(n-1)*stride`, so
        // the buffer's required length is that, not `n*stride`. Computed with
        // checked arithmetic: `n * stride` is caller-controlled and could
        // otherwise wrap on a 32-bit target.
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
        // The layout decides which evaluation runs, **once**, outside the loop:
        // two loops rather than one loop with a compare in it. The batch's whole
        // purpose is per-element cost, and this file does not put a
        // loop-invariant test on that path.
        //
        // The two bodies are deliberately not folded behind a closure. That was
        // measured: `examples/abi_cost.rs` reports 189.5 ns/element for the
        // form below and 199 ns for the closure form, ~5 %, because the closure
        // stops LLVM hoisting the plan and guard loads out of the loop. Twelve
        // duplicated lines is the cheaper trade, and the failure reporting —
        // the part with an argument in it — is shared by `note_batch_failure`.
        if layout::carries_twist(layout) {
            // The twist layout goes through `Plan::at_many_into` — the *same*
            // batch fold, and the same monotone cursor, the Rust and Python
            // batch paths use. Evaluating it with a scalar `at_with_derivatives`
            // per element, as this used to, cannot reach that branch and pays an
            // independent `O(log n)` bracket search per stamp per plan step.
            //
            // It returns `false` for *any* failure and reports nothing, because
            // `at_many_into` says only which error, never which element. The
            // scalar loop below then re-runs the batch, reproduces the failure
            // and reports the index — which is what keeps §4.3's promise that
            // `frame_b` says how many leading elements are live. The rows it
            // rewrites on the way are bit-identical to the ones already there
            // (that is what the cursor being a *hint* means), so the buffer ends
            // in the state the doc comment describes either way.
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

/// Evaluate `stamps` in [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] through
/// [`tf_tree::Plan::at_many_into`], returning `false` if any element failed.
///
/// # Why this exists at all
///
/// `Layout::QuatTwist`'s batch fold rides a monotone cursor per plan step, so
/// ascending stamps cost `O(1)` amortized in the bracket search instead of
/// `O(log n)`. That is the reason the layout is a *batch* layout — it is
/// `docs/API.md` §3.3's n = 1024 ML/perception row — and a per-element scalar
/// call cannot reach it.
///
/// **Measured, and smaller than the complexity suggests.** On
/// `tf_tree_bench`'s fixture — ten seconds of history, up to 1 kHz, n = 1024 —
/// the cursor is worth 3.0 % (342 against 352 ns/element, in-process A/B of the
/// two arms on the same stamps). On `examples/abi_cost.rs`'s 64-sample rings it
/// is 1.4 % (240 → 237 ns/element, packed), because a binary search over 64
/// L1-resident stamps is a handful of nanoseconds against ~240 of interpolation
/// and adjoint composition. The win scales with ring depth, and the reason to
/// take it is that it costs nothing: it *is* the Rust path.
///
/// # Two shapes, one fold
///
/// `at_many_into` writes thirteen **contiguous** `f64` per stamp into a Rust
/// slice; §4.3's `out_stride_bytes` exists because a C caller's buffer is often
/// neither contiguous nor `f64`-aligned.
///
/// * **Tightly packed and aligned** — a `Quat7Twist6[]`, or the `(N, 13)` array
///   §3.3 is about, which is what the C++ wrapper passes: the caller's own
///   memory *is* the output slice. No copy and no chunking, so one cursor runs
///   the length of the batch.
/// * **Anything else**: `CHUNK` rows at a time through a stack buffer, then
///   scattered at the caller's stride. The cursor restarts once per chunk
///   instead of once per stamp, which is 1/`CHUNK` of the searches the scalar
///   loop paid.
///
/// `domain` is the plan handle's tag, passed through to `at_many_into_tagged`
/// so the batch path checks the same domain the scalar one does
/// (`docs/decisions/0038`).
///
/// `CHUNK` is deliberately small. The buffer is a plain array, so its zeroing
/// is paid even by a batch of one: 32 rows is 3.3 KiB of stack and leaves under
/// one probe per element of restart cost, where 512 would save a rounding error
/// of search and charge every short batch 53 KiB.
fn twist_batch(
    plan: &tf_tree::Plan,
    g: &tf_tree::Guard<'_>,
    stamps: &[i64],
    domain: u8,
    dst: &mut [u8],
    stride: usize,
    payload: usize,
) -> bool {
    /// `f64` per row — `Layout::QuatTwist::elems()`.
    const ROW: usize = 13;
    /// Rows per pass of the scatter path.
    const CHUNK: usize = 32;
    debug_assert_eq!(payload, ROW * 8, "the twist layout is thirteen f64");

    if stride == payload && dst.as_ptr().align_offset(core::mem::align_of::<f64>()) == 0 {
        // `dst.len()` is the span the caller's arguments were validated
        // against — `(n-1) * stride + payload`, which with `stride == payload`
        // is exactly `n * ROW` f64.
        //
        // SAFETY: `dst` is a live, uniquely borrowed `&mut [u8]` whose start is
        // `f64`-aligned (tested immediately above) and whose length is `n *
        // payload`, a multiple of eight. The reborrow below is the only access
        // to those bytes while it lives, so the two slices never alias, and
        // `f64` has no invalid bit patterns — whatever the caller left in the
        // buffer is a valid, if meaningless, `f64`.
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

/// Record the failure of element `i` of a batch, at stamp `t`.
///
/// Stops at the first failure, leaving the elements already written in place: a
/// batch is not a transaction, and the caller learns the index from the error
/// rather than from the buffer's contents.
///
/// `amend_error`, **not** `set_error`: the latter blanks the slot first, which
/// would erase the edge id and the retained window `record_lookup` just
/// recorded — leaving a batch caller with strictly less information than the
/// equivalent single call, the exact loss §3.3 exists to prevent. Found by
/// review.
///
/// The index goes in `frame_b`, which no lookup error uses, so a caller learns
/// *which element* failed as well as why, and `requested` keeps the stamp.
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

/// The number of bytes one transform occupies in `layout`, or `0` if the
/// discriminant is not one this build defines.
///
/// `0` is a safe sentinel here precisely because no real layout has size zero.
#[no_mangle]
pub extern "C" fn tft_layout_size(layout: tft_layout) -> usize {
    layout::payload_bytes(layout).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Extrapolation — `docs/decisions/0039`
// ---------------------------------------------------------------------------

/// What to do when the requested stamp is newer than every published sample on
/// the route.
///
/// A `uint32_t` typedef with named constants rather than a C `enum`, matching
/// [`tft_layout`] exactly — §3.6 needs the width of every ABI value stated, and
/// a C `enum`'s underlying type is the implementation's business. Every entry
/// point that takes one **rejects a discriminant it does not define** with
/// [`TFT_ERR_BAD_ENUM`], for [`layout::payload_bytes`]'s reason: an unknown
/// policy from a newer header must be an error, never a silent fallback to the
/// one this build happens to think is safest.
pub type tft_extrap_policy = u32;

/// Refuse: the lookup returns [`TFT_ERR_EXTRAPOLATION`] and writes nothing.
///
/// `0`, so a zeroed struct or a forgotten initialiser produces the refusal
/// rather than an invented pose. It is also `tf_tree::ExtrapPolicy`'s own
/// `Default`, and what [`tft_plan_at`] has always done.
pub const TFT_EXTRAP_ERROR: tft_extrap_policy = 0;
/// Hold the newest sample constant — `tf2`'s behaviour under some settings.
///
/// The honest primitive for a latched or displayed value. It is not the silent
/// staleness `tf2` is criticised for, because [`tft_extrapolated::by_ns`] comes
/// back in the same call and the caller had to pass somewhere to put it.
pub const TFT_EXTRAP_HOLD: tft_extrap_policy = 1;
/// Extend the constant screw twist implied by the two newest samples.
///
/// What a controller running faster than its state estimate wants
/// (`docs/decisions/0039` *Context*). Falls back to [`TFT_EXTRAP_HOLD`] on an
/// edge that retains a single sample: there is no twist to extend.
pub const TFT_EXTRAP_CONSTANT_TWIST: tft_extrap_policy = 2;

/// `policy` as the engine's enum, or `None` for a discriminant this build does
/// not define.
///
/// `None` rather than a default for the reason [`layout::payload_bytes`] gives:
/// a caller compiled against a newer header must be refused, not quietly served
/// a different policy than the one it named — and the two policies differ in
/// what the answer *is*, not in how it is formatted.
fn extrap_policy(policy: tft_extrap_policy) -> Option<tf_tree::ExtrapPolicy> {
    Some(match policy {
        TFT_EXTRAP_ERROR => tf_tree::ExtrapPolicy::Error,
        TFT_EXTRAP_HOLD => tf_tree::ExtrapPolicy::Hold,
        TFT_EXTRAP_CONSTANT_TWIST => tf_tree::ExtrapPolicy::ConstantTwist,
        _ => return None,
    })
}

/// How far past the route's newest common sample an answer was extrapolated.
///
/// **The caller has to pass one of these to get a pose at all**, and that is
/// the whole design rather than an out-parameter that happened to be
/// convenient (`docs/decisions/0039` §1). In Rust the property is a type with
/// no pose-only accessor; C has no such enforcement, so the closest honest
/// analogue is a *required* out-parameter — [`tft_plan_at_extrapolating`]
/// returns [`TFT_ERR_NULL_ARG`] when `info` is NULL and writes nothing. There
/// is deliberately no second spelling of that call without this argument, so
/// "forgot to check the staleness" is not reachable by omission; it takes a
/// caller who read `by_ns` and ignored it.
///
/// `struct_size` is §3.6's append mechanism, and it is checked exactly as
/// [`tft_error`]'s is: set it to `sizeof(tft_extrapolated)` before the call or
/// the call returns [`TFT_ERR_BAD_STRUCT_SIZE`]. It is written back on success,
/// so the struct a caller passes twice needs setting once per *object*, not
/// once per call.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tft_extrapolated {
    /// `sizeof(tft_extrapolated)` at the time this build was compiled — §3.6.
    pub struct_size: u32,
    /// Nanoseconds past the newest stamp that **every** dynamic edge on this
    /// plan has data for.
    ///
    /// `0` means every edge bracketed the query: the answer was interpolated
    /// between published samples, not invented past them, and the policy did
    /// not come into it. A positive value is the worst case over the route,
    /// because the edge that runs out of data first is what bounds how invented
    /// a composed answer is (`docs/decisions/0039` §3).
    pub by_ns: i64,
    /// The dynamic edge whose newest stamp is [`Self::by_ns`] behind the query,
    /// or [`TFT_INVALID_ID`] when `by_ns` is `0`.
    ///
    /// **The sentinel is this side's, and it is deliberately sharper than the
    /// Rust value it mirrors.** `tf_tree::Extrapolated::edge` carries a real
    /// `EdgeId` documented as *meaningless when `by_ns == 0`*; a C caller
    /// handed `0` there would be looking at a plausible edge id for an answer
    /// that was never extrapolated. [`TFT_INVALID_ID`] is what the rest of this
    /// header already means by "this field does not apply" ([`tft_error`]), so
    /// the sentence is checkable rather than only documented.
    pub edge: u32,
}

/// [`tft_plan_at`], permitting extrapolation past the newest sample under
/// `policy`, and reporting how far the answer was extrapolated.
///
/// The capability existed in the engine's sampler from the beginning and was
/// reachable from no shipped surface until `docs/decisions/0039`; this is the C
/// half of reaching it. A controller running at 1 kHz against a 100 Hz state
/// estimate is *always* asking for a stamp past the newest sample, and the
/// honest answer is a bounded prediction with its bound attached — not a
/// refusal, and not a silent stale pose.
///
/// **`info` is required.** That is the property the whole surface is for: the
/// distance is handed back in the same call as the pose, so a caller cannot get
/// one without the other. Passing NULL is [`TFT_ERR_NULL_ARG`] and nothing is
/// written, in either buffer.
///
/// [`tft_plan_at`] is untouched, still refuses, and remains what a caller that
/// must not act on invented data should call. This function with
/// [`TFT_EXTRAP_ERROR`] is that same refusal with a distance attached on
/// success.
///
/// The plan is evaluated in the domain it was compiled for
/// ([`tft_plan_create_in_domain`]); the tag is on the handle, not on this call.
///
/// # `TFT_LAYOUT_QVEC7_WXYZ_TWIST6` is not accepted here
///
/// Asking for that layout is asking for derivatives, and the engine has no
/// extrapolating form of `at_with_derivatives` — `docs/decisions/0039` adds one
/// pose-returning method and deliberately no second one. So the layout is
/// refused with [`TFT_ERR_BAD_ENUM`] and nothing is written, exactly as
/// [`tft_publisher_push`] refuses the write-only `TFT_LAYOUT_AFFINE12_ROW_F32`:
/// the discriminant is defined, and this entry point does not take it. The
/// alternative — emitting a twist evaluated under `ExtrapPolicy::Error` beside
/// a pose extrapolated under the caller's — would put two different policies in
/// one thirteen-`f64` row.
///
/// # Errors
///
/// Everything [`tft_plan_at`] returns. Under [`TFT_EXTRAP_ERROR`] a stamp past
/// the newest sample is [`TFT_ERR_EXTRAPOLATION`]; under the other two it is
/// not, and `info->by_ns` says how far. [`TFT_ERR_BAD_STRUCT_SIZE`] if
/// `info->struct_size` is not `sizeof(tft_extrapolated)`.
///
/// # Safety
///
/// `plan` must be a handle from [`tft_plan_create`] that has not been freed.
/// `out` must point to at least `tft_layout_size(layout)` writable bytes.
/// `info` must point to a writable `tft_extrapolated` whose `struct_size` this
/// caller has set.
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
        // The argument this entry point exists to make unavoidable. Checked
        // beside `out` rather than after the evaluation, so a caller who forgot
        // it gets the refusal instead of a pose it cannot judge.
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
        // SAFETY: `info` is non-null and the caller contracts it points at a
        // `tft_extrapolated` with `struct_size` set. `read_unaligned` for
        // `magic_check!`'s reason — a caller's struct need not be aligned to
        // *our* idea of the type's alignment for this read to be the one that
        // catches the mistake.
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
        // `_tagged`, and the tag is the handle's — `docs/decisions/0038`. There
        // is no type this file can name that stands for the caller's domain,
        // because `Domain` is an open trait.
        match h.plan.at_extrapolating_tagged(&g, stamp, h.domain, policy) {
            Ok(x) => {
                layout::write(&x.pose, layout, dst);
                // Written after the pose, and only on success: a caller that
                // got a status other than `TFT_OK` has nothing in `out` to
                // judge, so a distance describing it would be describing
                // nothing.
                //
                // See `tft_extrapolated::edge` for why `by_ns == 0` reports
                // the sentinel rather than the engine's argmin edge.
                let e = tft_extrapolated {
                    struct_size: core::mem::size_of::<tft_extrapolated>() as u32,
                    by_ns: x.by_ns,
                    edge: if x.by_ns == 0 {
                        TFT_INVALID_ID
                    } else {
                        x.edge.get()
                    },
                };
                // SAFETY: `info` is non-null, the caller contracts it writable,
                // and its `struct_size` matched this build's exactly — so the
                // whole struct is inside the caller's allocation.
                unsafe { core::ptr::write(info, e) };
                TFT_OK
            }
            Err(e) => record_lookup(e),
        }
    })
}

// ---------------------------------------------------------------------------
// Test-only panic hook — §6.1
// ---------------------------------------------------------------------------

/// A guarded entry point that does nothing, for measuring what `guard` costs.
///
/// Pairs with `tft_layout_size`, which is deliberately *not* guarded, so the
/// difference between the two is `catch_unwind`'s landing pads plus the
/// `clear_error` every guarded body performs — and nothing else. §3.4's
/// zero-cost claim is checked by `examples/abi_cost.rs`, not asserted.
#[cfg(feature = "test-hooks")]
#[no_mangle]
pub extern "C" fn tft_guarded_noop(x: i32) -> tft_status {
    guard(|| x)
}

/// Force a panic inside an `extern "C"` body, to prove the guard converts it
/// into a status rather than aborting. Compiled only under `--features test-hooks`.
///
/// # Safety
///
/// Takes no pointers; safe to call, and `unsafe` only for signature symmetry
/// with the rest of the ABI.
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

/// The same, for an entry point with **no status to report through**.
///
/// `tft_test_panic` proves [`guard`] converts a panic into `TFT_ERR_INTERNAL`.
/// It cannot cover the boundaries that return a count or a size, because those
/// have nowhere to put a status — and until `guard_value` existed they carried
/// no guard at all, which is the gap §6's "on every `extern \"C\"` boundary"
/// checkbox names. This is the shape of `tft_tree_frame_count` and
/// `tft_tree_edge_count`: panic inside, and the caller sees the fallback rather
/// than losing the process.
///
/// # Safety
///
/// Takes no pointers; safe to call, and `unsafe` only for signature symmetry.
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

/// Build an in-process fixture tree: `map -> odom -> base`, two dynamic ScLerp
/// edges with 64 samples each 10 ms apart, plus a static `base -> sensor`.
///
/// Exists so the §6.1 suite and the §7 benchmark can drive the **real** entry
/// points without a running arena. Compiled only under `--features test-hooks`,
/// so it is not part of the shipped ABI and never appears in `tf_tree.h`.
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

/// Build a fixture tree whose dynamic edge interpolates with **`LerpSlerp`**:
/// `map -> base`, 32 samples 10 ms apart.
///
/// A third fixture, because the refusal it exists to test cannot be reached
/// from either of the others. [`tft_test_tree_create`]'s edges are `ScLerp`, so
/// every derivative query against it succeeds — which is the right shape for
/// the numeric tests and the wrong one for the only error a twist layout has
/// that a pose layout does not. `LerpSlerp`'s body twist is an artifact of the
/// interpolant rather than of the motion, so `TFT_LAYOUT_QVEC7_WXYZ_TWIST6`
/// must come back `TFT_ERR_NO_DERIVATIVES` naming the edge instead of a
/// plausible number, and there was previously no way to assert that through
/// the C ABI at all.
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
        // Held for the life of the tree, exactly as `tft_test_tree_create`
        // does: a released claim would let a lookup race a reaper.
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

/// Build a fixture tree whose dynamic edge is published in time domain
/// `domain`: `map -> odom` (ScLerp, 32 samples 10 ms apart) plus a static
/// `odom -> sensor`.
///
/// A fourth fixture, because the arena `docs/decisions/0038` exists for cannot
/// be built from any of the others: [`tft_test_tree_create`] and its two
/// siblings publish in tag `0`, so every plan over them agrees with
/// [`tft_plan_create`] by default and the mismatch never arises. This is the
/// shape `ros/tf_tree_ros`'s `use_sim_time` warning tells an operator to
/// configure, and before `0038` it was unreadable from C by construction.
///
/// **The static edge is not decoration.** `Plan::domain()` reports `0` for a
/// route with no dynamic edge on it, so `odom -> sensor` is what proves
/// [`tft_test_domain_tree_create`]'s tag does not leak into a plan-time refusal
/// of a static lookup — the one case
/// [`tft_plan_create_in_domain`] deliberately accepts.
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
        // Held for the life of the tree, as the other fixtures do: a released
        // claim would let a lookup race a reaper.
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

/// Build a fixture tree with a **claimable** dynamic edge: `world -> robot`
/// (ScLerp, no samples, unclaimed) plus a static `robot -> tool`.
///
/// Separate from [`tft_test_tree_create`] on purpose. That fixture `forget`s its
/// writers so both of its dynamic edges stay claimed for the life of the tree,
/// which is exactly what the lookup tests want and exactly what a publisher test
/// cannot use. Changing it to leave an edge free would also perturb the arena
/// the §7 benchmark measures, and a benchmark that moves for a test's
/// convenience is a benchmark nobody can compare across commits.
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
            // **Headroom is not decoration.** Without it `max_frames` equals
            // `frame_count` and `tft_tree_frame_name`'s range check is
            // untestable — which is exactly why a real hole there went
            // unnoticed until review. Any publisher that interns frames at
            // runtime needs headroom, so this fixture is also the more
            // representative one.
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

// ---------------------------------------------------------------------------
// Small helpers, so every entry point reports failures identically
// ---------------------------------------------------------------------------

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
