//! The owner half of the §3.7 attach handshake.
//!
//! # Who serves
//!
//! A thread in the owning process, not a daemon (`docs/decisions/0005` §3).
//!
//! # Hangup
//!
//! D17: every accepted fd stays in the `epoll` set; its hangup is the reap
//! trigger, reported with the granted slot
//! ([`0057`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md)).
//!
//! # Policy is the caller's
//!
//! Slot assignment and reaping live in `tf_tree`; this module calls out to
//! `assign`/`on_hangup` and knows nothing about arenas (§2).

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

/// Per-client handshake timeout.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// `epoll` token for the listening socket.
const TOKEN_LISTENER: u64 = 0;
/// `epoll` token for the shutdown `eventfd`.
const TOKEN_SHUTDOWN: u64 = 1;
/// Client tokens start here.
const TOKEN_CLIENT_BASE: u64 = 2;

/// A bound, listening §3.7 server.
pub struct OwnerServer {
    listener: OwnedFd,
    shutdown: OwnedFd,
    sock_path: PathBuf,
    desc: SegmentDescriptor,
    owner_pid: u32,
    /// The fork generation this server bound in.
    fork_gen: u64,
    /// `(st_dev, st_ino)` of the published socket file.
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
    /// Binds a pid-suffixed temporary, restricts it to the owner, then atomically
    /// `rename`s it into place (§3.4 step 5).
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
        // Validate the final path before binding.
        let _ = crate::client::socket_addr(sock_path)?;

        let listener = socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .map_err(io)?;

        // A leftover from a previous owner is expected (§3.9).
        let _ = std::fs::remove_file(&tmp);
        bind(&listener, &addr).map_err(io)?;
        rustix::fs::chmod(&tmp, rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR).map_err(io)?;
        listen(&listener, BACKLOG).map_err(io)?;

        // Stat the temporary before the rename (the path afterwards could name a
        // successor); `fstat(listener)` is a `sockfs` inode that never matches.
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
    /// `assign` returns the slot to grant or the [`HelloStatus`] to reject with;
    /// `on_hangup` is called with a granted slot when that client's socket closes
    /// (D17). A failed handshake drops the client, not the server.
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

        // Token -> (client socket, granted slot). Dropping the fd tells the
        // client the owner died.
        let mut clients: Vec<Option<(OwnedFd, u32)>> = Vec::new();

        // Fixed buffer: rustix reports a `&mut Vec`'s `len()` as capacity, so
        // `epoll_wait` would fail `EINVAL`.
        let mut events = [core::mem::MaybeUninit::<epoll::Event>::uninit(); 16];

        loop {
            // `EINTR` (SIGSTOP/SIGCONT, debugger attach) must retry: returning
            // would unlink the socket while the process still holds ownership.
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
                            // Reuse a departed client's index.
                            let idx = clients
                                .iter()
                                .position(Option::is_none)
                                .unwrap_or(clients.len());
                            let token = TOKEN_CLIENT_BASE + idx as u64;
                            // RDHUP: clean shutdown; HUP: abrupt death.
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
                            // No `on_hangup` on `epoll::add` failure: the client
                            // holds the slot; releasing it could let two
                            // participants share one. A slot leaks instead.
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

    /// Accept one connection and run the handshake; returns the connection and
    /// granted slot. `on_hangup` is [`Self::serve`]'s callback.
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

        // The loop is single-threaded and `recvmsg` blocks: without a timeout a
        // silent client would stall every attach and shutdown.
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

        // A decode failure is a `Malformed` rejection.
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
        // A rejection carries no fd (§3.7).
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
            // A client killed mid-handshake surfaces as `EPIPE` and nothing will
            // hang up on it: return the slot now.
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

    /// Arena-free checks: version first (a mismatch makes later fields
    /// uncertain), then layout, then boot id.
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
        // Never from a `fork` child: it would unlink the parent's live socket.
        // No test covers this check.
        if self.fork_gen != crate::fork::generation() {
            return;
        }
        self.unlink_if_still_ours();
    }
}

impl OwnerServer {
    /// Remove the socket path only if it still names this server's socket: a
    /// successor may have `rename`d over it (§3.5). Pinned by
    /// `winding_down_leaves_a_successors_socket_alone`.
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

    /// An outgoing owner must not unlink its successor's socket.
    #[test]
    fn winding_down_leaves_a_successors_socket_alone() {
        let dir = scratch("succession");
        let sock = dir.join("a.sock");

        let first = OwnerServer::bind_at(&sock, desc(), 111).unwrap();
        let first_ino = rustix::fs::stat(&sock).unwrap().st_ino;

        let second = OwnerServer::bind_at(&sock, desc(), 222).unwrap();
        let heir_ino = rustix::fs::stat(&sock).unwrap().st_ino;
        assert_ne!(first_ino, heir_ino, "the heir did not republish the path");

        drop(first);
        assert_eq!(
            rustix::fs::stat(&sock).map(|s| s.st_ino).ok(),
            Some(heir_ino),
            "the outgoing owner unlinked its successor's socket"
        );

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
