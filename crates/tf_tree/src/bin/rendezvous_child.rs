//! Helper process for `tests/rendezvous.rs`.
//!
//! The rendezvous is a claim about what happens **between processes**, so the
//! test needs real ones: a thread cannot be `SIGKILL`ed out from under its
//! locks, and an inherited descriptor would share the parent's open file
//! description and make every contention assertion vacuous.
//!
//! Output is line-oriented on stdout and flushed before parking, so the parent
//! never has to guess whether a step has happened.
//!
//! ```text
//! rendezvous_child own    -> "owning <transform>", then parks serving
//! rendezvous_child join   -> "joined <transform>" | "error <display>"
//! ```
// This binary's stdout IS its protocol — the parent parses it line by line.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::panic
)]

#[cfg(all(feature = "shm", target_os = "linux"))]
fn main() {
    use std::io::Write;

    use tf_tree::{AttachMode, Capacity, EdgeCfg, InterpPolicy, Stamp, TreeBuilder};
    use tf_tree_ipc::CreatePolicy;

    fn say(line: &str) {
        println!("{line}");
        let _ = std::io::stdout().flush();
    }

    /// One dynamic edge is enough: the point is that two processes agree on the
    /// bytes, not that the topology is interesting.
    fn layout() -> TreeBuilder {
        TreeBuilder::new()
            .default_interp(InterpPolicy::LerpSlerp)
            .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64)))
    }

    /// Format a lookup so the parent can compare two processes' answers exactly.
    fn render(tree: &tf_tree::Tree, stamp: i64) -> String {
        let g = tree.guard();
        let target = tree.frame("map").unwrap();
        let source = tree.frame("base").unwrap();
        let plan = tree.plan(target, source).unwrap();
        let iso = plan
            .at(&g, Stamp::<tf_tree::SystemDomain>::from_nanos(stamp))
            .unwrap();
        // Bit patterns, not formatted floats: a comparison that rounds is a
        // comparison that can agree while the memory does not.
        let b = iso.to_bits();
        b.iter()
            .map(|w| format!("{w:016x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "own" => {
            let tree = tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::IfAbsent)
                .layout_if_creating(layout())
                .open()
                .expect("create the arena");

            let child = tree.frame("base").unwrap();
            let parent = tree.frame("map").unwrap();
            let pubr = tree.claim(child, parent).expect("claim");
            let iso = tf_tree::exp_se3([1.0, 2.0, 3.0, 0.1, 0.2, 0.3]);
            pubr.push(1_000, &iso).expect("push");
            pubr.push(2_000, &iso).expect("push");

            say(&format!("owning {}", render(&tree, 1_500)));
            // Park holding the tree: dropping it would stop the server and
            // release the ownership byte, which is what the joiner needs.
            loop {
                std::thread::park();
            }
        }
        "join" => {
            match tf_tree::Open::new()
                .create(CreatePolicy::Never)
                .timeout(std::time::Duration::from_millis(500))
                .open()
            {
                Ok(tree) => say(&format!("joined {}", render(&tree, 1_500))),
                Err(e) => say(&format!("error {e}")),
            }
        }
        other => panic!("unknown mode {other}"),
    }
}

#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn main() {}
