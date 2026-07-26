//! Fork poisoning — `docs/decisions/0005` step 9.
//!
//! The work happens in `src/bin/fork_child.rs`, which owns the workspace's only
//! `fork()` and explains why it needs one. This file is the gate: it runs that
//! binary in a scratch runtime directory and holds it to a single line.
//!
//! # What each assertion is worth
//!
//! `child=exited 0` — the child recognised itself as detached and every API it
//! touched refused with `ChildDetached` rather than reading the vanished
//! mapping.
//!
//! **`exited`, not merely `0`** — this is the assertion `0005` singles out. The
//! child passes every API check *before* its destructors run, so removing the
//! `Drop` guard leaves a child that answers correctly and then dies of
//! `SIGSEGV`. A test comparing only an exit code would report that as a pass,
//! because a signalled process has no exit code to compare.
//!
//! `parent_ok=true` — the harder half, and the one that motivated the guards on
//! `ClaimLease`, `OwnerThread` and `MappedArena` that `0005` §7 does not
//! mention. The child inherits *open file descriptions*, and an OFD lock belongs
//! to the description rather than to the process: an unlock issued in the child
//! releases the **parent's** byte. So a child that never touches the tree can
//! still release the parent's claim lease, shut down the parent's owner thread
//! by writing an inherited `eventfd`, and unlink the parent's socket. The parent
//! re-validates itself afterwards — lookup, push, its own liveness, and a fresh
//! `open()` through the socket — because none of that is visible in the child's
//! exit status.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

/// A scratch runtime directory, removed when the test ends, so a failure cannot
/// leave an arena behind in the shared `/tmp` location.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("tf_tree_fork-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(mode: &str, tag: &str) -> String {
    let scratch = Scratch::new(tag);
    let out = Command::new(env!("CARGO_BIN_EXE_fork_child"))
        .arg(mode)
        .env("TF_TREE_RUNTIME_DIR", &scratch.0)
        .output()
        .expect("run fork_child");
    assert!(
        out.status.success(),
        "fork_child itself failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The child reaches every guarded entry point and is refused by all of them.
///
/// It exits via `_exit`, so no destructor runs: this isolates the API checks
/// from the destructor checks below. If this fails and
/// [`a_forked_child_runs_its_destructors_without_touching_the_parent`] passes,
/// the poison is being applied in `Drop` alone.
#[test]
fn a_forked_child_is_refused_by_every_entry_point() {
    assert_eq!(
        run("api", "api"),
        "child=exited 0 parent_ok=true note=lookup:true,push:true,alive:true,serve:true,lease:true"
    );
}

/// **The destructor test.** The child drops its `EdgeWriter` and its `Tree`,
/// which between them own a claim lease, a participant slot, an owner thread and
/// a mapping — all of them the *parent's*.
///
/// Mutants this fails against:
///
/// * remove the `Tree::drop` guard ⇒ `child=signalled 11`, a store into the
///   unmapped participant record;
/// * remove the `ClaimLease::drop` guard ⇒ `parent_ok=false` with `push:false`,
///   because the parent's lease was released and its record reaped;
/// * remove the `OwnerThread::stop` guard ⇒ `parent_ok=false` with
///   `serve:false`, because the child's write to the inherited shutdown
///   `eventfd` stopped the parent's server, and the hang it would otherwise
///   cause (joining a thread this process never had) is why the guard clears
///   the `JoinHandle` too.
#[test]
fn a_forked_child_runs_its_destructors_without_touching_the_parent() {
    assert_eq!(
        run("drop", "drop"),
        "child=exited 0 parent_ok=true note=lookup:true,push:true,alive:true,serve:true,lease:true"
    );
}
