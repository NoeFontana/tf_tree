//! Child process for the multi-process rendezvous tests.
//!
//! Half of what this crate relies on is what the **kernel** does when a lock
//! holder dies, and there is no way to `SIGKILL` a thread out from under its
//! locks. So the tests spawn this binary, which takes a lock and then parks
//! until the parent kills it.
//!
//! It opens the lock file **by path**, not from an inherited descriptor. That is
//! the point: OFD locks belong to an open file description, so a child that
//! inherited the parent's fd would share the parent's locks and conflict with
//! nobody. Inheriting the fd — the transport
//! `crates/tf_tree_bench/src/shm_util.rs` uses for a shared *segment* — would
//! silently make every contention test vacuous.
//!
//! Output is line-oriented on stdout because the parent parses it, and every
//! line is flushed before the child blocks, so the parent never has to guess
//! whether a lock has been taken yet.
//!
//! ```text
//! ipc_child hold-ownership   <lock> [ms]     -> "won" | "lost", then parks
//! ipc_child hold-participant <lock> <slot>   -> "held <slot>" | "lost", then parks
//! ipc_child probe            <lock> <slot>   -> "ownership <held> <pid> participants <mask>"
//! ipc_child open             <lock-dir>      -> "<outcome> <slot>" | "error <display>"
//! ```
// This binary's stdout IS its protocol — the parent parses it line by line.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::panic
)]

#[cfg(target_os = "linux")]
fn main() {
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use tf_tree_ipc::{
        current_uid, AccessMode, ArenaName, EnvVar, Identity, LockAttempt, LockFile, NoServer,
        Open, OpenOutcome, Rendezvous, RuntimeDir,
    };

    /// Print and flush, then block until killed.
    ///
    /// Parking rather than sleeping a fixed time: the tests decide when the
    /// child dies, and a child that exited on its own would turn a "the kernel
    /// released it" assertion into "the child happened to finish".
    fn park() -> ! {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    fn say(line: &str) {
        println!("{line}");
        let _ = std::io::stdout().flush();
    }

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: ipc_child <mode> <path> [slot]");
        std::process::exit(2);
    }
    let mode = args[1].as_str();
    let path = PathBuf::from(&args[2]);

    match mode {
        // Retry until the deadline so the outcome is deterministic: whoever
        // holds it wins, everyone else reports "lost" rather than "lost the
        // race by starting first".
        "hold-ownership" => {
            let lock = LockFile::open(&path).expect("open lock file");
            let ms: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5_000);
            let deadline = Instant::now() + Duration::from_millis(ms);
            loop {
                match lock.try_take_ownership().expect("fcntl") {
                    LockAttempt::Acquired => {
                        say("won");
                        park();
                    }
                    LockAttempt::Contended if Instant::now() >= deadline => {
                        say("lost");
                        return;
                    }
                    LockAttempt::Contended => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        }
        "hold-participant" => {
            let slot: u32 = args[3].parse().expect("slot");
            let lock = LockFile::open(&path).expect("open lock file");
            let id = Identity::of_self_best_effort(AccessMode::ReadWrite);
            match lock.try_take_participant(slot).expect("fcntl") {
                LockAttempt::Acquired => {
                    lock.write_identity(slot, &id).expect("identity record");
                    say(&format!("held {slot}"));
                    park();
                }
                LockAttempt::Contended => {
                    say("lost");
                }
            }
        }
        "probe" => {
            let lock = LockFile::open(&path).expect("open lock file");
            let own = lock.probe_ownership().expect("fcntl");
            let mask = lock.held_participants().expect("fcntl");
            say(&format!(
                "ownership {} {} participants {mask}",
                own.held, own.holder_pid
            ));
        }
        // `path` is the runtime directory here, so the child resolves the same
        // rendezvous the parent did and runs the real §3.4 algorithm.
        "open" => {
            let rd = RuntimeDir::resolve_with(&Fixed(path), current_uid()).expect("runtime dir");
            let rv = Rendezvous::new(rd, 0, ArenaName::new("default", EnvVar::Name).unwrap());
            let timeout: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(200);
            let result = Open::new(rv)
                .timeout(Duration::from_millis(timeout))
                .open(&mut NoServer);
            match result {
                Ok(s) => {
                    let what = match s.outcome() {
                        OpenOutcome::Joined => "joined",
                        OpenOutcome::Created => "created",
                        OpenOutcome::TookOver => "tookover",
                    };
                    say(&format!("{what} {}", s.slot()));
                    park();
                }
                Err(e) => say(&format!("error {e}")),
            }
        }
        other => {
            eprintln!("ipc_child: unknown mode {other:?}");
            std::process::exit(2);
        }
    }

    /// An environment with `TF_TREE_RUNTIME_DIR` forced to one path, so the
    /// child cannot pick up whatever the test runner's environment says.
    struct Fixed(PathBuf);

    impl tf_tree_ipc::EnvLookup for Fixed {
        fn var(&self, key: &str) -> Option<std::ffi::OsString> {
            (key == "TF_TREE_RUNTIME_DIR").then(|| self.0.clone().into_os_string())
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ipc_child: Linux only (docs/PHASE2.md §2)");
    std::process::exit(2);
}
