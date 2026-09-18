//! `Copy`, `String`-free errors that name what failed.
//!
//! Same rule as the rest of the workspace (`docs/PROJECT.md` §5): integers and
//! enums, never an allocation. The rendezvous runs at process start, when the
//! arena may be unmappable and the failure has to be reportable by a binary
//! with no allocator state left to trust. It also means an error can be
//! returned from a signal-adjacent path later without revisiting the type.
//!
//! Every variant names *both* sides of whatever disagreed, or the exact
//! environment variable / slot / errno responsible. `docs/PHASE2.md` §3.7 makes
//! the point for `LayoutMismatch`; it applies equally to everything here,
//! because the symptom an operator sees ("it will not start") is identical for
//! all of them.

use crate::{HelloStatus, WireError};
use core::fmt;

use rustix::io::Errno;

/// Which candidate of the `docs/PHASE2.md` §3.1 resolution order a directory
/// came from, so an error can say *why* this path was even considered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeDirSource {
    /// `$TF_TREE_RUNTIME_DIR`, set explicitly by the operator.
    Env,
    /// `$XDG_RUNTIME_DIR/tf_tree` — normally `/run/user/<uid>/tf_tree`.
    XdgRuntimeDir,
    /// `/run/tf_tree`, for system services.
    Run,
    /// `/tmp/tf_tree-<uid>`, the last resort.
    Tmp,
}

impl RuntimeDirSource {
    /// The variable or literal path this source resolves from, for messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            RuntimeDirSource::Env => "$TF_TREE_RUNTIME_DIR",
            RuntimeDirSource::XdgRuntimeDir => "$XDG_RUNTIME_DIR/tf_tree",
            RuntimeDirSource::Run => "/run/tf_tree",
            RuntimeDirSource::Tmp => "/tmp/tf_tree-<uid>",
        }
    }
}

/// An environment variable the rendezvous reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvVar {
    /// `$TF_TREE_RUNTIME_DIR`.
    RuntimeDir,
    /// `$TF_TREE_DOMAIN`.
    Domain,
    /// `$ROS_DOMAIN_ID` — inherited so `tf_tree` partitions the way the rest of
    /// a ROS 2 stack already does (`docs/PHASE2.md` §3.2).
    RosDomainId,
    /// `$TF_TREE_NAME`.
    Name,
}

impl EnvVar {
    /// The variable's spelling, for messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            EnvVar::RuntimeDir => "TF_TREE_RUNTIME_DIR",
            EnvVar::Domain => "TF_TREE_DOMAIN",
            EnvVar::RosDomainId => "ROS_DOMAIN_ID",
            EnvVar::Name => "TF_TREE_NAME",
        }
    }
}

/// Why an arena name was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameProblem {
    /// Empty. An empty name would resolve to a path ending in `.lock`, which
    /// silently collides with nothing and shares with nobody.
    Empty,
    /// Longer than [`crate::MAX_NAME_LEN`].
    TooLong,
    /// Contains `/` or a NUL, or is `.`/`..`. The name is a single path
    /// component: anything that could traverse would let `$TF_TREE_NAME` point
    /// two processes at different directories while both believe they agreed.
    NotOneComponent,
    /// Not UTF-8. The name reaches both a filename and a fixed-size identity
    /// record; requiring UTF-8 keeps those two representations the same string.
    NotUtf8,
}

/// Which lock-file role a failing operation was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockRole {
    /// Byte 0 — ownership. Its holder serves the socket.
    Ownership,
    /// Byte 1 — A2's topology mutation lock. Held for one `Tree::reparent`, and
    /// never long enough for a peer to observe except under contention.
    Topology,
    /// Byte `16 + i` — participant liveness for slot `i`.
    Participant(u32),
    /// A per-edge claim lease (`docs/PHASE2.md` §6.1). The edge id, not a
    /// participant slot — the two index different byte ranges of the same file
    /// and confusing them would hand one edge to two writers.
    Claim(u32),
}

/// A `/proc/<pid>/stat` parse failure, split out from the read failure so a
/// malformed line is never confused with an exited process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcParseError {
    /// No `)` at all, so `comm` cannot be delimited. See
    /// [`crate::parse_start_time`] for why the *last* one is the only safe
    /// anchor.
    NoClosingParen,
    /// The line ends before field 22 (`starttime`), counting `comm` as field 2.
    TooFewFields,
    /// Field 22 is not a decimal integer.
    NotAnInteger,
}

/// Reading a process's identity out of `/proc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcError {
    /// `/proc/<pid>/stat` could not be read — usually because the process is
    /// gone, which is information rather than a fault.
    Unreadable {
        /// The process asked about.
        pid: u32,
        /// `errno`, or `0` if the OS did not supply one.
        raw_os_error: i32,
    },
    /// The line was read but did not parse.
    Parse {
        /// The process asked about.
        pid: u32,
        /// What went wrong.
        cause: ProcParseError,
    },
    /// `/proc/sys/kernel/random/boot_id` was unreadable or not a UUID. Without
    /// it, identity records cannot be compared across a reboot.
    BootId,
}

/// Everything the rendezvous substrate can fail at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcError {
    /// The runtime directory could not be created or opened.
    RuntimeDirUnusable {
        /// Which candidate failed.
        source: RuntimeDirSource,
        /// `errno`, or `0` if the OS did not supply one.
        raw_os_error: i32,
    },
    /// The resolved runtime directory path exists and is not a directory.
    RuntimeDirNotADirectory {
        /// Which candidate produced it.
        source: RuntimeDirSource,
    },
    /// The runtime directory belongs to another user.
    ///
    /// Only checked for [`RuntimeDirSource::Tmp`], whose parent is world
    /// writable: another user could have pre-created `/tmp/tf_tree-<uid>` and be
    /// holding locks in it. `docs/PHASE2.md` §3.10 scopes the trust model to
    /// same-user processes, and this is where that boundary is actually
    /// checkable.
    RuntimeDirForeignOwner {
        /// The directory's owner.
        owner_uid: u32,
        /// This process's uid.
        our_uid: u32,
    },
    /// `statfs` on the runtime directory failed, so the NORMATIVE §3.1 network
    /// filesystem check could not be performed. Refusing is the safe answer:
    /// the whole rendezvous is built on lock semantics this could not confirm.
    StatFsFailed {
        /// Which candidate produced the directory.
        source: RuntimeDirSource,
        /// `errno`.
        raw_os_error: i32,
    },
    /// The runtime directory is on NFS or CIFS.
    ///
    /// NORMATIVE refusal (`docs/PHASE2.md` §3.1): file locks over network
    /// filesystems have subtly different semantics — lease-based, recoverable,
    /// and not guaranteed to be released promptly on client death — and every
    /// property this design relies on ("released by the kernel, immediately at
    /// the end of the holder's exit") stops being true.
    NetworkFilesystem {
        /// Which candidate produced the directory.
        source: RuntimeDirSource,
        /// `statfs.f_type`, so the message can name the filesystem.
        magic: u64,
    },
    /// A domain variable was set to something that is not a `u32`.
    ///
    /// Deliberately fatal rather than falling back to domain 0: a typo in
    /// `$ROS_DOMAIN_ID` that silently resolved to the default would put a
    /// process on the *wrong arena*, which is the one outcome §3 exists to make
    /// impossible.
    DomainNotAnInteger {
        /// Which variable.
        var: EnvVar,
    },
    /// The arena name is unusable as a single path component.
    NameInvalid {
        /// Which variable supplied it (or [`EnvVar::Name`] for the default).
        var: EnvVar,
        /// What is wrong with it.
        problem: NameProblem,
    },
    /// The lock file could not be opened. It is created on demand with mode
    /// `0600`; failure here usually means the runtime directory is not writable.
    LockFileOpen {
        /// `errno`.
        raw_os_error: i32,
    },
    /// A `pread`/`pwrite` of an identity record failed.
    IdentityIo {
        /// The slot whose record was being read or written.
        slot: u32,
        /// `errno`.
        raw_os_error: i32,
    },
    /// `fcntl(F_OFD_SETLK)` or `F_OFD_GETLK` failed for a reason other than
    /// contention. Contention is not an error — it is the answer.
    LockFailed {
        /// Which byte.
        role: LockRole,
        /// `errno`.
        errno: Errno,
    },
    /// An edge id that cannot be addressed within the reserved claim region.
    ///
    /// Only reachable from a corrupt header: `ArenaLayout` accepts far fewer
    /// edges than the region holds. Bounded anyway, because the failure it
    /// prevents — a claim byte colliding with an identity record — hands one
    /// edge to two writers and presents as impossible numbers rather than as
    /// an error.
    ClaimOutOfRange {
        /// The edge asked for.
        edge: u32,
        /// The number of claim bytes reserved.
        limit: u64,
    },
    /// Every participant slot is locked, so this process cannot register.
    ///
    /// The limit is a build constant ([`crate::MAX_PARTICIPANTS`]); raising it
    /// is a recompile, and the message says so because the alternative is an
    /// operator concluding the machine is broken.
    NoParticipantSlots {
        /// The current limit.
        limit: u32,
    },
    /// Nothing was serving, nothing was alive, and the caller asked for
    /// [`crate::CreatePolicy::Never`].
    ///
    /// The intended failure for a supervised consumer that must not silently
    /// create an empty arena because the estimator has not started yet.
    ArenaAbsent,
    /// The §3.7 socket path exceeds `sun_path`.
    ///
    /// `$TF_TREE_RUNTIME_DIR` is arbitrary, so this is reachable from
    /// configuration rather than from a bug. Reported here, at construction,
    /// rather than as a bare `EINVAL` from inside `bind`.
    SocketPathTooLong {
        /// Length of the path.
        len: usize,
        /// The kernel's limit.
        limit: usize,
    },
    /// Nothing is listening on the §3.7 socket.
    ///
    /// **Not a failure by itself.** §3.9 makes a stale socket path an expected
    /// state, so `open()` reads this as "no server" and lets the ownership byte
    /// decide.
    ServerUnreachable {
        /// `connect` errno.
        raw_os_error: i32,
    },
    /// This process could not set up its own socket to attach with.
    ///
    /// **Deliberately distinct from [`IpcError::HandshakeIo`].** A probe reads
    /// `HandshakeIo` as "no server" (§3.9), which is right for a peer that died
    /// mid-handshake and catastrophically wrong for `EMFILE` in *this* process:
    /// running out of descriptors would be read as "the arena is not there",
    /// and this process would go on to create a second one beside a live arena
    /// it simply failed to reach.
    ClientSocketSetup {
        /// The errno.
        raw_os_error: i32,
    },
    /// A send or receive during the handshake failed, or timed out.
    HandshakeIo {
        /// The errno.
        raw_os_error: i32,
    },
    /// The owner accepted the connection and then closed it without replying.
    ///
    /// `recvmsg` returned **zero bytes**, which on a `SOCK_SEQPACKET` connection
    /// is the orderly end of the peer's writing end and not an error: the owner
    /// was there at `accept(2)` and gone before its `sendmsg`. Measured, because
    /// the two halves of "the owner went away" reach the client differently — an
    /// *accepted* connection whose peer dies gives this 0-byte read, while a
    /// connection the listener never accepted gives `ECONNRESET`, which arrives
    /// as [`IpcError::HandshakeIo`]. Both are the same fact about the arena, and
    /// [`crate::SocketProbe`] classifies them together.
    ///
    /// **Deliberately distinct from [`IpcError::HandshakeMalformed`], which is
    /// what this used to be reported as.** Handing an empty datagram to
    /// `HelloResponse::from_bytes` yields
    /// [`crate::WireError::BadLength`]` { got: 0 }` — a *protocol violation*,
    /// which §3.4 treats as terminal — so an owner that died inside the
    /// handshake failed the joiner's whole `open()` instead of being retried
    /// inside its deadline. `docs/decisions/0005`'s client-reachability table has
    /// answered `Absent` for "peer HUPs or times out mid-handshake" since it was
    /// written; this variant is the half of that row the code was missing.
    ///
    /// Carries nothing: there is no errno, and the pid on the far end is exactly
    /// what a dead owner cannot be asked for.
    HandshakeClosed,
    /// The peer's datagram was not a well-formed handshake message.
    ///
    /// **A protocol violation, and therefore terminal.** An owner that merely
    /// went away mid-handshake is [`IpcError::HandshakeClosed`] instead.
    HandshakeMalformed(crate::wire::WireError),
    /// The owner refused this client, and named its own side of the comparison.
    HandshakeRejected {
        /// Why.
        status: crate::wire::HelloStatus,
        /// The **owner's** format version, so a caller can print both sides.
        owner_format_version: u32,
        /// The **owner's** layout hash, likewise.
        owner_layout_hash: u32,
    },
    /// The owner *rejected* the attach but sent a segment fd anyway.
    ///
    /// A protocol violation (§3.7: a rejection carries no fd). Reported rather
    /// than silently dropped, because the failure it guards against is a client
    /// that ignores `status` and maps a segment it was refused — and a
    /// violation nobody reports is one nobody fixes.
    RejectionCarriedFd {
        /// The status the owner sent alongside the fd.
        status: crate::wire::HelloStatus,
    },
    /// The owner accepted but sent no `SCM_RIGHTS` fd.
    ///
    /// A protocol violation rather than an I/O failure: an acceptance with no
    /// segment is not something a correct owner can produce, and silently
    /// treating it as "no server" would hide an owner bug behind a retry.
    NoFdReceived,
    /// A live arena exists — some participant still holds its lock byte — but
    /// nothing is serving it, and nobody took over before the deadline.
    ///
    /// This is the §3.4 timeout, and it is **correct behaviour rather than a
    /// limitation**: the alternative to refusing is creating a second arena
    /// while the first is still in use, which diverges silently. The stuck slots
    /// are named so an operator can see exactly what to `kill`; full identity
    /// records for them are readable with [`crate::LockFile::read_identity`].
    ArenaHeldButUnreachable {
        /// Bitmask of participant slots whose lock byte is still held. Zero
        /// means nobody is attached and the *ownership* byte was what stayed
        /// held — a process that took it and never began serving.
        holder_slots: u64,
        /// The lowest held slot, or `None` when no participant byte is held.
        ///
        /// `Option` rather than a sentinel: an empty mask has no first slot, and
        /// encoding that as a number invites a consumer to log a slot that does
        /// not exist.
        first_slot: Option<u32>,
        /// The pid in that slot's identity record, or `0` if it was never
        /// written. Advisory (§5.1): the lock is the liveness, this is the name.
        first_pid: u32,
        /// Whether the **ownership** byte was held by somebody else at the
        /// moment this error was built.
        ///
        /// One `F_OFD_GETLK` taken at the deadline, so — like `first_pid` — it
        /// is advisory: it says what was true at that instant, not what was
        /// true for the whole timeout. It is carried because it is the one bit
        /// that separates the two remedies, and `Display` spends it: a forced
        /// create ([`crate::CreatePolicy::Always`]) has to take the ownership
        /// byte before it reaches the participant bytes it is allowed to skip,
        /// so with this `true` it cannot help, and with it `false` and
        /// `first_slot` above 0 it is exactly the case `docs/PHASE2.md` §3.4
        /// offers it for.
        ownership_held: bool,
    },
    /// A `/proc` read needed for an identity record failed.
    Proc(ProcError),
}

impl IpcError {
    /// Build the `errno`-carrying variants from a [`std::io::Error`] without
    /// keeping the (allocating, non-`Copy`) error itself.
    pub(crate) fn os(err: &std::io::Error) -> i32 {
        err.raw_os_error().unwrap_or(0)
    }
}

impl From<ProcError> for IpcError {
    fn from(e: ProcError) -> IpcError {
        IpcError::Proc(e)
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            IpcError::RuntimeDirUnusable {
                source,
                raw_os_error,
            } => write!(
                f,
                "runtime directory from {} is unusable (errno {raw_os_error})",
                source.as_str()
            ),
            IpcError::SocketPathTooLong { len, limit } => write!(
                f,
                "attach socket path is {len} bytes; the kernel's sun_path limit is {limit}. \
                 Set TF_TREE_RUNTIME_DIR to a shorter directory"
            ),
            IpcError::ServerUnreachable { raw_os_error } => write!(
                f,
                "nothing is listening on the attach socket (errno {raw_os_error})"
            ),
            IpcError::ClientSocketSetup { raw_os_error } => write!(
                f,
                "could not create a socket to attach with (errno {raw_os_error}); \
                 this is a local resource failure, not an absent arena"
            ),
            IpcError::HandshakeIo { raw_os_error } => {
                write!(f, "attach handshake failed (errno {raw_os_error})")
            }
            IpcError::HandshakeClosed => f.write_str(
                "the arena owner accepted this attach and then closed the connection without \
                 replying, so it went away mid-handshake; retrying is the right response",
            ),
            // Matched inline rather than through a `Display` on `WireError`,
            // as `NameProblem` and `LockRole` are below: a trait impl on a
            // published type is a commitment, and this sentence is not one.
            IpcError::HandshakeMalformed(e) => {
                f.write_str("attach handshake reply was not well-formed: ")?;
                match e {
                    WireError::BadLength { got, expected } => write!(
                        f,
                        "{got} bytes where the message is {expected}"
                    ),
                    WireError::BadMagic => f.write_str("the magic bytes are not tf_tree's"),
                    WireError::BadMode { got } => write!(
                        f,
                        "mode byte {got} names neither read-only nor read-write"
                    ),
                }
            }
            IpcError::HandshakeRejected {
                status,
                owner_format_version,
                owner_layout_hash,
            } => write!(
                f,
                "the arena owner refused this attach: {status:?} \
                 (owner format_version {owner_format_version}, \
                 layout_hash 0x{owner_layout_hash:08X}). {}",
                rejection_advice(status)
            ),
            IpcError::RejectionCarriedFd { status } => write!(
                f,
                "the arena owner refused this attach ({status:?}) but sent a segment fd anyway; \
                 this is an owner bug and the segment was not mapped"
            ),
            IpcError::NoFdReceived => write!(
                f,
                "the arena owner accepted the attach but sent no segment fd; this is an owner bug"
            ),
            IpcError::RuntimeDirNotADirectory { source } => {
                write!(f, "{} exists but is not a directory", source.as_str())
            }
            IpcError::RuntimeDirForeignOwner {
                owner_uid,
                our_uid,
            } => write!(
                f,
                "runtime directory is owned by uid {owner_uid}, not by uid {our_uid}; \
                 refusing to share a lock file with another user"
            ),
            IpcError::StatFsFailed {
                source,
                raw_os_error,
            } => write!(
                f,
                "statfs on the runtime directory from {} failed (errno {raw_os_error}); \
                 cannot confirm it is not a network filesystem",
                source.as_str()
            ),
            IpcError::NetworkFilesystem { source, magic } => write!(
                f,
                "runtime directory from {} is on a network filesystem (statfs f_type {magic:#x}); \
                 file-lock semantics there are not exact enough for the rendezvous; set \
                 TF_TREE_RUNTIME_DIR to a local path",
                source.as_str()
            ),
            IpcError::DomainNotAnInteger { var } => {
                write!(f, "${} is not a non-negative integer", var.as_str())
            }
            IpcError::NameInvalid { var, problem } => {
                write!(f, "${} is not a usable arena name: ", var.as_str())?;
                match problem {
                    NameProblem::Empty => f.write_str("it is empty"),
                    NameProblem::TooLong => write!(f, "longer than {} bytes", crate::MAX_NAME_LEN),
                    NameProblem::NotOneComponent => {
                        f.write_str("it must be a single path component")
                    }
                    NameProblem::NotUtf8 => f.write_str("it is not UTF-8"),
                }
            }
            IpcError::LockFileOpen { raw_os_error } => {
                write!(f, "cannot open the lock file (errno {raw_os_error})")
            }
            IpcError::IdentityIo { slot, raw_os_error } => write!(
                f,
                "identity record for slot {slot} could not be read or written (errno {raw_os_error})"
            ),
            IpcError::LockFailed { role, errno } => {
                match role {
                    LockRole::Ownership => f.write_str("ownership byte")?,
                    LockRole::Topology => f.write_str("topology byte")?,
                    LockRole::Participant(slot) => write!(f, "participant byte for slot {slot}")?,
                    LockRole::Claim(edge) => write!(f, "claim byte for edge {edge}")?,
                }
                write!(f, ": fcntl failed with errno {}", errno.raw_os_error())
            }
            IpcError::ClaimOutOfRange { edge, limit } => write!(
                f,
                "edge {edge} is outside the {limit} claim bytes reserved in the lock file; \
                 the arena header is inconsistent"
            ),
            IpcError::NoParticipantSlots { limit } => write!(
                f,
                "all {limit} participant slots are live; raising the limit requires \
                 rebuilding with a larger MAX_PARTICIPANTS and recreating the arena"
            ),
            IpcError::ArenaAbsent => f.write_str(
                "no arena is serving and CreatePolicy::Never forbids creating one",
            ),
            // **These arms state facts and end with the variant name; the
            // remedy lives in `docs/RUNBOOK.md`** (`0055` part 4, step 6).
            //
            // They used to carry the remedy itself, in four branches that
            // crossed the participant mask with `ownership_held`, and it went
            // wrong three times in one day: a missing requirement, a discarded
            // `ownership_held`, and a repair that asserted two holders and was
            // false in the steady state of a healthy arena. The remedy wants to
            // say which *processes* to stop, and this type sees which *bytes*
            // are held — an inference it cannot make. The runbook's reader has
            // every process in hand and can be given an ordering; this one is a
            // process being refused an attachment.
            //
            // The shape is `0059`'s convention (g): ASCII, ending with the
            // variant name **in parentheses** as the runbook's search key — the
            // spelling `tf_tree_arena`'s `check.rs` and `frozen.rs` already use
            // (`(Unsealed)`, `(LayoutMismatch)`). A first version ended
            // `: ArenaHeldButUnreachable`, a second spelling of a convention
            // living in two other crates.
            //
            // **Convention (e) — at most 120 bytes — is NOT met, and that is
            // stated rather than claimed away.** These arms are over it; the
            // gate measures them and `MESSAGE_BUDGET` is what binds them. (e)
            // was
            // derived for an arena error nested inside *two* wrappers (35 + 21
            // bytes of prefix) carrying errnos and layout hashes; this variant
            // carries a 64-bit mask and two 32-bit ids, up to 38 bytes of
            // digits on their own. `MESSAGE_BUDGET` below is what binds it,
            // derived from the same C path `0059` measured.
            //
            // ASCII and length both matter at the C boundary:
            // `tft_tree_open_named` formats this into a 256-byte
            // `tft_error::message` and substitutes `?` per non-ASCII byte, so
            // the four-branch version this replaced lost its whole remedy to
            // truncation and printed its em-dashes as `???`.
            //
            // **No byte counts in this comment, on purpose.** Every figure here
            // was written into a code comment, a changelog entry and a decision
            // record at once, and four review rounds found a stale copy every
            // time. `MESSAGE_BUDGET` and
            // `every_ipc_error_message_fits_the_c_abis_buffer` hold the numbers
            // where they are executable.
            IpcError::ArenaHeldButUnreachable {
                holder_slots: 0,
                ownership_held: true,
                ..
            } => f.write_str(
                "nobody attached; the ownership byte was held for the whole open timeout \
                 by a process that never served; nothing created (ArenaHeldButUnreachable)",
            ),
            // Same empty mask, but the ownership probe at the deadline came back
            // free: the mask and the probe are read at two instants, so a holder
            // that let go in between lands here and the arm above would claim it
            // was held throughout.
            IpcError::ArenaHeldButUnreachable {
                holder_slots: 0, ..
            } => f.write_str(
                "no byte was held at the open deadline, so whatever blocked every attempt \
                 had let go; retry (ArenaHeldButUnreachable)",
            ),
            IpcError::ArenaHeldButUnreachable {
                holder_slots,
                first_slot: Some(slot),
                first_pid,
                ownership_held,
            } => write!(
                f,
                "arena alive but unreachable: participant bytes {holder_slots:#x} held, \
                 lowest slot {slot} (pid {first_pid}{creator}), ownership byte {own} \
                 (ArenaHeldButUnreachable)",
                creator = if slot == 0 { ", the creator's" } else { "" },
                own = if ownership_held { "held" } else { "free" },
            ),
            IpcError::ArenaHeldButUnreachable {
                first_slot: None, ..
            } => f.write_str(
                "arena alive but unreachable: no participant byte held, yet ownership \
                 could not be taken before the deadline (ArenaHeldButUnreachable)",
            ),
            IpcError::Proc(e) => write!(f, "{e}"),
        }
    }
}

/// What to do about a refused attach, for the status that was actually
/// returned.
///
/// **One arm per status, because the alternative was measured and it misleads.**
/// This used to be a single sentence appended to every rejection explaining what
/// a `LayoutMismatch` means. A torture run that exhausted the participant table
/// therefore printed, thousands of times:
///
/// ```text
/// the arena owner refused this attach: NoParticipantSlots (owner format_version 3,
/// layout_hash 0x3D104195). A LayoutMismatch means this binary was built against a
/// different record layout than the running arena — rebuild both from the same source
/// ```
///
/// The status is right there and it is not `LayoutMismatch`, but the advice is
/// the longest and last thing on the line, so it reads as the diagnosis and
/// points the operator at their build. Prose that explains a status the caller
/// did not get is worse than no prose: it costs a rebuild before anyone rereads
/// the word in front of it.
fn rejection_advice(status: HelloStatus) -> &'static str {
    match status {
        // Reachable only from a rejection, which `Ok` is not — but the match is
        // total so the compiler tells the next person who adds a status that
        // this list needs a line.
        HelloStatus::Ok => "this was not a refusal",
        HelloStatus::VersionMismatch => {
            "this binary speaks a different arena FORMAT_VERSION than the running owner; \
             both sides must be built from the same release"
        }
        HelloStatus::LayoutMismatch => {
            "same version, different record layout: this binary was built against a \
             different arena layout than the running owner; rebuild both from the same source"
        }
        HelloStatus::BootIdMismatch => {
            "the arena records a different boot id than this host is running, so it \
             outlived a reboot; nothing in it is alive and it should be removed"
        }
        HelloStatus::NoParticipantSlots => {
            "every participant slot is taken. If the participants are real, raise the \
             arena's participant limit, which needs an owner restart; if they are not, \
             the slots are held by records of processes that died: `tf_tree participants` \
             prints one line per slot and marks those `stale`"
        }
        HelloStatus::ModeNotPermitted => {
            "this attach asked for read-write on an arena the owner will not let it write; \
             attach read-only, which is the consumer default"
        }
        HelloStatus::Malformed => {
            "the owner could not decode this attach request, or refused it for a reason \
             this build has no name for; the two are indistinguishable on the wire, so \
             check that both sides are the same release before reading it as corruption"
        }
    }
}

impl fmt::Display for ProcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ProcError::Unreadable { pid, raw_os_error } => {
                write!(f, "/proc/{pid}/stat is unreadable (errno {raw_os_error})")
            }
            ProcError::Parse { pid, cause } => {
                write!(f, "/proc/{pid}/stat did not parse: ")?;
                match cause {
                    ProcParseError::NoClosingParen => f.write_str("no ')' delimits comm"),
                    ProcParseError::TooFewFields => {
                        f.write_str("the line ends before field 22 (starttime)")
                    }
                    ProcParseError::NotAnInteger => {
                        f.write_str("field 22 is not a decimal integer")
                    }
                }
            }
            ProcError::BootId => f.write_str("/proc/sys/kernel/random/boot_id is not a UUID"),
        }
    }
}

impl std::error::Error for IpcError {}
impl std::error::Error for ProcError {}

#[cfg(test)]
mod tests {
    use super::{IpcError, ProcError, ProcParseError};
    use crate::WireError;

    /// Every [`IpcError`] variant, **and every value its `Display` branches
    /// on**, for the length and ASCII gates.
    ///
    /// **Two mechanisms, and what each one actually catches — because a first
    /// version of this doc claimed one of them did both.**
    ///
    /// * The exhaustive `match` at the end forces a *new* variant to be
    ///   handled here: `IpcError` is not `#[non_exhaustive]` and this module is
    ///   inside its own crate, so adding a variant without adding an arm fails
    ///   to compile. It does **not** force a `push`, which is what the first
    ///   version claimed ("a new variant that is not sampled here fails to
    ///   compile") — deleting `out.push(IpcError::ArenaAbsent)` left all three
    ///   tests green.
    /// * `VARIANTS_SAMPLED` is what catches that: the gate counts distinct
    ///   `mem::discriminant`s and refuses a set smaller than this. It catches a
    ///   push that is deleted or forgotten among the variants that exist today.
    ///
    /// **What neither forces** is bumping `VARIANTS_SAMPLED` when a variant is
    /// added, so a new variant can still arrive sampled-by-nobody if its author
    /// adds a match arm and stops. Safe Rust has no way to enumerate a plain
    /// enum's variants, so this is a review rule and is written down rather
    /// than implied. The match arm is the prompt; this sentence is the reason.
    const VARIANTS_SAMPLED: usize = 24;
    ///
    /// **One sample per variant is not enough, and four mutants proved it.** A
    /// first version of this returned exactly that. Three of four deliberate
    /// defects — a procedure added to an arm, 220 bytes added to an arm, a
    /// non-ASCII byte added to an arm — **passed**, because every one of them
    /// was in an `ArenaHeldButUnreachable` branch the single sample did not
    /// select. A gate over a branching `Display` has to enumerate the branches
    /// or it measures one of them and reports on all.
    ///
    /// So each field the formatter switches on is swept: the four
    /// [`RuntimeDirSource`]s, the four [`EnvVar`]s, the four [`NameProblem`]s,
    /// the four [`LockRole`]s, the seven `HelloStatus`es, the three
    /// [`WireError`]s, [`ProcError`]'s arms including all three parse causes,
    /// and `ArenaHeldButUnreachable`'s seven reachable states.
    fn samples() -> Vec<IpcError> {
        use crate::wire::HelloStatus as H;
        use crate::{EnvVar, LockRole, NameProblem, RuntimeDirSource as R};
        let sources = [R::Env, R::XdgRuntimeDir, R::Run, R::Tmp];
        let vars = [
            EnvVar::RuntimeDir,
            EnvVar::Domain,
            EnvVar::RosDomainId,
            EnvVar::Name,
        ];
        let problems = [
            NameProblem::Empty,
            NameProblem::TooLong,
            NameProblem::NotOneComponent,
            NameProblem::NotUtf8,
        ];
        let roles = [
            LockRole::Ownership,
            LockRole::Topology,
            LockRole::Participant(63),
            LockRole::Claim(63),
        ];
        let statuses = [
            H::Ok,
            H::VersionMismatch,
            H::LayoutMismatch,
            H::BootIdMismatch,
            H::NoParticipantSlots,
            H::ModeNotPermitted,
            H::Malformed,
        ];
        let mut out = Vec::new();
        for source in sources {
            out.push(IpcError::RuntimeDirUnusable {
                source,
                raw_os_error: 13,
            });
            out.push(IpcError::RuntimeDirNotADirectory { source });
            out.push(IpcError::StatFsFailed {
                source,
                raw_os_error: 13,
            });
            out.push(IpcError::NetworkFilesystem {
                source,
                magic: 0x6969,
            });
        }
        for var in vars {
            out.push(IpcError::DomainNotAnInteger { var });
            for problem in problems {
                out.push(IpcError::NameInvalid { var, problem });
            }
        }
        for role in roles {
            out.push(IpcError::LockFailed {
                role,
                errno: rustix::io::Errno::ACCESS,
            });
        }
        for status in statuses {
            out.push(IpcError::HandshakeRejected {
                status,
                owner_format_version: 3,
                owner_layout_hash: 0x3D10_4195,
            });
            out.push(IpcError::RejectionCarriedFd { status });
        }
        for wire in [
            WireError::BadLength {
                got: 3,
                expected: 16,
            },
            WireError::BadMagic,
            WireError::BadMode { got: 9 },
        ] {
            out.push(IpcError::HandshakeMalformed(wire));
        }
        for proc in [
            ProcError::Unreadable {
                pid: 4242,
                raw_os_error: 13,
            },
            ProcError::Parse {
                pid: 4242,
                cause: ProcParseError::NoClosingParen,
            },
            ProcError::Parse {
                pid: 4242,
                cause: ProcParseError::TooFewFields,
            },
            ProcError::Parse {
                pid: 4242,
                cause: ProcParseError::NotAnInteger,
            },
            ProcError::BootId,
        ] {
            out.push(IpcError::Proc(proc));
        }
        // `ArenaHeldButUnreachable`'s own arms: the two empty-mask states, the
        // `Some(slot)` arm with slot 0 and with a joiner's slot, both ownership
        // readings, and the `first_slot: None` arm with a non-empty mask, which
        // no other test constructs.
        for (holder_slots, first_slot, ownership_held) in [
            // The widest ids the fields can carry: a 64-bit mask and two 32-bit
            // ids are 38 bytes of digits, which a `0x5` / `pid 4242` sample does
            // not measure and the budget has to survive.
            (u64::MAX, Some(u32::MAX), true),
            (u64::MAX, Some(u32::MAX), false),
            (0u64, None, true),
            (0, None, false),
            (0b1, Some(0u32), false),
            (0b1, Some(0), true),
            (0b101, Some(0), false),
            (0b101, Some(0), true),
            (0b1000, Some(3), false),
            (0b1000, Some(3), true),
            (0b1, None, false),
            (0b1, None, true),
        ] {
            out.push(IpcError::ArenaHeldButUnreachable {
                holder_slots,
                first_slot,
                first_pid: 4242,
                ownership_held,
            });
        }
        out.push(IpcError::LockFileOpen { raw_os_error: 13 });
        out.push(IpcError::IdentityIo {
            slot: 7,
            raw_os_error: 13,
        });
        out.push(IpcError::ClaimOutOfRange { edge: 9, limit: 64 });
        out.push(IpcError::NoParticipantSlots { limit: 64 });
        out.push(IpcError::ArenaAbsent);
        out.push(IpcError::SocketPathTooLong {
            len: 120,
            limit: 108,
        });
        out.push(IpcError::ServerUnreachable { raw_os_error: 111 });
        out.push(IpcError::ClientSocketSetup { raw_os_error: 13 });
        out.push(IpcError::HandshakeIo { raw_os_error: 13 });
        out.push(IpcError::HandshakeClosed);
        out.push(IpcError::NoFdReceived);
        // Exhaustiveness: no wildcard arm, so a new variant breaks the build.
        for s in &out {
            match s {
                IpcError::RuntimeDirUnusable { .. }
                | IpcError::RuntimeDirNotADirectory { .. }
                | IpcError::RuntimeDirForeignOwner { .. }
                | IpcError::StatFsFailed { .. }
                | IpcError::NetworkFilesystem { .. }
                | IpcError::DomainNotAnInteger { .. }
                | IpcError::NameInvalid { .. }
                | IpcError::LockFileOpen { .. }
                | IpcError::IdentityIo { .. }
                | IpcError::LockFailed { .. }
                | IpcError::ClaimOutOfRange { .. }
                | IpcError::NoParticipantSlots { .. }
                | IpcError::ArenaAbsent
                | IpcError::SocketPathTooLong { .. }
                | IpcError::ServerUnreachable { .. }
                | IpcError::ClientSocketSetup { .. }
                | IpcError::HandshakeIo { .. }
                | IpcError::HandshakeClosed
                | IpcError::HandshakeMalformed(_)
                | IpcError::HandshakeRejected { .. }
                | IpcError::RejectionCarriedFd { .. }
                | IpcError::NoFdReceived
                | IpcError::ArenaHeldButUnreachable { .. }
                | IpcError::Proc(_) => {}
            }
        }
        out.push(IpcError::RuntimeDirForeignOwner {
            owner_uid: 1000,
            our_uid: 1001,
        });
        out
    }

    /// **Every message must survive the C ABI, which is a 256-byte array**
    /// (`0055` step 6; `0059` convention (g) is where the shape comes from — (e)
    /// is not met here, and the arm's own comment says why).
    ///
    /// `tft_tree_open_named`'s failure arm formats `could not open the arena:
    /// {e}` into `tft_error::message`, a `[c_char; TFT_MESSAGE_LEN]` with
    /// `TFT_MESSAGE_LEN = 256`. `tf_tree_c::error::set_message` truncates at 255
    /// bytes and substitutes `?` for **each non-ASCII byte**, so an em-dash
    /// renders `???` and a long message loses its tail silently.
    ///
    /// **This gate did not exist, and that is how a remedy three times this
    /// budget shipped.** `ArenaHeldButUnreachable`'s four-branch remedy reached
    /// a C operator truncated before the remedy began, em-dashes as `???`.
    /// Measured on #355 rather than inferred; the figures live in `0055` step 6
    /// and in `CHANGELOG.md`, once each, rather than a third time here.
    ///
    /// **Why 220.** 255 bytes are usable (the NUL takes one) and the longest
    /// fixed text a C path puts *before* one of these renderings is the bridge's
    /// `shared arena could not be created: `, 35 bytes
    /// (`tf_tree_c::bridge::generic_failure_message`). The three C-side facts
    /// this rests on — the buffer size, the truncation bound and the `?`
    /// substitution — are pinned by `tf_tree_c`'s
    /// `the_message_buffer_is_the_size_this_crates_budget_assumes`, which is
    /// what keeps the two crates in step in the direction this one cannot see. 255 − 35 = 220. That path
    /// appends `(arena_name {name:?})` afterwards, and the suffix is **not**
    /// subtracted because that function's own doc makes the name the part the
    /// buffer is meant to eat: *"the fixed clause leads, `OpenError`'s unbounded
    /// rendering comes second, and the name … is what the buffer eats into."*
    ///
    /// **A first version of this said 229, from the 26-byte
    /// `could not open the arena: ` wrapper — and that contradicted `0059`,
    /// which this arm cites two paragraphs earlier.** That record's *Rationale*
    /// names the 35-byte bridge prefix as "the longest fixed text a C path puts
    /// before one of these payloads". Taking the shorter wrapper as "the
    /// longest known" was wrong by inspection of a record already in hand.
    ///
    /// The worst message under this budget is currently **205 bytes**
    /// (`NetworkFilesystem` from `$XDG_RUNTIME_DIR`), not the 147 the first
    /// version of this comment claimed — so the real headroom is 15 bytes, and
    /// `0059`'s aspirational 120 is a long way below what these texts are.
    const MESSAGE_BUDGET: usize = 220;

    /// `HandshakeRejected` is over budget and **this step is not chartered to
    /// fix it** — a ratchet, not an exemption.
    ///
    /// Its length is `rejection_advice`, seven per-status remedies concatenated
    /// into the message. That is exactly the pattern `0055` part 4 ends, and
    /// applying it here means writing a `docs/RUNBOOK.md` section for the seven
    /// statuses, which is `0055` step 7 rather than step 6.
    ///
    /// **Per status, because a max-over-statuses ratchet is not a ratchet.** A
    /// first version pinned only the worst (378); growing `HelloStatus::Ok`'s
    /// advice from 112 to 306 bytes — enough to truncate in C — passed it, and
    /// six of seven statuses carried 58–266 bytes of silent headroom. Each
    /// status is now pinned at its own measured length, so any of them growing
    /// fails. When step 7 lands, this table and the exception go with it.
    ///
    /// Four of the seven truncate today at the 26-byte wrapper (239, 253, 320,
    /// 378 against 255), and under this crate's own 220-byte budget all but
    /// `Ok` are over.
    const HANDSHAKE_REJECTED_LENGTHS: [(crate::wire::HelloStatus, usize); 7] = {
        use crate::wire::HelloStatus as H;
        [
            (H::Ok, 112),
            (H::VersionMismatch, 225),
            (H::LayoutMismatch, 253),
            (H::BootIdMismatch, 239),
            (H::NoParticipantSlots, 378),
            (H::ModeNotPermitted, 229),
            (H::Malformed, 320),
        ]
    };

    #[test]
    fn every_ipc_error_message_fits_the_c_abis_buffer() {
        let mut worst = 0usize;
        for e in samples() {
            let text = e.to_string();
            assert!(
                text.is_ascii(),
                "a non-ASCII byte reaches C as `?` per byte: {text}"
            );
            if matches!(e, IpcError::HandshakeRejected { .. }) {
                continue;
            }
            assert!(
                text.len() <= MESSAGE_BUDGET,
                "{} bytes over the {MESSAGE_BUDGET}-byte budget, so a C caller reads a \
                 truncated message: {text}",
                text.len()
            );
            worst = worst.max(text.len());
        }
        // Every variant that exists today is represented: this is what catches a
        // deleted or forgotten `push`, which the exhaustive match cannot.
        let kinds: std::collections::HashSet<_> =
            samples().iter().map(core::mem::discriminant).collect();
        assert_eq!(
            kinds.len(),
            VARIANTS_SAMPLED,
            "samples() covers {} of {VARIANTS_SAMPLED} IpcError variants; a push is missing, \
             or a variant was added and this constant not bumped",
            kinds.len()
        );

        // **The budget must be a real constraint, not headroom nobody uses.**
        // Without this the assertions above would pass just as well against a
        // set of one-word messages, and the gate would say nothing about whether
        // this budget is the right number.
        assert!(
            worst > MESSAGE_BUDGET / 2,
            "the worst message is only {worst} bytes, so this budget is not measuring anything"
        );

        // The ratchet, per status: each is pinned at the length measured on
        // 2026-09-18, so growth anywhere fails rather than only growth of the
        // current worst.
        for (status, pinned) in HANDSHAKE_REJECTED_LENGTHS {
            let text = IpcError::HandshakeRejected {
                status,
                owner_format_version: 3,
                owner_layout_hash: 0x3D10_4195,
            }
            .to_string();
            assert!(text.is_ascii(), "non-ASCII in {status:?}: {text}");
            assert!(
                text.len() <= pinned,
                "{status:?} grew from {pinned} bytes to {}; these are over budget already \
                 and may only shrink, which is `0055` step 7's work",
                text.len()
            );
        }
    }

    /// **The facts each `ArenaHeldButUnreachable` state prints** — the remedy is
    /// `docs/RUNBOOK.md`'s (`0055` part 4, step 6).
    ///
    /// This test used to assert the remedy, per branch, because the message
    /// carried one. It carried three wrong ones in a day, all from the same
    /// category error: the remedy says which *processes* to stop and this type
    /// sees which *bytes* are held. So the remedy left, and what is asserted
    /// here is that each state reports the facts that select the runbook's row —
    /// the mask, the lowest slot and whether it is the creator's, the ownership
    /// byte — and that **no state claims anything about processes**, which is
    /// the rule that makes the old defect unexpressible rather than merely
    /// fixed.
    ///
    /// Length and ASCII are `every_ipc_error_message_fits_the_c_abis_buffer`'s.
    #[test]
    fn every_unreachable_state_reports_the_facts_and_prescribes_nothing() {
        let held = |slots: u64, first: Option<u32>, owned: bool| {
            IpcError::ArenaHeldButUnreachable {
                holder_slots: slots,
                first_slot: first,
                first_pid: 4242,
                ownership_held: owned,
            }
            .to_string()
        };

        // **Every state the four arms can render, not the seven a first version
        // listed.** That list omitted `(0b1000, Some(3), true)` and both
        // `first_slot: None` states with a non-empty mask — and a procedure
        // added to the `first_slot: None` arm passed both gates because of it.
        // Those two are unconstructible through the rendezvous, which derives
        // `first_slot` from the mask (`crates/tf_tree_ipc/src/open.rs`), so no
        // operator meets them; the variant and its fields are `pub`, so the arm
        // is reachable by construction and is swept rather than argued away.
        let states = [
            ("slot 0 alone, ownership free", held(0b1, Some(0), false)),
            ("slot 0 alone, ownership held", held(0b1, Some(0), true)),
            ("slot 0 and others, free", held(0b101, Some(0), false)),
            ("slot 0 and others, held", held(0b101, Some(0), true)),
            ("stranded non-creator, free", held(0b1000, Some(3), false)),
            ("stranded non-creator, held", held(0b1000, Some(3), true)),
            ("nobody attached, held", held(0, None, true)),
            ("nobody attached, nothing held", held(0, None, false)),
            ("no first slot, mask set, free", held(0b1, None, false)),
            ("no first slot, mask set, held", held(0b1, None, true)),
            ("widest ids", held(u64::MAX, Some(u32::MAX), true)),
        ];

        for (state, message) in &states {
            // **The rule, as a negative, and it took two attempts to state.**
            // A first version forbade the word "process" outright and failed on
            // the empty-mask arm's "a process that never served" — which is a
            // *fact*: the ownership byte was held throughout and nothing
            // answered the socket. What must be forbidden is narrower and is
            // exactly what went wrong three times:
            //
            //   * **a count of holders.** `Display` sees two bits and cannot
            //     tell one process holding both bytes from two holding one
            //     each, so it may not say which it is;
            //   * **a procedure.** What to stop, and which policy or builder to
            //     use, belong to `RUNBOOK.md`, whose reader can see the
            //     processes.
            // **Case-folded, because the defect this forbids was capitalised.**
            // The shipped text read "… Stop the process holding slot 0"; a
            // lowercase-only `contains` let exactly that wording back in with
            // all 98 tests green. Measured, not supposed.
            let lower = message.to_ascii_lowercase();
            for forbidden in [
                "same process",
                "second process",
                "one process",
                "two processes",
                "stop",
                "createpolicy",
                "attachmode",
                "layout_if_creating",
            ] {
                assert!(
                    !lower.contains(forbidden),
                    "{state}: {forbidden:?} is a count or a procedure, and this type has \
                     neither to offer: {message}"
                );
            }
            // (g): the runbook's search key is how a reader gets from the message
            // to the remedy, so it is the one thing every arm must end with.
            assert!(
                message.ends_with("(ArenaHeldButUnreachable)"),
                "{state}: must end with the search key: {message}"
            );
        }

        // The facts, per state. These are exactly the columns `RUNBOOK.md`'s
        // table is indexed by, so a message that drops one leaves a reader
        // unable to find their row.
        //
        // **Looked up by label, not by index.** A first version indexed
        // `states` positionally and broke the moment three states were added to
        // close a coverage hole — silently pointing each assertion at a
        // different state than its text claimed.
        // `assert!` rather than `expect`/`panic!`: the workspace denies
        // `clippy::panic`, `expect_used` and `unwrap_used`, in test code too.
        let of = |label: &str| -> String {
            let found = states.iter().find(|(l, _)| *l == label);
            assert!(found.is_some(), "no state labelled {label:?}");
            found.map(|(_, m)| m.clone()).unwrap_or_default()
        };
        let free_alone = of("slot 0 alone, ownership free");
        assert!(free_alone.contains("participant bytes 0x1 held"));
        assert!(free_alone.contains("lowest slot 0 (pid 4242, the creator's)"));
        assert!(free_alone.contains("ownership byte free"));
        assert!(of("slot 0 alone, ownership held").contains("ownership byte held"));
        assert!(of("slot 0 and others, free").contains("participant bytes 0x5 held"));
        let stranded = of("stranded non-creator, free");
        assert!(stranded.contains("lowest slot 3"));
        assert!(
            !stranded.contains("the creator's"),
            "slot 3 is not the creator's slot: {stranded}"
        );
        assert!(of("nobody attached, held").contains("nobody attached"));
        assert!(of("nobody attached, nothing held").contains("retry"));

        // **The pid is carried, and it is the one identifying fact the message
        // may state**, because it is read from the identity record rather than
        // inferred. `0055` part 4's prescription names it explicitly.
        assert!(
            free_alone.contains("4242"),
            "the first slot's pid is a fact, not an inference: {free_alone}"
        );

        // **The widest ids are what the budget has to survive, and a sample at
        // 0x5 / pid 4242 does not measure them.** A 64-bit mask and two 32-bit
        // ids are up to 38 bytes of digits on their own.
        let widest = of("widest ids");
        assert!(
            widest.contains("0xffffffffffffffff") && widest.contains("4294967295"),
            "the widest ids must render in full rather than being abbreviated: {widest}"
        );
    }

    #[test]
    fn wire_and_proc_parse_causes_render_as_prose() {
        let wire = [
            (
                WireError::BadLength {
                    got: 3,
                    expected: 56,
                },
                "BadLength",
            ),
            (WireError::BadMagic, "BadMagic"),
            (WireError::BadMode { got: 9 }, "BadMode"),
        ];
        for (e, name) in wire {
            let shown = IpcError::HandshakeMalformed(e).to_string();
            assert!(
                !shown.contains(name) && !shown.contains('{'),
                "{shown:?} is a Debug dump"
            );
        }
        let shown = IpcError::HandshakeMalformed(WireError::BadLength {
            got: 3,
            expected: 56,
        })
        .to_string();
        assert!(shown.contains('3') && shown.contains("56"), "{shown:?}");

        let causes = [
            (ProcParseError::NoClosingParen, "NoClosingParen"),
            (ProcParseError::TooFewFields, "TooFewFields"),
            (ProcParseError::NotAnInteger, "NotAnInteger"),
        ];
        for (cause, name) in causes {
            let shown = ProcError::Parse { pid: 7, cause }.to_string();
            assert!(!shown.contains(name), "{shown:?} is a Debug dump");
            assert!(shown.starts_with("/proc/7/stat"), "{shown:?}");
        }
    }
}
