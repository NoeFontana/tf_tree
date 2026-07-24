#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Workspace automation runner (`cargo xtask <task>`).
//!
//! Concrete tasks (`loom`, `miri`, `bench-gate`) are wired up by the Phase 1
//! PRs that introduce the code they exercise.

use std::process::ExitCode;

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    match task.as_deref() {
        Some("loom") | Some("miri") | Some("bench-gate") => {
            eprintln!(
                "xtask: '{}' is wired up by its Phase 1 PR",
                task.unwrap_or_default()
            );
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: cargo xtask <loom|miri|bench-gate>");
            ExitCode::FAILURE
        }
    }
}
