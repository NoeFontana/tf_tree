#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Workspace automation runner (`cargo xtask <task>`).
//!
//! `loom` is wired up (decision `0003` step 5 gate); `miri` and `bench-gate`
//! are wired up by the later Phase 1 PRs that introduce the code they exercise.

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    match task.as_deref() {
        Some("loom") => run_loom(),
        Some("miri") | Some("bench-gate") => {
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

/// Run the `tf_tree_core` concurrency model-checking suite under `--cfg loom`.
///
/// Builds and runs the crate's tests with `RUSTFLAGS="--cfg loom"`, which selects
/// the loom atomics in `crate::sync` and enables the `#[cfg(all(test, loom))]`
/// test module. A bounded preemption count keeps the state space tractable while
/// still exercising every ordering; override via `LOOM_MAX_PREEMPTIONS`.
fn run_loom() -> ExitCode {
    // Fold `--cfg loom` into any existing RUSTFLAGS rather than clobbering them.
    let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    if !rustflags.is_empty() {
        rustflags.push(' ');
    }
    rustflags.push_str("--cfg loom");

    let max_preemptions = std::env::var("LOOM_MAX_PREEMPTIONS").unwrap_or_else(|_| "3".into());

    println!("xtask loom: RUSTFLAGS=\"{rustflags}\" LOOM_MAX_PREEMPTIONS={max_preemptions}");
    let status = Command::new(env!("CARGO"))
        .args(["test", "-p", "tf_tree_core", "--tests", "--release"])
        .env("RUSTFLAGS", rustflags)
        .env("LOOM_MAX_PREEMPTIONS", max_preemptions)
        .status();

    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(s) => {
            eprintln!("xtask loom: tests failed ({s})");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("xtask loom: failed to spawn cargo: {e}");
            ExitCode::FAILURE
        }
    }
}
