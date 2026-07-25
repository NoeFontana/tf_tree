//! Zero-configuration rendezvous for `tf_tree` shared arenas.
//!
//! A process calls `open()` and either joins the arena that already exists on
//! this machine or creates it. No configuration file, no daemon, no start-order
//! requirement, and **no possibility of two processes silently ending up on
//! different arenas.** This crate is the substrate that makes that true:
//! `docs/PHASE2.md` §3.1–§3.4 and §5.1.
//!
//! # The design principle
//!
//! **Do not implement leader election — borrow the kernel's.** A rendezvous
//! needs exactly three properties: mutual exclusion, automatic release when the
//! holder dies, and a way to ask whether anyone holds it. Linux open file
//! description locks provide all three, maintained by the kernel, with no
//! timeouts, no heartbeats, and no stale state that can survive a `SIGKILL`.
//! Every distributed-consensus-flavoured problem in this area dissolves into one
//! `fcntl` call.
//!
//! Concretely, that buys three things that a heartbeat protocol cannot have at
//! any price:
//!
//! * A `SIGKILL`ed participant's lock is released *by the kernel, immediately*.
//!   There is no timeout to tune and no state left behind to reap.
//! * A `SIGSTOP`ped participant **still holds its lock**, so it can never be
//!   mistaken for a dead one. A liveness heuristic that is wrong once in a
//!   thousand hours is exactly the kind of bug that ships.
//! * "Is anyone alive?" is a kernel fact rather than an inference, so
//!   `/proc` parsing and PID-reuse defence leave the correctness path entirely
//!   (§5.1) and survive only as diagnostics.
//!
//! # The sharing boundary
//!
//! Two processes share an arena **if and only if they resolve to the same
//! runtime directory, domain and name**:
//!
//! ```text
//! <runtime_dir>/<domain>/<name>.lock     # rendezvous + kernel-managed liveness
//! <runtime_dir>/<domain>/<name>.sock     # SOCK_SEQPACKET, owner-bound, FD passing
//! ```
//!
//! Sharing that directory between containers is a volume mount; not sharing it
//! is complete isolation. Either way the boundary is inspectable with `ls`,
//! which is why it is a directory and not an abstract socket namespace.
//!
//! # What is implemented here
//!
//! | Spec | Status |
//! |---|---|
//! | §3.1 runtime directory, incl. the NORMATIVE NFS/CIFS refusal | implemented |
//! | §3.2 domain and name defaults | implemented |
//! | §3.3 lock file: ownership byte, participant bytes, identity records | implemented |
//! | §3.4 `open()` decision algorithm, incl. the split-brain check | implemented, with the socket half injected as [`ServerProbe`] |
//! | §5.1 `(pid, start_time, boot_id)` and the `/proc` parsing trap | implemented |
//! | §3.6 `memfd` creation, §3.7 `SOCK_SEQPACKET` + `SCM_RIGHTS` attach | **not yet** — lands with `MappedArena` |
//! | §6.1 claims as OFD locks | **not yet** — the byte range is reserved ([`CLAIM_BASE`]) |
//!
//! Because §3.7 is absent, [`Open::open`] takes a [`ServerProbe`] that answers
//! "is anyone serving?". That is not a placeholder for the interesting part: the
//! interesting part is the lock-file half, where every race in §3.4 lives, and
//! injecting the probe is what makes the split-brain race reproducible on demand
//! instead of once in a thousand runs.
//!
//! # Platform
//!
//! Linux only (§2), on x86-64 or aarch64. The architecture list is explicit
//! rather than optimistic because the OFD lock commands are issued as raw
//! `fcntl` syscalls — `rustix` 1.1 has no OFD locking, and §2 permits no libc
//! crate and no C build step — and `struct flock`'s layout is not the same
//! everywhere. Building elsewhere is a `compile_error!`, not a silent guess.
#![cfg(target_os = "linux")]
#![deny(missing_docs)]

mod error;
mod identity;
mod lockfile;
mod ofd;
mod open;
mod procstat;
mod rendezvous;
mod runtime_dir;

pub use error::{
    EnvVar, IpcError, LockRole, NameProblem, ProcError, ProcParseError, RuntimeDirSource,
};
pub use identity::{AccessMode, Identity, IDENTITY_RECORD_LEN};
pub use lockfile::{LockFile, LockProbe, CLAIM_BASE, MAX_PARTICIPANTS};
pub use ofd::LockAttempt;
pub use open::{
    CreatePolicy, NoServer, Open, OpenOutcome, Reach, ServerProbe, Session, DEFAULT_OPEN_TIMEOUT,
};
pub use procstat::{boot_id, parse_start_time, self_comm, self_start_time, start_time_of};
pub use rendezvous::{
    domain_from_env, name_from_env, ArenaName, Rendezvous, DEFAULT_NAME, MAX_NAME_LEN,
};
pub use runtime_dir::{current_uid, EnvLookup, RuntimeDir, SystemEnv};
