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
//! Every entry point wraps its body in [`error::guard`], which converts a panic
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

pub mod error;
pub mod layout;

use core::ffi::{c_char, c_void};
use std::sync::Arc;

use tf_tree::{Stamp, SystemDomain, Tree};

pub use error::{
    tft_error, tft_last_error, tft_status, TFT_ERR_BAD_ENUM, TFT_ERR_BAD_HANDLE,
    TFT_ERR_BAD_STRUCT_SIZE, TFT_ERR_BUFFER_TOO_SMALL, TFT_ERR_CHILD_DETACHED,
    TFT_ERR_DISCONNECTED, TFT_ERR_EXTRAPOLATION, TFT_ERR_INTERNAL, TFT_ERR_NO_DATA,
    TFT_ERR_NO_DERIVATIVES, TFT_ERR_NO_SEGMENT, TFT_ERR_NULL_ARG, TFT_ERR_SLOT_CONTENDED,
    TFT_ERR_SLOT_RECYCLED, TFT_ERR_TIME_DOMAIN, TFT_ERR_TOPOLOGY_CHANGED, TFT_ERR_TREE_TOO_DEEP,
    TFT_ERR_UNKNOWN_FRAME, TFT_ERR_WRONG_THREAD, TFT_INVALID_ID, TFT_MESSAGE_LEN, TFT_OK,
};
pub use layout::{
    tft_layout, TFT_LAYOUT_AFFINE12_ROW_F32, TFT_LAYOUT_MAT4_COL, TFT_LAYOUT_MAT4_ROW,
    TFT_LAYOUT_QVEC7_WXYZ, TFT_LAYOUT_QVEC7_XYZW,
};

use error::{amend_error, guard, record_lookup, set_error};

// ---------------------------------------------------------------------------
// ABI version — §3.6
// ---------------------------------------------------------------------------

/// Major ABI version. **Must match exactly** between the header a caller
/// compiled against and the library it links.
pub const TFT_ABI_VERSION_MAJOR: u32 = 0;
/// Minor ABI version. The runtime's may be **≥** the compiled-against value.
pub const TFT_ABI_VERSION_MINOR: u32 = 1;

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

// ---------------------------------------------------------------------------
// Handles — §3.2
// ---------------------------------------------------------------------------

/// Magic words. Distinct per type, so passing a `tft_plan*` where a `tft_tree*`
/// is expected is caught rather than followed into a type confusion.
const MAGIC_TREE: u64 = 0x7446_5F54_5245_4531; // "tFT_TREE1"-ish
const MAGIC_PLAN: u64 = 0x7446_5F50_4C41_4E31;

/// An opaque handle to a transform tree. `Send + Sync`: shareable across threads.
///
/// `#[repr(C)]` is load-bearing, not decoration: [`check_tree`] validates the
/// magic word through a field projection, and under the default `repr(Rust)` the
/// compiler is free to reorder fields — so "the magic is first" would be an
/// assumption that happens to hold today and could stop holding on a compiler
/// upgrade, silently turning every handle check into a read of an `Arc` pointer.
#[repr(C)]
pub struct tft_tree {
    magic: u64,
    share: Arc<TreeShare>,
}

/// An opaque handle to a compiled plan. `Send + Sync`, immutable.
///
/// `#[repr(C)]` for the same reason as [`tft_tree`].
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
}

/// The tree, shared between its own handle and every plan compiled from it.
struct TreeShare {
    tree: Tree,
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
        unsafe fn $name(p: *const $ty) -> bool {
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
                    share: Arc::new(TreeShare { tree }),
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
/// refcounted and this drops one reference (see [`tft_plan::share`]).
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
                let p = Box::new(tft_plan {
                    magic: MAGIC_PLAN,
                    plan,
                    share: Arc::clone(&h.share),
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
        match h.plan.at(&g, Stamp::<SystemDomain>::from_nanos(stamp)) {
            Ok(iso) => {
                layout::write(&iso, layout, dst);
                TFT_OK
            }
            Err(e) => record_lookup(e),
        }
    })
}

/// Evaluate `plan` at `n` stamps, writing each result `out_stride_bytes` apart.
///
/// `out_stride_bytes == 0` means tightly packed. A stride larger than the
/// payload writes directly into an array of caller structs — §4.3 is why this
/// parameter exists at all (`Sophus::SE3d` is usually *not* tightly packed).
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
        for (i, &t) in ts.iter().enumerate() {
            match h.plan.at(&g, Stamp::<SystemDomain>::from_nanos(t)) {
                Ok(iso) => {
                    let off = i * stride;
                    layout::write(&iso, layout, &mut dst[off..off + payload]);
                }
                // Stop at the first failure. A partially written buffer is worse
                // than none because it looks like data (PHASE3 §5.3's reasoning,
                // applies identically here).
                //
                // `amend_error`, **not** `set_error`: the latter blanks the slot
                // first, which would erase the edge id and the retained window
                // `record_lookup` just recorded — leaving a batch caller with
                // strictly less information than the equivalent single call, the
                // exact loss §3.3 exists to prevent. Found by review.
                //
                // The index goes in `frame_b`, which no lookup error uses, so a
                // caller learns *which element* failed as well as why, and
                // `requested` keeps the stamp.
                Err(e) => {
                    let status = record_lookup(e);
                    amend_error(|d| {
                        d.frame_b = u32::try_from(i).unwrap_or(TFT_INVALID_ID);
                        if d.requested == 0 {
                            d.requested = t;
                        }
                    });
                    return status;
                }
            }
        }
        TFT_OK
    })
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
// Test-only panic hook — §6.1
// ---------------------------------------------------------------------------

/// A guarded entry point that does nothing, for measuring what [`guard`] costs.
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
            share: Arc::new(TreeShare { tree }),
        });
        // SAFETY: `out` is non-null and the caller contracts it writable.
        unsafe { core::ptr::write(out, Box::into_raw(h)) };
        TFT_OK
    })
}

// ---------------------------------------------------------------------------
// Small helpers, so every entry point reports failures identically
// ---------------------------------------------------------------------------

fn bad_handle(what: &str) -> tft_status {
    set_error(
        TFT_ERR_BAD_HANDLE,
        "handle is NULL, freed, or not a tf_tree handle",
        |_| {},
    );
    let _ = what;
    TFT_ERR_BAD_HANDLE
}

fn null_arg(what: &str) -> tft_status {
    set_error(TFT_ERR_NULL_ARG, "a required argument was NULL", |_| {});
    let _ = what;
    TFT_ERR_NULL_ARG
}

fn bad_enum(what: &str) -> tft_status {
    set_error(
        TFT_ERR_BAD_ENUM,
        "an enum argument is outside the range this build defines",
        |_| {},
    );
    let _ = what;
    TFT_ERR_BAD_ENUM
}
