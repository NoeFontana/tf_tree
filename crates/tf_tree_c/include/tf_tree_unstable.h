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
 *
 * There is deliberately no `tft_twist_layout` enum. A twist is a 6-vector in
 * one universally agreed order (`tf_tree_math::twist`'s convention, which is
 * also Sophus's and Pinocchio's), so the quaternion-order trap §3.5 exists for
 * has no analogue here — and inventing a second layout enum would create one.
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
 * §5.5's response to the clock being judged to have moved.
 *
 * **Not only backwards.** Since the authoritative path
 * ([`tft_bridge_note_time_jump`]) and the common-mode path both see a sim
 * fast-forward or a bag seek, this policy applies to a *forward* jump too — a
 * backward-regression watcher structurally could not see one.
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
   * The time-domain tag the bridge stamps in — `use_sim_time` decides it
   * (§5.5). Every declared *dynamic* edge must agree, or creation fails with
   * [`TFT_ERR_TIME_DOMAIN`]: sim and real transforms in one arena is a class
   * of bug worth making impossible, and §5.5 is NORMATIVE that it fails at
   * startup rather than at first message. Must fit in a `uint8_t`.
   */
  uint32_t domain;
  /**
   * `tf_prefix` remapping (§5.6), or NULL for none.
   */
  const char *tf_prefix;
} tft_bridge_options;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Which topic a sample arrived on. `/tf_static` is latched and its stamp is
 * meaningless (§5.7), which is why the bridge has to be told rather than
 * guessing from the sample.
 */
typedef int32_t tft_bridge_topic;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * One `geometry_msgs/TransformStamped`, in the ABI's terms.
 *
 * `pose` is `[qw qx qy qz tx ty tz]` — the canonical order (`docs/PHASE1.md`
 * §3.1), **not** `geometry_msgs`' `x y z w`. The ROS half reorders; that is a
 * four-line conversion in the caller and a conversion this side cannot get
 * wrong on the caller's behalf.
 */
typedef struct {
  /**
   * `sizeof(tft_bridge_sample)` in the caller's build (§3.6).
   *
   * **A size from before `received_steady_nanos` existed is accepted**, and read as
   * the prefix it is — see [`tft_bridge_offer`]'s *"An older caller's sample
   * still works"*.
   */
  uint32_t struct_size;
  /**
   * Parent frame, NUL-terminated UTF-8, **exactly as it arrived**. Passing
   * the raw name is deliberate: §5.6's normalization is what the bridge is
   * for, and a pre-normalized name would move that judgment into C++.
   */
  const char *frame_id;
  /**
   * Child frame, likewise raw.
   */
  const char *child_frame_id;
  /**
   * Stamp, nanoseconds, in the bridge's own time domain (§5.5).
   *
   * **The publisher's number, in the domain under suspicion.** Nothing §5.5
   * concludes about the clock is concluded by comparing this against another
   * publisher's stamp; it is compared against `received_steady_nanos`.
   */
  int64_t stamp_nanos;
  /**
   * `[qw qx qy qz tx ty tz]`.
   */
  double pose[7];
  /**
   * A reading of a local **steady (monotonic)** clock, in nanoseconds, taken
   * when the message carrying this transform arrived. `0` for "none".
   *
   * # Where a ROS caller gets one
   *
   * `rclcpp::Clock(RCL_STEADY_TIME).now().nanoseconds()`, read **once per
   * `TFMessage`** at subscription-callback entry and copied onto every sample
   * the message expands into.
   *
   * Not `node->get_clock()`: that is `RCL_ROS_TIME`, which under
   * `use_sim_time` *is* `/clock` — the clock under test. A detector whose
   * reference is the signal it is judging cannot judge it. `RCL_STEADY_TIME`
   * is unaffected by `use_sim_time`, which is the entire reason it is the
   * reference.
   *
   * Not once per transform, either: that puts a clock read on a 1 kHz path,
   * and it turns one measurement of a publisher's offset into twenty
   * slightly different ones.
   *
   * # What it is for, and what `0` costs
   *
   * §5.5 measures `stamp_nanos - received_steady_nanos` per publisher. That
   * difference *is* the publisher's `transform_tolerance` — a localizer
   * dating `map -> odom` 300 ms into the future has a steady offset of
   * +300 ms — so it is measured and subtracted rather than mistaken for a
   * jump. A **step** in it, agreed on by two or more distinct publishers
   * inside one correlation window, is the fallback evidence that the clock
   * moved.
   *
   * `0` means the caller has no steady clock to offer. The offset layer is
   * then simply absent for that sample: per-edge monotonicity is still
   * enforced and non-monotonic samples are still dropped and counted, so the
   * arena is protected exactly as before, and only the *corroborated* clock
   * verdict is unavailable. That is the honest degradation, and a safe one,
   * because a single witness never halts anything.
   *
   * **Do not pass `stamp_nanos` here.** It makes the difference identically
   * zero for every publisher, which re-enables inference over the signal
   * under suspicion and resurrects the `transform_tolerance` false positive
   * this field exists to remove.
   *
   * The name says *which clock*, and that is not verbosity. The whole bug
   * class this design removes is two clocks being confused for one, and a
   * field called `received_steady_nanos` sitting next to `stamp_nanos` would be an
   * invitation to fill it from whichever one was nearest.
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
 * What the bridge decided, and everything needed to print a sentence about it.
 *
 * **Every `const char *` here is borrowed from the handle and valid only until
 * the next call on that handle.** They are never NULL: a field that does not
 * apply to this outcome is the empty string. That is the same lifetime rule
 * [`tft_last_error`](crate::tft_last_error) already states for `tft_error`, and
 * stating it twice is cheaper than one node logging a dangling pointer.
 */
typedef struct {
  /**
   * `sizeof(tft_bridge_outcome)` in the caller's build (§3.6).
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
   * The engine status, when `action` is [`TFT_BRIDGE_REJECTED`]; otherwise
   * [`TFT_OK`].
   */
  tft_status status;
  /**
   * `1` the first time this edge produced this outcome, `0` afterwards — the
   * rate limiter behind §5.6's "warn once" and §5.8's "once per edge". An
   * undeclared 1 kHz edge otherwise emits a thousand identical lines a
   * second.
   *
   * **Set on every outcome a caller is expected to log, including
   * [`TFT_BRIDGE_HALT`] and [`TFT_BRIDGE_RECREATE`].** Those two are latched:
   * the offer that stops the bridge carries `first_time = 1` and every offer
   * after it replays the same action with `first_time = 0`, because the
   * bridge answers `HALT` to every later transform forever. A caller that
   * logged them unconditionally would emit one line per transform for the
   * life of the process — at 20 edges and 100 Hz, 2000 `FATAL` lines a
   * second, each taking the logging mutex on the ingest thread and burying
   * the one actionable line. §5.4 requires the diagnostic be "loud,
   * **rate-limited**"; this field is the whole of that mechanism.
   */
  uint8_t first_time;
  /**
   * How far time went **backwards**, as a positive magnitude. `0` when it did
   * not.
   *
   * This is a *distance*, and it is unchanged: for
   * [`TFT_BRIDGE_REASON_NON_MONOTONIC`] it is how far this edge's stamp fell
   * short of its own last accepted one, which is what a caller printing
   * *"went backwards by %ld ns"* wants and always wanted.
   *
   * **Not the same field as [`tft_bridge_outcome::delta_nanos`], and
   * deliberately not merged with it.** One is a backwards distance and the
   * other is a signed displacement; they agree in magnitude on a rewind and
   * say different things on a jump forwards, where this is `0` and
   * `delta_nanos` is positive. C has no type that carries the distinction, so
   * the two names are the only thing preserving it — a later tidy-up that
   * collapsed them would print *"went backwards by -5000000000 ns"* on
   * exactly the fault the sentence exists for.
   */
  int64_t by_nanos;
  /**
   * The parent frame. Normalized (§5.6) for every outcome the pipeline
   * named an edge in; **as it arrived** for `TFT_BRIDGE_DROPPED`,
   * `TFT_BRIDGE_HALT` and `TFT_BRIDGE_RECREATE`, whose actions carry only a
   * reason. The difference is one leading `/` and any `tf_prefix`, so the
   * pair identifies the same edge either way — and for
   * [`TFT_BRIDGE_REASON_BAD_NAME`] the raw name is the only useful one.
   *
   * **Empty when the outcome is not about an arriving transform**: a
   * `STRICT` startup-window close, and a jump reported through
   * [`tft_bridge_note_time_jump`]. Both are judgments about transforms
   * counted earlier or about no transform at all, so any edge they named
   * would be an innocent one.
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
   * How far time moved, and **which way**: new time minus old time, so a
   * rewind is **negative**. `0` where it does not apply.
   *
   * Set for [`TFT_BRIDGE_REASON_CLOCK_RESET`] and [`TFT_BRIDGE_RECREATE`] —
   * the clock event itself, however it was concluded — and for
   * [`TFT_BRIDGE_REASON_NON_MONOTONIC`], where it is the negation of
   * `by_nanos`.
   *
   * **Signed, because the clock can now be judged to have moved forwards.**
   * An authoritative jump report and a common-mode step both see a bag seek
   * or a sim fast-forward, which no backward-regression watcher could. The
   * convention is `rcl_time_jump_t::delta`'s — *"the new time minus the last
   * time before the jump"* — so the number a node reads out of `rcl` and the
   * number it reads back out of this struct are the same quantity, with no
   * conversion nobody would remember to write.
   */
  int64_t delta_nanos;
  /**
   * **Which rung of §5.5's ladder concluded the clock moved** — one of the
   * `TFT_BRIDGE_EVIDENCE_*` codes, or [`TFT_BRIDGE_EVIDENCE_NONE`].
   *
   * This is the first thing an operator woken at 3 a.m. by a stopped bridge
   * needs, before the edge and before the delta. *"The time source reported a
   * backward jump"* is a fact: go and look at the bag or the simulator.
   * *"Three publishers stepped together by about the same amount"* is an
   * inference, well corroborated but capable of being wrong in a way the
   * first is not: go and look at those three nodes.
   *
   * The pipeline knows which one fired and used to discard it at this
   * boundary, leaving `detail` — a sentence — as the only carrier. A code is
   * what a caller can branch on.
   */
  int32_t clock_evidence;
  /**
   * What the evidence consisted of, read according to `clock_evidence`:
   *
   * * [`TFT_BRIDGE_EVIDENCE_REPORTED`] — the [`tft_bridge_jump_kind`] the
   *   time source reported.
   * * [`TFT_BRIDGE_EVIDENCE_COMMON_MODE`] — how many distinct publishers
   *   stepped together and agreed. Always ≥ 2; one witness never concludes
   *   anything.
   * * [`TFT_BRIDGE_EVIDENCE_NONE`] — `0`, and meaningless.
   */
  uint32_t clock_evidence_detail;
} tft_bridge_outcome;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * One row of §5.6's remap table: a frame name as it arrives, and the name the
 * arena knows it by.
 *
 * Both strings are borrowed from the handle and valid until the next
 * [`tft_bridge_get_remap`] call on it — the same rule as
 * [`tft_bridge_outcome`], and deliberately *not* invalidated by
 * [`tft_bridge_offer`], because the startup loop that reads this table logs as
 * it walks.
 */
typedef struct {
  /**
   * `sizeof(tft_bridge_remap)` in the caller's build (§3.6).
   */
  uint32_t struct_size;
  /**
   * The name as it appears on `/tf` — and in every launch file and RViz
   * config on the robot.
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
 * Which way, and in what sense, the time source said its clock jumped —
 * [`tft_bridge_note_time_jump`].
 *
 * Mirrors `rcl_time_jump_t`: `rcl_clock_change_t` distinguishes a change of
 * time *source* from motion within one source, and `rcl_duration_t delta` is
 * *"the new time minus the last time before the jump"*.
 */
typedef int32_t tft_bridge_jump_kind;
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * §5.9's counters, plus the two the C layer alone can see.
 *
 * **The ledger balances**, and that is the point of exposing it rather than a
 * prose summary:
 *
 * ```text
 * applied + rejected_by_arena + static_verified
 *         + dropped_authority + dropped_non_monotonic + dropped_bad_name
 *         + dropped_kind_change + dropped_undeclared + dropped_bad_pose
 *         + refused_after_halt
 *     == transforms
 * ```
 *
 * A mismatch means some path returns without counting, which is how "we are
 * not dropping anything" becomes false with no test failing.
 *
 * `refused_after_halt` is a term because `transforms` counts those offers:
 * the first revision of this comment omitted it, so the documented ledger
 * stopped balancing the moment a bridge halted — which is precisely the moment
 * an operator starts reading the counters.
 */
typedef struct {
  /**
   * `sizeof(tft_bridge_stats)` in the caller's build (§3.6).
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
   * Transforms **the arena took**. Not the number the pipeline approved:
   * `rejected_by_arena` is subtracted, so this field means what its name
   * says and a caller watching it is watching the arena.
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
   * Transforms **the clock rules refused** (§5.5).
   *
   * Named for the common case and wider than the name: an edge's stamp going
   * backwards against its own last accepted one, at any magnitude, *and* the
   * sample that completed a common-mode step — which may be perfectly
   * monotone, because the clock can be judged to have jumped forward. There
   * is one bucket for "refused because time misbehaved" and a second one
   * would be a `struct_size`-versioned growth of this struct, so the meaning
   * is stated here rather than left to the name to imply.
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
   * Dropped because the topology config does not declare the edge (§5.8).
   * **The counter to look at first** when a lookup returns no path.
   */
  uint64_t dropped_undeclared;
  /**
   * Dropped because the pose was not a transform (NaN, or a non-unit
   * quaternion). `tf2` has no equivalent check and no equivalent counter.
   */
  uint64_t dropped_bad_pose;
  /**
   * The pipeline approved the write and the arena refused it — a revoked
   * claim, or a writer poisoned by a `fork()`. **Not a stamp the clock guard
   * missed:** since `docs/decisions/0011` the guard is per edge, so its
   * high-water mark is that edge's own last accepted stamp and the ring it
   * feeds cannot disagree with it.
   */
  uint64_t rejected_by_arena;
  /**
   * Offers refused because the bridge had already stopped — after a
   * [`TFT_BRIDGE_HALT`] *or* a [`TFT_BRIDGE_RECREATE`], both of which latch.
   */
  uint64_t refused_after_halt;
  /**
   * Clock resets concluded (§5.5) — **promotions**, not regressions.
   *
   * A single publisher's stamp going backwards is counted in
   * `dropped_non_monotonic` and nowhere else, however far it went. This
   * counts the times the clock itself was judged to have moved, by either
   * rung of §5.5's ladder: a jump the time source reported through
   * [`tft_bridge_note_time_jump`], or two or more distinct publishers whose
   * offsets stepped by the same amount inside one correlation window. Under
   * `HALT` it is therefore 0 or 1 for the life of a bridge.
   */
  uint64_t clock_resets;
  /**
   * Static-transform value conflicts (§5.7).
   */
  uint64_t static_conflicts;
  /**
   * The **deepest** the subscription queue has been, as reported by
   * [`tft_bridge_note_queue_depth`] — not its depth now. A queue that fills
   * only between two samples is invisible to polling and is exactly the
   * condition that drops transforms.
   */
  uint32_t queue_high_water;
  /**
   * The subscription's configured depth, so the high-water mark reads as a
   * fraction. `100` per §5.2.
   */
  uint32_t queue_capacity;
} tft_bridge_stats;
#endif

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
 * `/tf_static` — latched, **transient_local**, `KeepLast(100)` (§5.2). A
 * `volatile` subscription here receives nothing from publishers that started
 * earlier, which is the single most common ROS 2 tf integration bug.
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
 * Refuse to start if a conflict is detected within the startup window. For CI.
 *
 * **Not "halt on the first conflict"**, which is what this said and what the
 * code did before `docs/decisions/0011`. A conflict inside the window is
 * dropped and counted like `FIRST_WRITER_WINS`, and the bridge halts **once**,
 * at the window's close, reporting everything it found — CI wants every
 * misconfiguration out of one run, not the first one out of four. Outside the
 * window this policy *is* `FIRST_WRITER_WINS` plus counters, so a bridge that
 * has been healthy for an hour is not killed by a late-joining publisher.
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
 * Report [`TFT_BRIDGE_RECREATE`] and let the caller rebuild. See
 * [`tft_bridge_offer`] for why the ABI cannot recreate the arena itself.
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
 * A `/tf_static` value matching the declared constant. Nothing to write; the
 * arena already holds it (§5.7 idempotent, §5.8 verification).
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
 * A transform for an edge the topology config does not declare (§5.8).
 * `parent`, `child` and `first_time` are set.
 */
#define TFT_BRIDGE_UNDECLARED 3
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * A `/tf_static` value that disagrees with the one on file (§5.7).
 * `owner`, `intruder`, `existing` and `offered` are all set — the diagnostic
 * §5.7 requires names both publishers **and both values**.
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
 * The clock moved under `RECREATE`: the caller must tear this bridge down and
 * build a fresh one. `delta_nanos` says how far, and which way.
 */
#define TFT_BRIDGE_RECREATE 6
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The pipeline said write and **the arena refused**. `status` carries the
 * engine's status code, which is the one an operator can act on.
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
 * Another publisher owns the edge (§5.4). `parent`, `child`, `owner`,
 * `intruder` and `first_time` are **all** set — that is §5.4's diagnostic, and
 * `first_time` is what keeps it to one line per pair of colliding publishers
 * rather than one per message.
 */
#define TFT_BRIDGE_REASON_NOT_THE_OWNER 2
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * **This edge's** stamp went backwards (§5.5). `delta_nanos` says how far, and
 * is negative.
 *
 * One publisher's stamps arriving out of order, at any magnitude: a few
 * milliseconds of interleaving, or a node that restarted and is replaying its
 * own buffer from five seconds ago. The sample is dropped and counted either
 * way, which is the whole disposition — Phase 1's ring would refuse these
 * stamps regardless, so the arena is protected without the bridge stopping.
 *
 * **Distance is not evidence about the clock.** A lone regression is never
 * promoted to [`TFT_BRIDGE_REASON_CLOCK_RESET`], however far it goes; that
 * needs a reported jump or corroboration from a second publisher.
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
 * `STRICT`, and a conflict was recorded on an edge (§5.4).
 *
 * On a [`TFT_BRIDGE_HALT`] this is `STRICT`'s startup window closing with
 * conflicts in it. `detail` carries how many of each kind — authority (§5.4)
 * **and** static-value (§5.7) — because the halt is about a set of edges and
 * this POD has room for one. `owner` and `intruder` are empty there, and so are
 * `parent`/`child`: the window closed on transforms counted long before the one
 * in hand, so there is no edge to name that would not be the wrong one.
 */
#define TFT_BRIDGE_REASON_AUTHORITY_CONFLICT 5
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The clock was judged to have moved (§5.5). `delta_nanos` is by how much —
 * **negative for a rewind** — and `detail` names *which rung of §5.5's ladder
 * fired*, because they are not equally strong:
 *
 * * *"the time source reported it"* — [`tft_bridge_note_time_jump`], the
 *   authoritative path. No threshold, no window, no corroboration. This is a
 *   fact, and an operator reading it should look at the bag or the simulator.
 * * *"N publishers stepped together"* — the fallback path, where two or more
 *   distinct publishers' stamp-to-receipt offsets moved by the same amount
 *   inside one correlation window. This is an inference, well corroborated;
 *   its one false-positive mode is two nodes restarting in lockstep, which is
 *   what the operator would go and look at.
 *
 * **A single publisher regressing is never this.** It is
 * [`TFT_BRIDGE_REASON_NON_MONOTONIC`], dropped and counted, because one node
 * restarting, hiccuping or replaying its own buffer is observationally
 * identical to it and halting a healthy robot for it is an outage caused by the
 * diagnostic rather than by the fault.
 *
 * `parent`/`child` name the edge whose sample completed a common-mode step, and
 * are **empty** for a reported jump: that entry point has no transform in hand,
 * so any edge it named would be an innocent one.
 */
#define TFT_BRIDGE_REASON_CLOCK_RESET 6
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The pose was not a transform: NaN, infinity, or a quaternion that is not a
 * unit quaternion. Checked **before** the pipeline — see [`tft_bridge_offer`].
 */
#define TFT_BRIDGE_REASON_BAD_POSE 7
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The bridge had already halted and this offer was refused without being
 * processed. The halt that caused it reported the actionable reason — and both
 * publishers' names, if it was an authority conflict — on the outcome
 * **before** this one.
 */
#define TFT_BRIDGE_REASON_ALREADY_HALTED 8
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The clock *source* changed: `use_sim_time` was switched at runtime
 * (`RCL_ROS_TIME_ACTIVATED` / `RCL_ROS_TIME_DEACTIVATED`).
 *
 * Its own kind rather than a large backward or forward jump because the delta
 * across that boundary compares two different time bases and is not a duration
 * in either of them.
 */
#define TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED 0
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Time moved backwards: a bag loop, a sim reset, an NTP step back.
 * `delta_nanos` is negative.
 */
#define TFT_BRIDGE_JUMP_BACKWARD 1
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Time moved forwards past the source's reporting threshold: a bag seek, a sim
 * fast-forward, an NTP step. `delta_nanos` is positive.
 *
 * **Only the authoritative path can see this cheaply.** A forward jump leaves
 * every edge's stamps perfectly monotone, so nothing in the per-edge machinery
 * is even disturbed by it.
 */
#define TFT_BRIDGE_JUMP_FORWARD 2
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * No clock judgment was made on this outcome, and
 * `clock_evidence_detail` is `0`.
 *
 * The value **every** outcome starts at, set by `blank_outcome` before any arm
 * runs, so a caller reading these two fields on an unrelated outcome sees
 * "nothing to report" rather than the last clock event's evidence. That is the
 * same mechanism the borrowed strings use, and it exists for the same reason: a
 * field left over from a previous outcome points at valid memory and says
 * something false, which is the failure a bridge diagnostic can least afford.
 */
#define TFT_BRIDGE_EVIDENCE_NONE 0
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * The time source itself reported the jump, through
 * [`tft_bridge_note_time_jump`]. No threshold, no window, no corroboration —
 * this is not an inference at all. `clock_evidence_detail` is the
 * [`tft_bridge_jump_kind`].
 */
#define TFT_BRIDGE_EVIDENCE_REPORTED 1
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Two or more distinct publishers' stamp-to-receipt offsets stepped by the same
 * amount inside one correlation window. `clock_evidence_detail` is how many.
 *
 * The fallback rung, for callers with no authoritative signal and for
 * system-clock steps `/clock` never reports. A real clock step moves every
 * publisher by the same amount and independent restarts do not, which is what
 * makes agreement — rather than mere coincidence in time — the evidence.
 */
#define TFT_BRIDGE_EVIDENCE_COMMON_MODE 2
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Build a bridge over the topology described by `config_toml`, and the arena
 * that topology declares.
 *
 * **The config is text, not a path.** A ROS node gets its topology from a
 * parameter, a launch file or a bag sidecar, and every one of those is already
 * a string in the node's hands; taking a path would put file IO — and its
 * errors, and its `String`s — inside the ABI for no gain.
 *
 * This is where §5.8's amendment is enforced: the engine has **no runtime edge
 * declaration** (`docs/decisions/0004`, D4), so everything the bridge will ever
 * write must be in this file. It creates the arena, claims every declared
 * dynamic edge, and refuses to start if any of that fails.
 *
 * The thread that calls this **owns** the bridge; see the module docs.
 *
 * # Errors
 *
 * * [`TFT_ERR_BAD_CONFIG`] — the file does not parse, **declares no edges**,
 *   declares a cycle, or describes a topology the engine will not build. The
 *   message names the line or the frame. An empty config parses fine and
 *   describes a tree with no edges; it is refused because a bridge built from
 *   one can only ever answer [`TFT_BRIDGE_UNDECLARED`], which is a switch that
 *   drops 100 % of the traffic with nothing failing at startup.
 * * [`TFT_ERR_TIME_DOMAIN`] — a declared dynamic edge's domain is not
 *   `opts->domain` (§5.5, NORMATIVE, and at startup by design).
 * * [`TFT_ERR_ALREADY_CLAIMED`](crate::TFT_ERR_ALREADY_CLAIMED) and the rest of
 *   the claim family — another participant holds a declared edge.
 *
 * # Safety
 *
 * `config_toml` must be NUL-terminated UTF-8. `opts` must be NULL or point to a
 * `tft_bridge_options` whose `struct_size` is set. `out` must be NULL or point
 * to a writable `*mut tft_bridge`.
 */
tft_status tft_bridge_create(const char *config_toml,
                             const tft_bridge_options *opts,
                             tft_bridge **out);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * A [`tft_tree`] handle onto the arena this bridge writes, for reading.
 *
 * The bridge builds the arena, so without this nothing could read what it
 * ingests. The returned handle is **independently owned**: it shares the
 * refcount, so freeing it does not disturb the bridge and freeing the bridge
 * does not dangle it. Free it with
 * [`tft_tree_free`](crate::tft_tree_free) exactly once.
 *
 * The handle is `Send + Sync` — unlike the bridge itself — so the node's reader
 * threads may use it while the executor thread ingests. That is the whole point
 * of Phase 1's single-writer-many-reader design, and it is why this returns a
 * handle rather than a pointer into the bridge.
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `out` must be
 * NULL or point to a writable `*mut tft_tree`.
 */
tft_status tft_bridge_tree(tft_bridge *b, tft_tree **out);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Release the bridge, its claims and its arena reference. Freeing NULL is a
 * no-op.
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
 * `gid` is the publisher's `rmw_message_info_t::publisher_gid`, 16 bytes, or
 * NULL. It is looked up in the cache [`tft_bridge_attribute`] fills. **A GID
 * that resolves to nothing is not an error** — §5.3 is explicit that
 * attribution degrades: an unmatched GID makes the publisher
 * `<unknown publisher>` and a missing one `<unattributed>`, and the bridge
 * keeps running either way.
 *
 * # The return value answers a different question from the outcome
 *
 * The status says whether the *call* was well-formed: a NULL handle, a
 * `struct_size` from another build, a name that is not UTF-8. **Everything that
 * happened to the sample is in `*out`**, including rejection — a bridge that
 * returned a failing status for a dropped duplicate would train its caller to
 * ignore statuses. `*out` is filled before any of this can fail, so a caller
 * that ignores the status reads a well-formed "nothing happened" rather than
 * stack garbage.
 *
 * # Two orderings that are not arbitrary
 *
 * **The pose is validated before the pipeline runs.** A NaN or a non-unit
 * quaternion is refused without the sample reaching §5.4 — otherwise a
 * publisher whose first message is garbage takes ownership of the edge under
 * `FirstWriterWins` and the *correct* publisher is locked out of it for the
 * life of the arena. It also keeps the clock's high-water mark clean.
 *
 * **A halted bridge refuses everything.** See [`tft_bridge`]'s `halted` field:
 * §5.5 says the bridge stops, and the ABI cannot stop the caller's process, so
 * this is what stopping means here.
 *
 * # `TFT_BRIDGE_RECREATE` is a report, not an action
 *
 * §5.5's `recreate` builds a fresh arena. This ABI will not: every
 * [`tft_plan`](crate::tft_plan) the node compiled, and every `tft_tree` handle
 * it took from [`tft_bridge_tree`], points into the *current* arena, and
 * swapping it underneath them would turn a bag loop into a fleet of dangling
 * plans. The caller tears the bridge down, rebuilds it, and re-plans — which is
 * the only sequence that is correct, so it is the only one offered.
 *
 * # An older caller's sample still works
 *
 * §3.6 promises fields can be appended to a `struct_size`-versioned struct
 * *"without a major bump"*, and until `tft_bridge_sample::received_steady_nanos` was
 * appended nothing here implemented it: the check was an exact equality, so a
 * caller holding a `libtf_tree_c.a` newer than its header got
 * [`TFT_ERR_BAD_STRUCT_SIZE`] on **every** offer — a total outage in precisely the case §3.6 was written for, and
 * exactly the shape §4.4's prebuilt-library path makes reachable.
 *
 * So a `struct_size` naming the pre-`received_steady_nanos` layout is accepted and
 * read as the prefix it is. A *larger* size is still refused: that is a newer
 * caller against an older library, where the library cannot know what the extra
 * bytes mean, and [`tft_check_abi`](crate::tft_check_abi)'s minor rule already
 * covers it.
 *
 * **The missing field is filled from this library's own steady clock**, not
 * left at `0`. `0` would be honest for a caller that has one and chose not to
 * supply it, but a caller that predates the field cannot have chosen anything,
 * and a monotonic reading taken microseconds after the message arrived is a
 * good measurement of when it arrived — inside the 100 ms threshold and the 1 s
 * correlation window by four orders of magnitude. The cost is that the reading
 * is per *transform* rather than per message, so a 20-transform `TFMessage`
 * spreads a publisher's offset by however long those 20 calls take; that is
 * microseconds, and the baseline smooths it away. The alternative —
 * substituting `stamp_nanos` — is the one thing that must never happen, because
 * it re-enables inference over the signal under suspicion for exactly the
 * callers who cannot see the fix.
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `s` must
 * point to a `tft_bridge_sample` with `struct_size` set and at least that many
 * readable bytes, and both frame pointers NUL-terminated. `gid` must be NULL or
 * point to 16 readable bytes. `out` must point to a writable
 * `tft_bridge_outcome` with `struct_size` set.
 */
tft_status tft_bridge_offer(tft_bridge *b,
                            tft_bridge_topic topic,
                            const tft_bridge_sample *s,
                            const uint8_t *gid,
                            tft_bridge_outcome *out);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Record that `gid` belongs to `node_name` — §5.3's cache, filled from the
 * node's graph-change handler.
 *
 * This is the whole of §5.3 that is not `rclcpp`: matching
 * `rmw_message_info_t::publisher_gid` against
 * `get_publishers_info_by_topic()`'s `endpoint_gid()`. The ROS half walks the
 * graph; this side remembers, and [`tft_bridge_offer`] resolves.
 *
 * Calling it again for a known GID **replaces** the name: a node that restarts
 * keeps its GID only if the middleware says so, and the graph is the authority
 * on who is publishing now.
 *
 * An all-zero `gid` is refused with [`TFT_ERR_BAD_ENUM`](crate::TFT_ERR_BAD_ENUM):
 * that pattern is what an RMW leaves when it has no GID to report, so caching a
 * name under it would attribute every unattributed sample to one node.
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `gid` must
 * point to 16 readable bytes and `node_name` be NUL-terminated UTF-8.
 */
tft_status tft_bridge_attribute(tft_bridge *b, const uint8_t *gid, const char *node_name);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Read row `index` of §5.6's remap table, or report that there is no such row.
 *
 * §5.6 is normative and the sentence is short: *"Apply `tf_prefix` remapping if
 * configured, and log the resulting mapping table at startup. **A silent remap
 * is worse than no remap.**"* Without this a C caller has no way to obey it —
 * the table lives in the pipeline's `NameNormalizer`, in Rust, and every name
 * it holds is a `String`.
 *
 * **The table is complete before the first message.** §5.8's amendment made the
 * config the sole source of declared edges, so `tft_bridge_create` puts every
 * declared frame through the same normalizer the wire will use; a row that
 * appears later can only be a frame the config never declared. Walk it right
 * after create:
 *
 * ```c
 * tft_bridge_remap r = { .struct_size = sizeof r };
 * for (uint32_t i = 0; tft_bridge_get_remap(b, i, &r) == TFT_OK; i++)
 *     RCLCPP_INFO(log, "tf_tree: frame %s is declared as %s", r.from, r.to);
 * ```
 *
 * A bridge with no `tf_prefix` and no ROS 1 names has an empty table and the
 * first call returns [`TFT_ERR_NO_DATA`](crate::TFT_ERR_NO_DATA), which is the
 * loop's termination condition rather than a fault.
 *
 * # Errors
 *
 * * [`TFT_ERR_NO_DATA`](crate::TFT_ERR_NO_DATA) — `index` is past the last row.
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `out` must
 * point to a writable `tft_bridge_remap` whose `struct_size` is set.
 */
tft_status tft_bridge_get_remap(tft_bridge *b, uint32_t index, tft_bridge_remap *out);
#endif

#if defined(TFT_HAVE_BRIDGE)
/**
 * Note that a `TFMessage` arrived, whatever it contained (§5.9).
 *
 * Separate from [`tft_bridge_offer`] because one message carries many
 * transforms, and the ratio between the two counters is what tells an operator
 * whether a publisher is batching or spamming.
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
 * ROS 2 publishes clock jumps. `rcl_clock_add_jump_callback`, surfaced by
 * rclcpp as `Clock::create_jump_callback`, hands a node an `rcl_time_jump_t`
 * the moment `/clock` steps or `use_sim_time` is switched. That is the event
 * itself, observed at its source, with no threshold to tune and nothing to
 * corroborate — so this entry point applies
 * [`TFT_BRIDGE_ON_CLOCK_RESET_HALT`] or [`TFT_BRIDGE_ON_CLOCK_RESET_RECREATE`]
 * directly. The inference the offer path runs is the *fallback*, for callers
 * with no such signal, for system-clock steps `/clock` never reports, and as
 * defence in depth.
 *
 * `delta_nanos` is `rcl_time_jump_t::delta.nanoseconds` — *"the new time minus
 * the last time before the jump"* — so a rewind is **negative**. Pass it
 * through unnegated; `kind` is `rcl_clock_change_t` collapsed onto the three
 * [`tft_bridge_jump_kind`] codes.
 *
 * # It must not be called from the jump callback
 *
 * rclcpp's jump post-callback does **not** run on the bridge's ingest thread:
 * with `NodeOptions::use_clock_thread` at its default of `true` the node's
 * `TimeSource` owns a dedicated `/clock` thread, and a source change can
 * instead arrive on whichever executor spins the node. Every entry point here
 * is thread-affine — a debug build of this library `abort()`s the whole ROS
 * process, a release build returns
 * [`TFT_ERR_WRONG_THREAD`](crate::TFT_ERR_WRONG_THREAD), so the release-only
 * gate this repository runs would show the benign half of that. The callback
 * must therefore only *record* the jump into a slot the ingest thread drains,
 * and call this from there.
 *
 * # It charges no counter
 *
 * A reported jump is not an arriving transform, so it is not in the ledger
 * `tft_bridge_stats` documents: `transforms` does not move and neither does any
 * bucket — including on a bridge that has already stopped, where an offer would
 * have charged `refused_after_halt`. `clock_resets` *is* incremented, because
 * it counts clock events rather than transforms and is not a ledger term.
 *
 * # Errors
 *
 * * [`TFT_ERR_BAD_ENUM`](crate::TFT_ERR_BAD_ENUM) — `kind` is not one of the
 *   three [`tft_bridge_jump_kind`] codes.
 *
 * A bridge that has already stopped is **not** an error: `*out` replays the
 * latched action with [`TFT_BRIDGE_REASON_ALREADY_HALTED`], exactly as
 * [`tft_bridge_offer`] does, because a bag that loops twice reports twice and
 * the second report must not read as a call the caller got wrong.
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `out` must
 * point to a writable `tft_bridge_outcome` with `struct_size` set.
 */
tft_status tft_bridge_note_time_jump(tft_bridge *b,
                                     int64_t delta_nanos,
                                     tft_bridge_jump_kind kind,
                                     tft_bridge_outcome *out);
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
 * Copy §5.9's counters into `out`.
 *
 * **Named `get_stats` and not `stats`** because `cbindgen` emits the struct as
 * `typedef struct { … } tft_bridge_stats;`, and in C a typedef name and a
 * function name share one namespace — `tft_status tft_bridge_stats(…)` next to
 * that typedef does not compile, in any of the four compiler/standard rows
 * `just c-header-check` runs. Rust has no such collision, so nothing here would
 * have caught it.
 *
 * # Safety
 *
 * `b` must be a live handle used from the thread that created it. `out` must
 * point to a writable `tft_bridge_stats` whose `struct_size` is set.
 */
tft_status tft_bridge_get_stats(tft_bridge *b, tft_bridge_stats *out);
#endif

/**
 * Evaluate `plan` at `stamp`, reporting the pose **and its first derivative**.
 *
 * `out_pose` receives `tft_layout_size(layout)` bytes; `out_twist` receives
 * [`TFT_TWIST_BYTES`] as `[ωx ωy ωz vx vy vz]`. Either may be NULL, in which
 * case that half is not written — asking for only the twist is a real request
 * and costs the same as asking for both.
 *
 * [`crate::TFT_LAYOUT_QVEC7_WXYZ_TWIST6`] puts both halves in `out_pose` as
 * one contiguous row of thirteen `f64` — `docs/API.md` §3.3's `(N, 13)` shape.
 * Its tail holds exactly the six numbers `out_twist` would receive, so a
 * caller wanting them together does not pay two buffers for it.
 *
 * **That layout is not exclusive to this function.** `tft_plan_at` and
 * `tft_plan_at_many` accept it too, and both are in the *stable* header — this
 * entry point is the only way to get pose and twist into two *separate*
 * buffers, and the only one that will report a twist for a layout that carries
 * none. If the 13-element row is what you want, the stable pair is where to
 * get it, batched.
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
