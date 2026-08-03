//! The one real `fork()` in this workspace — `docs/decisions/0005` step 9.
//!
//! # Why this bends the unsafe budget, deliberately
//!
//! `docs/PROJECT.md` §5 budgets `unsafe` to `tf_tree_arena` and
//! `tf_tree_core::{buffer, arena_view}`. This file is outside it, and `0005`
//! records the exception rather than quietly taking it.
//!
//! There is no way around it. What has to be tested is `fork()` **without
//! `exec`** — a child that inherits the parent's address space, its `MADV_DONTFORK`
//! hole where the arena used to be, and its open file descriptions with the OFD
//! locks attached to them. `std::process::Command` always `exec`s, which
//! replaces every one of those things; a thread shares them but is not a
//! process and cannot demonstrate a single one of the failure modes. `libc::fork`
//! is the only primitive that produces the state under test.
//!
//! `tf_tree_bench` is `publish = false`, so this ships to nobody.
//!
//! # What is being tested, and what "passing" would look like if it were broken
//!
//! A shared arena is mapped `MADV_DONTFORK` (`docs/PHASE2.md` §7.3): the child
//! has no mapping there. Nothing in the child *notices* — the `Tree` value is
//! byte-identical and its pointers still look like pointers. Two consequences,
//! and the second is the one that bites:
//!
//! 1. Reading the arena in the child is a `SIGSEGV`.
//! 2. **The child does not have to read anything.** `Tree`, `EdgeWriter` and
//!    `Attachment` have destructors, which run at scope exit whether or not the
//!    child ever touched the tree. Some of them release OFD locks — and an OFD
//!    lock belongs to the *open file description*, which the child inherited, so
//!    unlocking in the child releases the **parent's** byte. Others signal an
//!    `eventfd` that the parent's owner thread is waiting on.
//!
//! So the child can sabotage a parent it never interacted with, from another
//! process, leaving nothing in the parent's logs. That is what these modes
//! check, and it is why the parent re-validates *itself* after the child is
//! gone rather than only inspecting the child's exit code.
//!
//! # Modes
//!
//! `api` runs the child's checks and leaves via `_exit`, so no destructor runs.
//! `drop` runs them too and then drops the tree and the writer explicitly.
//! `owned` is `drop` with the writer claimed through
//! [`tf_tree::Tree::claim_owned`] instead of `claim` — `docs/decisions/0017`
//! step 4. That handle owns an `Arc<Tree>` and an `EdgeWriter<'static>`, so it
//! is the one shape whose destructor could plausibly have lost a guard on the
//! way to being owned: the hand-rolled ancestor `0017` exists to delete
//! (`transmute::<EdgeWriter, Publisher>`) dropped both the claim lease and the
//! fork-generation compare, and neither loss is visible from inside the child.
//!
//! `bridge` is the **C ABI** layer above all three —
//! `docs/decisions/0015`'s *Invariants to maintain*. The three modes above hold
//! the Rust shapes a bridge is built out of (`Arc<Tree>` and one
//! [`tf_tree::OwnedWriter`] per declared dynamic edge) and say nothing about the
//! `extern "C"` functions in front of them, which is the layer an `rclcpp` node
//! plus `multiprocessing`'s default `fork` start method actually reaches. It
//! needs a fourth crate edge — `tf_tree_c` with `bridge,shm` — and is therefore
//! behind this crate's own default-off `bridge` feature.
//!
//! # Output protocol
//!
//! One line: `child=<exited N|signalled N> parent_ok=<bool> note=<text>`.
//!
//! `exited` versus `signalled` is load-bearing and not decoration. Remove the
//! `Drop` guard in `Tree` and the child dies of `SIGSEGV` *after* passing every
//! API check — so its "exit code" as `std::process` would report it is
//! meaningless, and a test that only compared a number would go green.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::panic
)]

#[cfg(all(feature = "shm", target_os = "linux"))]
fn main() {
    use std::io::Write;
    use std::sync::Arc;

    use tf_tree::{
        AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, Iso3, LookupError, PushError,
        Stamp, SystemDomain, TreeBuilder,
    };

    /// The two claim shapes, behind one `push`.
    ///
    /// A local enum rather than two copies of the body below: the child's checks
    /// and the parent's re-validation are the assertions under test, and running
    /// a *different* sequence for the owned writer would be testing a different
    /// thing while claiming to compare.
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

        /// The edge the writer itself says it claimed.
        ///
        /// Asked of the writer rather than re-derived from the topology on
        /// purpose: the parent's post-fork re-validation compares this against
        /// what the arena reports, and a value read out of the arena cannot
        /// disagree with the arena.
        fn edge(&self) -> u32 {
            match self {
                Writer::Scoped(w) => w.edge().get(),
                Writer::Owned(w) => w.edge().get(),
            }
        }
    }

    // Exit codes the child uses. Distinct per assertion, so a failure names
    // itself without needing a channel back to the parent.
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

    // **The C ABI modes dispatch before anything below runs.** `bridge` builds
    // its arena through `tft_bridge_create`, not through `Open` here, and
    // `bridge-reader` is the third process and must not create an arena at all —
    // so neither can share the setup the three Rust modes need.
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

    // `Arc` unconditionally, so the three modes differ in exactly one thing —
    // which claim they take. `Tree`'s own methods are reached through `Deref`
    // and behave identically; `claim_owned` is the one that needs the handle.
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

    // Everything the child will inspect must exist *before* the fork, so that
    // what the child holds is genuinely inherited rather than re-derived.
    let plan = tree.plan(parent_frame, child_frame).expect("plan");
    let slot = tree.participant_slot();
    let edge = writer.edge();

    // The fork generation as the parent last saw it. Everything the detachment
    // checks below rely on is downstream of this counter, but none of them can
    // tell how *far* it moved — `detached()` is a comparison against a captured
    // value, so it says "different" for a bump of one and for a bump of two
    // alike. The size of the bump is the only thing that pins
    // `tf_tree_ipc::fork::arm`'s `Once`: registering the handler on every call
    // (and `arm` is called from several constructors) would step the generation
    // by more than one per fork, and an `arm` that registered nothing at all
    // would step it by none. Neither is visible anywhere else in this workspace,
    // because this file owns its only `fork()`.
    let gen_before = tf_tree_ipc::fork::generation();

    // SAFETY: `fork` is called from a single-threaded region of this process —
    // no worker threads of our own have been started, and the only extra thread
    // this process has is the arena's owner-server thread, which the child never
    // touches (that is the property under test). The child branch below performs
    // no allocation before it has established what it needs, and every path out
    // of it ends in `libc::_exit`, which runs no destructors and no atexit
    // handlers — so the child cannot flush the parent's `stdout` buffer twice or
    // re-enter an allocator it may have inherited locked.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");

    if pid == 0 {
        // ---- child -------------------------------------------------------
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
            // Run the destructors, which is the whole point of these modes: the
            // child returns through normal scope exit so `Tree`, the writer and
            // `Attachment` all drop. `_exit` below then skips only the runtime's
            // own teardown.
            //
            // In `owned` mode the writer holds its own `Arc<Tree>`, so this is
            // also the ordering check: the writer must stand itself down
            // *before* the last handle to the mapping goes.
            drop(writer);
            drop(tree);
        }

        // SAFETY: `_exit` terminates immediately without running destructors or
        // atexit handlers. That is exactly what is wanted here — in `drop` mode
        // the destructors under test have already run explicitly, and in the
        // other modes leaving them unrun is the point of comparison.
        unsafe { libc::_exit(status) };
    }

    // ---- parent ----------------------------------------------------------
    let child = wait_for(pid);

    // Now prove the parent is unharmed. Each of these is a distinct sabotage
    // route the child had:
    //
    // * `lookup` — the mapping and the participant record survived `Tree::drop`.
    // * `push` — the claim record still names this process, i.e. no reaper was
    //   handed an edge by a released lease (`ClaimLease::drop`).
    // * `participant_alive(self)` — the participant lock byte is still held, so
    //   the child's inherited description did not release it.
    // * `probe_claim` from a *fresh* description — the claim lease is still
    //   held. See the comment at its definition for why nothing else can see
    //   this.
    //
    // A fresh `open()` is what closes the last one: it goes through the socket,
    // so it fails outright if the child stopped the owner thread by writing the
    // inherited shutdown `eventfd` (`OwnerThread::stop`).
    let lookup_ok = tree
        .lookup("map", "base", Stamp::<SystemDomain>::from_nanos(1_000))
        .is_ok();
    let push_ok = writer.push(3_000, &pose).is_ok();
    let alive_ok = tree.participant_alive(slot);
    // The claim *lease*, asked from an independent open file description.
    //
    // This is the only way to see a released lease from in here. OFD locks are
    // self-blind — a description never conflicts with itself — so the tree's own
    // lock file would report the byte free whether or not it still holds it, and
    // the arena record and `push` both stay perfectly healthy after a lease is
    // lost. Without this check the `ClaimLease::drop` guard would have no test
    // that can fail.
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

/// Reap `pid` and render its wait status as the output protocol's `child=` field.
///
/// **`exited` versus `signalled`, and that distinction is the whole harness.**
/// `WIFEXITED`/`WIFSIGNALED` are macros in C, so the status is decoded here. A
/// child that passes every check and then dies in its destructors — or in a C
/// ABI call that faulted instead of returning — has still failed, and it has no
/// exit code left for a test comparing only a number to look at.
///
/// Shared by every mode rather than written twice: a fourth mode that decoded
/// the status slightly differently would be a fourth mode that could pass a
/// `SIGSEGV`.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn wait_for(pid: libc::pid_t) -> String {
    let mut wstatus: libc::c_int = 0;
    // SAFETY: `waitpid` writes only through the `&mut c_int` supplied, for the
    // duration of the call, and `pid` is the child this process just forked.
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

/// **The C ABI across a `fork()`** — `docs/decisions/0015`'s *Invariants to
/// maintain*, the half its own blockquote says was never built.
///
/// # What the three Rust modes above do not cover
///
/// A bridge's guarded state is exactly `Arc<Tree>` plus one
/// [`tf_tree::OwnedWriter`] per declared dynamic edge, and [`super::main`]'s
/// `owned` mode already forks that pair. What it cannot reach is the
/// `extern "C"` layer in front of it: an `rclcpp` node — or anything using
/// `multiprocessing`, whose start method on Linux is `fork` — calls
/// [`tft_bridge_offer`], [`tft_bridge_get_stats`] and [`tft_bridge_free`], and
/// the property those must have is that they **return** in a forked child rather
/// than faulting in it.
///
/// # Where the arena name comes from, and why it is the default one
///
/// `tft_bridge_options::arena_name` is passed `"default"` rather than something
/// distinctive. That is not laziness: `crates/tf_tree_c/tests/bridge_shared.rs`
/// is where the *name* is the subject, and here a distinctive one would have to
/// be threaded into three places that must agree — the bridge, the third
/// process, and [`tf_tree_ipc::Rendezvous`], which is the only vantage point an
/// OFD claim lease is visible from and which resolves `$TF_TREE_NAME` (absent
/// here) to `"default"`. One name from one source cannot disagree with itself.
///
/// The rendezvous is still per-run: `tests/fork.rs` gives each mode its own
/// `$TF_TREE_RUNTIME_DIR`.
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

    /// The same shape `crates/tf_tree_c/tests/bridge_shared.rs` declares, so a
    /// difference between that test and this one cannot hide behind a different
    /// topology.
    ///
    /// The static edge earns its place here rather than being carried over
    /// verbatim: the bridge claims a writer only for *dynamic* edges, so with it
    /// present `BridgeInner::writers` is a strict subset of the arena's edges —
    /// which is the shape whose destructor `tft_bridge_free` runs in the child.
    /// A topology of one dynamic edge would make the map and the arena
    /// coincide, and coincidences are what hide indexing mistakes.
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

    /// A 30° yaw and a translation nothing else in the fixture shares, so a
    /// read-back that returns identity — or the static edge's pose — fails
    /// rather than coincidentally passing.
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

    /// See [`super::main`]'s block of the same shape: distinct per assertion, so
    /// a failing child names itself without a channel back to the parent. The
    /// numbering starts at 20 to leave the Rust modes' 10–16 alone.
    const OK: i32 = 0;
    const OFFER_NOT_OK: i32 = 20;
    const OFFER_NOT_REJECTED: i32 = 21;
    const OFFER_STATUS_NOT_DETACHED: i32 = 22;
    const STATS_NOT_OK: i32 = 23;
    const STATS_NOT_COUNTED: i32 = 24;
    const GEN_NOT_BUMPED_ONCE: i32 = 25;

    /// The bridge, the fork, the child's three calls, and the parent's
    /// re-validation.
    pub(crate) fn run() {
        let toml = CString::new(TOPO).unwrap();
        // "default" — see the module docs for why this name and not another.
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
        // SAFETY: NUL-terminated config and name, a live full-size `opts`, and
        // `b` a live local.
        let rc = unsafe { tft_bridge_create(toml.as_ptr(), &opts, &mut b) };
        assert_eq!(
            rc,
            TFT_OK,
            "tft_bridge_create with an arena_name: {}",
            last_message()
        );
        assert!(!b.is_null());

        // One transform before the fork, so the child inherits a bridge that has
        // already written — a claim taken, a lease held, a sample in the ring.
        let (rc, out) = offer(b, 1_000 * MS);
        assert_eq!(rc, TFT_OK, "the first offer: {}", last_message());
        assert_eq!(
            out.action,
            TFT_BRIDGE_APPLIED,
            "the fixture edge must be applied before the fork: {}",
            text(out.detail)
        );

        let gen_before = tf_tree_ipc::fork::generation();

        // SAFETY: the same argument as [`super::main`]'s `fork`, which this file
        // exists for and which `docs/decisions/0005` step 9 records the
        // exception for. The one addition: the bridge's arena is a shared one, so
        // this process also carries the owner-server thread — the child never
        // touches it, which is half of what is under test, and every path out of
        // the child branch ends in `libc::_exit`.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");

        if pid == 0 {
            // ---- child ---------------------------------------------------
            //
            // **Every line here is a call that must return.** A `SIGSEGV` in any
            // of them is reported by the parent as `signalled`, which is the
            // whole reason this harness decodes the wait status instead of
            // comparing an exit code.
            let mut status = OK;

            let (rc, out) = offer(b, 2_000 * MS);
            let mut stats = blank_stats();
            // **Called unconditionally, not inside the chain below.** All three
            // entry points under test must be *reached* in the child; the chain
            // only classifies what they answered. Folding this into an
            // `else if` would skip it whenever the offer misbehaved — which is
            // exactly the run in which a second faulting entry point would
            // matter most.
            //
            // SAFETY: a live handle on its creating thread — the token is a
            // thread-local counter, which a forked child inherits unchanged —
            // and `stats` is a live local with `struct_size` set.
            let stats_rc = unsafe { tft_bridge_get_stats(b, &mut stats) };
            if rc != TFT_OK {
                // The *call* was well-formed — a live handle on its creating
                // thread, current `struct_size`s, a UTF-8 name. `tft_bridge_offer`
                // documents its return value as answering that question and
                // nothing else, so a detached writer must arrive on the outcome.
                status = OFFER_NOT_OK;
            } else if out.action != TFT_BRIDGE_REJECTED {
                status = OFFER_NOT_REJECTED;
            } else if out.status != TFT_ERR_CHILD_DETACHED {
                status = OFFER_STATUS_NOT_DETACHED;
            } else if stats_rc != TFT_OK {
                status = STATS_NOT_OK;
            } else if stats.rejected_by_arena != 1 || stats.applied != 1 {
                // The counters the C layer owns, read back in the child: the
                // refusal above was *counted* as a refusal, and the offer before
                // the fork is still the only applied one. A `get_stats` that
                // returned `TFT_OK` having read nothing would pass the line
                // above and fail this one.
                status = STATS_NOT_COUNTED;
            } else if tf_tree_ipc::fork::generation().wrapping_sub(gen_before) != 1 {
                // The same pin as the Rust modes'. It is worth repeating here
                // because the arena was created by `tft_bridge_create` rather
                // than by an `Open` in this file: if that path ever stopped
                // arming the handler, every check above would still pass —
                // `detached()` compares a generation nothing moved — and this is
                // the only line that would notice.
                status = GEN_NOT_BUMPED_ONCE;
            }

            // **The destructor half, through the ABI.** `tft_bridge_free` drops
            // the `BTreeMap<String, OwnedWriter>` and the `Arc<TreeShare>`
            // behind it, which between them own every claim lease, the
            // participant slot, the owner thread and the mapping — all of them
            // the *parent's*. It returns `void`, so unlike the two calls above
            // there is no status to inspect: what it has to do is come back at
            // all, and leave the parent's leases alone. The parent's `lease:`
            // and `serve:` fields are where that is observed.
            //
            // SAFETY: a live handle, freed exactly once, on its creating thread.
            unsafe { tft_bridge_free(b) };

            // SAFETY: `_exit` terminates without running destructors or atexit
            // handlers. The destructors under test have already run, inside
            // `tft_bridge_free`.
            unsafe { libc::_exit(status) };
        }

        // ---- parent ------------------------------------------------------
        let child = super::wait_for(pid);

        // **The parent's bridge still applies an offer.** The first half of what
        // the record asks the parent to prove, and it goes through the same
        // `extern "C"` entry point the child was just refused at.
        let (rc, out) = offer(b, 3_000 * MS);
        let offer_ok = rc == TFT_OK && out.action == TFT_BRIDGE_APPLIED;

        // A fresh attach, read-only, with `Open::new()`'s consumer defaults. It
        // goes through the rendezvous socket, so it fails outright if the child
        // stopped the owner thread by writing the inherited shutdown `eventfd`.
        let attached = tf_tree::Open::new().open();
        let serve_ok = attached.is_ok();

        // **The second half: the arena is still readable from a third process.**
        // The child was the second. This one is this executable re-`exec`ed, so
        // it shares no address space, no mapping and no open file description
        // with the bridge, and finds the arena from `$TF_TREE_RUNTIME_DIR` and
        // the default name alone.
        //
        // What it reads is the transform the parent published *after* the child
        // died, at 3 000 ms — so one comparison carries both halves of the
        // record's sentence.
        let line = read_in_a_third_process(3_000 * MS);
        let their_bits = line.strip_prefix("ok ").unwrap_or_default().to_string();
        // The control, in this process, compared bit for bit: a comparison that
        // rounds is a comparison that can agree while the memory does not.
        let ours = attached
            .as_ref()
            .ok()
            .and_then(|t| lookup(t, "odom", "base", 3_000 * MS));
        let read_ok = !their_bits.is_empty() && ours.as_ref().map(bits_of) == Some(their_bits);
        // And the bytes are the pose that was offered, not merely a value two
        // readers agree on.
        let value_ok = ours.is_some_and(|iso| {
            (iso.t.x - POSE[4]).abs() < 1e-12
                && (iso.t.y - POSE[5]).abs() < 1e-12
                && (iso.t.z - POSE[6]).abs() < 1e-12
        });

        // The claim *lease*, asked from an independent open file description —
        // see [`super::main`]'s comment at the same check for why nothing else in
        // this process can see it. The edge is read out of the topology rather
        // than guessed: `tft_bridge_create` assigns ids from the config, and a
        // hardcoded 1 would keep passing if it ever assigned them differently.
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
        // SAFETY: a live handle, freed exactly once, on its creating thread.
        // Last, so nothing above observes an arena this call has torn down.
        unsafe { tft_bridge_free(b) };
    }

    /// The third process: attach with the consumer defaults and print the
    /// lookup as bit patterns.
    ///
    /// One line on stdout, `ok <16-hex-word>:<…>` or `error <display>`, which is
    /// `crates/tf_tree_c/src/bin/bridge_reader.rs`'s protocol. It is a mode of
    /// this binary rather than that one because `CARGO_BIN_EXE_*` is set only for
    /// the tests of the package that declares the binary, and this test is
    /// `tf_tree_bench`'s.
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

    /// Run this executable again, as a genuinely separate process.
    ///
    /// The environment is inherited, so the child resolves the same
    /// `$TF_TREE_RUNTIME_DIR` — and the same rendezvous domain — as the bridge,
    /// with the arena name being the default on both sides.
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

    /// Offer one `/tf` transform through the ABI, returning the call's status
    /// and the outcome it filled.
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
        let mut out = tft_bridge_outcome {
            struct_size: core::mem::size_of::<tft_bridge_outcome>() as u32,
            // SAFETY: `tft_bridge_outcome` is `#[repr(C)]`, `Copy`, and made of
            // integers, `f64` arrays and pointers, so all-zero is a valid value
            // of it. The ABI overwrites every field before it returns.
            ..unsafe { core::mem::zeroed() }
        };
        // SAFETY: a live handle on its creating thread; the `CString`s outlive
        // the call; `out` is a live local with `struct_size` set.
        let rc = unsafe { tft_bridge_offer(b, TFT_BRIDGE_TOPIC_TF, &s, ptr::null(), &mut out) };
        (rc, out)
    }

    fn blank_stats() -> tft_bridge_stats {
        tft_bridge_stats {
            struct_size: core::mem::size_of::<tft_bridge_stats>() as u32,
            // SAFETY: `tft_bridge_stats` is `#[repr(C)]`, `Copy` and made
            // entirely of integers, so all-zero is a valid value of it.
            ..unsafe { core::mem::zeroed() }
        }
    }

    /// `target <- source` at `stamp`, or `None` for any reason it could not be
    /// answered — an absent frame, no plan, no sample.
    ///
    /// **One lookup behind both the third process's answer and this process's
    /// control.** Two spellings of the same query would let the comparison in
    /// [`run`] pass because the two sides asked different questions.
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

    /// A transform as bit patterns. A comparison that rounds is a comparison
    /// that can agree while the memory does not.
    fn bits_of(iso: &tf_tree::Iso3) -> String {
        iso.to_bits()
            .iter()
            .map(|w| format!("{w:016x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// The id of the edge whose child frame is `child`, read out of the
    /// topology block. `tf_tree`'s `unstable` tier, which this crate already
    /// takes for the same reason (`docs/API.md` §2.6).
    fn edge_of(tree: &tf_tree::Tree, child: &str) -> Option<u32> {
        let id = tree.frame(child).ok()?;
        let (_, _, edge, _) = tree.arena_view().topology().read_frame(id)?;
        (edge != 0).then_some(edge)
    }

    /// This thread's last error message, as Rust text.
    fn last_message() -> String {
        let mut e = tft_error {
            struct_size: core::mem::size_of::<tft_error>() as u32,
            // SAFETY: `tft_error` is `#[repr(C)]`, `Copy`, and made entirely of
            // integers and a byte array, so all-zero is a valid value of it.
            ..unsafe { core::mem::zeroed() }
        };
        // SAFETY: `e` is a live local with `struct_size` set.
        let _ = unsafe { tft_last_error(&mut e) };
        text(e.message.as_ptr())
    }

    fn text(p: *const c_char) -> String {
        if p.is_null() {
            return String::new();
        }
        // SAFETY: the ABI contracts every string it hands out is NUL-terminated
        // and valid until the next call on the handle; nothing intervenes here.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}
