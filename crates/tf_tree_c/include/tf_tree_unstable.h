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

#if defined(TFT_HAVE_BRIDGE)
/*
 * The ingest bridge — docs/PHASE4.md §5.
 *
 *   tft_bridge  Send + !Sync   ONE THREAD AT A TIME
 *
 * Same affinity rule, and for a sharper reason than tft_publisher's: the handle
 * holds one claim per declared dynamic edge, so using it from a second thread
 * would write the arena from a thread that does not own those claims. §5.9 asks
 * for a dedicated SingleThreadedExecutor on its own thread, which is exactly the
 * shape this allows.
 *
 * Every const char * in tft_bridge_outcome is borrowed from the handle and
 * valid only until the next call on it. None is ever NULL; a field that does not
 * apply to an outcome is the empty string.
 */
typedef struct tft_bridge tft_bridge;
#endif  /* TFT_HAVE_BRIDGE */

/**
 * Bytes one twist occupies: `[ωx ωy ωz vx vy vz]`, `f64`, rad/s and m/s.
 */
#define TFT_TWIST_BYTES (6 * 8)

#if defined(TFT_HAVE_BRIDGE)
/**
 * §5.4's authority policy.
 */
typedef int32_t tft_bridge_authority;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * §5.5's response to the clock being judged to have moved, forwards or backwards.
 */
typedef int32_t tft_bridge_on_clock_reset;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * How the bridge is configured at creation.
 */
typedef struct {
  /**
   * `sizeof(tft_bridge_options)` in the caller's build (§3.6).
   */
  uint32_t struct_size;
  /**
   * One of the `TFT_BRIDGE_AUTHORITY_*` codes.
   */
  tft_bridge_authority authority;
  /**
   * One of the `TFT_BRIDGE_ON_CLOCK_RESET_*` codes.
   */
  tft_bridge_on_clock_reset on_clock_reset;
  /**
   * The time-domain tag the bridge stamps in (§5.5); every dynamic edge must agree or creation fails
   * with [`TFT_ERR_TIME_DOMAIN`]. Must fit in a `uint8_t`.
   */
  uint32_t domain;
  /**
   * `tf_prefix` remapping (§5.6), or NULL for none.
   */
  const char *tf_prefix;
  /**
   * Rendezvous name for a **shared** arena, or NULL for a private heap arena
   * (`docs/decisions/0015`).
   *
   * Failure is [`TFT_ERR_ARENA_UNAVAILABLE`](crate::TFT_ERR_ARENA_UNAVAILABLE), never a heap
   * fallback. Its rendezvous domain is `$TF_TREE_DOMAIN`, else `$ROS_DOMAIN_ID`, else 0
   * (`docs/decisions/0019` §3).
   */
  const char *arena_name;
} tft_bridge_options;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Which topic a sample arrived on (§5.7).
 */
typedef int32_t tft_bridge_topic;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * One `geometry_msgs/TransformStamped`; `pose` is `[qw qx qy qz tx ty tz]`, not `x y z w`.
 */
typedef struct {
  /**
   * `sizeof(tft_bridge_sample)` in the caller's build (§3.6).
   */
  uint32_t struct_size;
  /**
   * Parent frame, NUL-terminated UTF-8, as it arrived.
   */
  const char *frame_id;
  /**
   * Child frame, likewise raw.
   */
  const char *child_frame_id;
  /**
   * Stamp, nanoseconds, in the bridge's own time domain (§5.5).
   */
  int64_t stamp_nanos;
  /**
   * `[qw qx qy qz tx ty tz]`.
   */
  double pose[7];
  /**
   * A local steady-clock reading in nanoseconds, taken when the message arrived; `0` for none. Read
   * `rclcpp::Clock(RCL_STEADY_TIME)` once per `TFMessage`. **Never pass `stamp_nanos`** (§5.5).
   */
  int64_t received_steady_nanos;
} tft_bridge_sample;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * What happened to one offered transform.
 */
typedef int32_t tft_bridge_action;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Why a transform was dropped or the bridge halted.
 */
typedef int32_t tft_bridge_reason;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * What the bridge decided. Every `const char *` is borrowed until the next call on the handle; `""` if not applicable.
 */
typedef struct {
  /**
   * `sizeof(tft_bridge_outcome)` in the caller's build (§3.6). **Exact equality**: an `out` parameter.
   */
  uint32_t struct_size;
  /**
   * One of the `TFT_BRIDGE_*` action codes.
   */
  tft_bridge_action action;
  /**
   * One of the `TFT_BRIDGE_REASON_*` codes, or `TFT_BRIDGE_REASON_NONE`.
   */
  tft_bridge_reason reason;
  /**
   * The engine status when `action` is [`TFT_BRIDGE_REJECTED`], else [`TFT_OK`].
   */
  tft_status status;
  /**
   * `1` the first time this edge produced this outcome (§5.4, §5.6); also set on HALT and RECREATE.
   */
  uint8_t first_time;
  /**
   * How far time went **backwards**, as a positive magnitude; `0` otherwise.
   */
  int64_t by_nanos;
  /**
   * The parent frame: normalized (§5.6) where the pipeline named an edge, else as it arrived; empty
   * for a window close or a reported jump.
   */
  const char *parent;
  /**
   * The child frame, on the same terms as `parent`.
   */
  const char *child;
  /**
   * Who owns the edge, for an authority or static conflict.
   */
  const char *owner;
  /**
   * Who contradicted them.
   */
  const char *intruder;
  /**
   * The value on file, for [`TFT_BRIDGE_STATIC_CONFLICT`].
   */
  double existing[7];
  /**
   * The value just offered, for [`TFT_BRIDGE_STATIC_CONFLICT`].
   */
  double offered[7];
  /**
   * A one-line human-readable description, or `""`.
   */
  const char *detail;
  /**
   * New time minus old (a rewind is **negative**); `0` where not applicable.
   */
  int64_t delta_nanos;
  /**
   * A `TFT_BRIDGE_EVIDENCE_*` code: which rung of §5.5's ladder concluded the clock moved.
   */
  int32_t clock_evidence;
  /**
   * Per `clock_evidence`: the [`tft_bridge_jump_kind`] (reported), the publisher count (common-mode), or `0`.
   */
  uint32_t clock_evidence_detail;
} tft_bridge_outcome;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * One row of §5.6's remap table; both strings are borrowed until the next [`tft_bridge_get_remap`] call.
 */
typedef struct {
  /**
   * `sizeof(tft_bridge_remap)` in the caller's build (§3.6); exact equality.
   */
  uint32_t struct_size;
  /**
   * The name as it appears on `/tf`.
   */
  const char *from;
  /**
   * The name the arena declares, and the one a consumer must look up.
   */
  const char *to;
} tft_bridge_remap;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Which way the time source said its clock jumped ([`tft_bridge_note_time_jump`]); mirrors `rcl_time_jump_t`.
 */
typedef int32_t tft_bridge_jump_kind;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * §5.9's counters, plus two only the C layer sees. The ledger balances:
 *
 * ```text
 * applied + rejected_by_arena + static_verified
 *         + dropped_authority + dropped_non_monotonic + dropped_bad_name
 *         + dropped_kind_change + dropped_undeclared + dropped_bad_pose
 *         + refused_after_halt
 *     == transforms
 * ```
 */
typedef struct {
  /**
   * `sizeof(tft_bridge_stats)` in the caller's build (§3.6); exact equality.
   */
  uint32_t struct_size;
  /**
   * `TFMessage`es reported by [`tft_bridge_note_message`].
   */
  uint64_t messages;
  /**
   * Transforms offered, including those refused before the pipeline.
   */
  uint64_t transforms;
  /**
   * Transforms the arena took.
   */
  uint64_t applied;
  /**
   * `/tf_static` transforms that matched the declared constant (§5.7, §5.8).
   */
  uint64_t static_verified;
  /**
   * Dropped because another publisher owns the edge (§5.4).
   */
  uint64_t dropped_authority;
  /**
   * Transforms the clock rules refused (§5.5).
   */
  uint64_t dropped_non_monotonic;
  /**
   * Dropped because the frame name was unusable (§5.6).
   */
  uint64_t dropped_bad_name;
  /**
   * Dropped because the edge kind would have changed (§5.7).
   */
  uint64_t dropped_kind_change;
  /**
   * Dropped because the topology config does not declare the edge (§5.8). Look here first when a lookup has no path.
   */
  uint64_t dropped_undeclared;
  /**
   * Dropped because the pose was not a transform (NaN, non-unit quaternion).
   */
  uint64_t dropped_bad_pose;
  /**
   * The pipeline approved the write and the arena refused it.
   */
  uint64_t rejected_by_arena;
  /**
   * Offers refused after a `HALT` or `RECREATE`, both of which latch.
   */
  uint64_t refused_after_halt;
  /**
   * Clock resets concluded (§5.5): promotions, not regressions. Under `HALT` this is 0 or 1.
   */
  uint64_t clock_resets;
  /**
   * Static-transform value conflicts (§5.7).
   */
  uint64_t static_conflicts;
  /**
   * The deepest the subscription queue has been ([`tft_bridge_note_queue_depth`]).
   */
  uint32_t queue_high_water;
  /**
   * The subscription's configured depth (`100`, §5.2).
   */
  uint32_t queue_capacity;
} tft_bridge_stats;
#endif

/**
 * How [`tft_tree_inherit_ownership`] resolved. Mirrors `tf_tree::Inheritance`.
 *
 * Only `TFT_INHERITED` means this process is now the owner.
 */
typedef uint8_t tft_inheritance;

#if defined(TFT_HAVE_BRIDGE)
/**
 * Which rung of §5.5's ladder concluded that the clock moved.
 */
typedef int32_t tft_bridge_evidence;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * `/tf` — dynamic, volatile, `KeepLast(100)` (§5.2).
 */
#define TFT_BRIDGE_TOPIC_TF 0
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * `/tf_static` — latched, **transient_local**, `KeepLast(100)` (§5.2).
 */
#define TFT_BRIDGE_TOPIC_TF_STATIC 1
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The first attributed publisher of an edge owns it. **The default.**
 */
#define TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS 0
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Reclaim on each new publisher. Documented as chaotic; never the default.
 */
#define TFT_BRIDGE_AUTHORITY_LAST_WRITER_WINS 1
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Halt at the startup window's close if it recorded conflicts (`docs/decisions/0011`). For CI.
 */
#define TFT_BRIDGE_AUTHORITY_STRICT 2
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Stop and report. **The default.**
 */
#define TFT_BRIDGE_ON_CLOCK_RESET_HALT 0
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Report [`TFT_BRIDGE_RECREATE`] and let the caller rebuild (see [`tft_bridge_offer`]).
 */
#define TFT_BRIDGE_ON_CLOCK_RESET_RECREATE 1
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Written into the arena.
 */
#define TFT_BRIDGE_APPLIED 0
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * A `/tf_static` value matching the declared constant; nothing to write (§5.7, §5.8).
 */
#define TFT_BRIDGE_STATIC_VERIFIED 1
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Dropped. `reason` says why.
 */
#define TFT_BRIDGE_DROPPED 2
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * An edge the topology config does not declare (§5.8); `parent`, `child`, `first_time` are set.
 */
#define TFT_BRIDGE_UNDECLARED 3
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * A `/tf_static` value disagreeing with the one on file (§5.7); `existing`, `offered` are set.
 */
#define TFT_BRIDGE_STATIC_CONFLICT 4
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The bridge must stop. `reason` is the authority conflict or the clock reset.
 */
#define TFT_BRIDGE_HALT 5
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The clock moved under `RECREATE`: build a fresh bridge.
 */
#define TFT_BRIDGE_RECREATE 6
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The pipeline approved the write and the arena refused; `status` carries the engine's code.
 */
#define TFT_BRIDGE_REJECTED 7
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Not applicable to this outcome.
 */
#define TFT_BRIDGE_REASON_NONE 0
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The frame name was empty or only a slash (§5.6).
 */
#define TFT_BRIDGE_REASON_BAD_NAME 1
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Another publisher owns the edge (§5.4); `owner` and `intruder` are set.
 */
#define TFT_BRIDGE_REASON_NOT_THE_OWNER 2
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * **This edge's** stamp went backwards (§5.5); `delta_nanos` is negative. Dropped and counted.
 */
#define TFT_BRIDGE_REASON_NON_MONOTONIC 3
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The edge is already declared with the other kind (§5.7).
 */
#define TFT_BRIDGE_REASON_KIND_CHANGE 4
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * `STRICT`, and a conflict was recorded on an edge (§5.4); `owner`, `intruder`, `parent`, `child` name it.
 */
#define TFT_BRIDGE_REASON_AUTHORITY_CONFLICT 5
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The clock was judged to have moved (§5.5); see `clock_evidence` and `delta_nanos`.
 */
#define TFT_BRIDGE_REASON_CLOCK_RESET 6
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * NaN, infinity or a non-unit quaternion; checked before the pipeline ([`tft_bridge_offer`]).
 */
#define TFT_BRIDGE_REASON_BAD_POSE 7
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The bridge had already halted; the cause was reported earlier.
 */
#define TFT_BRIDGE_REASON_ALREADY_HALTED 8
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * `STRICT`'s startup window closed with conflicts recorded (§5.4, `docs/decisions/0011`); `detail`
 * lists every edge.
 */
#define TFT_BRIDGE_REASON_STARTUP_CONFLICTS 9
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The clock *source* changed (`use_sim_time` switched); the delta is not a duration.
 */
#define TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED 0
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Time moved backwards; `delta_nanos` is negative.
 */
#define TFT_BRIDGE_JUMP_BACKWARD 1
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Time moved forwards past the source's threshold; `delta_nanos` is positive.
 */
#define TFT_BRIDGE_JUMP_FORWARD 2
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * No clock judgment was made; `clock_evidence_detail` is `0`.
 */
#define TFT_BRIDGE_EVIDENCE_NONE 0
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The time source itself reported the jump; `clock_evidence_detail` is the [`tft_bridge_jump_kind`].
 */
#define TFT_BRIDGE_EVIDENCE_REPORTED 1
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Two or more publishers' offsets stepped together (the fallback); `clock_evidence_detail` is how many.
 */
#define TFT_BRIDGE_EVIDENCE_COMMON_MODE 2
#endif

/**
 * This process is now the owner and is serving the rendezvous.
 */
#define TFT_INHERITED 0

/**
 * `tft_tree_owner_lost` would have answered `false`; nothing was attempted.
 * Not final (`0057` Decision 3): call again.
 */
#define TFT_OWNER_ALIVE 1

/**
 * Another survivor or a fresh open took the ownership byte. Not final: call again.
 */
#define TFT_CONTENDED 2

/**
 * A read-only attachment cannot serve, so it cannot be the heir (D18).
 */
#define TFT_READ_ONLY 3

/**
 * A heap tree, a frozen `.tft`, or a tree this process already owns.
 */
#define TFT_NOT_APPLICABLE 4

#if defined(TFT_HAVE_BRIDGE)
/**
 * Build a bridge over the topology `config_toml` declares (text, not a path; `docs/decisions/0004`),
 * and its arena.
 *
 * Claims every declared dynamic edge and refuses to start if any fails. The calling thread **owns**
 * the bridge. `opts->struct_size` selects the layout; the pre-`arena_name` one is a prefix (§3.6).
 *
 * # Blocking
 *
 * A non-NULL `opts->arena_name` may block up to `DEFAULT_OPEN_TIMEOUT` (5 s).
 *
 * # Errors
 *
 * * [`TFT_ERR_BAD_CONFIG`] — unparsable, no edges, a cycle, or will not build.
 * * [`TFT_ERR_TIME_DOMAIN`] — a dynamic edge's domain is not `opts->domain` (§5.5).
 * * The claim family — another participant holds a declared edge.
 * * [`TFT_ERR_ARENA_UNAVAILABLE`](crate::TFT_ERR_ARENA_UNAVAILABLE) — `arena_name` could not be
 *   served; no heap fallback.
 * * [`TFT_ERR_BAD_STRUCT_SIZE`] — `opts->struct_size` is not a known size.
 *
 * # Safety
 *
 * `config_toml` must be NUL-terminated UTF-8. `opts` must be NULL or point to a
 * `tft_bridge_options` whose `struct_size` is set **and which has at least that many readable
 * bytes**. `out` must be NULL or point to a writable `*mut tft_bridge`.
 */
tft_status tft_bridge_create(const char *config_toml,
                             const tft_bridge_options *opts,
                             tft_bridge **out);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * A [`tft_tree`] handle onto this bridge's arena, for reading. Independently owned; free it with
 * [`tft_tree_free`](crate::tft_tree_free). `Send + Sync`.
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `out` must be NULL or point to a
 * writable `*mut tft_tree`.
 */
tft_status tft_bridge_tree(tft_bridge *b, tft_tree **out);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Release the bridge, its claims and its arena reference. Freeing NULL is a no-op.
 *
 * # Safety
 *
 * `b` must be NULL or a handle from [`tft_bridge_create`] not already freed,
 * and must be freed from the thread that created it.
 */
void tft_bridge_free(tft_bridge *b);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Offer one transform: run every §5 table, then write the arena.
 *
 * `gid` is `rmw_message_info_t::publisher_gid` (16 bytes) or NULL; an unresolved GID is not an error
 * (§5.3).
 *
 * # The return value answers a different question from the outcome
 *
 * The status says whether the *call* was well-formed; what happened to the sample is in `*out`.
 *
 * # Orderings
 *
 * The pose is validated before the pipeline, so a garbage first message cannot take an edge. A
 * halted bridge refuses everything.
 *
 * # `TFT_BRIDGE_RECREATE` is a report, not an action
 *
 * The caller tears the bridge down, rebuilds it, and re-plans.
 *
 * # An older caller's sample still works
 *
 * The pre-`received_steady_nanos` size is a prefix (§3.6); a larger size is refused
 * ([`tft_check_abi`](crate::tft_check_abi)). The missing field comes from this library's steady
 * clock, never `stamp_nanos`.
 *
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `s` must point to a
 * `tft_bridge_sample` with `struct_size` set and at least that many readable bytes, and both frame
 * pointers NUL-terminated. `gid` must be NULL or point to 16 readable bytes. `out` must point to a
 * writable `tft_bridge_outcome` with `struct_size` set.
 */
tft_status tft_bridge_offer(tft_bridge *b,
                            tft_bridge_topic topic,
                            const tft_bridge_sample *s,
                            const uint8_t *gid,
                            tft_bridge_outcome *out);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Record that `gid` belongs to `node_name` (§5.3); a known GID's name is **replaced**. An all-zero
 * `gid` is refused with [`TFT_ERR_BAD_ENUM`](crate::TFT_ERR_BAD_ENUM).
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `gid` must point to 16 readable
 * bytes and `node_name` be NUL-terminated UTF-8.
 */
tft_status tft_bridge_attribute(tft_bridge *b, const uint8_t *gid, const char *node_name);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Read row `index` of §5.6's remap table; the table is complete before the first message.
 *
 * ```c
 * tft_bridge_remap r = { .struct_size = sizeof r };
 * for (uint32_t i = 0; tft_bridge_get_remap(b, i, &r) == TFT_OK; i++)
 *     RCLCPP_INFO(log, "tf_tree: frame %s is declared as %s", r.from, r.to);
 * ```
 *
 * An empty table returns [`TFT_ERR_NO_DATA`](crate::TFT_ERR_NO_DATA) on the first call.
 *
 * # Errors
 *
 * * [`TFT_ERR_NO_DATA`](crate::TFT_ERR_NO_DATA) — `index` is past the last row.
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `out` must point to a writable
 * `tft_bridge_remap` whose `struct_size` is set.
 */
tft_status tft_bridge_get_remap(tft_bridge *b, uint32_t index, tft_bridge_remap *out);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Note that a `TFMessage` arrived (§5.9).
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it.
 */
tft_status tft_bridge_note_message(tft_bridge *b);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * **The time source itself said its clock jumped** — §5.5's authoritative path.
 *
 * Feed it `rcl_time_jump_t` from `rcl_clock_add_jump_callback`. `delta_nanos` is
 * `delta.nanoseconds` unnegated (a rewind is negative).
 *
 * # Not from the jump callback
 *
 * rclcpp's jump callback runs off the bridge's thread: record the jump there and call this from the
 * ingest thread.
 *
 * # Charges no counter
 *
 * No ledger term moves; `clock_resets` does increment.
 *
 * # Errors
 *
 * * [`TFT_ERR_BAD_ENUM`](crate::TFT_ERR_BAD_ENUM) — `kind` is not one of the three codes.
 *
 * A stopped bridge is not an error: `*out` replays the latched action with
 * [`TFT_BRIDGE_REASON_ALREADY_HALTED`].
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `out` must point to a writable
 * `tft_bridge_outcome` with `struct_size` set.
 */
tft_status tft_bridge_note_time_jump(tft_bridge *b,
                                     int64_t delta_nanos,
                                     tft_bridge_jump_kind kind,
                                     tft_bridge_outcome *out);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Close `STRICT`'s startup window (§5.4), halting once if conflicts were recorded in it
 * (`docs/decisions/0011` step 6). Charges no counter.
 *
 * # Closing too early costs the policy
 *
 * A window closing before the conflicting sample arrives reports nothing, and `STRICT` degrades to
 * `FirstWriterWins` plus counters; the caller owns the duration.
 *
 * # Outcomes
 *
 * * **Conflicts recorded** — [`TFT_BRIDGE_HALT`] with [`TFT_BRIDGE_REASON_STARTUP_CONFLICTS`];
 *   latched.
 * * **None, or not `STRICT`** — the blank outcome.
 * * **Called twice, or already halted** — replays [`TFT_BRIDGE_REASON_ALREADY_HALTED`] if halted,
 *   else the blank outcome.
 *
 * # Bridge thread only
 *
 * §3.2's affinity applies: a timer in the node's default callback group fires on the wrong thread
 * and the window never closes (`ros/tf_tree_ros/src/bridge_handle.cpp`).
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `out` must point to a writable
 * `tft_bridge_outcome` with `struct_size` set.
 */
tft_status tft_bridge_close_startup_window(tft_bridge *b, tft_bridge_outcome *out);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Report the subscription queue depth (§5.9). The high-water mark is kept.
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it.
 */
tft_status tft_bridge_note_queue_depth(tft_bridge *b, uint32_t depth);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Copy §5.9's counters into `out`. Named `get_stats` because a `tft_bridge_stats` function would
 * collide with the struct's typedef in C.
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `out` must point to a writable
 * `tft_bridge_stats` whose `struct_size` is set.
 */
tft_status tft_bridge_get_stats(tft_bridge *b, tft_bridge_stats *out);
#endif

/**
 * Evaluate `plan` at `stamp`, reporting the pose **and its first derivative**.
 *
 * `out_pose` receives `tft_layout_size(layout)` bytes; `out_twist` receives
 * [`TFT_TWIST_BYTES`]. Either may be NULL, and that half is then not written.
 *
 * [`crate::TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] puts both halves in `out_pose`
 * (`docs/API.md` §3.3). The twist is a body twist in the plan's **source** frame.
 *
 * # Errors
 *
 * * `TFT_ERR_NO_DERIVATIVES` — an edge interpolates with `LerpSlerp` (§2.4).
 * * `TFT_ERR_NO_SEGMENT` — no segment to differentiate at this stamp.
 *
 * # Safety
 *
 * `plan` must be live; a non-NULL `out_pose` needs `tft_layout_size(layout)`
 * writable bytes, a non-NULL `out_twist` [`TFT_TWIST_BYTES`].
 */
tft_status tft_plan_at_with_derivatives(const tft_plan *plan,
                                        int64_t stamp,
                                        tft_layout layout,
                                        void *out_pose,
                                        double *out_twist);

/**
 * How many frames this tree has declared, including tombstoned ones.
 *
 * Valid frame ids are `1 ..= tft_tree_frame_count()` (append-only; id `0` is the
 * root sentinel, `TFT_ERR_UNKNOWN_FRAME` in [`tft_tree_frame_name`]).
 *
 * Returns `0` for a NULL or dead handle, as for an empty tree.
 *
 * # Safety
 *
 * `tree` must be NULL or a live handle.
 */
uint32_t tft_tree_frame_count(const tft_tree *tree);

/**
 * How many edges this tree has declared, including tombstoned ones.
 *
 * Valid edge ids are `1 ..= tft_tree_edge_count()`, as for
 * [`tft_tree_frame_count`].
 *
 * # Safety
 *
 * `tree` must be NULL or a live handle.
 */
uint32_t tft_tree_edge_count(const tft_tree *tree);

/**
 * Copy frame `id`'s name into `buf`, NUL-terminated.
 *
 * Returns `TFT_ERR_BUFFER_TOO_SMALL` without writing when the name plus its NUL
 * does not fit, with the error detail's `requested` set to the bytes needed.
 *
 * The arena keeps at most 48 bytes of a name, so 64 bytes always fits.
 *
 * # Safety
 *
 * `tree` must be a live handle. `buf` must point to `buf_len` writable bytes.
 */
tft_status tft_tree_frame_name(const tft_tree *tree, uint32_t id, char *buf, size_t buf_len);

/**
 * Copy this tree's 16-byte arena instance UUID into `out`.
 *
 * A heap arena has none (`docs/PHASE2.md` §1, A1): `TFT_ERR_NO_DATA`, nothing written.
 *
 * # Safety
 *
 * `tree` must be a live handle. `out` must point to 16 writable bytes.
 */
tft_status tft_tree_instance_uuid(const tft_tree *tree, uint8_t *out);

#if defined(TFT_HAVE_SHM)
/**
 * Join a shared arena by name, **read-write** if asked (`0044`).
 *
 * * `name` — NULL for the environment's default, as `tft_tree_open`.
 * * `read_write` — `false` is the consumer default (D18); `true` only to
 *   publish, reap, or inherit the owner role.
 *
 * Never creates: a missing arena is `TFT_ERR_ARENA_UNAVAILABLE`.
 *
 * # Safety
 *
 * `name` must be NULL or NUL-terminated; `out` must be writable.
 */
tft_status tft_tree_open_named(const char *name, bool read_write, tft_tree **out);
#endif

#if defined(TFT_HAVE_SHM)
/**
 * Has the process that owns this arena gone away (`docs/PHASE2.md` §3.5)?
 *
 * Answers "the arena has no owner" (`0043`); `false` for anything but a joined
 * rendezvous attachment. Non-blocking (`0057`).
 *
 * Caller-driven (`0019`): pair it with [`tft_tree_inherit_ownership`].
 *
 * # Safety
 *
 * `tree` must be NULL or a live handle; `out` must be a writable `bool`.
 */
tft_status tft_tree_owner_lost(const tft_tree *tree, bool *out);
#endif

#if defined(TFT_HAVE_SHM)
/**
 * Inherit the owner role from a departed owner and begin serving
 * (`docs/PHASE2.md` §3.5; `0044`).
 *
 * Writes one of the `TFT_INHERITED` … `TFT_NOT_APPLICABLE` values. On failure
 * the process stays a plain participant.
 *
 * # Safety
 *
 * `tree` must be NULL or a live handle; `out` must be a writable
 * `tft_inheritance`.
 */
tft_status tft_tree_inherit_ownership(const tft_tree *tree, tft_inheritance *out);
#endif

#if defined(TFT_HAVE_SHM)
/**
 * Free what dead participants left behind and write how many records were
 * freed: stale claim leases (`Tree::reap_dead`) plus participant records
 * (`Tree::reap_participants`). Needed after a dead owner, which has no hangup
 * (`docs/PHASE2.md` §6.3).
 *
 * A process tree containing a `build_shared` participant with no socket is out
 * of contract (`0031`): sweeping frees *live* publishers' claims.
 *
 * Writes `0` for a read-only, heap or rendezvous-less tree.
 *
 * # Safety
 *
 * `tree` must be NULL or a live handle; `out` must be a writable `uint32_t`.
 */
tft_status tft_tree_reap_dead(const tft_tree *tree, uint32_t *out);
#endif

#ifdef __cplusplus
}  /* extern "C" */
#endif

#endif  /* TF_TREE_UNSTABLE_H */
