//! Child process for the multi-process rendezvous tests.
//!
//! Spawned so tests can `SIGKILL` a lock holder and watch the kernel release
//! its locks. It opens the lock file **by path**, never from an inherited fd:
//! OFD locks belong to the open file description, so an inherited fd would share
//! the parent's locks and make every contention test vacuous.
//!
//! Output is line-oriented on stdout, flushed before the child blocks. The bin
//! is `tf_tree_ipc_child` (see the manifest). Its argv:
//!
//! ```text
//! hold-ownership   <lock> [ms]     -> "won" | "lost", then parks
//! hold-participant <lock> <slot>   -> "held <slot>" | "lost", then parks
//! probe            <lock> <slot>   -> "ownership <held> <pid> participants <mask>"
//! hold-claim       <lock> <edge>   -> "held <edge>" | "lost", then parks
//! open             <lock-dir>      -> "<outcome> <slot>" | "error <display>"
//! serve            <sock> <size>   -> "serving", then serves until killed
//! attach           <sock>          -> "attached <slot> <size> <uuid>" | "error <display>"
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

    use rustix::fd::AsFd;
    use tf_tree_ipc::{
        current_uid, self_start_time, AccessMode, ArenaName, EnvVar, HelloRequest, Identity,
        LockAttempt, LockFile, NoServer, Open, OpenOutcome, OwnerServer, Rendezvous, RuntimeDir,
        SegmentDescriptor,
    };

    /// Block until killed; the tests decide when the child dies.
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
        eprintln!("usage: tf_tree_ipc_child <mode> <path> [slot]");
        std::process::exit(2);
    }
    let mode = args[1].as_str();
    let path = PathBuf::from(&args[2]);

    match mode {
        // Retry until the deadline so "lost" means the lock was held throughout.
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
        "hold-claim" => {
            let lock = LockFile::open(&path).expect("open lock file");
            let edge: u32 = args.get(3).and_then(|s| s.parse().ok()).expect("edge");
            match lock.try_take_claim(edge).expect("fcntl") {
                LockAttempt::Acquired => {
                    say(&format!("held {edge}"));
                    park();
                }
                LockAttempt::Contended => say("lost"),
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
        // Serve a §3.7 handshake over `path`, backed by a bare memfd (no arena).
        "serve" => {
            use rustix::fs::{ftruncate, memfd_create, MemfdFlags};

            let size: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4096);
            let fd = memfd_create(c"tf_tree.serve_test", MemfdFlags::CLOEXEC).expect("memfd");
            ftruncate(&fd, size).expect("ftruncate");

            let desc = SegmentDescriptor {
                format_version: 2,
                layout_hash: 0xDEAD_BEEF,
                arena_size: size,
                instance_uuid: [0x5A; 16],
                boot_id: tf_tree_ipc::boot_id().unwrap_or([0; 16]),
            };
            let server = OwnerServer::bind_at(&path, desc, std::process::id()).expect("bind");
            say("serving");
            let mut next = 0u32;
            let outcome = server.serve(
                fd.as_fd(),
                |_req| {
                    let s = next;
                    next += 1;
                    Ok(s)
                },
                |slot| {
                    say(&format!("hangup {slot}"));
                },
            );
            // A returning server is a bug the parent must see.
            say(&format!("server-stopped {outcome:?}"));
        }
        // Attach and report, including the received fd's `fstat` size (proof a
        // real descriptor crossed).
        "attach" => {
            let req = HelloRequest {
                format_version: 2,
                layout_hash: 0xDEAD_BEEF,
                mode: AccessMode::ReadOnly,
                client_pid: std::process::id(),
                client_start_time: self_start_time().unwrap_or(0),
                client_boot_id: tf_tree_ipc::boot_id().unwrap_or([0; 16]),
                client_name: [0; 32],
            };
            match tf_tree_ipc::attach(&path, &req, Duration::from_secs(5)) {
                Ok(a) => {
                    let st = rustix::fs::fstat(&a.segment).expect("fstat");
                    let uuid: String = a
                        .response
                        .instance_uuid
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    say(&format!(
                        "attached {} {} {uuid}",
                        a.response.participant_slot, st.st_size
                    ));
                    park();
                }
                Err(e) => say(&format!("error {e}")),
            }
        }
        // `path` is the runtime directory; runs the real §3.4 algorithm.
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
                    };
                    say(&format!("{what} {}", s.slot()));
                    park();
                }
                Err(e) => say(&format!("error {e}")),
            }
        }
        other => {
            eprintln!("tf_tree_ipc_child: unknown mode {other:?}");
            std::process::exit(2);
        }
    }

    /// Environment with `TF_TREE_RUNTIME_DIR` forced to one path.
    struct Fixed(PathBuf);

    impl tf_tree_ipc::EnvLookup for Fixed {
        fn var(&self, key: &str) -> Option<std::ffi::OsString> {
            (key == "TF_TREE_RUNTIME_DIR").then(|| self.0.clone().into_os_string())
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("tf_tree_ipc_child: Linux only (docs/PHASE2.md §2)");
    std::process::exit(2);
}
