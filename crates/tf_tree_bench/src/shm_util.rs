//! Spawning child processes attached to a shared arena.
//!
//! `docs/PHASE2.md` §3.3 specifies the production attach protocol: a
//! `SOCK_SEQPACKET` connection carrying the fd by `SCM_RIGHTS`, with version and
//! layout negotiation in the handshake. That protocol, the participant registry
//! it feeds, and the liveness/reaping machinery are **not implemented here**.
//!
//! What this module does instead is the minimum honest transport for testing and
//! benchmarking the thing that actually matters — the *mapping* — by handing the
//! child the segment as its **standard input**. The segment is still a sealed
//! `memfd` mapped `MAP_SHARED`, and the child still runs the unmodified Phase 1
//! reader, so every property being measured is the real one. Only the rendezvous
//! is simpler.
//!
//! # Why stdin rather than `dup2` in a `pre_exec` hook
//!
//! The obvious route — `dup2` the segment onto a known descriptor from
//! `Command::pre_exec` — requires `unsafe`, and this crate is
//! `#![forbid(unsafe_code)]`. Passing it as stdin needs none: `Stdio::from`
//! consumes an `OwnedFd` on this side and `stdin().as_fd().try_clone_to_owned()`
//! recovers it on the other, both safe. A file descriptor is a file descriptor;
//! nothing about the mapping cares which number it arrived on.
//!
//! The distinction between this and the real protocol matters for what may be
//! claimed: this proves the *mechanism* works across process boundaries. It does
//! not exercise attach-time negotiation, nor any of the crash-consistency
//! machinery, which is why `docs/PHASE2.md` §1's amendments remain outstanding.

use std::os::fd::BorrowedFd;
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, Context, Result};

/// Slack added to a `contended_scaling` writer's publishing window, in seconds.
///
/// **Shared between the coordinator and `load_child`, and that is the point.**
/// The coordinator spends it at the *end* — a writer that exits before the
/// readers it contends with turns the tail of every reader row into a
/// quiescent-tree measurement, silently — and the writer child spends it at the
/// *start*, as the budget its rendezvous join is allowed to take. Both halves
/// are the same margin: a writer's rate loop covers `[join, join + seconds +
/// WRITER_SLACK_S]` while its readers cover `[0, seconds]`, so a join longer
/// than this leaves more of the reader window uncontended than the harness ever
/// budgeted for. Two copies of the number could drift into a window with a hole
/// at both ends, which no column in the table would show.
pub const WRITER_SLACK_S: f64 = 1.0;

/// Spawn `program` with `segment` as its standard input.
///
/// The segment's own fd is `CLOEXEC` — deliberately, so a shared arena never
/// leaks into an unrelated child by accident — so it is duplicated here; the
/// duplicate is what `Stdio` installs as fd 0 in the child.
///
/// # Errors
///
/// If the descriptor cannot be duplicated or the child cannot be spawned.
pub fn spawn_attached(
    program: &std::path::Path,
    segment: BorrowedFd<'_>,
    args: &[String],
) -> Result<Child> {
    let dup = segment
        .try_clone_to_owned()
        .context("duplicating the segment fd for the child")?;

    Command::new(program)
        .args(args)
        .stdin(Stdio::from(dup))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow!("spawn {}: {e}", program.display()))
}

/// Path to a sibling binary in the same build directory as the current
/// executable.
///
/// `CARGO_BIN_EXE_<name>` is set for integration tests but not for benchmark
/// binaries, and both need this, so derive it from the running executable's
/// directory instead — handling the `deps/` subdirectory that test binaries live
/// in.
///
/// # Errors
///
/// If the current executable's path cannot be determined, or no such sibling
/// exists (usually: it was not built, because it is behind `--features shm`).
pub fn sibling_binary(name: &str) -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("locating the current executable")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("current exe has no parent directory"))?;
    for candidate in [dir.join(name), dir.join("..").join(name)] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "{name} not found next to {}; build it with \
         `cargo build --features shm --bin {name}`",
        exe.display()
    ))
}
