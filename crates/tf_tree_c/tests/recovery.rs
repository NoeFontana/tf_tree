//! **Recovery from C — `docs/decisions/0044`.**
//!
//! Until these three entry points existed, an all-C++/Python fleet whose arena
//! owner was `SIGKILL`ed **could not rejoin it**. The survivors keep their
//! participant bytes, so `docs/PHASE2.md` §3.4 step 4 refuses every new create
//! with `ArenaHeldButUnreachable`; the one call that ends that state —
//! `Tree::inherit_ownership` — was Rust-only, and worse, took `&mut self` while
//! both bindings hold the tree in an `Arc`. The documented recovery was to stop
//! every attached process. ROS 2 nodes are written in C++ and Python.
//!
//! So this test is the scenario, run through the C ABI: own, join, kill, and
//! recover. It fails on every revision before `0044`, at the point where the
//! symbol does not exist.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::ptr;

use tf_tree_c::{
    tft_tree, tft_tree_free, TFT_CONTENDED, TFT_INHERITED, TFT_NOT_APPLICABLE, TFT_OK,
    TFT_OWNER_ALIVE,
};

/// One runtime directory for the file, for `bridge_shared.rs`'s reason: `set_var`
/// is process-wide, so per-test directories would race.
fn scratch_dir() -> &'static PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let p = std::env::temp_dir().join(format!("tft-c-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        // SAFETY: set before any thread in this test binary opens a rendezvous.
        unsafe { std::env::set_var("TF_TREE_RUNTIME_DIR", &p) };
        p
    })
}

/// The owner, as a **process**: only the kernel can take its locks away without
/// its cooperation, which is the whole state under test.
fn spawn_owner(name: &str) -> Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_arena_owner"))
        .arg(name)
        .env("TF_TREE_RUNTIME_DIR", scratch_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn arena_owner");
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.as_mut().expect("piped stdout"))
        .read_line(&mut line)
        .expect("read arena_owner's first line");
    assert_eq!(line.trim_end(), "owning", "the owner did not come up");
    child
}

/// Open read-write through the C ABI, which is what a C++ node does.
fn open_rw(name: &str) -> *mut tft_tree {
    let cname = std::ffi::CString::new(name).unwrap();
    let mut tree: *mut tft_tree = ptr::null_mut();
    // SAFETY: `cname` outlives the call; `tree` is a writable out-parameter.
    let st = unsafe { tf_tree_c::tft_tree_open_named(cname.as_ptr(), true, &mut tree) };
    assert_eq!(st, TFT_OK, "tft_tree_open_named failed with {st}");
    assert!(!tree.is_null());
    tree
}

/// **The scenario `0044` is about: owner dies, and a C consumer recovers the
/// arena by itself.**
///
/// Four claims, in the order a node would meet them:
///
/// 1. With the owner alive, `tft_tree_owner_lost` is `false` and inheriting
///    reports `TFT_OWNER_ALIVE` — so the loop is cheap and does nothing.
/// 2. With the owner dead, `owner_lost` is `true`.
/// 3. Inheriting then reports `TFT_INHERITED`: **this** process is the owner.
/// 4. And it stops saying the owner is gone, so the loop settles rather than
///    re-attempting the ownership lock every cycle
///    (`docs/decisions/0043`).
/// 5. `tft_tree_reap_dead` then collects **one** record — the dead owner's.
///    Nothing hangs up on an owner, so its `LIVE` record over a kernel-released
///    byte is one of the exactly two states the hangup callback cannot reach,
///    and a C process had no way to collect it. Written as `1` and then `0`
///    rather than "some number": the count is the evidence.
///
/// **Mutant:** none is offered for the symbols themselves — deleting any of the
/// three stops this file compiling, which is the strongest form of the
/// assertion and is exactly the state every revision before `0044` was in.
#[test]
fn a_c_consumer_recovers_an_arena_whose_owner_died() {
    let name = "c_recovery";
    let mut owner = spawn_owner(name);
    let tree = open_rw(name);

    let mut lost = true;
    let mut how = TFT_NOT_APPLICABLE;

    // SAFETY: `tree` is live; both out-parameters are writable.
    unsafe {
        assert_eq!(tf_tree_c::tft_tree_owner_lost(tree, &mut lost), TFT_OK);
        assert!(!lost, "the owner is alive and serving");
        assert_eq!(
            tf_tree_c::tft_tree_inherit_ownership(tree, &mut how),
            TFT_OK
        );
        assert_eq!(
            how, TFT_OWNER_ALIVE,
            "inheriting against a live owner must refuse without touching the lock"
        );
    }

    // `wait` after `kill`, so the kernel has torn the descriptors down — its
    // ownership byte and its participant byte are released with no cooperation.
    owner.kill().expect("kill the owner");
    owner.wait().expect("reap the owner");

    // SAFETY: as above.
    unsafe {
        assert_eq!(tf_tree_c::tft_tree_owner_lost(tree, &mut lost), TFT_OK);
        assert!(lost, "the owner is gone and its socket hung up");
        assert_eq!(
            tf_tree_c::tft_tree_inherit_ownership(tree, &mut how),
            TFT_OK
        );
        assert_eq!(
            how, TFT_INHERITED,
            "the sole read-write survivor should have taken the vacant role \
             (got {how}; TFT_CONTENDED is {TFT_CONTENDED})"
        );

        // And it settles: this process is the owner now, so there is nothing to
        // report and nothing to retry.
        assert_eq!(tf_tree_c::tft_tree_owner_lost(tree, &mut lost), TFT_OK);
        assert!(
            !lost,
            "an owner that reads its own death would retry the lock forever"
        );

        // **And the dead owner's own record is collected, which is one of the
        // exactly two states with no hangup for anybody to observe.** The
        // owner's socket-hangup callback frees a dead *joiner*; nothing hangs up
        // on the owner itself, so its `LIVE` record over a byte the kernel has
        // released outlives it until some survivor sweeps. That survivor could
        // not be a C process before this. The count is asserted rather than
        // ignored because it is the whole reason the entry point exists.
        let mut reaped = u32::MAX;
        assert_eq!(tf_tree_c::tft_tree_reap_dead(tree, &mut reaped), TFT_OK);
        assert_eq!(
            reaped, 1,
            "the dead owner's participant record should have been collected"
        );

        // Idempotent: a second sweep finds nothing, so a supervisor may call it
        // on a timer without the count meaning something different each time.
        assert_eq!(tf_tree_c::tft_tree_reap_dead(tree, &mut reaped), TFT_OK);
        assert_eq!(reaped, 0, "nothing is left, and sweeping again must say so");

        tft_tree_free(tree);
    }
}

/// The three entry points refuse a NULL handle and a NULL out-parameter rather
/// than dereferencing either, which is `docs/PHASE4.md` §3.2's rule for every
/// function in the header.
#[test]
fn the_recovery_entry_points_validate_their_arguments() {
    let mut b = false;
    let mut u8v = 0u8;
    let mut u32v = 0u32;
    // SAFETY: passing NULL is the thing under test; the out-parameters are real.
    unsafe {
        assert_ne!(
            tf_tree_c::tft_tree_owner_lost(ptr::null(), &mut b),
            TFT_OK,
            "a NULL handle must be refused"
        );
        assert_ne!(
            tf_tree_c::tft_tree_inherit_ownership(ptr::null(), &mut u8v),
            TFT_OK
        );
        assert_ne!(
            tf_tree_c::tft_tree_reap_dead(ptr::null(), &mut u32v),
            TFT_OK
        );
    }
}
