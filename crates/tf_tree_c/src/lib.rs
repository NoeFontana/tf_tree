//! The `tf_tree` C ABI — `docs/PHASE4.md` §3.
//!
//! # SAFETY (module-level, per `docs/decisions/0007`)
//!
//! This crate is one of the boundaries where `unsafe` is permitted: **a foreign
//! caller**. The `unsafe` here turns caller pointers into Rust references, once,
//! at the entry point; past that check every body is safe code. Three rules:
//!
//! 1. **Handles carry a magic word** written by the constructor and zeroed by
//!    the destructor (§3.2). It rejects NULL and handle-type confusion (a
//!    `tft_plan*` for a `tft_tree*`). It does **not** validate an arbitrary
//!    foreign pointer (reading 8 bytes through one is UB whatever the bytes are;
//!    the read is [`core::ptr::read_unaligned`] so alignment is not a hazard): the
//!    caller's contract is that the pointer is NULL or a handle. Use-after-free
//!    is best-effort, only while the memory is untouched.
//! 2. **`#![deny(unsafe_op_in_unsafe_fn)]`**: each dereference carries its own
//!    `// SAFETY:` naming what it relies on.
//! 3. **This crate depends on `tf_tree`, not `tf_tree_core`**, so it cannot reach
//!    an invariant the safe facade protects (fork-generation and detach checks).
//!
//! # Panics cannot escape
//!
//! A panic crossing an `extern "C"` boundary aborts the process, so every entry
//! point wraps its body in `error::guard`, which converts a panic into
//! `TFT_ERR_INTERNAL`. The exception is [`tft_last_error`], which must stay
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

// ABI version — §3.6

/// Major ABI version. **Must match exactly** between the header a caller
/// compiled against and the library it links.
pub const TFT_ABI_VERSION_MAJOR: u32 = 0;
/// Minor ABI version. The runtime's may be **≥** the compiled-against value (§3.6).
///
/// Every bump is an append (nothing moved, changed type or changed meaning), and
/// the minor answers "can I name this symbol?", so a new function or enumerator
/// bumps it even in the unstable tier:
///
/// * `1` → `2`: `tft_bridge_note_time_jump`; fields appended to
///   `tft_bridge_sample` and `tft_bridge_outcome`. `tft_bridge_offer` reads a
///   shorter `tft_bridge_sample` as the prefix it is.
/// * `2` → `3`: [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] (`docs/API.md` §3.3).
/// * `3` → `4`: [`tft_stamp_from_parts`], [`tft_stamp_from_timespec`] (`docs/API.md`
///   §5.1) and [`TFT_ERR_BAD_STAMP`], which only they return.
/// * `4` → `5`: `tft_bridge_options::arena_name` (`docs/decisions/0015`) and
///   [`TFT_ERR_ARENA_UNAVAILABLE`], reachable only when `arena_name` is non-NULL;
///   `tft_bridge_create` reads a shorter `tft_bridge_options` as its prefix.
/// * `5` → `6`: [`tft_plan_create_in_domain`] (`docs/decisions/0038`).
///   [`tft_plan_create`] is it with `domain = 0`, and now returns
///   [`TFT_ERR_TIME_DOMAIN`] at plan time instead of on every lookup.
/// * `6` → `7`: [`tft_plan_at_extrapolating`], [`tft_extrap_policy`] and
///   [`tft_extrapolated`] (`docs/decisions/0039`).
/// * `7` → `8`: `tft_bridge_close_startup_window` and
///   `TFT_BRIDGE_REASON_STARTUP_CONFLICTS` (`docs/decisions/0011` step 6). A
///   `STRICT` startup halt reached through `tft_bridge_offer` now reports 9
///   where it reported `TFT_BRIDGE_REASON_AUTHORITY_CONFLICT` (5); the action is
///   `TFT_BRIDGE_HALT` either way.
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

/// Check the header a caller compiled against against the library they linked:
/// major must match exactly; the runtime minor may be ≥ the compiled-against
/// minor (§3.6).
///
/// Call it as `tft_check_abi(TFT_ABI_VERSION_MAJOR, TFT_ABI_VERSION_MINOR)` with
/// the constants **from the header**, once at startup (the C++ wrapper does).
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
        // Allocates only on the failure path.
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

// Stamps — `docs/API.md` §5.1

/// Assemble a stamp from a `(sec, nanos)` pair, exactly — `docs/API.md` §5.1.
///
/// The C spelling of `Stamp::from_parts`, for a ROS 2 `builtin_interfaces/Time`.
/// No float on any surface (R3). It returns a status because two inputs have no
/// correct answer and no `int64_t` sentinel exists: normalising an out-of-range
/// `nanos` or wrapping an out-of-range sum would each yield a plausible stamp.
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
        // SAFETY: `out` is non-null by the check above and the caller contracts
        // it writable. Written only after the conversion succeeded, so a refused
        // call leaves the caller's variable as it was.
        unsafe { core::ptr::write(out, s.nanos()) };
        TFT_OK
    })
}

/// Assemble a stamp from the two fields of a POSIX `struct timespec`
/// (`tft_stamp_from_timespec(ts.tv_sec, ts.tv_nsec, &out)`).
///
/// # Errors
///
/// Everything [`tft_stamp_from_parts`] refuses, plus a negative `tv_nsec`
/// (legal only in a relative `timespec`, so an interval is being converted as an
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

/// The one refusal both stamp converters raise; `requested` carries the seconds
/// and `newest` the nanoseconds.
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

// Handles — §3.2

/// Magic words, distinct per type.
const MAGIC_TREE: u64 = 0x7446_5F54_5245_4531; // "tFT_TREE1"-ish
const MAGIC_PLAN: u64 = 0x7446_5F50_4C41_4E31;

/// An opaque handle to a transform tree. `Send + Sync`.
///
/// `#[repr(C)]` because `check_tree` reads the magic through a field projection.
/// The generated header declares this as an incomplete type (§3.2): `xtask
/// headers` excludes it and emits the forward declaration itself.
#[repr(C)]
pub struct tft_tree {
    magic: u64,
    share: Arc<TreeShare>,
}

/// An opaque handle to a compiled plan. `Send + Sync`, immutable. `#[repr(C)]`
/// and an incomplete type in the header, as [`tft_tree`].
#[repr(C)]
pub struct tft_plan {
    magic: u64,
    plan: tf_tree::Plan,
    /// The plan owns a share of the tree, not a pointer: freeing the tree before
    /// its plans is the natural C order and must not dangle.
    share: Arc<TreeShare>,
    /// The time domain every evaluate call on this handle asks the engine about
    /// (`docs/decisions/0038`), set by [`tft_plan_create_in_domain`].
    domain: u8,
}

/// The tree, shared between its own handle and every plan compiled from it.
///
/// `Arc<Tree>` because [`tf_tree::Tree::claim_owned`] takes `self: &Arc<Tree>`
/// (`docs/decisions/0017`). `Arc<TreeShare>` is the handle refcount (free order
/// is irrelevant); `Arc<Tree>` is the arena refcount a publisher holds after
/// every handle is gone.
pub(crate) struct TreeShare {
    pub(crate) tree: Arc<Tree>,
}

/// Generate a magic-word validator that reads the field **by name** (correct
/// under any layout, and keeps `magic` from being dead code) with
/// [`core::ptr::read_unaligned`] (an aligned read through a non-handle pointer is
/// UB) via `addr_of!`, so no reference to a possibly-invalid handle is created.
/// Not a validator for arbitrary memory; see the module docs.
macro_rules! magic_check {
    ($name:ident, $ty:ty, $magic:expr) => {
        /// # Safety
        ///
        /// `p` must be NULL, or point to at least `size_of::<u64>()` readable
        /// bytes at the offset of the handle's `magic` field (any live handle type
        /// satisfies that, which is what makes type-confusion detection work).
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

/// Wrap a share of an already-built tree in a fresh, independently owned handle,
/// so free order is irrelevant to the bridge's reader.
#[cfg(feature = "bridge")]
pub(crate) fn tree_handle(share: Arc<TreeShare>) -> Box<tft_tree> {
    Box::new(tft_tree {
        magic: MAGIC_TREE,
        share,
    })
}

// Lifecycle — §3.2

/// Join the running arena named by the environment, read-only (D18).
///
/// Mirrors `tf_tree::open()`: `$TF_TREE_DOMAIN`, `$TF_TREE_NAME` and
/// `$TF_TREE_RUNTIME_DIR` select the arena. On success `*out` must be passed to
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

/// Release a tree handle. Freeing NULL is a no-op. Plans compiled from it stay
/// valid (the tree is refcounted).
///
/// # Safety
///
/// `tree` must be NULL or a handle from a `tft_tree_*` constructor that has not
/// already been freed. Double-free is undefined; the magic word catches it only
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
    // Zero the magic before dropping so a repeated free sees a dead handle.
    // SAFETY: `check` confirmed this is a live `tft_tree`.
    unsafe { core::ptr::write(tree.cast::<u64>(), 0) };
    // SAFETY: the handle was produced by `Box::into_raw` in a constructor above.
    drop(unsafe { Box::from_raw(tree) });
}

/// Compile a plan for `target <- source`, by frame name.
///
/// Compilation walks the topology once; evaluating is the hot path (D3), so
/// compile once and evaluate many times.
///
/// This is [`tft_plan_create_in_domain`] with `domain = 0`, the real-time tag. On
/// an arena whose dynamic edges carry another tag (`docs/PHASE4.md` §5.5) it
/// returns [`TFT_ERR_TIME_DOMAIN`] here (`docs/decisions/0038`).
///
/// # Errors
///
/// `*out` is not written on any failure. Three codes carry extra meaning here:
///
/// * [`TFT_ERR_UNKNOWN_FRAME`] — a name is not UTF-8 or does not resolve (on a
///   read-only attachment: undeclared, or a hash-slot collision (permanent), or
///   being interned right now (transient, retry); on a writable tree: a full
///   frame table). From compilation, with `frame_a` set: no consistent topology
///   snapshot within the retry limit (transient), or a parent index outside the
///   frame table (corrupt arena).
/// * [`TFT_ERR_NO_DATA`] — the topology records a parent for `frame_a` but no
///   edge (a corrupt arena). An edge with no samples yet compiles.
/// * [`TFT_ERR_TIME_DOMAIN`] — the route's dynamic edges publish in a tag other
///   than `domain`, or disagree among themselves (`edge` names the one that did).
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
    // SAFETY: every pointer is forwarded under the identical contract.
    unsafe { tft_plan_create_in_domain(tree, target, source, 0, out) }
}

/// Compile a plan for `target <- source` that will be queried in time domain
/// `domain` (`docs/decisions/0038`).
///
/// [`tf_tree::Domain`] is an open trait, so a foreign caller carries the tag
/// (`0`–`3` are the built-in domains, `4`+ are driver-declared; `docs/API.md`
/// §2.5) as data. `0` is [`tft_plan_create`]. A mismatch is reported once, at
/// plan time, with the frame names in hand; every evaluate entry point still
/// passes the handle's tag to the engine and the engine still compares it.
///
/// # Errors
///
/// Everything [`tft_plan_create`] returns, plus [`TFT_ERR_TIME_DOMAIN`] when
/// this route has a dynamic edge whose tag is not `domain`; `*out` is not
/// written. The condition is the engine's `has_dynamic() && domain !=
/// self.domain`: a bare `domain != plan.domain()` would wrongly refuse a static
/// route (`Plan::domain` reports `0` for one), so this asks
/// [`tf_tree::Plan::steps`] whether any [`tf_tree::Step::Dyn`] is present.
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
                // `Plan::has_dynamic` is private; rebuild its predicate from
                // `steps()`. It must match the engine's exactly (see *Errors*).
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

// Hot path — §3.7

/// Evaluate `plan` at `stamp`, writing the result into `out` in `layout`.
///
/// `out` must have room for at least `tft_layout_size(layout)` bytes.
///
/// **On a hot path, prefer [`tft_plan_at_many`]**: this builds a `Guard` per
/// lookup (`docs/decisions/0022`), the batch pays it once per call. The plan is
/// evaluated in the domain it was compiled for ([`tft_plan_create_in_domain`]).
///
/// [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] is asking for derivatives: thirteen `f64`
/// are written, and it fails with `TFT_ERR_NO_DERIVATIVES` (a `LerpSlerp` edge)
/// or `TFT_ERR_NO_SEGMENT` (a pose but no segment), writing nothing.
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
        // `_tagged`, with the handle's tag (`docs/decisions/0038`): `Domain` is
        // an open trait, so no type here stands for the caller's domain.
        // Two evaluations, chosen once from the layout: a pose layout must not
        // pay for `at_with_derivatives`' adjoint chain.
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

/// [`tft_plan_at`]'s body without the panic guard: the read-path twin of
/// [`tft_test_push_unguarded`], so `examples/abi_cost.rs` can price
/// `catch_unwind` on a real, non-inlinable call by subtraction
/// (`docs/decisions/0022`). Behind `test-hooks`.
///
/// # Safety
///
/// As [`tft_plan_at`]. Unlike it, this does not catch unwinds: a panic crossing
/// this boundary is undefined behaviour.
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
    // Tagged, as `tft_plan_at` is: only the guard may differ.
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
/// `out_stride_bytes == 0` means tightly packed; a larger stride writes into an
/// array of caller structs (§4.3).
///
/// # Partial writes
///
/// Evaluation stops at the first failing stamp and earlier elements stay
/// written; `tft_last_error`'s `frame_b` carries the failing index. Only the
/// argument checks (NULL, stride, overflow, unknown layout) are all-or-nothing.
///
/// # `TFT_LAYOUT_QVEC7_WXYZ_TWIST6`
///
/// Accepted as by [`tft_plan_at`], per element. `TFT_ERR_NO_DERIVATIVES` is a
/// property of an edge and fires on the first element with the buffer untouched;
/// `TFT_ERR_NO_SEGMENT` can fire part-way. Sort your stamps: non-decreasing
/// stamps ride a resumable cursor (`O(1)` amortized bracket search). A packed,
/// `f64`-aligned `out` is written in place; any other stride is evaluated in
/// chunks and scattered.
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
        // Zero elements is a no-op before the NULL checks.
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
        // The extent is `(n-1)*stride + payload`, checked for overflow.
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
        // The layout decides which evaluation runs once, outside the loop, in
        // two loops rather than a compare (or a closure, ~5 % slower per
        // `examples/abi_cost.rs`) on the per-element path.
        if layout::carries_twist(layout) {
            // `Plan::at_many_into`: the same batch fold and monotone cursor as
            // the Rust path. It reports only success, so on failure the scalar
            // loop re-runs to find the index for `frame_b` (§4.3); rewritten rows
            // are bit-identical.
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
/// Its batch fold rides a monotone cursor per plan step (`docs/API.md` §3.3's
/// n = 1024 row). A tightly packed, aligned `dst` (the C++ wrapper's case) is
/// the output slice itself; any other stride goes `CHUNK` rows at a time through
/// a stack buffer and is scattered, restarting the cursor per chunk. `domain` is
/// the plan handle's tag (`docs/decisions/0038`).
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
        // `dst.len()` is `(n-1) * stride + payload`, which with `stride ==
        // payload` is `n * ROW` f64.
        //
        // SAFETY: `dst` is a live, uniquely borrowed `&mut [u8]` whose start is
        // `f64`-aligned (tested above) and whose length is `n * payload`, a
        // multiple of eight; the reborrow is the only access to those bytes, and
        // `f64` has no invalid bit patterns.
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
/// `amend_error`, not `set_error`: the latter blanks the slot and would erase
/// the edge id and retained window `record_lookup` just recorded (§3.3). The
/// index goes in `frame_b`, which no lookup error uses; `requested` keeps the
/// stamp.
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
#[no_mangle]
pub extern "C" fn tft_layout_size(layout: tft_layout) -> usize {
    layout::payload_bytes(layout).unwrap_or(0)
}

// Extrapolation — `docs/decisions/0039`

/// What to do when the requested stamp is newer than every published sample on
/// the route.
///
/// A `uint32_t` typedef with named constants, like [`tft_layout`] (§3.6 needs
/// every ABI value's width stated). Every entry point that takes one rejects an
/// undefined discriminant with [`TFT_ERR_BAD_ENUM`].
pub type tft_extrap_policy = u32;

/// Refuse: the lookup returns [`TFT_ERR_EXTRAPOLATION`] and writes nothing. `0`,
/// so a zeroed struct refuses; it is `tf_tree::ExtrapPolicy`'s `Default` and what
/// [`tft_plan_at`] does.
pub const TFT_EXTRAP_ERROR: tft_extrap_policy = 0;
/// Hold the newest sample constant; [`tft_extrapolated::by_ns`] comes back in
/// the same call.
pub const TFT_EXTRAP_HOLD: tft_extrap_policy = 1;
/// Extend the constant screw twist implied by the two newest samples
/// (`docs/decisions/0039` *Context*); falls back to [`TFT_EXTRAP_HOLD`] on an
/// edge retaining a single sample.
pub const TFT_EXTRAP_CONSTANT_TWIST: tft_extrap_policy = 2;

/// `policy` as the engine's enum, or `None` for an undefined discriminant.
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
/// The caller must pass one to get a pose at all (`docs/decisions/0039` §1):
/// [`tft_plan_at_extrapolating`] returns [`TFT_ERR_NULL_ARG`] for a NULL `info`,
/// and there is no second spelling without it. `struct_size` is §3.6's append
/// mechanism, checked as [`tft_error`]'s is: set it to `sizeof(tft_extrapolated)`
/// or the call returns [`TFT_ERR_BAD_STRUCT_SIZE`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tft_extrapolated {
    /// `sizeof(tft_extrapolated)` — §3.6.
    pub struct_size: u32,
    /// Nanoseconds past the newest stamp that every dynamic edge on this plan has
    /// data for; `0` means every edge bracketed the query. Otherwise the worst
    /// case over the route (`docs/decisions/0039` §3).
    pub by_ns: i64,
    /// The dynamic edge whose newest stamp is [`Self::by_ns`] behind the query, or
    /// [`TFT_INVALID_ID`] when `by_ns` is `0` (where the engine's edge id is
    /// meaningless).
    pub edge: u32,
}

impl tft_extrapolated {
    /// A "not extrapolated" value with `struct_size` set and `edge` the sentinel;
    /// public to spare callers `unsafe { core::mem::zeroed() }` (`docs/decisions/0048`).
    #[must_use]
    pub const fn blank() -> tft_extrapolated {
        tft_extrapolated {
            struct_size: core::mem::size_of::<tft_extrapolated>() as u32,
            by_ns: 0,
            edge: TFT_INVALID_ID,
        }
    }
}

/// [`tft_plan_at`], permitting extrapolation past the newest sample under
/// `policy` and reporting how far (`docs/decisions/0039`).
///
/// `info` is required: the distance comes back with the pose. NULL is
/// [`TFT_ERR_NULL_ARG`] and nothing is written. [`tft_plan_at`] still refuses, and
/// is what a caller that must not act on invented data should call. The plan is
/// evaluated in the domain it was compiled for.
///
/// [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] is refused with [`TFT_ERR_BAD_ENUM`]: the
/// engine has no extrapolating `at_with_derivatives`.
///
/// # Errors
///
/// Everything [`tft_plan_at`] returns. Under [`TFT_EXTRAP_ERROR`] a stamp past
/// the newest sample is [`TFT_ERR_EXTRAPOLATION`]; otherwise `info->by_ns` says
/// how far. [`TFT_ERR_BAD_STRUCT_SIZE`] if `info->struct_size` is not
/// `sizeof(tft_extrapolated)`.
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
        // Checked beside `out`, so a caller who forgot it gets the refusal.
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
        // `tft_extrapolated` with `struct_size` set; `read_unaligned` as
        // `magic_check!`.
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
                // Written only on success, after the pose. `by_ns == 0` reports
                // the sentinel edge (see `tft_extrapolated::edge`).
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

// Test-only panic hook — §6.1

/// A guarded entry point that does nothing, for pricing `guard` against the
/// unguarded `tft_layout_size` (`examples/abi_cost.rs`).
#[cfg(feature = "test-hooks")]
#[no_mangle]
pub extern "C" fn tft_guarded_noop(x: i32) -> tft_status {
    guard(|| x)
}

/// Force a panic inside an `extern "C"` body, to prove the guard converts it
/// into a status. Only under `--features test-hooks`.
///
/// # Safety
///
/// Takes no pointers; `unsafe` only for signature symmetry.
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

/// The same, for an entry point with no status to report through (a count or
/// size, like `tft_tree_frame_count`): the caller sees the fallback.
///
/// # Safety
///
/// Takes no pointers; `unsafe` only for signature symmetry.
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
/// Lets the §6.1 suite and §7 benchmark drive the real entry points without a
/// running arena; only under `--features test-hooks`.
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

/// Build a fixture tree whose dynamic edge interpolates with `LerpSlerp`:
/// `map -> base`, 32 samples 10 ms apart, for the `TFT_ERR_NO_DERIVATIVES` path.
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
        // Held for the life of the tree: a released claim would let a lookup race a reaper.
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
/// `odom -> sensor` (which must not trigger a plan-time refusal;
/// `docs/decisions/0038`).
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

/// Build a fixture tree with a **claimable** dynamic edge: `world -> robot`
/// (ScLerp, no samples, unclaimed) plus a static `robot -> tool`. Separate from
/// [`tft_test_tree_create`], whose writers are forgotten and which the §7
/// benchmark measures.
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
            // Headroom makes `tft_tree_frame_name`'s range check testable.
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

// Small helpers, so every entry point reports failures identically

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
