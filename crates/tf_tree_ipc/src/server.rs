//! The owner half of the §3.7 attach handshake.
//!
//! # Who serves
//!
//! A thread in the **owning process**, not a daemon (`docs/decisions/0005` §3).
//! §3.5 makes ownership a role a surviving participant *inherits*, which is only
//! possible if any participant can bind. A daemon-only design would make
//! `tf_treed` a hard prerequisite and owner death fatal instead of recoverable.
//!
//! # Why this loop exists after the handshake is done
//!
//! §3.7 step 9 says keep the client sockets open; it never says who watches
//! them. D17 answers it: *"Participants hold their Unix socket open for the
//! lifetime of the attachment. Process death of any kind closes it, and the
//! owner sees `EPOLLHUP` in microseconds — exact, immediate, with no timeout to
//! tune."* That is the reap trigger, so the server keeps every accepted fd in
//! its `epoll` set and reports a hangup with the slot it granted. Holding the
//! fds without watching them would keep the cost and throw away the signal.
//!
//! # Policy is the caller's
//!
//! Slot assignment and reaping live in `tf_tree`, which has the arena. This
//! module does the protocol and calls out: `assign` turns a validated request
//! into a slot or a rejection, and `on_hangup` is told which slot went away. So
//! `tf_tree_ipc` still knows nothing about arenas (§2).

use std::path::{Path, PathBuf};

use rustix::event::epoll;
use rustix::fd::{BorrowedFd, OwnedFd};
use rustix::net::{
    accept_with, bind, listen, recvmsg, sendmsg, socket_with, AddressFamily, RecvFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType,
};

use crate::error::IpcError;
use crate::wire::{HelloRequest, HelloResponse, HelloStatus, SegmentDescriptor, HELLO_REQUEST_LEN};

/// Connection backlog. Generous: a thundering herd of participants at boot is
/// the expected case (§11.2 scenario 7), not an anomaly to shed.
const BACKLOG: i32 = 64;

/// How long the owner will wait on one client's half of the handshake.
///
/// Two messages over a connected local socket; a client that cannot manage that
/// in two seconds is not going to. Deliberately much shorter than the §3.4
/// open deadline, because this budget is per-client and that one is per-attempt
/// — a stalled peer must not consume the deadline of everybody queued behind it.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// `epoll` token for the listening socket.
const TOKEN_LISTENER: u64 = 0;
/// `epoll` token for the shutdown `eventfd`.
const TOKEN_SHUTDOWN: u64 = 1;
/// Client tokens start here, so neither of the above can be mistaken for one.
const TOKEN_CLIENT_BASE: u64 = 2;

/// A bound, listening §3.7 server.
pub struct OwnerServer {
    listener: OwnedFd,
    shutdown: OwnedFd,
    sock_path: PathBuf,
    desc: SegmentDescriptor,
    owner_pid: u32,
    /// The fork generation this server bound its socket in — see `Drop`.
    fork_gen: u64,
    /// `(st_dev, st_ino)` of the socket file this server published, captured
    /// from the *path* right after the `rename` — see `unlink_if_still_ours`.
    bound: (u64, u64),
}

/// Ask a running [`OwnerServer`] to stop.
///
/// Cloneable and `Send`, so the thread that owns the server does not have to be
/// the thread that stops it.
#[derive(Debug)]
pub struct ShutdownHandle {
    eventfd: OwnedFd,
}

impl ShutdownHandle {
    /// Wake the server and make it return.
    ///
    /// # Errors
    ///
    /// If the `eventfd` write fails, which means the server is already gone.
    pub fn stop(&self) -> Result<(), IpcError> {
        rustix::io::write(&self.eventfd, &1u64.to_ne_bytes()).map_err(|e| {
            IpcError::HandshakeIo {
                raw_os_error: e.raw_os_error(),
            }
        })?;
        Ok(())
    }
}

impl OwnerServer {
    /// Bind `sock_path` and start listening.
    ///
    /// # The bind sequence, and why it is not just `bind`
    ///
    /// §3.4 step 5 says "bind sock.tmp", naming no per-process suffix. Two
    /// processes taking ownership in sequence — or one stale file from a binder
    /// that died — then collide on `EADDRINUSE`, and a Unix socket path is not
    /// removed when its process exits. So: unlink any stale path, bind a
    /// **pid-suffixed** temporary, restrict it to the owner, and `rename` it
    /// into place. `rename` is atomic, so a client either sees no socket or a
    /// fully-listening one, never a bound-but-not-listening one.
    ///
    /// # Errors
    ///
    /// [`IpcError::SocketPathTooLong`] if the runtime directory pushes the path
    /// past `sun_path`; [`IpcError::HandshakeIo`] for the syscalls.
    pub fn bind_at(
        sock_path: &Path,
        desc: SegmentDescriptor,
        owner_pid: u32,
    ) -> Result<OwnerServer, IpcError> {
        let tmp = sock_path.with_extension(format!("sock.{owner_pid}"));
        let addr = crate::client::socket_addr(&tmp)?;
        // Validate the final path too, so an over-long name fails here rather
        // than after a successful bind to the temporary.
        let _ = crate::client::socket_addr(sock_path)?;

        let listener = socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .map_err(io)?;

        // A leftover from a previous owner is expected (§3.9), not exceptional.
        let _ = std::fs::remove_file(&tmp);
        bind(&listener, &addr).map_err(io)?;
        // The trust model is same-user cooperating processes (§0), and the
        // runtime directory is already 0700 — but the socket inherits the
        // umask, so an operator running with `umask 000` would otherwise widen
        // it. Set it explicitly rather than depend on ambient state.
        rustix::fs::chmod(&tmp, rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR).map_err(io)?;
        listen(&listener, BACKLOG).map_err(io)?;
        // Publish atomically: a client sees the old socket, or this one
        // listening, never a half-built one.
        std::fs::rename(&tmp, sock_path).map_err(|e| IpcError::HandshakeIo {
            raw_os_error: e.raw_os_error().unwrap_or(0),
        })?;

        // Identify the file just published, so teardown can tell it apart from a
        // successor's. It must be `stat` on the *path*: `fstat` on the listener
        // returns its `sockfs` inode, which shares no device with any filesystem
        // and so never compares equal to the bound path (measured: `dev=8` for
        // the listener against `dev=2049` for the path on tmpfs).
        #[allow(clippy::unnecessary_cast)]
        let bound = rustix::fs::stat(sock_path)
            .map(|s| (s.st_dev as u64, s.st_ino as u64))
            .map_err(io)?;

        let shutdown = rustix::event::eventfd(
            0,
            rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
        )
        .map_err(io)?;

        Ok(OwnerServer {
            bound,
            listener,
            shutdown,
            sock_path: sock_path.to_path_buf(),
            desc,
            owner_pid,
            fork_gen: {
                // Bound the socket, so from here on a `fork` matters. Arming is
                // idempotent and the server is created once per arena.
                crate::fork::arm();
                crate::fork::generation()
            },
        })
    }

    /// A handle that can stop this server from another thread.
    ///
    /// # Errors
    ///
    /// If the `eventfd` cannot be duplicated.
    pub fn shutdown_handle(&self) -> Result<ShutdownHandle, IpcError> {
        let eventfd = rustix::io::fcntl_dupfd_cloexec(&self.shutdown, 0).map_err(io)?;
        Ok(ShutdownHandle { eventfd })
    }

    /// The path this server is listening on.
    #[must_use]
    pub fn sock_path(&self) -> &Path {
        &self.sock_path
    }

    /// Serve until [`ShutdownHandle::stop`].
    ///
    /// `assign` validates a request against arena policy and returns the slot to
    /// grant, or the [`HelloStatus`] to reject with. `on_hangup` is called with
    /// a granted slot when that client's socket closes — the D17 reap trigger.
    ///
    /// A failed handshake never takes the server down: the client is dropped and
    /// the loop continues. An owner that died because one peer sent a short
    /// datagram would be a denial of service from any process that can reach the
    /// socket.
    ///
    /// # Errors
    ///
    /// Only for failures of the loop itself (`epoll`), not of any one client.
    pub fn serve<A, H>(
        self,
        segment: BorrowedFd<'_>,
        mut assign: A,
        mut on_hangup: H,
    ) -> Result<(), IpcError>
    where
        A: FnMut(&HelloRequest) -> Result<u32, HelloStatus>,
        H: FnMut(u32),
    {
        let ep = epoll::create(epoll::CreateFlags::CLOEXEC).map_err(io)?;
        epoll::add(
            &ep,
            &self.listener,
            epoll::EventData::new_u64(TOKEN_LISTENER),
            epoll::EventFlags::IN,
        )
        .map_err(io)?;
        epoll::add(
            &ep,
            &self.shutdown,
            epoll::EventData::new_u64(TOKEN_SHUTDOWN),
            epoll::EventFlags::IN,
        )
        .map_err(io)?;

        // Token -> (client socket, granted slot). The fd must be kept alive
        // here: dropping it would close the connection and tell the client the
        // *owner* died, which is the opposite of the truth.
        let mut clients: Vec<Option<(OwnedFd, u32)>> = Vec::new();

        // A fixed buffer, reused across iterations: this loop wakes on every
        // attach and every participant death, and an allocation per wakeup
        // would be pure waste.
        //
        // It must NOT be a `Vec`. rustix's `Buffer` impl for `&mut Vec<T>`
        // reports `len()` as the capacity, not the spare capacity, so a
        // `Vec::with_capacity(16)` passes `maxevents = 0` and `epoll_wait`
        // fails with `EINVAL` — it compiles cleanly and only fails at runtime.
        let mut events = [core::mem::MaybeUninit::<epoll::Event>::uninit(); 16];

        loop {
            let (ready, _) = epoll::wait(&ep, &mut events, None).map_err(io)?;

            for ev in ready.iter() {
                match ev.data.u64() {
                    TOKEN_SHUTDOWN => {
                        self.unlink_if_still_ours();
                        return Ok(());
                    }
                    TOKEN_LISTENER => {
                        if let Ok((sock, slot)) =
                            self.accept_one(segment, &mut assign, &mut on_hangup)
                        {
                            // Reuse a departed client's index rather than always
                            // appending. An owner runs for the life of the robot
                            // and §11.2 cycles attach/detach 10^4 times; an
                            // append-only table would grow without bound and
                            // hand out ever-larger tokens for a fleet whose size
                            // never changes.
                            let idx = clients
                                .iter()
                                .position(Option::is_none)
                                .unwrap_or(clients.len());
                            let token = TOKEN_CLIENT_BASE + idx as u64;
                            // Watch for the peer going away. RDHUP catches a
                            // clean shutdown, HUP an abrupt death; both mean the
                            // participant is gone.
                            if epoll::add(
                                &ep,
                                &sock,
                                epoll::EventData::new_u64(token),
                                epoll::EventFlags::RDHUP | epoll::EventFlags::HUP,
                            )
                            .is_ok()
                            {
                                if idx == clients.len() {
                                    clients.push(Some((sock, slot)));
                                } else {
                                    clients[idx] = Some((sock, slot));
                                }
                            } else {
                                // `epoll::add` can fail with ENOSPC once
                                // `fs.epoll.max_user_watches` is reached, or
                                // ENOMEM under pressure. An unwatched connection
                                // produces no hangup event, so the same leak as
                                // the `sendmsg` path above applies: give the slot
                                // back rather than granting it forever.
                                on_hangup(slot);
                            }
                        }
                    }
                    token => {
                        let idx = (token - TOKEN_CLIENT_BASE) as usize;
                        if let Some(entry) = clients.get_mut(idx) {
                            if let Some((sock, slot)) = entry.take() {
                                let _ = epoll::delete(&ep, &sock);
                                drop(sock);
                                on_hangup(slot);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Accept one connection and run the handshake on it.
    ///
    /// Returns the connection and the slot granted, or an error if the client
    /// was rejected or misbehaved — in which case its socket is dropped here.
    ///
    /// `on_hangup` is the caller's slot-release callback, the same one [`Self::serve`]
    /// runs when a watched participant dies. It is needed here because `assign`
    /// reserves the slot *before* the response is sent: a failure after that
    /// point produces a slot nobody holds and nobody will ever hang up on.
    fn accept_one<A, H>(
        &self,
        segment: BorrowedFd<'_>,
        assign: &mut A,
        on_hangup: &mut H,
    ) -> Result<(OwnedFd, u32), IpcError>
    where
        A: FnMut(&HelloRequest) -> Result<u32, HelloStatus>,
        H: FnMut(u32),
    {
        let sock = accept_with(&self.listener, SocketFlags::CLOEXEC).map_err(io)?;

        // **Bound the handshake, or one peer wedges the owner.** `recvmsg`
        // below is blocking and this loop is single-threaded, so a client that
        // connects and then never sends — hung, stopped, or hostile — would
        // otherwise stall every other participant's attach *and* the shutdown
        // path, indefinitely. §3.7 specifies no timeout on either side; the
        // client half sets one for the mirror-image reason.
        //
        // A slow client costs one timeout. An unbounded wait costs the arena.
        for dir in [
            rustix::net::sockopt::Timeout::Recv,
            rustix::net::sockopt::Timeout::Send,
        ] {
            rustix::net::sockopt::set_socket_timeout(&sock, dir, Some(HANDSHAKE_TIMEOUT))
                .map_err(io)?;
        }

        let mut buf = [0u8; HELLO_REQUEST_LEN];
        let recv = recvmsg(
            &sock,
            &mut [std::io::IoSliceMut::new(&mut buf)],
            &mut Default::default(),
            RecvFlags::empty(),
        )
        .map_err(io)?;

        // Length, then magic, then everything else — and a decode failure is a
        // `Malformed` rejection rather than a dropped connection, so a client
        // built against a different protocol learns why instead of seeing its
        // connection vanish.
        let (status, slot) = match HelloRequest::from_bytes(&buf[..recv.bytes]) {
            Err(_) => (HelloStatus::Malformed, u32::MAX),
            Ok(req) => match self.check(&req) {
                Some(bad) => (bad, u32::MAX),
                None => match assign(&req) {
                    Ok(slot) => (HelloStatus::Ok, slot),
                    Err(bad) => (bad, u32::MAX),
                },
            },
        };

        let response = if status == HelloStatus::Ok {
            HelloResponse::accept(&self.desc, slot, self.owner_pid)
        } else {
            HelloResponse::reject(status, &self.desc, self.owner_pid)
        };
        let bytes = response.to_bytes();

        let mut space = [core::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut cmsg = SendAncillaryBuffer::new(&mut space);
        // A rejection carries **no fd** (§3.7): handing a segment to a peer we
        // just refused would make the refusal advisory.
        let granted = [segment];
        if status == HelloStatus::Ok {
            cmsg.push(SendAncillaryMessage::ScmRights(&granted));
        }

        if let Err(e) = sendmsg(
            &sock,
            &[std::io::IoSlice::new(&bytes)],
            &mut cmsg,
            SendFlags::empty(),
        ) {
            // `assign` has already reserved the slot, and this send is where a
            // client that died mid-handshake shows up: `SIGKILL`ed between its
            // own `sendmsg` and `recvmsg`, the owner's send returns `EPIPE`.
            // Nothing will ever hang up on a connection the peer never received,
            // so the slot must go back here or it stays granted for the lifetime
            // of the owner. Sixty-four such deaths — one supervised node in a
            // crash loop — would otherwise wedge an empty arena at
            // `NoParticipantSlots` until the owner itself is restarted.
            if status == HelloStatus::Ok {
                on_hangup(slot);
            }
            return Err(io(e));
        }

        if status == HelloStatus::Ok {
            Ok((sock, slot))
        } else {
            Err(IpcError::HandshakeRejected {
                status,
                owner_format_version: self.desc.format_version,
                owner_layout_hash: self.desc.layout_hash,
            })
        }
    }

    /// The checks that do not need the arena: version, then layout, then boot id.
    ///
    /// Order matters. A version mismatch makes every later field's meaning
    /// uncertain, so it is reported first rather than surfacing as a confusing
    /// layout complaint about a struct the peer lays out differently anyway.
    fn check(&self, req: &HelloRequest) -> Option<HelloStatus> {
        if req.format_version != self.desc.format_version {
            return Some(HelloStatus::VersionMismatch);
        }
        if req.layout_hash != self.desc.layout_hash {
            return Some(HelloStatus::LayoutMismatch);
        }
        if req.client_boot_id != self.desc.boot_id {
            return Some(HelloStatus::BootIdMismatch);
        }
        None
    }
}

impl Drop for OwnerServer {
    fn drop(&mut self) {
        // Never from a `fork` child. The listener fd is inherited, so it still
        // `stat`s equal to the path — `unlink_if_still_ours` would conclude the
        // socket is ours and remove the **parent's** live listening path, after
        // which no client can find an owner that is still perfectly happy to
        // serve one. The child's own fd closing is harmless: the description
        // stays open in the parent.
        //
        // **Coverage, stated plainly: no test fails when this check is
        // removed.** In this workspace an `OwnerServer` only ever lives on the
        // serving thread's stack, and `fork` does not copy threads, so the
        // child has no such value to drop. It is reachable only through the
        // public API — bind on the main thread, then fork — which is exactly
        // the case a library owes a guard for, and nothing else provides one.
        if self.fork_gen != crate::fork::generation() {
            return;
        }
        self.unlink_if_still_ours();
    }
}

impl OwnerServer {
    /// Remove the socket path **only if it is still this server's socket**.
    ///
    /// A plain `remove_file` here is a real hazard rather than a tidy-up. §3.5
    /// lets a successor take over, and a successor publishes by `rename`ing its
    /// own socket over this path — so by the time this server winds down, the
    /// path may name *somebody else's* live listener, and unlinking it would
    /// silently make the new owner unreachable while it happily keeps serving a
    /// socket no client can find.
    ///
    /// Comparing the identity this server *published* (`bound`, captured from
    /// the path at bind time) against what the path names today closes it:
    /// after a successor's `rename` the inodes differ, so this leaves the path
    /// alone. Not perfectly atomic — the successor could rename between the
    /// `stat` and the `unlink` — but that window is a single syscall wide,
    /// against a window that is otherwise the entire lifetime of the process,
    /// and §3.9 already makes a stale socket path a state every client
    /// tolerates.
    ///
    /// **This compares against `bound`, not against `fstat(listener)`.** It used
    /// to do the latter, which made the whole function a no-op: a listening
    /// socket's fd resolves to an inode in `sockfs`, which shares no device with
    /// the filesystem holding the path, so the equality could never hold on any
    /// kernel. The socket was therefore *never* unlinked, not even on a clean
    /// stop, and the successor protection this comment argues for had never run.
    /// Nothing failed either way, which is how it survived — hence the two tests
    /// below, which fail for an unconditional `remove_file` and for the old
    /// `fstat` form respectively.
    fn unlink_if_still_ours(&self) {
        let Ok(theirs) = rustix::fs::stat(&self.sock_path) else {
            return;
        };
        #[allow(clippy::unnecessary_cast)]
        let theirs = (theirs.st_dev as u64, theirs.st_ino as u64);
        if theirs == self.bound {
            let _ = std::fs::remove_file(&self.sock_path);
        }
    }
}

/// Every rustix error in this module becomes the same shape.
fn io(e: rustix::io::Errno) -> IpcError {
    IpcError::HandshakeIo {
        raw_os_error: e.raw_os_error(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn desc() -> SegmentDescriptor {
        SegmentDescriptor {
            format_version: 3,
            layout_hash: 0xDEAD_BEEF,
            arena_size: 4096,
            instance_uuid: [0x5A; 16],
            boot_id: [0xCD; 16],
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tf_tree_ipc_srv-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// **An outgoing owner must not unlink its successor's socket.**
    ///
    /// §3.5 lets a survivor inherit the owner role, and it publishes by
    /// `rename`ing its own socket over the shared path. By the time the previous
    /// owner winds down, that path names *somebody else's live listener* — and a
    /// plain `remove_file` there makes the new owner unreachable while it keeps
    /// serving a socket no client can find, with nothing reporting an error
    /// anywhere.
    ///
    /// Mutants this kills: replacing `unlink_if_still_ours` with an
    /// unconditional `remove_file` fails the second assertion; comparing
    /// `fstat(listener)` against the path — the form this code shipped with —
    /// fails the third, because a listening socket's inode lives in `sockfs` and
    /// never matches the filesystem the path is on.
    #[test]
    fn winding_down_leaves_a_successors_socket_alone() {
        let dir = scratch("succession");
        let sock = dir.join("a.sock");

        let first = OwnerServer::bind_at(&sock, desc(), 111).unwrap();
        let first_ino = rustix::fs::stat(&sock).unwrap().st_ino;

        // The heir takes over: same path, its own socket.
        let second = OwnerServer::bind_at(&sock, desc(), 222).unwrap();
        let heir_ino = rustix::fs::stat(&sock).unwrap().st_ino;
        assert_ne!(first_ino, heir_ino, "the heir did not republish the path");

        drop(first);
        assert_eq!(
            rustix::fs::stat(&sock).map(|s| s.st_ino).ok(),
            Some(heir_ino),
            "the outgoing owner unlinked its successor's socket"
        );

        // And the last owner *does* clean up after itself, so §3.9's stale path
        // is a crash artefact rather than the normal outcome of a clean stop.
        drop(second);
        assert!(
            rustix::fs::stat(&sock).is_err(),
            "a cleanly-stopping owner must not leave its socket behind"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The pid-suffixed temporary `bind_at` renames from is an implementation
    /// detail that must not survive the bind, or a runtime directory accumulates
    /// one dead socket per owner that ever ran.
    #[test]
    fn binding_leaves_only_the_published_path() {
        let dir = scratch("tmp-path");
        let sock = dir.join("b.sock");
        let server = OwnerServer::bind_at(&sock, desc(), 4242).unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["b.sock"], "leftover files in {dir:?}");
        assert_eq!(server.sock_path(), sock);
        drop(server);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
