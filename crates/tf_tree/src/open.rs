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
    boot_id, self_start_time, AccessMode, ArenaName, CreatePolicy, EnvVar, HelloRequest,
    HelloStatus, IpcError, OpenOutcome, OwnerServer, Rendezvous, RuntimeDir, SegmentDescriptor,
    ShutdownHandle, SocketProbe, SystemEnv, DEFAULT_OPEN_TIMEOUT,
};

use crate::tree::{BuildError, Tree, TreeBuilder};

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

/// Join the running arena, or create it.
///
/// Zero configuration: domain and name come from `$TF_TREE_DOMAIN` (else
/// `$ROS_DOMAIN_ID`, else 0) and `$TF_TREE_NAME` (else `default`), and the
/// runtime directory from `$TF_TREE_RUNTIME_DIR`, `$XDG_RUNTIME_DIR`, `/run`, or
/// `/tmp` in that order.
///
/// # Errors
///
/// See [`OpenError`]. With the default `create = IfAbsent` and no layout, an
/// absent arena is [`OpenError::NoLayoutToCreate`].
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
}

impl Default for Open {
    fn default() -> Open {
        Open::new()
    }
}

impl Open {
    /// Defaults: read-only, create-if-absent, the §3.4 timeout, env discovery.
    ///
    /// **`ReadOnly` is deliberate and differs from the Rust in-process default**
    /// (D18). A `PROT_READ` mapping makes a buggy consumer *incapable* of
    /// corrupting a robot's transform tree, enforced by the MMU rather than by
    /// convention, and most processes that open a tree are consumers.
    #[must_use]
    pub fn new() -> Open {
        Open {
            domain: None,
            name: None,
            mode: AttachMode::ReadOnly,
            create: CreatePolicy::IfAbsent,
            timeout: DEFAULT_OPEN_TIMEOUT,
            layout: None,
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
    #[must_use]
    pub fn create(mut self, create: CreatePolicy) -> Open {
        self.create = create;
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
    /// # Errors
    ///
    /// See [`OpenError`].
    pub fn open(self) -> Result<Tree, OpenError> {
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
        let mut probe = SocketProbe::new(request, self.timeout);

        let mut session = tf_tree_ipc::Open::new(rv.clone())
            .mode(request.mode)
            .create(self.create)
            .timeout(self.timeout)
            .open(&mut probe)
            .map_err(OpenError::Rendezvous)?;

        match session.outcome() {
            OpenOutcome::Joined => {
                let attached = session
                    .take_attached()
                    .ok_or(OpenError::Rendezvous(IpcError::ArenaAbsent))?;
                let slot = attached.response.participant_slot;
                let mut tree = Tree::attach_shared_at(attached.segment, self.mode, slot)?;
                // The socket and the lock file must outlive the handshake: the
                // first is how the owner learns we died (D17), the second is
                // what holds our participant byte. Parking them in the `Tree`
                // ties both to the lifetime a caller actually manages.
                tree.hold_attachment(session, attached.socket);
                Ok(tree)
            }
            OpenOutcome::Created | OpenOutcome::TookOver => {
                let builder = self.layout.ok_or(OpenError::NoLayoutToCreate)?;
                let mut tree = builder.build_shared(rv.name().as_str())?;
                let server = spawn_owner_server(&rv, &tree)?;
                tree.hold_ownership(session, server);
                Ok(tree)
            }
        }
    }
}

/// The owner's serving thread, and the handle that stops it.
pub(crate) struct OwnerThread {
    shutdown: ShutdownHandle,
    join: Option<std::thread::JoinHandle<()>>,
    /// Set by the loop when it returns, so `Drop` can tell "still serving" from
    /// "already stopped" without blocking on a thread that has gone.
    running: Arc<AtomicBool>,
}

impl OwnerThread {
    /// Stop the server and wait for the thread.
    pub(crate) fn stop(&mut self) {
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
    let view = tree.arena_view();
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

    // A second, read-only mapping so the serving thread can read the
    // participant table without borrowing the `Tree`. One extra mapping of an
    // already-resident segment costs page-table entries and nothing else, and
    // it keeps the thread's lifetime independent of the handle a caller holds.
    let table_fd = {
        use std::os::fd::AsFd;
        rustix_dup(segment.as_fd()).map_err(OpenError::Rendezvous)?
    };
    let table_arena = tf_tree_arena::MappedArena::attach(table_fd, AttachMode::ReadOnly)?;

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
                    let view = crate::ArenaView::new(&table_arena);
                    let table = view.participants();
                    for slot in 0..table.capacity() as u32 {
                        let bit = 1u64 << slot;
                        if granted_assign.get() & bit == 0 && table.identity(slot).is_none() {
                            granted_assign.set(granted_assign.get() | bit);
                            return Ok(slot);
                        }
                    }
                    Err(HelloStatus::NoParticipantSlots)
                },
                |slot| {
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
