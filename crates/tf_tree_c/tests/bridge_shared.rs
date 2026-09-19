//! The bridge fills a **shared** arena — `docs/decisions/0015`.
#![cfg(all(feature = "bridge", feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Posture (`docs/decisions/0007` rule 1, kind 5; `0048` step 4): our own C ABI called
// from Rust; declared here because a test is a separate crate root.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_char;
use core::ptr;
use std::ffi::{CStr, CString};
use std::path::PathBuf;

use tf_tree_c::bridge::*;
use tf_tree_c::*;

/// The same fixture `tests/bridge.rs` uses, so a difference between the heap and shared paths
/// cannot hide behind a different topology.
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

const MS: i64 = 1_000_000;

/// The one scratch runtime directory this **process** uses, removed when the last test holding
/// it finishes.
struct Scratch;

static LIVE: std::sync::Mutex<usize> = std::sync::Mutex::new(0);

fn scratch_dir() -> &'static PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let p = std::env::temp_dir().join(format!("tf_tree_bs-{}", std::process::id()));
        std::env::set_var("TF_TREE_RUNTIME_DIR", &p);
        // The domain is the *rendezvous* domain and comes from the environment
        // (`docs/decisions/0019` §3 answer 2), never from `tft_bridge_options::domain`.
        std::env::set_var("TF_TREE_DOMAIN", "0");
        p
    })
}

impl Scratch {
    fn new() -> Scratch {
        let mut live = LIVE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = scratch_dir();
        if *live == 0 {
            // Anything here is from a previous process that shared this pid.
            let _ = std::fs::remove_dir_all(dir);
        }
        std::fs::create_dir_all(dir).unwrap();
        *live += 1;
        Scratch
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let mut live = LIVE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *live -= 1;
        if *live == 0 {
            let _ = std::fs::remove_dir_all(scratch_dir());
        }
    }
}

/// A bridge handle, freed on drop on its creating thread.
#[derive(Debug)]
struct Bridge(*mut tft_bridge);

impl Drop for Bridge {
    fn drop(&mut self) {
        // SAFETY: created below, freed exactly once, on the creating thread.
        unsafe { tft_bridge_free(self.0) };
    }
}

/// `tft_bridge_create` with an `arena_name`, returning the status on failure.
fn create_shared(name: &str) -> Result<Bridge, tft_status> {
    let toml = CString::new(TOPO).unwrap();
    let arena = CString::new(name).unwrap();
    let opts = tft_bridge_options {
        struct_size: core::mem::size_of::<tft_bridge_options>() as u32,
        authority: TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        on_clock_reset: TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        domain: 0,
        tf_prefix: ptr::null(),
        arena_name: arena.as_ptr(),
    };
    let mut b: *mut tft_bridge = ptr::null_mut();
    // SAFETY: NUL-terminated config and name, a live full-size `opts`, `b` a
    // live local.
    let rc = unsafe { tft_bridge_create(toml.as_ptr(), &opts, &mut b) };
    if rc == TFT_OK {
        assert!(!b.is_null());
        Ok(Bridge(b))
    } else {
        assert!(b.is_null(), "a failed create must not hand out a handle");
        Err(rc)
    }
}

/// Offer one `/tf` transform through the ABI and return its action.
fn offer(b: &Bridge, parent: &str, child: &str, stamp: i64, pose: [f64; 7]) -> tft_bridge_action {
    let (p, c) = (CString::new(parent).unwrap(), CString::new(child).unwrap());
    let s = tft_bridge_sample {
        struct_size: core::mem::size_of::<tft_bridge_sample>() as u32,
        frame_id: p.as_ptr(),
        child_frame_id: c.as_ptr(),
        stamp_nanos: stamp,
        pose,
        received_steady_nanos: 0,
    };
    let mut out = tft_bridge_outcome::blank();
    // SAFETY: live handle on its creating thread; the `CString`s outlive the
    // call; `out` is a live local with `struct_size` set.
    let rc = unsafe { tft_bridge_offer(b.0, TFT_BRIDGE_TOPIC_TF, &s, ptr::null(), &mut out) };
    assert_eq!(rc, TFT_OK, "the call was malformed: {}", last_message());
    assert_eq!(
        out.action,
        TFT_BRIDGE_APPLIED,
        "the fixture edge must be applied: {}",
        text(out.detail)
    );
    out.action
}

/// This thread's last error message, as Rust text.
fn last_message() -> String {
    let mut e = tft_error::blank();
    // SAFETY: `e` is a live local with `struct_size` set.
    let rc = unsafe { tft_last_error(&mut e) };
    assert_eq!(rc, TFT_OK);
    text(e.message.as_ptr())
}

fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the ABI contracts every string it hands out is NUL-terminated and
    // valid until the next call on the handle; nothing intervenes here.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Attach to `name` **read-only**, in this process.
fn attach(name: &str) -> Result<tf_tree::Tree, tf_tree::OpenError> {
    // `Open::new()`'s defaults are the consumer (`docs/decisions/0019` §2a): read-only, never
    // create.
    tf_tree::Open::new().name(name)?.open()
}

/// Run `bridge_reader` as a **separate process** and return its one line.
fn read_in_a_second_process(name: &str, target: &str, source: &str, stamp: i64) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bridge_reader"))
        .args([name, target, source, &stamp.to_string()])
        .env("TF_TREE_RUNTIME_DIR", scratch_dir())
        .env("TF_TREE_DOMAIN", "0")
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("spawn bridge_reader");
    assert!(
        out.status.success(),
        "bridge_reader exited {:?}",
        out.status.code()
    );
    String::from_utf8(out.stdout)
        .expect("bridge_reader's protocol is ASCII")
        .trim_end()
        .to_string()
}

/// The same lookup in **this** process, rendered the same way, so the two can be compared bit
/// for bit rather than through two roundings.
fn read_bits(tree: &tf_tree::Tree, target: &str, source: &str, stamp: i64) -> String {
    let g = tree.guard();
    let t = tree
        .frame(target)
        .expect("the target frame is in the arena");
    let s = tree
        .frame(source)
        .expect("the source frame is in the arena");
    let plan = tree.plan(t, s).expect("plan");
    let iso = plan
        .at(
            &g,
            tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(stamp),
        )
        .expect("the bridge's sample is retained at this stamp");
    iso.to_bits()
        .iter()
        .map(|w| format!("{w:016x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Read `target <- source` at `stamp` through a plain `tf_tree::Tree`.
fn read(tree: &tf_tree::Tree, target: &str, source: &str, stamp: i64) -> [f64; 7] {
    let g = tree.guard();
    let t = tree
        .frame(target)
        .expect("the target frame is in the arena");
    let s = tree
        .frame(source)
        .expect("the source frame is in the arena");
    let plan = tree.plan(t, s).expect("plan");
    let iso = plan
        .at(
            &g,
            tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(stamp),
        )
        .expect("the bridge's sample is retained at this stamp");
    [
        iso.q.w, iso.q.x, iso.q.y, iso.q.z, iso.t.x, iso.t.y, iso.t.z,
    ]
}

/// **A separate process reads what the bridge wrote.** The property the whole record exists
/// for.
#[test]
fn a_second_process_reads_what_the_bridge_wrote() {
    let _scratch = Scratch::new();
    let name = "bridge-read";

    let b = create_shared(name).unwrap_or_else(|rc| {
        panic!(
            "tft_bridge_create with an arena_name: {rc} ({})",
            last_message()
        );
    });
    offer(&b, "odom", "base", 1_000 * MS, POSE);

    // **The claim.** A process that was not there when the arena was made.
    let line = read_in_a_second_process(name, "odom", "base", 1_000 * MS);
    let child_bits = line.strip_prefix("ok ").unwrap_or_else(|| {
        panic!("the second process could not read the bridge's arena: {line}");
    });

    // The control: the same lookup here, compared as bit patterns. A comparison that rounds is
    // a comparison that can agree while the memory does not.
    let tree = attach(name).unwrap_or_else(|e| {
        panic!("a consumer must be able to find the bridge's arena: {e:?}");
    });
    assert_eq!(
        child_bits,
        read_bits(&tree, "odom", "base", 1_000 * MS),
        "the second process read different bytes than this one"
    );

    // And the bytes are the pose the bridge was handed, not merely a value two readers agree
    // on.
    let got = read(&tree, "odom", "base", 1_000 * MS);
    assert!(
        (got[4] - POSE[4]).abs() < 1e-12
            && (got[5] - POSE[5]).abs() < 1e-12
            && (got[6] - POSE[6]).abs() < 1e-12,
        "the consumer read a different transform than the bridge wrote: {got:?}"
    );

    // The static edge is in the same arena, written by the builder rather than by an offer — so
    // this also rules out an arena that merely happens to hold one dynamic sample.
    let lidar_line = read_in_a_second_process(name, "base", "lidar", 1_000 * MS);
    assert!(
        lidar_line.starts_with("ok "),
        "the declared static edge must be readable from outside too: {lidar_line}"
    );
    assert_eq!(
        lidar_line.strip_prefix("ok ").unwrap_or_default(),
        read_bits(&tree, "base", "lidar", 1_000 * MS),
        "the second process read a different static edge"
    );
    let lidar = read(&tree, "base", "lidar", 1_000 * MS);
    assert!(
        (lidar[4] - 0.35).abs() < 1e-12,
        "the declared static edge is in the shared arena too: {lidar:?}"
    );
}

/// **A second bridge on a name already held is refused, and the first keeps serving.**
#[test]
fn a_second_bridge_on_a_held_name_is_refused() {
    let _scratch = Scratch::new();
    let name = "bridge-held";

    let first = create_shared(name).unwrap_or_else(|rc| {
        panic!("the first bridge must start: {rc} ({})", last_message());
    });
    offer(&first, "odom", "base", 1_000 * MS, POSE);

    let rc = create_shared(name).expect_err("a second bridge must not start");
    assert_eq!(
        rc, TFT_ERR_ARENA_UNAVAILABLE,
        "a second bridge must not join an arena it did not size"
    );
    let msg = last_message();
    assert!(
        msg.contains("already holds this rendezvous name"),
        "the message must say the name is taken, not merely that something failed: {msg}"
    );
    assert!(
        msg.contains(name),
        "and it must name the arena the operator has to change: {msg}"
    );

    // **And the first bridge is still the one serving.** A refusal that tore down the
    // incumbent's rendezvous would be worse than one that joined.
    let tree = attach(name).expect("the first bridge still serves its arena");
    let got = read(&tree, "odom", "base", 1_000 * MS);
    assert!(
        (got[4] - POSE[4]).abs() < 1e-12,
        "the surviving arena is the first bridge's: {got:?}"
    );
}

/// **A bridge with a NULL `arena_name` publishes nothing**, under `shm` exactly as without it.
#[test]
fn a_null_arena_name_publishes_no_rendezvous() {
    let _scratch = Scratch::new();

    let toml = CString::new(TOPO).unwrap();
    let opts = tft_bridge_options {
        struct_size: core::mem::size_of::<tft_bridge_options>() as u32,
        authority: TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        on_clock_reset: TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        domain: 0,
        tf_prefix: ptr::null(),
        arena_name: ptr::null(),
    };
    let mut raw: *mut tft_bridge = ptr::null_mut();
    // SAFETY: NUL-terminated config, a live full-size `opts`, `raw` a live
    // local.
    let rc = unsafe { tft_bridge_create(toml.as_ptr(), &opts, &mut raw) };
    assert_eq!(rc, TFT_OK, "{}", last_message());
    let b = Bridge(raw);
    offer(&b, "odom", "base", 1_000 * MS, POSE);

    let err = match attach("default") {
        Err(e) => e,
        Ok(_) => panic!("a NULL arena_name must publish no rendezvous"),
    };
    // `tf_tree_ipc` is not a dependency of this crate — the C ABI reaches the rendezvous only
    // through the facade — so the variant is asserted on its rendering rather than by pattern.
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("ArenaAbsent"),
        "and the absence must be the ordinary one a consumer sees: {rendered}"
    );
}
