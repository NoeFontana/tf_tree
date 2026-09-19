//! Helper **process** for `tests/bridge_shared.rs` (`docs/decisions/0015`): attaches
//! read-only through the rendezvous, knowing only `$TF_TREE_RUNTIME_DIR`,
//! `$TF_TREE_DOMAIN` and the name on its command line. Deliberately does not link
//! the C ABI: a bridge-filled arena's consumer is an ordinary `tf_tree` consumer.
//!
//! ```text
//! bridge_reader <name> <target> <source> <stamp_nanos>
//!   -> "ok <16-hex-word>:<...>"   the lookup, as bit patterns
//!   -> "error <display>"          attach or lookup failed
//! ```

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

    // `Open::new()` defaults are the consumer: read-only, never create (0019 §2a).
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

// Unreachable under `required-features`; keeps the file a valid binary elsewhere.
#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn main() {
    eprintln!("bridge_reader needs --features shm on Linux");
    std::process::exit(2);
}
