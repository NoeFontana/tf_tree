//! The ingest-bridge seam of the C ABI — `docs/PHASE4.md` §5 and §6.3.
//!
//! `abi.rs` covers misuse with no handle, `live.rs` the read path, `publish.rs`
//! the write path. This covers the seam an `rclcpp` node calls, which is the
//! only one that both decides *and* writes: everything §5 judges about somebody
//! else's misconfigured robot is reachable from C through exactly these nine
//! entry points, and if a judgment cannot be printed by a C caller it may as
//! well not have been made.
//!
//! **Every test here drives the `extern "C"` functions, not the Rust behind
//! them.** `tf_tree_bridge`'s own tests already cover the pipeline; what is
//! untested without this file is the boundary — the POD outcome, the borrowed
//! `const char *` lifetimes, the arena write, and the counters the C layer adds
//! to the pipeline's own.
#![cfg(feature = "bridge")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::ffi::c_char;
use core::ptr;
use std::ffi::{CStr, CString};

use tf_tree_c::bridge::*;
use tf_tree_c::*;

/// One dynamic edge and one static one — the smallest topology that still
/// exercises both `/tf` and `/tf_static`, in the real config format so these
/// tests go through the parser an operator will use.
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

/// A 30° yaw with a translation nothing else in the fixture shares, so a
/// read-back that returns identity — or the static edge's pose — fails rather
/// than coincidentally passing.
const POSE: [f64; 7] = [
    0.965_925_826_289_068_3,
    0.0,
    0.0,
    0.258_819_045_102_520_74,
    1.5,
    -2.25,
    0.75,
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
            // A private heap arena, which is what this whole file tests. The
            // shared path needs `--features shm` and lives in
            // `tests/bridge_shared.rs`.
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

    /// Offer one transform with **no receipt clock**, which is what a caller
    /// that has none supplies.
    ///
    /// Deliberately the default for this file: §5.5's offset layer is then
    /// absent, so every fixture that is about names, authority, statics or the
    /// arena keeps testing exactly what it used to. The handful that are about
    /// the clock say so by calling [`Bridge::offer_at`].
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

    /// Offer one transform and return the outcome, checking the call itself was
    /// well-formed. The `CString`s outlive the call, which is all the ABI asks.
    ///
    /// `received` is the local steady clock's reading for the message this
    /// transform came in — the reference §5.5 measures each publisher's stamp
    /// against, and never derived from a stamp.
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

    /// Report a jump the time source itself announced — §5.5's authoritative
    /// rung, with no transform in hand.
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
            ..zeroed_stats()
        };
        // SAFETY: live handle on its creating thread; `s` is a live local with
        // `struct_size` set.
        assert_eq!(unsafe { tft_bridge_get_stats(self.0, &mut s) }, TFT_OK);
        s
    }

    /// §5.6's remap table, walked exactly as the doc comment's C loop walks it:
    /// row by row until `TFT_ERR_NO_DATA`.
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
///
/// The ABI promises `*out` is filled with a well-formed "nothing happened"
/// before anything can fail. A zeroed struct would let that promise pass by
/// accident — `TFT_BRIDGE_APPLIED` and `TFT_BRIDGE_REASON_NONE` are both 0 —
/// and a NULL string pointer would read as "empty" to a lenient test while
/// crashing a C caller that printed it.
fn poisoned_outcome() -> tft_bridge_outcome {
    // SAFETY: `tft_bridge_outcome` is `#[repr(C)]`, `Copy`, and made of
    // integers, `f64`s and raw pointers — every bit pattern is a valid value of
    // each. It is never *read* through until the ABI has written it.
    let mut o: tft_bridge_outcome = unsafe { core::mem::transmute([0xAAu8; SIZEOF_OUTCOME]) };
    o.struct_size = SIZEOF_OUTCOME as u32;
    o
}

const SIZEOF_OUTCOME: usize = core::mem::size_of::<tft_bridge_outcome>();

fn zeroed_stats() -> tft_bridge_stats {
    // SAFETY: `tft_bridge_stats` is `#[repr(C)]` and made of integers, for
    // which all-zero is a valid value.
    unsafe { core::mem::zeroed() }
}

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

// `c_char` is `i8` on x86_64 and `u8` on aarch64, so this cast is necessary
// on one target and a no-op on the other; see `src/error.rs` for the full
// note. The allow is the fix — deleting the cast breaks x86_64.
#[allow(clippy::unnecessary_cast)]
fn last_message() -> String {
    let mut e = tft_error {
        struct_size: core::mem::size_of::<tft_error>() as u32,
        ..unsafe { core::mem::zeroed() }
    };
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

// ---------------------------------------------------------------------------

/// **The seam writes the arena, and the arena is readable through the handle it
/// hands back.** Both halves of `docs/PHASE4.md` §5 in one call: the pipeline
/// decides, and Rust — not the C++ node — performs the write.
///
/// Mutant: make `write_sample` return `TFT_OK` without calling `w.push` ⇒ the
/// outcome is still `TFT_BRIDGE_APPLIED`, so the first assertion survives and
/// the read-back is what fails, with `TFT_ERR_NO_DATA`.
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
///
/// A publisher whose first message is garbage must not take ownership of the
/// edge under `FirstWriterWins`, because the ownership is for the life of the
/// arena and the *correct* publisher would be locked out of it by one bad
/// message.
///
/// Mutant: move the `layout::from_wxyz_pose` check below `inner.ingest.offer`
/// ⇒ `/rogue` owns `odom -> base`, the `/ekf` offer comes back
/// `TFT_BRIDGE_DROPPED` with `TFT_BRIDGE_REASON_NOT_THE_OWNER`, and this fails.
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
        // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
        assert_eq!(
            unsafe { tft_bridge_attribute(b.0, g.as_ptr(), name.as_ptr()) },
            TFT_OK
        );
    }

    // A quaternion of norm 2 — a plausible mistake (an unnormalized message),
    // not a wild value, so `NotAUnitQuaternion` rather than `NotFinite` is what
    // catches it.
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

/// **§5.4's headline diagnostic survives the C boundary: both nodes, the edge,
/// and a rate-limit flag.**
///
/// This is the sentence §5.4 calls the better sales pitch — *"your `/ekf` and
/// `/odom_node` have both been publishing `odom -> base_link`"* — and it is
/// only printable if all four pieces reach C. It was not: the pipeline
/// collapsed `Verdict::Reject`, which carries all of them, into an
/// `Action::Drop { reason }` that carries none.
///
/// Mutant: drop `o.first_time = u8::from(*first_time)` from the
/// `AuthorityConflict` arm ⇒ the second offer's `first_time` is 0 like the
/// first, and a 1 kHz intruder becomes silent instead of rate-limited, so the
/// `first_time == 1` assertion fails.
#[test]
fn an_authority_conflict_names_both_publishers_and_the_edge() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    let (ekf, odom_node) = ([0x33u8; 16], [0x44u8; 16]);
    for (g, n) in [(&ekf, "/ekf"), (&odom_node, "/odom_node")] {
        let name = CString::new(n).unwrap();
        // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
        assert_eq!(
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
///
/// This is the regression test for the defect `crates/tf_tree_bench`'s DDS
/// comparison found. `rmw_fastrtps` reports `_NODE_NAME_UNKNOWN_` for an
/// endpoint discovered before its participant's node information arrives and
/// corrects it on a later graph walk, so the *same* GID is attributed twice with
/// two different names. When identity was the name, `FirstWriterWins` gave the
/// edge to the placeholder and then rejected the real publisher forever:
/// measured at 9 864 of 10 070 transforms dropped, against one correctly
/// configured publisher, with 100 % of consumer lookups failing.
///
/// Mutant: make `tft_bridge_attribute` `insert` a fresh `Publisher` keyed on the
/// name instead of mutating the entry's name ⇒ the second offer is
/// `TFT_BRIDGE_DROPPED` / `NOT_THE_OWNER` and this fails on the first assert.
#[test]
fn a_publisher_renamed_by_a_later_graph_walk_keeps_its_edge() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    let gid = [0x55u8; 16];

    // The graph's first answer: an endpoint it can see but cannot yet name.
    let placeholder = CString::new("/_NODE_NAMESPACE_UNKNOWN_/_NODE_NAME_UNKNOWN_").unwrap();
    // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
    assert_eq!(
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
    // SAFETY: as above.
    assert_eq!(
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
///
/// The other half of the same defect, and the one `docs/PHASE4.md` §5.3's
/// amendment already named: `Publisher::UnknownGid` was a *unit* variant, so on
/// a walk that resolved no names every publisher compared equal and §5.4's
/// conflict detection was silently off — in exactly the deployment least able to
/// diagnose it. A GID with no name is now a distinct identity.
///
/// Note what this does **not** change: a publisher with no GID *at all* is still
/// `Publisher::Unattributed`, a unit variant, because `0012`'s ladder requires
/// that less attribution mean less detection and never more stopping. That case
/// is `an_unreported_gid_degrades_rather_than_failing` below.
///
/// Mutant: have `publisher_of` return one shared sentinel for an uncached GID
/// instead of populating the cache ⇒ both offers are `TFT_BRIDGE_APPLIED` and
/// the conflict assertion fails.
#[test]
fn two_unnamed_publishers_on_one_edge_still_conflict() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    // Neither GID is ever passed to `tft_bridge_attribute`, so neither has a
    // name — the state an RMW without endpoint introspection leaves.
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
    // And the diagnostic must be able to tell them apart, or it says two
    // identical things are fighting.
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
///
/// A GID of all zeroes is what an RMW that reports none leaves behind, so it
/// must mean "nothing was told to us" rather than "publisher number zero" —
/// otherwise every unattributed sample on the bus would be attributed to one
/// imaginary node and `FirstWriterWins` would hand it every edge.
///
/// Mutant: delete the `key == [0u8; 16]` early return in `publisher_of` ⇒ the
/// zero GID misses the cache and resolves to `<unknown publisher>`, which under
/// `Strict` is a second publisher on the edge, so the sample is dropped as
/// `NOT_THE_OWNER` and the `TFT_BRIDGE_APPLIED` assertion fails.
#[test]
fn an_unreported_gid_degrades_rather_than_failing() {
    let b = Bridge::new(TFT_BRIDGE_AUTHORITY_STRICT, TFT_BRIDGE_ON_CLOCK_RESET_HALT);
    // No GID at all.
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_APPLIED);

    // An all-zero GID is the same publisher as no GID, so `Strict` — which
    // records a conflict on the *second* distinct publisher of an edge, and
    // halts once at its startup window's close if it recorded any — must find
    // nothing to record.
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

/// **`STRICT` halts when its startup window closes, not on the message that
/// collided — and a halted bridge then refuses everything, with the ledger still
/// balancing.**
///
/// §5.5 says the bridge *stops*. This ABI cannot exit somebody else's process,
/// so stopping means latching: a caller that logs the halt and keeps offering
/// would push exactly the stamps §5.5 exists to prevent. That half is unchanged.
///
/// What `docs/decisions/0011` changed is the **trigger**. `STRICT` used to
/// return `Fatal` on the second message that collided, which is neither §5.4's
/// *"refuse to start if a conflict is detected within a startup window"* nor of
/// any use to the CI the policy exists for: a deployment with four misconfigured
/// publishers took four boots to diagnose, each reporting one of them. Now the
/// collision is dropped and counted exactly as `FIRST_WRITER_WINS` would,
/// conflicts accumulate while the window is open, and **one** halt at its close
/// reports how many of each kind were found.
///
/// This fixture drives the window's **backstop** — 4096 transforms, a private
/// constant of `tf_tree_bridge` — because that is the only close a C caller can
/// reach today: `Ingest::close_startup_window` has no ABI entry point yet
/// (`docs/decisions/0011`'s implementation step 6). Hence the loop rather than a
/// third offer.
///
/// Mutant: delete `inner.stopped = Some(…)` from the `Action::Halt` arm ⇒ the
/// offers after the halt are processed and the `TFT_BRIDGE_HALT` assertion in
/// the replay loop fails. Mutant: drop `+ inner.refused_after_halt` from
/// `transforms` in `tft_bridge_get_stats` ⇒ the offered-transform assertion
/// fails first (4096 against 4099), and `assert_balanced` would too, short by 3.
/// Mutant: restore the unconditional `set(&mut inner.strings.detail, "the bridge
/// halted; …")` after the `Action::Halt` match — the shape this arm had before,
/// and the one the halt's numbers cannot survive ⇒ the `"1 authority"`
/// assertion fails on an outcome that says only that something halted. Mutant:
/// call `name_the_edge(inner, o)` in the `StartupConflicts` arm ⇒ the halt names
/// whichever edge happened to be next on the wire as its cause and the
/// empty-name assertion fails.
#[test]
fn a_halted_bridge_refuses_every_later_offer() {
    let b = Bridge::new(TFT_BRIDGE_AUTHORITY_STRICT, TFT_BRIDGE_ON_CLOCK_RESET_HALT);
    let (a, z) = ([0x55u8; 16], [0x66u8; 16]);
    for (g, n) in [(&a, "/a"), (&z, "/b")] {
        let name = CString::new(n).unwrap();
        // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
        assert_eq!(
            unsafe { tft_bridge_attribute(b.0, g.as_ptr(), name.as_ptr()) },
            TFT_OK
        );
    }
    // Every `tft_bridge_offer` call this test makes, counted, because the
    // ledger assertion at the end is about one specific offer that is
    // deliberately **not** counted by the bridge.
    let mut offers = 0u64;
    let mut offer = |stamp: i64, gid: &[u8; 16]| {
        offers += 1;
        b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", stamp, POSE, Some(gid))
    };

    offer(1_000 * MS, &a);
    let o = offer(1_010 * MS, &z);
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

    // The backstop. Every offer until it fires is the owner's and is written;
    // the bound is generous so that a changed constant fails the `expect` below
    // rather than silently passing on a rewritten rule.
    let mut halt = None;
    for k in 0..16_384i64 {
        let o = offer(2_000 * MS + k * MS, &a);
        if o.action == TFT_BRIDGE_HALT {
            halt = Some(o);
            break;
        }
        assert_eq!(
            o.action,
            TFT_BRIDGE_APPLIED,
            "the owner keeps working while the window is open: {} / {}",
            o.reason,
            text(o.detail)
        );
    }
    let o = halt.expect("the startup window's backstop must close it and halt");
    assert_eq!(o.reason, TFT_BRIDGE_REASON_AUTHORITY_CONFLICT);
    assert_eq!(o.first_time, 1, "the transition is the loud one");
    let detail = text(o.detail);
    assert!(
        detail.contains("1 authority") && detail.contains("0 static"),
        "the close reports how many of each kind it found, or CI learns nothing \
         from it: {detail:?}"
    );
    assert_eq!(
        (text(o.parent), text(o.child)),
        (String::new(), String::new()),
        "a window-close halt is not about the transform in hand, so it names no \
         edge rather than an innocent one"
    );

    for k in 0..3i64 {
        let o = offer(90_000 * MS + k * MS, &a);
        assert_eq!(o.action, TFT_BRIDGE_HALT, "a halt does not wear off");
        assert_eq!(o.reason, TFT_BRIDGE_REASON_ALREADY_HALTED);
        assert_eq!(o.first_time, 0, "and the replay is rate-limited");
    }
    let s = b.stats();
    assert_eq!(s.refused_after_halt, 3);
    assert_eq!(
        s.dropped_authority, 1,
        "the collision was counted once, by the message that made it"
    );
    assert_eq!(
        s.transforms,
        offers - 1,
        "a window-close halt is caused by transforms already counted, so it \
         charges no bucket and is not itself an offered transform"
    );
    assert_balanced(&s);
}

/// **`TFT_BRIDGE_RECREATE` stops the bridge too, and keeps saying `RECREATE`.**
///
/// §5.5's `recreate` builds a fresh arena; this ABI will not, because every
/// plan the node compiled points into the current one. So the only correct
/// continuation is that the caller tears the bridge down — and the pipeline has
/// *already forgotten* every edge's high-water mark on this path, so an
/// unlatched bridge would approve every subsequent sample and let the arena
/// refuse them one at a time as non-monotonic: a bag loop turning into a silent
/// permanent stall.
///
/// **The stop arrives through [`tft_bridge_note_time_jump`], and it has to.**
/// This fixture is one publisher on one dynamic edge, and under §5.5's ladder a
/// single source *never* promotes its own regression, however far back it goes
/// — that is the whole correction: a lone node restarting is observationally
/// identical to a bag loop, and the previous rule's floor turned it into a
/// latched bridge on a healthy robot. What a lone bag loop really has is the
/// authoritative signal, because `rcl` reports the `/clock` rewind to the node
/// directly. So the fixture uses it, and the latch is exercised through the
/// entry point a real replay deployment would use.
///
/// Mutant: delete `inner.stopped = Some(…)` from the `RecreateArena` arm ⇒ the
/// next offer comes back **`TFT_BRIDGE_REJECTED`** (*"left: 7, right: 6"*), not
/// `APPLIED` — which is the failure mode this test's second paragraph describes,
/// caught in the act: the pipeline waved the sample through because the recreate
/// had rewound every high-water mark, and the arena refused it as
/// non-monotonic. One `rejected_by_arena` per sample, forever, instead of one
/// loud outcome. Mutant: latch it with `action: TFT_BRIDGE_HALT` ⇒ the caller is
/// told a worse fault than the one that happened, and both the second
/// `TFT_BRIDGE_RECREATE` assertion and the `"re-plan"` one fail. Mutant: negate
/// the delta on the way through `note_time_jump` ⇒ *"left: 5000000000, right:
/// -5000000000"*, a rewind reported as a fast-forward.
#[test]
fn a_clock_reset_under_recreate_latches_and_keeps_its_own_action() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_RECREATE,
    );
    b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 10_000 * MS, POSE, None);
    // A bag loop, as `rcl_time_jump_t` reports it: new time minus old, so a
    // five-second rewind is negative.
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

/// **A lone publisher regressing by five seconds is dropped, not promoted —
/// however many times it does it.**
///
/// This is the defect §5.5's ladder exists to remove, stated as a fixture. One
/// node restarting and replaying its own buffer looks *exactly* like a bag loop
/// to anything watching one edge's stamps, and the rule that promoted it stopped
/// robots that had nothing wrong with them. Distance is not evidence: the
/// arriving samples are refused either way, because Phase 1's ring would refuse
/// them anyway, so nothing is lost by not stopping.
///
/// Note the offers carry a real receipt clock, so the offset layer is fully
/// engaged and this is not passing merely because that layer was asleep.
///
/// Mutant: promote a lone step by returning `Some(CommonMode { publishers: 1, …
/// })` from `OffsetTable::observe` when one publisher steps ⇒ the first
/// regression halts, and every `TFT_BRIDGE_DROPPED` assertion here fails.
/// Mutant: drop the `publishers < 2` guard entirely ⇒ the same.
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

/// **Two publishers whose offsets step by the same amount are the clock; one is
/// not — and the halt says which rung concluded it.**
///
/// §5.5's fallback rung at the seam, in the shape the rest of this file cannot
/// express: `TOPO` declares one dynamic edge, so nothing else here can put two
/// distinct publishers inside one correlation window. Hence a local topology
/// with a second dynamic edge and a second publisher — the pair the rule was
/// opened about, a localizer's `map -> odom` and a wheel driver's `odom ->
/// base`.
///
/// **The receipt clock is what makes this work at all.** Each publisher's
/// `stamp - received` offset is tracked, so a `transform_tolerance` is measured
/// and subtracted rather than mistaken for a jump; what promotes is a *step* in
/// that offset, seen in two distinct publishers within a second of each other
/// and agreeing about its size. A real `/clock` step moves everybody by the same
/// amount and two independent restarts do not, which is why agreement is the
/// evidence rather than mere coincidence in time.
///
/// The delta is `-5_020 * MS` and not a round five seconds because it is
/// measured against the *receipt* clock: the wheel driver's post-rewind message
/// arrives 20 ms of real time after its last pre-rewind one, and those 20 ms are
/// part of how far its offset moved. That is the measurement being honest about
/// what it is, and the agreement tolerance — 25 % of 5 s, or 1.25 s — exists
/// precisely so two publishers sampling the same jump at different instants
/// still agree.
///
/// The evidence has to ride in `detail` because `tft_bridge_outcome` has room
/// for exactly one `(parent, child)` pair — filled here with the edge whose
/// sample *completed* the step — and growing that POD is a
/// `struct_size`-versioned break. It is not decoration: an operator told "two
/// publishers stepped together" goes and looks at those two nodes, and one told
/// "the time source reported it" goes and looks at the bag.
///
/// Mutant: drop the evidence from the `HaltReason::ClockReset` detail and return
/// the plain "the bridge halted" sentence ⇒ the `"publishers"` assertion fails
/// and the halt no longer says what it concluded. Mutant: promote a single
/// witness, by removing `OffsetTable::observe`'s `publishers < 2` guard ⇒ the
/// first offer halts and the `TFT_BRIDGE_DROPPED` assertion fails, which is the
/// false halt on a healthy robot this design exists to remove. Mutant: increment
/// `clock_resets` on the isolated regression too ⇒ the `0` assertion fails, and
/// the counter goes back to meaning "regressions" instead of "promotions".
#[test]
fn a_clock_reset_needs_a_second_publisher_and_reports_how_many_corroborated() {
    /// Two dynamic edges from two nodes — the pair 0011 was opened about: a
    /// localizer's `map -> odom` and a wheel driver's `odom -> base`.
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
        // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
        assert_eq!(
            unsafe { tft_bridge_attribute(b.0, g.as_ptr(), name.as_ptr()) },
            TFT_OK
        );
    }
    // Both publishers' first sample defines their offset baseline: there is
    // nothing yet for either to have stepped away from.
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
    // Alone, that is a node restarting — dropped, counted, and the bridge keeps
    // running.
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

    // The wheel driver's offset steps by the same five seconds, 10 ms after
    // that. Two independent publishers do not restart in lockstep *and by the
    // same amount*, so the only cause left is the clock they share.
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

/// **A stop is `first_time = 1` exactly once, and the replay after it is
/// rate-limited like every other repeated outcome.**
///
/// `HALT` and `RECREATE` are the only actions a caller *must* log, and they are
/// the only ones that repeat forever: every offer after the stop replays the
/// latched action. Without a rate limiter on them the rclcpp bridge emitted one
/// `RCLCPP_FATAL` per transform for the life of the process — at 20 edges and
/// 100 Hz, 2000 lines a second, each taking rcutils' logging mutex on the
/// ingest thread and burying the one actionable line. §5.4 requires the
/// diagnostic be "loud, **rate-limited**"; `first_time` is the whole of that
/// mechanism and it was set on three arms out of five.
///
/// **The stop is a reported clock jump**, because a single publisher on a single
/// edge can no longer produce one and should not be able to: §5.5's ladder never
/// promotes one witness. A bag replay really does have the authoritative signal,
/// so the fixture uses it — and it also proves the new entry point latches
/// through exactly the same machinery `offer` does, which is why it goes through
/// `fill` rather than growing a second copy of the halt wording.
///
/// The `clock_resets` assertion is what says a promotion happened rather than a
/// bare drop — that counter counts promotions, not regressions.
///
/// Mutant: delete `o.first_time = 1` from the `Action::Halt` arm ⇒ the halting
/// call reports 0 and a caller has no way to tell the transition from the
/// replay; the first `assert_eq!(o.first_time, 1)` fails. Mutant: the same
/// deletion in `Action::RecreateArena` ⇒ the second one fails. Mutant: set
/// `o.first_time = 1` on either `Stopped` short-circuit path ⇒ every replayed
/// call claims to be the first and the `0` assertions fail. Mutant: hard-code
/// `clock_resets: 0` in `tft_bridge_get_stats` ⇒ the promotion the halt was
/// raised from is invisible to `tf_tree doctor`, and the `clock_resets`
/// assertion fails.
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
    // A jump reported twice — a bag that loops twice — replays the latch and is
    // rate-limited exactly like a repeated offer.
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

/// **A reported jump charges no counter, names no edge, and refuses a code it
/// does not know.**
///
/// Three properties of the authoritative entry point that nothing else pins.
///
/// *No counter.* `tft_bridge_stats`' ledger totals to `transforms`, and this
/// call is not a transform — not even on a stopped bridge, where an offer would
/// have charged `refused_after_halt`. A counter that moved here would have to be
/// added to the ledger to keep it balancing, which would be the ledger lying in
/// order to look consistent. `clock_resets` moves, because it counts clock
/// events rather than transforms and is not a ledger term.
///
/// *No edge.* The call has no transform in hand, so `scratch` holds whichever
/// edge happened to be last on the wire — an innocent one. This is the same
/// argument the `STRICT` window-close halt makes, and it is why the `ClockReset`
/// arm names an edge only for the inferred rung.
///
/// *An unknown kind is a call fault*, like an out-of-range topic: it says the
/// caller's build disagrees with this one about an enum, which is not something
/// an outcome code can express.
///
/// Mutant: call `name_the_edge` unconditionally in the `HaltReason::ClockReset`
/// arm ⇒ the halt names `odom -> base`, an edge that did nothing wrong:
/// *"assertion `left == right` failed: a reported jump is not about any
/// transform, so it names no edge rather than an innocent one; left:
/// `("odom", "base")`, right: `("", "")`"*.
///
/// Mutant: increment `inner.refused_after_halt` on `note_time_jump`'s stopped
/// path ⇒ `(transforms, refused_after_halt)` reads `(2, 1)` against `(1, 0)`.
/// Note what that mutant does **not** break: `assert_balanced` still passes,
/// because `tft_bridge_get_stats` folds `refused_after_halt` into `transforms`
/// as well, so the ledger stays self-consistent while both numbers describe an
/// event that was never a transform. The explicit assertion is the only thing
/// standing between that and a counter nobody can interpret.
///
/// Mutant: accept any `kind` by defaulting to `JumpKind::Backward` ⇒ the
/// `TFT_ERR_BAD_ENUM` assertion fails.
///
/// Mutant: report the evidence as `TFT_BRIDGE_EVIDENCE_COMMON_MODE` ⇒ the halt
/// sends a field engineer to look at two publishers that did nothing, when the
/// time source had already said what happened. Mutant: fill
/// `clock_evidence_detail` from `delta_nanos` instead of the jump kind ⇒ the
/// code is right and the number is nonsense, which the paired assertion catches
/// and a `clock_evidence`-only assertion would not.
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

    // `use_sim_time` switched at runtime: a source change, whose delta compares
    // two different time bases and is therefore not printed as a duration.
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
///
/// An empty config *parses* — it is a legal description of a tree with no edges
/// — so nothing below this refused one, and a bridge built from it starts
/// clean, reports "ingest bridge up" and answers `TFT_BRIDGE_UNDECLARED` to
/// 100 % of the robot's traffic. That is the same shape as the `tf_prefix`
/// defect §5.6's clarification records: a switch that drops every transform
/// with nothing failing at startup. The engine has no runtime edge declaration
/// (`docs/decisions/0004`, D4), so zero edges at create time means zero edges
/// forever.
///
/// The check lives here rather than in one of §5.8's three deployment forms
/// because it is a policy, and every other startup refusal — domain, cycle,
/// claim — is already here. A form-3 `BridgeHandle` used to accept it.
///
/// Mutant: delete the `config.edges.is_empty()` refusal ⇒ both creates return
/// `TFT_OK` and every `assert_eq!(…, Err(TFT_ERR_BAD_CONFIG))` fails.
#[test]
fn a_topology_declaring_no_edges_is_refused_rather_than_started() {
    for toml in [
        "",
        // Not merely the empty string: a config with frames and headroom but no
        // edge is equally unable to write anything, and it is what a truncated
        // or half-written file looks like.
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

/// **A `/tf_static` value that disagrees with the config is reported with both
/// values and names the file as the incumbent** (§5.7, re-aimed by §5.8).
///
/// Two `robot_state_publisher`s with different URDFs is the canonical
/// misconfiguration, and the actionable half of the diagnostic is the pair of
/// values — "your file says the lidar is at x = 0.35, `/rsp_b` says 0.60".
///
/// Mutant: swap `o.existing` and `o.offered` in the `StaticConflict` arm ⇒ the
/// operator is told the file holds the value the wire just offered, and both
/// value assertions fail.
#[test]
fn a_static_that_disagrees_with_the_config_reports_both_values() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    // Exactly the declared constant: silent verification, and a stamp of zero
    // as `robot_state_publisher` commonly sends.
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

/// **An edge the config does not declare is dropped, counted, and diagnosed
/// once — naming both frames** (§5.8's amendment).
///
/// The engine has no runtime edge declaration, so a transform for a forgotten
/// edge has nowhere to go and the only downstream symptom is a lookup returning
/// no path with nothing anywhere saying why.
///
/// Mutant: set `o.first_time = 1` unconditionally in the `UndeclaredEdge` arm
/// ⇒ an undeclared 1 kHz edge emits a thousand identical lines a second, and
/// the loop's assertion fails on the second offer.
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

/// **A stamp that goes backwards by less than the reset threshold is a drop,
/// not a reset — and the drop names the edge that stalled.**
///
/// A `/tf` stream carries several publishers whose stamps interleave by
/// milliseconds routinely, so a `< 0` test would restart the arena
/// continuously; §5.5's threshold is 100 ms. But `Action::Drop` carries only a
/// reason, so without the C layer filling the names from the sample a caller
/// is told "something went backwards by 40 ms" and cannot say which edge.
///
/// Mutant: delete the `name_the_edge(inner, o)` call from `fill`'s
/// `Action::Drop` arm ⇒ `parent` and `child` read `""` and this fails.
/// **`by_nanos` and `delta_nanos` are both set, and they are not the same
/// number.** One is a backwards *distance* — what a caller printing "went
/// backwards by %ld ns" wants — and the other is the signed displacement the
/// clock events use, so a caller reading either gets a true answer without
/// having to know which arm produced the outcome. C has no type that carries
/// that distinction; two field names are the whole of it.
///
/// Mutant: raise the drop's `by_nanos` assignment to `0` ⇒ the caller cannot
/// tell a 40 ms interleave from a 4 s one, and the `by_nanos` assertion fails.
/// Mutant: assign `o.delta_nanos = *by_nanos`, dropping the negation ⇒ a
/// backward step reports a positive displacement, which under that field's one
/// convention reads as a jump *forward*. Mutant: collapse the two, setting only
/// `by_nanos` ⇒ `delta_nanos` stays 0 and the second assertion fails — which is
/// the tidy-up this pair exists to stop.
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
///
/// A caller that ignores the status must read "nothing happened" with printable
/// empty strings, not its own stack — and that has to hold for the case where
/// the handle is the thing that was wrong, which is the likeliest way a C
/// caller gets here at all.
///
/// The fixture poisons every byte with 0xAA, so a struct the ABI never wrote is
/// caught: zeroing would let `TFT_BRIDGE_APPLIED == 0` pass by accident, and a
/// NULL string would read as empty here while crashing `printf("%s")`.
///
/// Mutant: delete the blank `core::ptr::write(out, o)` that precedes
/// `bridge_of` ⇒ the poisoned struct survives untouched, `action` reads
/// 0xAAAAAAAA, and this fails.
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

/// **A `struct_size` from another build is refused, on every struct that
/// carries one** (§3.6, §6.1).
///
/// Mutant: delete the `tft_bridge_sample` size check — that is,
/// `read_sample`'s `declared != current && declared != v1` ⇒ the second case
/// reads a struct laid out by a different build and returns `TFT_OK`, so the
/// `TFT_ERR_BAD_STRUCT_SIZE` assertion fails.
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

    // …and a size *larger* than this build's is refused too: that is a newer
    // caller against an older library, whose extra bytes this build cannot
    // interpret. `tft_check_abi`'s minor rule is what covers that direction.
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

/// **A caller built before `received_steady_nanos` existed still works** — §3.6's
/// append rule, which the exact-equality check had promised and never
/// implemented.
///
/// §3.6 says fields may be appended to a `struct_size`-versioned struct without
/// a major bump. Until this test there was nothing behind that sentence: every
/// `struct_size` check in this file is an exact equality, so a caller holding a
/// `libtf_tree_c.a` newer than its own header got `TFT_ERR_BAD_STRUCT_SIZE` on
/// **every** offer — a total outage, in precisely the case the rule exists for,
/// and reachable through §4.4's prebuilt-library path.
///
/// The old size is *computed* — `offset_of!` of the appended field is where the
/// old struct ended — rather than written as `88`, which is right on the targets
/// somebody checked and silently wrong elsewhere.
///
/// The missing field is filled from the library's own steady clock rather than
/// left at the "no receipt clock" sentinel, so a legacy caller still gets the
/// offset layer: the reading is taken microseconds after the message arrived,
/// which against a 100 ms threshold is the same answer. What must never happen
/// is substituting `stamp_nanos`, which would make every publisher's offset
/// identically zero and re-enable inference over the signal under suspicion —
/// for exactly the callers who cannot see the fix.
///
/// Mutant: accept only the current size, by dropping `declared != v1` from
/// `read_sample`'s guard ⇒ *"an appended field must not lock an older caller
/// out: a struct_size field names a size this build does not know; left: -3,
/// right: 0"*.
///
/// Mutant: keep accepting the short struct but restore the whole-struct
/// `core::ptr::read_unaligned` ⇒ **this test still passes**, which is exactly
/// why the fixture allocates the prefix tightly instead of declaring a short
/// size over a full-size struct. Under `just asan` the same run reports
/// *"AddressSanitizer: heap-buffer-overflow … READ of size 96"* — the whole
/// current struct, read out of an 88-byte allocation. Relaxing the size check
/// without narrowing the read is the trap this pair of mutants exists to mark.
#[test]
fn a_sample_from_before_the_receipt_clock_is_read_as_a_prefix() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    let (p, c) = (CString::new("odom").unwrap(), CString::new("base").unwrap());
    // The size a caller compiled against ABI 0.1 sends: everything up to but
    // not including the appended field.
    let v1_size = core::mem::offset_of!(tft_bridge_sample, received_steady_nanos);
    assert!(v1_size < core::mem::size_of::<tft_bridge_sample>());

    // **Allocated as exactly `v1_size` bytes**, so a read past the prefix is a
    // genuine heap overrun a sanitizer can see, rather than a read into the
    // tail of a full-size struct that happens to be there.
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

/// **A caller built before `arena_name` existed still gets a bridge, and a heap
/// arena** — `docs/decisions/0015` step 1, and the same §3.6 append rule one
/// struct over.
///
/// `tft_bridge_create` used to validate `struct_size` with exact equality and
/// then `read_unaligned` the whole struct, so this call was
/// `TFT_ERR_BAD_STRUCT_SIZE` — every 0.4 caller locked out of the entry point by
/// an appended field, which is the outage §3.6 exists to prevent. **This test
/// fails against the code that shipped before the record.**
///
/// The fixture allocates **exactly** the old struct's bytes, for the reason
/// `a_sample_from_before_the_receipt_clock_is_read_as_a_prefix` allocates its
/// own tightly: a narrowed size check over a full-size struct would let a
/// restored whole-struct read pass unnoticed, and only a real short allocation
/// makes that read an overrun a sanitizer can see. `just c-abi-check`'s ASan row
/// now runs this file with `bridge,shm`.
///
/// The prefix's **last** field is the one at risk of arriving at the wrong
/// offset, so the assertion is on `tf_prefix`: a remap table that renames
/// `odom` proves the pointer was read from where the old layout put it, not
/// merely that the call returned `TFT_OK`.
///
/// Mutant: accept only the current size, by dropping `declared != v1` from
/// `read_options`'s guard ⇒ *"an appended field must not lock an older caller
/// out: a struct_size field names a size this build does not know; left: -3,
/// right: 0"*.
///
/// Mutant: keep accepting the short struct but restore the whole-struct
/// `core::ptr::read_unaligned` ⇒ under `just shm-check` this is
/// *"SIGSEGV [ 1.107s] an_options_struct_from_before_the_arena_name_is_read_as_a_prefix"*
/// — the garbage past the prefix lands in `arena_name` and is walked as a C
/// string — and under `just c-abi-check`'s ASan row it is diagnosed properly:
/// *"AddressSanitizer: heap-buffer-overflow … READ of size 32 at … is located 0
/// bytes after 24-byte region"*. **The crash is luck; the ASan report is the
/// gate.** Relaxing the size check without narrowing the read is the trap this
/// pair of mutants exists to mark, and it is the same pair
/// `a_sample_from_before_the_receipt_clock_is_read_as_a_prefix` carries.
#[test]
fn an_options_struct_from_before_the_arena_name_is_read_as_a_prefix() {
    let toml = CString::new(TOPO).unwrap();
    let prefix = CString::new("robot1").unwrap();
    // Computed, never a literal: `offset_of!` of the appended field is where
    // the old struct ended, on whatever pointer width this build has.
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

    // **And it is a heap arena.** `arena_name` is the one field the copy leaves
    // untouched, and the zero it is left at is NULL — the documented "private
    // heap arena, as before". Had it been left undefined the create would have
    // walked a garbage pointer as a C string instead of applying transforms.
    // The wire carries the robot's own names; the arena knows the prefixed ones.
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
///
/// The prefix rule accepts *two* sizes and nothing else. A size in between is a
/// build this library has never seen; a size larger is a newer caller against an
/// older library, whose extra bytes this build cannot interpret and must not
/// read. Both are `TFT_ERR_BAD_STRUCT_SIZE` **before** anything reads that far,
/// which is what keeps `read_options`'s bounded copy in bounds.
///
/// Mutant: replace `read_options`'s guard with `declared > current` ⇒ the
/// in-between size is accepted and *"a size between the two known layouts is
/// not a layout: left: 0, right: -3"* fails.
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

/// **A `bridge`-without-`shm` build refuses a shared arena rather than ignoring
/// it** — `docs/decisions/0015` *Failure*, the silent downgrade in its other
/// costume.
///
/// `bridge` and `shm` are independent cargo features, so this configuration
/// carries `arena_name` in its header with no `tf_tree::Open` behind it.
/// Ignoring the field would start a bridge that fills a private heap arena while
/// every consumer waits forever on a rendezvous that will never appear — reached
/// through a *build* rather than a runtime fault, and the more likely of the two
/// because it needs no misconfiguration on the robot at all.
///
/// **This test only exists in the `--features bridge` configuration**, which is
/// `just test-rust`'s and `just lint`'s. Under `bridge,shm` the same request
/// succeeds, and `tests/bridge_shared.rs` is where that is asserted.
///
/// Mutant: make `open_shared`'s no-`shm` arm ignore the field and fall through
/// to `declared.builder().build()` ⇒ *"a shared arena with no shm behind it
/// must refuse, not downgrade: left: 0, right: -42"*.
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

/// **A declared dynamic edge whose domain is not the bridge's is refused at
/// startup** — §5.5, NORMATIVE, *"and fails at startup rather than at first
/// message"*.
///
/// Sim and real transforms in one arena is a class of bug worth making
/// impossible, and finding out at the first message means finding out after
/// twenty nodes have attached.
///
/// Mutant: delete the `config.check_domain(domain)` call in
/// `tft_bridge_create` ⇒ creation succeeds and this fails.
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

/// **A config that does not describe a tree is refused, and says so in terms of
/// the file** rather than of an arena that was never built.
///
/// Mutant: delete the `config.cycle_child()` check and let the builder find it
/// ⇒ the message names `FrameId(1)`, an index into an arena the operator
/// holding a text file cannot resolve, and the `"cycle"` assertion fails.
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

/// **The message and queue-depth counters are what §5.9 asks for**: a mark that
/// only rises, and a capacity to read it against.
///
/// Mutant: assign rather than `max` in `Ingest::note_queue_depth` ⇒ the final
/// reading of 0 wins and a queue that was saturated reports idle.
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

/// **The tree handle outlives the bridge**, so a reader thread cannot be
/// dangled by the executor thread freeing its bridge.
///
/// **No mutant is claimed here, because the property is structurally
/// guarded**: `tft_bridge_tree` hands back `Arc::clone(&share)`, and the only
/// way to break it is to stop using a refcount at all — which does not
/// type-check rather than failing this test. What this test does buy is that
/// the *ordering* works in practice and that the read after the free is a real
/// read of live memory, which is a claim `just c-abi-check`'s Miri and ASan
/// rows can check and no amount of assertion here can.
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
///
/// The bridge normalizes incoming frame names with the prefix and then keys
/// every §5 table on the result — so if the *config* keeps its raw names, the
/// prefixed names match nothing the config declared and the bridge drops 100 %
/// of a correctly configured robot's traffic, reporting `TFT_BRIDGE_UNDECLARED`
/// with a diagnostic that blames the config rather than the prefix. Worse, the
/// arena would be built from the raw names, so even a fixed lookup table would
/// have nowhere to write.
///
/// The direction is settled by the documented operator workflow: `tf_tree
/// topology --discover` emits the names as they appear on the wire, and adding
/// `tf_prefix` for a second robot must not mean hand-editing every name in the
/// file it just produced.
///
/// Mutant: seed `StaticStore` from `config` instead of from `config.rewritten`
/// in `Ingest::with` ⇒ the first offer is `TFT_BRIDGE_UNDECLARED` and the
/// `TFT_BRIDGE_APPLIED` assertion fails. Mutant: build the arena from `config`
/// rather than `ingest.declared()` in `tft_bridge_create` ⇒ the pipeline
/// approves the write and the arena has no `robot1/base` frame, so the outcome
/// is `TFT_BRIDGE_REJECTED` and the same assertion fails with a different code.
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

    // The wire carries the robot's own names, exactly as `--discover` wrote them
    // into the config.
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

    // The arena is the prefixed one, so a consumer looks up the prefixed names —
    // and the raw ones are not frames at all.
    let tree = b.tree();
    let got = tree
        .at("robot1/odom", "robot1/base", 1_000 * MS)
        .expect("the prefixed edge is what was written");
    assert!((got[4] - POSE[4]).abs() < 1e-12, "{got:?}");

    let s = b.stats();
    assert_eq!((s.applied, s.dropped_undeclared), (1, 0));
    assert_balanced(&s);
}

/// **§5.6's remap table is readable from C, and complete before the first
/// message.**
///
/// *"A silent remap is worse than no remap"* is normative, and a C caller can
/// only obey it if the table crosses the boundary. It is complete at startup
/// because §5.8's amendment makes the config the sole source of declared edges,
/// so every declared frame goes through the normalizer at create time.
///
/// Mutant: have `TopologyConfig::rewritten` bypass the caller's `NameNormalizer`
/// (rewrite the strings itself) ⇒ nothing is recorded at create time, the first
/// three rows are absent, and the `"odom"` assertion fails on `TFT_ERR_NO_DATA`.
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

    // A frame the config never declared still earns a row when it is first seen,
    // because that is the remap an operator has no other way to learn about.
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

    // A bridge with nothing to remap has an empty table, and the first read is
    // the loop's termination condition rather than a fault.
    let plain = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_HALT,
    );
    assert!(plain.remaps().is_empty());
}

/// **A `/tf` message for an edge the config declared static is a kind change**
/// (§5.7: *"the edge kind cannot change"*), and the drop names the edge.
///
/// The reachable half of §5.7's hard error through the C seam: a static edge's
/// pose is inline in the arena and its ring capacity is zero, so there is
/// genuinely nowhere to put a dynamic sample for it.
///
/// Mutant: map `DropReason::KindChange` to `TFT_BRIDGE_REASON_BAD_NAME` in
/// `fill` ⇒ the operator is sent to look at frame names for an edge whose names
/// are fine, and the `reason` assertion fails.
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

/// **`TFT_BRIDGE_AUTHORITY_LAST_WRITER_WINS` decodes to the policy it names**
/// (§5.4).
///
/// It is the one authority code with no other test behind it, and an enum
/// decoded to the wrong arm is the quietest possible bug here: `FirstWriterWins`
/// would still *look* correct — one publisher owning the edge — while silently
/// being the opposite of what the launch file asked for.
///
/// Mutant: decode `TFT_BRIDGE_AUTHORITY_LAST_WRITER_WINS` to
/// `AuthorityPolicy::FirstWriterWins` in `tft_bridge_create` ⇒ `/b`'s sample is
/// dropped as `NOT_THE_OWNER` and the `TFT_BRIDGE_APPLIED` assertion fails.
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
        // SAFETY: live handle, 16 readable bytes, NUL-terminated name.
        assert_eq!(
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
///
/// The affinity rule bites harder on `free` than on `offer`. Dropping the handle
/// drops one `EdgeWriter` per declared dynamic edge, and each of those releases
/// a claim and an OFD lease — machine-wide state, per D7 — from a thread that
/// never owned them. That is the corruption §3.2 exists to prevent rather than
/// merely a misuse, so refusing and leaking the handle is the right trade: the
/// claims stay held by the process that legitimately took them, and the operator
/// gets a status naming the handle instead of an edge silently changing owner.
///
/// This runs in a **subprocess** for the same reason as `publish.rs`'s
/// `a_publisher_refuses_the_wrong_thread`: a passing debug build aborts, and a
/// test that aborts the runner is not a test.
///
/// * **debug** — the child must die by `SIGABRT` (6), naming `tft_bridge`.
///   Mutant: delete the `check_thread_token` call from `tft_bridge_free` ⇒ the
///   child frees the handle from the wrong thread, exits 0, and this fails.
/// * **release** — the child must exit 0 having observed `TFT_ERR_WRONG_THREAD`
///   *and* found the bridge still alive and writable afterwards.
// Miri cannot spawn a process, and there is no way to observe an `abort()` from
// inside the process performing it. The misuse is a logic error, not a
// memory-model one.
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

/// The child arm of [`a_bridge_refuses_to_be_freed_from_the_wrong_thread`].
/// Inert unless `TFT_BRIDGE_FREE_CHILD` is set, so a normal run does not abort
/// itself.
///
/// stdout is the channel the parent reads; see `publish.rs`'s
/// `cross_thread_child`, which carries the same allow for the same reason.
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
    // The handle survived: the claims were not released by a thread that never
    // held them, and the bridge still writes.
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_APPLIED, "{}", text(o.detail));
    println!("BRIDGE FREE REFUSED OK");
}
