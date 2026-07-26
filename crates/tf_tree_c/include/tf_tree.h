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
 * Minor ABI version. The runtime's may be **≥** the compiled-against value.
 */
#define TFT_ABI_VERSION_MINOR 1

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
 * Structured detail for the most recent failure **on this thread**.
 *
 * Every field that does not apply to a given error is `TFT_INVALID_ID` (ids) or
 * `0` (stamps and generations), so a caller can print the whole struct without
 * checking which variant produced it.
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
 */
#define TFT_ERR_UNKNOWN_FRAME -10

/**
 * Target and source are in different connected components.
 */
#define TFT_ERR_DISCONNECTED -11

/**
 * The edge has no published samples yet.
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
 * The path between the two frames is deeper than `TFT_MAX_DEPTH`.
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
 * Both frame names are known, but the child is attached to a **different**
 * parent than the one named.
 *
 * Distinct from [`TFT_ERR_UNKNOWN_FRAME`] on purpose: that one means "check
 * your spelling", and this one means "check your topology". Reported as
 * `UNKNOWN_FRAME` until review pointed out that its documented meaning — "a
 * frame name that this tree never interned" — is false for *every* instance of
 * this case, since `tft_tree_claim` resolves both names before it can arise.
 *
 * The detail carries `frame_a` = the child, `frame_b` = its actual parent.
 */
#define TFT_ERR_PARENT_MISMATCH -38

/**
 * The named child frame has no incoming edge at all — it is a root, or was
 * never attached. Also formerly `TFT_ERR_UNKNOWN_FRAME`, and false for the
 * same reason.
 */
#define TFT_ERR_NO_EDGE -39

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
 * The library's major ABI version.
 */
uint32_t tft_abi_version_major(void);

/**
 * The library's minor ABI version.
 */
uint32_t tft_abi_version_minor(void);

/**
 * Check the header a caller compiled against against the library they linked.
 *
 * §3.6 states the rule — **major must match exactly; the runtime minor may be
 * ≥ the compiled-against minor** — and until this existed nothing enforced it.
 * Two getters let a caller *implement* the rule; only one of them will, and the
 * one who does not is the one who needs it.
 *
 * Call it as `tft_check_abi(TFT_ABI_VERSION_MAJOR, TFT_ABI_VERSION_MINOR)`
 * using the constants **from the header**, so the arguments are baked in at the
 * caller's compile time and the comparison is genuinely between two builds. The
 * C++ wrapper does this in a static initializer (§3.6); a C caller should do it
 * once at startup.
 *
 * # Errors
 *
 * [`TFT_ERR_ABI_MISMATCH`], with both version pairs in the error detail:
 * `frame_a`/`frame_b` carry the caller's major/minor, `plan_generation` and
 * `current_generation` the library's. The message names all four, because a
 * silently mismatched ABI is a debugging session nobody deserves.
 */
tft_status tft_check_abi(uint32_t compiled_major, uint32_t compiled_minor);

#if defined(TFT_HAVE_SHM)
/**
 * Join the running arena named by the environment, read-only.
 *
 * Mirrors `tf_tree::open()`: `$TF_TREE_DOMAIN`, `$TF_TREE_NAME` and
 * `$TF_TREE_RUNTIME_DIR` select which arena, and the attach is **read-only**
 * (D18) — a diagnostic or consumer process linked against this ABI cannot
 * corrupt a robot's transform tree, and the MMU is what enforces that rather
 * than our own care.
 *
 * On success `*out` receives a handle the caller must pass to
 * [`tft_tree_free`] exactly once.
 *
 * # Safety
 *
 * `out` must be NULL or point to a writable `*mut tft_tree`.
 */
tft_status tft_tree_open(tft_tree **out);
#endif

/**
 * Release a tree handle. Freeing NULL is a no-op.
 *
 * Any plan compiled from this tree stays valid: the underlying tree is
 * refcounted and this drops one reference (see [`tft_plan::share`]).
 *
 * # Safety
 *
 * `tree` must be NULL or a handle from a `tft_tree_*` constructor that has not
 * already been freed. Double-free is undefined; the magic word catches it in
 * every case that leaves the allocation intact, but not after the allocator has
 * reused the memory.
 */
void tft_tree_free(tft_tree *tree);

/**
 * Compile a plan for `target <- source`, by frame name.
 *
 * Plan compilation walks the topology once; evaluating the result is the hot
 * path (D3). A C caller should compile once and evaluate many times, exactly as
 * a Rust one would.
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
 * # Safety
 *
 * `plan` must be a handle from `tft_plan_create` that has not been freed.
 * `out` must point to at least `tft_layout_size(layout)` writable bytes.
 */
tft_status tft_plan_at(const tft_plan *plan, int64_t stamp, tft_layout layout, void *out);

/**
 * Evaluate `plan` at `n` stamps, writing each result `out_stride_bytes` apart.
 *
 * `out_stride_bytes == 0` means tightly packed. A stride larger than the
 * payload writes directly into an array of caller structs — §4.3 is why this
 * parameter exists at all (`Sophus::SE3d` is usually *not* tightly packed).
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
 *
 * `0` is a safe sentinel here precisely because no real layout has size zero.
 */
size_t tft_layout_size(tft_layout layout);

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
 * Exactly one participant may hold an edge (D7), across the whole machine when
 * the arena is shared. The claim is released by [`tft_publisher_release`] or
 * [`tft_publisher_free`]; a leaked handle is a leaked claim.
 *
 * The thread that calls this **owns** the resulting publisher — see §3.2 and
 * this module's documentation.
 *
 * # A frame name you have not used before is *created*, not rejected
 *
 * `Tree::frame` interns; it does not look up. So mistyping `child` declares a
 * new frame, which then has no incoming edge and the claim fails with
 * [`TFT_ERR_NO_EDGE`] — not [`TFT_ERR_UNKNOWN_FRAME`], which you only see once
 * the frame table's headroom is exhausted and the name genuinely cannot be
 * interned. Frame ids are never recycled (`docs/PROJECT.md` §5 D10), so a typo
 * costs a headroom slot for the life of the arena.
 *
 * That is Phase 2's interning semantics, shared with the Python binding and the
 * CLI, and it is documented here rather than special-cased at this boundary.
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
 * `src` must hold at least `tft_layout_size(layout)` bytes.
 *
 * `TFT_LAYOUT_AFFINE12_ROW_F32` is **not accepted**: it is an `f32` output
 * encoding for GPU upload, and publishing through it would silently halve the
 * precision of everything downstream. It returns `TFT_ERR_BAD_ENUM`.
 *
 * Matrix layouts are validated — a left-handed or scaled matrix is refused
 * rather than converted into a plausible wrong rotation. See
 * [`crate::layout::read`].
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
 * Publish `n` transforms, reading each `src_stride_bytes` apart.
 *
 * `src_stride_bytes == 0` means tightly packed. The stride exists for the same
 * reason it does on `tft_plan_at_many`: an array of `Sophus::SE3d` is usually
 * *not* tightly packed (§4.3).
 *
 * **Stops at the first rejected element**, leaving the earlier ones published.
 * That is the opposite of `tft_plan_at_many`'s all-or-nothing rule and it is
 * deliberate: a publication is not a buffer to be filled, it is a sequence of
 * independent release-stores that readers may already have observed. There is
 * no unpublishing. The failing index is reported in the error detail's
 * `frame_b` so the caller knows exactly where the stream stopped.
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
 *
 * The claim is *also* released by [`tft_publisher_free`]. This exists because a
 * C caller frequently wants to give the edge back at a known point — the end of
 * a calibration pass, say — while the handle's lifetime is managed elsewhere.
 * Calling it twice is a no-op, not an error.
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
