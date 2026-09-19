//! The client half of the §3.7 attach handshake.
//!
//! Connect, send a [`HelloRequest`], receive a [`HelloResponse`] with the
//! segment fd riding as `SCM_RIGHTS`, and **keep the socket open**.
//!
//! **The socket is not a handshake channel.** A participant holds its connection
//! for the lifetime of the attachment: that is how it learns the owner died
//! (`docs/PHASE2.md` §3.7 step 9, §3.5 `owner_lost()`; `docs/PROJECT.md` D17).
//! [`Attached`] owns the socket and the caller must keep it alive.
//!
//! No `unsafe`: rustix's ancillary API is safe end to end
//! (`docs/decisions/0005`).

use std::path::Path;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags};
use rustix::fd::{BorrowedFd, OwnedFd};
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
    /// The segment fd. Not validated; §3.7 step 4 is the mapper's job.
    pub segment: OwnedFd,
    /// The connection. **Hold it for the lifetime of the attachment**; dropping
    /// it tells the owner this participant is gone.
    pub socket: OwnedFd,
}

/// Whether the peer of `socket` has closed it — the owner's death signal (D17).
///
/// The participant-side view of the D17 hangup: the owner is gone, which is
/// §3.5's trigger. The kernel closes a dying process's files only at the end of
/// its exit, after any core dump
/// ([`0057`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md)).
///
/// **Non-blocking, and it must stay that way**: a zero-timeout predicate keeps
/// §3.5 free of a background thread (`docs/decisions/0019`). A hangup is
/// one-way, so `true` may be cached.
///
/// # Errors
///
/// [`IpcError::HandshakeIo`] if `poll` fails. `EINTR` reads as *no hangup*.
pub fn peer_hung_up(socket: BorrowedFd<'_>) -> Result<bool, IpcError> {
    let mut fds = [PollFd::new(&socket, PollFlags::empty())];
    match rustix::event::poll(
        &mut fds,
        Some(&rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        }),
    ) {
        Ok(0) => Ok(false),
        Ok(_) => Ok(fds[0]
            .revents()
            .intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL)),
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(e) => Err(IpcError::HandshakeIo {
            raw_os_error: e.raw_os_error(),
        }),
    }
}

/// Perform the §3.7 handshake against `sock_path`.
///
/// # Errors
///
/// - [`IpcError::ServerUnreachable`] if nothing is listening: read it as "no
///   server", not a failure (§3.9; the ownership byte is the discriminator).
/// - [`IpcError::HandshakeIo`] on a send/receive failure or timeout.
/// - [`IpcError::HandshakeClosed`] if the owner closed without replying.
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
    .map_err(|e| IpcError::ClientSocketSetup {
        raw_os_error: e.raw_os_error(),
    })?;

    // Both directions: §3.7 has no timeout, and a SIGSTOPped owner would wedge
    // this client past the §3.4 deadline.
    for dir in [
        rustix::net::sockopt::Timeout::Recv,
        rustix::net::sockopt::Timeout::Send,
    ] {
        rustix::net::sockopt::set_socket_timeout(&sock, dir, Some(timeout)).map_err(|e| {
            IpcError::ClientSocketSetup {
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
    let recv = recvmsg(
        &sock,
        &mut [std::io::IoSliceMut::new(&mut buf)],
        &mut cmsg,
        RecvFlags::CMSG_CLOEXEC,
    )
    .map_err(|e| IpcError::HandshakeIo {
        raw_os_error: e.raw_os_error(),
    })?;

    // Drain the fd before inspecting status so a rejection carrying one cannot leak it.
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

    // Zero bytes is the owner dying mid-handshake, not a malformed reply
    // (`BadLength` is terminal in `verdict`; this must be retryable). `Absent`
    // is safe: §3.4 step 2 still sees a live owner's byte 0, and step 4 refuses
    // to create while any participant byte is held, except under
    // `CreatePolicy::Always`, which skips that check by design.
    if recv.bytes == 0 {
        return Err(IpcError::HandshakeClosed);
    }

    let response =
        HelloResponse::from_bytes(&buf[..recv.bytes]).map_err(IpcError::HandshakeMalformed)?;

    if response.status != HelloStatus::Ok {
        // §3.7: a rejection carries no fd; name the violation rather than drop it.
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
/// `sun_path` is 108 bytes and `$TF_TREE_RUNTIME_DIR` is arbitrary.
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

/// The real [`crate::ServerProbe`]: connect and complete the §3.7 handshake.
///
/// Attaching *is* the probe (a separate probe then attach would re-run the §3.4
/// race), so a successful probe comes back holding the segment.
pub struct SocketProbe {
    request: HelloRequest,
    timeout: Duration,
}

impl SocketProbe {
    /// A probe that will introduce itself as `request`.
    #[must_use]
    pub fn new(request: HelloRequest, timeout: Duration) -> SocketProbe {
        SocketProbe { request, timeout }
    }
}

impl crate::open::ServerProbe for SocketProbe {
    type Attached = Attached;

    fn probe(&mut self, sock: &Path) -> Result<crate::open::Reach<Attached>, IpcError> {
        match attach(sock, &self.request, self.timeout) {
            Ok(a) => {
                let slot = a.response.participant_slot;
                Ok(crate::open::Reach::Serving { attached: a, slot })
            }
            Err(e) => match verdict(&e) {
                Verdict::Absent => Ok(crate::open::Reach::Absent),
                Verdict::Rejected => Ok(crate::open::Reach::Rejected(e)),
                Verdict::Fatal => Err(e),
            },
        }
    }
}

/// What a failed [`attach`] means to the §3.4 loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// Treat as "no server": carry on and let the ownership byte decide.
    Absent,
    /// The owner answered and refused. Stop; retrying cannot change the answer.
    Rejected,
    /// Neither. Propagate.
    Fatal,
}

/// Classify an attach failure.
///
/// Pure, so it is testable without syscalls. **A wrong arm is not a small bug**:
/// anything mapped to [`Verdict::Absent`] makes `open()` create an arena, so a
/// local failure misfiled there produces a second arena beside a live one.
fn verdict(e: &IpcError) -> Verdict {
    match e {
        // Nobody listening, or an owner dying mid-handshake (`ECONNRESET` on an
        // unaccepted connection, a 0-byte `recvmsg` on an accepted one; §3.9).
        // Safe for the same reason as the zero-byte check in `attach`.
        IpcError::ServerUnreachable { .. }
        | IpcError::HandshakeIo { .. }
        | IpcError::HandshakeClosed => Verdict::Absent,
        // The owner answered; waiting cannot change it and would replace a precise
        // message with a timeout.
        IpcError::HandshakeRejected { .. }
        | IpcError::HandshakeMalformed(_)
        | IpcError::RejectionCarriedFd { .. }
        | IpcError::NoFdReceived => Verdict::Rejected,
        // Everything else, notably `ClientSocketSetup` (this process out of
        // descriptors): calling that "no server" would create a second arena.
        _ => Verdict::Fatal,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// `ClientSocketSetup` is a local failure, never `Absent` (second arena).
    #[test]
    fn a_local_socket_failure_is_never_read_as_an_absent_arena() {
        assert_eq!(
            verdict(&IpcError::ClientSocketSetup { raw_os_error: 24 }),
            Verdict::Fatal
        );
    }

    /// The three arms that mean "no server": `connect` found nothing, and the
    /// two ways a dying owner reaches a client.
    #[test]
    fn the_no_server_arms_are_exactly_the_three_that_mean_no_server() {
        assert_eq!(
            verdict(&IpcError::ServerUnreachable { raw_os_error: 2 }),
            Verdict::Absent
        );
        assert_eq!(
            verdict(&IpcError::HandshakeIo { raw_os_error: 110 }),
            Verdict::Absent
        );
        assert_eq!(verdict(&IpcError::HandshakeClosed), Verdict::Absent);
    }

    /// Every arm that means "the owner answered" is terminal.
    #[test]
    fn an_owner_that_answered_is_terminal() {
        for e in [
            IpcError::HandshakeRejected {
                status: crate::wire::HelloStatus::LayoutMismatch,
                owner_format_version: 2,
                owner_layout_hash: 1,
            },
            IpcError::NoFdReceived,
            IpcError::RejectionCarriedFd {
                status: crate::wire::HelloStatus::VersionMismatch,
            },
            IpcError::HandshakeMalformed(crate::wire::WireError::BadMagic),
        ] {
            assert_eq!(verdict(&e), Verdict::Rejected, "{e:?}");
        }
    }

    /// An owner that accepts, reads the request and closes without replying is
    /// `Absent`, not malformed. Staged on real sockets, with the `accept(2)`
    /// (without it the client sees `ECONNRESET`, an already-correct arm).
    /// Mutant: deleting the `recv.bytes == 0` guard in [`attach`] fails the first
    /// assertion with `HandshakeMalformed(BadLength { got: 0, .. })`.
    #[test]
    fn an_owner_that_closes_after_accepting_is_absent_not_malformed() {
        use crate::wire::HELLO_REQUEST_LEN;
        use rustix::net::{accept_with, bind, listen};

        let dir =
            std::env::temp_dir().join(format!("tf_tree_ipc_cli-{}-zero-byte", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("a.sock");

        // Listening before the thread exists, so `connect` cannot hit `ServerUnreachable`.
        let listener = socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        bind(&listener, &socket_addr(&sock_path).unwrap()).unwrap();
        listen(&listener, 4).unwrap();

        let dying_owner = std::thread::spawn(move || {
            let accepted = accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
            let mut buf = [0u8; HELLO_REQUEST_LEN];
            let _ = recvmsg(
                &accepted,
                &mut [std::io::IoSliceMut::new(&mut buf)],
                &mut Default::default(),
                RecvFlags::empty(),
            );
        });

        let request = HelloRequest {
            format_version: 3,
            layout_hash: 0xDEAD_BEEF,
            mode: crate::identity::AccessMode::ReadOnly,
            client_pid: std::process::id(),
            client_start_time: 0,
            client_boot_id: [0; 16],
            client_name: [0; 32],
        };
        let err = attach(&sock_path, &request, Duration::from_secs(5))
            .expect_err("the staged owner never replies, so the attach cannot succeed");

        assert_eq!(
            err,
            IpcError::HandshakeClosed,
            "a 0-byte reply was read as something other than the owner going away"
        );
        assert_eq!(
            verdict(&err),
            Verdict::Absent,
            "the §3.4 loop would not retry an owner that died mid-handshake"
        );

        dying_owner.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
