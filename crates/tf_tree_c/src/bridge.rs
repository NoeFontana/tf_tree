//! The ingest-bridge seam a ROS 2 node calls — `docs/PHASE4.md` §5, the half that is not `rclcpp`.
//!
//! A feature of `tf_tree_c` rather than a second staticlib, so the node holds one `Tree`, one
//! thread-local error slot and one CMake package. Default-off: everything here is `docs/PHASE4.md`
//! §3.1's *unstable* tier. `docs/decisions/0007` sanctions the `unsafe` boundary.
//!
//! C++ never sees a `String` or the arena. [`tft_bridge_offer`] runs names, kind, static,
//! authority, clock and the arena write, and reports a POD [`tft_bridge_outcome`] whose `const char
//! *` fields are borrowed until the next call on that handle. Attribution is the cold call
//! [`tft_bridge_attribute`].
//!
//! # Thread affinity
//!
//! A bridge holds `Send + !Sync` [`OwnedWriter`]s: the thread that called [`tft_bridge_create`]
//! owns the handle; a debug build `abort()`s on use from another, a release build returns
//! [`TFT_ERR_WRONG_THREAD`](crate::TFT_ERR_WRONG_THREAD).

use core::ffi::c_char;
use core::fmt::Write as _;
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

/// Which topic a sample arrived on; the bridge is told because `/tf_static` stamps are meaningless
/// (§5.7).
pub type tft_bridge_topic = i32;
/// `/tf` — dynamic, volatile, `KeepLast(100)` (§5.2).
pub const TFT_BRIDGE_TOPIC_TF: tft_bridge_topic = 0;
/// `/tf_static` — latched, **transient_local**, `KeepLast(100)` (§5.2); a volatile subscription
/// misses earlier publishers.
pub const TFT_BRIDGE_TOPIC_TF_STATIC: tft_bridge_topic = 1;

/// §5.4's authority policy.
pub type tft_bridge_authority = i32;
/// The first attributed publisher of an edge owns it. **The default.**
pub const TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS: tft_bridge_authority = 0;
/// Reclaim on each new publisher. Documented as chaotic; never the default.
pub const TFT_BRIDGE_AUTHORITY_LAST_WRITER_WINS: tft_bridge_authority = 1;
/// Refuse to start if a conflict is detected within the startup window. For CI.
///
/// Conflicts in the window are dropped and counted; the bridge halts once at its close
/// (`docs/decisions/0011`), reporting all of them. Outside the window this is `FIRST_WRITER_WINS`
/// plus counters.
pub const TFT_BRIDGE_AUTHORITY_STRICT: tft_bridge_authority = 2;

/// §5.5's response to the clock being judged to have moved, forwards or backwards.
pub type tft_bridge_on_clock_reset = i32;
/// Stop and report. **The default.**
pub const TFT_BRIDGE_ON_CLOCK_RESET_HALT: tft_bridge_on_clock_reset = 0;
/// Report [`TFT_BRIDGE_RECREATE`] and let the caller rebuild (see [`tft_bridge_offer`]).
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
/// A `/tf_static` value that disagrees with the one on file (§5.7); `owner`, `intruder`, `existing`
/// and `offered` are set.
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
/// Another publisher owns the edge (§5.4). `parent`, `child`, `owner`, `intruder` and `first_time`
/// are set.
pub const TFT_BRIDGE_REASON_NOT_THE_OWNER: tft_bridge_reason = 2;
/// **This edge's** stamp went backwards (§5.5); `delta_nanos` is negative. Dropped and counted; a
/// lone regression is never promoted to [`TFT_BRIDGE_REASON_CLOCK_RESET`].
pub const TFT_BRIDGE_REASON_NON_MONOTONIC: tft_bridge_reason = 3;
/// The edge is already declared with the other kind (§5.7).
pub const TFT_BRIDGE_REASON_KIND_CHANGE: tft_bridge_reason = 4;
/// `STRICT`, and a conflict was recorded on an edge (§5.4). Per-sample: `owner`, `intruder`,
/// `parent` and `child` name it. The window closing is [`TFT_BRIDGE_REASON_STARTUP_CONFLICTS`].
pub const TFT_BRIDGE_REASON_AUTHORITY_CONFLICT: tft_bridge_reason = 5;
/// The clock was judged to have moved (§5.5). `delta_nanos` is by how much (**negative for a
/// rewind**); `detail` names which rung of §5.5's ladder fired:
///
/// * *"the time source reported it"* — [`tft_bridge_note_time_jump`]; a fact.
/// * *"N publishers stepped together"* — the fallback; an inference from two or more publishers'
///   offsets stepping together.
///
/// A single publisher regressing is never this. `parent`/`child` name the edge that completed a
/// common-mode step and are **empty** for a reported jump.
pub const TFT_BRIDGE_REASON_CLOCK_RESET: tft_bridge_reason = 6;
/// The pose was not a transform: NaN, infinity, or a quaternion that is not a
/// unit quaternion. Checked **before** the pipeline — see [`tft_bridge_offer`].
pub const TFT_BRIDGE_REASON_BAD_POSE: tft_bridge_reason = 7;
/// The bridge had already halted; the halt that caused it was reported on an earlier outcome.
pub const TFT_BRIDGE_REASON_ALREADY_HALTED: tft_bridge_reason = 8;
/// `STRICT`'s startup window closed with conflicts recorded in it (§5.4), so the bridge refused to
/// start (`docs/decisions/0011` step 6).
///
/// A judgment about a *set* of edges, authority (§5.4) and static-value (§5.7) alike. `detail`
/// states both counts and enumerates **every** recorded edge with both publishers (§5.4's
/// amendment). `owner`, `intruder`, `parent` and `child` are **empty**.
pub const TFT_BRIDGE_REASON_STARTUP_CONFLICTS: tft_bridge_reason = 9;

/// One `geometry_msgs/TransformStamped`, in the ABI's terms.
///
/// `pose` is `[qw qx qy qz tx ty tz]` (`docs/PHASE1.md` §3.1), **not** `geometry_msgs`' `x y z w`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tft_bridge_sample {
    /// `sizeof(tft_bridge_sample)` in the caller's build (§3.6). A size predating
    /// `received_steady_nanos` is accepted as a prefix; see [`tft_bridge_offer`].
    pub struct_size: u32,
    /// Parent frame, NUL-terminated UTF-8, **exactly as it arrived**: §5.6's normalization is the
    /// bridge's job.
    pub frame_id: *const c_char,
    /// Child frame, likewise raw.
    pub child_frame_id: *const c_char,
    /// Stamp, nanoseconds, in the bridge's own time domain (§5.5): the publisher's number, compared
    /// only against `received_steady_nanos`.
    pub stamp_nanos: i64,
    /// `[qw qx qy qz tx ty tz]`.
    pub pose: [f64; 7],
    /// A reading of a local **steady (monotonic)** clock, in nanoseconds, taken when the message
    /// carrying this transform arrived. `0` for "none".
    ///
    /// A ROS caller reads `rclcpp::Clock(RCL_STEADY_TIME).now().nanoseconds()` **once per
    /// `TFMessage`** at callback entry. Not `node->get_clock()`: under `use_sim_time` that is
    /// `/clock`, the clock under test.
    ///
    /// §5.5 measures `stamp_nanos - received_steady_nanos` per publisher (its
    /// `transform_tolerance`); a step in it agreed on by two or more publishers is the fallback
    /// evidence that the clock moved. `0` drops only that corroborated verdict; per-edge
    /// monotonicity still holds.
    ///
    /// **Do not pass `stamp_nanos` here**: it zeroes the difference and reintroduces the
    /// `transform_tolerance` false positive.
    pub received_steady_nanos: i64,
}

/// `tft_bridge_sample` as ABI **0.1** laid it out, before `received_steady_nanos`.
///
/// Lets [`tft_bridge_offer`] compute the older size; the assertions below fail to compile if the
/// prefix stops being a prefix.
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
    // The appended field begins where the old struct ended.
    assert!(
        size_of::<tft_bridge_sample_v1>() == offset_of!(tft_bridge_sample, received_steady_nanos)
    );
    assert!(size_of::<tft_bridge_sample_v1>() < size_of::<tft_bridge_sample>());
};

/// Which way, and in what sense, the time source said its clock jumped —
/// [`tft_bridge_note_time_jump`]. Mirrors `rcl_time_jump_t`; `delta` is *"the new time minus the
/// last time before the jump"*.
pub type tft_bridge_jump_kind = i32;
/// The clock *source* changed (`use_sim_time` switched at runtime); the delta compares two time
/// bases and is not a duration.
pub const TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED: tft_bridge_jump_kind = 0;
/// Time moved backwards: a bag loop, a sim reset, an NTP step back.
/// `delta_nanos` is negative.
pub const TFT_BRIDGE_JUMP_BACKWARD: tft_bridge_jump_kind = 1;
/// Time moved forwards past the source's threshold: a bag seek, sim fast-forward, NTP step.
/// `delta_nanos` is positive. Only the authoritative path sees this cheaply.
pub const TFT_BRIDGE_JUMP_FORWARD: tft_bridge_jump_kind = 2;

/// What the bridge decided, and everything needed to print a sentence about it.
///
/// Every `const char *` is borrowed from the handle, valid until the next call on it, and never
/// NULL: a field that does not apply is `""`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tft_bridge_outcome {
    /// `sizeof(tft_bridge_outcome)` in the caller's build (§3.6). **Exact equality**, unlike
    /// `tft_bridge_options` and `tft_bridge_sample`: this is an `out` parameter (see
    /// `read_options`).
    pub struct_size: u32,
    /// One of the `TFT_BRIDGE_*` action codes.
    pub action: tft_bridge_action,
    /// One of the `TFT_BRIDGE_REASON_*` codes, or `TFT_BRIDGE_REASON_NONE`.
    pub reason: tft_bridge_reason,
    /// The engine status, when `action` is [`TFT_BRIDGE_REJECTED`]; otherwise
    /// [`TFT_OK`].
    pub status: tft_status,
    /// `1` the first time this edge produced this outcome, `0` afterwards — §5.6's "warn once" and
    /// §5.4's rate limit.
    ///
    /// Also set on [`TFT_BRIDGE_HALT`] and [`TFT_BRIDGE_RECREATE`]; both latch and every later
    /// offer replays the action with `first_time = 0`.
    pub first_time: u8,
    /// How far time went **backwards**, as a positive magnitude; `0` when it did not.
    ///
    /// Not merged with [`tft_bridge_outcome::delta_nanos`]: this is a distance, that a signed
    /// displacement, and they differ on a forward jump.
    pub by_nanos: i64,
    /// The parent frame. Normalized (§5.6) where the pipeline named an edge; **as it arrived** for
    /// `TFT_BRIDGE_DROPPED`, `TFT_BRIDGE_HALT` and `TFT_BRIDGE_RECREATE`.
    ///
    /// **Empty when the outcome is not about an arriving transform**: a `STRICT` window close, and
    /// a reported jump ([`tft_bridge_note_time_jump`]).
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
    /// How far time moved, and **which way**: new time minus old, so a rewind is **negative**; `0`
    /// where it does not apply.
    ///
    /// Set for [`TFT_BRIDGE_REASON_CLOCK_RESET`], [`TFT_BRIDGE_RECREATE`] and (as the negation of
    /// `by_nanos`) [`TFT_BRIDGE_REASON_NON_MONOTONIC`]. Same convention as
    /// `rcl_time_jump_t::delta`.
    pub delta_nanos: i64,
    /// **Which rung of §5.5's ladder concluded the clock moved** — a `TFT_BRIDGE_EVIDENCE_*` code.
    /// A reported jump is a fact (look at the bag or simulator); a common-mode step is an inference
    /// (look at those nodes).
    pub clock_evidence: i32,
    /// Read according to `clock_evidence`:
    ///
    /// * [`TFT_BRIDGE_EVIDENCE_REPORTED`] — the [`tft_bridge_jump_kind`] reported.
    /// * [`TFT_BRIDGE_EVIDENCE_COMMON_MODE`] — how many distinct publishers agreed (≥ 2).
    /// * [`TFT_BRIDGE_EVIDENCE_NONE`] — `0`.
    pub clock_evidence_detail: u32,
}

/// Which rung of §5.5's ladder concluded that the clock moved.
pub type tft_bridge_evidence = i32;
/// No clock judgment was made on this outcome; `clock_evidence_detail` is `0`. Every outcome starts
/// here (`tft_bridge_outcome::blank`).
pub const TFT_BRIDGE_EVIDENCE_NONE: tft_bridge_evidence = 0;
/// The time source itself reported the jump, through [`tft_bridge_note_time_jump`];
/// `clock_evidence_detail` is the [`tft_bridge_jump_kind`].
pub const TFT_BRIDGE_EVIDENCE_REPORTED: tft_bridge_evidence = 1;
/// Two or more distinct publishers' stamp-to-receipt offsets stepped by the same amount inside one
/// correlation window; `clock_evidence_detail` is how many. The fallback rung.
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
    /// The time-domain tag the bridge stamps in — `use_sim_time` decides it (§5.5). Every declared
    /// *dynamic* edge must agree or creation fails with [`TFT_ERR_TIME_DOMAIN`]. Must fit in a
    /// `uint8_t`.
    pub domain: u32,
    /// `tf_prefix` remapping (§5.6), or NULL for none.
    pub tf_prefix: *const c_char,
    /// Rendezvous name for a **shared** arena, or NULL for a private heap arena
    /// (`docs/decisions/0015`).
    ///
    /// When non-NULL any process may attach read-only with [`tft_tree_open`](crate::tft_tree_open).
    /// Not [`tft_bridge_options::domain`]: the *rendezvous* domain is `$TF_TREE_DOMAIN`, else
    /// `$ROS_DOMAIN_ID`, else 0 (`docs/decisions/0019` §3).
    ///
    /// Failure is [`TFT_ERR_ARENA_UNAVAILABLE`](crate::TFT_ERR_ARENA_UNAVAILABLE) and **never falls
    /// back to a heap arena**. A library built without `--features shm` refuses a non-NULL value.
    pub arena_name: *const c_char,
}

/// `tft_bridge_options` as ABI **0.4** laid it out, before `arena_name`; the same device as
/// [`tft_bridge_sample_v1`] (`docs/PHASE4.md` §3.6, `docs/decisions/0015`).
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
    // The appended field begins where the old struct ended.
    assert!(size_of::<tft_bridge_options_v1>() == offset_of!(tft_bridge_options, arena_name));
    assert!(size_of::<tft_bridge_options_v1>() < size_of::<tft_bridge_options>());
};

/// Read a caller's [`tft_bridge_options`], accepting the layout that predates `arena_name` as a
/// prefix of the current one.
///
/// `None` means the size belongs to neither build
/// ([`TFT_ERR_BAD_STRUCT_SIZE`](crate::TFT_ERR_BAD_STRUCT_SIZE)). The bounded copy is the safety
/// argument: it never reads past `declared`. Only `tft_bridge_options` and `tft_bridge_sample`
/// accept a prefix; `outcome`, `remap` and `stats` are `out` parameters and stay exact-equality.
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
    // An unwritten `arena_name` is NULL: a private heap arena.
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

/// One row of §5.6's remap table: a frame name as it arrives, and the name the arena knows it by.
///
/// Both strings are borrowed until the next [`tft_bridge_get_remap`] call; [`tft_bridge_offer`]
/// does not invalidate them.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tft_bridge_remap {
    /// `sizeof(tft_bridge_remap)` in the caller's build (§3.6). Exact equality,
    /// for the reason [`tft_bridge_outcome::struct_size`] gives.
    pub struct_size: u32,
    /// The name as it appears on `/tf`.
    pub from: *const c_char,
    /// The name the arena declares, and the one a consumer must look up.
    pub to: *const c_char,
}

/// §5.9's counters, plus the two the C layer alone can see.
///
/// The ledger balances; a mismatch means some path returns without counting:
///
/// ```text
/// applied + rejected_by_arena + static_verified
///         + dropped_authority + dropped_non_monotonic + dropped_bad_name
///         + dropped_kind_change + dropped_undeclared + dropped_bad_pose
///         + refused_after_halt
///     == transforms
/// ```
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
    /// Transforms **the arena took**: the pipeline's approvals minus `rejected_by_arena`.
    pub applied: u64,
    /// `/tf_static` transforms that matched the declared constant (§5.7, §5.8).
    pub static_verified: u64,
    /// Dropped because another publisher owns the edge (§5.4).
    pub dropped_authority: u64,
    /// Transforms **the clock rules refused** (§5.5): an edge's stamp going backwards at any
    /// magnitude, and the sample that completed a common-mode step (which may be monotone).
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
    /// The pipeline approved the write and the arena refused it — a revoked claim, or a writer
    /// poisoned by a `fork()`.
    pub rejected_by_arena: u64,
    /// Offers refused because the bridge had already stopped — after a
    /// [`TFT_BRIDGE_HALT`] *or* a [`TFT_BRIDGE_RECREATE`], both of which latch.
    pub refused_after_halt: u64,
    /// Clock resets concluded (§5.5) — **promotions**, not regressions; a lone publisher's
    /// regression is counted in `dropped_non_monotonic` only. Under `HALT` this is 0 or 1.
    pub clock_resets: u64,
    /// Static-transform value conflicts (§5.7).
    pub static_conflicts: u64,
    /// The **deepest** the subscription queue has been, as reported by
    /// [`tft_bridge_note_queue_depth`].
    pub queue_high_water: u32,
    /// The subscription's configured depth, so the high-water mark reads as a
    /// fraction. `100` per §5.2.
    pub queue_capacity: u32,
}

/// Borrowed NUL-terminated scratch for one outcome's strings, rewritten in place on every offer.
///
/// Nothing resets these between calls: [`tft_bridge_outcome::blank`] starts every pointer at a
/// static empty string, so a buffer pointer appears only where the same arm just wrote it.
#[derive(Default)]
struct Strings {
    parent: Vec<u8>,
    child: Vec<u8>,
    owner: Vec<u8>,
    intruder: Vec<u8>,
    detail: Vec<u8>,
    /// The row [`tft_bridge_get_remap`] last returned, in its own buffers so logging an outcome
    /// while walking the remap table cannot rewrite it.
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

/// The `""` every outcome field starts at. `static` so `blank` needs no handle and `*out` can be
/// filled before the handle is validated.
static EMPTY: [c_char; 1] = [0];

/// An ingest bridge: the decision pipeline, the arena it writes to, one claim per declared dynamic
/// edge, and §5.3's GID cache.
///
/// `#[repr(C)]` because `check_bridge` reads the magic through a field projection. The generated
/// header declares it incomplete.
#[repr(C)]
pub struct tft_bridge {
    magic: u64,
    /// The token of the thread that called [`tft_bridge_create`].
    owner: u64,
    inner: Box<BridgeInner>,
}

/// Everything behind the handle, boxed so `cbindgen` has one type to exclude.
struct BridgeInner {
    ingest: Ingest,
    /// One claim per declared dynamic edge, keyed by the **normalized** child name.
    ///
    /// Keyed on the name, not the `FrameId`: `Tree::frame` on a writable arena is a blake3 hash
    /// plus an intern probe per sample, a third of the call (`examples/bridge_cost.rs`).
    writers: BTreeMap<String, OwnedWriter>,
    /// §5.3's GID → publisher cache, the one home of publisher identity. Filled on first sight by
    /// [`publisher_of`], named by [`tft_bridge_attribute`]. Holds a whole [`Publisher`] so the hot
    /// path allocates nothing.
    gids: BTreeMap<[u8; 16], Publisher>,
    /// The reusable `Sample` handed to [`Ingest::offer`].
    scratch: Sample,
    /// The outcome's borrowed strings.
    strings: Strings,
    /// Latched once the pipeline says stop (§5.5); there is deliberately no resume.
    ///
    /// [`TFT_BRIDGE_RECREATE`] latches too: the pipeline has already forgotten every edge's
    /// high-water mark, so later offers would be approved and refused by the arena one by one.
    stopped: Option<Stopped>,
    dropped_bad_pose: u64,
    rejected_by_arena: u64,
    refused_after_halt: u64,
    /// The handle share this bridge reads and hands out through [`tft_bridge_tree`]. Each
    /// [`OwnedWriter`] carries its own `Arc<Tree>` (`docs/decisions/0017`), so field order is not
    /// load-bearing.
    share: Arc<TreeShare>,
}

/// Why the bridge stopped, replayed on every later offer. The *action* is kept so a `RECREATE` is
/// not replayed as `HALT`.
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

/// Create the **shared** arena `tft_bridge_options::arena_name` asks for, and publish it under that
/// name.
///
/// Uses `tf_tree::Open`, not `TreeBuilder::build_shared`, which publishes no rendezvous
/// (`docs/decisions/0015`). `require_create(true)` because `IfAbsent` would silently join an arena
/// somebody else sized (`docs/decisions/0019` §3, question 3). The rendezvous domain is the
/// environment's, not `tft_bridge_options::domain`.
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
        // The one failure an operator will actually hit.
        Err(OpenError::ArenaAlreadyLive) => Err(arena_unavailable(&already_live_message(name))),
        // Everything else arrives with its own text (`docs/decisions/0059`).
        Err(e) => Err(arena_unavailable(&generic_failure_message(name, &e))),
    }
}

/// "Somebody else holds this name" — [`open_shared`]'s named arm.
///
/// A function so [`tests::both_named_messages_survive_the_longest_arena_name`] can measure it. Kept
/// short: `set_message` truncates at [`crate::TFT_MESSAGE_LEN`].
#[cfg(any(test, all(feature = "shm", target_os = "linux")))]
fn already_live_message(name: &str) -> String {
    format!(
        "shared arena {name:?}: another participant already holds this rendezvous \
         name, and a bridge will not join an arena it did not size \
         (docs/decisions/0015). Stop it, or use a different arena_name."
    )
}

/// [`open_shared`]'s catch-all arm: the condition first, the detail last, so truncation eats the
/// caller's own name and not the diagnosis.
#[cfg(any(test, all(feature = "shm", target_os = "linux")))]
fn generic_failure_message(name: &str, detail: &dyn core::fmt::Display) -> String {
    format!("shared arena could not be created: {detail} (arena_name {name:?})")
}

/// The `bridge`-without-`shm` refusal's text; short for the same reason as
/// [`already_live_message`].
#[cfg(any(test, not(all(feature = "shm", target_os = "linux"))))]
fn no_shm_message(name: &str) -> String {
    format!(
        "shared arena {name:?}: built without --features shm, so this library has no \
         shared memory behind arena_name. Rebuild with \
         `cargo build -p tf_tree_c --features bridge,shm`, or leave it NULL."
    )
}

/// The `bridge`-without-`shm` build's answer: a **refusal**, never a silent downgrade to a heap
/// arena (`docs/decisions/0015`).
#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn open_shared(name: &str, _builder: tf_tree::TreeBuilder) -> Result<tf_tree::Tree, tft_status> {
    Err(arena_unavailable(&no_shm_message(name)))
}

/// Build a bridge over the topology described by `config_toml`, and the arena that topology
/// declares.
///
/// The config is text, not a path. The engine has no runtime edge declaration
/// (`docs/decisions/0004`, §5.8's amendment), so everything the bridge will write must be in it.
/// Creates the arena, claims every declared dynamic edge, and refuses to start if any of that
/// fails. The calling thread **owns** the bridge.
///
/// `opts->struct_size` selects the layout; the one predating `arena_name` is accepted as a prefix
/// (§3.6) and keeps its private heap arena.
///
/// # Blocking
///
/// With a non-NULL `opts->arena_name` this goes through `tf_tree::Open` and may block up to
/// `DEFAULT_OPEN_TIMEOUT` (5 s). A NULL `arena_name` does not.
///
/// # Errors
///
/// * [`TFT_ERR_BAD_CONFIG`] — the file does not parse, **declares no edges**, declares a cycle, or
///   describes a topology the engine will not build.
/// * [`TFT_ERR_TIME_DOMAIN`] — a declared dynamic edge's domain is not `opts->domain` (§5.5,
///   NORMATIVE, at startup by design).
/// * [`TFT_ERR_ALREADY_CLAIMED`](crate::TFT_ERR_ALREADY_CLAIMED) and the rest of the claim family —
///   another participant holds a declared edge.
/// * [`TFT_ERR_ARENA_UNAVAILABLE`](crate::TFT_ERR_ARENA_UNAVAILABLE) — a non-NULL
///   `opts->arena_name` could not be served (name held, unusable runtime directory, no `shm`
///   feature); **no heap fallback**.
/// * [`TFT_ERR_BAD_STRUCT_SIZE`] — `opts->struct_size` is neither this build's size nor the one
///   before it.
///
/// # Safety
///
/// `config_toml` must be NUL-terminated UTF-8. `opts` must be NULL or point to a
/// `tft_bridge_options` whose `struct_size` is set **and which has at least that many readable
/// bytes**. `out` must be NULL or point to a writable `*mut tft_bridge`.
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
        // SAFETY: `out` is non-null and the caller contracts it writable; a caller who ignores
        // the status must not read an uninitialised `*out`.
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
            // Rendered here: `ConfigError` borrows from `text`.
            Err(e) => return bad_config(&format!("topology config: {e}")),
        };
        // A topology declaring no edges is refused: such a bridge answers `TFT_BRIDGE_UNDECLARED`
        // to all traffic with nothing failing at startup (§5.8's amendment, `docs/decisions/0004`).
        if config.edges.is_empty() {
            // ASCII only: `set_message` substitutes `?` per non-ASCII byte.
            return bad_config(
                "topology config: no edges are declared, so this bridge could never write \
                 anything. Produce a config with `tf_tree topology --discover`; the engine \
                 has no runtime edge declaration (docs/PHASE4.md 5.8, docs/decisions/0004).",
            );
        }
        // §5.5's NORMATIVE startup refusal, before the arena is built.
        if let Err(e) = config.check_domain(domain) {
            set_error(
                TFT_ERR_TIME_DOMAIN,
                &format!("topology config: {e}"),
                |_| {},
            );
            return TFT_ERR_TIME_DOMAIN;
        }
        // The builder would name the cycle by an arena index the operator cannot resolve.
        if let Some(child) = config.cycle_child() {
            return bad_config(&format!(
                "topology config: the declared topology has a cycle through frame {child:?}"
            ));
        }
        // The pipeline is built first and the arena from `ingest.declared()`: with a `tf_prefix`
        // that differs from `config`, and an arena built from `config` would make every sample
        // undeclared.
        let ingest = Ingest::with(&config, authority, on_reset, prefix);
        let declared = ingest.declared();
        // The same builder either way, so a `tf_prefix`-rewritten topology sizes the shared arena
        // too.
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
        // Claim every declared dynamic edge now (D7): a deployment fault should refuse to start,
        // not climb a drop counter.
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
/// Independently owned: it shares the refcount, so freeing either does not disturb the other. Free
/// it with [`tft_tree_free`](crate::tft_tree_free) exactly once. `Send + Sync`, so reader threads
/// may use it while the executor ingests.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `out` must be NULL or point to a
/// writable `*mut tft_tree`.
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
    // Affinity applies to `free`: dropping the writers releases claims and OFD leases, from the
    // owning thread only (§3.2).
    // SAFETY: `check_bridge` confirmed this is a live `tft_bridge`.
    if check_thread_token(unsafe { (*b).owner }, "tft_bridge") != TFT_OK {
        return;
    }
    // Zero the magic first, so a repeated free sees a dead handle.
    // SAFETY: `check_bridge` confirmed the magic word.
    unsafe { core::ptr::write(b.cast::<u64>(), 0) };
    // SAFETY: produced by `Box::into_raw` in `tft_bridge_create`.
    drop(unsafe { Box::from_raw(b) });
}

/// Offer one transform: run every §5 table, then write the arena.
///
/// `gid` is the publisher's `rmw_message_info_t::publisher_gid` (16 bytes) or NULL. An unresolved
/// GID is not an error (§5.3): the publisher is `<unknown publisher>`, a missing one
/// `<unattributed>`.
///
/// # The return value answers a different question from the outcome
///
/// The status says whether the *call* was well-formed. Everything that happened to the sample,
/// including rejection, is in `*out`, which is filled before anything can fail.
///
/// # Orderings
///
/// The pose is validated before the pipeline runs, so a garbage first message cannot take an edge
/// under `FirstWriterWins`. A halted bridge refuses everything: the ABI cannot stop the caller's
/// process, so this is what stopping means.
///
/// # `TFT_BRIDGE_RECREATE` is a report, not an action
///
/// This ABI will not build a fresh arena: every plan and `tft_tree` handle points into the current
/// one. The caller tears the bridge down, rebuilds it, and re-plans.
///
/// # An older caller's sample still works
///
/// A `struct_size` naming the pre-`received_steady_nanos` layout is accepted as a prefix (§3.6); a
/// *larger* size is refused ([`tft_check_abi`](crate::tft_check_abi)). The missing field is filled
/// from this library's own steady clock, never from `stamp_nanos`, which would reintroduce
/// inference over the signal under suspicion.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `s` must point to a
/// `tft_bridge_sample` with `struct_size` set and at least that many readable bytes, and both frame
/// pointers NUL-terminated. `gid` must be NULL or point to 16 readable bytes. `out` must point to a
/// writable `tft_bridge_outcome` with `struct_size` set.
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
        // A blank outcome first, **before the handle is validated**, so a caller that ignores the
        // status reads a well-formed "nothing happened"
        // (`a_bad_handle_still_leaves_a_printable_outcome`).
        let mut o = tft_bridge_outcome::blank();
        // SAFETY: `out` is non-null and the caller contracts a writable `tft_bridge_outcome`,
        // aligned for the whole struct; `ptr::write` neither reads nor drops the old value, and the
        // type is `Copy` with no padding invariants.
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
            // An argument fault, not a sample outcome.
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
            // The evidence is not replayed: it was reported once, with `first_time = 1`.
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

        // Reuse the scratch sample's allocations.
        inner.scratch.frame_id.clear();
        inner.scratch.frame_id.push_str(parent);
        inner.scratch.child_frame_id.clear();
        inner.scratch.child_frame_id.push_str(child);
        inner.scratch.stamp_nanos = sample.stamp_nanos;
        inner.scratch.pose = sample.pose;
        // `read_sample` substituted the steady clock for a pre-field caller; `0` means no receipt
        // clock.
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

/// Read a caller's `tft_bridge_sample`, accepting the layout that predates `received_steady_nanos`
/// as a prefix of the current one.
///
/// `None` means the size belongs to neither build. The bounded copy is the safety argument: it
/// never reads past `declared`, and `u8` tolerates a misaligned pointer.
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
    // Unwritten fields default to "not supplied"; a `0` receipt time is corrected below for v1.
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

/// This library's own steady clock, in nanoseconds, for a caller too old to supply one.
///
/// Independent of the clock under test, as §5.5 requires. The epoch is the first call; only
/// differences are taken. The `+ 1` keeps the first reading off `0`, which means "no receipt
/// clock".
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
    /// Returned when the middleware told us nothing.
    static UNATTRIBUTED: Publisher = Publisher::Unattributed;
    if gid.is_null() {
        return &UNATTRIBUTED;
    }
    // SAFETY: the caller contracts 16 readable bytes.
    let key: [u8; 16] = unsafe { core::ptr::read_unaligned(gid.cast::<[u8; 16]>()) };
    // An all-zero GID is what an RMW with no GID to report leaves behind.
    if key == [0u8; 16] {
        return &UNATTRIBUTED;
    }
    // First sight populates the cache, so a GID is a distinct publisher from its first sample,
    // named or not. `tft_bridge_attribute` later upgrades the name in place.
    gids.entry(key).or_insert_with(|| Publisher::from_gid(&key))
}

/// Turn a pipeline [`Action`] into the outcome POD, performing the arena write when there is one.
/// Safe code only.
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
                // Borrow the engine's message rather than invent a second wording.
                set(&mut inner.strings.detail, &crate::error::last_message());
                o.detail = ptr(&inner.strings.detail);
                // Reachable only when the arena refuses a write the pipeline approved: a revoked
                // claim or a `fork()`ed child.
                // `a_forked_child_is_refused_by_every_bridge_entry_point`
                // (`crates/tf_tree_bench/tests/fork.rs`) pins it.
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
            // A `DROPPED` with fields, not an action of its own: §5.4 needs both nodes, the edge
            // and `first_time`.
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
                    // Both fields, in their two conventions: a caller may read either.
                    o.by_nanos = *by_nanos;
                    o.delta_nanos = -*by_nanos;
                    TFT_BRIDGE_REASON_NON_MONOTONIC
                }
            };
            name_the_edge(inner, o);
        }
        Action::Halt { reason } => {
            o.action = TFT_BRIDGE_HALT;
            // Announced once: `inner.stopped` is latched below and later offers replay via the
            // `Stopped` path with `first_time` 0.
            o.first_time = 1;
            // The detail is the match's value so no arm's `detail` can be overwritten; the evidence
            // and startup counts have nowhere else to go.
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
                    // Named only for the inferred rung: a reported jump has no transform in hand,
                    // so `scratch` would name an innocent edge.
                    if matches!(evidence, ClockEvidence::CommonMode { .. }) {
                        name_the_edge(inner, o);
                    }
                    format!(
                        "the clock moved: {}; the bridge halted, free it and build a new one",
                        clock_evidence(*evidence, *delta_nanos)
                    )
                }
                HaltReason::StartupConflicts { authority, statics } => {
                    o.reason = TFT_BRIDGE_REASON_STARTUP_CONFLICTS;
                    // No `name_the_edge`: the window closed on transforms counted earlier, so
                    // `scratch` would name an innocent edge. The edges go in `detail`.
                    let mut d = format!(
                        "STRICT: the startup window closed with {authority} authority and \
                         {statics} static conflict(s); this deployment is misconfigured and \
                         the bridge will not start"
                    );
                    // §5.4's amendment: `detail` enumerates **every** recorded edge with both
                    // publishers, in one shape for authority and static conflicts.
                    for (parent, child, owner, intruder, n) in inner.ingest.authority().conflicts()
                    {
                        let _ = write!(
                            d,
                            "; authority {parent}->{child}: {owner} vs {intruder} \
                             ({n} sample(s) dropped)"
                        );
                    }
                    for (parent, child, owner, intruder, n) in
                        inner.ingest.statics().conflicts_by_edge()
                    {
                        // Observations, not drops: `/tf_static` is `transient_local` (§5.7).
                        let _ = write!(
                            d,
                            "; static {parent}->{child}: {owner} vs {intruder} \
                             ({n} observation(s))"
                        );
                    }
                    d
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
            // Evidence on both rungs, so an inferred recreate is distinguishable from an announced
            // one.
            set_evidence(o, *evidence);
            // No edge is named: the pipeline does not say which entry point reached this arm, and
            // no edge is more implicated under `Recreate`.
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

/// How far a signed displacement went **backwards**, as a positive magnitude; `0` if forwards.
/// `unsigned_abs` because `-i64::MIN` overflows.
fn backwards_by(delta_nanos: i64) -> i64 {
    if delta_nanos >= 0 {
        return 0;
    }
    i64::try_from(delta_nanos.unsigned_abs()).unwrap_or(i64::MAX)
}

/// Copy the pipeline's evidence into the outcome's two branchable fields, the same value
/// [`clock_evidence`] words.
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

/// The half-sentence naming which rung of §5.5's ladder concluded the clock moved, and by how much.
fn clock_evidence(evidence: ClockEvidence, delta_nanos: i64) -> String {
    let (way, magnitude) = if delta_nanos < 0 {
        ("backwards", delta_nanos.unsigned_abs())
    } else {
        ("forwards", delta_nanos.unsigned_abs())
    };
    match evidence {
        ClockEvidence::Reported { kind } => {
            let what = match kind {
                // The delta across a source change is not a duration.
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

/// Fill `parent`/`child` from the sample as it arrived, for outcomes whose [`Action`] does not
/// carry the normalized pair (§5.4, §5.5).
///
/// The raw names identify the same edge, and are the only useful ones for
/// `TFT_BRIDGE_REASON_BAD_NAME`.
fn name_the_edge(inner: &mut BridgeInner, o: &mut tft_bridge_outcome) {
    set(&mut inner.strings.parent, &inner.scratch.frame_id);
    set(&mut inner.strings.child, &inner.scratch.child_frame_id);
    o.parent = ptr(&inner.strings.parent);
    o.child = ptr(&inner.strings.child);
}

/// Write one approved sample into the arena: one `BTreeMap` probe on the normalized child name,
/// then the ring push (see `BridgeInner::writers`).
fn write_sample(
    inner: &mut BridgeInner,
    child: &str,
    stamp: i64,
    iso: tf_tree::Iso3,
) -> tft_status {
    let Some(w) = inner.writers.get(child) else {
        // Unreachable through the pipeline; resolved here only so the diagnostic names the frame.
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

/// Record that `gid` belongs to `node_name` — §5.3's cache, filled from the node's graph-change
/// handler.
///
/// A known GID's name is **replaced**. An all-zero `gid` is refused with
/// [`TFT_ERR_BAD_ENUM`](crate::TFT_ERR_BAD_ENUM): it is what an RMW leaves when it has no GID.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `gid` must point to 16 readable
/// bytes and `node_name` be NUL-terminated UTF-8.
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
        // Upgrade the name, never the identity: mutate the entry rather than insert a new
        // `Publisher`.
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
/// §5.6: *"A silent remap is worse than no remap."* The table is complete before the first message;
/// walk it right after create:
///
/// ```c
/// tft_bridge_remap r = { .struct_size = sizeof r };
/// for (uint32_t i = 0; tft_bridge_get_remap(b, i, &r) == TFT_OK; i++)
///     RCLCPP_INFO(log, "tf_tree: frame %s is declared as %s", r.from, r.to);
/// ```
///
/// An empty table returns [`TFT_ERR_NO_DATA`](crate::TFT_ERR_NO_DATA) on the first call, the loop's
/// termination condition.
///
/// # Errors
///
/// * [`TFT_ERR_NO_DATA`](crate::TFT_ERR_NO_DATA) — `index` is past the last row.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `out` must point to a writable
/// `tft_bridge_remap` whose `struct_size` is set.
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
        // Copied into the handle's own buffers: a Rust `String` is not NUL-terminated.
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

/// Note that a `TFMessage` arrived, whatever it contained (§5.9); the ratio to `transforms` shows
/// batching versus spamming.
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
/// Feed it `rcl_time_jump_t` from `rcl_clock_add_jump_callback`: no threshold, no corroboration, so
/// it applies [`TFT_BRIDGE_ON_CLOCK_RESET_HALT`] or [`TFT_BRIDGE_ON_CLOCK_RESET_RECREATE`]
/// directly. `delta_nanos` is `rcl_time_jump_t::delta.nanoseconds` (new minus old; a rewind is
/// **negative**), unnegated; `kind` collapses `rcl_clock_change_t` onto the three
/// [`tft_bridge_jump_kind`] codes.
///
/// # Not from the jump callback
///
/// rclcpp's jump callback does not run on the bridge's thread. The callback must record the jump
/// into a slot the ingest thread drains, and call this from there.
///
/// # Charges no counter
///
/// It is not a transform, so no ledger term moves, even on a stopped bridge; `clock_resets` does
/// increment.
///
/// # Errors
///
/// * [`TFT_ERR_BAD_ENUM`](crate::TFT_ERR_BAD_ENUM) — `kind` is not one of the three codes.
///
/// A stopped bridge is **not** an error: `*out` replays the latched action with
/// [`TFT_BRIDGE_REASON_ALREADY_HALTED`], as [`tft_bridge_offer`] does.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `out` must point to a writable
/// `tft_bridge_outcome` with `struct_size` set.
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
        // Blank outcome before the handle is validated, as in `tft_bridge_offer`.
        let mut o = tft_bridge_outcome::blank();
        // SAFETY: `out` is non-null and the caller contracts a writable `tft_bridge_outcome`,
        // aligned for the whole struct; `ptr::write` neither reads nor drops the old value, and the
        // type is `Copy` with no padding invariants.
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

        // A stopped bridge stops, charging nothing: this call is not a transform.
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
        // Through `fill`, so the latch, `first_time` and halt wording exist once. `Iso3::IDENTITY`
        // is ignored: the action is only `Halt` or `RecreateArena`.
        fill(inner, &action, tf_tree::Iso3::IDENTITY, &mut o);
        // SAFETY: as the first write above.
        unsafe { core::ptr::write(out, o) };
        TFT_OK
    })
}

/// Close `STRICT`'s startup window (§5.4), halting once if conflicts were recorded in it.
///
/// The **primary** mechanism §5.4's amendment names (`docs/decisions/0011` step 6); otherwise the
/// window closes only on the 4096-transform backstop.
///
/// # Closing too early costs the policy
///
/// A window that closes before the conflicting sample arrives reports nothing, and `STRICT`
/// degrades to `FirstWriterWins` plus counters for the life of the process. `/tf_static` samples
/// can land seconds after start, so the duration chosen trades coverage, not just start-up latency;
/// the caller owns it.
///
/// It charges no counter, like [`tft_bridge_note_time_jump`].
///
/// # Outcomes
///
/// * **Conflicts recorded** — [`TFT_BRIDGE_HALT`] with [`TFT_BRIDGE_REASON_STARTUP_CONFLICTS`];
///   `detail` enumerates every edge with both publishers. The bridge is latched.
/// * **None, or not `STRICT`** — [`TFT_BRIDGE_DROPPED`] with [`TFT_BRIDGE_REASON_NONE`] (the blank
///   outcome).
/// * **Called twice** — never an error: replays [`TFT_BRIDGE_REASON_ALREADY_HALTED`] if the first
///   call halted, else the "none" arm (`Ingest::close_startup_window` is idempotent).
/// * **Already halted** — [`TFT_BRIDGE_REASON_ALREADY_HALTED`], as [`tft_bridge_note_time_jump`]
///   does.
///
/// # Bridge thread only
///
/// §3.2's affinity applies. A `create_wall_timer` with no `callback_group` lands in the node's
/// default group and fires on the wrong thread: a debug build aborts, a release build returns
/// [`TFT_ERR_WRONG_THREAD`](crate::TFT_ERR_WRONG_THREAD) and the window is never closed.
/// `ros/tf_tree_ros/src/bridge_handle.cpp` spins its own group for this reason.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `out` must point to a writable
/// `tft_bridge_outcome` with `struct_size` set.
#[no_mangle]
pub unsafe extern "C" fn tft_bridge_close_startup_window(
    b: *mut tft_bridge,
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
        // Blank outcome before the handle is validated, as in `tft_bridge_offer`.
        let mut o = tft_bridge_outcome::blank();
        // SAFETY: `out` is non-null and the caller contracts a writable `tft_bridge_outcome`,
        // aligned for the whole struct; `ptr::write` neither reads nor drops the old value, and the
        // type is `Copy` with no padding invariants.
        unsafe { core::ptr::write(out, o) };

        // SAFETY: the caller contracts a live handle.
        let h = match unsafe { bridge_of(b) } {
            Ok(h) => h,
            Err(rc) => return rc,
        };
        let inner = &mut *h.inner;

        // A stopped bridge stops, charging nothing.
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

        // Through `fill`, like every action-producing path. `None` means the window was already
        // closed, nothing was recorded, or the policy is not `STRICT`; the blank outcome is the
        // answer.
        if let Some(action) = inner.ingest.close_startup_window() {
            fill(inner, &action, tf_tree::Iso3::IDENTITY, &mut o);
            // SAFETY: as the first write above.
            unsafe { core::ptr::write(out, o) };
        }
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
/// Named `get_stats` because a `tft_bridge_stats` function would collide with the struct's typedef
/// in C.
///
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `out` must point to a writable
/// `tft_bridge_stats` whose `struct_size` is set.
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
            // The pipeline never saw bad-pose drops or offers refused after a halt; add them so the
            // ledger balances.
            transforms: s.transforms + inner.dropped_bad_pose + inner.refused_after_halt,
            // `applied` means the arena took it: the pipeline's count minus `rejected_by_arena`;
            // `saturating_sub` regardless.
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

impl tft_bridge_outcome {
    /// A well-formed "nothing happened" outcome, with every string pointing at the static empty
    /// string (never NULL). Public so callers need not `mem::zeroed()` (`docs/decisions/0048`).
    #[must_use]
    pub fn blank() -> tft_bridge_outcome {
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
            // Clears the evidence fields once, for every arm.
            clock_evidence: TFT_BRIDGE_EVIDENCE_NONE,
            clock_evidence_detail: 0,
        }
    }
}

impl tft_bridge_stats {
    /// An all-zero `tft_bridge_stats` with `struct_size` set. Enumerated so a new counter is a
    /// compile error here (`docs/decisions/0048`).
    #[must_use]
    pub const fn blank() -> tft_bridge_stats {
        tft_bridge_stats {
            struct_size: core::mem::size_of::<tft_bridge_stats>() as u32,
            messages: 0,
            transforms: 0,
            applied: 0,
            static_verified: 0,
            dropped_authority: 0,
            dropped_non_monotonic: 0,
            dropped_bad_name: 0,
            dropped_kind_change: 0,
            dropped_undeclared: 0,
            dropped_bad_pose: 0,
            rejected_by_arena: 0,
            refused_after_halt: 0,
            clock_resets: 0,
            static_conflicts: 0,
            queue_high_water: 0,
            queue_capacity: 0,
        }
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

/// `docs/decisions/0015`'s startup refusal: a shared arena was asked for and could not be had; no
/// heap fallback. The message says *which* fault.
fn arena_unavailable(msg: &str) -> tft_status {
    set_error(crate::TFT_ERR_ARENA_UNAVAILABLE, msg, |_| {});
    crate::TFT_ERR_ARENA_UNAVAILABLE
}

#[cfg(test)]
mod tests {
    //! Unit tests for the message text, in a module that compiles in both `shm` and non-`shm`
    //! builds so both messages are measurable.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use crate::TFT_MESSAGE_LEN;

    /// `tf_tree_ipc::MAX_NAME_LEN`, mirrored; the `shm` test below binds it to the real one.
    const MAX_NAME_LEN: usize = 64;

    /// The longest name a message can be asked to carry: `{:?}` escapes none of it.
    fn longest_name() -> String {
        "x".repeat(MAX_NAME_LEN)
    }

    /// What `tft_error::set_message` will actually keep: it truncates at
    /// `TFT_MESSAGE_LEN - 1` and writes the NUL itself.
    fn fits_whole(msg: &str) -> bool {
        crate::error::set_error(crate::TFT_ERR_ARENA_UNAVAILABLE, msg, |_| {});
        crate::error::last_message() == msg
    }

    /// Both named `arena_unavailable` messages survive the longest arena name whole.
    ///
    /// The assertion is a round trip through `set_message`; the slack is reported on failure.
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

    /// The catch-all arm's ordering: the condition survives, the name is what gets eaten.
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

    /// Checks the literal above against the rendezvous: `MAX_NAME_LEN` bytes is accepted, one more
    /// is not.
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
