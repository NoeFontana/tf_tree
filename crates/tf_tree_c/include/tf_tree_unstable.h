/*
 * GENERATED FILE — do not edit.
 *
 * Regenerate with `cargo xtask headers`; `cargo xtask headers --check` fails if
 * this file and crates/tf_tree_c/src/ have drifted. The file is committed on
 * purpose (docs/decisions/0007): an ABI change should be a diff somebody
 * approves, not something that materialises during a build.
 */

/*
 * tf_tree — the UNSTABLE C API.  docs/PHASE4.md §3.1.
 *
 * NOTHING HERE IS COVERED BY ANY COMPATIBILITY PROMISE.  A symbol in this
 * header may change signature, change meaning, or disappear in a patch
 * release.  It exists so that work which needs derivatives or introspection
 * today is not blocked on freezing an interface a year of use has not yet
 * argued with.
 *
 * You must #define TFT_ENABLE_UNSTABLE before including this file.  That is a
 * speed bump, deliberately: it means nobody reaches these symbols by accident
 * and then reports their removal as a regression.
 */
#ifndef TFT_ENABLE_UNSTABLE
#error "tf_tree_unstable.h has no stability guarantee; #define TFT_ENABLE_UNSTABLE to accept that"
#endif

#include "tf_tree.h"

#ifndef TF_TREE_UNSTABLE_H
#define TF_TREE_UNSTABLE_H

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Bytes one twist occupies: `[ωx ωy ωz vx vy vz]`, `f64`, rad/s and m/s.
 *
 * There is deliberately no `tft_twist_layout` enum. A twist is a 6-vector in
 * one universally agreed order (`tf_tree_math::twist`'s convention, which is
 * also Sophus's and Pinocchio's), so the quaternion-order trap §3.5 exists for
 * has no analogue here — and inventing a second layout enum would create one.
 */
#define TFT_TWIST_BYTES (6 * 8)

/**
 * Evaluate `plan` at `stamp`, reporting the pose **and its first derivative**.
 *
 * `out_pose` receives `tft_layout_size(layout)` bytes; `out_twist` receives
 * [`TFT_TWIST_BYTES`] as `[ωx ωy ωz vx vy vz]`. Either may be NULL, in which
 * case that half is not written — asking for only the twist is a real request
 * and costs the same as asking for both.
 *
 * # The twist is in the plan's *source* frame
 *
 * `plan(target, source)` evaluates `T_target_source`, and the body twist of
 * that transform is expressed in the **source** frame, not the target. For
 * `plan("map", "base_link")` — the usual direction — the reported twist is the
 * robot's own velocity in its own frame, which is almost always what a
 * consumer wants and almost never what they expect the first time.
 *
 * # Errors
 *
 * * `TFT_ERR_NO_DERIVATIVES` — an edge on the path interpolates with
 *   `LerpSlerp`, whose body twist is an artifact of the interpolant rather than
 *   of the motion, so it is refused rather than reported (§2.4).
 * * `TFT_ERR_NO_SEGMENT` — an edge has a pose at this stamp but no segment to
 *   differentiate: one retained sample, or two with equal stamps.
 *
 * # Safety
 *
 * `plan` must be a live handle. `out_pose`, when non-NULL, must point to at
 * least `tft_layout_size(layout)` writable bytes; `out_twist`, when non-NULL,
 * to at least [`TFT_TWIST_BYTES`].
 */
tft_status tft_plan_at_with_derivatives(const tft_plan *plan,
                                        int64_t stamp,
                                        tft_layout layout,
                                        void *out_pose,
                                        double *out_twist);

/**
 * How many frames this tree has declared, including tombstoned ones.
 *
 * **Valid frame ids are `1 ..= tft_tree_frame_count()`.** Ids are append-only
 * and never recycled (`docs/PROJECT.md` §5), so iterating that range visits
 * every frame that has ever existed.
 *
 * # Why ids start at 1
 *
 * `FrameId` is a `NonZeroU32` so that `Option<FrameId>` costs four bytes and
 * index `0` can mean "root / no parent". Passing `0` to
 * [`tft_tree_frame_name`] is therefore `TFT_ERR_UNKNOWN_FRAME`, not the first
 * frame — and a C loop written `for (i = 0; i < n; i++)` gets one error and
 * then misses the last frame, which is why this says so here rather than
 * leaving it to be discovered.
 *
 * Returns `0` for a NULL or dead handle, which is indistinguishable from an
 * empty tree — deliberately, because there is no error channel on a function
 * that returns a count and adding one would put a `tft_status` out-parameter on
 * the simplest call in the header.
 *
 * # Safety
 *
 * `tree` must be NULL or a live handle.
 */
uint32_t tft_tree_frame_count(const tft_tree *tree);

/**
 * How many edges this tree has declared, including tombstoned ones.
 *
 * **Valid edge ids are `1 ..= tft_tree_edge_count()`** — the same convention as
 * [`tft_tree_frame_count`], deliberately, because a C caller should not have to
 * remember two.
 *
 * # This is not the arena header's field
 *
 * The header stores `declared + 1`: `TreeBuilder` reserves index `0` and
 * `tf_tree doctor` iterates `1..edge_count` to skip it. The two id spaces
 * therefore agree from outside while disagreeing in the header, and *this
 * function is where they are reconciled* — it subtracts the reservation so the
 * count means the same thing for edges as it does for frames.
 *
 * The first version returned the header field raw. Its test asserted 3 for a
 * three-edge tree and got 4, which is how the reservation was found — from
 * outside, exactly where a C consumer would have found it. `error.rs`'s
 * `EdgeId` doc still claims edge 0 is an ordinary slot; the builder disagrees,
 * and the builder is what runs.
 *
 * # Safety
 *
 * `tree` must be NULL or a live handle.
 */
uint32_t tft_tree_edge_count(const tft_tree *tree);

/**
 * Copy frame `id`'s name into `buf` as a NUL-terminated string.
 *
 * Returns `TFT_ERR_BUFFER_TOO_SMALL` — **without writing anything** — when the
 * name plus its NUL does not fit, and sets the error detail's `requested` to
 * the number of bytes needed. A truncated frame name is worse than no name: it
 * is a *different, plausible* frame name, and this library's whole argument is
 * that plausible wrong answers are the expensive kind.
 *
 * **The arena stores at most 48 bytes of a frame name** (`FrameRecord::name`),
 * so a longer declared name is already truncated before this function sees it
 * and what you get back is the stored form. Frames are still *identified* by a
 * hash of the full name, so two long names sharing a 48-byte prefix are
 * distinct frames that report the same string here. That is a property of the
 * Phase 1 layout, not of this function; it is documented rather than papered
 * over because a diagnostic that quietly conflates two frames is worse than one
 * that admits it. `64` bytes is enough for any name the arena can hold.
 *
 * # Safety
 *
 * `tree` must be a live handle. `buf` must point to `buf_len` writable bytes.
 */
tft_status tft_tree_frame_name(const tft_tree *tree, uint32_t id, char *buf, size_t buf_len);

/**
 * Copy this tree's 16-byte arena instance UUID into `out`.
 *
 * Two processes holding the same UUID are looking at the same arena instance.
 * It is what distinguishes "we both attached to the robot's tree" from "we each
 * created our own", which otherwise look identical from inside.
 *
 * # A private in-process arena has no instance UUID
 *
 * The UUID is written when a *shared* arena is created (`docs/PHASE2.md` §1,
 * A1); a heap arena leaves the field zero. Returning those zeros would be
 * actively harmful: two unrelated private trees would compare equal and a
 * caller would conclude they had joined the same arena. So this returns
 * `TFT_ERR_NO_DATA` and **writes nothing** when the arena is not shared, which
 * is a fact the caller can act on rather than a coincidence they cannot detect.
 *
 * # Safety
 *
 * `tree` must be a live handle. `out` must point to 16 writable bytes.
 */
tft_status tft_tree_instance_uuid(const tft_tree *tree, uint8_t *out);

#ifdef __cplusplus
}  /* extern "C" */
#endif

#endif  /* TF_TREE_UNSTABLE_H */
