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
//! * Continuous invariant checking — yes, see `Invariant`.
//!
//! …with one boundary on the second bullet that is worth stating before anyone
//! quotes this as "we kill anything, anywhere": **the killed processes are the
//! joiners, never the rendezvous owner.** The driver creates and serves the
//! arena and is not a candidate victim, because `docs/PHASE2.md` §3.5's takeover
//! is not wired into `tf_tree::open` yet and a run that kills the owner spends
//! the rest of its life in `ArenaHeldButUnreachable` proving nothing.
//! `imp::attach_observer` carries the measurement and the one-line change that
//! reverses this when §3.5 lands.
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
//! process validates every read unconditionally (see `Invariant`), which is
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
//! and exits `EXIT_VIOLATION`. The driver distinguishes that from the exit of
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
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};
    use tf_tree::{
        AttachMode, Capacity, EdgeCfg, EdgeId, Guard, InterpPolicy, Iso3, Plan, Stamp, Tree,
        TreeBuilder,
    };
    use tf_tree_ipc::CreatePolicy;

    /// A child exits with this after printing a `VIOLATION` line. Distinct from
    /// 1 (an ordinary error, which is expected and retried) and from the
    /// signalled exit of a child the driver killed.
    const EXIT_VIOLATION: i32 = 3;

    /// The fewest composed `map -> tool` reads an observation round must
    /// validate for the run's verdict to mean anything.
    ///
    /// Each round *attempts* 256 (see [`observe`]), so this is a 6% success
    /// rate, and it is set that low on purpose: the chain is readable only while
    /// all four rings overlap, and a round that lands entirely inside the gap
    /// left by a freshly killed writer legitimately reads nothing. What the
    /// floor is written against is the *systematic* zero — a reader that stopped
    /// finding the window at all — not one unlucky round.
    ///
    /// Measured over seven seeds in each of two configurations — 8 s with 4
    /// children at 4 Hz, and 15 s with 6 children at 6 Hz — the worst of the
    /// fourteen averaged **248** composed reads per round, 15x this floor. Under
    /// `just shm-torture-asan`, which is the slowest way this binary runs, 256.
    /// Against the harness this replaced, where a *child* owned the rendezvous,
    /// seed 999 in the second of those configurations averaged exactly 0 and
    /// exited `PASS`.
    const MIN_CHAIN_READS_PER_ROUND: u64 = 16;

    /// The same floor for single-edge reads, of which each round attempts 64.
    ///
    /// A single edge needs only its own ring to be non-empty, which is true
    /// unless every writer of that edge has been dead for longer than the ring
    /// covers — so all fourteen runs above hit the full 64 per round, 8x this.
    const MIN_EDGE_READS_PER_ROUND: u64 = 8;

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
    /// violations having validated almost nothing, which is
    /// indistinguishable from a clean run.
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
        readers_only: bool,
    }

    /// `30m`, `120s`, `500ms`, `1h`, or a bare number of seconds.
    ///
    /// **`m` and `h` are here because the nightly is spelled in minutes.**
    /// `docs/PHASE2.md` §13 says "30 minutes" and `just shm-torture` defaults to
    /// `--duration 30m`; a parser that knows only `s` and `ms` rejects that with
    /// `invalid float literal` while every seconds-based test still passes, so
    /// the one command this binary exists for is the only thing that breaks.
    fn parse_duration(s: &str) -> Result<Duration> {
        // `ms` first: it ends in `s`, so the seconds arm would eat it.
        if let Some(v) = s.strip_suffix("ms") {
            return Ok(Duration::from_millis(v.parse()?));
        }
        for (suffix, scale) in [("h", 3600.0), ("m", 60.0), ("s", 1.0)] {
            if let Some(v) = s.strip_suffix(suffix) {
                return Ok(Duration::from_secs_f64(v.parse::<f64>()? * scale));
            }
        }
        Ok(Duration::from_secs_f64(s.parse()?))
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
            readers_only: false,
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
                // **The negative control for the read floor**, and the reason
                // it is a flag rather than a mocked-out reader: children that
                // attach and read but never claim or publish leave every ring
                // empty, so the observer validates exactly nothing — and a run
                // that validated nothing is the one state this harness used to
                // report as `PASS`. `just shm-torture-self-test` runs it and
                // asserts the run *fails*. It measures the harness, never the
                // arena, so it has no place in a real soak.
                "--readers-only" => a.readers_only = true,
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
                         [--kill-hz 4] [--inject-violation] [--readers-only]"
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

    fn spawn(
        exe: &std::path::Path,
        dir: &PathBuf,
        seed: u64,
        inject: bool,
        readers_only: bool,
    ) -> Result<Kid> {
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
        if readers_only {
            cmd.arg("--readers-only");
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

        // **The driver owns the rendezvous, and it comes up first.** Three
        // roles, all load-bearing:
        //
        // 1. It serves the socket, so a child killed at any moment leaves a
        //    rendezvous somebody still answers (see [`attach_observer`] for why
        //    that cannot be a child).
        // 2. It keeps the segment alive. Without it the last kill frees the
        //    arena with its final mapping, and `check_recovery` has nothing to
        //    check — which is exactly what an earlier revision did, printing a
        //    green "PASS" whose recovery half had silently skipped every run.
        // 3. It is a reader that is never killed, so §11.4's "invariants checked
        //    continuously" does not depend on which child happened to survive.
        let observer = attach_observer()?;

        // Kids are held in fixed slots, not a list, so "slot 0 injects" survives
        // slot 0 being killed and replaced.
        let mut kids: Vec<Option<Kid>> = Vec::with_capacity(a.children);
        for i in 0..a.children {
            // Only *one* slot injects, and only when asked: a corrupt sample
            // from every writer would let a child detect its own corruption,
            // which proves nothing about what crosses a process boundary.
            kids.push(Some(spawn(
                &exe,
                &dir,
                rng.next_u64(),
                a.inject && i == 0,
                a.readers_only,
            )?));
        }

        let deadline = Instant::now() + a.duration;
        let mut kills = 0usize;
        let mut reads = Reads::default();
        let mut rounds = 0u64;
        let mut violations = Vec::new();
        let interval = Duration::from_secs_f64(1.0 / a.kill_hz);

        while Instant::now() < deadline {
            // Jitter, so the kills do not land in phase with any loop a child
            // runs. An in-phase killer reaches one point in the protocol over
            // and over and calls the other points covered.
            let jitter = 0.5 + rng.unit();
            // `saturating_duration_since`, not `deadline - Instant::now()`:
            // `Instant`'s `Sub` panics when the operand is later, and the clock
            // can cross the deadline between the `while` test and this line. A
            // 30-minute soak that panics on its last iteration would report a
            // crash in the harness as a failure of the arena.
            let left = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(interval.mul_f64(jitter).min(left));

            reads.add(observe(&observer, &mut rng, &mut violations));
            rounds += 1;
            reap_finished(&mut kids, &mut violations);
            if !violations.is_empty() {
                break;
            }
            for (slot, kid) in kids.iter_mut().enumerate() {
                if kid.is_none() {
                    let seed = rng.next_u64();
                    *kid = Some(spawn(
                        &exe,
                        &dir,
                        seed,
                        a.inject && slot == 0,
                        a.readers_only,
                    )?);
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
        reads.add(observe(&observer, &mut rng, &mut violations));
        rounds += 1;
        reap_finished(&mut kids, &mut violations);
        // Kill every remaining child *before* the recovery check: "no claim is
        // held by a dead participant" is only a statement about a quiescent
        // arena, and a live writer would fail it correctly and uselessly.
        //
        // **Signal them all, then wait.** Dropping the vector kills and waits
        // one child at a time, and the children reap on 3% of their operations
        // — so the survivors clean up after the ones already dead and
        // `check_recovery` inspects an arena somebody else already recovered,
        // with nothing left for its own `reap_dead` to do. Killing the whole set
        // first leaves the claim word of every writer that held one at that
        // instant, which is the state §11.4's recovery clause is about.
        for kid in kids.iter_mut().flatten() {
            let _ = kid.proc.kill();
        }
        for kid in kids.iter_mut().flatten() {
            let _ = kid.proc.wait();
        }
        drop(kids);
        std::thread::sleep(Duration::from_millis(100));

        let recovery = check_recovery(&observer);
        drop(observer);
        drop(scratch);

        println!(
            "shm_torture: {} checked reads from the observer",
            reads.total()
        );
        println!(
            "shm_torture:   {} composed map->tool, {} single-edge, over {rounds} rounds",
            reads.chain, reads.edge
        );

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
        // **A run that validated nothing must not print PASS.** This is the
        // guard the harness shipped without, and the reason is worth stating:
        // every other outcome of this binary — a corrupt read, a leaked claim, a
        // dead child — is a thing that *happened*, while "the observer never
        // managed a single lookup" is a thing that did not, and the two printed
        // the same verdict. A 30-minute nightly that quietly stops reading is
        // strictly worse than no nightly, because it also occupies the slot
        // where somebody would have noticed.
        //
        // The floor is per observation round, not absolute, so it scales with
        // `--duration` and `--kill-hz` instead of being a number tuned to one
        // recipe. `MIN_*_READS_PER_ROUND` says what the two shapes cost.
        let want_chain = rounds * MIN_CHAIN_READS_PER_ROUND;
        let want_edge = rounds * MIN_EDGE_READS_PER_ROUND;
        let vacuous = reads.chain < want_chain || reads.edge < want_edge;
        // The self-test's other half, checked **before** the floor because it is
        // the more specific diagnosis: `--inject-violation` publishes a NaN for
        // the whole run and the expected outcome is a failure, so reaching this
        // line at all is a defect. Which defect it is depends on the counts, so
        // the message carries them rather than making the reader go look.
        if a.inject {
            bail!(
                "--inject-violation ran to completion with 0 violations: a child published \
                 a NaN translation for the whole run and no reader — not the observer, not \
                 a sibling — reported it. The observer validated {} composed and {} \
                 single-edge transforms over {rounds} rounds, so {}",
                reads.chain,
                reads.edge,
                if vacuous {
                    "the run read too little to conclude anything: fix the reader first"
                } else {
                    "it read plenty and the detector is what failed"
                }
            );
        }
        if vacuous {
            bail!(
                "the observer validated {} composed and {} single-edge transforms over \
                 {rounds} rounds, under the floor of {want_chain}/{want_edge}. This run \
                 proves nothing: `0 violation(s)` is also what a harness that never read \
                 anything prints. Something is wrong with the reader (see \
                 `common_window`) or with the rendezvous, not necessarily with the arena.",
                reads.chain,
                reads.edge
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

    /// Create and serve the arena the children will join, and hold a reader on
    /// it for the whole run.
    ///
    /// # Why the *driver* owns it, and what that costs
    ///
    /// An earlier revision had the driver join with [`CreatePolicy::Never`] and
    /// let the children race to create, on the argument that the create race is
    /// worth exercising. It is — but it is exercised by
    /// `crates/tf_tree/tests/rendezvous.rs` and `tf_tree_ipc`'s multiprocess
    /// suite, and here it cost the entire rest of the run:
    ///
    /// **`docs/PHASE2.md` §3.5's takeover is not wired into `tf_tree::open`.**
    /// `crates/tf_tree/src/open.rs`'s module documentation says so in as many
    /// words: a participant noticing the owner died and promoting itself "is not
    /// here", it needs a watcher on the client socket. So when the owner is a
    /// child and the driver kills it, *nothing* takes over: no process is
    /// serving, and every joiner that wins the ownership byte is then turned
    /// away by §3.4's split-brain check, because the driver's own participant
    /// byte is held. Measured, before this changed: children failing `open()`
    /// with `ArenaHeldButUnreachable { holder_slots: 4, first_pid: <the driver> }`
    /// after the full 2 s timeout, over and over, while the driver read an arena
    /// nobody was left to publish to. The run then reported `0 violations` and
    /// `PASS` having validated nothing at all — including with a child
    /// publishing NaN throughout.
    ///
    /// So this harness kills **joiners, never the owner**, and says so rather
    /// than quietly covering less than its name suggests. Killing the owner is
    /// worth testing and is *not tested here*; it becomes testable when §3.5's
    /// watcher lands, and the change at that point is one line — spawn a child
    /// as the owner again and let the driver join with `Never`.
    fn attach_observer() -> Result<Tree> {
        tf_tree::Open::new()
            .mode(AttachMode::ReadWrite)
            .create(CreatePolicy::IfAbsent)
            .layout_if_creating(layout())
            .timeout(Duration::from_secs(5))
            .open()
            .context("the driver could not create the torture arena")
    }

    /// How many transforms one observation round validated.
    #[derive(Debug, Default, Clone, Copy)]
    struct Reads {
        /// `map -> tool`, composed over all four edges at one stamp.
        chain: u64,
        /// Single edges, each read inside its own retained window.
        edge: u64,
    }

    impl Reads {
        fn total(self) -> u64 {
            self.chain + self.edge
        }
        fn add(&mut self, other: Reads) {
            self.chain += other.chain;
            self.edge += other.edge;
        }
    }

    /// One chain edge's compiled plan and the `[oldest, newest]` it retains.
    struct EdgeWindow {
        /// Index into [`CHAIN`], so a violation names the edge it came from.
        which: usize,
        plan: Plan,
        oldest: i64,
        newest: i64,
    }

    /// Probe every chain edge for the window it currently retains.
    ///
    /// **The probe is a deliberate `Extrapolation`.** `Plan::at` is the only
    /// public way to learn what a ring holds, and it reports `oldest`/`newest`
    /// exactly when it refuses. An hour past the shared clock is beyond anything
    /// a writer paced at ~1 kHz can have published, so the probe never
    /// accidentally succeeds and never disturbs what it measures.
    ///
    /// Returns **fewer** than `CHAIN.len()` entries when an edge is empty
    /// ([`tf_tree::LookupError::NoData`], no writer has ever claimed it) or
    /// unreadable; callers must treat that as "no common window" rather than
    /// intersect what came back.
    fn edge_windows(tree: &Tree, guard: &Guard<'_>) -> Vec<EdgeWindow> {
        let probe = Stamp::from_nanos(now_nanos().saturating_add(3_600_000_000_000));
        let mut out = Vec::with_capacity(CHAIN.len());
        for (which, (parent, child)) in CHAIN.iter().enumerate() {
            let (Ok(p), Ok(c)) = (tree.frame(parent), tree.frame(child)) else {
                continue;
            };
            let Ok(plan) = tree.plan(p, c) else {
                continue;
            };
            if let Err(tf_tree::LookupError::Extrapolation { oldest, newest, .. }) =
                plan.at::<tf_tree::SystemDomain>(guard, probe)
            {
                out.push(EdgeWindow {
                    which,
                    plan,
                    oldest,
                    newest,
                });
            }
        }
        out
    }

    /// The stamps every chain edge can answer *at once*, if there are any.
    ///
    /// **An intersection, not a hill climb**, and the honest reason is narrower
    /// than it looks. The reader this replaced asked at `now` and, on
    /// `Extrapolation`, re-aimed at the window the failing edge reported. That
    /// is a search, and `map -> tool` needs one stamp inside all four windows at
    /// once, so it can oscillate between two disjoint ones; an intersection
    /// cannot. But it is **not** what made this harness validate nothing — that
    /// was the rendezvous (see [`attach_observer`]). Measured with the
    /// rendezvous fixed and the hill climb restored: 12 967 composed reads over
    /// 8 s at seed 999, against 12 800 for this code. So the property here is
    /// structural — an intersection has no state to get stuck in — and no mutant
    /// of it is fatal to the suite. What the per-edge probing buys that the hill
    /// climb cannot is the single-edge reads in [`observe`], which keep
    /// validating while the four windows are apart.
    ///
    /// `None` means the windows are disjoint *right now*, which is a normal
    /// state a few hundred milliseconds after a writer was killed — not an
    /// error. It is the run-level floor in [`drive`], not this function, that
    /// refuses a run where they were disjoint the whole time.
    fn common_window(windows: &[EdgeWindow]) -> Option<(i64, i64)> {
        if windows.len() != CHAIN.len() {
            return None;
        }
        let lo = windows.iter().map(|w| w.oldest).max()?;
        let hi = windows.iter().map(|w| w.newest).min()?;
        (lo <= hi).then_some((lo, hi))
    }

    /// A stamp inside `[lo, hi]`, so consecutive reads are different queries and
    /// the interpolation *between* two slots is exercised rather than one exact
    /// hit repeated.
    fn pick(rng: &mut Rng, (lo, hi): (i64, i64)) -> i64 {
        let span = hi.saturating_sub(lo);
        if span <= 0 {
            lo
        } else {
            lo.saturating_add(rng.below(span as u64 + 1) as i64)
        }
    }

    /// A burst of checked reads from the never-killed observer.
    ///
    /// Returns how many transforms were validated, which the run prints and
    /// [`drive`] enforces a floor on. A count is not decoration here: "0
    /// violations" and "0 reads" print the same verdict, and only one of them is
    /// a result.
    ///
    /// Both shapes are read. The composed `map -> tool` is the one §11.4 is
    /// about — a bad sample on any edge reaches it — but it needs all four
    /// windows to overlap. The per-edge reads need only that edge's own ring, so
    /// they keep validating across the moments when the chain cannot be read at
    /// all, and they still see the injected corruption: the injector writes NaN
    /// to whichever single edge it holds.
    fn observe(tree: &Tree, rng: &mut Rng, violations: &mut Vec<String>) -> Reads {
        let mut reads = Reads::default();
        let guard = tree.guard();
        let windows = edge_windows(tree, &guard);

        for w in &windows {
            let (parent, child) = CHAIN[w.which];
            for _ in 0..16 {
                let at = pick(rng, (w.oldest, w.newest));
                if let Ok(iso) = w
                    .plan
                    .at::<tf_tree::SystemDomain>(&guard, Stamp::from_nanos(at))
                {
                    reads.edge += 1;
                    if let Err(why) = Invariant::check(&iso) {
                        violations.push(format!(
                            "the observer read a bad transform: {parent}->{child} at {at}: {why}"
                        ));
                        return reads;
                    }
                }
            }
        }

        let (Ok(map), Ok(tool)) = (tree.frame(CHAIN[0].0), tree.frame(CHAIN[CHAIN.len() - 1].1))
        else {
            return reads;
        };
        let Ok(plan) = tree.plan(map, tool) else {
            return reads;
        };
        let Some(window) = common_window(&windows) else {
            return reads;
        };
        for _ in 0..256 {
            let at = pick(rng, window);
            if let Ok(iso) = plan.at::<tf_tree::SystemDomain>(&guard, Stamp::from_nanos(at)) {
                reads.chain += 1;
                if let Err(why) = Invariant::check(&iso) {
                    violations.push(format!(
                        "the observer read a bad transform: map->tool at {at}: {why}"
                    ));
                    return reads;
                }
            }
        }
        reads
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

        let me = tree.participant_slot();
        let view = tree.arena_view();

        // **Count what the dead left before reclaiming it.** `reap_dead` returns
        // how many claims it reclaimed, and that number alone cannot distinguish
        // "there was nothing to reclaim" from "this step did not run" — an
        // earlier revision printed `reaped 0` on every run and read like a step
        // that had worked. The owner word is the state reaping acts on, so it is
        // what the note reports: `stale` is the arena's own record of claims held
        // by participants that are all now dead, and `reaped` is how many of them
        // came back.
        //
        // Anything non-zero here is a killed writer's claim, because every child
        // has been waited for and this process holds none.
        let mut stale = 0usize;
        for edge in 0..view.header().max_edges {
            let Some(rec) = view.claim(EdgeId(edge)) else {
                continue;
            };
            if rec.owner.load(Ordering::Acquire) != 0 {
                stale += 1;
            }
        }

        // Reap *after* the count and *before* the claim probe below: a claim held
        // by a process the kernel has already cleaned up is reapable, not leaked,
        // and the distinction is the whole of A3. Leaving it un-reaped and calling
        // it a leak would fail the property the design actually promises.
        let reaped = tree.reap_dead();
        out.notes.push(format!(
            "recovery: {stale} edge(s) still carried a killed writer's claim word; \
             reap_dead reclaimed {reaped}"
        ));

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
        let mut readers_only = false;
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
                "--readers-only" => readers_only = true,
                other => bail!("child: unknown argument `{other}`"),
            }
        }
        let mut rng = Rng::new(seed);

        loop {
            // `Never`, in every child: the driver creates and serves the arena
            // (see [`attach_observer`]), so there is always one to join, and a
            // child that created a second one would silently split the run in
            // two — half the participants publishing where the observer cannot
            // see them, which is a *green* run that validates nothing. `Never`
            // makes that state a failed `open()` this loop retries instead.
            let tree = match tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::Never)
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
            work(&tree, &mut rng, inject, readers_only)?;
        }
    }

    /// The random-operation loop against one attachment. Returns when it decides
    /// to detach and re-join, which is §11.4's "attach/detach".
    fn work(tree: &Tree, rng: &mut Rng, inject: bool, readers_only: bool) -> Result<()> {
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

        let mut held: Option<tf_tree::EdgeWriter<'_>> = None;
        // A bounded number of operations per attachment, so every child
        // re-attaches regularly instead of one lucky survivor holding the arena
        // for the whole run.
        for _ in 0..2_000 {
            // **Pacing, and it is not politeness.** A 64-slot ring filled by an
            // unthrottled loop covers about nine *microseconds* of history, and
            // six children spinning on `push` is a busy-wait on every core the
            // scheduler will give them — so the kills all land inside one hot
            // loop instead of spreading across the protocol, which is the only
            // thing this harness is for. At ~1 kHz the same ring covers ~64 ms.
            //
            // It is **not** what keeps the reader fed: `observe` probes each
            // ring for its window immediately before reading it, so removing
            // this leaves the read counts unchanged (measured: 11 989 composed
            // reads over 8 s, against 12 800 with it). That was not true of the
            // reader this replaced, and `tests/torture.rs` records the change.
            std::thread::sleep(Duration::from_micros(200 + rng.below(1_600)));
            match rng.below(100) {
                // Claim an edge, if we hold none.
                0..=9 => {
                    if held.is_none() && !readers_only {
                        let i = rng.below(CHAIN.len() as u64) as usize;
                        let (p, c) = ids[i];
                        // A refused claim is the correct answer when somebody
                        // else holds it. Only a *granted* one is interesting.
                        if let Ok(w) = tree.claim(c, p) {
                            held = Some(w);
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
                    if let Some(w) = &held {
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
                    // The observer's window arithmetic, for the same reason: a
                    // stamp that all four rings can answer is an intersection of
                    // what they retain, and after a writer is killed that is
                    // nowhere near `now`. Four extra probe lookups per read op
                    // cost nothing against the ~1 ms pacing above.
                    let windows = edge_windows(tree, &guard);
                    let Some(window) = common_window(&windows) else {
                        continue;
                    };
                    let Ok(plan) = tree.plan(map, tool) else {
                        continue;
                    };
                    let at = pick(rng, window);
                    if let Ok(iso) = plan.at::<tf_tree::SystemDomain>(&guard, Stamp::from_nanos(at))
                    {
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
