//! `docs/PHASE2.md` §12.3 gate 4: **kill -> re-claimable p99 under 10 ms**.
//!
//! The interval between a claim-holding process being `SIGKILL`ed and *another*
//! process being able to take that edge: what a supervisor experiences restarting
//! a dead publisher. **The clock starts at the `kill(2)` return**, since kernel
//! teardown is part of what a supervisor waits through.
//!
//! **Reaping is caller-driven** (`docs/decisions/0019`): the harness spins on
//! `reap_dead()` itself, so a poll period is not in the number. It does not
//! distinguish the owner's hangup callback from the survivor's `reap_dead()`; both
//! answer the gate's question.

// A harness: its output is its result, and `expect` is right because a
// harness that cannot set up its fixture has nothing to report.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::expect_used,
    clippy::unwrap_used
)]

fn main() {
    #[cfg(not(all(feature = "shm", target_os = "linux")))]
    {
        eprintln!("reclaim_latency needs --features shm on Linux");
        std::process::exit(2);
    }
    #[cfg(all(feature = "shm", target_os = "linux"))]
    real::main();
}

#[cfg(all(feature = "shm", target_os = "linux"))]
mod real {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use tf_tree::{AttachMode, Capacity, EdgeCfg, Open, TreeBuilder};
    use tf_tree_ipc::CreatePolicy;

    fn layout() -> TreeBuilder {
        TreeBuilder::new()
            .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(64)))
            .dynamic_edge("base", "cam", EdgeCfg::new(Capacity::slots(64)))
    }

    /// The child: join, claim `base -> cam`, say so, and park until killed.
    fn child() -> ! {
        let tree = Open::new()
            .mode(AttachMode::ReadWrite)
            .create(CreatePolicy::Never)
            .timeout(Duration::from_secs(5))
            .open()
            .expect("join");
        let c = tree.frame("cam").expect("cam");
        let p = tree.frame("base").expect("base");
        let _w = tree.claim(c, p).expect("claim");
        println!("claimed");
        let _ = std::io::stdout().flush();
        loop {
            std::thread::park();
        }
    }

    pub(super) fn main() {
        let mut args = std::env::args().skip(1);
        if args.next().as_deref() == Some("child") {
            child();
        }
        let trials: usize = std::env::var("TRIALS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200);

        let dir = std::env::temp_dir().join(format!("tf_tree_reclaim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        std::env::set_var("TF_TREE_RUNTIME_DIR", &dir);

        let owner = Open::new()
            .mode(AttachMode::ReadWrite)
            .create(CreatePolicy::IfAbsent)
            .layout_if_creating(layout())
            .open()
            .expect("create the arena");
        let cam = owner.frame("cam").expect("cam");
        let base = owner.frame("base").expect("base");

        let exe = std::env::current_exe().expect("own path");
        let mut samples = Vec::with_capacity(trials);
        let mut first_ok = 0usize;

        for i in 0..trials {
            let mut kid = std::process::Command::new(&exe)
                .arg("child")
                .env("TF_TREE_RUNTIME_DIR", &dir)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawn child");
            // Block until the child holds the claim, or the kill could precede it.
            let mut line = String::new();
            {
                use std::io::BufRead;
                let out = kid.stdout.take().expect("piped");
                let mut r = std::io::BufReader::new(out);
                r.read_line(&mut line).expect("child line");
            }
            assert_eq!(line.trim(), "claimed", "trial {i}: child did not claim");

            let t0 = Instant::now();
            let _ = kid.kill();
            // **`wait()` is deliberately NOT here**: it blocks until kernel teardown, the
            // very interval measured (with it, 50/50 trials succeeded on attempt one). A
            // supervisor does not `wait` on the process whose slot it takes; the zombie holds
            // no descriptors, so the lock byte is already released.

            let mut spins = 0u64;
            // Spin until takeable; `reap_dead` is the caller-driven collector, though the
            // owner's hangup callback may get there first.
            loop {
                let _ = owner.reap_dead();
                spins += 1;
                if let Ok(w) = owner.claim(cam, base) {
                    if spins == 1 {
                        first_ok += 1;
                    }
                    samples.push(t0.elapsed());
                    drop(w); // release it for the next trial
                    break;
                }
                if t0.elapsed() > Duration::from_secs(5) {
                    eprintln!("trial {i}: edge never became re-claimable within 5 s");
                    std::process::exit(1);
                }
                std::hint::spin_loop();
            }
            let _ = kid.wait();
        }

        samples.sort_unstable();
        let at = |q: f64| samples[((samples.len() as f64 - 1.0) * q).round() as usize];
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        println!("tf_tree §12.3 gate 4 — kill -> re-claimable");
        println!("  trials      {trials}");
        // **The non-vacuity guard, a refusal not a note**: if the edge is takeable on
        // the first attempt there was no reclaim to wait for and the p99 is the cost of
        // `kill`; no verdict may be printed.
        println!(
            "  contended   {}/{} trials needed more than one attempt",
            trials - first_ok,
            trials
        );
        if first_ok * 10 > trials {
            eprintln!(
                "::error::{first_ok} of {trials} claims succeeded immediately; this run \
                 measured process teardown, not reclaim latency. INVALID, not FAIL."
            );
            std::process::exit(2);
        }
        println!("  p50         {:.3} ms", ms(at(0.50)));
        println!("  p99         {:.3} ms", ms(at(0.99)));
        println!(
            "  max         {:.3} ms",
            ms(*samples.last().expect("samples"))
        );
        let p99 = ms(at(0.99));
        println!(
            "  verdict     {} (criterion: p99 < 10 ms)",
            if p99 < 10.0 { "PASS" } else { "FAIL" }
        );
        let _ = std::fs::remove_dir_all(&dir);
        if p99 >= 10.0 {
            std::process::exit(1);
        }
    }
}
