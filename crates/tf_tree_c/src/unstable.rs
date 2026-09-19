//! The **unstable** tier of the C ABI — `docs/PHASE4.md` §3.1.
//!
//! Generated into `tf_tree_unstable.h`, which carries no compatibility
//! guarantee and requires `#define TFT_ENABLE_UNSTABLE`: a symbol here may
//! change or disappear in a patch release. It holds derivatives (§2) and
//! introspection (counts, frame names, the instance UUID) plus recovery.

use core::ffi::{c_char, c_void};

use crate::error::{guard, record_lookup, set_error};
use crate::layout;
use crate::{bad_enum, bad_handle, null_arg};
use crate::{tft_plan, tft_status, tft_tree, TFT_ERR_BUFFER_TOO_SMALL, TFT_OK};

/// Bytes one twist occupies: `[ωx ωy ωz vx vy vz]`, `f64`, rad/s and m/s.
/// There is no `tft_twist_layout` enum: the 6-vector order is universal.
pub const TFT_TWIST_BYTES: usize = 6 * 8;

/// Evaluate `plan` at `stamp`, reporting the pose **and its first derivative**.
///
/// `out_pose` receives `tft_layout_size(layout)` bytes; `out_twist` receives
/// [`TFT_TWIST_BYTES`]. Either may be NULL, and that half is then not written.
///
/// [`crate::TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] puts both halves in `out_pose`
/// (`docs/API.md` §3.3's `(N, 13)`); the stable `tft_plan_at` and
/// `tft_plan_at_many` accept it too.
///
/// The twist is a body twist in the plan's **source** frame: for
/// `plan("map", "base_link")` it is the robot's own velocity in its own frame.
///
/// # Errors
///
/// * `TFT_ERR_NO_DERIVATIVES` — an edge on the path interpolates with
///   `LerpSlerp` (§2.4).
/// * `TFT_ERR_NO_SEGMENT` — an edge has a pose at this stamp but no segment to
///   differentiate.
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
            // An unknown layout is an error even when `out_pose` is NULL.
            None => return bad_enum("layout"),
        };
        // SAFETY: `check_plan` confirmed the magic word.
        let h = unsafe { &*plan };
        let g = h.share.tree.guard();
        // Tagged with the handle's domain, as `tft_plan_at` is (`docs/decisions/0038`).
        let sample = match h.plan.at_with_derivatives_tagged(&g, stamp, h.domain) {
            Ok(s) => s,
            Err(e) => return record_lookup(e),
        };
        if !out_pose.is_null() {
            // SAFETY: the caller contracts `n` writable bytes at `out_pose`.
            let dst = unsafe { core::slice::from_raw_parts_mut(out_pose.cast::<u8>(), n) };
            // The twist-carrying layout writes both halves; every other layout
            // writes the pose alone.
            if layout::carries_twist(layout) {
                layout::write_twist6(&sample.pose, &sample.twist, dst);
            } else {
                layout::write(&sample.pose, layout, dst);
            }
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
/// Valid frame ids are `1 ..= tft_tree_frame_count()` (append-only, never
/// recycled; `docs/PROJECT.md` §5). Id `0` is the root sentinel, so passing it to
/// [`tft_tree_frame_name`] is `TFT_ERR_UNKNOWN_FRAME`.
///
/// Returns `0` for a NULL or dead handle, indistinguishable from an empty tree:
/// a count has no error channel.
///
/// # Safety
///
/// `tree` must be NULL or a live handle.
#[no_mangle]
pub unsafe extern "C" fn tft_tree_frame_count(tree: *const tft_tree) -> u32 {
    crate::error::guard_value(0, || {
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
    })
}

/// How many edges this tree has declared, including tombstoned ones.
///
/// Valid edge ids are `1 ..= tft_tree_edge_count()`, as for
/// [`tft_tree_frame_count`]. The arena header stores `declared + 1`
/// (`TreeBuilder` reserves index 0); this function subtracts the reservation.
///
/// # Safety
///
/// `tree` must be NULL or a live handle.
#[no_mangle]
pub unsafe extern "C" fn tft_tree_edge_count(tree: *const tft_tree) -> u32 {
    crate::error::guard_value(0, || {
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
            // A built arena stores at least the sentinel; `saturating_sub`
            // makes an un-built one report 0.
            .saturating_sub(1)
    })
}

/// Copy frame `id`'s name into `buf` as a NUL-terminated string.
///
/// Returns `TFT_ERR_BUFFER_TOO_SMALL` without writing anything when the name
/// plus its NUL does not fit, and sets the error detail's `requested` to the
/// bytes needed.
///
/// The arena stores at most 48 bytes of a frame name (`FrameRecord::name`), so
/// two longer names sharing a 48-byte prefix are distinct frames reporting the
/// same string. 64 bytes fits any name the arena can hold.
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
        // Three checks; `frame_record` alone is none of them:
        //  1. `FrameId::new` rejects 0, the root sentinel.
        //  2. `id <= frame_count`: `frame_record` bounds against `max_frames`,
        //     which includes zeroed headroom slots.
        //  3. `name_hash != 0`: `FrameTable::finish` bumps `frame_count`
        //     before `write_record`, so a reader can see an all-zero record; no
        //     name, even "", hashes to 0.
        let count = view
            .header()
            .frame_count
            .load(core::sync::atomic::Ordering::Acquire);
        let rec = tf_tree::FrameId::new(id)
            .filter(|_| id <= count)
            .and_then(|f| view.frame_record(f))
            .filter(|r| r.name_hash != 0);
        let Some(rec) = rec else {
            set_error(
                crate::TFT_ERR_UNKNOWN_FRAME,
                "no such frame id in this tree (ids run 1..=tft_tree_frame_count)",
                |d| d.frame_a = id,
            );
            return crate::TFT_ERR_UNKNOWN_FRAME;
        };
        // `FrameRecord` has no name accessor; read the bytes here rather than
        // widen the engine's API.
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

/// Copy this tree's 16-byte arena instance UUID into `out`. Two processes with
/// the same UUID are looking at the same arena instance.
///
/// A heap arena has no UUID (`docs/PHASE2.md` §1, A1): this returns
/// `TFT_ERR_NO_DATA` and writes nothing.
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

// Recovery — `docs/decisions/0044`

/// How [`tft_tree_inherit_ownership`] resolved. Mirrors `tf_tree::Inheritance`.
///
/// A value you do not recognise means this process is not the owner; only
/// `TFT_INHERITED` says otherwise.
pub type tft_inheritance = u8;

/// This process is now the owner and is serving the rendezvous.
pub const TFT_INHERITED: tft_inheritance = 0;
/// `tft_tree_owner_lost` would have answered `false`, so nothing was attempted.
/// Not final while it keeps answering `true` (`0057` Decision 3): call again.
pub const TFT_OWNER_ALIVE: tft_inheritance = 1;
/// The ownership byte was taken by another survivor or a fresh open. This
/// process kept its slot and keeps reading. Not final: call again.
pub const TFT_CONTENDED: tft_inheritance = 2;
/// A read-only attachment cannot serve, so it cannot be the heir (D18).
pub const TFT_READ_ONLY: tft_inheritance = 3;
/// Nothing to inherit from: a heap tree, a frozen `.tft`, or a tree this
/// process already owns.
pub const TFT_NOT_APPLICABLE: tft_inheritance = 4;

/// Join a shared arena by name, **read-write** if asked (`0044`).
///
/// * `name` — NULL for the environment's default, as `tft_tree_open` resolves it.
/// * `read_write` — `false` is the consumer default (D18); pass `true` only for a
///   process that publishes, reaps, or must be able to inherit the owner role.
///
/// Never creates (`CreatePolicy::Never`): a missing arena is
/// `TFT_ERR_ARENA_UNAVAILABLE`. A C creator is `tft_bridge_create`.
///
/// # Safety
///
/// `name` must be NULL or a NUL-terminated string valid for the call; `out` must
/// point to a writable `*mut tft_tree`.
#[cfg(all(feature = "shm", target_os = "linux"))]
#[no_mangle]
pub unsafe extern "C" fn tft_tree_open_named(
    name: *const c_char,
    read_write: bool,
    out: *mut *mut tft_tree,
) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("tft_tree_open_named");
        }
        let mut open = tf_tree::Open::new().mode(if read_write {
            tf_tree::AttachMode::ReadWrite
        } else {
            tf_tree::AttachMode::ReadOnly
        });
        if !name.is_null() {
            // SAFETY: the caller contracts a NUL-terminated string.
            let raw = unsafe { core::ffi::CStr::from_ptr(name) };
            let Ok(text) = raw.to_str() else {
                set_error(
                    crate::TFT_ERR_BAD_CONFIG,
                    "arena name is not valid UTF-8",
                    |_| {},
                );
                return crate::TFT_ERR_BAD_CONFIG;
            };
            match open.name(text) {
                Ok(o) => open = o,
                Err(e) => {
                    set_error(
                        crate::TFT_ERR_BAD_CONFIG,
                        &format!("arena name refused: {e}"),
                        |_| {},
                    );
                    return crate::TFT_ERR_BAD_CONFIG;
                }
            }
        }
        match open.open() {
            Ok(tree) => {
                let h = Box::new(tft_tree {
                    magic: crate::MAGIC_TREE,
                    share: std::sync::Arc::new(crate::TreeShare {
                        tree: std::sync::Arc::new(tree),
                    }),
                });
                // SAFETY: the caller contracts a writable slot at `out`.
                unsafe { out.write(Box::into_raw(h)) };
                TFT_OK
            }
            Err(e) => {
                set_error(
                    crate::TFT_ERR_ARENA_UNAVAILABLE,
                    &format!("could not open the arena: {e}"),
                    |_| {},
                );
                crate::TFT_ERR_ARENA_UNAVAILABLE
            }
        }
    })
}

/// Has the process that owns this arena gone away (`docs/PHASE2.md` §3.5)?
///
/// One non-blocking `poll` of the attach socket, plus, once that reports a
/// hangup, one `F_OFD_GETLK` on the ownership byte: it answers "the arena has no
/// owner" (`0043`), and is `false` for anything that is not a joined rendezvous
/// attachment. A dying owner is seen at the end of its exit (`0057`;
/// `docs/PHASE2.md` §3.5 and §3.7 step 9, NORMATIVE).
///
/// Nothing calls this for you (`0019`): pair it with
/// [`tft_tree_inherit_ownership`] in your own loop.
///
/// # Safety
///
/// `tree` must be NULL or a live handle; `out` must be a writable `bool`.
#[cfg(all(feature = "shm", target_os = "linux"))]
#[no_mangle]
pub unsafe extern "C" fn tft_tree_owner_lost(tree: *const tft_tree, out: *mut bool) -> tft_status {
    guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { crate::check_tree(tree) } {
            return bad_handle("tft_tree_owner_lost");
        }
        if out.is_null() {
            return null_arg("tft_tree_owner_lost");
        }
        // SAFETY: `check_tree` confirmed the magic word.
        let h = unsafe { &*tree };
        let lost = h.share.tree.owner_lost();
        // SAFETY: the caller contracts a writable `bool` at `out`.
        unsafe { out.write(lost) };
        TFT_OK
    })
}

/// Inherit the owner role from a departed owner and begin serving
/// (`docs/PHASE2.md` §3.5; `0044`).
///
/// Writes one of the `TFT_INHERITED` … `TFT_NOT_APPLICABLE` values; none is a
/// reason to stop reading. On failure the process stays a plain participant.
///
/// # Safety
///
/// `tree` must be NULL or a live handle; `out` must be a writable
/// `tft_inheritance`.
#[cfg(all(feature = "shm", target_os = "linux"))]
#[no_mangle]
pub unsafe extern "C" fn tft_tree_inherit_ownership(
    tree: *const tft_tree,
    out: *mut tft_inheritance,
) -> tft_status {
    guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { crate::check_tree(tree) } {
            return bad_handle("tft_tree_inherit_ownership");
        }
        if out.is_null() {
            return null_arg("tft_tree_inherit_ownership");
        }
        // SAFETY: `check_tree` confirmed the magic word.
        let h = unsafe { &*tree };
        match h.share.tree.inherit_ownership() {
            Ok(o) => {
                let code = match o {
                    tf_tree::Inheritance::Inherited => TFT_INHERITED,
                    tf_tree::Inheritance::OwnerAlive => TFT_OWNER_ALIVE,
                    tf_tree::Inheritance::Contended => TFT_CONTENDED,
                    tf_tree::Inheritance::ReadOnly => TFT_READ_ONLY,
                    // `Inheritance` is `#[non_exhaustive]`; "not applicable" is the safe reading.
                    _ => TFT_NOT_APPLICABLE,
                };
                // SAFETY: the caller contracts a writable byte at `out`.
                unsafe { out.write(code) };
                TFT_OK
            }
            Err(e) => {
                set_error(
                    crate::TFT_ERR_ARENA_UNAVAILABLE,
                    &format!("could not inherit the owner role: {e}"),
                    |_| {},
                );
                crate::TFT_ERR_ARENA_UNAVAILABLE
            }
        }
    })
}

/// Collect what dead participants left behind, and report how many records were
/// freed.
///
/// Both sweeps, summed: claim leases no live process holds (`Tree::reap_dead`)
/// and participant records whose lock bytes the kernel has released
/// (`Tree::reap_participants`).
///
/// Usually there is nothing to do: the owner's hangup callback already revokes a
/// dead participant's claims. A dead owner (or any participant killed after it,
/// which nobody watches) has no hangup, and this is the collector
/// (`docs/PHASE2.md` §0.0 *Reaping (§6.3)*; §6.3: "reaping must not be
/// owner-only").
///
/// The other unwatched producer, a `build_shared` participant with no socket, is
/// **out of contract** (`docs/decisions/0031-the-participant-record-with-no-byte.md`,
/// `0028` step 0b): sweeping in a process tree that contains one frees the
/// records and claims of *live* publishers.
///
/// Writes `0` for a read-only tree, a heap tree, or a tree with no rendezvous.
///
/// # Safety
///
/// `tree` must be NULL or a live handle; `out` must be a writable `uint32_t`.
#[cfg(all(feature = "shm", target_os = "linux"))]
#[no_mangle]
pub unsafe extern "C" fn tft_tree_reap_dead(tree: *const tft_tree, out: *mut u32) -> tft_status {
    guard(|| {
        // SAFETY: validated before any field access.
        if !unsafe { crate::check_tree(tree) } {
            return bad_handle("tft_tree_reap_dead");
        }
        if out.is_null() {
            return null_arg("tft_tree_reap_dead");
        }
        // SAFETY: `check_tree` confirmed the magic word.
        let h = unsafe { &*tree };
        let n = h.share.tree.reap_dead() + h.share.tree.reap_participants();
        // SAFETY: the caller contracts a writable `u32` at `out`.
        unsafe { out.write(u32::try_from(n).unwrap_or(u32::MAX)) };
        TFT_OK
    })
}
