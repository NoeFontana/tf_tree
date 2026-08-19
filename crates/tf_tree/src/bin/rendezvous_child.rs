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
//! The bin target is `tf_tree_rendezvous_child`, not this file's name: the crate
//! is published, and under `--features shm` this binary lands in the installing
//! user's `bin/` (the manifest argues it, and says why installing nothing costs
//! more). Its argv:
//!
//! ```text
//! own           -> "owning <transform>", then parks serving
//! join          -> "joined <transform>" | "error <display>"   (read-only)
//! join-rw       -> as above, but registers in the arena table
//! open-free     -> as `join`, but through the zero-argument `tf_tree::open()`
//! own-headroom  -> "owning", then on stdin: "interned <frame id>"
//! peer-alive <slot> -> "alive <bool>", then parks
//! join-rw-report -> "joined", then one report line per slot number on stdin
//! own-claiming  -> "claimed <edge>", then parks holding it
//! join-claiming -> "claimed <edge>", then parks holding it
//! own-reap      -> "claimed", then on stdin: "reaped <n> still_ours <b>"
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
            // A second edge, so a peer can claim something the owner does not.
            .dynamic_edge("base", "cam", EdgeCfg::new(Capacity::slots(64)))
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
        // `join` is read-only (the consumer default, D18); `join-rw` registers
        // in the arena table, which is what a liveness probe can see.
        "join" | "join-rw" => {
            let mode = if mode == "join-rw" {
                AttachMode::ReadWrite
            } else {
                AttachMode::ReadOnly
            };
            match tf_tree::Open::new()
                .mode(mode)
                .create(CreatePolicy::Never)
                .timeout(std::time::Duration::from_millis(500))
                .open()
            {
                Ok(tree) => {
                    say(&format!("joined {}", render(&tree, 1_500)));
                    // Park holding the tree. Exiting here would release the
                    // participant slot immediately, so any test that asks
                    // whether this peer is alive would be racing its teardown
                    // rather than measuring liveness.
                    loop {
                        std::thread::park();
                    }
                }
                Err(e) => say(&format!("error {e}")),
            }
        }
        // **The zero-argument convenience, in a process that is not a child of
        // the owner.** `tf_tree::open()` is what a README reader types, and it
        // is the only caller of `Open::new()`'s *defaults* in the workspace —
        // every other one names `mode` and `create` explicitly, so nothing else
        // would notice if those defaults broke.
        "open-free" => {
            match tf_tree::open() {
                Ok(tree) => {
                    say(&format!("joined {}", render(&tree, 1_500)));
                    loop {
                        std::thread::park();
                    }
                }
                Err(e) => say(&format!("error {e}")),
            };
        }
        // Own an arena that has room for a frame nobody declared, then intern
        // one when the parent says so. The fixture `layout()` declares **no**
        // headroom, so a late intern there fails `CapacityExceeded` and the
        // waiting parent would time out for the wrong reason.
        "own-headroom" => {
            let tree = tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::IfAbsent)
                .layout_if_creating(layout().frame_headroom(4))
                .open()
                .expect("create the arena");
            say("owning");
            let mut line = String::new();
            let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line);
            let id = tree.frame("late_frame").expect("intern the late frame");
            say(&format!("interned {}", id.get()));
            loop {
                std::thread::park();
            }
        }
        // Create the arena, claim an edge, and park holding it — so the parent
        // can probe the lease and then kill this process.
        "own-claiming" => {
            let tree = tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::IfAbsent)
                .layout_if_creating(layout())
                .open()
                .expect("create the arena");
            let child = tree.frame("base").unwrap();
            let parent = tree.frame("map").unwrap();
            let w = tree.claim(child, parent).expect("claim");
            say(&format!("claimed {}", w.edge().get()));
            loop {
                std::thread::park();
            }
        }
        // Join read-write, claim the edge, and park holding it — so the parent
        // can kill this process and reap what it left behind.
        "join-claiming" => {
            let tree = tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::Never)
                .timeout(std::time::Duration::from_millis(500))
                .open()
                .expect("join");
            // The peer takes the *other* edge, so the owner's own claim is
            // distinguishable from the one it should reap.
            let child = tree.frame("cam").unwrap();
            let parent = tree.frame("base").unwrap();
            let w = tree.claim(child, parent).expect("claim");
            say(&format!("claimed {}", w.edge().get()));
            loop {
                std::thread::park();
            }
        }
        // Create, claim, and report what a reap sweep does — used to check the
        // reaper does not revoke its own live claims.
        "own-reap" => {
            let tree = tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::IfAbsent)
                .layout_if_creating(layout())
                .open()
                .expect("create");
            let child = tree.frame("base").unwrap();
            let parent = tree.frame("map").unwrap();
            let w = tree.claim(child, parent).expect("claim");
            say("claimed");
            // Read a line to know when the parent wants the sweep.
            let mut line = String::new();
            let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line);
            let n = tree.reap_dead();
            // The decisive part: our own claim must still work afterwards.
            let iso = tf_tree::exp_se3([0.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
            let still_ours = w.push(9_000, &iso).is_ok();
            say(&format!("reaped {n} still_ours {still_ours}"));
            loop {
                std::thread::park();
            }
        }
        // Join, then report whether a *named* peer slot reads alive. This is
        // what separates the kernel's answer from the /proc inference: a
        // SIGSTOPped holder still holds its lock byte.
        "peer-alive" => {
            let slot: u32 = std::env::args()
                .nth(2)
                .and_then(|s| s.parse().ok())
                .expect("slot");
            let tree = tf_tree::Open::new()
                .create(CreatePolicy::Never)
                .timeout(std::time::Duration::from_millis(500))
                .open()
                .expect("join");
            say(&format!("alive {}", tree.participant_alive(slot)));
            loop {
                std::thread::park();
            }
        }
        // **A surviving participant that reports on other slots, repeatedly.**
        //
        // `peer-alive` answers once and needs a *fresh* join to answer again,
        // which is exactly what a killed owner makes impossible: the rendezvous
        // socket dies with it, so no new process can attach
        // (`ArenaHeldButUnreachable`) and the only observer left is one that
        // joined while the owner was up. This mode is that observer. It reads
        // one slot number per line from stdin and answers with both facts the
        // arena carries about that slot — the record's `state` word and
        // `Tree::participant_alive` — so a caller can watch `LIVE -> FREE`, or
        // watch it fail to happen, without inferring either from the other.
        //
        // `unstable`, because the `state` word is only reachable through
        // `Tree::arena_view` (`docs/API.md` §2.6). Reporting `participant_alive`
        // alone would not do: it folds `state == LIVE` into its answer, so
        // `false` covers both "the record was cleared" and "the record is still
        // LIVE and its process is gone", and telling those two apart is the
        // whole question.
        #[cfg(feature = "unstable")]
        "join-rw-report" => {
            let tree = tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::Never)
                .timeout(std::time::Duration::from_millis(500))
                .open()
                .expect("join");
            say("joined");
            let stdin = std::io::stdin();
            let mut line = String::new();
            while std::io::BufRead::read_line(&mut stdin.lock(), &mut line).unwrap_or(0) > 0 {
                let slot: u32 = line.trim().parse().expect("a slot number per line");
                line.clear();
                let view = tree.arena_view();
                let rec = view.participants().get(slot).expect("slot in range");
                let word = rec.state.load(std::sync::atomic::Ordering::Acquire);
                let state = match tf_tree_core::participant::state_of(word) {
                    tf_tree_core::participant::LIVE => "live",
                    tf_tree_core::participant::RESERVED => "reserved",
                    _ => "free",
                };
                say(&format!(
                    "slot {slot} state {state} word {word:#x} pid {} alive {}",
                    rec.pid.load(std::sync::atomic::Ordering::Relaxed),
                    tree.participant_alive(slot),
                ));
            }
            loop {
                std::thread::park();
            }
        }
        other => panic!("tf_tree_rendezvous_child: unknown mode {other}"),
    }
}

#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn main() {}
