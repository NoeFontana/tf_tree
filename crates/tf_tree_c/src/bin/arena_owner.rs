//! Helper **process** for `tests/recovery.rs` — `docs/decisions/0044`.
//!
//! Creates and owns a shared arena, says so, and parks. The test then kills it,
//! which is the only way to produce the state the record is about: an arena
//! whose owner is gone, whose survivors still hold their participant bytes, and
//! which therefore refuses every new `open()` with `ArenaHeldButUnreachable`
//! until some survivor inherits the role.
//!
//! It is `tf_tree_c`'s rather than a reuse of `tf_tree`'s `rendezvous_child` for
//! the reason `bridge_reader`'s module doc gives: `CARGO_BIN_EXE_*` is set only
//! for the tests of the package that declares the binary.
//!
//! ```text
//! arena_owner <name>   -> "owning"   then parks until killed
//!                      -> "error <display>"
//! ```

// This binary's stdout IS its protocol — the parent parses it line by line.
#![allow(clippy::print_stdout, clippy::print_stderr)]

#[cfg(all(feature = "shm", target_os = "linux"))]
fn main() {
    use std::io::Write;

    use tf_tree::{AttachMode, Capacity, CreatePolicy, EdgeCfg, TreeBuilder};

    let Some(name) = std::env::args().nth(1) else {
        eprintln!("usage: arena_owner <name>");
        std::process::exit(2);
    };

    let built = match tf_tree::Open::new().name(&name) {
        Ok(o) => o,
        Err(e) => {
            println!("error {e}");
            std::process::exit(1);
        }
    }
    .mode(AttachMode::ReadWrite)
    .create(CreatePolicy::IfAbsent)
    .layout_if_creating(
        TreeBuilder::new()
            .dynamic_edge("map", "odom", EdgeCfg::new(Capacity::slots(64)))
            .dynamic_edge("odom", "base", EdgeCfg::new(Capacity::slots(64))),
    )
    .open();

    match built {
        // Held for the process's life: dropping it would release the ownership
        // byte politely, and the whole point is that the kernel does it.
        Ok(_tree) => {
            println!("owning");
            let _ = std::io::stdout().flush();
            loop {
                std::thread::park();
            }
        }
        Err(e) => {
            println!("error {e}");
            let _ = std::io::stdout().flush();
            std::process::exit(1);
        }
    }
}

#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn main() {
    eprintln!("arena_owner needs --features shm on Linux");
    std::process::exit(2);
}
