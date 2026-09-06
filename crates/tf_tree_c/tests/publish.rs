//! The publish surface of the C ABI — `docs/PHASE4.md` §3.2 and §6.1.
//!
//! `abi.rs` covers misuse with no handle; `live.rs` covers the read path against
//! a live handle. This covers the *write* path, which is the half where a
//! mistake corrupts a robot's transform tree rather than merely returning a bad
//! answer to the process that made it.
#![cfg(feature = "test-hooks")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::ffi::c_char;
use core::ptr;

use tf_tree_c::*;

/// A tree with one claimable dynamic edge, `world -> robot`.
struct Fixture(*mut tft_tree);

impl Fixture {
    fn new() -> Fixture {
        let mut p: *mut tft_tree = ptr::null_mut();
        // SAFETY: `p` is a live local.
        assert_eq!(unsafe { tft_test_publishable_tree_create(&mut p) }, TFT_OK);
        assert!(!p.is_null());
        Fixture(p)
    }

    fn claim(&self, child: &str, parent: &str) -> Result<Publisher, tft_status> {
        let c = std::ffi::CString::new(child).unwrap();
        let p = std::ffi::CString::new(parent).unwrap();
        let mut h: *mut tft_publisher = ptr::null_mut();
        // SAFETY: live tree handle, NUL-terminated names, `h` a live local.
        let rc = unsafe { tft_tree_claim(self.0, c.as_ptr(), p.as_ptr(), &mut h) };
        if rc == TFT_OK {
            Ok(Publisher(h))
        } else {
            Err(rc)
        }
    }

    fn plan(&self, target: &str, source: &str) -> *mut tft_plan {
        let t = std::ffi::CString::new(target).unwrap();
        let s = std::ffi::CString::new(source).unwrap();
        let mut p: *mut tft_plan = ptr::null_mut();
        // SAFETY: live handle, NUL-terminated names, `p` a live local.
        assert_eq!(
            unsafe { tft_plan_create(self.0, t.as_ptr(), s.as_ptr(), &mut p) },
            TFT_OK
        );
        p
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // SAFETY: created above, freed exactly once.
        unsafe { tft_tree_free(self.0) };
    }
}

#[derive(Debug)]
struct Publisher(*mut tft_publisher);

impl Publisher {
    fn push(&self, stamp: i64, layout: tft_layout, bytes: &[u8]) -> tft_status {
        // SAFETY: live handle; `bytes` is at least the layout's payload, which
        // every caller below sizes with `tft_layout_size`.
        unsafe { tft_publisher_push(self.0, stamp, layout, bytes.as_ptr().cast()) }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        // SAFETY: created above, freed exactly once, on its creating thread.
        unsafe { tft_publisher_free(self.0) };
    }
}

fn last_error() -> tft_error {
    let mut e = tft_error::blank();
    // SAFETY: `e` is a live, aligned `tft_error` with `struct_size` set.
    assert_eq!(unsafe { tft_last_error(&mut e) }, TFT_OK);
    e
}

/// `[qw qx qy qz tx ty tz]` as raw bytes.
fn quat7(q: [f64; 4], t: [f64; 3]) -> [u8; 56] {
    let mut b = [0u8; 56];
    for (i, v) in q.iter().chain(t.iter()).enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
    }
    b
}

fn read_f64(b: &[u8], i: usize) -> f64 {
    f64::from_ne_bytes(b[i * 8..i * 8 + 8].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// The working path
// ---------------------------------------------------------------------------

/// **What a C publisher actually does**: claim, push, and have a reader see it.
///
/// The assertion is against the *read* path, not against another write, so a
/// matched pair of bugs in `layout::read`/`layout::write` cannot satisfy it —
/// the read goes through `tft_plan_at`, which has its own hand-computed
/// byte-pattern tests.
#[test]
fn a_published_transform_is_visible_to_a_reader() {
    let f = Fixture::new();
    let p = f.claim("robot", "world").expect("the edge is unclaimed");

    // Rz(90°) at t=0 and t=1 s, so any stamp in between is exactly on the
    // segment and the reader has something to interpolate.
    let c = core::f64::consts::FRAC_1_SQRT_2;
    assert_eq!(
        p.push(
            0,
            TFT_LAYOUT_QVEC7_WXYZ,
            &quat7([1.0, 0.0, 0.0, 0.0], [0.0; 3])
        ),
        TFT_OK
    );
    assert_eq!(
        p.push(
            1_000_000_000,
            TFT_LAYOUT_QVEC7_WXYZ,
            &quat7([c, 0.0, 0.0, c], [2.0, 0.0, 0.0])
        ),
        TFT_OK
    );

    let plan = f.plan("world", "robot");
    let mut out = [0u8; 56];
    // SAFETY: live plan, `out` is exactly `tft_layout_size(QVEC7_WXYZ)`.
    let rc = unsafe {
        tft_plan_at(
            plan,
            1_000_000_000,
            TFT_LAYOUT_QVEC7_WXYZ,
            out.as_mut_ptr().cast(),
        )
    };
    assert_eq!(rc, TFT_OK, "{}", message(&last_error()));
    assert!((read_f64(&out, 0) - c).abs() < 1e-12, "qw");
    assert!((read_f64(&out, 3) - c).abs() < 1e-12, "qz");
    assert!((read_f64(&out, 4) - 2.0).abs() < 1e-12, "tx");
    // SAFETY: created by `f.plan`, freed once.
    unsafe { tft_plan_free(plan) };
}

/// **Every readable layout publishes the same transform.**
///
/// Mutant: drop the `column_major` transposition in `read_mat4` ⇒ the two matrix
/// layouts disagree and this fails. `every_readable_layout_round_trips` in the
/// unit tests catches the same mutant, but only through `write`; this catches it
/// through the shipped entry point, which is the one a C caller uses.
#[test]
fn all_four_readable_layouts_publish_identically() {
    let c = core::f64::consts::FRAC_1_SQRT_2;
    let mut results = Vec::new();
    for layout in [
        TFT_LAYOUT_QVEC7_WXYZ,
        TFT_LAYOUT_QVEC7_XYZW,
        TFT_LAYOUT_MAT4_ROW,
        TFT_LAYOUT_MAT4_COL,
    ] {
        let f = Fixture::new();
        let p = f.claim("robot", "world").unwrap();

        // Build the payload by writing a known transform out in this layout,
        // through the read path's inverse — `tft_plan_at` on a tree that already
        // holds it. Simpler and just as independent: hand-build for QVEC7 and
        // derive the matrices from Rz(90°)'s hand-computed pattern.
        let src: Vec<u8> = match layout {
            TFT_LAYOUT_QVEC7_WXYZ => quat7([c, 0.0, 0.0, c], [1.0, 2.0, 3.0]).to_vec(),
            TFT_LAYOUT_QVEC7_XYZW => quat7([0.0, 0.0, c, c], [1.0, 2.0, 3.0]).to_vec(),
            TFT_LAYOUT_MAT4_ROW => pack(&[
                0.0, -1.0, 0.0, 1.0, //
                1.0, 0.0, 0.0, 2.0, //
                0.0, 0.0, 1.0, 3.0, //
                0.0, 0.0, 0.0, 1.0,
            ]),
            TFT_LAYOUT_MAT4_COL => pack(&[
                0.0, 1.0, 0.0, 0.0, //
                -1.0, 0.0, 0.0, 0.0, //
                0.0, 0.0, 1.0, 0.0, //
                1.0, 2.0, 3.0, 1.0,
            ]),
            other => panic!("unexpected layout {other}"),
        };
        assert_eq!(
            p.push(0, layout, &src),
            TFT_OK,
            "layout {layout}: {}",
            message(&last_error())
        );
        // Read it back in the canonical layout and compare across layouts.
        let plan = f.plan("world", "robot");
        let mut out = [0u8; 56];
        // SAFETY: live plan; `out` is exactly the QVEC7 payload.
        assert_eq!(
            unsafe { tft_plan_at(plan, 0, TFT_LAYOUT_QVEC7_WXYZ, out.as_mut_ptr().cast()) },
            TFT_OK
        );
        // SAFETY: freed once.
        unsafe { tft_plan_free(plan) };
        results.push((layout, out));
    }
    let (_, first) = &results[0];
    for (layout, got) in &results[1..] {
        for i in 0..7 {
            assert!(
                (read_f64(first, i) - read_f64(got, i)).abs() < 1e-12,
                "layout {layout} element {i}: {} vs {}",
                read_f64(first, i),
                read_f64(got, i)
            );
        }
    }
}

fn pack(v: &[f64]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_ne_bytes()).collect()
}

// `c_char` is `i8` on x86_64 and `u8` on aarch64, so this cast is necessary
// on one target and a no-op on the other; see `src/error.rs` for the full
// note. The allow is the fix — deleting the cast breaks x86_64.
#[allow(clippy::unnecessary_cast)]
fn message(e: &tft_error) -> String {
    let bytes: Vec<u8> = e
        .message
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// **A batch publishes, and a stride lets it read out of an array of caller
/// structs** — §4.3's `Sophus::SE3d` case, in the publish direction.
#[test]
fn push_many_honours_a_stride() {
    let f = Fixture::new();
    let p = f.claim("robot", "world").unwrap();

    const N: usize = 8;
    const STRIDE: usize = 64; // 56-byte payload in a 64-byte struct
    let mut src = vec![0u8; N * STRIDE];
    let stamps: Vec<i64> = (0..N as i64).map(|i| i * 100_000_000).collect();
    for i in 0..N {
        let tx = i as f64;
        let one = quat7([1.0, 0.0, 0.0, 0.0], [tx, 0.0, 0.0]);
        src[i * STRIDE..i * STRIDE + 56].copy_from_slice(&one);
        // Poison the padding, so a stride bug reads garbage rather than zeros.
        src[i * STRIDE + 56..(i + 1) * STRIDE].fill(0xAB);
    }
    // SAFETY: live handle; `stamps` has N elements and `src` is N*STRIDE bytes.
    let rc = unsafe {
        tft_publisher_push_many(
            p.0,
            stamps.as_ptr(),
            N,
            TFT_LAYOUT_QVEC7_WXYZ,
            src.as_ptr().cast(),
            STRIDE,
        )
    };
    assert_eq!(rc, TFT_OK, "{}", message(&last_error()));

    let plan = f.plan("world", "robot");
    for (i, &t) in stamps.iter().enumerate() {
        let mut out = [0u8; 56];
        // SAFETY: live plan, correctly sized buffer.
        assert_eq!(
            unsafe { tft_plan_at(plan, t, TFT_LAYOUT_QVEC7_WXYZ, out.as_mut_ptr().cast()) },
            TFT_OK
        );
        assert!(
            (read_f64(&out, 4) - i as f64).abs() < 1e-12,
            "element {i}: tx = {}",
            read_f64(&out, 4)
        );
    }
    // SAFETY: freed once.
    unsafe { tft_plan_free(plan) };
}

// ---------------------------------------------------------------------------
// Misuse
// ---------------------------------------------------------------------------

/// **One writer per edge (D7), enforced across the C boundary.**
///
/// Mutant: return the writer without going through `Tree::claim` ⇒ two live
/// publishers on one edge, which is the invariant the whole engine is built on.
#[test]
fn a_second_claim_on_a_held_edge_is_refused() {
    let f = Fixture::new();
    let _first = f.claim("robot", "world").expect("first claim succeeds");
    assert_eq!(
        f.claim("robot", "world").unwrap_err(),
        TFT_ERR_ALREADY_CLAIMED
    );
    assert_eq!(last_error().code, TFT_ERR_ALREADY_CLAIMED);
}

/// **Releasing gives the edge back**, and the released handle refuses to
/// publish rather than writing through a dropped claim.
#[test]
fn release_frees_the_edge_and_disarms_the_handle() {
    let f = Fixture::new();
    let p = f.claim("robot", "world").unwrap();
    // SAFETY: live handle, on its creating thread.
    assert_eq!(unsafe { tft_publisher_release(p.0) }, TFT_OK);
    // Releasing twice is a no-op, not an error.
    // SAFETY: as above.
    assert_eq!(unsafe { tft_publisher_release(p.0) }, TFT_OK);

    assert_eq!(
        p.push(
            0,
            TFT_LAYOUT_QVEC7_WXYZ,
            &quat7([1.0, 0.0, 0.0, 0.0], [0.0; 3])
        ),
        TFT_ERR_RELEASED
    );
    // ...and the edge is genuinely free again.
    let _second = f
        .claim("robot", "world")
        .expect("release must return the edge");
}

/// **A static edge cannot be published to.** There is no ring behind it.
#[test]
fn claiming_a_static_edge_is_refused() {
    let f = Fixture::new();
    assert_eq!(f.claim("tool", "robot").unwrap_err(), TFT_ERR_NOT_DYNAMIC);
}

/// **Stamps are non-decreasing per edge**, and a violation is reported with
/// both the offending stamp and the edge's newest.
#[test]
fn a_backwards_stamp_is_refused_and_says_by_how_much() {
    let f = Fixture::new();
    let p = f.claim("robot", "world").unwrap();
    let id = quat7([1.0, 0.0, 0.0, 0.0], [0.0; 3]);
    assert_eq!(p.push(1_000, TFT_LAYOUT_QVEC7_WXYZ, &id), TFT_OK);
    assert_eq!(
        p.push(999, TFT_LAYOUT_QVEC7_WXYZ, &id),
        TFT_ERR_NON_MONOTONIC
    );
    let e = last_error();
    assert_eq!(e.requested, 999);
    assert_eq!(e.newest, 1_000);
}

/// **A left-handed matrix never reaches the arena**, through the shipped entry
/// point rather than only through the unit-tested helper.
///
/// Mutant: delete the determinant check in `layout::read_mat4` ⇒ the reflection
/// is published as a rotation and this returns `TFT_OK`.
#[test]
fn a_reflected_matrix_is_refused_at_the_boundary() {
    let f = Fixture::new();
    let p = f.claim("robot", "world").unwrap();
    // Rz(90°) with its **first column** negated — elements (0,0), (1,0), (2,0)
    // — giving det = −1. An earlier version of this fixture negated the third
    // diagonal entry as well; two sign flips cancel and det came back to +1, so
    // the test failed against correct code. Written out with the determinant
    // stated so the next reader does not have to re-derive it:
    //
    //   [ 0 −1  0 ]
    //   [−1  0  0 ]   det = 0·0 − (−1)·(−1·1) + 0 = −1
    //   [ 0  0  1 ]
    let src = pack(&[
        0.0, -1.0, 0.0, 1.0, //
        -1.0, 0.0, 0.0, 2.0, //
        0.0, 0.0, 1.0, 3.0, //
        0.0, 0.0, 0.0, 1.0,
    ]);
    assert_eq!(p.push(0, TFT_LAYOUT_MAT4_ROW, &src), TFT_ERR_NOT_A_ROTATION);
    assert!(message(&last_error()).contains("det R"));
}

/// **The `f32` GPU layout is write-only.** Publishing through it would halve
/// the precision of everything downstream, silently.
#[test]
fn the_f32_layout_cannot_be_published_through() {
    let f = Fixture::new();
    let p = f.claim("robot", "world").unwrap();
    assert_eq!(
        p.push(0, TFT_LAYOUT_AFFINE12_ROW_F32, &[0u8; 48]),
        TFT_ERR_BAD_ENUM
    );
}

/// **NaN never reaches the arena.** One NaN pose poisons every interpolation
/// that brackets it, forever, with no way to detect it after the fact.
#[test]
fn a_nan_transform_is_refused() {
    let f = Fixture::new();
    let p = f.claim("robot", "world").unwrap();
    assert_eq!(
        p.push(
            0,
            TFT_LAYOUT_QVEC7_WXYZ,
            &quat7([1.0, 0.0, 0.0, 0.0], [f64::NAN, 0.0, 0.0])
        ),
        TFT_ERR_NOT_FINITE
    );
}

/// **A batch stops at the first rejection and says which element.**
///
/// Unlike `tft_plan_at_many`, the earlier elements stay published — there is no
/// unpublishing a release-store a reader may already have observed. The
/// contract is therefore "you know exactly where the stream stopped", and this
/// is the test of that.
#[test]
fn a_batch_reports_the_index_that_failed() {
    let f = Fixture::new();
    let p = f.claim("robot", "world").unwrap();
    const N: usize = 5;
    let mut src = vec![0u8; N * 56];
    for i in 0..N {
        let one = quat7([1.0, 0.0, 0.0, 0.0], [i as f64, 0.0, 0.0]);
        src[i * 56..(i + 1) * 56].copy_from_slice(&one);
    }
    // Element 3's quaternion is zeroed: an uninitialized C struct.
    src[3 * 56..3 * 56 + 32].fill(0);
    let stamps: Vec<i64> = (0..N as i64).map(|i| i * 1_000_000).collect();
    // SAFETY: live handle; N stamps and N tightly packed payloads.
    let rc = unsafe {
        tft_publisher_push_many(
            p.0,
            stamps.as_ptr(),
            N,
            TFT_LAYOUT_QVEC7_WXYZ,
            src.as_ptr().cast(),
            0,
        )
    };
    assert_eq!(rc, TFT_ERR_NOT_A_ROTATION);
    let e = last_error();
    assert_eq!(e.frame_b, 3, "the failing index must be reported");

    // Elements 0..3 are published and readable — the documented behaviour, not
    // an accident: they are release-stores that already happened.
    let plan = f.plan("world", "robot");
    let mut out = [0u8; 56];
    // SAFETY: live plan, correctly sized buffer.
    assert_eq!(
        unsafe {
            tft_plan_at(
                plan,
                2_000_000,
                TFT_LAYOUT_QVEC7_WXYZ,
                out.as_mut_ptr().cast(),
            )
        },
        TFT_OK
    );
    assert!((read_f64(&out, 4) - 2.0).abs() < 1e-12);
    // SAFETY: freed once.
    unsafe { tft_plan_free(plan) };
}

/// **Handle-type confusion is caught on the publish surface too.**
#[test]
fn a_tree_is_not_a_publisher() {
    let f = Fixture::new();
    // SAFETY: `f.0` is a live handle, so the magic read is in bounds; it just
    // holds the wrong value.
    let rc = unsafe {
        tft_publisher_push(
            f.0.cast::<tft_publisher>(),
            0,
            TFT_LAYOUT_QVEC7_WXYZ,
            [0u8; 56].as_ptr().cast(),
        )
    };
    assert_eq!(rc, TFT_ERR_BAD_HANDLE);
    // SAFETY: freeing a non-publisher is a no-op — the magic word is what makes
    // it one, and this asserts the tree survives it.
    unsafe { tft_publisher_free(f.0.cast::<tft_publisher>()) };
    // SAFETY: `f.0` is a live tree handle.
    assert_eq!(
        unsafe { tft_tree_frame_count(f.0) },
        3,
        "the tree must be untouched"
    );
}

// ---------------------------------------------------------------------------
// Thread affinity — §3.2, NORMATIVE
// ---------------------------------------------------------------------------

/// **A publisher used from a thread that does not own it is stopped.**
///
/// §3.2 is NORMATIVE: `tft_publisher` is `Send + !Sync`, C cannot say so, and
/// the handle must therefore refuse cross-thread use — **`abort()` in debug
/// builds**, with a message that names the mistake.
///
/// This runs in a **subprocess** because a passing debug build aborts, and a
/// test that aborts the runner is not a test. The child is this same binary
/// re-invoked with `TFT_CROSS_THREAD_CHILD=1`; `cross_thread_child` below is
/// what it runs.
///
/// The assertion differs by profile, and both halves are real:
///
/// * **debug** — the child must die by `SIGABRT` (6). Mutant: delete the
///   `check_thread` call in `tft_publisher_push` ⇒ the child exits 0 and this
///   fails.
/// * **release** — the child must exit 0 having observed `TFT_ERR_WRONG_THREAD`,
///   which is the addition to §3.2 this crate makes and the reason that status
///   code is not dead.
// Miri cannot spawn a process (`extern static pidfd_spawnp is not supported`),
// and this test's whole mechanism is a subprocess — there is no way to observe
// an `abort()` from inside the process that performs it. Skipped there and run
// everywhere else; the misuse it covers is a logic error, not a memory-model
// one, so Miri is not the tool that would catch it anyway.
#[cfg_attr(miri, ignore = "needs a subprocess to observe abort()")]
#[test]
fn a_publisher_refuses_the_wrong_thread() {
    use std::process::Command;
    if std::env::var_os("TFT_CROSS_THREAD_CHILD").is_some() {
        return; // the child arm is `cross_thread_child`; nothing to do here
    }
    let exe = std::env::current_exe().expect("test binary path");
    let out = Command::new(exe)
        .args(["--exact", "cross_thread_child", "--nocapture"])
        .env("TFT_CROSS_THREAD_CHILD", "1")
        .output()
        .expect("re-invoke the test binary");

    if cfg!(debug_assertions) {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            out.status.signal(),
            Some(6),
            "debug builds must abort (SIGABRT) on cross-thread use; got {:?}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Send but not Sync"),
            "the abort must name the mistake: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    } else {
        assert!(
            out.status.success(),
            "release builds must return a status, not abort: {:?}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("WRONG_THREAD OK"),
            "the child must observe TFT_ERR_WRONG_THREAD: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

/// The child arm of [`a_publisher_refuses_the_wrong_thread`]. Inert unless
/// `TFT_CROSS_THREAD_CHILD` is set, so a normal run does not abort itself.
///
/// stdout is the channel the parent reads, so `print_stdout` — a `warn` in the
/// workspace lints — is what this arm is *for*. Allowed here rather than at the
/// file level so an accidental `println!` in an ordinary test still warns.
#[allow(clippy::print_stdout)]
#[test]
fn cross_thread_child() {
    if std::env::var_os("TFT_CROSS_THREAD_CHILD").is_none() {
        return;
    }
    let f = Fixture::new();
    let p = f.claim("robot", "world").unwrap();
    let raw = p.0 as usize; // a raw handle is not `Send`; the ABI's rule is what is under test
    let rc = std::thread::spawn(move || {
        let h = raw as *mut tft_publisher;
        let src = quat7([1.0, 0.0, 0.0, 0.0], [0.0; 3]);
        // SAFETY: `h` is a live handle. Using it from this thread is exactly
        // the misuse §3.2 requires the library to catch, and catching it is
        // what this call is here to demonstrate.
        unsafe { tft_publisher_push(h, 0, TFT_LAYOUT_QVEC7_WXYZ, src.as_ptr().cast()) }
    })
    .join()
    .expect("the pushing thread");

    // Only reached in a release build; a debug build aborted inside the spawn.
    assert_eq!(rc, TFT_ERR_WRONG_THREAD);
    println!("WRONG_THREAD OK");
    // Drop the publisher on its owning thread, or the destructor trips the same
    // check — which is deliberate, and tested by that being true.
    drop(p);
}

// ---------------------------------------------------------------------------
// Introspection against a tree with headroom — the case review found
// ---------------------------------------------------------------------------

/// **A frame id inside the arena's headroom is not a frame.**
///
/// `ArenaView::frame_record` bounds an id against `max_frames`, which is
/// `frame_count + 1 + frame_headroom` — *not* against `frame_count`. The slots
/// between are zeroed arena memory, and `tft_tree_frame_name` used to read one,
/// find `name_len == 0`, write a lone NUL and return `TFT_OK`. A diagnostic
/// iterating ids would have rendered blank frames that do not exist.
///
/// `live.rs`'s equivalent assertion passed the whole time, because both C
/// fixtures were built with **zero headroom**, which makes the two bounds
/// coincide. That is what makes this fixture's `frame_headroom(4)` the point of
/// the test rather than a detail of it — and why the fixture carries it now.
///
/// Mutant: **remove both filters** — that is, restore the original
/// `FrameId::new(id).and_then(|f| view.frame_record(f))` — ⇒ ids 4..=7 return
/// `TFT_OK` with an empty name.
///
/// Removing *either* filter alone leaves this passing, and that was checked
/// rather than assumed. The two are redundant for this case and not for others:
/// `id <= count` is the range check, while `name_hash != 0` additionally covers
/// an id *within* the count whose record `FrameTable::finish` has not written
/// yet (it bumps `frame_count` before `write_record`). That second case cannot
/// be provoked deterministically from a test, so it is guarded rather than
/// pinned, and this docstring says so instead of claiming a mutant it does not
/// kill.
#[test]
fn a_frame_id_in_the_headroom_is_refused_not_read() {
    let f = Fixture::new();
    // SAFETY: `f.0` is a live handle.
    let count = unsafe { tft_tree_frame_count(f.0) };
    assert_eq!(count, 3, "world/robot/tool");

    let mut buf: [c_char; 64] = [0; 64];
    // The real frames answer.
    for id in 1..=count {
        // SAFETY: live handle; 64 writable bytes.
        assert_eq!(
            unsafe { tft_tree_frame_name(f.0, id, buf.as_mut_ptr(), buf.len()) },
            TFT_OK,
            "frame {id} is real and must have a name"
        );
        assert!(buf[0] != 0, "frame {id} must not report an empty name");
    }

    // Everything past the count is a hole in the arena, headroom included.
    // Non-vacuity: at least one of these ids must be *inside* `max_frames`, or
    // the loop is only re-testing the out-of-range path `live.rs` covers.
    let mut in_range_holes = 0;
    for id in count + 1..=count + 5 {
        buf[0] = 0x7f;
        // SAFETY: live handle; 64 writable bytes.
        let rc = unsafe { tft_tree_frame_name(f.0, id, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(rc, TFT_ERR_UNKNOWN_FRAME, "id {id} is not a frame");
        assert_eq!(buf[0], 0x7f, "nothing may be written for a non-frame");
        if id <= count + 4 {
            in_range_holes += 1;
        }
    }
    assert!(
        in_range_holes >= 4,
        "the fixture must have headroom, or this test cannot fail"
    );

    // **And the guard must not over-reject.** A frame interned at runtime lands
    // in what was headroom a moment ago, and its name must be readable the
    // instant `frame_count` covers it. Without this the test would pass just as
    // well against a `tft_tree_frame_name` that refused every id.
    let _ = f.claim("late_arrival", "world"); // interns, then fails on NoEdge
                                              // SAFETY: `f.0` is a live handle.
    let after = unsafe { tft_tree_frame_count(f.0) };
    assert_eq!(after, count + 1, "the claim must have interned the name");
    // SAFETY: live handle; 64 writable bytes.
    assert_eq!(
        unsafe { tft_tree_frame_name(f.0, after, buf.as_mut_ptr(), buf.len()) },
        TFT_OK
    );
    // See `src/error.rs`: `c_char` is `i8` on x86_64 and `u8` on aarch64, so
    // this cast is necessary on one target and a no-op on the other.
    #[allow(clippy::unnecessary_cast)]
    let name: String = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8 as char)
        .collect();
    assert_eq!(name, "late_arrival");
}

/// **A wrong parent is not a wrong frame name.**
///
/// `ParentMismatch` was reported as `TFT_ERR_UNKNOWN_FRAME`, whose frozen
/// definition is "a frame name that this tree never interned" — false for
/// *every* instance of this case, because `tft_tree_claim` resolves both names
/// before the mismatch can arise. A C caller reading that code would go and
/// check its spelling, which is the wrong fix. Reported by review.
///
/// Mutant: map `ParentMismatch` back onto `TFT_ERR_UNKNOWN_FRAME` ⇒ this fails,
/// and the caller can no longer tell a typo from a topology error.
#[test]
fn claiming_the_wrong_parent_is_its_own_error() {
    let f = Fixture::new();
    // Both names exist; `robot` is attached to `world`, not to `tool`.
    assert_eq!(
        f.claim("robot", "tool").unwrap_err(),
        TFT_ERR_PARENT_MISMATCH
    );
    let e = last_error();
    assert_eq!(e.frame_a, 2, "the child frame");
    assert_eq!(e.frame_b, 1, "its ACTUAL parent, world");

    // **A name nobody has used before is *interned*, not rejected.**
    //
    // `Tree::frame` interns; it does not look up. So a typo'd child name becomes
    // a real frame — which then has no incoming edge, and the claim fails with
    // `TFT_ERR_NO_EDGE` rather than `TFT_ERR_UNKNOWN_FRAME`. This test asserted
    // the latter and found out otherwise, which is worth pinning: it is the
    // second reason `NoEdge` needed a code of its own, and it means a C caller
    // that mistypes a frame name consumes a headroom slot permanently (ids are
    // never recycled, D10). Documented on `tft_tree_claim` rather than changed
    // here — `Tree::frame`'s semantics are Phase 2's and are shared with the
    // Python binding and the CLI.
    assert_eq!(f.claim("nonesuch", "world").unwrap_err(), TFT_ERR_NO_EDGE);

    // With the headroom exhausted, interning fails and the name really is
    // unknown — so the older code is still reachable, and still means what the
    // header says it means.
    for i in 0..8 {
        let _ = f.claim(&format!("filler{i}"), "world");
    }
    assert_eq!(
        f.claim("one_too_many", "world").unwrap_err(),
        TFT_ERR_UNKNOWN_FRAME,
        "once the frame table is full, an unseen name cannot be interned"
    );
}

/// **A publisher outlives the tree handle it was claimed from**, and the arena
/// stays mapped underneath it — `docs/decisions/0017` step 7.
///
/// This handle no longer carries an `Arc<TreeShare>` of its own: the
/// `OwnedWriter` it holds carries the `Arc<Tree>`, and *that* is what keeps the
/// arena alive. Freeing the tree first is a reasonable thing for a C caller to
/// do — there is no borrow checker to tell them otherwise — so the free order
/// must not matter, exactly as it already does not for `tft_plan`.
///
/// **Where the gate is.** Nothing observable distinguishes a live arena from a
/// freed one on this path, so a plain `cargo nextest` run of this test is not
/// the check: `just c-abi-check`'s Miri and ASan rows are, and they build this
/// target. Replacing `OwnedWriter`'s `tree: Arc<Tree>` field with a
/// `PhantomData` and running the ASan row reports
/// `AddressSanitizer: heap-use-after-free ... READ of size 8`. The same
/// deletion is `0017` step 2's mutant under `just miri`, one crate over; this
/// is the C surface's statement of it.
#[test]
fn a_publisher_outlives_the_tree_handle_it_came_from() {
    let f = Fixture::new();
    let p = f.claim("robot", "world").expect("claim");
    // Free the tree handle while the claim is live. `Fixture::drop` would do it
    // too, but *after* the publisher — which is the order that proves nothing.
    let raw = f.0;
    core::mem::forget(f);
    // SAFETY: a live handle from `Fixture::new`, freed exactly once here.
    unsafe { tft_tree_free(raw) };

    // The claim still works, and still writes into a mapped arena.
    assert_eq!(
        p.push(
            1_000,
            TFT_LAYOUT_QVEC7_WXYZ,
            &quat7([1.0, 0.0, 0.0, 0.0], [1.0, 2.0, 3.0])
        ),
        TFT_OK
    );
    // And releasing it still releases the claim rather than faulting.
    // SAFETY: a live publisher handle, released from its creating thread.
    assert_eq!(unsafe { tft_publisher_release(p.0) }, TFT_OK);
}
