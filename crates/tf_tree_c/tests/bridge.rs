//! The ingest-bridge seam of the C ABI — `docs/PHASE4.md` §5 and §6.3.
#![cfg(feature = "bridge")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Posture (`docs/decisions/0007` rule 1, kind 5; `0048` step 4): our own C ABI called
// from Rust; declared here because a test is a separate crate root.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_char;
use core::ptr;
use std::ffi::{CStr, CString};

use tf_tree_c::bridge::*;
use tf_tree_c::*;

/// One dynamic edge and one static one — the smallest topology that still exercises both `/tf`
/// and `/tf_static`, in the real config format so these tests go through the parser an operator
/// will use.
const TOPO: &str = r#"
[[edge]]
parent = "odom"
child = "base"
kind = "dynamic"
capacity = 256

[[edge]]
parent = "base"
child = "lidar"
kind = "static"
pose = [0.9659258262890683, 0.0, 0.0, 0.25881904510252074, 0.35, -0.02, 0.61]
"#;

/// A 30° yaw with a translation nothing else in the fixture shares, so a read-back that returns
/// identity — or the static edge's pose — fails rather than coincidentally passing.
const POSE: [f64; 7] = [
    0.965_925_826_289_068_3,
    0.0,
    0.0,
    0.258_819_045_102_520_74,
    1.5,
    -2.25,
    0.75,
];

/// A pose that is a valid transform and **not** [`POSE`], for a §5.7 value disagreement.
const OTHER_POSE: [f64; 7] = [
    0.965_925_826_289_068_3,
    0.0,
    0.0,
    0.258_819_045_102_520_74,
    -9.5,
    4.75,
    -0.25,
];

const MS: i64 = 1_000_000;

#[derive(Debug)]
struct Bridge(*mut tft_bridge);

impl Bridge {
    fn new(authority: tft_bridge_authority, on_clock_reset: tft_bridge_on_clock_reset) -> Bridge {
        Bridge::try_new(TOPO, authority, on_clock_reset, 0, None).unwrap_or_else(|rc| {
            panic!("tft_bridge_create: {rc} ({})", last_message());
        })
    }

    fn try_new(
        toml: &str,
        authority: tft_bridge_authority,
        on_clock_reset: tft_bridge_on_clock_reset,
        domain: u32,
        tf_prefix: Option<&str>,
    ) -> Result<Bridge, tft_status> {
        let text = CString::new(toml).unwrap();
        let prefix = tf_prefix.map(|p| CString::new(p).unwrap());
        let opts = tft_bridge_options {
            struct_size: core::mem::size_of::<tft_bridge_options>() as u32,
            authority,
            on_clock_reset,
            domain,
            tf_prefix: prefix.as_ref().map_or(ptr::null(), |p| p.as_ptr()),
            // A private heap arena, which is what this whole file tests. The shared path needs
            // `--features shm` and lives in `tests/bridge_shared.rs`.
            arena_name: ptr::null(),
        };
        let mut b: *mut tft_bridge = ptr::null_mut();
        // SAFETY: NUL-terminated config, a live `opts`, `b` a live local.
        let rc = unsafe { tft_bridge_create(text.as_ptr(), &opts, &mut b) };
        if rc == TFT_OK {
            assert!(!b.is_null());
            Ok(Bridge(b))
        } else {
            assert!(b.is_null(), "a failed create must not hand out a handle");
            Err(rc)
        }
    }

    /// Offer one transform with **no receipt clock**, which is what a caller that has none
    /// supplies.
    fn offer(
        &self,
        topic: tft_bridge_topic,
        parent: &str,
        child: &str,
        stamp: i64,
        pose: [f64; 7],
        gid: Option<&[u8; 16]>,
    ) -> tft_bridge_outcome {
        self.offer_at(topic, parent, child, stamp, 0, pose, gid)
    }

    /// Offer one transform and return the outcome, checking the call itself was well-formed.
    /// The `CString`s outlive the call, which is all the ABI asks.
    #[allow(clippy::too_many_arguments)]
    fn offer_at(
        &self,
        topic: tft_bridge_topic,
        parent: &str,
        child: &str,
        stamp: i64,
        received: i64,
        pose: [f64; 7],
        gid: Option<&[u8; 16]>,
    ) -> tft_bridge_outcome {
        let (p, c) = (CString::new(parent).unwrap(), CString::new(child).unwrap());
        let s = tft_bridge_sample {
            struct_size: core::mem::size_of::<tft_bridge_sample>() as u32,
            frame_id: p.as_ptr(),
            child_frame_id: c.as_ptr(),
            stamp_nanos: stamp,
            pose,
            received_steady_nanos: received,
        };
        let mut out = poisoned_outcome();
        // SAFETY: live handle on its creating thread, a live sample whose name
        // pointers are NUL-terminated, `gid` NULL or 16 bytes, `out` writable.
        let rc = unsafe {
            tft_bridge_offer(
                self.0,
                topic,
                &s,
                gid.map_or(ptr::null(), |g| g.as_ptr()),
                &mut out,
            )
        };
        assert_eq!(rc, TFT_OK, "the call was malformed: {}", last_message());
        out
    }

    /// Close `STRICT`'s startup window — §5.4's primary close, with no transform in hand.
    fn close_startup_window(&self) -> tft_bridge_outcome {
        let mut out = poisoned_outcome();
        // SAFETY: live handle on its creating thread; `out` is a live local with
        // `struct_size` set.
        let rc = unsafe { tft_bridge_close_startup_window(self.0, &mut out) };
        assert_eq!(rc, TFT_OK, "the call was malformed: {}", last_message());
        out
    }

    /// Report a jump the time source itself announced — §5.5's authoritative rung, with no
    /// transform in hand.
    fn note_time_jump(&self, delta_nanos: i64, kind: tft_bridge_jump_kind) -> tft_bridge_outcome {
        let mut out = poisoned_outcome();
        // SAFETY: live handle on its creating thread; `out` is a live local with
        // `struct_size` set.
        let rc = unsafe { tft_bridge_note_time_jump(self.0, delta_nanos, kind, &mut out) };
        assert_eq!(rc, TFT_OK, "the call was malformed: {}", last_message());
        out
    }

    fn stats(&self) -> tft_bridge_stats {
        let mut s = tft_bridge_stats {
            struct_size: core::mem::size_of::<tft_bridge_stats>() as u32,
            ..tft_bridge_stats::blank()
        };
        // SAFETY: live handle on its creating thread; `s` is a live local with
        // `struct_size` set.
        assert_eq!(unsafe { tft_bridge_get_stats(self.0, &mut s) }, TFT_OK);
        s
    }

    /// §5.6's remap table, walked exactly as the doc comment's C loop walks it: row by row
    /// until `TFT_ERR_NO_DATA`.
    fn remaps(&self) -> Vec<(String, String)> {
        let mut rows = Vec::new();
        for i in 0u32.. {
            let mut r = tft_bridge_remap {
                struct_size: core::mem::size_of::<tft_bridge_remap>() as u32,
                from: ptr::null(),
                to: ptr::null(),
            };
            // SAFETY: live handle on its creating thread; `r` is a live local
            // with `struct_size` set.
            let rc = unsafe { tft_bridge_get_remap(self.0, i, &mut r) };
            if rc == TFT_ERR_NO_DATA {
                break;
            }
            assert_eq!(rc, TFT_OK, "{}", last_message());
            rows.push((text(r.from), text(r.to)));
        }
        rows
    }

    fn tree(&self) -> Tree {
        let mut t: *mut tft_tree = ptr::null_mut();
        // SAFETY: live handle on its creating thread, `t` a live local.
        assert_eq!(unsafe { tft_bridge_tree(self.0, &mut t) }, TFT_OK);
        assert!(!t.is_null());
        Tree(t)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // SAFETY: created above, freed exactly once, on the creating thread.
        unsafe { tft_bridge_free(self.0) };
    }
}

struct Tree(*mut tft_tree);

impl Tree {
    /// `target <- source` at `stamp`, as seven `f64`s.
    fn at(&self, target: &str, source: &str, stamp: i64) -> Result<[f64; 7], tft_status> {
        let (t, s) = (CString::new(target).unwrap(), CString::new(source).unwrap());
        let mut plan: *mut tft_plan = ptr::null_mut();
        // SAFETY: live tree handle, NUL-terminated names, `plan` a live local.
        let rc = unsafe { tft_plan_create(self.0, t.as_ptr(), s.as_ptr(), &mut plan) };
        assert_eq!(rc, TFT_OK, "tft_plan_create: {}", last_message());
        let mut out = [0.0f64; 7];
        // SAFETY: live plan; `out` is 56 bytes, which is QVEC7's payload.
        let rc =
            unsafe { tft_plan_at(plan, stamp, TFT_LAYOUT_QVEC7_WXYZ, out.as_mut_ptr().cast()) };
        // SAFETY: created just above, freed exactly once.
        unsafe { tft_plan_free(plan) };
        if rc == TFT_OK {
            Ok(out)
        } else {
            Err(rc)
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // SAFETY: created above, freed exactly once.
        unsafe { tft_tree_free(self.0) };
    }
}

/// An outcome whose every byte is 0xAA apart from `struct_size`.
fn poisoned_outcome() -> tft_bridge_outcome {
    // SAFETY: `tft_bridge_outcome` is `#[repr(C)]`, `Copy`, and made of
    // integers, `f64`s and raw pointers — every bit pattern is a valid value of
    // each. It is never *read* through until the ABI has written it.
    let mut o: tft_bridge_outcome = unsafe { core::mem::transmute([0xAAu8; SIZEOF_OUTCOME]) };
    o.struct_size = SIZEOF_OUTCOME as u32;
    o
}

const SIZEOF_OUTCOME: usize = core::mem::size_of::<tft_bridge_outcome>();

/// A borrowed outcome string, as a C caller would read it.
///
/// # Panics
///
/// If the pointer is NULL — which the ABI documents can never happen, and which
/// is worth asserting rather than papering over, because a NULL here is a
/// `printf("%s")` crash in the node.
fn text(p: *const c_char) -> String {
    assert!(!p.is_null(), "outcome strings are never NULL, only empty");
    // SAFETY: the ABI contracts a NUL-terminated string borrowed from the
    // handle and valid until the next call on it; no call intervenes here.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

// `c_char` is `i8` on x86_64 and `u8` on aarch64, so this cast is necessary on one target and a
// no-op on the other; see `src/error.rs` for the full note.
#[allow(clippy::unnecessary_cast)]
fn last_message() -> String {
    let mut e = tft_error::blank();
    // SAFETY: `e` is a live local with `struct_size` set.
    if unsafe { tft_last_error(&mut e) } != TFT_OK {
        return "<tft_last_error failed>".to_string();
    }
    let bytes: Vec<u8> = e
        .message
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The ledger the `tft_bridge_stats` doc comment states, as an assertion.
fn assert_balanced(s: &tft_bridge_stats) {
    let sum = s.applied
        + s.rejected_by_arena
        + s.static_verified
        + s.dropped_authority
        + s.dropped_non_monotonic
        + s.dropped_bad_name
        + s.dropped_kind_change
        + s.dropped_undeclared
        + s.dropped_bad_pose
        + s.refused_after_halt;
    assert_eq!(
        sum,
        s.transforms,
        "the documented ledger does not balance: {sum} accounted for against \
         {} offered (applied {}, rejected {}, verified {}, authority {}, \
         monotonic {}, name {}, kind {}, undeclared {}, pose {}, after-halt {})",
        s.transforms,
        s.applied,
        s.rejected_by_arena,
        s.static_verified,
        s.dropped_authority,
        s.dropped_non_monotonic,
        s.dropped_bad_name,
        s.dropped_kind_change,
        s.dropped_undeclared,
        s.dropped_bad_pose,
        s.refused_after_halt,
    );
}

/// **The seam writes the arena, and the arena is readable through the handle it hands back.**
/// Both halves of `docs/PHASE4.md` §5 in one call: the pipeline decides, and Rust — not the C++
/// node — performs the write.
#[test]
fn an_offer_on_a_declared_edge_is_written_and_reads_back() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_APPLIED, "{}", text(o.detail));
    assert_eq!(o.status, TFT_OK);
    assert_eq!(
        (text(o.parent), text(o.child)),
        ("odom".into(), "base".into())
    );

    let got = b
        .tree()
        .at("odom", "base", 1_000 * MS)
        .expect("the bridge's own arena must hold what it just applied");
    for (i, (g, w)) in got.iter().zip(POSE.iter()).enumerate() {
        assert!(
            (g - w).abs() < 1e-12,
            "component {i}: read {g}, wrote {w} — full read-back {got:?}"
        );
    }
    assert_eq!(
        (
            o.clock_evidence,
            o.clock_evidence_detail,
            o.by_nanos,
            o.delta_nanos
        ),
        (TFT_BRIDGE_EVIDENCE_NONE, 0, 0, 0),
        "an ordinary write says nothing about the clock, and the fields that \
         describe clock events say nothing rather than something stale"
    );
    let s = b.stats();
    assert_eq!((s.applied, s.rejected_by_arena), (1, 0));
    assert_balanced(&s);
}

/// **A malformed pose never reaches the authority table** (§5.4).
#[test]
fn a_bad_pose_is_refused_before_the_publisher_can_take_the_edge() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    let rogue = [0x11u8; 16];
    let ekf = [0x22u8; 16];
    for (g, n) in [(&rogue, "/rogue"), (&ekf, "/ekf")] {
        let name = CString::new(n).unwrap();
        assert_eq!(
            // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
            unsafe { tft_bridge_attribute(b.0, g.as_ptr(), name.as_ptr()) },
            TFT_OK
        );
    }

    // A quaternion of norm 2 — a plausible mistake (an unnormalized message), not a wild value,
    // so `NotAUnitQuaternion` rather than `NotFinite` is what catches it.
    let bad = [2.0, 0.0, 0.0, 0.0, 0.1, 0.2, 0.3];
    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_000 * MS,
        bad,
        Some(&rogue),
    );
    assert_eq!(o.action, TFT_BRIDGE_DROPPED);
    assert_eq!(o.reason, TFT_BRIDGE_REASON_BAD_POSE);
    assert_eq!(text(o.child), "base", "a bad pose still names its edge");
    assert!(
        text(o.detail).contains("unit quaternion"),
        "detail was {:?}",
        text(o.detail)
    );

    // The correct publisher is still able to take the edge.
    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_010 * MS,
        POSE,
        Some(&ekf),
    );
    assert_eq!(
        o.action,
        TFT_BRIDGE_APPLIED,
        "reason {} / {}",
        o.reason,
        text(o.detail)
    );
    let s = b.stats();
    assert_eq!((s.dropped_bad_pose, s.dropped_authority), (1, 0));
    assert_balanced(&s);
}

/// **§5.4's headline diagnostic survives the C boundary: both nodes, the edge, and a rate-limit
/// flag.**
#[test]
fn an_authority_conflict_names_both_publishers_and_the_edge() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    let (ekf, odom_node) = ([0x33u8; 16], [0x44u8; 16]);
    for (g, n) in [(&ekf, "/ekf"), (&odom_node, "/odom_node")] {
        let name = CString::new(n).unwrap();
        assert_eq!(
            // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
            unsafe { tft_bridge_attribute(b.0, g.as_ptr(), name.as_ptr()) },
            TFT_OK
        );
    }
    b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_000 * MS,
        POSE,
        Some(&ekf),
    );

    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_001 * MS,
        POSE,
        Some(&odom_node),
    );
    assert_eq!(o.action, TFT_BRIDGE_DROPPED);
    assert_eq!(o.reason, TFT_BRIDGE_REASON_NOT_THE_OWNER);
    assert_eq!(text(o.owner), "/ekf");
    assert_eq!(text(o.intruder), "/odom_node");
    assert_eq!(
        (text(o.parent), text(o.child)),
        ("odom".into(), "base".into())
    );
    assert_eq!(o.first_time, 1, "the first collision is the loud one");

    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_002 * MS,
        POSE,
        Some(&odom_node),
    );
    assert_eq!(o.first_time, 0, "and every one after it is rate-limited");
    assert_eq!(text(o.owner), "/ekf");
    assert_balanced(&b.stats());
}

/// **A publisher that gets renamed is still the same publisher.**
#[test]
fn a_publisher_renamed_by_a_later_graph_walk_keeps_its_edge() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    let gid = [0x55u8; 16];

    // The graph's first answer: an endpoint it can see but cannot yet name.
    let placeholder = CString::new("/_NODE_NAMESPACE_UNKNOWN_/_NODE_NAME_UNKNOWN_").unwrap();
    assert_eq!(
        // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
        unsafe { tft_bridge_attribute(b.0, gid.as_ptr(), placeholder.as_ptr()) },
        TFT_OK
    );
    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_000 * MS,
        POSE,
        Some(&gid),
    );
    assert_eq!(
        o.action, TFT_BRIDGE_APPLIED,
        "the first sample takes the edge"
    );

    // The graph's second answer, for the same endpoint.
    let real = CString::new("/tf_bench_publisher").unwrap();
    assert_eq!(
        // SAFETY: live handle, the same 16 readable bytes of `gid`, NUL-terminated
        // name.
        unsafe { tft_bridge_attribute(b.0, gid.as_ptr(), real.as_ptr()) },
        TFT_OK
    );
    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_001 * MS,
        POSE,
        Some(&gid),
    );
    assert_eq!(
        o.action, TFT_BRIDGE_APPLIED,
        "a rename is not a change of publisher; the edge's owner did not move"
    );
    assert_balanced(&b.stats());
}

/// **Two publishers the graph cannot name are still two publishers.**
#[test]
fn two_unnamed_publishers_on_one_edge_still_conflict() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    // Neither GID is ever passed to `tft_bridge_attribute`, so neither has a name — the state
    // an RMW without endpoint introspection leaves.
    let (one, two) = ([0x66u8; 16], [0x77u8; 16]);

    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_000 * MS,
        POSE,
        Some(&one),
    );
    assert_eq!(o.action, TFT_BRIDGE_APPLIED);

    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_001 * MS,
        POSE,
        Some(&two),
    );
    assert_eq!(
        o.action, TFT_BRIDGE_DROPPED,
        "two distinct GIDs are two publishers even with no names for them"
    );
    assert_eq!(o.reason, TFT_BRIDGE_REASON_NOT_THE_OWNER);
    // And the diagnostic must be able to tell them apart, or it says two identical things are
    // fighting.
    let (owner, intruder) = (text(o.owner), text(o.intruder));
    assert_ne!(
        owner, intruder,
        "the diagnostic must distinguish them: {owner} vs {intruder}"
    );
    assert!(
        owner.starts_with("<gid:"),
        "unnamed publishers print their GID: {owner}"
    );
    assert_balanced(&b.stats());
}

/// **An unattributed publisher is not an error** (§5.3: attribution degrades).
#[test]
fn an_unreported_gid_degrades_rather_than_failing() {
    let b = Bridge::new(TFT_BRIDGE_AUTHORITY_STRICT, TFT_BRIDGE_ON_CLOCK_RESET_HALT);
    // No GID at all.
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_APPLIED);

    // An all-zero GID is the same publisher as no GID, so `Strict` — which records a conflict
    // on the *second* distinct publisher of an edge, and halts once at its startup window's
    // close if it recorded any — must find nothing to record.
    let zero = [0u8; 16];
    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_010 * MS,
        POSE,
        Some(&zero),
    );
    assert_eq!(
        o.action,
        TFT_BRIDGE_APPLIED,
        "an unreported GID must not read as a second publisher: {} / {}",
        o.reason,
        text(o.detail)
    );

    // …and caching a name under the zero GID is refused, for the same reason.
    let name = CString::new("/somebody").unwrap();
    // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
    let rc = unsafe { tft_bridge_attribute(b.0, zero.as_ptr(), name.as_ptr()) };
    assert_eq!(rc, TFT_ERR_BAD_ENUM);
}

/// **`STRICT` halts when its startup window closes, not on the message that collided — and a
/// halted bridge then refuses everything, with the ledger still balancing.**
#[test]
fn a_halted_bridge_refuses_every_later_offer() {
    let b = Bridge::new(TFT_BRIDGE_AUTHORITY_STRICT, TFT_BRIDGE_ON_CLOCK_RESET_HALT);
    let (a, z) = ([0x55u8; 16], [0x66u8; 16]);
    for (g, n) in [(&a, "/a"), (&z, "/b")] {
        let name = CString::new(n).unwrap();
        assert_eq!(
            // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
            unsafe { tft_bridge_attribute(b.0, g.as_ptr(), name.as_ptr()) },
            TFT_OK
        );
    }
    // **Every `tft_bridge_offer` call this test makes, counted** — all of them, which is itself
    // the shape step 6 changed: §5.4's primary close is a separate call, so there is no
    // uncounted offer left to explain.
    let mut offers = 0u64;
    let mut offer = |topic: tft_bridge_topic,
                     parent: &str,
                     child: &str,
                     stamp: i64,
                     pose: [f64; 7],
                     gid: &[u8; 16]| {
        offers += 1;
        b.offer(topic, parent, child, stamp, pose, Some(gid))
    };

    offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, &a);
    let o = offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_010 * MS, POSE, &z);
    assert_eq!(
        o.action,
        TFT_BRIDGE_DROPPED,
        "inside the window a STRICT collision is dropped, not halted on: {}",
        text(o.detail)
    );
    assert_eq!(o.reason, TFT_BRIDGE_REASON_NOT_THE_OWNER);
    assert_eq!(
        (text(o.owner), text(o.intruder)),
        ("/a".into(), "/b".into()),
        "and it still names both publishers, which is what the close will count"
    );

    // A §5.7 static disagreement as well, so the halt has to report **both** kinds.
    offer(TFT_BRIDGE_TOPIC_TF_STATIC, "base", "lidar", 0, POSE, &a);
    let o = offer(
        TFT_BRIDGE_TOPIC_TF_STATIC,
        "base",
        "lidar",
        0,
        OTHER_POSE,
        &z,
    );
    assert_eq!(
        o.action,
        TFT_BRIDGE_STATIC_CONFLICT,
        "inside the window a STRICT static disagreement is reported and not halted          on — §5.7's own action, disposed of exactly as FIRST_WRITER_WINS would: {}",
        text(o.detail)
    );

    // **§5.4's primary close.** One call, no transform in hand, no bucket charged.
    let o = b.close_startup_window();
    assert_eq!(
        o.action,
        TFT_BRIDGE_HALT,
        "a window closing over a non-empty record must halt: {} / {}",
        o.reason,
        text(o.detail)
    );
    assert_eq!(o.reason, TFT_BRIDGE_REASON_STARTUP_CONFLICTS);
    assert_eq!(o.first_time, 1, "the transition is the loud one");
    let detail = text(o.detail);
    assert!(
        detail.contains("1 authority") && detail.contains("1 static"),
        "the close reports how many of each kind it found, or CI learns nothing \
         from it: {detail:?}"
    );
    // *"the seam's `detail` enumerates **every** recorded edge
    // with both of its publishers, not the first."* Both halves, because a report that named
    // only the authority edges would send a CI operator to fix half a misconfiguration.
    assert!(
        detail.contains("authority odom->base") && detail.contains("static base->lidar"),
        "every recorded edge must be enumerated, both kinds: {detail:?}"
    );
    // **"with both of its publishers"** taken literally: one `X vs Y` pair per recorded edge
    // and no more, so a `detail` that named an edge and only the publisher that arrived last
    // would fail here.
    assert_eq!(
        detail.matches(" vs ").count(),
        2,
        "one publisher pair per recorded edge, and exactly the recorded ones: \
         {detail:?}"
    );
    assert!(
        detail.contains("authority odom->base: /a vs /b"),
        "the authority edge names the owner and the intruder: {detail:?}"
    );
    // The static half's "owner" is the **declared constant**, not a node, because the fixture's
    // topology declares `base -> lidar` static and both offers disagreed with it.
    assert!(
        detail.contains("static base->lidar: <topology config> vs /a"),
        "the static edge names what declared the constant and who contradicted \
         it: {detail:?}"
    );
    assert_eq!(
        (text(o.parent), text(o.child)),
        (String::new(), String::new()),
        "a window-close halt is not about the transform in hand, so it names no \
         edge rather than an innocent one"
    );

    // Called twice is not an error and does not reopen anything; the second call takes the
    // already-halted path, like every other call now does.
    let again = b.close_startup_window();
    assert_eq!(again.action, TFT_BRIDGE_HALT, "the latch holds");
    assert_eq!(again.reason, TFT_BRIDGE_REASON_ALREADY_HALTED);

    for k in 0..3i64 {
        let o = offer(
            TFT_BRIDGE_TOPIC_TF,
            "odom",
            "base",
            90_000 * MS + k * MS,
            POSE,
            &a,
        );
        assert_eq!(o.action, TFT_BRIDGE_HALT, "a halt does not wear off");
        assert_eq!(o.reason, TFT_BRIDGE_REASON_ALREADY_HALTED);
        assert_eq!(o.first_time, 0, "and the replay is rate-limited");
    }
    let s = b.stats();
    assert_eq!(s.refused_after_halt, 3);
    // **Three drops for one authority collision, and the arithmetic is the point.**
    // `dropped_authority` is the ledger's only term for a static value conflict too —
    // `static_conflicts` is a side count and *not* a ledger term, so a §5.7 disagreement has to
    // be charged somewhere or `assert_balanced` breaks.
    assert_eq!(
        (s.applied, s.dropped_authority, s.static_conflicts),
        (1, 3, 2),
        "one write, three drops in one bucket, two static observations"
    );
    // **Two observations, one fault** — and the `"1 static"` assertion above is what separates
    // them.
    assert!(
        s.static_conflicts > 1,
        "the fixture must keep observations and faults apart: {}",
        s.static_conflicts
    );
    assert_eq!(
        s.transforms, offers,
        "the close charges no bucket and refuses no offer, so every offer this \
         test made is counted — the `- 1` this assertion used to carry was the \
         backstop swallowing the offer it halted on"
    );
    assert_balanced(&s);
}

/// **`TFT_BRIDGE_RECREATE` stops the bridge too, and keeps saying `RECREATE`.**
#[test]
fn a_clock_reset_under_recreate_latches_and_keeps_its_own_action() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_RECREATE,
    );
    b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 10_000 * MS, POSE, None);
    // A bag loop, as `rcl_time_jump_t` reports it: new time minus old, so a five-second rewind
    // is negative.
    let o = b.note_time_jump(-5_000 * MS, TFT_BRIDGE_JUMP_BACKWARD);
    assert_eq!(o.action, TFT_BRIDGE_RECREATE, "{}", text(o.detail));
    assert_eq!(o.delta_nanos, -5_000 * MS);
    assert_eq!(o.by_nanos, 5_000 * MS);
    assert_eq!(
        (o.clock_evidence, o.clock_evidence_detail),
        (
            TFT_BRIDGE_EVIDENCE_REPORTED,
            TFT_BRIDGE_JUMP_BACKWARD as u32
        ),
        "a reported jump keeps its evidence through RECREATE, where the \
         pipeline's own action does not carry any"
    );
    assert_eq!(o.first_time, 1, "the transition is the loud one");
    assert!(text(o.detail).contains("re-plan"));

    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 5_010 * MS, POSE, None);
    assert_eq!(
        o.action, TFT_BRIDGE_RECREATE,
        "a recreate must not degrade into a halt on the next call"
    );
    assert_eq!(o.reason, TFT_BRIDGE_REASON_ALREADY_HALTED);
    assert_eq!(o.delta_nanos, -5_000 * MS);
    assert!(
        text(o.detail).contains("re-plan"),
        "and the sentence keeps saying what to do, not \"halted\": {:?}",
        text(o.detail)
    );
    assert_balanced(&b.stats());
}

/// **A lone publisher regressing by five seconds is dropped, not promoted — however many times
/// it does it.**
#[test]
fn a_lone_publisher_regressing_is_never_promoted_however_far_it_goes() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    // A steady 100 Hz stream, stamps and receipts advancing together.
    for k in 0..5i64 {
        let o = b.offer_at(
            TFT_BRIDGE_TOPIC_TF,
            "odom",
            "base",
            10_000 * MS + k * 10 * MS,
            1_000 * MS + k * 10 * MS,
            POSE,
            None,
        );
        assert_eq!(o.action, TFT_BRIDGE_APPLIED, "{}", text(o.detail));
    }
    // It restarts and replays from five seconds ago, forever.
    for k in 0..20i64 {
        let o = b.offer_at(
            TFT_BRIDGE_TOPIC_TF,
            "odom",
            "base",
            5_000 * MS + k * 10 * MS,
            1_050 * MS + k * 10 * MS,
            POSE,
            None,
        );
        assert_eq!(
            o.action,
            TFT_BRIDGE_DROPPED,
            "sample {k}: one publisher is not the clock: {}",
            text(o.detail)
        );
        assert_eq!(o.reason, TFT_BRIDGE_REASON_NON_MONOTONIC);
        assert!(o.by_nanos > 0, "and the drop says how far");
        assert!(o.delta_nanos < 0, "and which way");
        assert_eq!(o.clock_evidence, TFT_BRIDGE_EVIDENCE_NONE);
    }
    let s = b.stats();
    assert_eq!(
        (s.dropped_non_monotonic, s.clock_resets),
        (20, 0),
        "twenty refusals and not one conclusion about the clock"
    );
    assert_balanced(&s);
}

/// **Two publishers whose offsets step by the same amount are the clock; one is not — and the
/// halt says which rung concluded it.**
#[test]
fn a_clock_reset_needs_a_second_publisher_and_reports_how_many_corroborated() {
    const TWO_PUBLISHERS: &str = r#"
[[edge]]
parent = "map"
child = "odom"
kind = "dynamic"
capacity = 256

[[edge]]
parent = "odom"
child = "base"
kind = "dynamic"
capacity = 256
"#;
    let b = Bridge::try_new(
        TWO_PUBLISHERS,
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        0,
        None,
    )
    .unwrap_or_else(|rc| panic!("tft_bridge_create: {rc} ({})", last_message()));
    let (amcl, wheels) = ([0x77u8; 16], [0x88u8; 16]);
    for (g, n) in [(&amcl, "/amcl"), (&wheels, "/wheel_driver")] {
        let name = CString::new(n).unwrap();
        assert_eq!(
            // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
            unsafe { tft_bridge_attribute(b.0, g.as_ptr(), name.as_ptr()) },
            TFT_OK
        );
    }
    // Both publishers' first sample defines their offset baseline: there is nothing yet for
    // either to have stepped away from.
    for (p, c, g) in [("map", "odom", &amcl), ("odom", "base", &wheels)] {
        let o = b.offer_at(
            TFT_BRIDGE_TOPIC_TF,
            p,
            c,
            10_000 * MS,
            1_000 * MS,
            POSE,
            Some(g),
        );
        assert_eq!(o.action, TFT_BRIDGE_APPLIED, "{}", text(o.detail));
    }

    // `/amcl` republishes from five seconds ago, 10 ms of real time later.
    let o = b.offer_at(
        TFT_BRIDGE_TOPIC_TF,
        "map",
        "odom",
        5_000 * MS,
        1_010 * MS,
        POSE,
        Some(&amcl),
    );
    assert_eq!(
        o.action,
        TFT_BRIDGE_DROPPED,
        "one publisher stepping is that publisher, not the clock: {}",
        text(o.detail)
    );
    assert_eq!(o.reason, TFT_BRIDGE_REASON_NON_MONOTONIC);
    assert_eq!(o.delta_nanos, -5_000 * MS);
    let s = b.stats();
    assert_eq!((s.dropped_non_monotonic, s.clock_resets), (1, 0));

    // The wheel driver's offset steps by the same five seconds, 10 ms later.
    let o = b.offer_at(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        5_000 * MS,
        1_020 * MS,
        POSE,
        Some(&wheels),
    );
    assert_eq!(o.action, TFT_BRIDGE_HALT, "{}", text(o.detail));
    assert_eq!(o.reason, TFT_BRIDGE_REASON_CLOCK_RESET);
    assert_eq!(
        (o.clock_evidence, o.clock_evidence_detail),
        (TFT_BRIDGE_EVIDENCE_COMMON_MODE, 2),
        "the inferred rung, and how many publishers agreed — the first thing an \
         operator needs, and a code rather than a sentence to grep"
    );
    assert_eq!(
        o.by_nanos,
        5_020 * MS,
        "the backwards distance is the magnitude of the displacement, because \
         this jump went backwards"
    );
    assert_eq!(
        o.delta_nanos,
        -5_020 * MS,
        "the step is measured against the receipt clock, so it carries the 20 ms \
         of real time that passed as well as the 5 s rewind"
    );
    assert_eq!(
        (text(o.parent), text(o.child)),
        ("odom".into(), "base".into()),
        "the outcome names the edge whose sample completed the step"
    );
    let detail = text(o.detail);
    assert!(
        detail.contains("2 publishers") && detail.contains("backwards"),
        "and the detail carries what the pair cannot: which rung concluded it, \
         and which way: {detail:?}"
    );
    let s = b.stats();
    assert_eq!((s.dropped_non_monotonic, s.clock_resets), (2, 1));
    assert_balanced(&s);
}

/// **A stop is `first_time = 1` exactly once, and the replay after it is rate-limited like
/// every other repeated outcome.**
#[test]
fn a_stop_is_announced_once_and_every_replay_after_it_is_rate_limited() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 10_000 * MS, POSE, None);
    let o = b.note_time_jump(-5_000 * MS, TFT_BRIDGE_JUMP_BACKWARD);
    assert_eq!(o.action, TFT_BRIDGE_HALT, "{}", text(o.detail));
    assert_eq!(o.reason, TFT_BRIDGE_REASON_CLOCK_RESET);
    assert_eq!(o.first_time, 1, "the transition is the loud one");
    assert_eq!(
        b.stats().clock_resets,
        1,
        "the reported jump is a promotion, which is what `clock_resets` counts"
    );
    // A jump reported twice — a bag that loops twice — replays the latch and is rate-limited
    // exactly like a repeated offer.
    let o = b.note_time_jump(-5_000 * MS, TFT_BRIDGE_JUMP_BACKWARD);
    assert_eq!(o.action, TFT_BRIDGE_HALT);
    assert_eq!(o.reason, TFT_BRIDGE_REASON_ALREADY_HALTED);
    assert_eq!(o.first_time, 0);
    for k in 0..4i64 {
        let o = b.offer(
            TFT_BRIDGE_TOPIC_TF,
            "odom",
            "base",
            5_010 * MS + k * MS,
            POSE,
            None,
        );
        assert_eq!(o.action, TFT_BRIDGE_HALT);
        assert_eq!(
            o.first_time, 0,
            "and the halt a bag loop replays 100 times a second is not"
        );
    }

    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_RECREATE,
    );
    b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 10_000 * MS, POSE, None);
    let o = b.note_time_jump(-5_000 * MS, TFT_BRIDGE_JUMP_BACKWARD);
    assert_eq!(o.action, TFT_BRIDGE_RECREATE, "{}", text(o.detail));
    assert_eq!(o.first_time, 1);
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 5_010 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_RECREATE);
    assert_eq!(o.first_time, 0);
}

/// **A reported jump charges no counter, names no edge, and refuses a code it does not know.**
#[test]
fn a_reported_jump_charges_nothing_and_names_no_edge() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 10_000 * MS, POSE, None);

    let mut out = poisoned_outcome();
    // SAFETY: live handle on its creating thread; `out` is a live local with
    // `struct_size` set.
    let rc = unsafe { tft_bridge_note_time_jump(b.0, -MS, 99, &mut out) };
    assert_eq!(rc, TFT_ERR_BAD_ENUM, "an unknown jump kind is a call fault");
    assert_eq!(
        out.action, TFT_BRIDGE_DROPPED,
        "and *out is still well-formed"
    );

    // `use_sim_time` switched at runtime: a source change, whose delta compares two different
    // time bases and is therefore not printed as a duration.
    let o = b.note_time_jump(7_000 * MS, TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED);
    assert_eq!(o.action, TFT_BRIDGE_HALT, "{}", text(o.detail));
    assert_eq!(o.reason, TFT_BRIDGE_REASON_CLOCK_RESET);
    assert_eq!(
        (text(o.parent), text(o.child)),
        (String::new(), String::new()),
        "a reported jump is not about any transform, so it names no edge rather \
         than an innocent one"
    );
    assert_eq!(
        (o.clock_evidence, o.clock_evidence_detail),
        (
            TFT_BRIDGE_EVIDENCE_REPORTED,
            TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED as u32
        ),
        "the strongest rung, and which kind of jump it was"
    );
    assert_eq!(
        o.by_nanos, 0,
        "a source change reported as a positive delta did not go backwards, so \
         the backwards distance is 0 rather than the magnitude"
    );
    assert_eq!(o.delta_nanos, 7_000 * MS);
    let detail = text(o.detail);
    assert!(
        detail.contains("time source"),
        "the strongest rung says so, so an operator knows this is a fact and not \
         an inference: {detail:?}"
    );

    let s = b.stats();
    assert_eq!(
        (s.transforms, s.refused_after_halt),
        (1, 0),
        "one offered transform, and the jump reports are not transforms"
    );
    assert_eq!(s.clock_resets, 1);
    assert_balanced(&s);

    // Again, on a stopped bridge: still no bucket moves.
    b.note_time_jump(-MS, TFT_BRIDGE_JUMP_BACKWARD);
    let s = b.stats();
    assert_eq!((s.transforms, s.refused_after_halt), (1, 0));
    assert_balanced(&s);
}

/// **A topology that declares no edges is refused at `tft_bridge_create`.**
#[test]
fn a_topology_declaring_no_edges_is_refused_rather_than_started() {
    for toml in [
        "",
        // Not merely the empty string: a config with frames and headroom but no edge is equally
        // unable to write anything, and it is what a truncated or half-written file looks like.
        "[topology]\nframes = [\"odom\", \"base\"]\nframe_headroom = 8\n",
    ] {
        let rc = Bridge::try_new(
            toml,
            TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
            TFT_BRIDGE_ON_CLOCK_RESET_HALT,
            0,
            None,
        )
        .err();
        assert_eq!(rc, Some(TFT_ERR_BAD_CONFIG), "config was {toml:?}");
        assert!(
            last_message().contains("no edges are declared"),
            "the message must say what is wrong, not just that something is: {:?}",
            last_message()
        );
    }
}

/// **A `/tf_static` value that disagrees with the config is reported with both values and names
/// the file as the incumbent** (§5.7, re-aimed by §5.8).
#[test]
fn a_static_that_disagrees_with_the_config_reports_both_values() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    // Exactly the declared constant: silent verification, and a stamp of zero as
    // `robot_state_publisher` commonly sends.
    let declared = [
        0.965_925_826_289_068_3,
        0.0,
        0.0,
        0.258_819_045_102_520_74,
        0.35,
        -0.02,
        0.61,
    ];
    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF_STATIC,
        "base",
        "lidar",
        0,
        declared,
        None,
    );
    assert_eq!(o.action, TFT_BRIDGE_STATIC_VERIFIED, "{}", text(o.detail));

    let mut moved = declared;
    moved[4] = 0.60;
    let o = b.offer(TFT_BRIDGE_TOPIC_TF_STATIC, "base", "lidar", 0, moved, None);
    assert_eq!(o.action, TFT_BRIDGE_STATIC_CONFLICT, "{}", text(o.detail));
    assert!(
        (o.existing[4] - 0.35).abs() < 1e-12,
        "existing {:?}",
        o.existing
    );
    assert!(
        (o.offered[4] - 0.60).abs() < 1e-12,
        "offered {:?}",
        o.offered
    );
    assert_eq!(text(o.owner), "<topology config>");
    assert_eq!(o.first_time, 1);

    // A static's stamp of zero must not have dragged the clock to the epoch.
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_APPLIED);
    let s = b.stats();
    assert_eq!((s.static_verified, s.static_conflicts), (1, 1));
    assert_eq!(s.clock_resets, 0);
    assert_balanced(&s);
}

/// **An edge the config does not declare is dropped, counted, and diagnosed once — naming both
/// frames** (§5.8's amendment).
#[test]
fn an_undeclared_edge_is_diagnosed_once_and_names_both_frames() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "base",
        "camera",
        1_000 * MS,
        POSE,
        None,
    );
    assert_eq!(o.action, TFT_BRIDGE_UNDECLARED);
    assert_eq!(
        (text(o.parent), text(o.child)),
        ("base".into(), "camera".into())
    );
    assert_eq!(o.first_time, 1);
    assert!(text(o.detail).contains("does not declare this edge"));

    for k in 1..8i64 {
        let o = b.offer(
            TFT_BRIDGE_TOPIC_TF,
            "base",
            "camera",
            1_000 * MS + k * MS,
            POSE,
            None,
        );
        assert_eq!(o.action, TFT_BRIDGE_UNDECLARED);
        assert_eq!(o.first_time, 0, "rate-limited after the first");
    }
    let s = b.stats();
    assert_eq!(s.dropped_undeclared, 8);
    assert_eq!(s.applied, 0);
    assert_balanced(&s);
}

/// **A stamp that goes backwards by less than the reset threshold is a drop, not a reset — and
/// the drop names the edge that stalled.**
#[test]
fn a_jittered_stamp_is_dropped_and_names_the_edge() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, None);
    // 40 ms back: interleaved publishers, not a bag loop.
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 960 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_DROPPED);
    assert_eq!(o.reason, TFT_BRIDGE_REASON_NON_MONOTONIC);
    assert_eq!(o.by_nanos, 40 * MS, "the backwards distance, positive");
    assert_eq!(o.delta_nanos, -40 * MS, "and the signed displacement");
    assert_eq!(
        (o.clock_evidence, o.clock_evidence_detail),
        (TFT_BRIDGE_EVIDENCE_NONE, 0),
        "no clock judgment was made, so the evidence fields say so rather than \
         holding whatever the last one held"
    );
    assert_eq!(
        (text(o.parent), text(o.child)),
        ("odom".into(), "base".into()),
        "a drop must still say which edge stalled"
    );
    let s = b.stats();
    assert_eq!((s.dropped_non_monotonic, s.clock_resets), (1, 0));
    assert_balanced(&s);
}

/// **`*out` is well-formed before the handle is validated.**
#[test]
fn a_bad_handle_still_leaves_a_printable_outcome() {
    let s = tft_bridge_sample {
        struct_size: core::mem::size_of::<tft_bridge_sample>() as u32,
        frame_id: ptr::null(),
        child_frame_id: ptr::null(),
        stamp_nanos: 0,
        pose: POSE,
        received_steady_nanos: 0,
    };
    let mut out = poisoned_outcome();
    // SAFETY: a NULL handle is explicitly contracted as valid input; `s` and
    // `out` are live locals with `struct_size` set.
    let rc = unsafe {
        tft_bridge_offer(
            ptr::null_mut(),
            TFT_BRIDGE_TOPIC_TF,
            &s,
            ptr::null(),
            &mut out,
        )
    };
    assert_eq!(rc, TFT_ERR_BAD_HANDLE);
    assert_eq!(out.action, TFT_BRIDGE_DROPPED);
    assert_eq!(out.reason, TFT_BRIDGE_REASON_NONE);
    assert_eq!(out.status, TFT_OK);
    for p in [out.parent, out.child, out.owner, out.intruder, out.detail] {
        assert_eq!(text(p), "", "every unset string is a printable empty one");
    }
}

/// **A `struct_size` from another build is refused, on every struct that carries one** (§3.6,
/// §6.1).
#[test]
fn a_struct_size_from_another_build_is_refused() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    let (p, c) = (CString::new("odom").unwrap(), CString::new("base").unwrap());
    let good = tft_bridge_sample {
        struct_size: core::mem::size_of::<tft_bridge_sample>() as u32,
        frame_id: p.as_ptr(),
        child_frame_id: c.as_ptr(),
        stamp_nanos: MS,
        pose: POSE,
        received_steady_nanos: 0,
    };

    let mut out = poisoned_outcome();
    out.struct_size = 8; // an outcome from a build that had fewer fields
                         // SAFETY: live handle, live sample, live `out`.
    let rc = unsafe { tft_bridge_offer(b.0, TFT_BRIDGE_TOPIC_TF, &good, ptr::null(), &mut out) };
    assert_eq!(rc, TFT_ERR_BAD_STRUCT_SIZE);

    let stale = tft_bridge_sample {
        struct_size: 8,
        ..good
    };
    let mut out = poisoned_outcome();
    // SAFETY: as above.
    let rc = unsafe { tft_bridge_offer(b.0, TFT_BRIDGE_TOPIC_TF, &stale, ptr::null(), &mut out) };
    assert_eq!(rc, TFT_ERR_BAD_STRUCT_SIZE);

    // …and an out-of-range topic is a call fault, not a sample outcome.
    let mut out = poisoned_outcome();
    // SAFETY: as above.
    let rc = unsafe { tft_bridge_offer(b.0, 99, &good, ptr::null(), &mut out) };
    assert_eq!(rc, TFT_ERR_BAD_ENUM);
    assert_eq!(
        out.action, TFT_BRIDGE_DROPPED,
        "and *out is still well-formed"
    );

    // …and a size *larger* than this build's is refused too: that is a newer caller against an
    // older library, whose extra bytes this build cannot interpret.
    let ahead = tft_bridge_sample {
        struct_size: core::mem::size_of::<tft_bridge_sample>() as u32 + 8,
        ..good
    };
    let mut out = poisoned_outcome();
    // SAFETY: as above. The declared size overstates the struct, which is
    // exactly what must be refused *before* anything reads that far.
    let rc = unsafe { tft_bridge_offer(b.0, TFT_BRIDGE_TOPIC_TF, &ahead, ptr::null(), &mut out) };
    assert_eq!(rc, TFT_ERR_BAD_STRUCT_SIZE);

    assert_eq!(
        b.stats().transforms,
        0,
        "no malformed call reached the pipeline"
    );
}

/// **A caller built before `received_steady_nanos` existed still works** — §3.6's append rule.
#[test]
fn a_sample_from_before_the_receipt_clock_is_read_as_a_prefix() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    let (p, c) = (CString::new("odom").unwrap(), CString::new("base").unwrap());
    // The size a caller compiled against ABI 0.1 sends.
    let v1_size = core::mem::offset_of!(tft_bridge_sample, received_steady_nanos);
    assert!(v1_size < core::mem::size_of::<tft_bridge_sample>());

    // **Allocated as exactly `v1_size` bytes**, so a read past the prefix is a genuine heap
    // overrun a sanitizer can see, rather than a read into the tail of a full-size struct that
    // happens to be there.
    let mut short = vec![0u8; v1_size];
    {
        let full = tft_bridge_sample {
            struct_size: v1_size as u32,
            frame_id: p.as_ptr(),
            child_frame_id: c.as_ptr(),
            stamp_nanos: 1_000 * MS,
            pose: POSE,
            received_steady_nanos: 0,
        };
        // SAFETY: `full` is a live `tft_bridge_sample` and `short` has exactly
        // `v1_size` bytes, which is less than its size — a prefix copy.
        unsafe {
            ptr::copy_nonoverlapping(
                ptr::addr_of!(full).cast::<u8>(),
                short.as_mut_ptr(),
                v1_size,
            );
        }
    }

    let mut out = poisoned_outcome();
    // SAFETY: live handle on its creating thread; `short` holds `v1_size`
    // readable bytes and declares that size, which is what the ABI contracts;
    // `out` is a live local with `struct_size` set.
    let rc = unsafe {
        tft_bridge_offer(
            b.0,
            TFT_BRIDGE_TOPIC_TF,
            short.as_ptr().cast::<tft_bridge_sample>(),
            ptr::null(),
            &mut out,
        )
    };
    assert_eq!(
        rc,
        TFT_OK,
        "an appended field must not lock an older caller out: {}",
        last_message()
    );
    assert_eq!(out.action, TFT_BRIDGE_APPLIED, "{}", text(out.detail));
    assert_eq!(
        (text(out.parent), text(out.child)),
        ("odom".into(), "base".into()),
        "and every field the prefix does carry survived the bounded copy"
    );
    let got = b
        .tree()
        .at("odom", "base", 1_000 * MS)
        .expect("a prefix sample is written like any other");
    assert!(
        (got[4] - POSE[4]).abs() < 1e-12,
        "the pose came through the prefix intact: {got:?}"
    );
    assert_balanced(&b.stats());
}

/// **A caller built before `arena_name` existed still gets a bridge, and a heap arena** —
/// `docs/decisions/0015` step 1, and the same §3.6 append rule one struct over.
#[test]
fn an_options_struct_from_before_the_arena_name_is_read_as_a_prefix() {
    let toml = CString::new(TOPO).unwrap();
    let prefix = CString::new("robot1").unwrap();
    // Computed, never a literal: `offset_of!` of the appended field is where the old struct
    // ended, on whatever pointer width this build has.
    let v1_size = core::mem::offset_of!(tft_bridge_options, arena_name);
    assert!(v1_size < core::mem::size_of::<tft_bridge_options>());

    let mut short = vec![0u8; v1_size];
    {
        let full = tft_bridge_options {
            struct_size: v1_size as u32,
            authority: TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
            on_clock_reset: TFT_BRIDGE_ON_CLOCK_RESET_HALT,
            domain: 0,
            tf_prefix: prefix.as_ptr(),
            arena_name: ptr::null(),
        };
        // SAFETY: `full` is a live `tft_bridge_options` and `short` has exactly
        // `v1_size` bytes, which is less than its size — a prefix copy.
        unsafe {
            ptr::copy_nonoverlapping(
                ptr::addr_of!(full).cast::<u8>(),
                short.as_mut_ptr(),
                v1_size,
            );
        }
    }

    let mut raw: *mut tft_bridge = ptr::null_mut();
    // SAFETY: NUL-terminated config; `short` holds `v1_size` readable bytes and
    // declares that size, which is what the ABI contracts; `raw` a live local.
    let rc = unsafe {
        tft_bridge_create(
            toml.as_ptr(),
            short.as_ptr().cast::<tft_bridge_options>(),
            &mut raw,
        )
    };
    assert_eq!(
        rc,
        TFT_OK,
        "an appended field must not lock an older caller out: {}",
        last_message()
    );
    let b = Bridge(raw);

    assert!(
        b.remaps()
            .iter()
            .any(|(from, to)| from == "odom" && to == "robot1/odom"),
        "the prefix's last field must arrive at the old layout's offset, not be \
         read from the appended one: {:?}",
        b.remaps()
    );

    // **And it is a heap arena.** `arena_name` is the one field the copy leaves untouched, and
    // the zero it is left at is NULL — the documented "private heap arena, as before".
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_APPLIED, "{}", text(o.detail));
    let got = b
        .tree()
        .at("robot1/odom", "robot1/base", 1_000 * MS)
        .expect("a bridge built from a prefix options struct writes like any other");
    assert!(
        (got[4] - POSE[4]).abs() < 1e-12,
        "the pose came through: {got:?}"
    );
}

/// **An options `struct_size` belonging to neither build is still refused.**
#[test]
fn an_options_size_from_neither_build_is_refused() {
    let toml = CString::new(TOPO).unwrap();
    let current = core::mem::size_of::<tft_bridge_options>();
    let v1_size = core::mem::offset_of!(tft_bridge_options, arena_name);
    let template = tft_bridge_options {
        struct_size: current as u32,
        authority: TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        on_clock_reset: TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        domain: 0,
        tf_prefix: ptr::null(),
        arena_name: ptr::null(),
    };

    // Strictly between the two known layouts, so it is neither.
    assert!(v1_size + 1 < current, "the append left room to be wrong in");
    let between = tft_bridge_options {
        struct_size: (v1_size + 1) as u32,
        ..template
    };
    let mut b: *mut tft_bridge = ptr::null_mut();
    // SAFETY: NUL-terminated config; `between` is a live full-size struct, so
    // the declared size understates it and nothing can be read out of bounds.
    let rc = unsafe { tft_bridge_create(toml.as_ptr(), &between, &mut b) };
    assert_eq!(
        rc, TFT_ERR_BAD_STRUCT_SIZE,
        "a size between the two known layouts is not a layout"
    );
    assert!(b.is_null(), "a failed create must not hand out a handle");

    // Larger than this build's: a newer caller against an older library.
    let ahead = tft_bridge_options {
        struct_size: (current + 8) as u32,
        ..template
    };
    let mut b: *mut tft_bridge = ptr::null_mut();
    // SAFETY: as above. The declared size overstates the struct, which is
    // exactly what must be refused *before* anything reads that far.
    let rc = unsafe { tft_bridge_create(toml.as_ptr(), &ahead, &mut b) };
    assert_eq!(rc, TFT_ERR_BAD_STRUCT_SIZE);
    assert!(b.is_null(), "a failed create must not hand out a handle");

    // Zero, which is what an uninitialised `opts` most often holds.
    let zero = tft_bridge_options {
        struct_size: 0,
        ..template
    };
    let mut b: *mut tft_bridge = ptr::null_mut();
    // SAFETY: as above.
    let rc = unsafe { tft_bridge_create(toml.as_ptr(), &zero, &mut b) };
    assert_eq!(rc, TFT_ERR_BAD_STRUCT_SIZE);
    assert!(b.is_null(), "a failed create must not hand out a handle");
}

/// **A `bridge`-without-`shm` build refuses a shared arena rather than ignoring it** —
/// `docs/decisions/0015` *Failure*, the silent downgrade in its other costume.
#[cfg(not(all(feature = "shm", target_os = "linux")))]
#[test]
fn a_shared_arena_without_the_shm_feature_is_refused() {
    let toml = CString::new(TOPO).unwrap();
    let name = CString::new("bridge-without-shm").unwrap();
    let opts = tft_bridge_options {
        struct_size: core::mem::size_of::<tft_bridge_options>() as u32,
        authority: TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        on_clock_reset: TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        domain: 0,
        tf_prefix: ptr::null(),
        arena_name: name.as_ptr(),
    };
    let mut b: *mut tft_bridge = ptr::null_mut();
    // SAFETY: NUL-terminated config and name, a live full-size `opts`, `b` a
    // live local.
    let rc = unsafe { tft_bridge_create(toml.as_ptr(), &opts, &mut b) };
    assert_eq!(
        rc, TFT_ERR_ARENA_UNAVAILABLE,
        "a shared arena with no shm behind it must refuse, not downgrade"
    );
    assert!(
        b.is_null(),
        "and it must not hand out a heap bridge instead"
    );
    let msg = last_message();
    assert!(
        msg.contains("shm"),
        "the message must name the missing feature: {msg}"
    );
    assert!(
        msg.contains("--features bridge,shm"),
        "and the rebuild command: {msg}"
    );
}

/// **A declared dynamic edge whose domain is not the bridge's is refused at startup** — §5.5,
/// NORMATIVE, *"and fails at startup rather than at first message"*.
#[test]
fn a_domain_the_bridge_does_not_run_in_is_refused_at_creation() {
    const SIM: &str = r#"
[[edge]]
parent = "odom"
child = "base"
kind = "dynamic"
capacity = 64
domain = 1
"#;
    // The bridge runs in domain 1 (`use_sim_time`): fine.
    Bridge::try_new(
        SIM,
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        1,
        None,
    )
    .expect("a matching domain must build");

    // The same file against a bridge running in domain 0: refused, at startup.
    let rc = Bridge::try_new(
        SIM,
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        0,
        None,
    )
    .expect_err("a cross-domain arena must not be constructible");
    assert_eq!(rc, TFT_ERR_TIME_DOMAIN);
    let msg = last_message();
    assert!(
        msg.contains("base") || msg.contains("odom"),
        "the diagnostic must name the offending edge, not just the mismatch: {msg:?}"
    );
}

/// **A config that does not describe a tree is refused, and says so in terms of the file**
/// rather than of an arena that was never built.
#[test]
fn a_cyclic_topology_is_refused_in_the_files_own_terms() {
    const CYCLE: &str = r#"
[[edge]]
parent = "a"
child = "b"
kind = "dynamic"
capacity = 16

[[edge]]
parent = "b"
child = "a"
kind = "dynamic"
capacity = 16
"#;
    let rc = Bridge::try_new(
        CYCLE,
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        0,
        None,
    )
    .expect_err("a cycle is not a tree");
    assert_eq!(rc, TFT_ERR_BAD_CONFIG);
    let msg = last_message();
    assert!(msg.contains("cycle"), "message was {msg:?}");
    assert!(
        msg.contains('a') || msg.contains('b'),
        "and it names a frame: {msg:?}"
    );
}

/// **The message and queue-depth counters are what §5.9 asks for**: a mark that only rises, and
/// a capacity to read it against.
#[test]
fn the_queue_high_water_mark_survives_the_boundary() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    for d in [3u32, 100, 0] {
        // SAFETY: live handle on its creating thread.
        assert_eq!(unsafe { tft_bridge_note_queue_depth(b.0, d) }, TFT_OK);
    }
    for _ in 0..4 {
        // SAFETY: live handle on its creating thread.
        assert_eq!(unsafe { tft_bridge_note_message(b.0) }, TFT_OK);
    }
    let s = b.stats();
    assert_eq!(s.queue_high_water, 100);
    assert_eq!(s.queue_capacity, 100, "§5.2's KeepLast(100)");
    assert_eq!(s.messages, 4);
    assert_eq!(s.transforms, 0, "a message is not a transform");
}

/// **The tree handle outlives the bridge**, so a reader thread cannot be dangled by the
/// executor thread freeing its bridge.
#[test]
fn the_tree_handle_outlives_the_bridge_that_made_it() {
    let tree = {
        let b = Bridge::new(
            TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
            TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        );
        b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, None);
        b.tree()
    };
    let got = tree
        .at("odom", "base", 1_000 * MS)
        .expect("the arena outlives the bridge handle");
    assert!((got[4] - POSE[4]).abs() < 1e-12, "{got:?}");
}

/// **`tf_prefix` rewrites the declared topology, not only the wire** (§5.6).
#[test]
fn a_tf_prefix_rewrites_the_declared_topology_and_the_arena_with_it() {
    let b = Bridge::try_new(
        TOPO,
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        0,
        Some("robot1"),
    )
    .expect("a prefixed bridge must build");

    // The wire carries the robot's own names, exactly as `--discover` wrote them into the
    // config.
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, None);
    assert_eq!(
        o.action,
        TFT_BRIDGE_APPLIED,
        "a prefixed bridge must still recognise its own declared edge: reason {} / {}",
        o.reason,
        text(o.detail)
    );
    assert_eq!(
        (text(o.parent), text(o.child)),
        ("robot1/odom".into(), "robot1/base".into()),
        "and it reports the names the arena knows"
    );

    // The arena is the prefixed one, so a consumer looks up the prefixed names — and the raw
    // ones are not frames at all.
    let tree = b.tree();
    let got = tree
        .at("robot1/odom", "robot1/base", 1_000 * MS)
        .expect("the prefixed edge is what was written");
    assert!((got[4] - POSE[4]).abs() < 1e-12, "{got:?}");

    let s = b.stats();
    assert_eq!((s.applied, s.dropped_undeclared), (1, 0));
    assert_balanced(&s);
}

/// **§5.6's remap table is readable from C, and complete before the first message.**
#[test]
fn the_remap_table_crosses_the_boundary_and_is_complete_at_startup() {
    let b = Bridge::try_new(
        TOPO,
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        0,
        Some("robot1"),
    )
    .expect("a prefixed bridge must build");

    // Not one message has been offered.
    assert_eq!(
        b.remaps(),
        vec![
            ("odom".to_string(), "robot1/odom".to_string()),
            ("base".to_string(), "robot1/base".to_string()),
            ("lidar".to_string(), "robot1/lidar".to_string()),
        ],
        "every declared frame, in file order, before any traffic"
    );

    // A frame the config never declared still earns a row when it is first seen, because that
    // is the remap an operator has no other way to learn about.
    b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "/camera_mount",
        "camera",
        1_000 * MS,
        POSE,
        None,
    );
    let rows = b.remaps();
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert_eq!(
        rows[3],
        (
            "/camera_mount".to_string(),
            "robot1/camera_mount".to_string()
        ),
        "the leading slash is stripped and the prefix applied"
    );

    // A bridge with nothing to remap has an empty table, and the first read is the loop's
    // termination condition rather than a fault.
    let plain = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    assert!(plain.remaps().is_empty());
}

/// **A `/tf` message for an edge the config declared static is a kind change** (§5.7: *"the
/// edge kind cannot change"*), and the drop names the edge.
#[test]
fn a_static_edge_offered_on_slash_tf_is_a_kind_change() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "base", "lidar", 1_000 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_DROPPED, "{}", text(o.detail));
    assert_eq!(o.reason, TFT_BRIDGE_REASON_KIND_CHANGE);
    assert_eq!(
        (text(o.parent), text(o.child)),
        ("base".into(), "lidar".into()),
        "a kind change must say which edge"
    );
    let s = b.stats();
    assert_eq!((s.dropped_kind_change, s.applied), (1, 0));
    assert_balanced(&s);
}

/// **`TFT_BRIDGE_AUTHORITY_LAST_WRITER_WINS` decodes to the policy it names** (§5.4).
#[test]
fn last_writer_wins_hands_the_edge_to_the_newcomer() {
    let b = Bridge::try_new(
        TOPO,
        TFT_BRIDGE_AUTHORITY_LAST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        0,
        None,
    )
    .expect("last-writer-wins is a supported policy");
    let (first, second) = ([0x77u8; 16], [0x88u8; 16]);
    for (g, n) in [(&first, "/a"), (&second, "/b")] {
        let name = CString::new(n).unwrap();
        assert_eq!(
            // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
            unsafe { tft_bridge_attribute(b.0, g.as_ptr(), name.as_ptr()) },
            TFT_OK
        );
    }
    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_000 * MS,
        POSE,
        Some(&first),
    );
    assert_eq!(o.action, TFT_BRIDGE_APPLIED);

    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_010 * MS,
        POSE,
        Some(&second),
    );
    assert_eq!(
        o.action,
        TFT_BRIDGE_APPLIED,
        "the second publisher reclaims the edge: reason {} / {}",
        o.reason,
        text(o.detail)
    );
    let s = b.stats();
    assert_eq!(
        (s.applied, s.dropped_authority),
        (2, 0),
        "reclaiming is not a conflict"
    );
    assert_balanced(&s);
}

/// **A bridge freed from the wrong thread is refused, not freed** (§3.2).
// Miri cannot spawn a process, and there is no way to observe an `abort()` from inside the
// process performing it.
#[cfg_attr(miri, ignore = "needs a subprocess to observe abort()")]
#[test]
fn a_bridge_refuses_to_be_freed_from_the_wrong_thread() {
    use std::process::Command;
    if std::env::var_os("TFT_BRIDGE_FREE_CHILD").is_some() {
        return; // the child arm is `bridge_free_cross_thread_child`
    }
    let exe = std::env::current_exe().expect("test binary path");
    let out = Command::new(exe)
        .args(["--exact", "bridge_free_cross_thread_child", "--nocapture"])
        .env("TFT_BRIDGE_FREE_CHILD", "1")
        .output()
        .expect("re-invoke the test binary");

    if cfg!(debug_assertions) {
        use std::os::unix::process::ExitStatusExt;
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.signal(),
            Some(6),
            "debug builds must abort (SIGABRT) on a cross-thread free; got {:?}\n{err}",
            out.status
        );
        assert!(
            err.contains("tft_bridge is Send but not Sync"),
            "the abort must name the handle that moved: {err}"
        );
    } else {
        assert!(
            out.status.success(),
            "release builds must refuse and return, not abort: {:?}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("BRIDGE FREE REFUSED OK"),
            "the child must observe TFT_ERR_WRONG_THREAD and a surviving bridge: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

/// The child arm of [`a_bridge_refuses_to_be_freed_from_the_wrong_thread`]. Inert unless
/// `TFT_BRIDGE_FREE_CHILD` is set, so a normal run does not abort itself.
#[allow(clippy::print_stdout)]
#[test]
fn bridge_free_cross_thread_child() {
    if std::env::var_os("TFT_BRIDGE_FREE_CHILD").is_none() {
        return;
    }
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    // A raw handle is not `Send`; the ABI's own rule is what is under test.
    let raw = b.0 as usize;
    let saw = std::thread::spawn(move || {
        let h = raw as *mut tft_bridge;
        // SAFETY: `h` is a live handle. Using it from this thread is exactly the
        // misuse under test, and the ABI contracts that it is detected rather
        // than followed.
        unsafe { tft_bridge_free(h) };
        // `tft_error` is thread-local, so the refusal has to be read here.
        last_message()
    })
    .join()
    .expect("the child thread must not panic");

    assert!(
        saw.contains("tft_bridge is Send but not Sync"),
        "the refusal must name the handle: {saw:?}"
    );
    // The handle survived: the claims were not released by a thread that never held them, and
    // the bridge still writes.
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_APPLIED, "{}", text(o.detail));
    println!("BRIDGE FREE REFUSED OK");
}
