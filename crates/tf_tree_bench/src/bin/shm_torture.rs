//! `shm_torture` — `docs/PHASE2.md` §11.4, and `docs/PHASE5.md` §10's nightly CI job.
//!
//! N processes join one arena through the real rendezvous and hammer it with
//! random claim / push / lookup / release / reap / re-attach, while the driver
//! `SIGKILL`s one of them at 1–10 Hz and replaces it. Every reader validates
//! every transform it receives. Nothing here is a benchmark: the output is a
//! verdict.
//!
//! # What this covers, and what it does not — read this before quoting it
//!
//! §11.4 asks for four things. **Three are here:**
//!
//! * N processes doing random attach/detach/claim/reap/push/lookup — yes.
//! * Random `SIGKILL` at 1–10 Hz — yes, and it is the whole point: a killed
//!   process cannot clean up, so every claim, participant slot and lock byte it
//!   held has to be recovered by somebody else with no cooperation from it.
//! * Continuous invariant checking — yes, see [`Invariant`].
//!
//! **One is not**, and is not faked: "a random crash point armed in 10% of
//! children". Crash points are `docs/PHASE2.md` §11.3's `crash-points` feature,
//! which `docs/PHASE2.md` §0.0 records as **not implemented** — there is no
//! `TF_TREE_CRASH_AT` to arm. `--crash-points` therefore *refuses* rather than
//! silently running a weaker test, because the difference between the two
//! matters: `SIGKILL` lands between instructions at a distribution the scheduler
//! chooses, and crash points land at the eleven named mid-protocol states §11.3
//! enumerates. This harness reaches the first and cannot reach the second.
//!
//! §11.4 also says "run it under ASan ... and with `TF_TREE_PARANOID=1`".
//! `just shm-torture-asan` is the first. There is no `TF_TREE_PARANOID`: this
//! process validates every read unconditionally (see [`Invariant`]), which is
//! what that env var was for, so adding a switch to turn the checking *off*
//! would be the only thing it could mean.
//!
//! # Why one binary with a `child` mode
//!
//! The children must be real processes — a thread cannot be `SIGKILL`ed out from
//! under its locks, and Phase 2's recovery machinery is defined in terms of what
//! the *kernel* releases when a process dies. They re-exec this same binary
//! through `current_exe()`, so the protocol between driver and child is one file
//! and cannot drift.
//!
//! # How a violation gets out of a child
//!
//! A child that sees a bad transform prints one `VIOLATION ...` line to stderr
//! and exits [`EXIT_VIOLATION`]. The driver distinguishes that from the exit of
//! a child it killed itself, and from an ordinary error — a child that fails to
//! join because the owner was killed mid-handshake is expected, and is retried,
//! not reported.
// This binary's output is its result.
#![allow(clippy::print_stdout, clippy::print_stderr)]

#[cfg(not(all(feature = "shm", target_os = "linux")))]
fn main() {
    eprintln!(
        "shm_torture needs `--features shm` on Linux: it is a test of the shared arena, \
         and there is no shared arena in this build. Run `just shm-torture`."
    );
    std::process::exit(2);
}

#[cfg(all(feature = "shm", target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    imp::main()
}

#[cfg(all(feature = "shm", target_os = "linux"))]
mod imp {
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};
    use tf_tree::{AttachMode, Capacity, EdgeCfg, InterpPolicy, Iso3, Stamp, Tree, TreeBuilder};
    use tf_tree_ipc::CreatePolicy;

    /// A child exits with this after printing a `VIOLATION` line. Distinct from
    /// 1 (an ordinary error, which is expected and retried) and from the
    /// signalled exit of a child the driver killed.
    const EXIT_VIOLATION: i32 = 3;

    /// Frames in the torture topology.
    ///
    /// Four dynamic edges over five frames, all in one chain, so a `map ->
    /// tool` lookup composes every edge and a bad sample anywhere is visible at
    /// the end. A star topology would let three quarters of the corruption hide.
    const CHAIN: &[(&str, &str)] = &[
        ("map", "odom"),
        ("odom", "base"),
        ("base", "arm"),
        ("arm", "tool"),
    ];

    /// The topology every participant agrees on. Only the creator's copy is
    /// used, but all of them must be able to produce it — a participant that
    /// arrives with a different layout is refused by the rendezvous, which is
    /// itself worth exercising.
    fn layout() -> TreeBuilder {
        let mut b = TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
        for (parent, child) in CHAIN {
            // 64 slots: small enough that the ring wraps constantly during a
            // run, which is where a reader racing a writer actually happens.
            b = b.dynamic_edge(parent, child, EdgeCfg::new(Capacity::slots(64)));
        }
        b
    }

    /// What a reader checks on every single transform it receives.
    ///
    /// §11.4: "no reader ever observes a non-unit quaternion or a NaN". Both are
    /// checked here, on every read, in every child, with no way to turn them
    /// off — which is what `TF_TREE_PARANOID=1` was for.
    ///
    /// The quaternion bound is loose (1e-6) on purpose. A composed chain of four
    /// interpolated rotations accumulates float error, so a tight bound would
    /// report arithmetic as corruption. What it is written against is torn
    /// memory: half of one sample and half of another give a norm nowhere near
    /// 1, not 1 + 1e-9.
    #[derive(Debug, Clone, Copy)]
    struct Invariant;

    impl Invariant {
        /// `Ok(())`, or the reason this transform cannot have come from a
        /// consistent sample.
        fn check(iso: &Iso3) -> Result<(), String> {
            let t = [iso.t.x, iso.t.y, iso.t.z];
            let q = [iso.q.x, iso.q.y, iso.q.z, iso.q.w];
            for (name, v) in [("tx", t[0]), ("ty", t[1]), ("tz", t[2])] {
                if !v.is_finite() {
                    return Err(format!("translation {name} is {v}"));
                }
            }
            for (name, v) in [("qx", q[0]), ("qy", q[1]), ("qz", q[2]), ("qw", q[3])] {
                if !v.is_finite() {
                    return Err(format!("quaternion {name} is {v}"));
                }
            }
            let n2 = q.iter().map(|v| v * v).sum::<f64>();
            if (n2 - 1.0).abs() > 1e-6 {
                return Err(format!("quaternion norm^2 is {n2}, not 1"));
            }
            Ok(())
        }
    }

    /// Wall-clock nanoseconds, which every participant uses as its stamp.
    ///
    /// **A shared clock, not a per-process counter.** Writers come and go — a
    /// child that claims an edge after the previous owner was killed must push a
    /// stamp the ring will accept, and a reader that has never met either writer
    /// has to know where to look. A private counter per process gives four
    /// unrelated timelines: pushes fail `NonMonotonicStamp` on every hand-over
    /// and every lookup lands outside the ring, so the run reports zero
    /// violations having validated almost nothing. That is what the first
    /// revision of this file did.
    fn now_nanos() -> i64 {
        // `as i64` saturates in the year 2262; `unwrap_or_default` covers a
        // clock before the epoch, which would be a host problem, not ours.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or_default()
    }

    /// A 64-bit xorshift. Deterministic from a seed, so a failing run is
    /// replayable with `--seed`, and no `rand` dependency is added to a crate
    /// that does not have one.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Rng {
            // A zero seed is a fixed point of xorshift64 — it would emit zeros
            // forever and every child would take the same branch.
            Rng(if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            })
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }
        fn unit(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    struct Args {
        duration: Duration,
        children: usize,
        seed: u64,
        kill_hz: f64,
        inject: bool,
    }

    fn parse_duration(s: &str) -> Result<Duration> {
        if let Some(ms) = s.strip_suffix("ms") {
            return Ok(Duration::from_millis(ms.parse()?));
        }
        let secs = s.strip_suffix('s').unwrap_or(s);
        Ok(Duration::from_secs_f64(secs.parse()?))
    }

    pub(crate) fn main() -> Result<()> {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        if argv.first().map(String::as_str) == Some("child") {
            return child(&argv[1..]);
        }

        let mut a = Args {
            duration: Duration::from_secs(30),
            children: 6,
            seed: 0x7085_1234_ABCD_0001,
            kill_hz: 4.0,
            inject: false,
        };
        let mut it = argv.into_iter();
        while let Some(arg) = it.next() {
            let mut value = |name: &str| -> Result<String> {
                it.next().with_context(|| format!("{name} needs a value"))
            };
            match arg.as_str() {
                "--duration" => a.duration = parse_duration(&value("--duration")?)?,
                "--children" => a.children = value("--children")?.parse()?,
                "--seed" => a.seed = value("--seed")?.parse()?,
                "--kill-hz" => a.kill_hz = value("--kill-hz")?.parse()?,
                // The self-test: one child deliberately publishes a transform
                // that violates §11.4's rule, and the run is expected to FAIL.
                // Without it, "0 violations" is also what a harness that never
                // looked would print — which is how a soak test spends two years
                // being green and worthless.
                "--inject-violation" => a.inject = true,
                "--crash-points" => bail!(
                    "--crash-points would arm `docs/PHASE2.md` §11.3's fault-injection sites, \
                     and there are none: the `crash-points` feature and `TF_TREE_CRASH_AT` are \
                     recorded as not implemented in §0.0, so there is nothing to arm. This \
                     harness kills children with SIGKILL, which reaches a different (and \
                     scheduler-chosen) set of mid-protocol states. Accepting the flag and \
                     running the SIGKILL test anyway would report §11.3 coverage that does \
                     not exist."
                ),
                "-h" | "--help" => {
                    println!(
                        "usage: shm_torture [--duration 30s] [--children 6] [--seed N] \
                         [--kill-hz 4] [--inject-violation]"
                    );
                    println!(
                        "  --crash-points is PHASE2 §11.3's fault injection and is refused: \
                         the feature it would arm does not exist."
                    );
                    return Ok(());
                }
                other => bail!("unknown argument `{other}`"),
            }
        }
        if a.children == 0 {
            bail!("--children 0 leaves nobody to torture the arena");
        }
        if !(0.1..=100.0).contains(&a.kill_hz) {
            bail!(
                "--kill-hz {} is outside §11.4's 1-10 Hz by more than a \
                   factor of ten either way",
                a.kill_hz
            );
        }
        drive(&a)
    }

    /// A scratch runtime directory, removed on drop so a failing run does not
    /// leave an arena in `/tmp` for the next one to join by accident.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One live child and the seed it was started with.
    struct Kid {
        proc: Child,
        seed: u64,
        /// Whether this child publishes the deliberate corruption. Carried per
        /// child rather than decided at spawn time, so a respawn of the
        /// injecting slot keeps injecting: otherwise the self-test's outcome
        /// depends on whether the killer happened to pick slot 0 in the first
        /// second, and a self-test that passes by luck is not one.
        inject: bool,
    }

    impl Drop for Kid {
        fn drop(&mut self) {
            let _ = self.proc.kill();
            let _ = self.proc.wait();
        }
    }

    fn spawn(exe: &std::path::Path, dir: &PathBuf, seed: u64, inject: bool) -> Result<Kid> {
        let mut cmd = Command::new(exe);
        cmd.arg("child")
            .arg("--seed")
            .arg(seed.to_string())
            .env("TF_TREE_RUNTIME_DIR", dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Inherited: a `VIOLATION` line must reach the operator's terminal
            // and the CI log even if the driver is killed before it reports.
            .stderr(Stdio::inherit());
        if inject {
            cmd.arg("--inject-violation");
        }
        let proc = cmd.spawn().context("spawning a torture child")?;
        Ok(Kid { proc, seed, inject })
    }

    fn drive(a: &Args) -> Result<()> {
        let exe = std::env::current_exe().context("locating this executable")?;
        let dir = std::env::temp_dir().join(format!("tf_tree_torture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let scratch = Scratch(dir.clone());
        std::env::set_var("TF_TREE_RUNTIME_DIR", &dir);

        let mut rng = Rng::new(a.seed);
        println!(
            "shm_torture: {} children, {:?}, SIGKILL at {} Hz, seed {}, runtime dir {}",
            a.children,
            a.duration,
            a.kill_hz,
            a.seed,
            dir.display()
        );
        if a.inject {
            println!(
                "  --inject-violation: one child publishes a corrupt transform on purpose. \
                 THIS RUN IS EXPECTED TO FAIL."
            );
        }

        // Kids are held in fixed slots, not a list, so "slot 0 injects" survives
        // slot 0 being killed and replaced.
        let mut kids: Vec<Option<Kid>> = Vec::with_capacity(a.children);
        for i in 0..a.children {
            // Only *one* slot injects, and only when asked: a corrupt sample
            // from every writer would let a child detect its own corruption,
            // which proves nothing about what crosses a process boundary.
            kids.push(Some(spawn(&exe, &dir, rng.next_u64(), a.inject && i == 0)?));
        }

        // **The driver attaches too, and stays.** Two reasons, both load-bearing:
        //
        // 1. It keeps the segment alive. Without it the last kill frees the
        //    arena with its final mapping, and `check_recovery` has nothing to
        //    check — which is exactly what an earlier revision did, printing a
        //    green "PASS" whose recovery half had silently skipped every run.
        // 2. It is a reader that is never killed, so §11.4's "invariants checked
        //    continuously" does not depend on which child happened to survive.
        //
        // It retries: the first child has to win the create race before there is
        // anything to join.
        let observer = attach_observer()?;

        let deadline = Instant::now() + a.duration;
        let mut kills = 0usize;
        let mut reads = 0u64;
        let mut violations = Vec::new();
        let interval = Duration::from_secs_f64(1.0 / a.kill_hz);

        while Instant::now() < deadline {
            // Jitter, so the kills do not land in phase with any loop a child
            // runs. An in-phase killer reaches one point in the protocol over
            // and over and calls the other points covered.
            let jitter = 0.5 + rng.unit();
            std::thread::sleep(interval.mul_f64(jitter).min(deadline - Instant::now()));

            reads += observe(&observer, &mut rng, &mut violations);
            reap_finished(&mut kids, &mut violations);
            if !violations.is_empty() {
                break;
            }
            for (slot, kid) in kids.iter_mut().enumerate() {
                if kid.is_none() {
                    let seed = rng.next_u64();
                    *kid = Some(spawn(&exe, &dir, seed, a.inject && slot == 0)?);
                }
            }
            let victim = rng.below(a.children as u64) as usize;
            if let Some(kid) = kids[victim].as_mut() {
                let _ = kid.proc.kill();
                let _ = kid.proc.wait();
                kids[victim] = None;
                kills += 1;
            }
        }

        // Give the survivors a moment to exit on their own so a violation
        // detected in the last instant is not lost to the teardown.
        std::thread::sleep(Duration::from_millis(200));
        reads += observe(&observer, &mut rng, &mut violations);
        reap_finished(&mut kids, &mut violations);
        // Kill every remaining child *before* the recovery check: "no claim is
        // held by a dead participant" is only a statement about a quiescent
        // arena, and a live writer would fail it correctly and uselessly.
        drop(kids);
        std::thread::sleep(Duration::from_millis(100));

        let recovery = check_recovery(&observer);
        drop(observer);
        drop(scratch);

        println!("shm_torture: {reads} checked reads from the observer");

        println!(
            "shm_torture: {kills} kills, {} violation(s)",
            violations.len()
        );
        for v in &violations {
            println!("  {v}");
        }
        let recovery = recovery?;
        for line in &recovery.notes {
            println!("  {line}");
        }
        if !violations.is_empty() {
            bail!(
                "{} invariant violation(s) — the arena is not crash-consistent \
                 (docs/PHASE2.md §12.3 gate 3)",
                violations.len()
            );
        }
        if !recovery.failures.is_empty() {
            for f in &recovery.failures {
                eprintln!("  {f}");
            }
            bail!(
                "{} recovery failure(s) after the run",
                recovery.failures.len()
            );
        }
        println!("shm_torture: PASS");
        Ok(())
    }

    /// Collect children that have exited, recording the ones that reported a
    /// violation.
    ///
    /// A child that exits non-zero for any *other* reason is not a failure: a
    /// joiner whose owner was killed mid-handshake fails to open, and that is
    /// the run working as intended. Only [`EXIT_VIOLATION`] means the arena lied
    /// to somebody.
    fn reap_finished(kids: &mut [Option<Kid>], violations: &mut Vec<String>) {
        for slot in kids.iter_mut() {
            let Some(kid) = slot.as_mut() else { continue };
            match kid.proc.try_wait() {
                Ok(Some(status)) => {
                    if status.code() == Some(EXIT_VIOLATION) {
                        violations.push(format!(
                            "child (seed {}{}) reported an invariant violation; its \
                             `VIOLATION` line is on stderr above",
                            kid.seed,
                            if kid.inject { ", the injector" } else { "" }
                        ));
                    }
                    *slot = None;
                }
                Ok(None) => {}
                Err(_) => *slot = None,
            }
        }
    }

    /// Join the arena the children create, retrying until one of them wins the
    /// create race.
    ///
    /// `CreatePolicy::Never`: the driver must not be the one that creates the
    /// arena. If it were, the very first thing the run tests — N processes
    /// racing to create one arena, exactly one winning — would never happen.
    fn attach_observer() -> Result<Tree> {
        let start = Instant::now();
        loop {
            match tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::Never)
                .timeout(Duration::from_millis(200))
                .open()
            {
                Ok(t) => return Ok(t),
                Err(e) => {
                    if start.elapsed() > Duration::from_secs(10) {
                        bail!("no child created an arena within 10 s: {e}");
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// A burst of checked reads from the never-killed observer.
    ///
    /// Returns how many transforms were validated, which the run prints. A count
    /// is not decoration here: "0 violations" and "0 reads" print the same
    /// verdict, and only one of them is a result.
    fn observe(tree: &Tree, rng: &mut Rng, violations: &mut Vec<String>) -> u64 {
        let (Ok(map), Ok(tool)) = (tree.frame(CHAIN[0].0), tree.frame(CHAIN[CHAIN.len() - 1].1))
        else {
            return 0;
        };
        let Ok(plan) = tree.plan(map, tool) else {
            return 0;
        };
        let guard = tree.guard();
        let mut n = 0;
        let mut aim = now_nanos();
        for _ in 0..256 {
            // Jitter backwards, so consecutive reads are not the same query and
            // the interpolation between two slots is exercised rather than one
            // exact hit repeated 256 times.
            let at = aim - rng.below(2_000_000) as i64;
            match plan.at::<tf_tree::SystemDomain>(&guard, Stamp::from_nanos(at)) {
                Ok(iso) => {
                    n += 1;
                    if let Err(why) = Invariant::check(&iso) {
                        violations.push(format!(
                            "the observer read a bad transform: map->tool at {at}: {why}"
                        ));
                        return n;
                    }
                }
                // **Aim from the arena, not from the clock.** Writers here are
                // killed and replaced constantly, so the newest sample can be
                // hundreds of milliseconds behind `now`; a reader that only ever
                // asks for `now` reads nothing at all and then reports zero
                // violations having validated nothing. The error carries the
                // window it wanted, so the next query uses it.
                Err(tf_tree::LookupError::Extrapolation { newest, oldest, .. }) => {
                    aim = newest.max(oldest);
                }
                Err(_) => {}
            }
        }
        n
    }

    struct Recovery {
        failures: Vec<String>,
        notes: Vec<String>,
    }

    /// §11.4's "participant and claim slots never leak", checked once the run is
    /// quiescent.
    ///
    /// Runs on the observer's attachment, which has been held for the whole run
    /// — so the arena being checked is the one the children tortured, not a
    /// fresh one created after they all died.
    fn check_recovery(tree: &Tree) -> Result<Recovery> {
        let mut out = Recovery {
            failures: Vec::new(),
            notes: Vec::new(),
        };

        // Reap first: a claim held by a process the kernel has already cleaned
        // up is *reapable*, not leaked, and the distinction is the whole of A3.
        // Leaving it un-reaped and calling it a leak would fail the property the
        // design actually promises.
        let reaped = tree.reap_dead();
        out.notes.push(format!(
            "recovery: reaped {reaped} slot(s) left by killed children"
        ));

        let me = tree.participant_slot();
        let view = tree.arena_view();
        for (parent, child) in CHAIN {
            let (Ok(p), Ok(c)) = (tree.frame(parent), tree.frame(child)) else {
                out.failures.push(format!(
                    "frame `{parent}` or `{child}` vanished from the arena"
                ));
                continue;
            };
            // Claiming is the sharpest test available from here: it succeeds
            // only if no owner remains, so it answers "did every killed writer's
            // claim come back" without reading a private field.
            match tree.claim(c, p) {
                Ok(w) => drop(w),
                Err(e) => out.failures.push(format!(
                    "edge {parent}->{child} is still claimed after every writer died and \
                     reap_dead ran: {e:?}"
                )),
            }
            let _ = (p, c);
        }

        let table = view.participants();
        let mut live = Vec::new();
        for slot in 0..table.capacity() as u32 {
            if slot != me && tree.participant_alive(slot) {
                live.push(slot);
            }
        }
        if !live.is_empty() {
            out.failures.push(format!(
                "participant slot(s) {live:?} are still marked alive after every child was \
                 killed or exited; only this process (slot {me}) should remain"
            ));
        }
        Ok(out)
    }

    /// One worker: join the arena and hammer it until killed.
    fn child(argv: &[String]) -> Result<()> {
        let mut seed = 1u64;
        let mut inject = false;
        let mut it = argv.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--seed" => {
                    seed = it
                        .next()
                        .context("--seed needs a value")?
                        .parse()
                        .context("--seed")?;
                }
                "--inject-violation" => inject = true,
                other => bail!("child: unknown argument `{other}`"),
            }
        }
        let mut rng = Rng::new(seed);

        loop {
            // `IfAbsent`, in every child: the driver kills the owner too, and
            // the survivor that gets there first has to be able to bring the
            // arena back. `Never` here would turn every owner kill into the end
            // of the run.
            let tree = match tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::IfAbsent)
                .layout_if_creating(layout())
                .timeout(Duration::from_secs(2))
                .open()
            {
                Ok(t) => t,
                // Expected while an owner is being killed: back off and retry
                // rather than exiting, so the driver does not have to
                // distinguish "lost a race" from "the arena is broken".
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(10 + rng.below(40)));
                    continue;
                }
            };
            work(&tree, &mut rng, inject)?;
        }
    }

    /// The random-operation loop against one attachment. Returns when it decides
    /// to detach and re-join, which is §11.4's "attach/detach".
    fn work(tree: &Tree, rng: &mut Rng, inject: bool) -> Result<()> {
        // Interning can fail while another participant is mid-mutation; that is
        // not a violation, it is a retry.
        let mut ids = Vec::new();
        for (parent, child) in CHAIN {
            match (tree.frame(parent), tree.frame(child)) {
                (Ok(p), Ok(c)) => ids.push((p, c)),
                _ => return Ok(()),
            }
        }
        let (map, tool) = (ids[0].0, ids[ids.len() - 1].1);

        let mut held: Option<(usize, tf_tree::EdgeWriter<'_>)> = None;
        let mut aim = now_nanos();
        // A bounded number of operations per attachment, so every child
        // re-attaches regularly instead of one lucky survivor holding the arena
        // for the whole run.
        for _ in 0..2_000 {
            // **Pacing, and it is not politeness.** A 64-slot ring filled by an
            // unthrottled loop covers about nine *microseconds* of history, so
            // by the time any reader looks the whole window has rolled past and
            // every lookup is `Extrapolation`. The first revision of this file
            // ran exactly that way: it reported zero violations having validated
            // zero transforms. At ~1 kHz the same ring covers ~64 ms, which is a
            // window a reader in another process can actually land in — and it
            // also spreads the driver's SIGKILLs across the protocol instead of
            // landing them all inside one hot loop.
            std::thread::sleep(Duration::from_micros(200 + rng.below(1_600)));
            match rng.below(100) {
                // Claim an edge, if we hold none.
                0..=9 => {
                    if held.is_none() {
                        let i = rng.below(CHAIN.len() as u64) as usize;
                        let (p, c) = ids[i];
                        // A refused claim is the correct answer when somebody
                        // else holds it. Only a *granted* one is interesting.
                        if let Ok(w) = tree.claim(c, p) {
                            held = Some((i, w));
                        }
                    }
                }
                // Release it.
                10..=14 => {
                    held = None;
                }
                // Reap whatever the last victim left behind.
                15..=17 => {
                    let _ = tree.reap_dead();
                }
                // Detach and re-join.
                18..=19 => return Ok(()),
                // Publish.
                20..=59 => {
                    if let Some((_, w)) = &held {
                        let iso = sample(rng, inject);
                        // `ClaimRevoked` is A4 working: this writer was judged
                        // dead, reaped, and is being fenced. Drop the claim and
                        // carry on.
                        if w.push(now_nanos(), &iso).is_err() {
                            held = None;
                        }
                    }
                }
                // Read, and check.
                _ => {
                    let guard = tree.guard();
                    let Ok(plan) = tree.plan(map, tool) else {
                        continue;
                    };
                    // Same self-aiming read as the observer's, for the same
                    // reason: `aim` tracks whatever window the arena actually
                    // holds, which after a writer is killed is not `now`.
                    let at = aim - rng.below(2_000_000) as i64;
                    match plan.at::<tf_tree::SystemDomain>(&guard, Stamp::from_nanos(at)) {
                        Err(tf_tree::LookupError::Extrapolation { newest, oldest, .. }) => {
                            aim = newest.max(oldest);
                        }
                        Err(_) => {}
                        Ok(iso) => {
                            if let Err(why) = Invariant::check(&iso) {
                                eprintln!(
                                    "VIOLATION pid {} map->tool at {at}: {why} (iso = {iso:?})",
                                    std::process::id()
                                );
                                // Exit immediately: the arena has already told this
                                // process something impossible, and every further
                                // read would report the same corruption again.
                                std::process::exit(EXIT_VIOLATION);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// A random rigid transform — or, under `--inject-violation`, one that is
    /// not rigid at all.
    ///
    /// The injected corruption is a **NaN translation**, not a denormalized
    /// quaternion. Both violate [`Invariant`], but only one of them survives the
    /// read path: `LerpSlerp` renormalizes the quaternion it interpolates, so a
    /// non-unit quaternion pushed here comes back unit and the self-test would
    /// pass while proving nothing. NaN propagates through every arithmetic
    /// operation between here and the reader, which is exactly the property that
    /// makes it a usable canary.
    fn sample(rng: &mut Rng, inject: bool) -> Iso3 {
        let xi = [
            rng.unit() - 0.5,
            rng.unit() - 0.5,
            rng.unit() - 0.5,
            (rng.unit() - 0.5) * 0.4,
            (rng.unit() - 0.5) * 0.4,
            (rng.unit() - 0.5) * 0.4,
        ];
        let iso = tf_tree::exp_se3(xi);
        if !inject {
            return iso;
        }
        let mut bits = iso.to_bits();
        bits[4] = f64::NAN.to_bits(); // translation x
        Iso3::from_bits(&bits)
    }
}
