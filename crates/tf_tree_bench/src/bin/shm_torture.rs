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
//! §11.4 asks for four things, and **the first three are here:**
//!
//! * N processes doing random attach/detach/claim/reap/push/lookup — yes.
//! * Random `SIGKILL` at 1–10 Hz — yes, and it is the whole point: a killed
//!   process cannot clean up, so every claim, participant slot and lock byte it
//!   held has to be recovered by somebody else with no cooperation from it.
//!   **Since 2026-09-04 that includes the rendezvous owner** — see the section
//!   below, and `kill_the_owner`.
//! * A random crash point armed in ~10% of children — yes, under
//!   `--crash-points`, which needs `--features crash-points`. See
//!   `armed_site` and the reachability section below for which of §11.3's
//!   thirteen sites this workload can actually reach, which is not all of them.
//! * Continuous invariant checking — **two of §11.4's four clauses.** The
//!   quaternion/NaN clause is checked on every read in every process
//!   (`Invariant`); the two-writers clause is checked on every push by the
//!   writer that holds the edge (`work`'s `two_writers` arm). The other two
//!   are not: slot leakage is checked once, at teardown (`check_recovery`),
//!   and "the arena hash is stable across quiescent points" is not implemented
//!   at all — there is no safe accessor for the arena's bytes from a
//!   `forbid(unsafe_code)` crate, so it needs a new public API and therefore a
//!   decision record. `docs/PHASE2.md` §0.0's §11.4 row says so where a reader
//!   will meet it.
//!
//! **Three claims that stood in this header until 2026-09-04 were false, and
//! are corrected rather than deleted**, because each one was a reason somebody
//! could have given for not doing the work:
//!
//! 1. *"the killed processes are the joiners, never the rendezvous owner … §3.5's
//!    takeover is not wired into `tf_tree::open` yet"*. §3.5's ownership
//!    migration landed on **2026-08-28** (`Tree::owner_lost`,
//!    `Tree::inherit_ownership`, `Session::take_over_ownership`), and this
//!    harness now kills the owner on a schedule and requires a survivor to
//!    inherit.
//! 2. *"Crash points are §11.3's `crash-points` feature, which §0.0 records as
//!    **not implemented** — there is no `TF_TREE_CRASH_AT` to arm"*. The feature
//!    and the variable shipped on **2026-08-29**; `--crash-points` has armed
//!    them since. What survives from that paragraph is the part that was never
//!    about implementation status, and it is kept below.
//! 3. *"the eleven named mid-protocol states §11.3 enumerates"*. §11.3's table
//!    has fourteen rows, **thirteen** of which carry a site; the fourteenth
//!    (`reclaim.probe_then_reoccupied`) names an interleaving between two live
//!    processes and is deliberately not an abort site.
//!
//! **`SIGKILL` is still not §11.3 coverage**, and that distinction has nothing
//! to do with what is implemented: a signal lands wherever the scheduler puts
//! it, which is a different and much shallower set of mid-protocol states than
//! the named ones. A run without `--crash-points` must not be quoted as §11.3.
//!
//! # Which of §11.3's thirteen sites this workload can reach
//!
//! A site armed in a process that never executes the instruction is a draw
//! spent on nothing, and `armed N, aborted 0` reads like a scheduling problem
//! rather than a structural one. **Measured** 2026-09-04, not reasoned: each
//! site was forced in every child with `--crash-site NAME:1` over
//! `--duration 12-15s --children 4 --kill-hz 1`, and the run's own armed/fired
//! line is the result. **Twelve of the thirteen fire; one does not.** The
//! interesting split is not that one, though — it is that "fires" and
//! "exercises the row's repair claim" are two different things:
//!
//! | Fires, in a live arena, and the state it leaves is met by live peers | Where |
//! |---|---|
//! | `push.after_seq_odd`, `push.after_data_before_seq_even`, `push.after_seq_even_before_head` | `work` publishes on 40% of its operations |
//! | `claim.after_cas` | `work` claims |
//! | `attach.after_slot_assigned_before_publish` | every join, and children re-join constantly |
//! | `reclaim.after_probe_before_cas` | `work`'s `reap_participants` arm, added with the owner kill because a migration orphans the hangup collector (see `check_recovery`) |
//! | `hangup.after_probe_before_cas` | the owner is a **child** now, so it is armed like any other, and a joiner hanging up drives its callback. It needed `reap_owner` as well: the owner is not a worker slot, so an owner that aborted mid-run was not counted and this site read `20 armed, 0 aborted` while it had in fact fired |
//! | `takeover.after_ownership_lock_before_bind` | inside `Tree::inherit_ownership`, which every survivor calls |
//!
//! | Fires only in the **creating** owner child, and the row's claim is still what gets exercised | Where |
//! |---|---|
//! | `open.after_ownership_lock_before_bind`, `open.after_create_before_bind` | inside the `OpenOutcome::Created` arm. §11.3's rows for both are about what the *next* `open()` finds, and `spawn_owner`'s retry is that next `open()` — driven as a run rather than staged |
//!
//! | Fires only in the creating owner child, where the state the row names is **not produced** | Why |
//! |---|---|
//! | `topo.after_copy_before_publish` | `TreeBuilder::build_with` calls `set_parent`, so a creator armed here aborts. But the row is about "inactive block dirty, word unchanged → no observable effect (A1)" *in a live arena*, and this abort destroys the arena being built. `spawn_owner` retries and a fresh one is created; nothing observes the state |
//! | `intern.after_hash_cas_before_id_store` | same shape: the owner child interns all five chain names at build time, so it is the first interner. The row is about the *next* interner spinning and recovering under A8, and there is no next interner of a name whose arena never existed |
//!
//! | Never fires | Why |
//! |---|---|
//! | `topo.holding_lock` | inside `Tree::reparent`, and nothing here reparents. The topology is a fixed four-edge chain and a participant that reparented it would destroy the property every other check reads. Confirmed by probe: `16 armed, 0 aborted` |
//!
//! So **"every §11.3 crash point recovers" is not a claim this binary can
//! make**, at any duration: one site never fires and two fire somewhere the
//! state they name cannot exist. That is a statement about this workload and not
//! about the sites — every one of the thirteen has a targeted test in
//! `tf_tree_core::crash_tests` or `tf_tree/tests/rendezvous.rs`, which is where
//! §11.3's per-site coverage lives. The run prints the sites it armed and the
//! sites that fired, so this table is checkable from any run rather than trusted.
//!
//! **A forced `--crash-site` can fail a run for the probe's own reasons.**
//! Arming *every* child at `hangup.after_probe_before_cas:1` makes each new
//! owner abort on the first participant hangup, so the role churns faster than
//! the driver can follow it and the migration check fails. That is the probe
//! being a probe; §11.4's configuration is a random site in a tenth of children,
//! which is what `--crash-points` alone does.
//!
//! # Killing the owner (§3.5), and why the driver stopped being it
//!
//! The driver used to create and serve the arena, which made the owner
//! structurally unkillable and left §12.3 gate 3's "the owner dies mid-run"
//! unmet. It now spawns an **owner child** that creates and serves and does
//! nothing else, joins with `CreatePolicy::Never` itself, and `SIGKILL`s
//! whichever process currently holds the role every `--owner-kill-every`.
//!
//! Every child is a potential heir: `work` evaluates `Tree::owner_lost` in
//! its own loop and calls `Tree::inherit_ownership` when it answers true, which
//! is §3.5's caller-driven trigger used exactly as specified — there is no
//! daemon and no background thread, so *the participants are the callers*. Two
//! things are then required of every owner kill, and a run where either fails
//! is a failed run rather than a quiet one:
//!
//! * **a fresh process can join again**, probed from the outside with a real
//!   `Open::new().create(Never)`. That is the property that was broken between
//!   2026-08-27 and 2026-08-28 and the whole point of §3.5: an ownerless arena
//!   refuses a new joiner with `ArenaHeldButUnreachable` while every already
//!   attached process keeps reading, so an internal flag would not have
//!   detected it and this probe does; and
//! * **some survivor recorded an inheritance**, which is what says the fresh
//!   join succeeded because §3.5's trigger ran rather than because the role was
//!   never vacant.
//!
//! `--no-inherit` is the negative control: children skip the `owner_lost` call,
//! nothing inherits, and the run must **fail**. `tests/torture.rs` asserts that,
//! for the same reason `--readers-only` exists one level down.
//!
//! The replacement for a killed owner **rejoins as an ordinary participant**,
//! deliberately: the role is inherited, and a second process opening with
//! `IfAbsent` after a migration would either join as a plain participant (doing
//! nothing) or, worse, create a second arena and split the run in half. So the
//! owner role exists exactly once, at startup, and is inherited from there on.
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
    use std::io::{BufRead, BufReader, Write as _};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};
    use tf_tree::{
        AttachMode, Capacity, EdgeCfg, EdgeId, Guard, Inheritance, InterpPolicy, Iso3, Plan, Stamp,
        Tree, TreeBuilder,
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
    ///
    /// # Half of that calibration is refuted, and the floor stays anyway
    ///
    /// The 15 s / 6 children / 6 Hz half was measured against a wedged arena.
    /// Before the owner-side fix in `crates/tf_tree/src/open.rs`, that
    /// configuration exhausted the 64-slot participant table about ten seconds
    /// in (see [`check_recovery`]), so from two thirds of the way through there
    /// was no writer left — and the composed count does not notice, because four
    /// frozen rings whose windows overlap answer every lookup. Measured
    /// 2026-08-17 on the unfixed build, `--duration 60s --children 6
    /// --kill-hz 6 --seed 4242`: **93 689 of 93 952 composed reads, 99.7%, on a
    /// run whose arena had a live writer on 21% of its rounds** and whose
    /// freshest sample averaged 16.9 seconds old. So "248 per round" was never
    /// evidence that the reader worked; it is equally compatible with the reader
    /// working perfectly and nothing being under test.
    ///
    /// The floor is **not** lowered on the strength of that, because nothing
    /// about it was too high — it is a *lower* bound on reading, and reading is
    /// still required. What was missing is the orthogonal bound:
    /// [`RoundHealth::arena_is_live`], which asks whether anybody was writing.
    /// A number that cannot distinguish a healthy run from a dead one is not
    /// corrected by moving it.
    const MIN_CHAIN_READS_PER_ROUND: u64 = 16;

    /// The same floor for single-edge reads, of which each round attempts 64.
    ///
    /// A single edge needs only its own ring to be non-empty, which is true
    /// unless every writer of that edge has been dead for longer than the ring
    /// covers — so all fourteen runs above hit the full 64 per round, 8x this.
    const MIN_EDGE_READS_PER_ROUND: u64 = 8;

    /// How long after start-up the first owner kill lands, and how long a
    /// migration is given before the run is failed.
    ///
    /// **Four seconds, so the rings are full when the role first goes vacant.**
    /// An owner killed before any writer has published leaves nothing for the
    /// data-plane half of the check to observe, and the run would then report
    /// "reads did not stop" about an arena nobody was reading anyway.
    ///
    /// It is also what keeps the two shortest self-tests in `tests/torture.rs`
    /// meaning what they meant: the 3 s `--readers-only` case never reaches an
    /// owner kill, and the 8 s clean case reaches exactly one.
    const OWNER_KILL_FIRST: Duration = Duration::from_secs(4);

    /// Ten seconds, matching `owner_migration`'s deadline for the same probe.
    ///
    /// **This is not a latency budget.** `docs/PHASE2.md` §12.2's row measures
    /// that, on an idle host, through `just owner-migration`, and reports 0.6–1.2
    /// ms p50. This number is only "long enough that a loaded CI runner is not
    /// the reason a run goes red", because the failure it is written against —
    /// nothing inherited at all — never recovers however long it waits.
    const OWNER_RECOVERY_DEADLINE: Duration = Duration::from_secs(10);

    /// How often the observer reads while a migration is in flight.
    ///
    /// **Throttled on purpose.** Each observation is a round, and the run-level
    /// read floor is per round, so polling the fresh-join at 1 kHz *and*
    /// observing at 1 kHz would add hundreds of near-empty rounds to a
    /// millisecond-long event and drag the floor down with them.
    const MIGRATION_OBSERVE_EVERY: Duration = Duration::from_millis(5);

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
        crash_points: bool,
        /// How often the process that currently holds the rendezvous is
        /// `SIGKILL`ed. `None` disables the arm (`--no-kill-owner`).
        owner_kill_every: Option<Duration>,
        /// The negative control for §3.5: children never call
        /// `Tree::owner_lost`, so nothing inherits and the run must fail.
        no_inherit: bool,
        /// `--crash-site NAME`: arm **this** §11.3 site in every child instead
        /// of drawing one at random in a tenth of them.
        ///
        /// A reachability probe, not §11.4's configuration: §11.4 asks for a
        /// random site in 10% of children and that is what a soak runs. This
        /// answers the different question *can this workload reach site X at
        /// all*, which the header's reachability table states as a measurement
        /// and which nobody could re-run before this existed.
        crash_site: Option<String>,
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
            crash_points: false,
            inject: false,
            readers_only: false,
            // **On by default**, and that is a deliberate choice about where
            // the coverage lives rather than an oversight. `just shm-torture`
            // takes no flag for it, so an arm that had to be asked for would be
            // absent from the one command §13 names — which is how §12.3 gate 3
            // came to be unmet for the life of the harness in the first place.
            owner_kill_every: Some(Duration::from_secs(8)),
            no_inherit: false,
            crash_site: None,
        };
        let mut help = false;
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
                // **§3.5's owner death, on a schedule.** Slower than the child
                // kills by design: a migration is a control-plane event whose
                // recovery is milliseconds, and killing the owner faster than
                // survivors can notice measures the poll interval rather than
                // the protocol.
                "--owner-kill-every" => {
                    a.owner_kill_every = Some(parse_duration(&value("--owner-kill-every")?)?);
                }
                // Turns the arm off. Here so a bisect can separate a failure of
                // the owner-kill arm from a failure of everything else, and so
                // the pre-2026-09-04 topology is still runnable for comparison.
                "--no-kill-owner" => a.owner_kill_every = None,
                // **The negative control for §3.5**, and the counterpart of
                // `--readers-only`. Children skip `Tree::owner_lost` entirely,
                // so no survivor ever inherits, the arena goes ownerless on the
                // first owner kill, and no fresh process can join again. The run
                // is expected to FAIL; `tests/torture.rs` asserts that it does,
                // and asserts on the message, because "the run failed" is also
                // what a harness that fails unconditionally produces.
                "--no-inherit" => a.no_inherit = true,
                // **This used to refuse unconditionally**, on the grounds that
                // "the `crash-points` feature and `TF_TREE_CRASH_AT` are
                // recorded as not implemented in §0.0, so there is nothing to
                // arm". That was true when written and stopped being true when
                // the sites landed; the §0.0 row it cited had gone stale too.
                //
                // What survives from that refusal is the part that was never
                // about implementation status: **`SIGKILL` is not §11.3
                // coverage.** It lands wherever the scheduler puts it, which is
                // a different and much shallower set of mid-protocol states, so
                // a run *without* this flag still must not be quoted as §11.3.
                // The flag is what makes the difference real rather than
                // rhetorical.
                "--crash-points" => {
                    if !cfg!(feature = "crash-points") {
                        bail!(
                            "--crash-points needs this binary built with the `crash-points` \
                             feature: the children are this same executable, so a site that \
                             is compiled out here is compiled out in every child and the \
                             flag would arm nothing while looking like it had. Rebuild with \
                             `--features shm,crash-points`."
                        );
                    }
                    a.crash_points = true;
                }
                // **Validated against the published list, never a literal.**
                // The same argument `all_sites` carries one level down: a typo
                // in a site name would arm nothing and the probe would report
                // "unreachable" about a site it never armed, which is the
                // conclusion this flag exists to make checkable.
                // `NAME` or `NAME:nth`. The `nth` matters more than it looks:
                // a site a process reaches exactly once — `open.*` in a creator,
                // for instance — never fires at `:2`, and a probe that drew its
                // hit count at random reported four sites unreachable that were
                // not. Measured; see the header's reachability table.
                "--crash-site" => {
                    let spec = value("--crash-site")?;
                    #[cfg(feature = "crash-points")]
                    {
                        let name = site_of(&spec);
                        let sites = all_sites();
                        if !sites.contains(&name) {
                            bail!(
                                "`{name}` is not a §11.3 crash site. The build carries: {}",
                                sites.join(", ")
                            );
                        }
                    }
                    a.crash_site = Some(spec);
                }
                // **Recorded, not acted on.** This arm used to print and
                // `return Ok(())` on the spot, and arguments are processed in
                // order — so every validation below it was unreachable from any
                // command line ending in `--help`. Those are exactly the checks
                // that compare *two* flags and therefore cannot live inside an
                // arm: `shm_torture --crash-site claim.after_cas --help` exited
                // 0 having refused nothing, and the test written to red-test
                // that refusal is what found it. The in-arm refusals — a bad
                // `--duration`, `--crash-points` on a build without the feature
                // — were always reachable, because they fire where they parse.
                "-h" | "--help" => help = true,
                other => bail!("unknown argument `{other}`"),
            }
        }
        if a.children == 0 {
            bail!("--children 0 leaves nobody to torture the arena");
        }
        if a.crash_site.is_some() && !a.crash_points {
            bail!(
                "--crash-site needs --crash-points: without it the sites are compiled out and \
                 the probe would report every site unreachable while arming none of them."
            );
        }
        // **A run with one child cannot exercise §3.5 and must say so rather
        // than pass.** The owner is a child; killing it leaves `children - 1`
        // survivors, and with none of them left there is nobody to inherit —
        // the run would then report "nothing inherited" as a defect of the
        // engine when it is a defect of the population.
        if a.owner_kill_every.is_some() && a.children < 2 {
            bail!(
                "--children {} with the owner-kill arm on leaves no survivor to inherit: the \
                 owner is a child, so the property would be untestable and the run would fail \
                 for a reason that says nothing about the arena. Use --children 2 or more, or \
                 --no-kill-owner.",
                a.children
            );
        }
        if !(0.1..=100.0).contains(&a.kill_hz) {
            bail!(
                "--kill-hz {} is outside §11.4's 1-10 Hz by more than a \
                   factor of ten either way",
                a.kill_hz
            );
        }
        if help {
            println!(
                "usage: shm_torture [--duration 30s] [--children 6] [--seed N] \
                 [--kill-hz 4] [--owner-kill-every 8s] [--no-kill-owner] \
                 [--inject-violation] [--readers-only] [--no-inherit] \
                 [--crash-points] [--crash-site NAME[:nth]]"
            );
            println!(
                "  the rendezvous owner is a child and is SIGKILLed every \
                 --owner-kill-every (PHASE2 §3.5). Each kill must be followed by a fresh \
                 process joining the arena again and by a survivor recording an inheritance, \
                 or the run fails. --no-inherit is the negative control and is expected \
                 to fail."
            );
            println!(
                "  --crash-points arms PHASE2 §11.3's fault injection in ~10% of children \
                 (§11.4). Needs --features crash-points; without it the sites are compiled \
                 out and the flag is refused rather than silently arming nothing. \
                 --crash-site forces ONE site in EVERY child — a reachability probe, not \
                 §11.4's configuration."
            );
            return Ok(());
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
        /// The `TF_TREE_CRASH_AT` this child was started with, kept so a child
        /// that aborts can be reported **by site name** rather than only
        /// counted.
        ///
        /// Without it the run can say "4 of 11 armed children aborted" and
        /// cannot say at which sites, so a site this workload never reaches is
        /// indistinguishable from one whose race the driver's `SIGKILL` keeps
        /// winning — and the module doc's reachability table would be
        /// unfalsifiable from a run.
        crash_at: Option<String>,
    }

    impl Drop for Kid {
        fn drop(&mut self) {
            let _ = self.proc.kill();
            let _ = self.proc.wait();
        }
    }

    /// What a `--crash-points` run knows about §11.3, kept in one place.
    ///
    /// **Four numbers and not two**, and each pair answers a question the other
    /// cannot. `armed`/`aborted` separate "a site was armed" from "a site
    /// fired" — an armed child the driver's `SIGKILL` reached first never got
    /// there, and `armed N, aborted 0` is a run that exercised nothing. The two
    /// name lists separate "this workload cannot reach that site" from "the
    /// race went the other way", which counts alone cannot: see the
    /// reachability table in this file's header for the three sites that are
    /// structurally unreachable here.
    #[derive(Default)]
    struct CrashLedger {
        armed: usize,
        aborted: usize,
        /// Site names armed, one entry per child armed.
        armed_sites: Vec<String>,
        /// Site names that actually aborted a process.
        fired: Vec<String>,
    }

    impl CrashLedger {
        fn record_abort(&mut self, spec: Option<&str>) {
            self.aborted += 1;
            if let Some(spec) = spec {
                self.fired.push(site_of(spec).to_string());
            }
        }
    }

    /// `armed_site` where the feature exists, `None` where it does not.
    ///
    /// The `cfg` lives here rather than at the two call sites, so a build with
    /// the feature off compiles the same control flow.
    fn crash_spec(
        enabled: bool,
        forced: Option<&str>,
        rng: &mut Rng,
        ledger: &mut CrashLedger,
    ) -> Option<String> {
        #[cfg(feature = "crash-points")]
        {
            if enabled {
                let spec = armed_site(rng, forced);
                if let Some(spec) = spec.as_deref() {
                    ledger.armed += 1;
                    // The site, not the `site:nth` spec: two children armed at
                    // the same site on different hit counts are one site's worth
                    // of coverage, and the report is about coverage.
                    ledger.armed_sites.push(site_of(spec).to_string());
                }
                return spec;
            }
        }
        let _ = (enabled, forced, rng, ledger);
        None
    }

    /// The site name out of a `TF_TREE_CRASH_AT` spec (`<name>:<nth_hit>`).
    ///
    /// Split here rather than carried alongside, so there is one representation
    /// of "which site" and nothing can drift from the string the child was
    /// actually given.
    fn site_of(spec: &str) -> &str {
        spec.split_once(':').map_or(spec, |(name, _)| name)
    }

    /// Every §11.3 site this build carries, in one list.
    ///
    /// **Read from the published consts, never re-spelled.** Both
    /// `tf_tree_core::crash::SITES` and `tf_tree::CRASH_SITES` exist for exactly
    /// this — a typo in a literal here would arm nothing, and the run would look
    /// clean, which is the failure this harness exists to not have.
    ///
    /// Two lists because §11.3's table spans two crates: the mutation protocols
    /// are core's, the rendezvous is the facade's.
    #[cfg(feature = "crash-points")]
    fn all_sites() -> Vec<&'static str> {
        let mut v: Vec<&'static str> = tf_tree_core::crash::SITES.to_vec();
        v.extend_from_slice(tf_tree::CRASH_SITES);
        v
    }

    /// `docs/PHASE2.md` §11.4: "a random crash point armed in 10% of children".
    ///
    /// Returns `TF_TREE_CRASH_AT`'s value, or `None` for the other ~90%. The
    /// `nth_hit` is drawn too: a site armed at `:1` fires on the first
    /// participant that reaches it, which for a child that attaches and then
    /// loops is almost always during start-up — so the states past the first
    /// would never be sampled.
    #[cfg(feature = "crash-points")]
    fn armed_site(rng: &mut Rng, forced: Option<&str>) -> Option<String> {
        // `--crash-site` arms every child at the named site: the question it
        // asks is whether this workload reaches the site at all, and a tenth of
        // children at a random site answers it only in expectation over a soak
        // far longer than a probe.
        if let Some(site) = forced {
            // An explicit `NAME:nth` passes through unchanged; a bare `NAME`
            // draws its hit count as the §11.4 path does.
            if site.contains(':') {
                return Some(site.to_string());
            }
            let nth = 1 + rng.below(4);
            return Some(std::format!("{site}:{nth}"));
        }
        if rng.below(10) != 0 {
            return None;
        }
        let sites = all_sites();
        let site = sites[rng.below(sites.len() as u64) as usize];
        let nth = 1 + rng.below(4);
        Some(std::format!("{site}:{nth}"))
    }

    /// The runtime directory every process in this run shares.
    ///
    /// The driver creates it and passes it in `TF_TREE_RUNTIME_DIR`, which is
    /// also what the rendezvous itself keys on, so there is no second spelling
    /// of "where this run lives".
    fn runtime_dir() -> PathBuf {
        std::env::var_os("TF_TREE_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    /// Where the process that currently holds the rendezvous records its pid.
    ///
    /// # Why a file and not a pipe
    ///
    /// The driver has to know *which process to kill*, and after the first
    /// migration the owner is whichever child happened to win byte 0 — a fact
    /// that lives in the kernel and is not reported to anybody. A pipe per child
    /// would work and would cost a reader thread per child plus a protocol that
    /// has to survive the child being `SIGKILL`ed mid-line, which is the one
    /// thing every process here is guaranteed to do.
    ///
    /// **The file is evidence, never the criterion.** What decides that a
    /// migration succeeded is a *fresh process joining the arena*
    /// ([`kill_the_owner`]); this only names the next victim and records that
    /// §3.5's trigger, rather than luck, is why the join worked.
    fn owner_pid_path(dir: &Path) -> PathBuf {
        dir.join("owner.pid")
    }

    /// One line per successful inheritance, appended by the heir.
    ///
    /// Append-only rather than a counter file: two survivors can inherit in
    /// sequence within one migration window (the first is killed, the second
    /// takes over), and a counter that is read-modify-written by processes being
    /// `SIGKILL`ed would lose exactly the events this run is about.
    fn inherited_path(dir: &Path) -> PathBuf {
        dir.join("inherited.log")
    }

    /// Publish this process as the owner, atomically.
    ///
    /// `write` then `rename`, so the driver never reads a half-written pid and
    /// kills a process id that is a truncated prefix of somebody's.
    fn publish_owner_pid(dir: &Path) {
        let tmp = dir.join(format!("owner.pid.{}", std::process::id()));
        if std::fs::write(&tmp, std::process::id().to_string()).is_ok()
            && std::fs::rename(&tmp, owner_pid_path(dir)).is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    fn read_owner_pid(dir: &Path) -> Option<u32> {
        std::fs::read_to_string(owner_pid_path(dir))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Record that this process inherited the role.
    ///
    /// One `write` of one short line to an `O_APPEND` descriptor, which Linux
    /// serialises against other appenders — so concurrent heirs cannot interleave
    /// a line, and a heir `SIGKILL`ed a microsecond later has either written its
    /// line or not.
    fn record_inheritance(dir: &Path) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(inherited_path(dir))
        {
            let _ = writeln!(f, "{}", std::process::id());
        }
    }

    fn inheritance_count(dir: &Path) -> usize {
        std::fs::read_to_string(inherited_path(dir))
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }

    /// The owner child: create the arena, serve the rendezvous, and park.
    ///
    /// **It does nothing else, and that is the point.** The obvious shortcut —
    /// let the owner also publish, as this harness's driver used to — makes the
    /// owner's death stop the data stream, so §3.5's "lookups do not pause"
    /// would be measured against a writer's death rather than an owner's.
    /// `owner_migration`'s header argues the same split for the same reason.
    ///
    /// It publishes its pid **before** reporting ready, so the driver never has
    /// a window in which it knows an owner is up and does not know which process
    /// it is.
    fn owner_child() -> Result<()> {
        let tree = tf_tree::Open::new()
            .mode(AttachMode::ReadWrite)
            .create(CreatePolicy::IfAbsent)
            .layout_if_creating(layout())
            .timeout(Duration::from_secs(5))
            .open()
            .context("the owner child could not create the torture arena")?;
        publish_owner_pid(&runtime_dir());
        println!("ready");
        std::io::stdout().flush().ok();
        // **Hold the tree.** Dropping it stops serving the rendezvous; the loop
        // only keeps the binding alive. This process exists to be killed.
        let _owner = tree;
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    /// Bring up the owner child and wait for it to report ready.
    ///
    /// # It retries, and the retry is coverage rather than robustness
    ///
    /// `open.after_create_before_bind` and `open.after_ownership_lock_before_bind`
    /// live in the `OpenOutcome::Created` arm, which only a creating process
    /// takes — so before there was an owner child they could not fire in this
    /// harness at all. Now they can, and when one does the owner aborts before
    /// printing `ready`. §11.3's rows for both say what must be true afterwards:
    /// "the next `open()` proceeds", "next `open()` finds nothing alive and
    /// creates fresh". This loop is that next `open()`, driven as a run instead
    /// of staged in a test, and a retry that never succeeds is a failed run.
    fn spawn_owner(
        exe: &std::path::Path,
        dir: &PathBuf,
        rng: &mut Rng,
        crash_points: bool,
        crash_site: Option<&str>,
        ledger: &mut CrashLedger,
    ) -> Result<Kid> {
        let mut last = String::new();
        for attempt in 0..6 {
            let crash_at = crash_spec(crash_points, crash_site, rng, ledger);
            let mut cmd = Command::new(exe);
            cmd.arg("child")
                .arg("--role")
                .arg("owner")
                .env("TF_TREE_RUNTIME_DIR", dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            if let Some(spec) = crash_at.clone() {
                cmd.env("TF_TREE_CRASH_AT", spec);
            }
            let mut proc = cmd.spawn().context("spawning the owner child")?;
            let stdout = proc.stdout.take().context("owner child stdout")?;
            let mut lines = BufReader::new(stdout);
            let mut line = String::new();
            let ready =
                matches!(lines.read_line(&mut line), Ok(n) if n > 0) && line.trim() == "ready";
            if ready {
                return Ok(Kid {
                    proc,
                    seed: 0,
                    inject: false,
                    crash_at,
                });
            }
            let status = proc.wait().ok();
            #[cfg(unix)]
            if let Some(st) = status {
                use std::os::unix::process::ExitStatusExt as _;
                if st.signal() == Some(libc::SIGABRT) {
                    ledger.record_abort(crash_at.as_deref());
                }
            }
            last = format!(
                "attempt {}: the owner child exited before reporting ready ({:?}){}",
                attempt + 1,
                status,
                crash_at
                    .as_deref()
                    .map(|c| format!(", armed at {c}"))
                    .unwrap_or_default()
            );
            println!("shm_torture: {last}");
        }
        bail!(
            "the owner child never came up after six attempts. {last}. Without an owner \
             nothing serves the rendezvous and no child can join, so the run would validate \
             nothing while printing that it validated nothing. If this is a \
             `--crash-site` probe naming a site on the *creation* path \
             (`open.*`, `topo.after_copy_before_publish`, `intern.*`), every attempt aborts \
             by construction and that is the probe answering `reachable` rather than a \
             defect: an ordinary `--crash-points` run redraws the site on each attempt."
        )
    }

    fn spawn(
        exe: &std::path::Path,
        dir: &PathBuf,
        seed: u64,
        inject: bool,
        readers_only: bool,
        no_inherit: bool,
        crash_at: Option<String>,
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
        if no_inherit {
            cmd.arg("--no-inherit");
        }
        // **Per child, not per run.** §11.4 asks for a random site in 10% of
        // children, and `crash::spec` parses the variable once per process, so
        // the environment is the only place this can go.
        if let Some(spec) = crash_at.as_deref() {
            cmd.env("TF_TREE_CRASH_AT", spec);
        }
        let proc = cmd.spawn().context("spawning a torture child")?;
        Ok(Kid {
            proc,
            seed,
            inject,
            crash_at,
        })
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

        // How many children died at an armed §11.3 site, and how many were armed
        // at all. Both, because "armed 4, none fired" and "armed 4, all fired"
        // are different runs and only one of them exercised anything — and the
        // site names beside them, because "armed and never reached" and "armed
        // and lost the race" are also different runs.
        let mut ledger = CrashLedger::default();

        // **The owner is a child, and it comes up first.** It creates the arena
        // and serves the rendezvous and does nothing else, so killing it is an
        // owner's death rather than a writer's — which is the distinction §3.5
        // is about and the one this harness could not draw while the driver
        // owned the arena. See [`spawn_owner`] and [`attach_observer`].
        let mut owner_kid = Some(spawn_owner(
            &exe,
            &dir,
            &mut rng,
            a.crash_points,
            a.crash_site.as_deref(),
            &mut ledger,
        )?);

        // The driver keeps two of the three roles it had: it holds the segment
        // alive for `check_recovery`, and it is the never-killed reader. It
        // serves nothing and never inherits.
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
                a.no_inherit,
                crash_spec(
                    a.crash_points,
                    a.crash_site.as_deref(),
                    &mut rng,
                    &mut ledger,
                ),
            )?));
        }

        let deadline = Instant::now() + a.duration;
        let mut kills = 0usize;
        let mut reads = Reads::default();
        let mut rounds = 0u64;
        let mut violations = Vec::new();
        let interval = Duration::from_secs_f64(1.0 / a.kill_hz);
        let started = Instant::now();
        // Two accumulators: the whole run, and the last hundred rounds. The
        // second is what dates the moment an arena stopped being written —
        // without it a 30-minute failure is one average over a run that was
        // healthy for the first thirty seconds.
        let mut health = Health::default();
        let mut window = Health::default();
        // §3.5. `None` when `--no-kill-owner`; otherwise the instant of the next
        // owner kill, first one [`OWNER_KILL_FIRST`] in.
        let mut next_owner_kill = a.owner_kill_every.map(|_| started + OWNER_KILL_FIRST);
        let mut migrations: Vec<Migration> = Vec::new();
        // The observer's read total on the round immediately before an owner
        // kill. §3.5's "the data plane never pauses" is a claim *relative* to
        // the data plane working, and a run whose children never publish
        // (`--readers-only`) reads nothing before the kill either.
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

            let mut round = RoundHealth::default();
            let this_round = observe(&observer, &mut rng, &mut violations, &mut round);
            let last_round_reads = this_round.total();
            reads.add(this_round);
            health.add(round);
            window.add(round);
            if health.rounds % 100 == 0 {
                println!("{}", window.line(started.elapsed()));
                window = Health::default();
            }
            rounds += 1;
            reap_finished(&mut kids, &mut violations, &mut ledger);
            reap_owner(&mut owner_kid, &mut ledger);
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
                        a.no_inherit,
                        crash_spec(
                            a.crash_points,
                            a.crash_site.as_deref(),
                            &mut rng,
                            &mut ledger,
                        ),
                    )?);
                }
            }

            // **§3.5's owner death, before the ordinary victim draw.** It is a
            // separate event and not a lucky draw: the driver reads who holds
            // the role, kills that process, and then requires the two things a
            // migration owes. The ordinary kill below still runs on the same
            // round, so an owner dying while the fleet is churning is the state
            // under test rather than a quiet moment arranged for it.
            if let (Some(every), Some(at)) = (a.owner_kill_every, next_owner_kill) {
                if Instant::now() >= at {
                    let m = kill_the_owner(
                        &dir,
                        &observer,
                        &mut owner_kid,
                        &mut kids,
                        &mut rng,
                        &mut violations,
                        &mut health,
                        migrations.len() + 1,
                        last_round_reads > 0,
                    );
                    reads.add(m.reads);
                    rounds += m.rounds;
                    println!("{}", m.line());
                    migrations.push(m);
                    next_owner_kill = Some(Instant::now() + every);
                    if !violations.is_empty() {
                        break;
                    }
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
        let mut round = RoundHealth::default();
        reads.add(observe(&observer, &mut rng, &mut violations, &mut round));
        health.add(round);
        rounds += 1;
        reap_finished(&mut kids, &mut violations, &mut ledger);
        reap_owner(&mut owner_kid, &mut ledger);
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
        // **The owner outlives the workers, and the order is a check rather than
        // tidiness.** Whoever holds the role is running the hangup callback,
        // which is one of the two collectors that reclaim a dead participant's
        // record without being asked (`docs/decisions/0028` plan step 4) — and
        // it is the one `check_recovery`'s leak check has always been written
        // against. While the driver *was* the owner that collector ran in this
        // process for the whole teardown and nobody had to think about it.
        //
        // **Measured, and the measurement is narrower than the fix.** The first
        // version of this teardown killed the owner *before* `wait`ing the
        // workers, and a `--no-kill-owner` run then failed with `4 of 64
        // participant slot(s) hold a LIVE record` — a real regression of the
        // check, produced by killing the collector ahead of the records it was
        // going to collect. Reaping the workers first is what fixes that; the
        // 200 ms is margin on a loaded host and removing it does **not**
        // reproduce the failure, which is why this comment says so rather than
        // claiming a measurement for the sleep.
        //
        // On a run that migrated, whoever inherited is not this process and this
        // is a no-op — which is why [`check_recovery`] sweeps
        // `reap_participants` in that case.
        std::thread::sleep(Duration::from_millis(200));
        let teardown_owner_pid = owner_kid.as_ref().map(|k| k.proc.id());
        if let Some(kid) = owner_kid.as_mut() {
            let _ = kid.proc.kill();
            let _ = kid.proc.wait();
        }
        drop(owner_kid);
        std::thread::sleep(Duration::from_millis(100));

        let recovery = check_recovery(&observer, !migrations.is_empty(), teardown_owner_pid);
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

        println!("{}", health.line(started.elapsed()));
        println!(
            "shm_torture: {kills} kills, {} violation(s)",
            violations.len()
        );
        // **`--crash-points` says what it did, and both numbers are needed.**
        // "armed 4, fired 0" and "armed 4, fired 4" are different runs, and only
        // the second exercised §11.3 at all — an armed child that the driver's
        // `SIGKILL` reached first never got to its site. Printing only "armed"
        // would be the flag-that-arms-nothing failure this replaces, one level
        // up.
        if a.crash_points {
            let mut distinct_armed: Vec<&str> =
                ledger.armed_sites.iter().map(String::as_str).collect();
            distinct_armed.sort_unstable();
            distinct_armed.dedup();
            let mut distinct_fired: Vec<&str> = ledger.fired.iter().map(String::as_str).collect();
            distinct_fired.sort_unstable();
            distinct_fired.dedup();
            // **Derived from this run, never from a literal list.** A hand
            // written table of "sites this workload cannot reach" is the same
            // failure as a hand-written site list one level up: it would go
            // stale silently the first time the workload gained an operation.
            // The module doc states the reachability split as a dated
            // measurement; this line is what a reader can check it against.
            let never: Vec<&str> = distinct_armed
                .iter()
                .copied()
                .filter(|s| !distinct_fired.contains(s))
                .collect();
            println!(
                "  §11.3: {} child(ren) armed at {} distinct site(s), {} aborted \
                 at {} distinct site(s)",
                ledger.armed,
                distinct_armed.len(),
                ledger.aborted,
                distinct_fired.len()
            );
            if !distinct_fired.is_empty() {
                println!("         fired:  {}", distinct_fired.join(", "));
            }
            if !never.is_empty() {
                println!(
                    "         armed, never fired:  {}\n         (each is either a site this \
                     workload cannot reach — the module doc names three — or one whose race \
                     the driver's SIGKILL won; a run cannot tell them apart, a longer one \
                     narrows it)",
                    never.join(", ")
                );
            }
        }
        // **§3.5, and it prints per migration rather than as a total.** `armed
        // N, aborted 0` is the shape this harness already learned to refuse one
        // level up: a summary line reading "3 owner kills" says nothing about
        // whether any of them recovered, and a silent zero is exactly what a
        // disabled arm looks like.
        if a.owner_kill_every.is_some() {
            let recovered: Vec<f64> = migrations
                .iter()
                .filter_map(|m| m.recovered)
                .map(|d| d.as_secs_f64() * 1e3)
                .collect();
            let inherited: usize = migrations.iter().map(|m| m.inherits).sum();
            println!(
                "shm_torture: §3.5: {} owner kill(s), {} inheritance(s) recorded by survivors, \
                 {} fresh join(s) after a migration{}",
                migrations.len(),
                inherited,
                recovered.len(),
                if recovered.is_empty() {
                    String::new()
                } else {
                    let worst = recovered.iter().copied().fold(0.0_f64, f64::max);
                    let mean = recovered.iter().sum::<f64>() / recovered.len() as f64;
                    format!(" (mean {mean:.1} ms, worst {worst:.1} ms)")
                }
            );
            for m in &migrations {
                if let Some(why) = m.failure() {
                    println!("  MIGRATION FAILURE: {why}");
                }
            }
        }
        for v in &violations {
            println!("  {v}");
        }
        let recovery = recovery?;
        for line in &recovery.notes {
            println!("  {line}");
        }
        // **Printed here, bailed on at the end.** The recovery failures are the
        // most causal thing this run knows — a leaked participant slot is why a
        // later check will say the observer read a frozen arena — but the
        // ordering of the *bails* below is load-bearing for the self-test, which
        // asserts on which message a deliberately-broken run produces. So the
        // diagnosis is always visible even when a different check fires first.
        for f in &recovery.failures {
            println!("  RECOVERY FAILURE: {f}");
        }
        if !violations.is_empty() {
            bail!(
                "{} invariant violation(s) — the arena is not crash-consistent \
                 (docs/PHASE2.md §12.3 gate 3)",
                violations.len()
            );
        }
        // **§3.5 before the read floor**, because a migration that did not
        // recover *causes* the floor to trip: no fresh process can join, every
        // killed child stays out, and the rings freeze. Reporting "the observer
        // read too little" about that would name the symptom and hide the cause.
        // The `--readers-only` self-test is unaffected: at 3 s it never reaches
        // [`OWNER_KILL_FIRST`].
        let failed: Vec<String> = migrations.iter().filter_map(Migration::failure).collect();
        if !failed.is_empty() {
            bail!(
                "{} of {} owner kill(s) did not recover — docs/PHASE2.md §3.5's ownership \
                 migration did not happen:\n  {}",
                failed.len(),
                migrations.len(),
                failed.join("\n  ")
            );
        }
        // **A run that never killed the owner must not be quoted as §3.5
        // coverage**, and the only way to know it should have is arithmetic on
        // its own schedule. Silence here was the whole defect: §12.3 gate 3 read
        // "partly met" for the life of this harness because the arm did not
        // exist, and an arm that is on but never fires looks identical.
        if let Some(every) = a.owner_kill_every {
            if migrations.is_empty() && a.duration >= OWNER_KILL_FIRST + every {
                bail!(
                    "the owner-kill arm is on and ran {} time(s) in {:?}, which is fewer than \
                     the schedule (first at {:?}, then every {:?}) requires. This run covers \
                     none of docs/PHASE2.md §3.5 and must not be quoted as if it did.",
                    migrations.len(),
                    a.duration,
                    OWNER_KILL_FIRST,
                    every
                );
            }
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
                 anything prints. {}.\n\nRead that as follows. The composed read needs \
                 one stamp all four rings can answer at once, so it collapses when one \
                 edge stops being written while the others keep wrapping past it; the \
                 single-edge reads need only their own ring and keep succeeding on a ring \
                 nobody has touched for an hour. `writers` at or near 0 with a large \
                 `freshest` therefore means the arena was not being written at all — look \
                 at the `could not join` lines on stderr and at the leaked-slot count \
                 above, not at `common_window`.",
                reads.chain,
                reads.edge,
                health.diagnosis()
            );
        }
        // **A run nobody was writing to proves nothing either, and it does not
        // trip the floor above.** A ring outlives its writer, so four frozen
        // rings that happen to overlap answer all 256 composed reads a round
        // forever — measured on this host at 256.0 per round with every child
        // locked out of the arena and the last write 28 minutes old. That is a
        // green run over a dead arena, and it is strictly worse than the red one
        // the same wedge produced on CI. Both faces of that coin were run on
        // `ubuntu-latest` at the same seed on identical code — 256.00 composed
        // reads a round and 2.02 — so which one a nightly gets says nothing
        // about the arena (`RoundHealth`). This is the check that does not care
        // which way they fell.
        //
        // Half the rounds, not all of them: `--kill-hz` guarantees stretches
        // with no writer on a given edge, and that is the state under test.
        if health.live_rounds * 2 < rounds {
            bail!(
                "the arena was being written on only {}/{rounds} observation rounds: for the \
                 rest of the run no chain edge had a live writer, or the freshest sample on \
                 any of the four was over a second old. The {} transforms the observer \
                 validated came out of rings whose writers were gone — a ring outlives the \
                 process that filled it, so those reads say nothing about a live arena.\n\n{}",
                health.live_rounds,
                reads.total(),
                health.diagnosis()
            );
        }
        if !recovery.failures.is_empty() {
            bail!(
                "{} recovery failure(s) after the run",
                recovery.failures.len()
            );
        }
        println!("shm_torture: PASS");
        Ok(())
    }

    /// One owner death and everything the run learned from it.
    ///
    /// Three independent facts, and none of them substitutes for another:
    ///
    /// * `recovered` — a **fresh process** joined the arena again, and how long
    ///   after the `SIGKILL`. Measured from the outside because that is the only
    ///   place the question is meaningful: an ownerless arena is exactly what
    ///   refuses a new joiner, so this is the property §3.5 exists to restore
    ///   and the one that was broken between 2026-08-27 and 2026-08-28.
    /// * `inherits` — how many survivors recorded an inheritance in this
    ///   window. This is what says the join above succeeded *because* §3.5's
    ///   caller-driven trigger ran.
    /// * `reads` — what the observer validated **while the role was vacant**,
    ///   against §3.5's "lookups do not stop, slow down, or observe anything
    ///   during a takeover". `read_before` records whether it was reading at all
    ///   beforehand, so a run whose children never publish is not failed for a
    ///   pause that was never there to begin with.
    struct Migration {
        n: usize,
        victim: Option<u32>,
        recovered: Option<Duration>,
        inherits: usize,
        reads: Reads,
        rounds: u64,
        read_before: bool,
    }

    impl Migration {
        /// The per-migration line, printed as it happens so a silent zero is
        /// impossible — `owner_migration` prints the same shape for the same
        /// reason.
        fn line(&self) -> String {
            let Migration {
                n,
                victim,
                recovered,
                inherits,
                ..
            } = self;
            format!(
                "shm_torture: §3.5 owner kill {n}: killed pid {}; {}; {inherits} survivor(s) \
                 inherited; the observer validated {} transform(s) while the role was vacant",
                victim.map_or_else(|| "?".to_string(), |p| p.to_string()),
                recovered.map_or_else(
                    || format!("NO fresh process joined within {OWNER_RECOVERY_DEADLINE:?}"),
                    |d| format!(
                        "a fresh process joined {:.1} ms later",
                        d.as_secs_f64() * 1e3
                    )
                ),
                self.reads.total(),
            )
        }

        /// `Some(why)` if this migration failed the run.
        fn failure(&self) -> Option<String> {
            let n = self.n;
            if self.victim.is_none() {
                return Some(format!(
                    "owner kill {n}: the recorded owner pid names no live process of this \
                     run, so there was nothing to kill. On a second or later kill that is what \
                     §3.5 *not happening* looks like — the marker still names the owner the \
                     previous migration killed, because nothing inherited and nothing \
                     republished it. On the first it is a harness defect: the owner child \
                     never published its pid. Neither is a passing run."
                ));
            }
            if self.recovered.is_none() {
                return Some(format!(
                    "owner kill {n}: no fresh process could join within {:?} of the owner's \
                     death, and {} survivor(s) recorded an inheritance. That is the state \
                     docs/PHASE2.md §3.5 exists to end — the arena is ownerless, every \
                     already-attached process keeps reading, and every new joiner is refused \
                     `ArenaHeldButUnreachable` against the survivors' held participant bytes.",
                    OWNER_RECOVERY_DEADLINE, self.inherits
                ));
            }
            if self.inherits == 0 {
                return Some(format!(
                    "owner kill {n}: a fresh process joined, but no survivor recorded an \
                     inheritance. Something is serving the rendezvous and §3.5's trigger is \
                     not why, so this run cannot claim the mechanism it is here to exercise."
                ));
            }
            // §3.5, NORMATIVE: "Lookups do not stop, slow down, or observe
            // anything during a takeover. Not during the poll, not during the
            // lock, not during the bind." Only asserted where there were
            // lookups to stop.
            if self.read_before && self.reads.total() == 0 {
                return Some(format!(
                    "owner kill {n}: the observer validated 0 transforms while the role was \
                     vacant, having validated some on the round before. docs/PHASE2.md §3.5 is \
                     NORMATIVE that the data plane never pauses during a takeover — `Plan::at` \
                     touches the mapping and nothing else — so a read that stops here is a \
                     finding about the engine and not about this harness."
                ));
            }
            None
        }
    }

    /// `SIGKILL` whichever process currently holds the rendezvous, then require
    /// what §3.5 owes.
    ///
    /// # The victim is looked up, not drawn
    ///
    /// After the first migration the owner is whichever child won byte 0, which
    /// is a fact in the kernel that nothing reports. So an heir publishes its
    /// pid on inheriting ([`publish_owner_pid`]) and this reads it. A draw would
    /// make the whole arm depend on luck: with six children, five rounds in six
    /// would kill a plain participant and the run would report an owner kill
    /// having performed an ordinary one.
    ///
    /// # Whose process it is
    ///
    /// Every process here is one this driver spawned, so the pid resolves to
    /// either the owner child or a worker slot and is `wait`ed for in place. A
    /// pid that matches neither is not signalled at all — killing an
    /// unrecognised pid on a shared machine is not this harness's business —
    /// and the migration records `victim: None`, which [`Migration::failure`]
    /// treats as a harness defect rather than a passing run.
    ///
    /// # The replacement is an ordinary participant
    ///
    /// A killed worker slot is refilled by [`drive`]'s existing respawn loop. A
    /// killed **owner child** is not replaced at all: the role is inherited from
    /// here on, and a second process opening `IfAbsent` would either join as a
    /// participant that does nothing or create a second arena and split the run.
    #[allow(clippy::too_many_arguments)]
    fn kill_the_owner(
        dir: &Path,
        observer: &Tree,
        owner_kid: &mut Option<Kid>,
        kids: &mut [Option<Kid>],
        rng: &mut Rng,
        violations: &mut Vec<String>,
        health: &mut Health,
        n: usize,
        read_before: bool,
    ) -> Migration {
        let mut m = Migration {
            n,
            victim: None,
            recovered: None,
            inherits: 0,
            reads: Reads::default(),
            rounds: 0,
            read_before,
        };
        let before = inheritance_count(dir);
        let Some(pid) = read_owner_pid(dir) else {
            return m;
        };

        let mut killed = false;
        if owner_kid.as_ref().is_some_and(|k| k.proc.id() == pid) {
            if let Some(kid) = owner_kid.as_mut() {
                let _ = kid.proc.kill();
                let _ = kid.proc.wait();
            }
            *owner_kid = None;
            killed = true;
        } else {
            for slot in kids.iter_mut() {
                if slot.as_ref().is_some_and(|k| k.proc.id() == pid) {
                    if let Some(kid) = slot.as_mut() {
                        let _ = kid.proc.kill();
                        let _ = kid.proc.wait();
                    }
                    *slot = None;
                    killed = true;
                    break;
                }
            }
        }
        if !killed {
            // A pid we did not spawn, or one that has already exited — the
            // marker is stale. Not signalled; recorded.
            return m;
        }
        m.victim = Some(pid);

        // **Read and probe in one loop.** The observer keeps validating while
        // the role is vacant (§3.5's data-plane claim) and the fresh join is
        // what says the role stopped being vacant. Doing them in sequence would
        // measure the reads *after* recovery, which is the easy case.
        let start = Instant::now();
        let mut next_observe = Instant::now();
        while start.elapsed() < OWNER_RECOVERY_DEADLINE {
            if Instant::now() >= next_observe {
                let mut round = RoundHealth::default();
                m.reads.add(observe(observer, rng, violations, &mut round));
                health.add(round);
                m.rounds += 1;
                next_observe = Instant::now() + MIGRATION_OBSERVE_EVERY;
                if !violations.is_empty() {
                    break;
                }
            }
            // A real `open()` from a process that was not here when the owner
            // died. `ReadOnly` because a consumer is what a robot restarts, and
            // because a read-write join would register a participant record this
            // run then has to account for.
            let joined = tf_tree::Open::new()
                .mode(AttachMode::ReadOnly)
                .create(CreatePolicy::Never)
                // Short: this is a poll, and a long timeout would measure the
                // timeout rather than the recovery.
                .timeout(Duration::from_millis(20))
                .open()
                .is_ok();
            if joined {
                m.recovered = Some(start.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        // **The evidence trails the recovery, and reading it immediately is a
        // race the harness loses.** `Tree::inherit_ownership` binds and renames
        // the socket over the rendezvous path *before* it returns, so the fresh
        // join above can succeed while the heir has not yet reached its two
        // `publish_owner_pid`/`record_inheritance` writes — measured, on a run
        // that recovered in 0.6 ms and reported `a fresh process joined, but no
        // survivor recorded an inheritance`, which is a true statement about an
        // instant and a false one about the migration.
        //
        // Bounded, and short. The point of the wait is a file write that has
        // already been decided on, not a second chance at inheriting: a
        // migration where nothing inherited never produces this line however
        // long it waits, and the `--no-inherit` control is what proves that.
        let evidence_deadline = Instant::now() + Duration::from_millis(500);
        loop {
            m.inherits = inheritance_count(dir).saturating_sub(before);
            if m.inherits > 0 || Instant::now() >= evidence_deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        m
    }

    /// Collect children that have exited, recording the ones that reported a
    /// violation.
    ///
    /// A child that exits non-zero for any *other* reason is not a failure: a
    /// joiner whose owner was killed mid-handshake fails to open, and that is
    /// the run working as intended. Only [`EXIT_VIOLATION`] means the arena lied
    /// to somebody.
    /// Collect the **owner** child if it has exited on its own.
    ///
    /// **It needs its own sweep, and leaving it out hid a whole class of
    /// coverage.** `reap_finished` walks the worker slots; the owner is not one
    /// of them, so an owner that aborted mid-run at an armed §11.3 site — which
    /// `hangup.after_probe_before_cas` can only do, since that callback is the
    /// owner's — was never counted as an abort and the site read as unreachable.
    /// Measured: `--crash-site hangup.after_probe_before_cas` reported
    /// `20 armed, 0 aborted` on a run in which the owner had in fact aborted.
    ///
    /// An owner that dies this way is not a failure: the role goes vacant and a
    /// survivor inherits, which is the same recovery [`kill_the_owner`] drives
    /// deliberately. It is left un-replaced for the same reason a killed owner
    /// is — the role is inherited from here on.
    fn reap_owner(owner: &mut Option<Kid>, ledger: &mut CrashLedger) {
        let Some(kid) = owner.as_mut() else { return };
        match kid.proc.try_wait() {
            Ok(Some(status)) => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt as _;
                    if status.signal() == Some(libc::SIGABRT) {
                        ledger.record_abort(kid.crash_at.as_deref());
                    }
                }
                let _ = status;
                *owner = None;
            }
            Ok(None) => {}
            Err(_) => *owner = None,
        }
    }

    fn reap_finished(
        kids: &mut [Option<Kid>],
        violations: &mut Vec<String>,
        ledger: &mut CrashLedger,
    ) {
        for slot in kids.iter_mut() {
            let Some(kid) = slot.as_mut() else { continue };
            match kid.proc.try_wait() {
                Ok(Some(status)) => {
                    // **A child that aborted at an armed §11.3 site**, counted
                    // so a `--crash-points` run can say the sites *fired*
                    // rather than only that they were armed. Not a failure:
                    // firing is the point, and the invariant checks that follow
                    // are what decides whether the state it left was repairable.
                    //
                    // `SIGABRT` and not an exit code, because §11.3 is explicit
                    // that a crash point must `abort()` rather than `panic!` —
                    // a panic unwinds and runs the `Drop`s that would repair the
                    // damage the test exists to observe.
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt as _;
                        if status.signal() == Some(libc::SIGABRT) {
                            ledger.record_abort(kid.crash_at.as_deref());
                        }
                    }
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

    /// Join the arena the owner child created, and hold a reader on it for the
    /// whole run.
    ///
    /// # The driver is a joiner now, and what that changed
    ///
    /// It used to be the creator and the owner, for a reason that expired.
    /// **This function's doc comment said §3.5's takeover "is not wired into
    /// `tf_tree::open`"** and cited `crates/tf_tree/src/open.rs`'s module
    /// documentation as saying so in as many words. That was true when written
    /// and stopped being true on **2026-08-28**: `Tree::inherit_ownership` and
    /// `Tree::owner_lost` exist, that module doc no longer says it, and the
    /// measurement the paragraph rested on — children failing `open()` with
    /// `ArenaHeldButUnreachable { holder_slots: 4, first_pid: <the driver> }`
    /// after a 2 s timeout, for the rest of the run — is exactly the state §3.5
    /// ends. It is kept here rather than deleted because it is also the *shape*
    /// of the failure this harness must now detect: [`kill_the_owner`] probes
    /// for it from the outside on every migration.
    ///
    /// Two of the three roles the driver had are unchanged by the move, and the
    /// third is the one that mattered:
    ///
    /// 1. **It keeps the segment alive.** A joiner's mapping holds the memfd
    ///    open exactly as a creator's does, so the last child's death cannot
    ///    free the arena out from under [`check_recovery`].
    /// 2. **It is a reader that is never killed**, so §11.4's continuous
    ///    checking does not depend on which child happened to survive.
    /// 3. It no longer *serves*, which is what made the owner unkillable and
    ///    left §12.3 gate 3's "the owner dies mid-run" unmet.
    ///
    /// **The driver deliberately never inherits.** It has an attach socket like
    /// any joiner and could call `Tree::owner_lost` itself, and then the process
    /// that must not be killed would be the owner again — the same fusion, one
    /// layer down. Inheriting is the children's job; see [`work`].
    ///
    /// `Never`, so a driver that somehow raced ahead of its own owner child
    /// fails to join rather than quietly creating a second arena that half the
    /// run publishes into.
    fn attach_observer() -> Result<Tree> {
        tf_tree::Open::new()
            .mode(AttachMode::ReadWrite)
            .create(CreatePolicy::Never)
            .timeout(Duration::from_secs(10))
            .open()
            .context("the driver could not join the torture arena the owner child created")
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

    /// What one observation round saw *besides* the transforms it validated.
    ///
    /// # Why the read counts are not enough, which is not hypothetical
    ///
    /// A ring in a shared arena outlives the process that filled it. So an arena
    /// whose writers have all gone away still answers every lookup inside the
    /// window it froze with, for as long as the segment lives — and
    /// [`Reads`] cannot tell that from an arena six processes are hammering.
    /// Measured on this host, 2026-08-17, on the 30-minute nightly
    /// configuration: at minute 17 no child had the segment mapped at all, the
    /// last writer had died at about t = 30 s, and the observer was still
    /// scoring the full 256 composed reads per round out of four dead rings
    /// whose windows happened to overlap. The run would have printed `PASS`.
    ///
    /// On the GitHub runner the same wedge froze the four rings *without* an
    /// overlap, so the composed count went to ~0 and the read floor caught it —
    /// which is the only reason anybody looked.
    ///
    /// # Which way four dead rings fall is a coin flip, and both faces were run
    ///
    /// Two 30-minute nightlies on `ubuntu-latest`, same seed
    /// (`8107906721580384257`), and `git diff 817ce70 a3bc7f2 --
    /// crates/tf_tree/src/open.rs crates/tf_tree_bench/
    /// crates/tf_tree_core/src/participant.rs` is **empty** — the same engine,
    /// the same harness, the same workload:
    ///
    /// ```text
    /// 817ce70  wedged  FAIL   21756 composed / 10763 rounds =   2.02 a round
    /// a3bc7f2  wedged  PASS 2751232 composed / 10747 rounds = 256.00 a round
    /// adeb158  healthy PASS 2752957 composed / 10757 rounds = 255.92 a round
    /// ```
    ///
    /// The first two were wedged and differ only in whether the frozen windows
    /// intersected. The third is the same nightly with the owner-side fix in.
    ///
    /// **And the perfect score is the tell, not the reassurance.** 2 751 232 is
    /// 10 747 x 256 *exactly*, and the failing run's 688 832 single-edge reads
    /// are 10 763 x 64 exactly: across an hour of CI neither wedged run missed a
    /// single read. The healthy run on the same runner missed 835 of 2 753 792,
    /// because a writer moves the ring between the window probe and the read —
    /// as it does here, 5 to 25 misses in every 25 600. A hundred per cent is
    /// not what a well-tortured arena looks like; it is what nothing moving
    /// looks like.
    ///
    /// Locally only the overlapping face reproduces: seven attempts — four seeds
    /// at `--kill-hz 6`, the nightly's seed under `taskset -c 0-3` (which froze
    /// with a 689 ms intersection), and `--kill-hz 2 --duration 150s` on the
    /// theory that survivors killed half a second apart would freeze their edges
    /// further apart than a ring is wide (`overlap=100% window=327ms`, so that
    /// theory is refuted) — all froze *with* an overlap and scored ~256.
    ///
    /// Which is the argument for what follows. A gate that depends on which way
    /// four dead rings happen to fall is not a gate — the same defect went green
    /// and red on the same runner over two runs — so everything below is checked
    /// on every round and reported whether the run passes or fails, and none of
    /// it asks how the windows landed.
    #[derive(Default, Clone, Copy)]
    struct RoundHealth {
        /// How many chain edges reported a window at all. Fewer than
        /// `CHAIN.len()` means an edge has never been written.
        windows: usize,
        /// Did the four windows intersect?
        overlap: bool,
        /// Width of the intersection, ns (`overlap`), or how far apart the
        /// nearest pair was (`!overlap`).
        width: i64,
        gap: i64,
        /// Index into [`CHAIN`] of the edge whose `newest` was the minimum —
        /// the one holding the intersection back — and of the edge whose
        /// `oldest` was the maximum.
        laggard: usize,
        blocker: usize,
        /// Composed reads that succeeded, of the 256 attempted.
        chain_ok: u64,
        /// `now - newest` for the *freshest* chain edge, ns. This is the number
        /// that says whether anybody is still writing.
        ///
        /// **`None`, not a sentinel, when no edge reported a window at all.**
        /// This was `i64::MAX` for that case and the sentinel was summed into
        /// the run's average like a measurement: `--duration 3s --children 2
        /// --readers-only` printed `freshest=709490156681ms` — twenty-two years
        /// — and the failure diagnosis below it said the freshest edge was
        /// `9223372036855 ms` old. Both are `i64::MAX` wearing a unit. A harness
        /// whose whole subject is "a number that cannot tell a live arena from a
        /// dead one" must not invent one, so the rounds with nothing to measure
        /// are counted separately and left out of the mean.
        freshest_age: Option<i64>,
        /// Chain edges whose claim word names a participant that is still
        /// alive / is already dead / is free.
        writers_live: u64,
        writers_dead: u64,
        /// Participant slots holding a `LIVE` record, and slots the kernel
        /// agrees are alive. **These two disagreeing is the leak** — see
        /// [`check_recovery`].
        slots_registered: u64,
        slots_alive: u64,
    }

    impl RoundHealth {
        /// Was the arena *being written* during this round?
        ///
        /// Both halves are needed. A live claim holder that is not pushing
        /// leaves the rings frozen; a recent sample with no holder is the
        /// hundred milliseconds after a writer died. Only the conjunction says
        /// the torture is still happening.
        ///
        /// One second, and the derivation rather than a tuned constant: `work`
        /// paces itself at ~1 kHz and publishes on 40% of its operations, so a
        /// held edge is written every ~2.5 ms. A hundredfold slowdown still
        /// leaves the freshest edge a quarter of a second old. What this is
        /// written against is minutes, not milliseconds.
        /// A round where no edge has ever been written is **not** live: an arena
        /// nobody has published to yet is exactly the vacuous state this check
        /// exists to refuse, so `None` fails the conjunction.
        fn arena_is_live(self) -> bool {
            self.writers_live > 0 && self.freshest_age.is_some_and(|age| age < 1_000_000_000)
        }
    }

    /// [`RoundHealth`] summed over a run, or over the last hundred rounds.
    #[derive(Default, Clone, Copy)]
    struct Health {
        rounds: u64,
        short: u64,
        overlap: u64,
        live_rounds: u64,
        chain_ok: u64,
        width_sum: i64,
        gap_sum: i64,
        gap_max: i64,
        laggard: [u64; CHAIN.len()],
        /// **Denominators, not `rounds`.** Both of the averages below are over
        /// the rounds that had something to average: a round where no edge had
        /// ever been written contributes no age, and a round whose windows
        /// intersected contributes no gap. Dividing either by `rounds` reports a
        /// mean of a set that includes members it never measured.
        gap_rounds: u64,
        freshest_rounds: u64,
        freshest_sum: i64,
        freshest_max: i64,
        writers_live_sum: u64,
        writers_dead_sum: u64,
        slots_registered_max: u64,
        slots_alive_min: u64,
    }

    impl Health {
        fn add(&mut self, h: RoundHealth) {
            if self.rounds == 0 {
                self.slots_alive_min = u64::MAX;
            }
            self.rounds += 1;
            if h.windows != CHAIN.len() {
                self.short += 1;
            }
            if h.overlap {
                self.overlap += 1;
                self.width_sum += h.width;
            } else if h.windows == CHAIN.len() {
                self.gap_rounds += 1;
                self.gap_sum += h.gap;
                self.gap_max = self.gap_max.max(h.gap);
                self.laggard[h.laggard.min(CHAIN.len() - 1)] += 1;
            }
            if h.arena_is_live() {
                self.live_rounds += 1;
            }
            self.chain_ok += h.chain_ok;
            if let Some(age) = h.freshest_age {
                self.freshest_rounds += 1;
                self.freshest_sum += age;
                self.freshest_max = self.freshest_max.max(age);
            }
            self.writers_live_sum += h.writers_live;
            self.writers_dead_sum += h.writers_dead;
            self.slots_registered_max = self.slots_registered_max.max(h.slots_registered);
            self.slots_alive_min = self.slots_alive_min.min(h.slots_alive);
        }

        /// Milliseconds, or `n/a` when the quantity was never observed.
        ///
        /// A mean over zero samples is not zero and it is not `i64::MAX`; it is
        /// nothing, and a diagnostic that says so is worth more than one that
        /// prints a plausible number. `n/a` on `freshest` means no chain edge
        /// was ever written; on `gap` it means the windows always intersected.
        fn mean_ms(sum: i64, n: u64) -> String {
            if n == 0 {
                "n/a".to_string()
            } else {
                format!("{:.0}ms", sum as f64 / n as f64 / 1e6)
            }
        }

        /// The same, for a maximum: `n/a` unless at least one round contributed
        /// one.
        fn max_ms(v: i64, n: u64) -> String {
            if n == 0 {
                "n/a".to_string()
            } else {
                format!("{:.0}ms", v as f64 / 1e6)
            }
        }

        /// The one-line periodic summary. **One space after the prefix**, not
        /// three: `tests/torture.rs` finds the composed-read total by taking the
        /// first `shm_torture:   ` line, and a second one would shadow it.
        fn line(&self, elapsed: Duration) -> String {
            let r = self.rounds.max(1) as f64;
            format!(
                "shm_torture: t={:.0}s rounds={} composed={}/{} overlap={:.0}% \
                 window={} freshest={} writers={:.1}/4 slots={}reg/{}alive live={:.0}%",
                elapsed.as_secs_f64(),
                self.rounds,
                self.chain_ok,
                self.rounds * 256,
                100.0 * self.overlap as f64 / r,
                Self::mean_ms(self.width_sum, self.overlap),
                Self::mean_ms(self.freshest_sum, self.freshest_rounds),
                self.writers_live_sum as f64 / r,
                self.slots_registered_max,
                if self.slots_alive_min == u64::MAX {
                    0
                } else {
                    self.slots_alive_min
                },
                100.0 * self.live_rounds as f64 / r,
            )
        }

        /// The sentence a failing run should not make the next person derive.
        fn diagnosis(&self) -> String {
            let r = self.rounds.max(1) as f64;
            // `n/a` rather than the array's default winner. With no
            // non-overlapping round, `laggard` is all zeroes and `max_by_key`
            // still returns an index — `arm->tool`, the last maximum — so the
            // sentence would name an edge that held nothing back on a run where
            // the windows always intersected.
            let worst = if self.gap_rounds == 0 {
                "n/a".to_string()
            } else {
                let (i, _) = self
                    .laggard
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, n)| **n)
                    .unwrap_or((0, &0));
                format!("{}->{}", CHAIN[i].0, CHAIN[i].1)
            };
            format!(
                "the four chain windows intersected on {}/{} rounds ({:.1}%); on {} rounds an \
                 edge had never been written at all. When they did not intersect the nearest \
                 pair was {} apart (worst {}) and the edge holding the \
                 intersection back was most often {worst}. Averaged over the run \
                 the freshest of the four edges was {} old (worst {}) and {:.2} of \
                 the 4 chain edges had a *live* writer; {} participant slot(s) held a LIVE \
                 record at the high-water mark while as few as {} were alive by the kernel's \
                 answer",
                self.overlap,
                self.rounds,
                100.0 * self.overlap as f64 / r,
                self.short,
                Self::mean_ms(self.gap_sum, self.gap_rounds),
                Self::max_ms(self.gap_max, self.gap_rounds),
                Self::mean_ms(self.freshest_sum, self.freshest_rounds),
                Self::max_ms(self.freshest_max, self.freshest_rounds),
                self.writers_live_sum as f64 / r,
                self.slots_registered_max,
                if self.slots_alive_min == u64::MAX {
                    0
                } else {
                    self.slots_alive_min
                },
            )
        }
    }

    /// Read the four chain edges' claim words and the participant table.
    ///
    /// **Two predicates, deliberately.** `slots_registered` counts records whose
    /// `state` is `LIVE`, which is what the arena owner's slot assigner in
    /// `crates/tf_tree/src/open.rs` consults before granting a slot;
    /// `slots_alive` counts the ones the *kernel* agrees are alive, which is
    /// what `docs/PHASE2.md` §5 says liveness is ("any code deciding liveness
    /// from `state` or `heartbeat` is a bug"). A healthy arena has them equal.
    /// Their difference is §11.4's leaked participant slot, and it is invisible
    /// to either predicate alone.
    fn census(tree: &Tree, h: &mut RoundHealth) {
        let view = tree.arena_view();
        // **`1..=CHAIN.len()`, and this line read `0..CHAIN.len()` until
        // 2026-09-04.** `EdgeId` is 1-based: `TreeBuilder::build_with` sizes the
        // table as `declared + 1` and writes each declared edge's capacity at
        // `i + 1`, leaving id 0 a zero-capacity sentinel nothing can ever claim.
        // So the old bound walked the sentinel and the first three chain edges
        // and never looked at `arm->tool` — `writers=x/4` was a count over three
        // real edges and a slot that is free by construction, and
        // `RoundHealth::arena_is_live` was blind to a round in which the only
        // live writer held the last edge. Found while adding the two-writers
        // check, which made the same mistake and failed a healthy arena for it.
        let last = view.header().max_edges.min(CHAIN.len() as u32 + 1);
        for edge in 1..last {
            let Some(rec) = view.claim(EdgeId(edge)) else {
                continue;
            };
            let owner = rec.owner.load(Ordering::Acquire);
            if owner == 0 {
                continue;
            }
            // `slot_of`, not a hand-rolled `& 0xFFFF`: the owner word packs
            // `(epoch << 16) | (slot + 1)` and reserves a `CLAIMING` sentinel
            // for a claim in flight, and only the shared helper knows both. A
            // second spelling here would resolve the sentinel to a plausible
            // slot number and count a claim nobody holds as a live writer.
            let slot = tf_tree_core::edge::slot_of(owner);
            if slot != u32::MAX && tree.participant_alive(slot) {
                h.writers_live += 1;
            } else {
                h.writers_dead += 1;
            }
        }
        let me = tree.participant_slot();
        let table = view.participants();
        for slot in 0..table.capacity() as u32 {
            if slot == me {
                continue;
            }
            if table.identity(slot).is_some() {
                h.slots_registered += 1;
            }
            if tree.participant_alive(slot) {
                h.slots_alive += 1;
            }
        }
    }

    /// A burst of checked reads from the never-killed observer, and a
    /// [`RoundHealth`] describing the arena they came out of.
    ///
    /// Returns how many transforms were validated, which the run prints and
    /// [`drive`] enforces a floor on. A count is not decoration here: "0
    /// violations" and "0 reads" print the same verdict, and only one of them is
    /// a result — and neither is "256 reads out of four rings nobody has written
    /// since the first minute", which is what `health` exists to say.
    ///
    /// Both shapes are read. The composed `map -> tool` is the one §11.4 is
    /// about — a bad sample on any edge reaches it — but it needs all four
    /// windows to overlap. The per-edge reads need only that edge's own ring, so
    /// they keep validating across the moments when the chain cannot be read at
    /// all, and they still see the injected corruption: the injector writes NaN
    /// to whichever single edge it holds.
    fn observe(
        tree: &Tree,
        rng: &mut Rng,
        violations: &mut Vec<String>,
        health: &mut RoundHealth,
    ) -> Reads {
        let mut reads = Reads::default();
        let guard = tree.guard();
        let windows = edge_windows(tree, &guard);

        // **Measured before the reads, not after.** Everything here is a
        // statement about the arena this round's reads came out of, and the
        // reads themselves take long enough for a writer to move.
        health.windows = windows.len();
        let now = now_nanos();
        health.freshest_age = windows.iter().map(|w| now.saturating_sub(w.newest)).min();
        census(tree, health);
        // `let Some(..)` rather than `expect`: both are `None` only when no edge
        // reported a window at all, in which case there is no laggard to name
        // and `Health::add` records the round as `short` from `windows`. A
        // 30-minute soak must not panic over a diagnostic.
        if let (Some(blocker), Some(laggard)) = (
            windows.iter().max_by_key(|w| w.oldest),
            windows.iter().min_by_key(|w| w.newest),
        ) {
            health.blocker = blocker.which;
            health.laggard = laggard.which;
            if windows.len() == CHAIN.len() && blocker.oldest <= laggard.newest {
                health.overlap = true;
                health.width = laggard.newest - blocker.oldest;
            } else {
                health.gap = blocker.oldest - laggard.newest;
            }
        }

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
                health.chain_ok += 1;
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
    fn check_recovery(
        tree: &Tree,
        migrated: bool,
        teardown_owner_pid: Option<u32>,
    ) -> Result<Recovery> {
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

        // §11.4's "participant ... slots never leak", and it needs **both**
        // predicates, because the two can disagree and the disagreement *is* the
        // leak.
        //
        // `participant_alive` is the kernel's answer: `state == LIVE` *and* the
        // participant's OFD lock byte still held. A `SIGKILL`ed child's byte is
        // released by the kernel, so that predicate reports it dead — and the
        // `live` loop below therefore finds nothing, which is exactly what this
        // function did for the life of the harness while sitting on an arena
        // that had run out of slots half an hour earlier.
        //
        // The arena's owner *used* to use the other one, and this paragraph
        // said so in the present tense for longer than it was true. Its slot
        // assigner (`crates/tf_tree/src/open.rs`) skipped any slot where
        // `table.identity(slot).is_some()` — `state == LIVE`, no liveness check
        // — and `ParticipantTable::register_at` fills only a `FREE` slot, so a
        // record left `LIVE` by a process that never got to run `Drop` was a
        // slot no future participant could be granted. **`docs/decisions/0028`
        // plan step 3 ended that**: the assigner takes its verdict from the
        // participant's OFD lock byte, through `reclamation_verdict`, and
        // reclaims the dead record before it grants the slot.
        //
        // So the two predicates no longer disagree for ever — but they still
        // disagree until somebody looks, and that is what this check is for
        // now. Collection is **lazy**: the assigner reaches a slot only when a
        // grant walks past its index, the hangup callback only when the socket
        // that owned it closes, and the one collector that does sweep the whole
        // table — plan step 5's `Tree::reap_participants` — runs when a
        // participant calls it, which this harness never does. A record still
        // held by a dead process when the run is over has leaked, because no
        // further grant is coming and nobody here is going to sweep.
        //
        // # What this found, and what fixed it
        //
        // Nothing performed that CAS. `docs/PHASE2.md` §3.9 says a dying
        // participant's records are reaped by the owner and §5 says liveness is
        // a kernel fact — "any code deciding liveness from `state` or
        // `heartbeat` is a bug" — but the owner's hangup callback only cleared
        // its own `granted` bitmask. Measured 2026-08-17 on the unfixed build,
        // at `--children 6 --kill-hz 6`: 63 of the 64 slots held records for
        // dead pids within the first hundred rounds, 8193 attaches were refused
        // `NoParticipantSlots` in sixty seconds, and `tf_tree participants`
        // against a live 30-minute run printed one `live` row and 63 `stale`
        // ones. The table holds `DEFAULT_MAX_PARTICIPANTS` = 64 and a run leaks
        // one per `SIGKILL`, so at 6 Hz it wedges about ten seconds in — which
        // is why `tests/torture.rs`'s 8 s clean case never saw it and the
        // 30-minute nightly spent 99% of itself reading rings whose writers were
        // gone.
        //
        // The owner now collects the record in its hangup callback, which is
        // §3.9 implemented — `release` when that landed (#191), one
        // `ParticipantTable::reclaim` guarded by the observed word since `0028`
        // plan step 4. Measured after, 728 kills over 120 s: the registered
        // slot count never left 5 of 64.
        //
        // This check stays because both collectors that run *without being
        // asked* are one CAS on one code path apiece — the callback's, and the
        // assigner's — and neither runs unless something drives it, while the
        // third has to be called and nothing here calls it; and the failure it
        // prevents is silent by
        // construction: a wedged arena reads exactly like a healthy one. `arena_is_live` catches
        // the *consequence* a round at a time; this names the cause.
        // **Polled, not sampled once, and the difference is a flaky gate.**
        // Reclamation is asynchronous by construction: the child's socket closes
        // when the kernel tears the process down, and the owner's serving thread
        // performs the `LIVE -> FREE` CAS when it is next scheduled and sees the
        // hangup. `drive` waits 100 ms after the last `wait()` before calling
        // this, which is generous on an idle host and is a *scheduling*
        // assumption on a loaded four-vCPU runner — and the failure it would
        // produce is the worst kind, a red nightly naming a defect that is not
        // there. A real leak never clears, so re-probing costs a genuinely
        // failing run two seconds and buys a check that does not depend on when
        // a thread woke up.
        // **After a migration, one of the two automatic collectors is not
        // coming, and this is where that is paid for.**
        //
        // `docs/decisions/0043` records the residue in as many words: a survivor
        // of a migration keeps its slot, its byte and its mapping, but its
        // attach socket still points at the dead owner and it never registers
        // with the new one — "that owner never learns this process exists". So
        // when such a survivor dies, the new owner's hangup callback has no
        // socket to notice, and the record is collected only by a byte-keyed
        // collector: the slot assigner, a grant later, or `reap_participants`,
        // a sweep later.
        //
        // This run is over, so no grant is coming. The sweep is therefore the
        // collector that must run, and the check becomes "a record survives even
        // a full sweep" rather than "a record survived the hangup callback".
        //
        // **Only when the run actually killed the owner.** Calling it
        // unconditionally would weaken the check for the topology it was written
        // against — where the owner lives for the whole run, every worker's
        // record *is* hangup-collected, and a leak is a real one — and
        // weakening a check before the change that needs it is how an exemption
        // arrives without the argument that would have justified it. The
        // `--no-kill-owner` run is unchanged.
        if migrated {
            let swept = tree.reap_participants();
            out.notes.push(format!(
                "recovery: the run killed the rendezvous owner, so `reap_participants` was \
                 swept once before judging leaks (docs/decisions/0043: a pre-migration \
                 survivor is invisible to the new owner's hangup callback); it reclaimed \
                 {swept} record(s)"
            ));
        }

        let table = view.participants();
        let mut leaked = Vec::new();
        for attempt in 0..9 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(250));
            }
            leaked.clear();
            for slot in 0..table.capacity() as u32 {
                if slot != me && table.identity(slot).is_some() && !tree.participant_alive(slot) {
                    leaked.push(slot);
                }
            }
            if leaked.is_empty() {
                break;
            }
        }
        // **The owner child's own record is the one no hangup callback can ever
        // collect, and it is separated by pid rather than excused by a
        // tolerance.** A process does not run its own hangup callback, so when
        // the owner is a child — which it has been since 2026-09-04 — its record
        // is left for a byte-keyed collector: the slot assigner on the next
        // grant, or `Tree::reap_participants`. This run is over, so no grant is
        // coming. Before the owner became a child the driver *was* the owner and
        // its own slot was `me`, skipped, which is why nothing here had to say
        // this.
        //
        // It is **not** given a pass. It is swept, and the sweep is then
        // required to have worked — so what was an exception becomes a check of
        // plan step 5's collector. Every other leaked slot fails exactly as it
        // did before, and the strictness this check was written for (the
        // 2026-08-17 defect, where *nothing* performed the CAS) is untouched:
        // that defect leaves records for slots this branch does not cover.
        let (mut owner_own, others): (Vec<u32>, Vec<u32>) = leaked.iter().partition(|slot| {
            teardown_owner_pid.is_some_and(|pid| {
                table
                    .identity(**slot)
                    .is_some_and(|(rec_pid, _, _)| rec_pid == pid)
            })
        });
        leaked = others;
        if !owner_own.is_empty() {
            let swept = tree.reap_participants();
            owner_own
                .retain(|slot| table.identity(*slot).is_some() && !tree.participant_alive(*slot));
            out.notes.push(format!(
                "recovery: slot(s) held the owner child's own record, which no hangup callback \
                 can collect because a process does not hang up on itself; `reap_participants` \
                 reclaimed {swept} record(s) and {} remain",
                owner_own.len()
            ));
            if !owner_own.is_empty() {
                out.failures.push(format!(
                    "participant slot(s) {owner_own:?} still hold the dead owner child's LIVE \
                     record after `Tree::reap_participants` swept the whole table. That sweep \
                     is `docs/decisions/0028` plan step 5 and is the collector of last resort \
                     for a record no socket closure can reach, so a record surviving it is a \
                     defect in the sweeper, not a scheduling delay."
                ));
            }
        }
        if !leaked.is_empty() {
            out.failures.push(format!(
                "{} of {} participant slot(s) hold a LIVE record for a process the kernel \
                 says is dead {:?}{}. These are leaked: nothing in this harness sweeps the \
                 participant table, so a dead record is collected only when a grant walks past \
                 its slot (`docs/decisions/0028` plan step 3) or when the socket that owned it \
                 closes (step 4) — and this run is over, so neither is coming. \
                 `docs/PHASE2.md` §11.4 \
                 requires that participant slots never leak and §5 requires that liveness \
                 come from the lock byte, never from `state`. Still held two seconds after \
                 the last child was reaped, so this is not the owner's hangup callback \
                 running late.",
                leaked.len(),
                table.capacity(),
                &leaked[..leaked.len().min(8)],
                if leaked.len() > 8 { " ..." } else { "" },
            ));
        }

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
        let mut no_inherit = false;
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
                // The one child that creates and serves; see [`owner_child`].
                "--role" => match it.next().map(String::as_str) {
                    Some("owner") => return owner_child(),
                    other => bail!("child: unknown --role `{other:?}`"),
                },
                "--inject-violation" => inject = true,
                "--readers-only" => readers_only = true,
                "--no-inherit" => no_inherit = true,
                other => bail!("child: unknown argument `{other}`"),
            }
        }
        let mut rng = Rng::new(seed);
        let dir = runtime_dir();
        // One `could not join` line per process; see the arm below.
        let mut reported = false;

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
                // **Printed, not swallowed — but once per process.** A child
                // that can never join is the failure this harness spent its
                // life unable to see: it leaves every ring frozen, and a frozen
                // ring answers lookups exactly like a live one, so the driver's
                // read counts stay perfect while nothing is under test. The
                // reason belongs in the log by name (`NoParticipantSlots` is a
                // different bug from `ArenaHeldButUnreachable`) and no reader
                // needs it twice: a child retries this loop tens of times a
                // second, and a healthy run prints none of these at all.
                Err(e) => {
                    if !reported {
                        reported = true;
                        eprintln!(
                            "shm_torture: child {} could not join: {e}",
                            std::process::id()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(10 + rng.below(40)));
                    continue;
                }
            };
            work(&tree, &dir, &mut rng, inject, readers_only, no_inherit)?;
        }
    }

    /// The random-operation loop against one attachment. Returns when it decides
    /// to detach and re-join, which is §11.4's "attach/detach".
    fn work(
        tree: &Tree,
        dir: &Path,
        rng: &mut Rng,
        inject: bool,
        readers_only: bool,
        no_inherit: bool,
    ) -> Result<()> {
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

        // **The edge index travels with the writer.** §11.4's "no two writers
        // ever hold one edge" is checked from inside the writer that holds it
        // (see the `push` arm below), and that check needs to know *which* claim
        // record to read. `EdgeWriter` exposes only `push`, and adding an
        // accessor to it would be a public-API change for a harness's benefit.
        let mut held: Option<(usize, tf_tree::EdgeWriter<'_>)> = None;
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

            // **§3.5's trigger, and the participants are the callers.** There is
            // no daemon and no background thread by design
            // (`docs/decisions/0019`), so a survivor that never evaluates
            // `owner_lost` never becomes owner and the arena stays ownerless —
            // which is precisely the state `kill_the_owner` probes for from the
            // outside. This loop is what the design means by "the caller's own
            // loop", and every child runs it, so the property is never left to
            // whether the surviving population happened to include a read-write
            // participant.
            //
            // The cost in the healthy case is one non-blocking `poll` of one
            // descriptor: `owner_lost` only reaches its `F_OFD_GETLK` once the
            // poll reports a hangup.
            //
            // `Contended` and `OwnerAlive` are **not** errors — a loser keeps
            // its slot and stops asking by itself (`0043`) — so only `Inherited`
            // is recorded.
            if !no_inherit && tree.owner_lost() {
                if let Ok(Inheritance::Inherited) = tree.inherit_ownership() {
                    // pid first, then the log line: the driver reads the count
                    // to decide a migration happened and the pid to pick the
                    // next victim, so the pid must never be behind.
                    publish_owner_pid(dir);
                    record_inheritance(dir);
                }
            }

            match rng.below(100) {
                // Claim an edge, if we hold none.
                0..=9 => {
                    if held.is_none() && !readers_only {
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
                15..=16 => {
                    let _ = tree.reap_dead();
                }
                // **Sweep the participant table**, which is a different
                // collector from `reap_dead` above: that one reclaims *claims*
                // held by dead writers, this one reclaims the dead
                // participants' *records* (`docs/decisions/0028` plan step 5).
                //
                // It is here for two reasons and both are load-bearing. A
                // migration orphans the hangup collector for every
                // pre-migration survivor (`0043`), so without a sweeper those
                // records are collected only when a grant happens to walk past.
                // And it is the only path in this workload that reaches §11.3's
                // `reclaim.after_probe_before_cas`, whose row is about a
                // sweeper killed between the verdict and the CAS — a site that
                // could not fire in a torture child before, because
                // `check_recovery`'s own comment recorded that this harness
                // never called the sweep.
                17 => {
                    let _ = tree.reap_participants();
                }
                // Detach and re-join.
                18..=19 => return Ok(()),
                // Publish.
                20..=59 => {
                    if let Some((edge, w)) = &held {
                        // **§11.4's "no two writers ever hold one edge",
                        // checked on every push instead of once at teardown.**
                        //
                        // The observable form, from inside the writer that
                        // holds the edge: read the claim word; if it names a
                        // participant slot that is not ours, then *push*. A
                        // claim can only pass to another slot through
                        // `edge::claim`, which CASes the word from free and
                        // bumps the epoch — and `Publisher::push` compares that
                        // epoch on every push (A4). So a push that **succeeds**
                        // while the word names somebody else means the epoch
                        // did not move, which means the edge was granted twice
                        // without a release: two writers on a single-writer
                        // ring, which is the failure D7, A3 and A4 exist to
                        // prevent.
                        //
                        // The ordinary case is a *revoked* claim — this writer
                        // was judged dead and reaped, another claimed, the
                        // epoch moved — and there the push fails, which is A4
                        // working and is not a violation.
                        //
                        // # What it does and does not prove
                        //
                        // It is a check by the holder, at the holder's rate, so
                        // it sees every window in which this process held an
                        // edge somebody else also held. It cannot see a double
                        // grant on an edge **no live writer holds**, and it
                        // adds no synchronisation of its own — one `Acquire`
                        // load of a word this writer's cacheline already owns —
                        // so it does not perturb the race it is looking for.
                        // The property itself is held by construction, not by
                        // this; what this refuses is the version of the harness
                        // in which "checked continuously" meant a teardown
                        // probe. `check_recovery` still runs, and answers the
                        // different question of whether every claim came back.
                        // **`w.edge()`, never the chain index.** `EdgeId` is
                        // **1-based** — `TreeBuilder::build_with` sizes the edge
                        // table as `declared + 1` and writes capacities at
                        // `i + 1`, reserving id 0 as a sentinel — so the chain's
                        // four edges are `EdgeId(1)..=EdgeId(4)` and an index
                        // used as an id reads the *neighbouring* edge's claim
                        // word. Measured: the first version of this check did
                        // exactly that and reported three two-writer violations
                        // in twenty seconds on a healthy arena. `EdgeWriter`
                        // derefs to `Publisher`, which answers the id this claim
                        // actually took.
                        let word = tree
                            .arena_view()
                            .claim(w.edge())
                            .map(|rec| rec.owner.load(Ordering::Acquire))
                            .unwrap_or(0);
                        let slot = tf_tree_core::edge::slot_of(word);
                        let foreign = slot != u32::MAX && slot != tree.participant_slot();

                        let iso = sample(rng, inject);
                        // `ClaimRevoked` is A4 working: this writer was judged
                        // dead, reaped, and is being fenced. Drop the claim and
                        // carry on.
                        if w.push(now_nanos(), &iso).is_err() {
                            held = None;
                        } else if foreign {
                            let (parent, child) = CHAIN[*edge];
                            eprintln!(
                                "VIOLATION pid {} two writers on {parent}->{child}: the claim \
                                 word named participant slot {slot} while this process (slot \
                                 {}) pushed to the same edge successfully, so the claim epoch \
                                 never moved and the edge was granted twice \
                                 (docs/PHASE2.md §11.4, §1 A3/A4, D7)",
                                std::process::id(),
                                tree.participant_slot()
                            );
                            std::process::exit(EXIT_VIOLATION);
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
