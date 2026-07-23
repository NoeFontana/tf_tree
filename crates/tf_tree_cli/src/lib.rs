#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Implementation of the `tf_tree` diagnostics CLI.
//!
//! Printing to stdout/stderr is this crate's whole job, so the workspace
//! `print_stdout`/`print_stderr` lints are allowed here. Subcommands are wired
//! up by the Phase 1 CLI PR; the argument surface is defined now so the two
//! binaries (`tf_tree` and `tft`) share it.

use clap::{Parser, Subcommand};

/// `tf_tree` — inspect and debug a live transform tree.
#[derive(Parser)]
#[command(name = "tf_tree", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show live topology, per-edge rate, buffer occupancy, and writer PID.
    Tree,
    /// Continuously look up `target` relative to `source`.
    Echo {
        /// Target frame.
        target: String,
        /// Source frame.
        source: String,
    },
    /// Diagnose cycles, unclaimed edges, contention, and stale buffers.
    Doctor,
    /// Run the benchmark suite; exits non-zero if the go/no-go gate fails.
    Bench {
        /// Fail the process if the Phase 1 gate is not met.
        #[arg(long)]
        gate: bool,
    },
}

/// Parse arguments and dispatch. Entry point shared by both binaries.
pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Tree => println!("tf_tree tree: implemented by the Phase 1 CLI PR"),
        Command::Echo { target, source } => {
            println!("tf_tree echo {target} {source}: implemented by the Phase 1 CLI PR");
        }
        Command::Doctor => println!("tf_tree doctor: implemented by the Phase 1 CLI PR"),
        Command::Bench { gate } => {
            println!("tf_tree bench (gate={gate}): implemented by the Phase 1 bench PR");
        }
    }
    Ok(())
}
