//! Recovery from C (`docs/decisions/0044`): own, join, kill, recover, through
//! the C ABI.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// `docs/decisions/0007` rule 1, kind 5: our own C ABI called from Rust; a test
// is a separate crate root, so the posture is declared here (`0048`).
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::ptr;

use tf_tree_c::{
    tft_tree, tft_tree_free, TFT_CONTENDED, TFT_INHERITED, TFT_NOT_APPLICABLE, TFT_OK,
    TFT_OWNER_ALIVE,
};

/// One runtime directory for the file: `set_var` is process-wide.
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

/// The owner as a process: only the kernel can release its locks uncooperatively.
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

/// Owner dies; a C consumer recovers the arena itself.
///
/// Pins: owner alive gives `owner_lost == false` and `TFT_OWNER_ALIVE`; owner dead
/// gives `true` and `TFT_INHERITED`; afterwards `owner_lost` settles to `false`
/// (`0043`); `tft_tree_reap_dead` collects exactly the dead owner's record (`1`,
/// then `0`).
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

    // `wait` after `kill`, so the kernel has released the owner's bytes.
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

        // Settled: this process is the owner now.
        assert_eq!(tf_tree_c::tft_tree_owner_lost(tree, &mut lost), TFT_OK);
        assert!(
            !lost,
            "an owner that reads its own death would retry the lock forever"
        );

        // Nothing hangs up on an owner, so its record survives until a sweep.
        let mut reaped = u32::MAX;
        assert_eq!(tf_tree_c::tft_tree_reap_dead(tree, &mut reaped), TFT_OK);
        assert_eq!(
            reaped, 1,
            "the dead owner's participant record should have been collected"
        );

        // Idempotent.
        assert_eq!(tf_tree_c::tft_tree_reap_dead(tree, &mut reaped), TFT_OK);
        assert_eq!(reaped, 0, "nothing is left, and sweeping again must say so");

        tft_tree_free(tree);
    }
}

/// NULL handles and out-parameters are refused (`docs/PHASE4.md` §3.2).
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
