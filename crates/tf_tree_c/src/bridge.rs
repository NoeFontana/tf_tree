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
//! A bridge holds one [`EdgeWriter`](tf_tree::EdgeWriter) per declared dynamic
//! edge, and those are `Send + !Sync`. §5.9 asks for a dedicated
//! `SingleThreadedExecutor` on its own thread, which is exactly the shape this
//! allows: the thread that called [`tft_bridge_create`] owns the handle, a debug
//! build `abort()`s on use from another, and a release build returns
//! [`TFT_ERR_WRONG_THREAD`](crate::TFT_ERR_WRONG_THREAD).

use core::ffi::c_char;
use std::collections::BTreeMap;
use std::sync::Arc;

use tf_tree::EdgeWriter;
use tf_tree_bridge::{
    Action, AuthorityPolicy, DropReason, HaltReason, Ingest, OnClockReset, Publisher, Sample,
    Topic, TopologyConfig,
};

use crate::error::{guard, set_error};
use crate::publisher::{check_thread_token, extend_to_static, thread_token};
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
/// Halt on the first conflict. For CI.
pub const TFT_BRIDGE_AUTHORITY_STRICT: tft_bridge_authority = 2;

/// §5.5's response to a backwards clock jump beyond the reset threshold.
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
/// The clock went backwards past the threshold under `RECREATE`: the caller
/// must tear this bridge down and build a fresh one. `by_nanos` says how far.
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
/// The stamp went backwards, but not far enough to be a reset (§5.5).
pub const TFT_BRIDGE_REASON_NON_MONOTONIC: tft_bridge_reason = 3;
/// The edge is already declared with the other kind (§5.7).
pub const TFT_BRIDGE_REASON_KIND_CHANGE: tft_bridge_reason = 4;
/// `STRICT`, and two publishers appeared on one edge (§5.4).
pub const TFT_BRIDGE_REASON_AUTHORITY_CONFLICT: tft_bridge_reason = 5;
/// `HALT`, and the clock went backwards past the threshold (§5.5).
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
    pub struct_size: u32,
    /// Parent frame, NUL-terminated UTF-8, **exactly as it arrived**. Passing
    /// the raw name is deliberate: §5.6's normalization is what the bridge is
    /// for, and a pre-normalized name would move that judgment into C++.
    pub frame_id: *const c_char,
    /// Child frame, likewise raw.
    pub child_frame_id: *const c_char,
    /// Stamp, nanoseconds, in the bridge's own time domain (§5.5).
    pub stamp_nanos: i64,
    /// `[qw qx qy qz tx ty tz]`.
    pub pose: [f64; 7],
}

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
    /// How far time went backwards, for
    /// [`TFT_BRIDGE_REASON_NON_MONOTONIC`] and [`TFT_BRIDGE_RECREATE`].
    pub by_nanos: i64,
    /// The parent frame. Normalized (§5.6) for every outcome the pipeline
    /// named an edge in; **as it arrived** for `TFT_BRIDGE_DROPPED`,
    /// `TFT_BRIDGE_HALT` and `TFT_BRIDGE_RECREATE`, whose actions carry only a
    /// reason. The difference is one leading `/` and any `tf_prefix`, so the
    /// pair identifies the same edge either way — and for
    /// [`TFT_BRIDGE_REASON_BAD_NAME`] the raw name is the only useful one.
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
}

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
    /// `sizeof(tft_bridge_remap)` in the caller's build (§3.6).
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
    /// `sizeof(tft_bridge_stats)` in the caller's build (§3.6).
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
    /// Dropped because the stamp went backwards (§5.5).
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
    /// claim, or a per-edge stamp the global clock guard could not see.
    pub rejected_by_arena: u64,
    /// Offers refused because the bridge had already stopped — after a
    /// [`TFT_BRIDGE_HALT`] *or* a [`TFT_BRIDGE_RECREATE`], both of which latch.
    pub refused_after_halt: u64,
    /// Clock resets detected (§5.5).
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
/// `#[repr(C)]` for the same reason as [`tft_tree`]: [`check_bridge`] validates
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
    writers: BTreeMap<String, EdgeWriter<'static>>,
    /// §5.3's GID → publisher cache, filled by [`tft_bridge_attribute`].
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
    /// the pipeline's clock guard has already accepted the rewound stamp
    /// (`ClockGuard::accept_reset`), so every subsequent offer would be approved
    /// and the arena would refuse it per edge as non-monotonic: a bag loop would
    /// turn into a silent, permanent stall reported one `rejected_by_arena` at a
    /// time rather than the one loud outcome §5.5 asks for.
    stopped: Option<Stopped>,
    dropped_bad_pose: u64,
    rejected_by_arena: u64,
    refused_after_halt: u64,
    /// Keeps the arena alive for at least as long as the claims point into it.
    ///
    /// **Declared last on purpose.** Fields drop in declaration order, so every
    /// [`EdgeWriter`] is dropped — releasing its claim and its lease — before
    /// the last reference to the `Tree` it borrows from goes away.
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
    /// How far time went backwards, or `0` for an authority conflict.
    by_nanos: i64,
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
///
/// # Safety
///
/// `config_toml` must be NUL-terminated UTF-8. `opts` must be NULL or point to a
/// `tft_bridge_options` whose `struct_size` is set. `out` must be NULL or point
/// to a writable `*mut tft_bridge`.
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
        if !opts.is_null() {
            // SAFETY: the caller contracts a readable `tft_bridge_options` with
            // `struct_size` initialised; the field is read before anything else.
            let declared = unsafe { core::ptr::addr_of!((*opts).struct_size).read_unaligned() };
            if declared as usize != core::mem::size_of::<tft_bridge_options>() {
                return bad_struct_size("tft_bridge_options");
            }
            // SAFETY: `struct_size` matched this build's, so the whole struct is
            // present and readable.
            let o = unsafe { core::ptr::read_unaligned(opts) };
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
        let Ok(tree) = declared.builder().build() else {
            return bad_config("topology config: the declared topology does not build");
        };

        let share = Arc::new(TreeShare { tree });
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
            let w = match share.tree.claim(c, p) {
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
            // SAFETY: `EdgeWriter<'a>` borrows the `Tree` inside `share`; the
            // `Arc<TreeShare>` stored in the same struct is a strong reference
            // to that same `Tree` and is dropped *after* the writers (see
            // `BridgeInner::share`), so the borrow cannot outlive the arena.
            writers.insert(e.child.clone(), unsafe { extend_to_static(w) });
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
/// # Safety
///
/// `b` must be a live handle used from the thread that created it. `s` must
/// point to a readable `tft_bridge_sample` with `struct_size` set and both frame
/// pointers NUL-terminated. `gid` must be NULL or point to 16 readable bytes.
/// `out` must point to a writable `tft_bridge_outcome` with `struct_size` set.
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
        if declared as usize != core::mem::size_of::<tft_bridge_sample>() {
            return bad_struct_size("tft_bridge_sample");
        }
        // SAFETY: `struct_size` matched this build's, so the whole struct is
        // present and readable.
        let sample = unsafe { core::ptr::read_unaligned(s) };
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
            // The wording follows the *latched* action, not the word "halt": a
            // caller told to recreate and then told it halted would read the
            // second as a different, worse fault than the first.
            set(
                &mut inner.strings.detail,
                if st.action == TFT_BRIDGE_RECREATE {
                    "the clock went backwards past the reset threshold; free this bridge, \
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

        // SAFETY: the caller contracts `gid` is NULL or 16 readable bytes.
        let who = unsafe { publisher_of(&inner.gids, gid) };
        let action = inner.ingest.offer(topic, &inner.scratch, who);
        fill(inner, &action, iso, &mut o);
        // SAFETY: as the first write above.
        unsafe { core::ptr::write(out, o) };
        TFT_OK
    })
}

/// Resolve a GID against the cache, per §5.3's degradation rules.
///
/// # Safety
///
/// `gid` must be NULL or point to 16 readable bytes.
unsafe fn publisher_of(gids: &BTreeMap<[u8; 16], Publisher>, gid: *const u8) -> &Publisher {
    /// Returned when the middleware told us nothing. `static` so the borrow
    /// outlives the map's.
    static UNATTRIBUTED: Publisher = Publisher::Unattributed;
    static UNKNOWN: Publisher = Publisher::UnknownGid;
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
    gids.get(&key).unwrap_or(&UNKNOWN)
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
                // * `PushError::NonMonotonicStamp` is dominated by the global
                //   clock guard. `Action::Publish` requires `stamp >= newest`,
                //   and `newest` is the maximum over every accepted sample, so
                //   it is `>=` any one edge's own last stamp. The `Recreate`
                //   path is the only thing that rewinds `newest`, and it
                //   latches `stopped` before another offer can be processed.
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
                    o.by_nanos = *by_nanos;
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
            match reason {
                HaltReason::AuthorityConflict { owner, intruder } => {
                    o.reason = TFT_BRIDGE_REASON_AUTHORITY_CONFLICT;
                    set(&mut inner.strings.owner, &owner.to_string());
                    set(&mut inner.strings.intruder, &intruder.to_string());
                    o.owner = ptr(&inner.strings.owner);
                    o.intruder = ptr(&inner.strings.intruder);
                }
                HaltReason::ClockReset { by_nanos } => {
                    o.reason = TFT_BRIDGE_REASON_CLOCK_RESET;
                    o.by_nanos = *by_nanos;
                }
            }
            name_the_edge(inner, o);
            inner.stopped = Some(Stopped {
                action: TFT_BRIDGE_HALT,
                by_nanos: o.by_nanos,
            });
            set(
                &mut inner.strings.detail,
                "the bridge halted; free it and build a new one",
            );
            o.detail = ptr(&inner.strings.detail);
        }
        Action::RecreateArena { by_nanos } => {
            o.action = TFT_BRIDGE_RECREATE;
            // Latched on the same terms as `Action::Halt` above.
            o.first_time = 1;
            o.reason = TFT_BRIDGE_REASON_CLOCK_RESET;
            o.by_nanos = *by_nanos;
            name_the_edge(inner, o);
            inner.stopped = Some(Stopped {
                action: TFT_BRIDGE_RECREATE,
                by_nanos: *by_nanos,
            });
            set(
                &mut inner.strings.detail,
                "the clock went backwards past the reset threshold; free this bridge, \
                 build a new one, and re-plan",
            );
            o.detail = ptr(&inner.strings.detail);
        }
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
        h.inner.gids.insert(key, Publisher::Node(name.to_string()));
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
