//! `tf_tree::open()` — zero-config rendezvous (`docs/PHASE2.md` §3.2).
//!
//! # The seam
//!
//! `tf_tree_ipc` knows the lock file and the socket; `tf_tree_arena` knows the
//! mapping. This module joins them because [`Tree`]'s constructor surface is
//! private (`docs/decisions/0005`).
//!
//! # What the outcomes owe
//!
//! - **Joined** — map the fd, register into the granted slot, and hold the
//!   socket open: its closure is how the owner learns this process is gone (D17).
//! - **Created** — bind the socket and answer handshakes for as long as this
//!   process lives.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tf_tree_arena::AttachMode;
use tf_tree_ipc::{
    boot_id, self_start_time, AccessMode, ArenaName, EnvVar, HelloRequest, HelloStatus, IpcError,
    OpenOutcome, OwnerServer, Rendezvous, RuntimeDir, SegmentDescriptor, ShutdownHandle,
    SocketProbe, SystemEnv, DEFAULT_OPEN_TIMEOUT,
};

use crate::tree::{BuildError, Tree, TreeBuilder, MAX_BACKOFF, MIN_BACKOFF};

/// Re-exported so a caller need not depend on `tf_tree_ipc` to name the policy.
pub use tf_tree_ipc::CreatePolicy;

/// A third description of the lock file, for claim leases (§6.1).
///
/// `F_OFD_SETLK` conflicts are per open-file-description, so sharing the
/// `Session`'s would make this process's participant and claim bytes
/// indistinguishable to itself.
fn open_claim_lock(rv: &Rendezvous) -> Result<std::sync::Arc<tf_tree_ipc::LockFile>, OpenError> {
    Ok(std::sync::Arc::new(
        tf_tree_ipc::LockFile::open(rv.lock_path()).map_err(OpenError::Rendezvous)?,
    ))
}

/// A kernel-authoritative liveness probe over the lock file (§5.1).
///
/// Holds its own open file description so it answers the same way about every
/// slot including ours (`F_OFD_GETLK` reports only *conflicting* locks).
pub(crate) struct LivenessProbe {
    lock: tf_tree_ipc::LockFile,
    /// How many times [`Self::is_held`] has asked the kernel; read by
    /// [`reclamation_verdict_for_test`] and
    /// `a_free_word_is_decided_without_asking_the_kernel`.
    #[cfg(feature = "test-hooks")]
    probes: std::sync::atomic::AtomicU32,
}

impl LivenessProbe {
    /// Wrap a description this caller already opened.
    fn from_lock(lock: tf_tree_ipc::LockFile) -> LivenessProbe {
        LivenessProbe {
            lock,
            #[cfg(feature = "test-hooks")]
            probes: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Open a second description of the rendezvous lock file.
    fn open(rv: &Rendezvous) -> Result<LivenessProbe, IpcError> {
        Ok(LivenessProbe::from_lock(tf_tree_ipc::LockFile::open(
            rv.lock_path(),
        )?))
    }

    /// The description this probe holds, for callers that need a lock other
    /// than the participant bytes (the hangup callback reaps claim leases).
    pub(crate) fn lock(&self) -> &tf_tree_ipc::LockFile {
        &self.lock
    }

    /// Whether `slot`'s byte is held, or `None` if the kernel could not say.
    ///
    /// `None` rather than a guess: §6.2 requires failing safe.
    pub(crate) fn is_held(&self, slot: u32) -> Option<bool> {
        // Counted before the syscall: an `Err` is still a read of the byte.
        #[cfg(feature = "test-hooks")]
        self.probes.fetch_add(1, Ordering::Relaxed);
        self.lock.probe_participant(slot).ok().map(|p| p.held)
    }

    /// Test scaffolding; see the field.
    #[cfg(feature = "test-hooks")]
    fn probe_count(&self) -> u32 {
        self.probes.load(Ordering::Relaxed)
    }
}

/// What a reclamation sweep may do with one participant slot
/// (`docs/decisions/0028-the-slot-a-killed-participant-keeps.md`, piece 2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Reclamation {
    /// The record may be collected; `observed` is the `state` word the verdict
    /// was formed against.
    ///
    /// The word travels with the verdict because `ParticipantTable::reclaim`'s
    /// CAS guard must be the word read *before* the byte was probed
    /// ([`reclamation_verdict`], constraint 3).
    Reclaimable { observed: u32 },
    /// Somebody is running here, or the slot is this process's own.
    Live,
    /// No verdict, so not collectable (§6.2): the kernel declined to answer, or
    /// the slot holds no record (a `FREE` word).
    Unknown,
}

/// Whether `slot`'s participant record may be reclaimed — from the lock byte,
/// and from nothing else.
///
/// The one predicate for every reclamation decision (`0028` piece 2); it is
/// `docs/PHASE2.md` §5.1's *"whether it is live is a kernel fact"* in code.
///
/// # Scope
///
/// A [`LivenessProbe`] is installed by [`Open::attempt`] only, so this runs
/// only against a tree obtained from the rendezvous. Soundness there rests on
/// two steps of `0028` (open question 6):
///
/// - **Step 0b**: every participant that joined through the rendezvous holds a
///   byte. [`Tree::attach_shared`] and [`Tree::attach_shared_at`] refuse
///   [`AttachMode::ReadWrite`]. A `TreeBuilder::build_shared` creator served
///   through a hand-bound `OwnerServer` registers without a byte and would read
///   `Reclaimable`; that composition is out of contract
///   (`docs/decisions/0031-the-participant-record-with-no-byte.md`), and the
///   predicate is total over the supported population
///   (`a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`
///   characterises the rest).
/// - **Step 0c**: the byte at index `slot` is the byte of the record at `slot`.
///   Hand-rolled `tf_tree_ipc::Open` plus `build_shared` still chooses the two
///   indices separately (`defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`;
///   [`OpenError::ParticipantSlotDiverged`]). Without the assertion in
///   [`Open::attempt`]'s `Created` arm every verdict names the wrong process.
///   `0037` closed the takeover arm; do not delete 0c on that account.
///
/// # The three constraints
///
/// 1. **Not built on `record_is_alive`.** That opens with
///    `state_of(..) != LIVE`, deciding liveness from `state` (§5.1's bug), and
///    `/proc` maps a hidden entry to `NoSuchProcess`, the proof-of-death branch.
/// 2. **This process's own slot is skipped, first.** `F_OFD_GETLK` does not
///    report a description's own lock; do not lean on the probe's separate
///    description making it visible.
/// 3. **The state word is read before the byte is probed.** The `Acquire` load
///    of a `live_word` synchronises-with `fill_slot`'s `Release` store, so a
///    later probe sees the byte held. Reversed (or with an up-front holder
///    mask), `loom` erases a published record.
///
///    Pinned here only by `a_free_word_is_decided_without_asking_the_kernel`
///    (`crates/tf_tree/tests/rendezvous.rs`): word-first decides a `FREE` slot
///    with no syscall. The interleaving, and that `observed` is not re-read
///    after the probe, are pinned only by `reclaim_races_register` in
///    `crates/tf_tree_core/src/loom_tests.rs`.
///
/// # The `FREE` word is a live participant, more often than not
///
/// A live read-only joiner takes its lock byte but registers no arena record
/// (D18), so its slot reads `FREE` with the byte held. That is
/// [`Reclamation::Unknown`], never [`Reclamation::Reclaimable`]: `reclaim`
/// would CAS `FREE -> FREE`, succeed, and the slot would be handed to somebody
/// else while its byte is held. `a_live_read_only_joiner_is_unknown_not_reclaimable`
/// (`crates/tf_tree/tests/rendezvous.rs`) fails both mutations. The word answers
/// *is there a record*, never *is its process alive*.
pub(crate) fn reclamation_verdict(
    probe: &LivenessProbe,
    own_slot: u32,
    slot: u32,
    rec: &tf_tree_core::ParticipantRecord,
) -> Reclamation {
    // Constraint 2.
    if slot == own_slot {
        return Reclamation::Live;
    }
    // Constraint 3: the word first.
    let observed = rec.state.load(Ordering::Acquire);
    if tf_tree_core::participant::state_of(observed) == tf_tree_core::participant::FREE {
        return Reclamation::Unknown;
    }
    // Constraint 1: the kernel only. `None` is "would not say", not "dead".
    match probe.is_held(slot) {
        Some(true) => Reclamation::Live,
        Some(false) => Reclamation::Reclaimable { observed },
        None => Reclamation::Unknown,
    }
}

/// [`reclamation_verdict`] for `slot`, rendered as one line.
///
/// Test scaffolding, present only under `--features test-hooks`: the sweeps
/// report counts, which cannot separate the three answers. `own_slot` is a
/// parameter so a test can point it at a slot whose byte reads free and see the
/// own-slot guard rather than the byte.
///
/// The line is `reclaimable word 0x… probes=N`, `live probes=N` or
/// `unknown probes=N`, plus `no-lock-file` and `no-such-slot` (no count). `N` is
/// [`LivenessProbe::probe_count`], the only observable of the predicate's read
/// order: `probes=0` on a `FREE` word and on our own slot, `probes=1` on a
/// `LIVE` word.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
#[must_use]
pub fn reclamation_verdict_for_test(
    tree: &Tree,
    lock_path: &std::path::Path,
    own_slot: u32,
    slot: u32,
) -> String {
    let Ok(lock) = tf_tree_ipc::LockFile::open(lock_path) else {
        return "no-lock-file".to_string();
    };
    let probe = LivenessProbe::from_lock(lock);
    let view = tree.view();
    let Some(rec) = view.participants().get(slot) else {
        return "no-such-slot".to_string();
    };
    let verdict = match reclamation_verdict(&probe, own_slot, slot, rec) {
        Reclamation::Reclaimable { observed } => format!("reclaimable word {observed:#x}"),
        Reclamation::Live => "live".to_string(),
        Reclamation::Unknown => "unknown".to_string(),
    };
    // After the call, so it counts this verdict's syscalls only.
    format!("{verdict} probes={}", probe.probe_count())
}

/// The rendezvous session a `Tree` from [`Open::open`] holds.
pub(crate) type JoinedSession = tf_tree_ipc::Session<tf_tree_ipc::Attached>;

/// What keeps a rendezvous-obtained [`Tree`] attached.
///
/// Held for its `Drop`: the session releases the participant lock byte, the
/// socket's closure tells the owner this process is gone (D17), and the owner
/// variant also stops the serving thread.
pub(crate) enum Attachment {
    /// This process joined somebody else's arena.
    ///
    /// `rendezvous` is kept so a survivor inheriting the owner role can bind it
    /// ([`0037`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0037-a-takeover-is-not-a-second-open.md)
    /// question 1); the session takes byte 0 and the socket shows the owner died
    /// (`Tree::owner_lost`).
    Joined {
        session: JoinedSession,
        socket: std::os::fd::OwnedFd,
        rendezvous: Rendezvous,
    },
    /// This process owns the arena and serves it.
    ///
    /// **`_server` is declared before `_session`; do not reorder.** Fields drop
    /// in declaration order, so serving stops before the session releases the
    /// ownership byte. The reverse leaves two servers on one path.
    Owner {
        _server: OwnerThread,
        _session: JoinedSession,
    },
}

/// Every `docs/PHASE2.md` §11.3 crash point compiled into this crate.
///
/// Companion to [`tf_tree_core::crash::SITES`]; a harness arming sites at
/// random (§11.4) must read the literals from here rather than re-spell them.
#[cfg(feature = "crash-points")]
pub const CRASH_SITES: &[&str] = &[
    "takeover.after_ownership_lock_before_bind",
    "topo.holding_lock",
    "open.after_ownership_lock_before_bind",
    "open.after_create_before_bind",
    "reclaim.after_probe_before_cas",
    "hangup.after_probe_before_cas",
];

/// How [`Tree::inherit_ownership`] resolved (§3.5).
///
/// Anything but [`Inheritance::Inherited`] means this process is not the owner
/// and should keep behaving as a plain participant; lookups are unaffected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Inheritance {
    /// This process is now the owner and is serving the rendezvous.
    Inherited,
    /// [`Tree::owner_lost`] answered `false`, so nothing was attempted.
    ///
    /// Not final while `owner_lost` keeps answering `true`: byte 0 can be held
    /// by a fresh `open()` passing through `docs/PHASE2.md` §3.4 steps 2–4
    /// ([`0057`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md)
    /// Decision 3). Call again on the next pass.
    OwnerAlive,
    /// The ownership byte was taken when this process tried: another survivor
    /// won and is binding, or a fresh `open()` holds it in passing.
    ///
    /// Transient, not an error: this process keeps its slot, byte and mapping
    /// ([`0037`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0037-a-takeover-is-not-a-second-open.md)).
    /// `Tree::owner_lost` asks the kernel whether byte 0 is held
    /// ([`0043`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0043-owner-lost-is-a-question-about-the-owner.md)),
    /// so a loser stops calling on its own and resumes if the new owner dies.
    /// §3.5's `retry connect with backoff` is deliberately not implemented; `0043`
    /// records the residue.
    Contended,
    /// This tree is a read-only attachment, so it cannot serve (D18); some
    /// read-write participant must be the heir.
    ReadOnly,
    /// This tree has no owner to inherit from: a heap tree, a frozen `.tft`, an
    /// `attach_shared` over an inherited fd, or a tree this process already owns.
    NotApplicable,
}

impl Tree {
    /// Inherit the owner role from a departed owner, and begin serving (§3.5).
    ///
    /// Ownership is a role: whichever participant holds byte 0 and the listening
    /// socket. The heir is chosen by an uncontended `F_OFD_SETLK`, so this is
    /// inheritance, not the election D16 rejects.
    ///
    /// # The trigger is the caller's
    ///
    /// Nothing happens until somebody calls this; pair it with
    /// [`Tree::owner_lost`], a non-blocking `poll` of the attach socket. There is
    /// no background thread (`docs/PHASE2.md` §3.5). Lookups do not pause during
    /// a takeover (§3.5).
    ///
    /// ```ignore
    /// if tree.owner_lost() {
    ///     // Contended or OwnerAlive: neither is final; the next pass asks again.
    ///     let _ = tree.inherit_ownership()?;
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// [`OpenError::Rendezvous`] if the `fcntl` fails, or whatever binding the
    /// rendezvous socket fails with. On every error path this process keeps its
    /// participant slot, byte and mapping, and gives back the ownership byte if
    /// it had taken one.
    ///
    /// # It takes `&self`
    ///
    /// Both bindings hold the tree in an `Arc`, where `Arc::get_mut` fails as
    /// soon as a plan or publisher holds a clone
    /// ([`0044`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0044-recovery-the-languages-a-robot-is-written-in-cannot-reach.md)
    /// Decision 1).
    #[cfg(all(feature = "shm", target_os = "linux"))]
    pub fn inherit_ownership(&self) -> Result<Inheritance, OpenError> {
        if !self.is_joined() {
            return Ok(Inheritance::NotApplicable);
        }
        if !self.owner_lost() {
            return Ok(Inheritance::OwnerAlive);
        }
        if !self.is_writable() {
            return Ok(Inheritance::ReadOnly);
        }
        // Every exit from here on puts it back: dropping it would release this
        // process's participant byte and close the socket.
        let (mut session, socket, rendezvous) = match self.take_attachment() {
            Some(Attachment::Joined {
                session,
                socket,
                rendezvous,
            }) => (session, socket, rendezvous),
            other => {
                self.put_attachment(other);
                return Ok(Inheritance::NotApplicable);
            }
        };

        // Taken on the description already holding our participant byte, so the
        // slot cannot move and nothing needs verifying (`0037`).
        let acquired = match session.take_over_ownership() {
            Ok(v) => v,
            Err(e) => {
                self.put_attachment(Some(Attachment::Joined {
                    session,
                    socket,
                    rendezvous,
                }));
                return Err(OpenError::Rendezvous(e));
            }
        };
        if !acquired {
            self.put_attachment(Some(Attachment::Joined {
                session,
                socket,
                rendezvous,
            }));
            return Ok(Inheritance::Contended);
        }

        // §11.3 `takeover.after_ownership_lock_before_bind`: byte 0 held,
        // nothing listening; executed by
        // `a_killed_heir_leaves_the_role_for_the_next_survivor`.
        #[cfg(feature = "crash-points")]
        tf_tree_core::crash::maybe_abort(CRASH_SITES[0]);

        match spawn_owner_server(&rendezvous, self) {
            Ok(server) => {
                // The old socket is already hung up; do not keep it open.
                drop(socket);
                self.put_attachment(Some(Attachment::Owner {
                    _server: server,
                    _session: session,
                }));
                Ok(Inheritance::Inherited)
            }
            Err(e) => {
                // Hand the role back: an owner of byte 0 that is not listening
                // makes the arena unjoinable.
                let _ = session.release_ownership();
                self.put_attachment(Some(Attachment::Joined {
                    session,
                    socket,
                    rendezvous,
                }));
                Err(e)
            }
        }
    }
}

/// The arena and lock-file participant tables index the same slot space, and
/// nothing but this crate can see both constants to check it.
const _: () = assert!(
    tf_tree_ipc::MAX_PARTICIPANTS == tf_tree_arena::DEFAULT_MAX_PARTICIPANTS,
    "the lock file and the arena must agree on the participant slot space"
);

/// Why [`Open::open`] could not produce a [`Tree`].
///
/// `Copy` and `String`-free; the one place the rendezvous ([`IpcError`]), the
/// mapping ([`tf_tree_arena::ShmError`]) and construction ([`BuildError`])
/// meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The rendezvous failed — no runtime directory, no arena, a stuck peer,
    /// or an owner that refused this build.
    #[error("{0}")]
    Rendezvous(IpcError),
    /// The segment was handed over but could not be mapped.
    #[error("{0}")]
    Map(tf_tree_arena::ShmError),
    /// This process had to create the arena and could not.
    #[error("{0}")]
    Build(BuildError),
    /// `create` was not `Never`, but no layout was supplied to create the arena
    /// from (`0004`). A consumer that only joins should use `create = Never`.
    #[error("no layout was supplied and the arena had to be created")]
    NoLayoutToCreate,
    /// [`AttachMode::ReadOnly`] was combined with a `create` policy other than
    /// [`CreatePolicy::Never`] (`docs/decisions/0019` §2a).
    ///
    /// A read-only creator would bring into existence an arena it cannot write
    /// (`docs/API.md` R6). Reported before the runtime directory is resolved.
    #[error("a read-only attach cannot create an arena: use CreatePolicy::Never, or AttachMode::ReadWrite")]
    ReadOnlyCannotCreate,
    /// [`Open::require_create`] was set and the rendezvous resolved to
    /// [`OpenOutcome::Joined`] (`docs/decisions/0019` §3 question 1).
    ///
    /// The session is dropped before this is returned, so a refused attach
    /// leaves nothing behind.
    #[error("an arena is already live at this rendezvous and require_create was set")]
    ArenaAlreadyLive,
    /// This process's participant **lock byte** and its arena participant
    /// **record** came out at different indices, so the arena was not published
    /// (`docs/decisions/0028` plan step 0c, issue #201).
    ///
    /// [`Tree::participant_slot`] is "the one number that indexes both tables",
    /// and §5.1's liveness predicate spends it that way; when they disagree
    /// every answer is about somebody else. The engine's bias is that a false
    /// "dead" corrupts and a false "alive" only delays, so this is an error, not
    /// a `debug_assert!`.
    ///
    /// No create path reaches it since `0035` (a creator takes byte 0 with one
    /// `F_OFD_SETLK`). What remains is hand-rolled `tf_tree_ipc::Open` plus
    /// `TreeBuilder::build_shared`, which is why the guard stays.
    ///
    /// # What a caller does about it
    ///
    /// Stop the process still holding the byte (`tf_tree participants` names it),
    /// or open with [`CreatePolicy::IfAbsent`]. There is no takeover arm
    /// (`docs/decisions/0037`).
    ///
    /// Nothing is left behind: the arena is dropped before the owner server
    /// binds and the session releases both bytes. Neither index is carried
    /// (`docs/API.md` R5).
    #[error("this process's participant lock byte and its arena participant record are different slots; the arena was not published")]
    ParticipantSlotDiverged,
}

impl From<IpcError> for OpenError {
    fn from(e: IpcError) -> OpenError {
        OpenError::Rendezvous(e)
    }
}
impl From<tf_tree_arena::ShmError> for OpenError {
    fn from(e: tf_tree_arena::ShmError) -> OpenError {
        OpenError::Map(e)
    }
}
impl From<BuildError> for OpenError {
    fn from(e: BuildError) -> OpenError {
        OpenError::Build(e)
    }
}

/// Join the running arena, read-only.
///
/// Domain and name come from `$TF_TREE_DOMAIN` (else `$ROS_DOMAIN_ID`, else 0)
/// and `$TF_TREE_NAME` (else `default`); the runtime directory from
/// `$TF_TREE_RUNTIME_DIR`, `$XDG_RUNTIME_DIR`, `/run`, or `/tmp` in that order.
///
/// **This never creates anything** (`docs/decisions/0019` §2a); use
/// [`Open::create`] and [`Open::layout_if_creating`] to create.
///
/// # Errors
///
/// See [`OpenError`]. Where nothing is serving this is
/// [`IpcError::ArenaAbsent`], fast; see [`Open::await_open`] to wait instead.
pub fn open() -> Result<Tree, OpenError> {
    Open::new().open()
}

/// The `docs/PHASE2.md` §3.2 builder.
pub struct Open {
    domain: Option<u32>,
    name: Option<ArenaName>,
    mode: AttachMode,
    create: CreatePolicy,
    timeout: Duration,
    layout: Option<TreeBuilder>,
    require_create: bool,
}

impl Default for Open {
    fn default() -> Open {
        Open::new()
    }
}

impl Open {
    /// Defaults: read-only, **never create**, the §3.4 timeout, env discovery.
    ///
    /// `ReadOnly` makes a buggy consumer incapable of corrupting the tree,
    /// enforced by the MMU (D18). `Never` because `ReadOnly` plus `IfAbsent` is
    /// [`OpenError::ReadOnlyCannotCreate`] (`docs/decisions/0019` §2a). A creator
    /// sets [`Open::mode`] to [`AttachMode::ReadWrite`], [`Open::create`], and the
    /// [`TreeBuilder`] that `0004` sizes the arena from.
    #[must_use]
    pub fn new() -> Open {
        Open {
            domain: None,
            name: None,
            mode: AttachMode::ReadOnly,
            create: CreatePolicy::Never,
            timeout: DEFAULT_OPEN_TIMEOUT,
            layout: None,
            require_create: false,
        }
    }

    /// Override the domain (default: `$TF_TREE_DOMAIN`, `$ROS_DOMAIN_ID`, 0).
    #[must_use]
    pub fn domain(mut self, domain: u32) -> Open {
        self.domain = Some(domain);
        self
    }

    /// Override the arena name (default: `$TF_TREE_NAME`, else `default`).
    ///
    /// # Errors
    ///
    /// [`IpcError::NameInvalid`] if the name is empty, over 64 bytes, or has a
    /// path separator in it.
    pub fn name(mut self, name: &str) -> Result<Open, OpenError> {
        self.name = Some(ArenaName::new(name, EnvVar::Name).map_err(OpenError::Rendezvous)?);
        Ok(self)
    }

    /// Read-only (default) or read-write.
    #[must_use]
    pub fn mode(mut self, mode: AttachMode) -> Open {
        self.mode = mode;
        self
    }

    /// Whether to create the arena when none exists.
    ///
    /// Anything other than [`CreatePolicy::Never`] needs [`Open::mode`] set to
    /// [`AttachMode::ReadWrite`]; the pair is [`OpenError::ReadOnlyCannotCreate`]
    /// otherwise.
    #[must_use]
    pub fn create(mut self, create: CreatePolicy) -> Open {
        self.create = create;
        self
    }

    /// Refuse to *join*: this process must be the one that creates the arena.
    ///
    /// No [`CreatePolicy`] means "create, or refuse if one is live"
    /// (`docs/decisions/0019` §3 question 1, for the ROS bridge of `0015`).
    /// With this set, [`OpenOutcome::Joined`] becomes
    /// [`OpenError::ArenaAlreadyLive`] and the session is dropped first.
    /// `Never` plus this reports as [`IpcError::ArenaAbsent`].
    #[must_use]
    pub fn require_create(mut self, require: bool) -> Open {
        self.require_create = require;
        self
    }

    /// How long to wait for a live-but-unreachable arena to resolve (§3.4).
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Open {
        self.timeout = timeout;
        self
    }

    /// The topology to create the arena *with*, if this process has to create it.
    ///
    /// A [`TreeBuilder`] because `0004` sizes the arena from its declared edges.
    /// A joiner never uses this.
    #[must_use]
    pub fn layout_if_creating(mut self, builder: TreeBuilder) -> Open {
        self.layout = Some(builder);
        self
    }

    /// Run §3.4 and produce a [`Tree`].
    ///
    /// One attempt; a consumer that starts before its publisher wants
    /// [`Open::await_open`].
    ///
    /// # Errors
    ///
    /// See [`OpenError`].
    pub fn open(mut self) -> Result<Tree, OpenError> {
        let per_attempt = self.timeout;
        self.attempt(per_attempt)
    }

    /// Run §3.4 repeatedly until an arena is there, or `timeout` runs out
    /// (`docs/decisions/0019` §2b).
    ///
    /// # What is retried
    ///
    /// Only [`IpcError::ArenaAbsent`] and [`IpcError::ArenaHeldButUnreachable`]
    /// (`docs/decisions/0018`). Every other error is terminal and returned
    /// verbatim.
    ///
    /// # There is no `Timeout` variant
    ///
    /// On expiry this returns the last retryable error it saw.
    ///
    /// # Granularity
    ///
    /// A bounded poll — `MIN_BACKOFF` doubling to `MAX_BACKOFF`, shared with
    /// [`Tree::await_frames`] — not a notification (`docs/decisions/0018`), so
    /// this returns later than the arena appeared by up to one backoff interval.
    /// [`Open::timeout`] is clamped to what is left of `timeout` on every
    /// attempt, floored at one backoff interval and truncated to whole
    /// microseconds (see the loop).
    ///
    /// # Errors
    ///
    /// See [`OpenError`].
    pub fn await_open(mut self, timeout: Duration) -> Result<Tree, OpenError> {
        let start = std::time::Instant::now();
        let mut backoff = MIN_BACKOFF;
        loop {
            // Clamp to the caller's remaining budget. Floored at `MIN_BACKOFF`
            // because a zero `Duration` is `EINVAL` from `SO_RCVTIMEO`, and
            // truncated to whole microseconds because a `tv_usec` of 1_000_000
            // is `EDOM`; both surface as the terminal `ClientSocketSetup`.
            let left = timeout.saturating_sub(start.elapsed());
            let per_attempt = core::cmp::max(core::cmp::min(self.timeout, left), MIN_BACKOFF);
            let per_attempt =
                Duration::new(per_attempt.as_secs(), per_attempt.subsec_micros() * 1_000);
            let err = match self.attempt(per_attempt) {
                Ok(tree) => return Ok(tree),
                Err(e) if is_retryable(e) => e,
                // Terminal: no amount of waiting alters it.
                Err(e) => return Err(e),
            };
            // Deadline after the work and before the sleep.
            if start.elapsed() >= timeout {
                return Err(err);
            }
            let left = timeout.saturating_sub(start.elapsed());
            std::thread::sleep(core::cmp::min(backoff, left));
            backoff = core::cmp::min(backoff * 2, MAX_BACKOFF);
        }
    }

    /// One pass of §3.4.
    ///
    /// `&mut self` so [`Open::await_open`] can call it repeatedly; the layout is
    /// cloned rather than taken, so a retry after a retryable failure on the
    /// `Created` arm still finds it.
    fn attempt(&mut self, per_attempt: Duration) -> Result<Tree, OpenError> {
        // Before `RuntimeDir::resolve()` (`docs/decisions/0019` plan step 1): a
        // misconfigured builder must not report as a missing machine resource.
        if self.mode == AttachMode::ReadOnly && self.create != CreatePolicy::Never {
            return Err(OpenError::ReadOnlyCannotCreate);
        }
        let rd = RuntimeDir::resolve().map_err(OpenError::Rendezvous)?;
        let domain = match self.domain {
            Some(d) => d,
            None => tf_tree_ipc::domain_from_env(&SystemEnv).map_err(OpenError::Rendezvous)?,
        };
        let name = match self.name {
            Some(n) => n,
            None => tf_tree_ipc::name_from_env(&SystemEnv).map_err(OpenError::Rendezvous)?,
        };
        let rv = Rendezvous::new(rd, domain, name);

        let request = HelloRequest {
            format_version: tf_tree_arena::FORMAT_VERSION,
            layout_hash: tf_tree_arena::layout_hash(),
            mode: match self.mode {
                AttachMode::ReadOnly => AccessMode::ReadOnly,
                AttachMode::ReadWrite => AccessMode::ReadWrite,
            },
            client_pid: std::process::id(),
            client_start_time: self_start_time().unwrap_or(0),
            client_boot_id: boot_id().unwrap_or([0; 16]),
            client_name: name_bytes(),
        };
        let mut probe = SocketProbe::new(request, per_attempt);

        let ipc_open = tf_tree_ipc::Open::new(rv.clone())
            .mode(request.mode)
            .create(self.create)
            .timeout(per_attempt);
        let mut session = ipc_open.open(&mut probe).map_err(OpenError::Rendezvous)?;

        match session.outcome() {
            OpenOutcome::Joined => {
                if self.require_create {
                    // Drop the session first: it holds our participant byte and
                    // the connection the owner counts.
                    drop(session);
                    return Err(OpenError::ArenaAlreadyLive);
                }
                let attached = session
                    .take_attached()
                    .ok_or(OpenError::Rendezvous(IpcError::ArenaAbsent))?;
                let slot = attached.response.participant_slot;
                // `attach_joined_at`, not the `pub` `attach_shared_at`, which
                // refuses `ReadWrite`: here `session` already holds the byte
                // (`docs/decisions/0028` plan step 0b).
                let mut tree = Tree::attach_joined_at(attached.segment, self.mode, slot)?;
                tree.use_ofd_liveness(LivenessProbe::open(&rv)?);
                tree.use_claim_leases(open_claim_lock(&rv)?);
                // The socket and lock file must outlive the handshake (D17).
                tree.hold_attachment(session, attached.socket, rv);
                Ok(tree)
            }
            OpenOutcome::Created => {
                // §11.3 `open.after_ownership_lock_before_bind`: byte held,
                // nothing created; a killed process gives it back and the next
                // `open()` creates, so no arena is created twice.
                #[cfg(feature = "crash-points")]
                tf_tree_core::crash::maybe_abort(CRASH_SITES[2]);

                // `clone`, not `take`: a retry must find the layout again.
                let builder = self.layout.clone().ok_or(OpenError::NoLayoutToCreate)?;
                let mut tree = builder.build_shared(rv.name().as_str())?;

                // §11.3 `open.after_create_before_bind`: the arena exists and
                // nothing serves; the orphan memfd is freed with its last
                // mapping (§3.9). Before `use_ofd_liveness`, so no participant
                // byte is held.
                #[cfg(feature = "crash-points")]
                tf_tree_core::crash::maybe_abort(CRASH_SITES[3]);

                tree.use_ofd_liveness(LivenessProbe::open(&rv)?);
                tree.use_claim_leases(open_claim_lock(&rv)?);

                // Byte/record correspondence (`docs/decisions/0028` plan step 0c,
                // issue #201): the byte is chosen in `tf_tree_ipc`, the record in
                // the arena, and nothing else reconciles them.
                //
                // Before `spawn_owner_server`, while the arena is still private;
                // afterwards a joiner could hold it. That ordering is an argument,
                // not a tested property: no test can schedule a joiner between
                // these statements.
                if session.slot() != tree.participant_slot() {
                    // Record first, then byte: a healthy participant's order.
                    drop(tree);
                    drop(session);
                    return Err(OpenError::ParticipantSlotDiverged);
                }

                let server = spawn_owner_server(&rv, &tree)?;
                tree.hold_ownership(session, server);
                Ok(tree)
            }
        }
    }
}

/// Whether [`Open::await_open`] should try again, or report this verbatim.
///
/// Exactly [`IpcError::ArenaAbsent`] and [`IpcError::ArenaHeldButUnreachable`],
/// the publisher-mid-start window (`docs/decisions/0019` plan step 2,
/// `docs/decisions/0018`). Everything else is terminal.
fn is_retryable(err: OpenError) -> bool {
    matches!(
        err,
        OpenError::Rendezvous(IpcError::ArenaAbsent)
            | OpenError::Rendezvous(IpcError::ArenaHeldButUnreachable { .. })
    )
}

/// The owner's serving thread, and the handle that stops it.
pub(crate) struct OwnerThread {
    shutdown: ShutdownHandle,
    join: Option<std::thread::JoinHandle<()>>,
    /// The fork generation the thread was spawned in.
    ///
    /// `fork` does not copy threads, and `ShutdownHandle` is an `eventfd` whose
    /// description the child shares, so stopping from a child would shut down
    /// the parent's server.
    fork_gen: u64,
    /// Set by the loop when it returns, so `Drop` need not block on a gone thread.
    running: Arc<AtomicBool>,
}

impl OwnerThread {
    /// Stop the server and wait for the thread; a no-op in a `fork` child.
    pub(crate) fn stop(&mut self) {
        if self.fork_gen != tf_tree_ipc::fork::generation() {
            // Neither join a thread that was never forked nor signal the
            // parent's eventfd.
            self.join = None;
            return;
        }
        let _ = self.shutdown.stop();
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
        self.running.store(false, Ordering::Release);
    }
}

impl Drop for OwnerThread {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bind the §3.7 socket and serve it from a thread for this arena's lifetime.
///
/// A thread rather than a daemon: §3.5 requires that any participant can bind.
fn spawn_owner_server(rv: &Rendezvous, tree: &Tree) -> Result<OwnerThread, OpenError> {
    // The assigner's own view of the lock file.
    let lock_probe = LivenessProbe::open(rv)?;
    let view = tree.view();
    let header = view.header();
    let desc = SegmentDescriptor {
        format_version: header.format_version,
        layout_hash: header.layout_hash,
        arena_size: header.arena_size,
        instance_uuid: header.instance_uuid,
        boot_id: header.boot_id,
    };

    let server = OwnerServer::bind_at(rv.sock_path(), desc, std::process::id())
        .map_err(OpenError::Rendezvous)?;
    let shutdown = server.shutdown_handle().map_err(OpenError::Rendezvous)?;

    // `try_clone` gives the thread an independent descriptor.
    let segment = tree
        .shared_fd()
        .ok_or(OpenError::Rendezvous(IpcError::ArenaAbsent))?;
    let segment = rustix_dup(segment).map_err(OpenError::Rendezvous)?;

    // A second mapping so the thread reaches the participant table without
    // borrowing the `Tree`. Read-write: the hangup callback frees records with
    // a CAS (§3.9), and an owner's segment is always writable.
    let table_fd = {
        use std::os::fd::AsFd;
        rustix_dup(segment.as_fd()).map_err(OpenError::Rendezvous)?
    };
    let table_arena = tf_tree_arena::MappedArena::attach(table_fd, AttachMode::ReadWrite)?;

    // The owner's own slot, which no sweep may collect (`reclamation_verdict`,
    // constraint 2); `Open::attempt` asserts it indexes both tables.
    let own_slot = tree.participant_slot();

    let running = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&running);
    let join = std::thread::Builder::new()
        .name("tf_tree-owner".into())
        .spawn(move || {
            use std::os::fd::AsFd;

            // Slots granted but not yet hung up: a joiner registers after it
            // takes its byte, so until then the slot still reads free and
            // re-granting it would hand two clients one slot. `Rc<Cell>`
            // because both closures below are alive at once and this loop is
            // single-threaded.
            let granted = std::rc::Rc::new(std::cell::Cell::new(0u64));
            let granted_assign = std::rc::Rc::clone(&granted);
            let granted_hangup = std::rc::Rc::clone(&granted);

            let _ = server.serve(
                segment.as_fd(),
                |_req| {
                    let view = tf_tree_core::arena_view::ArenaView::new(&table_arena);
                    let table = view.participants();
                    // `granted` is a u64; the const assert above ties the table
                    // to 64 slots, and this bound keeps a failure of it a
                    // compile error rather than a shift overflow.
                    let n = table.capacity().min(64) as u32;
                    for slot in 0..n {
                        let bit = 1u64 << slot;
                        if granted_assign.get() & bit != 0 {
                            continue; // granted, not yet hung up
                        }
                        let Some(rec) = table.get(slot) else {
                            continue;
                        };
                        // Is there a record here at all: whether `fill_slot`'s
                        // `FREE -> RESERVED` CAS could succeed. This is not a
                        // liveness decision from `state` (§5.1).
                        let word = rec.state.load(Ordering::Acquire);
                        if tf_tree_core::participant::state_of(word)
                            != tf_tree_core::participant::FREE
                        {
                            // The verdict comes from the kernel through the one
                            // predicate; deciding from `state` wedged the arena
                            // after 64 abnormal exits (`0028`, #184).
                            match reclamation_verdict(&lock_probe, own_slot, slot, rec) {
                                Reclamation::Reclaimable { observed } => {
                                    // Reclaim first, grant second: `fill_slot`
                                    // CASes from `FREE`. `observed` comes from
                                    // the verdict, never a reload (the failing
                                    // order; pinned only by the loom model).
                                    if !table.reclaim(slot, observed) {
                                        // The word moved; leave the slot, the
                                        // next handshake forms a fresh verdict.
                                        // Unpinned by any test (removing this
                                        // passes the rendezvous suite); kept on
                                        // the argument that it bounds a slot
                                        // re-registered and abandoned before the
                                        // byte probe below.
                                        continue;
                                    }
                                }
                                // A live participant holds the byte, or the
                                // slot is our own.
                                Reclamation::Live => continue,
                                // The kernel would not say: fail safe (§6.2).
                                // Cannot mean "no record"; the `if` above
                                // excluded `FREE`.
                                Reclamation::Unknown => continue,
                            }
                        }
                        // The lock byte must be free too: a read-only
                        // participant (D18) holds a byte but writes no record.
                        // A just-reclaimed slot is probed twice on purpose;
                        // `hold-participant` can take any byte at any moment,
                        // and losing that race only skips a slot.
                        if lock_probe.is_held(slot).unwrap_or(false) {
                            continue;
                        }
                        granted_assign.set(granted_assign.get() | bit);
                        return Ok(slot);
                    }
                    Err(HelloStatus::NoParticipantSlots)
                },
                |slot| {
                    // §3.9: "the owner reaps its arena-side records". A
                    // `SIGKILL`ed participant never runs `Tree`'s `Drop`.
                    //
                    // The single load handed to `reclaim` is one
                    // `compare_exchange(observed, FREE)`
                    // (`docs/decisions/0028` plan step 4). For a `live_word` the
                    // word is the incarnation, so a re-granted slot fails the
                    // CAS. A `RESERVED` word carries none, so the CAS can ABA
                    // against a new occupancy; what bounds that is the lock byte
                    // (steps 0b, 0c): the outcome is a spurious free, never a
                    // second occupant. `ParticipantTable::reclaim` states the
                    // precondition. The `granted` bit is cleared after the CAS.
                    let view = tf_tree_core::arena_view::ArenaView::new(&table_arena);

                    // §3.9's other half: revoke the dead participant's claims
                    // before its record is freed, so a restarted publisher
                    // granted its predecessor's slot can repair its own edges.
                    // `own_slot` is passed because `F_OFD_GETLK` does not report
                    // a description's own byte.
                    let revoked =
                        crate::tree::reap_claims(&view, lock_probe.lock(), Some(slot), own_slot);
                    let _ = revoked;

                    let table = view.participants();
                    if let Some(rec) = table.get(slot) {
                        let observed = rec.state.load(Ordering::Acquire);
                        if tf_tree_core::participant::state_of(observed)
                            != tf_tree_core::participant::FREE
                        {
                            // §11.3 `hangup.after_probe_before_cas`: one CAS, so
                            // no torn state; a lost reclamation is repaired by
                            // the assigner or `Tree::reap_participants`. The
                            // return is dropped: `false` means the word moved.
                            #[cfg(feature = "crash-points")]
                            tf_tree_core::crash::maybe_abort(CRASH_SITES[5]);

                            let _ = table.reclaim(slot, observed);
                        }
                    }
                    // D17: the socket closed; the slot can be handed out again.
                    granted_hangup.set(granted_hangup.get() & !(1u64 << slot));
                },
            );
            flag.store(false, Ordering::Release);
        })
        .map_err(|_| OpenError::Rendezvous(IpcError::ArenaAbsent))?;

    Ok(OwnerThread {
        shutdown,
        join: Some(join),
        running,
        fork_gen: tf_tree_ipc::fork::generation(),
    })
}

/// `dup` a borrowed fd into an owned one.
///
/// Reported as [`IpcError::ClientSocketSetup`]: a local resource failure, not
/// the lock file.
fn rustix_dup(fd: std::os::fd::BorrowedFd<'_>) -> Result<std::os::fd::OwnedFd, IpcError> {
    fd.try_clone_to_owned()
        .map_err(|e| IpcError::ClientSocketSetup {
            raw_os_error: e.raw_os_error().unwrap_or(0),
        })
}

/// This process's name, NUL-padded, for the handshake's diagnostic field.
///
/// The 32 is the **wire** width (`HelloRequest::client_name`, bytes `56..88`,
/// pinned by `the_byte_layout_is_pinned` and `docs/PHASE2.md` §3.7); the lock
/// file's identity `name` is `[u8; 16]` (`docs/decisions/0033`). Do not
/// collapse them.
fn name_bytes() -> [u8; 32] {
    let mut out = [0u8; 32];
    let comm = tf_tree_ipc::self_comm();
    out[..comm.len()].copy_from_slice(&comm);
    out
}
