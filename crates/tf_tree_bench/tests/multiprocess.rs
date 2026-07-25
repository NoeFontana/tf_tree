//! The multi-process gate: a *different process* reads the same arena and gets
//! the same answers.
//!
//! `crates/tf_tree_bench/tests/relocation.rs` proves the arena survives a move
//! to a different address within one process. This proves the whole thing across
//! a real process boundary — separate address space, separate page tables,
//! separate `exec` — which is what `docs/PHASE2.md` exists to deliver and what
//! `HeapArena` structurally cannot do.
//!
//! The reader in the child is the **unmodified Phase 1 reader**. Nothing in
//! `Plan::at`, the bracket search, slot reads or interning knows which backend
//! it has; that is `docs/PHASE2.md` §4's "zero lines in the read path", tested
//! rather than asserted.
//!
//! Requires `--features shm` (Linux). Run: `just shm-test`.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader};

use tf_tree::{AttachMode, Capacity, EdgeCfg, InterpPolicy, Iso3, Stamp, Tree, TreeBuilder};
use tf_tree_bench::fixture;
use tf_tree_bench::shm_util::{sibling_binary, spawn_attached};

/// Build the §11.1 fixture topology into a shared segment and populate it.
fn shared_fixture() -> Tree {
    let mut b = TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
    for e in fixture::EDGES {
        b = match e.kind {
            fixture::EdgeDefKind::Static { xi } => {
                b.static_edge(e.parent, e.child, &tf_tree_math::exp_se3(xi))
            }
            fixture::EdgeDefKind::Dynamic { rate_hz } => b.dynamic_edge(
                e.parent,
                e.child,
                EdgeCfg::new(Capacity::history(rate_hz, fixture::HISTORY_SECS)),
            ),
        };
    }
    let tree = b.build_shared("tf_tree.test").expect("build shared arena");
    assert!(tree.is_shared());
    {
        let (writers, samples) = fixture::spin_up(&tree).expect("spin up");
        drop(writers);
        drop(samples);
    }
    tree
}

/// Ask the parent's own tree the same questions the child will be asked.
fn parent_answers(
    tree: &Tree,
    target: &str,
    source: &str,
    base_ns: i64,
    count: usize,
) -> Vec<Option<[u64; 7]>> {
    let t = tree.frame(target).unwrap();
    let s = tree.frame(source).unwrap();
    let plan = tree.plan(t, s).unwrap();
    let guard = tree.guard();
    (0..count)
        .map(|i| {
            let stamp: Stamp = Stamp::from_nanos(base_ns - (i as i64) * 1_000_000);
            plan.at(&guard, stamp).ok().map(|p: Iso3| p.to_bits())
        })
        .collect()
}

/// The gate: a second process answers bit-identically from the shared segment.
#[test]
fn another_process_reads_the_same_arena_bit_identically() {
    let tree = shared_fixture();
    let child_bin = sibling_binary("shm_child").expect("shm_child binary");

    const COUNT: usize = 512;
    let base_ns = fixture::NOW_NS;
    let want = parent_answers(&tree, "imu_link", "map", base_ns, COUNT);

    let fd = tree.shared_fd().expect("shared tree exposes its fd");
    let args: Vec<String> = [
        "verify",
        "imu_link",
        "map",
        &base_ns.to_string(),
        &COUNT.to_string(),
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();

    let mut child = spawn_attached(&child_bin, fd, &args).expect("spawn child");
    let out = BufReader::new(child.stdout.take().expect("child stdout"));

    let mut compared = 0usize;
    let mut lines = 0usize;
    for (i, line) in out.lines().enumerate() {
        let line = line.expect("read child line");
        lines += 1;
        let got: Option<[u64; 7]> = if line == "err" {
            None
        } else {
            let mut it = line.split_whitespace();
            assert_eq!(it.next(), Some("ok"), "unexpected child output: {line:?}");
            let mut bits = [0u64; 7];
            for b in &mut bits {
                *b = it.next().expect("bit field").parse().expect("u64");
            }
            Some(bits)
        };
        assert_eq!(
            got, want[i],
            "lookup {i} differs between processes — the shared arena is not the same data"
        );
        if got.is_some() {
            compared += 1;
        }
    }

    let status = child.wait().expect("wait for child");
    assert!(status.success(), "child exited with {status}");
    assert_eq!(lines, COUNT, "child answered {lines} of {COUNT} queries");
    // Guard against a vacuous pass: if every query declined, comparing `None` to
    // `None` proves nothing.
    assert!(
        compared > COUNT / 2,
        "only {compared} of {COUNT} queries produced a pose; the gate is vacuous"
    );
}

/// A read-only attachment must be exactly that: the MMU, not politeness, is what
/// stops a consumer corrupting the arena.
///
/// Verified in-process because the observable effect of writing through a
/// `PROT_READ` mapping is `SIGSEGV`, which is awkward to assert on. What *can*
/// be asserted cheaply is that the mode is carried through and reported, and
/// that a read-only tree still answers queries.
#[test]
fn read_only_attachment_still_answers() {
    let tree = shared_fixture();
    let fd = tree
        .shared_fd()
        .expect("shared fd")
        .try_clone_to_owned()
        .expect("dup fd");

    let ro = Tree::attach_shared(fd, AttachMode::ReadOnly).expect("attach read-only");
    assert!(ro.is_shared());

    let t = ro.frame("imu_link").expect("imu_link");
    let s = ro.frame("map").expect("map");
    let plan = ro.plan(t, s).expect("plan");
    let guard = ro.guard();
    let stamp: Stamp = Stamp::from_nanos(fixture::NOW_NS);
    let got = plan.at(&guard, stamp).expect("read-only lookup");

    let want = parent_answers(&tree, "imu_link", "map", fixture::NOW_NS, 1)[0].expect("parent");
    assert_eq!(got.to_bits(), want, "read-only mapping disagreed");
}

/// Samples published *after* a peer attached must be visible to it.
///
/// The bit-identity test above could pass against a snapshot. This one cannot:
/// the reader attaches first, then the writer publishes, and the reader has to
/// see it. That is the difference between sharing memory and copying it.
#[test]
fn writes_are_visible_to_an_already_attached_peer() {
    let tree = shared_fixture();
    let fd = tree
        .shared_fd()
        .expect("shared fd")
        .try_clone_to_owned()
        .expect("dup fd");
    let reader = Tree::attach_shared(fd, AttachMode::ReadOnly).expect("attach");

    // The 1 kHz edge, whose history ends at HISTORY_SECS.
    let (parent, child, rate_hz) = fixture::DYNAMIC_EDGES[2];
    let p = tree.frame(parent).unwrap();
    let c = tree.frame(child).unwrap();
    let w = tree.claim(c, p).expect("claim");

    // A stamp strictly past everything `spin_up` published.
    let period = (1e9 / rate_hz) as i64;
    let future = (fixture::HISTORY_SECS * 1e9) as i64 + 5 * period;

    let rt = reader.frame(child).unwrap();
    let rp = reader.frame(parent).unwrap();
    let plan = reader.plan(rt, rp).expect("plan");

    // Before the push, the reader must refuse: nothing covers that stamp.
    {
        let guard = reader.guard();
        let stamp: Stamp = Stamp::from_nanos(future);
        assert!(
            plan.at(&guard, stamp).is_err(),
            "reader answered for a stamp nobody has published yet"
        );
    }

    // Publish two samples bracketing `future`, from the parent process.
    for k in 0..2i64 {
        let stamp = future - period + k * 2 * period;
        w.push(stamp, &fixture::dynamic_pose(2.0, stamp))
            .expect("push");
    }

    // A fresh guard, and now it must resolve.
    let guard = reader.guard();
    let stamp: Stamp = Stamp::from_nanos(future);
    let got = plan
        .at(&guard, stamp)
        .expect("reader did not observe the writer's samples");

    // And it must equal what the writer's own process computes.
    let wt = tree.frame(child).unwrap();
    let wp = tree.frame(parent).unwrap();
    let wplan = tree.plan(wt, wp).expect("plan");
    let wguard = tree.guard();
    let want = wplan.at(&wguard, stamp).expect("writer lookup");
    assert_eq!(
        got.to_bits(),
        want.to_bits(),
        "cross-process read of a fresh sample disagreed"
    );
}

/// A read-only attachment must **refuse** mutations, not fault on them.
///
/// This is the test that makes `AttachMode::ReadOnly` a safety boundary rather
/// than a loaded gun. Every one of these calls reaches a `compare_exchange` in
/// the arena, and a `PROT_READ` mapping does not report that politely — the
/// process takes `SIGSEGV`. A consumer that merely misspells a frame name would
/// have died instead of getting an `Err`.
///
/// Verified against the real failure: before the guards, `ro.claim(..)` exited
/// with `signal: 11`, and so did `ro.frame("never-declared")` once the frame
/// table had headroom for the intern to get past its capacity pre-check.
#[test]
fn read_only_refuses_mutation_instead_of_faulting() {
    let tree = shared_fixture();
    let fd = tree
        .shared_fd()
        .expect("shared fd")
        .try_clone_to_owned()
        .expect("dup fd");
    let ro = Tree::attach_shared(fd, AttachMode::ReadOnly).expect("attach read-only");
    assert!(
        !ro.is_writable(),
        "read-only attach reports itself writable"
    );

    // Resolving a name the creator declared is a pure read and must still work.
    let child = ro.frame("imu_link").expect("declared frame still resolves");
    let parent = ro
        .frame("base_link")
        .expect("declared frame still resolves");

    // Interning a *new* name would publish into the hash table.
    assert_eq!(
        ro.frame("never-declared-anywhere"),
        Err(tf_tree_core::FrameError::ReadOnly),
        "interning through a read-only mapping was not refused"
    );

    // Claiming writes the claim record.
    assert!(
        matches!(
            ro.claim(child, parent),
            Err(tf_tree::ClaimApiError::ReadOnly)
        ),
        "claim through a read-only mapping was not refused"
    );

    // Re-parenting writes the topology block.
    assert!(
        matches!(
            ro.reparent(child, parent),
            Err(tf_tree::ReparentError::ReadOnly)
        ),
        "reparent through a read-only mapping was not refused"
    );
}

/// Runtime re-parenting is refused on a shared arena even when writable.
///
/// `Tree::reparent` is serialized only by a **process-local** mutex, and
/// `set_parent` publishes an odd generation for the duration of its block copy —
/// so a writer killed mid-mutation leaves every reader in every process spinning
/// forever in plan compilation. `docs/PHASE2.md` §1 amendments A1/A2 fix that;
/// until they land the operation is refused rather than raced.
///
/// This test is also what makes `PHASE2.md` §0.0's claim that "topology is
/// immutable after `build_shared`" true by construction rather than by hope.
#[test]
fn reparent_is_refused_on_a_shared_arena() {
    let tree = shared_fixture();
    assert!(tree.is_writable(), "creator's tree should be writable");

    let child = tree.frame("imu_link").expect("imu_link");
    let parent = tree.frame("odom").expect("odom");
    assert!(
        matches!(
            tree.reparent(child, parent),
            Err(tf_tree::ReparentError::SharedArena)
        ),
        "reparent on a shared arena was not refused"
    );
}
