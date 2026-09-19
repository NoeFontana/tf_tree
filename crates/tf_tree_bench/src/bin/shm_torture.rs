//! `shm_torture` — `docs/PHASE2.md` §11.4, and `docs/PHASE5.md` §10's nightly CI job.
//!
//! N processes join one arena through the real rendezvous and hammer it with
//! random claim / push / lookup / release / reap / re-attach while the driver
//! `SIGKILL`s one of them at 1–10 Hz and replaces it. Every reader validates
//! every transform it receives. The output is a verdict, not a benchmark.
//!
//! # What this covers, and what it does not
//!
//! * Random attach/detach/claim/reap/push/lookup, and random `SIGKILL` at
//!   1–10 Hz, **including the rendezvous owner** (`kill_the_owner`).
//! * A random crash point armed in ~10% of children under `--crash-points`
//!   (needs `--features crash-points`); see `armed_site` and the reachability
//!   section below.
//! * Two of §11.4's four continuous invariants: the quaternion/NaN clause on
//!   every read (`Invariant`) and the two-writers clause on every push
//!   (`work`'s `two_writers` arm). Slot leakage is checked once, at teardown
//!   (`check_recovery`); "the arena hash is stable across quiescent points" is
//!   not implemented (it needs a safe byte accessor, a new public API, and
//!   "quiescent" is undefined here). `PHASE2.md` §0.0's §11.4 row says so.
//!
//! **`SIGKILL` alone is not §11.3 coverage**: a signal lands wherever the
//! scheduler puts it. A run without `--crash-points` must not be quoted as
//! §11.3.
//!
//! # Which of §11.3's thirteen sites this workload can reach
//!
//! Measured with `--crash-site NAME[:nth]` in every child: twelve fire, one does
//! not, and "fires" is not "exercises the row's repair claim". The armed/aborted
//! counts a probe prints are scheduling outcomes and are not recorded here.
//!
//! * Eight are read off the run's own `§11.3:` line at `NAME:1`: `push.*` x3,
//!   `claim.after_cas`, `takeover.*`, `reclaim.*`, `hangup.*` and the negative
//!   control `topo.holding_lock`.
//! * `attach.after_slot_assigned_before_publish` needs the bare `NAME` form (nth
//!   drawn above 1); at `:1` every `spawn_owner` attempt aborts and there is no
//!   `§11.3:` line. The run then fails §3.5, which is the probe being a probe.
//! * Four are read off the child's `crash point <site> hit N, aborting` stderr
//!   at `:1`: `open.after_ownership_lock_before_bind`,
//!   `open.after_create_before_bind`, `topo.after_copy_before_publish` and
//!   `intern.after_hash_cas_before_id_store`.
//!
//! | Fires in a live arena; live peers meet the state | Where |
//! |---|---|
//! | `push.after_seq_odd`, `push.after_data_before_seq_even`, `push.after_seq_even_before_head` | `work` publishes on 40% of operations |
//! | `claim.after_cas` | `work` claims |
//! | `attach.after_slot_assigned_before_publish` | every join; probe with the **bare** form |
//! | `reclaim.after_probe_before_cas` | `work`'s `reap_participants` arm |
//! | `hangup.after_probe_before_cas` | the owner is a child, armed like any other; needs `reap_owner` because the owner is not a worker slot |
//! | `takeover.after_ownership_lock_before_bind` | inside `Tree::inherit_ownership`, which every survivor calls |
//!
//! | Fires only in the creating owner child; the row's claim is still exercised | Where |
//! |---|---|
//! | `open.after_ownership_lock_before_bind`, `open.after_create_before_bind` | the `OpenOutcome::Created` arm; `spawn_owner`'s retry is the "next `open()`" the rows describe |
//!
//! | Fires only in the creating owner child; the named state is **not produced** | Why |
//! |---|---|
//! | `topo.after_copy_before_publish` | `TreeBuilder::build_with` calls `set_parent`; the abort destroys the arena the row is about |
//! | `intern.after_hash_cas_before_id_store` | the owner interns all five names first; no later interner of a name whose arena never existed |
//!
//! | Never fires | Why |
//! |---|---|
//! | `topo.holding_lock` | inside `Tree::reparent`; nothing here reparents the fixed four-edge chain |
//!
//! So "every §11.3 crash point recovers" is not a claim this binary can make at
//! any duration; per-site coverage lives in `tf_tree_core::crash_tests` and
//! `tf_tree/tests/rendezvous.rs`. A forced `--crash-site` can also fail a run for
//! the probe's own reasons (arming every child at `hangup.*:1` churns the owner
//! role faster than the driver can follow).
//!
//! # Killing the owner (§3.5)
//!
//! An **owner child** creates and serves the arena and does nothing else; the
//! driver `SIGKILL`s whichever process holds the role every `--owner-kill-every`.
//! Every child is a potential heir: `work` evaluates `Tree::owner_lost` and calls
//! `Tree::inherit_ownership` when it answers true (§3.5's caller-driven trigger;
//! no daemon). Every owner kill must show both, or the run fails:
//!
//! * **a fresh process can join again**, probed from outside with a real
//!   `Open::new().create(Never)` (an ownerless arena refuses it with
//!   `ArenaHeldButUnreachable` while attached processes keep reading); and
//! * **some survivor recorded an inheritance**, so the join succeeded because
//!   §3.5's trigger ran and not because the role was never vacant.
//!
//! `--no-inherit` is the negative control: nothing inherits and the run must
//! **fail** (`tests/torture.rs` asserts it). A killed owner's replacement
//! rejoins as an ordinary participant; the role exists once, at startup, and is
//! inherited from there.
//!
//! §11.4's ASan clause is `just shm-torture-asan`. There is no
//! `TF_TREE_PARANOID`: every read is validated unconditionally (`Invariant`).
//!
//! # One binary, a `child` mode
//!
//! Children are real processes that re-exec this binary through `current_exe()`:
//! a thread cannot be `SIGKILL`ed out from under its locks, and recovery is
//! defined in terms of what the kernel releases when a process dies.
//!
//! # How a violation gets out of a child
//!
//! A child that sees a bad transform prints one `VIOLATION ...` line to stderr
//! and exits `EXIT_VIOLATION`. A child that fails to join because the owner was
//! killed mid-handshake is expected and retried, not reported.
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

    /// The fewest composed `map -> tool` reads a round must validate for the
    /// verdict to mean anything. Each round attempts 256; the floor is a
    /// lower bound against a *systematic* zero (a reader that stopped finding
    /// the window), not one unlucky round after a writer was killed. It cannot
    /// tell a live arena from a dead one; [`RoundHealth::arena_is_live`] does.
    const MIN_CHAIN_READS_PER_ROUND: u64 = 16;

    /// The same floor for single-edge reads, of which each round attempts 64.
    const MIN_EDGE_READS_PER_ROUND: u64 = 8;

    /// How long after start-up the first owner kill lands: long enough that the
    /// rings are full when the role first goes vacant. It also keeps the 3 s
    /// `--readers-only` and 8 s clean cases in `tests/torture.rs` meaning what
    /// they meant.
    const OWNER_KILL_FIRST: Duration = Duration::from_secs(4);

    /// How long a migration is given before the run is failed; matches
    /// `owner_migration`'s deadline. Not a latency budget (`PHASE2.md` §12.2 and
    /// `just owner-migration` measure that): the failure it guards, nothing
    /// inherited, never recovers however long it waits.
    const OWNER_RECOVERY_DEADLINE: Duration = Duration::from_secs(10);

    /// Operations one attachment performs before detaching and re-joining, so
    /// no single lucky survivor holds the arena for a whole run.
    const OPS_PER_ATTACHMENT: u32 = 2_000;

    /// How many times the operation cap may be extended for a process serving
    /// the rendezvous. Bounded so a stale `owner.pid` marker cannot pin one child
    /// forever. Ten extensions is 22 000 operations in all, which is shorter than
    /// the tenure of some accepted `--owner-kill-every` values: a role holder can
    /// leave through this cap with no census, kill window or `Migration`
    /// (`[diag]` instrument 3, `role-holder-cap-exit`, prints each).
    const MAX_OWNER_CAP_EXTENSIONS: u32 = 10;

    /// The attached read-write population below which the **ordinary** victim
    /// draw stops taking anybody. Three leaves the role holder plus one eligible
    /// heir, the precondition [`kill_the_owner`] censuses for; both arms draw
    /// from one pool. Without it `--children 4 --kill-hz 4` reaches zero attached
    /// participants inside 13 s. Skipped draws are counted and printed.
    const MIN_ATTACHED_FOR_ORDINARY_KILL: u64 = 3;

    /// How long one scheduled owner kill may stay deferred for want of a second
    /// eligible heir before the run calls it a wedge, in `--owner-kill-every`
    /// intervals. Three, not one: a single attempt can catch the fleet mid-churn.
    /// The budget is kept apart from the retry cadence ([`OWNER_KILL_DEFERRAL_RETRY`])
    /// so a short run gets many attempts rather than one.
    const OWNER_KILL_DEFERRAL_BUDGET_INTERVALS: u32 = 3;

    /// How soon a deferred owner kill is retried: a deferral waits for a
    /// replacement's handshake (milliseconds locally, ~150 ms on a cold CI
    /// runner), so it must not cost a whole inter-kill period. Clamped to
    /// `--owner-kill-every` at the call site.
    const OWNER_KILL_DEFERRAL_RETRY: Duration = Duration::from_millis(250);

    /// The shortest a deferral budget may be. `--owner-kill-every 0s` is accepted
    /// and would otherwise derive a zero budget and make the first deferral
    /// fatal; one second is four attempts at the 250 ms retry.
    const MIN_OWNER_KILL_DEFERRAL_BUDGET: Duration = Duration::from_secs(1);

    /// How often the observer reads while a migration is in flight. Throttled so
    /// a millisecond-long event does not add hundreds of near-empty rounds that
    /// drag the per-round read floor down.
    const MIGRATION_OBSERVE_EVERY: Duration = Duration::from_millis(5);

    /// Frames in the torture topology: one chain of four dynamic edges, so a
    /// `map -> tool` lookup composes every edge and a bad sample anywhere shows.
    const CHAIN: &[(&str, &str)] = &[
        ("map", "odom"),
        ("odom", "base"),
        ("base", "arm"),
        ("arm", "tool"),
    ];

    /// The topology every participant agrees on; a participant with a different
    /// layout is refused by the rendezvous.
    fn layout() -> TreeBuilder {
        let mut b = TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
        for (parent, child) in CHAIN {
            // 64 slots: the ring wraps constantly, which is where a reader races a writer.
            b = b.dynamic_edge(parent, child, EdgeCfg::new(Capacity::slots(64)));
        }
        b
    }

    /// What a reader checks on every transform (§11.4: no non-unit quaternion, no
    /// NaN); it cannot be turned off. The 1e-6 norm bound is loose on purpose:
    /// composed float error is arithmetic, torn memory is nowhere near norm 1.
    #[derive(Debug, Clone, Copy)]
    struct Invariant;

    impl Invariant {
        /// `Ok(())`, or why this transform cannot be a consistent sample.
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

    /// Wall-clock nanoseconds, the stamp every participant uses. It must be a
    /// shared clock: with per-process counters every hand-over fails
    /// `NonMonotonicStamp` and lookups land outside the ring, so a run validates
    /// nothing and looks clean.
    fn now_nanos() -> i64 {
        // `unwrap_or_default` covers a pre-epoch clock (a host problem).
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or_default()
    }

    /// A 64-bit xorshift: replayable with `--seed`, no `rand` dependency.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Rng {
            // Zero is a fixed point of xorshift64.
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
        /// How often the current rendezvous holder is `SIGKILL`ed; `None` disables
        /// (`--no-kill-owner`).
        owner_kill_every: Option<Duration>,
        /// §3.5's negative control: children never call `Tree::owner_lost`, so the
        /// run must fail.
        no_inherit: bool,
        /// `--defer-owner-kills N`: force the first `N` owner-kill attempts to
        /// defer. The positive control for the deferral path, which a real run
        /// reaches only through page-cache stalls no flag reproduces. It defers
        /// *after* the pre-kill census so `heirs_before` and `starved` stay
        /// truthful.
        defer_owner_kills: usize,
        /// `--crash-site NAME`: arm **this** §11.3 site in every child instead of
        /// a random one in a tenth. A reachability probe (header table), not
        /// §11.4's configuration.
        crash_site: Option<String>,
        /// Megabytes of dirty anonymous memory each child holds: the positive
        /// control for the kill-window class (see [`kill_window_path`]), zero by
        /// default. It makes a ~1% defect reproducible in a run worth waiting for.
        victim_ballast_mb: usize,
        /// Milliseconds to hold the owner **stopped** before killing it: the
        /// portable positive control for the same window, zero by default. The
        /// ballast depends on 4 KiB pages (with `transparent_hugepage=always`,
        /// GitHub's runners, the reap drops to ~1.5 ms and it does nothing). A
        /// stopped owner keeps its socket open, so nothing inherits and nobody
        /// can join: the blind window, for exactly this long, on any host.
        stop_owner_ms: u64,
    }

    /// `30m`, `120s`, `500ms`, `1h`, or a bare number of seconds. `m` and `h`
    /// exist because `just shm-torture` defaults to `--duration 30m`.
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
            // On by default: an arm that had to be asked for would be absent from
            // the one command §13 names.
            owner_kill_every: Some(Duration::from_secs(8)),
            no_inherit: false,
            defer_owner_kills: 0,
            crash_site: None,
            victim_ballast_mb: 0,
            stop_owner_ms: 0,
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
                // Self-test: one child publishes a bad transform; the run must FAIL.
                "--inject-violation" => a.inject = true,
                // Negative control for the read floor: children read but never
                // publish, so nothing is validated and the run must FAIL. Never
                // for a real soak.
                "--readers-only" => a.readers_only = true,
                // §3.5's owner death, on a schedule slower than the child kills:
                // killing faster than survivors notice measures the poll interval.
                "--owner-kill-every" => {
                    a.owner_kill_every = Some(parse_duration(&value("--owner-kill-every")?)?);
                }
                // Turns the arm off, so a bisect can separate its failures.
                "--no-kill-owner" => a.owner_kill_every = None,
                // §3.5's negative control: nothing inherits, the run must FAIL
                // (`tests/torture.rs` asserts on the message).
                "--no-inherit" => a.no_inherit = true,
                // Positive control for the deferral path (see
                // [`Args::defer_owner_kills`]); refused without the arm it controls.
                "--defer-owner-kills" => {
                    a.defer_owner_kills = value("--defer-owner-kills")?.parse()?;
                }
                // Positive control for the kill-window class (see
                // [`Args::victim_ballast_mb`]); 512 is roughly a 49 ms window.
                "--victim-ballast-mb" => {
                    a.victim_ballast_mb = value("--victim-ballast-mb")?.parse()?;
                }
                // Portable sibling of `--victim-ballast-mb` (see
                // [`Args::stop_owner_ms`]); independent of the THP setting.
                "--stop-owner-ms" => {
                    a.stop_owner_ms = value("--stop-owner-ms")?.parse()?;
                }
                // The flag that makes the §11.3 distinction real.
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
                // Validated against the published list, never a literal, so a typo
                // cannot arm nothing and read as "unreachable". `NAME` or
                // `NAME:nth`: a site reached once per process never fires at `:2`.
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
                // Recorded, not acted on, so cross-flag validation below runs
                // even for `--help`.
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
        // Too few children cannot exercise §3.5 and must be refused, not passed.
        // The floor is `MIN_ATTACHED_FOR_ORDINARY_KILL` plus one: at exactly the
        // floor a replacement's handshake would throttle every ordinary draw.
        let children_floor = MIN_ATTACHED_FOR_ORDINARY_KILL as usize + 1;
        if a.owner_kill_every.is_some() && a.children < children_floor {
            bail!(
                "--children {} with the owner-kill arm on cannot sustain the workload: the \
                 owner is a child, so a kill has to leave both a survivor eligible to inherit \
                 and an attached pool at or above MIN_ATTACHED_FOR_ORDINARY_KILL ({}), below \
                 which the ordinary victim draw stops. At this setting §3.5 would be untestable \
                 and the run would fail at its `kills == 0` floor after burning the whole \
                 duration, for a reason that says nothing about the arena. Use --children {} or \
                 more, or --no-kill-owner.",
                a.children,
                MIN_ATTACHED_FOR_ORDINARY_KILL,
                children_floor
            );
        }
        // Refused rather than silently doing nothing.
        if a.defer_owner_kills > 0 && a.owner_kill_every.is_none() {
            bail!(
                "--defer-owner-kills {} with --no-kill-owner: there is no owner-kill arm to \
                 defer. It is the positive control for the deferral path, so it needs the arm \
                 it controls.",
                a.defer_owner_kills
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
                 [--victim-ballast-mb N] [--stop-owner-ms N] \
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
                "  --victim-ballast-mb is the POSITIVE control for the kill window: each \
                 child holds N MB of dirty memory, which widens the interval between the \
                 SIGKILL and the victim's `exit_files` — the interval in which the owner \
                 is dead, nothing can inherit yet, and a survivor that detaches cannot \
                 rejoin. 512 wedges an unfixed --children 4 run within a few kills on a \
                 plain build; ASan reaches the same width by accident at 43-49 MB/child. \
                 It is NOT portable: on a host with transparent_hugepage=always the pages \
                 are 2 MiB and the window collapses, which is why the regression test uses \
                 the flag below instead."
            );
            println!(
                "  --defer-owner-kills N is the positive control for the DEFERRAL path: the \
                 first N owner-kill attempts defer whatever the population is, as if the fleet \
                 held only the role holder. A real deferral needs the attached pool below its \
                 floor at the instant the arm fires, which the CLI cannot arrange — --children \
                 is refused below the floor, and on an unloaded host a replacement's handshake \
                 is over in about a millisecond. Refused with --no-kill-owner."
            );
            println!(
                "  --stop-owner-ms is the PORTABLE positive control for the same window: \
                 SIGSTOP the owner for N ms before killing it. A stopped owner holds its \
                 rendezvous socket open, so nothing can inherit, and it has stopped serving, \
                 so nothing can join — the blind window, for as long as you ask, with no \
                 dependence on the victim's page size. 300 wedges an unfixed --children 4 \
                 run on its FIRST owner kill."
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

    /// A scratch runtime directory, removed on drop.
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
        /// Carried per child so a respawn of the injecting slot keeps injecting
        /// and the self-test does not depend on which slot the killer picked.
        inject: bool,
        /// The `TF_TREE_CRASH_AT` this child was started with, so an aborting
        /// child is reported by site name (see [`CrashLedger`]).
        crash_at: Option<String>,
        /// First round at which this armed child showed `CoreDumping: 1`;
        /// diagnostic (see [`note_core_dumping`]).
        dumping_seen: Option<Instant>,
    }

    impl Drop for Kid {
        fn drop(&mut self) {
            let _ = self.proc.kill();
            let _ = self.proc.wait();
        }
    }

    /// What a `--crash-points` run knows about §11.3. `armed`/`aborted` separate
    /// "armed" from "fired"; the name lists separate "unreachable here" from "the
    /// race went the other way" (see the header's reachability tables).
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

    /// `armed_site` where the feature exists, `None` where it does not; the
    /// `cfg` lives here so both builds share control flow.
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
                    // The site, not `site:nth`: the report is about coverage.
                    ledger.armed_sites.push(site_of(spec).to_string());
                }
                return spec;
            }
        }
        let _ = (enabled, forced, rng, ledger);
        None
    }

    /// The site name out of a `TF_TREE_CRASH_AT` spec (`<name>:<nth_hit>`).
    fn site_of(spec: &str) -> &str {
        spec.split_once(':').map_or(spec, |(name, _)| name)
    }

    /// Every §11.3 site this build carries, read from the published consts
    /// (`tf_tree_core::crash::SITES` and `tf_tree::CRASH_SITES`), never
    /// re-spelled: a typo would arm nothing and look clean.
    #[cfg(feature = "crash-points")]
    fn all_sites() -> Vec<&'static str> {
        let mut v: Vec<&'static str> = tf_tree_core::crash::SITES.to_vec();
        v.extend_from_slice(tf_tree::CRASH_SITES);
        v
    }

    /// `docs/PHASE2.md` §11.4: "a random crash point armed in 10% of children".
    /// Returns `TF_TREE_CRASH_AT`'s value, or `None`. `nth_hit` is drawn too, so
    /// states past the first hit are sampled.
    #[cfg(feature = "crash-points")]
    fn armed_site(rng: &mut Rng, forced: Option<&str>) -> Option<String> {
        // `--crash-site` arms every child at the named site.
        if let Some(site) = forced {
            // `NAME:nth` passes through; a bare `NAME` draws its hit count.
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

    /// The runtime directory every process in this run shares, passed in
    /// `TF_TREE_RUNTIME_DIR`, which the rendezvous also keys on.
    fn runtime_dir() -> PathBuf {
        std::env::var_os("TF_TREE_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    /// Where the current rendezvous holder records its pid, so the driver knows
    /// whom to kill after a migration (a pipe per child would need a protocol
    /// that survives `SIGKILL` mid-line). The file is evidence, never the
    /// criterion: what decides success is a *fresh process joining*
    /// ([`kill_the_owner`]).
    fn owner_pid_path(dir: &Path) -> PathBuf {
        dir.join("owner.pid")
    }

    /// Present exactly while the driver is inside an owner kill: the window in
    /// which leaving the arena is irreversible.
    ///
    /// A `SIGKILL`ed process releases its lock byte and socket in `exit_files()`,
    /// after `exit_mm()`, so between `kill()` and `wait()` the owner is
    /// undetectably dead (`owner_lost` is a socket hangup; nothing can inherit).
    /// A survivor that detaches then cannot rejoin (`ArenaHeldButUnreachable`),
    /// and if all leave the arena is absorbing. The window scales with the
    /// victim's dirty pages (~0.09 ms per resident MB), which is why ASan hits it
    /// by accident and `--victim-ballast-mb` on purpose.
    ///
    /// This marker suppresses only the harness's own detach churn for one reap
    /// (`docs/PHASE2.md` §0.0, `docs/decisions/0057`); it does not suppress a
    /// kill, a violation or an inheritance.
    fn kill_window_path(dir: &Path) -> PathBuf {
        dir.join("kill.in_progress")
    }

    /// Open the window, **before** `kill()`, so no child sees the corpse without it.
    fn open_kill_window(dir: &Path, victim: u32) {
        let _ = std::fs::write(kill_window_path(dir), victim.to_string());
    }

    /// Close it after the post-reap census, which the verdict reads.
    fn close_kill_window(dir: &Path) {
        let _ = std::fs::remove_file(kill_window_path(dir));
    }

    /// An existence check, read on the detach arm only.
    fn kill_window_open(dir: &Path) -> bool {
        kill_window_path(dir).exists()
    }

    /// One line per successful inheritance, appended by the heir. Append-only:
    /// a read-modify-write counter would lose events to `SIGKILL`.
    fn inherited_path(dir: &Path) -> PathBuf {
        dir.join("inherited.log")
    }

    /// Where a child records which owner it last attached under (one file per
    /// pid, one owner pid). `check_recovery`'s leak check must separate a record
    /// the current owner's hangup callback could have collected from one it could
    /// not (`docs/decisions/0043`: a participant that attached under a since-dead
    /// owner never registers with the new one). Only the child knows, and it
    /// re-attaches often. `write` then `rename`: last-write-wins answers "the
    /// last owner".
    fn attach_dir(dir: &Path) -> PathBuf {
        dir.join("attached-under")
    }

    /// Record the owner this process is about to attach under, **before** the
    /// `open()`. The worst case is then a marker naming a dead owner (lost
    /// coverage on one record), never a record with no marker (a false failure).
    fn record_attachment(dir: &Path, under: Option<u32>) {
        let d = attach_dir(dir);
        let _ = std::fs::create_dir_all(&d);
        let tmp = d.join(format!(".tmp.{}", std::process::id()));
        let final_path = d.join(std::process::id().to_string());
        if std::fs::write(&tmp, under.unwrap_or(0).to_string()).is_ok()
            && std::fs::rename(&tmp, &final_path).is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// `(child pid, owner pid it last attached under)` for every child that recorded one.
    fn attachments(dir: &Path) -> Vec<(u32, u32)> {
        let Ok(entries) = std::fs::read_dir(attach_dir(dir)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            if let Ok(under) = text.trim().parse::<u32>() {
                out.push((pid, under));
            }
        }
        out
    }

    /// Publish this process as the owner, atomically (`write` then `rename`).
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

    /// Where a child records a detach it skipped because a kill window was open,
    /// one `<pid>` line per skip. The driver counts them at teardown so a run
    /// that leaned on the exemption says how hard.
    fn detach_skips_path(dir: &Path) -> PathBuf {
        dir.join("detach_skips.log")
    }

    fn record_detach_skip(dir: &Path) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(detach_skips_path(dir))
        {
            let _ = f.write_all(format!("{}\n", std::process::id()).as_bytes());
        }
    }

    /// How many detaches the kill-window exemption suppressed over the whole run.
    fn detach_skip_count(dir: &Path) -> usize {
        std::fs::read_to_string(detach_skips_path(dir))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// Record that this process inherited the role: one `write_all` of one
    /// formatted buffer to an `O_APPEND` descriptor. Not `writeln!`, which is two
    /// `write(2)` calls and can be killed between the digits and the newline.
    fn record_inheritance(dir: &Path) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(inherited_path(dir))
        {
            let _ = f.write_all(format!("{}\n", std::process::id()).as_bytes());
        }
    }

    /// Where every survivor records what §3.5's trigger answered, one
    /// `<pid> <outcome>` line per evaluation. [`inherited_path`] records only
    /// winners, so it cannot tell "everyone was refused" (engine) from "nobody
    /// asked" (population). Read with [`trigger_tally`].
    fn triggers_path(dir: &Path) -> PathBuf {
        dir.join("triggers.log")
    }

    /// A short, stable tag for one trigger outcome. [`Inheritance`] is
    /// `#[non_exhaustive]`, so the wildcard falls back to `Debug`.
    fn trigger_tag(outcome: &Result<Inheritance, tf_tree::OpenError>) -> String {
        match outcome {
            Ok(Inheritance::Inherited) => "inherited".to_string(),
            Ok(Inheritance::OwnerAlive) => "owner-alive".to_string(),
            Ok(Inheritance::Contended) => "contended".to_string(),
            Ok(Inheritance::ReadOnly) => "read-only".to_string(),
            Ok(Inheritance::NotApplicable) => "not-applicable".to_string(),
            Ok(other) => format!("ok-{other:?}"),
            Err(e) => format!("err-{e:?}"),
        }
    }

    /// Append one trigger outcome, up to `TRIGGER_LOG_CAP` per process: a
    /// survivor evaluates the trigger at ~1 kHz for as long as the role is
    /// vacant, and a truncated count still separates "nobody asked" from
    /// "everybody refused".
    fn record_trigger_outcome(
        dir: &Path,
        written: &mut usize,
        outcome: &Result<Inheritance, tf_tree::OpenError>,
    ) {
        const TRIGGER_LOG_CAP: usize = 256;
        if *written >= TRIGGER_LOG_CAP {
            return;
        }
        *written += 1;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(triggers_path(dir))
        {
            // One `write_all` of one line, like `record_inheritance`, so
            // children's lines cannot interleave.
            let _ = f
                .write_all(format!("{} {}\n", std::process::id(), trigger_tag(outcome)).as_bytes());
        }
    }

    /// The run's trigger outcomes, most frequent first.
    fn trigger_tally(dir: &Path) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        let Ok(text) = std::fs::read_to_string(triggers_path(dir)) else {
            return counts;
        };
        for line in text.lines() {
            let Some((_, tag)) = line.trim().split_once(' ') else {
                continue;
            };
            if let Some(slot) = counts.iter_mut().find(|(t, _)| t == tag) {
                slot.1 += 1;
            } else {
                counts.push((tag.to_string(), 1));
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        counts
    }

    /// [`trigger_tally`] as the one line the teardown summary and the wedge
    /// report both print. The empty case is a sentence: "no survivor ever
    /// evaluated the trigger" separates a population condition from an engine
    /// refusal.
    fn trigger_tally_line(dir: &Path) -> String {
        let counts = trigger_tally(dir);
        if counts.is_empty() {
            return "none recorded — no survivor ever evaluated the trigger".to_string();
        }
        counts
            .iter()
            .map(|(tag, n)| format!("{tag}={n}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn inheritance_count(dir: &Path) -> usize {
        std::fs::read_to_string(inherited_path(dir))
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }

    /// Every process that has ever inherited the role, from the ledger. The whole
    /// set, not a sample: the `owner.pid` marker is last-write-wins and turns
    /// over faster than a driver round, and each hole in a sampled exempt set is
    /// a healthy run failing `check_recovery`'s strict path. It does not name
    /// the creator, which the caller adds.
    fn inherited_pids(dir: &Path) -> Vec<u32> {
        std::fs::read_to_string(inherited_path(dir))
            .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
            .unwrap_or_default()
    }

    /// The prefix of every diagnostic line, the driver's and the children's.
    ///
    /// # Instruments, not checks
    ///
    /// Five instruments print under this prefix, each stating the hypothesis it
    /// tests and how many lines it can print (the 2026-09-12 and 2026-09-13
    /// nightlies reached the absorbing state through an owner departure
    /// [`kill_the_owner`] never brackets). **None may change what the run
    /// does**: no RNG draw, predicate, floor, deferral, detach, cap, kill,
    /// teardown decision or sleep is added, and nothing reads `/proc` or a file
    /// on the per-operation path.
    ///
    /// Every line is one `write(2)` ([`driver_diag`], [`child_diag`]); a line
    /// past `PIPE_BUF` (4096 bytes) can still be split. `tests/torture.rs` finds
    /// its numbers by phrase (` kills, `, `§3.5 owner kill 1:`, `UNRECOVERABLE`,
    /// `a fresh process joined`, `recovery:`, and some whole-line prefixes), so
    /// diagnostic wording must avoid them.
    const DIAG: &str = "shm_torture: [diag]";

    /// `CLOCK_REALTIME` as `seconds.nanoseconds`: a writer-side stamp is the only
    /// order the driver's stdout and the children's stderr share.
    fn wall_stamp() -> String {
        stamp_of(std::time::SystemTime::now())
    }

    /// [`wall_stamp`] for an instant already captured.
    fn stamp_of(t: std::time::SystemTime) -> String {
        let d = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        format!("{}.{:09}", d.as_secs(), d.subsec_nanos())
    }

    /// One diagnostic line from a child as **one** `write(2)`: `eprintln!` writes
    /// piecewise and children share the driver's stderr pipe.
    fn child_diag(body: &str) {
        let line = format!("{DIAG} {body}\n");
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }

    /// One diagnostic line from the driver as **one** `write(2)`: `println!`
    /// hands the body and newline to the `LineWriter` separately, so a long line
    /// can be split by a child's stderr line. See [`DIAG`].
    fn driver_diag(line: &str) {
        let mut buf = String::with_capacity(line.len() + 1);
        buf.push_str(line);
        buf.push('\n');
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(buf.as_bytes());
        let _ = out.flush();
    }

    /// `State/CoreDumping/Threads/wchan` for one process from `/proc`, or
    /// `absent`. `Threads` tells a serving owner from a plain joiner; `wchan`
    /// names where a sleeping main thread is blocked.
    fn proc_brief(pid: u32) -> String {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            return "absent".to_string();
        };
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .map(|v| v.trim().to_string())
        };
        let state = field("State:")
            .and_then(|s| s.split_whitespace().next().map(str::to_string))
            .unwrap_or_else(|| "?".to_string());
        let dumping = field("CoreDumping:").unwrap_or_else(|| "?".to_string());
        let threads = field("Threads:").unwrap_or_else(|| "?".to_string());
        let wchan = std::fs::read_to_string(format!("/proc/{pid}/wchan"))
            .ok()
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty())
            .unwrap_or_else(|| "?".to_string());
        format!("{state}/cd{dumping}/thr{threads}/{wchan}")
    }

    /// Whether `/proc/<pid>/status` reads `CoreDumping: 1`.
    fn proc_core_dumping(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("CoreDumping:"))
                    .map(|v| v.trim() == "1")
            })
            .unwrap_or(false)
    }

    /// **Instrument 1: the host's core-dump configuration, once, at startup.**
    ///
    /// Hypothesis (2026-09-12): an aborting owner dumps core before
    /// `exit_files` releases its socket and byte 0, so `owner_lost()` answers
    /// `false` for as long as a pipe helper takes (explained in
    /// `docs/decisions/0057`). `RLIMIT_CORE` is read from `/proc/self/limits`
    /// (`getrlimit` needs `unsafe`). The torture recipes set `prlimit
    /// --core=1:1`; a run without it is reported by [`core_dump_warning`].
    ///
    /// Bound: one line per run, plus at most one warning line.
    fn host_diag(crash_points: bool) -> (String, Option<String>) {
        let read = |p: &str| {
            std::fs::read_to_string(p)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|e| format!("<unreadable: {e}>"))
        };
        let limits = std::fs::read_to_string("/proc/self/limits").ok();
        let core_rlimit = match limits.as_deref().and_then(core_rlimit_columns) {
            Some((soft, hard)) => format!("soft={soft} hard={hard}"),
            None => "<unreadable>".to_string(),
        };
        let core_pattern = read("/proc/sys/kernel/core_pattern");
        let warning = core_dump_warning(crash_points, &core_pattern, limits.as_deref());
        let line = format!(
            "{DIAG} host: core_pattern=`{}` core_pipe_limit={} core_rlimit {} osrelease={} \
             stamp={}",
            core_pattern,
            read("/proc/sys/kernel/core_pipe_limit"),
            core_rlimit,
            read("/proc/sys/kernel/osrelease"),
            wall_stamp()
        );
        (line, warning)
    }

    /// **`docs/decisions/0057` Decision 6's warning: armed crash points on a host
    /// whose crash helper runs inside every armed child's exit.**
    ///
    /// With a pipe `core_pattern` the kernel runs the helper before the child's
    /// files close, so an armed owner or heir holds the role that long. Only a
    /// soft `RLIMIT_CORE` of exactly 1 refuses a pipe dump (the recipes set
    /// `prlimit --core=1:1 --`). Reported, never a verdict; only a `|` pattern is
    /// recognised, and an unreadable limit warns. It takes the whole limits text
    /// so the soft-column choice is inside what
    /// `the_core_dump_warning_names_only_a_dumping_pipe` reaches.
    fn core_dump_warning(
        crash_points: bool,
        core_pattern: &str,
        limits: Option<&str>,
    ) -> Option<String> {
        let soft = limits.and_then(core_rlimit_columns).map(|(soft, _)| soft);
        if !crash_points || !core_pattern.starts_with('|') || soft == Some("1") {
            return None;
        }
        Some(format!(
            "{DIAG} warning: --crash-points with a pipe core_pattern and a core soft limit of \
             {}: an armed child's abort runs the host's crash helper before its socket and \
             byte 0 are released, so this run's recovery depends on that helper. A soft limit \
             of 0 does not stop a pipe dump; only 1 does, which `just \
             shm-torture-crash-points` sets with `prlimit --core=1:1 --` \
             (docs/decisions/0057 Decision 6). Reported, not a verdict.",
            soft.unwrap_or("<unreadable>")
        ))
    }

    /// `(soft, hard)` from the `Max core file size` row of `/proc/self/limits`, `?` for a missing column.
    fn core_rlimit_columns(limits: &str) -> Option<(&str, &str)> {
        limits
            .lines()
            .find_map(|l| l.strip_prefix("Max core file size"))
            .map(|rest| {
                let mut f = rest.split_whitespace();
                (f.next().unwrap_or("?"), f.next().unwrap_or("?"))
            })
    }

    /// One participant slot as [`census_with`] read it, in the pass that decided.
    #[derive(Clone, Copy)]
    struct SlotSeen {
        slot: u32,
        /// The identity record's pid, when `LIVE`.
        pid: Option<u32>,
        /// `participant_alive`, which `slots_alive` counts.
        alive: bool,
    }

    /// `[slot:pid, ...]`, `(dead-record)` on a `LIVE` record with a free byte,
    /// `(driver)` on the driver's observer.
    fn seen_list(seen: &[SlotSeen], driver: Option<u32>) -> String {
        let items: Vec<String> = seen
            .iter()
            .map(|s| {
                format!(
                    "{}:{}{}{}",
                    s.slot,
                    s.pid.map_or_else(|| "?".to_string(), |p| p.to_string()),
                    if s.alive { "" } else { "(dead-record)" },
                    if s.pid.is_some() && s.pid == driver {
                        "(driver)"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    /// **Instrument 2: the population by identity, not by count.**
    ///
    /// Hypothesis (2026-09-13): the kill-time census is bimodal (6, 5, 4, 3, or
    /// else 1, never 2), pointing at a common-mode stall on the rejoin path. Per
    /// tracked slot this prints the pid, what the census `seen` made of it,
    /// whether the owner marker names it, and its `/proc` state (including
    /// `CoreDumping`, since a dumping child holds its byte and is counted).
    /// `census_when` names when `seen` was read; `/proc` is read at the print.
    ///
    /// Bound: one line per deferred owner kill, plus one at an unrecoverable
    /// kill.
    fn population_diag(
        what: &str,
        census_when: &str,
        dir: &Path,
        observer: &Tree,
        owner_kid: Option<&Kid>,
        kids: &[Option<Kid>],
        seen: &[SlotSeen],
    ) -> String {
        let marker = read_owner_pid(dir);
        let driver = std::process::id();
        let census_of = |pid: u32| -> String {
            let mine: Vec<&SlotSeen> = seen.iter().filter(|s| s.pid == Some(pid)).collect();
            if let Some(s) = mine.iter().find(|s| s.alive) {
                format!("counted@slot{}", s.slot)
            } else if let Some(s) = mine.first() {
                format!("dead-record@slot{}", s.slot)
            } else {
                "absent".to_string()
            }
        };
        let entry = |label: String, pid: u32| {
            format!(
                "{label}={pid} census={} marker={} proc={}",
                census_of(pid),
                if marker == Some(pid) { "yes" } else { "no" },
                proc_brief(pid)
            )
        };
        let mut tracked: Vec<u32> = Vec::new();
        let mut entries: Vec<String> = Vec::new();
        for (i, kid) in kids.iter().enumerate() {
            match kid {
                Some(k) => {
                    tracked.push(k.proc.id());
                    entries.push(entry(format!("k{i}"), k.proc.id()));
                }
                None => entries.push(format!("k{i}=empty")),
            }
        }
        if let Some(k) = owner_kid {
            tracked.push(k.proc.id());
            entries.push(entry("owner-child".to_string(), k.proc.id()));
        }
        let untracked: Vec<SlotSeen> = seen
            .iter()
            .copied()
            .filter(|s| s.pid.is_none_or(|p| !tracked.contains(&p)))
            .collect();
        format!(
            "{DIAG} population [{what}] stamp={} census_at={census_when} census_alive={} \
             marker={} observer=pid{driver}/slot{} | {} | untracked census slots {}",
            wall_stamp(),
            seen.iter().filter(|s| s.alive).count(),
            marker.map_or_else(|| "none".to_string(), |p| p.to_string()),
            observer.participant_slot(),
            entries.join("; "),
            seen_list(&untracked, Some(driver)),
        )
    }

    /// `/proc/loadavg`'s last field: the most recently allocated pid.
    fn last_pid() -> String {
        std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.split_whitespace().last().map(str::to_string))
            .unwrap_or_else(|| "?".to_string())
    }

    /// What instrument 5's poll knows about one child's core dump ([`reap_diag`]).
    #[derive(Clone, Copy)]
    enum DumpPoll {
        /// Unarmed, or reaped by a blocking `wait()` before any round ran.
        NotPolled,
        /// Polled once per round and never seen dumping (a dump shorter than a round can be missed).
        NeverSeen,
        /// First seen dumping at this instant.
        SeenAt(Instant),
    }

    impl DumpPoll {
        fn of(kid: &Kid) -> DumpPoll {
            match (&kid.crash_at, kid.dumping_seen) {
                (None, _) => DumpPoll::NotPolled,
                (Some(_), None) => DumpPoll::NeverSeen,
                (Some(_), Some(t)) => DumpPoll::SeenAt(t),
            }
        }
    }

    /// One reap as it happened: the status and two clock reads, nothing that
    /// touches a file, so a site inside [`kill_the_owner`]'s kill window pays
    /// only two vDSO reads. Formatted later by [`reap_diag`].
    #[derive(Clone, Copy)]
    struct Reaped {
        status: std::process::ExitStatus,
        at: Instant,
        wall: std::time::SystemTime,
    }

    impl Reaped {
        fn now(status: std::process::ExitStatus) -> Reaped {
            Reaped {
                status,
                at: Instant::now(),
                wall: std::time::SystemTime::now(),
            }
        }
    }

    /// Everything [`reap_diag`] needs about one reaped child, owned.
    struct Reap {
        site: &'static str,
        slot: Option<usize>,
        pid: u32,
        crash_at: Option<String>,
        dump: DumpPoll,
        reaped: Reaped,
    }

    impl Reap {
        /// For a site outside any measured interval. Clones `crash_at`.
        fn of(site: &'static str, slot: Option<usize>, kid: &Kid, reaped: Reaped) -> Reap {
            Reap {
                site,
                slot,
                pid: kid.proc.id(),
                crash_at: kid.crash_at.clone(),
                dump: DumpPoll::of(kid),
                reaped,
            }
        }

        /// For a site inside [`kill_the_owner`]'s kill window: **moves** `crash_at`
        /// out so the capture does not allocate.
        fn taken_from(
            site: &'static str,
            slot: Option<usize>,
            kid: &mut Kid,
            reaped: Reaped,
        ) -> Reap {
            let dump = DumpPoll::of(kid);
            Reap {
                site,
                slot,
                pid: kid.proc.id(),
                crash_at: kid.crash_at.take(),
                dump,
                reaped,
            }
        }
    }

    /// Where a worker that held the rendezvous role records that it **left the
    /// role alive**, one `<pid>` line per departure. Diagnostic only.
    ///
    /// Instrument 5 cannot ask the `owner.pid` marker: a migration completes in
    /// about a millisecond, faster than a driver round (see [`inherited_pids`]),
    /// and a worker can stop holding the role and go on living (the operation cap
    /// at the end of [`work`], after which [`child`] rejoins under the same pid).
    /// So a worker held the role when it exited exactly when its inheritances
    /// outnumber its departures. The creating owner child appears in neither
    /// ledger. Appended after the `Tree` is dropped, so the write cannot lengthen
    /// the tenure; a kill landing in the microsecond gap before it can read as a
    /// role holder in teardown's batch. One `write(2)`.
    fn role_left_path(dir: &Path) -> PathBuf {
        dir.join("diag_role_left.log")
    }

    fn record_role_left(dir: &Path) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(role_left_path(dir))
        {
            let _ = f.write_all(format!("{}\n", std::process::id()).as_bytes());
        }
    }

    /// The two role ledgers and the marker, read once for a reap or teardown.
    struct RoleLedgers {
        inherited: Vec<u32>,
        left: Vec<u32>,
        marker: Option<u32>,
    }

    impl RoleLedgers {
        /// Three small file reads, called only where a line may follow.
        fn read(dir: &Path) -> RoleLedgers {
            RoleLedgers {
                inherited: inherited_pids(dir),
                left: std::fs::read_to_string(role_left_path(dir))
                    .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
                    .unwrap_or_default(),
                marker: read_owner_pid(dir),
            }
        }

        fn of(&self, pid: u32, creator: bool) -> RoleAtReap {
            RoleAtReap::Read {
                creator,
                inherited: self.inherited.iter().filter(|&&p| p == pid).count(),
                left: self.left.iter().filter(|&&p| p == pid).count(),
                marker: self.marker,
            }
        }
    }

    /// What the driver knows, at a reap, about the reaped process and the role
    /// (see [`role_left_path`]).
    #[derive(Clone, Copy)]
    enum RoleAtReap {
        /// Not read: the driver killed this process knowing what it was.
        NotAsked,
        Read {
            /// The creating owner child.
            creator: bool,
            /// Lines naming this pid in [`inherited_path`].
            inherited: usize,
            /// Lines naming this pid in [`role_left_path`].
            left: usize,
            /// The marker when the ledgers were read.
            marker: Option<u32>,
        },
    }

    impl RoleAtReap {
        fn held(self) -> bool {
            match self {
                RoleAtReap::NotAsked => false,
                RoleAtReap::Read {
                    creator,
                    inherited,
                    left,
                    ..
                } => creator || inherited > left,
            }
        }

        fn field(self, pid: u32) -> String {
            match self {
                RoleAtReap::NotAsked => "role=not-asked".to_string(),
                RoleAtReap::Read {
                    creator,
                    inherited,
                    left,
                    marker,
                } => {
                    let counts = format!("(inherited={inherited},left-alive={left})");
                    let role = if creator {
                        "held-at-exit(creating-owner)".to_string()
                    } else if inherited > left {
                        format!("held-at-exit{counts}")
                    } else if inherited > 0 {
                        format!("held-earlier-and-left-alive{counts}")
                    } else {
                        "never-held".to_string()
                    };
                    let marker = match marker {
                        Some(m) if m == pid => "names-this-pid".to_string(),
                        Some(m) => format!("no-longer-this-pid(now={m})"),
                        None => "none".to_string(),
                    };
                    format!("role={role} marker={marker}")
                }
            }
        }
    }

    /// **Instrument 5: owner deaths nothing scheduled, and every reaped abort.**
    ///
    /// Hypothesis (2026-09-12): the heir armed at `hangup.after_probe_before_cas`
    /// died inside its serving thread and [`reap_finished`] counted it as an
    /// ordinary abort. `role` says whether it held the role at exit (from the
    /// ledgers, [`role_left_path`]); `held_tag` is the tag a held role earns at
    /// this site, or `None` where that is expected. `last_pid` hints at a
    /// core-dump helper chain; `dump_seen` is how long before the reap the poll
    /// first saw `CoreDumping: 1`.
    ///
    /// A child `SIGKILL`ed while dumping reaps as `SIGKILL`, not `SIGABRT` (6.8,
    /// pipe `core_pattern`), so at a driver kill a poll sighting prints the line
    /// too (`was-dumping-when-killed`) and [`CrashLedger`] does not count such a
    /// child as aborted.
    ///
    /// Bound: at most one line per reaped child that aborted, was seen dumping,
    /// or held the role at a site whose `held_tag` is set.
    fn reap_diag(r: &Reap, role: RoleAtReap, held_tag: Option<&str>) -> Option<String> {
        use std::os::unix::process::ExitStatusExt as _;
        let status = r.reaped.status;
        let abort = status.signal() == Some(libc::SIGABRT);
        let held = held_tag.is_some() && role.held();
        let seen_dumping = matches!(r.dump, DumpPoll::SeenAt(_));
        if !abort && !held && !seen_dumping {
            return None;
        }
        Some(format!(
            "{DIAG} reap{}{} site={} slot={} pid={} status=`{status}` core_dumped={} armed={} \
             {} stamp={} last_pid={} dump_seen={}",
            match held_tag {
                Some(tag) if held => format!(" {tag}"),
                _ => String::new(),
            },
            if seen_dumping && status.signal() == Some(libc::SIGKILL) {
                " was-dumping-when-killed"
            } else {
                ""
            },
            r.site,
            r.slot.map_or_else(|| "-".to_string(), |s| s.to_string()),
            r.pid,
            status.core_dumped(),
            r.crash_at.as_deref().unwrap_or("no"),
            role.field(r.pid),
            stamp_of(r.reaped.wall),
            last_pid(),
            match r.dump {
                DumpPoll::NotPolled => "not-polled".to_string(),
                DumpPoll::NeverSeen => "never(polled once per driver round)".to_string(),
                DumpPoll::SeenAt(t) => format!(
                    "{:.1}ms-before-reap",
                    r.reaped.at.saturating_duration_since(t).as_secs_f64() * 1e3
                ),
            }
        ))
    }

    /// Instrument 5's optional half: once per round, for an **armed** child only
    /// (a plain soak does no per-round `/proc` read), records the first round
    /// showing `CoreDumping: 1` so [`reap_diag`] can bracket the dump.
    /// [`drive`]'s teardown calls it once before killing the role holder last.
    /// No output of its own.
    fn note_core_dumping(kid: &mut Kid) {
        if kid.crash_at.is_some() && kid.dumping_seen.is_none() && proc_core_dumping(kid.proc.id())
        {
            kid.dumping_seen = Some(Instant::now());
        }
    }

    /// **Instrument 4: the env var carrying a worker's spawn instant** (wall
    /// nanoseconds, set before `spawn()`), so `fork`, `exec` and start-up are
    /// inside the first join's measurement. Not a `TF_TREE_` name.
    const SPAWNED_AT_ENV: &str = "SHM_TORTURE_SPAWNED_AT_NS";

    /// Instrument 4's threshold: a healthy rejoin takes milliseconds; a stall
    /// that could empty the pool must outlast a driver round (~80 ms at 6 Hz).
    const SLOW_JOIN: Duration = Duration::from_millis(100);

    /// Instrument 4's per-process line cap.
    const SLOW_JOIN_LINES: u32 = 20;

    /// Instrument 4's **run-wide** line cap; the per-process cap alone bounds
    /// nothing (a 30-minute nightly spawns ~10 800 replacements). The driver
    /// prints how many lines either cap withheld ([`slow_join_budget_line`]).
    const SLOW_JOIN_RUN_LINES: u64 = 200;

    /// Instrument 4's run-wide budget, one byte per line claimed, shared with no
    /// lock and no `unsafe`: a child appends one byte through its own `O_APPEND`
    /// descriptor and reads the offset back, which is this line's unique
    /// run-wide number. A child killed between claim and print only under-fills
    /// the budget. Touched only on the already-slow path.
    fn slow_join_claims_path(dir: &Path) -> PathBuf {
        dir.join("diag_slow_join.claims")
    }

    /// Instrument 4's lines withheld by a process's own cap, one byte each.
    fn slow_join_withheld_path(dir: &Path) -> PathBuf {
        dir.join("diag_slow_join.withheld")
    }

    /// Claim this line's run-wide number, or `None` when the budget is spent or
    /// the claim could not be written.
    fn claim_slow_join_line(dir: &Path) -> Option<u64> {
        use std::io::Seek as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(slow_join_claims_path(dir))
            .ok()?;
        f.write_all(b".").ok()?;
        let n = f.stream_position().ok()?;
        (n <= SLOW_JOIN_RUN_LINES).then_some(n)
    }

    fn record_slow_join_withheld(dir: &Path) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(slow_join_withheld_path(dir))
        {
            let _ = f.write_all(b".");
        }
    }

    /// The driver's closing line for instrument 4, when either cap withheld
    /// anything; read before the scratch directory is removed.
    fn slow_join_budget_line(dir: &Path) -> Option<String> {
        let bytes = |p: PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let claimed = bytes(slow_join_claims_path(dir));
        let past_run_cap = claimed.saturating_sub(SLOW_JOIN_RUN_LINES);
        let past_process_cap = bytes(slow_join_withheld_path(dir));
        if past_run_cap + past_process_cap == 0 {
            return None;
        }
        Some(format!(
            "{DIAG} slow-join budget: {} line(s) withheld, {past_run_cap} past the run-wide cap of \
             {SLOW_JOIN_RUN_LINES} and {past_process_cap} past a process's own cap of \
             {SLOW_JOIN_LINES}; {} line number(s) were claimed within the run-wide cap (a child \
             killed between its claim and its write printed none)",
            past_run_cap + past_process_cap,
            claimed.min(SLOW_JOIN_RUN_LINES),
        ))
    }

    /// **Instrument 4: one trip from holding no `Tree` to holding one.**
    ///
    /// Hypothesis (2026-09-13): something common to every non-owner stalls
    /// between dropping a `Tree` and holding the next (the drop,
    /// [`record_attachment`] with the marker read, the `open()` handshake, or for
    /// a fresh process `exec` and start-up). A rejoin is a loop in the same
    /// process, so its episode starts just before the drop; a replacement is a
    /// new process and starts at the spawn stamp ([`SPAWNED_AT_ENV`]).
    struct JoinEpisode {
        /// `rejoin` or `first-join`.
        kind: &'static str,
        /// Just before the drop (`rejoin`), or entry to `child` (`first-join`).
        started: Instant,
        /// Spawn stamp to `child` entry, `first-join` only.
        exec: Option<Duration>,
        /// `started` to the first attempt (after a role holder's cap exit it also
        /// holds instrument 3's two writes).
        lead: Duration,
        /// [`record_attachment`] and the marker read, summed over attempts.
        pre_open: Duration,
        /// `open()` itself, summed over attempts.
        in_open: Duration,
        /// The existing back-off sleeps between refused attempts.
        backoff: Duration,
        attempts: u32,
        first_refusal: Option<tf_tree::OpenError>,
        /// Whether this episode's refusal was printed or withheld, so a refusal
        /// loop claims from the budget once.
        refusal_reported: bool,
    }

    impl JoinEpisode {
        fn new(kind: &'static str, started: Instant, exec: Option<Duration>) -> JoinEpisode {
            JoinEpisode {
                kind,
                started,
                exec,
                lead: Duration::ZERO,
                pre_open: Duration::ZERO,
                in_open: Duration::ZERO,
                backoff: Duration::ZERO,
                attempts: 0,
                first_refusal: None,
                refusal_reported: false,
            }
        }

        fn total(&self) -> Duration {
            self.exec.unwrap_or_default() + self.started.elapsed()
        }

        fn splits(&self) -> String {
            let ms = |d: Duration| d.as_secs_f64() * 1e3;
            format!(
                "{}{}={:.1} pre_open_ms={:.1} in_open_ms={:.1} backoff_ms={:.1}",
                self.exec
                    .map_or_else(String::new, |d| format!("exec_ms={:.1} ", ms(d))),
                if self.kind == "rejoin" {
                    "drop_ms"
                } else {
                    "setup_ms"
                },
                ms(self.lead),
                ms(self.pre_open),
                ms(self.in_open),
                ms(self.backoff),
            )
        }

        /// Both caps, this process's first, so a process that spent its own does
        /// not spend the run's. `None` when either withholds the line.
        fn claim(dir: &Path, lines: &mut u32) -> Option<(u32, u64)> {
            if *lines >= SLOW_JOIN_LINES {
                record_slow_join_withheld(dir);
                return None;
            }
            let run_line = claim_slow_join_line(dir)?;
            *lines += 1;
            Some((*lines, run_line))
        }

        /// A refused attempt, accounted once per episode and only past [`SLOW_JOIN`].
        fn refused(&mut self, e: tf_tree::OpenError, lines: &mut u32, dir: &Path) {
            self.first_refusal.get_or_insert(e);
            if self.refusal_reported || self.total() <= SLOW_JOIN {
                return;
            }
            self.refusal_reported = true;
            let Some((line, run_line)) = Self::claim(dir, lines) else {
                return;
            };
            child_diag(&format!(
                "join-refused pid={} kind={} since_ms={:.1} {} attempt={} error={e:?} stamp={} \
                 line={line}/{SLOW_JOIN_LINES} run_line={run_line}/{SLOW_JOIN_RUN_LINES}",
                std::process::id(),
                self.kind,
                self.total().as_secs_f64() * 1e3,
                self.splits(),
                self.attempts,
                wall_stamp(),
            ));
        }

        /// A successful attach, accounted when the whole trip passed [`SLOW_JOIN`].
        fn joined(&self, lines: &mut u32, dir: &Path) {
            if self.total() <= SLOW_JOIN {
                return;
            }
            let Some((line, run_line)) = Self::claim(dir, lines) else {
                return;
            };
            child_diag(&format!(
                "slow-join pid={} kind={} total_ms={:.1} {} attempts={} outcome=joined \
                 first_refusal={} stamp={} line={line}/{SLOW_JOIN_LINES} \
                 run_line={run_line}/{SLOW_JOIN_RUN_LINES}",
                std::process::id(),
                self.kind,
                self.total().as_secs_f64() * 1e3,
                self.splits(),
                self.attempts,
                self.first_refusal
                    .map_or_else(|| "none".to_string(), |e| format!("{e:?}")),
                wall_stamp(),
            ));
        }
    }

    /// The owner child: create the arena, serve the rendezvous, and park. It does
    /// nothing else, so the owner's death does not also stop the data stream
    /// (`owner_migration` argues the same split). It publishes its pid **before**
    /// reporting ready.
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
        // Hold the tree: dropping it stops serving the rendezvous.
        let _owner = tree;
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    /// Bring up the owner child and wait for it to report ready. The retry is
    /// coverage: `open.after_create_before_bind` and
    /// `open.after_ownership_lock_before_bind` fire only in a creating process,
    /// and this loop is the "next `open()`" §11.3's rows for them describe. A
    /// retry that never succeeds is a failed run.
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
                    dumping_seen: None,
                });
            }
            let status = proc.wait().ok();
            // [diag] Instrument 5's two clock reads, at the reap.
            let reaped = status.map(Reaped::now);
            #[cfg(unix)]
            if let Some(st) = status {
                use std::os::unix::process::ExitStatusExt as _;
                if st.signal() == Some(libc::SIGABRT) {
                    ledger.record_abort(crash_at.as_deref());
                }
            }
            // [diag] Instrument 5 for an owner child that aborted before `ready`
            // (it never held the role). At most six lines.
            if let Some(line) = reaped.and_then(|reaped| {
                let reap = Reap {
                    site: "owner-startup",
                    slot: None,
                    pid: proc.id(),
                    crash_at: crash_at.clone(),
                    dump: DumpPoll::NotPolled,
                    reaped,
                };
                reap_diag(&reap, RoleAtReap::NotAsked, None)
            }) {
                driver_diag(&line);
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

    /// The behaviour a child is started with, as one value (`crash_at` is
    /// per-child and stays a separate argument).
    #[derive(Clone, Copy)]
    struct ChildSpec {
        /// This child publishes the deliberate corruption (`--inject-violation`).
        inject: bool,
        /// Attach and read but never claim or publish (`--readers-only`).
        readers_only: bool,
        /// Skip §3.5's trigger entirely (`--no-inherit`).
        no_inherit: bool,
        /// Megabytes of dirty memory to hold, widening the reap window
        /// (`--victim-ballast-mb`). See [`kill_window_path`].
        ballast_mb: usize,
    }

    fn spawn(
        exe: &std::path::Path,
        dir: &PathBuf,
        seed: u64,
        spec: ChildSpec,
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
        if spec.inject {
            cmd.arg("--inject-violation");
        }
        if spec.readers_only {
            cmd.arg("--readers-only");
        }
        if spec.no_inherit {
            cmd.arg("--no-inherit");
        }
        if spec.ballast_mb > 0 {
            cmd.arg("--ballast-mb").arg(spec.ballast_mb.to_string());
        }
        // Per child, not per run: `crash::spec` parses the variable once per process.
        if let Some(site) = crash_at.as_deref() {
            cmd.env("TF_TREE_CRASH_AT", site);
        }
        // [diag] Instrument 4: stamped last, immediately before `spawn()`, so a
        // first join's `exec_ms` covers the spawn itself.
        cmd.env(SPAWNED_AT_ENV, now_nanos().to_string());
        let proc = cmd.spawn().context("spawning a torture child")?;
        Ok(Kid {
            proc,
            seed,
            inject: spec.inject,
            crash_at,
            dumping_seen: None,
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
        // [diag] Instrument 1 (`host_diag`, `core_dump_warning`): one line, plus a warning.
        let (host, core_warning) = host_diag(a.crash_points);
        driver_diag(&host);
        if let Some(w) = &core_warning {
            driver_diag(w);
        }
        if a.inject {
            println!(
                "  --inject-violation: one child publishes a corrupt transform on purpose. \
                 THIS RUN IS EXPECTED TO FAIL."
            );
        }

        let mut ledger = CrashLedger::default();

        // The owner is a child and comes up first ([`spawn_owner`], [`attach_observer`]).
        let mut owner_kid = Some(spawn_owner(
            &exe,
            &dir,
            &mut rng,
            a.crash_points,
            a.crash_site.as_deref(),
            &mut ledger,
        )?);

        // The one process that holds the role without inheriting it, so
        // [`inherited_pids`] cannot contain it; the driver knows its pid exactly.
        let creating_owner_pid = owner_kid.as_ref().map(|k| k.proc.id());
        let observer = attach_observer()?;

        // Fixed slots, so "slot 0 injects" survives slot 0 being replaced.
        let mut kids: Vec<Option<Kid>> = Vec::with_capacity(a.children);

        for i in 0..a.children {
            // Only one slot injects: a corrupt sample from every writer would let a
            // child detect its own corruption.
            let kid = spawn(
                &exe,
                &dir,
                rng.next_u64(),
                ChildSpec {
                    inject: a.inject && i == 0,
                    readers_only: a.readers_only,
                    no_inherit: a.no_inherit,
                    ballast_mb: a.victim_ballast_mb,
                },
                crash_spec(
                    a.crash_points,
                    a.crash_site.as_deref(),
                    &mut rng,
                    &mut ledger,
                ),
            )?;
            kids.push(Some(kid));
        }

        let deadline = Instant::now() + a.duration;
        let mut kills = 0usize;
        // Ordinary victim draws that landed on the role holder and were skipped.
        let mut role_holder_draws_skipped = 0usize;
        // Owner kills deferred for want of a second eligible heir.
        let mut owner_kills_deferred = 0usize;
        // When the current unbroken run of deferrals began, and its attempt count;
        // a clock rather than a count because the retry cadence is not the
        // schedule ([`OWNER_KILL_DEFERRAL_RETRY`]).
        let mut deferring_since: Option<Instant> = None;
        let mut deferrals_in_a_row = 0usize;
        // Every firing of the arm, deferred or not: the §3.5 floor separates
        // "never came due" from "came due and nothing came of it".
        let mut owner_kill_attempts = 0usize;
        // Ordinary draws skipped because the attached pool was at its floor.
        let mut thin_pool_draws_skipped = 0usize;
        // A wedge is not a `violations` entry: that bails as an engine
        // crash-consistency failure, while a wedge is a population condition, and
        // conflating them is the INVALID-is-not-FAIL collapse. Its own verdict.
        let mut wedge: Option<String> = None;
        let mut reads = Reads::default();
        let mut rounds = 0u64;
        let mut violations = Vec::new();
        let interval = Duration::from_secs_f64(1.0 / a.kill_hz);
        let started = Instant::now();
        // The whole run and the last hundred rounds; the second dates the moment
        // an arena stopped being written.
        let mut health = Health::default();
        let mut window = Health::default();
        // §3.5: `None` when `--no-kill-owner`, else the next owner kill, the first
        // [`OWNER_KILL_FIRST`] in.
        let mut next_owner_kill = a.owner_kill_every.map(|_| started + OWNER_KILL_FIRST);
        let mut migrations: Vec<Migration> = Vec::new();
        // The observer's read total on the round before an owner kill: "the data
        // plane never pauses" is relative to the data plane working.
        while Instant::now() < deadline {
            // Jitter, so the kills do not land in phase with a child's loop.
            let jitter = 0.5 + rng.unit();
            // `saturating_duration_since`: `Instant`'s `Sub` panics if the clock
            // crossed the deadline since the `while` test.
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
            reap_finished(&mut kids, &mut violations, &mut ledger, &dir);
            reap_owner(&mut owner_kid, &mut ledger, &dir);
            if !violations.is_empty() {
                break;
            }
            for (slot, kid) in kids.iter_mut().enumerate() {
                if kid.is_none() {
                    let seed = rng.next_u64();
                    let fresh = spawn(
                        &exe,
                        &dir,
                        seed,
                        ChildSpec {
                            inject: a.inject && slot == 0,
                            readers_only: a.readers_only,
                            no_inherit: a.no_inherit,
                            ballast_mb: a.victim_ballast_mb,
                        },
                        crash_spec(
                            a.crash_points,
                            a.crash_site.as_deref(),
                            &mut rng,
                            &mut ledger,
                        ),
                    )?;
                    *kid = Some(fresh);
                }
            }

            // §3.5's owner death, before the ordinary draw: the driver kills the role
            // holder and requires what a migration owes. The ordinary kill still
            // runs this round, so the owner dies while the fleet churns.
            if let (Some(every), Some(at)) = (a.owner_kill_every, next_owner_kill) {
                if Instant::now() >= at {
                    owner_kill_attempts += 1;
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
                        a.stop_owner_ms,
                        owner_kill_attempts <= a.defer_owner_kills,
                    );
                    reads.add(m.reads);
                    rounds += m.rounds;
                    println!("{}", m.line());
                    // A deferral re-arms in milliseconds, a kill on the schedule
                    // (see [`OWNER_KILL_DEFERRAL_RETRY`]); clamped to `every`.
                    next_owner_kill = Some(
                        Instant::now()
                            + if m.deferred {
                                OWNER_KILL_DEFERRAL_RETRY.min(every)
                            } else {
                                every
                            },
                    );
                    // A deferral is not a migration and stays out of the verdict's
                    // ledger; it has its own tally.
                    if m.deferred {
                        owner_kills_deferred += 1;
                        deferrals_in_a_row += 1;
                        let since = *deferring_since.get_or_insert_with(Instant::now);
                        // `saturating_mul`: `Duration`'s `Mul` panics on overflow.
                        let budget = every
                            .saturating_mul(OWNER_KILL_DEFERRAL_BUDGET_INTERVALS)
                            .max(MIN_OWNER_KILL_DEFERRAL_BUDGET);
                        // A deferral has to clear or it is the wedge under a politer
                        // name: it ends the run when nothing is attached
                        // (`heirs_before == 0`) or when it repeats past the budget.
                        // Otherwise ten good migrations then a stall would pass the
                        // `migrations.is_empty()` floor over a mostly-dead arena.
                        let starved = m.heirs_before == Some(0);
                        // [diag] Instrument 2 at every deferral (the starved stop
                        // included), about one line per
                        // [`OWNER_KILL_DEFERRAL_RETRY`] (~96 for a full budget):
                        // the census *trajectory* is what diagnoses a wedge.
                        driver_diag(&population_diag(
                            &format!(
                                "at deferral of owner kill {} (attempt {} in a row, \
                                 {:.1}s of {:.1}s budget, starved={starved})",
                                m.n,
                                deferrals_in_a_row,
                                since.elapsed().as_secs_f64(),
                                budget.as_secs_f64()
                            ),
                            "pre-kill census",
                            &dir,
                            &observer,
                            owner_kid.as_ref(),
                            &kids,
                            &m.before_seen,
                        ));
                        if starved || since.elapsed() >= budget {
                            wedge = Some(format!(
                                "owner kill {} could not be attempted and the deferral did not \
                                 clear: {}. §3.5's trigger outcomes at this instant: {}. \
                                 Last migration that recovered: {}. Deferrals so far: {}. \
                                 Elapsed {:.1}s, round {}. A deferral exists so the arm never \
                                 kills the last eligible heir; one that never clears is the \
                                 absorbing state itself, and this run stops here rather than \
                                 spending its remaining duration proving it again.",
                                m.n,
                                if starved {
                                    "zero read-write participants were attached, so there was no \
                                     heir to wait for — an ownerless arena admits no new one, so \
                                     the population cannot recover on its own"
                                } else {
                                    "the fleet held only the role holder for every attempt in a \
                                     row, so replacements are not completing their handshake \
                                     between retries"
                                },
                                trigger_tally_line(&dir),
                                migrations
                                    .iter()
                                    .rev()
                                    .find(|p| p.recovered.is_some())
                                    .map_or_else(|| "none".to_string(), |p| p.n.to_string()),
                                owner_kills_deferred,
                                started.elapsed().as_secs_f64(),
                                rounds,
                            ));
                        }
                    } else {
                        deferring_since = None;
                        deferrals_in_a_row = 0;
                        // Stop at the first unrecoverable wedge: `recovered.is_none()`
                        // with `heirs_at_kill == 0` is absorbing (nothing can join an
                        // ownerless arena), and continuing yields one failure per
                        // later kill with the cause thousands of lines above the
                        // verdict. Classified from the census and trigger tally, not
                        // `slots=Nreg/Malive` (a run-wide minimum that reads 0 for
                        // both producers). POPULATION is entailed by the branch
                        // condition: the census was taken after the victim was
                        // reaped, so there provably was no heir to ask.
                        if m.recovered.is_none() && m.heirs_at_kill == Some(0) {
                            // [diag] Instrument 2 at the unrecoverable stop (post-reap
                            // census, `/proc` after the deadline); one line.
                            driver_diag(&population_diag(
                                &format!("at unrecovered owner kill {}", m.n),
                                "post-reap census",
                                &dir,
                                &observer,
                                owner_kid.as_ref(),
                                &kids,
                                &m.at_kill_seen,
                            ));
                            let tally = trigger_tally_line(&dir);
                            wedge = Some(format!(
                                "owner kill {} left the arena in an UNRECOVERABLE state, and the \
                                 run stops here rather than reporting the same failure for every \
                                 later kill. Classification: {}. Last migration that recovered: \
                                 {}. Heirs attached at this kill: 0 (first round after: {}). \
                                 §3.5 trigger outcomes at this instant: {}. Recorded owner pid: \
                                 {}. Elapsed {:.1}s, round {}. \
                                 Held participant bytes at the refusal are in the \
                                 `ArenaHeldButUnreachable` text above, including this driver's \
                                 own slot — it holds one for the life of the run and never \
                                 inherits, by design.",
                                m.n,
                                {
                                    "POPULATION — no eligible heir remained to ask, so §3.5's \
                                     trigger was never answered. This is the state the \
                                     pre-kill census exists to prevent; reaching it means the \
                                     census passed and the pool drained inside the vacancy"
                                },
                                migrations
                                    .iter()
                                    .rev()
                                    .find(|p| p.recovered.is_some())
                                    .map_or_else(|| "none".to_string(), |p| p.n.to_string()),
                                m.heirs_first_round
                                    .map_or_else(|| "?".to_string(), |c| c.to_string()),
                                tally,
                                read_owner_pid(&dir)
                                    .map_or_else(|| "none".to_string(), |p| p.to_string()),
                                started.elapsed().as_secs_f64(),
                                rounds,
                            ));
                        }
                        migrations.push(m);
                    }
                    // A wedge ends the run like a violation, by a different verdict.
                    if !violations.is_empty() || wedge.is_some() {
                        break;
                    }
                }
            }

            // The ordinary draw does not take the role holder: an owner killed here
            // is silent (no census, no `Migration`, nothing requires recovery) and
            // consumes an eligible heir at the worst instant (the population
            // reached zero on the nightlies from 2026-09-07). Owners still die on
            // the arm that requires what §3.5 owes. Skipped, not redrawn, and
            // **counted**. Nor does it draw the fleet down to the role holder
            // ([`MIN_ATTACHED_FOR_ORDINARY_KILL`]). The pool is censused per draw,
            // because a forked replacement is not a participant until its
            // handshake finishes.
            let mut pool = RoundHealth::default();
            census(&observer, &mut pool);
            let role_holder = read_owner_pid(&dir);
            let victim = rng.below(a.children as u64) as usize;
            if pool.slots_alive < MIN_ATTACHED_FOR_ORDINARY_KILL {
                thin_pool_draws_skipped += 1;
            } else if let Some(kid) = kids[victim].as_mut() {
                if Some(kid.proc.id()) == role_holder {
                    role_holder_draws_skipped += 1;
                } else {
                    let _ = kid.proc.kill();
                    let reaped = kid.proc.wait();
                    // [diag] Instrument 5 at a driver kill: a draw victim that had
                    // aborted or was still dumping is reaped here as an ordinary
                    // kill (see `reap_diag`). The draw skips the marker's pid, so
                    // the ledgers are not read. One line per such draw.
                    if let Some(line) = reaped.ok().and_then(|st| {
                        let reap = Reap::of("ordinary-draw", Some(victim), kid, Reaped::now(st));
                        reap_diag(&reap, RoleAtReap::NotAsked, None)
                    }) {
                        driver_diag(&line);
                    }
                    kids[victim] = None;
                    kills += 1;
                }
            }
        }

        // Let survivors exit on their own so a last-instant violation is not lost.
        std::thread::sleep(Duration::from_millis(200));
        let mut round = RoundHealth::default();
        reads.add(observe(&observer, &mut rng, &mut violations, &mut round));
        health.add(round);
        rounds += 1;
        reap_finished(&mut kids, &mut violations, &mut ledger, &dir);
        reap_owner(&mut owner_kid, &mut ledger, &dir);
        // Kill every remaining child *before* the recovery check: "no claim is held
        // by a dead participant" holds only for a quiescent arena. Signal them
        // all, then wait: killing one at a time lets survivors reap on ~3% of
        // operations and leaves `check_recovery` nothing to recover. Whoever holds
        // the role now (after a migration, a worker slot) is held back from the
        // batch, or the hangup collector dies alongside the records it was about
        // to collect.
        let current_owner = read_owner_pid(&dir).or(creating_owner_pid);
        // Whether this run can pin the owner's hangup callback, decided from the
        // ledger rather than the flags. The strict half of [`check_recovery`]
        // needs the process serving the rendezvous at worker-kill time to still
        // serve during their hangups: true when the role never moved (the parked
        // creating owner cannot detach), usually false after a migration (the heir
        // is a worker that detaches or hits its cap, leaving the arena ownerless).
        // `inherited_pids` empty plus the holder being our unkilled owner child
        // states that exactly; `--no-kill-owner` would be a flag standing in for
        // a fact, since an armed §11.3 site can end the creating owner anyway.
        let hangup_collector_pinned = inherited_pids(&dir).is_empty()
            && owner_kid
                .as_ref()
                .is_some_and(|k| Some(k.proc.id()) == current_owner);
        for kid in kids.iter_mut().flatten() {
            if Some(kid.proc.id()) == current_owner {
                continue;
            }
            let _ = kid.proc.kill();
        }
        // [diag] Instrument 5 at teardown: each wait keeps its status and two clock
        // reads and nothing else; lines are written after `check_recovery`. One
        // line per batch child that aborted, was seen dumping, or held the role
        // (the batch holds it back, so a normal teardown prints none).
        let mut batch_reaps: Vec<(usize, Reaped)> = Vec::with_capacity(kids.len());
        for (i, kid) in kids.iter_mut().enumerate() {
            let Some(kid) = kid else { continue };
            if Some(kid.proc.id()) == current_owner {
                continue;
            }
            if let Ok(st) = kid.proc.wait() {
                batch_reaps.push((i, Reaped::now(st)));
            }
        }
        // A run that migrated cannot pin the collector: the heir is an ordinary
        // worker that sweeps the table several times a second (`reap_participants`
        // is one value in a hundred; `reap_dead` two more) and whose `work` loop
        // ends, leaving no owner. So `hangup_collector_pinned` gates the strict
        // verdict and this arm gets the sweep-and-require half. Deleting the
        // owner's hangup CAS in `crates/tf_tree/src/open.rs` is the mutant that
        // shows it; `--no-kill-owner` is where the collector is pinned, and
        // `tests/torture.rs` runs it for that reason.

        // The records no hangup callback can collect, named by pid: every process
        // that has held the rendezvous (the creating owner plus
        // [`inherited_pids`]; a process does not run its own hangup callback), and
        // every worker that attached under an owner since dead
        // (`docs/decisions/0043`). Everything else takes the strict path on a run
        // whose owner never changed; on one that migrated `check_recovery`
        // reports and sweeps it. A child with no recorded attachment is judged
        // strictly: [`record_attachment`] writes before `open()`, so absence means
        // the write failed, and exempting it would make a broken ledger permissive.
        let mut unreachable_by_hangup: Vec<u32> = inherited_pids(&dir);
        unreachable_by_hangup.extend(creating_owner_pid);
        unreachable_by_hangup.extend(current_owner);
        unreachable_by_hangup.extend(
            attachments(&dir)
                .into_iter()
                .filter(|(_, under)| Some(*under) != current_owner)
                .map(|(pid, _)| pid),
        );
        unreachable_by_hangup.sort_unstable();
        unreachable_by_hangup.dedup();

        // The owner outlives the workers, as a check rather than tidiness: the role
        // holder runs the hangup callback, one of the two collectors that reclaim a
        // dead participant unasked (`docs/decisions/0028` plan step 4), and the one
        // `check_recovery`'s leak check is written against. Killing it before
        // waiting the workers made a `--no-kill-owner` run fail naming slots. On a
        // migrated run the heir is a worker and both passes above hold it back by
        // pid; the sweep answers only for the records `0043` names
        // (`unreachable_by_hangup`).
        //
        // The wait belongs here, while the collector is alive: `check_recovery`'s
        // re-probe runs after every process that could run a hangup callback is
        // reaped. A bounded poll of the set the strict verdict is about replaces a
        // flat sleep's scheduling guess; a genuine leak never clears and costs the
        // deadline. It runs only where there is something to wait for (a migrated
        // arena usually has no owner and does not reach the strict verdict).
        std::thread::sleep(Duration::from_millis(200));
        if hangup_collector_pinned {
            let deadline = Instant::now() + Duration::from_secs(4);
            while Instant::now() < deadline {
                let outstanding = dead_participant_slots(&observer).into_iter().any(|slot| {
                    observer
                        .arena_view()
                        .participants()
                        .identity(slot)
                        .is_some_and(|(pid, _, _)| !unreachable_by_hangup.contains(&pid))
                });
                if !outstanding {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // The owner last, wherever it lives.
        //
        // [diag] Instrument 5 for the role holder, which dies here: the batch's
        // hangups run its callback, so an armed holder can abort during teardown
        // and its dump can outlast the 200 ms, reaping as `SIGKILL` with no core.
        // `note_core_dumping` is called once more before the kill (one `/proc`
        // read, armed holder only). The line prints only for an abort or a dump.
        let mut owner_last: Option<(Option<usize>, Reaped)> = None;
        if let Some(pid) = current_owner {
            if owner_kid.as_ref().is_some_and(|k| k.proc.id() == pid) {
                if let Some(kid) = owner_kid.as_mut() {
                    note_core_dumping(kid);
                    let _ = kid.proc.kill();
                    if let Ok(st) = kid.proc.wait() {
                        owner_last = Some((None, Reaped::now(st)));
                    }
                }
            } else {
                for (i, slot) in kids.iter_mut().enumerate() {
                    if slot.as_ref().is_some_and(|k| k.proc.id() == pid) {
                        if let Some(kid) = slot.as_mut() {
                            note_core_dumping(kid);
                            let _ = kid.proc.kill();
                            if let Ok(st) = kid.proc.wait() {
                                owner_last = Some((Some(i), Reaped::now(st)));
                            }
                        }
                        break;
                    }
                }
            }
        }
        // [diag] What the teardown lines need from each `Kid`; `true` marks the
        // role holder killed last.
        let mut teardown_reaps: Vec<(Reap, bool)> = batch_reaps
            .into_iter()
            .filter_map(|(i, reaped)| {
                let kid = kids[i].as_ref()?;
                Some((Reap::of("teardown-batch", Some(i), kid, reaped), false))
            })
            .collect();
        if let Some((slot, reaped)) = owner_last {
            let kid = match slot {
                None => owner_kid.as_ref(),
                Some(i) => kids[i].as_ref(),
            };
            if let Some(kid) = kid {
                teardown_reaps.push((Reap::of("teardown-owner-last", slot, kid, reaped), true));
            }
        }
        // `Kid::drop` collects anything left, an ex-owner child most of all.
        drop(kids);
        drop(owner_kid);
        std::thread::sleep(Duration::from_millis(100));

        let recovery = check_recovery(&observer, &unreachable_by_hangup, hangup_collector_pinned);
        drop(observer);
        // [diag] Instrument 5's teardown lines and instrument 4's closing line,
        // written once every child is reaped and the ledgers are still on disk.
        // In the batch, holding the role earns a tag; for the process killed last
        // it is the norm and the ledgers only annotate an abort or a dump.
        {
            let ledgers = RoleLedgers::read(&dir);
            for (reap, last) in &teardown_reaps {
                let (role, tag) = if *last {
                    (ledgers.of(reap.pid, reap.slot.is_none()), None)
                } else {
                    (
                        ledgers.of(reap.pid, false),
                        Some("role-holder-in-teardown-batch"),
                    )
                };
                if let Some(line) = reap_diag(reap, role, tag) {
                    driver_diag(&line);
                }
            }
            if let Some(line) = slow_join_budget_line(&dir) {
                driver_diag(&line);
            }
        }
        // Read and formatted before the scratch directory is removed:
        // `Scratch::drop` deletes every ledger, so reading at the print site
        // would return an empty tally on every run.
        let trigger_outcomes = trigger_tally_line(&dir);
        // Same for this counter: read at the print site it would be 0 on every run.
        let detach_skips = detach_skip_count(&dir);
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
        // `--crash-points` prints both numbers (see [`CrashLedger`]).
        if a.crash_points {
            let mut distinct_armed: Vec<&str> =
                ledger.armed_sites.iter().map(String::as_str).collect();
            distinct_armed.sort_unstable();
            distinct_armed.dedup();
            let mut distinct_fired: Vec<&str> = ledger.fired.iter().map(String::as_str).collect();
            distinct_fired.sort_unstable();
            distinct_fired.dedup();
            // Derived from this run, never a literal list, which would go stale
            // when the workload gained an operation.
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
        // §3.5 prints per migration, not as a total: a bare "3 owner kills" says
        // nothing about recovery, and a silent zero looks like a disabled arm.
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
            // The tally prints whatever the verdict: an empty one is a population
            // with nobody to ask, `contended` or `err-` is the engine refusing.
            println!("shm_torture: §3.5 trigger outcomes: {trigger_outcomes}");
            // Both of these reduce what the run did, so they print even as
            // zeroes: a kill rate that cannot be audited is a gate doing less
            // than it claims.
            println!(
                "shm_torture: §3.5 owner kills deferred for want of a second eligible heir: {}; \
                 ordinary victim draws skipped — {} that landed on the role holder, {} against an \
                 attached pool already at its floor of {}",
                owner_kills_deferred,
                role_holder_draws_skipped,
                thin_pool_draws_skipped,
                MIN_ATTACHED_FOR_ORDINARY_KILL
            );
            // The third reduction (see [`kill_window_path`]), printed for the same reason.
            println!(
                "shm_torture: detaches suppressed inside an owner-kill window: {detach_skips}"
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
        // Printed here, bailed on at the end: the bail order is load-bearing for
        // the self-test, and the diagnosis stays visible either way.
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
        // After the violations, before everything else: a corrupt read outranks
        // this, but a wedge *causes* every later check (no join, frozen rings,
        // floors tripping), so reporting one would bury the cause. Not phrased as a
        // §12.3 gate-3 failure: the arena is not accused, the fleet stopped existing.
        if let Some(why) = wedge {
            bail!(
                "the run stopped early because the arena became unrecoverable, which is a \
                 statement about this fleet's population and NOT about the engine's \
                 crash-consistency (docs/PHASE2.md §12.3 gate 3 is not what failed here):\n  \
                 {why}"
            );
        }
        // §3.5 before the read floor: an unrecovered migration *causes* the floor to
        // trip. The `--readers-only` self-test never reaches [`OWNER_KILL_FIRST`].
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
        // A run that killed nobody is not §11.4's workload. The ordinary draw can
        // decline (role holder, or a pool at [`MIN_ATTACHED_FOR_ORDINARY_KILL`]),
        // and the floor that makes that auditable ships with it. "At least one"
        // rather than a fraction of `duration x --kill-hz`, which would be a tuned
        // number that expires; the printed skip tallies say how much it bit.

        if kills == 0 && a.duration >= interval {
            bail!(
                "no participant was killed in {:?} at --kill-hz {}, so this run is not \
                 docs/PHASE2.md §11.4's workload — it validated an arena nobody was tearing \
                 down. Ordinary draws skipped: {} that landed on the rendezvous role holder, \
                 {} against an attached pool at its floor of {}. If the second number is \
                 large the fleet never sustained {} attached participants: raise --children, \
                 or lower --kill-hz so a replacement finishes its handshake before the next \
                 draw.",
                a.duration,
                a.kill_hz,
                role_holder_draws_skipped,
                thin_pool_draws_skipped,
                MIN_ATTACHED_FOR_ORDINARY_KILL,
                MIN_ATTACHED_FOR_ORDINARY_KILL
            );
        }
        // A run that never killed the owner must not be quoted as §3.5 coverage.
        // The condition uses `owner_kill_attempts`, not arithmetic on the duration:
        // a duration test is the schedule's second attempt, so a short run could
        // defer its only attempt and print PASS over an arm that never fired. The
        // duration arm stays for an arm that was due and never fired at all.
        if let Some(every) = a.owner_kill_every {
            let attempted = owner_kill_attempts > 0;
            if migrations.is_empty() && (attempted || a.duration >= OWNER_KILL_FIRST + every) {
                bail!(
                    "the owner-kill arm is on and produced {} migration(s) from {} attempt(s) \
                     in {:?} (first due at {:?}, then every {:?}). This run covers none of \
                     docs/PHASE2.md §3.5 and must not be quoted as if it did. \
                     {} owner kill(s) were DEFERRED for want of a second eligible heir — \
                     deferrals are deliberately not counted as migrations, precisely so that a \
                     run which deferred every one of them lands here instead of printing PASS \
                     over an arm that never fired. A nonzero deferral count with zero migrations \
                     means the fleet never held two read-write participants at any attempt: \
                     raise `--children`, or lower `--kill-hz` so a replacement finishes its \
                     handshake before the next draw.",
                    migrations.len(),
                    owner_kill_attempts,
                    a.duration,
                    OWNER_KILL_FIRST,
                    every,
                    owner_kills_deferred
                );
            }
        }
        // A run that validated nothing must not print PASS: "the observer never
        // managed a lookup" is a thing that did not happen, and printed the same
        // verdict as one that did. The floor is per round, so it scales with
        // `--duration` and `--kill-hz`.
        let want_chain = rounds * MIN_CHAIN_READS_PER_ROUND;
        let want_edge = rounds * MIN_EDGE_READS_PER_ROUND;
        let vacuous = reads.chain < want_chain || reads.edge < want_edge;
        // The self-test's other half, before the floor as the more specific
        // diagnosis: `--inject-violation` publishes a NaN all run and must fail.
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
        // A run nobody was writing to proves nothing and does not trip the floor
        // above: four frozen rings that overlap answer every composed read
        // forever (see [`RoundHealth`]). Half the rounds, not all: `--kill-hz`
        // guarantees stretches with no writer on a given edge.
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
        // `--crash-points` that exercised nothing is a failure: a workflow reads
        // only the exit status, so `armed 0, aborted 0` must not exit 0
        // (`a_run_that_validates_nothing_fails_instead_of_passing` is the shape one
        // level down). The two counts fail separately: `armed 0` means arming never
        // happened (feature compiled out, empty site list), `armed N, aborted 0`
        // means no armed child reached its site (a tuning statement). Both bounds
        // are "at least one", not tuned thresholds.
        if a.crash_points {
            if ledger.armed == 0 {
                bail!(
                    "--crash-points armed 0 children over {rounds} rounds, so this run \
                     exercised no §11.3 site at all and `0 violation(s)` says nothing \
                     about crash consistency. The children are this same executable, \
                     so check that the driver was built `--features shm,crash-points` \
                     and that `tf_tree_core::crash::SITES` and `tf_tree::CRASH_SITES` \
                     are non-empty in it."
                );
            }
            if ledger.aborted == 0 {
                bail!(
                    "--crash-points armed {} child(ren) and none of them aborted \
                     at its site, so no process was killed mid-protocol and this run \
                     is a plain SIGKILL soak wearing §11.3's name. The driver's kills \
                     reach an armed child before it gets there — raise --duration or \
                     lower --kill-hz. This is the run the `§11.3:` line's second \
                     number exists to distinguish; the exit status now carries it, \
                     because a workflow reads only that.",
                    ledger.armed
                );
            }
        }
        println!("shm_torture: PASS");
        Ok(())
    }

    /// One owner death and everything the run learned from it. Three independent
    /// facts:
    ///
    /// * `recovered`: a **fresh process** joined again, and how long after the
    ///   `SIGKILL`; measured from outside because an ownerless arena is what
    ///   refuses a new joiner.
    /// * `inherits`: survivors that recorded an inheritance, which says the join
    ///   succeeded because §3.5's trigger ran.
    /// * `reads`: what the observer validated **while the role was vacant**
    ///   (§3.5: lookups do not stop or observe anything during a takeover);
    ///   `read_before` says whether it was reading at all beforehand.
    struct Migration {
        n: usize,
        victim: Option<u32>,
        recovered: Option<Duration>,
        inherits: usize,
        reads: Reads,
        rounds: u64,
        read_before: bool,
        /// The kill was **not performed**: the census showed no read-write survivor
        /// besides the role holder. §11.3's `takeover.*` row presumes "another
        /// participant takes over", and killing the last eligible heir yields an
        /// absorbing arena that tests nothing §3.5 claims. Deferred to the next
        /// retry and counted. Not a weakened gate: it is the difference from
        /// `a_killed_heir_leaves_the_role_for_the_next_survivor`
        /// (`crates/tf_tree/tests/rendezvous.rs`), which keeps a second heir
        /// attached. A deferral is transient or it is a wedge, and the caller
        /// separates them (see [`Migration::heirs_before`] and the call-site
        /// bound); deferring indefinitely turns a loud failure into a silent stall.
        deferred: bool,
        /// Read-write participants attached **before** the kill, the role holder
        /// included, or `None` when the marker named nobody. `Some(0)` is the
        /// absorbing state itself; `Some(1)` is only the role holder, where
        /// deferring is right if the arena is otherwise healthy, hence the bound.
        heirs_before: Option<u64>,
        /// Live participants other than the driver's observer, censused the
        /// instant the victim was reaped: the heirs that existed when the role
        /// fell vacant. §3.5 is caller-driven (`docs/decisions/0019`), so "nothing
        /// inherited" is either an engine refusing every heir or a population with
        /// none to ask, and this number separates them.
        heirs_at_kill: Option<u64>,
        /// The same census on the first round after the kill; the pool drains
        /// during a vacancy, so only the pair says which population a late
        /// recovery had.
        heirs_first_round: Option<u64>,
        /// Wall time from `kill()` to `wait()` returning: **the blind window** in
        /// which the pool can drain unseen (a survivor's detach arm runs while the
        /// marker still names the corpse). It is dominated by the victim's
        /// teardown, so it scales with RSS: a plain release child reaps in
        /// ~0.25 ms, an ASan child (43-49 MB) in ~5 ms, and the victim is the
        /// oldest child, so the window grows with `owner_kill_every`.
        kill_to_reaped: Option<Duration>,
        /// Diagnostic only: the slots behind `heirs_before`, from the same
        /// census pass. See [`population_diag`].
        before_seen: Vec<SlotSeen>,
        /// Diagnostic only: the slots behind `heirs_at_kill`.
        at_kill_seen: Vec<SlotSeen>,
    }

    impl Migration {
        /// The per-migration line, printed as it happens so a silent zero is impossible.
        fn line(&self) -> String {
            let Migration {
                n,
                victim,
                recovered,
                inherits,
                heirs_before,
                heirs_at_kill,
                heirs_first_round,
                kill_to_reaped,
                ..
            } = self;
            let unknown = || "?".to_string();
            if self.deferred {
                // The census is the whole content: it separates a thin fleet from the
                // absorbing state.
                return format!(
                    "shm_torture: §3.5 owner kill {n} DEFERRED: {} read-write participant(s) \
                     attached including the role holder, below the floor this arm needs, and \
                     §11.3's row for this migration presumes a survivor (\"another participant \
                     takes over\"). Killing the last eligible heir would produce an arena that \
                     is ownerless, uninheritable and unjoinable — absorbing, not slow — which \
                     tests nothing §3.5 claims. Retrying at the next interval.",
                    heirs_before.map_or_else(unknown, |c| c.to_string()),
                );
            }
            // `heirs_before` and the reap window print because
            // `heirs_at_kill = heirs_before - 1 - departures` is otherwise not
            // recoverable from a nightly's log.
            format!(
                "shm_torture: §3.5 owner kill {n}: killed pid {}{}; {}; {inherits} survivor(s) \
                 inherited; {} heir(s) attached before the kill, {} after it, {} on the first \
                 round after; the observer validated {} transform(s) while the role was vacant",
                victim.map_or_else(unknown, |p| p.to_string()),
                kill_to_reaped.map_or_else(String::new, |d| format!(
                    " (reaped in {:.1} ms)",
                    d.as_secs_f64() * 1e3
                )),
                recovered.map_or_else(
                    || format!("NO fresh process joined within {OWNER_RECOVERY_DEADLINE:?}"),
                    |d| format!(
                        "a fresh process joined {:.1} ms later",
                        d.as_secs_f64() * 1e3
                    )
                ),
                heirs_before.map_or_else(unknown, |c| c.to_string()),
                heirs_at_kill.map_or_else(unknown, |c| c.to_string()),
                heirs_first_round.map_or_else(unknown, |c| c.to_string()),
                self.reads.total(),
            )
        }

        /// `Some(why)` if this migration failed the run.
        fn failure(&self) -> Option<String> {
            let n = self.n;
            // Before `victim`, which a deferral also leaves `None`.
            if self.deferred {
                return None;
            }
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
    /// The victim is looked up, not drawn: after a migration the holder is
    /// whichever child won byte 0, so an heir publishes its pid on inheriting
    /// ([`publish_owner_pid`]); a draw would mostly kill a plain participant. A
    /// pid matching neither the owner child nor a worker slot is never signalled
    /// (`victim: None`, which [`Migration::failure`] treats as a harness defect).
    /// A killed worker slot is refilled by [`drive`]'s respawn loop; a killed
    /// owner child is not replaced, since the role is inherited from here on.
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
        stop_owner_ms: u64,
        // `--defer-owner-kills`: defer this attempt, applied *after* the census
        // so `heirs_before` and `starved` stay truthful.
        force_defer: bool,
    ) -> Migration {
        let mut m = Migration {
            n,
            victim: None,
            recovered: None,
            inherits: 0,
            reads: Reads::default(),
            rounds: 0,
            read_before,
            heirs_at_kill: None,
            heirs_first_round: None,
            deferred: false,
            heirs_before: None,
            kill_to_reaped: None,
            // Reserved before any window opens, so the census inside it does not allocate.
            before_seen: Vec::with_capacity(observer.arena_view().participants().capacity()),
            at_kill_seen: Vec::with_capacity(observer.arena_view().participants().capacity()),
        };
        let before = inheritance_count(dir);
        let Some(pid) = read_owner_pid(dir) else {
            return m;
        };

        // §3.5's precondition is established, not raced for: §11.3's `takeover.*`
        // row presumes another participant (see [`Migration::deferred`]), and
        // this is the only instant at which whether one exists is decidable.
        // Censused before the kill, so the count includes the role holder about
        // to die; `census` skips the driver's observer, so `>= 2` is the role
        // holder plus another read-write survivor. Erring toward deferral is
        // safe. Without this the harness reddened three nightlies from
        // 2026-09-07: zero attached heirs with the role vacant is absorbing (only
        // an already-joined participant can inherit, `crates/tf_tree/src/open.rs`;
        // the segment is an unnamed `memfd` handed over only by a serving owner,
        // `docs/PHASE2.md` §3.6; §3.4 step 4 refuses to create while any
        // participant byte is held, and this driver holds one). The trigger
        // tally showed zero `err-*`: the engine refused no heir, there was none.
        let mut before_kill = RoundHealth::default();
        census_with(observer, &mut before_kill, Some(&mut m.before_seen));
        m.heirs_before = Some(before_kill.slots_alive);
        if force_defer || before_kill.slots_alive < 2 {
            m.deferred = true;
            return m;
        }

        let mut killed = false;
        // Opened before `kill()`, or the interval the marker closes would remain.
        open_kill_window(dir, pid);
        // Brackets both arms, including the deliberate hold below, which is part
        // of the blind window the printed figure is compared on.
        let kill_started = Instant::now();

        // `SIGSTOP` before `SIGKILL`, when asked (see [`Args::stop_owner_ms`]).
        // Sent through `/bin/kill`, not `libc::kill`, so this file gains no
        // `unsafe` (`scripts/unsafe-budget.txt`); a failure to signal is reported,
        // since a control that silently does not fire is the defect it guards.
        if stop_owner_ms > 0 {
            match Command::new("kill")
                .args(["-STOP", &pid.to_string()])
                .status()
            {
                Ok(st) if st.success() => {
                    std::thread::sleep(Duration::from_millis(stop_owner_ms));
                }
                other => {
                    println!(
                        "shm_torture: --stop-owner-ms could not stop pid {pid} ({other:?}); this \
                         run's positive control did NOT fire and its result says nothing about \
                         the kill window"
                    );
                }
            }
        }
        // [diag] Instrument 5 at a driver kill: an owner that aborted and is still
        // dumping holds its byte and counts in the census, so this kill would be
        // recorded as ordinary (see `reap_diag`). Only the status and two clock
        // reads are taken here, inside `kill_to_reaped`; the `/proc` read,
        // formatting and write wait until the window and the recovery probe are
        // done. `Reap::taken_from` moves `crash_at` so nothing allocates. At most
        // one line per owner kill.
        let mut owner_reap: Option<Reap> = None;
        if owner_kid.as_ref().is_some_and(|k| k.proc.id() == pid) {
            if let Some(kid) = owner_kid.as_mut() {
                let _ = kid.proc.kill();
                if let Ok(st) = kid.proc.wait() {
                    owner_reap = Some(Reap::taken_from("owner-kill", None, kid, Reaped::now(st)));
                }
            }
            *owner_kid = None;
            killed = true;
        } else {
            for (i, slot) in kids.iter_mut().enumerate() {
                if slot.as_ref().is_some_and(|k| k.proc.id() == pid) {
                    if let Some(kid) = slot.as_mut() {
                        let _ = kid.proc.kill();
                        if let Ok(st) = kid.proc.wait() {
                            owner_reap = Some(Reap::taken_from(
                                "owner-kill",
                                Some(i),
                                kid,
                                Reaped::now(st),
                            ));
                        }
                    }
                    *slot = None;
                    killed = true;
                    break;
                }
            }
        }
        if !killed {
            // A pid we did not spawn, or already exited (a stale marker): not
            // signalled. The window closes here too, or the stale marker would
            // pin the churn off for the rest of the run.
            close_kill_window(dir);
            return m;
        }
        m.victim = Some(pid);
        m.kill_to_reaped = Some(kill_started.elapsed());

        // Censused here and nowhere else: `drive`'s round census is stale across
        // the interval in which the pool changes. Taken after `kill`+`wait`, so
        // the dead owner's byte is released and not counted; `census` skips the
        // observer, so what remains is the read-write survivors that could answer
        // `owner_lost`. `Health::add` keeps only a run-wide `slots_alive_min`,
        // which reads 0 for any run that stayed ownerless, so it cannot classify
        // this; this field is not an aggregate.
        let mut at_kill = RoundHealth::default();
        census_with(observer, &mut at_kill, Some(&mut m.at_kill_seen));
        m.heirs_at_kill = Some(at_kill.slots_alive);
        // Survivors churn again from here; see [`close_kill_window`].
        close_kill_window(dir);

        // Read and probe in one loop: the observer keeps validating while the role
        // is vacant (§3.5's data-plane claim), and the fresh join says the role
        // stopped being vacant.
        let start = Instant::now();
        let mut next_observe = Instant::now();
        while start.elapsed() < OWNER_RECOVERY_DEADLINE {
            if Instant::now() >= next_observe {
                let mut round = RoundHealth::default();
                m.reads.add(observe(observer, rng, violations, &mut round));
                if m.heirs_first_round.is_none() {
                    m.heirs_first_round = Some(round.slots_alive);
                }
                health.add(round);
                m.rounds += 1;
                next_observe = Instant::now() + MIGRATION_OBSERVE_EVERY;
                if !violations.is_empty() {
                    break;
                }
            }
            // A real `open()` as a process that was not here when the owner died;
            // `ReadOnly` because a consumer is what a robot restarts, and a
            // read-write join would add a record to account for.
            let joined = tf_tree::Open::new()
                .mode(AttachMode::ReadOnly)
                .create(CreatePolicy::Never)
                // Short: a long timeout would measure the timeout, not the recovery.
                .timeout(Duration::from_millis(20))
                .open()
                .is_ok();
            if joined {
                m.recovered = Some(start.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        // The evidence trails the recovery: `Tree::inherit_ownership` renames the
        // socket in *before* the heir reaches its `publish_owner_pid` and
        // `record_inheritance` writes, so reading immediately can report "joined
        // but nothing inherited" (seen after a 0.6 ms recovery). The wait is
        // bounded and short: a migration where nothing inherited never produces
        // the line, and `--no-inherit` proves it.
        let evidence_deadline = Instant::now() + Duration::from_millis(500);
        loop {
            m.inherits = inheritance_count(dir).saturating_sub(before);
            if m.inherits > 0 || Instant::now() >= evidence_deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // [diag] Instrument 5's line for this kill, written now that nothing is
        // timed. The kill was deliberate, so the ledgers are not read.
        if let Some(line) = owner_reap
            .as_ref()
            .and_then(|r| reap_diag(r, RoleAtReap::NotAsked, None))
        {
            driver_diag(&line);
        }
        m
    }

    /// Collect the **owner** child if it has exited on its own. It needs its own
    /// sweep because it is not a worker slot: an owner that aborted at an armed
    /// site (only `hangup.after_probe_before_cas`, the owner's callback) was
    /// otherwise never counted. Such a death is not a failure (a survivor
    /// inherits) and the owner is left un-replaced.
    fn reap_owner(owner: &mut Option<Kid>, ledger: &mut CrashLedger, dir: &Path) {
        let Some(kid) = owner.as_mut() else { return };
        // [diag] Instrument 5's poll. See `note_core_dumping`.
        note_core_dumping(kid);
        match kid.proc.try_wait() {
            Ok(Some(status)) => {
                // [diag] Instrument 5's two clock reads, taken at the reap.
                let reaped = Reaped::now(status);
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt as _;
                    if status.signal() == Some(libc::SIGABRT) {
                        ledger.record_abort(kid.crash_at.as_deref());
                    }
                }
                // [diag] Instrument 5. The creating owner runs no `work` loop, so it
                // holds the role from creation until it exits, whatever the marker
                // names by now (which is why the marker is printed, not asked). One
                // line: this child is reaped once.
                let reap = Reap::of("reap_owner", None, kid, reaped);
                let role = RoleLedgers::read(dir).of(reap.pid, true);
                if let Some(line) = reap_diag(&reap, role, Some("unscheduled-owner-death")) {
                    driver_diag(&line);
                }
                let _ = status;
                *owner = None;
            }
            Ok(None) => {}
            Err(_) => *owner = None,
        }
    }

    /// Collect children that have exited, recording the ones that reported a
    /// violation. Only [`EXIT_VIOLATION`] means the arena lied; any other
    /// non-zero exit (a joiner whose owner died mid-handshake) is expected.
    fn reap_finished(
        kids: &mut [Option<Kid>],
        violations: &mut Vec<String>,
        ledger: &mut CrashLedger,
        dir: &Path,
    ) {
        for (i, slot) in kids.iter_mut().enumerate() {
            let Some(kid) = slot.as_mut() else { continue };
            // [diag] Instrument 5's poll. See `note_core_dumping`.
            note_core_dumping(kid);
            match kid.proc.try_wait() {
                Ok(Some(status)) => {
                    // [diag] Instrument 5's two clock reads, taken at the reap.
                    let reaped = Reaped::now(status);
                    // A child that aborted at an armed §11.3 site is counted so the run
                    // can say sites *fired*. Not a failure: the invariant checks decide
                    // whether the state was repairable. `SIGABRT` and not an exit code,
                    // because §11.3 requires `abort()` (a panic would run the `Drop`s
                    // that repair the damage under test).
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
                    // [diag] Instrument 5: after a migration the role holder is a worker
                    // in `kids`, so an heir aborting in its own hangup callback is reaped
                    // here. Held-role status comes from the ledgers (`role_left_path`),
                    // read only for a child that exited on its own. One line per reap.
                    let reap = Reap::of("reap_finished", Some(i), kid, reaped);
                    let role = RoleLedgers::read(dir).of(reap.pid, false);
                    if let Some(line) = reap_diag(&reap, role, Some("unscheduled-owner-death")) {
                        driver_diag(&line);
                    }
                    *slot = None;
                }
                Ok(None) => {}
                Err(_) => *slot = None,
            }
        }
    }

    /// Join the arena the owner child created, and hold a reader on it for the
    /// whole run. The driver is a joiner, not the owner: it keeps the segment
    /// alive (a joiner's mapping holds the memfd, so the last child's death cannot
    /// free the arena under [`check_recovery`]) and is a reader that is never
    /// killed. It **never inherits**: an owner driver would be the unkillable
    /// process again (see [`work`]). `Never`, so a driver that raced its owner
    /// child fails rather than creating a second arena. [`kill_the_owner`]
    /// probes from outside for the `ArenaHeldButUnreachable` failure shape.
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

    /// Probe every chain edge for the window it currently retains, by a
    /// deliberate `Extrapolation` an hour past the shared clock (`Plan::at`
    /// reports `oldest`/`newest` exactly when it refuses, and the probe never
    /// succeeds or disturbs a ring). Returns **fewer** than `CHAIN.len()` entries
    /// when an edge is empty or unreadable; callers must treat that as "no common
    /// window".
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

    /// The stamps every chain edge can answer *at once*, if there are any. An
    /// intersection has no state to get stuck in (a hill climb re-aiming at the
    /// failing edge can oscillate between disjoint windows); the per-edge probing
    /// also buys the single-edge reads in [`observe`]. `None` means the windows
    /// are disjoint *right now*, normal shortly after a writer was killed; the
    /// run-level floor in [`drive`] refuses a run where they always were.
    fn common_window(windows: &[EdgeWindow]) -> Option<(i64, i64)> {
        if windows.len() != CHAIN.len() {
            return None;
        }
        let lo = windows.iter().map(|w| w.oldest).max()?;
        let hi = windows.iter().map(|w| w.newest).min()?;
        (lo <= hi).then_some((lo, hi))
    }

    /// A stamp inside `[lo, hi]`, so interpolation *between* slots is exercised.
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
    /// # Why the read counts are not enough
    ///
    /// A ring in a shared arena outlives the process that filled it, so an arena
    /// whose writers have all gone still answers every lookup inside the window
    /// it froze with, and [`Reads`] cannot tell that from a live one. On the
    /// nightly configuration (2026-08-17) an arena with no writer since t = 30 s
    /// still scored the full 256 composed reads per round and would have printed
    /// `PASS`; on the GitHub runner the same wedge froze the rings *without*
    /// overlap and the floor caught it. Which way dead rings fall is a coin flip,
    /// so a gate that depends on it is not a gate: everything below is checked on
    /// every round. **A perfect score is the tell**: a live arena misses 5 to 25
    /// in every 25 600 reads because a writer moves the ring between the probe
    /// and the read; 100% is what nothing moving looks like.
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
        /// `now - newest` for the *freshest* chain edge, ns: whether anybody is
        /// still writing. `None`, not a sentinel, when no edge reported a window
        /// (an `i64::MAX` sentinel was once summed into the mean and printed as
        /// twenty-two years); such rounds are counted separately.
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
        /// Was the arena *being written* during this round? Both halves are
        /// needed: a live claim holder that is not pushing leaves the rings
        /// frozen, and a recent sample with no holder is the moments after a
        /// writer died. One second is derived, not tuned: `work` publishes at
        /// ~1 kHz on 40% of operations, so a held edge is written every ~2.5 ms.
        /// A round where no edge has ever been written is not live (`None` fails
        /// the conjunction).
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
        /// Denominators for the averages below: a round with no window contributes
        /// no age, and one with an intersection contributes no gap.
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

        /// Milliseconds, or `n/a` when the quantity was never observed (a mean over
        /// zero samples is nothing): on `freshest` no edge was ever written, on
        /// `gap` the windows always intersected.
        fn mean_ms(sum: i64, n: u64) -> String {
            if n == 0 {
                "n/a".to_string()
            } else {
                format!("{:.0}ms", sum as f64 / n as f64 / 1e6)
            }
        }

        /// The same, for a maximum.
        fn max_ms(v: i64, n: u64) -> String {
            if n == 0 {
                "n/a".to_string()
            } else {
                format!("{:.0}ms", v as f64 / 1e6)
            }
        }

        /// The one-line periodic summary. One space after the prefix, not three:
        /// `tests/torture.rs` finds the composed-read total by the first
        /// `shm_torture:   ` line.
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
            // `n/a` when no round was non-overlapping: `laggard` is then all zeroes
            // and `max_by_key` would still name an edge.
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

    /// Read the four chain edges' claim words and the participant table. Two
    /// predicates, deliberately: `slots_registered` counts `LIVE` records (what
    /// the owner's slot assigner in `crates/tf_tree/src/open.rs` consults),
    /// `slots_alive` counts those the *kernel* agrees are alive (`docs/PHASE2.md`
    /// §5). A healthy arena has them equal; their difference is §11.4's leaked
    /// participant slot.
    fn census(tree: &Tree, h: &mut RoundHealth) {
        census_with(tree, h, None);
    }

    /// [`census`], optionally keeping each slot it counted. `seen` is filled from
    /// the same reads and order as the counts, so the `[diag]` population line
    /// names exactly the processes the deferral decision counted.
    fn census_with(tree: &Tree, h: &mut RoundHealth, mut seen: Option<&mut Vec<SlotSeen>>) {
        let view = tree.arena_view();
        // `1..=CHAIN.len()`: `EdgeId` is 1-based (`TreeBuilder::build_with` sizes the
        // table `declared + 1`, leaving id 0 an unclaimable sentinel); a `0..` walk
        // never looked at `arm->tool`.
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
            // `(epoch << 16) | (slot + 1)` and reserves a `CLAIMING` sentinel only
            // the helper knows.
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
            let identity = table.identity(slot);
            if identity.is_some() {
                h.slots_registered += 1;
            }
            let alive = tree.participant_alive(slot);
            if alive {
                h.slots_alive += 1;
            }
            if let Some(seen) = seen.as_deref_mut() {
                if identity.is_some() || alive {
                    seen.push(SlotSeen {
                        slot,
                        pid: identity.map(|(pid, _, _)| pid),
                        alive,
                    });
                }
            }
        }
    }

    /// A burst of checked reads from the never-killed observer, and a
    /// [`RoundHealth`] describing the arena they came out of. Returns how many
    /// transforms were validated; [`drive`] enforces a floor on it ("0
    /// violations" and "0 reads" print the same verdict). Both shapes are read:
    /// the composed `map -> tool` (a bad sample on any edge reaches it, but it
    /// needs all four windows to overlap) and per-edge reads, which need only
    /// their own ring and see the injected NaN on whichever edge the injector holds.
    fn observe(
        tree: &Tree,
        rng: &mut Rng,
        violations: &mut Vec<String>,
        health: &mut RoundHealth,
    ) -> Reads {
        let mut reads = Reads::default();
        let guard = tree.guard();
        let windows = edge_windows(tree, &guard);

        // Measured before the reads: they take long enough for a writer to move.
        health.windows = windows.len();
        let now = now_nanos();
        health.freshest_age = windows.iter().map(|w| now.saturating_sub(w.newest)).min();
        census(tree, health);
        // `let Some(..)`, not `expect`: `None` only when no edge reported a window,
        // and a soak must not panic over a diagnostic.
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

    /// Slots holding a `LIVE` record for a process the kernel says is dead, from
    /// the caller's own attachment and excluding its own slot. One function so
    /// the teardown's wait and `check_recovery` ask the same question.
    fn dead_participant_slots(tree: &Tree) -> Vec<u32> {
        let me = tree.participant_slot();
        let view = tree.arena_view();
        let table = view.participants();
        (0..table.capacity() as u32)
            .filter(|slot| {
                *slot != me && table.identity(*slot).is_some() && !tree.participant_alive(*slot)
            })
            .collect()
    }

    struct Recovery {
        failures: Vec<String>,
        notes: Vec<String>,
    }

    /// §11.4's "participant and claim slots never leak", checked once the run is
    /// quiescent, on the observer's attachment held for the whole run (the arena
    /// the children tortured, not a fresh one).
    fn check_recovery(
        tree: &Tree,
        unreachable_by_hangup: &[u32],
        hangup_collector_pinned: bool,
    ) -> Result<Recovery> {
        let mut out = Recovery {
            failures: Vec::new(),
            notes: Vec::new(),
        };

        // Which verdict is in force, printed on every run, so a change that made
        // `hangup_collector_pinned` false everywhere cannot leave
        // `a_run_that_never_migrates_holds_every_worker_record_to_the_strict_path`
        // green while asserting nothing (that test reads this line).
        out.notes.push(
            if hangup_collector_pinned {
                "recovery: the role never moved, so the owner's hangup callback was a live \
                 collector for the whole teardown and every record outside the swept \
                 partition is judged on the STRICT path"
            } else {
                "recovery: the role moved during this run, so there may have been no owner \
                 to run a hangup callback by teardown; the strict path is NOT in force and \
                 every record is swept and then required to have been collected"
            }
            .to_string(),
        );

        let me = tree.participant_slot();
        let view = tree.arena_view();

        // Count what the dead left before reclaiming it: `reap_dead`'s return
        // cannot tell "nothing to reclaim" from "this step did not run". `stale`
        // is the arena's record of claims held by now-dead participants (every
        // child has been waited for and this process holds none), `reaped` how
        // many came back.
        let mut stale = 0usize;
        for edge in 0..view.header().max_edges {
            let Some(rec) = view.claim(EdgeId(edge)) else {
                continue;
            };
            if rec.owner.load(Ordering::Acquire) != 0 {
                stale += 1;
            }
        }

        // Reap after the count and before the claim probe: a claim held by a
        // process the kernel has cleaned up is reapable, not leaked (A3).
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
            // Claiming is the sharpest test available: it succeeds only if no
            // owner remains.
            match tree.claim(c, p) {
                Ok(w) => drop(w),
                Err(e) => out.failures.push(format!(
                    "edge {parent}->{child} is still claimed after every writer died and \
                     reap_dead ran: {e:?}"
                )),
            }
        }

        // §11.4's "participant ... slots never leak" needs **both** predicates (see
        // [`census`]). `participant_alive` is the kernel's answer (`state == LIVE`
        // and the OFD lock byte held), so a `SIGKILL`ed child's record reads dead;
        // `state == LIVE` alone (`table.identity`) does not. Collection is lazy: the
        // assigner (`docs/decisions/0028` plan step 3) reaches a slot only when a
        // grant walks past it, the hangup callback (step 4) only when the owning
        // socket closes, and the sweep (step 5, `Tree::reap_participants`) only
        // when called. A record still held by a dead process when the run is over
        // has therefore leaked. Before the owner's hangup callback collected
        // records (`docs/PHASE2.md` §3.9/§5), 63 of 64 slots leaked within a
        // hundred rounds at `--children 6 --kill-hz 6` and this check was blind to
        // it. It stays because each automatic collector is one CAS on one path
        // that runs only if something drives it, and a wedged arena reads like a
        // healthy one; `arena_is_live` catches the consequence, this names the cause.
        //
        // `SETTLE_WINDOW` is a fixed sleep, not a poll with an exit: a record whose
        // kernel teardown has not finished reads as *alive*, so an early exit would
        // take the verdict before it appears (see [`dead_participant_slots`]).
        // Every process that could run a hangup callback has been reaped by now, so
        // it does not give that callback more time; it covers the gap between the
        // last `wait()` and the kernel finishing, and the sweep below. The poll
        // with a reachable exit is the teardown one in [`drive`].
        //
        // After a migration one of the two automatic collectors is not coming for
        // *some* records, and that is paid for here for those records only:
        // `unreachable_by_hangup` (built in [`drive`]; [`record_attachment`] says
        // why only the child knows) is partitioned **before** anything is swept.
        // Sweeping the whole table on a migrated run would let a deleted hangup
        // collector still print `PASS`; enforcing the strict verdict there failed
        // healthy runs, because usually no owner is alive by teardown. What ships
        // requires a sweep to have worked on every run and reaches the strict
        // verdict only where the collector is pinned.
        /// How long the kernel gets to finish tearing the killed children down
        /// before the strict verdict is read: eight 250 ms steps.
        const SETTLE_WINDOW: Duration = Duration::from_millis(8 * 250);

        let table = view.participants();
        std::thread::sleep(SETTLE_WINDOW);
        let mut leaked = dead_participant_slots(tree);
        // The records no hangup callback can reach are separated by pid, and
        // first. The partition is taken from `leaked`, computed **before** any
        // sweep, so a record outside it fails this run whatever
        // `reap_participants` does afterwards; that ordering is the difference
        // between a check and an exemption. Partitioned records are not given a
        // pass: they are swept and the sweep must have worked
        // (`docs/decisions/0028` plan step 5, the collector of last resort).
        let (mut unreachable, others): (Vec<u32>, Vec<u32>) = leaked.iter().partition(|slot| {
            table
                .identity(**slot)
                .is_some_and(|(rec_pid, _, _)| unreachable_by_hangup.contains(&rec_pid))
        });
        leaked = others;
        // On a run whose role migrated the partition is reported, not enforced:
        // the strict half presumes an owner was alive and serving while these
        // sockets closed, which after a migration is usually false (the heir is a
        // worker whose loop ends, leaving no owner; `drive` measured this). The
        // migrating arm still requires every record swept and then collected, so
        // broken reclamation fails it (`Tree::reap_participants` returning 0 fails
        // every seed tried); what it cannot say is *which* collector should have
        // acted. `--no-kill-owner` pins that, and
        // `a_run_that_never_migrates_holds_every_worker_record_to_the_strict_path`
        // drives it.
        if !hangup_collector_pinned && !leaked.is_empty() {
            out.notes.push(format!(
                "recovery: {} slot(s) were held by processes whose last recorded attachment \
                 named the owner still serving at teardown, so the hangup callback was their \
                 expected collector — but the role migrated during this run, and a heir is an \
                 ordinary worker that can detach and leave the arena ownerless before the \
                 teardown finishes, so this run cannot say the callback was the collector \
                 that failed. They are swept with the rest rather than failed. Run \
                 `--no-kill-owner` for the arm that pins this.",
                leaked.len()
            ));
            unreachable.append(&mut leaked);
            unreachable.sort_unstable();
        }
        if !unreachable.is_empty() {
            let partitioned = unreachable.len();
            let swept = tree.reap_participants();
            unreachable
                .retain(|slot| table.identity(*slot).is_some() && !tree.participant_alive(*slot));
            out.notes.push(format!(
                "recovery: {partitioned} slot(s) held a record this run does not require the \
                 hangup callback to have collected — an owner's own, a pre-migration \
                 survivor's (docs/decisions/0043), or, on a run whose role migrated, any \
                 record at all (see the note above) — so `reap_participants` was asked for \
                 them; it reclaimed {swept} record(s) and {} of those slot(s) remain",
                unreachable.len()
            ));
            if !unreachable.is_empty() {
                out.failures.push(format!(
                    "participant slot(s) {unreachable:?} still hold a dead process's LIVE \
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
                 says is dead {:?}{}. No collector reclaimed these, and the driver did not \
                 classify them as records no hangup callback could reach: each was held by a \
                 process whose last recorded attachment named the owner that was still \
                 serving at teardown, so step 4's hangup callback is the collector this run \
                 expected — which is a statement about what the driver knows, not a \
                 reconstruction of which socket closed when. The verdict was taken before any \
                 sweep ran, so `reap_participants` cannot answer for them. A dead record is \
                 otherwise collected only when a grant walks past its slot \
                 (`docs/decisions/0028` plan step 3), and this run is over, so none is coming. \
                 `docs/PHASE2.md` §11.4 requires that participant slots never leak and §5 \
                 requires that liveness come from the lock byte, never from `state`. This \
                 verdict is only reached on a run whose owner never changed, where that \
                 callback ran in a process that was parked and serving for the whole \
                 teardown. **The message used to close by ruling out a late hangup callback \
                 on the strength of the retry loop above, which is unsound**: by the time \
                 that loop runs every process that could run such a callback has been reaped, \
                 so elapsed time here is evidence of nothing.",
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
        // [diag] Instrument 4's clock for this process's first join, taken first so
        // argument parsing and ballast count as `setup_ms`.
        let entered = Instant::now();
        let exec = std::env::var(SPAWNED_AT_ENV)
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .map(|at| Duration::from_nanos(now_nanos().saturating_sub(at).max(0) as u64));
        let mut seed = 1u64;
        let mut inject = false;
        let mut readers_only = false;
        let mut no_inherit = false;
        let mut ballast_mb = 0usize;
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
                "--ballast-mb" => {
                    ballast_mb = it
                        .next()
                        .context("--ballast-mb needs a value")?
                        .parse()
                        .context("--ballast-mb")?;
                }
                other => bail!("child: unknown argument `{other}`"),
            }
        }
        // The positive control for the kill-window class (see [`kill_window_path`],
        // [`Args::victim_ballast_mb`]): a window on a plain release build in one
        // flag. Dirtied a page at a time, since an untouched page costs nothing to
        // tear down. Allocated in 1 MiB chunks, not one block, because transparent
        // huge pages would otherwise shrink the reap ~500x and the control would
        // silently do nothing (measured 2026-09-12; see [`Args::stop_owner_ms`]): a
        // 1 MiB mmap VMA cannot hold a 2 MiB-aligned range, so THP cannot back it.
        // `MADV_NOHUGEPAGE` would need `unsafe` (`scripts/unsafe-budget.txt`).
        const CHUNK_MIB: usize = 1;
        let ballast: Vec<Vec<u8>> = (0..ballast_mb / CHUNK_MIB)
            .map(|_| {
                let mut v = vec![0u8; CHUNK_MIB * 1024 * 1024];
                for i in (0..v.len()).step_by(4096) {
                    v[i] = 0xA5;
                }
                v
            })
            .collect();
        // One `could not join` line per process; see the arm below.
        let _ballast = std::hint::black_box(&ballast);

        let mut rng = Rng::new(seed);
        let dir = runtime_dir();
        // One `could not join` line per process; see the arm below.
        let mut reported = false;
        // [diag] Instrument 4 (see `JoinEpisode`): bounded by `SLOW_JOIN_LINES` per
        // process, `SLOW_JOIN_RUN_LINES` per run, and one refusal and one join
        // line per episode.
        let mut join = JoinEpisode::new("first-join", entered, exec);
        let mut slow_join_lines = 0u32;

        loop {
            // `Never` in every child: the owner child creates the arena, and a child
            // that created a second would split the run into a green one that
            // validates nothing; `Never` makes that a failed `open()` this loop
            // retries. The owner this attachment will register with is recorded
            // before the `open()` ([`record_attachment`]), so `check_recovery` can
            // judge this child strictly instead of exempting it (`0043`).
            let attempt_started = Instant::now();
            if join.attempts == 0 {
                join.lead = attempt_started.duration_since(join.started);
            }
            record_attachment(&dir, read_owner_pid(&dir));
            let open_started = Instant::now();
            join.pre_open += open_started.duration_since(attempt_started);
            join.attempts += 1;
            let tree = match tf_tree::Open::new()
                .mode(AttachMode::ReadWrite)
                .create(CreatePolicy::Never)
                .timeout(Duration::from_secs(2))
                .open()
            {
                Ok(t) => {
                    join.in_open += open_started.elapsed();
                    t
                }
                // Expected while an owner is being killed: back off and retry. The
                // reason is printed, once per process, because a child that can
                // never join leaves every ring frozen and the read counts perfect
                // (`NoParticipantSlots` and `ArenaHeldButUnreachable` are different
                // bugs); a healthy run prints none.
                Err(e) => {
                    join.in_open += open_started.elapsed();
                    if !reported {
                        reported = true;
                        eprintln!(
                            "shm_torture: child {} could not join: {e}",
                            std::process::id()
                        );
                    }
                    join.refused(e, &mut slow_join_lines, &dir);
                    let backoff_started = Instant::now();
                    std::thread::sleep(Duration::from_millis(10 + rng.below(40)));
                    join.backoff += backoff_started.elapsed();
                    continue;
                }
            };
            // [diag] Instrument 4 writes here, attached and before `work`'s first
            // `owner_lost` poll, the one child-side site not moved past the drop:
            // deferring would lose the line whenever the attachment ends in the
            // driver's `SIGKILL`. It costs one claim-file append and one `stderr`
            // write, only after a join past [`SLOW_JOIN`].
            join.joined(&mut slow_join_lines, &dir);
            let cap_exit = work(&tree, &dir, &mut rng, inject, readers_only, no_inherit)?;
            // [diag] Instrument 4: the drop is spelled out so the next episode's
            // `drop_ms` can time it (for an owner it also stops the serving thread).
            let detached = Instant::now();
            drop(tree);
            // [diag] Instrument 3's two writes, after the drop so neither lengthens
            // the tenure `CapExit` reports; the ledger line only when this
            // attachment inherited, the one way a worker leaves the role alive
            // (`role_left_path`).
            if let Some(exit) = cap_exit {
                if exit.tenure.is_some() {
                    record_role_left(&dir);
                }
                child_diag(&exit.line());
            }
            join = JoinEpisode::new("rejoin", detached, None);
        }
    }

    /// The random-operation loop against one attachment. Returns when it decides
    /// to detach and re-join (§11.4's "attach/detach"). `Some` is `[diag]`
    /// instrument 3's reading at a cap exit, for [`child`] to write after the
    /// drop; it decides nothing.
    fn work(
        tree: &Tree,
        dir: &Path,
        rng: &mut Rng,
        inject: bool,
        readers_only: bool,
        no_inherit: bool,
    ) -> Result<Option<CapExit>> {
        // Interning can fail while another participant is mid-mutation: a retry.
        let mut ids = Vec::new();
        for (parent, child) in CHAIN {
            match (tree.frame(parent), tree.frame(child)) {
                (Ok(p), Ok(c)) => ids.push((p, c)),
                _ => return Ok(None),
            }
        }
        let (map, tool) = (ids[0].0, ids[ids.len() - 1].1);

        // The edge index travels with the writer: §11.4's two-writers check reads
        // its claim record from inside the holder (`EdgeWriter` exposes only
        // `push`, and an accessor would be a public-API change).
        let mut held: Option<(usize, tf_tree::EdgeWriter<'_>)> = None;
        // Per attachment, so `record_trigger_outcome`'s cap bounds one `work` call.
        let mut triggers_logged = 0usize;
        // A bounded number of operations per attachment, so every child re-attaches.
        // The cap is extended, not waived, while this process serves the rendezvous
        // (returning drops the attachment like the detach arm, an unobserved owner
        // death), until its extensions run out; see [`MAX_OWNER_CAP_EXTENSIONS`].
        let mut extensions = 0u32;
        let mut ops_left: u32 = OPS_PER_ATTACHMENT;
        // Whether this attachment is parked at its cap awaiting a kill window, so
        // the stall is recorded once, not once per re-check.
        let mut capped_in_window = false;
        // [diag] Instrument 3's counters, each advanced only on a rare branch:
        // one-operation budgets granted while a kill window holds the cap, and
        // when this attachment took the role.
        let mut cap_rechecks = 0u32;
        let mut inherited_at: Option<Instant> = None;
        while ops_left > 0 {
            ops_left -= 1;
            // The cap is the detach arm's quieter twin and leaves the arena the same
            // way, so it asks the same two questions: am I the role holder, and is
            // an owner kill in flight? It is rarer (~2% of kills per heir against
            // the detach arm's ~56% for a 49 ms window), but one departure short of
            // the pool is still one. A cap reached inside the window is extended,
            // not charged against `MAX_OWNER_CAP_EXTENSIONS`, since the window is
            // bounded by the driver's own `wait()`.
            if ops_left == 0
                && extensions < MAX_OWNER_CAP_EXTENSIONS
                && read_owner_pid(dir) == Some(std::process::id())
            {
                extensions += 1;
                ops_left = OPS_PER_ATTACHMENT;
            } else if ops_left == 0 && kill_window_open(dir) {
                // One record per stall, not per re-check: `ops_left = 1` re-enters
                // this branch every operation until the window closes.
                if !capped_in_window {
                    capped_in_window = true;
                    record_detach_skip(dir);
                }
                ops_left = 1;
                cap_rechecks += 1;
            } else if ops_left > 0 {
                capped_in_window = false;
            }
            // Pacing, not politeness: an unthrottled loop covers ~9 microseconds of
            // a 64-slot ring and busy-waits every core, so the kills land in one
            // hot loop instead of across the protocol; at ~1 kHz the ring covers
            // ~64 ms. It is not what feeds the reader (`observe` probes each ring
            // first).
            std::thread::sleep(Duration::from_micros(200 + rng.below(1_600)));

            // §3.5's trigger, and the participants are the callers: there is no
            // daemon (`docs/decisions/0019`), so a survivor that never evaluates
            // `owner_lost` never becomes owner. That is worth nothing when no child
            // is *attached* (only a joined participant can inherit and an ownerless
            // arena admits no new one), hence the pre-kill census and the pool
            // floor. Healthy cost: one non-blocking `poll`; `F_OFD_GETLK` only
            // after a hangup.
            if !no_inherit && tree.owner_lost() {
                // The answer is recorded before it is filtered: `Contended` and
                // `OwnerAlive` are not inheritances, but discarding them would lose
                // whether the trigger was ever *answered*, the difference between
                // an engine that refused every heir and an arena with none left.
                let outcome = tree.inherit_ownership();
                record_trigger_outcome(dir, &mut triggers_logged, &outcome);
                if let Ok(Inheritance::Inherited) = outcome {
                    // pid first, then the log line: the driver reads the pid to pick the
                    // next victim, so it must never be behind.
                    publish_owner_pid(dir);
                    record_inheritance(dir);
                    inherited_at = Some(Instant::now());
                }
            }

            match rng.below(100) {
                // Claim an edge, if we hold none.
                0..=9 => {
                    if held.is_none() && !readers_only {
                        let i = rng.below(CHAIN.len() as u64) as usize;
                        let (p, c) = ids[i];
                        // A refused claim is correct when somebody else holds it.
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
                //
                // It has a cost, and [`drive`]'s teardown names it.
                17 => {
                    let _ = tree.reap_participants();
                }
                // Detach and re-join — **unless this process is the one serving
                // the rendezvous.**
                //
                // The role holder abdicating here is an owner death that nothing
                // observes: dropping `Attachment::Owner` stops the serving
                // thread and releases byte 0, so the role falls vacant with no
                // `Migration` recorded and nothing requiring it to recover. It
                // is the same defect as an ordinary victim draw taking the role
                // holder, one layer in, and it is the larger of the two — the
                // arm fires on ~2% of operations, so a worker abdicates roughly
                // every fifty, against an owner-kill interval measured in
                // seconds.
                //
                // Asked of the harness's own `owner.pid` marker rather than of
                // the engine: `Tree::is_joined` is `pub(crate)`, and adding a
                // public predicate for a harness's benefit would be an API
                // change (`docs/API.md` §7). The marker is written by the heir
                // before it logs the inheritance, so it is never behind. Read
                // only on this arm and at the operation cap — about one file
                // read per fifty operations — because reading it per operation
                // would be 2000 of them per attachment and would change the
                // workload being measured.
                //
                // §11.4's attach/detach churn is unaffected: every participant
                // that is *not* serving still detaches on this arm, and the
                // owner still dies several times a minute on the arm built to
                // require what §3.5 owes for each death.
                18..=19 => {
                    if read_owner_pid(dir) != Some(std::process::id()) {
                        // **Checked last, immediately before leaving.** The
                        // driver opens the window before it signals, so a child
                        // that reads `false` here is reading it at an instant
                        // when the owner is still alive — and a detach then is
                        // recoverable, because its re-`open()` still finds a
                        // serving owner. Reading earlier in the arm, or once per
                        // round, would leave a window whose width is the harness's
                        // own scheduling rather than a syscall's.
                        //
                        // See [`kill_window_path`] for what this is and what it
                        // costs. It suppresses a detach, never a kill, never an
                        // inheritance and never a violation.
                        if kill_window_open(dir) {
                            record_detach_skip(dir);
                            continue;
                        }
                        return Ok(None);
                    }
                }
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
                        // 1-based (see [`census`]), so the chain's four edges
                        // are `EdgeId(1)..=EdgeId(4)` and an index used as an id
                        // reads the *neighbouring* edge's claim
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
        // [diag] **Instrument 3: the role holder leaving through its own cap.**
        //
        // Hypothesis (2026-09-13): H114 held the role past
        // `MAX_OWNER_CAP_EXTENSIONS` because two owner kills were deferred, and
        // then left here. Dropping the `Tree` stops the serving thread and
        // frees byte 0 with no census, no kill window and no `Migration`. That
        // log could not say whether anybody was attached to inherit. The line
        // gives the pid, how long it held the role, and the attached slots by
        // identity as *this* process's attachment sees them immediately before
        // the drop.
        //
        // The only way out of this loop is `ops_left` reaching zero, so this is
        // exactly the cap exit. The detach arm returns earlier and cannot leave
        // as the role holder, since it is gated on the marker. A `VIOLATION`
        // exits the process. Operations are derived rather than counted per
        // operation: the initial budget, one full budget per extension, and one
        // operation per kill-window re-check.
        //
        // The marker is read after the loop, one operation after the cap
        // decision, so an inheritance taken in that last operation is caught
        // too. That is an unobserved owner departure all the same.
        //
        // **The census is taken here, while still attached, and nothing is
        // written.** [`child`] writes the line, and the `role_left_path` ledger
        // line, after it has dropped the `Tree`, so a stalled stderr reader or
        // a slow `/tmp` cannot lengthen the tenure being reported.
        //
        // Bound: one reading per attachment, and only an attachment whose
        // process the marker names, or that inherited, reaches it, so at most
        // one per inheritance.
        let named = read_owner_pid(dir) == Some(std::process::id());
        if named || inherited_at.is_some() {
            let mut h = RoundHealth::default();
            let mut seen = Vec::new();
            census_with(tree, &mut h, Some(&mut seen));
            return Ok(Some(CapExit {
                named,
                attachment_ops: u64::from(OPS_PER_ATTACHMENT) * u64::from(1 + extensions)
                    + u64::from(cap_rechecks),
                extensions,
                tenure: inherited_at.map(|t| t.elapsed()),
                seen,
                wall: std::time::SystemTime::now(),
            }));
        }
        Ok(None)
    }

    /// `[diag]` instrument 3's reading at a cap exit, taken in [`work`] while
    /// the attachment is still held and written by [`child`] after the drop.
    struct CapExit {
        /// Whether the marker named this process at the cap exit.
        named: bool,
        attachment_ops: u64,
        extensions: u32,
        /// Since this attachment inherited. `None` when it did not, which with
        /// `named` set means the marker named a process that this attachment
        /// never made the role holder.
        tenure: Option<Duration>,
        /// The census's slots, excluding this process's own.
        seen: Vec<SlotSeen>,
        wall: std::time::SystemTime,
    }

    impl CapExit {
        /// **`heirs_attached_after_exit` means what the driver's post-kill
        /// count means** (`N after it` on a `§3.5 owner kill` line): read-write
        /// participants alive other than the departing role holder and the
        /// driver's observer, which never inherits. The census skips its own
        /// slot and counts the observer's, and this line used to print that
        /// raw figure as `alive_other_than_self`, one more than the heirs this
        /// departure could leave behind. The driver is this process's parent.
        /// The identity list still names the observer's slot, marked
        /// `(driver)`.
        fn line(&self) -> String {
            let driver = std::os::unix::process::parent_id();
            let heirs = self
                .seen
                .iter()
                .filter(|s| s.alive && s.pid != Some(driver))
                .count();
            format!(
                "role-holder-cap-exit pid={} marker_named_this_pid={} attachment_ops={} \
                 extensions={}/{} tenure={} attached_by_identity={} \
                 heirs_attached_after_exit={heirs} stamp={}",
                std::process::id(),
                self.named,
                self.attachment_ops,
                self.extensions,
                MAX_OWNER_CAP_EXTENSIONS,
                self.tenure.map_or_else(
                    || "unknown(this attachment recorded no inheritance)".to_string(),
                    |t| format!("{:.3}s", t.as_secs_f64())
                ),
                seen_list(&self.seen, Some(driver)),
                stamp_of(self.wall),
            )
        }
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

    #[cfg(test)]
    mod tests {
        use super::core_dump_warning;

        /// `/proc/self/limits` as the kernel prints it, with the core row set
        /// to `soft`/`hard` and one neighbour on each side, so the parse has to
        /// find its row rather than read the first one.
        fn limits(soft: &str, hard: &str) -> String {
            format!(
                "Limit                     Soft Limit           Hard Limit           Units     \n\
                 Max file size             unlimited            unlimited            bytes     \n\
                 Max core file size        {soft:<20} {hard:<20} bytes     \n\
                 Max resident set          unlimited            unlimited            bytes     \n"
            )
        }

        /// **`docs/decisions/0057` Decision 6's warning fires on exactly one
        /// configuration**: armed crash points, a `|` `core_pattern`, and a
        /// soft `RLIMIT_CORE` that is not 1. The recipes' own configuration
        /// (`prlimit --core=1:1`) and the plain soak must stay silent, or the
        /// line stops meaning anything; the dev host's and the runner's shell
        /// default (`soft=0` on a pipe) must not, because 0 does not stop a
        /// pipe dump. The last two assertions separate the soft column from
        /// the hard one, which the recipes' `1:1` and the shell's
        /// `0:unlimited` cannot. The mutants are recorded on
        /// [`core_dump_warning`].
        #[test]
        fn the_core_dump_warning_names_only_a_dumping_pipe() {
            let apport = "|/usr/share/apport/apport -p%p -s%s -c%c -d%d -P%P -u%u -g%g -F%F -- %E";
            let coredumpd = "|/usr/lib/systemd/systemd-coredump %P %u %g %s %t \
                             9223372036854775808 %h %d";

            // The bare binary on either measured host: warns, naming the limit.
            let bare = core_dump_warning(true, apport, Some(&limits("0", "unlimited")));
            assert!(
                bare.as_deref()
                    .is_some_and(|w| w.contains("core soft limit of 0")),
                "soft=0 on a pipe dumps, so it must warn: {bare:?}"
            );
            assert!(
                core_dump_warning(true, coredumpd, Some(&limits("unlimited", "unlimited")))
                    .is_some()
            );
            assert!(
                core_dump_warning(true, coredumpd, None).is_some(),
                "an unreadable limit cannot be shown to be 1"
            );
            assert!(
                core_dump_warning(true, coredumpd, Some("Limit Soft Hard\n")).is_some(),
                "a limits file with no core row cannot be shown to be 1"
            );

            // The recipes' configuration: silent.
            assert_eq!(
                core_dump_warning(true, coredumpd, Some(&limits("1", "1"))),
                None
            );
            // Not armed: an abort is not what this run does, so silent.
            assert_eq!(
                core_dump_warning(false, apport, Some(&limits("0", "unlimited"))),
                None
            );
            // A file pattern writes a file, not a helper's run: silent.
            assert_eq!(
                core_dump_warning(true, "core", Some(&limits("0", "unlimited"))),
                None
            );

            // The kernel reads the SOFT limit. soft=1 with a looser hard limit
            // (what the ASan build read without the prefix) is suppressed...
            assert_eq!(
                core_dump_warning(true, coredumpd, Some(&limits("1", "unlimited"))),
                None,
                "soft=1 suppresses a pipe dump whatever the hard limit is"
            );
            // ...and soft=0 under hard=1 is not.
            let soft0_hard1 = core_dump_warning(true, apport, Some(&limits("0", "1")));
            assert!(
                soft0_hard1
                    .as_deref()
                    .is_some_and(|w| w.contains("core soft limit of 0")),
                "soft=0 dumps on a pipe even under hard=1, so it must warn: {soft0_hard1:?}"
            );
        }
    }
}
