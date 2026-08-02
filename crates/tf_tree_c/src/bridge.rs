//! The ingest-bridge seam a ROS 2 node calls — `docs/PHASE4.md` §5, the half
//! that is not `rclcpp`.
//!
//! # Why this is a feature of `tf_tree_c` and not a second staticlib
//!
//! An `rclcpp` node that ingests `/tf` needs **both** halves of this repository:
//! the decisions (`tf_tree_bridge`: §5.4 authority, §5.5 clock, §5.6 names,
//! §5.7 statics, §5.9 counters) *and* the arena writes (`tf_tree`). Two Rust
//! staticlibs would each statically contain their own copy of `tf_tree`, so the
//! node would hold two unrelated `Tree`s and two unrelated thread-local error
//! slots — the bridge would write into an arena the reader half could not see.
//! One library also means one CMake package (`find_package(tf_tree CONFIG)`,
//! proved by `just cmake-check`), one `cbindgen` path and one drift check.
//! `docs/decisions/0007` already sanctions this crate as the foreign-caller
//! `unsafe` boundary, so nothing new is being sanctioned here.
//!
//! It is **default-off** because `tf_tree_bridge` is a dependency a C caller who
//! only reads transforms should not pay for, and because everything in this
//! module is `docs/PHASE4.md` §3.1's *unstable* tier: the ROS-facing shape is
//! exactly the thing a year of dogfooding is expected to argue with.
//!
//! # Where the boundary is drawn
//!
//! **C++ never sees a `String` and never touches the arena.** One hot call,
//! [`tft_bridge_offer`], runs names → declared? → kind → static → authority →
//! clock **and the arena write**, and reports a POD
//! [`tft_bridge_outcome`] whose diagnostic strings are borrowed `const char *`
//! valid until the next call on that handle. Attribution is a separate cold
//! call, [`tft_bridge_attribute`], driven from the node's graph-change handler,
//! so §5.3's GID → node cache is Rust-side and unit-testable.
//!
//! The consequence worth stating: the ROS half becomes subscriptions, QoS,
//! executors and GIDs, and *nothing else*. Every judgment about somebody else's
//! misconfigured robot is on this side of the boundary, under `just test`.
//!
//! # Thread affinity, for the same reason as `tft_publisher`
//!
//! A bridge holds one [`OwnedWriter`] per declared
//! dynamic edge, and those are `Send + !Sync`. §5.9 asks for a dedicated
//! `SingleThreadedExecutor` on its own thread, which is exactly the shape this
//! allows: the thread that called [`tft_bridge_create`] owns the handle, a debug
//! build `abort()`s on use from another, and a release build returns
//! [`TFT_ERR_WRONG_THREAD`](crate::TFT_ERR_WRONG_THREAD).

use core::ffi::c_char;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use tf_tree::OwnedWriter;
use tf_tree_bridge::{
    Action, AuthorityPolicy, ClockEvidence, DropReason, HaltReason, Ingest, JumpKind, OnClockReset,
    Publisher, Sample, SteadyNanos, Topic, TopologyConfig,
};

use crate::error::{guard, set_error};
use crate::publisher::{check_thread_token, thread_token};
use crate::{bad_enum, bad_handle, layout, null_arg, TreeShare};
use crate::{
    tft_status, tft_tree, TFT_ERR_BAD_CONFIG, TFT_ERR_BAD_STRUCT_SIZE, TFT_ERR_TIME_DOMAIN,
    TFT_ERR_UNKNOWN_FRAME, TFT_OK,
};

const MAGIC_BRIDGE: u64 = 0x7446_5F42_5249_4431; // "tF_BRID1"-ish

// ---------------------------------------------------------------------------
// The POD surface
// ---------------------------------------------------------------------------

/// Which topic a sample arrived on. `/tf_static` is latched and its stamp is
/// meaningless (§5.7), which is why the bridge has to be told rather than
/// guessing from the sample.
pub type tft_bridge_topic = i32;
/// `/tf` — dynamic, volatile, `KeepLast(100)` (§5.2).
pub const TFT_BRIDGE_TOPIC_TF: tft_bridge_topic = 0;
/// `/tf_static` — latched, **transient_local**, `KeepLast(100)` (§5.2). A
/// `volatile` subscription here receives nothing from publishers that started
/// earlier, which is the single most common ROS 2 tf integration bug.
pub const TFT_BRIDGE_TOPIC_TF_STATIC: tft_bridge_topic = 1;

/// §5.4's authority policy.
pub type tft_bridge_authority = i32;
/// The first attributed publisher of an edge owns it. **The default.**
pub const TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS: tft_bridge_authority = 0;
/// Reclaim on each new publisher. Documented as chaotic; never the default.
pub const TFT_BRIDGE_AUTHORITY_LAST_WRITER_WINS: tft_bridge_authority = 1;
/// Refuse to start if a conflict is detected within the startup window. For CI.
///
/// **Not "halt on the first conflict"**, which is what this said and what the
/// code did before `docs/decisions/0011`. A conflict inside the window is
/// dropped and counted like `FIRST_WRITER_WINS`, and the bridge halts **once**,
/// at the window's close, reporting everything it found — CI wants every
/// misconfiguration out of one run, not the first one out of four. Outside the
/// window this policy *is* `FIRST_WRITER_WINS` plus counters, so a bridge that
/// has been healthy for an hour is not killed by a late-joining publisher.
pub const TFT_BRIDGE_AUTHORITY_STRICT: tft_bridge_authority = 2;

/// §5.5's response to the clock being judged to have moved.
///
/// **Not only backwards.** Since the authoritative path
/// ([`tft_bridge_note_time_jump`]) and the common-mode path both see a sim
/// fast-forward or a bag seek, this policy applies to a *forward* jump too — a
/// backward-regression watcher structurally could not see one.
pub type tft_bridge_on_clock_reset = i32;
/// Stop and report. **The default.**
pub const TFT_BRIDGE_ON_CLOCK_RESET_HALT: tft_bridge_on_clock_reset = 0;
/// Report [`TFT_BRIDGE_RECREATE`] and let the caller rebuild. See
/// [`tft_bridge_offer`] for why the ABI cannot recreate the arena itself.
pub const TFT_BRIDGE_ON_CLOCK_RESET_RECREATE: tft_bridge_on_clock_reset = 1;

/// What happened to one offered transform.
pub type tft_bridge_action = i32;
/// Written into the arena.
pub const TFT_BRIDGE_APPLIED: tft_bridge_action = 0;
/// A `/tf_static` value matching the declared constant. Nothing to write; the
/// arena already holds it (§5.7 idempotent, §5.8 verification).
pub const TFT_BRIDGE_STATIC_VERIFIED: tft_bridge_action = 1;
/// Dropped. `reason` says why.
pub const TFT_BRIDGE_DROPPED: tft_bridge_action = 2;
/// A transform for an edge the topology config does not declare (§5.8).
/// `parent`, `child` and `first_time` are set.
pub const TFT_BRIDGE_UNDECLARED: tft_bridge_action = 3;
/// A `/tf_static` value that disagrees with the one on file (§5.7).
/// `owner`, `intruder`, `existing` and `offered` are all set — the diagnostic
/// §5.7 requires names both publishers **and both values**.
pub const TFT_BRIDGE_STATIC_CONFLICT: tft_bridge_action = 4;
/// The bridge must stop. `reason` is the authority conflict or the clock reset.
pub const TFT_BRIDGE_HALT: tft_bridge_action = 5;
/// The clock moved under `RECREATE`: the caller must tear this bridge down and
/// build a fresh one. `delta_nanos` says how far, and which way.
pub const TFT_BRIDGE_RECREATE: tft_bridge_action = 6;
/// The pipeline said write and **the arena refused**. `status` carries the
/// engine's status code, which is the one an operator can act on.
pub const TFT_BRIDGE_REJECTED: tft_bridge_action = 7;

/// Why a transform was dropped or the bridge halted.
pub type tft_bridge_reason = i32;
/// Not applicable to this outcome.
pub const TFT_BRIDGE_REASON_NONE: tft_bridge_reason = 0;
/// The frame name was empty or only a slash (§5.6).
pub const TFT_BRIDGE_REASON_BAD_NAME: tft_bridge_reason = 1;
/// Another publisher owns the edge (§5.4). `parent`, `child`, `owner`,
/// `intruder` and `first_time` are **all** set — that is §5.4's diagnostic, and
/// `first_time` is what keeps it to one line per pair of colliding publishers
/// rather than one per message.
pub const TFT_BRIDGE_REASON_NOT_THE_OWNER: tft_bridge_reason = 2;
/// **This edge's** stamp went backwards (§5.5). `delta_nanos` says how far, and
/// is negative.
///
/// One publisher's stamps arriving out of order, at any magnitude: a few
/// milliseconds of interleaving, or a node that restarted and is replaying its
/// own buffer from five seconds ago. The sample is dropped and counted either
/// way, which is the whole disposition — Phase 1's ring would refuse these
/// stamps regardless, so the arena is protected without the bridge stopping.
///
/// **Distance is not evidence about the clock.** A lone regression is never
/// promoted to [`TFT_BRIDGE_REASON_CLOCK_RESET`], however far it goes; that
/// needs a reported jump or corroboration from a second publisher.
pub const TFT_BRIDGE_REASON_NON_MONOTONIC: tft_bridge_reason = 3;
/// The edge is already declared with the other kind (§5.7).
pub const TFT_BRIDGE_REASON_KIND_CHANGE: tft_bridge_reason = 4;
/// `STRICT`, and a conflict was recorded on an edge (§5.4).
///
/// On a [`TFT_BRIDGE_HALT`] this is `STRICT`'s startup window closing with
/// conflicts in it. `detail` carries how many of each kind — authority (§5.4)
/// **and** static-value (§5.7) — because the halt is about a set of edges and
/// this POD has room for one. `owner` and `intruder` are empty there, and so are
/// `parent`/`child`: the window closed on transforms counted long before the one
/// in hand, so there is no edge to name that would not be the wrong one.
pub const TFT_BRIDGE_REASON_AUTHORITY_CONFLICT: tft_bridge_reason = 5;
/// The clock was judged to have moved (§5.5). `delta_nanos` is by how much —
/// **negative for a rewind** — and `detail` names *which rung of §5.5's ladder
/// fired*, because they are not equally strong:
///
/// * *"the time source reported it"* — [`tft_bridge_note_time_jump`], the
///   authoritative path. No threshold, no window, no corroboration. This is a
///   fact, and an operator reading it should look at the bag or the simulator.
/// * *"N publishers stepped together"* — the fallback path, where two or more
///   distinct publishers' stamp-to-receipt offsets moved by the same amount
///   inside one correlation window. This is an inference, well corroborated;
///   its one false-positive mode is two nodes restarting in lockstep, which is
///   what the operator would go and look at.
///
/// **A single publisher regressing is never this.** It is
/// [`TFT_BRIDGE_REASON_NON_MONOTONIC`], dropped and counted, because one node
/// restarting, hiccuping or replaying its own buffer is observationally
/// identical to it and halting a healthy robot for it is an outage caused by the
/// diagnostic rather than by the fault.
///
/// `parent`/`child` name the edge whose sample completed a common-mode step, and
/// are **empty** for a reported jump: that entry point has no transform in hand,
/// so any edge it named would be an innocent one.
pub const TFT_BRIDGE_REASON_CLOCK_RESET: tft_bridge_reason = 6;
/// The pose was not a transform: NaN, infinity, or a quaternion that is not a
/// unit quaternion. Checked **before** the pipeline — see [`tft_bridge_offer`].
pub const TFT_BRIDGE_REASON_BAD_POSE: tft_bridge_reason = 7;
/// The bridge had already halted and this offer was refused without being
/// processed. The halt that caused it reported the actionable reason — and both
/// publishers' names, if it was an authority conflict — on the outcome
/// **before** this one.
pub const TFT_BRIDGE_REASON_ALREADY_HALTED: tft_bridge_reason = 8;

/// One `geometry_msgs/TransformStamped`, in the ABI's terms.
///
/// `pose` is `[qw qx qy qz tx ty tz]` — the canonical order (`docs/PHASE1.md`
/// §3.1), **not** `geometry_msgs`' `x y z w`. The ROS half reorders; that is a
/// four-line conversion in the caller and a conversion this side cannot get
/// wrong on the caller's behalf.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tft_bridge_sample {
    /// `sizeof(tft_bridge_sample)` in the caller's build (§3.6).
    ///
    /// **A size from before `received_steady_nanos` existed is accepted**, and read as
    /// the prefix it is — see [`tft_bridge_offer`]'s *"An older caller's sample
    /// still works"*.
    pub struct_size: u32,
    /// Parent frame, NUL-terminated UTF-8, **exactly as it arrived**. Passing
    /// the raw name is deliberate: §5.6's normalization is what the bridge is
    /// for, and a pre-normalized name would move that judgment into C++.
    pub frame_id: *const c_char,
    /// Child frame, likewise raw.
    pub child_frame_id: *const c_char,
    /// Stamp, nanoseconds, in the bridge's own time domain (§5.5).
    ///
    /// **The publisher's number, in the domain under suspicion.** Nothing §5.5
    /// concludes about the clock is concluded by comparing this against another
    /// publisher's stamp; it is compared against `received_steady_nanos`.
    pub stamp_nanos: i64,
    /// `[qw qx qy qz tx ty tz]`.
    pub pose: [f64; 7],
    /// A reading of a local **steady (monotonic)** clock, in nanoseconds, taken
    /// when the message carrying this transform arrived. `0` for "none".
    ///
    /// # Where a ROS caller gets one
    ///
    /// `rclcpp::Clock(RCL_STEADY_TIME).now().nanoseconds()`, read **once per
    /// `TFMessage`** at subscription-callback entry and copied onto every sample
    /// the message expands into.
    ///
    /// Not `node->get_clock()`: that is `RCL_ROS_TIME`, which under
    /// `use_sim_time` *is* `/clock` — the clock under test. A detector whose
    /// reference is the signal it is judging cannot judge it. `RCL_STEADY_TIME`
    /// is unaffected by `use_sim_time`, which is the entire reason it is the
    /// reference.
    ///
    /// Not once per transform, either: that puts a clock read on a 1 kHz path,
    /// and it turns one measurement of a publisher's offset into twenty
    /// slightly different ones.
    ///
    /// # What it is for, and what `0` costs
    ///
    /// §5.5 measures `stamp_nanos - received_steady_nanos` per publisher. That
    /// difference *is* the publisher's `transform_tolerance` — a localizer
    /// dating `map -> odom` 300 ms into the future has a steady offset of
    /// +300 ms — so it is measured and subtracted rather than mistaken for a
    /// jump. A **step** in it, agreed on by two or more distinct publishers
    /// inside one correlation window, is the fallback evidence that the clock
    /// moved.
    ///
    /// `0` means the caller has no steady clock to offer. The offset layer is
    /// then simply absent for that sample: per-edge monotonicity is still
    /// enforced and non-monotonic samples are still dropped and counted, so the
    /// arena is protected exactly as before, and only the *corroborated* clock
    /// verdict is unavailable. That is the honest degradation, and a safe one,
    /// because a single witness never halts anything.
    ///
    /// **Do not pass `stamp_nanos` here.** It makes the difference identically
    /// zero for every publisher, which re-enables inference over the signal
    /// under suspicion and resurrects the `transform_tolerance` false positive
    /// this field exists to remove.
    ///
    /// The name says *which clock*, and that is not verbosity. The whole bug
    /// class this design removes is two clocks being confused for one, and a
    /// field called `received_steady_nanos` sitting next to `stamp_nanos` would be an
    /// invitation to fill it from whichever one was nearest.
    pub received_steady_nanos: i64,
}

/// `tft_bridge_sample` as ABI **0.1** laid it out, before `received_steady_nanos`.
///
/// Frozen here so [`tft_bridge_offer`] can *compute* the size an older caller
/// will send instead of hardcoding one. A literal `88` is right on exactly the
/// targets somebody checked and silently wrong on any other pointer width or
/// `i64` alignment — and it rots the first time a field before `pose` changes.
///
/// The assertions below are what keep it a description of history rather than a
/// second, drifting definition: every field it declares must still sit at the
/// same offset in the current struct, and its size must be exactly where the
/// appended field begins. Reorder or resize anything ahead of `received_steady_nanos`
/// and this fails to compile, which is the only moment at which the prefix rule
/// could quietly stop being true.
#[repr(C)]
#[derive(Clone, Copy)]
struct tft_bridge_sample_v1 {
    struct_size: u32,
    frame_id: *const c_char,
    child_frame_id: *const c_char,
    stamp_nanos: i64,
    pose: [f64; 7],
}

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(
        offset_of!(tft_bridge_sample_v1, struct_size) == offset_of!(tft_bridge_sample, struct_size)
    );
    assert!(offset_of!(tft_bridge_sample_v1, frame_id) == offset_of!(tft_bridge_sample, frame_id));
    assert!(
        offset_of!(tft_bridge_sample_v1, child_frame_id)
            == offset_of!(tft_bridge_sample, child_frame_id)
    );
    assert!(
        offset_of!(tft_bridge_sample_v1, stamp_nanos) == offset_of!(tft_bridge_sample, stamp_nanos)
    );
    assert!(offset_of!(tft_bridge_sample_v1, pose) == offset_of!(tft_bridge_sample, pose));
    // The appended field begins exactly where the old struct ended, so a v1
    // caller's bytes are a prefix of a current one's with nothing in between.
    assert!(
        size_of::<tft_bridge_sample_v1>() == offset_of!(tft_bridge_sample, received_steady_nanos)
    );
    assert!(size_of::<tft_bridge_sample_v1>() < size_of::<tft_bridge_sample>());
};

/// Which way, and in what sense, the time source said its clock jumped —
/// [`tft_bridge_note_time_jump`].
///
/// Mirrors `rcl_time_jump_t`: `rcl_clock_change_t` distinguishes a change of
/// time *source* from motion within one source, and `rcl_duration_t delta` is
/// *"the new time minus the last time before the jump"*.
pub type tft_bridge_jump_kind = i32;
/// The clock *source* changed: `use_sim_time` was switched at runtime
/// (`RCL_ROS_TIME_ACTIVATED` / `RCL_ROS_TIME_DEACTIVATED`).
///
/// Its own kind rather than a large backward or forward jump because the delta
/// across that boundary compares two different time bases and is not a duration
/// in either of them.
pub const TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED: tft_bridge_jump_kind = 0;
/// Time moved backwards: a bag loop, a sim reset, an NTP step back.
/// `delta_nanos` is negative.
pub const TFT_BRIDGE_JUMP_BACKWARD: tft_bridge_jump_kind = 1;
/// Time moved forwards past the source's reporting threshold: a bag seek, a sim
/// fast-forward, an NTP step. `delta_nanos` is positive.
///
/// **Only the authoritative path can see this cheaply.** A forward jump leaves
/// every edge's stamps perfectly monotone, so nothing in the per-edge machinery
/// is even disturbed by it.
pub const TFT_BRIDGE_JUMP_FORWARD: tft_bridge_jump_kind = 2;

/// What the bridge decided, and everything needed to print a sentence about it.
///
/// **Every `const char *` here is borrowed from the handle and valid only until
/// the next call on that handle.** They are never NULL: a field that does not
/// apply to this outcome is the empty string. That is the same lifetime rule
/// [`tft_last_error`](crate::tft_last_error) already states for `tft_error`, and
/// stating it twice is cheaper than one node logging a dangling pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tft_bridge_outcome {
    /// `sizeof(tft_bridge_outcome)` in the caller's build (§3.6).
    ///
    /// **Exact equality, unlike `tft_bridge_options` and `tft_bridge_sample`.**
    /// This is an `out` parameter: accepting a short one means the callee must
    /// know which fields to skip *writing*, which is a different and larger
    /// design than reading a prefix. Do not "finish the job" by symmetry — see
    /// `read_options`.
    pub struct_size: u32,
    /// One of the `TFT_BRIDGE_*` action codes.
    pub action: tft_bridge_action,
    /// One of the `TFT_BRIDGE_REASON_*` codes, or `TFT_BRIDGE_REASON_NONE`.
    pub reason: tft_bridge_reason,
    /// The engine status, when `action` is [`TFT_BRIDGE_REJECTED`]; otherwise
    /// [`TFT_OK`].
    pub status: tft_status,
    /// `1` the first time this edge produced this outcome, `0` afterwards — the
    /// rate limiter behind §5.6's "warn once" and §5.8's "once per edge". An
    /// undeclared 1 kHz edge otherwise emits a thousand identical lines a
    /// second.
    ///
    /// **Set on every outcome a caller is expected to log, including
    /// [`TFT_BRIDGE_HALT`] and [`TFT_BRIDGE_RECREATE`].** Those two are latched:
    /// the offer that stops the bridge carries `first_time = 1` and every offer
    /// after it replays the same action with `first_time = 0`, because the
    /// bridge answers `HALT` to every later transform forever. A caller that
    /// logged them unconditionally would emit one line per transform for the
    /// life of the process — at 20 edges and 100 Hz, 2000 `FATAL` lines a
    /// second, each taking the logging mutex on the ingest thread and burying
    /// the one actionable line. §5.4 requires the diagnostic be "loud,
    /// **rate-limited**"; this field is the whole of that mechanism.
    pub first_time: u8,
    /// How far time went **backwards**, as a positive magnitude. `0` when it did
    /// not.
    ///
    /// This is a *distance*, and it is unchanged: for
    /// [`TFT_BRIDGE_REASON_NON_MONOTONIC`] it is how far this edge's stamp fell
    /// short of its own last accepted one, which is what a caller printing
    /// *"went backwards by %ld ns"* wants and always wanted.
    ///
    /// **Not the same field as [`tft_bridge_outcome::delta_nanos`], and
    /// deliberately not merged with it.** One is a backwards distance and the
    /// other is a signed displacement; they agree in magnitude on a rewind and
    /// say different things on a jump forwards, where this is `0` and
    /// `delta_nanos` is positive. C has no type that carries the distinction, so
    /// the two names are the only thing preserving it — a later tidy-up that
    /// collapsed them would print *"went backwards by -5000000000 ns"* on
    /// exactly the fault the sentence exists for.
    pub by_nanos: i64,
    /// The parent frame. Normalized (§5.6) for every outcome the pipeline
    /// named an edge in; **as it arrived** for `TFT_BRIDGE_DROPPED`,
    /// `TFT_BRIDGE_HALT` and `TFT_BRIDGE_RECREATE`, whose actions carry only a
    /// reason. The difference is one leading `/` and any `tf_prefix`, so the
    /// pair identifies the same edge either way — and for
    /// [`TFT_BRIDGE_REASON_BAD_NAME`] the raw name is the only useful one.
    ///
    /// **Empty when the outcome is not about an arriving transform**: a
    /// `STRICT` startup-window close, and a jump reported through
    /// [`tft_bridge_note_time_jump`]. Both are judgments about transforms
    /// counted earlier or about no transform at all, so any edge they named
    /// would be an innocent one.
    pub parent: *const c_char,
    /// The child frame, on the same terms as `parent`.
    pub child: *const c_char,
    /// Who owns the edge, for an authority or static conflict.
    pub owner: *const c_char,
    /// Who contradicted them.
    pub intruder: *const c_char,
    /// The value on file, for [`TFT_BRIDGE_STATIC_CONFLICT`].
    pub existing: [f64; 7],
    /// The value just offered, for [`TFT_BRIDGE_STATIC_CONFLICT`].
    pub offered: [f64; 7],
    /// A one-line human-readable description, or `""`.
    pub detail: *const c_char,
    /// How far time moved, and **which way**: new time minus old time, so a
    /// rewind is **negative**. `0` where it does not apply.
    ///
    /// Set for [`TFT_BRIDGE_REASON_CLOCK_RESET`] and [`TFT_BRIDGE_RECREATE`] —
    /// the clock event itself, however it was concluded — and for
    /// [`TFT_BRIDGE_REASON_NON_MONOTONIC`], where it is the negation of
    /// `by_nanos`.
    ///
    /// **Signed, because the clock can now be judged to have moved forwards.**
    /// An authoritative jump report and a common-mode step both see a bag seek
    /// or a sim fast-forward, which no backward-regression watcher could. The
    /// convention is `rcl_time_jump_t::delta`'s — *"the new time minus the last
    /// time before the jump"* — so the number a node reads out of `rcl` and the
    /// number it reads back out of this struct are the same quantity, with no
    /// conversion nobody would remember to write.
    pub delta_nanos: i64,
    /// **Which rung of §5.5's ladder concluded the clock moved** — one of the
    /// `TFT_BRIDGE_EVIDENCE_*` codes, or [`TFT_BRIDGE_EVIDENCE_NONE`].
    ///
    /// This is the first thing an operator woken at 3 a.m. by a stopped bridge
    /// needs, before the edge and before the delta. *"The time source reported a
    /// backward jump"* is a fact: go and look at the bag or the simulator.
    /// *"Three publishers stepped together by about the same amount"* is an
    /// inference, well corroborated but capable of being wrong in a way the
    /// first is not: go and look at those three nodes.
    ///
    /// The pipeline knows which one fired and used to discard it at this
    /// boundary, leaving `detail` — a sentence — as the only carrier. A code is
    /// what a caller can branch on.
    pub clock_evidence: i32,
    /// What the evidence consisted of, read according to `clock_evidence`:
    ///
    /// * [`TFT_BRIDGE_EVIDENCE_REPORTED`] — the [`tft_bridge_jump_kind`] the
    ///   time source reported.
    /// * [`TFT_BRIDGE_EVIDENCE_COMMON_MODE`] — how many distinct publishers
    ///   stepped together and agreed. Always ≥ 2; one witness never concludes
    ///   anything.
    /// * [`TFT_BRIDGE_EVIDENCE_NONE`] — `0`, and meaningless.
    pub clock_evidence_detail: u32,
}

/// Which rung of §5.5's ladder concluded that the clock moved.
pub type tft_bridge_evidence = i32;
/// No clock judgment was made on this outcome, and
/// `clock_evidence_detail` is `0`.
///
/// The value **every** outcome starts at, set by `blank_outcome` before any arm
/// runs, so a caller reading these two fields on an unrelated outcome sees
/// "nothing to report" rather than the last clock event's evidence. That is the
/// same mechanism the borrowed strings use, and it exists for the same reason: a
/// field left over from a previous outcome points at valid memory and says
/// something false, which is the failure a bridge diagnostic can least afford.
pub const TFT_BRIDGE_EVIDENCE_NONE: tft_bridge_evidence = 0;
/// The time source itself reported the jump, through
/// [`tft_bridge_note_time_jump`]. No threshold, no window, no corroboration —
/// this is not an inference at all. `clock_evidence_detail` is the
/// [`tft_bridge_jump_kind`].
pub const TFT_BRIDGE_EVIDENCE_REPORTED: tft_bridge_evidence = 1;
/// Two or more distinct publishers' stamp-to-receipt offsets stepped by the same
/// amount inside one correlation window. `clock_evidence_detail` is how many.
///
/// The fallback rung, for callers with no authoritative signal and for
/// system-clock steps `/clock` never reports. A real clock step moves every
/// publisher by the same amount and independent restarts do not, which is what
/// makes agreement — rather than mere coincidence in time — the evidence.
pub const TFT_BRIDGE_EVIDENCE_COMMON_MODE: tft_bridge_evidence = 2;

/// How the bridge is configured at creation.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tft_bridge_options {
    /// `sizeof(tft_bridge_options)` in the caller's build (§3.6).
    pub struct_size: u32,
    /// One of the `TFT_BRIDGE_AUTHORITY_*` codes.
    pub authority: tft_bridge_authority,
    /// One of the `TFT_BRIDGE_ON_CLOCK_RESET_*` codes.
    pub on_clock_reset: tft_bridge_on_clock_reset,
    /// The time-domain tag the bridge stamps in — `use_sim_time` decides it
    /// (§5.5). Every declared *dynamic* edge must agree, or creation fails with
    /// [`TFT_ERR_TIME_DOMAIN`]: sim and real transforms in one arena is a class
    /// of bug worth making impossible, and §5.5 is NORMATIVE that it fails at
    /// startup rather than at first message. Must fit in a `uint8_t`.
    pub domain: u32,
    /// `tf_prefix` remapping (§5.6), or NULL for none.
    pub tf_prefix: *const c_char,
    /// Rendezvous name for a **shared** arena, or NULL for a private heap arena.
    ///
    /// When non-NULL the bridge publishes its arena under this name, and any
    /// process may attach read-only with
    /// [`tft_tree_open`](crate::tft_tree_open) / `tf_tree::open()`. **NULL is
    /// the default and preserves the previous behaviour exactly**
    /// (`docs/decisions/0015`).
    ///
    /// # This is not `domain`
    ///
    /// [`tft_bridge_options::domain`] is §5.5's *time* domain and has nothing to
    /// do with the rendezvous. The **rendezvous** domain comes from
    /// `$TF_TREE_DOMAIN`, else `$ROS_DOMAIN_ID`, else 0 — the convention two
    /// robots on one host already use, and the reason no name is derived from
    /// `tf_prefix` (`docs/decisions/0019` §3, answers 2 and 3). Two fields
    /// spelled "domain" in one header meaning different things is a
    /// documentation obligation, and this paragraph is it.
    ///
    /// # It can fail, and it never downgrades
    ///
    /// A shared build can fail where a heap build cannot: the name is already
    /// held by a live arena, the runtime directory is unusable, `memfd_create`
    /// is refused. All of those are
    /// [`TFT_ERR_ARENA_UNAVAILABLE`](crate::TFT_ERR_ARENA_UNAVAILABLE) with a
    /// message that distinguishes them, and **none of them falls back to a heap
    /// arena** — a silent downgrade leaves every consumer waiting on a
    /// rendezvous that will never appear, which is the failure mode hardest to
    /// diagnose from the consumer's side.
    ///
    /// A library built **without** `--features shm` carries this field with
    /// nothing behind it and refuses a non-NULL value for the same reason.
    pub arena_name: *const c_char,
}

/// `tft_bridge_options` as ABI **0.4** laid it out, before `arena_name`.
///
/// The same device as [`tft_bridge_sample_v1`] and for the same reason: so
/// [`read_options`] can *compute* the size an older caller sends rather than
/// hardcode one, and so the prefix rule stops being true at a compile error
/// rather than at somebody's robot.
///
/// The `struct_size` prefix rule of `docs/PHASE4.md` §3.6 had, until
/// `docs/decisions/0015`, exactly one implementation — `tft_bridge_sample`'s —
/// while `tft_bridge_create` validated with exact equality and read the whole
/// struct. Appending a field under those terms would have locked every 0.4
/// caller out of the entry point, which is the case §3.6 exists to prevent.
#[repr(C)]
#[derive(Clone, Copy)]
struct tft_bridge_options_v1 {
    struct_size: u32,
    authority: tft_bridge_authority,
    on_clock_reset: tft_bridge_on_clock_reset,
    domain: u32,
    tf_prefix: *const c_char,
}

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(
        offset_of!(tft_bridge_options_v1, struct_size)
            == offset_of!(tft_bridge_options, struct_size)
    );
    assert!(
        offset_of!(tft_bridge_options_v1, authority) == offset_of!(tft_bridge_options, authority)
    );
    assert!(
        offset_of!(tft_bridge_options_v1, on_clock_reset)
            == offset_of!(tft_bridge_options, on_clock_reset)
    );
    assert!(offset_of!(tft_bridge_options_v1, domain) == offset_of!(tft_bridge_options, domain));
    assert!(
        offset_of!(tft_bridge_options_v1, tf_prefix) == offset_of!(tft_bridge_options, tf_prefix)
    );
    // The appended field begins exactly where the old struct ended, so a v1
    // caller's bytes are a prefix of a current one's with nothing in between.
    assert!(size_of::<tft_bridge_options_v1>() == offset_of!(tft_bridge_options, arena_name));
    assert!(size_of::<tft_bridge_options_v1>() < size_of::<tft_bridge_options>());
};

/// Read a caller's [`tft_bridge_options`], accepting the layout that predates
/// `arena_name` as a prefix of the current one.
///
/// `None` means the size belongs to neither build and the caller gets
/// [`TFT_ERR_BAD_STRUCT_SIZE`](crate::TFT_ERR_BAD_STRUCT_SIZE).
///
/// **The bounded copy is the whole safety argument**, exactly as it is for
/// [`read_sample`]: relaxing the old `!=` to a length test *without* narrowing
/// the read would be an out-of-bounds read, in the one crate whose entire
/// `unsafe` budget is argument validation. `copy_nonoverlapping` over `u8` also
/// inherits `read_unaligned`'s tolerance of a misaligned caller pointer, because
/// `u8` has alignment 1.
///
/// Unlike [`read_sample`] there is **no post-copy fixup**: the zero the copy
/// leaves in `arena_name` is NULL, which is already the documented "a private
/// heap arena, as before".
///
/// **Only `tft_bridge_options` gets this treatment.** `tft_bridge_outcome`,
/// `tft_bridge_remap` and `tft_bridge_stats` stay exact-equality and should stay
/// that way: they are `out` parameters, so accepting a short one means the
/// callee must know which fields to skip *writing* — a different and larger
/// design than reading a prefix, and not one to "finish the job" into by
/// symmetry.
///
/// # Safety
///
/// `o` must be non-NULL and point to at least `declared` readable bytes.
unsafe fn read_options(o: *const tft_bridge_options, declared: u32) -> Option<tft_bridge_options> {
    let current = core::mem::size_of::<tft_bridge_options>();
    let v1 = core::mem::size_of::<tft_bridge_options_v1>();
    let declared = declared as usize;
    if declared != current && declared != v1 {
        return None;
    }
    // Every field the copy below may leave untouched needs a defined value, and
    // for the one field that can be left untouched the default *is* the
    // documented meaning: a NULL `arena_name` is a private heap arena.
    let mut opts = tft_bridge_options {
        struct_size: 0,
        authority: TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        on_clock_reset: TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        domain: 0,
        tf_prefix: core::ptr::null(),
        arena_name: core::ptr::null(),
    };
    // SAFETY: `declared` is one of the two validated sizes and both are at most
    // `size_of::<tft_bridge_options>()`, so the destination has room; the caller
    // contracts `declared` readable bytes at `o`; the two regions cannot overlap
    // because `opts` is a fresh local; `u8` imposes no alignment.
    unsafe {
        core::ptr::copy_nonoverlapping(
            o.cast::<u8>(),
            core::ptr::addr_of_mut!(opts).cast::<u8>(),
            declared,
        );
    }
    Some(opts)
}

/// One row of §5.6's remap table: a frame name as it arrives, and the name the
/// arena knows it by.
///
/// Both strings are borrowed from the handle and valid until the next
/// [`tft_bridge_get_remap`] call on it — the same rule as
/// [`tft_bridge_outcome`], and deliberately *not* invalidated by
/// [`tft_bridge_offer`], because the startup loop that reads this table logs as
/// it walks.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tft_bridge_remap {
    /// `sizeof(tft_bridge_remap)` in the caller's build (§3.6). Exact equality,
    /// for the reason [`tft_bridge_outcome::struct_size`] gives.
    pub struct_size: u32,
    /// The name as it appears on `/tf` — and in every launch file and RViz
    /// config on the robot.
    pub from: *const c_char,
    /// The name the arena declares, and the one a consumer must look up.
    pub to: *const c_char,
}

/// §5.9's counters, plus the two the C layer alone can see.
///
/// **The ledger balances**, and that is the point of exposing it rather than a
/// prose summary:
///
/// ```text
/// applied + rejected_by_arena + static_verified
///         + dropped_authority + dropped_non_monotonic + dropped_bad_name
///         + dropped_kind_change + dropped_undeclared + dropped_bad_pose
///         + refused_after_halt
///     == transforms
/// ```
///
/// A mismatch means some path returns without counting, which is how "we are
/// not dropping anything" becomes false with no test failing.
///
/// `refused_after_halt` is a term because `transforms` counts those offers:
/// the first revision of this comment omitted it, so the documented ledger
/// stopped balancing the moment a bridge halted — which is precisely the moment
/// an operator starts reading the counters.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tft_bridge_stats {
    /// `sizeof(tft_bridge_stats)` in the caller's build (§3.6). Exact equality,
    /// for the reason [`tft_bridge_outcome::struct_size`] gives.
    pub struct_size: u32,
    /// `TFMessage`es reported by [`tft_bridge_note_message`].
    pub messages: u64,
    /// Transforms offered, including those refused before the pipeline.
    pub transforms: u64,
    /// Transforms **the arena took**. Not the number the pipeline approved:
    /// `rejected_by_arena` is subtracted, so this field means what its name
    /// says and a caller watching it is watching the arena.
    pub applied: u64,
    /// `/tf_static` transforms that matched the declared constant (§5.7, §5.8).
    pub static_verified: u64,
    /// Dropped because another publisher owns the edge (§5.4).
    pub dropped_authority: u64,
    /// Transforms **the clock rules refused** (§5.5).
    ///
    /// Named for the common case and wider than the name: an edge's stamp going
    /// backwards against its own last accepted one, at any magnitude, *and* the
    /// sample that completed a common-mode step — which may be perfectly
    /// monotone, because the clock can be judged to have jumped forward. There
    /// is one bucket for "refused because time misbehaved" and a second one
    /// would be a `struct_size`-versioned growth of this struct, so the meaning
    /// is stated here rather than left to the name to imply.
    pub dropped_non_monotonic: u64,
    /// Dropped because the frame name was unusable (§5.6).
    pub dropped_bad_name: u64,
    /// Dropped because the edge kind would have changed (§5.7).
    pub dropped_kind_change: u64,
    /// Dropped because the topology config does not declare the edge (§5.8).
    /// **The counter to look at first** when a lookup returns no path.
    pub dropped_undeclared: u64,
    /// Dropped because the pose was not a transform (NaN, or a non-unit
    /// quaternion). `tf2` has no equivalent check and no equivalent counter.
    pub dropped_bad_pose: u64,
    /// The pipeline approved the write and the arena refused it — a revoked
    /// claim, or a writer poisoned by a `fork()`. **Not a stamp the clock guard
    /// missed:** since `docs/decisions/0011` the guard is per edge, so its
    /// high-water mark is that edge's own last accepted stamp and the ring it
    /// feeds cannot disagree with it.
    pub rejected_by_arena: u64,
    /// Offers refused because the bridge had already stopped — after a
    /// [`TFT_BRIDGE_HALT`] *or* a [`TFT_BRIDGE_RECREATE`], both of which latch.
    pub refused_after_halt: u64,
    /// Clock resets concluded (§5.5) — **promotions**, not regressions.
    ///
    /// A single publisher's stamp going backwards is counted in
    /// `dropped_non_monotonic` and nowhere else, however far it went. This
    /// counts the times the clock itself was judged to have moved, by either
    /// rung of §5.5's ladder: a jump the time source reported through
    /// [`tft_bridge_note_time_jump`], or two or more distinct publishers whose
    /// offsets stepped by the same amount inside one correlation window. Under
    /// `HALT` it is therefore 0 or 1 for the life of a bridge.
    pub clock_resets: u64,
    /// Static-transform value conflicts (§5.7).
    pub static_conflicts: u64,
    /// The **deepest** the subscription queue has been, as reported by
    /// [`tft_bridge_note_queue_depth`] — not its depth now. A queue that fills
    /// only between two samples is invisible to polling and is exactly the
    /// condition that drops transforms.
    pub queue_high_water: u32,
    /// The subscription's configured depth, so the high-water mark reads as a
    /// fraction. `100` per §5.2.
    pub queue_capacity: u32,
}

// ---------------------------------------------------------------------------
// The handle
// ---------------------------------------------------------------------------

/// Borrowed NUL-terminated scratch for one outcome's strings.
///
/// One `Vec<u8>` per field, rewritten in place on every offer, so a warm bridge
/// allocates nothing for its diagnostics and the pointers stay valid for exactly
/// as long as the documented contract says: until the next call.
///
/// **Nothing resets these between calls, and nothing needs to.** A field left
/// over from the previous outcome would still point at valid memory — so no
/// sanitizer would complain — and would name the wrong publisher, which is the
/// failure mode a bridge diagnostic can least afford. What prevents it is that
/// [`blank_outcome`] starts every pointer at a *static* empty string, so a
/// pointer into one of these buffers appears in the outcome only where the same
/// arm just wrote it. Clearing them as well would make a forgotten
/// `o.field = ptr(...)` show `""` instead of the wrong name either way, at the
/// cost of five writes per transform on the hot path.
#[derive(Default)]
struct Strings {
    parent: Vec<u8>,
    child: Vec<u8>,
    owner: Vec<u8>,
    intruder: Vec<u8>,
    detail: Vec<u8>,
    /// The row [`tft_bridge_get_remap`] last returned.
    ///
    /// **Its own pair of buffers, not the outcome's.** §5.6's table is read in a
    /// startup loop that logs as it goes, and a caller printing an outcome it is
    /// still holding while walking the remap table would otherwise find the
    /// outcome's strings rewritten underneath it by the very log statement.
    /// Two extra `Vec`s per bridge is a cheaper answer than a lifetime rule with
    /// an exception in it.
    remap_from: Vec<u8>,
    remap_to: Vec<u8>,
}

fn set(v: &mut Vec<u8>, s: &str) {
    v.clear();
    v.extend_from_slice(s.as_bytes());
    v.push(0);
}

fn ptr(v: &[u8]) -> *const c_char {
    v.as_ptr().cast::<c_char>()
}

/// The `""` every outcome field starts at.
///
/// `static`, not a field of [`Strings`], so [`blank_outcome`] needs no handle:
/// `*out` can then be filled **before** the handle is validated, which is what
/// makes [`tft_bridge_offer`]'s promise — that a caller who ignores the status
/// reads a well-formed "nothing happened" rather than its own stack — true for
/// a bad handle too, and not only for a well-formed call that went badly.
static EMPTY: [c_char; 1] = [0];

/// An ingest bridge: the decision pipeline, the arena it writes to, one claim
/// per declared dynamic edge, and §5.3's GID cache.
///
/// `#[repr(C)]` for the same reason as [`tft_tree`]: `check_bridge` validates
/// the magic word through a field projection, and `repr(Rust)` promises nothing
/// about where that field lands.
///
/// **The generated header declares this as an incomplete type.**
#[repr(C)]
pub struct tft_bridge {
    magic: u64,
    /// The token of the thread that called [`tft_bridge_create`].
    owner: u64,
    inner: Box<BridgeInner>,
}

/// Everything behind the handle.
///
/// Boxed away from [`tft_bridge`] so the handle itself is three words and
/// `cbindgen` has exactly one type to be kept away from — the alternative is a
/// growing exclusion list in `xtask/src/headers.rs` naming every Rust type that
/// happens to be a field.
struct BridgeInner {
    ingest: Ingest,
    /// One claim per declared dynamic edge, keyed by the **normalized** child
    /// frame name — a child has exactly one parent, so the child alone
    /// identifies the edge.
    ///
    /// **Keyed on the name and not on the `FrameId`**, because the name is what
    /// the hot path already holds. `Action::Publish` hands back the normalized
    /// child; going from there to a `FrameId` means `Tree::frame`, which on a
    /// *writable* arena is `view().intern(name)` — a blake3 hash of the name
    /// plus a probe of the intern table — run once per sample to index a map
    /// this process owns.
    ///
    /// Measured with `examples/bridge_cost.rs`, 20 dynamic edges, the two
    /// variants alternated over 20 rounds so drift is common-mode: **418.6 ns**
    /// per accepted transform keyed on the `FrameId` against **281.5 ns** keyed
    /// on the name (best round of each; medians 435.6 and 293.7). The
    /// re-derivation was **a third of the whole call**.
    ///
    /// `BTreeMap<String, _>::get(&str)` allocates nothing — the probe borrows
    /// through `String: Borrow<str>` — so there is no per-sample allocation to
    /// trade against, and the keys come from the same `ingest.declared()` the
    /// arena was built from, so there is no second set of names to keep in step
    /// with either.
    writers: BTreeMap<String, OwnedWriter>,
    /// §5.3's GID → publisher cache: **the one home of publisher identity**.
    ///
    /// Populated on first sight of a GID by [`publisher_of`] and enriched with a
    /// node name by [`tft_bridge_attribute`]. Both halves matter — the first is
    /// what makes an unnamed publisher a *distinct* publisher, the second is
    /// what makes a diagnostic readable.
    ///
    /// Holds a whole [`Publisher`] rather than a `String` so the hot path can
    /// hand the pipeline a `&Publisher` without building one — a
    /// `Publisher::Node(name.to_string())` per sample would be an allocation on
    /// every transform, in a function whose entire job is to be cheap.
    gids: BTreeMap<[u8; 16], Publisher>,
    /// The reusable `Sample` handed to [`Ingest::offer`]. Its `String`s are
    /// overwritten in place, so a warm bridge does not allocate to describe a
    /// transform it is about to drop.
    scratch: Sample,
    /// The outcome's borrowed strings.
    strings: Strings,
    /// Latched once the pipeline says stop.
    ///
    /// §5.5: *"the bridge stops and reports"*. A C caller that logs the halt and
    /// keeps offering would push exactly the non-monotonic stamps §5.5 exists to
    /// prevent, one at a time, and under `STRICT` it would keep writing an edge
    /// two nodes are fighting over. Latching is how the ABI enforces "stops"
    /// without exiting somebody else's process. A stopped bridge is freed and
    /// rebuilt; there is deliberately no resume.
    ///
    /// **[`TFT_BRIDGE_RECREATE`] latches too.** It is the `recreate` half of
    /// §5.5's `--on-clock-reset`, and this ABI cannot recreate the arena for the
    /// reasons in [`tft_bridge_offer`]'s docs — so the *only* correct
    /// continuation is that the caller tears this bridge down. Left unlatched,
    /// the pipeline has already **forgotten every edge's high-water mark** — the
    /// `Recreate` path rewinds each guard to "no stamp seen yet", which is what
    /// `docs/decisions/0011` replaced the old single `ClockGuard::accept_reset`
    /// with — so every subsequent offer would be approved and the arena would
    /// refuse it per edge as non-monotonic: a bag loop would turn into a silent,
    /// permanent stall reported one `rejected_by_arena` at a time rather than the
    /// one loud outcome §5.5 asks for.
    stopped: Option<Stopped>,
    dropped_bad_pose: u64,
    rejected_by_arena: u64,
    refused_after_halt: u64,
    /// The handle share this bridge reads and hands out through
    /// [`tft_bridge_tree`].
    ///
    /// **It is no longer what keeps the arena alive for the writers.** Each
    /// [`OwnedWriter`] above carries its own `Arc<Tree>`
    /// (`docs/decisions/0017`), so the arena outlives every claim whatever
    /// order these fields drop in — which is why the "declared last on purpose"
    /// note that used to sit here is gone rather than merely reworded. A field
    /// order that is load-bearing and a field order that is not must not read
    /// the same.
    share: Arc<TreeShare>,
}

/// Why the bridge stopped, replayed on every later offer.
///
/// The *action* is kept, not just the fact of stopping: a caller told
/// `TFT_BRIDGE_RECREATE` once and `TFT_BRIDGE_HALT` forever afterwards would
/// read the second as a different, worse fault than the first.
#[derive(Clone, Copy)]
struct Stopped {
    /// [`TFT_BRIDGE_HALT`] or [`TFT_BRIDGE_RECREATE`].
    action: tft_bridge_action,
    /// How far time went backwards, or `0` for a conflict halt or a forward
    /// jump. The replayed outcome's `by_nanos`.
    by_nanos: i64,
    /// How far time moved and which way. The replayed outcome's `delta_nanos`.
    delta_nanos: i64,
}

/// # Safety
///
/// `b` must be NULL or point to a live handle — see `crate`'s `magic_check!`,
/// whose contract this shares.
#[inline]
unsafe fn check_bridge(b: *const tft_bridge) -> bool {
    if b.is_null() {
        return false;
    }
    // SAFETY: non-null, and the caller contracts eight readable bytes at the
    // magic field's offset. `read_unaligned` for the same reason as elsewhere.
    unsafe { core::ptr::addr_of!((*b).magic).read_unaligned() == MAGIC_BRIDGE }
}

/// Validate the handle and the calling thread in one place, so no entry point
/// can forget the affinity rule.
///
/// # Safety
///
/// `b` must satisfy [`check_bridge`]'s contract.
unsafe fn bridge_of<'a>(b: *mut tft_bridge) -> Result<&'a mut tft_bridge, tft_status> {
    // SAFETY: the caller's contract; validated before any field access.
    if !unsafe { check_bridge(b) } {
        return Err(bad_handle("tft_bridge"));
    }
    // SAFETY: `check_bridge` confirmed the magic word.
    let h = unsafe { &mut *b };
    let rc = check_thread_token(h.owner, "tft_bridge");
    if rc != TFT_OK {
        return Err(rc);
    }
    Ok(h)
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Create the **shared** arena `tft_bridge_options::arena_name` asks for, and
/// publish it under that name.
///
/// # Why `tf_tree::Open` and not `TreeBuilder::build_shared`
///
/// `build_shared(name)` **publishes no rendezvous**: its name is a debug label
/// that shows up in `/proc/<pid>/fd`, and the fd is the capability — segments
/// are not discoverable by name. A second process could never find the arena, so
/// the option would appear to work and deliver nothing. The path that publishes
/// is [`tf_tree::Open::open`]'s `Created` arm, which is `build_shared` **plus**
/// OFD liveness, claim leases, the owner server and ownership
/// (`docs/decisions/0015`, *The ABI*).
///
/// # Why `require_create(true)`
///
/// [`tf_tree::CreatePolicy`] has no "create, or refuse if one is already live"
/// setting: `IfAbsent` silently *joins*, which would have this bridge claiming
/// edges in an arena somebody else sized, and `Always` is `--force-new` and
/// documents itself as never to be taken automatically.
/// `docs/decisions/0019` §3's question 3 settles it — a second bridge on a held
/// name is a rendezvous refusal, and this is where it is refused.
///
/// The rendezvous **domain** is the environment's (`$TF_TREE_DOMAIN`, else
/// `$ROS_DOMAIN_ID`, else 0) and is deliberately not
/// `tft_bridge_options::domain`, which is §5.5's *time* domain.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn open_shared(name: &str, builder: tf_tree::TreeBuilder) -> Result<tf_tree::Tree, tft_status> {
    use tf_tree::{AttachMode, CreatePolicy, Open, OpenError};

    let opened = Open::new().name(name).and_then(|o| {
        o.mode(AttachMode::ReadWrite)
            .create(CreatePolicy::IfAbsent)
            .require_create(true)
            .layout_if_creating(builder)
            .open()
    });
    match opened {
        Ok(tree) => Ok(tree),
        // The one failure an operator will actually hit, and the one a generic
        // rendering would bury: another bridge is already serving this name.
        Err(OpenError::ArenaAlreadyLive) => Err(arena_unavailable(&already_live_message(name))),
        // Everything else — an unusable or network runtime directory, a lock
        // file that will not open, no participant slots, a refused memfd, a
        // name this rendezvous will not take — arrives with its own text, and
        // that text is what separates "the runtime directory is unusable" from
        // the case above.
        Err(e) => Err(arena_unavailable(&generic_failure_message(name, &e))),
    }
}

/// "Somebody else holds this name" — [`open_shared`]'s named arm.
///
/// A function rather than a `format!` in place so
/// [`tests::both_named_messages_survive_the_longest_arena_name`] can measure it.
/// Compiled under `test` as well as under `shm` because that test is the *only*
/// thing that keeps its length honest and it must run in both builds.
///
/// **Kept short deliberately.** `tft_error::set_message` truncates at
/// [`crate::TFT_MESSAGE_LEN`], `{name:?}` of a `MAX_NAME_LEN` name is 66 bytes,
/// and this sentence then has four to spare — so a longer one would lose its own
/// advice at exactly the arena name that made it interesting.
#[cfg(any(test, all(feature = "shm", target_os = "linux")))]
fn already_live_message(name: &str) -> String {
    format!(
        "shared arena {name:?}: another participant already holds this rendezvous \
         name, and a bridge will not join an arena it did not size \
         (docs/decisions/0015). Stop it, or use a different arena_name."
    )
}

/// [`open_shared`]'s catch-all arm: **the condition first, the detail last.**
///
/// The order is the whole content of this function. `arena_name` arrives as an
/// arbitrary-length C string and is only length-checked *by* the call that
/// failed, so the name is the one part of this message that can be thousands of
/// bytes long — and an earlier spelling put it first. A caller that passed a
/// 400-byte name got 255 bytes of its own name back and no statement of what had
/// gone wrong, which is the truncation that costs the most: an operator must
/// always be able to tell *which* failure this was.
///
/// So the fixed clause leads, [`tf_tree::OpenError`]'s unbounded rendering comes
/// second, and the name — the caller's own input, and the part they can
/// reconstruct without help — is what the buffer eats into.
#[cfg(any(test, all(feature = "shm", target_os = "linux")))]
fn generic_failure_message(name: &str, detail: &dyn core::fmt::Display) -> String {
    format!("shared arena could not be created: {detail} (arena_name {name:?})")
}

/// The `bridge`-without-`shm` refusal's text; see [`already_live_message`] for
/// why it is a function and why it is compiled under `test` too.
///
/// **Short for the same reason, with eight bytes of slack** at `MAX_NAME_LEN`:
/// a longer sentence would drop the rebuild command for exactly the operator who
/// needs it.
#[cfg(any(test, not(all(feature = "shm", target_os = "linux"))))]
fn no_shm_message(name: &str) -> String {
    format!(
        "shared arena {name:?}: built without --features shm, so this library has no \
         shared memory behind arena_name. Rebuild with \
         `cargo build -p tf_tree_c --features bridge,shm`, or leave it NULL."
    )
}

/// The `bridge`-without-`shm` build's answer, and it is a **refusal**.
///
/// `bridge` and `shm` are independent cargo features, so this configuration
/// carries `arena_name` in its header with no `tf_tree::Open` behind it.
/// Ignoring the field would be exactly the silent downgrade
/// `docs/decisions/0015` forbids, reached through a *build* rather than a
/// runtime fault — and it is the more likely of the two, because it needs no
/// misconfiguration on the robot at all.
#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn open_shared(name: &str, _builder: tf_tree::TreeBuilder) -> Result<tf_tree::Tree, tft_status> {
    Err(arena_unavailable(&no_shm_message(name)))
}

/// Build a bridge over the topology described by `config_toml`, and the arena
/// that topology declares.
///
/// **The config is text, not a path.** A ROS node gets its topology from a
/// parameter, a launch file or a bag sidecar, and every one of those is already
/// a string in the node's hands; taking a path would put file IO — and its
/// errors, and its `String`s — inside the ABI for no gain.
///
/// This is where §5.8's amendment is enforced: the engine has **no runtime edge
/// declaration** (`docs/decisions/0004`, D4), so everything the bridge will ever
/// write must be in this file. It creates the arena, claims every declared
/// dynamic edge, and refuses to start if any of that fails.
///
/// The thread that calls this **owns** the bridge; see the module docs.
///
/// # An older caller's options still work
///
/// `opts->struct_size` selects the layout, and the layout that predates
/// `arena_name` is accepted and read as the prefix it is — the §3.6 rule
/// [`tft_bridge_offer`] already applies to `tft_bridge_sample`. A `0.4` caller
/// therefore keeps the private heap arena it always had, with no source change
/// and no recompile.
///
/// # It can now block, for up to five seconds
///
/// **Only when `opts->arena_name` is non-NULL.** The shared path goes through
/// `tf_tree::Open`, whose rendezvous waits up to `DEFAULT_OPEN_TIMEOUT` (5 s) for
/// an arena that is held but not yet reachable. That is a real change for §5.8's
/// form 3, where this runs inside a constructor: a node that constructs its
/// bridge on the executor thread will not spin that executor until this returns.
/// A NULL `arena_name` — the default, and every pre-`0.5` caller — is exactly as
/// prompt as before.
///
/// # Errors
///
/// * [`TFT_ERR_BAD_CONFIG`] — the file does not parse, **declares no edges**,
///   declares a cycle, or describes a topology the engine will not build. The
///   message names the line or the frame. An empty config parses fine and
///   describes a tree with no edges; it is refused because a bridge built from
///   one can only ever answer [`TFT_BRIDGE_UNDECLARED`], which is a switch that
///   drops 100 % of the traffic with nothing failing at startup.
/// * [`TFT_ERR_TIME_DOMAIN`] — a declared dynamic edge's domain is not
///   `opts->domain` (§5.5, NORMATIVE, and at startup by design).
/// * [`TFT_ERR_ALREADY_CLAIMED`](crate::TFT_ERR_ALREADY_CLAIMED) and the rest of
///   the claim family — another participant holds a declared edge.
/// * [`TFT_ERR_ARENA_UNAVAILABLE`](crate::TFT_ERR_ARENA_UNAVAILABLE) — a
///   non-NULL `opts->arena_name` could not be served: another bridge already
///   holds the name, the runtime directory is unusable, the segment could not be
///   made — or this library was built without `--features shm`. The message
///   distinguishes them, and **there is no fallback to a heap arena**.
/// * [`TFT_ERR_BAD_STRUCT_SIZE`] —
///   `opts->struct_size` is neither this build's size nor the one layout that
///   precedes it.
///
/// # Safety
///
/// `config_toml` must be NUL-terminated UTF-8. `opts` must be NULL or point to a
/// `tft_bridge_options` whose `struct_size` is set **and which has at least that
/// many readable bytes** — the same contract [`tft_bridge_offer`] states for its
/// sample, and it is what makes the narrowed read sound. `out` must be NULL or
/// point to a writable `*mut tft_bridge`.
#[no_mangle]
pub unsafe extern "C" fn tft_bridge_create(
    config_toml: *const c_char,
    opts: *const tft_bridge_options,
    out: *mut *mut tft_bridge,
) -> tft_status {
    guard(|| {
        if config_toml.is_null() || out.is_null() {
            return null_arg("config_toml/out");
        }
        // A caller who ignores the status must not read an uninitialised
        // pointer out of `*out`.
        // SAFETY: `out` is non-null and the caller contracts it writable.
        unsafe { core::ptr::write(out, core::ptr::null_mut()) };

        // Defaults, so `opts == NULL` is the documented "everything default".
        let (mut authority, mut on_reset, mut domain) =
            (AuthorityPolicy::FirstWriterWins, OnClockReset::Halt, 0u8);
        let mut prefix: Option<&str> = None;
        let mut arena_name: Option<&str> = None;
        if !opts.is_null() {
            // SAFETY: the caller contracts a readable `tft_bridge_options` with
            // `struct_size` initialised; the field is read before anything else.
            let declared = unsafe { core::ptr::addr_of!((*opts).struct_size).read_unaligned() };
            // SAFETY: the caller contracts at least `declared` readable bytes at
            // `opts`, and `read_options` copies no more than that.
            let Some(o) = (unsafe { read_options(opts, declared) }) else {
                return bad_struct_size("tft_bridge_options");
            };
            authority = match o.authority {
                TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS => AuthorityPolicy::FirstWriterWins,
                TFT_BRIDGE_AUTHORITY_LAST_WRITER_WINS => AuthorityPolicy::LastWriterWins,
                TFT_BRIDGE_AUTHORITY_STRICT => AuthorityPolicy::Strict,
                _ => return bad_enum("authority"),
            };
            on_reset = match o.on_clock_reset {
                TFT_BRIDGE_ON_CLOCK_RESET_HALT => OnClockReset::Halt,
                TFT_BRIDGE_ON_CLOCK_RESET_RECREATE => OnClockReset::Recreate,
                _ => return bad_enum("on_clock_reset"),
            };
            let Ok(d) = u8::try_from(o.domain) else {
                return bad_enum("domain");
            };
            domain = d;
            if !o.tf_prefix.is_null() {
                // SAFETY: the caller contracts a NUL-terminated C string.
                let Ok(s) = (unsafe { core::ffi::CStr::from_ptr(o.tf_prefix) }).to_str() else {
                    return bad_config("tf_prefix is not valid UTF-8");
                };
                prefix = Some(s);
            }
            if !o.arena_name.is_null() {
                // SAFETY: the caller contracts a NUL-terminated C string.
                let Ok(s) = (unsafe { core::ffi::CStr::from_ptr(o.arena_name) }).to_str() else {
                    return bad_config("arena_name is not valid UTF-8");
                };
                arena_name = Some(s);
            }
        }

        // SAFETY: the caller contracts a NUL-terminated C string.
        let Ok(text) = (unsafe { core::ffi::CStr::from_ptr(config_toml) }).to_str() else {
            return bad_config("the topology config is not valid UTF-8");
        };
        let config = match TopologyConfig::parse(text) {
            Ok(c) => c,
            // `ConfigError` borrows from `text`, so it is rendered here rather
            // than escaping — that is the trade a `Copy`, allocation-free error
            // type makes, and the message keeps the line number that makes it
            // actionable.
            Err(e) => return bad_config(&format!("topology config: {e}")),
        };
        // **A topology declaring no edges is refused here**, and here is the
        // only place it can be. §5.8's amendment makes the config the sole
        // source of declared edges and the engine has no runtime declaration
        // (`docs/decisions/0004`, D4), so a bridge with zero edges cannot ever
        // apply a transform: it starts clean, reports "ingest bridge up", and
        // answers `TFT_BRIDGE_UNDECLARED` to 100 % of the robot's traffic with
        // nothing failing at startup — the same shape as the `tf_prefix` defect
        // §5.6's clarification records.
        //
        // This is a *policy*, and it belongs at the seam where every other
        // startup refusal (domain, cycle, claim) already lives rather than in
        // one of §5.8's three deployment forms — a form that did not repeat the
        // check would accept `topology_toml = ""` and start clean, and form 3
        // is a library handle a caller constructs directly, with no parameter
        // layer above it to put a check in.
        if config.edges.is_empty() {
            // ASCII only: `tft_error::set_message` substitutes `?` for every
            // non-ASCII byte so truncation cannot split a code point, which
            // turns a `§` into `??` in the one string an operator reads.
            return bad_config(
                "topology config: no edges are declared, so this bridge could never write \
                 anything. Produce a config with `tf_tree topology --discover`; the engine \
                 has no runtime edge declaration (docs/PHASE4.md 5.8, docs/decisions/0004).",
            );
        }
        // §5.5's NORMATIVE startup refusal, before the arena is built: finding
        // out at the first message means finding out after twenty nodes have
        // attached.
        if let Err(e) = config.check_domain(domain) {
            set_error(
                TFT_ERR_TIME_DOMAIN,
                &format!("topology config: {e}"),
                |_| {},
            );
            return TFT_ERR_TIME_DOMAIN;
        }
        // Ask the config before asking the builder: the builder finds the same
        // cycle and names it `FrameId(1)`, an index into an arena that was never
        // constructed and which an operator holding a text file cannot resolve.
        if let Some(child) = config.cycle_child() {
            return bad_config(&format!(
                "topology config: the declared topology has a cycle through frame {child:?}"
            ));
        }
        // **The pipeline is built first, and the arena is built from what it
        // says the topology is.** `Ingest::with` applies §5.6's `tf_prefix` to
        // the declared names as well as to the wire, so with a prefix
        // configured `ingest.declared()` and `config` name different frames.
        // Building the arena from `config` would then produce a tree whose
        // frames no approved sample can name — every transform on the robot
        // reported as an undeclared edge, with the diagnostic blaming the
        // config rather than the prefix. There is one normalized topology in
        // this process and this is it.
        let ingest = Ingest::with(&config, authority, on_reset, prefix);
        let declared = ingest.declared();
        // **The same builder either way**, which is the paragraph above being
        // obeyed on the shared path too: `layout_if_creating` takes
        // `declared.builder()` and never `config`'s, so a `tf_prefix`-rewritten
        // topology sizes the shared arena exactly as it sizes the heap one.
        let tree = match arena_name {
            None => {
                let Ok(tree) = declared.builder().build() else {
                    return bad_config("topology config: the declared topology does not build");
                };
                tree
            }
            Some(name) => match open_shared(name, declared.builder()) {
                Ok(tree) => tree,
                Err(rc) => return rc,
            },
        };

        let share = Arc::new(TreeShare {
            tree: Arc::new(tree),
        });
        let mut writers = BTreeMap::new();
        // Claim every declared dynamic edge **now**, not on first message. D7
        // gives an edge one writer machine-wide, so "somebody else already owns
        // `odom -> base`" is a deployment fault, and a deployment fault should
        // be a refusal to start rather than a drop counter that climbs after the
        // robot is moving.
        for e in &declared.edges {
            if !matches!(e.shape, tf_tree_bridge::EdgeShape::Dynamic { .. }) {
                continue;
            }
            let (Ok(c), Ok(p)) = (share.tree.frame(&e.child), share.tree.frame(&e.parent)) else {
                return bad_config("topology config: a declared frame is not in the built tree");
            };
            let w = match share.tree.claim_owned(c, p) {
                Ok(w) => w,
                Err(err) => {
                    let rc = crate::publisher::map::claim(&err);
                    crate::error::amend_error(|d| {
                        d.frame_a = p.get();
                        d.frame_b = c.get();
                    });
                    return rc;
                }
            };
            writers.insert(e.child.clone(), w);
        }

        let inner = Box::new(BridgeInner {
            ingest,
            writers,
            gids: BTreeMap::new(),
            scratch: Sample::identity("", "", 0),
            strings: Strings::default(),
            stopped: None,
            dropped_bad_pose: 0,
            rejected_by_arena: 0,
            refused_after_halt: 0,
            share,
        });
        let h = Box::new(tft_bridge {
            magic: MAGIC_BRIDGE,
            owner: thread_token(),
            inner,
        });
        // SAFETY: `out` is non-null and the caller contracts it writable.
        unsafe { core::ptr::write(out, Box::into_raw(h)) };
        TFT_OK
    })
}

/// A [`tft_tree`] handle onto the arena this bridge writes, for reading.
///
/// The bridge builds the arena, so without this nothing could read what it
/// ingests. The returned handle is **independently owned**: it shares the
/// refcount, so freeing it does not disturb the bridge and freeing the bridge
/// does not dangle it. Free it with
/// [`tft_tree_free`](crate::tft_tree_free) exactly once.
///
/// The handle is `Send + Sync` — unlike the bridge itself — so the node's reader
/// threads may use it while the executor thread ingests. That is the whole point
/// of Phase 1's single-writer-many-reader design, and it is why this returns a
/// handle rather than a pointer into the bridge.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `out` must be
/// NULL or point to a writable `*mut tft_tree`.
#[no_mangle]
pub unsafe extern "C" fn tft_bridge_tree(
    b: *mut tft_bridge,
    out: *mut *mut tft_tree,
) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        // SAFETY: the caller contracts a live handle.
        let h = match unsafe { bridge_of(b) } {
            Ok(h) => h,
            Err(rc) => return rc,
        };
        let t = crate::tree_handle(Arc::clone(&h.inner.share));
        // SAFETY: `out` is non-null and the caller contracts it writable.
        unsafe { core::ptr::write(out, Box::into_raw(t)) };
        TFT_OK
    })
}

/// Release the bridge, its claims and its arena reference. Freeing NULL is a
/// no-op.
///
/// # Safety
///
/// `b` must be NULL or a handle from [`tft_bridge_create`] not already freed,
/// and must be freed from the thread that created it.
#[no_mangle]
pub unsafe extern "C" fn tft_bridge_free(b: *mut tft_bridge) {
    if b.is_null() {
        return;
    }
    // SAFETY: validated before the box is reconstituted.
    if !unsafe { check_bridge(b) } {
        return;
    }
    // The affinity check applies to `free` for a sharper reason than to
    // `offer`: dropping the writers releases every claim and every OFD lease,
    // and doing that from a thread that does not own them is the corruption
    // §3.2 exists to prevent, not merely a misuse.
    //
    // SAFETY: `check_bridge` confirmed the magic word.
    if check_thread_token(unsafe { (*b).owner }, "tft_bridge") != TFT_OK {
        return;
    }
    // Zero the magic before dropping, so a racing or repeated free sees a dead
    // handle rather than following a freed `Box`.
    // SAFETY: `check_bridge` confirmed this is a live `tft_bridge`.
    unsafe { core::ptr::write(b.cast::<u64>(), 0) };
    // SAFETY: produced by `Box::into_raw` in `tft_bridge_create`.
    drop(unsafe { Box::from_raw(b) });
}

// ---------------------------------------------------------------------------
// The hot call
// ---------------------------------------------------------------------------

/// Offer one transform: run every §5 table, then write the arena.
///
/// `gid` is the publisher's `rmw_message_info_t::publisher_gid`, 16 bytes, or
/// NULL. It is looked up in the cache [`tft_bridge_attribute`] fills. **A GID
/// that resolves to nothing is not an error** — §5.3 is explicit that
/// attribution degrades: an unmatched GID makes the publisher
/// `<unknown publisher>` and a missing one `<unattributed>`, and the bridge
/// keeps running either way.
///
/// # The return value answers a different question from the outcome
///
/// The status says whether the *call* was well-formed: a NULL handle, a
/// `struct_size` from another build, a name that is not UTF-8. **Everything that
/// happened to the sample is in `*out`**, including rejection — a bridge that
/// returned a failing status for a dropped duplicate would train its caller to
/// ignore statuses. `*out` is filled before any of this can fail, so a caller
/// that ignores the status reads a well-formed "nothing happened" rather than
/// stack garbage.
///
/// # Two orderings that are not arbitrary
///
/// **The pose is validated before the pipeline runs.** A NaN or a non-unit
/// quaternion is refused without the sample reaching §5.4 — otherwise a
/// publisher whose first message is garbage takes ownership of the edge under
/// `FirstWriterWins` and the *correct* publisher is locked out of it for the
/// life of the arena. It also keeps the clock's high-water mark clean.
///
/// **A halted bridge refuses everything.** See [`tft_bridge`]'s `halted` field:
/// §5.5 says the bridge stops, and the ABI cannot stop the caller's process, so
/// this is what stopping means here.
///
/// # `TFT_BRIDGE_RECREATE` is a report, not an action
///
/// §5.5's `recreate` builds a fresh arena. This ABI will not: every
/// [`tft_plan`](crate::tft_plan) the node compiled, and every `tft_tree` handle
/// it took from [`tft_bridge_tree`], points into the *current* arena, and
/// swapping it underneath them would turn a bag loop into a fleet of dangling
/// plans. The caller tears the bridge down, rebuilds it, and re-plans — which is
/// the only sequence that is correct, so it is the only one offered.
///
/// # An older caller's sample still works
///
/// §3.6 promises fields can be appended to a `struct_size`-versioned struct
/// *"without a major bump"*, and until `tft_bridge_sample::received_steady_nanos` was
/// appended nothing here implemented it: the check was an exact equality, so a
/// caller holding a `libtf_tree_c.a` newer than its header got
/// [`TFT_ERR_BAD_STRUCT_SIZE`] on **every** offer — a total outage in precisely the case §3.6 was written for, and
/// exactly the shape §4.4's prebuilt-library path makes reachable.
///
/// So a `struct_size` naming the pre-`received_steady_nanos` layout is accepted and
/// read as the prefix it is. A *larger* size is still refused: that is a newer
/// caller against an older library, where the library cannot know what the extra
/// bytes mean, and [`tft_check_abi`](crate::tft_check_abi)'s minor rule already
/// covers it.
///
/// **The missing field is filled from this library's own steady clock**, not
/// left at `0`. `0` would be honest for a caller that has one and chose not to
/// supply it, but a caller that predates the field cannot have chosen anything,
/// and a monotonic reading taken microseconds after the message arrived is a
/// good measurement of when it arrived — inside the 100 ms threshold and the 1 s
/// correlation window by four orders of magnitude. The cost is that the reading
/// is per *transform* rather than per message, so a 20-transform `TFMessage`
/// spreads a publisher's offset by however long those 20 calls take; that is
/// microseconds, and the baseline smooths it away. The alternative —
/// substituting `stamp_nanos` — is the one thing that must never happen, because
/// it re-enables inference over the signal under suspicion for exactly the
/// callers who cannot see the fix.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `s` must
/// point to a `tft_bridge_sample` with `struct_size` set and at least that many
/// readable bytes, and both frame pointers NUL-terminated. `gid` must be NULL or
/// point to 16 readable bytes. `out` must point to a writable
/// `tft_bridge_outcome` with `struct_size` set.
#[no_mangle]
pub unsafe extern "C" fn tft_bridge_offer(
    b: *mut tft_bridge,
    topic: tft_bridge_topic,
    s: *const tft_bridge_sample,
    gid: *const u8,
    out: *mut tft_bridge_outcome,
) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        // SAFETY: `out` is non-null and the caller contracts `struct_size` set.
        let declared = unsafe { core::ptr::addr_of!((*out).struct_size).read_unaligned() };
        if declared as usize != core::mem::size_of::<tft_bridge_outcome>() {
            return bad_struct_size("tft_bridge_outcome");
        }
        // A blank outcome first — **before the handle is validated**, so a
        // caller that ignores the status reads "dropped, no reason" with live
        // empty strings rather than whatever was on its stack, even when the
        // handle is the thing that was wrong. Written through `write` because
        // the caller's struct may be uninitialised apart from `struct_size`.
        //
        // **Yes, `*out` is written twice per successful offer**, and 112 of the
        // struct's ~184 bytes are the two 7-vectors only `STATIC_CONFLICT` ever
        // fills. That is not an oversight: it is the mechanism
        // `a_bad_handle_still_leaves_a_printable_outcome` pins, and the blank
        // has to precede `bridge_of` or the promise it makes is only true for
        // calls that reached a live handle. Narrowing the first write to the
        // fields a caller could misread would trade a rule stated in one line
        // for a rule with an exception list. The cost is a couple of
        // nanoseconds against a call measured in the hundreds.
        //
        // SAFETY: as above; `tft_bridge_outcome` is `Copy` with no padding
        // invariants, so a bitwise write is a complete initialisation.
        let mut o = blank_outcome();
        unsafe { core::ptr::write(out, o) };

        // SAFETY: the caller contracts a live handle.
        let h = match unsafe { bridge_of(b) } {
            Ok(h) => h,
            Err(rc) => return rc,
        };
        let inner = &mut *h.inner;

        if s.is_null() {
            return null_arg("s");
        }
        // SAFETY: the caller contracts a readable sample with `struct_size` set.
        let declared = unsafe { core::ptr::addr_of!((*s).struct_size).read_unaligned() };
        // SAFETY: the caller contracts `struct_size` readable bytes at `s`, and
        // `read_sample` reads no more than the size it validates.
        let Some(sample) = (unsafe { read_sample(s, declared) }) else {
            return bad_struct_size("tft_bridge_sample");
        };
        let topic = match topic {
            TFT_BRIDGE_TOPIC_TF => Topic::Tf,
            TFT_BRIDGE_TOPIC_TF_STATIC => Topic::TfStatic,
            _ => return bad_enum("topic"),
        };
        if sample.frame_id.is_null() || sample.child_frame_id.is_null() {
            return null_arg("frame_id/child_frame_id");
        }
        // SAFETY: the caller contracts both are NUL-terminated C strings.
        let (Ok(parent), Ok(child)) = (unsafe {
            (
                core::ffi::CStr::from_ptr(sample.frame_id).to_str(),
                core::ffi::CStr::from_ptr(sample.child_frame_id).to_str(),
            )
        }) else {
            // An argument fault, not a sample outcome — the same call this ABI
            // already makes in `tft_tree_claim`. ROS frame names come from a
            // `std::string` that is UTF-8 in every implementation that exists;
            // a non-UTF-8 one is a corrupted message, not a misconfigured robot.
            set_error(
                TFT_ERR_UNKNOWN_FRAME,
                "frame name is not valid UTF-8",
                |_| {},
            );
            return TFT_ERR_UNKNOWN_FRAME;
        };

        // A stopped bridge stops. See the doc comment.
        if let Some(st) = inner.stopped {
            inner.refused_after_halt += 1;
            o.action = st.action;
            o.reason = TFT_BRIDGE_REASON_ALREADY_HALTED;
            o.by_nanos = st.by_nanos;
            o.delta_nanos = st.delta_nanos;
            // **The evidence is not replayed**, and stays at
            // `TFT_BRIDGE_EVIDENCE_NONE`. It was reported once, on the outcome
            // that carried `first_time = 1`; a caller logging every replay is
            // the failure `first_time` exists to prevent, and repeating the
            // evidence would make each replay look like a fresh conclusion.
            //
            // The wording follows the *latched* action, not the word "halt": a
            // caller told to recreate and then told it halted would read the
            // second as a different, worse fault than the first.
            set(
                &mut inner.strings.detail,
                if st.action == TFT_BRIDGE_RECREATE {
                    "the clock moved past the reset threshold; free this bridge, \
                     build a new one, and re-plan"
                } else {
                    "the bridge halted; free it and build a new one"
                },
            );
            o.detail = ptr(&inner.strings.detail);
            // SAFETY: as the first write above.
            unsafe { core::ptr::write(out, o) };
            return TFT_OK;
        }

        // The pose, **before** the pipeline. See the doc comment.
        let iso = match layout::from_wxyz_pose(sample.pose) {
            Ok(iso) => iso,
            Err(e) => {
                inner.dropped_bad_pose += 1;
                o.action = TFT_BRIDGE_DROPPED;
                o.reason = TFT_BRIDGE_REASON_BAD_POSE;
                set(&mut inner.strings.parent, parent);
                set(&mut inner.strings.child, child);
                set(&mut inner.strings.detail, layout::read_error_text(e));
                o.parent = ptr(&inner.strings.parent);
                o.child = ptr(&inner.strings.child);
                o.detail = ptr(&inner.strings.detail);
                // SAFETY: as the first write above.
                unsafe { core::ptr::write(out, o) };
                return TFT_OK;
            }
        };

        // Reuse the scratch sample's allocations rather than building a fresh
        // one per transform.
        inner.scratch.frame_id.clear();
        inner.scratch.frame_id.push_str(parent);
        inner.scratch.child_frame_id.clear();
        inner.scratch.child_frame_id.push_str(child);
        inner.scratch.stamp_nanos = sample.stamp_nanos;
        inner.scratch.pose = sample.pose;
        // `read_sample` has already substituted this build's own steady clock
        // for a caller that predates the field; a current caller's `0` means
        // "no receipt clock", and the pipeline skips its offset layer for this
        // sample rather than being fed a fiction.
        inner.scratch.received = SteadyNanos(sample.received_steady_nanos);

        // SAFETY: the caller contracts `gid` is NULL or 16 readable bytes.
        let who = unsafe { publisher_of(&mut inner.gids, gid) };
        let action = inner.ingest.offer(topic, &inner.scratch, who);
        fill(inner, &action, iso, &mut o);
        // SAFETY: as the first write above.
        unsafe { core::ptr::write(out, o) };
        TFT_OK
    })
}

/// Read a caller's `tft_bridge_sample`, accepting the layout that predates
/// `received_steady_nanos` as a prefix of the current one.
///
/// `None` means the size belongs to neither build and the caller gets
/// [`TFT_ERR_BAD_STRUCT_SIZE`](crate::TFT_ERR_BAD_STRUCT_SIZE).
///
/// **The bounded copy is the whole safety argument**, and it is why relaxing the
/// old `!=` to a `<=` would not have been enough on its own: the previous code
/// read the *whole* struct with `read_unaligned`, so accepting a shorter one
/// without narrowing the read is an out-of-bounds read in the one crate whose
/// entire `unsafe` budget is argument validation. `copy_nonoverlapping` over
/// `u8` also inherits `read_unaligned`'s tolerance of a misaligned caller
/// pointer, because `u8` has alignment 1.
///
/// # Safety
///
/// `s` must be non-NULL and point to at least `declared` readable bytes.
unsafe fn read_sample(s: *const tft_bridge_sample, declared: u32) -> Option<tft_bridge_sample> {
    let current = core::mem::size_of::<tft_bridge_sample>();
    let v1 = core::mem::size_of::<tft_bridge_sample_v1>();
    let declared = declared as usize;
    if declared != current && declared != v1 {
        return None;
    }
    // Every field the copy below may leave untouched needs a defined value, and
    // the defaults are the documented "not supplied": a NULL name is rejected by
    // the caller a few lines later, and a `0` receipt time means "no steady
    // clock", which is corrected immediately for the v1 case.
    let mut sample = tft_bridge_sample {
        struct_size: 0,
        frame_id: core::ptr::null(),
        child_frame_id: core::ptr::null(),
        stamp_nanos: 0,
        pose: [0.0; 7],
        received_steady_nanos: 0,
    };
    // SAFETY: `declared` is one of the two validated sizes and both are at most
    // `size_of::<tft_bridge_sample>()`, so the destination has room; the caller
    // contracts `declared` readable bytes at `s`; the two regions cannot overlap
    // because `sample` is a fresh local; `u8` imposes no alignment.
    unsafe {
        core::ptr::copy_nonoverlapping(
            s.cast::<u8>(),
            core::ptr::addr_of_mut!(sample).cast::<u8>(),
            declared,
        );
    }
    if declared == v1 {
        sample.received_steady_nanos = steady_now_nanos();
    }
    Some(sample)
}

/// This library's own steady clock, in nanoseconds, for a caller too old to have
/// one to give.
///
/// `Instant` is Rust's monotonic clock — `CLOCK_MONOTONIC` on Linux, the same
/// source `RCL_STEADY_TIME` reaches — so it satisfies the one property §5.5's
/// detector needs of a reference: it is **independent of the clock under test**
/// and unaffected by `use_sim_time`, by `/clock`, or by anything a publisher
/// does.
///
/// Read only on the legacy path. A current caller supplies its own reading at
/// message granularity, which is strictly better, and putting a clock read on
/// the hot path for everybody in order to serve the callers who cannot ask for
/// one would charge every caller 1 kHz for a measurement most of them already
/// have.
///
/// The epoch is this process's first call. Nothing may be compared across
/// processes and nothing here does — only differences within one publisher's
/// stream are ever taken. The `+ 1` keeps the very first reading off `0`, which
/// the pipeline reads as "no receipt clock at all": a one-nanosecond bias, in
/// exchange for the one value in the range that means something else.
fn steady_now_nanos() -> i64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = *BASE.get_or_init(Instant::now);
    let ns = Instant::now().saturating_duration_since(base).as_nanos();
    i64::try_from(ns).unwrap_or(i64::MAX).saturating_add(1)
}

/// Resolve a GID against the cache, per §5.3's degradation rules.
///
/// # Safety
///
/// `gid` must be NULL or point to 16 readable bytes.
unsafe fn publisher_of(gids: &mut BTreeMap<[u8; 16], Publisher>, gid: *const u8) -> &Publisher {
    /// Returned when the middleware told us nothing. `static` so the borrow
    /// outlives the map's.
    static UNATTRIBUTED: Publisher = Publisher::Unattributed;
    if gid.is_null() {
        return &UNATTRIBUTED;
    }
    // SAFETY: the caller contracts 16 readable bytes.
    let key: [u8; 16] = unsafe { core::ptr::read_unaligned(gid.cast::<[u8; 16]>()) };
    // An all-zero GID is what an RMW that does not report one leaves behind, so
    // it means "nothing was told to us" and not "publisher number zero".
    if key == [0u8; 16] {
        return &UNATTRIBUTED;
    }
    // **First sight populates the cache, so this map is the one home of
    // publisher identity** and a GID is a distinct publisher from the first
    // sample, named or not. Previously an unresolved GID became the unit variant
    // `Publisher::UnknownGid`, which made every unnamed publisher compare equal
    // — §5.4 detection silently off, which `docs/PHASE4.md` §5.3's amendment
    // already named as a blend.
    //
    // The insert is bounded by the number of publishers on `/tf`, not by the
    // message rate: every later sample from the same GID takes the `Occupied`
    // arm, which allocates nothing. `tft_bridge_attribute` then *upgrades* the
    // entry's name in place without touching its identity.
    gids.entry(key).or_insert_with(|| Publisher::from_gid(&key))
}

/// Turn a pipeline [`Action`] into the outcome POD, performing the arena write
/// when there is one.
///
/// Split out of [`tft_bridge_offer`] so the unsafe entry point stays short
/// enough to audit: everything below this line is safe code.
fn fill(inner: &mut BridgeInner, action: &Action, iso: tf_tree::Iso3, o: &mut tft_bridge_outcome) {
    match action {
        Action::Publish {
            parent,
            child,
            stamp_nanos,
            ..
        } => {
            set(&mut inner.strings.parent, parent);
            set(&mut inner.strings.child, child);
            o.parent = ptr(&inner.strings.parent);
            o.child = ptr(&inner.strings.child);
            // The pipeline approved it; now the arena has its own say.
            let rc = write_sample(inner, child, *stamp_nanos, iso);
            if rc == TFT_OK {
                o.action = TFT_BRIDGE_APPLIED;
            } else {
                inner.rejected_by_arena += 1;
                o.action = TFT_BRIDGE_REJECTED;
                o.status = rc;
                // The engine already recorded a message in this thread's
                // `tft_error`; borrow it rather than inventing a second wording
                // that could drift from it.
                set(&mut inner.strings.detail, &crate::error::last_message());
                o.detail = ptr(&inner.strings.detail);
                // **This arm has no test, and it is not for want of trying.**
                // Reaching it needs the arena to refuse a write the pipeline
                // approved, and on a private heap arena that cannot happen:
                //
                // * `PushError::NonMonotonicStamp` is dominated by the clock
                //   guard, and `docs/decisions/0011` made that argument
                //   *simpler* rather than breaking it. `Action::Publish`
                //   requires `stamp >= newest`, and since the guards are per
                //   edge, `newest` is **this edge's** last accepted stamp —
                //   exactly the value its ring compares against, so the two
                //   cannot disagree. (The old single guard dominated the ring
                //   only incidentally, by being the maximum over every edge.)
                //   The `Recreate` path is the only thing that rewinds those
                //   marks, and it latches `stopped` before another offer can be
                //   processed.
                // * `PushError::ClaimRevoked` needs a reaper, and
                //   `PushError::ChildDetached` needs a `fork()`; both are
                //   `--features shm` machinery (`docs/PHASE2.md` §1, A4) that a
                //   bridge over its own arena never engages.
                // * `TFT_ERR_NO_EDGE` from `write_sample` needs `writers` and
                //   `ingest.declared()` to disagree about an edge, and they are
                //   built from the same object in `tft_bridge_create`.
                //
                // Deliberately kept anyway: the third case is one refactor away
                // from being reachable, and it is exactly the failure a
                // deployment would present as "the bridge says applied and the
                // lookups say no data". Breaking that invariant on purpose is
                // how `a_tf_prefix_rewrites_the_declared_topology_and_the_arena
                // _with_it`'s second mutant dies — through this arm, reporting
                // status 7.
            }
        }
        Action::StaticVerified { parent, child } => {
            o.action = TFT_BRIDGE_STATIC_VERIFIED;
            set(&mut inner.strings.parent, parent);
            set(&mut inner.strings.child, child);
            o.parent = ptr(&inner.strings.parent);
            o.child = ptr(&inner.strings.child);
        }
        Action::UndeclaredEdge {
            parent,
            child,
            first_time,
        } => {
            o.action = TFT_BRIDGE_UNDECLARED;
            o.first_time = u8::from(*first_time);
            set(&mut inner.strings.parent, parent);
            set(&mut inner.strings.child, child);
            set(
                &mut inner.strings.detail,
                "the topology config does not declare this edge; the engine has no \
                 runtime edge declaration, so nothing can be written for it",
            );
            o.parent = ptr(&inner.strings.parent);
            o.child = ptr(&inner.strings.child);
            o.detail = ptr(&inner.strings.detail);
        }
        Action::AuthorityConflict {
            parent,
            child,
            owner,
            intruder,
            first_time,
        } => {
            // A `DROPPED`, not an action of its own: the sample really is just
            // dropped, and what §5.4 needs is not a new code but the *fields* —
            // both nodes, the edge, and `first_time` so a 1 kHz intruder is one
            // log line rather than a thousand a second. `TFT_BRIDGE_STATIC_
            // CONFLICT` is separate only because it also has to carry two
            // 7-vectors.
            o.action = TFT_BRIDGE_DROPPED;
            o.reason = TFT_BRIDGE_REASON_NOT_THE_OWNER;
            o.first_time = u8::from(*first_time);
            set(&mut inner.strings.parent, parent);
            set(&mut inner.strings.child, child);
            set(&mut inner.strings.owner, &owner.to_string());
            set(&mut inner.strings.intruder, &intruder.to_string());
            set(
                &mut inner.strings.detail,
                "two publishers are writing one edge; tf2 would have interleaved them silently",
            );
            o.parent = ptr(&inner.strings.parent);
            o.child = ptr(&inner.strings.child);
            o.owner = ptr(&inner.strings.owner);
            o.intruder = ptr(&inner.strings.intruder);
            o.detail = ptr(&inner.strings.detail);
        }
        Action::StaticConflict {
            parent,
            child,
            owner,
            intruder,
            existing,
            offered,
            first_time,
        } => {
            o.action = TFT_BRIDGE_STATIC_CONFLICT;
            o.first_time = u8::from(*first_time);
            o.existing = *existing;
            o.offered = *offered;
            set(&mut inner.strings.parent, parent);
            set(&mut inner.strings.child, child);
            set(&mut inner.strings.owner, &owner.to_string());
            set(&mut inner.strings.intruder, &intruder.to_string());
            set(
                &mut inner.strings.detail,
                "a latched static transform disagrees with the declared constant",
            );
            o.parent = ptr(&inner.strings.parent);
            o.child = ptr(&inner.strings.child);
            o.owner = ptr(&inner.strings.owner);
            o.intruder = ptr(&inner.strings.intruder);
            o.detail = ptr(&inner.strings.detail);
        }
        Action::Drop { reason } => {
            o.action = TFT_BRIDGE_DROPPED;
            o.reason = match reason {
                DropReason::BadName => TFT_BRIDGE_REASON_BAD_NAME,
                DropReason::KindChange => TFT_BRIDGE_REASON_KIND_CHANGE,
                DropReason::NonMonotonic { by_nanos } => {
                    // **Both, and they are not redundant.** `by_nanos` is the
                    // backwards distance a caller prints in "went backwards by
                    // %ld ns"; `delta_nanos` is the signed displacement, so a
                    // caller that reads only the signed field gets this edge's
                    // regression in the same convention as the clock events
                    // beside it. Filling one and leaving the other at 0 would
                    // make the outcome quietly wrong for whichever caller read
                    // the other.
                    o.by_nanos = *by_nanos;
                    o.delta_nanos = -*by_nanos;
                    TFT_BRIDGE_REASON_NON_MONOTONIC
                }
            };
            name_the_edge(inner, o);
        }
        Action::Halt { reason } => {
            o.action = TFT_BRIDGE_HALT;
            // The stop is announced once. This arm runs exactly once per bridge
            // — `inner.stopped` is latched immediately below and every later
            // offer short-circuits to the `Stopped` path, which leaves
            // `first_time` at `blank_outcome`'s 0 — so the flag is definitional
            // here rather than a counter. It is the only thing distinguishing
            // the transition from the replay: without it a caller that logs a
            // halt logs one line per transform for the life of the process,
            // because a halted bridge answers `HALT` to every transform
            // forever.
            o.first_time = 1;
            // **The detail is the match's value, not a write inside it.** The
            // evidence two of these variants carry — which rung of §5.5's ladder
            // fired, and the pair of startup conflict counts — has nowhere else
            // to go: `tft_bridge_outcome` is a `struct_size`-versioned POD and
            // growing it is a break neither `docs/decisions/0011` nor this took.
            // So they ride in `detail`, and an arm that wrote `detail` itself
            // would have had it overwritten by the "the bridge halted" sentence
            // that used to follow this match unconditionally. Returning the
            // string makes that mistake unrepresentable rather than a comment
            // asking the next author not to make it.
            let detail = match reason {
                HaltReason::AuthorityConflict { owner, intruder } => {
                    o.reason = TFT_BRIDGE_REASON_AUTHORITY_CONFLICT;
                    set(&mut inner.strings.owner, &owner.to_string());
                    set(&mut inner.strings.intruder, &intruder.to_string());
                    o.owner = ptr(&inner.strings.owner);
                    o.intruder = ptr(&inner.strings.intruder);
                    name_the_edge(inner, o);
                    "the bridge halted; free it and build a new one".to_string()
                }
                HaltReason::ClockReset {
                    delta_nanos,
                    evidence,
                } => {
                    o.reason = TFT_BRIDGE_REASON_CLOCK_RESET;
                    o.delta_nanos = *delta_nanos;
                    o.by_nanos = backwards_by(*delta_nanos);
                    set_evidence(o, *evidence);
                    // **Named only for the inferred rung**, and the split is the
                    // same argument `StartupConflicts` below makes. A
                    // common-mode step is completed *by the arriving sample*, so
                    // `scratch` holds the edge that completed it and naming it
                    // is the diagnostic. A reported jump arrives through
                    // `tft_bridge_note_time_jump` with no transform in hand at
                    // all, so `scratch` holds whichever edge happened to be last
                    // on the wire — an innocent one, printed as the cause.
                    if matches!(evidence, ClockEvidence::CommonMode { .. }) {
                        name_the_edge(inner, o);
                    }
                    format!(
                        "the clock moved: {}; the bridge halted, free it and build a new one",
                        clock_evidence(*evidence, *delta_nanos)
                    )
                }
                HaltReason::StartupConflicts { authority, statics } => {
                    // **Reported under the authority reason, and that is a
                    // limitation rather than a claim.** §5.4's `Strict` is what
                    // raised this and a conflict is what it found, so the code is
                    // the closest true one — but `statics` counts §5.7 value
                    // disagreements, which that code does not name. A dedicated
                    // `TFT_BRIDGE_REASON_STARTUP_CONFLICTS` is
                    // `docs/decisions/0011`'s implementation step 6 and cannot
                    // land here: an unstable constant has to be added to
                    // `UNSTABLE` in `xtask/src/headers.rs` in the same commit,
                    // because the stable tier's cbindgen config is
                    // exclude-by-complement and an unclassified constant is
                    // emitted into the **frozen** `tf_tree.h` with nothing
                    // failing. Until then both counts are in `detail`, which is
                    // what the `rclcpp` HALT arm prints anyway.
                    o.reason = TFT_BRIDGE_REASON_AUTHORITY_CONFLICT;
                    // **No `name_the_edge` here, deliberately.** The other two
                    // arms are judgments *about the arriving sample*, so the
                    // scratch names are that sample's and naming it is the
                    // diagnostic. This one is not: the window closed on
                    // transforms counted minutes ago and the arriving transform
                    // was never processed, so `scratch` holds whichever edge
                    // happened to be next on the wire. Printing it would name an
                    // innocent edge as the cause of the halt. `parent`/`child`
                    // stay at `blank_outcome`'s `""`, which is the documented
                    // "does not apply to this outcome".
                    format!(
                        "STRICT: the startup window closed with {authority} authority and \
                         {statics} static conflict(s); this deployment is misconfigured and \
                         the bridge will not start"
                    )
                }
            };
            inner.stopped = Some(Stopped {
                action: TFT_BRIDGE_HALT,
                by_nanos: o.by_nanos,
                delta_nanos: o.delta_nanos,
            });
            set(&mut inner.strings.detail, &detail);
            o.detail = ptr(&inner.strings.detail);
        }
        Action::RecreateArena {
            delta_nanos,
            evidence,
        } => {
            o.action = TFT_BRIDGE_RECREATE;
            // Latched on the same terms as `Action::Halt` above.
            o.first_time = 1;
            o.reason = TFT_BRIDGE_REASON_CLOCK_RESET;
            o.delta_nanos = *delta_nanos;
            o.by_nanos = backwards_by(*delta_nanos);
            // **The evidence, on both rungs.**
            //
            // An earlier revision of this arm could only report the
            // authoritative case, because `Action::RecreateArena` carried the
            // delta and nothing else — so an *inferred* recreate came out as
            // `TFT_BRIDGE_EVIDENCE_NONE` and the operator could not tell a
            // rebuild the sim or the bag had announced from one this bridge had
            // decided on. `evidence` was added to the variant in
            // `tf_tree_bridge` for exactly that, and it matters more under
            // `Recreate` than under `Halt`: a halt stops and waits for a human,
            // while a recreate throws the arena away and carries on, so nobody
            // is looking unless the line says which of the two happened.
            set_evidence(o, *evidence);
            // **No edge is named**, unlike the halt above, and the asymmetry is
            // deliberate rather than an omission: this arm is reached from both
            // entry points and the pipeline's `RecreateArena` does not say which
            // — it carries only the delta, because under `Recreate` the response
            // is to throw the whole arena away and no edge is more implicated
            // than any other. Naming `scratch` here would name an innocent edge
            // for every jump reported with no transform in hand.
            inner.stopped = Some(Stopped {
                action: TFT_BRIDGE_RECREATE,
                by_nanos: o.by_nanos,
                delta_nanos: *delta_nanos,
            });
            set(
                &mut inner.strings.detail,
                "the clock moved past the reset threshold; free this bridge, \
                 build a new one, and re-plan",
            );
            o.detail = ptr(&inner.strings.detail);
        }
    }
}

/// How far a signed displacement went **backwards**, as a positive magnitude —
/// `0` if it went forwards.
///
/// The one place the two representations are converted between, so a forward
/// jump cannot end up reported as a backward distance by an arm that reached for
/// `abs()`. `unsigned_abs` then `try_from` rather than `-delta`, because
/// `-i64::MIN` overflows and the deltas here are caller-supplied.
fn backwards_by(delta_nanos: i64) -> i64 {
    if delta_nanos >= 0 {
        return 0;
    }
    i64::try_from(delta_nanos.unsigned_abs()).unwrap_or(i64::MAX)
}

/// Copy the pipeline's evidence into the outcome's two branchable fields.
///
/// One function so the code and its detail can never disagree about which rung
/// fired: [`clock_evidence`] writes the sentence and this writes the machine
/// -readable form of the *same* value, and both take it as an argument rather
/// than deriving it.
fn set_evidence(o: &mut tft_bridge_outcome, evidence: ClockEvidence) {
    match evidence {
        ClockEvidence::Reported { kind } => {
            o.clock_evidence = TFT_BRIDGE_EVIDENCE_REPORTED;
            o.clock_evidence_detail = match kind {
                JumpKind::ClockTypeChanged => TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED,
                JumpKind::Backward => TFT_BRIDGE_JUMP_BACKWARD,
                JumpKind::Forward => TFT_BRIDGE_JUMP_FORWARD,
            }
            .unsigned_abs();
        }
        ClockEvidence::CommonMode { publishers } => {
            o.clock_evidence = TFT_BRIDGE_EVIDENCE_COMMON_MODE;
            o.clock_evidence_detail = publishers;
        }
    }
}

/// The half-sentence naming which rung of §5.5's ladder concluded the clock
/// moved, and by how much.
///
/// Kept out of [`fill`] so the two rungs are described in one place: *"the time
/// source reported it"* and *"three publishers stepped together"* send an
/// operator to different places, and a wording that drifted between the halt and
/// the recreate would be worse than no wording at all.
fn clock_evidence(evidence: ClockEvidence, delta_nanos: i64) -> String {
    let (way, magnitude) = if delta_nanos < 0 {
        ("backwards", delta_nanos.unsigned_abs())
    } else {
        ("forwards", delta_nanos.unsigned_abs())
    };
    match evidence {
        ClockEvidence::Reported { kind } => {
            let what = match kind {
                // The delta across a source change compares two different time
                // bases, so it is not a duration in either and is not printed as
                // one.
                JumpKind::ClockTypeChanged => {
                    return "the time source itself changed (use_sim_time was switched)".to_string()
                }
                JumpKind::Backward => "backwards",
                JumpKind::Forward => "forwards",
            };
            format!("the time source reported a jump {what} of {magnitude} ns")
        }
        ClockEvidence::CommonMode { publishers } => format!(
            "{publishers} publishers' stamps stepped {way} together by about {magnitude} ns"
        ),
    }
}

/// Fill `parent`/`child` from the sample as it arrived, for the outcomes whose
/// [`Action`] does not carry the normalized pair.
///
/// §5.4 requires the authority diagnostic to name *"both nodes **and the
/// edge**"*, and §5.5's non-monotonic drop is useless without knowing which edge
/// stalled — but `Action::Drop` and `Action::Halt` carry only a reason, so
/// without this a C caller gets a code and nothing to print. Asking
/// `tf_tree_bridge` to attach the names instead would put two `String`s on the
/// hot path of every dropped 1 kHz sample; these are already in `scratch`.
///
/// **They are the raw names, not the normalized ones**, and that is why this is
/// separate from the arms that set `o.parent` themselves rather than folded into
/// [`blank_outcome`]. §5.6's normalization strips one leading `/` and applies
/// `tf_prefix`, so the pair still identifies the same edge — and for
/// `TFT_BRIDGE_REASON_BAD_NAME` the raw name is the *only* useful one, because
/// the whole outcome is that it did not normalize.
fn name_the_edge(inner: &mut BridgeInner, o: &mut tft_bridge_outcome) {
    set(&mut inner.strings.parent, &inner.scratch.frame_id);
    set(&mut inner.strings.child, &inner.scratch.child_frame_id);
    o.parent = ptr(&inner.strings.parent);
    o.child = ptr(&inner.strings.child);
}

/// Write one approved sample into the arena.
///
/// **There is no name → `FrameId` step here, and that is the point.** The
/// claims are keyed by the normalized child name (see `BridgeInner::writers`),
/// which is exactly what `Action::Publish` carries, so this is one `BTreeMap`
/// probe over borrowed `str`s and then the ring push. Asking the tree to resolve
/// the name instead put a blake3 hash and an intern probe on every sample, and
/// cost a third of the whole call — see `BridgeInner::writers`.
fn write_sample(
    inner: &mut BridgeInner,
    child: &str,
    stamp: i64,
    iso: tf_tree::Iso3,
) -> tft_status {
    let Some(w) = inner.writers.get(child) else {
        // Cold, and unreachable through the pipeline: `Action::Publish` is only
        // produced for an edge the declared topology holds as dynamic, and the
        // claims were taken from that same `ingest.declared()`. Resolving the
        // id for the diagnostic is therefore free of any hot-path cost, and a
        // caller that somehow gets here should still be told which frame.
        set_error(
            crate::TFT_ERR_NO_EDGE,
            "no claim is held on this edge; it was declared static or not at all",
            |d| d.frame_a = inner.share.tree.frame(child).map_or(0, |c| c.get()),
        );
        return crate::TFT_ERR_NO_EDGE;
    };
    match w.push(stamp, &iso) {
        Ok(()) => TFT_OK,
        Err(e) => crate::publisher::map::push(&e),
    }
}

// ---------------------------------------------------------------------------
// Cold calls
// ---------------------------------------------------------------------------

/// Record that `gid` belongs to `node_name` — §5.3's cache, filled from the
/// node's graph-change handler.
///
/// This is the whole of §5.3 that is not `rclcpp`: matching
/// `rmw_message_info_t::publisher_gid` against
/// `get_publishers_info_by_topic()`'s `endpoint_gid()`. The ROS half walks the
/// graph; this side remembers, and [`tft_bridge_offer`] resolves.
///
/// Calling it again for a known GID **replaces** the name: a node that restarts
/// keeps its GID only if the middleware says so, and the graph is the authority
/// on who is publishing now.
///
/// An all-zero `gid` is refused with [`TFT_ERR_BAD_ENUM`](crate::TFT_ERR_BAD_ENUM):
/// that pattern is what an RMW leaves when it has no GID to report, so caching a
/// name under it would attribute every unattributed sample to one node.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `gid` must
/// point to 16 readable bytes and `node_name` be NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn tft_bridge_attribute(
    b: *mut tft_bridge,
    gid: *const u8,
    node_name: *const c_char,
) -> tft_status {
    guard(|| {
        // SAFETY: the caller contracts a live handle.
        let h = match unsafe { bridge_of(b) } {
            Ok(h) => h,
            Err(rc) => return rc,
        };
        if gid.is_null() || node_name.is_null() {
            return null_arg("gid/node_name");
        }
        // SAFETY: the caller contracts 16 readable bytes.
        let key: [u8; 16] = unsafe { core::ptr::read_unaligned(gid.cast::<[u8; 16]>()) };
        if key == [0u8; 16] {
            return bad_enum("gid is all zero");
        }
        // SAFETY: the caller contracts a NUL-terminated C string.
        let Ok(name) = (unsafe { core::ffi::CStr::from_ptr(node_name) }).to_str() else {
            set_error(
                TFT_ERR_UNKNOWN_FRAME,
                "node name is not valid UTF-8",
                |_| {},
            );
            return TFT_ERR_UNKNOWN_FRAME;
        };
        // **Upgrade the name, never the identity.** A graph walk can rename a
        // GID — `rmw_fastrtps` reports `_NODE_NAME_UNKNOWN_` for an endpoint
        // found before its participant's node info arrives and corrects it
        // later — and an `insert` of a freshly built `Publisher` would be a new
        // identity only if identity were the name. It is not, and this says so
        // in the code rather than relying on that: the entry is mutated.
        h.inner
            .gids
            .entry(key)
            .and_modify(|p| p.set_name(name))
            .or_insert_with(|| Publisher::named(&key, name));
        TFT_OK
    })
}

/// Read row `index` of §5.6's remap table, or report that there is no such row.
///
/// §5.6 is normative and the sentence is short: *"Apply `tf_prefix` remapping if
/// configured, and log the resulting mapping table at startup. **A silent remap
/// is worse than no remap.**"* Without this a C caller has no way to obey it —
/// the table lives in the pipeline's `NameNormalizer`, in Rust, and every name
/// it holds is a `String`.
///
/// **The table is complete before the first message.** §5.8's amendment made the
/// config the sole source of declared edges, so `tft_bridge_create` puts every
/// declared frame through the same normalizer the wire will use; a row that
/// appears later can only be a frame the config never declared. Walk it right
/// after create:
///
/// ```c
/// tft_bridge_remap r = { .struct_size = sizeof r };
/// for (uint32_t i = 0; tft_bridge_get_remap(b, i, &r) == TFT_OK; i++)
///     RCLCPP_INFO(log, "tf_tree: frame %s is declared as %s", r.from, r.to);
/// ```
///
/// A bridge with no `tf_prefix` and no ROS 1 names has an empty table and the
/// first call returns [`TFT_ERR_NO_DATA`](crate::TFT_ERR_NO_DATA), which is the
/// loop's termination condition rather than a fault.
///
/// # Errors
///
/// * [`TFT_ERR_NO_DATA`](crate::TFT_ERR_NO_DATA) — `index` is past the last row.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `out` must
/// point to a writable `tft_bridge_remap` whose `struct_size` is set.
#[no_mangle]
pub unsafe extern "C" fn tft_bridge_get_remap(
    b: *mut tft_bridge,
    index: u32,
    out: *mut tft_bridge_remap,
) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        // SAFETY: `out` is non-null and the caller contracts `struct_size` set.
        let declared = unsafe { core::ptr::addr_of!((*out).struct_size).read_unaligned() };
        if declared as usize != core::mem::size_of::<tft_bridge_remap>() {
            return bad_struct_size("tft_bridge_remap");
        }
        // SAFETY: the caller contracts a live handle.
        let h = match unsafe { bridge_of(b) } {
            Ok(h) => h,
            Err(rc) => return rc,
        };
        let inner = &mut *h.inner;
        let Some((from, to)) = inner.ingest.remaps().get(index as usize) else {
            return crate::TFT_ERR_NO_DATA;
        };
        // Copied into the handle's own buffers rather than handed out directly:
        // a Rust `String` is not NUL-terminated, so there is no pointer into the
        // table that C could print.
        set(&mut inner.strings.remap_from, from);
        set(&mut inner.strings.remap_to, to);
        let row = tft_bridge_remap {
            struct_size: core::mem::size_of::<tft_bridge_remap>() as u32,
            from: ptr(&inner.strings.remap_from),
            to: ptr(&inner.strings.remap_to),
        };
        // SAFETY: `out` is non-null, writable, and `tft_bridge_remap` is `Copy`
        // with no padding invariants, so a bitwise write initialises it fully.
        unsafe { core::ptr::write(out, row) };
        TFT_OK
    })
}

/// Note that a `TFMessage` arrived, whatever it contained (§5.9).
///
/// Separate from [`tft_bridge_offer`] because one message carries many
/// transforms, and the ratio between the two counters is what tells an operator
/// whether a publisher is batching or spamming.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it.
#[no_mangle]
pub unsafe extern "C" fn tft_bridge_note_message(b: *mut tft_bridge) -> tft_status {
    guard(|| {
        // SAFETY: the caller contracts a live handle.
        match unsafe { bridge_of(b) } {
            Ok(h) => {
                h.inner.ingest.note_message();
                TFT_OK
            }
            Err(rc) => rc,
        }
    })
}

/// **The time source itself said its clock jumped** — §5.5's authoritative path.
///
/// ROS 2 publishes clock jumps. `rcl_clock_add_jump_callback`, surfaced by
/// rclcpp as `Clock::create_jump_callback`, hands a node an `rcl_time_jump_t`
/// the moment `/clock` steps or `use_sim_time` is switched. That is the event
/// itself, observed at its source, with no threshold to tune and nothing to
/// corroborate — so this entry point applies
/// [`TFT_BRIDGE_ON_CLOCK_RESET_HALT`] or [`TFT_BRIDGE_ON_CLOCK_RESET_RECREATE`]
/// directly. The inference the offer path runs is the *fallback*, for callers
/// with no such signal, for system-clock steps `/clock` never reports, and as
/// defence in depth.
///
/// `delta_nanos` is `rcl_time_jump_t::delta.nanoseconds` — *"the new time minus
/// the last time before the jump"* — so a rewind is **negative**. Pass it
/// through unnegated; `kind` is `rcl_clock_change_t` collapsed onto the three
/// [`tft_bridge_jump_kind`] codes.
///
/// # It must not be called from the jump callback
///
/// rclcpp's jump post-callback does **not** run on the bridge's ingest thread:
/// with `NodeOptions::use_clock_thread` at its default of `true` the node's
/// `TimeSource` owns a dedicated `/clock` thread, and a source change can
/// instead arrive on whichever executor spins the node. Every entry point here
/// is thread-affine — a debug build of this library `abort()`s the whole ROS
/// process, a release build returns
/// [`TFT_ERR_WRONG_THREAD`](crate::TFT_ERR_WRONG_THREAD), so the release-only
/// gate this repository runs would show the benign half of that. The callback
/// must therefore only *record* the jump into a slot the ingest thread drains,
/// and call this from there.
///
/// # It charges no counter
///
/// A reported jump is not an arriving transform, so it is not in the ledger
/// `tft_bridge_stats` documents: `transforms` does not move and neither does any
/// bucket — including on a bridge that has already stopped, where an offer would
/// have charged `refused_after_halt`. `clock_resets` *is* incremented, because
/// it counts clock events rather than transforms and is not a ledger term.
///
/// # Errors
///
/// * [`TFT_ERR_BAD_ENUM`](crate::TFT_ERR_BAD_ENUM) — `kind` is not one of the
///   three [`tft_bridge_jump_kind`] codes.
///
/// A bridge that has already stopped is **not** an error: `*out` replays the
/// latched action with [`TFT_BRIDGE_REASON_ALREADY_HALTED`], exactly as
/// [`tft_bridge_offer`] does, because a bag that loops twice reports twice and
/// the second report must not read as a call the caller got wrong.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `out` must
/// point to a writable `tft_bridge_outcome` with `struct_size` set.
#[no_mangle]
pub unsafe extern "C" fn tft_bridge_note_time_jump(
    b: *mut tft_bridge,
    delta_nanos: i64,
    kind: tft_bridge_jump_kind,
    out: *mut tft_bridge_outcome,
) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        // SAFETY: `out` is non-null and the caller contracts `struct_size` set.
        let declared = unsafe { core::ptr::addr_of!((*out).struct_size).read_unaligned() };
        if declared as usize != core::mem::size_of::<tft_bridge_outcome>() {
            return bad_struct_size("tft_bridge_outcome");
        }
        // A blank outcome before the handle is validated, for the same reason
        // and with the same promise as `tft_bridge_offer`'s.
        //
        // SAFETY: as above; `tft_bridge_outcome` is `Copy` with no padding
        // invariants, so a bitwise write is a complete initialisation.
        let mut o = blank_outcome();
        unsafe { core::ptr::write(out, o) };

        let kind = match kind {
            TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED => JumpKind::ClockTypeChanged,
            TFT_BRIDGE_JUMP_BACKWARD => JumpKind::Backward,
            TFT_BRIDGE_JUMP_FORWARD => JumpKind::Forward,
            _ => return bad_enum("kind"),
        };
        // SAFETY: the caller contracts a live handle.
        let h = match unsafe { bridge_of(b) } {
            Ok(h) => h,
            Err(rc) => return rc,
        };
        let inner = &mut *h.inner;

        // A stopped bridge stops — and charges nothing, unlike the offer path.
        // `refused_after_halt` is a term in a ledger whose total is
        // `transforms`, and this call is not a transform; counting it there
        // would unbalance the ledger to keep a counter looking busy.
        if let Some(st) = inner.stopped {
            o.action = st.action;
            o.reason = TFT_BRIDGE_REASON_ALREADY_HALTED;
            o.by_nanos = st.by_nanos;
            o.delta_nanos = st.delta_nanos;
            set(
                &mut inner.strings.detail,
                if st.action == TFT_BRIDGE_RECREATE {
                    "the clock moved past the reset threshold; free this bridge, \
                     build a new one, and re-plan"
                } else {
                    "the bridge halted; free it and build a new one"
                },
            );
            o.detail = ptr(&inner.strings.detail);
            // SAFETY: as the first write above.
            unsafe { core::ptr::write(out, o) };
            return TFT_OK;
        }

        let action = inner.ingest.note_time_jump(delta_nanos, kind);
        // **Through `fill`, with the same arena-write function the offer path
        // uses.** `note_time_jump` produces only `Halt` or `RecreateArena`, so
        // the pose is never read — and routing it here anyway is what stops the
        // latch, the `first_time` rate limiter and the halt wording from
        // existing twice and drifting. `Iso3::IDENTITY` is the argument the
        // unreachable arm would ignore.
        fill(inner, &action, tf_tree::Iso3::IDENTITY, &mut o);
        // SAFETY: as the first write above.
        unsafe { core::ptr::write(out, o) };
        TFT_OK
    })
}

/// Report the subscription queue depth (§5.9). The high-water mark is kept.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it.
#[no_mangle]
pub unsafe extern "C" fn tft_bridge_note_queue_depth(b: *mut tft_bridge, depth: u32) -> tft_status {
    guard(|| {
        // SAFETY: the caller contracts a live handle.
        match unsafe { bridge_of(b) } {
            Ok(h) => {
                h.inner.ingest.note_queue_depth(depth);
                TFT_OK
            }
            Err(rc) => rc,
        }
    })
}

/// Copy §5.9's counters into `out`.
///
/// **Named `get_stats` and not `stats`** because `cbindgen` emits the struct as
/// `typedef struct { … } tft_bridge_stats;`, and in C a typedef name and a
/// function name share one namespace — `tft_status tft_bridge_stats(…)` next to
/// that typedef does not compile, in any of the four compiler/standard rows
/// `just c-header-check` runs. Rust has no such collision, so nothing here would
/// have caught it.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `out` must
/// point to a writable `tft_bridge_stats` whose `struct_size` is set.
#[no_mangle]
pub unsafe extern "C" fn tft_bridge_get_stats(
    b: *mut tft_bridge,
    out: *mut tft_bridge_stats,
) -> tft_status {
    guard(|| {
        if out.is_null() {
            return null_arg("out");
        }
        // SAFETY: `out` is non-null and the caller contracts `struct_size` set.
        let declared = unsafe { core::ptr::addr_of!((*out).struct_size).read_unaligned() };
        if declared as usize != core::mem::size_of::<tft_bridge_stats>() {
            return bad_struct_size("tft_bridge_stats");
        }
        // SAFETY: the caller contracts a live handle.
        let h = match unsafe { bridge_of(b) } {
            Ok(h) => h,
            Err(rc) => return rc,
        };
        let inner = &*h.inner;
        let s = inner.ingest.stats();
        let stats = tft_bridge_stats {
            struct_size: core::mem::size_of::<tft_bridge_stats>() as u32,
            messages: s.messages,
            // The pipeline never saw the bad-pose drops (they are refused ahead
            // of it) and never saw the offers refused after a halt, so both are
            // added here — otherwise the ledger this struct documents would be
            // short by exactly the transforms the C layer handled alone.
            transforms: s.transforms + inner.dropped_bad_pose + inner.refused_after_halt,
            // `applied` means "the arena took it". The pipeline's `applied`
            // means "the pipeline approved it", and those differ by exactly the
            // writes the engine refused: `rejected_by_arena` is only ever
            // incremented on the `Action::Publish` arm, which is the same arm
            // that already bumped the pipeline's counter, so the difference
            // cannot go negative. `saturating_sub` anyway — a counter that
            // wrapped to 18 exatransforms would be read as a catastrophe by the
            // one person looking at it during an actual incident.
            applied: s.applied.saturating_sub(inner.rejected_by_arena),
            static_verified: s.static_verified,
            dropped_authority: s.dropped_authority,
            dropped_non_monotonic: s.dropped_non_monotonic,
            dropped_bad_name: s.dropped_bad_name,
            dropped_kind_change: s.dropped_kind_change,
            dropped_undeclared: s.dropped_undeclared,
            dropped_bad_pose: inner.dropped_bad_pose,
            rejected_by_arena: inner.rejected_by_arena,
            refused_after_halt: inner.refused_after_halt,
            clock_resets: s.clock_resets,
            static_conflicts: s.static_conflicts,
            queue_high_water: s.queue_high_water,
            queue_capacity: s.queue_capacity,
        };
        // SAFETY: as above; `tft_bridge_stats` is `Copy` with no padding
        // invariants, so a bitwise write is a complete initialisation.
        unsafe { core::ptr::write(out, stats) };
        TFT_OK
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A well-formed "nothing happened" outcome, with every string pointing at the
/// static empty string.
fn blank_outcome() -> tft_bridge_outcome {
    let empty: *const c_char = EMPTY.as_ptr();
    tft_bridge_outcome {
        struct_size: core::mem::size_of::<tft_bridge_outcome>() as u32,
        action: TFT_BRIDGE_DROPPED,
        reason: TFT_BRIDGE_REASON_NONE,
        status: TFT_OK,
        first_time: 0,
        by_nanos: 0,
        parent: empty,
        child: empty,
        owner: empty,
        intruder: empty,
        existing: [0.0; 7],
        offered: [0.0; 7],
        detail: empty,
        delta_nanos: 0,
        // **The `NONE` that keeps the evidence fields from ever going stale.**
        // Every outcome passes through here before any arm runs, so the two
        // clock-evidence fields are cleared once, in one place, rather than by
        // each of the seven arms remembering to.
        clock_evidence: TFT_BRIDGE_EVIDENCE_NONE,
        clock_evidence_detail: 0,
    }
}

fn bad_struct_size(what: &str) -> tft_status {
    set_error(
        TFT_ERR_BAD_STRUCT_SIZE,
        "a struct_size field names a size this build does not know",
        |_| {},
    );
    let _ = what;
    TFT_ERR_BAD_STRUCT_SIZE
}

fn bad_config(msg: &str) -> tft_status {
    set_error(TFT_ERR_BAD_CONFIG, msg, |_| {});
    TFT_ERR_BAD_CONFIG
}

/// `docs/decisions/0015`'s startup refusal: a shared arena was asked for and
/// could not be had, and there is no fallback to a heap one.
///
/// The message is the whole diagnostic — the status code says "the rendezvous",
/// and only the text says *which* rendezvous fault — so every caller of this
/// passes one specific enough to act on.
fn arena_unavailable(msg: &str) -> tft_status {
    set_error(crate::TFT_ERR_ARENA_UNAVAILABLE, msg, |_| {});
    crate::TFT_ERR_ARENA_UNAVAILABLE
}

#[cfg(test)]
mod tests {
    //! Unit tests for the parts of §5 that are text rather than behaviour.
    //!
    //! They are here rather than in `tests/` because both messages must be
    //! measured in **both** builds: `already_live_message` exists only under
    //! `bridge,shm` and `no_shm_message` only under `bridge`-without-`shm`, so
    //! no single integration target can see both. This module compiles in both,
    //! which is why the two helpers carry `#[cfg(any(test, ...))]`.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use crate::TFT_MESSAGE_LEN;

    /// `tf_tree_ipc::MAX_NAME_LEN`, mirrored.
    ///
    /// `tf_tree_ipc` is deliberately not a dependency of this crate — the C ABI
    /// reaches the rendezvous only through the facade — so the value cannot be
    /// imported. Under `shm` the test below binds this literal back to the real
    /// one through `tf_tree::Open::name`, so a raised limit fails here rather
    /// than silently making the measurement below optimistic.
    const MAX_NAME_LEN: usize = 64;

    /// The longest name a message can be asked to carry: every byte of a
    /// maximum-length name, none of which `{:?}` escapes.
    ///
    /// A name of `"` or `\` bytes would render longer than 66, but the
    /// rendezvous does not accept those — which is what the `shm` half of
    /// [`the_message_budget_is_the_rendezvous_limit`] establishes rather than
    /// assumes.
    fn longest_name() -> String {
        "x".repeat(MAX_NAME_LEN)
    }

    /// What `tft_error::set_message` will actually keep: it truncates at
    /// `TFT_MESSAGE_LEN - 1` and writes the NUL itself.
    fn fits_whole(msg: &str) -> bool {
        crate::error::set_error(crate::TFT_ERR_ARENA_UNAVAILABLE, msg, |_| {});
        crate::error::last_message() == msg
    }

    /// **Both named `arena_unavailable` messages survive the longest arena
    /// name.**
    ///
    /// They fit with four and eight bytes to spare, and nothing pinned that. A
    /// message that truncates at a 64-byte name is a diagnostic that fails
    /// exactly when the name is the interesting part — the `arena_name` an
    /// operator has to change is the last thing in one of them and the rebuild
    /// command is the last thing in the other, so what a longer sentence would
    /// drop is the advice, not the prose.
    ///
    /// The assertion is a **round trip through `set_message`**, not arithmetic
    /// against `TFT_MESSAGE_LEN`: it is the truncation itself that must not
    /// happen, and re-deriving the off-by-one here would be a second chance to
    /// get it wrong.
    ///
    /// The slack is reported on failure so the next person editing either
    /// sentence learns how much room there is rather than only that there is
    /// none.
    ///
    /// **Neither half is `#[cfg]`-ed**, which is the point of the two helpers
    /// carrying `test` in their own `cfg`: each message belongs to one build,
    /// but both are *measurable* in either, so this runs whole under `just
    /// test-rust`'s `bridge` line and `just shm-check`'s `bridge,shm` one alike.
    #[test]
    fn both_named_messages_survive_the_longest_arena_name() {
        let name = longest_name();

        let m = super::already_live_message(&name);
        assert!(
            fits_whole(&m),
            "the 'name is already held' message truncates at MAX_NAME_LEN \
             ({} bytes, budget {}): {m}",
            m.len(),
            TFT_MESSAGE_LEN - 1
        );

        let m = super::no_shm_message(&name);
        assert!(
            fits_whole(&m),
            "the 'built without shm' message truncates at MAX_NAME_LEN \
             ({} bytes, budget {}): {m}",
            m.len(),
            TFT_MESSAGE_LEN - 1
        );
    }

    /// The catch-all arm's ordering: **the condition survives, the name is what
    /// gets eaten.**
    ///
    /// `arena_name` is an arbitrary-length C string and is only length-checked
    /// by the call that failed, so this arm can be handed a name far past
    /// `MAX_NAME_LEN` — that *is* one of the failures it reports. With the name
    /// first, a 400-byte one returned 255 bytes of the caller's own name and no
    /// statement of what went wrong.
    #[test]
    fn the_catch_all_names_the_condition_even_under_an_absurd_arena_name() {
        let name = "z".repeat(4000);
        let msg = super::generic_failure_message(&name, &"longer than 64 bytes");
        crate::error::set_error(crate::TFT_ERR_ARENA_UNAVAILABLE, &msg, |_| {});
        let kept = crate::error::last_message();
        assert!(
            kept.starts_with("shared arena could not be created: longer than 64 bytes"),
            "the fault must survive truncation, not the caller's own input: {kept}"
        );
        assert!(kept.len() < msg.len(), "this case must actually truncate");
    }

    /// The literal above is the rendezvous's, checked against the rendezvous.
    ///
    /// Only under `shm`, because the facade's `Open` is where the limit lives.
    /// Two directions: `MAX_NAME_LEN` bytes is accepted, one more is not — so
    /// neither raising nor lowering the real limit leaves the measurement above
    /// describing a name that cannot occur.
    #[cfg(all(feature = "shm", target_os = "linux"))]
    #[test]
    fn the_message_budget_is_the_rendezvous_limit() {
        assert!(
            tf_tree::Open::new().name(&longest_name()).is_ok(),
            "MAX_NAME_LEN is lower than {MAX_NAME_LEN}; the messages are measured against a \
             name the rendezvous will not accept"
        );
        let over = "x".repeat(MAX_NAME_LEN + 1);
        assert!(
            tf_tree::Open::new().name(&over).is_err(),
            "MAX_NAME_LEN is higher than {MAX_NAME_LEN}; the messages have less slack than \
             both_named_messages_survive_the_longest_arena_name measures"
        );
    }
}
