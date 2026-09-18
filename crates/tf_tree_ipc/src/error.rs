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
    ///
    /// **The message states the status and the owner's two numbers, and
    /// prescribes nothing** (`0055` step 7). What to do about each status is
    /// [`docs/RUNBOOK.md`](https://github.com/NoeFontana/tf_tree/blob/main/docs/RUNBOOK.md)'s `HandshakeRejected`
    /// section, one row per status, and `(HandshakeRejected)` at the end of the
    /// rendering is the search key that reaches it
    /// ([`0059`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0059-the-arena-errors-that-cannot-describe-themselves.md)
    /// convention (g)).
    ///
    /// **Two measurements put the remedy there rather than here, and the second
    /// is why the first one's fix was not enough.** The message once appended a
    /// single sentence about `LayoutMismatch` to *every* rejection, so a torture
    /// run that exhausted the participant table printed, thousands of times:
    ///
    /// ```text
    /// the arena owner refused this attach: NoParticipantSlots (owner format_version 3,
    /// layout_hash 0x3D104195). A LayoutMismatch means this binary was built against a
    /// different record layout than the running arena — rebuild both from the same source
    /// ```
    ///
    /// The status is right there and it is not `LayoutMismatch`, but the advice
    /// is the longest and last thing on the line, so it reads as the diagnosis
    /// and costs a rebuild before anyone rereads the word in front of it. That
    /// was repaired with one arm per status — and the *messages* those arms
    /// produced were 112 to 378 bytes (the arms themselves 22 to 272), while
    /// `tft_error::message` is 256 and `set_message` truncates at 255, so
    /// **four of the seven statuses reached a C operator cut off mid-remedy** —
    /// four on `tft_tree_open_named`'s path, whose wrapper is 26 bytes, and six
    /// on the bridge's 35-byte one, which `MESSAGE_BUDGET`'s doc below records
    /// as the longest a C path puts in front. A count of truncations is a
    /// statement about a prefix, and this one used to name none.
    /// It is the message length the buffer sees, which is why the figures
    /// quoted anywhere are renderings and not arms. A per-status remedy that C cannot finish reading is the
    /// same defect one layer down, which is what a runbook row does not have.
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
            // Facts only, and the variant name as the runbook's search key: the
            // seven per-status remedies this arm used to carry are that
            // section's rows. Why they left is on the variant.
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

    /// Every `HelloStatus`, written out because safe Rust cannot enumerate an
    /// enum — and [`status_is_a_refusal`] is the compile error that says so
    /// when one is added.
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

    /// Is this status a refusal — that is, does it need a `docs/RUNBOOK.md`
    /// row?
    ///
    /// **Total by construction, and that is its whole job.** `rejection_advice`
    /// used to be the place a new `HelloStatus` broke the build, and step 7
    /// deleted it; `HelloStatus::from_u32`'s `_` arm cannot replace it, because
    /// a variant added without touching the codec compiles clean. So the prompt
    /// lives here: adding one fails to compile, and the author who fixes this
    /// match owes an entry to [`ALL_STATUSES`] and a row to the runbook's
    /// table.
    ///
    /// **`tf_tree_cli`'s `tests/runbook.rs` needs nothing added and will tell
    /// you about the row.** It has no status list: it derives the set from
    /// `HelloStatus::from_u32`, so the new status appears there as soon as the
    /// codec can deliver it and the missing row fails `just shm-check`. *This
    /// doc said the opposite — that the downstream crate could not detect an
    /// addition because `HelloStatus` is `#[non_exhaustive]`. That is true of a
    /// `match` and false of enumeration, and it was refuted two rounds after it
    /// was written; a reader who believed it had no reason to expect the row to
    /// be caught, which is exactly the step they would skip.*
    ///
    /// It is in `#[cfg(test)]`, so `cargo check -p tf_tree_ipc` still passes and
    /// `--all-targets` is what fails. `just build` and `just lint` pass it.
    fn status_is_a_refusal(status: crate::wire::HelloStatus) -> bool {
        use crate::wire::HelloStatus as H;
        match status {
            // The acceptance: no error is built from it, so it has no row.
            H::Ok => false,
            H::VersionMismatch
            | H::LayoutMismatch
            | H::BootIdMismatch
            | H::NoParticipantSlots
            | H::ModeNotPermitted
            | H::Malformed => true,
        }
    }

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
    ///
    /// **And the widths a formatter does not branch on but a budget counts**:
    /// the ids that carry one are sampled at `u64::MAX` / `Some(u32::MAX)`, and
    /// `HandshakeRejected`'s two owner numbers at `u32::MAX` beside their
    /// realistic values. A `format_version` of `3` renders one digit where the
    /// type renders ten.
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
            // Both the realistic numbers and the widest ones. `Display` does
            // not branch on either, but the budget is a length, and a
            // `format_version` of 3 is nine digits short of what a `u32` can
            // render.
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
        // `ArenaHeldButUnreachable`'s own arms: the two empty-mask states, the
        // `Some(slot)` arm with slot 0 and with a joiner's slot, both ownership
        // readings, and the `first_slot: None` arm with a non-empty mask, which
        // no other test constructs.
        for (holder_slots, first_slot, first_pid, ownership_held) in [
            // The widest the arm can render, and **`first_pid` is part of it**:
            // a first version of this swept the mask and the slot to their
            // maxima and left the pid at 4242, six digits short, so the figure
            // it measured was not the worst case it was quoted as. The widest
            // of all is slot **0**, whose `, the creator's` costs more than the
            // nine digits a wide slot adds.
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
    }

    /// **A rejection is facts, and facts have a width.** [`0059`]'s convention
    /// (e) asks for 120 bytes; these carry the owner's `format_version` and
    /// `layout_hash`, and a `u32` renders ten digits where a plausible
    /// `format_version` renders one.
    ///
    /// **`PHASE2.md` §3.7 asks for *both* sides' values and this arm prints
    /// one**, which is a divergence older than `0055` step 7 and is recorded
    /// there rather than fixed here. `tf_tree_ipc` depends on `rustix` and
    /// `libc` and nothing else, so it cannot read this build's `FORMAT_VERSION`
    /// or `layout_hash()`: printing both needs two more fields on the variant
    /// or a dependency edge, and either is a change to a published crate's
    /// surface rather than a reduction. An earlier revision of this doc cited
    /// §3.7 as though the arm satisfied it. This is that convention plus the digits,
    /// and the slack it leaves is single digits — so a clause of prose
    /// returning to this arm fails here long before it reaches
    /// `MESSAGE_BUDGET`, which has room for a paragraph.
    ///
    /// [`0059`]: https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0059-the-arena-errors-that-cannot-describe-themselves.md
    const REJECTION_BUDGET: usize = 140;

    /// **A rejection reports the status it got and prescribes nothing**
    /// (`0055` step 7) — and, above all, **never names a status it did not
    /// get**.
    ///
    /// That last rule is the shipped defect made unexpressible rather than
    /// fixed. One sentence about `LayoutMismatch` was once appended to every
    /// rejection, so a torture run that exhausted the participant table printed
    /// a `NoParticipantSlots` refusal whose longest, last clause explained a
    /// layout mismatch; the repair was one arm per status, and *those* were 112
    /// to 378 bytes against a 256-byte C buffer. Both defects are refused here:
    /// the prose is gone, and no rendering may contain another status's name.
    ///
    /// Length and ASCII are `every_ipc_error_message_fits_the_c_abis_buffer`'s,
    /// which this variant is no longer excepted from.
    #[test]
    fn every_rejection_names_only_the_status_it_carries() {
        // Six of the seven are refusals, and the odd one out is the acceptance.
        // Cheap, and it is what keeps `status_is_a_refusal` — the compile error
        // a new `HelloStatus` meets — attached to something that runs.
        assert_eq!(
            ALL_STATUSES
                .into_iter()
                .filter(|s| status_is_a_refusal(*s))
                .count(),
            ALL_STATUSES.len() - 1,
            "exactly one `HelloStatus` is not a refusal"
        );
        // **And it is the acceptance — which the count above does not say.**
        // Inverting the predicate leaves the count at six and this file green,
        // and the predicate is what the runbook and this module both name as
        // the answer to *which statuses need a row*.
        assert!(
            !status_is_a_refusal(crate::wire::HelloStatus::Ok),
            "`Ok` is the acceptance: no error is built from it and it has no runbook row"
        );

        for status in ALL_STATUSES {
            // **A leading-zero nibble, deliberately.** The live
            // `layout_hash()` starts `0x3D`, so a sample carrying it cannot
            // tell `{:08X}` from `{:X}` — measured, with the width spec
            // dropped and every test green. The zero-padding is load-bearing:
            // `tf_tree doctor --explain-version` prints `0x{h:08X}`, and the
            // runbook's `LayoutMismatch` row sends an operator to compare
            // exactly those two renderings.
            let text = IpcError::HandshakeRejected {
                status,
                owner_format_version: u32::MAX,
                owner_layout_hash: 0x0D10_4195,
            }
            .to_string();

            // The facts: which status, the owner's two numbers, and the search
            // key that reaches the runbook (`0059` convention (g)).
            assert!(
                text.contains(&format!("{status:?}")),
                "a rejection that does not name its status: {text}"
            );
            // **Each number with its label, and not one `contains` for both.**
            // A conjunction of two bare `contains` is satisfied by two
            // unlabelled digits: rewriting the arm as `(owner {n}, 0x{h:08X})`
            // — which leaves an operator holding two numbers with no way to
            // tell which is which — passed this assertion. That is the same
            // anti-pattern this file splits apart for the widest
            // `ArenaHeldButUnreachable` ids, and it was here at the same time.
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

            // No other status's name, ever.
            //
            // **The status's own name is removed first, rather than skipped in
            // the loop.** A future status whose name extends an existing one —
            // `LayoutMismatchV2` — makes the longer one's correct rendering
            // contain the shorter one's name, and a bare `contains` would fail
            // a message that is right. Cutting the name this rendering is
            // *supposed* to carry leaves exactly the question being asked.
            let without_its_own = text.replacen(&format!("{status:?}"), "", 1);
            for other in ALL_STATUSES {
                assert!(
                    other == status || !without_its_own.contains(&format!("{other:?}")),
                    "a {status:?} rejection names {other:?}, which is the defect that \
                     cost a rebuild before anyone reread the word in front of it: {text}"
                );
            }
        }
    }

    /// **A status this build cannot receive needs no runbook row**, which is
    /// what closes the gap `status_is_a_refusal` leaves.
    ///
    /// That match is a compile error *in this crate*, and nothing downstream can
    /// have one — `HelloStatus` is `#[non_exhaustive]` on purpose. So a variant
    /// could in principle be added, that one match fixed, and
    /// `tf_tree_cli`'s list and the runbook's table left behind. The reason that
    /// is harmless is on the wire rather than in the type: a joining client's
    /// status comes from `HelloResponse::from_bytes`, therefore from
    /// `HelloStatus::from_u32`, which folds every code it has no name for onto
    /// `Malformed`. A variant that codec cannot produce never reaches a client,
    /// so no operator ever follows `(HandshakeRejected)` to a row that is not
    /// there.
    ///
    /// **So this test guards the one addition that can reach an operator**: a
    /// status wired into the codec. `from_u32` answering anything but
    /// `Malformed` for the first unused value means exactly that, and the
    /// failure names what is owed.
    ///
    /// *A first cut of step 7 had this test and not the match, and called it
    /// the tripwire for a new status; it is half of one, and the half that
    /// fires later.*
    #[test]
    fn a_status_this_build_cannot_receive_needs_no_row() {
        use crate::wire::HelloStatus as H;
        // The named codes each round-trip to themselves, so none of them is a
        // fallback.
        let mut v = 0u32;
        for status in ALL_STATUSES {
            assert_eq!(status.as_u32(), v, "{status:?} is not wire value {v}");
            assert_eq!(H::from_u32(v), status, "wire value {v} lost its status");
            v += 1;
        }

        // **And nothing above forces `ALL_STATUSES` to be every status**, which
        // matters because its length is written out: a variant wired in at a
        // *gapped* value — 10, with 7 to 9 still folding — leaves the loop
        // above and the check below both satisfied. So the codec is walked, and
        // what it can deliver must be exactly this list.
        //
        // **The probe covers every `u16`, and 64 was not enough.** A first cut
        // stopped at 64 and its comment argued only the gapped-below-64 case; a
        // status at 64 itself was delivered by the codec, rendered by `Display`
        // and enumerated by nothing — measured. A discriminant is a wire
        // contract assigned explicitly (`wire.rs`), so `u16` is far past
        // anything the protocol contemplates; a status beyond it would escape,
        // and that is stated rather than left for the next person to measure.
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

        // `v` is the first value past the list, which is the one a new
        // status would take if the numbering stays contiguous.
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
        // **The widest state needs the pid too, and that is why it is a second
        // closure.** `samples()` carried the same defect until step 7: it swept
        // the mask and the slot to their maxima, left `first_pid` at 4242 —
        // six digits short — and its label said *widest*. The figure that came
        // out was 152, and the arm renders 164. A label is not a measurement.
        let held_wide = |slots: u64, first: Option<u32>, owned: bool| {
            IpcError::ArenaHeldButUnreachable {
                holder_slots: slots,
                first_slot: first,
                first_pid: u32::MAX,
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
            ("widest ids", held_wide(u64::MAX, Some(u32::MAX), true)),
            // Slot 0 is the widest of all: `, the creator's` costs more than
            // the nine digits a `u32::MAX` slot adds.
            ("widest of all", held_wide(u64::MAX, Some(0), true)),
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
        //
        // **Each id is checked on its own**, because the conjunction of two
        // `contains` over one string is satisfied by *either* field being wide:
        // `4294967295` was the slot, and the pid stayed at 4242 with this
        // assertion green — which is how the label outlived the measurement.
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
        // And the widest of all is the creator's slot, for the reason above it.
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
