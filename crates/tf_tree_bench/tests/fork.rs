//! Fork poisoning — `docs/decisions/0005` step 9.
//!
//! `src/bin/fork_child.rs` does the work; this gate runs it in a scratch runtime
//! directory and holds it to one line. `child=exited 0` (not merely `0`: a
//! signalled child has no exit code) means every API refused with
//! `ChildDetached` and destructors ran cleanly. `parent_ok=true` means the
//! parent survived the child's inherited open file descriptions: lease, owner
//! thread, socket, lookup and push.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

/// A scratch runtime directory, removed on drop.
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

/// Every guarded entry point refuses the child; it exits via `_exit`, so no
/// destructor runs.
#[test]
fn a_forked_child_is_refused_by_every_entry_point() {
    assert_eq!(
        run("api", "api"),
        "child=exited 0 parent_ok=true note=lookup:true,push:true,alive:true,serve:true,lease:true"
    );
}

/// The child drops its `EdgeWriter` and `Tree`, which own the parent's lease,
/// slot, owner thread and mapping.
///
/// Mutants: without the `Tree::drop` guard `child=signalled 11`; without the
/// `ClaimLease::drop` guard `push:false`; without the `OwnerThread::stop` guard
/// `serve:false`.
#[test]
fn a_forked_child_runs_its_destructors_without_touching_the_parent() {
    assert_eq!(
        run("drop", "drop"),
        "child=exited 0 parent_ok=true note=lookup:true,push:true,alive:true,serve:true,lease:true"
    );
}

/// Same for an owned writer — `docs/decisions/0017` step 4. The loss is invisible
/// from the child, so the parent's `lease:true` (probed from an independent open
/// file description) carries the test.
///
/// Mutants: omitting the fork-generation compare in `ClaimLease::drop` gives
/// `lease:false`; keeping only the `Publisher` inside `OwnedWriter`, or omitting
/// `Publisher::abandon` on the detached path, gives `child=signalled 11`.
#[test]
fn a_forked_child_stands_down_an_owned_writer_too() {
    assert_eq!(
        run("owned", "owned"),
        "child=exited 0 parent_ok=true note=lookup:true,push:true,alive:true,serve:true,lease:true"
    );
}

/// The C ABI across a `fork()` — `docs/decisions/0015`, *Invariants to maintain*.
///
/// The child calls `tft_bridge_offer`, `tft_bridge_get_stats` and
/// `tft_bridge_free` on an inherited handle; `child=exited 0` means all returned
/// (a fault in an `extern "C"` body leaves no exit code). `parent_ok=true` means
/// the parent's offer still applies, a third process reads it back bit for bit,
/// the rendezvous still serves, and the leases are still held.
///
/// Mutants: `if false && self.detached()` in `EdgeWriter::push` gives
/// `child=signalled 11`; deleting the `PushError::ChildDetached` arm of
/// `map::push` in `crates/tf_tree_c/src/publisher.rs` gives `child=exited 22`.
/// Removing the `guard()` `catch_unwind` in `crates/tf_tree_c/src/error.rs` does
/// not fail it: nothing panics in the child.
/// returning — which is why the two mutants above target the detach check and
/// the error mapping, and not `catch_unwind`.
#[cfg(feature = "bridge")]
#[test]
fn a_forked_child_is_refused_by_every_bridge_entry_point() {
    assert_eq!(
        run("bridge", "bridge"),
        "child=exited 0 parent_ok=true note=offer:true,read:true,value:true,serve:true,lease:true"
    );
}
