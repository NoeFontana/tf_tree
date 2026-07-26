//! The **unstable** tier of the C ABI — `docs/PHASE4.md` §3.1.
//!
//! Everything here is generated into `tf_tree_unstable.h`, which carries **no
//! compatibility guarantee at all** and requires `#define TFT_ENABLE_UNSTABLE`
//! before inclusion. §3.1's reasoning: the stable header is the first ABI freeze
//! in the project and is permanent, so it stays at roughly thirty functions and
//! anything a C++ user does not need in the hot path waits until Phase 7 has
//! told us what is actually used.
//!
//! What that means concretely, and it is not a formality: **a symbol in this
//! module may change signature, change meaning, or disappear in a patch
//! release.** The `#define` is a speed bump so that nobody reaches this by
//! accident and then reports the removal as a regression.
//!
//! Two families live here today:
//!
//! * **Derivatives** (§2). `at_with_derivatives` is younger than the rest of the
//!   engine and its twist convention — body-frame, in the plan's *source* frame —
//!   is exactly the kind of thing a year of use might argue with.
//! * **Introspection.** Frame and edge counts, frame names, the instance UUID.
//!   Diagnostic surface, needed by `tf_tree top` and by anything that wants to
//!   render a tree, and none of it belongs in a frozen hot-path header.

use core::ffi::{c_char, c_void};

use tf_tree::{Stamp, SystemDomain};

use crate::error::{guard, record_lookup, set_error};
use crate::layout;
use crate::{bad_enum, bad_handle, null_arg};
use crate::{tft_plan, tft_status, tft_tree, TFT_ERR_BUFFER_TOO_SMALL, TFT_OK};

/// Bytes one twist occupies: `[ωx ωy ωz vx vy vz]`, `f64`, rad/s and m/s.
///
/// There is deliberately no `tft_twist_layout` enum. A twist is a 6-vector in
/// one universally agreed order (`tf_tree_math::twist`'s convention, which is
/// also Sophus's and Pinocchio's), so the quaternion-order trap §3.5 exists for
/// has no analogue here — and inventing a second layout enum would create one.
pub const TFT_TWIST_BYTES: usize = 6 * 8;

/// Evaluate `plan` at `stamp`, reporting the pose **and its first derivative**.
///
/// `out_pose` receives `tft_layout_size(layout)` bytes; `out_twist` receives
/// [`TFT_TWIST_BYTES`] as `[ωx ωy ωz vx vy vz]`. Either may be NULL, in which
/// case that half is not written — asking for only the twist is a real request
/// and costs the same as asking for both.
///
/// # The twist is in the plan's *source* frame
///
/// `plan(target, source)` evaluates `T_target_source`, and the body twist of
/// that transform is expressed in the **source** frame, not the target. For
/// `plan("map", "base_link")` — the usual direction — the reported twist is the
/// robot's own velocity in its own frame, which is almost always what a
/// consumer wants and almost never what they expect the first time.
///
/// # Errors
///
/// * `TFT_ERR_NO_DERIVATIVES` — an edge on the path interpolates with
///   `LerpSlerp`, whose body twist is an artifact of the interpolant rather than
///   of the motion, so it is refused rather than reported (§2.4).
/// * `TFT_ERR_NO_SEGMENT` — an edge has a pose at this stamp but no segment to
///   differentiate: one retained sample, or two with equal stamps.
///
/// # Safety
///
/// `plan` must be a live handle. `out_pose`, when non-NULL, must point to at
/// least `tft_layout_size(layout)` writable bytes; `out_twist`, when non-NULL,
/// to at least [`TFT_TWIST_BYTES`].
#[no_mangle]
pub unsafe extern "C" fn tft_plan_at_with_derivatives(
    plan: *const tft_plan,
    stamp: i64,
    layout: crate::tft_layout,
    out_pose: *mut c_void,
    out_twist: *mut f64,
) -> tft_status {
    guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { crate::check_plan(plan) } {
            return bad_handle("tft_plan");
        }
        if out_pose.is_null() && out_twist.is_null() {
            return null_arg("out_pose and out_twist are both NULL");
        }
        let n = match layout::payload_bytes(layout) {
            Some(n) => n,
            // An unknown layout is an error even when `out_pose` is NULL: a
            // caller who passes a discriminant this build does not define has a
            // header/library mismatch, and telling them so on the call where
            // they would not have used the result anyway is still telling them.
            None => return bad_enum("layout"),
        };
        // SAFETY: `check_plan` confirmed the magic word.
        let h = unsafe { &*plan };
        let g = h.share.tree.guard();
        let sample = match h
            .plan
            .at_with_derivatives(&g, Stamp::<SystemDomain>::from_nanos(stamp))
        {
            Ok(s) => s,
            Err(e) => return record_lookup(e),
        };
        if !out_pose.is_null() {
            // SAFETY: the caller contracts `n` writable bytes at `out_pose`.
            let dst = unsafe { core::slice::from_raw_parts_mut(out_pose.cast::<u8>(), n) };
            layout::write(&sample.pose, layout, dst);
        }
        if !out_twist.is_null() {
            let v = sample.twist;
            let vals = [v.omega.x, v.omega.y, v.omega.z, v.v.x, v.v.y, v.v.z];
            // SAFETY: the caller contracts `TFT_TWIST_BYTES` writable bytes,
            // which is exactly six `f64`, and `f64` has no alignment stronger
            // than the pointer type already promises.
            unsafe { core::ptr::copy_nonoverlapping(vals.as_ptr(), out_twist, 6) };
        }
        TFT_OK
    })
}

/// How many frames this tree has declared, including tombstoned ones.
///
/// **Valid frame ids are `1 ..= tft_tree_frame_count()`.** Ids are append-only
/// and never recycled (`docs/PROJECT.md` §5), so iterating that range visits
/// every frame that has ever existed.
///
/// # Why ids start at 1
///
/// `FrameId` is a `NonZeroU32` so that `Option<FrameId>` costs four bytes and
/// index `0` can mean "root / no parent". Passing `0` to
/// [`tft_tree_frame_name`] is therefore `TFT_ERR_UNKNOWN_FRAME`, not the first
/// frame — and a C loop written `for (i = 0; i < n; i++)` gets one error and
/// then misses the last frame, which is why this says so here rather than
/// leaving it to be discovered.
///
/// Returns `0` for a NULL or dead handle, which is indistinguishable from an
/// empty tree — deliberately, because there is no error channel on a function
/// that returns a count and adding one would put a `tft_status` out-parameter on
/// the simplest call in the header.
///
/// # Safety
///
/// `tree` must be NULL or a live handle.
#[no_mangle]
pub unsafe extern "C" fn tft_tree_frame_count(tree: *const tft_tree) -> u32 {
    // SAFETY: validated before any field access.
    if !unsafe { crate::check_tree(tree) } {
        return 0;
    }
    // SAFETY: `check_tree` confirmed the magic word.
    let h = unsafe { &*tree };
    h.share
        .tree
        .arena_view()
        .header()
        .frame_count
        .load(core::sync::atomic::Ordering::Acquire)
}

/// How many edges this tree has declared, including tombstoned ones.
///
/// **Valid edge ids are `1 ..= tft_tree_edge_count()`** — the same convention as
/// [`tft_tree_frame_count`], deliberately, because a C caller should not have to
/// remember two.
///
/// # This is not the arena header's field
///
/// The header stores `declared + 1`: `TreeBuilder` reserves index `0` and
/// `tf_tree doctor` iterates `1..edge_count` to skip it. The two id spaces
/// therefore agree from outside while disagreeing in the header, and *this
/// function is where they are reconciled* — it subtracts the reservation so the
/// count means the same thing for edges as it does for frames.
///
/// The first version returned the header field raw. Its test asserted 3 for a
/// three-edge tree and got 4, which is how the reservation was found — from
/// outside, exactly where a C consumer would have found it. `error.rs`'s
/// `EdgeId` doc still claims edge 0 is an ordinary slot; the builder disagrees,
/// and the builder is what runs.
///
/// # Safety
///
/// `tree` must be NULL or a live handle.
#[no_mangle]
pub unsafe extern "C" fn tft_tree_edge_count(tree: *const tft_tree) -> u32 {
    // SAFETY: validated before any field access.
    if !unsafe { crate::check_tree(tree) } {
        return 0;
    }
    // SAFETY: `check_tree` confirmed the magic word.
    let h = unsafe { &*tree };
    h.share
        .tree
        .arena_view()
        .header()
        .edge_count
        .load(core::sync::atomic::Ordering::Acquire)
        // Never underflows: a built arena always stores at least the sentinel,
        // and `saturating_sub` makes an un-built one report 0 rather than wrap
        // to 4 billion edges.
        .saturating_sub(1)
}

/// Copy frame `id`'s name into `buf` as a NUL-terminated string.
///
/// Returns `TFT_ERR_BUFFER_TOO_SMALL` — **without writing anything** — when the
/// name plus its NUL does not fit, and sets the error detail's `requested` to
/// the number of bytes needed. A truncated frame name is worse than no name: it
/// is a *different, plausible* frame name, and this library's whole argument is
/// that plausible wrong answers are the expensive kind.
///
/// **The arena stores at most 48 bytes of a frame name** (`FrameRecord::name`),
/// so a longer declared name is already truncated before this function sees it
/// and what you get back is the stored form. Frames are still *identified* by a
/// hash of the full name, so two long names sharing a 48-byte prefix are
/// distinct frames that report the same string here. That is a property of the
/// Phase 1 layout, not of this function; it is documented rather than papered
/// over because a diagnostic that quietly conflates two frames is worse than one
/// that admits it. `64` bytes is enough for any name the arena can hold.
///
/// # Safety
///
/// `tree` must be a live handle. `buf` must point to `buf_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn tft_tree_frame_name(
    tree: *const tft_tree,
    id: u32,
    buf: *mut c_char,
    buf_len: usize,
) -> tft_status {
    guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { crate::check_tree(tree) } {
            return bad_handle("tft_tree");
        }
        if buf.is_null() {
            return null_arg("buf");
        }
        // SAFETY: `check_tree` confirmed the magic word.
        let h = unsafe { &*tree };
        let view = h.share.tree.arena_view();
        // `FrameId::new` rejects 0 — the root sentinel — so an id of 0 lands
        // here as "unknown frame", which is the honest answer.
        let Some(rec) = tf_tree::FrameId::new(id).and_then(|f| view.frame_record(f)) else {
            set_error(
                crate::TFT_ERR_UNKNOWN_FRAME,
                "no such frame id in this tree",
                |d| d.frame_a = id,
            );
            return crate::TFT_ERR_UNKNOWN_FRAME;
        };
        // `FrameRecord` stores the name NUL-padded in 48 bytes with an explicit
        // length, and has no accessor — reading it here rather than adding one
        // to `tf_tree_core` keeps the unstable tier from widening the engine's
        // API for a diagnostic.
        let n = usize::from(rec.name_len).min(rec.name.len());
        let name = core::str::from_utf8(&rec.name[..n]).unwrap_or("");
        let need = name.len() + 1;
        if buf_len < need {
            set_error(
                TFT_ERR_BUFFER_TOO_SMALL,
                "the frame name does not fit; a truncated name is a different name",
                |d| {
                    d.frame_a = id;
                    d.requested = i64::try_from(need).unwrap_or(i64::MAX);
                },
            );
            return TFT_ERR_BUFFER_TOO_SMALL;
        }
        // SAFETY: `buf` has `buf_len >= need` writable bytes by the check above.
        let dst = unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), need) };
        dst[..name.len()].copy_from_slice(name.as_bytes());
        dst[name.len()] = 0;
        TFT_OK
    })
}

/// Copy this tree's 16-byte arena instance UUID into `out`.
///
/// Two processes holding the same UUID are looking at the same arena instance.
/// It is what distinguishes "we both attached to the robot's tree" from "we each
/// created our own", which otherwise look identical from inside.
///
/// # A private in-process arena has no instance UUID
///
/// The UUID is written when a *shared* arena is created (`docs/PHASE2.md` §1,
/// A1); a heap arena leaves the field zero. Returning those zeros would be
/// actively harmful: two unrelated private trees would compare equal and a
/// caller would conclude they had joined the same arena. So this returns
/// `TFT_ERR_NO_DATA` and **writes nothing** when the arena is not shared, which
/// is a fact the caller can act on rather than a coincidence they cannot detect.
///
/// # Safety
///
/// `tree` must be a live handle. `out` must point to 16 writable bytes.
#[no_mangle]
pub unsafe extern "C" fn tft_tree_instance_uuid(tree: *const tft_tree, out: *mut u8) -> tft_status {
    guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { crate::check_tree(tree) } {
            return bad_handle("tft_tree");
        }
        if out.is_null() {
            return null_arg("out");
        }
        // SAFETY: `check_tree` confirmed the magic word.
        let h = unsafe { &*tree };
        if !h.share.tree.is_shared() {
            set_error(
                crate::TFT_ERR_NO_DATA,
                "a private in-process arena has no instance uuid (it is not shared)",
                |_| {},
            );
            return crate::TFT_ERR_NO_DATA;
        }
        let uuid = h.share.tree.instance_uuid();
        // SAFETY: the caller contracts 16 writable bytes at `out`.
        unsafe { core::ptr::copy_nonoverlapping(uuid.as_ptr(), out, 16) };
        TFT_OK
    })
}
