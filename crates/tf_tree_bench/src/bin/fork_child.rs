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
    let mut wstatus: libc::c_int = 0;
    // SAFETY: `waitpid` writes only through the `&mut c_int` supplied, for the
    // duration of the call, and `pid` is the child this process just forked.
    let waited = unsafe { libc::waitpid(pid, &mut wstatus, 0) };
    assert_eq!(waited, pid, "waitpid");

    // `WIFEXITED`/`WIFSIGNALED` are macros in C, so they are decoded here. This
    // is the assertion `0005` singles out: a child that passes every check and
    // then dies in its destructors has still failed.
    let child = if wstatus & 0x7f == 0 {
        format!("exited {}", (wstatus >> 8) & 0xff)
    } else if wstatus & 0x7f == 0x7f {
        format!("stopped {}", (wstatus >> 8) & 0xff)
    } else {
        format!("signalled {}", wstatus & 0x7f)
    };

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

#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn main() {
    println!("child=skipped parent_ok=true note=shm-unavailable");
}
