//! The bridge fills a **shared** arena — `docs/decisions/0015`.
//!
//! `tests/bridge.rs` covers the seam an `rclcpp` node calls against a private
//! heap arena, and it cannot host these: its `#![cfg(feature = "bridge")]` is at
//! file scope, so every test in it must compile in a build with no `shm` at all.
//! Everything here needs a real rendezvous — a runtime directory, a `memfd`, a
//! lock file and an owner socket — and therefore `--features bridge,shm`, which
//! is `just shm-check`'s two new lines and nothing else's.
//!
//! **The centrepiece is `a_second_process_reads_what_the_bridge_wrote`.** The
//! whole record exists because `tft_bridge_create` built a heap arena, so
//! `docs/PHASE5.md` §9.1's *"one bridge plus N `tf_tree` consumers"* arm was not
//! merely unmeasured but unconstructible. A test that only asserted
//! `tft_bridge_create` returned `TFT_OK` under a name would pass against
//! `TreeBuilder::build_shared`, which publishes no rendezvous at all and which
//! no second process can find. Reading the transform back from **another
//! process** — `src/bin/bridge_reader.rs`, spawned — is what distinguishes the
//! two, and it is a real process for the reason that binary's own docs give.
#![cfg(all(feature = "bridge", feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::ffi::c_char;
use core::ptr;
use std::ffi::{CStr, CString};
use std::path::PathBuf;

use tf_tree_c::bridge::*;
use tf_tree_c::*;

/// The same fixture `tests/bridge.rs` uses, so a difference between the heap and
/// shared paths cannot hide behind a different topology.
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

/// The one scratch runtime directory this **process** uses, removed when the
/// last test holding it finishes.
///
/// **Per process rather than per test, and that is the whole design.** The
/// rendezvous is selected by `$TF_TREE_RUNTIME_DIR`, and `set_var` is
/// process-wide — so the per-test scratch directory
/// `crates/tf_tree/tests/rendezvous.rs` uses is only safe because every recipe
/// that runs *that* target is `cargo nextest run`, which gives each test its own
/// process. This target is different: `just c-abi-check`'s ASan row is plain
/// `cargo test`, which runs these three in **threads of one process**, and three
/// per-test `Scratch`es would race — the loser resolving the winner's
/// rendezvous, and one test's `Drop` deleting the directory another was still
/// using.
///
/// So the directory is shared and the arena *names* are what keep the tests
/// apart. `set_var` runs exactly once, inside the `OnceLock`, before any test
/// gets past its first line — so no thread can be reading the environment while
/// another writes it.
///
/// The count is taken under the same lock that creates and removes, so the
/// directory cannot be deleted while any test still holds one.
struct Scratch;

static LIVE: std::sync::Mutex<usize> = std::sync::Mutex::new(0);

fn scratch_dir() -> &'static PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let p = std::env::temp_dir().join(format!("tf_tree_bs-{}", std::process::id()));
        std::env::set_var("TF_TREE_RUNTIME_DIR", &p);
        // The domain is the *rendezvous* domain and comes from the environment
        // (`docs/decisions/0019` §3 answer 2), never from
        // `tft_bridge_options::domain`. Pinning it keeps a developer's own
        // `$ROS_DOMAIN_ID` out of the test.
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
    let mut out = tft_bridge_outcome {
        struct_size: core::mem::size_of::<tft_bridge_outcome>() as u32,
        // SAFETY: `tft_bridge_outcome` is `#[repr(C)]`, `Copy`, and made of
        // integers, `f64` arrays and pointers, so all-zero is a valid value of
        // it. The ABI overwrites every field before it returns.
        ..unsafe { core::mem::zeroed() }
    };
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
    let mut e = tft_error {
        struct_size: core::mem::size_of::<tft_error>() as u32,
        ..blank_error()
    };
    // SAFETY: `e` is a live local with `struct_size` set.
    let rc = unsafe { tft_last_error(&mut e) };
    assert_eq!(rc, TFT_OK);
    text(e.message.as_ptr())
}

fn blank_error() -> tft_error {
    // SAFETY: `tft_error` is `#[repr(C)]`, `Copy`, and made entirely of integers
    // and a byte array, so all-zero is a valid value of it.
    unsafe { core::mem::zeroed() }
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
    // `Open::new()`'s defaults are the consumer (`docs/decisions/0019` §2a):
    // read-only, never create. Spelling neither is the point — a consumer of a
    // bridge-filled arena is an ordinary consumer, which is the record's
    // "**no new consumer API**".
    tf_tree::Open::new().name(name)?.open()
}

/// Run `bridge_reader` as a **separate process** and return its one line.
///
/// `src/bin/bridge_reader.rs` says why a process rather than another `Open`
/// here. The environment is passed explicitly rather than inherited: the parent
/// sets `$TF_TREE_RUNTIME_DIR` with `set_var` inside [`scratch_dir`]'s
/// `OnceLock`, and reading it back through `std::env` here is what makes the
/// child's rendezvous provably the same one, with no shared state but those two
/// strings and the name.
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

/// The same lookup in **this** process, rendered the same way, so the two can be
/// compared bit for bit rather than through two roundings.
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

/// **A separate process reads what the bridge wrote.** The property the whole
/// record exists for.
///
/// `docs/PHASE5.md` §9.1's *"one bridge plus N `tf_tree` consumers"* arm needs
/// the bridge's arena to be reachable from outside its own process.
/// `tft_bridge_create` built a heap one, so the arm was not unmeasured but
/// unconstructible — `just dds-bench` prints that gap above its own table on
/// every run.
///
/// **The reader is `src/bin/bridge_reader.rs`, spawned.** An earlier revision of
/// this test carried this name over a second `tf_tree::Open` inside the test
/// process. That attach was genuine — it resolves the rendezvous socket and
/// receives the segment by fd passing, so it is not reading the bridge's own
/// `Tree` — but "another process" is a claim about process boundaries, and the
/// only thing that settles it is one. The child shares no address space, no
/// mapping and no open file description with the bridge, and finds the arena
/// from `$TF_TREE_RUNTIME_DIR`, `$TF_TREE_DOMAIN` and the name.
///
/// The in-process attach is kept and the two are compared **bit for bit**: it is
/// now the control rather than the claim, and it is what turns "the child
/// printed something plausible" into "the child read these bytes".
///
/// The attach is `tf_tree::Open` with its **defaults** — read-only, never
/// create — on both sides, because the record's claim is that a bridge becomes
/// an ordinary producer of the arena Phase 2 already specified, with **no new
/// consumer API**. The child links no `tf_tree_c` at all, which is the sharper
/// half of that: if a consumer needed a bridge-specific call, it could not be
/// written.
///
/// Mutant: route the shared arm through `declared.builder().build_shared(name)`
/// instead of `tf_tree::Open` ⇒ the create still returns `TFT_OK` (a
/// `build_shared` name is a debug label, and no rendezvous is published), and
/// this fails with *"the second process could not read the bridge's arena:
/// error no arena is serving and CreatePolicy::Never forbids creating one"*.
/// That mutant is the reason this test reads from outside the process rather
/// than through `tft_bridge_tree`.
///
/// Mutant: drop `.layout_if_creating(builder)` ⇒ *"tft_bridge_create with an
/// arena_name: -42 (shared arena could not be created: no layout was supplied
/// and the arena had to be created (arena_name "bridge-read"))"*. That the
/// message arrives intact is the generic arm doing its job.
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

    // The control: the same lookup here, compared as bit patterns. A comparison
    // that rounds is a comparison that can agree while the memory does not.
    let tree = attach(name).unwrap_or_else(|e| {
        panic!("a consumer must be able to find the bridge's arena: {e:?}");
    });
    assert_eq!(
        child_bits,
        read_bits(&tree, "odom", "base", 1_000 * MS),
        "the second process read different bytes than this one"
    );

    // And the bytes are the pose the bridge was handed, not merely a value two
    // readers agree on.
    let got = read(&tree, "odom", "base", 1_000 * MS);
    assert!(
        (got[4] - POSE[4]).abs() < 1e-12
            && (got[5] - POSE[5]).abs() < 1e-12
            && (got[6] - POSE[6]).abs() < 1e-12,
        "the consumer read a different transform than the bridge wrote: {got:?}"
    );

    // The static edge is in the same arena, written by the builder rather than
    // by an offer — so this also rules out an arena that merely happens to hold
    // one dynamic sample. Read from the second process too: the builder's half
    // of the arena has to cross the boundary as well as the publisher's.
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

/// **A second bridge on a name already held is refused, and the first keeps
/// serving.**
///
/// `docs/decisions/0019` §3's question 3: a second bridge on a held name is a
/// *rendezvous* fault, reported as a startup refusal with its own message —
/// beside §5.4's per-edge authority machinery, never inside it.
///
/// The mechanism is `Open::require_create(true)`. Without it `CreatePolicy`
/// offers no "create, or refuse if one is already live" setting: `IfAbsent`
/// takes the **join** path, so the second bridge attaches read-write to an arena
/// it did not size and goes on to claim edges in it.
///
/// Mutant: drop `.require_create(true)` from `open_shared` ⇒ *"a second bridge
/// must not join an arena it did not size: left: -31, right: -42"*. **The `-31`
/// is the point, not an incidental code.** `TFT_ERR_ALREADY_CLAIMED` is §5.4's
/// per-edge authority error, so the joining bridge gets as far as the claim loop
/// and reports an *edge* conflict for what is an *arena ownership* fault —
/// exactly the one-diagnostic-two-meanings collapse `docs/decisions/0019` §3's
/// question 3 refuses. And it is an accident of this fixture: the second bridge
/// fails only because it declares the same edges. One declaring different edges
/// would join and start writing, with nothing failing at all.
///
/// Mutant: map `OpenError::ArenaAlreadyLive` onto the generic arm ⇒ the status
/// still matches and the message assertion fails: *"the message must say the
/// name is taken, not merely that something failed: shared arena could not be
/// created: an arena is already live at this rendezvous and require_create was
/// set (arena_name "bridge-held")"*. That is the half keeping *"another bridge
/// holds this name"* distinguishable from *"the runtime directory is
/// unusable"*.
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

    // **And the first bridge is still the one serving.** A refusal that tore
    // down the incumbent's rendezvous would be worse than one that joined.
    let tree = attach(name).expect("the first bridge still serves its arena");
    let got = read(&tree, "odom", "base", 1_000 * MS);
    assert!(
        (got[4] - POSE[4]).abs() < 1e-12,
        "the surviving arena is the first bridge's: {got:?}"
    );
}

/// **A bridge with a NULL `arena_name` publishes nothing**, under `shm` exactly
/// as without it.
///
/// The default is the whole compatibility claim: `arena_name` is opt-in, and a
/// caller that does not set it — including every caller compiled against the
/// 0.4 header, whose bytes end before the field — gets the private heap arena it
/// always had. A shared build that leaked a rendezvous for every bridge would
/// put a `memfd`, a lock file and a participant slot on §5.8's form 3, which
/// exists precisely to need none of them.
///
/// Mutant: make the arm unconditional — `let arena_name = arena_name.or(Some(
/// "default"))` before the match in `tft_bridge_create` ⇒ the attach below
/// succeeds and the run reports *"a NULL arena_name must publish no
/// rendezvous"*.
///
/// **No line number, deliberately.** An earlier revision of this note cited one
/// and it was wrong by 46 lines — a mutant note is re-run when the code under it
/// changes, but a line number goes stale when anything *above* it changes, which
/// is every edit to this file. The panic text is unique in the workspace and
/// does not rot.
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
    // `tf_tree_ipc` is not a dependency of this crate — the C ABI reaches the
    // rendezvous only through the facade — so the variant is asserted on its
    // rendering rather than by pattern.
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("ArenaAbsent"),
        "and the absence must be the ordinary one a consumer sees: {rendered}"
    );
}
