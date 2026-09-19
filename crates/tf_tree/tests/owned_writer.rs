//! `Tree::claim_owned` / [`tf_tree::OwnedWriter`] — `docs/decisions/0017` steps 2
//! and 3. Each test pins a failure mode of the hand-rolled predecessor: a leaked
//! claim lease and a bypassed fork guard.
//!
//! The claim lease is an OFD byte in the rendezvous lock file, which a heap tree
//! lacks, so the two lease tests are `shm`-gated: `just test` compiles them out
//! and `just shm-check` runs this target
//! (`cargo nextest run -p tf_tree --features shm --test owned_writer`). Each takes
//! its own `TF_TREE_RUNTIME_DIR`, sound under nextest's process-per-test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use tf_tree::{Capacity, EdgeCfg, Iso3, TreeBuilder};

/// A one-dynamic-edge layout, shared by every test here.
fn layout() -> TreeBuilder {
    TreeBuilder::new().dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(64)))
}

/// The `Arc` field is real and keeps the arena alive.
///
/// Asserted with a `Weak` (no nightly needed); the `push` between the halves is
/// what makes the mutant a Miri use-after-free.
///
/// Mutants: `tree: Arc<Tree>` replaced by `PhantomData<Arc<Tree>>` ⇒ the
/// `strong_count` assertion fails; `core::mem::forget(self)` in
/// `OwnedWriter::release` ⇒ the final `upgrade` assertion fails; un-boxing
/// `OwnedWriter::writer` passes here but fails under `just miri`, because
/// `release` passes the writer by value and the last `Arc` drops inside the call.
#[test]
fn an_owned_writer_keeps_its_tree_alive_by_itself() {
    let tree = Arc::new(layout().build().expect("layout"));
    let base = tree.frame("base").unwrap();
    let odom = tree.frame("odom").unwrap();

    // Non-owning observer; only the writer holds a strong reference.
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

    // The writer alone keeps the arena mapped.
    writer.push(1_000, &Iso3::IDENTITY).expect("push");

    writer.release();
    assert!(
        watch.upgrade().is_none(),
        "releasing the writer left the tree alive, so `OwnedWriter` is leaking \
         a strong reference and the arena outlives every handle to it"
    );
}

/// `OwnedWriter` is `Send` and not `Sync`, as `Publisher` is (D7).
///
/// The `Sync` refusal is a `compile_fail` doctest on the type; this is the
/// positive half. Mutant: `unsafe impl Sync for OwnedWriter {}` ⇒ the doctest
/// fails.
#[test]
fn an_owned_writer_moves_between_threads_but_is_never_shared() {
    fn assert_send<T: Send>() {}
    assert_send::<tf_tree::OwnedWriter>();

    let tree = Arc::new(layout().build().expect("layout"));
    let base = tree.frame("base").unwrap();
    let odom = tree.frame("odom").unwrap();
    let writer = tree.claim_owned(base, odom).expect("claim");

    // The whole handle crosses the boundary, tree and all.
    let joined = std::thread::spawn(move || {
        writer
            .push(2_000, &Iso3::IDENTITY)
            .expect("push from another thread");
        writer
    })
    .join()
    .expect("join");

    drop(joined);
    tree.claim(base, odom)
        .expect("the edge is claimable again after the moved writer dropped");
}

/// A scratch runtime directory, removed on drop.
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

/// Step 2: the edge is re-claimable from a fresh tree over the same arena.
///
/// `keeper` holds the segment so the claimer's `Tree`, and its own `mmap`, can be
/// dropped: a writer without its own `Arc` would release through an unmapped
/// address.
///
/// Mutants: `PhantomData` for the `Arc` ⇒ SIGSEGV on the `push` after
/// `drop(claimer)`; `core::mem::forget(self)` in `release` ⇒ the final `claim`
/// fails with `AlreadyClaimed`.
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

/// Step 3: dropping an `OwnedWriter` frees the edge's OFD byte.
///
/// A leaked lease looks healthy in-process but no reaper can collect the edge.
/// The probe opens the lock file a second time, because OFD locks are self-blind.
///
/// Mutants: a `Box<Publisher<'static>>` field taken with `ptr::read`, or
/// `core::mem::forget(self)` in `release` ⇒ the second assertion fails.
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
    // Stable-tier `OwnedWriter::edge`, so this test stays outside the `unstable`
    // gate that no `shm-check` recipe would run.
    let edge = writer.edge().get();
    // Cross-check against the builder-filled edge records, else the probe below
    // could be reading another edge's byte.
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
