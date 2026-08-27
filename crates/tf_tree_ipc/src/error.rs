//! `Copy`, `String`-free errors that name what failed.
//!
//! Same rule as the rest of the workspace (`docs/PROJECT.md` §5): an error
//! carries integers and enums, never an allocation. That is not asceticism here
//! — the rendezvous runs at process start, when the arena may be unmappable and
//! the failure has to be reportable by a binary that has no allocator state left
//! to trust. It also means an error can be returned from a signal-adjacent path
//! later without revisiting the type.
//!
//! Every variant names *both* sides of whatever disagreed, or the exact
//! environment variable / slot / errno responsible. `docs/PHASE2.md` §3.7 makes
//! the point for `LayoutMismatch`; it applies equally to everything here,
//! because the symptom an operator sees ("it will not start") is identical for
//! all of them.

use crate::HelloStatus;
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
    /// Fewer than 22 fields after `comm`.
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
    /// property this design relies on ("released by the kernel, immediately")
    /// stops being true.
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
    /// [`Open::already_attached_at`] named a slot whose participant byte nobody
    /// holds, so the declaration is not true.
    ///
    /// **The declaration is a precondition, and this is the check.** That
    /// builder asserts *"I already hold the arena at slot `n`"*, and every path
    /// through [`Open`] then hands `n` back as the session's slot rather than
    /// assigning one. If it is not true, the session names a participant record
    /// whose lock byte is free — the class `docs/PHASE2.md` §0.0 describes as
    /// reading dead to every probe-carrying observer, so a peer's reaper CASes
    /// the record away while its owner publishes.
    ///
    /// One `F_OFD_GETLK` buys the whole guarantee, and it also range-checks:
    /// `probe_participant` refuses a slot past `MAX_PARTICIPANTS` before it
    /// probes anything.
    ///
    /// [`Open`]: crate::Open
    /// [`Open::already_attached_at`]: crate::Open::already_attached_at
    NotAttachedAt {
        /// The slot the caller declared.
        slot: u32,
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
    /// The peer's datagram was not a well-formed handshake message.
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
    /// records for them are readable with
    /// [`crate::LockFile::read_identity`].
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
            IpcError::HandshakeMalformed(e) => {
                write!(f, "attach handshake reply was not well-formed: {e:?}")
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
                 file-lock semantics there are not exact enough for the rendezvous — \
                 set TF_TREE_RUNTIME_DIR to a local path",
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
            IpcError::NotAttachedAt { slot } => write!(
                f,
                "already_attached_at({slot}) was declared, but no process holds \
                 participant byte {slot}: a takeover keeps the slot it already \
                 has, and this one has none"
            ),
            IpcError::NoParticipantSlots { limit } => write!(
                f,
                "all {limit} participant slots are live; raising the limit requires \
                 rebuilding with a larger MAX_PARTICIPANTS and recreating the arena"
            ),
            IpcError::ArenaAbsent => f.write_str(
                "no arena is serving and CreatePolicy::Never forbids creating one",
            ),
            // The mask is empty in one distinct situation — nobody is attached,
            // but some process held the ownership byte for the whole deadline
            // without ever serving. Saying "slot 64, pid 0" there would point an
            // operator at a slot that does not exist.
            IpcError::ArenaHeldButUnreachable {
                holder_slots: 0,
                ownership_held: true,
                ..
            } => f.write_str(
                "the ownership byte was held for the whole open timeout by a process that \
                 never started serving, and no participant is attached; nothing was created",
            ),
            // Same empty mask, but the ownership probe at the deadline came back
            // free. The mask and the probe are read one after the other, at two
            // instants, so a holder that let go between the last acquire attempt
            // and the probe lands here — the arm above would claim it was held
            // "for the whole open timeout", which this cannot say.
            IpcError::ArenaHeldButUnreachable {
                holder_slots: 0, ..
            } => f.write_str(
                "nothing held the ownership byte or any participant byte when the open timeout \
                 expired, so whatever blocked every attempt had let go by then; retrying is the \
                 right response",
            ),
            IpcError::ArenaHeldButUnreachable {
                holder_slots,
                first_slot,
                first_pid,
                ownership_held,
            } => match (first_slot, ownership_held) {
                // Slot 0 is the creator's slot (`0035`, `CREATOR_SLOT`), and a
                // forced create takes slot 0 or nothing — so this is the one
                // state the escape hatch provably cannot pass, and pointing an
                // operator at it would send them down a path that fails.
                (Some(0), _) => write!(
                    f,
                    "an arena is alive but unreachable: participant slots {holder_slots:#x} still \
                     hold their lock bytes, and slot 0 (pid {first_pid}) is the arena creator's \
                     own slot — CreatePolicy::Always takes slot 0 or nothing, so no forced create \
                     can pass this. Stop the process holding slot 0: {next}",
                    next = if holder_slots == 1 {
                        "it is the only holder, so an ordinary open will then create"
                    } else {
                        "the other slots in the mask above are still held, so an ordinary open \
                         will still refuse — that is when PHASE2 §3.4's CreatePolicy::Always \
                         becomes the escape hatch, provided nothing still holds the ownership byte"
                    }
                ),
                // Every held byte is a non-owner's and nothing holds ownership:
                // §3.4's stranded-participant case, and the only one where
                // `CreatePolicy::Always` is the answer.
                (Some(slot), false) => write!(
                    f,
                    "an arena is alive but unreachable: participant slots {holder_slots:#x} still \
                     hold their lock bytes (slot {slot}, pid {first_pid}) and none took over \
                     ownership before the deadline; refusing to create a second arena. The \
                     creator's slot 0 and the ownership byte are both free, which is the case \
                     PHASE2 §3.4's escape hatch is for: CreatePolicy::Always will create a fresh \
                     arena and abandon this one"
                ),
                // A live holder of the ownership byte blocks the forced create
                // before it ever reaches the participant bytes it may skip.
                (Some(slot), true) => write!(
                    f,
                    "an arena is alive but unreachable: participant slots {holder_slots:#x} still \
                     hold their lock bytes (slot {slot}, pid {first_pid}), and the ownership byte \
                     is held by a process that is not serving. CreatePolicy::Always will not pass \
                     this — it has to take the ownership byte before the participant bytes it may \
                     skip. Stop the holders; refusing to create a second arena"
                ),
                (None, _) => write!(
                    f,
                    "an arena is alive but unreachable: no participant byte is held, yet ownership \
                     could not be taken before the deadline; refusing to create a second arena"
                ),
            },
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
             different arena layout than the running owner — rebuild both from the same source"
        }
        HelloStatus::BootIdMismatch => {
            "the arena records a different boot id than this host is running, so it \
             outlived a reboot; nothing in it is alive and it should be removed"
        }
        HelloStatus::NoParticipantSlots => {
            "every participant slot is taken. If the participants are real, raise the \
             arena's participant limit, which needs an owner restart; if they are not, \
             the slots are held by records of processes that died — `tf_tree participants` \
             prints one line per slot and marks those `stale`"
        }
        HelloStatus::ModeNotPermitted => {
            "this attach asked for read-write on an arena the owner will not let it write; \
             attach read-only, which is the consumer default"
        }
        HelloStatus::Malformed => {
            "the owner could not decode this attach request, or refused it for a reason \
             this build has no name for — the two are indistinguishable on the wire, so \
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
                write!(f, "/proc/{pid}/stat did not parse: {cause:?}")
            }
            ProcError::BootId => f.write_str("/proc/sys/kernel/random/boot_id is not a UUID"),
        }
    }
}

impl std::error::Error for IpcError {}
impl std::error::Error for ProcError {}
