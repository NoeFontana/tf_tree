//! `Copy`, `String`-free errors that name what failed.
//!
//! Integers and enums, never an allocation (`docs/PROJECT.md` §5). Every variant
//! names both sides of whatever disagreed, or the variable / slot / errno
//! responsible (`docs/PHASE2.md` §3.7).

use crate::WireError;
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
    /// Empty.
    Empty,
    /// Longer than [`crate::MAX_NAME_LEN`].
    TooLong,
    /// Contains `/` or a NUL, or is `.`/`..`; the name is one path component.
    NotOneComponent,
    /// Not UTF-8.
    NotUtf8,
}

/// Which lock-file role a failing operation was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockRole {
    /// Byte 0 — ownership. Its holder serves the socket.
    Ownership,
    /// Byte 1 — A2's topology mutation lock, held for one `Tree::reparent`.
    Topology,
    /// Byte `16 + i` — participant liveness for slot `i`.
    Participant(u32),
    /// A per-edge claim lease (`docs/PHASE2.md` §6.1); the edge id, not a slot.
    Claim(u32),
}

/// A `/proc/<pid>/stat` parse failure, split out from the read failure so a
/// malformed line is never confused with an exited process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcParseError {
    /// No `)`, so `comm` cannot be delimited (see [`crate::parse_start_time`]).
    NoClosingParen,
    /// The line ends before field 22 (`starttime`), counting `comm` as field 2.
    TooFewFields,
    /// Field 22 is not a decimal integer.
    NotAnInteger,
}

/// Reading a process's identity out of `/proc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcError {
    /// `/proc/<pid>/stat` could not be read — usually the process is gone.
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
    /// `/proc/sys/kernel/random/boot_id` was unreadable or not a UUID.
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
    /// Only checked for [`RuntimeDirSource::Tmp`] (world-writable parent);
    /// `docs/PHASE2.md` §3.10 scopes trust to same-user processes.
    RuntimeDirForeignOwner {
        /// The directory's owner.
        owner_uid: u32,
        /// This process's uid.
        our_uid: u32,
    },
    /// `statfs` failed, so the NORMATIVE §3.1 network-filesystem check could not
    /// be performed; refusing is the safe answer.
    StatFsFailed {
        /// Which candidate produced the directory.
        source: RuntimeDirSource,
        /// `errno`.
        raw_os_error: i32,
    },
    /// The runtime directory is on NFS or CIFS.
    ///
    /// NORMATIVE refusal (`docs/PHASE2.md` §3.1): network file locks are not
    /// released promptly on client death.
    NetworkFilesystem {
        /// Which candidate produced the directory.
        source: RuntimeDirSource,
        /// `statfs.f_type`, so the message can name the filesystem.
        magic: u64,
    },
    /// A domain variable was set to something that is not a `u32`.
    ///
    /// Fatal, never a fallback to domain 0: a typo must not land on the wrong arena.
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
    /// The lock file (created on demand, mode `0600`) could not be opened.
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
    /// `fcntl(F_OFD_SETLK)`/`F_OFD_GETLK` failed for a reason other than contention.
    LockFailed {
        /// Which byte.
        role: LockRole,
        /// `errno`.
        errno: Errno,
    },
    /// An edge id that cannot be addressed within the reserved claim region.
    ///
    /// Only reachable from a corrupt header. Bounded because a claim byte
    /// colliding with an identity record would hand one edge to two writers.
    ClaimOutOfRange {
        /// The edge asked for.
        edge: u32,
        /// The number of claim bytes reserved.
        limit: u64,
    },
    /// Every participant slot is locked, so this process cannot register.
    ///
    /// The limit is a build constant ([`crate::MAX_PARTICIPANTS`]).
    NoParticipantSlots {
        /// The current limit.
        limit: u32,
    },
    /// Nothing was serving, nothing was alive, and the caller asked for
    /// [`crate::CreatePolicy::Never`].
    ///
    /// For a consumer that must not create an empty arena.
    ArenaAbsent,
    /// The §3.7 socket path exceeds `sun_path`.
    ///
    /// Reachable from configuration; reported at construction, not as `EINVAL`.
    SocketPathTooLong {
        /// Length of the path.
        len: usize,
        /// The kernel's limit.
        limit: usize,
    },
    /// Nothing is listening on the §3.7 socket.
    ///
    /// Not a failure by itself: §3.9 makes a stale socket path expected, so
    /// `open()` lets the ownership byte decide.
    ServerUnreachable {
        /// `connect` errno.
        raw_os_error: i32,
    },
    /// This process could not set up its own socket to attach with.
    ///
    /// **Distinct from [`IpcError::HandshakeIo`]**, which a probe reads as "no
    /// server": `EMFILE` here must not create a second arena beside a live one.
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
    /// `recvmsg` returned zero bytes: the owner was there at `accept(2)` and gone
    /// before its `sendmsg`. (A never-accepted connection gives `ECONNRESET`, as
    /// [`IpcError::HandshakeIo`]; [`crate::SocketProbe`] treats both as absent.)
    ///
    /// **Distinct from [`IpcError::HandshakeMalformed`]**: an empty datagram is
    /// `BadLength { got: 0 }`, terminal in §3.4, but this must be retried
    /// (`docs/decisions/0005`'s client-reachability table).
    HandshakeClosed,
    /// The peer's datagram was not a well-formed handshake message.
    ///
    /// **A protocol violation, and therefore terminal.** An owner that merely
    /// went away mid-handshake is [`IpcError::HandshakeClosed`] instead.
    HandshakeMalformed(crate::wire::WireError),
    /// The owner refused this client, and named its own side of the comparison.
    ///
    /// The message states the status and the owner's two numbers and prescribes
    /// nothing (`0055` step 7); remedies are
    /// [`docs/RUNBOOK.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/RUNBOOK.md)'s
    /// `HandshakeRejected` section, reached by the trailing `(HandshakeRejected)`
    /// key ([`0059`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0059-the-arena-errors-that-cannot-describe-themselves.md)
    /// convention (g)).
    ///
    /// Why only the owner's numbers: `docs/PHASE2.md` §3.7, Erratum.
    HandshakeRejected {
        /// Why.
        status: crate::wire::HelloStatus,
        /// The **owner's** format version; the client's is not carried
        /// (`docs/PHASE2.md` §3.7, Erratum).
        owner_format_version: u32,
        /// The **owner's** layout hash.
        owner_layout_hash: u32,
    },
    /// The owner *rejected* the attach but sent a segment fd anyway.
    ///
    /// A protocol violation (§3.7: a rejection carries no fd), reported so a
    /// client cannot map a segment it was refused.
    RejectionCarriedFd {
        /// The status the owner sent alongside the fd.
        status: crate::wire::HelloStatus,
    },
    /// The owner accepted but sent no `SCM_RIGHTS` fd.
    ///
    /// A protocol violation, not an I/O failure; not retried as "no server".
    NoFdReceived,
    /// A live arena exists — some participant still holds its lock byte — but
    /// nothing is serving it, and nobody took over before the deadline.
    ///
    /// The §3.4 timeout. Refusing is correct: creating a second arena beside one
    /// in use diverges silently. Identity records: [`crate::LockFile::read_identity`].
    ArenaHeldButUnreachable {
        /// Bitmask of held participant slots; zero means the *ownership* byte was
        /// held by a process that never began serving.
        holder_slots: u64,
        /// The lowest held slot, or `None` when no participant byte is held.
        first_slot: Option<u32>,
        /// The pid in that slot's identity record, or `0` if it was never
        /// written. Advisory (§5.1): the lock is the liveness, this is the name.
        first_pid: u32,
        /// Whether the **ownership** byte was held by somebody else at the
        /// deadline (one `F_OFD_GETLK`, so advisory). It separates the two
        /// remedies: a forced create ([`crate::CreatePolicy::Always`]) must take
        /// it first, so with `true` it cannot help (`docs/PHASE2.md` §3.4).
        ownership_held: bool,
    },
    /// A `/proc` read needed for an identity record failed.
    Proc(ProcError),
}

impl IpcError {
    /// The errno of a [`std::io::Error`], `0` if none.
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
            // Inline, not a `Display` on `WireError`: a trait impl on a published
            // type is a commitment.
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
            // Facts only, with the variant name as the runbook's search key.
            IpcError::HandshakeRejected {
                status,
                owner_format_version,
                owner_layout_hash,
            } => write!(
                f,
                "the arena owner refused this attach: {status:?} \
                 (owner format_version {owner_format_version}, \
                 layout_hash 0x{owner_layout_hash:08X}) (HandshakeRejected)"
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
            // These arms state facts and end with the variant name in parentheses
            // as the runbook's search key (`0055` part 4, step 6; `0059` convention
            // (g)); the remedy is `docs/RUNBOOK.md`'s because this type sees bytes,
            // not processes. Convention (e) (120 bytes) is not met; `MESSAGE_BUDGET`
            // binds instead. Keep them ASCII and short: the C boundary truncates at
            // 255 bytes and substitutes `?` per non-ASCII byte.
            IpcError::ArenaHeldButUnreachable {
                holder_slots: 0,
                ownership_held: true,
                ..
            } => f.write_str(
                "nobody attached; the ownership byte was held for the whole open timeout \
                 by a process that never served; nothing created (ArenaHeldButUnreachable)",
            ),
            // Empty mask but ownership free at the deadline: a holder let go
            // between the two reads.
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

    /// Every `HelloStatus`; safe Rust cannot enumerate an enum, and
    /// [`status_is_a_refusal`] is the compile error that flags an addition.
    const ALL_STATUSES: [crate::wire::HelloStatus; 7] = {
        use crate::wire::HelloStatus as H;
        [
            H::Ok,
            H::VersionMismatch,
            H::LayoutMismatch,
            H::BootIdMismatch,
            H::NoParticipantSlots,
            H::ModeNotPermitted,
            H::Malformed,
        ]
    };

    /// Is this status a refusal, i.e. does it need a `docs/RUNBOOK.md` row?
    ///
    /// Total by construction: a new `HelloStatus` fails to compile here (under
    /// `--all-targets`), and its author owes an entry to [`ALL_STATUSES`] and a
    /// runbook row. `tf_tree_cli`'s `tests/runbook.rs` derives the set from
    /// `HelloStatus::from_u32` and fails until the row exists.
    fn status_is_a_refusal(status: crate::wire::HelloStatus) -> bool {
        use crate::wire::HelloStatus as H;
        match status {
            // The acceptance: no error is built from it.
            H::Ok => false,
            H::VersionMismatch
            | H::LayoutMismatch
            | H::BootIdMismatch
            | H::NoParticipantSlots
            | H::ModeNotPermitted
            | H::Malformed => true,
        }
    }

    /// Every [`IpcError`] variant, and every value its `Display` branches on,
    /// for the length and ASCII gates.
    ///
    /// The exhaustive `match` at the end forces a new variant to be handled but
    /// not pushed; `VARIANTS_SAMPLED` catches a deleted `push`. Neither forces
    /// bumping `VARIANTS_SAMPLED` for a new variant, so that is a review rule.
    const VARIANTS_SAMPLED: usize = 24;
    ///
    /// One sample per variant is not enough (mutants in unsampled
    /// `ArenaHeldButUnreachable` branches passed), so each field the formatter
    /// switches on is swept, and the ids are also sampled at their widest
    /// (`u64::MAX`, `u32::MAX`), which the budget counts.
    fn samples() -> Vec<IpcError> {
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
        let statuses = ALL_STATUSES;
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
            // Realistic and widest: the budget is a length.
            for (owner_format_version, owner_layout_hash) in
                [(3, 0x3D10_4195), (u32::MAX, u32::MAX)]
            {
                out.push(IpcError::HandshakeRejected {
                    status,
                    owner_format_version,
                    owner_layout_hash,
                });
            }
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
        // `ArenaHeldButUnreachable`'s arms, including `first_slot: None` with a
        // non-empty mask, which no other test constructs.
        for (holder_slots, first_slot, first_pid, ownership_held) in [
            // Widest, `first_pid` included; slot 0 is widest of all (`, the
            // creator's` outweighs a wide slot's digits).
            (u64::MAX, Some(0u32), u32::MAX, true),
            (u64::MAX, Some(u32::MAX), u32::MAX, true),
            (u64::MAX, Some(u32::MAX), u32::MAX, false),
            (0u64, None, 4242, true),
            (0, None, 4242, false),
            (0b1, Some(0), 4242, false),
            (0b1, Some(0), 4242, true),
            (0b101, Some(0), 4242, false),
            (0b101, Some(0), 4242, true),
            (0b1000, Some(3), 4242, false),
            (0b1000, Some(3), 4242, true),
            (0b1, None, 4242, false),
            (0b1, None, 4242, true),
        ] {
            out.push(IpcError::ArenaHeldButUnreachable {
                holder_slots,
                first_slot,
                first_pid,
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

    /// Every message must survive the C ABI's 256-byte `tft_error::message`
    /// (`0055` step 6; `0059` convention (g)), which truncates at 255 bytes and
    /// substitutes `?` per non-ASCII byte.
    ///
    /// 220 = 255 minus the 35-byte `shared arena could not be created: ` lead
    /// (`tf_tree_c::bridge::generic_failure_message`); the trailing name suffix
    /// is deliberately not subtracted. `tf_tree_c`'s
    /// `the_message_buffer_is_the_size_this_crates_budget_assumes` pins the
    /// C-side numbers. Figures: `0055` step 6.
    const MESSAGE_BUDGET: usize = 220;

    #[test]
    fn every_ipc_error_message_fits_the_c_abis_buffer() {
        let mut worst = 0usize;
        for e in samples() {
            let text = e.to_string();
            assert!(
                text.is_ascii(),
                "a non-ASCII byte reaches C as `?` per byte: {text}"
            );
            assert!(
                text.len() <= MESSAGE_BUDGET,
                "{} bytes over the {MESSAGE_BUDGET}-byte budget, so a C caller reads a \
                 truncated message: {text}",
                text.len()
            );
            worst = worst.max(text.len());
        }
        // Catches a deleted `push`, which the exhaustive match cannot.
        let kinds: std::collections::HashSet<_> =
            samples().iter().map(core::mem::discriminant).collect();
        assert_eq!(
            kinds.len(),
            VARIANTS_SAMPLED,
            "samples() covers {} of {VARIANTS_SAMPLED} IpcError variants; a push is missing, \
             or a variant was added and this constant not bumped",
            kinds.len()
        );

        // The budget must be a real constraint, not unused headroom.
        assert!(
            worst > MESSAGE_BUDGET / 2,
            "the worst message is only {worst} bytes, so this budget is not measuring anything"
        );
    }

    /// A rejection is facts, and facts have a width: [`0059`] convention (e) asks
    /// for 120 bytes; these carry the owner's two numbers (up to ten digits each).
    ///
    /// `PHASE2.md` §3.7 asks for both sides' values and this arm prints one
    /// (`0055` step 7): this crate cannot read `FORMAT_VERSION` or `layout_hash()`.
    /// Slack runs from 7 bytes (`NoParticipantSlots`) to 16 (`Malformed`) over the
    /// statuses a library can send; `Ok` is only reachable by hand-building the
    /// variant and is excluded from this bound. A short clause on a short status
    /// can still slip past; `tf_tree_cli`'s `runbook.rs` forbidden-word rule and
    /// review catch that.
    ///
    /// [`0059`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0059-the-arena-errors-that-cannot-describe-themselves.md
    const REJECTION_BUDGET: usize = 140;

    /// A rejection reports the status it got, prescribes nothing (`0055` step 7),
    /// and **never names a status it did not get**. Covers both variants that
    /// carry a `HelloStatus`; `RejectionCarriedFd` still lacks `0059` convention
    /// (g)'s search key (owed, `0055` step 7).
    #[test]
    fn both_rejection_arms_name_only_the_status_they_carry() {
        // Keeps `status_is_a_refusal` attached to something that runs.
        assert_eq!(
            ALL_STATUSES
                .into_iter()
                .filter(|s| status_is_a_refusal(*s))
                .count(),
            ALL_STATUSES.len() - 1,
            "exactly one `HelloStatus` is not a refusal"
        );
        // The count alone survives an inverted predicate.
        assert!(
            !status_is_a_refusal(crate::wire::HelloStatus::Ok),
            "`Ok` is the acceptance: no error is built from it and it has no runbook row"
        );

        for status in ALL_STATUSES {
            // A leading-zero nibble, so `{:08X}` is distinguishable from `{:X}`;
            // `tf_tree doctor --explain-version` prints `0x{h:08X}`.
            let text = IpcError::HandshakeRejected {
                status,
                owner_format_version: u32::MAX,
                owner_layout_hash: 0x0D10_4195,
            }
            .to_string();

            assert!(
                text.contains(&format!("{status:?}")),
                "a rejection that does not name its status: {text}"
            );
            // Each number with its label: two bare `contains` pass on unlabelled digits.
            assert!(
                text.contains("owner format_version 4294967295"),
                "the owner's format_version is missing or unlabelled: {text}"
            );
            assert!(
                text.contains("layout_hash 0x0D104195"),
                "the owner's layout_hash is missing or unlabelled: {text}"
            );
            assert!(
                text.ends_with("(HandshakeRejected)"),
                "no runbook search key: {text}"
            );
            assert!(
                text.len() <= REJECTION_BUDGET,
                "{status:?} renders {} bytes against a {REJECTION_BUDGET}-byte budget; \
                 prose has come back to a message a C caller reads 255 bytes of: {text}",
                text.len()
            );

            let carried = IpcError::RejectionCarriedFd { status }.to_string();
            assert!(
                carried.contains(&format!("{status:?}")),
                "a carried-fd rejection that does not name its status: {carried}"
            );

            // No other status's name. The status's own is removed first so a name
            // extending another (`LayoutMismatchV2`) cannot false-fail.
            for rendering in [&text, &carried] {
                let without_its_own = rendering.replacen(&format!("{status:?}"), "", 1);
                for other in ALL_STATUSES {
                    assert!(
                        other == status || !without_its_own.contains(&format!("{other:?}")),
                        "a {status:?} rejection names {other:?}, which is the defect that \
                         cost a rebuild before anyone reread the word in front of it: \
                         {rendering}"
                    );
                }
            }
        }
    }

    /// A status this build cannot receive needs no runbook row. `HelloStatus` is
    /// `#[non_exhaustive]`, so downstream cannot have a compile error; the harm
    /// is only when the codec can deliver it, because `from_u32` folds every
    /// unnamed code onto `Malformed`. This guards that addition: the failure
    /// names what is owed.
    #[test]
    fn a_status_this_build_cannot_receive_needs_no_row() {
        use crate::wire::HelloStatus as H;
        let mut v = 0u32;
        for status in ALL_STATUSES {
            assert_eq!(status.as_u32(), v, "{status:?} is not wire value {v}");
            assert_eq!(H::from_u32(v), status, "wire value {v} lost its status");
            v += 1;
        }

        // Nothing above forces `ALL_STATUSES` to be complete (a gapped value would
        // pass), so walk the codec over every `u16`; a status beyond that would
        // escape, and wire discriminants are far below it.
        let mut delivered: Vec<H> = Vec::new();
        for probe in 0..=u32::from(u16::MAX) {
            let status = H::from_u32(probe);
            if !delivered.contains(&status) {
                delivered.push(status);
            }
        }
        assert_eq!(
            delivered.len(),
            ALL_STATUSES.len(),
            "the codec delivers {} statuses and ALL_STATUSES names {}: {delivered:?}. A \
             status was added to `HelloStatus` and `from_u32` without being added here, \
             so nothing renders it, nothing measures it against the budget, and \
             docs/RUNBOOK.md owes it a row",
            delivered.len(),
            ALL_STATUSES.len()
        );

        // `v` is the first value a new status would take.
        assert_eq!(
            H::from_u32(v),
            H::Malformed,
            "the first unused wire value decodes to a status of its own, so `HelloStatus` \
             grew and the codec can now deliver it: give it an entry in ALL_STATUSES \
             here, an arm in `status_is_a_refusal`, and a row in docs/RUNBOOK.md's \
             `HandshakeRejected` section. tf_tree_cli's tests/runbook.rs needs no \
             list — it derives one from this codec — and it is what fails until the \
             row exists"
        );
    }

    /// Each `ArenaHeldButUnreachable` state reports the facts that select the
    /// runbook's row (mask, lowest slot and whether it is the creator's,
    /// ownership byte) and **claims nothing about processes** (`0055` part 4,
    /// step 6): this type sees bytes, not processes.
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
        // Widest needs `first_pid` at its maximum too.
        let held_wide = |slots: u64, first: Option<u32>, owned: bool| {
            IpcError::ArenaHeldButUnreachable {
                holder_slots: slots,
                first_slot: first,
                first_pid: u32::MAX,
                ownership_held: owned,
            }
            .to_string()
        };

        // Every state the four arms can render. `first_slot: None` with a mask is
        // unconstructible via the rendezvous but reachable by construction.
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
            ("widest ids", held_wide(u64::MAX, Some(u32::MAX), true)),
            ("widest of all", held_wide(u64::MAX, Some(0), true)),
        ];

        for (state, message) in &states {
            // Forbid a count of holders and a procedure, case-folded ("Stop the
            // process ..." shipped in capitals). Not the word "process": the
            // empty-mask arm's "a process that never served" is a fact.
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
            // (g): every arm ends with the runbook's search key.
            assert!(
                message.ends_with("(ArenaHeldButUnreachable)"),
                "{state}: must end with the search key: {message}"
            );
        }

        // The facts, per state: the columns `RUNBOOK.md`'s table is indexed by,
        // looked up by label rather than index.
        // `assert!`: the workspace denies `panic`/`expect_used`/`unwrap_used` in tests.
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

        // The pid is read from the identity record, not inferred (`0055` part 4).
        assert!(
            free_alone.contains("4242"),
            "the first slot's pid is a fact, not an inference: {free_alone}"
        );

        // Each widest id is checked on its own: one `contains` conjunction passes
        // if either field is wide.
        let widest = of("widest ids");
        assert!(
            widest.contains("0xffffffffffffffff"),
            "the mask must render in full: {widest}"
        );
        assert!(
            widest.contains("lowest slot 4294967295"),
            "the slot must render in full: {widest}"
        );
        assert!(
            widest.contains("pid 4294967295"),
            "the pid must render in full, and it is the field that was left at 4242 \
             under this very label: {widest}"
        );
        let widest_of_all = of("widest of all");
        assert!(
            widest_of_all.len() > widest.len(),
            "slot 0's `, the creator's` must cost more than a wide slot's digits, or the \
             worst case this budget is sized against is the other state: \
             {widest_of_all}"
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
