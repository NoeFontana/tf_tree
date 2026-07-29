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

    /// Offer one transform and return the outcome, checking the call itself was
    /// well-formed. The `CString`s outlive the call, which is all the ABI asks.
    fn offer(
        &self,
        topic: tft_bridge_topic,
        parent: &str,
        child: &str,
        stamp: i64,
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

/// **An unattributed publisher is not an error** (§5.3: attribution degrades).
///
/// A GID of all zeroes is what an RMW that reports none leaves behind, so it
/// must mean "nothing was told to us" rather than "publisher number zero" —
/// otherwise every unattributed sample on the bus would be attributed to one
/// imaginary node and `FirstWriterWins` would hand it every edge.
///
/// Mutant: delete the `key == [0u8; 16]` early return in `publisher_of` ⇒ the
/// zero GID misses the cache and resolves to `<unknown publisher>`, so the
/// second assertion fails.
#[test]
fn an_unreported_gid_degrades_rather_than_failing() {
    let b = Bridge::new(TFT_BRIDGE_AUTHORITY_STRICT, TFT_BRIDGE_ON_CLOCK_RESET_HALT);
    // No GID at all.
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 1_000 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_APPLIED);

    // An all-zero GID is the same publisher as no GID, so `Strict` — which
    // halts on the *second* distinct publisher — must not fire.
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

/// **A halted bridge refuses everything afterwards, and the ledger still
/// balances.**
///
/// §5.5 says the bridge *stops*. This ABI cannot exit somebody else's process,
/// so stopping means latching: a caller that logs the halt and keeps offering
/// would push exactly the stamps §5.5 exists to prevent.
///
/// Mutant: delete `inner.stopped = Some(…)` from the `Action::Halt` arm ⇒ the
/// offer after the halt is applied and the `TFT_BRIDGE_HALT` assertion fails.
/// Mutant: drop `+ inner.refused_after_halt` from `transforms` in
/// `tft_bridge_get_stats` ⇒ `assert_balanced` fails, short by 3.
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
    b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_000 * MS,
        POSE,
        Some(&a),
    );
    let o = b.offer(
        TFT_BRIDGE_TOPIC_TF,
        "odom",
        "base",
        1_010 * MS,
        POSE,
        Some(&z),
    );
    assert_eq!(o.action, TFT_BRIDGE_HALT);
    assert_eq!(o.reason, TFT_BRIDGE_REASON_AUTHORITY_CONFLICT);
    assert_eq!(
        (text(o.owner), text(o.intruder)),
        ("/a".into(), "/b".into())
    );

    for k in 0..3i64 {
        let o = b.offer(
            TFT_BRIDGE_TOPIC_TF,
            "odom",
            "base",
            1_020 * MS + k * MS,
            POSE,
            Some(&a),
        );
        assert_eq!(o.action, TFT_BRIDGE_HALT, "a halt does not wear off");
        assert_eq!(o.reason, TFT_BRIDGE_REASON_ALREADY_HALTED);
    }
    let s = b.stats();
    assert_eq!(s.refused_after_halt, 3);
    assert_eq!(
        s.applied, 1,
        "only the first publisher's sample was written"
    );
    assert_balanced(&s);
}

/// **`TFT_BRIDGE_RECREATE` stops the bridge too, and keeps saying `RECREATE`.**
///
/// §5.5's `recreate` builds a fresh arena; this ABI will not, because every
/// plan the node compiled points into the current one. So the only correct
/// continuation is that the caller tears the bridge down — and the pipeline's
/// clock guard has *already accepted* the rewound stamp, so an unlatched bridge
/// would approve every subsequent sample and let the arena refuse them one at a
/// time as non-monotonic: a bag loop turning into a silent permanent stall.
///
/// Mutant: delete `inner.stopped = Some(…)` from the `RecreateArena` arm ⇒ the
/// next offer is `TFT_BRIDGE_APPLIED` and this fails. Mutant: latch it with
/// `action: TFT_BRIDGE_HALT` ⇒ the caller is told a worse fault than the one
/// that happened, and both the second `TFT_BRIDGE_RECREATE` assertion and the
/// `"re-plan"` one fail.
#[test]
fn a_clock_reset_under_recreate_latches_and_keeps_its_own_action() {
    let b = Bridge::new(
        TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        TFT_BRIDGE_ON_CLOCK_RESET_RECREATE,
    );
    b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 10_000 * MS, POSE, None);
    // A bag loop: far more than the 100 ms jitter threshold.
    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 5_000 * MS, POSE, None);
    assert_eq!(o.action, TFT_BRIDGE_RECREATE, "{}", text(o.detail));
    assert_eq!(o.by_nanos, 5_000 * MS);
    assert!(text(o.detail).contains("re-plan"));

    let o = b.offer(TFT_BRIDGE_TOPIC_TF, "odom", "base", 5_010 * MS, POSE, None);
    assert_eq!(
        o.action, TFT_BRIDGE_RECREATE,
        "a recreate must not degrade into a halt on the next call"
    );
    assert_eq!(o.reason, TFT_BRIDGE_REASON_ALREADY_HALTED);
    assert_eq!(o.by_nanos, 5_000 * MS);
    assert!(
        text(o.detail).contains("re-plan"),
        "and the sentence keeps saying what to do, not \"halted\": {:?}",
        text(o.detail)
    );
    assert_balanced(&b.stats());
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
/// Mutant: raise the drop's `by_nanos` assignment to `0` ⇒ the caller cannot
/// tell a 40 ms interleave from a 4 s one, and the `by_nanos` assertion fails.
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
    assert_eq!(o.by_nanos, 40 * MS);
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
/// Mutant: delete the `tft_bridge_sample` size check ⇒ the second case reads a
/// struct laid out by a different build and returns `TFT_OK`, so the
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

    assert_eq!(
        b.stats().transforms,
        0,
        "no malformed call reached the pipeline"
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
