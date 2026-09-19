//! Helper process for `tests/rendezvous.rs`: real processes, because a thread
//! cannot be `SIGKILL`ed out from under its locks and an inherited descriptor
//! would make contention assertions vacuous. `tests/python/test_shared.py` also
//! reads `join-reparent`'s `joined` and `reparented` lines (`docs/decisions/0058`
//! step 2), so those are a protocol. Stdout is line-oriented and flushed before
//! parking. The bin target is `tf_tree_rendezvous_child` (see the manifest). Its argv:
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
//! join-reparent -> "joined", then on stdin: "reparented" | "refused <e>"
//!                  (arm §11.3's topo.holding_lock to die inside the section)
//! join-sweep    -> "joined <slot>", then on one stdin poke: "swept <n>", and exits
//!                  (arm §11.3's reclaim.after_probe_before_cas to die before the CAS)
//! hold-topo <lock> -> "holding-topo", then parks holding A2's topology byte
//! join-heir     -> "joined <slot>", then per stdin poke "<owner_lost> <inheritance> <slot>",
//!                  serving if it inherited (§3.5; arm TF_TREE_CRASH_AT to die mid-inherit)
//! serve-then-die -> "serving", then "dying" and `abort` on the first client, from
//!                  inside the slot assigner (an owner dead between accept and reply)
//! ```
// stdout IS this binary's protocol.
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

    /// One dynamic edge: two processes agreeing on the bytes is the point.
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
        // Bit patterns, not formatted floats: a rounding comparison can agree while memory differs.
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
            // Park holding the tree: dropping it releases the ownership byte.
            loop {
                std::thread::park();
            }
        }
        // `join` is read-only (D18); `join-rw` registers in the arena table.
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
                    // Park holding the tree: exiting releases the participant slot.
                    loop {
                        std::thread::park();
                    }
                }
                Err(e) => say(&format!("error {e}")),
            }
        }
        // The zero-argument `tf_tree::open()`, the only caller of `Open::new()`'s defaults.
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
        // Own an arena with headroom (`layout()` has none), intern a frame on the parent's cue.
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
        // Create, claim, park holding it, so the parent can probe the lease and kill us.
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
        // Join read-write, claim, park holding it, so the parent can kill us and reap.
        "join-claiming" => {
            let tree = tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::Never)
                .timeout(std::time::Duration::from_millis(500))
                .open()
                .expect("join");
            // The other edge, so the owner's claim is distinguishable from the one to reap.
            let child = tree.frame("cam").unwrap();
            let parent = tree.frame("base").unwrap();
            // Reported, not `expect`ed: a refusal is a state the test asserts on. Bound for
            // the scope: `Ok(w) => say(...)` would drop the writer and release the claim.
            let _held = match tree.claim(child, parent) {
                Ok(w) => {
                    say(&format!("claimed {}", w.edge().get()));
                    Some(w)
                }
                Err(e) => {
                    say(&format!("refused {e:?}"));
                    None
                }
            };
            loop {
                std::thread::park();
            }
        }
        // Create, claim, report what a reap sweep does (it must not revoke our live claim).
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
        // Report whether a named peer slot reads alive; a SIGSTOPped holder keeps its lock byte.
        // A live holder of A2's topology byte (`docs/decisions/0029`), taken through
        // `tf_tree_ipc::LockFile` (a second open file description, as a mutator mid-`reparent`
        // presents) so a real process can be `SIGKILL`ed. The path is passed so the parent
        // decides which rendezvous this is.
        // §11.3's `topo.holding_lock`: re-parent `cam` under `map`; arm `TF_TREE_CRASH_AT` to
        // die inside the topology critical section.
        "join-reparent" => {
            let tree = tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::Never)
                .timeout(std::time::Duration::from_millis(500))
                .open()
                .expect("join");
            say("joined");

            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line).expect("read");
            let cam = tree.frame("cam").unwrap();
            let map = tree.frame("map").unwrap();
            match tree.reparent(cam, map) {
                Ok(()) => say("reparented"),
                Err(e) => say(&format!("refused {e:?}")),
            }
            loop {
                std::thread::park();
            }
        }
        // Sweep the participant table on demand (`reap_participants`; `own-reap` sweeps
        // claims), so `reclaim.after_probe_before_cas` can be armed outside the test binary.
        "join-sweep" => {
            let tree = tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::Never)
                .timeout(std::time::Duration::from_secs(5))
                .open()
                .expect("join");
            say(&format!("joined {}", tree.participant_slot()));
            // One sweep, then exit, not `park`: a disarmed site then exits 0 and the test
            // fails fast instead of waiting out a 180 s nextest timeout.
            let mut line = String::new();
            let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line);
            let n = tree.reap_participants();
            say(&format!("swept {n}"));
        }
        "hold-topo" => {
            let path = std::env::args().nth(2).expect("lock file path");
            let lock = tf_tree_ipc::LockFile::open(std::path::Path::new(&path))
                .expect("open the lock file");
            match lock.try_take_topology().expect("fcntl the topology byte") {
                tf_tree_ipc::LockAttempt::Acquired => say("holding-topo"),
                tf_tree_ipc::LockAttempt::Contended => say("topo-contended"),
            }
            loop {
                std::thread::park();
            }
        }
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
        // A survivor reporting on other slots repeatedly, for after the owner dies and no
        // new process can attach. Reads a slot per stdin line; prints the record's `state`
        // (`unstable`, via `Tree::arena_view`) and `participant_alive`, which folds `state`
        // in, so the two are needed to tell "cleared" from "LIVE but gone".
        // §3.5's survivor: on a poke reports whether the owner is gone and what inheriting
        // produced, then parks holding the tree (if it inherited it is now the server).
        "join-heir" => {
            let tree = tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::Never)
                .open()
                .expect("join");
            // The slot is printed on both lines to assert it does not move across takeover.
            say(&format!("joined {}", tree.participant_slot()));

            // One report per poke: a loser is asked twice (`docs/decisions/0043`), and what
            // serves is the tree this scope holds.
            let stdin = std::io::stdin();
            let mut line = String::new();
            while std::io::BufRead::read_line(&mut stdin.lock(), &mut line).unwrap_or(0) > 0 {
                line.clear();
                let lost = tree.owner_lost();
                let outcome = match tree.inherit_ownership() {
                    Ok(o) => format!("{o:?}"),
                    Err(e) => format!("error {e}"),
                };
                say(&format!("{lost} {outcome} {}", tree.participant_slot()));
            }
            loop {
                std::thread::park();
            }
        }
        // An owner that dies between `accept(2)` and its reply: aborting inside the slot
        // assigner (called after the `HelloRequest` is read, before any response) reaches
        // that window through public API. The client then reads zero bytes, not an error.
        // `abort`, so `OwnerServer::drop` leaves the socket stale as a crashed owner does (§3.9).
        // This is the only mode that binds a rendezvous path (`OwnerServer::bind_at` renames
        // over it), inside §3.10's trust model; the manifest records the installed residue.
        "serve-then-die" => {
            // Resolved the way `tf_tree::Open` resolves it, so the joiner finds the same one.
            let rd = tf_tree_ipc::RuntimeDir::resolve().expect("runtime dir");
            let domain =
                tf_tree_ipc::domain_from_env(&tf_tree_ipc::SystemEnv).expect("domain from env");
            let name = tf_tree_ipc::name_from_env(&tf_tree_ipc::SystemEnv).expect("name from env");
            let rv = tf_tree_ipc::Rendezvous::new(rd, domain, name);
            rv.ensure_dir().expect("runtime dir");

            // The descriptor must pass §3.7's `check` (version, layout hash, boot id), or the
            // owner rejects the client before the assigner.
            let desc = tf_tree_ipc::SegmentDescriptor {
                format_version: tf_tree_arena::FORMAT_VERSION,
                layout_hash: tf_tree_arena::layout_hash(),
                // Never sent: this process dies first.
                arena_size: 0,
                instance_uuid: [0; 16],
                boot_id: tf_tree_ipc::boot_id().unwrap_or([0; 16]),
            };
            let server =
                tf_tree_ipc::OwnerServer::bind_at(rv.sock_path(), desc, std::process::id())
                    .expect("bind the rendezvous socket");
            // `/dev/null` stands in for the segment; it never reaches a client.
            let devnull = std::fs::File::open("/dev/null").expect("/dev/null");
            say("serving");
            let outcome = server.serve(
                std::os::fd::AsFd::as_fd(&devnull),
                |_req| {
                    say("dying");
                    std::process::abort();
                },
                |_slot| {},
            );
            // Reached only if no client arrived; the parent must see that.
            say(&format!("server-stopped {outcome:?}"));
        }
        // `docs/PHASE2.md` §11.2 scenarios 7 and 9: every process on one
        // `(runtime_dir, domain, name)` sees the same `instance_uuid`. Whether this process
        // created or joined is not reported, so tests compare uuids instead of counting announcements.
        "open-uuid" => {
            match tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::IfAbsent)
                .layout_if_creating(layout())
                // The timeout is an argument: scenario 9 runs a thousand times and the ownerless
                // arm waits it out; the default stays generous for the herd.
                .timeout(std::time::Duration::from_millis(
                    std::env::args()
                        .nth(2)
                        .and_then(|a| a.parse::<u64>().ok())
                        .unwrap_or(5_000),
                ))
                .open()
            {
                Ok(tree) => {
                    let u = tree.instance_uuid();
                    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
                    say(&format!("uuid {hex}"));
                    // Park holding the tree: releasing it frees the byte the next step needs.
                    loop {
                        std::thread::park();
                    }
                }
                // A refusal is legitimate for scenario 9 (a second arena is not); report it as data.
                Err(e) => say(&format!("refused {e:?}")),
            }
        }
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
