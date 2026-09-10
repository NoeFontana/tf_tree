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
    /// The segment fd. Not yet validated — §3.7 step 4 (`fstat` against
    /// `arena_size`, seals present) is the mapper's job, not the wire's.
    pub segment: OwnedFd,
    /// The connection. **Hold it for the lifetime of the attachment**; dropping
    /// it tells the owner this participant is gone.
    pub socket: OwnedFd,
}

/// Whether the peer of `socket` has closed it — the owner's death signal (D17).
///
/// A participant holds its attach socket for the lifetime of the attachment, so
/// the owner sees `EPOLLHUP` in microseconds when the participant dies. This is
/// the same fact read from the other end: the *participant* learns that the
/// owner is gone, which is §3.5's trigger and the thing that has never existed —
/// `docs/PHASE2.md` §0.0 records that nothing watches the client socket, so no
/// participant ever reached the takeover path even while one was implemented.
///
/// **Non-blocking, and it must stay that way.** A zero timeout makes this a
/// predicate a caller can evaluate inside its own loop rather than a wait it has
/// to schedule around, which is what keeps §3.5 free of a background thread and
/// of `docs/decisions/0019`'s "every process a user is *required* to run is a
/// place adoption dies". `POLLHUP` and `POLLERR` are both reported regardless of
/// the requested events, so nothing is requested.
///
/// A hangup is a one-way door: once the writing end is gone it stays gone, so a
/// `true` here never goes back to `false` and a caller may cache it.
///
/// # Errors
///
/// [`IpcError::HandshakeIo`] if `poll` itself fails. `EINTR` is reported as *no
/// hangup* rather than as an error: a signal arriving during a zero-timeout poll
/// says nothing about the peer, and the caller's next call answers the question
/// (`crates/tf_tree_ipc/src/server.rs` carries the same reading for the serve
/// loop, #260).
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
/// - [`IpcError::ServerUnreachable`] if nothing is listening, which the caller
///   should read as "no server" rather than as a failure — a stale socket path
///   is an expected state (§3.9), and the ownership byte is the real
///   discriminator.
/// - [`IpcError::HandshakeIo`] on a send/receive failure or timeout.
/// - [`IpcError::HandshakeClosed`] if the owner closed the connection without
///   replying — it died between `accept(2)` and its `sendmsg`.
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

    // Both directions, because §3.7 specifies no timeout at all and a server
    // SIGSTOPped between `accept` and `sendmsg` would otherwise block this
    // client in `recvmsg` past the §3.4 deadline — turning "the owner is
    // wedged" into "this process is wedged too".
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

    // **Zero bytes is the owner dying mid-handshake, not a malformed reply.**
    // On a `SOCK_SEQPACKET` connection a 0-byte `recvmsg` is the orderly end of
    // the peer's writing end — the owner was there at `accept(2)` and its
    // descriptors were torn down before it reached `sendmsg`. Handing `&[]` to
    // `from_bytes` produces `WireError::BadLength { got: 0 }`, and that spelling
    // is a *protocol violation*: `verdict` calls it terminal, so a §3.4 loop
    // that should have absorbed a transient inside its deadline instead failed
    // the caller's whole `open()`. Observed once as `BadLength { got: 0 }` in a
    // twelve-minute torture run.
    //
    // **Why `Absent` — which this becomes — is safe, stated here because it is
    // the arm that must never be wrong.** A spurious `Absent` cannot produce a
    // second arena beside a live one: it leads to §3.4 step 2, where a live
    // owner still holds byte 0 and `try_take_ownership` therefore fails, and to
    // step 4, which refuses to create while *any* participant byte is held. The
    // dangerous direction is a *local* failure misfiled as `Absent`
    // (`ClientSocketSetup`), and this is not one — the peer answered the
    // `connect` and then went away, which is a fact about the arena.
    //
    // Placed after the fd drain above rather than straight after `recvmsg`: EOF
    // cannot carry ancillary data, so there is nothing here to leak, and doing
    // it in this order keeps that from resting on a `Drop` in another crate.
    if recv.bytes == 0 {
        return Err(IpcError::HandshakeClosed);
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

/// The real [`crate::ServerProbe`]: connect and complete the §3.7 handshake.
///
/// Attaching *is* the probe. Answering "is anyone serving?" and then attaching
/// as a second step would connect twice, and the owner could die or be replaced
/// between the two — re-running the very race §3.4 exists to settle. So a
/// successful probe comes back holding the segment.
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
/// A pure function so the classification is testable without syscalls, and
/// **because getting one arm wrong here is not a small bug**: anything mapped to
/// [`Verdict::Absent`] tells `open()` there is no arena, and `open()` responds
/// by creating one. A local failure misfiled as `Absent` therefore produces a
/// *second arena beside a live one* — divergence, not an error message.
fn verdict(e: &IpcError) -> Verdict {
    match e {
        // Nobody listening, or an owner that went away mid-handshake — which
        // reaches a client two ways, and both are here: `ECONNRESET` on a
        // connection the dead listener never accepted (`HandshakeIo`), and a
        // 0-byte `recvmsg` on one it did (`HandshakeClosed`). §3.9 makes a stale
        // socket path expected, so all three are simply "no server".
        //
        // `Absent` is safe for the two death arms for the reason `attach` states
        // at the zero-byte check: it leads to §3.4 step 2, where a live owner
        // still holds byte 0, and step 4, which refuses to create while any
        // participant byte is held — so it cannot produce a second arena.
        IpcError::ServerUnreachable { .. }
        | IpcError::HandshakeIo { .. }
        | IpcError::HandshakeClosed => Verdict::Absent,
        // The owner answered. A version or layout disagreement cannot be fixed
        // by waiting, and burning the §3.4 deadline on it would replace a
        // precise message with a timeout. `HandshakeMalformed` belongs here
        // because it is now only reachable for a datagram that *arrived* and was
        // not a `HelloResponse`; the empty one that used to land on it is
        // `HandshakeClosed` above.
        IpcError::HandshakeRejected { .. }
        | IpcError::HandshakeMalformed(_)
        | IpcError::RejectionCarriedFd { .. }
        | IpcError::NoFdReceived => Verdict::Rejected,
        // Everything else — notably `ClientSocketSetup`, which is *this*
        // process running out of descriptors. Calling that "no server" would
        // make an `EMFILE` create a second arena.
        _ => Verdict::Fatal,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The arm that must never move.
    ///
    /// `ClientSocketSetup` is a failure of this process, not evidence about the
    /// arena. Classifying it as `Absent` would make descriptor exhaustion
    /// indistinguishable from an empty machine, and `open()` would create a
    /// second arena next to a live one it merely failed to reach — silent
    /// divergence with no error anywhere.
    #[test]
    fn a_local_socket_failure_is_never_read_as_an_absent_arena() {
        assert_eq!(
            verdict(&IpcError::ClientSocketSetup { raw_os_error: 24 }),
            Verdict::Fatal
        );
    }

    /// The three arms that mean "no server".
    ///
    /// **This test said `..._exactly_the_two_...` and listed two.** The third is
    /// [`IpcError::HandshakeClosed`], and its absence is the defect the variant
    /// was added for: an owner that died between `accept(2)` and its `sendmsg`
    /// reached this function as `HandshakeMalformed(BadLength { got: 0 })` and
    /// was classified **terminal**, so §3.4 failed the caller's whole `open()`
    /// on a transient it was built to absorb.
    ///
    /// `ServerUnreachable` is `connect` finding nothing; the other two are the
    /// two ways a *dying* owner reaches a client — `ECONNRESET` on a connection
    /// it never accepted, a 0-byte `recvmsg` on one it did.
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
    ///
    /// **`HandshakeMalformed` was the case this list was missing**, and it is the
    /// one that matters now: the variant's meaning narrowed when
    /// [`IpcError::HandshakeClosed`] took the empty datagram off it, so what is
    /// left is a reply that *arrived* and was not a `HelloResponse`. That is a
    /// protocol violation, waiting cannot repair it, and it must stay terminal —
    /// while the length error `from_bytes` produces for zero bytes must never
    /// reach here at all. `BadMagic` is the case that can only be a real
    /// violation, which is why it is the one written down.
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

    /// **An owner that closes after accepting is `Absent`, not malformed.**
    ///
    /// Staged against real sockets because the fact under test belongs to the
    /// kernel, and the two halves of "the owner went away" are not the same
    /// syscall result: an **accepted** connection whose peer's descriptors are
    /// torn down gives the client a 0-byte `recvmsg`, while a connection the
    /// listener never accepted gives `ECONNRESET`. Measured both ways before this
    /// was written — so a staging that skips the `accept(2)` would exercise the
    /// `HandshakeIo` arm, which was already right, and prove nothing.
    ///
    /// The thread below therefore accepts, reads the request, and closes without
    /// replying: byte for byte what the owner's descriptors do when it dies
    /// inside `OwnerServer::accept_one`, between the `accept` and the `sendmsg`.
    ///
    /// Mutant — delete the `recv.bytes == 0` guard in [`attach`] ⇒ applied, and
    /// this fails on the **first** assertion, with
    /// `left: HandshakeMalformed(BadLength { got: 0, expected: 56 })`. The second
    /// assertion is what that error costs and is asserted separately rather than
    /// reached: the panic stops the test before it, and
    /// `the_no_server_arms_are_exactly_the_three_that_mean_no_server` is where
    /// the classification is pinned on its own.
    #[test]
    fn an_owner_that_closes_after_accepting_is_absent_not_malformed() {
        use crate::wire::HELLO_REQUEST_LEN;
        use rustix::net::{accept_with, bind, listen};

        let dir =
            std::env::temp_dir().join(format!("tf_tree_ipc_cli-{}-zero-byte", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("a.sock");

        // Bound and listening *before* the thread exists, so `connect` cannot
        // race the listen and land on the `ServerUnreachable` arm instead.
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
            // Read the request, as the real accept loop does, and then let both
            // descriptors close with no reply written.
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
