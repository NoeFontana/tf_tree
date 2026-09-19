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
 * Major ABI version. **Must match exactly** between the header a caller
 * compiled against and the library it links.
 */
#define TFT_ABI_VERSION_MAJOR 0

/**
 * Minor ABI version. The runtime's may be **≥** the compiled-against value (§3.6).
 *
 * Every bump is an append (nothing moved, changed type or changed meaning), and
 * the minor answers "can I name this symbol?", so a new function or enumerator
 * bumps it even in the unstable tier:
 *
 * * `1` → `2`: `tft_bridge_note_time_jump`; fields appended to
 *   `tft_bridge_sample` and `tft_bridge_outcome`. `tft_bridge_offer` reads a
 *   shorter `tft_bridge_sample` as the prefix it is.
 * * `2` → `3`: [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] (`docs/API.md` §3.3).
 * * `3` → `4`: [`tft_stamp_from_parts`], [`tft_stamp_from_timespec`] (`docs/API.md`
 *   §5.1) and [`TFT_ERR_BAD_STAMP`], which only they return.
 * * `4` → `5`: `tft_bridge_options::arena_name` (`docs/decisions/0015`) and
 *   [`TFT_ERR_ARENA_UNAVAILABLE`], reachable only when `arena_name` is non-NULL;
 *   `tft_bridge_create` reads a shorter `tft_bridge_options` as its prefix.
 * * `5` → `6`: [`tft_plan_create_in_domain`] (`docs/decisions/0038`).
 *   [`tft_plan_create`] is it with `domain = 0`, and now returns
 *   [`TFT_ERR_TIME_DOMAIN`] at plan time instead of on every lookup.
 * * `6` → `7`: [`tft_plan_at_extrapolating`], [`tft_extrap_policy`] and
 *   [`tft_extrapolated`] (`docs/decisions/0039`).
 * * `7` → `8`: `tft_bridge_close_startup_window` and
 *   `TFT_BRIDGE_REASON_STARTUP_CONFLICTS` (`docs/decisions/0011` step 6). A
 *   `STRICT` startup halt reached through `tft_bridge_offer` now reports 9
 *   where it reported `TFT_BRIDGE_REASON_AUTHORITY_CONFLICT` (5); the action is
 *   `TFT_BRIDGE_HALT` either way.
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
 * What to do when the requested stamp is newer than every published sample on
 * the route.
 *
 * A `uint32_t` typedef with named constants, like [`tft_layout`] (§3.6 needs
 * every ABI value's width stated). Every entry point that takes one rejects an
 * undefined discriminant with [`TFT_ERR_BAD_ENUM`].
 */
typedef uint32_t tft_extrap_policy;

/**
 * How far past the route's newest common sample an answer was extrapolated.
 *
 * The caller must pass one to get a pose at all (`docs/decisions/0039` §1):
 * [`tft_plan_at_extrapolating`] returns [`TFT_ERR_NULL_ARG`] for a NULL `info`,
 * and there is no second spelling without it. `struct_size` is §3.6's append
 * mechanism, checked as [`tft_error`]'s is: set it to `sizeof(tft_extrapolated)`
 * or the call returns [`TFT_ERR_BAD_STRUCT_SIZE`].
 */
typedef struct {
  /**
   * `sizeof(tft_extrapolated)` — §3.6.
   */
  uint32_t struct_size;
  /**
   * Nanoseconds past the newest stamp that every dynamic edge on this plan has
   * data for; `0` means every edge bracketed the query. Otherwise the worst
   * case over the route (`docs/decisions/0039` §3).
   */
  int64_t by_ns;
  /**
   * The dynamic edge whose newest stamp is [`Self::by_ns`] behind the query, or
   * [`TFT_INVALID_ID`] when `by_ns` is `0` (where the engine's edge id is
   * meaningless).
   */
  uint32_t edge;
} tft_extrapolated;

/**
 * Structured detail for the most recent failure **on this thread**. Fields that do not apply are
 * `TFT_INVALID_ID` (ids) or `0`.
 */
typedef struct {
  /**
   * `sizeof(tft_error)` at the time this build was compiled — the Vulkan
   * approach to appending fields without a major version bump (§3.6).
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
   * NUL-terminated human-readable detail. Never contains a partial UTF-8
   * sequence: it is written from ASCII only.
   */
  char message[TFT_MESSAGE_LEN];
} tft_error;

/**
 * Refuse: the lookup returns [`TFT_ERR_EXTRAPOLATION`] and writes nothing. `0`,
 * so a zeroed struct refuses; it is `tf_tree::ExtrapPolicy`'s `Default` and what
 * [`tft_plan_at`] does.
 */
#define TFT_EXTRAP_ERROR 0

/**
 * Hold the newest sample constant; [`tft_extrapolated::by_ns`] comes back in
 * the same call.
 */
#define TFT_EXTRAP_HOLD 1

/**
 * Extend the constant screw twist implied by the two newest samples
 * (`docs/decisions/0039` *Context*); falls back to [`TFT_EXTRAP_HOLD`] on an
 * edge retaining a single sample.
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
 * A frame name that this tree never interned.
 *
 * From plan compilation it can also mean something else; see
 * `tft_plan_create`'s *Errors*.
 */
#define TFT_ERR_UNKNOWN_FRAME -10

/**
 * Target and source are in different connected components.
 */
#define TFT_ERR_DISCONNECTED -11

/**
 * The edge has no published samples yet.
 *
 * From plan compilation it can also mean something else; see
 * `tft_plan_create`'s *Errors*.
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
 * The query's time domain does not match the plan's.
 *
 * From plan compilation it can also mean something else; see
 * `tft_plan_create`'s *Errors*.
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
 * The path between the two frames is too long: more raw edges than a lookup will walk, or more
 * steps than a compiled plan holds once adjacent rigid links fold into one.
 *
 * Two engine bounds share one status because this table is frozen (`0034`); neither is exported as
 * a macro.
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
 * A published rotation is not one: a non-unit quaternion, or a matrix whose
 * determinant is not `+1` (reflected, or carrying scale).
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
 * The operation raced another participant's protocol. Retryable, and the
 * caller's only correct response is to try again.
 */
#define TFT_ERR_RETRY -36

/**
 * The publisher's claim was released; claim the edge again to publish.
 */
#define TFT_ERR_RELEASED -37

/**
 * Both frame names are known, but the child is attached to a **different** parent than the one
 * named. The detail carries `frame_a` = the child, `frame_b` = its actual parent.
 */
#define TFT_ERR_PARENT_MISMATCH -38

/**
 * The named child frame has no incoming edge at all — it is a root, or was never attached.
 */
#define TFT_ERR_NO_EDGE -39

/**
 * A configuration text could not be turned into a topology: it does not parse,
 * declares a cycle, or describes a tree the engine will not build. The
 * message names the line or the frame.
 */
#define TFT_ERR_BAD_CONFIG -40

/**
 * A `(sec, nanos)` pair is not a representable stamp: `nanos` is outside `[0, 1e9)`, or the total
 * does not fit `int64_t`.
 *
 * Returned only by `tft_stamp_from_parts` and `tft_stamp_from_timespec` (a minor bump under
 * `docs/PHASE4.md` §3.6). The detail carries `requested` = seconds, `newest` = nanoseconds. Not
 * `TFT_ERR_BAD_ENUM`: this is an arithmetic refusal.
 */
#define TFT_ERR_BAD_STAMP -41

/**
 * A **shared** arena was asked for and could not be had: the rendezvous name is held by a live
 * arena, the runtime directory is unusable, the segment could not be created or mapped, or this
 * library was built without `--features shm`. The message says which (`docs/decisions/0015`).
 *
 * Returned only by `tft_bridge_create` with a non-NULL `tft_bridge_options::arena_name` (a minor
 * bump under `docs/PHASE4.md` §3.6). There is **no fallback to a private heap arena**.
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
 * [`TFT_LAYOUT_QVEC7_WXYZ`] plus the `[ω, v]` twist `TFT_TWIST_BYTES` describes
 * (`docs/API.md` §3.3). Appending it is a minor ABI bump (`docs/PHASE4.md`
 * §3.6). `tft_plan_at`, `tft_plan_at_many` and `tft_plan_at_with_derivatives`
 * accept it, and asking for it *is* asking for derivatives.
 *
 * An edge interpolating with `LerpSlerp` has no exact twist: the call returns
 * `TFT_ERR_NO_DERIVATIVES` naming the edge and writes nothing for that element
 * or any after it. Not readable: a velocity is derived, never stored.
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
 * Check the header a caller compiled against against the library they linked:
 * major must match exactly; the runtime minor may be ≥ the compiled-against
 * minor (§3.6).
 *
 * Call it as `tft_check_abi(TFT_ABI_VERSION_MAJOR, TFT_ABI_VERSION_MINOR)` with
 * the constants **from the header**, once at startup (the C++ wrapper does).
 *
 * # Errors
 *
 * [`TFT_ERR_ABI_MISMATCH`]; `frame_a`/`frame_b` carry the caller's major/minor,
 * `plan_generation` and `current_generation` the library's.
 */
tft_status tft_check_abi(uint32_t compiled_major, uint32_t compiled_minor);

/**
 * Assemble a stamp from a `(sec, nanos)` pair, exactly — `docs/API.md` §5.1.
 *
 * The C spelling of `Stamp::from_parts`, for a ROS 2 `builtin_interfaces/Time`.
 * No float on any surface (R3). It returns a status because two inputs have no
 * correct answer and no `int64_t` sentinel exists: normalising an out-of-range
 * `nanos` or wrapping an out-of-range sum would each yield a plausible stamp.
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
 * Assemble a stamp from the two fields of a POSIX `struct timespec`
 * (`tft_stamp_from_timespec(ts.tv_sec, ts.tv_nsec, &out)`).
 *
 * # Errors
 *
 * Everything [`tft_stamp_from_parts`] refuses, plus a negative `tv_nsec`
 * (legal only in a relative `timespec`, so an interval is being converted as an
 * instant).
 *
 * # Safety
 *
 * `out` must be NULL or point to a writable `int64_t`.
 */
tft_status tft_stamp_from_timespec(int64_t tv_sec, int64_t tv_nsec, int64_t *out);

#if defined(TFT_HAVE_SHM)
/**
 * Join the running arena named by the environment, read-only (D18).
 *
 * Mirrors `tf_tree::open()`: `$TF_TREE_DOMAIN`, `$TF_TREE_NAME` and
 * `$TF_TREE_RUNTIME_DIR` select the arena. On success `*out` must be passed to
 * [`tft_tree_free`] exactly once.
 *
 * # Safety
 *
 * `out` must be NULL or point to a writable `*mut tft_tree`.
 */
tft_status tft_tree_open(tft_tree **out);
#endif

/**
 * Release a tree handle. Freeing NULL is a no-op. Plans compiled from it stay
 * valid (the tree is refcounted).
 *
 * # Safety
 *
 * `tree` must be NULL or a handle from a `tft_tree_*` constructor that has not
 * already been freed. Double-free is undefined; the magic word catches it only
 * while the allocation is intact.
 */
void tft_tree_free(tft_tree *tree);

/**
 * Compile a plan for `target <- source`, by frame name.
 *
 * Compilation walks the topology once; evaluating is the hot path (D3), so
 * compile once and evaluate many times.
 *
 * This is [`tft_plan_create_in_domain`] with `domain = 0`, the real-time tag. On
 * an arena whose dynamic edges carry another tag (`docs/PHASE4.md` §5.5) it
 * returns [`TFT_ERR_TIME_DOMAIN`] here (`docs/decisions/0038`).
 *
 * # Errors
 *
 * `*out` is not written on any failure. Three codes carry extra meaning here:
 *
 * * [`TFT_ERR_UNKNOWN_FRAME`] — a name is not UTF-8 or does not resolve (on a
 *   read-only attachment: undeclared, or a hash-slot collision (permanent), or
 *   being interned right now (transient, retry); on a writable tree: a full
 *   frame table). From compilation, with `frame_a` set: no consistent topology
 *   snapshot within the retry limit (transient), or a parent index outside the
 *   frame table (corrupt arena).
 * * [`TFT_ERR_NO_DATA`] — the topology records a parent for `frame_a` but no
 *   edge (a corrupt arena). An edge with no samples yet compiles.
 * * [`TFT_ERR_TIME_DOMAIN`] — the route's dynamic edges publish in a tag other
 *   than `domain`, or disagree among themselves (`edge` names the one that did).
 *
 * # Safety
 *
 * `tree` must be a live handle. `target` and `source` must be NUL-terminated
 * UTF-8. `out` must be NULL or point to a writable `*mut tft_plan`.
 */
tft_status tft_plan_create(const tft_tree *tree,
                           const char *target,
                           const char *source,
                           tft_plan **out);

/**
 * Compile a plan for `target <- source` that will be queried in time domain
 * `domain` (`docs/decisions/0038`).
 *
 * [`tf_tree::Domain`] is an open trait, so a foreign caller carries the tag
 * (`0`–`3` are the built-in domains, `4`+ are driver-declared; `docs/API.md`
 * §2.5) as data. `0` is [`tft_plan_create`]. A mismatch is reported once, at
 * plan time, with the frame names in hand; every evaluate entry point still
 * passes the handle's tag to the engine and the engine still compares it.
 *
 * # Errors
 *
 * Everything [`tft_plan_create`] returns, plus [`TFT_ERR_TIME_DOMAIN`] when
 * this route has a dynamic edge whose tag is not `domain`; `*out` is not
 * written. The condition is the engine's `has_dynamic() && domain !=
 * self.domain`: a bare `domain != plan.domain()` would wrongly refuse a static
 * route (`Plan::domain` reports `0` for one), so this asks
 * [`tf_tree::Plan::steps`] whether any [`tf_tree::Step::Dyn`] is present.
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
 * Release a plan handle. Freeing NULL is a no-op.
 *
 * # Safety
 *
 * `plan` must be NULL or a handle from [`tft_plan_create`] not already freed.
 */
void tft_plan_free(tft_plan *plan);

/**
 * Evaluate `plan` at `stamp`, writing the result into `out` in `layout`.
 *
 * `out` must have room for at least `tft_layout_size(layout)` bytes.
 *
 * **On a hot path, prefer [`tft_plan_at_many`]**: this builds a `Guard` per
 * lookup (`docs/decisions/0022`), the batch pays it once per call. The plan is
 * evaluated in the domain it was compiled for ([`tft_plan_create_in_domain`]).
 *
 * [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] is asking for derivatives: thirteen `f64`
 * are written, and it fails with `TFT_ERR_NO_DERIVATIVES` (a `LerpSlerp` edge)
 * or `TFT_ERR_NO_SEGMENT` (a pose but no segment), writing nothing.
 *
 * # Safety
 *
 * `plan` must be a handle from `tft_plan_create` that has not been freed.
 * `out` must point to at least `tft_layout_size(layout)` writable bytes.
 */
tft_status tft_plan_at(const tft_plan *plan, int64_t stamp, tft_layout layout, void *out);

/**
 * Evaluate `plan` at `n` stamps, writing each result `out_stride_bytes` apart.
 *
 * `out_stride_bytes == 0` means tightly packed; a larger stride writes into an
 * array of caller structs (§4.3).
 *
 * # Partial writes
 *
 * Evaluation stops at the first failing stamp and earlier elements stay
 * written; `tft_last_error`'s `frame_b` carries the failing index. Only the
 * argument checks (NULL, stride, overflow, unknown layout) are all-or-nothing.
 *
 * # `TFT_LAYOUT_QVEC7_WXYZ_TWIST6`
 *
 * Accepted as by [`tft_plan_at`], per element. `TFT_ERR_NO_DERIVATIVES` is a
 * property of an edge and fires on the first element with the buffer untouched;
 * `TFT_ERR_NO_SEGMENT` can fire part-way. Sort your stamps: non-decreasing
 * stamps ride a resumable cursor (`O(1)` amortized bracket search). A packed,
 * `f64`-aligned `out` is written in place; any other stride is evaluated in
 * chunks and scattered.
 *
 * # Safety
 *
 * `plan` must be a live handle. `stamps` must point to `n` readable `int64_t`.
 * `out` must point to at least `n * stride` writable bytes, where `stride` is
 * `out_stride_bytes` or the layout's payload size when that is zero.
 */
tft_status tft_plan_at_many(const tft_plan *plan,
                            const int64_t *stamps,
                            size_t n,
                            tft_layout layout,
                            void *out,
                            size_t out_stride_bytes);

/**
 * The number of bytes one transform occupies in `layout`, or `0` if the
 * discriminant is not one this build defines.
 */
size_t tft_layout_size(tft_layout layout);

/**
 * [`tft_plan_at`], permitting extrapolation past the newest sample under
 * `policy` and reporting how far (`docs/decisions/0039`).
 *
 * `info` is required: the distance comes back with the pose. NULL is
 * [`TFT_ERR_NULL_ARG`] and nothing is written. [`tft_plan_at`] still refuses, and
 * is what a caller that must not act on invented data should call. The plan is
 * evaluated in the domain it was compiled for.
 *
 * [`TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] is refused with [`TFT_ERR_BAD_ENUM`]: the
 * engine has no extrapolating `at_with_derivatives`.
 *
 * # Errors
 *
 * Everything [`tft_plan_at`] returns. Under [`TFT_EXTRAP_ERROR`] a stamp past
 * the newest sample is [`TFT_ERR_EXTRAPOLATION`]; otherwise `info->by_ns` says
 * how far. [`TFT_ERR_BAD_STRUCT_SIZE`] if `info->struct_size` is not
 * `sizeof(tft_extrapolated)`.
 *
 * # Safety
 *
 * `plan` must be a handle from [`tft_plan_create`] that has not been freed.
 * `out` must point to at least `tft_layout_size(layout)` writable bytes.
 * `info` must point to a writable `tft_extrapolated` whose `struct_size` this
 * caller has set.
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
 * Claim exclusive write access to the edge attaching `child` to `parent`.
 *
 * One participant per edge (D7), machine-wide when shared. Released by
 * [`tft_publisher_release`] or [`tft_publisher_free`]; a leaked handle leaks
 * the claim. The calling thread owns the publisher (§3.2).
 *
 * A frame name not seen before is interned, not rejected, so a mistyped
 * `child` fails with `TFT_ERR_NO_EDGE`, not [`TFT_ERR_UNKNOWN_FRAME`] (which
 * means the frame table is full). Ids are never recycled (D10).
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
 * `src` must hold at least `tft_layout_size(layout)` bytes. `AFFINE12_ROW_F32`
 * is not accepted (`TFT_ERR_BAD_ENUM`); matrix layouts are validated (see
 * `crate::layout::read`).
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
 * Publish `n` transforms, reading each `src_stride_bytes` apart (0 means
 * tightly packed; §4.3).
 *
 * Stops at the first rejected element, leaving earlier ones published (unlike
 * `tft_plan_at_many`'s all-or-nothing: there is no unpublishing). The failing
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
 * Release the claim now, leaving the handle valid but unusable for publishing.
 * Also released by [`tft_publisher_free`]; calling it twice is a no-op.
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
