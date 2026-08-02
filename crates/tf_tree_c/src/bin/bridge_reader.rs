//! Helper **process** for `tests/bridge_shared.rs` — `docs/decisions/0015`.
//!
//! The record's claim is that a bridge fills an arena *another process* can
//! read, and a second `tf_tree::Open` inside the test process does not show
//! that. The attach itself is genuine either way — it goes through the
//! rendezvous socket and receives the segment by fd passing — but "another
//! process" is a claim about **process** boundaries, and only a process can
//! carry it: this one shares no address space, no mapping and no open file
//! description with the bridge, and finds the arena from nothing but
//! `$TF_TREE_RUNTIME_DIR`, `$TF_TREE_DOMAIN` and the name on its command line.
//!
//! It is `tf_tree_c`'s rather than a reuse of `tf_tree`'s `rendezvous_child`,
//! because `CARGO_BIN_EXE_*` is set only for the tests of the package that
//! declares the binary. It deliberately does **not** link the C ABI: a consumer
//! of a bridge-filled arena is an ordinary `tf_tree` consumer, which is the
//! record's *"no new consumer API"*, and a reader built out of the same crate
//! the bridge lives in would not demonstrate that.
//!
//! Output is one line on stdout, so the parent parses rather than guesses:
//!
//! ```text
//! bridge_reader <name> <target> <source> <stamp_nanos>
//!   -> "ok <16-hex-word>:<...>"   the lookup, as bit patterns
//!   -> "error <display>"          attach or lookup failed
//! ```
//!
//! Bit patterns rather than formatted floats: a comparison that rounds is a
//! comparison that can agree while the memory does not.

// This binary's stdout IS its protocol — the parent parses it line by line.
#![allow(clippy::print_stdout, clippy::print_stderr)]

#[cfg(all(feature = "shm", target_os = "linux"))]
fn main() {
    use std::io::Write;

    fn say(line: &str) {
        println!("{line}");
        let _ = std::io::stdout().flush();
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let [name, target, source, stamp] = args.as_slice() else {
        eprintln!("usage: bridge_reader <name> <target> <source> <stamp_nanos>");
        std::process::exit(2);
    };
    let Ok(stamp) = stamp.parse::<i64>() else {
        eprintln!("bridge_reader: <stamp_nanos> is not an integer");
        std::process::exit(2);
    };

    // `Open::new()`'s defaults are the consumer (`docs/decisions/0019` §2a):
    // read-only, never create. Spelling neither is the point.
    let tree = match tf_tree::Open::new()
        .name(name)
        .and_then(tf_tree::Open::open)
    {
        Ok(t) => t,
        Err(e) => {
            say(&format!("error {e}"));
            return;
        }
    };

    let g = tree.guard();
    let (Ok(t), Ok(s)) = (tree.frame(target), tree.frame(source)) else {
        say("error a frame named on the command line is not in the arena");
        return;
    };
    let plan = match tree.plan(t, s) {
        Ok(p) => p,
        Err(e) => {
            say(&format!("error {e:?}"));
            return;
        }
    };
    match plan.at(
        &g,
        tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(stamp),
    ) {
        Ok(iso) => {
            let bits = iso
                .to_bits()
                .iter()
                .map(|w| format!("{w:016x}"))
                .collect::<Vec<_>>()
                .join(":");
            say(&format!("ok {bits}"));
        }
        Err(e) => say(&format!("error {e:?}")),
    }
}

// `required-features = ["shm"]` in Cargo.toml means this is the arm no build
// reaches; it exists so the file is still a valid binary on a non-Linux host.
#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn main() {
    eprintln!("bridge_reader needs --features shm on Linux");
    std::process::exit(2);
}
