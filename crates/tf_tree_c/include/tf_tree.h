/*
 * GENERATED FILE — do not edit.
 *
 * Regenerate with `cargo xtask headers`; `cargo xtask headers --check` fails if
 * this file and crates/tf_tree_c/src/ have drifted. The file is committed on
 * purpose (docs/decisions/0007): an ABI change should be a diff somebody
 * approves, not something that materialises during a build.
 */

/*
 * tf_tree — the stable C API.  docs/PHASE4.md §3.
 *
 * Every function returns tft_status: 0 on success, negative on failure.
 * On failure, tft_last_error() fills a tft_error with structured detail
 * for THIS THREAD, valid until the next tf_tree call on this thread.
 * That thread-local lifetime is the single most common C-API misuse, so
 * it is stated here and not only in the manual.
 *
 * No entry point can abort your process: every one wraps its body in a
 * panic guard (§3.4), so an internal bug becomes TFT_ERR_INTERNAL.
 */
#ifndef TF_TREE_H
#define TF_TREE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Opaque handles — docs/PHASE4.md §3.2.
 *
 *   tft_tree       Send + Sync   shareable across threads
 *   tft_plan       Send + Sync   shareable, immutable
 *   tft_publisher  Send + !Sync  ONE THREAD AT A TIME
 *
 * The publisher's thread affinity is not advisory: a debug build of the library
 * abort()s if you use one from a thread other than the one that claimed it, and
 * a release build returns TFT_ERR_WRONG_THREAD.
 */
typedef struct tft_tree tft_tree;
typedef struct tft_plan tft_plan;
typedef struct tft_publisher tft_publisher;

/**
 * Major ABI version; **must match exactly** between the compiled-against header and the linked library.
 */
#define TFT_ABI_VERSION_MAJOR 0

/**
 * Minor ABI version; the runtime's may be **≥** the compiled-against value (§3.6). Every bump is an
 * append.
 *
 * * `1` → `2`: `tft_bridge_note_time_jump`; fields appended to `tft_bridge_sample` and
 *   `tft_bridge_outcome`.
 * * `2` → `3`: [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] (`docs/API.md` §3.3).
 * * `3` → `4`: [`tft_stamp_from_parts`], [`tft_stamp_from_timespec`], [`TFT_ERR_BAD_STAMP`]
 *   (`docs/API.md` §5.1).
 * * `4` → `5`: `tft_bridge_options::arena_name`, [`TFT_ERR_ARENA_UNAVAILABLE`]
 *   (`docs/decisions/0015`).
 * * `5` → `6`: [`tft_plan_create_in_domain`] (`docs/decisions/0038`).
 * * `6` → `7`: [`tft_plan_at_extrapolating`], [`tft_extrap_policy`], [`tft_extrapolated`]
 *   (`docs/decisions/0039`).
 * * `7` → `8`: `tft_bridge_close_startup_window`, `TFT_BRIDGE_REASON_STARTUP_CONFLICTS`
 *   (`docs/decisions/0011` step 6).
 */
#define TFT_ABI_VERSION_MINOR 8

/**
 * Sentinel for an id field that does not apply to this error.
 */
#define TFT_INVALID_ID UINT32_MAX

/**
 * Length of [`tft_error::message`], including the NUL.
 */
#define TFT_MESSAGE_LEN 256

/**
 * `0` on success; negative on failure.
 */
typedef int32_t tft_status;

/**
 * How to write a transform into caller memory.
 */
typedef uint32_t tft_layout;

/**
 * What to do when the stamp is newer than every sample on the route; an undefined value is
 * [`TFT_ERR_BAD_ENUM`].
 */
typedef uint32_t tft_extrap_policy;

/**
 * How far past the route's newest common sample an answer was extrapolated (`docs/decisions/0039`
 * §1). Required: a NULL `info` is [`TFT_ERR_NULL_ARG`]; `struct_size` must be
 * `sizeof(tft_extrapolated)` or the call returns [`TFT_ERR_BAD_STRUCT_SIZE`] (§3.6).
 */
typedef struct {
  /**
   * `sizeof(tft_extrapolated)` (§3.6).
   */
  uint32_t struct_size;
  /**
   * Nanoseconds past the newest stamp every dynamic edge has data for; `0` means every edge
   * bracketed the query (`docs/decisions/0039` §3).
   */
  int64_t by_ns;
  /**
   * The dynamic edge whose newest stamp is `by_ns` behind the query, or [`TFT_INVALID_ID`] when
   * `by_ns` is `0`.
   */
  uint32_t edge;
} tft_extrapolated;

/**
 * Structured detail for the most recent failure **on this thread**. Fields that do not apply are
 * `TFT_INVALID_ID` (ids) or `0`.
 */
typedef struct {
  /**
   * `sizeof(tft_error)` when compiled (§3.6).
   */
  uint32_t struct_size;
  /**
   * The status code this detail belongs to.
   */
  tft_status code;
  /**
   * The offending edge, or [`TFT_INVALID_ID`].
   */
  uint32_t edge;
  /**
   * First frame involved, or [`TFT_INVALID_ID`].
   */
  uint32_t frame_a;
  /**
   * Second frame involved, or [`TFT_INVALID_ID`].
   */
  uint32_t frame_b;
  /**
   * The requested stamp, in nanoseconds.
   */
  int64_t requested;
  /**
   * Oldest retained stamp on the offending edge.
   */
  int64_t oldest;
  /**
   * Newest published stamp on the offending edge.
   */
  int64_t newest;
  /**
   * Topology generation the plan was compiled against.
   */
  uint64_t plan_generation;
  /**
   * Current topology generation.
   */
  uint64_t current_generation;
  /**
   * NUL-terminated human-readable detail, ASCII only.
   */
  char message[TFT_MESSAGE_LEN];
} tft_error;

/**
 * Refuse: [`TFT_ERR_EXTRAPOLATION`], nothing written. `0`, the default, and what [`tft_plan_at`] does.
 */
#define TFT_EXTRAP_ERROR 0

/**
 * Hold the newest sample constant; [`tft_extrapolated::by_ns`] comes back in the same call.
 */
#define TFT_EXTRAP_HOLD 1

/**
 * Extend the constant screw twist of the two newest samples (`docs/decisions/0039`); falls back to
 * [`TFT_EXTRAP_HOLD`] on a single-sample edge.
 */
#define TFT_EXTRAP_CONSTANT_TWIST 2

/**
 * Success.
 */
#define TFT_OK 0

/**
 * A required pointer argument was NULL.
 */
#define TFT_ERR_NULL_ARG -1

/**
 * A handle's magic word did not match: freed, corrupted, or not ours.
 */
#define TFT_ERR_BAD_HANDLE -2

/**
 * A `struct_size` field named a size this build does not know.
 */
#define TFT_ERR_BAD_STRUCT_SIZE -3

/**
 * An enum argument was outside its defined range.
 */
#define TFT_ERR_BAD_ENUM -4

/**
 * The caller's output buffer is too small for the request.
 */
#define TFT_ERR_BUFFER_TOO_SMALL -5

/**
 * A frame name this tree never interned (see also `tft_plan_create`'s *Errors*).
 */
#define TFT_ERR_UNKNOWN_FRAME -10

/**
 * Target and source are in different connected components.
 */
#define TFT_ERR_DISCONNECTED -11

/**
 * The edge has no published samples yet (see also `tft_plan_create`'s *Errors*).
 */
#define TFT_ERR_NO_DATA -12

/**
 * The requested stamp lies outside the edge's retained history.
 */
#define TFT_ERR_EXTRAPOLATION -13

/**
 * The topology changed since the plan was compiled; re-plan.
 */
#define TFT_ERR_TOPOLOGY_CHANGED -14

/**
 * The query's time domain does not match the plan's (see also `tft_plan_create`'s *Errors*).
 */
#define TFT_ERR_TIME_DOMAIN -15

/**
 * The ring lapped the reader mid-read. Retryable.
 */
#define TFT_ERR_SLOT_RECYCLED -16

/**
 * A slot stayed mid-write longer than the retry limit. Retryable.
 */
#define TFT_ERR_SLOT_CONTENDED -17

/**
 * This handle was created before a `fork()` and is being used in the child.
 */
#define TFT_ERR_CHILD_DETACHED -18

/**
 * The edge's interpolation policy has no reportable derivative.
 */
#define TFT_ERR_NO_DERIVATIVES -19

/**
 * There is a pose at this stamp but no segment to differentiate.
 */
#define TFT_ERR_NO_SEGMENT -20

/**
 * A `tft_publisher` was used from a thread other than its creator's.
 */
#define TFT_ERR_WRONG_THREAD -30

/**
 * The path is too long: more raw edges than a lookup will walk, or more steps than a compiled plan
 * holds. Two engine bounds share one status (`0034`).
 */
#define TFT_ERR_TREE_TOO_DEEP -21

/**
 * The compiled-against ABI version is incompatible with this library (§3.6).
 */
#define TFT_ERR_ABI_MISMATCH -6

/**
 * A published transform contained NaN or infinity.
 */
#define TFT_ERR_NOT_FINITE -7

/**
 * A published rotation is not one: a non-unit quaternion, or a matrix whose determinant is not `+1`.
 */
#define TFT_ERR_NOT_A_ROTATION -8

/**
 * Another participant already holds this edge. One writer per edge (D7).
 */
#define TFT_ERR_ALREADY_CLAIMED -31

/**
 * A published stamp predates the edge's newest sample.
 */
#define TFT_ERR_NON_MONOTONIC -32

/**
 * A reaper judged this writer dead and took the edge away. Re-claim.
 */
#define TFT_ERR_CLAIM_REVOKED -33

/**
 * The edge is static or tombstoned; there is nothing to publish to it.
 */
#define TFT_ERR_NOT_DYNAMIC -34

/**
 * The arena is mapped read-only, so nothing can be claimed for writing.
 */
#define TFT_ERR_READ_ONLY -35

/**
 * The operation raced another participant's protocol; retry.
 */
#define TFT_ERR_RETRY -36

/**
 * The publisher's claim was released; claim the edge again to publish.
 */
#define TFT_ERR_RELEASED -37

/**
 * The child is attached to a **different** parent than the one named; `frame_a` = child, `frame_b` =
 * its actual parent.
 */
#define TFT_ERR_PARENT_MISMATCH -38

/**
 * The named child frame has no incoming edge at all — it is a root, or was never attached.
 */
#define TFT_ERR_NO_EDGE -39

/**
 * A configuration text could not be turned into a topology (parse error, cycle, or unbuildable);
 * the message names the line or frame.
 */
#define TFT_ERR_BAD_CONFIG -40

/**
 * A `(sec, nanos)` pair is not a representable stamp: `nanos` outside `[0, 1e9)`, or the total does
 * not fit `int64_t`.
 *
 * Only `tft_stamp_from_parts` and `tft_stamp_from_timespec` return it (`docs/PHASE4.md` §3.6);
 * `requested` = seconds, `newest` = nanoseconds.
 */
#define TFT_ERR_BAD_STAMP -41

/**
 * A **shared** arena was asked for and could not be had: name held by a live arena, unusable runtime
 * directory, segment not creatable or mappable, or no `--features shm` (`docs/decisions/0015`).
 *
 * Only `tft_bridge_create` with a non-NULL `arena_name` returns it; there is **no fallback** to a
 * heap arena.
 */
#define TFT_ERR_ARENA_UNAVAILABLE -42

/**
 * Something the library did not anticipate — including a caught Rust panic.
 */
#define TFT_ERR_INTERNAL -99

/**
 * `[qw qx qy qz tx ty tz]` `f64` — canonical, matches the arena.
 */
#define TFT_LAYOUT_QVEC7_WXYZ 0

/**
 * `[qx qy qz qw tx ty tz]` `f64` — **Eigen/Sophus coefficient order**.
 */
#define TFT_LAYOUT_QVEC7_XYZW 1

/**
 * 4×4 `f64` column-major — Eigen's `Isometry3d`.
 */
#define TFT_LAYOUT_MAT4_COL 2

/**
 * 4×4 `f64` row-major — C and NumPy.
 */
#define TFT_LAYOUT_MAT4_ROW 3

/**
 * 3×4 `f32` row-major — GPU upload.
 */
#define TFT_LAYOUT_AFFINE12_ROW_F32 4

/**
 * `[qw qx qy qz tx ty tz | ωx ωy ωz vx vy vz]` `f64` — pose **and body twist**.
 *
 * [`TFT_LAYOUT_QVEC7_WXYZ`] plus the `[ω, v]` twist `TFT_TWIST_BYTES` describes (`docs/API.md` §3.3).
 * Asking for it *is* asking for derivatives: an edge interpolating with `LerpSlerp` returns
 * `TFT_ERR_NO_DERIVATIVES` and writes nothing for that element or any after it. Not readable.
 */
#define TFT_LAYOUT_QVEC7_WXYZ_TWIST6 5

/**
 * The library's major ABI version.
 */
uint32_t tft_abi_version_major(void);

/**
 * The library's minor ABI version.
 */
uint32_t tft_abi_version_minor(void);

/**
 * Check the compiled-against header against the linked library: major must match exactly; the
 * runtime minor may be ≥ (§3.6). Call once at startup with the header's constants (the C++ wrapper
 * does).
 *
 * # Errors
 *
 * [`TFT_ERR_ABI_MISMATCH`]; `frame_a`/`frame_b` carry the caller's major/minor,
 * `plan_generation` and `current_generation` the library's.
 */
tft_status tft_check_abi(uint32_t compiled_major,
                         uint32_t compiled_minor);

/**
 * Assemble a stamp from a `(sec, nanos)` pair, exactly (`docs/API.md` §5.1); the C spelling of
 * `Stamp::from_parts`. No float on any surface (R3).
 *
 * It returns a status because out-of-range `nanos` or a wrapping sum would each yield a plausible
 * stamp and no `int64_t` sentinel exists.
 *
 * # Errors
 *
 * [`TFT_ERR_NULL_ARG`] if `out` is NULL. [`TFT_ERR_BAD_STAMP`] if `nanos` is
 * outside `[0, 1e9)` or the sum does not fit `int64_t`; `*out` is not written.
 *
 * # Safety
 *
 * `out` must be NULL or point to a writable `int64_t`.
 */
tft_status tft_stamp_from_parts(int64_t sec, uint32_t nanos, int64_t *out);

/**
 * Assemble a stamp from a POSIX `struct timespec`'s fields.
 *
 * # Errors
 *
 * Everything [`tft_stamp_from_parts`] refuses, plus a negative `tv_nsec` (an interval passed as an
 * instant).
 *
 * # Safety
 *
 * `out` must be NULL or point to a writable `int64_t`.
 */
tft_status tft_stamp_from_timespec(int64_t tv_sec, int64_t tv_nsec, int64_t *out);

#if defined(TFT_HAVE_SHM)
/**
 * Join the running arena named by the environment, read-only (D18), as `tf_tree::open()` does
 * (`$TF_TREE_DOMAIN`, `$TF_TREE_NAME`, `$TF_TREE_RUNTIME_DIR`). Pass `*out` to [`tft_tree_free`]
 * exactly once.
 * # Safety
 *
 * `out` must be NULL or point to a writable `*mut tft_tree`.
 */
tft_status tft_tree_open(tft_tree **out);
#endif

/**
 * Release a tree handle; freeing NULL is a no-op. Plans compiled from it stay valid.
 *
 * # Safety
 *
 * `tree` must be NULL or a live handle not already freed; the magic word catches a double-free only
 * while the allocation is intact.
 */
void tft_tree_free(tft_tree *tree);

/**
 * Compile a plan for `target <- source`, by frame name; compile once, evaluate many times (D3).
 *
 * This is [`tft_plan_create_in_domain`] with `domain = 0`; on an arena whose dynamic edges carry
 * another tag (`docs/PHASE4.md` §5.5) it returns [`TFT_ERR_TIME_DOMAIN`] (`docs/decisions/0038`).
 *
 * # Errors
 *
 * `*out` is not written on any failure. Three codes carry extra meaning here:
 *
 * * [`TFT_ERR_UNKNOWN_FRAME`] — a name is not UTF-8 or does not resolve (read-only: undeclared, a
 *   permanent hash-slot collision, or being interned right now, retry; writable: a full frame
 *   table). With `frame_a` set: no consistent topology snapshot within the retry limit
 *   (transient), or a parent index outside the frame table (corrupt arena).
 * * [`TFT_ERR_NO_DATA`] — the topology records a parent for `frame_a` but no edge (corrupt arena).
 *   An edge with no samples yet compiles.
 * * [`TFT_ERR_TIME_DOMAIN`] — the route's dynamic edges publish in a tag other than `domain`, or
 *   disagree among themselves (`edge` names the one that did).
 *
 * # Safety
 *
 * `tree` must be a live handle. `target` and `source` must be NUL-terminated UTF-8. `out` must be
 * NULL or point to a writable `*mut tft_plan`.
 */
tft_status tft_plan_create(const tft_tree *tree,
                           const char *target,
                           const char *source,
                           tft_plan **out);

/**
 * Compile a plan for `target <- source` queried in time domain `domain` (`docs/decisions/0038`).
 *
 * A foreign caller carries the tag (`0`–`3` built-in, `4`+ driver-declared; `docs/API.md` §2.5) as
 * data; `0` is [`tft_plan_create`]. A mismatch is reported once, at plan time; every evaluate still
 * passes the tag to the engine.
 *
 * # Errors
 *
 * Everything [`tft_plan_create`] returns, plus [`TFT_ERR_TIME_DOMAIN`] when the route has a dynamic
 * edge whose tag is not `domain`; `*out` is not written. A bare `domain != plan.domain()` would
 * wrongly refuse a static route, so this asks [`tf_tree::Plan::steps`] whether any
 * [`tf_tree::Step::Dyn`] is present.
 *
 * # Safety
 *
 * As [`tft_plan_create`]: `tree` must be a live handle, `target` and `source`
 * NUL-terminated UTF-8, and `out` NULL or a writable `*mut tft_plan`.
 */
tft_status tft_plan_create_in_domain(const tft_tree *tree,
                                     const char *target,
                                     const char *source,
                                     uint8_t domain,
                                     tft_plan **out);

/**
 *
 * # Safety
 *
 * `plan` must be NULL or a handle from [`tft_plan_create`] not already freed.
 */
void tft_plan_free(tft_plan *plan);

/**
 * Evaluate `plan` at `stamp`, writing the result into `out` in `layout` (at least
 * `tft_layout_size(layout)` bytes).
 *
 * **On a hot path, prefer [`tft_plan_at_many`]**: this builds a `Guard` per lookup
 * (`docs/decisions/0022`). The plan is evaluated in the domain it was compiled for.
 *
 * [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] asks for derivatives: thirteen `f64` are written, failing with
 * `TFT_ERR_NO_DERIVATIVES` or `TFT_ERR_NO_SEGMENT` and writing nothing.
 * # Safety
 *
 * `plan` must be a handle from `tft_plan_create` that has not been freed.
 * `out` must point to at least `tft_layout_size(layout)` writable bytes.
 */
tft_status tft_plan_at(const tft_plan *plan, int64_t stamp, tft_layout layout, void *out);

/**
 * Evaluate `plan` at `n` stamps, writing each result `out_stride_bytes` apart (0 = packed; §4.3).
 *
 * # Partial writes
 *
 * Evaluation stops at the first failing stamp and earlier elements stay written; `frame_b` carries
 * the failing index. Only the argument checks are all-or-nothing.
 *
 * # `TFT_LAYOUT_QVEC7_WXYZ_TWIST6`
 *
 * Accepted as by [`tft_plan_at`], per element. `TFT_ERR_NO_DERIVATIVES` fires on the first element
 * with the buffer untouched; `TFT_ERR_NO_SEGMENT` can fire part-way. Sort your stamps:
 * non-decreasing stamps ride a resumable cursor.
 *
 * # Safety
 *
 * `plan` must be a live handle. `stamps` must point to `n` readable `int64_t`. `out` must point to at
 * least `n * stride` writable bytes (`stride` is `out_stride_bytes`, or the payload size if zero).
 */
tft_status tft_plan_at_many(const tft_plan *plan,
                            const int64_t *stamps,
                            size_t n,
                            tft_layout layout,
                            void *out,
                            size_t out_stride_bytes);

/**
 * The bytes one transform occupies in `layout`, or `0` for an undefined discriminant.
 */
size_t tft_layout_size(tft_layout layout);

/**
 * [`tft_plan_at`], permitting extrapolation under `policy` and reporting how far
 * (`docs/decisions/0039`).
 *
 * `info` is required (NULL is [`TFT_ERR_NULL_ARG`], nothing written). [`tft_plan_at`] still refuses
 * and is what a caller that must not act on invented data should call.
 *
 * [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] is refused with [`TFT_ERR_BAD_ENUM`].
 *
 * # Errors
 *
 * Everything [`tft_plan_at`] returns. Under [`TFT_EXTRAP_ERROR`] a stamp past the newest sample is
 * [`TFT_ERR_EXTRAPOLATION`]; otherwise `info->by_ns` says how far. [`TFT_ERR_BAD_STRUCT_SIZE`] if
 * `info->struct_size` is not `sizeof(tft_extrapolated)`.
 *
 * # Safety
 *
 * `plan` must be a live handle. `out` must point to at least `tft_layout_size(layout)` writable
 * bytes. `info` must point to a writable `tft_extrapolated` with `struct_size` set.
 */
tft_status tft_plan_at_extrapolating(const tft_plan *plan,
                                     int64_t stamp,
                                     tft_extrap_policy policy,
                                     tft_layout layout,
                                     void *out,
                                     tft_extrapolated *info);

/**
 * Copy this thread's most recent error into `out`.
 *
 * # Errors
 *
 * [`TFT_ERR_NULL_ARG`] if `out` is NULL, [`TFT_ERR_BAD_STRUCT_SIZE`] if
 * `out->struct_size` is not a size this build recognises.
 *
 * # Safety
 *
 * `out` must be NULL or point to a writable, correctly aligned `tft_error`
 * whose `struct_size` field has been initialised.
 */
tft_status tft_last_error(tft_error *out);

/**
 * Claim exclusive write access to the edge attaching `child` to `parent` (one participant per edge,
 * D7). Released by [`tft_publisher_release`] or [`tft_publisher_free`]; a leaked handle leaks the
 * claim. The calling thread owns the publisher. An unseen frame name is interned, so a mistyped
 * `child` fails with `TFT_ERR_NO_EDGE`.
 *
 * # Safety
 *
 * `tree` must be a live handle. `child` and `parent` must be NUL-terminated
 * UTF-8. `out` must be NULL or point to a writable `*mut tft_publisher`.
 */
tft_status tft_tree_claim(const tft_tree *tree,
                          const char *child,
                          const char *parent,
                          tft_publisher **out);

/**
 * Publish one transform at `stamp`, read from `src` in `layout`.
 *
 * `src` must hold at least `tft_layout_size(layout)` bytes. `AFFINE12_ROW_F32` is refused.
 *
 * # Safety
 *
 * `pubh` must be a live handle used from the thread that created it. `src` must
 * point to at least `tft_layout_size(layout)` readable bytes.
 */
tft_status tft_publisher_push(tft_publisher *pubh,
                              int64_t stamp,
                              tft_layout layout,
                              const void *src);

/**
 * Publish `n` transforms, reading each `src_stride_bytes` apart (0 means tightly packed; §4.3).
 *
 * Stops at the first rejected element, leaving earlier ones published (no unpublishing); the failing
 * index is in the error detail's `frame_b`.
 *
 * # Safety
 *
 * `pubh` must be a live handle used from its creating thread. `stamps` must
 * point to `n` readable `int64_t`, and `src` to `n` strided payloads.
 */
tft_status tft_publisher_push_many(tft_publisher *pubh,
                                   const int64_t *stamps,
                                   size_t n,
                                   tft_layout layout,
                                   const void *src,
                                   size_t src_stride_bytes);

/**
 * Release the claim now, leaving the handle valid but unusable. Calling it twice is a no-op.
 *
 * # Safety
 *
 * `pubh` must be a live handle used from the thread that created it.
 */
tft_status tft_publisher_release(tft_publisher *pubh);

/**
 * Release the claim and the handle. Freeing NULL is a no-op.
 *
 * # Safety
 *
 * `pubh` must be NULL or a handle from [`tft_tree_claim`] not already freed,
 * and must be freed from the thread that created it.
 */
void tft_publisher_free(tft_publisher *pubh);

#ifdef __cplusplus
}  /* extern "C" */
#endif

#endif  /* TF_TREE_H */
