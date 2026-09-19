//! The one real `fork()` in this workspace — `docs/decisions/0005` step 9.
//!
//! Why this bends the unsafe budget
//!
//! `docs/decisions/0007` rule 1 kinds 2 and 5 (`docs/decisions/0048`): the OS
//! (`fork`, `_exit`, `waitpid`) and our own C ABI driven across that fork.
//! `scripts/unsafe-budget.txt` records it. What is tested is `fork()` **without
//! `exec`**: the child inherits the address space, the `MADV_DONTFORK` hole where
//! the arena was, and open file descriptions with their OFD locks. `Command` execs
//! and a thread is not a process, so `libc::fork` is the only primitive that
//! produces the state under test.
//!
//! # What is being tested
//!
//! A shared arena is mapped `MADV_DONTFORK` (`docs/PHASE2.md` §7.3), so the child
//! has no mapping there and nothing in it notices. Reading the arena in the child
//! is a `SIGSEGV`; worse, **the child need not read anything**: `Tree`,
//! `EdgeWriter` and `Attachment` destructors release OFD locks that belong to the
//! inherited open file description (the **parent's** byte) and signal an `eventfd`
//! the parent's owner thread waits on. So the parent re-validates itself after the
//! child is gone rather than only checking the child's exit code.
//!
//! # Modes
//!
//! `api` leaves via `_exit`, so no destructor runs. `drop` also drops the tree and
//! writer explicitly. `owned` is `drop` with the writer claimed through
//! [`tf_tree::Tree::claim_owned`] (`docs/decisions/0017` step 4): the shape whose
//! destructor could lose a claim-lease or fork-generation guard invisibly.
//! `bridge` is the C ABI layer (`docs/decisions/0015` *Invariants to maintain*)
//! behind this crate's default-off `bridge` feature.
//!
//! # Output protocol
//!
//! One line: `child=<exited N|signalled N> parent_ok=<bool> note=<text>`.
//! `exited` versus `signalled` is load-bearing: without the `Drop` guard the child
//! dies of `SIGSEGV` *after* passing every API check, and a bare number would go
//! green.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::panic
)]
// **`docs/decisions/0007` rule 1, kinds 2 and 5** — the OS, and our own C ABI
// across the fork. Declared here because a bin is a separate crate root:
// `lib.rs`'s `#![forbid(unsafe_code)]` governs none of this file.
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

        /// The edge the writer says it claimed, not re-derived from the arena.
        fn edge(&self) -> u32 {
            match self {
                Writer::Scoped(w) => w.edge().get(),
                Writer::Owned(w) => w.edge().get(),
            }
        }
    }

    // Exit codes the child uses; distinct per assertion.
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

    // The C ABI modes dispatch first: they build their own arena and `bridge-reader`
    // must create none.
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

    // The three modes differ only in which claim they take.
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

    // Everything the child inspects exists before the fork, so it is inherited.
    let plan = tree.plan(parent_frame, child_frame).expect("plan");
    let slot = tree.participant_slot();
    let edge = writer.edge();

    // The fork generation as last seen. The size of the bump (exactly one) is what
    // pins `tf_tree_ipc::fork::arm`'s `Once`; this file owns the workspace's only
    // `fork()`.
    let gen_before = tf_tree_ipc::fork::generation();

    // SAFETY: `fork` runs with no worker threads of ours; the arena's owner-server
    // thread is never touched by the child. Every child path ends in `libc::_exit`,
    // so no destructor or atexit handler runs and no inherited lock is re-entered.
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
            // Run the destructors via normal scope exit; `_exit` skips only runtime
            // teardown. In `owned` mode this also checks the writer stands down before the
            // last handle to the mapping goes.
            drop(writer);
            drop(tree);
        }

        // SAFETY: `_exit` runs no destructors or atexit handlers, as wanted.
        unsafe { libc::_exit(status) };
    }

    let child = wait_for(pid);

    // Prove the parent is unharmed: `lookup` (mapping and record survived),
    // `push` (claim still names this process), `participant_alive(self)` (lock byte
    // still held), `probe_claim` from a fresh description (lease still held), and a
    // fresh `open()` (fails if the child stopped the owner thread via the inherited
    // shutdown `eventfd`).
    let lookup_ok = tree
        .lookup("map", "base", Stamp::<SystemDomain>::from_nanos(1_000))
        .is_ok();
    let push_ok = writer.push(3_000, &pose).is_ok();
    let alive_ok = tree.participant_alive(slot);
    // The claim lease, asked from an independent open file description: OFD locks
    // are self-blind, so nothing else here can see a released lease.
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

/// Reap `pid` and render its wait status as the `child=` field. `exited` versus
/// `signalled` is the whole harness; every mode shares this decoder.
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

/// **The C ABI across a `fork()`** — `docs/decisions/0015` *Invariants to
/// maintain*.
///
/// A forked `rclcpp` node or `multiprocessing` process calls
/// [`tft_bridge_offer`], [`tft_bridge_get_stats`] and [`tft_bridge_free`], which
/// must **return** in the child rather than fault. The arena name is `"default"`:
/// one name from one source (bridge, third process, [`tf_tree_ipc::Rendezvous`])
/// cannot disagree. The rendezvous is per-run (`tests/fork.rs` sets
/// `$TF_TREE_RUNTIME_DIR`).
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

    /// The topology `crates/tf_tree_c/tests/bridge_shared.rs` declares. The static
    /// edge makes `BridgeInner::writers` a strict subset of the arena's edges.
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

    /// A pose nothing else shares, so an identity read-back fails.
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

    /// Numbering starts at 20, clear of the Rust modes' 10-16.
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
        // "default" — see the module docs.
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

        // One transform before the fork, so the child inherits a bridge that has written.
        let (rc, out) = offer(b, 1_000 * MS);
        assert_eq!(rc, TFT_OK, "the first offer: {}", last_message());
        assert_eq!(
            out.action,
            TFT_BRIDGE_APPLIED,
            "the fixture edge must be applied before the fork: {}",
            text(out.detail)
        );

        let gen_before = tf_tree_ipc::fork::generation();

        // SAFETY: as [`super::main`]'s `fork`, plus the owner-server thread, which the
        // child never touches; every child path ends in `libc::_exit`.
        //
        // **This child allocates**: `offer()` builds `CString`s, the rejected arm goes
        // through `error::last_message()`, and `tft_bridge_free` drops a `BTreeMap`.
        // Relied on: glibc's `pthread_atfork` handlers reinitialise the malloc arena in
        // the child (a glibc guarantee, not POSIX). A libc without it needs the calls
        // split across three `fork()`s.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");

        if pid == 0 {
            // Every call here must return; a `SIGSEGV` is reported as `signalled`.
            let mut status = OK;

            let (rc, out) = offer(b, 2_000 * MS);
            let mut stats = tft_bridge_stats::blank();
            // Called unconditionally, not in the chain below, so all three entry points
            // are reached even when the offer misbehaves.
            // SAFETY: a live handle on its creating thread (the token is a thread-local
            // counter, inherited unchanged); `stats` is live with `struct_size` set.
            let stats_rc = unsafe { tft_bridge_get_stats(b, &mut stats) };
            if rc != TFT_OK {
                // The call was well-formed, so a detached writer must arrive on the outcome.
                status = OFFER_NOT_OK;
            } else if out.action != TFT_BRIDGE_REJECTED {
                status = OFFER_NOT_REJECTED;
            } else if out.status != TFT_ERR_CHILD_DETACHED {
                status = OFFER_STATUS_NOT_DETACHED;
            } else if stats_rc != TFT_OK {
                status = STATS_NOT_OK;
            } else if stats.rejected_by_arena != 1 || stats.applied != 1 {
                // The refusal was counted as a refusal and the pre-fork offer is the only
                // applied one.
                status = STATS_NOT_COUNTED;
            } else if tf_tree_ipc::fork::generation().wrapping_sub(gen_before) != 1 {
                // The Rust modes' generation pin, repeated: this arena comes from
                // `tft_bridge_create`, and if that stopped arming the handler nothing else here
                // would notice.
                status = GEN_NOT_BUMPED_ONCE;
            }

            // The destructor half: `tft_bridge_free` drops every claim lease, the
            // participant slot, the owner thread and the mapping, all the parent's. It returns
            // `void`, so it must come back and leave the parent's leases alone (the parent's
            // `lease:` and `serve:` fields observe that).
            //
            // SAFETY: a live handle, freed exactly once, on its creating thread.
            unsafe { tft_bridge_free(b) };

            // SAFETY: `_exit` runs no destructors; they ran inside `tft_bridge_free`.
            unsafe { libc::_exit(status) };
        }

        let child = super::wait_for(pid);

        // The parent's bridge still applies an offer through the same entry point.
        let (rc, out) = offer(b, 3_000 * MS);
        let offer_ok = rc == TFT_OK && out.action == TFT_BRIDGE_APPLIED;

        // A fresh read-only attach with the consumer defaults.
        let attached = tf_tree::Open::new().open();
        let serve_ok = attached.is_ok();

        // The arena is still readable from a third process (this executable
        // re-`exec`ed: no shared mapping or descriptions), reading the transform
        // published after the child died.
        let line = read_in_a_third_process(3_000 * MS);
        let their_bits = line.strip_prefix("ok ").unwrap_or_default().to_string();
        // The control, compared bit for bit.
        let ours = attached
            .as_ref()
            .ok()
            .and_then(|t| lookup(t, "odom", "base", 3_000 * MS));
        let read_ok = !their_bits.is_empty() && ours.as_ref().map(bits_of) == Some(their_bits);
        // The bytes are the pose offered, rotation as well as translation; `read_ok`
        // cannot stand in, since both readers could agree on a wrong rotation.
        let value_ok = ours.is_some_and(|iso| {
            (iso.q.w - POSE[0]).abs() < 1e-12
                && (iso.q.x - POSE[1]).abs() < 1e-12
                && (iso.q.y - POSE[2]).abs() < 1e-12
                && (iso.q.z - POSE[3]).abs() < 1e-12
                && (iso.t.x - POSE[4]).abs() < 1e-12
                && (iso.t.y - POSE[5]).abs() < 1e-12
                && (iso.t.z - POSE[6]).abs() < 1e-12
        });

        // The claim lease from an independent description (see [`super::main`]); the
        // edge is read from the topology, not hardcoded.
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

    /// The third process: attach with the consumer defaults and print the lookup as
    /// bit patterns, `ok <16-hex-word>:<…>` or `error <display>`
    /// (`crates/tf_tree_c/src/bin/bridge_reader.rs`'s protocol). A mode of this binary
    /// because `CARGO_BIN_EXE_*` is set only for the declaring package's tests.
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

    /// Re-run this executable as a separate process; the environment is inherited.
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

    /// Offer one `/tf` transform through the ABI; returns the status and outcome.
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

    /// `target <- source` at `stamp`, or `None` if unanswerable. One lookup serves
    /// both the third process and the control.
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

    /// A transform as bit patterns, so rounding cannot hide a difference.
    fn bits_of(iso: &tf_tree::Iso3) -> String {
        iso.to_bits()
            .iter()
            .map(|w| format!("{w:016x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// The id of the edge whose child frame is `child`, from the topology block
    /// (`unstable` tier, `docs/API.md` §2.6).
    fn edge_of(tree: &tf_tree::Tree, child: &str) -> Option<u32> {
        let id = tree.frame(child).ok()?;
        let (_, _, edge, _) = tree.arena_view().topology().read_frame(id)?;
        (edge != 0).then_some(edge)
    }

    /// This thread's last error message, as Rust text.
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
