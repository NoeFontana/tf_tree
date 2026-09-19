//! The one real `fork()` in this workspace — `docs/decisions/0005` step 9.
//!
//! Tests `fork()` **without `exec`** (inherited address space, the `MADV_DONTFORK` hole, OFD locks), which
//! only `libc::fork` produces; hence the unsafe-budget exception (`docs/decisions/0007` rule 1 kinds 2 and 5,
//! `0048`). The child's destructors release the **parent's** OFD locks and signal the owner's `eventfd`, so the
//! parent re-validates itself afterwards, not only the exit code.
//!
//! # Modes
//!
//! `api` leaves via `_exit` (no destructor); `drop` drops the tree and writer; `owned` is `drop` with the writer
//! from [`tf_tree::Tree::claim_owned`] (`docs/decisions/0017` step 4); `bridge` is the C ABI layer
//! (`docs/decisions/0015`) behind the `bridge` feature.
//!
//! # Output protocol
//!
//! One line: `child=<exited N|signalled N> parent_ok=<bool> note=<text>`. `exited` versus `signalled` is
//! load-bearing: without the `Drop` guard the child dies of `SIGSEGV` after passing every API check.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::panic
)]
// `docs/decisions/0007` rule 1, kinds 2 and 5 (the OS, our own C ABI); a bin is a separate crate root.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

// SAFETY (module invariant): (1) `libc::fork`, `libc::_exit`, `libc::waitpid` —
// the child path between `fork` and `_exit` is async-signal-safe, no destructor
// runs. (2) `tft_*` calls on a handle this process (or its parent) created, from
// its creating thread.

#[cfg(all(feature = "shm", target_os = "linux"))]
fn main() {
    use std::io::Write;
    use std::sync::Arc;

    use tf_tree::{
        AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, Iso3, LookupError, PushError,
        Stamp, SystemDomain, TreeBuilder,
    };

    /// The two claim shapes behind one `push`, so both run the same assertions.
    enum Writer<'a> {
        Scoped(tf_tree::EdgeWriter<'a>),
        Owned(tf_tree::OwnedWriter),
    }

    impl Writer<'_> {
        fn push(&self, stamp: i64, iso: &Iso3) -> Result<(), PushError> {
            match self {
                Writer::Scoped(w) => w.push(stamp, iso),
                Writer::Owned(w) => w.push(stamp, iso),
            }
        }

        fn edge(&self) -> u32 {
            match self {
                Writer::Scoped(w) => w.edge().get(),
                Writer::Owned(w) => w.edge().get(),
            }
        }
    }

    const OK: i32 = 0;
    const NOT_DETACHED: i32 = 10;
    const LOOKUP_NOT_DETACHED: i32 = 11;
    const PLAN_AT_NOT_DETACHED: i32 = 12;
    const PUSH_NOT_DETACHED: i32 = 13;
    const CLAIM_NOT_DETACHED: i32 = 14;
    const FRAME_NOT_DETACHED: i32 = 15;
    const GEN_NOT_BUMPED_ONCE: i32 = 16;

    fn layout() -> TreeBuilder {
        TreeBuilder::new()
            .default_interp(InterpPolicy::LerpSlerp)
            .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64)))
    }

    let mode = std::env::args().nth(1).unwrap_or_default();

    #[cfg(feature = "bridge")]
    if mode == "bridge" {
        bridge::run();
        return;
    }
    #[cfg(feature = "bridge")]
    if mode == "bridge-reader" {
        bridge::read_back();
        return;
    }

    let tree = Arc::new(
        tf_tree::Open::new()
            .mode(AttachMode::ReadWrite)
            .create(CreatePolicy::IfAbsent)
            .layout_if_creating(layout())
            .open()
            .expect("create the arena"),
    );

    let child_frame = tree.frame("base").unwrap();
    let parent_frame = tree.frame("map").unwrap();
    let writer = if mode == "owned" {
        Writer::Owned(tree.claim_owned(child_frame, parent_frame).expect("claim"))
    } else {
        Writer::Scoped(tree.claim(child_frame, parent_frame).expect("claim"))
    };
    let pose = tf_tree_math::exp_se3([0.0, 0.0, 0.2, 1.0, 2.0, 3.0]);
    writer.push(1_000, &pose).expect("push");

    let plan = tree.plan(parent_frame, child_frame).expect("plan");
    let slot = tree.participant_slot();
    let edge = writer.edge();

    // The fork generation as last seen; the bump being exactly one pins `tf_tree_ipc::fork::arm`.
    let gen_before = tf_tree_ipc::fork::generation();

    // SAFETY: `fork` runs with no worker threads of ours, the owner-server thread is never touched by the
    // child, and every child path ends in `libc::_exit`.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");

    if pid == 0 {
        let mut status = OK;

        if !tree.detached() {
            status = NOT_DETACHED;
        } else if !matches!(
            tree.lookup("map", "base", Stamp::<SystemDomain>::from_nanos(1_000)),
            Err(LookupError::ChildDetached)
        ) {
            status = LOOKUP_NOT_DETACHED;
        } else if !matches!(
            plan.at(&tree.guard(), Stamp::<SystemDomain>::from_nanos(1_000)),
            Err(LookupError::ChildDetached)
        ) {
            status = PLAN_AT_NOT_DETACHED;
        } else if !matches!(writer.push(2_000, &pose), Err(PushError::ChildDetached)) {
            status = PUSH_NOT_DETACHED;
        } else if tree.claim(child_frame, parent_frame).is_ok() {
            status = CLAIM_NOT_DETACHED;
        } else if tree.frame("brand_new_name").is_ok() {
            status = FRAME_NOT_DETACHED;
        } else if tf_tree_ipc::fork::generation().wrapping_sub(gen_before) != 1 {
            status = GEN_NOT_BUMPED_ONCE;
        }

        if mode == "drop" || mode == "owned" {
            // Run destructors via normal scope exit; in `owned` mode this checks the writer stands down first.
            drop(writer);
            drop(tree);
        }

        // SAFETY: `_exit` runs no destructors or atexit handlers, as wanted.
        unsafe { libc::_exit(status) };
    }

    let child = wait_for(pid);

    // Prove the parent is unharmed: `lookup`, `push`, `participant_alive(self)`, `probe_claim` from a fresh
    // description, and a fresh `open()` (fails if the child stopped the owner thread).
    let lookup_ok = tree
        .lookup("map", "base", Stamp::<SystemDomain>::from_nanos(1_000))
        .is_ok();
    let push_ok = writer.push(3_000, &pose).is_ok();
    let alive_ok = tree.participant_alive(slot);
    // The claim lease, asked from an independent description (OFD locks are self-blind).
    let lease_ok = tf_tree_ipc::Rendezvous::from_env()
        .and_then(|rv| tf_tree_ipc::LockFile::open(rv.lock_path()))
        .and_then(|lf| lf.probe_claim(edge))
        .map(|p| p.held)
        .unwrap_or(false);
    let serve_ok = tf_tree::Open::new()
        .mode(AttachMode::ReadOnly)
        .create(CreatePolicy::Never)
        .open()
        .is_ok();

    let parent_ok = lookup_ok && push_ok && alive_ok && serve_ok && lease_ok;
    println!(
        "child={child} parent_ok={parent_ok} note=lookup:{lookup_ok},push:{push_ok},alive:{alive_ok},serve:{serve_ok},lease:{lease_ok}"
    );
    let _ = std::io::stdout().flush();
}

/// Reap `pid` and render its wait status as the `child=` field.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn wait_for(pid: libc::pid_t) -> String {
    let mut wstatus: libc::c_int = 0;
    // SAFETY: `waitpid` writes only through the `&mut c_int`; `pid` is our child.
    let waited = unsafe { libc::waitpid(pid, &mut wstatus, 0) };
    assert_eq!(waited, pid, "waitpid");
    if wstatus & 0x7f == 0 {
        format!("exited {}", (wstatus >> 8) & 0xff)
    } else if wstatus & 0x7f == 0x7f {
        format!("stopped {}", (wstatus >> 8) & 0xff)
    } else {
        format!("signalled {}", wstatus & 0x7f)
    }
}

#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn main() {
    println!("child=skipped parent_ok=true note=shm-unavailable");
}

/// **The C ABI across a `fork()`** — `docs/decisions/0015` *Invariants to maintain*: a forked node calls
/// [`tft_bridge_offer`], [`tft_bridge_get_stats`] and [`tft_bridge_free`], which must **return** in the child
/// rather than fault. The arena name is `"default"`; the rendezvous is per-run (`tests/fork.rs`).
#[cfg(all(feature = "bridge", feature = "shm", target_os = "linux"))]
mod bridge {
    use core::ffi::c_char;
    use core::ptr;
    use std::ffi::{CStr, CString};
    use std::io::Write;

    use tf_tree_c::bridge::{
        tft_bridge, tft_bridge_create, tft_bridge_free, tft_bridge_get_stats, tft_bridge_offer,
        tft_bridge_options, tft_bridge_outcome, tft_bridge_sample, tft_bridge_stats,
        TFT_BRIDGE_APPLIED, TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS, TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        TFT_BRIDGE_REJECTED, TFT_BRIDGE_TOPIC_TF,
    };
    use tf_tree_c::{tft_error, tft_last_error, tft_status, TFT_ERR_CHILD_DETACHED, TFT_OK};

    /// The topology `crates/tf_tree_c/tests/bridge_shared.rs` declares.
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

    const OK: i32 = 0;
    const OFFER_NOT_OK: i32 = 20;
    const OFFER_NOT_REJECTED: i32 = 21;
    const OFFER_STATUS_NOT_DETACHED: i32 = 22;
    const STATS_NOT_OK: i32 = 23;
    const STATS_NOT_COUNTED: i32 = 24;
    const GEN_NOT_BUMPED_ONCE: i32 = 25;

    /// The bridge, the fork, the child's three calls, and the parent's re-validation.
    pub(crate) fn run() {
        let toml = CString::new(TOPO).unwrap();
        let arena = CString::new("default").unwrap();
        let opts = tft_bridge_options {
            struct_size: core::mem::size_of::<tft_bridge_options>() as u32,
            authority: TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
            on_clock_reset: TFT_BRIDGE_ON_CLOCK_RESET_HALT,
            domain: 0,
            tf_prefix: ptr::null(),
            arena_name: arena.as_ptr(),
        };
        let mut b: *mut tft_bridge = ptr::null_mut();
        // SAFETY: NUL-terminated config and name, live full-size `opts`, live `b`.
        let rc = unsafe { tft_bridge_create(toml.as_ptr(), &opts, &mut b) };
        assert_eq!(
            rc,
            TFT_OK,
            "tft_bridge_create with an arena_name: {}",
            last_message()
        );
        assert!(!b.is_null());

        let (rc, out) = offer(b, 1_000 * MS);
        assert_eq!(rc, TFT_OK, "the first offer: {}", last_message());
        assert_eq!(
            out.action,
            TFT_BRIDGE_APPLIED,
            "the fixture edge must be applied before the fork: {}",
            text(out.detail)
        );

        let gen_before = tf_tree_ipc::fork::generation();

        // SAFETY: as [`super::main`]'s `fork`, plus the owner-server thread, never touched by the child; every
        // child path ends in `libc::_exit`.
        //
        // **This child allocates** (`CString`s, `error::last_message()`, a `BTreeMap` drop), relying on glibc's
        // `pthread_atfork` handlers reinitialising malloc in the child; another libc needs three `fork()`s.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");

        if pid == 0 {
            let mut status = OK;

            let (rc, out) = offer(b, 2_000 * MS);
            let mut stats = tft_bridge_stats::blank();
            // Called unconditionally, so all three entry points are reached even if the offer misbehaves.
            // SAFETY: a live handle on its creating thread (the token is a thread-local
            // counter, inherited unchanged); `stats` is live with `struct_size` set.
            let stats_rc = unsafe { tft_bridge_get_stats(b, &mut stats) };
            if rc != TFT_OK {
                status = OFFER_NOT_OK;
            } else if out.action != TFT_BRIDGE_REJECTED {
                status = OFFER_NOT_REJECTED;
            } else if out.status != TFT_ERR_CHILD_DETACHED {
                status = OFFER_STATUS_NOT_DETACHED;
            } else if stats_rc != TFT_OK {
                status = STATS_NOT_OK;
            } else if stats.rejected_by_arena != 1 || stats.applied != 1 {
                status = STATS_NOT_COUNTED;
            } else if tf_tree_ipc::fork::generation().wrapping_sub(gen_before) != 1 {
                // The Rust modes' generation pin, repeated for `tft_bridge_create`'s arena.
                status = GEN_NOT_BUMPED_ONCE;
            }

            // The destructor half: `tft_bridge_free` must return and leave the parent's leases alone.
            //
            // SAFETY: a live handle, freed exactly once, on its creating thread.
            unsafe { tft_bridge_free(b) };

            // SAFETY: `_exit` runs no destructors; they ran inside `tft_bridge_free`.
            unsafe { libc::_exit(status) };
        }

        let child = super::wait_for(pid);

        let (rc, out) = offer(b, 3_000 * MS);
        let offer_ok = rc == TFT_OK && out.action == TFT_BRIDGE_APPLIED;

        let attached = tf_tree::Open::new().open();
        let serve_ok = attached.is_ok();

        // Still readable from a third process (this executable re-`exec`ed).
        let line = read_in_a_third_process(3_000 * MS);
        let their_bits = line.strip_prefix("ok ").unwrap_or_default().to_string();
        let ours = attached
            .as_ref()
            .ok()
            .and_then(|t| lookup(t, "odom", "base", 3_000 * MS));
        let read_ok = !their_bits.is_empty() && ours.as_ref().map(bits_of) == Some(their_bits);
        // The bytes are the pose offered, rotation included; `read_ok` alone could agree on a wrong rotation.
        let value_ok = ours.is_some_and(|iso| {
            (iso.q.w - POSE[0]).abs() < 1e-12
                && (iso.q.x - POSE[1]).abs() < 1e-12
                && (iso.q.y - POSE[2]).abs() < 1e-12
                && (iso.q.z - POSE[3]).abs() < 1e-12
                && (iso.t.x - POSE[4]).abs() < 1e-12
                && (iso.t.y - POSE[5]).abs() < 1e-12
                && (iso.t.z - POSE[6]).abs() < 1e-12
        });

        // The claim lease from an independent description (see [`super::main`]).
        let lease_ok = attached
            .as_ref()
            .ok()
            .and_then(|t| edge_of(t, "base"))
            .and_then(|edge| {
                tf_tree_ipc::Rendezvous::from_env()
                    .and_then(|rv| tf_tree_ipc::LockFile::open(rv.lock_path()))
                    .and_then(|lf| lf.probe_claim(edge))
                    .map(|p| p.held)
                    .ok()
            })
            .unwrap_or(false);

        let parent_ok = offer_ok && read_ok && value_ok && serve_ok && lease_ok;
        println!(
            "child={child} parent_ok={parent_ok} \
             note=offer:{offer_ok},read:{read_ok},value:{value_ok},serve:{serve_ok},lease:{lease_ok}"
        );
        let _ = std::io::stdout().flush();

        drop(attached);
        // SAFETY: a live handle, freed exactly once, on its creating thread. Last, so
        // nothing above observes a torn-down arena.
        unsafe { tft_bridge_free(b) };
    }

    /// The third process: attach with consumer defaults and print the lookup as bit patterns, `ok <16-hex-word>:<…>`
    /// or `error <display>` (`crates/tf_tree_c/src/bin/bridge_reader.rs`'s protocol); a mode of this binary because
    /// `CARGO_BIN_EXE_*` is set only for the declaring package's tests.
    pub(crate) fn read_back() {
        let stamp = std::env::args()
            .nth(2)
            .and_then(|s| s.parse::<i64>().ok())
            .expect("bridge-reader takes <stamp_nanos>");
        let line = match tf_tree::Open::new().open() {
            Ok(tree) => match lookup(&tree, "odom", "base", stamp) {
                Some(iso) => format!("ok {}", bits_of(&iso)),
                None => "error the arena is there but the lookup failed".to_string(),
            },
            Err(e) => format!("error {e}"),
        };
        println!("{line}");
        let _ = std::io::stdout().flush();
    }

    fn read_in_a_third_process(stamp: i64) -> String {
        let exe = std::env::current_exe().expect("current_exe");
        let out = std::process::Command::new(exe)
            .args(["bridge-reader", &stamp.to_string()])
            .stderr(std::process::Stdio::inherit())
            .output()
            .expect("spawn the third process");
        if !out.status.success() {
            return format!("error the third process exited {:?}", out.status.code());
        }
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn offer(b: *mut tft_bridge, stamp: i64) -> (tft_status, tft_bridge_outcome) {
        let (p, c) = (CString::new("odom").unwrap(), CString::new("base").unwrap());
        let s = tft_bridge_sample {
            struct_size: core::mem::size_of::<tft_bridge_sample>() as u32,
            frame_id: p.as_ptr(),
            child_frame_id: c.as_ptr(),
            stamp_nanos: stamp,
            pose: POSE,
            received_steady_nanos: 0,
        };
        let mut out = tft_bridge_outcome::blank();
        // SAFETY: a live handle on its creating thread; the `CString`s outlive
        // the call; `out` is a live local with `struct_size` set.
        let rc = unsafe { tft_bridge_offer(b, TFT_BRIDGE_TOPIC_TF, &s, ptr::null(), &mut out) };
        (rc, out)
    }

    /// `target <- source` at `stamp`, or `None` if unanswerable.
    fn lookup(
        tree: &tf_tree::Tree,
        target: &str,
        source: &str,
        stamp: i64,
    ) -> Option<tf_tree::Iso3> {
        let g = tree.guard();
        let (Ok(t), Ok(s)) = (tree.frame(target), tree.frame(source)) else {
            return None;
        };
        tree.plan(t, s)
            .ok()?
            .at(
                &g,
                tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(stamp),
            )
            .ok()
    }

    fn bits_of(iso: &tf_tree::Iso3) -> String {
        iso.to_bits()
            .iter()
            .map(|w| format!("{w:016x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// The id of the edge whose child frame is `child` (`unstable` tier, `docs/API.md` §2.6).
    fn edge_of(tree: &tf_tree::Tree, child: &str) -> Option<u32> {
        let id = tree.frame(child).ok()?;
        let (_, _, edge, _) = tree.arena_view().topology().read_frame(id)?;
        (edge != 0).then_some(edge)
    }

    fn last_message() -> String {
        let mut e = tft_error::blank();
        // SAFETY: `e` is a live local with `struct_size` set.
        let _ = unsafe { tft_last_error(&mut e) };
        text(e.message.as_ptr())
    }

    fn text(p: *const c_char) -> String {
        if p.is_null() {
            return String::new();
        }
        // SAFETY: ABI strings are NUL-terminated and valid until the next call.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}
