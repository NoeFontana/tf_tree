//! `tf_tree::open()` against real processes.
//!
//! This is the test the whole Phase 2 completion milestone exists for: **a
//! process that is not a child obtains the arena and reads the same transforms
//! as its creator.** Until `docs/decisions/0005` steps 1-5 landed, the only
//! transport was fd inheritance, so this could not be written at all.
//!
//! Real processes, not threads. The kernel is what makes the claims true —
//! that a `SIGKILL`ed participant's socket closes, that its lock byte is
//! released without its cooperation — and none of that is stageable in one
//! process. Children open the lock file and the socket **by path**: an
//! inherited descriptor shares the open file description, so every contention
//! assertion would pass vacuously.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// A scratch runtime directory, removed when the test ends.
///
/// **`set_var` is process-wide, and that is safe here only because `nextest`
/// gives every test its own process.** Under plain `cargo test` these tests
/// share one process and one environment, so two `Scratch`es would race and the
/// loser would resolve the winner's rendezvous. Every recipe that runs this
/// target uses `cargo nextest run`; a `cargo test` invocation of it is not
/// supported and would fail intermittently rather than loudly.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let p = std::env::temp_dir().join(format!("tf_tree_rv-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        std::env::set_var("TF_TREE_RUNTIME_DIR", &p);
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A spawned helper, killed on drop so a failing assertion cannot leave a
/// process holding the arena in `/tmp`.
struct Kid(Child, Option<BufReader<std::process::ChildStdout>>);

impl Kid {
    fn spawn(dir: &PathBuf, args: &[&str]) -> Kid {
        // The bin target carries the crate's name, not the file's: this crate is
        // published, and `--features shm` installs whatever is here into the
        // user's `bin/`. The manifest argues it; the source stays
        // `src/bin/rendezvous_child.rs`.
        let exe = env!("CARGO_BIN_EXE_tf_tree_rendezvous_child");
        let child = Command::new(exe)
            .args(args)
            .env("TF_TREE_RUNTIME_DIR", dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the rendezvous child helper");
        Kid(child, None)
    }

    /// The child's next line. It flushes before it parks, so this returning is
    /// proof the step actually happened — no sleeps, no polling.
    fn line(&mut self) -> String {
        let reader = self
            .1
            .get_or_insert_with(|| BufReader::new(self.0.stdout.take().expect("piped stdout")));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read child line");
        line.trim_end().to_string()
    }

    /// `SIGKILL`, then reap. After `wait` returns the kernel has torn down the
    /// process's descriptors, so its locks are gone with no cooperation from
    /// it — which is the entire point.
    /// Nudge a child that is waiting on stdin.
    fn poke(&mut self) {
        use std::io::Write;
        if let Some(mut stdin) = self.0.stdin.take() {
            let _ = writeln!(stdin, "go");
        }
    }

    /// Ask a `join-rw-report` child about one slot and read its answer.
    ///
    /// Unlike [`Self::poke`] this keeps the pipe: the child answers one line
    /// per line it reads, so a caller can put the same question either side of
    /// a `SIGKILL` and compare, which is the only way to observe a transition
    /// rather than a state.
    ///
    /// `unstable`, with both its callers: the helper mode this drives reads a
    /// participant record's raw `state` word through `Tree::arena_view`, and
    /// that feature is what gates it (`docs/API.md` §2.6).
    #[cfg(feature = "unstable")]
    fn ask(&mut self, slot: u32) -> String {
        use std::io::Write;
        let stdin = self.0.stdin.as_mut().expect("piped stdin");
        writeln!(stdin, "{slot}").expect("write a slot query");
        stdin.flush().expect("flush the slot query");
        self.line()
    }

    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Kid {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// **The milestone's headline: a foreign process joins and agrees.**
///
/// Not merely "attach succeeded" — the joiner performs a real lookup and must
/// get the transform the creator published, **bit for bit**. Attaching to the
/// wrong segment, or to a segment whose header this build misreads, would still
/// let `open()` return `Ok`; only comparing the numbers rules that out.
#[test]
fn a_foreign_process_joins_and_reads_the_same_transform() {
    let scratch = Scratch::new("join");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    let published = owner.line();
    assert!(published.starts_with("owning "), "got {published}");
    let owner_value = published.strip_prefix("owning ").unwrap().to_string();

    let mut joiner = Kid::spawn(&scratch.0, &["join"]);
    let joined = joiner.line();
    assert!(
        joined.starts_with("joined "),
        "the joiner did not attach: {joined}"
    );
    let joiner_value = joined.strip_prefix("joined ").unwrap().to_string();

    assert_eq!(
        joiner_value, owner_value,
        "the joiner read a different transform than the owner published"
    );
}

/// `create = Never` fails fast when nothing is serving, rather than waiting.
///
/// The intended behaviour for a supervised consumer: a notebook or a
/// visualiser started before the robot must say so, not create an empty arena
/// that the real publisher then refuses to join (§3.2, D18).
#[test]
fn a_consumer_that_will_not_create_fails_fast_on_an_empty_machine() {
    let scratch = Scratch::new("never");
    let mut kid = Kid::spawn(&scratch.0, &["join"]);
    let line = kid.line();
    assert!(
        line.starts_with("error"),
        "expected a fast failure, got {line}"
    );
    assert!(
        line.contains("no arena"),
        "the error should name the absent arena: {line}"
    );
}

/// **The zero-argument convenience still joins a served arena.**
///
/// `tf_tree::open()` is the call a README reader types and the only consumer of
/// [`tf_tree::Open::new`]'s *defaults* anywhere in the workspace — every other
/// caller names `mode` and `create` explicitly. `docs/decisions/0019` moved that
/// `create` default from `IfAbsent` to `Never`, and nothing but this test would
/// notice if the move had broken the join path.
///
/// Bit-for-bit, like `a_foreign_process_joins_and_reads_the_same_transform`:
/// attaching to the wrong segment would still return `Ok`.
///
/// **Mutant: make `open()` pass `CreatePolicy::IfAbsent`** ⇒ still passes here
/// (an arena is serving, so the join path is taken either way), which is why
/// `a_read_only_attach_refuses_to_create` exists as well. **Mutant: break the
/// free function's body** ⇒ the child reports `error ...` and this fails.
#[test]
fn the_free_open_joins_a_served_arena() {
    let scratch = Scratch::new("free-open");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    let published = owner.line();
    assert!(published.starts_with("owning "), "got {published}");
    let owner_value = published.strip_prefix("owning ").unwrap().to_string();

    let mut joiner = Kid::spawn(&scratch.0, &["open-free"]);
    let joined = joiner.line();
    assert!(
        joined.starts_with("joined "),
        "tf_tree::open() did not join a served arena: {joined}"
    );
    assert_eq!(
        joined.strip_prefix("joined ").unwrap(),
        owner_value,
        "tf_tree::open() read a different transform than the owner published"
    );
}

/// **`docs/decisions/0019` §2a: a read-only attach cannot create.**
///
/// **The `layout_if_creating` is load-bearing and the test is vacuous without
/// it.** Without a layout, `ro` + a creating policy already failed, with
/// `NoLayoutToCreate`, so an assertion on the *new* variant would pass against
/// a build that never learned the rule. Supplying the builder is what makes the
/// old code reach `OpenOutcome::Created`.
///
/// Both creating policies, because `Always` is a different branch of the
/// rendezvous (it skips the split-brain yield) and only `IfAbsent` would be
/// covered otherwise.
///
/// **Mutant: allow the combination** ⇒ the first open returns `Ok` and the
/// re-open below finds a freshly created empty arena instead of `ArenaAbsent`.
#[test]
fn a_read_only_attach_refuses_to_create() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let _scratch = Scratch::new("ro-create");

    let layout = || {
        TreeBuilder::new()
            .default_interp(InterpPolicy::LerpSlerp)
            .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64)))
    };

    for policy in [CreatePolicy::IfAbsent, CreatePolicy::Always] {
        let err = tf_tree::Open::new()
            .mode(AttachMode::ReadOnly)
            .create(policy)
            .layout_if_creating(layout())
            .open()
            .err()
            .expect("a read-only creator must be refused");
        assert!(
            matches!(err, tf_tree::OpenError::ReadOnlyCannotCreate),
            "expected ReadOnlyCannotCreate for {policy:?}, got {err:?}"
        );
    }

    // **And the machine is still empty.** The refusal has to happen before
    // anything is created, not after — an error returned over a segment that
    // now exists would leave the next `Never` consumer joining an empty arena
    // and reporting itself healthy.
    let err = tf_tree::Open::new()
        .create(CreatePolicy::Never)
        .open()
        .err()
        .expect("nothing should have been created");
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaAbsent)
        ),
        "the refused open left an arena behind: {err:?}"
    );
}

/// **`RUNBOOK.md`'s escape hatch out of `ArenaHeldButUnreachable`, run as written.**
///
/// `docs/PHASE2.md` §3.4 asks for it as `--force-new` and no binary ever grew
/// the flag (#189, and §0.0's row), so the procedure an operator follows is
/// `CreatePolicy::Always` on the process that creates the arena — and until this
/// test nothing above `tf_tree_ipc` ran it. That crate's
/// `create_always_overrides_the_split_brain_check` stops at the lock file; the
/// half a stranded operator needs is that the policy carries through
/// `build_shared` and comes back holding a `Tree`.
///
/// The stranded participant is a bare lock byte rather than a process, because a
/// held byte is the entirety of what §3.4 step 4 consults. A second process
/// would add a socket, a mapping and a race without changing the input.
///
/// The last two assertions are what separates the escape hatch from slot
/// reclamation (#184): it abandons the arena and **leaves the byte held**, so
/// the replacement's table is one slot smaller for as long as the survivor runs.
///
/// **Mutant: drop `self.create != CreatePolicy::Always &&` from step 4's
/// condition** ⇒ measured — `Always` yields the ownership byte like every other
/// policy, and the second half fails with `ArenaHeldButUnreachable`.
#[test]
fn the_escape_hatch_creates_over_a_stranded_participant() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let scratch = Scratch::new("force-new");
    let lock_path = scratch.0.join("0/default.lock");
    std::fs::create_dir_all(scratch.0.join("0")).unwrap();
    let survivor = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        survivor.try_take_participant(3).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );

    let layout = || {
        TreeBuilder::new()
            .default_interp(InterpPolicy::LerpSlerp)
            .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64)))
    };
    let creator = |policy| {
        tf_tree::Open::new()
            .mode(AttachMode::ReadWrite)
            .create(policy)
            .layout_if_creating(layout())
            .timeout(std::time::Duration::from_millis(100))
    };

    // The wedge itself: a held byte, nothing serving, and an ordinary creator
    // refusing rather than starting a second arena beside it.
    let err = creator(CreatePolicy::IfAbsent)
        .open()
        .err()
        .expect("a held participant byte must turn an ordinary creator away");
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaHeldButUnreachable { .. })
        ),
        "expected ArenaHeldButUnreachable, got {err:?}"
    );

    let tree = creator(CreatePolicy::Always)
        .open()
        .expect("CreatePolicy::Always must create over a stranded participant");
    assert_ne!(
        tree.participant_slot(),
        3,
        "the escape hatch took the stranded participant's slot"
    );

    // A second open file description, because `F_OFD_GETLK` reports only
    // *conflicting* locks and `survivor`'s own byte does not conflict with
    // itself — asking through `survivor` would report its own byte free.
    let witness = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert!(
        witness.probe_participant(3).unwrap().held,
        "abandoning the arena released the stranded participant's byte"
    );
}

/// **A pinned defect (#201), not desired behaviour. Invert or delete this test
/// when the divergence is fixed** — every assertion below states what the code
/// does today, and two of them state something wrong: that one process's lock
/// byte and its arena record are different integers, and that a live,
/// publishing arena owner reads dead.
///
/// `docs/PHASE2.md` makes the participant slot *one* number: its §0.0 table
/// says "the arena slot and the lock byte are the same integer", and §3.7's
/// `HelloResponse` repeats it field by field — `participant_slot` "matches the
/// lock-file byte the client must take". That is why
/// `Tree::participant_alive` can take an arena record index and hand it
/// straight to a probe of the lock byte at that index
/// (`crates/tf_tree/src/open.rs`'s `LivenessProbe::is_held`).
///
/// [`CreatePolicy::Always`] is the path on which it can break. §3.4 step 4 reads
/// `if self.create != CreatePolicy::Always && lock.any_participant_held()?`, so
/// the escape hatch skips the split-brain check *by design* and falls through
/// to `register_any` → `take_any_participant`, the first **free** byte. With
/// byte 0 already held the creator gets byte 1, while `build_shared` hands it
/// arena record **0** on a fresh arena. Nothing reconciles the two afterwards:
/// `hold_ownership` parks the session and never compares `Session::slot` with
/// `Tree::participant`.
///
/// **How that precondition arises is not known, and #201's answer to it is
/// measurably wrong.** The issue has the divergence biting "precisely in the
/// scenario the flag exists for: survivors are holding bytes" — an operator
/// force-creating past a wedge, with survivors on `0..k`. Run, that scenario
/// does not diverge. One `own` child plus two `join-rw` children hold bytes
/// `[0, 1, 2]`; `SIGKILL` the owner and the held set is `[1, 2]`; a
/// `CreatePolicy::Always` creator then takes byte **0** *and* arena record
/// **0**, one integer again. That is structural, not luck: an ordinary
/// creator's participant byte *is* 0, because step 4 refused to create while
/// any byte was held, and one `LockFile` description carries both it and the
/// ownership byte — so the death that frees the ownership byte frees byte 0 in
/// the same kernel action. §3.4 step 2 gates the hatch on that byte, so
/// against an owner that is wedged rather than dead (`SIGSTOP`, still holding
/// it) the hatch never reaches step 4 at all and `open()` fails
/// `ArenaHeldButUnreachable` naming the stuck slot. While the owner lives it
/// keeps arena record 0, and `open.rs`'s `assign` closure skips a record that
/// has an identity, so no joiner is granted slot 0 either: every byte a
/// survivor can hold is >= 1.
/// `the_escape_hatch_creates_over_a_stranded_participant` strands its byte at 3
/// and its creator takes byte 0 with record 0 — harmless for the same reason.
///
/// Those three readings were taken out of tree, driving
/// `tf_tree_rendezvous_child` and `tf_tree::Open` from a scratch binary, and
/// **nothing in this file pins them**. What they establish is negative: the
/// route #201 gives for reaching the divergence does not reach it.
///
/// So the divergence needs a **live holder of byte 0 that is not the arena's
/// owner**, and this test stages one with
/// `tf_tree_ipc::LockFile::try_take_participant(0)`, which nothing in the
/// workspace does outside tests. Two independent attempts to construct that
/// holder through `tf_tree::Open` alone failed. **A failed construction is not
/// an unreachability argument**, and settling which of the two this is — a
/// defect no public API can reach, or one nobody has found the route to — is
/// the question `0028` has to answer before #189's `--force-new` flag would
/// turn this policy into a documented operator procedure. #201's second path,
/// the takeover arm that also calls `register_any`, is out of reach for an
/// unrelated reason: nothing sets `Open::already_attached`. What is pinned
/// below is therefore the *consequence* of the divergence, on a staged
/// instance of it, and not its reachability.
///
/// Both signs below are read from a process that is not the creator, because
/// the creator's own `participant_alive(0)` hits `use_ofd_liveness`'s "never
/// report ourselves dead" guard — which compares the queried slot against the
/// *arena record* — and would show neither:
///
/// - **The right verdict, off the wrong byte.** While the survivor holds byte
///   0, record 0 reads alive on the strength of a byte belonging to somebody
///   else — a byte carrying no identity at all, since the survivor never wrote
///   one. The answer is nonetheless *correct* here: the process in record 0 is
///   alive. The genuinely wrong `alive` — the forced creator dies while the
///   survivor keeps byte 0, so record 0 reads alive about a dead process — is
///   **not measured, here or anywhere**. It would need a helper mode that
///   creates with `CreatePolicy::Always` inside a child, so the parent can
///   `SIGKILL` the creator and keep questioning a watcher that joined before
///   it died; `rendezvous_child.rs` has no such mode and this test does not add
///   one.
/// - **False dead, which is the corrupting one.** The survivor releases byte 0
///   and the verdict about record 0 flips to dead, though the process in that
///   record has not moved: it still holds byte 1, still owns the arena, still
///   publishes. **Nothing acts on that verdict destructively today**, which is
///   why this is a pinned defect rather than an incident: `Tree::reap_inner`
///   decides claim reaping from `lock.probe_claim(edge)` — the claim byte, held
///   by the live publisher — and not from `participant_alive`; the owner's
///   arena-record reap is driven by socket `HUP`; and `tf_tree_cli`'s
///   participant checks say "Detection only. Nothing here reclaims anything" in
///   as many words. The harm is conditional on a rescuer that reclaims from
///   this predicate — which `docs/PHASE2.md` §5.1 makes the authority on
///   liveness, and which `0028`'s piece 2 is a design for.
///
/// That same piece 2 would make the lock byte the *whole* predicate, with no
/// `record_is_alive` fallback — which does not soften this today (the fallback
/// is reached only on a probe *error*, and a free byte probes fine as
/// `Some(false)`) but would make the divergence unconditional rather than a
/// property of the probe a tree happens to carry.
///
/// Mutant: drop the `tree.use_ofd_liveness(...)` on `open`'s `Joined` arm, so
/// the watcher answers from the `/proc` inference instead of from the lock
/// byte. Applied: both asks report `slot 0 state live word 0x6 pid <creator>
/// alive true` and the final assertion fails. That is the reading of this test
/// — it measures the byte-index path and nothing else. Record 0's own pid was
/// alive the whole time, so every predicate that asked about *the record* got
/// the right answer; only the one that asked about the byte at its index did
/// not.
///
/// ---
///
/// **2026-08-20 — the consequence above is no longer constructible, and the
/// name is history** (`docs/decisions/0028` plan step 0c). The name is kept
/// because `CHANGELOG.md`'s `0.0.3` *Known issues*, `docs/PHASE2.md` §0.0's
/// participant-registry row and `0028` itself all cite it; renaming would leave
/// four dangling references for a cosmetic gain, and the transcript above is
/// the evidence those documents point at.
///
/// **Step 0c asked whether this test survives, on the reading that a facade
/// assertion cannot reach a state staged below the facade. Run, it does not
/// survive as written.** The state is staged below the facade, but the byte and
/// the record are *paired* at it — `Open::attempt` is what builds the arena and
/// registers the record — so the guard sits directly on this path. Measured, at
/// the guard's first green build: `CreatePolicy::Always must create over a
/// stranded participant: ParticipantSlotDiverged`, this test's own `expect`
/// panicking. So the staging is not a way around the assertion, which is the
/// answer the step wanted.
///
/// **What is lost, stated because it is the only thing this file loses.** The
/// measurement that gives the block above its force — a joined watcher
/// reporting `alive false` about a live, publishing process holding record 0 —
/// cannot be retaken through public API, because it needs a `Tree` whose byte
/// and record disagree and nothing will now produce one: `use_ofd_liveness` is
/// `pub(crate)` and installed only by the two arms of `Open::attempt`, one of
/// which is this refusal. That is the fix working, not a gap. The transcript
/// survives here, in `docs/PHASE2.md` §0.0 and in `0028`.
///
/// **What it pins now**, and neither is covered by its sibling
/// `defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`: that the
/// guard is *identity-blind* — this byte 0 carries no identity record at all,
/// where the sibling's is a real registration — and that a caller can act on
/// the refusal, which is the argument for returning an error rather than
/// asserting. Release the byte and the ordinary create succeeds with byte and
/// record at one index.
///
/// Mutant: delete the `session.slot() != tree.participant_slot()` guard from
/// `Open::attempt`. Applied: `.err().expect("a create that would diverge must
/// be refused")` panics on an `Ok(Tree)`.
///
/// Mutant: compare against `session.slot()` twice, i.e. a guard that can never
/// fire. Applied: the same `expect` panics — the recovery half of the test is
/// never reached, so a tautological guard cannot pass this file.
///
/// Mutant: `std::mem::forget` the session instead of dropping it on the refusal
/// path. Applied: `the refused creator kept a participant byte` fails,
/// `left: 3, right: 1`.
#[cfg(feature = "unstable")]
#[test]
fn defect_201_a_forced_creators_record_reads_dead_while_it_is_publishing() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, Stamp, TreeBuilder};

    let scratch = Scratch::new("slot-divergence");
    let lock_path = scratch.0.join("0/default.lock");
    std::fs::create_dir_all(scratch.0.join("0")).unwrap();

    // The stranded survivor, on **byte 0** — the index a fresh arena also gives
    // its first participant record. A separate open file description, so its
    // byte conflicts with everything that asks about it.
    //
    // **Staged through `tf_tree_ipc`, because no sequence of `tf_tree::Open`
    // calls is known to produce it** — see the doc comment. An owner death
    // frees byte 0 along with the ownership byte, so the operator scenario
    // #201 names arrives here with byte 0 free and diverges from nothing.
    let survivor = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    assert_eq!(
        survivor.try_take_participant(0).unwrap(),
        tf_tree_ipc::LockAttempt::Acquired
    );

    // **Refused, and this is the answer to the question `0028` plan step 0c
    // left open.** The state is staged *below* the facade, but the byte and the
    // record are still *paired* at the facade — `Open::attempt` is what builds
    // the arena and registers the record — so the guard does reach it. Measured
    // before this test was converted: the line that used to stand here,
    // `.expect("CreatePolicy::Always must create over a stranded participant")`,
    // panicked with `ParticipantSlotDiverged`.
    let witness = tf_tree_ipc::LockFile::open(&lock_path).unwrap();
    let err = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::Always)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
        )
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .err()
        .expect("a create that would diverge must be refused");
    assert!(
        matches!(err, tf_tree::OpenError::ParticipantSlotDiverged),
        "expected ParticipantSlotDiverged, got {err:?}"
    );

    // **What this pins that its sibling cannot.** The guard compares two
    // integers and consults nothing else. This byte 0 carries *no identity
    // record at all* — `try_take_participant` writes none — so a guard that
    // tried to decide from the lock file's identity rows, or to excuse a
    // divergence whose byte names nobody, would have nothing to read here.
    assert!(
        witness.read_identity(0).unwrap().is_none(),
        "the staged survivor writes no identity, and the refusal must not need one"
    );
    assert!(
        witness.probe_participant(0).unwrap().held,
        "the refusal disturbed the staged survivor's byte"
    );
    assert_eq!(
        witness.held_participants().unwrap(),
        0b1,
        "the refused creator kept a participant byte"
    );
    assert!(
        !witness.probe_ownership().unwrap().held,
        "the refused creator kept the ownership byte"
    );
    // **No bound rendezvous socket survives the refusal.** What that catches is
    // a refusal that returns while the owner server it spawned is still alive;
    // what it does *not* catch is the guard's **placement**, because
    // `impl Drop for OwnerServer` unlinks either way. Both measured, in the
    // sibling test, which carries the note.
    assert!(
        !scratch.0.join("0/default.sock").exists(),
        "a refused create must not leave a bound rendezvous socket"
    );

    // **The refusal is recoverable, which is the argument for an error rather
    // than an assertion.** Stop the process holding the byte — the one thing
    // `OpenError::ParticipantSlotDiverged`'s documentation tells a caller to do
    // — and the ordinary path works, with byte and record at one index again.
    drop(survivor);
    let tree = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
        )
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .expect("with byte 0 released, an ordinary create must succeed");
    assert_eq!(
        tree.participant_slot(),
        0,
        "a fresh arena registers its creator at record 0"
    );
    assert_eq!(
        witness.held_participants().unwrap(),
        0b1,
        "and at lock byte 0 — the same integer, which is the whole invariant"
    );

    // And it is a working arena, not merely an opened one: the process the
    // guard refused to strand is publishing and reading back its own transform.
    let publisher = tree
        .claim(tree.frame("base").unwrap(), tree.frame("map").unwrap())
        .expect("claim the one edge");
    publisher
        .push(1_000, &tf_tree::exp_se3([1.0, 2.0, 3.0, 0.1, 0.2, 0.3]))
        .expect("push");
    let g = tree.guard();
    let plan = tree
        .plan(tree.frame("map").unwrap(), tree.frame("base").unwrap())
        .unwrap();
    assert!(
        plan.at(&g, Stamp::<tf_tree::SystemDomain>::from_nanos(1_000))
            .is_ok(),
        "the recovered creator must serve its own transform"
    );
}

/// **The same divergence, reached through published API with nothing staged.**
///
/// The test above pins the *consequence* on a state built with
/// `LockFile::try_take_participant(0)`, and says in as many words that no
/// sequence of `tf_tree::Open` calls was known to produce it. One does, and it
/// is neither of the two paths #201 was filed on — both of those were measured
/// not to diverge (#214, #215): an owner death frees byte 0 along with the
/// ownership byte, and nothing sets `Open::already_attached`.
///
/// The producer is [`tf_tree_ipc::Session::release_ownership`], which gives up
/// the **ownership** byte and keeps **participant byte 0**. That is exactly
/// what §3.5 asks of it — "give up the owner role while staying attached" —
/// and what it leaves behind is a live **non-owner** on byte 0, the state
/// `docs/PHASE2.md` §0.0 said nothing outside a test produces. A forced create
/// then takes byte 1 against arena record 0, and every liveness predicate reads
/// record 0's liveness from byte 0, which belongs to somebody else.
///
/// **What this adds over the test above is the route, so it stops where the
/// route is established.** The corrupting consequence is measured there and is
/// not measured twice here. One thing does differ and is worth an assertion:
/// the staged survivor writes no identity, so its byte names nobody, while this
/// one is a real registration — byte 0 carries a plausible `tf_tree
/// participants` row for a process that is not the one in record 0.
///
/// Both sessions are opened by path, so each holds its own open file
/// description and the bytes genuinely conflict; that is the same reason the
/// staged survivor above works inside one process. Identity is *not* read to
/// tell the two apart here, because in one process they share a pid — the
/// holder mask is what separates them.
///
/// **2026-08-20 — converted from a defect pin into a regression test**
/// (`docs/decisions/0028` plan step 0c). Everything above is kept because the
/// *route* is the finding: two derivations of how this state arises were wrong
/// before a reproduction found it, and this project keeps its refutations on
/// the page. What changed is the verdict at the end. `Open::attempt` now
/// compares `tf_tree_ipc::Session::slot` with `Tree::participant_slot` at the
/// single `hold_ownership` call site — the one place in the workspace where
/// both numbers are in scope, since `tf_tree_ipc` has no arena dependency and
/// cannot see a record index — and returns
/// [`tf_tree::OpenError::ParticipantSlotDiverged`] instead of a `Tree` whose
/// every liveness answer would be about the stranded session. The assertions
/// that used to read `tree.participant_slot() == 0` with `0b11` bytes held are
/// now the refusal, plus the proof that it leaves the rendezvous exactly as it
/// found it.
///
/// Mutant: delete the `session.slot() != tree.participant_slot()` guard from
/// `Open::attempt`. Applied: `.err().expect("the divergence must be refused,
/// not handed back as a Tree")` panics — the open returns `Ok` and the whole
/// test is downstream of that line.
///
/// Mutant: `std::mem::forget` the session instead of dropping it on the refusal
/// path. Applied: `the refused creator kept a participant byte` fails,
/// `left: 3, right: 1` — bytes 0 and 1 both held. That is the assertion that
/// makes this a refusal which costs a caller nothing rather than one that burns
/// a slot per attempt.
///
/// Mutant: drop `stranded` before opening the forced creator, so byte 0 is free
/// when it runs. Applied: the same `expect` panics, because the creator then
/// takes byte 0 itself, the two indices agree and the open legitimately
/// succeeds. That is the reading of this test — the stranded byte 0 is the
/// whole cause, and `CreatePolicy::Always` on its own diverges from nothing, so
/// the guard refuses no healthy create. (Before the conversion this mutant
/// failed the **holder-mask** assertion first, `left: 1, right: 3`.)
#[cfg(feature = "unstable")]
#[test]
fn defect_201_release_ownership_strands_a_live_non_owner_on_byte_0() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};
    use tf_tree_ipc::{ArenaName, EnvVar, NoServer, OpenOutcome, Rendezvous, RuntimeDir};

    let scratch = Scratch::new("release-ownership-divergence");
    let rendezvous = || {
        Rendezvous::new(
            RuntimeDir::resolve().unwrap(),
            0,
            ArenaName::new("default", EnvVar::Name).unwrap(),
        )
    };

    // An ordinary `tf_tree_ipc` consumer creates, then hands back the owner
    // role. Every call here is published API; none of it is test-only.
    let mut stranded = tf_tree_ipc::Open::new(rendezvous())
        .timeout(std::time::Duration::from_millis(500))
        .open(&mut NoServer)
        .expect("the first open must create");
    assert_eq!(stranded.outcome(), OpenOutcome::Created);
    assert_eq!(stranded.slot(), 0, "a creator takes participant byte 0");
    stranded
        .release_ownership()
        .expect("§3.5: giving up the owner role must succeed");
    assert!(!stranded.is_owner(), "the owner role was given up");

    let witness = tf_tree_ipc::LockFile::open(&scratch.0.join("0/default.lock")).unwrap();
    assert!(
        !witness.probe_ownership().unwrap().held,
        "release_ownership must free the ownership byte"
    );
    assert!(
        witness.probe_participant(0).unwrap().held,
        "and must keep participant byte 0 — §3.5 stays attached"
    );
    assert!(
        witness.read_identity(0).unwrap().is_some(),
        "unlike the staged survivor above, byte 0 names a real registration"
    );

    // The forced create. §3.4 step 4 lets it past precisely because it is
    // `CreatePolicy::Always`, which is the escape hatch's whole purpose — and
    // one line further on, `Open::attempt` compares the byte it was handed with
    // the record `build_shared` registered it at, and refuses.
    let err = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::Always)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
        )
        .timeout(std::time::Duration::from_millis(500))
        .open()
        .err()
        .expect("the divergence must be refused, not handed back as a Tree");
    assert!(
        matches!(err, tf_tree::OpenError::ParticipantSlotDiverged),
        "**#201, closed.** The creator's lock byte would have been 1 and its \
         arena record 0, so every predicate reading record 0's liveness would \
         have asked the kernel about the stranded session's byte. Expected \
         ParticipantSlotDiverged, got {err:?}"
    );

    // **No bound rendezvous socket survives the refusal.** That is less than an
    // earlier revision of this comment claimed — it said the line below made the
    // guard's placement load-bearing — and it is worth keeping for what it does
    // catch: a refusal that returns while the owner server it spawned is still
    // alive. Measured: with the guard moved below `spawn_owner_server` *and*
    // the server `std::mem::forget`-ed instead of dropped, both `defect_201`
    // tests fail at this assertion — `a refused create must not leave a bound
    // rendezvous socket`.
    //
    // **It does not pin the placement, and nothing in this file does.** Measured
    // by moving the guard below `spawn_owner_server` on its own: the socket is
    // bound and published — an `eprintln!` from inside the refusal reports `sock
    // exists now = true` — and then `impl Drop for OwnerServer`
    // (`crates/tf_tree_ipc/src/server.rs:475`) unlinks the path it published, so
    // both tests stay green and this assertion never notices. The placement
    // rests on an argument instead, stated at the guard in
    // `crates/tf_tree/src/open.rs`: between the bind and the guard a joiner can
    // complete a §3.7 handshake and hold the segment, and refusing after that
    // would tear an arena out from under a process that did nothing wrong. That
    // needs a second process scheduled inside those two statements, which no
    // test here can arrange, so the ordering is defensible on its own merits and
    // untested.
    assert!(
        !scratch.0.join("0/default.sock").exists(),
        "a refused create must not leave a bound rendezvous socket"
    );

    // **And it left nothing behind.** Byte 1 — the one `register_any` handed
    // the forced creator — is free again, and so is the ownership byte it took
    // on the way in; only the stranded session's byte 0 is still held, by the
    // process entitled to it. A refusal that returned while its `Session` lived
    // would burn a slot per attempt, which is the failure
    // `require_create_refuses_a_live_arena_and_releases_its_slot` pins for the
    // other refusal on this path.
    assert_eq!(
        witness.held_participants().unwrap(),
        0b1,
        "the refused creator kept a participant byte"
    );
    assert!(
        !witness.probe_ownership().unwrap().held,
        "the refused creator kept the ownership byte"
    );
    assert!(
        witness.probe_participant(0).unwrap().held,
        "the refusal disturbed the stranded session's byte"
    );

    // The identity record at byte 1 outlives the byte, exactly as it does when
    // a process exits: §5.1 makes the *byte* the liveness and the record
    // advisory. Pinned so a later reader does not mistake the leftover row for
    // the leak the three assertions above rule out.
    assert!(
        witness.read_identity(1).unwrap().is_some(),
        "identity records are advisory, and releasing a byte does not erase one"
    );
}

/// **`Open::require_create` refuses to join, and leaves nothing behind.**
///
/// `CreatePolicy` has no "create, or refuse if one is already live" setting, so
/// a second arena owner — `docs/decisions/0015`'s ROS bridge is the caller this
/// exists for — would otherwise take the *join* path and start claiming edges
/// in an arena somebody else sized (`docs/decisions/0019` §3, question 1).
///
/// The second half is the part worth a test: a refusal that returned while its
/// `Session` lived would leave this process's participant lock byte taken and a
/// socket the owner still counts, so a bridge that retried a few times would
/// exhaust the slot table.
///
/// **Mutant: return `ArenaAlreadyLive` without dropping the session** ⇒ slot 1
/// reads held after the refusal and the second assertion fails.
/// **Mutant: ignore `require_create`** ⇒ the open returns `Ok` and the first
/// `expect` fails.
#[test]
fn require_create_refuses_a_live_arena_and_releases_its_slot() {
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let scratch = Scratch::new("require-create");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "));

    let err = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .require_create(true)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
        )
        .open()
        .err()
        .expect("a second owner must not silently join");
    assert!(
        matches!(err, tf_tree::OpenError::ArenaAlreadyLive),
        "expected ArenaAlreadyLive, got {err:?}"
    );

    // The owner holds slot 0. The refused attach was granted slot 1 and must
    // have given it back.
    let lock = tf_tree_ipc::LockFile::open(&scratch.0.join("0/default.lock")).unwrap();
    assert!(
        !lock.probe_participant(1).unwrap().held,
        "the refused attach kept its participant lock byte"
    );

    // And an ordinary consumer is unaffected — the refusal is about this
    // caller's intent, not about the arena.
    let mut joiner = Kid::spawn(&scratch.0, &["join"]);
    assert!(
        joiner.line().starts_with("joined "),
        "the refusal disturbed the arena"
    );
}

/// A consumer that starts before its publisher waits, and the wait resolves.
///
/// `docs/decisions/0019` §2b's first wait. The owner is spawned ~200 ms after
/// the wait begins, so `Ok` alone proves nothing — the elapsed lower bound is
/// what says the call really waited rather than racing to a lucky attach.
///
/// **Mutant: classify `ArenaAbsent` as terminal** ⇒ `await_open` returns
/// `Err(ArenaAbsent)` in microseconds and the `expect` fails.
#[test]
fn a_consumer_waits_for_an_arena_that_starts_late() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let scratch = Scratch::new("late-start");
    let dir = scratch.0.clone();

    let (tx, rx) = mpsc::channel::<Kid>();
    let spawner = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let mut owner = Kid::spawn(&dir, &["own"]);
        assert!(
            owner.line().starts_with("owning "),
            "the owner did not start"
        );
        // Hand it back so it stays alive for the assertions below; dropping it
        // here would kill the arena mid-test.
        let _ = tx.send(owner);
    });

    let started = Instant::now();
    let tree = tf_tree::Open::new()
        .await_open(Duration::from_secs(20))
        .expect("the wait should have outlasted a publisher 200 ms late");
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_millis(200),
        "the wait returned before the publisher could have started ({elapsed:?}) — \
         it did not actually wait"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the wait took far longer than the publisher's 200 ms delay: {elapsed:?}"
    );
    assert!(!tree.is_writable(), "the default attach is read-only (D18)");

    let owner = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the spawner thread never produced an owner");
    drop(tree);
    drop(owner);
    spawner.join().expect("spawner thread");
}

/// A wait with no publisher at all gives up inside a bounded time.
///
/// **Run on a worker thread with a `recv_timeout` on the main one, and that is
/// deliberate.** This repository has **no `.config/nextest.toml`**, so there is
/// no `slow-timeout` / `terminate-after` to bound a test that never returns: an
/// `await_open` that ignored its deadline would hang the whole suite instead of
/// failing one test. The channel is this test supplying its own bound.
///
/// **Mutant: ignore the deadline** ⇒ the `recv_timeout` expires and this fails
/// with the message below, rather than the run hanging.
#[test]
fn a_wait_for_an_arena_that_never_starts_gives_up() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let _scratch = Scratch::new("never-starts");

    let budget = Duration::from_millis(300);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        let outcome = tf_tree::Open::new().await_open(budget);
        let _ = tx.send((outcome.err(), started.elapsed()));
    });

    let (err, elapsed) = rx.recv_timeout(Duration::from_secs(30)).expect(
        "await_open never returned: it ignored its deadline. There is no \
         .config/nextest.toml in this repository, so nothing else would have \
         bounded this",
    );
    let err = err.expect("an empty machine has no arena to open");

    // **The last retryable error, verbatim — there is no `Timeout` variant.**
    // `ArenaAbsent` already says exactly what was true for the whole budget,
    // and a second spelling would carry strictly less.
    assert!(
        matches!(
            err,
            tf_tree::OpenError::Rendezvous(tf_tree_ipc::IpcError::ArenaAbsent)
                | tf_tree::OpenError::Rendezvous(
                    tf_tree_ipc::IpcError::ArenaHeldButUnreachable { .. }
                )
        ),
        "expected the last retryable rendezvous error, got {err:?}"
    );
    assert!(elapsed >= budget, "it gave up early: {elapsed:?}");
    assert!(
        elapsed < budget * 20,
        "it overran its budget by more than the backoff can explain: {elapsed:?}"
    );
}

/// **`docs/decisions/0019` §2b's second wait: a frame interned after the arena
/// already exists.**
///
/// The owner declares `frame_headroom`, because the shared fixture declares
/// none and a late intern into it fails `CapacityExceeded` — which would time
/// the waiter out for a reason that has nothing to do with the wait.
///
/// The consumer's id must equal the *owner's*, which is what rules out the
/// failure this method is shaped to avoid: a wait that interned the name itself
/// and handed back an id for a frame nobody declared.
///
/// **This is the only test in the repository that pins the predicate.**
/// `await_frames`'s `is_writable` guard fires before the predicate on every tree
/// a default build can construct, so `tests/await_frames.rs` is blind to it; a
/// read-only handle needs a live shared arena, and this is where one exists.
/// `just shm-rendezvous` is therefore the predicate's entire gate.
///
/// **Mutant: build the predicate on `Tree::frame`** (`match self.frame(name)`,
/// `is_writable` guard untouched) ⇒ verified. *"FAIL [0.009s] … the frame was
/// interned well inside the budget: Frame(ReadOnly)"* — the read-only consumer
/// is refused on the first probe and never resolves.
/// `a_frames_wait_for_a_name_nobody_will_intern_gives_up` fails with it
/// (*"left: Frame(ReadOnly), right: Timeout { … }"*), so the predicate has two
/// killers, both in this file. The same mutant leaves
/// `cargo nextest run -p tf_tree --test await_frames` at *"5 tests run: 5
/// passed"*, which is the measurement that moved this note here.
///
/// # What this test does not cover, stated rather than implied
///
/// An earlier revision claimed *"drop the memoization or the deadline ⇒ caught
/// by the elapsed bounds"*. **Both mutants pass**, measured:
///
/// - `if false && slot.is_some()` (memoization off) ⇒ *"15 tests run: 15
///   passed"*, this test at 0.210 s against 0.217 s unmutated, and the plain
///   `await_frames` target at *"5 tests run: 5 passed"*. It cannot be caught
///   here, and it cannot be caught anywhere: `find_frame` is idempotent and
///   frames are append-only, so re-probing a found name returns the same id for
///   a few hundred nanoseconds of hashing. The memoization is a **cost**
///   property with no observable behaviour, and this note says so instead of
///   inventing a guard for it. If that ever needs pinning it wants a benchmark,
///   not an assertion.
/// - `if false && start.elapsed() >= timeout` (deadline off) ⇒ **this test still
///   passes**. The frame arrives at 200 ms against a 20 s budget, so it never
///   reaches the deadline at all. That gap is real and is now closed by
///   `a_frames_wait_for_a_name_nobody_will_intern_gives_up` below — the only
///   test in the run that mutant fails — which is where the deadline claim
///   belongs.
///
/// What the elapsed bounds here *do* pin is that the call waited rather than
/// racing to a lucky read — the same thing they pin in
/// `a_consumer_waits_for_an_arena_that_starts_late`.
///
/// It also pins the **live** half of `ArenaBacking::is_frozen`, which
/// `tests/frozen.rs` cannot: mutating `Mapped(_) => true` ⇒ verified, *"the
/// frame was interned well inside the budget: FrozenTree"*.
#[test]
fn a_consumer_waits_for_a_frame_interned_after_the_arena_exists() {
    use std::io::Write;
    use std::time::{Duration, Instant};

    let scratch = Scratch::new("late-frame");

    let mut owner = Kid::spawn(&scratch.0, &["own-headroom"]);
    assert_eq!(owner.line(), "owning");

    let consumer = tf_tree::Open::new()
        .open()
        .expect("join the arena the owner already created");
    assert!(!consumer.is_writable(), "the default attach is read-only");
    assert!(
        consumer.frames().unwrap().iter().all(|n| n != "late_frame"),
        "the frame under test was already interned before the wait began"
    );

    // Poke from a thread, ~200 ms in, so the wait below has something to wait
    // for. Taking the pipe rather than calling `Kid::poke` leaves `owner`
    // borrowable for `line()` afterwards.
    let mut stdin = owner.0.stdin.take().expect("piped stdin");
    let poker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let _ = writeln!(stdin, "go");
    });

    let started = Instant::now();
    let [late] = consumer
        .await_frames(["late_frame"], Duration::from_secs(20))
        .expect("the frame was interned well inside the budget");
    let elapsed = started.elapsed();

    poker.join().expect("poker thread");
    let interned = owner.line();
    let owner_id: u32 = interned
        .strip_prefix("interned ")
        .expect(&interned)
        .parse()
        .unwrap();

    assert_eq!(
        late.get(),
        owner_id,
        "the waiter resolved to a different id than the owner interned"
    );
    assert!(
        elapsed >= Duration::from_millis(200),
        "the wait returned before the owner could have interned ({elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the wait far outlasted the intern it was waiting for: {elapsed:?}"
    );
}

/// **`AwaitError::Timeout`, which nothing else in any gate produces.**
///
/// Before this test the whole `if start.elapsed() >= timeout { … }` block in
/// `Tree::await_frames` could be deleted and every suite stayed green —
/// measured, `if false && start.elapsed() >= timeout` leaves `just test`'s
/// `await_frames` target at *"5 tests run: 5 passed"*, and in
/// `just shm-rendezvous` it fails **only** this test — *"15 tests run: 14
/// passed, 1 failed"*, so the fourteen that predate it are all blind to the
/// deadline. `docs/decisions/0019` §2b's second wait could hang a consumer
/// forever with nothing to say so. Untested along with it: the
/// first-missing-name hash, the `saturating_sub` clamp, and `all_interned::<0>`.
///
/// **The wait runs on a worker thread and the main thread bounds it with
/// `recv_timeout`.** This repository has **no `.config/` directory at all** —
/// verified, the root dotfiles are `.cargo`, `.claude`, `.git`, `.github`,
/// `.gitignore`, and `find` reports no `nextest.toml` anywhere — so there is no
/// `slow-timeout` or `terminate-after` profile setting, and a call that
/// ignored its deadline would wedge the whole run instead of failing one test.
/// The channel is this test supplying the bound nextest does not. It is the same
/// shape `a_wait_for_an_arena_that_never_starts_gives_up` uses one wait over.
///
/// The `Tree` is built *inside* the thread rather than moved into it: `Scratch`
/// has already put `TF_TREE_RUNTIME_DIR` in this process's environment, so the
/// thread resolves the same rendezvous, and nothing has to be `Send`.
///
/// **`["map", …]` and not just the missing name.** `map` is in the fixture
/// layout, so it resolves on the first probe and is memoized; the reported hash
/// must then be the *second* name's. A `Timeout` that hashed `names[0]` would
/// name a frame that is present, which is the most confusing answer available.
///
/// **Mutants, each applied, run, observed and reverted:**
///
/// - `if false && start.elapsed() >= timeout` (the deadline) ⇒ *"FAIL
///   [30.008s] … await_frames never returned: it ignored its deadline. There is
///   no .config/nextest.toml in this repository, so nothing else would have
///   bounded this: Timeout"*. Thirty seconds and a named failure, not a hang.
/// - `.next()` in place of `.find(|(_, slot)| slot.is_none())` (report
///   `names[0]` rather than the first *missing* name) ⇒ *"the timeout named the
///   wrong frame, or was not a timeout at all — left: Timeout { hash:
///   10663285463286226064 }, right: Timeout { hash: 15926179251682185921 }"*.
///   The left value is `map`'s hash, a frame that is present.
/// - `if N == 0 { return None; }` at the head of `all_interned` ⇒ *"a
///   zero-length request on a waitable tree must be answered without touching
///   the arena — left: Err(Timeout { hash: 0 }), right: Ok([])"*. That `hash: 0`
///   is the `map_or` fallback in the deadline branch, reached because no slot is
///   missing — so this mutant also exercises the arm whose comment calls itself
///   unreachable.
#[test]
fn a_frames_wait_for_a_name_nobody_will_intern_gives_up() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use tf_tree::AwaitError;
    use tf_tree_core::frame::blake3_64;

    /// Nothing in the helper's fixture interns this, and nothing in this test
    /// does either.
    const MISSING: &str = "no_publisher_will_ever_declare_this";

    let scratch = Scratch::new("frames-timeout");
    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(
        owner.line().starts_with("owning "),
        "the owner did not start"
    );

    let budget = Duration::from_millis(300);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let consumer = tf_tree::Open::new()
            .open()
            .expect("join the arena the owner is serving");
        let writable = consumer.is_writable();
        // **`all_interned::<0>` — reachable only from here.** The `N == 0` test
        // in `tests/await_frames.rs` runs on a heap tree, so it is refused by
        // the writable guard and the conversion helper is never called with an
        // empty array anywhere else in the workspace.
        let empty = consumer.await_frames([], Duration::from_millis(0));
        let started = Instant::now();
        let outcome = consumer.await_frames(["map", MISSING], budget);
        let _ = tx.send((writable, empty, outcome, started.elapsed()));
    });

    let (writable, empty, outcome, elapsed) = rx.recv_timeout(Duration::from_secs(30)).expect(
        "await_frames never returned: it ignored its deadline. There is no \
         .config/nextest.toml in this repository, so nothing else would have \
         bounded this",
    );

    assert!(!writable, "the default attach must be read-only (D18)");
    assert_eq!(
        empty,
        Ok([]),
        "a zero-length request on a waitable tree must be answered without \
         touching the arena"
    );

    let err = outcome.expect_err("a name nobody interned must not resolve");
    assert_eq!(
        err,
        AwaitError::Timeout {
            hash: blake3_64(MISSING)
        },
        "the timeout named the wrong frame, or was not a timeout at all"
    );
    assert!(elapsed >= budget, "it gave up early: {elapsed:?}");
    assert!(
        elapsed < budget * 20,
        "it overran its budget by more than the backoff can explain: {elapsed:?}"
    );
}

/// A `SIGSTOP`ped participant is alive; a `SIGKILL`ed one is not.
///
/// `docs/PROJECT.md` §5 D17 forbids treating staleness as death, because a
/// legitimately slow publisher is indistinguishable from a hung one. This is
/// that rule as an executable assertion.
///
/// **What this does *not* prove, stated because the obvious reading is wrong.**
/// It does not discriminate between `F_OFD_GETLK` and the `/proc` heuristic it
/// replaced: swapping the implementation back leaves this test passing, which
/// was verified rather than assumed. `/proc` gets both cases right here — a
/// stopped process still has a `/proc` entry, and a killed-and-reaped one does
/// not.
///
/// The case where `/proc` is genuinely wrong is **pid reuse**: a dead
/// participant's number handed to an unrelated process, which `record_is_alive`
/// only survives because it also compares the start time. Staging that requires
/// exhausting the pid space, so it is not tested here. What OFD actually buys
/// is that the answer is the kernel's rather than an inference — no parsing, no
/// permission dependence, and no window between reading a pid and acting on it.
/// That is an argument from construction, and this test is not evidence for it.
#[test]
fn a_stopped_peer_is_alive_and_a_killed_one_is_not() {
    let scratch = Scratch::new("liveness");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "));

    // A joiner that will be stopped, then killed. The owner holds slot 0, so
    // the first joiner takes slot 1.
    // Read-write, deliberately. A **read-only** joiner takes a lock byte but
    // writes no arena record — `attach_shared` skips registration when the
    // mapping is not writable — so `participant_alive` reports it dead before
    // the OFD probe is ever consulted. That asymmetry is real and is written up
    // in `docs/decisions/0005`; here it would only make the test measure the
    // wrong thing.
    let mut peer = Kid::spawn(&scratch.0, &["join-rw"]);
    assert!(peer.line().starts_with("joined "), "peer did not join");

    let observer_alive = |scratch: &PathBuf| {
        let mut k = Kid::spawn(scratch, &["peer-alive", "1"]);
        let line = k.line();
        k.kill();
        line
    };

    // Stopped: still holding its byte, so still alive.
    let pid = peer.0.id();
    assert!(
        std::process::Command::new("kill")
            .args(["-STOP", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success()),
        "could not SIGSTOP the peer"
    );
    assert_eq!(
        observer_alive(&scratch.0),
        "alive true",
        "a SIGSTOPped participant was reported dead — a slow publisher must \
         never be mistaken for a hung one (D17)"
    );

    // Killed: the kernel releases the byte, with no cooperation from the peer.
    let _ = std::process::Command::new("kill")
        .args(["-CONT", &pid.to_string()])
        .status();
    peer.kill();
    assert_eq!(
        observer_alive(&scratch.0),
        "alive false",
        "a SIGKILLed participant was still reported alive"
    );
}

/// **The hangup reap clears a joiner's slot and cannot clear the owner's — the
/// two halves of the same arena, measured against each other.**
///
/// `TFT014`'s participant arm fires on `state == LIVE` with a free lock byte.
/// This is the one shape of that state a test can stage out of real processes,
/// and it is `docs/decisions/0028`'s candidate-B hole 3, *"the owner's own slot
/// leaks unconditionally"* — the owner registers itself and nothing hangs up on
/// it, so no `LIVE -> FREE` transition exists for its record.
///
/// **The control matters more than the positive here.** #184 was written about
/// a `SIGKILL`ed rendezvous joiner under a running owner, and #191 closed that:
/// the owner's socket-hangup callback calls `ParticipantTable::release`, so the
/// killed peer's slot returns to `FREE` and `TFT014` correctly says nothing.
/// A test that only showed the leak would leave the reader unable to tell which
/// of the two shapes the check is for. Both are asserted, on one arena, from
/// one observer.
///
/// **The observer has to be a process that joined before the owner died.** With
/// the owner gone the rendezvous socket is gone, so a fresh attach is refused
/// `ArenaHeldButUnreachable` — asserted below, because it is also the reason
/// `tf_tree doctor --attach` cannot reach this arena: the check's predicate
/// covers the shape, the CLI's only live source cannot get to it.
///
/// Mutant: delete the `table.release(slot, incarnation)` from the hangup
/// callback in `crates/tf_tree/src/open.rs` (i.e. revert #191). Applied: the
/// `#191` assertion fails after the full two seconds of polling with *"it is
/// still: slot 1 state live word 0x6 pid … alive false"* — #184 reproduced,
/// and the exact predicate `TFT014` fires on.
/// Mutant B: have the child answer `alive` from its own `state` word instead
/// of from `Tree::participant_alive`. Applied: the dead owner's slot reports
/// *"slot 0 state live word 0x6 pid … alive true"* and the lock-byte assertion
/// fails. That mutant is `docs/PHASE2.md` §5.1's forbidden inference written
/// out, and it is the defect `0028` is about.
#[cfg(feature = "unstable")]
#[test]
fn the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live() {
    let scratch = Scratch::new("owner-slot");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "), "owner did not start");

    // Slots are handed out in join order, and each child is waited for before
    // the next is spawned, so these numbers are determined rather than hoped
    // for: owner 0, peer 1, watcher 2.
    let mut peer = Kid::spawn(&scratch.0, &["join-rw"]);
    assert!(peer.line().starts_with("joined "), "peer did not join");
    let mut watcher = Kid::spawn(&scratch.0, &["join-rw-report"]);
    assert_eq!(watcher.line(), "joined", "watcher did not join");

    for slot in 0..3 {
        let seen = watcher.ask(slot);
        assert!(
            seen.contains("state live") && seen.contains("alive true"),
            "all three participants must start out live and running: {seen}"
        );
    }

    // **#191, measured.** The owner is running, so its `epoll` sees the peer's
    // socket close and the callback releases the record. That is asynchronous
    // — a different process on a different thread — so this polls rather than
    // asserting once; a bounded wait that fails by timing out is the only
    // shape available for "something else will do this shortly".
    peer.kill();
    let mut seen = String::new();
    for _ in 0..100 {
        seen = watcher.ask(1);
        if seen.contains("state free") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        seen.contains("state free"),
        "the owner's hangup callback must release a killed joiner's record \
         (#191); it is still: {seen}"
    );

    // **Hole 3, measured.** Nothing hangs up on the owner, so its own record is
    // left exactly as `TFT014` describes: `LIVE`, with the kernel reporting its
    // lock byte free.
    owner.kill();
    let mut seen = String::new();
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        seen = watcher.ask(0);
        assert!(
            seen.contains("state live"),
            "no code path can clear the owner's own record, so it must stay \
             LIVE for the life of the segment: {seen}"
        );
    }
    assert!(
        seen.contains("alive false"),
        "the kernel released the dead owner's lock byte, so the liveness \
         predicate must say so: {seen}"
    );

    // **And nobody new can be told about it.** `doctor --attach` joins through
    // the rendezvous, which died with the owner, so the shape above is one the
    // CLI check cannot be pointed at — `docs/PHASE5.md` §6's `TFT014`
    // amendment says which shapes it can.
    let mut late = Kid::spawn(&scratch.0, &["join-rw"]);
    let refused = late.line();
    assert!(
        refused.starts_with("error") && refused.contains("unreachable"),
        "a fresh attach to an owner-less arena must be refused, or this leak \
         would be reachable from `doctor --attach`: {refused}"
    );
}

/// A read-only peer and a read-write peer get different slots.
///
/// **What this verifies, and what it cannot.** `mode="ro"` is the consumer
/// default (D18) and the Python default, and such a peer takes a lock byte but
/// writes *no* arena record — `attach_shared` cannot register a `PROT_READ`
/// mapping. So the owner's slot table and the lock file disagree about that
/// slot, which this test does observe: slot 1 holds a byte and no record, slot
/// 2 holds both.
///
/// It does **not** verify that the owner consults the lock file when assigning.
/// Removing that check leaves this test passing — verified, not assumed —
/// because the granted-slot bitmask already prevents a re-grant for the life of
/// one owner. The check matters only once §3.5 takeover exists and a *new*
/// owner inherits an arena whose read-only peers it never granted; then the
/// bitmask is empty, the arena table reports those slots free, and the owner
/// names one forever while the joiner loops. That scenario is unreachable
/// today, so the guard is forward-looking and this test does not cover it.
///
/// Kept anyway: the byte/record asymmetry is a real invariant, it is the thing
/// a future reader will be surprised by, and pinning it means a change to
/// read-only registration cannot pass unnoticed.
#[test]
fn a_read_only_peer_holds_a_byte_without_an_arena_record() {
    let scratch = Scratch::new("ro-slot");

    let mut owner = Kid::spawn(&scratch.0, &["own"]);
    assert!(owner.line().starts_with("owning "));

    // Read-only: takes a lock byte, writes no arena record.
    let mut ro = Kid::spawn(&scratch.0, &["join"]);
    assert!(
        ro.line().starts_with("joined "),
        "read-only peer did not join"
    );

    // A second joiner must land somewhere else. `join-rw` registers, so its
    // slot is observable.
    let mut rw = Kid::spawn(&scratch.0, &["join-rw"]);
    assert!(rw.line().starts_with("joined "), "second peer did not join");

    // The owner holds slot 0. If the assigner ignored the lock file it would
    // hand slot 1 to both peers; the read-write one would then be registered at
    // a slot whose byte belongs to the read-only one.
    let mut probe = Kid::spawn(&scratch.0, &["peer-alive", "1"]);
    let slot1 = probe.line();
    probe.kill();
    let mut probe2 = Kid::spawn(&scratch.0, &["peer-alive", "2"]);
    let slot2 = probe2.line();
    probe2.kill();

    // Exactly one of slots 1 and 2 carries a registered participant: the
    // read-write joiner. The read-only one has a byte and no record, which is
    // the asymmetry under test — what must not happen is *both* peers being
    // assigned the same slot.
    assert_eq!(
        (slot1.as_str(), slot2.as_str()),
        ("alive false", "alive true"),
        "the byte/record asymmetry changed: slot 1 should hold a read-only \
         peer's lock byte with no arena record, slot 2 a registered one"
    );
}

/// **A claim taken through `open()` also holds a kernel lease.**
///
/// The arena `ClaimRecord` alone cannot tell a live holder from a dead one — a
/// `SIGKILL`ed process leaves it set forever. The lease can, because the kernel
/// releases the byte with no cooperation. This checks both halves: that the
/// lease is taken at all, and that it is released when the holder dies.
#[test]
fn a_claim_takes_a_lease_and_a_dead_holder_releases_it() {
    let scratch = Scratch::new("claim-lease-e2e");

    let mut owner = Kid::spawn(&scratch.0, &["own-claiming"]);
    let line = owner.line();
    assert!(line.starts_with("claimed "), "got {line}");
    let edge: u32 = line.strip_prefix("claimed ").unwrap().parse().unwrap();

    // A separate description, from a process that knows nothing about the
    // claim, must see the byte held.
    let lock = tf_tree_ipc::LockFile::open(&scratch.0.join("0/default.lock")).unwrap();
    assert!(
        lock.probe_claim(edge).unwrap().held,
        "claiming through open() did not take the edge's lease"
    );

    owner.kill();
    assert!(
        !lock.probe_claim(edge).unwrap().held,
        "the lease outlived its holder: a dead writer would leak its edge"
    );
}

/// **A reaper must not reap itself.** `0005` calls this the single most
/// valuable test in the milestone, and it is, because the failure looks like a
/// bug somewhere else.
///
/// `F_OFD_GETLK` reports only *conflicting* locks, so a description never sees
/// its own — every edge this process holds reads lease-free. A literal §6.3
/// loop therefore revokes its own live writers, and A4 then *correctly* reports
/// `ClaimRevoked` on the next push. The operator sees a publisher that stopped
/// working and a reaper that says it reaped something; nothing points at the
/// reaper.
///
/// So the assertion is not "reaped 0" alone — it is that **the claim still
/// works afterwards**, which is the property that actually matters.
#[test]
fn a_reaper_does_not_reap_its_own_live_claim() {
    let scratch = Scratch::new("self-reap");
    let mut kid = Kid::spawn(&scratch.0, &["own-reap"]);
    assert_eq!(kid.line(), "claimed");
    kid.poke();
    assert_eq!(
        kid.line(),
        "reaped 0 still_ours true",
        "the reaper revoked its own live claim — F_OFD_GETLK does not report a \
         description's own locks, so every edge this process holds reads free"
    );
}

/// A killed writer's edge is reclaimed, and then reclaimable.
///
/// Reaping that clears the record but leaves the edge unclaimable would be
/// worse than not reaping: the operator sees a freed record and still cannot
/// publish.
#[test]
fn a_killed_writers_edge_is_reaped_and_can_be_reclaimed() {
    let scratch = Scratch::new("reap-dead");

    let mut owner = Kid::spawn(&scratch.0, &["own-reap"]);
    assert_eq!(owner.line(), "claimed");

    // A second process joins and claims the *other* edge of the fixture.
    let mut peer = Kid::spawn(&scratch.0, &["join-claiming"]);
    let claimed = peer.line();
    assert!(
        claimed.starts_with("claimed "),
        "peer did not claim: {claimed}"
    );

    peer.kill();

    // The owner sweeps. Its own claim survives; the dead peer's is reclaimed.
    owner.poke();
    let line = owner.line();
    assert!(
        line.starts_with("reaped 1 ") && line.ends_with("still_ours true"),
        "expected exactly the dead peer's edge to be reaped, got {line}"
    );
}

// ---------------------------------------------------------------------------
// The CAS-to-lease window (`docs/decisions/0005` §5)
// ---------------------------------------------------------------------------

/// The tree the in-window hook reaps from. A second, independent participant —
/// the reaper must not be the claimer, or the self-skip would fire.
#[cfg(feature = "test-hooks")]
static REAPER: std::sync::OnceLock<tf_tree::Tree> = std::sync::OnceLock::new();

/// Fires once. A `OnceLock` cannot be unset, so the hook disarms itself instead
/// — the point of the test is that the *retry* succeeds, which it cannot do if
/// a reaper keeps running inside every window.
#[cfg(feature = "test-hooks")]
static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// How many claims the in-window reaper actually cleared.
#[cfg(feature = "test-hooks")]
static REAPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "test-hooks")]
fn reap_from_inside_the_window() {
    use std::sync::atomic::Ordering;
    if !ARMED.swap(false, Ordering::Relaxed) {
        return;
    }
    if let Some(t) = REAPER.get() {
        REAPED.fetch_add(t.reap_dead(), Ordering::Relaxed);
    }
}

/// **The acquire window backs out.** `0005` step 7 has shipped this guard since
/// the lease landed and, until now, nothing could fail without it.
///
/// Between `edge::claim`'s CAS and the lease `SETLK` there is one syscall of
/// exposure. A reaper that runs inside it sees `record held ∧ lease free` —
/// which is precisely its "the holder is dead" signature — and clears a claim
/// that is in the middle of being taken. The claimer would then hold a lease on
/// an edge the arena reports free, and a third process could claim it: two
/// writers on one ring, which is what D7 and A4 exist to prevent.
///
/// `edge::reap` bumps the epoch *before* clearing the owner, and that ordering
/// is the whole reason the window is recoverable: re-reading the epoch after
/// taking the lease detects the reap.
///
/// The window is a syscall wide, so it cannot be hit by racing — hence
/// `CLAIM_WINDOW_HOOK`, which is what `--features test-hooks` exists for. Two
/// participants in one process, because the reaper must not be the claimer: a
/// process skips its own slots, so a self-reap would prove nothing.
///
/// **Mutant: delete the epoch re-check in `take_claim_lease`** ⇒ `claim`
/// returns `Ok`, and the writer it hands back publishes onto a record that was
/// reaped out from under it — visible immediately as `ClaimRevoked` from A4,
/// and as a second process being free to claim the same edge.
#[test]
#[cfg(feature = "test-hooks")]
fn the_acquire_window_backs_out() {
    use std::sync::atomic::Ordering;
    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, InterpPolicy, TreeBuilder};

    let _scratch = Scratch::new("acquire-window");

    let claimer = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::IfAbsent)
        .layout_if_creating(
            TreeBuilder::new()
                .default_interp(InterpPolicy::LerpSlerp)
                .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64))),
        )
        .open()
        .expect("create");
    let reaper = tf_tree::Open::new()
        .mode(AttachMode::ReadWrite)
        .create(CreatePolicy::Never)
        .open()
        .expect("join as a second read-write participant");
    assert_ne!(
        claimer.participant_slot(),
        reaper.participant_slot(),
        "both handles took the same slot, so the reaper would skip the claim as its own"
    );

    // `assert!(… .is_ok())` and not `.ok().expect()`: `OnceLock::set` hands the
    // value back in the `Err`, and `Tree` is not `Debug`, which is what the
    // older spelling was working around. `clippy::ok_expect` flags it, and no
    // recipe compiled this configuration until the line below in `shm-check`.
    assert!(REAPER.set(reaper).is_ok(), "set reaper");
    assert!(
        tf_tree::CLAIM_WINDOW_HOOK
            .set(reap_from_inside_the_window as fn())
            .is_ok(),
        "install hook"
    );

    let child = claimer.frame("base").unwrap();
    let parent = claimer.frame("map").unwrap();

    let err = claimer
        .claim(child, parent)
        .err()
        .expect("a claim reaped inside its own acquire window must not succeed");
    assert!(
        matches!(err, tf_tree::ClaimApiError::ReapedDuringClaim { .. }),
        "expected ReapedDuringClaim, got {err:?}"
    );
    assert_eq!(
        REAPED.load(Ordering::Relaxed),
        1,
        "the hook did not actually reap anything, so the guard was never exercised"
    );

    // **Backing out must give everything back.** A guard that detects the reap
    // and then leaks the record or the lease turns a recoverable race into a
    // permanently unclaimable edge, which is worse than the race.
    let writer = claimer
        .claim(child, parent)
        .expect("the retry after ReapedDuringClaim must succeed");
    writer
        .push(
            1_000,
            &tf_tree_math::exp_se3([0.0, 0.0, 0.1, 1.0, 0.0, 0.0]),
        )
        .expect("and the reclaimed edge must be publishable");
}
