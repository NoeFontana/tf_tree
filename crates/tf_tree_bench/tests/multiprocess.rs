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
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};

use tf_tree::{AttachMode, Capacity, EdgeCfg, InterpPolicy, Iso3, Stamp, Tree, TreeBuilder};
use tf_tree_bench::fixture;
use tf_tree_bench::shm_util::{sibling_binary, spawn_attached};
use tf_tree_bench::workload::Backing;

/// The §11.1 fixture topology, declared but not built.
///
/// Split out of [`shared_fixture`] because [`served_fixture`] declares the same
/// edges through a different constructor. Two copies of this loop would let the
/// two harnesses drift into different topologies, which is a difference that
/// shows up as a test result.
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

/// Fill a freshly built fixture tree with the history every reader below asks
/// about.
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

/// Runtime re-parenting **works** on a shared arena, and another process sees
/// the result (`docs/PHASE2.md` §1, A2).
///
/// This replaces `reparent_is_refused_on_a_shared_arena`. That test asserted a
/// placeholder: `Tree::reparent` was serialized only by a *process-local* mutex,
/// which serializes nothing against a peer that mapped the same segment, so the
/// operation was refused rather than raced. A1 removed the wedge a crashed
/// mutator caused; A2 put the mutation lock in the arena header where every
/// participant contends on it. The refusal is gone and this is what replaces it.
///
/// The reparent is non-trivial on purpose: moving `imu_link` from `base_link` to
/// `odom` drops the `odom → base_link` leg out of every `map → imu_link` path,
/// so the answers *must* change. Asserting they changed is what stops the test
/// passing vacuously against a reparent that silently did nothing.
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

    // Now a *second process* maps the same segment and is asked the same
    // questions. It compiles its own plan from the topology block, so it can
    // only agree if the reparent reached the shared bytes.
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

/// A scratch runtime directory for the tests below that need a rendezvous,
/// removed when it ends.
///
/// **`set_var` is process-wide, and that is safe here only because `nextest`
/// gives every test its own process** — `just shm-check` runs this target as
/// `cargo nextest run -p tf_tree_bench --features shm --test multiprocess`.
/// Under plain `cargo test` the tests in this file share one process and one
/// environment; a `cargo test` invocation of this target is not supported. The
/// same caveat is written out at greater length on `tf_tree`'s
/// `tests/rendezvous.rs`, which this is modelled on.
///
/// Every other test in this file goes through `build_shared`, which reaches no
/// lock file and no socket and needs none of this. Three use it now, under two
/// different arena names — which is why [`Scratch::lock_path`] takes the name
/// rather than assuming [`RACE_ARENA`].
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("tf_tree_mp-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create the scratch runtime directory");
        std::env::set_var("TF_TREE_RUNTIME_DIR", &p);
        Scratch(p)
    }

    /// The lock file the rendezvous puts `arena`'s participant bytes in.
    ///
    /// `<runtime dir>/<domain>/<name>.lock`, with the default domain 0 — the
    /// layout `tf_tree_ipc::Rendezvous` resolves and the one
    /// `tests/rendezvous.rs` reads the same way.
    ///
    /// **`arena` is a parameter and used to be [`RACE_ARENA`] inlined.** One
    /// scratch directory now serves two rendezvous names, and a hardcoded one
    /// would have opened `<dir>/0/tf_tree_reparent_race.lock` for a test whose
    /// arena is [`SERVED_WORKLOAD_ARENA`] — a lock file describing nothing, whose
    /// every `probe_participant(..).held` reads `false` and whose every
    /// `assert!(!..held)` therefore passes vacuously.
    fn lock_path(&self, arena: &str) -> std::path::PathBuf {
        self.0.join(format!("0/{arena}.lock"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The rendezvous name the reparent race creates its arena under. Distinct from
/// every other name in the workspace so a stray runtime directory cannot make
/// two harnesses share an arena.
const RACE_ARENA: &str = "tf_tree_reparent_race";

/// The rendezvous name [`a_served_workload_refuses_an_arena_it_did_not_create`]
/// uses. Distinct from [`RACE_ARENA`] for the same reason that one is distinct
/// from every other name here.
const SERVED_WORKLOAD_ARENA: &str = "tf_tree_served_workload";

/// **`Backing::Served` refuses an arena it did not create — including one of a
/// different shape.** (#258, at the site the ticket is named after.)
///
/// The ticket's five-arm table: against a healthy served arena,
/// `CreatePolicy::Always` and `CreatePolicy::IfAbsent` were indistinguishable
/// and *both* handed the caller the owner's arena. The second arm below is the
/// one that makes it a wrong number rather than a shared one — `humanoid` is a
/// ~117-frame synthetic spine and `robot` is the 24-frame fixture, so they
/// share not one frame name, and the rendezvous still resolved to `Joined`.
/// §3.7 cannot catch it: `layout_hash` is a *struct-layout* constant,
/// byte-identical between two processes that disagree about every frame in the
/// tree.
///
/// Both arms refuse in `build_tree`, before `Workload::build` populates
/// anything, so the second one costs its arithmetic `plan()` and nothing else.
///
/// **Mutant: restore `.create(Always)` and delete `.require_create(true)` from
/// `build_tree`** — HEAD before this change — and the two arms fail
/// *differently*, which is worth writing down because the second one is not
/// what was predicted:
///
/// * *same shape* returns `Ok`. The join is silent and total: a `Built` over
///   somebody else's arena, and nothing downstream can tell.
/// * *different shape* returns `Err`, but from two layers further on and about
///   the wrong thing — `populating workload humanoid: frame link_0:
///   CapacityExceeded`. The open succeeded; the 24-frame arena it was handed
///   simply had no room for the 117th frame. So the shape mismatch is caught
///   here only by accident, and only because these two shapes are far apart:
///   the ticket's own ALT arm asked for one extra edge and more slots, which
///   the owner's arena absorbed, and it reported success with the frame it
///   asked for absent.
///
/// The assertion is therefore on the *message*, not merely on failure — a test
/// that accepted any `Err` would pass against the defect for this pair.
///
/// **Mutant: keep `require_create` and set `Always`** ⇒ still passes. The
/// policy is not what refuses, which is the whole of the ticket.
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

/// The fixture topology, created **through the rendezvous** so that peers can
/// join it and be given a participant slot with a lock byte behind it.
///
/// `require_create(true)` rather than `CreatePolicy::Always`: the scratch
/// directory is this process's own and empty, so there is nothing to join, and
/// the point of saying so in code is to make that an assertion rather than a
/// hope. **`Always` was not making it one** (#258) — it skips §3.4's
/// split-brain check and nothing else, so step 1 still joins a server that
/// answers, and a stray arena under this name would have been joined in
/// silence. `require_create` is the setting that turns that outcome into
/// [`tf_tree::OpenError::ArenaAlreadyLive`], which is what
/// [`a_second_served_fixture_refuses_rather_than_joining_the_first`] measures.
fn served_fixture() -> Tree {
    let tree = try_served_fixture().expect("create and serve the fixture arena");
    assert!(tree.is_shared());
    assert!(tree.is_writable());
    populate(&tree);
    tree
}

/// [`served_fixture`]'s open, without the `expect` and without populating —
/// so a test can look at the *refusal* rather than only at the success.
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

/// One read-write peer, joined through the rendezvous.
///
/// This is the *only* way to get a read-write attachment since
/// `docs/decisions/0028` plan step 0b: `Tree::attach_shared(fd, ReadWrite)`
/// returns `ShmError::ReadWriteNeedsRendezvous`, because a bare descriptor has
/// no lock file to take a participant byte in. Every `Tree` this returns holds
/// one.
fn join_read_write() -> Tree {
    tf_tree::Open::new()
        .name(RACE_ARENA)
        .expect("a valid rendezvous name")
        .mode(AttachMode::ReadWrite)
        .create(tf_tree::CreatePolicy::Never)
        .open()
        .expect("join the served fixture arena read-write")
}

/// **A second `Backing::Served`-shaped open refuses instead of joining.**
///
/// The defect #258 reports, at the smaller of its two call sites: the harness
/// asks to *create and serve* an arena it sized, and gets somebody else's
/// instead, with no error and no way to tell from the returned `Tree`. Both
/// sites reached for a "create, and refuse to join" policy;
/// [`tf_tree::CreatePolicy`] has no such variant, and both settled for `Always`
/// under the belief that it was one.
///
/// The second open here is in this process rather than a child, which is the
/// same shape [`join_read_write`] already relies on: the owner's serving thread
/// answers its own socket, so step 1 connects and the rendezvous resolves to
/// `Joined` exactly as it would across a process boundary. What the test pins
/// is the arm after that resolution.
///
/// **Mutant: drop `.require_create(true)` from [`try_served_fixture`]** ⇒ the
/// second open returns `Ok`, and this test fails on `expect_err`. That is the
/// pre-fix behaviour verbatim.
///
/// **Mutant: `CreatePolicy::Always` instead of `IfAbsent`** ⇒ still passes, and
/// that is the point of the ticket: the policy is not what refuses.
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

    // **The refusal left nothing behind**, which is the half of the promise that
    // is not about the error value. The owner holds byte 0, so a refused attach
    // that kept its session would show as a held byte 1.
    //
    // Deterministic, and worth saying why: `Open::open` drops the session — and
    // with it the OFD lock — *before* it returns `ArenaAlreadyLive`, and that
    // happens in this process, on this thread, inside the call above. There is
    // nothing to wait for.
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

    // And the rendezvous is not wedged: a genuine joiner is still granted a
    // read-write attachment. `join_read_write` panics on failure, so reaching
    // the assertion is most of the claim.
    //
    // **Not an assertion about *which* slot**, and it was one until CI said
    // otherwise. Participant *indices* are assigned by the owner's serving
    // thread, which frees a departed client's index from `on_hangup` when epoll
    // reports `RDHUP`/`HUP` (`tf_tree_ipc::OwnerServer::serve`). Whether the
    // refused attach's index has been freed by the time the next joiner is
    // accepted is therefore a race between that loop and this thread — the lock
    // *byte* is released synchronously above, the *index* is not. It read 1 in
    // 30 of 30 local runs and something else on a shared CI runner, which is the
    // race resolving both ways rather than a defect either way.
    let peer = join_read_write();
    assert!(peer.is_writable(), "the peer joined read-only");

    drop(peer);
    drop(owner);
}

/// Two independent attachments race `reparent`, and only the **arena** lock can
/// stop them colliding.
///
/// Each `Tree` here is its own attachment: its own participant slot and its own
/// process-local `decl` mutex. That mutex is precisely the thing that does not
/// generalise across a boundary, so it serializes nothing between these two —
/// exactly the situation a second process is in. What is left is A2's in-arena
/// lock, and if it did not work these threads would race the topology block copy
/// and lose or corrupt mutations.
///
/// Verified in-process rather than across a `fork` because the failure being
/// tested is a *data race on the shared bytes*, which needs both mutators alive
/// and interleaved; the process-boundary half is covered by the test above.
///
/// # Why this one test needs a rendezvous when nothing else in the file does
///
/// It used to take its two attachments from `Tree::attach_shared(fd,
/// ReadWrite)` on a duplicated descriptor. `docs/decisions/0028` plan step 0b
/// removed that: a read-write attach registers a participant record, and over a
/// bare descriptor there is no lock file to take the byte that decides whether
/// the record may be reclaimed. So the arena is created through
/// [`served_fixture`] and the peers join through [`join_read_write`], which is
/// now the only read-write path there is.
///
/// **The port preserves each property the test was written for, and the
/// assertions below say which line preserves which:**
///
/// * *Separate participant slots.* Each joiner is granted its own by the
///   owner's assigner, and the byte assertion below proves three distinct ones
///   are held — the strongest form this property has ever had here, because
///   `attach_shared`'s self-assignment left nothing outside the arena to check.
/// * *Separate process-local `decl` mutexes.* Still three distinct `Tree`
///   values, so still three distinct `Mutex<()>` fields; nothing about the
///   transport changes that, and a rewrite that shared one attachment would
///   have deleted the test rather than ported it.
/// * *Only A2's in-arena lock can serialize them.* The racing block is
///   unchanged, and so is the generation count that falsifies it.
/// * *A third view of the segment.* The owner tree, exactly as before.
#[test]
fn concurrent_reparents_from_separate_attachments_are_serialized() {
    let scratch = Scratch::new("reparent-race");
    let tree = served_fixture();

    let a = join_read_write();
    let b = join_read_write();

    // **Three participants, three bytes, and that is what the port bought.**
    // The owner took byte 0 when it created the arena and each joiner took its
    // own during the handshake, before its arena record was written. Byte 3 is
    // asserted free so this cannot pass against a build that reports every byte
    // held.
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

    // Two frames with their own edges, moved between two parents that are
    // themselves unrelated, so neither mutation can create a cycle.
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

    // Every mutation published exactly once. A lost generation means two writers
    // shared one scratch block; there is no way to gain one.
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
        // `lidar`'s ring is the 10 Hz edge and `imu_link`'s the 1 kHz one; both
        // cover NOW_NS, so a well-formed topology must resolve.
        plan.at(&guard, stamp)
            .unwrap_or_else(|e| panic!("{name} unresolvable after the race: {e:?}"));
    }
}
