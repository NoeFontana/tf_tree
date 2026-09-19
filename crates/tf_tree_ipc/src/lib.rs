//! Zero-configuration rendezvous for `tf_tree` shared arenas.
//!
//! A process calls `open()` and either joins the arena that already exists on
//! this machine or creates it. No configuration file, no daemon, no start-order
//! requirement, and **no possibility of two processes silently ending up on
//! different arenas.** This crate is the substrate that makes that true:
//! `docs/PHASE2.md` §3.1–§3.4 and §5.1.
//!
//! # Design
//!
//! Do not implement leader election; borrow the kernel's. Linux open file
//! description locks give mutual exclusion, release on holder death, and a way
//! to ask whether anyone holds it — no timeouts, no heartbeats, no stale state.
//!
//! * A dead participant's lock is released by the kernel at the end of its exit,
//!   after any core dump
//!   ([`0057`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0057-an-owner-is-not-dead-until-its-files-close.md)).
//! * A `SIGSTOP`ped participant still holds its lock, so it is never mistaken
//!   for a dead one.
//! * `/proc` parsing and PID-reuse defence are diagnostics only (§5.1).
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
//! # What is implemented here
//!
//! | Spec | Status |
//! |---|---|
//! | §3.1 runtime directory, incl. the NORMATIVE NFS/CIFS refusal | implemented |
//! | §3.2 domain and name defaults | implemented |
//! | §3.3 lock file: ownership byte, participant bytes, identity records | implemented |
//! | §3.4 `open()` decision algorithm, incl. the split-brain check | implemented; [`SocketProbe`] is the real §3.7 half, [`NoServer`] the test one |
//! | §5.1 `(pid, start_time, boot_id)` and the `/proc` parsing trap | implemented |
//! | §3.7 handshake messages ([`HelloRequest`], [`HelloResponse`], [`HelloStatus`]) | implemented, offsets and status codes pinned |
//! | §3.7 `SOCK_SEQPACKET` transport + `SCM_RIGHTS` ([`OwnerServer`], [`attach`]) | implemented |
//! | §3.6 `memfd` creation, and wiring this into `tf_tree::open()` | **not yet** — `docs/decisions/0005` step 5 |
//! | §6.1 claim leases: [`LockFile::try_take_claim`] and friends | implemented; the arena-side two-phase acquire is `docs/decisions/0005` step 7 |
//!
//! [`Open::open`] takes a [`ServerProbe`] so the split-brain race in §3.4 is
//! reproducible on demand.
//!
//! # Platform
//!
//! Linux only (§2). OFD locks reach the kernel through `libc`'s `fcntl`, a
//! documented deviation from §2's "no libc crate": `rustix` has no OFD locking,
//! and classic locks are rejected in §3.3.
// `unsafe` boundary: the OS (one `pthread_atfork` shim). See `docs/decisions/0007`.
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg(target_os = "linux")]
#![deny(missing_docs)]

// Wires `README.md`'s fences to the doctest harness; `cfg(doctest)` keeps it out of `cargo doc`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

mod client;
mod error;
pub mod fork;
mod identity;
mod lockfile;
mod ofd;
mod open;
mod procstat;
mod rendezvous;
mod runtime_dir;
mod server;
mod wire;

pub use client::{attach, peer_hung_up, Attached, SocketProbe};
pub use error::{
    EnvVar, IpcError, LockRole, NameProblem, ProcError, ProcParseError, RuntimeDirSource,
};
pub use identity::{AccessMode, Identity, IDENTITY_RECORD_LEN};
pub use lockfile::{LockFile, LockProbe, CLAIM_BASE, MAX_CLAIM_BYTES, MAX_PARTICIPANTS};
pub use ofd::LockAttempt;
pub use open::{
    CreatePolicy, NoServer, Open, OpenOutcome, Reach, ServerProbe, Session, DEFAULT_OPEN_TIMEOUT,
};
pub use procstat::{
    boot_id, parse_start_time, proc_self_pid, self_comm, self_pid_ns_inode, self_start_time,
    start_time_of,
};
pub use rendezvous::{
    domain_from_env, name_from_env, ArenaName, Rendezvous, DEFAULT_NAME, MAX_NAME_LEN,
};
pub use runtime_dir::{current_uid, EnvLookup, RuntimeDir, SystemEnv};
pub use server::{OwnerServer, ShutdownHandle};
pub use wire::{
    HelloRequest, HelloResponse, HelloStatus, SegmentDescriptor, WireError, HELLO_REQUEST_LEN,
    HELLO_RESPONSE_LEN, MAX_SOCKET_PATH, WIRE_MAGIC,
};
