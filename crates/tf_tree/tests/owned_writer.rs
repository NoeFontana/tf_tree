//! `Tree::claim_owned` / [`tf_tree::OwnedWriter`] — `docs/decisions/0017`
//! steps 2 and 3.
//!
//! `OwnedWriter` exists because three consumers needed a writer that outlives
//! the scope that made it and two built it by hand; the first hand-rolled
//! version was a `transmute::<EdgeWriter, Publisher>` that kept only the first
//! field, so it **leaked the claim lease** — making the edge permanently
//! unclaimable and invisible to the reaper — and **bypassed the fork guard**.
//! Every test in this file is one of those failure modes, written down.
//!
//! # Why the interesting half is `shm`-gated
//!
//! The claim *lease* is an OFD byte in the rendezvous lock file, and a heap tree
//! has no lock file at all (`Tree::take_claim_lease` returns `None` for one).
//! So the defect that shipped is only reproducible under
//! `--features shm` on Linux. **`cargo nextest run --workspace` — `just test` —
//! therefore compiles those two tests out entirely**, and the gate that runs
//! them is `just shm-check`, which names this target explicitly:
//! `cargo nextest run -p tf_tree --features shm --test owned_writer`. If a test
//! is added here that needs `shm`, that line already covers it; if this file is
//! renamed, that line has to move with it. The first two tests below are the
//! part that holds everywhere — that the `Arc` field is really there and really
//! keeps the tree alive — and those two do run under `just test`.
//!
//! The `shm` tests each take their own scratch runtime directory and set
//! `TF_TREE_RUNTIME_DIR` for the process, exactly as `tests/rendezvous.rs` does
//! — which is sound because the suite runs under `cargo nextest`, one process
//! per test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use tf_tree::{Capacity, EdgeCfg, Iso3, TreeBuilder};

/// A one-dynamic-edge layout, shared by every test here.
fn layout() -> TreeBuilder {
    TreeBuilder::new().dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(64)))
}

/// **The `Arc` field is real, and it is what keeps the arena alive.**
///
/// The record's step 2 states the mutant as "drop the `Arc` field ⇒
/// use-after-free under `just miri`". That recipe now covers `tf_tree`, so the
/// mutant is observable as stated — but this test also asserts the property
/// *directly*, with a `Weak`: after the caller's handle is gone the tree must
/// still be alive, and after the writer is gone it must not be. That is the same
/// fact the UAF would be a consequence of, and it fails deterministically on a
/// machine with no nightly toolchain.
///
/// The `push` between the two halves is not decoration: it is the operation that
/// would be reading and writing the freed arena under the mutant, which is what
/// makes this a Miri test and not only an `Arc` bookkeeping test.
///
/// Mutant C — and this one was **found by adding the crate to `just miri`, not
/// predicted**: un-box `OwnedWriter::writer` (`Box<EdgeWriter<'static>>` ⇒
/// `EdgeWriter<'static>`). **Applied, observed:** the suite still passes (4/4);
/// `cargo +nightly miri test -p tf_tree --test owned_writer` fails *this* test
/// with *"error: Undefined Behavior: deallocating while item \[SharedReadOnly
/// for &lt;…&gt;\] is strongly protected"*, reported inside
/// `<HeapArena as Drop>::drop` with `OwnedWriter::release` on the backtrace;
/// `-Zmiri-tree-borrows` fails it too, as *"deallocation through &lt;…&gt;
/// (root of the allocation) … is forbidden"*. `release`
/// below passes the writer **by value**, which strongly protects the arena
/// references inside it for the duration of the call, and the call is where the
/// last `Arc` — and with it the arena — goes away. Implicit end-of-scope drop
/// does not trip it; the two by-value spellings this type documents are exactly
/// the ones that do.
///
/// Mutant: replace the `tree: Arc<Tree>` field with
/// `PhantomData<Arc<Tree>>`. **Applied, observed:** *"assertion `left == right`
/// failed: claim_owned did not take a strong reference — left: 1, right: 2"*.
/// Applied a second time with that first assertion also deleted, to check the
/// rest of the test is not dead weight: it then fails at *"the tree died with
/// the caller's handle: the writer is now pointing into a freed arena and the
/// push below is a use-after-free"*. Both halves are load-bearing.
///
/// Mutant B: `core::mem::forget(self)` in `OwnedWriter::release`, standing in
/// for any destructor the owned shape might drop on the floor. **Applied,
/// observed:** the final `upgrade` assertion fails — *"releasing the writer left
/// the tree alive, so `OwnedWriter` is leaking a strong reference…"* — and the
/// two `shm` tests below fail in the same run, on the claim record and on the
/// lease respectively, which is what says the three cover different halves
/// rather than the same one three times.
#[test]
fn an_owned_writer_keeps_its_tree_alive_by_itself() {
    let tree = Arc::new(layout().build().expect("layout"));
    let base = tree.frame("base").unwrap();
    let odom = tree.frame("odom").unwrap();

    // A non-owning observer of the same allocation. Nothing else in this test
    // holds a strong reference except the writer.
    let watch = Arc::downgrade(&tree);
    assert_eq!(
        Arc::strong_count(&tree),
        1,
        "the fixture already shares the tree, so the assertions below would \
         pass without `OwnedWriter` holding anything"
    );

    let writer = tree.claim_owned(base, odom).expect("claim");
    assert_eq!(
        Arc::strong_count(&tree),
        2,
        "claim_owned did not take a strong reference"
    );

    drop(tree);
    assert!(
        watch.upgrade().is_some(),
        "the tree died with the caller's handle: the writer is now pointing \
         into a freed arena and the push below is a use-after-free"
    );

    // Exercise the arena while the writer is the only thing keeping it mapped.
    writer.push(1_000, &Iso3::IDENTITY).expect("push");

    writer.release();
    assert!(
        watch.upgrade().is_none(),
        "releasing the writer left the tree alive, so `OwnedWriter` is leaking \
         a strong reference and the arena outlives every handle to it"
    );
}

/// `OwnedWriter` is `Send` and is not `Sync`, as `Publisher` is (D7).
///
/// The compile-time half — that `Sync` is *refused* — is a `compile_fail` doc
/// test on the type itself, because a negative bound cannot be written here.
/// This is the positive half plus the thread that proves `Send` is usable and
/// not merely satisfied.
///
/// Mutant: add `unsafe impl Sync for OwnedWriter {}`. **Applied, observed:**
/// this test still passes, and `cargo test --doc -p tf_tree` fails the
/// `compile_fail` doc test on `OwnedWriter` with *"Test compiled successfully,
/// but it's marked `compile_fail`"* — which is why both halves exist. (That
/// doc test's *error code* is a separate matter: stable ignores it, and
/// `just test-doc-error-codes` is what checks it. Its `compile_fail` half, the
/// one this mutant trips, is checked on stable.)
#[test]
fn an_owned_writer_moves_between_threads_but_is_never_shared() {
    fn assert_send<T: Send>() {}
    assert_send::<tf_tree::OwnedWriter>();

    let tree = Arc::new(layout().build().expect("layout"));
    let base = tree.frame("base").unwrap();
    let odom = tree.frame("odom").unwrap();
    let writer = tree.claim_owned(base, odom).expect("claim");

    // The whole handle crosses the boundary, tree and all — the case the
    // lifetime on `EdgeWriter<'a>` makes impossible without a scoped thread.
    let joined = std::thread::spawn(move || {
        writer
            .push(2_000, &Iso3::IDENTITY)
            .expect("push from another thread");
        writer
    })
    .join()
    .expect("join");

    drop(joined);
    // The claim was released on the other thread's value, so this one succeeds.
    tree.claim(base, odom)
        .expect("the edge is claimable again after the moved writer dropped");
}

/// A scratch runtime directory, removed when the test ends, so a failure cannot
/// leave an arena behind in the shared `/tmp` location.
#[cfg(all(feature = "shm", target_os = "linux"))]
struct Scratch(std::path::PathBuf);

#[cfg(all(feature = "shm", target_os = "linux"))]
impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("tf_tree_owned-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        std::env::set_var("TF_TREE_RUNTIME_DIR", &p);
        Scratch(p)
    }

    /// The rendezvous lock file for domain 0, name `default`.
    fn lock_path(&self) -> std::path::PathBuf {
        self.0.join("0/default.lock")
    }
}

#[cfg(all(feature = "shm", target_os = "linux"))]
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Open a second (or third) read-write handle onto the arena `keeper` created.
#[cfg(all(feature = "shm", target_os = "linux"))]
fn join() -> tf_tree::Tree {
    tf_tree::Open::new()
        .mode(tf_tree::AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::Never)
        .open()
        .expect("join the existing arena")
}

/// **Step 2, in its literal form: the edge is re-claimable from a fresh tree
/// over the same arena.**
///
/// `keeper` creates the arena and does nothing else — it is here so that the
/// *claimer*'s handle can be dropped without the segment going with it. That
/// separation is the point: each `Tree` carries its own `mmap`, so dropping the
/// claimer unmaps the region the writer's `ClaimRecord` reference points into
/// while the region itself lives on in the keeper. A writer that did not hold
/// its own `Arc` would then release its claim through an unmapped address.
///
/// The re-claim at the end is what proves the release actually happened. It is
/// the assertion that would fail if `OwnedWriter` dropped the `EdgeWriter`'s
/// guts on the floor the way the `transmute::<EdgeWriter, Publisher>` did.
///
/// Mutant: replace the `tree: Arc<Tree>` field with `PhantomData` ⇒ the `push`
/// after `drop(claimer)` stores through a reference into a region that has just
/// been `munmap`ped. **Applied, observed:** nextest reports `SIGSEGV` for this
/// test — *"(test aborted with signal 11: SIGSEGV)"* — rather than a failed
/// assertion, while the heap-only test above fails its `strong_count` assertion
/// in the same run.
///
/// Mutant B: `core::mem::forget(self)` in `OwnedWriter::release`, i.e. an owned
/// writer that keeps the claim record. **Applied, observed:** the final `claim`
/// fails — *"the edge was not released: a stored writer leaked its claim:
/// AlreadyClaimed(EdgeAlreadyClaimed { owner_slot: 1 })"* — a stored publisher
/// that has gone away and an edge nobody can take.
#[test]
#[cfg(all(feature = "shm", target_os = "linux"))]
fn an_owned_writer_releases_a_shared_edge_for_the_next_claimer() {
    let _scratch = Scratch::new("reclaim");

    let keeper = tf_tree::Open::new()
        .mode(tf_tree::AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::IfAbsent)
        .layout_if_creating(layout())
        .open()
        .expect("create the arena");

    let claimer = Arc::new(join());
    assert_ne!(
        keeper.participant_slot(),
        claimer.participant_slot(),
        "both handles took the same slot, so this is one participant and the \
         drop below would prove nothing"
    );

    let base = claimer.frame("base").unwrap();
    let odom = claimer.frame("odom").unwrap();
    let writer = claimer.claim_owned(base, odom).expect("claim");

    // The caller's handle goes. Only the writer's own `Arc` is left.
    drop(claimer);
    writer.push(1_000, &Iso3::IDENTITY).expect("push");
    writer.release();

    let rejoined = join();
    let base = rejoined.frame("base").unwrap();
    let odom = rejoined.frame("odom").unwrap();
    rejoined
        .claim(base, odom)
        .expect("the edge was not released: a stored writer leaked its claim");

    drop(keeper);
}

/// **Step 3: dropping an `OwnedWriter` frees the edge's OFD byte.**
///
/// This is the shipped `tf_tree_py` defect reproduced as a test. The arena
/// `ClaimRecord` and the kernel lease are two separate things, and releasing
/// only the first looks completely healthy from inside the process: `push`
/// works, a re-claim works, `doctor` says nothing. What is broken is
/// *observability* — a leaked lease is indistinguishable from a live writer, so
/// no reaper will ever collect the edge if this process dies badly.
///
/// The probe opens the lock file a **second time**, which is the only vantage
/// point that can see the answer at all: OFD locks are self-blind, so the tree's
/// own description reports every byte it holds as free. Without the second
/// description this test would pass against a writer that released nothing.
///
/// Mutant: give `OwnedWriter` a `Box<Publisher<'static>>` field instead of a
/// `Box<EdgeWriter<'static>>`, taking it out of the `EdgeWriter` with a
/// `ptr::read` and forgetting the rest — the exact shape of the `transmute` that
/// shipped. **Applied, observed:** the first assertion still passes (the probe
/// reports `held: true`, as it should) and the second fails — *"the lease
/// outlived the writer: the edge is now permanently unclaimable and no reaper
/// can tell…"* — because `probe_claim(edge).held` is still `true` after
/// `release`. The re-claim test above fails in the same run with
/// `LeaseContended { edge: EdgeId(1) }`, from the other side of the same byte.
///
/// Mutant B: `core::mem::forget(self)` in `OwnedWriter::release`, which reaches
/// the same place by a route that does not need the type edited. **Applied,
/// observed:** the second assertion fails, with the same message.
#[test]
#[cfg(all(feature = "shm", target_os = "linux"))]
fn dropping_an_owned_writer_releases_the_claim_lease() {
    let scratch = Scratch::new("lease");

    let tree = Arc::new(
        tf_tree::Open::new()
            .mode(tf_tree::AttachMode::ReadWrite)
            .create(tf_tree::CreatePolicy::IfAbsent)
            .layout_if_creating(layout())
            .open()
            .expect("create the arena"),
    );
    let base = tree.frame("base").unwrap();
    let odom = tree.frame("odom").unwrap();

    let writer = tree.claim_owned(base, odom).expect("claim");
    // **`OwnedWriter::edge`, not a topology read through `arena_view`.** It is
    // the stable-tier spelling of the same `EdgeId`, and its own docstring is
    // the argument for preferring it: a caller who cannot ask the writer has to
    // re-derive the id from a seqlock topology read that can fail, and then has
    // no cross-check that the two agree. Asking the writer also keeps this test
    // outside the `unstable` gate that the rest of this suite's arena readers
    // now carry — which is not a nicety here but the difference between running
    // and not: `just shm-check` runs this target as `cargo nextest run -p
    // tf_tree --features shm --test owned_writer`, so anything gated on
    // `unstable` in this file executes in no recipe at all.
    let edge = writer.edge().get();
    // **The cross-check that docstring names — which neither of this line's two
    // ancestors actually made.** The topology read this branch replaced derived
    // the id a second way and then compared the two not at all; and
    // `assert_ne!(edge, 0)`, what replaced it, *cannot* fail: `Tree::claim`
    // reads the same `edge_of_child` and returns `ClaimApiError::NoEdge
    // { child }` when it is 0, so a successful `claim_owned` has already ruled
    // the sentinel out.
    //
    // So compare the two structures instead. `writer.edge()` came from the
    // topology block; `Tree::edges` reads the *edge records*, which are a
    // separate table filled by the builder, and it is stable-tier. If they
    // disagree the probe below tests a different edge's byte and every
    // assertion in this test becomes vacuous.
    let declared = tree.edges().unwrap();
    assert_eq!(
        declared
            .get(edge.wrapping_sub(1) as usize)
            .map(|(p, c)| (p.as_str(), c.as_str())),
        Some(("odom", "base")),
        "the writer names EdgeId({edge}), which is not the odom -> base edge \
         this layout declares ({declared:?}), so the lease probe below would \
         be reading some other edge's byte"
    );

    let probe = tf_tree_ipc::LockFile::open(&scratch.lock_path()).expect("open the lock file");
    assert!(
        probe.probe_claim(edge).unwrap().held,
        "claim_owned did not take the edge's lease — the arena record alone \
         cannot tell a live holder from a dead one"
    );

    writer.release();
    assert!(
        !probe.probe_claim(edge).unwrap().held,
        "the lease outlived the writer: the edge is now permanently unclaimable \
         and no reaper can tell, which is the defect 0017 exists to remove"
    );

    drop(tree);
}
