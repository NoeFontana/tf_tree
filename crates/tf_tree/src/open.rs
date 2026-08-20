//! `tf_tree::open()` — zero-config rendezvous (`docs/PHASE2.md` §3.2).
//!
//! # The seam
//!
//! `tf_tree_ipc` knows the lock file and the socket; `tf_tree_arena` knows the
//! mapping; neither knows the other, and until this module existed nothing
//! joined them — so no process that was not a child could obtain the arena at
//! all. `docs/decisions/0005` puts the join here because [`Tree`]'s constructor
//! surface is private, and every other placement pays for the seam by widening
//! that API to a shape whose only consumer is the seam.
//!
//! # What the two outcomes owe
//!
//! - **Joined** — the owner handed over a segment fd and a participant slot.
//!   Map the fd, register into *that* slot, and hold the socket open, because
//!   its closure is how the owner learns this process is gone (D17).
//! - **Created / TookOver** — this process holds the arena, so it owes the
//!   service: bind the socket and answer handshakes for as long as it lives.
//!
//! # What this module does not do yet
//!
//! §3.5 takeover — a *participant* noticing the owner died and promoting itself
//! — is not here. It needs a watcher on the client socket and a second pass
//! through [`tf_tree_ipc::Open`] with `already_attached`, and it is behaviour
//! rather than plumbing. `docs/decisions/0005` step 5 covers it; this is the
//! part that makes `open()` work at all.

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

/// Re-exported so a caller does not have to depend on `tf_tree_ipc` directly
/// just to name a policy `open()` already takes.
pub use tf_tree_ipc::CreatePolicy;

/// A third description of the lock file, for claim leases (§6.1).
///
/// Its own description, like the liveness probe's and for the same reason:
/// `F_OFD_SETLK` conflicts are per open-file-description, so sharing one with
/// the `Session` would make this process's participant byte and its claim bytes
/// indistinguishable to itself. Separate descriptions keep each answer about
/// the thing it names.
fn open_claim_lock(rv: &Rendezvous) -> Result<std::sync::Arc<tf_tree_ipc::LockFile>, OpenError> {
    Ok(std::sync::Arc::new(
        tf_tree_ipc::LockFile::open(rv.lock_path()).map_err(OpenError::Rendezvous)?,
    ))
}

/// A kernel-authoritative liveness probe over the lock file (§5.1).
///
/// Holds its **own** open file description, deliberately. The alternative —
/// sharing the `Session`'s — would mean the probe could not see this process's
/// own byte, because `F_OFD_GETLK` reports only *conflicting* locks. That is
/// survivable (the caller guards its own slot anyway) but it makes a subtle
/// property load-bearing; a separate description makes the probe answer the
/// same way about every slot including ours.
///
/// The cost is one extra fd per attached tree, against a syscall that replaces
/// parsing `/proc` — which is an inference with a race in it, where this is the
/// kernel's own answer.
pub(crate) struct LivenessProbe {
    lock: tf_tree_ipc::LockFile,
}

impl LivenessProbe {
    /// Open a second description of the rendezvous lock file.
    fn open(rv: &Rendezvous) -> Result<LivenessProbe, IpcError> {
        Ok(LivenessProbe {
            lock: tf_tree_ipc::LockFile::open(rv.lock_path())?,
        })
    }

    /// Whether `slot`'s byte is held, or `None` if the kernel could not say.
    ///
    /// `None` rather than a guess: §6.2 requires this to fail safe, and the
    /// caller turns "cannot tell" back into the `/proc` inference rather than
    /// into a "dead" verdict that would steal a working process's claim.
    pub(crate) fn is_held(&self, slot: u32) -> Option<bool> {
        self.lock.probe_participant(slot).ok().map(|p| p.held)
    }
}

/// The rendezvous session a `Tree` from [`Open::open`] holds.
pub(crate) type JoinedSession = tf_tree_ipc::Session<tf_tree_ipc::Attached>;

/// What keeps a rendezvous-obtained [`Tree`] attached.
///
/// Held purely for its `Drop`: the session releases the participant lock byte,
/// the socket's closure tells the owner this process is gone (D17), and the
/// owner variant additionally stops the serving thread. Naming the fields with
/// a leading underscore is deliberate — nothing reads them, and the point is
/// that dropping them is the observable effect.
pub(crate) enum Attachment {
    /// This process joined somebody else's arena.
    Joined {
        _session: JoinedSession,
        _socket: std::os::fd::OwnedFd,
    },
    /// This process owns the arena and serves it.
    Owner {
        _session: JoinedSession,
        server: OwnerThread,
    },
}

impl Drop for Attachment {
    fn drop(&mut self) {
        // Stop serving *before* the session drops, so the socket is gone by the
        // time the ownership byte is released. The reverse order leaves a window
        // where a successor can take ownership and bind while this process is
        // still answering handshakes from the old socket — two servers, one
        // path, and clients split between them.
        if let Attachment::Owner { server, .. } = self {
            server.stop();
        }
    }
}

/// The arena and lock-file participant tables index the same slot space, and
/// nothing but this crate can see both constants to check it.
///
/// A disagreement would not fail loudly: the owner would grant a slot the arena
/// has no record for, or a reaper would walk past records no byte covers.
const _: () = assert!(
    tf_tree_ipc::MAX_PARTICIPANTS == tf_tree_arena::DEFAULT_MAX_PARTICIPANTS,
    "the lock file and the arena must agree on the participant slot space"
);

/// Why [`Open::open`] could not produce a [`Tree`].
///
/// `Copy` and `String`-free like every other error here, and the single place
/// the three previously-unrelated families meet: the rendezvous
/// ([`IpcError`]), the mapping ([`tf_tree_arena::ShmError`]) and construction
/// ([`BuildError`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The rendezvous failed — no runtime directory, no arena, a stuck peer,
    /// or an owner that refused this build.
    #[error("{0}")]
    Rendezvous(IpcError),
    /// The segment was handed over but could not be mapped.
    #[error("{0:?}")]
    Map(tf_tree_arena::ShmError),
    /// This process had to create the arena and could not.
    #[error("{0}")]
    Build(BuildError),
    /// `create` was not `Never`, but no layout was supplied, so there is
    /// nothing to create the arena *from*.
    ///
    /// Decision `0004` sizes an arena from its declared edges, so a creator must
    /// bring a [`TreeBuilder`]. A consumer that only ever joins should say so
    /// with `create = Never` and get [`IpcError::ArenaAbsent`] instead of this.
    #[error("no layout was supplied and the arena had to be created")]
    NoLayoutToCreate,
    /// [`AttachMode::ReadOnly`] was combined with a `create` policy other than
    /// [`CreatePolicy::Never`] (`docs/decisions/0019` §2a).
    ///
    /// A read-only creator is incoherent on its face: it asks to bring an arena
    /// into existence that it then cannot write, which is by definition the
    /// empty arena a consumer publishes nothing into while looking healthy.
    /// Refusing the combination removes that class by construction rather than
    /// by advice — which is `docs/API.md` R6 carried one step further, from
    /// *read-only is the default* to *read-only is the thing that cannot
    /// create*.
    ///
    /// Reported **before** the runtime directory is resolved, so a
    /// misconfiguration reports as itself rather than as whatever the machine
    /// happens to be missing.
    #[error("a read-only attach cannot create an arena: use CreatePolicy::Never, or AttachMode::ReadWrite")]
    ReadOnlyCannotCreate,
    /// [`Open::require_create`] was set and the rendezvous resolved to
    /// [`OpenOutcome::Joined`] — somebody else's arena is already live.
    ///
    /// The refusal `docs/decisions/0019` §3 question 1 settles for the ROS
    /// bridge, and `0015` depends on it: [`CreatePolicy`] has no
    /// "create, or refuse if one is already live" variant, so a second bridge
    /// would take the *join* path and start claiming edges in an arena it did
    /// not size. The session is dropped before this is returned, so the
    /// participant lock byte is released and the socket closed — a refused
    /// attach leaves nothing behind.
    #[error("an arena is already live at this rendezvous and require_create was set")]
    ArenaAlreadyLive,
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
/// Zero configuration: domain and name come from `$TF_TREE_DOMAIN` (else
/// `$ROS_DOMAIN_ID`, else 0) and `$TF_TREE_NAME` (else `default`), and the
/// runtime directory from `$TF_TREE_RUNTIME_DIR`, `$XDG_RUNTIME_DIR`, `/run`, or
/// `/tmp` in that order.
///
/// **This never creates anything.** [`Open::new`]'s defaults are the *consumer*
/// (`docs/decisions/0019` §2a), so a process that means to bring the arena into
/// existence says so with [`Open::create`] and [`Open::layout_if_creating`].
///
/// # Errors
///
/// See [`OpenError`]. On a machine where nothing is serving this is
/// [`IpcError::ArenaAbsent`], fast — see [`Open::await_open`] for the consumer
/// that would rather wait for the publisher to start.
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
    /// **`ReadOnly` is deliberate and differs from the Rust in-process default**
    /// (D18). A `PROT_READ` mapping makes a buggy consumer *incapable* of
    /// corrupting a robot's transform tree, enforced by the MMU rather than by
    /// convention, and most processes that open a tree are consumers.
    ///
    /// # Why `create` is [`CreatePolicy::Never`]
    ///
    /// It used to be [`CreatePolicy::IfAbsent`], which paired with the
    /// `ReadOnly` above into a configuration that asks to create an arena it
    /// cannot write — the one [`OpenError::ReadOnlyCannotCreate`] now rejects.
    /// A builder whose own documented defaults are an error is a defect on its
    /// own terms, so the defaults moved instead of the rule
    /// (`docs/decisions/0019` §2a and its plan's step 1, which supersedes
    /// `docs/decisions/0005` §3.2 and `docs/PHASE2.md` §3.2 on this one point).
    ///
    /// The defaults are therefore the *consumer*, and the error is reachable
    /// only by writing both halves out explicitly. A creator names both:
    /// [`Open::mode`] with [`AttachMode::ReadWrite`], [`Open::create`], and the
    /// [`TreeBuilder`] that decision `0004` sizes the arena from.
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
    /// [`CreatePolicy`] has three settings and none of them is "create, or
    /// refuse if one is already live" — `IfAbsent` silently joins and `Always`
    /// silently replaces. A process that owns an arena's topology needs the
    /// missing fourth answer: `docs/decisions/0019` §3 question 1 settles it for
    /// the ROS bridge of `0015`, where taking the join path would mean claiming
    /// edges in an arena somebody else sized.
    ///
    /// With this set, [`OpenOutcome::Joined`] becomes
    /// [`OpenError::ArenaAlreadyLive`] and the session is dropped before the
    /// error is returned — so the participant lock byte is released and the
    /// socket closed, and a refused attach is indistinguishable from one that
    /// never happened.
    ///
    /// It does **not** change what `create` means. `Never` plus this is a
    /// contradiction that reports as [`IpcError::ArenaAbsent`]: nothing to join
    /// and nothing permitted to create.
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
    /// A [`TreeBuilder`] rather than a raw layout, because decision `0004` sizes
    /// the arena from its declared edges and the creator also has to write the
    /// topology. A joiner never uses this — it maps what it is given.
    #[must_use]
    pub fn layout_if_creating(mut self, builder: TreeBuilder) -> Open {
        self.layout = Some(builder);
        self
    }

    /// Run §3.4 and produce a [`Tree`].
    ///
    /// One attempt. A consumer that starts before its publisher wants
    /// [`Open::await_open`].
    ///
    /// # Errors
    ///
    /// See [`OpenError`].
    pub fn open(mut self) -> Result<Tree, OpenError> {
        let per_attempt = self.timeout;
        self.attempt(per_attempt)
    }

    /// Run §3.4 repeatedly until an arena is there, or `timeout` runs out.
    ///
    /// **The first of `docs/decisions/0019` §2b's two waits**, and it exists
    /// because [`CreatePolicy::Never`] against an absent arena fails *fast* by
    /// design: a consumer racing its publisher's process *start* never obtains a
    /// [`Tree`] at all, so it cannot reach [`Tree::await_frames`]. Two absences,
    /// two names.
    ///
    /// # What is retried, and what is not
    ///
    /// Only [`IpcError::ArenaAbsent`] and [`IpcError::ArenaHeldButUnreachable`]
    /// — "never started" and "not yet", which `docs/decisions/0018` puts on one
    /// branch because a waiter cannot tell them apart and does the same thing
    /// about both. **Every other error is terminal and returned verbatim.**
    /// Retrying cannot change a `FORMAT_VERSION` disagreement, a layout hash
    /// mismatch or a missing runtime directory, and burning the budget against
    /// one would replace a precise message with a timeout.
    ///
    /// # There is no `Timeout` variant
    ///
    /// On expiry this returns the last retryable error it saw.
    /// [`IpcError::ArenaHeldButUnreachable`] already names the holder slots and
    /// the pid, and already means "I waited and it never resolved"; a second
    /// spelling would carry strictly less. [`IpcError::ArenaAbsent`] likewise
    /// says exactly what was true for the whole budget.
    ///
    /// # Granularity
    ///
    /// A bounded poll — `MIN_BACKOFF` doubling to `MAX_BACKOFF`, this crate's
    /// own pair, shared with [`Tree::await_frames`] and defined once in
    /// `crate::tree` — not a notification. `docs/decisions/0018`
    /// records why there is no arena-resident primitive to wake on, and it
    /// applies here with more force: topology settles once, at startup. So this
    /// returns *later* than the arena appeared, by up to one backoff interval
    /// plus scheduler granularity.
    ///
    /// [`Open::timeout`] is clamped to what is left of `timeout` on every
    /// attempt, or a default `Open` would let one held-but-unreachable attempt
    /// run the full [`DEFAULT_OPEN_TIMEOUT`] past the caller's deadline. That
    /// clamp is then floored at one backoff interval and truncated to whole
    /// microseconds, because the handshake's `SO_RCVTIMEO` rejects a value
    /// outside either bound and reports it as a *terminal* error; the comment on
    /// the loop has the two failures in full.
    ///
    /// # Errors
    ///
    /// See [`OpenError`].
    pub fn await_open(mut self, timeout: Duration) -> Result<Tree, OpenError> {
        let start = std::time::Instant::now();
        let mut backoff = MIN_BACKOFF;
        loop {
            // Clamp to what the *caller's* budget has left. `Open::timeout`
            // defaults to 5 s, so an unclamped attempt turns `await_open(1s)`
            // into a five-second call.
            //
            // **Floored at `MIN_BACKOFF`, and that is not tidiness.** The
            // handshake sets `SO_RCVTIMEO`/`SO_SNDTIMEO` from this value, and a
            // zero `Duration` there is `EINVAL` — reported as
            // `IpcError::ClientSocketSetup`, which is *terminal*, so a budget
            // that ran out exactly on an iteration boundary would replace the
            // rendezvous' real answer with a local socket error. The overrun
            // this permits is 200 µs, well inside one scheduler tick.
            //
            // **And truncated to whole microseconds, which is the same hazard
            // from the other end.** `SO_RCVTIMEO_NEW` carries a
            // `(tv_sec, tv_usec)` pair, and the conversion rounds the
            // sub-microsecond tail *up* without carrying into `tv_sec`: a
            // `Duration` in the last microsecond of a second becomes
            // `tv_usec == 1_000_000`, which the kernel rejects with `EDOM` —
            // again `IpcError::ClientSocketSetup`, again terminal. This loop is
            // the only place that *manufactures* a `Duration` by subtraction,
            // so it is the only place that produces one nobody wrote: a plain
            // `await_open(Duration::from_secs(1))` reaches here as
            // `999.999_9xx ms` on the first iteration and failed outright.
            // Observed as `setsockopt(4, SOL_SOCKET, SO_RCVTIMEO_NEW,
            // {tv_sec=0, tv_usec=1000000}) = -1 EDOM` under `strace`. Dropping
            // up to 999 ns costs nothing, and the floor above is a whole number
            // of microseconds so this cannot undercut it.
            let left = timeout.saturating_sub(start.elapsed());
            let per_attempt = core::cmp::max(core::cmp::min(self.timeout, left), MIN_BACKOFF);
            let per_attempt =
                Duration::new(per_attempt.as_secs(), per_attempt.subsec_micros() * 1_000);
            let err = match self.attempt(per_attempt) {
                Ok(tree) => return Ok(tree),
                Err(e) if is_retryable(e) => e,
                // Terminal: a version, layout or configuration disagreement no
                // amount of waiting alters.
                Err(e) => return Err(e),
            };
            // Deadline **after** the work and **before** the sleep: an attempt
            // that consumed the whole budget must report, not nap first.
            if start.elapsed() >= timeout {
                return Err(err);
            }
            // Never sleep past the caller's deadline.
            let left = timeout.saturating_sub(start.elapsed());
            std::thread::sleep(core::cmp::min(backoff, left));
            backoff = core::cmp::min(backoff * 2, MAX_BACKOFF);
        }
    }

    /// One pass of §3.4.
    ///
    /// `&mut self` so [`Open::await_open`] can call it repeatedly, and **the
    /// layout is cloned rather than taken** — which is the difference between an
    /// audit and a guarantee.
    ///
    /// The audit this replaces read: the only path that consumes the layout
    /// returns either `Ok` (no retry) or [`OpenError::NoLayoutToCreate`], which
    /// `is_retryable` classifies as terminal, so a second attempt never finds it
    /// missing where the first found it present. That was **wrong**. On the same
    /// `Created | TookOver` arm, `spawn_owner_server` returns
    /// `OpenError::Rendezvous(IpcError::ArenaAbsent)` when `tree.shared_fd()` is
    /// `None`, and `is_retryable` calls that **retryable** — so `await_open`
    /// would loop with the layout already gone and report `NoLayoutToCreate`, a
    /// diagnostic pointing at the caller for an internal invariant break.
    ///
    /// Unreachable today (a freshly `build_shared`-ed arena always has an fd),
    /// which is precisely why a comment is the wrong instrument: nothing would
    /// fail if the audit went stale again. Cloning removes the class instead of
    /// re-auditing it, and it is what `&mut self` cost in the first place —
    /// before `await_open` existed this method took `self` by value and simply
    /// moved the layout out.
    ///
    /// The price is one [`TreeBuilder`] clone per *creating* attempt, next to a
    /// `memfd_create`, an `mmap` and a socket bind. Consumers (`create =
    /// Never`) never reach it, and a creating caller reaches it at most once —
    /// `Created` either succeeds or fails terminally.
    fn attempt(&mut self, per_attempt: Duration) -> Result<Tree, OpenError> {
        // **Before `RuntimeDir::resolve()`, deliberately** (`docs/decisions/0019`
        // plan step 1). This is a property of the arguments alone; checking it
        // after would report a misconfigured builder as whatever the machine
        // happens to be missing, which is a diagnostic pointing at the wrong
        // process.
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

        let mut session = tf_tree_ipc::Open::new(rv.clone())
            .mode(request.mode)
            .create(self.create)
            .timeout(per_attempt)
            .open(&mut probe)
            .map_err(OpenError::Rendezvous)?;

        match session.outcome() {
            OpenOutcome::Joined => {
                if self.require_create {
                    // Drop the whole session before returning. It holds this
                    // process's participant lock byte and the connection whose
                    // closure tells the owner we are gone (D17), so returning
                    // the error while it lived would leave a slot taken and a
                    // client the owner still counts.
                    drop(session);
                    return Err(OpenError::ArenaAlreadyLive);
                }
                let attached = session
                    .take_attached()
                    .ok_or(OpenError::Rendezvous(IpcError::ArenaAbsent))?;
                let slot = attached.response.participant_slot;
                // `attach_joined_at`, not the `pub` `attach_shared_at`: that
                // one refuses `ReadWrite` because a caller holding a raw
                // descriptor holds no lock byte. Here the byte is already
                // taken — `session` is holding it, taken by `register_at`
                // during the handshake, before this record is written — which
                // is exactly the crate-private path's precondition
                // (`docs/decisions/0028` plan step 0b).
                let mut tree = Tree::attach_joined_at(attached.segment, self.mode, slot)?;
                tree.use_ofd_liveness(LivenessProbe::open(&rv)?);
                tree.use_claim_leases(open_claim_lock(&rv)?);
                // The socket and the lock file must outlive the handshake: the
                // first is how the owner learns we died (D17), the second is
                // what holds our participant byte. Parking them in the `Tree`
                // ties both to the lifetime a caller actually manages.
                tree.hold_attachment(session, attached.socket);
                Ok(tree)
            }
            OpenOutcome::Created | OpenOutcome::TookOver => {
                // `clone`, not `take` — see this method's doc comment. A retry
                // must find the layout exactly as the first attempt found it.
                let builder = self.layout.clone().ok_or(OpenError::NoLayoutToCreate)?;
                let mut tree = builder.build_shared(rv.name().as_str())?;
                tree.use_ofd_liveness(LivenessProbe::open(&rv)?);
                tree.use_claim_leases(open_claim_lock(&rv)?);
                let server = spawn_owner_server(&rv, &tree)?;
                tree.hold_ownership(session, server);
                Ok(tree)
            }
        }
    }
}

/// Whether [`Open::await_open`] should try again, or report this verbatim.
///
/// **Exactly two, and the list is the decision rather than a heuristic**
/// (`docs/decisions/0019` plan step 2). Both describe the publisher-mid-start
/// window: [`IpcError::ArenaAbsent`] is "nothing is there", and
/// [`IpcError::ArenaHeldButUnreachable`] is "something took the ownership byte
/// and has not begun serving". `docs/decisions/0018` puts "not yet" and "never
/// started" on one branch because a waiter cannot distinguish them and does the
/// same thing about both.
///
/// Everything else — a `FORMAT_VERSION` or layout-hash disagreement, a missing
/// runtime directory, a mapping failure, a builder misconfiguration — is
/// terminal. Retrying cannot change any of them, and burning the budget would
/// hand the caller a timeout where it had a precise message.
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
    /// `fork` copies the address space but **not the threads**: the child gets
    /// an `OwnerThread` value describing a thread that does not exist there. Its
    /// `JoinHandle` names nothing, and — worse — `ShutdownHandle` is an
    /// `eventfd` whose *description* the child inherited, so a write from the
    /// child is delivered to the **parent's** serving loop. Dropping an
    /// inherited `Tree` in a child would therefore shut down the parent's owner
    /// server, and every subsequent joiner would find an unreachable arena.
    fork_gen: u64,
    /// Set by the loop when it returns, so `Drop` can tell "still serving" from
    /// "already stopped" without blocking on a thread that has gone.
    running: Arc<AtomicBool>,
}

impl OwnerThread {
    /// Stop the server and wait for the thread.
    ///
    /// A no-op in a `fork` child — see [`OwnerThread::fork_gen`]. Both callers
    /// (this type's `Drop` and [`Attachment`]'s) route through here, so the
    /// check lives here rather than at each of them.
    pub(crate) fn stop(&mut self) {
        if self.fork_gen != tf_tree_ipc::fork::generation() {
            // Do not join a thread that was never forked, and do not signal the
            // parent's eventfd. Drop the handle so `Drop` does not try again.
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
/// A thread rather than a daemon: §3.5 makes ownership a role a survivor
/// inherits, which is only possible if any participant can bind.
fn spawn_owner_server(rv: &Rendezvous, tree: &Tree) -> Result<OwnerThread, OpenError> {
    // The assigner's own view of the lock file — see the closure below.
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

    // The serving thread needs the segment fd and the participant table for as
    // long as it runs. `try_clone` gives it an independent descriptor, so the
    // thread's lifetime is not tied to the `Tree`'s internals.
    let segment = tree
        .shared_fd()
        .ok_or(OpenError::Rendezvous(IpcError::ArenaAbsent))?;
    let segment = rustix_dup(segment).map_err(OpenError::Rendezvous)?;

    // A second mapping so the serving thread can reach the participant table
    // without borrowing the `Tree`. One extra mapping of an already-resident
    // segment costs page-table entries and nothing else, and it keeps the
    // thread's lifetime independent of the handle a caller holds.
    //
    // **Read-write, and it used to be read-only.** §3.9 says that when a
    // participant dies "the owner reaps its arena-side records"; the hangup
    // callback below is where that happens, and freeing a record is a CAS on the
    // arena. An owner always has a writable segment — it either created it or
    // took over by building one — so this cannot demote a read-only attachment.
    let table_fd = {
        use std::os::fd::AsFd;
        rustix_dup(segment.as_fd()).map_err(OpenError::Rendezvous)?
    };
    let table_arena = tf_tree_arena::MappedArena::attach(table_fd, AttachMode::ReadWrite)?;

    let running = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&running);
    let join = std::thread::Builder::new()
        .name("tf_tree-owner".into())
        .spawn(move || {
            use std::os::fd::AsFd;

            // Slots granted but not yet hung up. The arena table alone is not
            // enough: a joiner registers *after* it takes its lock byte, so
            // between the response and that registration the slot still reads
            // free — and re-granting it would hand two clients the same slot,
            // which is exactly what `register_at` exists to make impossible.
            // Shared between the two closures below, which `serve` holds
            // simultaneously — so a plain `u64` cannot be borrowed by both.
            // Single-threaded within this loop, hence `Cell` and not a mutex.
            let granted = std::rc::Rc::new(std::cell::Cell::new(0u64));
            let granted_assign = std::rc::Rc::clone(&granted);
            let granted_hangup = std::rc::Rc::clone(&granted);

            let _ = server.serve(
                segment.as_fd(),
                |_req| {
                    let view = tf_tree_core::arena_view::ArenaView::new(&table_arena);
                    let table = view.participants();
                    // `granted` is a u64, so the shift below is defined only
                    // for the first 64 slots. The const assert at the top of
                    // this file ties the table to exactly 64; this bound keeps
                    // that assert's failure a compile error rather than a
                    // shift overflow at runtime.
                    let n = table.capacity().min(64) as u32;
                    for slot in 0..n {
                        let bit = 1u64 << slot;
                        if granted_assign.get() & bit != 0 {
                            continue; // granted, not yet hung up
                        }
                        if table.identity(slot).is_some() {
                            continue; // a registered participant lives here
                        }
                        // **And the lock byte must be free too.** A read-only
                        // participant takes its byte but writes no arena
                        // record — `attach_shared` cannot register a
                        // `PROT_READ` mapping — so the table alone reports its
                        // slot empty. `mode="ro"` is the consumer default
                        // (D18) *and* the Python default, so this is the
                        // common case, not a corner. Granting such a slot
                        // hands the joiner a byte it cannot take; the
                        // `granted` bitmask hides that until an owner restart
                        // or a takeover clears it, after which the owner names
                        // the same slot forever and the joiner loops.
                        if lock_probe.is_held(slot).unwrap_or(false) {
                            continue;
                        }
                        granted_assign.set(granted_assign.get() | bit);
                        return Ok(slot);
                    }
                    Err(HelloStatus::NoParticipantSlots)
                },
                |slot| {
                    // **§3.9: "the owner reaps its arena-side records".** This
                    // is that sentence, and until it was written here nothing in
                    // the workspace performed it.
                    //
                    // A participant that is `SIGKILL`ed never runs `Tree`'s
                    // `Drop`, so its record stays `LIVE` for ever — and `assign`
                    // above skips a `LIVE` slot while `register_at` fills only a
                    // `FREE` one, so that slot could never be granted to anybody
                    // again. Measured before this existed, with
                    // `shm_torture --kill-hz 6`: 63 of the 64 slots held records
                    // for dead pids after thirty seconds, every subsequent
                    // attach was refused `NoParticipantSlots`, and the arena ran
                    // the remaining 29 minutes with no writer at all while the
                    // observer read four frozen rings and scored a perfect 256
                    // composed lookups per round out of them.
                    //
                    // **The incarnation is what makes this safe.**
                    // `ParticipantTable::release` is a single CAS of
                    // `live_word(incarnation) -> FREE`, so it frees the slot only
                    // if it is still the same occupancy this hangup is about. A
                    // participant that detached cleanly already released it and
                    // the CAS no-ops; one whose slot has since been re-granted
                    // has a different incarnation and the CAS fails. Neither can
                    // free a live participant's slot, which is the property
                    // `release`'s own doc comment is written around.
                    //
                    // Before the `granted` bit, so no `assign` can hand the slot
                    // out between the two — they run on this one thread, but the
                    // ordering costs nothing and does not rely on that.
                    let view = tf_tree_core::arena_view::ArenaView::new(&table_arena);
                    let table = view.participants();
                    if let Some((_pid, _start, incarnation)) = table.identity(slot) {
                        table.release(slot, incarnation);
                    }
                    // D17: the socket closed, so that participant is gone and
                    // its slot can be handed out again.
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
/// Reported as [`IpcError::ClientSocketSetup`] — a local resource failure of
/// this process, which is what running out of descriptors is. Naming it after
/// the lock file, as an earlier version did, would point an operator at a file
/// that is not involved.
fn rustix_dup(fd: std::os::fd::BorrowedFd<'_>) -> Result<std::os::fd::OwnedFd, IpcError> {
    fd.try_clone_to_owned()
        .map_err(|e| IpcError::ClientSocketSetup {
            raw_os_error: e.raw_os_error().unwrap_or(0),
        })
}

/// This process's name, NUL-padded, for the handshake's diagnostic field.
fn name_bytes() -> [u8; 32] {
    tf_tree_ipc::self_comm()
}
