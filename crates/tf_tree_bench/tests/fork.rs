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

/// **The same, for a writer that owns its tree** — `docs/decisions/0017` step 4.
///
/// `OwnedWriter` is an `EdgeWriter<'static>` beside an `Arc<Tree>`, and the
/// reason that record exists is that both hand-rolled versions of this shape got
/// the destructor wrong: one of them was a `transmute::<EdgeWriter, Publisher>`
/// which kept the first field and silently discarded the claim lease and the
/// fork-generation compare. **Neither loss is visible from inside the child** —
/// it passes every API check either way — so the assertion that carries this
/// test is `lease:true` in the parent, probed from an independent open file
/// description after the child is gone.
///
/// It also checks an ordering the scoped writer cannot have: the owned writer
/// holds its own strong reference to the tree, so the child drops *two* handles
/// to a mapping that is not there, in an order this type chose.
///
/// Mutants this fails against, each the shape of something that already shipped:
///
/// * omit the fork-generation compare in `ClaimLease::drop` (`if false` in place
///   of the comparison). **Applied, observed:** `child=exited 0 parent_ok=false
///   note=lookup:true,push:true,alive:true,serve:true,lease:false` — the child's
///   inherited open file description released the **parent's** byte, leaving a
///   live writer whose edge any reaper is now free to take. Note `push:true` in
///   that run: the arena record is untouched, so nothing inside the parent
///   notices, which is exactly why the probe is here;
/// * keep only the `Publisher` inside `OwnedWriter` and forget the rest of the
///   `EdgeWriter` — the literal shape of the `transmute` that shipped.
///   **Applied, observed:** `child=signalled 11 parent_ok=true …
///   lease:true`, *not* `lease:false`. Both halves of that are the defect:
///   the child faults because the `Publisher` it now holds has no fork guard in
///   front of its destructor's `compare_exchange`, and the parent's probe reads
///   `lease:true` because the lease was dropped on the floor at claim time and
///   so is **never** released — a byte held for the life of the process by
///   nothing. An earlier revision of this note predicted `lease:false` here; it
///   was written from intent and the run says otherwise, which is the whole
///   reason these are stated as observations;
/// * omit `Publisher::abandon` on the detached path (`EdgeWriter::drop`).
///   **Applied, observed:** `child=signalled 11 parent_ok=true …
///   lease:true` — a `compare_exchange` into the unmapped arena. The scoped
///   sibling test above fails identically, which is the point: this mutant is
///   not specific to the owned shape, and the one above is.
#[test]
fn a_forked_child_stands_down_an_owned_writer_too() {
    assert_eq!(
        run("owned", "owned"),
        "child=exited 0 parent_ok=true note=lookup:true,push:true,alive:true,serve:true,lease:true"
    );
}

/// **The C ABI across a `fork()`** — `docs/decisions/0015`'s *Invariants to
/// maintain*, which says in as many words that this test does not exist and that
/// the record is `ready` rather than `implemented` because of it.
///
/// The three tests above cover the Rust shapes a bridge is built out of:
/// `BridgeInner` holds an `Arc<Tree>` and one `tf_tree::OwnedWriter` per declared
/// dynamic edge, which is exactly the pair
/// [`a_forked_child_stands_down_an_owned_writer_too`] forks. **The layer they say
/// nothing about is the `extern "C"` one in front of it**, and that is the layer
/// a robot reaches: an `rclcpp` node, or anything built on `multiprocessing`,
/// whose start method on Linux is `fork`.
///
/// So the child here calls `tft_bridge_offer`, `tft_bridge_get_stats` and
/// `tft_bridge_free` on a handle it inherited, and the assertion is `child=exited
/// 0` — every one of them came back. `child=signalled 11` is the failure this is
/// for, and it is the reason the harness decodes the wait status rather than
/// comparing an exit code: a fault inside an `extern "C"` body leaves no exit
/// code to compare.
///
/// `parent_ok=true` carries the second half:
///
/// * `offer:` — the parent's bridge still applies a transform, through the same
///   entry point the child was refused at;
/// * `read:` and `value:` — a **third** process attaches to the arena with
///   `Open::new()`'s consumer defaults and reads back that post-fork transform,
///   bit for bit against this process's own lookup;
/// * `serve:` — a fresh attach still resolves the rendezvous socket, so the
///   child's `tft_bridge_free` did not stop the parent's owner thread with the
///   inherited shutdown `eventfd`;
/// * `lease:` — the claim leases behind the bridge's `OwnedWriter`s are still
///   held, probed from an independent open file description because an OFD lock
///   is self-blind and nothing else can see a released one.
///
/// # Mutants this fails against
///
/// * **the detach check on the write path**, which is the mutant this test is
///   worth having for: `if false && self.detached()` in `EdgeWriter::push`
///   (`crates/tf_tree/src/tree.rs`), the one guard every bridge write goes
///   through — `OwnedWriter::push` forwards to it and `write_sample` calls that.
///   The child's offer then stores into the `MADV_DONTFORK` hole instead of
///   returning. **Applied, observed:**
///
///   ```text
///     left: "child=signalled 11 parent_ok=true note=offer:true,read:true,value:true,serve:true,lease:true"
///    right: "child=exited 0 parent_ok=true note=offer:true,read:true,value:true,serve:true,lease:true"
///   ```
///
///   **Every other field in that run says the system is healthy** — the parent
///   went on to apply its own offer, a third process read it back bit for bit,
///   the rendezvous still served and the lease was still held. Only `signalled`
///   reports the fault, and a harness comparing an exit code would have had `11`
///   to compare against nothing and gone green.
/// * **the `ChildDetached` mapping.** Delete the `PushError::ChildDetached` arm
///   from `crates/tf_tree_c/src/publisher.rs`'s `map::push`, so the catch-all
///   `other` arm answers `TFT_ERR_INTERNAL` instead. **Applied, observed:**
///   `child=exited 22` — `OFFER_STATUS_NOT_DETACHED`. The call still returned,
///   the outcome still said `TFT_BRIDGE_REJECTED`, and a test that had checked
///   only "it came back" would not have noticed the one status code a caller can
///   branch on going missing.
/// * **`tft_bridge_free`'s destructors** are *not* mutated here.
///   [`a_forked_child_stands_down_an_owned_writer_too`] already covers the pair
///   that `free` drops, and `free` returns `void`, so all this test can hold it
///   to from the child's side is coming back at all. What it adds is the
///   parent's `lease:` and `serve:` fields *after* a `free` in the child.
///
/// # And one mutant it does **not** fail against, on purpose
///
/// Removing the §3.4 panic guard entirely — `guard()` in
/// `crates/tf_tree_c/src/error.rs` calling `body()` directly with no
/// `catch_unwind` — leaves this test **passing**. Verified, not assumed.
///
/// That is worth writing down because `docs/decisions/0015` attributes this
/// property to that guard ("§3.4's panic guard turning `ChildDetached` into
/// `TFT_ERR_CHILD_DETACHED`"), and it is not what happens: nothing panics in the
/// child. `OwnedWriter::push` returns an ordinary `Err(PushError::ChildDetached)`
/// and `map::push` maps it. The panic guard is a *second*, independent defence
/// that would matter only if some future detach path panicked instead of
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
