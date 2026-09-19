//! The owner half of the §3.7 attach handshake.
//!
//! # Who serves
//!
//! A thread in the **owning process**, not a daemon (`docs/decisions/0005` §3):
//! §3.5 makes ownership a role a survivor inherits, so any participant must be
//! able to bind.
//!
//! # Why this loop exists after the handshake is done
//!
//! D17: participants hold their socket open for the attachment, so the owner
//! sees `EPOLLHUP` when the kernel closes a dead participant's files — at the end
//! of its exit, not in microseconds
//! ([`0057`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md)),
//! so a core dump delays the reap. The hangup is the reap trigger: every
//! accepted fd stays in the `epoll` set and a hangup is reported with the slot
//! granted.
//!
//! # Policy is the caller's
//!
//! Slot assignment and reaping live in `tf_tree`, which has the arena. This
//! module does the protocol and calls out to `assign`/`on_hangup`, so
//! `tf_tree_ipc` still knows nothing about arenas (§2).

use std::path::{Path, PathBuf};

use rustix::event::epoll;
use rustix::fd::{BorrowedFd, OwnedFd};
use rustix::io::Errno;
use rustix::net::{
    accept_with, bind, listen, recvmsg, sendmsg, socket_with, AddressFamily, RecvFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType,
};

use crate::error::IpcError;
use crate::wire::{HelloRequest, HelloResponse, HelloStatus, SegmentDescriptor, HELLO_REQUEST_LEN};

/// Connection backlog: a thundering herd at boot is expected (§11.2 scenario 7).
const BACKLOG: i32 = 64;

/// How long the owner waits on one client's half of the handshake. Per-client,
/// so a stalled peer cannot consume the deadline of those queued behind it.
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
    /// `(st_dev, st_ino)` of the socket file this server published — see
    /// `unlink_if_still_ours`.
    bound: (u64, u64),
}

/// Ask a running [`OwnerServer`] to stop; `Send`, so another thread can stop it.
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
    /// # The bind sequence
    ///
    /// §3.4 step 5's "bind sock.tmp" names no per-process suffix, and a stale
    /// socket path outlives its process. So: unlink any stale path, bind a
    /// **pid-suffixed** temporary, restrict it to the owner, and `rename` it into
    /// place; `rename` is atomic, so a client never sees a bound-but-not-listening
    /// socket.
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
        // Validate the final path too, so an over-long name fails before binding.
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
        // The socket inherits the umask; set the mode explicitly.
        rustix::fs::chmod(&tmp, rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR).map_err(io)?;
        listen(&listener, BACKLOG).map_err(io)?;

        // Identify the file about to be published, so teardown can tell it from a
        // successor's. `stat` the temporary before the rename: stat of the path
        // afterwards could capture a concurrent successor's identity, and `fstat`
        // on the listener returns a `sockfs` inode that never equals the path's.
        #[allow(clippy::unnecessary_cast)]
        let bound = rustix::fs::stat(&tmp)
            .map(|s| (s.st_dev as u64, s.st_ino as u64))
            .map_err(io)?;

        std::fs::rename(&tmp, sock_path).map_err(|e| IpcError::HandshakeIo {
            raw_os_error: e.raw_os_error().unwrap_or(0),
        })?;

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
                // From here on a `fork` matters; arming is idempotent.
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
    /// grant, or the [`HelloStatus`] to reject with. `on_hangup` is called with a
    /// granted slot when that client's socket closes — the D17 reap trigger. A
    /// failed handshake drops the client and never the server.
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

        // Token -> (client socket, granted slot). Dropping the fd would tell the
        // client the *owner* died.
        let mut clients: Vec<Option<(OwnedFd, u32)>> = Vec::new();

        // Fixed buffer, not a `Vec`: rustix's `Buffer` for `&mut Vec<T>` reports
        // `len()` as capacity, so `maxevents = 0` and `epoll_wait` fails `EINVAL`
        // at runtime.
        let mut events = [core::mem::MaybeUninit::<epoll::Event>::uninit(); 16];

        loop {
            // **`EINTR` is not a failure here.** `epoll_wait` fails with `EINTR`
            // after a stop signal followed by `SIGCONT` even with no handler
            // installed (Ctrl-Z then `fg`; a debugger attaching every tid).
            // Propagating it makes `serve` return, `Drop` unlink the socket, and
            // the process live on holding byte 0 and the ownership byte with
            // nothing serving: §3.4 then has no exit for anybody. Retry on
            // `Errno::INTR` only, so a real `epoll` failure stays loud.
            let (ready, _) = match epoll::wait(&ep, &mut events, None) {
                Ok(ready) => ready,
                Err(e) if e == Errno::INTR => continue,
                Err(e) => return Err(io(e)),
            };

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
                            // Reuse a departed client's index so the table and
                            // tokens do not grow over 10^4 attach/detach cycles.
                            let idx = clients
                                .iter()
                                .position(Option::is_none)
                                .unwrap_or(clients.len());
                            let token = TOKEN_CLIENT_BASE + idx as u64;
                            // RDHUP catches a clean shutdown, HUP an abrupt death.
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
                            }
                            // No `on_hangup` on an `epoll::add` failure, unlike
                            // `accept_one`'s `sendmsg` arm: the handshake already
                            // succeeded, so the client holds the slot, and
                            // releasing it would let `register_at` overwrite a
                            // live participant's identity record. This leaks a
                            // slot (bounded at 64, only under ENOSPC/ENOMEM)
                            // rather than have two participants share one.
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

    /// Accept one connection and run the handshake on it; returns the connection
    /// and slot granted, or an error (the socket is dropped here).
    /// `on_hangup` is [`Self::serve`]'s slot-release callback.
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

        // **Bound the handshake, or one peer wedges the owner.** `recvmsg` is
        // blocking and this loop single-threaded, so a client that connects and
        // never sends would stall every attach and the shutdown path. §3.7 sets
        // no timeout; the client half sets one for the mirror-image reason.
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

        // Length, then magic, then the rest; a decode failure is a `Malformed`
        // rejection so a mismatched client learns why.
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
        // A rejection carries **no fd** (§3.7), or refusal would be advisory.
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
            // `assign` has reserved the slot and a client killed mid-handshake
            // surfaces here as `EPIPE`; nothing will ever hang up on a connection
            // the peer never received, so return the slot now or 64 crash-loop
            // deaths wedge an empty arena at `NoParticipantSlots`.
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
    /// Version first: a mismatch makes every later field uncertain.
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
        // Never from a `fork` child: `bound` is copied verbatim, so the child's
        // `unlink_if_still_ours` would remove the **parent's** live socket path.
        // **No test fails when this check is removed**: an `OwnerServer` lives on
        // the serving thread, which `fork` does not copy, so only the public API
        // (bind on the main thread, then fork) reaches it.
        if self.fork_gen != crate::fork::generation() {
            return;
        }
        self.unlink_if_still_ours();
    }
}

impl OwnerServer {
    /// Remove the socket path **only if it is still this server's socket**.
    ///
    /// A successor publishes by `rename`ing over this path (§3.5), so a plain
    /// `remove_file` would make a live new owner unreachable. Compares `bound`
    /// (captured at bind) with what the path names now; the residual
    /// `stat`-to-`unlink` window is one syscall, and §3.9 makes a stale path
    /// tolerable. Compares `bound`, not `fstat(listener)` (a `sockfs` inode that
    /// never matches); `winding_down_leaves_a_successors_socket_alone` covers both
    /// mutants.
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

/// Every rustix error in this module becomes `HandshakeIo`.
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

    /// **An outgoing owner must not unlink its successor's socket.** Kills an
    /// unconditional `remove_file` (second assertion) and an `fstat(listener)`
    /// comparison (third).
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

        // The last owner cleans up, so §3.9's stale path is a crash artefact.
        drop(second);
        assert!(
            rustix::fs::stat(&sock).is_err(),
            "a cleanly-stopping owner must not leave its socket behind"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The pid-suffixed temporary must not survive the bind.
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
