//! Spawning child processes attached to a shared arena.
//!
//! Not the `docs/PHASE2.md` §3.3 attach protocol: the child receives the sealed
//! `memfd` segment (mapped `MAP_SHARED`) as its **standard input**, which is
//! enough to test and benchmark the mapping across processes. No handshake,
//! registry or crash machinery is exercised.
//!
//! Stdin rather than `dup2` in `pre_exec`, because that needs `unsafe` and this
//! crate is `#![forbid(unsafe_code)]`.

use std::os::fd::BorrowedFd;
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, Context, Result};

/// Slack added to a `contended_scaling` writer's publishing window, in seconds.
///
/// Shared by the coordinator (the writer must outlast its readers) and
/// `load_child` (the writer's rendezvous-join budget); one constant so the two
/// cannot drift.
pub const WRITER_SLACK_S: f64 = 1.0;

/// Spawn `program` with `segment` as its standard input.
///
/// The segment's fd is `CLOEXEC`, so a duplicate is installed as the child's fd 0.
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
/// Derived from the running executable's directory (including `deps/`), since
/// `CARGO_BIN_EXE_<name>` is unset for benchmark binaries.
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
