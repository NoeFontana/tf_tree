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
/// [`crate::TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] puts both halves in `out_pose` as
/// one contiguous row of thirteen `f64` — `docs/API.md` §3.3's `(N, 13)` shape.
/// Its tail holds exactly the six numbers `out_twist` would receive, so a
/// caller wanting them together does not pay two buffers for it.
///
/// **That layout is not exclusive to this function.** `tft_plan_at` and
/// `tft_plan_at_many` accept it too, and both are in the *stable* header — this
/// entry point is the only way to get pose and twist into two *separate*
/// buffers, and the only one that will report a twist for a layout that carries
/// none. If the 13-element row is what you want, the stable pair is where to
/// get it, batched.
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
        // Tagged, with the handle's domain, exactly as `tft_plan_at` is: this
        // is a *query* site, so hard-coding `SystemDomain` here would leave the
        // derivatives entry point unreadable on the arenas `docs/decisions/0038`
        // makes readable everywhere else.
        let sample = match h.plan.at_with_derivatives_tagged(&g, stamp, h.domain) {
            Ok(s) => s,
            Err(e) => return record_lookup(e),
        };
        if !out_pose.is_null() {
            // SAFETY: the caller contracts `n` writable bytes at `out_pose`.
            let dst = unsafe { core::slice::from_raw_parts_mut(out_pose.cast::<u8>(), n) };
            // With `TFT_LAYOUT_QVEC7_WXYZ_TWIST6` the caller gets pose and twist
            // contiguous in a single 13-element row — `docs/API.md` §3.3's
            // `(N, 13)` shape — instead of two buffers holding the same numbers.
            // Every other layout writes the pose alone and ignores the twist,
            // which is already in `out_twist` if the caller asked for it.
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
        // **Three checks, and `frame_record` alone is none of them.**
        //
        // `ArenaView::frame_record` bounds `id` against `max_frames`, which is
        // `frame_count + 1 + frame_headroom` — not against `frame_count`. With
        // any headroom at all (and a publisher that wants runtime interning must
        // have some) the slots in between are zeroed arena memory that
        // `frame_record` happily returns: `name_len == 0`, so this used to write
        // a lone NUL and report success for a frame that does not exist.
        //
        // Reported by review, and the existing test missed it because both C
        // fixtures were built with zero headroom, which makes the two bounds
        // coincide.
        //
        //  1. `FrameId::new` rejects 0 — the root sentinel, not the first frame.
        //  2. `id <= frame_count` closes the headroom hole.
        //  3. `name_hash != 0` closes a narrower one: `FrameTable::finish`
        //     (`frame.rs`) does `frame_count.fetch_add` *before* `write_record`,
        //     so a reader that loads the count and immediately reads that id can
        //     see an all-zero record. `blake3_64` of any name — including the
        //     empty string, which hashes to `0xa6a1f9f5b94913af` — is non-zero,
        //     so a zero hash means the slot has not been written.
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

// ---------------------------------------------------------------------------
// Recovery — `docs/decisions/0044`
// ---------------------------------------------------------------------------

/// How [`tft_tree_inherit_ownership`] resolved. Mirrors `tf_tree::Inheritance`.
///
/// **A value you do not recognise means *this process is not the owner*.** The
/// Rust enum is `#[non_exhaustive]`, so a future variant can appear here, and a
/// `switch` that falls off its cases must treat that as "keep behaving as a
/// plain participant" — which is never wrong, because inheriting is an
/// escalation and not a requirement. Only `TFT_INHERITED` says otherwise.
pub type tft_inheritance = u8;

/// This process is now the owner and is serving the rendezvous.
pub const TFT_INHERITED: tft_inheritance = 0;
/// The owner is alive. Nothing was attempted.
pub const TFT_OWNER_ALIVE: tft_inheritance = 1;
/// Another survivor won the ownership byte and is binding. This process kept
/// its slot and keeps reading; it will be told again if that survivor dies too.
pub const TFT_CONTENDED: tft_inheritance = 2;
/// A read-only attachment cannot serve, so it cannot be the heir (D18).
pub const TFT_READ_ONLY: tft_inheritance = 3;
/// Nothing to inherit from: a heap tree, a frozen `.tft`, or a tree this
/// process already owns.
pub const TFT_NOT_APPLICABLE: tft_inheritance = 4;

/// Join a shared arena by name, **read-write** if asked.
///
/// **`tft_tree_open` is the whole of the frozen tier's opening surface, and it
/// is `tf_tree::open()` — read-only, name from `$TF_TREE_ARENA`.** So until this
/// existed a C or C++ consumer could only ever hold a read-only attachment, and
/// [`tft_tree_inherit_ownership`] would answer `TFT_READ_ONLY` every time: an
/// owner writes the participant table on every grant and a `PROT_READ` mapping
/// cannot, which is D18 working. The recovery entry points beside this one are
/// decoration without it, and that was found while writing their test rather
/// than while writing their record
/// ([`0044`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)).
///
/// * `name` — NULL for the environment's default, exactly as `tft_tree_open`
///   resolves it. A named arena is what a robot with more than one tree has.
/// * `read_write` — `false` is the consumer default and stays the right choice
///   for anything that only reads (D18: the MMU, not convention, is what stops a
///   consumer corrupting a robot's transform tree). Pass `true` only for a
///   process that publishes, reaps, or must be able to inherit the owner role.
///
/// **It never creates.** `CreatePolicy::Never`, because creating needs a layout
/// and there is no way to express one across this boundary — a C creator is
/// `tft_bridge_create`, which brings its own topology. A missing arena is
/// `TFT_ERR_ARENA_UNAVAILABLE`, not an empty tree.
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
/// One non-blocking `poll` of the attach socket, plus — only once that reports a
/// hangup — one `F_OFD_GETLK` on the ownership byte. So it answers *"the arena
/// has no owner"*, not *"my socket is dead"*, and a survivor that did not
/// inherit stops being told to try
/// ([`0043`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0043-owner-lost-is-a-question-about-the-owner.md)).
/// `false` for anything that is not a joined rendezvous attachment.
///
/// Pair it with [`tft_tree_inherit_ownership`] in your own loop — there is no
/// background thread and no daemon, per `0019`, so **nothing calls this for
/// you**, and an arena whose survivors never call it stays ownerless.
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
/// (`docs/PHASE2.md` §3.5).
///
/// **This is the call an all-C++/Python fleet did not have.** Until it existed,
/// an arena whose owner was `SIGKILL`ed could not be rejoined by anything: the
/// survivors keep their participant bytes, §3.4's split-brain check refuses
/// every new create with `TFT_ERR_ARENA_UNAVAILABLE`, and the only thing that
/// ends that state was a Rust method. The documented recovery was to stop every
/// attached process
/// ([`0044`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)).
///
/// Writes one of the `TFT_INHERITED` … `TFT_NOT_APPLICABLE` values. **Anything
/// but `TFT_INHERITED` means this process is not the owner, and none of them is
/// a reason to stop reading** — lookups are unaffected by ownership in every one
/// of these states, and unaffected *during* a takeover as well.
///
/// On failure the process keeps its participant slot, its byte and its mapping,
/// and gives back the ownership byte if it had taken one — so a failed attempt
/// leaves a plain participant rather than an arena with an owner that is not
/// serving.
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
        // `&self` since `0044` step 1 — the handle holds an `Arc`, and
        // `Arc::get_mut` fails whenever a plan or publisher holds a clone.
        match h.share.tree.inherit_ownership() {
            Ok(o) => {
                let code = match o {
                    tf_tree::Inheritance::Inherited => TFT_INHERITED,
                    tf_tree::Inheritance::OwnerAlive => TFT_OWNER_ALIVE,
                    tf_tree::Inheritance::Contended => TFT_CONTENDED,
                    tf_tree::Inheritance::ReadOnly => TFT_READ_ONLY,
                    // `Inheritance` is `#[non_exhaustive]`, so a variant added
                    // later lands here rather than failing to compile. Reporting
                    // it as "not applicable" is the safe reading: it is the one
                    // value that tells a caller to keep behaving as a plain
                    // participant, which is never wrong.
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
/// Both sweeps, summed: the claim leases no live process holds
/// (`Tree::reap_dead`) and the participant records whose lock bytes the kernel has
/// released (`Tree::reap_participants`). They differ in which arena table they
/// walk, and a caller in C has no basis to choose between them — the Rust
/// surface keeps them separate for a supervisor that does.
///
/// **The name overlaps a narrower Rust one on purpose, and it is worth knowing
/// which you have.** `tf_tree::Tree::reap_dead` is the *claim* sweep alone;
/// this is that plus `reap_participants`. Two functions here would be two
/// things a C caller has to learn the difference between in order to call both
/// of them every time.
///
/// **Most of the time there is nothing to do, and that is the design.** The
/// owner's socket-hangup callback already revokes a dead participant's claims
/// and frees its record, so an ordinary killed-and-restarted publisher needs no
/// reaper. Two producers have no hangup for anyone to observe — a dead **owner**,
/// and a `TreeBuilder::build_shared` participant with no socket — and this is
/// their only collector.
///
/// Returns `0` written to `out` for a read-only tree, a heap tree, or a tree
/// with no rendezvous: none of them can prove a holder is gone, and none of them
/// may write the arena.
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
