//! The multi-process gate: a *different process* reads the same arena and gets
//! the same answers.
//!
//! `crates/tf_tree_bench/tests/relocation.rs` proves the arena survives a move
//! within one process; this proves it across a process boundary. The child runs
//! the unmodified Phase 1 reader (`docs/PHASE2.md` §4, "zero lines in the read
//! path").
//!
//! Requires `--features shm` (Linux). Run: `just shm-test`.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};

use tf_tree::{AttachMode, Capacity, EdgeCfg, InterpPolicy, Iso3, Stamp, Tree, TreeBuilder};
use tf_tree_bench::fixture;
use tf_tree_bench::shm_util::{sibling_binary, spawn_attached};
use tf_tree_bench::workload::Backing;

/// The §11.1 fixture topology, declared but not built; shared by both harnesses
/// so their topologies cannot drift.
fn fixture_builder() -> TreeBuilder {
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
    b
}

/// Fill a freshly built fixture tree with the history the readers ask about.
fn populate(tree: &Tree) {
    let (writers, samples) = fixture::spin_up(tree).expect("spin up");
    drop(writers);
    drop(samples);
}

/// Build the §11.1 fixture topology into a shared segment and populate it.
fn shared_fixture() -> Tree {
    let tree = fixture_builder()
        .build_shared("tf_tree.test")
        .expect("build shared arena");
    assert!(tree.is_shared());
    populate(&tree);
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

/// A read-only attachment still answers queries; the fault a `PROT_READ` write
/// takes is awkward to assert, so only the mode and reads are checked.
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

/// Samples published after a peer attached are visible to it; a snapshot would
/// fail this.
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

/// A read-only attachment refuses mutations rather than taking `SIGSEGV` from a
/// `compare_exchange` into a `PROT_READ` mapping.
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

/// Runtime re-parenting works on a shared arena and another process sees the
/// result (`docs/PHASE2.md` §1, A2). The reparent changes every `map →
/// imu_link` path, so a no-op reparent fails.
#[test]
fn reparent_on_a_shared_arena_is_visible_to_another_process() {
    let tree = shared_fixture();
    assert!(tree.is_writable(), "creator's tree should be writable");

    const COUNT: usize = 64;
    let base_ns = fixture::NOW_NS;
    let before = parent_answers(&tree, "imu_link", "map", base_ns, COUNT);

    let child = tree.frame("imu_link").expect("imu_link");
    let new_parent = tree.frame("odom").expect("odom");
    let generation_before = tree.guard().generation();
    tree.reparent(child, new_parent)
        .expect("reparent on a shared arena");
    assert!(
        tree.guard().generation() > generation_before,
        "reparent did not publish a new topology generation"
    );

    let after = parent_answers(&tree, "imu_link", "map", base_ns, COUNT);
    assert_ne!(
        before, after,
        "the reparent changed nothing; this test would pass vacuously"
    );
    assert!(
        after.iter().filter(|a| a.is_some()).count() > COUNT / 2,
        "the reparented topology answers almost nothing; the comparison is vacuous"
    );

    // A second process compiles its own plan from the topology block, so it
    // agrees only if the reparent reached the shared bytes.
    let child_bin = sibling_binary("shm_child").expect("shm_child binary");
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

    let mut proc = spawn_attached(&child_bin, fd, &args).expect("spawn child");
    let out = BufReader::new(proc.stdout.take().expect("child stdout"));
    let got: Vec<Option<[u64; 7]>> = out
        .lines()
        .map(|line| {
            let line = line.expect("read child line");
            if line == "err" {
                return None;
            }
            let mut it = line.split_whitespace();
            assert_eq!(it.next(), Some("ok"), "unexpected child output: {line:?}");
            let mut bits = [0u64; 7];
            for b in &mut bits {
                *b = it.next().expect("bit field").parse().expect("u64");
            }
            Some(bits)
        })
        .collect();
    let status = proc.wait().expect("wait for child");
    assert!(status.success(), "child exited with {status}");

    assert_eq!(got.len(), COUNT, "child answered {} of {COUNT}", got.len());
    assert_eq!(
        got, after,
        "the peer process did not see the re-parented topology"
    );
    assert_ne!(
        got, before,
        "the peer process answered from the pre-reparent topology"
    );
}

/// A scratch runtime directory for the rendezvous tests, removed on drop.
///
/// `set_var` is process-wide: safe only because nextest gives every test its own
/// process (`just shm-check`); plain `cargo test` is unsupported.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("tf_tree_mp-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create the scratch runtime directory");
        std::env::set_var("TF_TREE_RUNTIME_DIR", &p);
        Scratch(p)
    }

    /// The lock file the rendezvous puts `arena`'s participant bytes in:
    /// `<runtime dir>/<domain>/<name>.lock`, domain 0. `arena` is a parameter
    /// so a wrong name cannot make every `!held` assertion pass vacuously.
    fn lock_path(&self, arena: &str) -> std::path::PathBuf {
        self.0.join(format!("0/{arena}.lock"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The rendezvous name the reparent race creates its arena under.
const RACE_ARENA: &str = "tf_tree_reparent_race";

/// The rendezvous name of the served-workload refusal test.
const SERVED_WORKLOAD_ARENA: &str = "tf_tree_served_workload";

/// `Backing::Served` refuses an arena it did not create, including one of a
/// different shape (#258).
///
/// Both arms refuse in `build_tree`, before the workload is populated.
/// `layout_hash` is a struct-layout constant, so §3.7 cannot catch a shape
/// mismatch. The assertion is on the message, not merely on failure: without
/// `require_create(true)` the different-shape arm still errs, but as
/// `CapacityExceeded` from further on. Setting `Always` with `require_create`
/// kept still passes; the policy is not what refuses.
#[test]
fn a_served_workload_refuses_an_arena_it_did_not_create() {
    let _scratch = Scratch::new("served-workload");
    let robot = tf_tree_bench::workload::by_name("robot").expect("the robot workload");
    let humanoid = tf_tree_bench::workload::by_name("humanoid").expect("the humanoid workload");

    let owner = robot
        .build(
            InterpPolicy::LerpSlerp,
            Backing::Served(SERVED_WORKLOAD_ARENA),
        )
        .expect("create and serve the robot arena");

    for (label, w) in [("same shape", robot), ("different shape", humanoid)] {
        let e = w
            .build(
                InterpPolicy::LerpSlerp,
                Backing::Served(SERVED_WORKLOAD_ARENA),
            )
            .err()
            .unwrap_or_else(|| {
                panic!(
                    "{label}: a second Served build joined the owner's arena instead of refusing"
                )
            });
        let msg = format!("{e:#}");
        assert!(
            msg.contains("already live"),
            "{label}: refused for the wrong reason: {msg}"
        );
    }

    drop(owner);
}

/// The fixture topology created through the rendezvous, so peers can join and
/// be given a participant slot.
///
/// `require_create(true)`, not `CreatePolicy::Always` (#258): `Always` still
/// joins a live server, while `require_create` yields
/// [`tf_tree::OpenError::ArenaAlreadyLive`], which
/// [`a_second_served_fixture_refuses_rather_than_joining_the_first`] measures.
fn served_fixture() -> Tree {
    let tree = try_served_fixture().expect("create and serve the fixture arena");
    assert!(tree.is_shared());
    assert!(tree.is_writable());
    populate(&tree);
    tree
}

/// [`served_fixture`]'s open without populating, so a test can inspect the refusal.
fn try_served_fixture() -> Result<Tree, tf_tree::OpenError> {
    tf_tree::Open::new()
        .name(RACE_ARENA)
        .expect("a valid rendezvous name")
        .mode(AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::IfAbsent)
        .require_create(true)
        .layout_if_creating(fixture_builder())
        .open()
}

/// One read-write peer joined through the rendezvous, the only read-write
/// attachment since `docs/decisions/0028` plan step 0b
/// (`ShmError::ReadWriteNeedsRendezvous` otherwise).
fn join_read_write() -> Tree {
    tf_tree::Open::new()
        .name(RACE_ARENA)
        .expect("a valid rendezvous name")
        .mode(AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::Never)
        .open()
        .expect("join the served fixture arena read-write")
}

/// A second `Backing::Served`-shaped open refuses instead of joining (#258).
/// The second open is in this process; the owner's serving thread answers, so
/// the rendezvous resolves to `Joined` as it would across processes.
///
/// Mutant: dropping `.require_create(true)` from [`try_served_fixture`] makes
/// the second open return `Ok`; `CreatePolicy::Always` still passes.
#[test]
fn a_second_served_fixture_refuses_rather_than_joining_the_first() {
    let scratch = Scratch::new("served-refuses-to-join");
    let owner = served_fixture();

    // `Tree` is not `Debug`, so this is `match` rather than `expect_err`.
    let refused = match try_served_fixture() {
        Err(e) => e,
        Ok(_) => {
            panic!("a second creator was handed the first one's arena instead of being refused")
        }
    };
    assert!(
        matches!(refused, tf_tree::OpenError::ArenaAlreadyLive),
        "expected ArenaAlreadyLive, got {refused:?}"
    );

    // The refusal left nothing behind: the owner holds byte 0, so a kept
    // session would show as a held byte 1. `Open::open` drops the session (and
    // its OFD lock) before returning `ArenaAlreadyLive`.
    {
        let lock = tf_tree_ipc::LockFile::open(&scratch.lock_path(RACE_ARENA))
            .expect("the rendezvous created a lock file");
        assert!(
            !lock
                .probe_participant(1)
                .expect("probe a participant byte")
                .held,
            "the refused open kept its participant byte"
        );
    }

    // The rendezvous is not wedged: a genuine joiner is still granted a
    // read-write attachment. Not an assertion about which slot: the index is
    // freed on the owner's epoll (`on_hangup`), racing this thread, while the
    // lock byte is released synchronously.
    let peer = join_read_write();
    assert!(peer.is_writable(), "the peer joined read-only");

    drop(peer);
    drop(owner);
}

/// Two independent attachments race `reparent`; the process-local mutex is not
/// what stops them colliding.
///
/// Each `Tree` has its own slot, `decl` mutex and open file description. A2 is
/// two locks since `docs/decisions/0029`: the lock file's topology byte
/// (`tf_tree_ipc`'s `two_descriptions_in_one_process_still_conflict`) and the
/// in-arena word; the generation count falsifies a failure of either. In-process
/// rather than across a `fork`, because a data race needs both mutators alive;
/// the process boundary is covered above. Peers join through [`join_read_write`]
/// per `docs/decisions/0028` plan step 0b.
#[test]
fn concurrent_reparents_from_separate_attachments_are_serialized() {
    let scratch = Scratch::new("reparent-race");
    let tree = served_fixture();

    let a = join_read_write();
    let b = join_read_write();

    // Three participants, three bytes: the owner took byte 0 at creation and
    // each joiner its own. Byte 3 is asserted free so a build reporting every
    // byte held fails.
    {
        let lock = tf_tree_ipc::LockFile::open(&scratch.lock_path(RACE_ARENA))
            .expect("the rendezvous created a lock file");
        for slot in 0..3 {
            assert!(
                lock.probe_participant(slot)
                    .expect("probe a participant byte")
                    .held,
                "participant byte {slot} is free: this attachment is byte-less, \
                 which is the state step 0b exists to make unconstructible"
            );
        }
        assert!(
            !lock
                .probe_participant(3)
                .expect("probe a participant byte")
                .held,
            "byte 3 reads held with three participants; the probe says yes to everything"
        );
    }

    // Two frames moved between two unrelated parents, so no mutation can cycle.
    const ROUNDS: u32 = 64;
    let start = tree.guard().generation();

    std::thread::scope(|s| {
        for (t, child_name) in [(&a, "imu_link"), (&b, "lidar")] {
            s.spawn(move || {
                let child = t.frame(child_name).expect("child frame");
                let p1 = t.frame("odom").expect("odom");
                let p2 = t.frame("base_link").expect("base_link");
                for r in 0..ROUNDS {
                    let parent = if r % 2 == 0 { p1 } else { p2 };
                    loop {
                        match t.reparent(child, parent) {
                            Ok(()) => break,
                            // The only tolerated failure: a live peer holds the
                            // lock. Anything else is a real defect.
                            Err(tf_tree::ReparentError::LockContended { .. }) => {
                                std::hint::spin_loop();
                            }
                            Err(other) => panic!("reparent failed: {other}"),
                        }
                    }
                }
            });
        }
    });

    // Every mutation published exactly once; a lost generation means two
    // writers shared one scratch block.
    assert_eq!(
        tree.guard().generation() - start,
        u64::from(2 * ROUNDS),
        "topology generations were lost — the mutations were not serialized"
    );

    // And the tree is intact and consistent from a third view of the segment.
    let final_parent = tree.frame("base_link").expect("base_link");
    for name in ["imu_link", "lidar"] {
        let f = tree.frame(name).expect("frame");
        let plan = tree
            .plan(f, final_parent)
            .expect("plan against a live tree");
        let guard = tree.guard();
        let stamp: Stamp = Stamp::from_nanos(fixture::NOW_NS);
        // Both edges cover NOW_NS, so a well-formed topology must resolve.
        plan.at(&guard, stamp)
            .unwrap_or_else(|e| panic!("{name} unresolvable after the race: {e:?}"));
    }
}
