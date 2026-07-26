//! The client half of the §3.7 attach handshake.
//!
//! Connect, send a [`HelloRequest`], receive a [`HelloResponse`] with the
//! segment fd riding as `SCM_RIGHTS`, and **keep the socket open**.
//!
//! # The socket is not a handshake channel
//!
//! `docs/PHASE2.md` §3.7 step 9 and `docs/PROJECT.md` §5 D17 both say it: a
//! participant holds its connection for the lifetime of the attachment, because
//! that is how it learns the *owner* died — process death of any kind closes
//! the fd and the peer sees it immediately, with no timeout to tune and no
//! heartbeat to misinterpret. Closing after the handshake would throw away the
//! liveness signal the whole design rests on, so [`Attached`] owns the socket
//! and the caller must keep it alive.
//!
//! # No `unsafe`
//!
//! `SCM_RIGHTS` is usually a `cmsg` macro walk. rustix 1.1's ancillary API is
//! safe end to end — `RecvAncillaryBuffer::drain` yields `OwnedFd`s directly —
//! so nothing here needs `unsafe`, which is also why the seam could live in a
//! `forbid(unsafe_code)` crate (`docs/decisions/0005`).

use std::path::Path;
use std::time::Duration;

use rustix::fd::OwnedFd;
use rustix::net::{
    connect, recvmsg, sendmsg, socket_with, AddressFamily, RecvAncillaryBuffer,
    RecvAncillaryMessage, RecvFlags, SendFlags, SocketAddrUnix, SocketFlags, SocketType,
};

use crate::error::IpcError;
use crate::wire::{HelloRequest, HelloResponse, HelloStatus, HELLO_RESPONSE_LEN, MAX_SOCKET_PATH};

/// A completed attachment: the answer, the segment, and the live socket.
#[derive(Debug)]
pub struct Attached {
    /// What the owner said, including the slot this client must take.
    pub response: HelloResponse,
    /// The segment fd. Not yet validated — §3.7 step 4 (`fstat` against
    /// `arena_size`, seals present) is the mapper's job, not the wire's.
    pub segment: OwnedFd,
    /// The connection. **Hold it for the lifetime of the attachment**; dropping
    /// it tells the owner this participant is gone.
    pub socket: OwnedFd,
}

/// Perform the §3.7 handshake against `sock_path`.
///
/// # Errors
///
/// - [`IpcError::ServerUnreachable`] if nothing is listening, which the caller
///   should read as "no server" rather than as a failure — a stale socket path
///   is an expected state (§3.9), and the ownership byte is the real
///   discriminator.
/// - [`IpcError::HandshakeIo`] on a send/receive failure or timeout.
/// - [`IpcError::HandshakeMalformed`] if the reply is not a `HelloResponse`.
/// - [`IpcError::HandshakeRejected`] if the owner said no.
/// - [`IpcError::NoFdReceived`] if the owner accepted but sent no fd.
pub fn attach(
    sock_path: &Path,
    request: &HelloRequest,
    timeout: Duration,
) -> Result<Attached, IpcError> {
    let addr = socket_addr(sock_path)?;

    let sock = socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| IpcError::HandshakeIo {
        raw_os_error: e.raw_os_error(),
    })?;

    // Both directions, because §3.7 specifies no timeout at all and a server
    // SIGSTOPped between `accept` and `sendmsg` would otherwise block this
    // client in `recvmsg` past the §3.4 deadline — turning "the owner is
    // wedged" into "this process is wedged too".
    for dir in [
        rustix::net::sockopt::Timeout::Recv,
        rustix::net::sockopt::Timeout::Send,
    ] {
        rustix::net::sockopt::set_socket_timeout(&sock, dir, Some(timeout)).map_err(|e| {
            IpcError::HandshakeIo {
                raw_os_error: e.raw_os_error(),
            }
        })?;
    }

    connect(&sock, &addr).map_err(|e| IpcError::ServerUnreachable {
        raw_os_error: e.raw_os_error(),
    })?;

    let bytes = request.to_bytes();
    sendmsg(
        &sock,
        &[std::io::IoSlice::new(&bytes)],
        &mut Default::default(),
        SendFlags::empty(),
    )
    .map_err(|e| IpcError::HandshakeIo {
        raw_os_error: e.raw_os_error(),
    })?;

    let mut buf = [0u8; HELLO_RESPONSE_LEN];
    let mut space = [core::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut cmsg = RecvAncillaryBuffer::new(&mut space);
    // CMSG_CLOEXEC so a received fd is never leaked into a concurrent `exec` in
    // another thread — the window is small and entirely avoidable.
    let recv = recvmsg(
        &sock,
        &mut [std::io::IoSliceMut::new(&mut buf)],
        &mut cmsg,
        RecvFlags::CMSG_CLOEXEC,
    )
    .map_err(|e| IpcError::HandshakeIo {
        raw_os_error: e.raw_os_error(),
    })?;

    // Take the fd out before inspecting the status, so a rejection that
    // wrongly carried one does not leak it.
    let mut segment = None;
    for msg in cmsg.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = msg {
            for fd in fds {
                if segment.is_none() {
                    segment = Some(fd);
                }
            }
        }
    }

    let response =
        HelloResponse::from_bytes(&buf[..recv.bytes]).map_err(IpcError::HandshakeMalformed)?;

    if response.status != HelloStatus::Ok {
        // §3.7: a rejection carries no fd. An owner that sends one anyway is
        // buggy or hostile, and the distinction does not matter here — what
        // matters is that a client which ignored `status` would go on to map a
        // segment it was refused. Name it rather than dropping the fd quietly,
        // because a silently-tolerated protocol violation is one nobody fixes.
        if segment.is_some() {
            return Err(IpcError::RejectionCarriedFd {
                status: response.status,
            });
        }
        return Err(IpcError::HandshakeRejected {
            status: response.status,
            owner_format_version: response.format_version,
            owner_layout_hash: response.layout_hash,
        });
    }

    Ok(Attached {
        response,
        segment: segment.ok_or(IpcError::NoFdReceived)?,
        socket: sock,
    })
}

/// Build a `SocketAddrUnix`, rejecting an over-long path with a typed error.
///
/// `sun_path` is 108 bytes and `$TF_TREE_RUNTIME_DIR` is arbitrary, so this is
/// reachable from configuration rather than from a bug. §3.1 and §3.7 mention
/// neither the limit nor what to do about it; failing here with the length
/// beats failing inside `bind` with a bare `EINVAL`.
pub(crate) fn socket_addr(path: &Path) -> Result<SocketAddrUnix, IpcError> {
    let len = path.as_os_str().len();
    if len >= MAX_SOCKET_PATH {
        return Err(IpcError::SocketPathTooLong {
            len,
            limit: MAX_SOCKET_PATH,
        });
    }
    SocketAddrUnix::new(path).map_err(|_| IpcError::SocketPathTooLong {
        len,
        limit: MAX_SOCKET_PATH,
    })
}
