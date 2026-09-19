//! The `docs/PHASE5.md` §9 benchmark artifact: one reproducible report (`results.json`, `index.html`, environment description).
//!
//! §9.3 is normative: if a row cannot be measured fairly, omit it and say why. The
//! measurement code lives in `just` recipes and examples; this module is refusal
//! machinery, so the report cannot print a number it has no right to:
//!
//! * [`Fitness::probe`] measures the host and decides whether a timing number taken
//!   here would describe this engine or somebody else's scheduler.
//! * [`Report::validate`] refuses a report whose rows overclaim: a timing row cannot
//!   be [`Status::Measured`] on a host that failed the probe, an unavailable row
//!   needs a reason *and* a reproduce command, the four §9.3 "where we are worse"
//!   topics must be present, each with a number or
//!   [`Worse::metrics_absent_because`]. Failure is "no report", never a flattering one.
//! * [`Status::Indicative`] labels numbers taken under `TF_TREE_BENCH_FORCE=1` as
//!   *not a claim*.
//!
//! `String` is fine here: reasons embed measured host facts.
//!
//! # Schema stability
//!
//! `results.json` is emitted by hand (`to_json`), so a field rename is a deliberate
//! edit; §12 gate 7 diffs the schema across machines. `SCHEMA` is the version.

use std::fmt::Write as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use tf_tree::{InterpPolicy, Stamp};

/// `results.json` schema identifier. Bump on any consumer-visible change.
///
/// `/2` added `drift` and `tolerance` to every metric, so [`crate::baseline`] knows
/// which direction is a regression.
pub const SCHEMA: &str = "tf_tree.bench-report/2";

/// The command that regenerates the report directory.
///
/// Not `cargo xtask bench-report`: `xtask` dispatches `loom | bench-gate | headers` only.
pub const REPRODUCE_RECIPE: &str = "just bench-report";

/// The row ids `docs/PHASE5.md` §9.2 requires. A row may be [`Status::Unavailable`]
/// but not *missing*.
pub const REQUIRED_ROWS: &[&str] = &[
    "cpu_per_consumer",
    "total_rss_n_consumers",
    "lookup_latency",
    "publish_to_visible",
    "scaling_curve",
    "tft_16_workers_rss",
    "tft_open_vs_bag_parse",
    "differential_agreement",
    "embedding_cross_crate",
    "lookup_ratio_vs_tf2",
];

/// Relative slack on the tf2 ratio row, as a fraction of the baseline. Wider than
/// the ~3% within-run band because between-build codegen also moves a ratio.
#[cfg(feature = "tf2")]
const RATIO_SLACK: f64 = 0.15;

/// Relative slack on the differential row's `max_deviation`: `9.0` fires above
/// **10x** the baseline.
///
/// The deviation sits near machine epsilon (~2.5e-16 rad/m), so a tight bound would
/// gate the compiler. A real disagreement lands at 1e-3 or worse, and the bound
/// still fires three orders below the row's 1e-12 pass tolerance.
pub const DEVIATION_SLACK: f64 = 9.0;

/// Relative slack (25%) on a latency percentile.
///
/// p99.9 moves several percent run to run even on a fit host, so 10% would flap; 25%
/// is well under any regression worth a bisect.
pub const LATENCY_SLACK: f64 = 0.25;

/// Relative slack on the idle arena's resident footprint: 300%, the metric may
/// quadruple before the gate fires.
///
/// `idle_arena_resident_bytes` is a Pss delta quantised to the page size (24 576 B
/// here, six 4 KiB pages). A 64 KiB-page host would read one page as a +166%
/// regression at a 100% band, and this is the one baseline number compared across
/// two machines (CI's `bench-gate` runs on another host).
///
/// The regression it guards (`docs/decisions/0021`) sits at 2 408 448 B, 24x this
/// bound: the order of magnitude is the finding, not the third digit.
pub const RESIDENCY_SLACK: f64 = 3.0;

/// The "where `tf_tree` is worse" topics `docs/PHASE5.md` §9.3 names, verbatim.
pub const REQUIRED_WORSE: &[&str] = &[
    "arena_memory_floor",
    "attach_latency",
    "format_bump_cost",
    "bridge_supervision",
];

/// The provenance facts `docs/PHASE5.md` §9.3 requires, as a closed list
/// [`Report::validate`] checks for *present and non-empty*. An explicit `unknown`
/// from an unreadable sysfs file counts as a report of that host.
///
/// Two THP keys, not one: §9.3 says "THP setting" but there are two knobs (see
/// [`Provenance::collect`] and §9.3's amendment).
pub const REQUIRED_FACTS: &[&str] = &[
    // Bullet 3, verbatim.
    "tf2_version",
    "ros_distro",
    "rmw_implementation",
    "kernel",
    "cpu_model",
    "transparent_hugepage",
    "transparent_hugepage_shmem",
    // Bullet 1, verbatim.
    "dds_qos",
    "executor_config",
];

/// The build facts an `unavailable` row's reason may rest on, evaluated once in
/// [`Build::current`] and carried on the [`Report`] so a test can hand
/// [`Report::validate`] a different build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Build {
    /// `cfg!(feature = "tf2")` — whether `tf2::BufferCore` is linked into this
    /// binary and therefore callable in-process, as
    /// `crate::ratio::measure` and `crate::differential::run_tf2` call it (both
    /// are `tf2`-gated, so they are named rather than linked).
    pub tf2_linked: bool,
    /// `cfg!(all(feature = "shm", target_os = "linux"))` — whether
    /// `Tree::open_frozen` and `Tree::freeze_to` exist in this binary at all.
    pub frozen_backend: bool,
}

impl Build {
    /// The build this binary actually is.
    #[must_use]
    pub fn current() -> Build {
        Build {
            tf2_linked: cfg!(feature = "tf2"),
            frozen_backend: cfg!(all(feature = "shm", target_os = "linux")),
        }
    }
}

/// The machine-checkable claim an `unavailable` row's reason rests on.
///
/// §9.3 requires a reason, and a check keyed on wording is defeated by rewording
/// (`tests::no_unavailable_reason_rests_on_a_claim_that_has_gone_stale`), so
/// [`Report::validate`] re-derives the ground on every run; the prose only elaborates.
///
/// `Ground::holds` returns [`None`] for [`Ground::MeasuredElsewhere`],
/// [`Ground::NoInstrument`] and [`Ground::MeasurementRefused`]: claims about the
/// repository or this run that nothing here can decide.
/// `tests::every_command_the_report_names_is_a_command_that_exists` resolves the
/// named recipe against the `justfile`. `NoInstrument` is as trustworthy as its
/// author, which is why it is its own greppable variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ground {
    /// This host cannot produce a trustworthy number on the row's own
    /// sensitivity axis. Decided by [`Fitness::axis`] — the same call the
    /// `measured` arm makes, read the other way round, so a row cannot claim
    /// the host is unfit on an axis the host passes.
    HostFitness,
    /// This host has fewer than `consumers + 1` physical cores, *and* the core
    /// budget reaches this row. Decided by [`Fitness::enough_cores`] together
    /// with the row's own `needs_n_cores` and sensitivity: a
    /// [`Sensitivity::Memory`] row is exempt from the budget by §9.3's
    /// amendment, so one that blames the core count is blaming a check that
    /// does not apply to it.
    HostCores,
    /// `tf2::BufferCore` is not linked into this binary. Decided by
    /// [`Build::tf2_linked`].
    Tf2NotLinked,
    /// `Tree::open_frozen` is not compiled into this binary. Decided by
    /// [`Build::frozen_backend`].
    FrozenBackendNotCompiled,
    /// The number exists and belongs to another artifact in this repository;
    /// the row names the recipe. **Not decidable here** — see the type's docs.
    MeasuredElsewhere,
    /// Nothing in this repository measures the quantity at all. **Not decidable
    /// here**, and the weakest of them.
    NoInstrument,
    /// The instrument ran on this host and declined to answer — an unresolved
    /// band, a verdict below the floor, a load error. **Not decidable here**:
    /// the ground is the run itself, which `validate` cannot repeat.
    MeasurementRefused,
}

impl Ground {
    /// Whether the claim still holds, or [`None`] where nothing here can decide
    /// it.
    #[must_use]
    fn holds(self, build: Build, fitness: &Fitness, row: &Row) -> Option<bool> {
        match self {
            Ground::HostFitness => Some(!fitness.axis(row.sensitivity).0),
            Ground::HostCores => Some(
                !fitness.enough_cores
                    && row.needs_n_cores
                    && row.sensitivity != Sensitivity::Memory,
            ),
            Ground::Tf2NotLinked => Some(!build.tf2_linked),
            Ground::FrozenBackendNotCompiled => Some(!build.frozen_backend),
            Ground::MeasuredElsewhere | Ground::NoInstrument | Ground::MeasurementRefused => None,
        }
    }

    /// What decides this ground, for the failure message. A reader who has just
    /// been told their reason is stale needs to know what to look at.
    #[must_use]
    fn decided_by(self) -> &'static str {
        match self {
            Ground::HostFitness => "the fitness probe's verdict on this row's sensitivity axis",
            Ground::HostCores => {
                "the measured physical-core count, together with this row's own \
                 `needs_n_cores` and sensitivity"
            }
            Ground::Tf2NotLinked => "cfg!(feature = \"tf2\")",
            Ground::FrozenBackendNotCompiled => {
                "cfg!(all(feature = \"shm\", target_os = \"linux\"))"
            }
            Ground::MeasuredElsewhere => "nothing here — the recipe it names is checked by a test",
            Ground::NoInstrument => "nothing here",
            Ground::MeasurementRefused => "nothing here — the run itself",
        }
    }

    /// The stable spelling, for [`Report::validate`]'s refusal messages and the test
    /// that seeds a stale ground.
    ///
    /// No row carries it into `results.json` or `index.html`: a new key rides a `SCHEMA`
    /// bump.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Ground::HostFitness => "host_fitness",
            Ground::HostCores => "host_cores",
            Ground::Tf2NotLinked => "tf2_not_linked",
            Ground::FrozenBackendNotCompiled => "frozen_backend_not_compiled",
            Ground::MeasuredElsewhere => "measured_elsewhere",
            Ground::NoInstrument => "no_instrument",
            Ground::MeasurementRefused => "measurement_refused",
        }
    }
}

/// Which way a metric may move before it is a regression, for [`crate::baseline`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drift {
    /// Context, not a claim: sample counts, the tolerance a comparison was run
    /// against, a clock-overhead control. The gate checks that the key is still
    /// *present* (its disappearance would silently shrink the artifact) and
    /// never compares the value.
    Informational,
    /// Smaller is better — latency, memory, deviation from a reference.
    LowerIsBetter,
    /// Larger is better — throughput, a scaling factor.
    HigherIsBetter,
}

impl Drift {
    /// The JSON/HTML spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Drift::Informational => "informational",
            Drift::LowerIsBetter => "lower_is_better",
            Drift::HigherIsBetter => "higher_is_better",
        }
    }
}

/// One named scalar inside a report cell.
#[derive(Debug, Clone)]
pub struct Metric {
    /// Stable key, e.g. `p99_ns`. Part of the JSON schema.
    pub key: &'static str,
    /// The measured value. Non-finite values are emitted as JSON `null`.
    pub value: f64,
    /// Unit, for the HTML column and for a reader of the JSON.
    pub unit: &'static str,
    /// Which way this number may move before [`crate::baseline`] calls it a
    /// regression.
    pub drift: Drift,
    /// Relative slack the regression gate allows, e.g. `0.10` for 10%. Only
    /// read when `drift` is directional.
    pub tolerance: f64,
}

impl Metric {
    /// A metric with the given key, value and unit, **informational**: a wrong
    /// direction is worse than none. [`Report::validate`] refuses a `measured` row with
    /// nothing directional.
    #[must_use]
    pub fn new(key: &'static str, value: f64, unit: &'static str) -> Metric {
        Metric {
            key,
            value,
            unit,
            drift: Drift::Informational,
            tolerance: 0.0,
        }
    }

    /// Mark this metric as one where growth is a regression, with `tolerance`
    /// relative slack (`0.10` = 10%).
    #[must_use]
    pub fn lower_is_better(mut self, tolerance: f64) -> Metric {
        self.drift = Drift::LowerIsBetter;
        self.tolerance = tolerance;
        self
    }

    /// Mark this metric as one where shrinkage is a regression, with
    /// `tolerance` relative slack (`0.10` = 10%).
    #[must_use]
    pub fn higher_is_better(mut self, tolerance: f64) -> Metric {
        self.drift = Drift::HigherIsBetter;
        self.tolerance = tolerance;
        self
    }
}

/// What a row's numbers are worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Measured on a host that passed [`Fitness::probe`], or not timing
    /// sensitive at all. This is the only status that is a claim.
    Measured,
    /// Measured after the operator overrode the fitness refusal with
    /// `TF_TREE_BENCH_FORCE=1`. Reported, labelled, and explicitly not a claim.
    Indicative,
    /// Not measured here. Carries the reason and the command that produces it
    /// on a host that can.
    Unavailable,
}

impl Status {
    /// The JSON/HTML spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Measured => "measured",
            Status::Indicative => "indicative",
            Status::Unavailable => "unavailable",
        }
    }
}

/// What kind of host fitness a row's numbers depend on. One boolean could not
/// answer this: different quantities fail for different facts about a machine.
///
/// - a **frequency governor** moves an absolute latency and cancels out of an
///   interleaved ratio;
/// - **SMT** likewise cancels when both arms interleave on one thread;
/// - a **busy machine** does *not* cancel out of a cross-engine ratio: the arms are
///   asymmetric (`tf2::BufferCore` locks per lookup, `tf_tree` does not), so load
///   inflates the quotient in our favour;
/// - **PSS** involves no clock, but `smaps_rollup` must be *readable*; a silent zero
///   would be a false PASS.
///
/// Every axis needs a release build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    /// The same inputs give the same answer on any host — a differential
    /// deviation, or arithmetic on the arena layout. Nothing to check.
    HostIndependent,
    /// An absolute duration. Needs a trustworthy clock *and* a quiet machine:
    /// every check in [`Fitness::reasons`] applies.
    AbsoluteTiming,
    /// A ratio between two engines measured by interleaving them within each round
    /// and taking the median of per-round quotients. Governor and SMT divide out; a
    /// debug build and a busy machine still invalidate it.
    ///
    /// `lookup_ratio_vs_tf2` constructs this, so a 4-core host can gate a *ratio*
    /// against tf2 where it cannot gate either absolute latency.
    /// `embedding_cross_crate` is **not** on this axis: §9.2 gates its absolute
    /// durations too.
    Ratio,
    /// Resident or proportional memory. Not a timing measurement, so the timing
    /// checks do not apply to it — but it does require that Pss be readable at
    /// all, which [`Fitness::memory_reasons`] carries.
    Memory,
}

/// One §9.2 row.
#[derive(Debug, Clone)]
pub struct Row {
    /// Stable id; must be one of [`REQUIRED_ROWS`].
    pub id: &'static str,
    /// Human title, as §9.2 words it.
    pub title: &'static str,
    /// What the two columns mean for *this* row, since they are not always
    /// "the same measurement on two engines".
    pub note: String,
    /// Which host facts this row's numbers actually depend on.
    pub sensitivity: Sensitivity,
    /// Whether this row runs `consumers` processes or threads at once, and so
    /// needs the core budget rather than (or as well as) a trustworthy clock.
    pub needs_n_cores: bool,
    /// Status of the row as a whole.
    pub status: Status,
    /// Why the row is unavailable (or indicative). Required unless `Measured`.
    pub reason: String,
    /// The machine-checkable claims [`Row::reason`] rests on.
    ///
    /// [`Report::validate`] refuses an `unavailable` row with none, and a row that
    /// prints numbers while carrying one. See [`Ground`] for the three it cannot
    /// re-derive.
    pub grounds: Vec<Ground>,
    /// The command that produces this row on a host that can measure it.
    pub reproduce: &'static str,
    /// The `tf_tree` column.
    pub tf_tree: Vec<Metric>,
    /// The `tf2` column.
    pub tf2: Vec<Metric>,
}

impl Row {
    /// An unavailable row: reason and reproduction command, no numbers.
    #[must_use]
    pub fn unavailable(
        id: &'static str,
        title: &'static str,
        note: &str,
        sensitivity: Sensitivity,
        reason: String,
        reproduce: &'static str,
    ) -> Row {
        Row {
            id,
            title,
            note: note.to_owned(),
            sensitivity,
            needs_n_cores: false,
            status: Status::Unavailable,
            reason,
            grounds: Vec::new(),
            reproduce,
            tf_tree: Vec::new(),
            tf2: Vec::new(),
        }
    }

    /// State the machine-checkable claims this row's reason rests on; an
    /// `unavailable` row with none fails [`Report::validate`].
    #[must_use]
    pub fn on(mut self, grounds: &[Ground]) -> Row {
        self.grounds = grounds.to_vec();
        self
    }

    /// Promote this row out of [`Status::Unavailable`], dropping the grounds that
    /// explained its absence so none goes stale.
    fn measured_as(&mut self, status: Status, reason: String) {
        self.status = status;
        self.reason = reason;
        self.grounds.clear();
    }

    /// Mark the row as running `consumers` processes or threads at once.
    #[must_use]
    pub fn n_way(mut self) -> Row {
        self.needs_n_cores = true;
        self
    }

    /// Whether this row reports an absolute duration; the JSON field of the same
    /// name, kept so `tf_tree.bench-report/2` does not change shape.
    #[must_use]
    pub fn timing_sensitive(&self) -> bool {
        matches!(self.sensitivity, Sensitivity::AbsoluteTiming)
    }

    /// The status this row should carry on `fitness`; the caller applies the core
    /// budget on top when [`Row::needs_n_cores`] is set.
    #[must_use]
    pub fn status_on(&self, fitness: &Fitness) -> Status {
        let (fair, _, _) = fitness.axis(self.sensitivity);
        Fitness::status_from(fair, fitness.forced)
    }
}

/// One §9.3 "where `tf_tree` is worse" entry, rendered inside the results table
/// by [`Report::to_html`].
#[derive(Debug, Clone)]
pub struct Worse {
    /// Stable id; must be one of [`REQUIRED_WORSE`].
    pub id: &'static str,
    /// The topic, as §9.3 names it.
    pub topic: &'static str,
    /// What is worse, stated plainly enough to be quoted against us.
    pub statement: String,
    /// Numbers, where the cost is measurable rather than operational.
    pub metrics: Vec<Metric>,
    /// Why [`Self::metrics`] is empty — required whenever it is, and forbidden
    /// beside metrics ([`Report::validate`]).
    ///
    /// Legitimate reasons: the cost is measured elsewhere by a recipe this binary
    /// cannot run (`bridge_supervision`), or it is not denominated in nanoseconds or
    /// bytes (`format_bump_cost`). "Nobody has got round to it" is not one.
    pub metrics_absent_because: Option<String>,
    /// Metric keys this entry deliberately did **not** carry, with the reason in
    /// [`Worse::statement`].
    ///
    /// Rust-side only: a JSON field would be a `SCHEMA` bump that invalidates
    /// `baseline/results-tf2.json`. It exists because `measure_idle_arena_resident`
    /// withholds the residency figure when Pss is unreadable or the delta is
    /// non-positive, and the direction rule would otherwise turn that into no artifact.
    pub metrics_withheld: Vec<&'static str>,
}

/// Whether this host can produce a timing number that means anything.
///
/// Two independent verdicts, deliberately not merged: clock trustworthiness
/// (`fair_for_timing`) and room for N consumers plus a publisher (`enough_cores`).
/// Merging them makes every stated reason wrong for half the rows.
#[derive(Debug, Clone)]
pub struct Fitness {
    /// True when no fact makes a clock reading untrustworthy: release build, quiet
    /// machine, no SMT, `performance` governor.
    pub fair_for_timing: bool,
    /// True when this host can produce a trustworthy *ratio* between two engines
    /// interleaved within a round; strictly weaker than [`Fitness::fair_for_timing`]
    /// (see [`Sensitivity`]).
    pub fair_for_ratios: bool,
    /// True when this host can produce a trustworthy *memory* figure; strictly weaker
    /// than [`Fitness::fair_for_timing`].
    pub fair_for_memory: bool,
    /// True when the host has at least `consumers + 1` physical cores.
    pub enough_cores: bool,
    /// Whether `TF_TREE_BENCH_FORCE=1` was set.
    pub forced: bool,
    /// One string per failed *timing* check; [`Fitness::ratio_reasons`] and
    /// [`Fitness::memory_reasons`] are subsets.
    pub reasons: Vec<String>,
    /// The subset of [`Fitness::reasons`] that also invalidates a ratio.
    pub ratio_reasons: Vec<String>,
    /// The subset of [`Fitness::reasons`] that also invalidates a memory figure.
    pub memory_reasons: Vec<String>,
    /// Why the core budget is short, when it is.
    pub core_reason: Option<String>,
    /// Consumer count the probe was asked about.
    pub consumers: usize,
    /// Measured busy fraction of the machine before the run.
    pub busy_fraction: f64,
    /// Physical cores from `/proc/cpuinfo` core ids, or the logical count when none
    /// are published; read `physical_cores_known` before quoting it.
    pub physical_cores: usize,
    /// Whether `physical_cores` is a measurement or the logical-CPU fallback.
    pub physical_cores_known: bool,
    /// Logical CPUs.
    pub logical_cpus: usize,
}

impl Fitness {
    /// Probe the host for `consumers` concurrent consumers plus one publisher.
    ///
    /// Thresholds are deliberately strict: failing one makes the affected rows
    /// [`Status::Unavailable`], never estimated.
    #[must_use]
    pub fn probe(consumers: usize) -> Fitness {
        // Every input is read here so `assess` can be handed hosts this one is not.
        Fitness::assess(
            consumers,
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            physical_cores(),
            crate::mp::busy_fraction(Duration::from_millis(300)),
            governors(),
            // Checked here so the CLI path (debug by default) cannot publish debug latencies.
            cfg!(debug_assertions),
            // `self_pss_kib` returns 0 when `smaps_rollup` is unreadable; a silent 0 would be a false PASS.
            crate::mp::self_pss_kib() > 0,
        )
    }

    /// The judgement half of [`Fitness::probe`], over measurements already taken, so
    /// hosts this one is not (no published core ids, an absurd `--consumers`) can be
    /// tested. `detected_physical` is [`None`] when the host published no core ids,
    /// which is not `Some(logical)`.
    #[must_use]
    pub fn assess(
        consumers: usize,
        logical: usize,
        detected_physical: Option<usize>,
        busy: f64,
        governors: Option<Vec<String>>,
        debug_build: bool,
        pss_readable: bool,
    ) -> Fitness {
        // Each failing check goes into the bucket naming the claims it invalidates; the
        // verdicts are unions of buckets, so a new check must state its reach.
        let mut reasons = Vec::new();
        // Invalidates everything: a debug build is a different program.
        let mut universal = Vec::new();
        // Invalidates a duration and a quotient, not a page count.
        let mut timing_and_ratio = Vec::new();
        // Invalidates a page count only.
        let mut memory_only = Vec::new();

        if !pss_readable {
            memory_only.push(
                "/proc/self/smaps_rollup is unreadable, so Pss cannot be measured on this \
                 host at all"
                    .to_owned(),
            );
        }

        if debug_build {
            universal.push(
                "built with debug assertions on; this measures the debug build, \
                 not the shipped one"
                    .to_owned(),
            );
        }
        reasons.extend(universal.iter().cloned());

        // Falling back to `logical` silently would make the SMT reason vacuously false
        // and check the core budget against siblings: a false PASS. The fallback is stated
        // and fails both verdicts.
        let physical = detected_physical.unwrap_or(logical);
        let unknown_cores = detected_physical.is_none();
        if unknown_cores {
            reasons.push(format!(
                "the physical core count is unknown on this host: /proc/cpuinfo publishes \
                 no `physical id`/`core id` pairs (aarch64 never does, and many container \
                 configurations do not), leaving {logical} logical CPUs as the only \
                 denominator — and that one counts SMT siblings"
            ));
        }

        // `saturating_add`: `--consumers` of `usize::MAX` would wrap to 0 in release and print the budget as PASS.
        let needed = consumers.saturating_add(1);
        // Not in `reasons`: this governs the N-way rows only.
        let core_reason = if unknown_cores {
            Some(format!(
                "the physical core count is unknown on this host, so a {consumers}-consumer \
                 budget cannot be checked against anything ({logical} logical CPUs counts \
                 SMT siblings and would answer the wrong question)"
            ))
        } else if physical < needed {
            Some(format!(
                "{physical} physical cores for {consumers} consumers plus a publisher \
                 ({needed} needed); above the core count the rows measure the scheduler"
            ))
        } else {
            None
        };
        if logical > physical {
            reasons.push(format!(
                "SMT is on ({logical} logical CPUs over {physical} physical cores); \
                 sibling threads share execution resources, so a per-thread number \
                 depends on what the sibling is doing"
            ));
        }

        // Load does not divide out of a cross-engine quotient (only one arm locks) and
        // fails in our favour, so `busy` reaches the ratio axis.
        if busy > crate::mp::QUIET_ENOUGH {
            timing_and_ratio.push(format!(
                "machine is {:.0}% busy before the run starts (threshold {:.0}%); a \
                 cross-engine ratio does not divide this out, because only one of the \
                 two arms takes a lock",
                busy * 100.0,
                crate::mp::QUIET_ENOUGH * 100.0
            ));
        }

        match governors {
            Some(g) if g.iter().all(|s| s == "performance") => {}
            Some(g) => reasons.push(format!(
                "CPU frequency governor is {} on at least one CPU, not `performance`; \
                 frequency scaling moves latency by more than most of the gates",
                g.first().map_or("unknown", String::as_str)
            )),
            None => reasons.push(
                "CPU frequency governor is unreadable (no cpufreq sysfs), so frequency \
                 scaling cannot be ruled out"
                    .to_owned(),
            ),
        }

        // The unions.
        reasons.extend(timing_and_ratio.iter().cloned());
        let ratio_reasons: Vec<String> = universal
            .iter()
            .chain(timing_and_ratio.iter())
            .cloned()
            .collect();
        let memory_reasons: Vec<String> = universal
            .iter()
            .chain(memory_only.iter())
            .cloned()
            .collect();

        Fitness {
            fair_for_timing: reasons.is_empty(),
            fair_for_ratios: ratio_reasons.is_empty(),
            fair_for_memory: memory_reasons.is_empty(),
            enough_cores: core_reason.is_none(),
            forced: std::env::var_os("TF_TREE_BENCH_FORCE").is_some(),
            ratio_reasons,
            memory_reasons,
            reasons,
            core_reason,
            consumers,
            busy_fraction: busy,
            physical_cores: physical,
            physical_cores_known: !unknown_cores,
            logical_cpus: logical,
        }
    }

    /// The status a single-threaded, in-process timing row should carry; the core
    /// budget is not consulted.
    #[must_use]
    pub fn timing_status(&self) -> Status {
        Fitness::status_from(self.fair_for_timing, self.forced)
    }

    /// The one place a [`Sensitivity`] is mapped to a verdict.
    ///
    /// Returns `(is_fair, how a refusal describes the row, why it failed)`.
    /// `Report::validate` and [`Row::status_on`] call it.
    ///
    /// `status_on` and [`Fitness::memory_status`] currently have no caller; kept for
    /// the memory rows, so the single-match property is enforced only for the axes
    /// `validate` exercises.
    #[must_use]
    pub fn axis(&self, sensitivity: Sensitivity) -> (bool, &'static str, String) {
        match sensitivity {
            Sensitivity::HostIndependent => (true, "host independent", String::new()),
            Sensitivity::AbsoluteTiming => {
                (self.fair_for_timing, "timing sensitive", self.reason_line())
            }
            Sensitivity::Ratio => (
                self.fair_for_ratios,
                "an interleaved ratio",
                self.ratio_reason_line(),
            ),
            Sensitivity::Memory => (
                self.fair_for_memory,
                "a memory figure",
                self.memory_reason_line(),
            ),
        }
    }

    /// The status an interleaved two-engine ratio row should carry.
    #[must_use]
    pub fn ratio_status(&self) -> Status {
        Fitness::status_from(self.fair_for_ratios, self.forced)
    }

    /// The status a resident/proportional memory row should carry.
    #[must_use]
    pub fn memory_status(&self) -> Status {
        Fitness::status_from(self.fair_for_memory, self.forced)
    }

    /// The shared `fair → Measured, forced → Indicative, else Unavailable` rule.
    fn status_from(fair: bool, forced: bool) -> Status {
        if fair {
            Status::Measured
        } else if forced {
            Status::Indicative
        } else {
            Status::Unavailable
        }
    }

    /// The reasons, joined for a single-line report field.
    #[must_use]
    pub fn reason_line(&self) -> String {
        if self.reasons.is_empty() {
            "host passed every fitness check".to_owned()
        } else {
            self.reasons.join("; ")
        }
    }

    /// [`Fitness::reason_line`] for a ratio row.
    #[must_use]
    pub fn ratio_reason_line(&self) -> String {
        if self.ratio_reasons.is_empty() {
            "host can measure an interleaved ratio".to_owned()
        } else {
            self.ratio_reasons.join("; ")
        }
    }

    /// [`Fitness::reason_line`] for a memory row.
    #[must_use]
    pub fn memory_reason_line(&self) -> String {
        if self.memory_reasons.is_empty() {
            "host can measure resident memory".to_owned()
        } else {
            self.memory_reasons.join("; ")
        }
    }
}

/// A `key = value` fact about the environment the report was produced in.
#[derive(Debug, Clone)]
pub struct Fact {
    /// Stable key; part of the JSON schema.
    pub key: &'static str,
    /// The measured value, or an explicit "unknown"/"none" spelling.
    pub value: String,
}

/// Everything §9.3 requires the report to state about where it came from.
#[derive(Debug, Clone)]
pub struct Provenance {
    /// Ordered facts; order is the JSON and HTML order.
    pub facts: Vec<Fact>,
}

impl Provenance {
    /// Collect the environment description, measuring rather than assuming.
    /// Collect the environment description, measuring rather than assuming. Missing
    /// ROS 2 facts are recorded as `none (…)`, not omitted.
    #[must_use]
    pub fn collect() -> Provenance {
        let mut f = Vec::new();
        let mut push = |key: &'static str, value: String| f.push(Fact { key, value });

        push("generated_utc", iso8601_utc(SystemTime::now()));
        push("schema", SCHEMA.to_owned());
        push("git_commit", git("rev-parse HEAD").unwrap_or_else(unknown));
        push(
            "git_dirty",
            git("status --porcelain").map_or_else(unknown, |s| {
                if s.trim().is_empty() {
                    "false".to_owned()
                } else {
                    "true".to_owned()
                }
            }),
        );
        push(
            "rustc",
            capture("rustc", &["--version"]).unwrap_or_else(unknown),
        );
        // The profile directory as built (see `build.rs`), not `cfg!(debug_assertions)`:
        // under `--profile embedder` debug assertions are off, so the guess said `release`
        // and two different questions carried identical provenance
        // (`baseline::PORTABLE_FACTS`, `runstore::BUILD_CRITICAL_FACTS` compare this key).
        push("build_profile", crate::embed::PROFILE_DIR.to_owned());
        // `build_lto` says what the profile means: thin LTO inlines across a crate boundary.
        push("build_lto", build_lto());
        push("target", std::env::consts::ARCH.to_owned());
        push("counters_feature", cfg!(feature = "counters").to_string());
        push("shm_feature", cfg!(feature = "shm").to_string());
        push("tf2_feature", cfg!(feature = "tf2").to_string());
        push(
            "format_version",
            tf_tree::arena_format_version().to_string(),
        );
        push(
            "layout_hash",
            format!("{:#010X}", tf_tree::arena_layout_hash()),
        );
        push("interp_policy", "LerpSlerp (tf2's policy)".to_owned());
        push("cpu_model", cpu_model().unwrap_or_else(unknown));
        // `unknown`, not backfilled from `available_parallelism`: a number here reads as a measured fact.
        push(
            "physical_cores",
            physical_cores().map_or_else(unknown, |n| n.to_string()),
        );
        push(
            "logical_cpus",
            std::thread::available_parallelism()
                .map_or(1, std::num::NonZeroUsize::get)
                .to_string(),
        );
        push(
            "cpu_governor",
            governors().map_or_else(unknown, |g| dedup_join(&g)),
        );
        push(
            "kernel",
            read_trim("/proc/sys/kernel/osrelease").unwrap_or_else(unknown),
        );
        // Two THP knobs: `enabled` governs anonymous mappings, but a live arena is a
        // sealed `memfd` `MAP_SHARED` mapping governed by `shmem_enabled` (see
        // `crates/tf_tree_cli/src/hostfacts.rs`, `TFT016`). Raw text, not parsed: no second
        // copy of `hostfacts`'s parsers.
        push(
            "transparent_hugepage",
            read_trim("/sys/kernel/mm/transparent_hugepage/enabled").unwrap_or_else(unknown),
        );
        push(
            "transparent_hugepage_shmem",
            read_trim("/sys/kernel/mm/transparent_hugepage/shmem_enabled").unwrap_or_else(unknown),
        );
        push(
            "perf_event_paranoid",
            read_trim("/proc/sys/kernel/perf_event_paranoid").unwrap_or_else(unknown),
        );
        push(
            "load_average",
            read_trim("/proc/loadavg").unwrap_or_else(unknown),
        );
        push(
            "container",
            if std::path::Path::new("/.dockerenv").exists() {
                "yes (/.dockerenv present)".to_owned()
            } else {
                "no".to_owned()
            },
        );
        push(
            "ros_distro",
            std::env::var("ROS_DISTRO")
                .unwrap_or_else(|_| "none (no ROS 2 in this run)".to_owned()),
        );
        push(
            "tf2_version",
            std::env::var("ROS_DISTRO").map_or_else(
                |_| "none — the tf2 columns are UNAVAILABLE, not zero".to_owned(),
                |d| format!("the tf2 shipped with ROS 2 {d}"),
            ),
        );
        push(
            "rmw_implementation",
            std::env::var("RMW_IMPLEMENTATION").unwrap_or_else(|_| {
                "none — no middleware is in any measurement here; both engines are \
                 driven in-process from the same loop"
                    .to_owned()
            }),
        );
        push(
            "dds_qos",
            "not applicable — no DDS in this configuration (see rmw_implementation)".to_owned(),
        );
        push(
            "executor_config",
            "not applicable — no rclcpp executor; the harness drives both engines directly"
                .to_owned(),
        );
        Provenance { facts: f }
    }

    /// Look a fact up by key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.facts
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.value.as_str())
    }
}

/// The whole artifact.
#[derive(Debug, Clone)]
pub struct Report {
    /// Environment description (§9.3).
    pub provenance: Provenance,
    /// The build facts [`Ground`] is decided against.
    pub build: Build,
    /// Host fitness verdict, and why.
    pub fitness: Fitness,
    /// Seconds of warm-up discarded before any timing row (§9.3 requires it stated).
    pub warmup_discarded_s: f64,
    /// The §9.2 rows.
    pub rows: Vec<Row>,
    /// The §9.3 "where we are worse" entries.
    pub worse: Vec<Worse>,
}

impl Report {
    /// Enforce §9.3 against the assembled report.
    ///
    /// | §9.3 bullet | Held by |
    /// |---|---|
    /// | 1 — QoS, executor, DDS vendor recorded | [`REQUIRED_FACTS`]' `dds_qos` / `executor_config` / `rmw_implementation`, **recorded only** |
    /// | 2 — warm-up discarded, N stated | the warm-up rule below |
    /// | 3 — `tf2` version, distro, RMW, kernel, CPU, THP | [`REQUIRED_FACTS`] |
    /// | 4 — where `tf_tree` is worse | [`REQUIRED_WORSE`] and the `worse` loop |
    /// | 5 — harness published in the repository | the reproduce-command rule, **partially** |
    ///
    /// Bullet 1's *identical* cannot be checked: this process stands up neither stack.
    /// Bullet 2's N is [`Options::warmup`], which reaches only
    /// `measure_lookup_latency`; for `embedding_cross_crate` and `lookup_ratio_vs_tf2`
    /// it is a report-level declaration. Bullet 5's mechanical half is that every row
    /// names a command;
    /// `tests::every_command_the_report_names_is_a_command_that_exists` checks it
    /// exists.
    ///
    /// # Errors
    ///
    /// One string per violation; the caller fails rather than emit the report.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut bad = Vec::new();

        // §9.3 bullets 1 and 3: a closed list, like `REQUIRED_ROWS`.
        for key in REQUIRED_FACTS {
            match self.provenance.get(key) {
                None => bad.push(format!(
                    "PHASE5 §9.3 requires the report to state `{key}`, and the provenance \
                     header has no such fact"
                )),
                Some(v) if v.trim().is_empty() => bad.push(format!(
                    "PHASE5 §9.3's `{key}` is present but empty. `Provenance::collect` \
                     writes an explicit `unknown`/`none (...)` where a fact cannot be \
                     read, precisely so a reader can tell a missing measurement from a \
                     missing line of code"
                )),
                Some(_) => {}
            }
        }

        // §9.3 bullet 2: timed rows that print numbers need a positive N. `Memory` and
        // `HostIndependent` have no cold path to discard.
        if !self.warmup_discarded_s.is_finite() || self.warmup_discarded_s < 0.0 {
            bad.push(format!(
                "PHASE5 §9.3 requires the discarded warm-up to be stated; this report \
                 states {}",
                self.warmup_discarded_s
            ));
        } else if self.warmup_discarded_s == 0.0 {
            for r in &self.rows {
                let timed = matches!(
                    r.sensitivity,
                    Sensitivity::AbsoluteTiming | Sensitivity::Ratio
                );
                if timed && r.status != Status::Unavailable {
                    bad.push(format!(
                        "row `{}` is `{}` and reports a timed measurement, but the report \
                         states a discarded warm-up of zero seconds. PHASE5 §9.3 requires \
                         both stacks to be warmed and the discarded window stated",
                        r.id,
                        r.status.as_str()
                    ));
                }
            }
        }

        for id in REQUIRED_ROWS {
            match self.rows.iter().filter(|r| r.id == *id).count() {
                1 => {}
                0 => bad.push(format!("PHASE5 §9.2 row `{id}` is missing from the report")),
                n => bad.push(format!("row `{id}` appears {n} times")),
            }
        }
        for id in REQUIRED_WORSE {
            if !self.worse.iter().any(|w| w.id == *id) {
                bad.push(format!(
                    "PHASE5 §9.3 requires a `where we are worse` entry for `{id}`"
                ));
            }
        }

        for r in &self.rows {
            // Both `measured` and `indicative` rows print numbers and so need a direction.
            if r.status != Status::Unavailable
                && !r
                    .tf_tree
                    .iter()
                    .chain(&r.tf2)
                    .any(|m| m.drift != Drift::Informational)
            {
                bad.push(format!(
                    "row `{}` prints numbers but every one of them is informational, so \
                     nothing in it can ever be gated — on the host that cuts the baseline \
                     as much as on this one. Give at least one metric a direction with \
                     `Metric::lower_is_better`/`higher_is_better`, or say plainly why this \
                     row is context rather than a claim",
                    r.id
                ));
            }
            for m in r.tf_tree.iter().chain(&r.tf2) {
                if m.drift != Drift::Informational
                    && !(m.tolerance.is_finite() && m.tolerance >= 0.0)
                {
                    bad.push(format!(
                        "row `{}` metric `{}` is directional with tolerance {} — a \
                         negative or non-finite tolerance makes the gate either always \
                         or never fire",
                        r.id, m.key, m.tolerance
                    ));
                }
            }
            // §9.3 bullet 5's mechanical half, for every status.
            if r.reproduce.trim().is_empty() {
                bad.push(format!(
                    "row `{}` is `{}` and names no command that re-derives it. PHASE5 \
                     §9.3 requires the harness to be published in this repository, and a \
                     row that names nothing in it cannot be reproduced from it",
                    r.id,
                    r.status.as_str()
                ));
            }
            // The grounds under this row's reason, re-derived (see [`Ground`]).
            if r.status == Status::Unavailable && r.grounds.is_empty() {
                bad.push(format!(
                    "row `{}` is `unavailable` with a reason resting on no stated ground. \
                     A reason nothing re-derives is the prose that went stale four times \
                     in this file; give it a `Ground`, or say in the `Ground` list which \
                     of the three undecidable ones it is",
                    r.id
                ));
            }
            if r.status != Status::Unavailable && !r.grounds.is_empty() {
                bad.push(format!(
                    "row `{}` is `{}` and still carries the ground(s) that explained its \
                     absence: {}. A row that prints numbers has nothing to excuse, and a \
                     ground left behind is one nothing will ever re-check",
                    r.id,
                    r.status.as_str(),
                    r.grounds
                        .iter()
                        .map(|g| g.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            for g in &r.grounds {
                if g.holds(self.build, &self.fitness, r) == Some(false) {
                    bad.push(format!(
                        "row `{}` is `unavailable` on the ground `{}`, and that ground does \
                         not hold in this build on this host — it is decided by {}. The \
                         reason is stale: it explains the gap with something that is no \
                         longer true. Reason was: {}",
                        r.id,
                        g.as_str(),
                        g.decided_by(),
                        r.reason
                    ));
                }
            }
            match r.status {
                Status::Measured => {
                    if r.tf_tree.is_empty() && r.tf2.is_empty() {
                        bad.push(format!("row `{}` is `measured` with no numbers", r.id));
                    }
                    // Each sensitivity is checked against the axis it rests on.
                    let (fair, axis, why) = self.fitness.axis(r.sensitivity);
                    if !fair {
                        bad.push(format!(
                            "row `{}` is {axis} and claims `measured`, but the host \
                             failed the fitness probe: {why}",
                            r.id,
                        ));
                    }
                    // An N-way row on a host with fewer cores than consumers measures the
                    // scheduler, except a memory row: Pss is decided by page tables, not by who runs
                    // (§12 gate 4).
                    let core_budget_applies =
                        r.needs_n_cores && r.sensitivity != Sensitivity::Memory;
                    if core_budget_applies && !self.fitness.enough_cores {
                        bad.push(format!(
                            "row `{}` runs {} consumers and claims `measured`, but {}",
                            r.id,
                            self.fitness.consumers,
                            self.fitness
                                .core_reason
                                .as_deref()
                                .unwrap_or("the core budget check did not pass")
                        ));
                    }
                }
                Status::Indicative => {
                    // Per-axis, as for `measured`.
                    let (fair, _, _) = self.fitness.axis(r.sensitivity);
                    if fair {
                        bad.push(format!(
                            "row `{}` is `indicative` on a host that passed the fitness probe; \
                             an indicative label there hides a usable number",
                            r.id
                        ));
                    }
                    if !self.fitness.forced {
                        bad.push(format!(
                            "row `{}` is `indicative` without TF_TREE_BENCH_FORCE=1",
                            r.id
                        ));
                    }
                    if r.reason.trim().is_empty() {
                        bad.push(format!("row `{}` is `indicative` with no reason", r.id));
                    }
                }
                Status::Unavailable => {
                    if r.reason.trim().is_empty() {
                        bad.push(format!("row `{}` is `unavailable` with no reason", r.id));
                    }
                    if !r.tf_tree.is_empty() || !r.tf2.is_empty() {
                        bad.push(format!(
                            "row `{}` is `unavailable` but carries numbers",
                            r.id
                        ));
                    }
                }
            }
        }

        for w in &self.worse {
            if w.statement.trim().is_empty() {
                bad.push(format!("`worse` entry `{}` states nothing", w.id));
            }
            match (
                w.metrics.is_empty(),
                w.metrics_absent_because.as_deref().map(str::trim),
            ) {
                (true, None | Some("")) => bad.push(format!(
                    "`worse` entry `{}` carries no metrics and no `metrics_absent_because`. §9.3's section is the one a reader is entitled to quote against us, and an entry with an empty metric list reads as an oversight whether or not it is one. Either give it a number, or say — in the entry — why the cost has none",
                    w.id
                )),
                (false, Some(_)) => bad.push(format!(
                    "`worse` entry `{}` carries {} metric(s) *and* a reason they are absent. One of the two is wrong, and a reader has no way to tell which",
                    w.id,
                    w.metrics.len()
                )),
                _ => {}
            }
            // The rule the rows carry, applied to `worse` entries: `arena_memory_floor`
            // printed five numbers with no direction, so `crate::baseline` had nothing to
            // compare and reverting `docs/decisions/0021` step 2 still printed
            // `PASS - 1 directional metric held`.
            //
            // Scoped to a host that could have produced the number: `worse_entries` withholds
            // the Pss metrics when the memory axis fails, and refusing the whole report there
            // would turn "cannot read `smaps_rollup`" into "no artifact".
            if self.fitness.fair_for_memory
                && w.metrics_withheld.is_empty()
                && !w.metrics.is_empty()
                && !w.metrics.iter().any(|m| m.drift != Drift::Informational)
            {
                bad.push(format!(
                    "`worse` entry `{}` prints numbers and every one of them is \
                     informational, so nothing in it can ever be gated. §9.3's section is \
                     the one a reader is entitled to quote against us, and a cost that \
                     cannot regress is not a cost anybody is holding us to. Give at least \
                     one metric a direction with \
                     `Metric::lower_is_better`/`higher_is_better`",
                    w.id
                ));
            }
            for m in &w.metrics {
                if m.drift != Drift::Informational
                    && !(m.tolerance.is_finite() && m.tolerance >= 0.0)
                {
                    bad.push(format!(
                        "`worse` entry `{}` metric `{}` is directional with tolerance {} \
                         — a negative or non-finite tolerance makes the gate either always \
                         or never fire",
                        w.id, m.key, m.tolerance
                    ));
                }
            }
        }

        if bad.is_empty() {
            Ok(())
        } else {
            Err(bad)
        }
    }

    /// `results.json` — stable schema, CI-diffable.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(8192);
        s.push_str("{\n");
        let _ = writeln!(s, "  \"schema\": {},", jstr(SCHEMA));
        s.push_str("  \"provenance\": {\n");
        for (i, f) in self.provenance.facts.iter().enumerate() {
            let comma = if i + 1 == self.provenance.facts.len() {
                ""
            } else {
                ","
            };
            let _ = writeln!(s, "    {}: {}{comma}", jstr(f.key), jstr(&f.value));
        }
        s.push_str("  },\n");

        s.push_str("  \"host_fitness\": {\n");
        let _ = writeln!(
            s,
            "    \"fair_for_timing\": {},",
            self.fitness.fair_for_timing
        );
        // The other axes are published too, so the split is visible in the artifact.
        let _ = writeln!(
            s,
            "    \"fair_for_ratios\": {},",
            self.fitness.fair_for_ratios
        );
        let _ = writeln!(
            s,
            "    \"fair_for_memory\": {},",
            self.fitness.fair_for_memory
        );
        let _ = writeln!(s, "    \"enough_cores\": {},", self.fitness.enough_cores);
        let _ = writeln!(
            s,
            "    \"core_reason\": {},",
            self.fitness
                .core_reason
                .as_deref()
                .map_or_else(|| "null".to_owned(), jstr)
        );
        let _ = writeln!(s, "    \"forced\": {},", self.fitness.forced);
        let _ = writeln!(s, "    \"consumers\": {},", self.fitness.consumers);
        let _ = writeln!(
            s,
            "    \"busy_fraction\": {},",
            jnum(self.fitness.busy_fraction)
        );
        let _ = writeln!(
            s,
            "    \"physical_cores\": {},",
            self.fitness.physical_cores
        );
        let _ = writeln!(s, "    \"logical_cpus\": {},", self.fitness.logical_cpus);
        let _ = writeln!(
            s,
            "    \"warmup_discarded_s\": {},",
            jnum(self.warmup_discarded_s)
        );
        s.push_str("    \"reasons\": [");
        for (i, r) in self.fitness.reasons.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&jstr(r));
        }
        s.push_str("]\n  },\n");

        s.push_str("  \"rows\": [\n");
        for (i, r) in self.rows.iter().enumerate() {
            s.push_str("    {\n");
            let _ = writeln!(s, "      \"id\": {},", jstr(r.id));
            let _ = writeln!(s, "      \"title\": {},", jstr(r.title));
            let _ = writeln!(s, "      \"note\": {},", jstr(&r.note));
            let _ = writeln!(s, "      \"timing_sensitive\": {},", r.timing_sensitive());
            let _ = writeln!(s, "      \"needs_n_cores\": {},", r.needs_n_cores);
            let _ = writeln!(s, "      \"status\": {},", jstr(r.status.as_str()));
            let _ = writeln!(s, "      \"reason\": {},", jstr(&r.reason));
            let _ = writeln!(s, "      \"reproduce\": {},", jstr(r.reproduce));
            let _ = writeln!(s, "      \"tf_tree\": {},", jmetrics(&r.tf_tree));
            let _ = writeln!(s, "      \"tf2\": {}", jmetrics(&r.tf2));
            s.push_str(if i + 1 == self.rows.len() {
                "    }\n"
            } else {
                "    },\n"
            });
        }
        s.push_str("  ],\n");

        s.push_str("  \"where_we_are_worse\": [\n");
        for (i, w) in self.worse.iter().enumerate() {
            s.push_str("    {\n");
            let _ = writeln!(s, "      \"id\": {},", jstr(w.id));
            let _ = writeln!(s, "      \"topic\": {},", jstr(w.topic));
            let _ = writeln!(s, "      \"statement\": {},", jstr(&w.statement));
            let _ = writeln!(s, "      \"metrics\": {},", jmetrics(&w.metrics));
            match &w.metrics_absent_because {
                Some(why) => {
                    let _ = writeln!(s, "      \"metrics_absent_because\": {}", jstr(why));
                }
                None => {
                    let _ = writeln!(s, "      \"metrics_absent_because\": null");
                }
            }
            s.push_str(if i + 1 == self.worse.len() {
                "    }\n"
            } else {
                "    },\n"
            });
        }
        s.push_str("  ]\n}\n");
        s
    }

    /// `index.html` — self-contained, no external assets, no script. The §9.3
    /// "where we are worse" entries render **inside the results table** ("not in a footnote").
    #[must_use]
    pub fn to_html(&self) -> String {
        let mut s = String::with_capacity(16384);
        s.push_str(
            "<meta charset=\"utf-8\">\n<title>tf_tree benchmark report</title>\n\
             <style>\n\
             body{font:15px/1.5 system-ui,sans-serif;margin:2rem auto;max-width:70rem;padding:0 1rem}\n\
             table{border-collapse:collapse;width:100%;margin:1rem 0}\n\
             th,td{border:1px solid #b0b6bd;padding:.4rem .6rem;text-align:left;vertical-align:top}\n\
             th{background:#eef1f4}\n\
             tr.section th{background:#dfe4ea;font-size:1.05rem}\n\
             .measured{color:#0a6b2a;font-weight:600}\n\
             .indicative{color:#a35a00;font-weight:600}\n\
             .unavailable{color:#8a1c1c;font-weight:600}\n\
             .reason{color:#333;font-size:.9em}\n\
             code{background:#f2f4f6;padding:.05rem .25rem}\n\
             .banner{border:2px solid #8a1c1c;padding:.6rem 1rem;background:#fff2f2}\n\
             </style>\n",
        );
        s.push_str("<h1>tf_tree benchmark report</h1>\n");

        if !self.fitness.fair_for_timing {
            s.push_str("<div class=\"banner\"><strong>This host cannot measure the timing rows fairly.</strong><ul>\n");
            for r in &self.fitness.reasons {
                let _ = writeln!(s, "<li>{}</li>", esc_html(r));
            }
            s.push_str("</ul>");
            if self.fitness.forced {
                s.push_str(
                    "<p><strong>TF_TREE_BENCH_FORCE=1 was set</strong>, so timing rows below \
                     are marked <span class=\"indicative\">indicative</span>. \
                     An indicative number is not a claim and must not be quoted as one.</p>",
                );
            } else {
                s.push_str(
                    "<p>Timing rows are therefore reported as \
                     <span class=\"unavailable\">unavailable</span> with the command that \
                     produces them on a host that qualifies.</p>",
                );
            }
            s.push_str("</div>\n");
        }

        s.push_str("<h2>Results</h2>\n<table>\n");
        s.push_str("<tr><th>Measurement</th><th>tf_tree</th><th>tf2</th><th>Status</th></tr>\n");
        for r in &self.rows {
            let _ = writeln!(
                s,
                "<tr><td><strong>{}</strong><br><span class=\"reason\">{}</span></td>\
                 <td>{}</td><td>{}</td>\
                 <td class=\"{}\">{}</td></tr>",
                esc_html(r.title),
                esc_html(&r.note),
                cell_html(&r.tf_tree),
                cell_html(&r.tf2),
                r.status.as_str(),
                r.status.as_str().to_uppercase(),
            );
            if r.status != Status::Measured {
                let _ = writeln!(
                    s,
                    "<tr><td colspan=\"4\" class=\"reason\">why: {} &middot; \
                     reproduce: <code>{}</code></td></tr>",
                    esc_html(&r.reason),
                    esc_html(r.reproduce)
                );
            }
        }
        // §9.3: in the same table, not in a footnote.
        s.push_str(
            "<tr class=\"section\"><th colspan=\"4\">Where tf_tree is worse</th></tr>\n\
             <tr><th>Cost</th><th colspan=\"3\">What it means for an operator</th></tr>\n",
        );
        for w in &self.worse {
            let _ = writeln!(
                s,
                "<tr><td><strong>{}</strong>{}</td><td colspan=\"3\">{}</td></tr>",
                esc_html(w.topic),
                if w.metrics.is_empty() {
                    String::new()
                } else {
                    format!("<br>{}", cell_html(&w.metrics))
                },
                // The reason renders beside the place the number would have been.
                match &w.metrics_absent_because {
                    None => esc_html(&w.statement),
                    Some(why) => format!(
                        "{}<br><em>No metric here: {}</em>",
                        esc_html(&w.statement),
                        esc_html(why)
                    ),
                }
            );
        }
        s.push_str("</table>\n");

        s.push_str("<h2>Provenance</h2>\n<table>\n");
        for f in &self.provenance.facts {
            let _ = writeln!(
                s,
                "<tr><th>{}</th><td>{}</td></tr>",
                esc_html(f.key),
                esc_html(&f.value)
            );
        }
        let _ = writeln!(
            s,
            "<tr><th>warmup_discarded_s</th><td>{}</td></tr>",
            fmt_value(self.warmup_discarded_s)
        );
        s.push_str("</table>\n");
        let _ = write!(
            s,
            "<h2>Reproducing this</h2>\n<p><code>{REPRODUCE_RECIPE}</code> \
             regenerates every file in this directory. \
             Rows marked unavailable name the command that measures them on a host that can; \
             the harness for all of them is in this repository \
             (<code>crates/tf_tree_bench/</code>), per PHASE5 §9.3's \
             &ldquo;no private benchmark&rdquo;.</p>\n"
        );
        s
    }
}

/// What the artifact was asked to produce.
#[derive(Debug, Clone)]
pub struct Options {
    /// Consumer count the comparison is scoped to (§9.1's `--consumers`).
    pub consumers: usize,
    // No `--duration` field: every row it would govern is an N-way row, unavailable
    // here, so the binary rejects the flag rather than store a knob nothing reads.
    /// Warm-up discarded before any timing row is recorded (§9.3).
    pub warmup: Duration,
    /// Lookup samples for the latency row.
    pub lookup_samples: usize,
    /// Random queries for the differential row.
    pub differential_queries: usize,
    /// Directory holding the two `embed_cost` runs (§9.2's last row).
    /// [`None`] is the ordinary case: the row compares two profiles, so one build
    /// cannot measure it. `just embed-cost` produces the pair; without it the row is
    /// [`Status::Unavailable`].
    pub embed_cost: Option<std::path::PathBuf>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            consumers: 16,
            warmup: Duration::from_secs(2),
            lookup_samples: 200_000,
            differential_queries: 50_000,
            embed_cost: None,
        }
    }
}

/// Why the two `.tft` rows are unavailable, for a given `attempt`.
///
/// The reason is derived from a `cfg`: the frozen backend needs
/// `all(feature = "shm", target_os = "linux")` and `just bench-report` builds
/// without it, so `Tree::open_frozen` is not compiled in. The other branch carries
/// `Ground::MeasuredElsewhere`, which `Report::validate` cannot decide; only the
/// recipe's *name* is checked
/// (`every_command_the_report_names_is_a_command_that_exists`).
fn frozen_row_reason(attempt: &str, recipe: &str) -> String {
    if cfg!(all(feature = "shm", target_os = "linux")) {
        format!(
            "{attempt} is not done inside this process: `bench_report` is one process and \
             maps no .tft. It is done elsewhere, on an index frozen at §12 gate 2's 233 MB \
             scale rather than at a fixture's. Run it with `{recipe}`"
        )
    } else {
        format!(
            "{attempt} needs `tf_tree`'s frozen backend, which is \
             `#[cfg(all(feature = \"shm\", target_os = \"linux\"))]` and is therefore not \
             compiled into this binary — `just bench-report` builds without `--features shm`. \
             There is no `Tree::open_frozen` here to call"
        )
    }
}

/// Build the whole §9 artifact for this host. Every row is measured here or
/// [`Status::Unavailable`] with a reason and the command that measures it elsewhere;
/// nothing is estimated. The caller runs [`Report::validate`] before writing.
///
/// # Errors
///
/// Only a *measurement* failure propagates; an unmeasurable row is a row.
pub fn assemble(opts: &Options) -> Result<Report> {
    assemble_on(
        opts,
        Fitness::probe(opts.consumers),
        Build::current(),
        std::env::var("ROS_DISTRO").is_ok(),
    )
}

/// [`assemble`] with the host verdict, build and ROS environment handed in.
///
/// The seam exists so `tests::a_host_with_no_obstacle_still_grounds_every_n_way_row`
/// can reach the all-clear host (see the `host_grounds` comment). Not `pub`: `build`
/// must agree with the `cfg!`s the binary was compiled under.
///
/// # Errors
///
/// As [`assemble`].
fn assemble_on(opts: &Options, fitness: Fitness, build: Build, ros_env: bool) -> Result<Report> {
    let n = opts.consumers;

    let no_ros = !ros_env && !build.tf2_linked;
    let ros_reason = if no_ros {
        "there is no ROS 2 in this build or environment, so the tf2 column cannot be \
         measured at all"
    } else {
        ""
    };
    // The reason an N-way, cross-engine row is missing, and the host obstacles under
    // it; each `parts.push` has one `host_grounds.push` beside it.
    //
    // `host_grounds` can legitimately be empty, so every row must add a ground of its
    // own: `bench_report` is one process and measures no N-way row on any host, so that
    // leads the reason unconditionally. `MeasuredElsewhere` where the named recipe takes
    // the number, `NoInstrument` where nothing does (`publish_to_visible`); seeding
    // `MeasuredElsewhere` here would over-claim on that row.
    let mut host_grounds: Vec<Ground> = Vec::new();
    let host_reason = {
        // Only the process-count half is unconditional. The second-engine half fires
        // only when `no_ros`, which implies `!build.tf2_linked`; under `--features tf2` this
        // binary calls `tf2::BufferCore` in-process.
        let mut parts: Vec<String> = vec![format!(
            "this tool is a single process: it stands up none of the {n} consumers this \
             row compares, so the row is not measured here on any host — the command \
             below is what measures it"
        )];
        if !ros_reason.is_empty() {
            parts.push(format!("this build links no second engine: {ros_reason}"));
            host_grounds.push(Ground::Tf2NotLinked);
        }
        if let Some(c) = fitness.core_reason.as_deref() {
            parts.push(format!("the host also has {c}"));
            host_grounds.push(Ground::HostCores);
        }
        if !fitness.fair_for_timing {
            parts.push(fitness.reason_line());
            host_grounds.push(Ground::HostFitness);
        }
        parts.join("; ")
    };

    // The rows whose recipe really takes the number; non-empty by construction.
    let n_way_grounds: Vec<Ground> = std::iter::once(Ground::MeasuredElsewhere)
        .chain(host_grounds.iter().copied())
        .collect();

    let mut rows = Vec::new();

    rows.push(
        Row::unavailable(
            "cpu_per_consumer",
            "CPU per consumer at steady state (%CPU)",
            "Both stacks, N consumers plus one publisher, steady state.",
            Sensitivity::AbsoluteTiming,
            host_reason.clone(),
            "just mp-bench (tf_tree) / just mp-bench-tf2 (both, in the ROS container)",
        )
        .n_way()
        .on(&n_way_grounds),
    );

    // True in both builds: `bench_report` is one process and has no consumers to weigh.
    let rss_reason = {
        let mut r = "this row sums Pss across N consumer processes, and `bench_report` is \
             one process — it stands up no consumers and has none to weigh. `just \
             mp-bench-tf2` runs both stacks as processes and prints Pss for each; `just \
             dds-bench` does the same over a real DDS. A one-sided memory row is exactly \
             the thumb on the scale §9.3 warns about, so it is a gap rather than a \
             half-filled row"
            .to_owned();
        if !build.tf2_linked {
            r.push_str(
                ". The tf2 column additionally needs a ROS 2 install this build does not \
                 have: `tf_tree_bench` was compiled without `--features tf2`",
            );
        }
        r
    };
    let mut rss_grounds = vec![Ground::MeasuredElsewhere];
    if !build.tf2_linked {
        rss_grounds.push(Ground::Tf2NotLinked);
    }
    rows.push(
        Row::unavailable(
            "total_rss_n_consumers",
            "Total RSS across N consumers (MB)",
            "Both stacks, summed Pss from /proc/*/smaps_rollup. Memory is exact even on a \
         loaded machine, so this row's gap is the process count, not the host.",
            Sensitivity::Memory,
            rss_reason,
            "just mp-bench-tf2",
        )
        .n_way()
        .on(&rss_grounds),
    );

    // The one timing row this tool measures itself: single-threaded, so only the
    // host stands between it and a number. Its reason is `fitness.reason_line()`, not
    // `host_reason`, which would state a false why (17 cores, ROS 2).
    let mut lookup = Row::unavailable(
        "lookup_latency",
        "Lookup latency, depth 3, hot path (p50, p99, p99.9)",
        LOOKUP_NOTE,
        Sensitivity::AbsoluteTiming,
        format!(
            "this row is single-threaded and in-process, so the only thing between it \
             and a number is the host, and the host failed the fitness probe: {}",
            fitness.reason_line()
        ),
        "cargo bench -p tf_tree_bench --bench lookup (or this tool on a quiet, \
         non-SMT, performance-governor host)",
    )
    .on(&[Ground::HostFitness]);
    match fitness.timing_status() {
        Status::Unavailable => {}
        status => {
            lookup.tf_tree = measure_lookup_latency(opts.lookup_samples, opts.warmup)?;
            lookup.measured_as(
                status,
                if status == Status::Indicative {
                    format!(
                        "INDICATIVE, not a claim: TF_TREE_BENCH_FORCE=1 overrode the fitness \
                         refusal. {}",
                        fitness.reason_line()
                    )
                } else {
                    String::new()
                },
            );
        }
    }
    rows.push(lookup);

    // Built before the row so the `shm` clause and `Ground::FrozenBackendNotCompiled` are one claim.
    let (ptv_reason, ptv_grounds) = {
        let mut r = format!(
            "{host_reason}. There is a further gap, and it is not the one this reason \
             used to name: nothing in this repository times publish-to-visible end to \
             end. `just dds-bench` is a real DDS round trip — one publisher, N tf2_ros \
             listeners, §5.2's QoS — but `dds_report` reports `svc`, the engine call \
             itself, so its number answers a different question"
        );
        // `NoInstrument`, not `MeasuredElsewhere`: nothing here times publish-to-visible
        // end to end (`just mp-bench` measures service latency). Unconditional, so the
        // ground list is non-empty on every host.
        let mut g = vec![Ground::NoInstrument];
        g.extend(host_grounds.iter().copied());
        if !build.frozen_backend {
            r.push_str(
                ". A cross-process publish additionally goes through the shared-memory \
                 backend, and this binary was compiled without `--features shm`",
            );
            g.push(Ground::FrozenBackendNotCompiled);
        }
        (r, g)
    };

    rows.push(
        Row::unavailable(
            "publish_to_visible",
            "Publish -> visible-to-consumer (p50, p99.9)",
            "Both stacks, publisher process to consumer process.",
            Sensitivity::AbsoluteTiming,
            // What is missing is the *instrument*: `dds_report`'s `svc` column times the
            // engine call, not publish-to-visible. No numeral in front of a `cfg!`-decided list.
            ptv_reason,
            "just mp-bench (tf_tree, service latency) / just mp-bench-tf2",
        )
        .n_way()
        .on(&ptv_grounds),
    );

    rows.push(
        Row::unavailable(
            "scaling_curve",
            "Scaling curve, N = 1..16 (throughput, CPU)",
            "Both stacks. The claim under test is that reads scale with threads.",
            Sensitivity::AbsoluteTiming,
            // The 5.35-5.62x figure is attributed to the host `docs/PHASE5.md` §0.0 recorded
            // it on, and stated only where a short host applies.
            {
                // Moves `host_reason`: its last reader.
                let mut r = host_reason;
                if fitness.core_reason.is_some() {
                    r.push_str(
                        ". That a short host produces a bent curve rather than a slow one \
                         is not a guess: `docs/PHASE1.md` §11.3's read-scaling gate \
                         (>= 6x from 1 to 8 threads) is recorded in `docs/PHASE5.md` §0.0 \
                         as FAILING at 5.35-5.62x on the 4-physical-core development host, \
                         which is what an oversubscribed 8-thread row looks like",
                    );
                }
                r
            },
            "just tf2-scaling / just shm-scaling, on >= 16 physical cores",
        )
        .n_way()
        .on(&n_way_grounds),
    );

    rows.push(
        Row::unavailable(
            "tft_16_workers_rss",
            "Frozen .tft: 16 dataloader workers, total RSS vs 16 bag parses (MB)",
            "The wedge's central claim (§12 gate 4: total Pss within 1.2x of one worker).",
            Sensitivity::Memory,
            frozen_row_reason(
                "mapping one .tft from sixteen worker processes",
                "just gate4",
            ),
            // Not "on >= 16 physical cores": a `Memory` row is exempt from the core budget
            // and `just gate4` measures it on this host.
            "just gate4 (the Rust worker, which is what §12 criterion 4 is stated over; \
             `just gate4-python` reports the same measurement with a CPython worker and \
             does not gate)",
        )
        .n_way()
        .on(&[if build.frozen_backend {
            Ground::MeasuredElsewhere
        } else {
            Ground::FrozenBackendNotCompiled
        }]),
    );

    rows.push(
        Row::unavailable(
            "tft_open_vs_bag_parse",
            ".tft open time vs bag parse time (ms)",
            "§12 gate 2 wants open under 10 ms for a 233 MB index.",
            Sensitivity::AbsoluteTiming,
            // Both halves have an instrument (`just gate2`, `just gate5`) but no artifact
            // holds them over one recording, so the ground stays `NoInstrument`: a ratio across
            // two corpora is not this row's quantity.
            format!(
                "{}. The comparison itself is what is missing: the open time and an ingest \
             time both have recipes, but no artifact holds them over one recording — the \
             gated `.tft` is frozen from a generated fleet and was never a bag — so there \
             is still nothing to divide the open time by",
                frozen_row_reason(
                    "timing `Tree::open_frozen` against a 233 MB index",
                    "just gate2"
                )
            ),
            "just bench-report-shm against a .tft built by `tf_tree freeze --from-bag` from a \
         recording large enough to produce §12 gate 2's 233 MB index",
        )
        .on(&[
            if build.frozen_backend {
                Ground::MeasuredElsewhere
            } else {
                Ground::FrozenBackendNotCompiled
            },
            Ground::NoInstrument,
        ]),
    );

    // Correctness: host-independent by construction.
    let diff =
        crate::differential::run_naive_rust(opts.differential_queries, 0x5EED_1234_ABCD_0001)?;
    // A differential that scored nothing has `max_error` 0.0 and looks perfect; `passed()` tells them apart.
    if !diff.passed() {
        bail!(
            "the naive-Rust differential did not pass ({} queries scored, max error {})",
            diff.compared,
            diff.max_error
        );
    }
    #[cfg(feature = "tf2")]
    let tf2_metrics = {
        let t = crate::differential::run_tf2(opts.differential_queries, 0x5EED_1234_ABCD_0001)?;
        if !t.passed() {
            bail!(
                "the tf2::BufferCore differential did not pass ({} queries scored, max error {})",
                t.compared,
                t.max_error
            );
        }
        vec![
            Metric::new("max_deviation", t.max_error, "rad or m").lower_is_better(DEVIATION_SLACK),
            Metric::new("compared", t.compared as f64, "queries"),
            Metric::new("tolerance", t.tolerance, "rad or m"),
        ]
    };
    #[cfg(not(feature = "tf2"))]
    let tf2_metrics: Vec<Metric> = Vec::new();
    let agreement = Row {
        id: "differential_agreement",
        title: "Differential agreement (LerpSlerp), max deviation",
        note: "tf_tree column: against the independent naive-Rust reference model. \
               tf2 column: against tf2::BufferCore. Deviation is \
               max(rotation-angle error in rad, translation error in m), so a \
               quaternion sign flip cannot pass. Not timing sensitive: the same \
               inputs give the same disagreement on any host."
            .to_owned(),
        sensitivity: Sensitivity::HostIndependent,
        needs_n_cores: false,
        status: Status::Measured,
        reason: String::new(),
        // Measured: nothing to excuse.
        grounds: Vec::new(),
        reproduce: "cargo test -p tf_tree_bench --release --test differential",
        tf_tree: vec![
            // The one number in this report that is a claim on any host.
            Metric::new("max_deviation", diff.max_error, "rad or m")
                .lower_is_better(DEVIATION_SLACK),
            Metric::new("compared", diff.compared as f64, "queries"),
            Metric::new("tolerance", diff.tolerance, "rad or m"),
        ],
        tf2: tf2_metrics,
    };
    rows.push(agreement);

    rows.push(embedding_row(opts, &fitness)?);
    rows.push(ratio_row(&fitness));

    // Built before the struct takes `fitness`: the memory entry is gated on the verdict.
    let worse = worse_entries(opts, &fitness);
    Ok(Report {
        provenance: Provenance::collect(),
        build,
        fitness,
        warmup_discarded_s: opts.warmup.as_secs_f64(),
        rows,
        worse,
    })
}

/// §9.2's cross-crate row is built by [`embedding_row`]; the measurement is [`crate::embed`].
/// What the two columns of the tf2 ratio row mean, and what they do not.
const RATIO_NOTE: &str = "Both engines in one process, `LerpSlerp` on both sides (tf2's \
    policy), depth 3 after constant folding, 256 off-grid stamps. `speedup_vs_tf2` is the \
    MEDIAN PER-ROUND quotient, not the quotient of the two medians: the arms are timed back \
    to back inside every round and the leading arm alternates, so drift common to both \
    cancels and no arm always gets the colder cache. That pairing is what makes this \
    resolvable where an absolute is not. The two engines are checked to agree on every \
    stamp before either is timed. **The tf2 column goes through `tf_tree_tf2_sys` and \
    therefore FLATTERS tf_tree** by the residual FFI boundary, 45.3 ns / 10% at this depth \
    (498.2 ns through the binding against 452.9 ns native — this figure used to read \
    `~21 ns / 8%` here, which `docs/benchmarks/tf2.md` withdrew for having no derivation); \
    the binding-free comparison is `docker/tf2/native_scaling.cpp` and its headline is \
    2.7x. The floor is set well under both for that reason: this row catches an engine \
    regression, it does not publish the headline. **The floor speaks for the build in this \
    report's `build_profile` / `build_lto` provenance fields, and `just tf2-bench-check` \
    sets those to `release` / `\"thin\"` — NOT to what a consumer compiles, which is \
    cargo's release defaults (no LTO) and measures 244 ns rather than 202 on this arm, a \
    paired 2.07x rather than 2.49x. `just tf2-ratio-profiles` is that measurement.** \
    That build is not a hypothetical and not an approximation: `[profile.*]` is honoured \
    only in a workspace root, and the published crates declare none, so `cargo add \
    tf_tree` AND `cargo install tf_tree_cli` both get `lto = false, codegen-units = 16` \
    — `[profile.embedder]` on both knobs. **The 2.49x is reachable only by building inside \
    this repository.** `docs/decisions/0025` is why there is nevertheless no second gated \
    row for it, and the reason is a measurement rather than a preference: across three \
    repeats the consumer median is stable (2.047-2.088) but its BAND STRADDLES THE FLOOR \
    in two of the three, so `ratio.rs` returns `Unresolved` there. A threshold cannot be \
    derived from a band that contains it, and one chosen low enough to pass would be a \
    gate that always passes — worse than no gate, because it reads as evidence. With the \
    binding bias above removed as well the consumer estimate is ~1.80x, under the floor; \
    that is `UNBIASED_ESTIMATE_DEFAULT_RELEASE`, it is `pub` so a reader can reach it, and \
    `ratio.rs`'s FLOOR doc comment is why it does not move the constant. \
    `ns_per_lookup` on either side is \
    REPORTED, NEVER GATED — it is an absolute duration and this host cannot claim one. \
    Single-threaded and uncontended, which is both engines' best case; the contended \
    comparison, where tf2 anti-scales, is `just tf2-scaling`.";

/// The depth-3 tf2 ratio, the first row on the `Ratio` axis.
///
/// The row a 4-core host can gate: every other tf2 comparison is an absolute
/// duration and unavailable here, while a paired quotient has a ~3% within-run band.
/// `unavailable` without the `tf2` feature (needs ROS 2; `just tf2-check`'s
/// container is where it resolves).
fn ratio_row(fitness: &Fitness) -> Row {
    const ID: &str = "lookup_ratio_vs_tf2";
    const TITLE: &str = "Depth-3 hot lookup, tf_tree vs tf2 (paired ratio)";
    const REPRODUCE: &str = "just tf2-bench (the ratio resolves only where ROS 2 is installed; \
         `docker/tf2/run.sh` is that place on this host)";

    #[cfg(not(feature = "tf2"))]
    {
        let _ = fitness;
        Row::unavailable(
            ID,
            TITLE,
            RATIO_NOTE,
            Sensitivity::Ratio,
            "this row times `tf2::BufferCore` in-process, which needs a ROS 2 install this \
             build does not have — `tf_tree_bench` was compiled without `--features tf2`. \
             It is the build and not the host: a ratio is measurable here, and the fitness \
             probe's timing verdict does not reach it"
                .to_owned(),
            REPRODUCE,
        )
        .on(&[Ground::Tf2NotLinked])
    }

    #[cfg(feature = "tf2")]
    {
        let mut row = Row::unavailable(
            ID,
            TITLE,
            RATIO_NOTE,
            Sensitivity::Ratio,
            String::new(),
            REPRODUCE,
        )
        .on(&[Ground::MeasurementRefused]);
        let run = match crate::ratio::measure() {
            Ok(r) => r,
            Err(e) => {
                row.reason = format!("the paired measurement could not be taken: {e}");
                return row;
            }
        };
        // A band that straddles the floor has not answered (`embed.rs`'s rule). Both
        // non-`Above` verdicts stop here: a `Below` band must not publish as a clean
        // `measured` row.
        match run.verdict() {
            crate::ratio::Verdict::Above => {}
            crate::ratio::Verdict::Unresolved => {
                row.reason = format!(
                    "the pair was measured and cannot resolve the {:.1}x floor: {}",
                    crate::ratio::FLOOR,
                    run.verdict_line()
                );
                return row;
            }
            crate::ratio::Verdict::Below => {
                row.reason = format!(
                    "the pair was measured and is BELOW the {:.1}x floor this row gates: {}",
                    crate::ratio::FLOOR,
                    run.verdict_line()
                );
                return row;
            }
        }
        let status = fitness.ratio_status();
        let reason = match status {
            Status::Measured => String::new(),
            Status::Indicative => format!(
                "INDICATIVE, not a claim: TF_TREE_BENCH_FORCE=1 overrode the refusal. {}",
                fitness.ratio_reason_line()
            ),
            Status::Unavailable => format!(
                "the pair was measured, but this host cannot produce a trustworthy ratio: {}",
                fitness.ratio_reason_line()
            ),
        };
        if status == Status::Unavailable {
            // The refusal is now "host unfit on the ratio axis", so the ground moves.
            row.reason = reason;
            row.grounds = vec![Ground::HostFitness];
        } else {
            row.measured_as(status, reason);
        }
        if row.status != Status::Unavailable {
            row.tf_tree = vec![
                Metric::new("speedup_vs_tf2", run.ratio, "x").higher_is_better(RATIO_SLACK),
                Metric::new("ratio_lo", run.ratio_lo, "x"),
                Metric::new("ratio_hi", run.ratio_hi, "x"),
                Metric::new("floor", crate::ratio::FLOOR, "x"),
                Metric::new("ns_per_lookup", run.tf_tree_ns, "ns"),
                Metric::new("agreed", run.agreed as f64, "queries"),
            ];
            row.tf2 = vec![Metric::new("ns_per_lookup", run.tf2_ns, "ns")];
        }
        row
    }
}

/// Module-level so `the_row_note_states_the_settings_the_manifest_declares` can
/// check this prose against the workspace manifest.
const EMBEDDING_NOTE: &str = "One build, one profile, two identical bodies. `out_of_crate_ns` \
    times an `#[inline(never)]` depth-3 lookup compiled in `tf_tree_bench` — an embedder's \
    position; `in_crate_ns` times the same three lines compiled in `tf_tree_core`, the \
    crate that defines `Plan::at` and the fold. `boundary_ratio` is the median per-round \
    quotient, paired so that machine noise common to both columns cancels. The profile \
    is `[profile.embedder]` (lto = false, codegen-units = 16 — cargo's `--release` \
    defaults), which §9.2 requires: under this workspace's `lto = \"thin\"` the crate \
    boundary is erased at link time and the comparison measures nothing. Depth 3, \
    LerpSlerp, off-grid stamps so the interpolation actually runs. A probe in the \
    `tf_tree` facade would NOT be in-crate and was measured not to be (241.5 vs 243.6 ns); \
    `crates/tf_tree_bench/src/embed.rs` carries that table. There is no tf2 column: this \
    row is `tf_tree` against itself.";

/// `pub(crate)` so `crate::baseline`'s test builds the real `None`-arm row.
pub(crate) fn embedding_row(opts: &Options, fitness: &Fitness) -> Result<Row> {
    const ID: &str = "embedding_cross_crate";
    const TITLE: &str = "Facade Plan::at from a separate crate vs in-crate, depth 3 (ratio)";
    const NOTE: &str = EMBEDDING_NOTE;
    const REPRODUCE: &str = "just embed-cost";

    let Some(dir) = opts.embed_cost.as_deref() else {
        return Ok(Row::unavailable(
            ID,
            TITLE,
            NOTE,
            Sensitivity::AbsoluteTiming,
            "this row's in-crate column is `tf_tree_core::bench_probe`, which is compiled \
             only under the default-off `bench-probe` feature, and it must be measured at \
             `[profile.embedder]` — this tool is built with `lto = \"thin\"`, which is \
             exactly what erases the boundary. It cannot measure the row from inside \
             itself. `just embed-cost` builds and runs the probe and writes the pair, and \
             both `just bench-check` and `just bench-baseline-update` depend on that \
             recipe and pass the directory back in with --embed-cost. Reaching this \
             branch means `bench_report` was invoked directly without the flag, so the \
             row is reported without a number rather than left out"
                .to_owned(),
            REPRODUCE,
        )
        .on(&[Ground::MeasuredElsewhere]));
    };

    // A pair that will not load is a measurement failure, not a row; loading the
    // *pair* runs `Pair::load`'s provenance checks.
    let pair = crate::embed::Pair::load(dir)
        .with_context(|| format!("loading the embed_cost pair from {}", dir.display()))?;
    let run = &pair.embedder;

    // The spread gates the verdict, independently of the fitness probe: a band
    // straddling §9.2's threshold is reported unavailable with the band.
    if run.verdict() == crate::embed::Verdict::Unresolved {
        return Ok(Row::unavailable(
            ID,
            TITLE,
            NOTE,
            Sensitivity::AbsoluteTiming,
            format!(
                "the pair was measured and cannot resolve §9.2's 5% criterion: {}",
                run.verdict_line()
            ),
            REPRODUCE,
        )
        .on(&[Ground::MeasurementRefused]));
    }

    let mut row = Row::unavailable(
        ID,
        TITLE,
        NOTE,
        Sensitivity::AbsoluteTiming,
        format!(
            "the pair was measured, but the host failed the fitness probe, so neither half \
             of the ratio is a claim: {}",
            fitness.reason_line()
        ),
        REPRODUCE,
    )
    .on(&[Ground::HostFitness]);
    match fitness.timing_status() {
        Status::Unavailable => {}
        status => {
            row.tf_tree = run.metrics();
            row.measured_as(
                status,
                if status == Status::Indicative {
                    format!(
                        "INDICATIVE, not a claim: TF_TREE_BENCH_FORCE=1 overrode the fitness \
                         refusal. {} Measured here: {}",
                        fitness.reason_line(),
                        run.verdict_line()
                    )
                } else {
                    String::new()
                },
            );
        }
    }
    Ok(row)
}

/// Pss actually held by an idle arena of §9.3's stated geometry.
///
/// Returns `(resident_bytes, reserved_bytes)`, or [`None`] where the figure cannot
/// be trusted (no `smaps_rollup`, or a non-positive delta).
///
/// A delta of a whole-process counter, quantised to pages with a page or two of
/// slack; not byte-exact (`mincore(2)` would need an `unsafe` boundary and a
/// decision record). The tree is held across the second read.
fn measure_idle_arena_resident() -> Option<(f64, f64)> {
    use tf_tree::{Capacity, EdgeCfg, TreeBuilder};

    // The geometry `from_totals` is asked about below, built for real.
    const DYNAMIC_EDGES: u32 = 32;
    const SLOTS_PER_EDGE: u32 = 1024;
    const FRAMES: u32 = 64;
    const EDGE_SLOTS: u32 = 64;

    let mut b = TreeBuilder::new().frame("root");
    let mut names = Vec::with_capacity(DYNAMIC_EDGES as usize);
    for i in 0..DYNAMIC_EDGES {
        names.push(format!("f{i}"));
    }
    for name in &names {
        b = b.dynamic_edge("root", name, EdgeCfg::new(Capacity::slots(SLOTS_PER_EDGE)));
    }
    // Headroom to the stated totals: the reservation is what is under test.
    let b = b
        .frame_headroom(FRAMES - DYNAMIC_EDGES - 1)
        .edge_headroom(EDGE_SLOTS - DYNAMIC_EDGES);

    // Warm the reader so its buffer is not in the delta.
    let _ = crate::mp::self_pss_kib();
    let before = crate::mp::self_pss_kib();
    let tree = b.build().ok()?;
    let after = crate::mp::self_pss_kib();

    let reserved = tree.arena_size_bytes() as f64;
    // Held across the read above.
    std::hint::black_box(&tree);
    drop(tree);

    if before == 0 || after <= before {
        return None;
    }
    Some(((after - before) as f64 * 1024.0, reserved))
}

/// §9.3's "where `tf_tree` is worse": the four costs, with a number wherever one exists.
fn worse_entries(opts: &Options, fitness: &Fitness) -> Vec<Worse> {
    // A deployment-shaped arena; `from_totals` reproduces its region geometry.
    const FRAMES: u32 = 64;
    const EDGES: u32 = 64;
    const SLOTS: u32 = 32 * 1024;
    let floor_bytes = tf_tree_arena::ArenaLayout::from_totals(FRAMES, EDGES, SLOTS)
        .map(|l| l.total_size() as f64);

    // The *resident* half of the claim: a mapping is not a footprint. The measurement
    // confirms it: an idle arena is ~100% resident because `alloc_zeroed` at 64-byte
    // alignment zero-fills by hand (`docs/decisions/0021`).
    let resident = measure_idle_arena_resident();

    // The measured half is stated only when it exists.
    let measured_half = match resident {
        Some((resident_bytes, arena_bytes)) => format!(
            "`idle_arena_resident_bytes` is the measured Pss an idle arena of that \
             geometry actually costs, and it is now {:.1}% of what the arena reserves \
             ({resident_bytes:.0} B held against {arena_bytes:.0} B reserved by the \
             arena actually built). **This row used to say the opposite.** The \
             measurement was added expecting the resident figure to come out far below \
             the reserved one, found the arena ~100% resident instead, and stood on \
             that. Decision 0021 then found the cause — `HeapArena` asked the allocator \
             for 64-byte alignment (`PoseSlot` is one cache line), and Rust's \
             `alloc_zeroed` reaches `calloc` only at alignment <= 16, falling back above \
             it to `posix_memalign` plus an explicit zero-fill that touches every page. \
             The arena is now over-allocated at 16 and aligned to 64 by hand, so \
             `calloc` returns demand-faulted pages the kernel already guarantees to be \
             zero. The reservation is unchanged and this entry stands on the \
             reservation: address space is still a cost tf2 does not pay, a \
             fixed-capacity arena still cannot grow, and a machine under strict \
             overcommit is still constrained. What is gone is the *residency*. \
             `idle_arena_resident_bytes` is a delta of a *whole-process* Pss counter \
             across building one tree, quantised to 4 KiB pages, so it also carries \
             the tree's own non-arena allocations — which is most of what is left. The \
             order of magnitude is the finding; the third digit is not.",
            resident_bytes / arena_bytes * 100.0
        ),
        None => "`idle_arena_resident_bytes` is absent: Pss could not be measured on \
             this host (no readable /proc/self/smaps_rollup, or a non-positive delta), \
             so how much of the reservation is actually resident is unmeasured here \
             rather than assumed. `just bench-report` on Linux fills it in."
            .to_owned(),
    };

    let mut floor = Worse {
        id: "arena_memory_floor",
        topic: "Arena memory floor",
        statement: format!(
            "A tf_tree arena is fixed-capacity and allocated up front, so an idle tree \
             reserves its full size from the first second. A tf2 BufferCore starts near \
             empty and grows into whatever the stream actually contains, so on a robot \
             that publishes far less than it declared, tf2 reserves less and tf_tree \
             is simply worse. The figure is for {FRAMES} frames, {EDGES} edge slots and \
             {SLOTS} sample slots. `idle_arena_bytes` is arithmetic on the layout — what \
             the arena *reserves* — and does not depend on this host. {measured_half}"
        ),
        metrics: Vec::new(),
        metrics_absent_because: None,
        metrics_withheld: Vec::new(),
    };
    // Arithmetic and measurement are independent, so emitted independently.
    if let Ok(bytes) = floor_bytes {
        floor
            .metrics
            .push(Metric::new("idle_arena_bytes", bytes, "B"));
        floor.metrics.push(Metric::new(
            "idle_arena_mib",
            bytes / (1024.0 * 1024.0),
            "MiB",
        ));
    }
    // The memory axis reaches here too: `Worse` entries carry no `Sensitivity`, so a
    // debug build would otherwise publish a figure `fair_for_memory: false` disowns.
    let resident = if fitness.fair_for_memory {
        resident
    } else {
        None
    };
    // Recorded, not silent: `Report::validate`'s direction rule stands down on it
    // (see `Worse::metrics_withheld`).
    if resident.is_none() {
        floor.metrics_withheld.push("idle_arena_resident_bytes");
        floor
            .metrics_withheld
            .push("idle_arena_measured_reserved_bytes");
        floor.metrics_withheld.push("idle_arena_resident_fraction");
    }
    if let Some((resident_bytes, arena_bytes)) = resident {
        // The one gated number here (`0021` step 4; its falsifier, a revert of the
        // alignment fix, lands at 98x). The *fraction* stays informational: it is this
        // divided by the reserved bytes, so gating both would be one claim twice.
        floor.metrics.push(
            Metric::new("idle_arena_resident_bytes", resident_bytes, "B")
                .lower_is_better(RESIDENCY_SLACK),
        );
        // Both sides describe the arena the measurement built, not `from_totals`'s.
        floor.metrics.push(Metric::new(
            "idle_arena_measured_reserved_bytes",
            arena_bytes,
            "B",
        ));
        floor.metrics.push(Metric::new(
            "idle_arena_resident_fraction",
            resident_bytes / arena_bytes,
            "of measured reserved",
        ));
    }

    // If both sources fail, say so rather than present an empty list.
    if floor.metrics.is_empty() {
        floor.metrics_absent_because = Some(
            "neither half landed on this run: `ArenaLayout::from_totals` did not return a layout for the stated geometry, and the Pss measurement was unavailable or was withheld because this host failed the memory axis of the fitness probe. The reservation arithmetic is host-independent, so this state is a bug or a hostile /proc, not a property of the machine — `just bench-report` on any Linux host that passes `Fitness::probe` fills both in."
                .to_owned(),
        );
    }

    vec![
        floor,
        Worse {
            id: "attach_latency",
            topic: "Attach latency",
            statement: "Joining a live arena is a rendezvous: open the runtime directory, take \
                 the lock file, receive the segment fd over a unix socket, map it, and \
                 validate the header. A tf2 consumer constructs a buffer in-process and \
                 is ready immediately. The cost is paid once per process, but it is real, \
                 and it is a cost tf2 does not have. **Measured on the §11.1 fixture** \
                 (`just attach-bench`, 201 attach/lookup cycles, ReadOnly). This entry \
                 carried no number at all until it was built, which made it an honesty \
                 section that could not regress. \
                 \
                 **The number improved seven- to eightfold, and that must not be read as \
                 the cost going away.** Attach was 99.8 us p50 on the commit before \
                 `docs/decisions/0024` landed — 99 791 ns, that record's own before \
                 column — almost all of it `populate_hot`; it is now \
                 **12.3-14.2 us p50**, which is 7.0x to 8.1x — `8x` is the best run, not \
                 the figure — because `0024` moved ring population out of attach and onto \
                 the moment an edge is taken up. The cost *moved*: first plan compile \
                 went 550 ns to **66.3-92.3 us p50** on this fixture, whose plan walks \
                 essentially every edge. Summed, **100.3 us before** — that is 99 791 \
                 + 550 ns, `0024`'s paired before column, and *not* the 97.5 us \
                 `docs/PHASE2.md` §12.2 used to carry, which was a different sitting on \
                 a different commit and never had 100.3 as its sum — against \
                 **79.3-106.4 us after**, per run and paired. On the \
                 fixture that gains no memory from the change, a wash. **The after ranges \
                 are observed extremes over 28 runs on one busy host, load average 4 to 7, \
                 rounded outward — what was seen, not a bound**; §12.2 carries the same \
                 spread and the reason for it, and the ranges printed here before these \
                 were falsified by the next nine runs. What tf2 does not pay is still \
                 what tf2 does not pay; it is now itemised at two line items instead of \
                 one, and a reader quoting only the first would be quoting a sevenfold \
                 improvement that this fixture did not deliver. \
                 \
                 §7.1's guarantee holds throughout: the **first** lookup after attach is \
                 130 ns p50 before and 130-170 ns p50 after, indistinguishable from a \
                 steady-state one, and the fault *count* is zero. Recompiling a plan \
                 whose pages are already resident costs ~1.4 us (1.33 us at \
                 `0f17fb8`, 1.36-1.44 us across the sixteen runs since), which bounds the \
                 topology-change path — a `reparent` invalidates every cached plan, so \
                 that figure is the one standing between a reparent and a fault storm \
                 across every reader."
                .to_owned(),
            metrics: Vec::new(),
            metrics_withheld: Vec::new(),
            metrics_absent_because: Some(
                "the figure is `just attach-bench`'s: a separate binary that opens a live \
                 shared arena over the §11.1 fixture and times the rendezvous. This report \
                 is produced in one process that never attaches — there is nothing here to \
                 attach *to* — so the number is stated above with its recipe rather than \
                 re-measured and gated here. It is a real measurement in the wrong binary, \
                 not a missing one."
                    .to_owned(),
            ),
        },
        Worse {
            id: "format_bump_cost",
            topic: "Operational cost of a format bump",
            statement: format!(
                "Every participant shares one arena layout, so a FORMAT_VERSION change \
                 (this build: {}) is a fleet-wide, all-at-once restart: mixed versions do \
                 not attach, by design. `docs/PHASE5.md` §1 bumps v2 to v3 for exactly \
                 this reason — to break it once. tf2 has no shared binary layout and no \
                 equivalent event. `tf_tree doctor --explain-version` prints what an \
                 operator meeting the refusal needs. **This cost is qualitative, and \
                 that is a finding rather than an omission** — see below.",
                tf_tree::arena_format_version()
            ),
            metrics: Vec::new(),
            metrics_withheld: Vec::new(),
            metrics_absent_because: Some(
                "this cost is not denominated in nanoseconds or bytes, and no run of this \
                 benchmark on any host would produce it. Its units are *participants* and \
                 *coordination*: every process sharing an arena must be rebuilt and \
                 restarted together, so the quantity is the size of a fleet and the length \
                 of the window in which it can all be down — properties of a deployment, \
                 not of a machine. There is no distribution to sample either, because the \
                 refusal is deterministic and total: a mismatched participant does not \
                 attach at all, so there is no latency, no failure rate and no tail to \
                 measure. The one number in reach — how long a single participant takes to \
                 restart — would be worse than none, because it is precisely the *together* \
                 that costs, and quoting a per-process figure would understate it while \
                 looking rigorous. What would genuinely quantify this is operating \
                 evidence from a deployment that has lived through a bump (how long the \
                 fleet was mixed, what it cost to hold it down), which is the same class of \
                 evidence `docs/PROJECT.md` D21 gates PHASE7 on and which this project does \
                 not have. Until then the honest artifact is the sentence, plus the version \
                 this build refuses to attach across, which is stated above."
                    .to_owned(),
            ),
        },
        Worse {
            id: "bridge_supervision",
            topic: "The bridge is another process to supervise",
            statement: format!(
                "The {} consumers in this comparison read one arena, which somebody has \
                 to fill: the ROS 2 ingest bridge is a process that must be started, \
                 supervised, restarted and monitored. With tf2 there is no such process — \
                 every node subscribes to /tf directly. That is one more thing to page \
                 somebody about at 3 a.m., and it is the honest cost of the shared arena. \
                 **It has been measured, over a real DDS**, by `just dds-bench` — one run, \
                 four consumers, a 15 s window, on this project's unpinned host \
                 (`docs/benchmarks/tf2.md`, the `tf_tree.processes` arm): the bridge \
                 process burns **0.362 s of CPU in 15 s, about 2.4% of one core**, and it \
                 burns it whatever N is. Against it a marginal tf_tree consumer costs \
                 0.0186 s and a marginal tf2 listener 0.445 s over the same window, so the \
                 supervision cost pays for itself at **roughly one consumer** — the bridge \
                 is cheaper than the single tf2 listener it replaces. Two significant \
                 figures is what one run of four processes supports. Memory is the half \
                 that stays unattributed: `dds_report` sums Pss across an arm and the \
                 bridge is the process in it reporting `consumers 0`, so its footprint is \
                 inside the arm total (69.51 MiB over five processes, against tf2's 63.15 \
                 over four) and is **not** the 6.36 MiB difference — Pss divides a shared \
                 page by the number of processes mapping it, and those two arms map from \
                 four and five, so the difference is confounded before any bridge exists. \
                 **The curve settles what that one point could not.** Run at N = 8, 12 and \
                 16 as well, the arm totals are 113.80/113.96, 168.39/167.41 and \
                 219.06/226.59 MiB (tf_tree/tf2): the sign flips between 4 and 8, the two \
                 stacks are indistinguishable from 8 to 12, and by 16 tf_tree is 3.3% \
                 ahead. The mechanism is visible in the per-consumer column — tf_tree's \
                 marginal consumer falls 17.38 -> 13.69 MiB across the sweep while tf2's \
                 stays flat near 14.2 — which is this entry's cost seen from the other \
                 side: **one fixed process, amortised.** It is still not an arena result. \
                 The `composed` arms put both stacks in one process and differ by only \
                 0.75-1.04 MiB; everything else in those totals is rclcpp and DDS, paid \
                 identically per process by both. \
                 What the CPU column shows is the operational shape of this trade: a fixed \
                 cost you must supervise, bought against a per-consumer cost you do not.",
                opts.consumers
            ),
            metrics: Vec::new(),
            metrics_withheld: Vec::new(),
            metrics_absent_because: Some(
                "the number above exists, and it belongs to a different artifact. It takes \
                 ROS 2, a real DDS and five processes — `just dds-bench`, inside \
                 `docker/tf2` — and `bench_report` runs in one process on the host, where \
                 `rclcpp` is not linked and `ros/` is not even in the cargo workspace. That \
                 is the same reason the `.tft` rows in the table above are `unavailable` \
                 rather than guessed. Carrying the figure here as a `Metric` would be worse \
                 than leaving it out: metrics in this file are what `crate::baseline` \
                 compares run to run on one host, so a constant transcribed from another \
                 host's container run would sit in the gate looking measured and never move \
                 — a row that cannot regress, in the section whose entire purpose is to be \
                 the row that can. `dds_report` gates it where it is produced; this entry \
                 states it with its recipe."
                    .to_owned(),
            ),
        },
    ]
}

/// The `lookup_latency` row's note, the only place the row explains what it measured.
///
/// `docs/PHASE1.md` §11.3 (NORMATIVE) requires the dynamic-step count, and the stamp
/// regime decides whether the interpolator runs (`docs/decisions/0013`).
/// `tests::the_lookup_row_note_states_what_phase1_requires` checks both against the
/// code, using [`LOOKUP_STAMP_NS`].
const LOOKUP_NOTE: &str = "tf_tree column: `map <- imu_link`, LerpSlerp, in-process, one \
     thread. **3 dynamic steps** after constant folding (1 kHz, 200 Hz, 50 Hz), which is what \
     docs/PHASE1.md §11.3's NORMATIVE reading of \"depth 3\" means. The query stamp is \
     fixture::QUERY_NS = NOW_NS - 500 us, which is off-grid on every one of those three rates, \
     so the interpolator runs on every step; it was on-grid until \
     docs/decisions/0013, and a p50 published before that change is not comparable with one \
     published after. Percentiles include two Instant::now() calls, whose own cost is reported \
     alongside as clock_overhead_p50_ns. The tf2 column is a separate, cross-engine comparison \
     and is not attempted here.";

/// The stamp [`measure_lookup_latency`] queries, named so a test can check
/// [`LOOKUP_NOTE`]'s claims about it (`fixture::QUERY_NS`, off every dynamic grid).
const LOOKUP_STAMP_NS: i64 = crate::fixture::QUERY_NS;

/// Measure depth-3 hot-path lookup latency on this process.
///
/// Percentiles include two `Instant::now()` calls; the clock's own cost is reported
/// as `clock_overhead_p50_ns` and left for the reader to subtract. The query stamp
/// is [`crate::fixture::QUERY_NS`], which interpolates (`LOOKUP_NOTE`,
/// `docs/decisions/0013`).
///
/// # Errors
///
/// Any fixture failure.
pub fn measure_lookup_latency(samples: usize, warmup: Duration) -> Result<Vec<Metric>> {
    let tree = crate::fixture::build_tree_with(InterpPolicy::LerpSlerp)?;
    let (_writers, _pushed) = crate::fixture::spin_up(&tree)?;
    let target = tree
        .frame("imu_link")
        .map_err(|e| anyhow!("fixture frame `imu_link` is missing: {e:?}"))?;
    let source = tree
        .frame("map")
        .map_err(|e| anyhow!("fixture frame `map` is missing: {e:?}"))?;
    // `LookupError` is `Copy` and not `std::error::Error`, so `?` cannot convert it.
    let plan = tree
        .plan(target, source)
        .map_err(|e| anyhow!("compiling the map <- imu_link plan: {e:?}"))?;
    let guard = tree.guard();
    let stamp: Stamp = Stamp::from_nanos(LOOKUP_STAMP_NS);

    // §9.3: warm, then discard; time-based so the stated number is the printed one.
    let mut sink = 0.0f64;
    let warm_start = Instant::now();
    while warm_start.elapsed() < warmup {
        for _ in 0..1024 {
            sink += plan.at(&guard, stamp).map_err(eval_failed)?.t.x;
        }
    }

    let mut hist = crate::mp::Histogram::new();
    for _ in 0..samples {
        let t0 = Instant::now();
        let iso = plan.at(&guard, stamp).map_err(eval_failed)?;
        hist.record(elapsed_ns(t0));
        sink += iso.t.x;
    }

    // The clock's own cost, in the same loop shape.
    let mut clock = crate::mp::Histogram::new();
    for _ in 0..samples.min(50_000) {
        let t0 = Instant::now();
        clock.record(elapsed_ns(t0));
    }

    // Keeps the loop from being optimised away; NaN means samples were discarded.
    if sink.is_nan() {
        bail!("lookup sink went NaN — the measured loop did not run as written");
    }

    // `samples` and `clock_overhead_p50_ns` stay informational: a run parameter and the host's clock.
    Ok(vec![
        Metric::new("p50_ns", hist.quantile(0.50) as f64, "ns").lower_is_better(LATENCY_SLACK),
        Metric::new("p99_ns", hist.quantile(0.99) as f64, "ns").lower_is_better(LATENCY_SLACK),
        Metric::new("p999_ns", hist.quantile(0.999) as f64, "ns").lower_is_better(LATENCY_SLACK),
        Metric::new("samples", hist.count() as f64, "lookups"),
        Metric::new("clock_overhead_p50_ns", clock.quantile(0.50) as f64, "ns"),
    ])
}

/// Nanoseconds since `t0` in 64-bit arithmetic (`Duration::as_nanos` is `u128`).
#[inline]
fn elapsed_ns(t0: Instant) -> u64 {
    let d = t0.elapsed();
    d.as_secs()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::from(d.subsec_nanos()))
}

/// Lift a `Copy` [`tf_tree::LookupError`] into `anyhow`, out of line for the hot loops.
#[cold]
fn eval_failed(e: tf_tree::LookupError) -> anyhow::Error {
    anyhow!("plan evaluation failed: {e:?}")
}

/// Physical core count from `/proc/cpuinfo` `physical id` / `core id` pairs, or
/// [`None`] when none are published (aarch64 and many containers, so the ordinary
/// answer). `available_parallelism` counts SMT siblings and is not substituted;
/// [`Fitness::assess`] decides what to do.
#[must_use]
pub fn physical_cores() -> Option<usize> {
    physical_cores_from_cpuinfo(&std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default())
}

/// The parse over text, so a host this one is not can be tested.
fn physical_cores_from_cpuinfo(text: &str) -> Option<usize> {
    let mut ids = std::collections::HashSet::new();
    let (mut phys, mut core) = (None, None);
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("physical id") {
            phys = v
                .split(':')
                .nth(1)
                .and_then(|x| x.trim().parse::<u32>().ok());
        } else if let Some(v) = line.strip_prefix("core id") {
            core = v
                .split(':')
                .nth(1)
                .and_then(|x| x.trim().parse::<u32>().ok());
        }
        if let (Some(p), Some(c)) = (phys, core) {
            ids.insert((p, c));
            phys = None;
            core = None;
        }
    }
    (!ids.is_empty()).then_some(ids.len())
}

fn cpu_model() -> Option<String> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    text.lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_owned())
}

fn governors() -> Option<Vec<String>> {
    let dir = std::fs::read_dir("/sys/devices/system/cpu").ok()?;
    let mut out = Vec::new();
    for e in dir.filter_map(Result::ok) {
        let p = e.path().join("cpufreq/scaling_governor");
        if let Ok(g) = std::fs::read_to_string(p) {
            out.push(g.trim().to_owned());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn dedup_join(v: &[String]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for s in v {
        if !seen.contains(&s.as_str()) {
            seen.push(s);
        }
    }
    seen.join(", ")
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| {
        let s = s.trim();
        s.lines().next().unwrap_or(s).to_owned()
    })
}

fn unknown() -> String {
    "unknown".to_owned()
}

/// The `lto` setting of the profile this binary was built into, read from the
/// workspace manifest at run time (the parser lives in [`crate::embed`]).
/// `CARGO_MANIFEST_DIR` is compile-time, so it names the build's source tree. The
/// failure arm names the unreadable path, distinct from "no LTO".
fn build_lto() -> String {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("Cargo.toml");
    match std::fs::read_to_string(&manifest) {
        Ok(text) => crate::embed::lto_for_profile_dir(&text, crate::embed::PROFILE_DIR),
        Err(e) => format!(
            "unknown (the workspace manifest at {} could not be read: {e})",
            manifest.display()
        ),
    }
}

fn git(args: &str) -> Option<String> {
    capture("git", &args.split(' ').collect::<Vec<_>>())
}

fn capture(bin: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// UTC timestamp as `YYYY-MM-DDTHH:MM:SSZ`; hand-rolled (no date crate), Hinnant's civil-from-days, correct before 1970.
fn iso8601_utc(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// JSON string literal, escaped per RFC 8259.
pub(crate) fn jstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// JSON number, or `null` for a non-finite value (`NaN` is not JSON).
pub(crate) fn jnum(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "null".to_owned()
    }
}

pub(crate) fn jmetrics(m: &[Metric]) -> String {
    let mut s = String::from("{");
    for (i, x) in m.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let _ = write!(
            s,
            "{}: {{\"value\": {}, \"unit\": {}, \"drift\": {}, \"tolerance\": {}}}",
            jstr(x.key),
            jnum(x.value),
            jstr(x.unit),
            jstr(x.drift.as_str()),
            jnum(x.tolerance)
        );
    }
    s.push('}');
    s
}

fn esc_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Format a value for a human column: scientific where fixed-point would print `0.000`.
fn fmt_value(v: f64) -> String {
    if !v.is_finite() {
        return "n/a".to_owned();
    }
    let a = v.abs();
    if a != 0.0 && !(1e-3..1e9).contains(&a) {
        format!("{v:.4e}")
    } else if a >= 100.0 || a == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}

fn cell_html(m: &[Metric]) -> String {
    if m.is_empty() {
        return "&mdash;".to_owned();
    }
    m.iter()
        .map(|x| {
            format!(
                "{} = {} {}",
                esc_html(x.key),
                fmt_value(x.value),
                esc_html(x.unit)
            )
        })
        .collect::<Vec<_>>()
        .join("<br>")
}

#[cfg(test)]
mod tests {
    // Failed assertions are the intended failure mode; `panic!` is allowed so a message can name the missing metric.
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    /// A report skeleton with every required row and worse-entry, all unavailable; tests mutate one thing from it.
    fn skeleton(fair: bool, forced: bool) -> Report {
        let rows = REQUIRED_ROWS
            .iter()
            .map(|id| {
                Row::unavailable(
                    id,
                    "title",
                    "note",
                    Sensitivity::AbsoluteTiming,
                    "a stated reason".to_owned(),
                    "just something",
                )
                // `MeasuredElsewhere` is undecidable by `validate`, so rows are never refused for their ground.
                .on(&[Ground::MeasuredElsewhere])
            })
            .collect();
        let worse = REQUIRED_WORSE
            .iter()
            .map(|id| Worse {
                id,
                topic: "topic",
                statement: "a stated cost".to_owned(),
                metrics: Vec::new(),
                metrics_absent_because: Some("a stated reason".to_owned()),
                metrics_withheld: Vec::new(),
            })
            .collect();
        Report {
            // Every §9.3 fact from the constant the rule reads, so a new key the fixture
            // does not satisfy fails every test; negative tests blank one at a time.
            provenance: Provenance {
                facts: REQUIRED_FACTS
                    .iter()
                    .map(|k| Fact {
                        key: k,
                        value: "a stated value".to_owned(),
                    })
                    .collect(),
            },
            // The shipped default build: no tf2, no frozen backend.
            build: Build {
                tf2_linked: false,
                frozen_backend: false,
            },
            fitness: Fitness {
                fair_for_timing: fair,
                // Timing fails but ratio and memory stay fair: a host can be unfit to time and fit to weigh.
                fair_for_ratios: true,
                fair_for_memory: true,
                // Rows are not `n_way`, so the core budget is out of the picture.
                enough_cores: true,
                core_reason: None,
                forced,
                reasons: if fair {
                    Vec::new()
                } else {
                    vec!["4 physical cores for 16 consumers".to_owned()]
                },
                ratio_reasons: Vec::new(),
                memory_reasons: Vec::new(),
                consumers: 16,
                busy_fraction: 0.01,
                physical_cores: 4,
                physical_cores_known: true,
                logical_cpus: 8,
            },
            warmup_discarded_s: 1.0,
            rows,
            worse,
        }
    }

    /// The skeleton itself must validate, or every negative test passes for the wrong reason.
    #[test]
    fn a_fully_unavailable_report_is_valid() {
        assert_eq!(skeleton(false, false).validate(), Ok(()));
    }

    /// A row that prints numbers must print at least one the regression gate can hold.
    ///
    /// It binds `indicative` as well as `measured`: a row's status is a property of
    /// the host, and a direction never acquired while cheap is one nothing gates later.
    ///
    /// Mutant: restrict the arm to `Status::Measured`; the `indicative` half fails.
    #[test]
    fn a_row_that_prints_numbers_must_print_one_the_gate_can_hold() {
        for (fair, forced, status) in [
            (true, false, Status::Measured),
            (false, true, Status::Indicative),
        ] {
            let mut r = skeleton(fair, forced);
            let row = &mut r.rows[0];
            row.measured_as(
                status,
                if status == Status::Indicative {
                    "forced on an unfit host".to_owned()
                } else {
                    String::new()
                },
            );
            row.tf_tree = vec![Metric::new("samples", 1024.0, "lookups")];
            let errs = r
                .validate()
                .expect_err("a row of pure context claimed to be a result");
            assert!(
                errs.iter()
                    .any(|e| e.contains("every one of them is informational")),
                "{status:?}: {errs:?}"
            );

            // One directional metric is enough.
            r.rows[0]
                .tf_tree
                .push(Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK));
            assert_eq!(r.validate(), Ok(()), "{status:?}");
        }
    }

    /// A directional metric with a negative or non-finite tolerance is refused: both
    /// look like a working gate (always or never firing).
    ///
    /// Mutant: drop the `m.tolerance >= 0.0` conjunct.
    #[test]
    fn a_directional_metric_needs_a_usable_tolerance() {
        for bad in [-0.5, f64::NAN, f64::INFINITY] {
            let mut r = skeleton(true, false);
            let row = &mut r.rows[0];
            row.status = Status::Measured;
            row.reason = String::new();
            row.tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(bad)];
            let errs = r
                .validate()
                .expect_err("an unusable tolerance was accepted");
            assert!(
                errs.iter()
                    .any(|e| e.contains("makes the gate either always")),
                "tolerance {bad}: {errs:?}"
            );
        }
    }

    /// §9.3's central rule: a timing row may not claim `measured` on a host that failed the probe.
    ///
    /// Mutant: delete the fitness arm in `validate`.
    #[test]
    fn a_timing_row_cannot_claim_measured_on_an_unfit_host() {
        let mut r = skeleton(false, false);
        let row = &mut r.rows[0];
        row.measured_as(Status::Measured, String::new());
        row.tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        let errs = r.validate().expect_err("unfit host must reject the claim");
        assert!(
            errs.iter().any(|e| e.contains("failed the fitness probe")),
            "{errs:?}"
        );

        // Same row on a fit host is fine.
        let mut ok = skeleton(true, false);
        let row = &mut ok.rows[0];
        row.measured_as(Status::Measured, String::new());
        row.tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        assert_eq!(ok.validate(), Ok(()));
    }

    /// An N-way row on a host with fewer cores than consumers measures the scheduler
    /// even with a perfect clock. The fixture is `HostIndependent` so only the core
    /// budget can fail.
    ///
    /// Mutant: delete the `needs_n_cores && !enough_cores` arm.
    #[test]
    fn an_n_way_row_cannot_claim_measured_without_the_cores() {
        let mut r = skeleton(true, false);
        r.fitness.enough_cores = false;
        r.fitness.core_reason =
            Some("4 physical cores for 16 consumers plus a publisher (17 needed)".to_owned());
        let row = &mut r.rows[0];
        row.needs_n_cores = true;
        row.sensitivity = Sensitivity::HostIndependent;
        row.measured_as(Status::Measured, String::new());
        row.tf_tree = vec![Metric::new("cpu_pct", 3.0, "%").lower_is_better(0.20)];
        let errs = r
            .validate()
            .expect_err("short core budget must reject the claim");
        assert!(
            errs.iter().any(|e| e.contains("runs 16 consumers")),
            "{errs:?}"
        );
        assert!(errs.iter().any(|e| e.contains("17 needed")), "{errs:?}");

        // Converse: enough cores is fine.
        r.fitness.enough_cores = true;
        r.fitness.core_reason = None;
        assert_eq!(r.validate(), Ok(()));
    }

    /// A memory row must be allowed to claim `measured` on a host failing every *timing* check.
    ///
    /// Mutant: the `Memory` branch reads `fair_for_timing`.
    #[test]
    fn a_memory_row_is_measurable_on_a_host_that_only_fails_the_timing_checks() {
        let mut r = skeleton(false, false);
        assert!(!r.fitness.fair_for_timing, "fixture must be unfit to time");
        assert!(r.fitness.fair_for_memory, "fixture must be fit to weigh");

        let row = &mut r.rows[0];
        row.sensitivity = Sensitivity::Memory;
        row.measured_as(Status::Measured, String::new());
        row.tf_tree = vec![Metric::new("pss_kib", 4096.0, "KiB").lower_is_better(0.20)];
        assert_eq!(r.validate(), Ok(()));

        // Converse: a memory-axis failure (debug build) is refused.
        r.fitness.fair_for_memory = false;
        r.fitness.memory_reasons = vec!["built with debug assertions on".to_owned()];
        let errs = r
            .validate()
            .expect_err("a debug build must refuse a memory claim");
        assert!(
            errs.iter()
                .any(|e| e.contains("a memory figure") && e.contains("debug assertions")),
            "{errs:?}"
        );
    }

    /// The core budget must not reach a memory row (Pss is not scheduled).
    ///
    /// Mutant: drop `&& r.sensitivity != Sensitivity::Memory` from `core_budget_applies`.
    #[test]
    fn a_memory_row_does_not_need_the_core_budget() {
        let mut r = skeleton(true, false);
        r.fitness.enough_cores = false;
        r.fitness.core_reason =
            Some("4 physical cores for 16 consumers plus a publisher (17 needed)".to_owned());

        let row = &mut r.rows[0];
        row.needs_n_cores = true;
        row.sensitivity = Sensitivity::Memory;
        row.measured_as(Status::Measured, String::new());
        row.tf_tree = vec![Metric::new("total_pss_kib", 65536.0, "KiB").lower_is_better(0.20)];
        assert_eq!(r.validate(), Ok(()));

        // Not vacuous: the same row reporting a duration is refused.
        r.rows[0].sensitivity = Sensitivity::AbsoluteTiming;
        let errs = r
            .validate()
            .expect_err("a timing row must still want the cores");
        assert!(
            errs.iter().any(|e| e.contains("runs 16 consumers")),
            "{errs:?}"
        );
    }

    /// Which measured facts invalidate which kind of claim: SMT and governor are
    /// common-mode, a busy machine is not, a debug build reaches everything, an
    /// unreadable `smaps_rollup` reaches memory alone.
    ///
    /// Mutant: `fair_for_ratios: reasons.is_empty()` in `assess`.
    #[test]
    fn each_host_check_reaches_only_the_axes_it_bears_on() {
        // SMT and an unreadable governor on a quiet machine: common-mode between interleaved arms.
        let clock_only = Fitness::assess(2, 8, Some(4), 0.0, None, false, true);
        assert!(
            !clock_only.fair_for_timing,
            "reasons: {:?}",
            clock_only.reasons
        );
        assert!(
            clock_only.fair_for_ratios,
            "an interleaved ratio divides these out: {:?}",
            clock_only.ratio_reasons
        );
        assert!(
            clock_only.fair_for_memory,
            "Pss involves no clock: {:?}",
            clock_only.memory_reasons
        );
        assert!(
            clock_only.reasons.len() >= 2,
            "expected the SMT and governor reasons: {:?}",
            clock_only.reasons
        );

        // Load reaches a cross-engine ratio and flatters, so it must not be waved through.
        let busy = Fitness::assess(
            2,
            4,
            Some(4),
            0.9,
            Some(vec!["performance".to_owned()]),
            false,
            true,
        );
        assert!(!busy.fair_for_timing, "{:?}", busy.reasons);
        assert!(
            !busy.fair_for_ratios,
            "load does not cancel between a locking engine and a lock-free one: {:?}",
            busy.ratio_reasons
        );
        assert!(
            busy.fair_for_memory,
            "a busy machine does not change a page count: {:?}",
            busy.memory_reasons
        );

        // Unreadable smaps_rollup reaches memory only.
        let no_pss = Fitness::assess(
            2,
            4,
            Some(4),
            0.0,
            Some(vec!["performance".to_owned()]),
            false,
            false,
        );
        assert!(no_pss.fair_for_timing, "{:?}", no_pss.reasons);
        assert!(no_pss.fair_for_ratios, "{:?}", no_pss.ratio_reasons);
        assert!(
            !no_pss.fair_for_memory,
            "a Pss figure that cannot be read is not a Pss figure"
        );
        assert!(
            no_pss
                .memory_reasons
                .iter()
                .any(|r| r.contains("smaps_rollup")),
            "{:?}",
            no_pss.memory_reasons
        );

        // A debug build reaches all three.
        let debug = Fitness::assess(
            2,
            4,
            Some(4),
            0.0,
            Some(vec!["performance".to_owned()]),
            true,
            true,
        );
        assert!(!debug.fair_for_timing);
        assert!(!debug.fair_for_ratios, "{:?}", debug.ratio_reasons);
        assert!(!debug.fair_for_memory, "{:?}", debug.memory_reasons);
        assert!(
            debug.ratio_reasons.iter().any(|r| r.contains("debug")),
            "{:?}",
            debug.ratio_reasons
        );

        // A passing host passes all three.
        let good = Fitness::assess(
            2,
            4,
            Some(4),
            0.0,
            Some(vec!["performance".to_owned()]),
            false,
            true,
        );
        assert!(good.fair_for_timing, "{:?}", good.reasons);
        assert!(good.fair_for_ratios);
        assert!(good.fair_for_memory);
    }

    /// A required row may be unavailable but not dropped, and a gap must say why
    /// *and* name a command.
    ///
    /// Mutants: remove the `REQUIRED_ROWS` loop, the empty-`reason` check, or the empty-`reproduce` check.
    #[test]
    fn required_rows_cannot_be_dropped_and_gaps_must_be_actionable() {
        let mut r = skeleton(false, false);
        r.rows.retain(|row| row.id != "scaling_curve");
        let errs = r.validate().expect_err("a dropped required row must fail");
        assert!(errs.iter().any(|e| e.contains("scaling_curve")), "{errs:?}");

        let mut r = skeleton(false, false);
        r.rows[1].reason = "   ".to_owned();
        let errs = r.validate().expect_err("a silent gap must fail");
        assert!(
            errs.iter().any(|e| e.contains("with no reason")),
            "{errs:?}"
        );

        let mut r = skeleton(false, false);
        r.rows[2].reproduce = "";
        let errs = r.validate().expect_err("an unactionable gap must fail");
        assert!(
            errs.iter().any(|e| e.contains("names no command")),
            "{errs:?}"
        );
    }

    /// §9.3 bullet 1, the recorded half: QoS, executor and DDS facts are in the report
    /// or it is refused. One seeded violation per key. ("Identical" is not checkable;
    /// see `validate`.)
    ///
    /// Mutant: delete the `REQUIRED_FACTS` loop.
    #[test]
    fn bullet_1_the_middleware_facts_are_recorded_or_the_report_is_refused() {
        for key in ["dds_qos", "executor_config", "rmw_implementation"] {
            // Absent: the `push` line was deleted.
            let mut r = skeleton(false, false);
            r.provenance.facts.retain(|f| f.key != key);
            let errs = r
                .validate()
                .expect_err(&format!("a missing `{key}` must be refused"));
            assert!(
                errs.iter()
                    .any(|e| e.contains(key) && e.contains("no such fact")),
                "{key}: {errs:?}"
            );

            // Blank: `collect` writes `unknown`, never an empty string.
            let mut r = skeleton(false, false);
            for f in &mut r.provenance.facts {
                if f.key == key {
                    f.value = "   ".to_owned();
                }
            }
            let errs = r
                .validate()
                .expect_err(&format!("a blank `{key}` must be refused"));
            assert!(
                errs.iter()
                    .any(|e| e.contains(key) && e.contains("present but empty")),
                "{key}: {errs:?}"
            );
        }
    }

    /// §9.3 bullet 3: the facts it names, one seeded deletion each.
    ///
    /// Mutant: drop `"kernel"` from `REQUIRED_FACTS`.
    #[test]
    fn bullet_3_every_host_fact_the_spec_names_must_be_present() {
        // Spelled out, so shrinking the constant cannot shrink its test.
        for key in [
            "tf2_version",
            "ros_distro",
            "rmw_implementation",
            "kernel",
            "cpu_model",
            "transparent_hugepage",
            "transparent_hugepage_shmem",
        ] {
            let mut r = skeleton(false, false);
            r.provenance.facts.retain(|f| f.key != key);
            let errs = r
                .validate()
                .expect_err(&format!("a report with no `{key}` must be refused"));
            assert!(errs.iter().any(|e| e.contains(key)), "{key}: {errs:?}");
        }

        // Non-degenerate: the full report validates.
        assert_eq!(skeleton(false, false).validate(), Ok(()));
    }

    /// §9.3 bullet 2: a report that timed something and discarded nothing is refused,
    /// as is a non-numeric N. Scoped to the axes a warm-up is *for*.
    ///
    /// Mutant: delete the `warmup_discarded_s` clause.
    #[test]
    fn bullet_2_a_timed_claim_needs_a_stated_warm_up() {
        for sensitivity in [Sensitivity::AbsoluteTiming, Sensitivity::Ratio] {
            let mut r = skeleton(true, false);
            r.warmup_discarded_s = 0.0;
            let row = &mut r.rows[0];
            row.sensitivity = sensitivity;
            row.measured_as(Status::Measured, String::new());
            row.tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
            let errs = r
                .validate()
                .expect_err("a timed claim with no warm-up must be refused");
            assert!(
                errs.iter().any(|e| e.contains("warm-up of zero seconds")),
                "{sensitivity:?}: {errs:?}"
            );

            // With a warm-up it is fine.
            r.warmup_discarded_s = 2.0;
            assert_eq!(r.validate(), Ok(()), "{sensitivity:?}");
        }

        // A memory claim is exempt: no cold path in a page table.
        let mut r = skeleton(true, false);
        r.warmup_discarded_s = 0.0;
        let row = &mut r.rows[0];
        row.sensitivity = Sensitivity::Memory;
        row.measured_as(Status::Measured, String::new());
        row.tf_tree = vec![Metric::new("pss_kib", 4096.0, "KiB").lower_is_better(0.20)];
        assert_eq!(r.validate(), Ok(()));

        // N must be a number: `jnum` writes non-finite as `null`, reading as "none stated".
        for bad in [f64::NAN, -1.0, f64::INFINITY] {
            let mut r = skeleton(false, false);
            r.warmup_discarded_s = bad;
            let errs = r
                .validate()
                .expect_err("a warm-up that is not a duration must be refused");
            assert!(
                errs.iter()
                    .any(|e| e.contains("requires the discarded warm-up")),
                "{bad}: {errs:?}"
            );
        }
    }

    /// §9.3 bullet 5, mechanical half: every row names a command, whatever its status.
    ///
    /// Mutant: scope the reproduce check back to `Status::Unavailable`.
    #[test]
    fn bullet_5_a_measured_row_also_names_the_command_that_re_derives_it() {
        let mut r = skeleton(true, false);
        let row = &mut r.rows[0];
        row.measured_as(Status::Measured, String::new());
        row.reproduce = "";
        row.tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        let errs = r
            .validate()
            .expect_err("a measured row with no command must be refused");
        assert!(
            errs.iter()
                .any(|e| e.contains("names no command") && e.contains("measured")),
            "{errs:?}"
        );
    }

    /// A stale ground is refused while the prose reads perfectly. One seeded
    /// violation per decidable [`Ground`]; the reason string is untouched in each arm.
    ///
    /// Mutant: `Ground::holds` returns `None` for every variant.
    #[test]
    fn a_ground_that_no_longer_holds_is_refused_one_arm_at_a_time() {
        // `Tf2NotLinked`: true in the default build, false in the container's.
        let mut r = skeleton(false, false);
        r.build.tf2_linked = true;
        r.rows[0].grounds = vec![Ground::Tf2NotLinked];
        let errs = r
            .validate()
            .expect_err("a tf2 claim in a tf2 build must be refused");
        assert!(
            errs.iter().any(|e| e.contains("tf2_not_linked")),
            "{errs:?}"
        );

        // `FrozenBackendNotCompiled`: true under `just bench-report`, false under `bench-report-shm`.
        let mut r = skeleton(false, false);
        r.build.frozen_backend = true;
        r.rows[0].grounds = vec![Ground::FrozenBackendNotCompiled];
        let errs = r
            .validate()
            .expect_err("a frozen-backend claim in an shm build must be refused");
        assert!(
            errs.iter()
                .any(|e| e.contains("frozen_backend_not_compiled")),
            "{errs:?}"
        );

        // `HostFitness`: blaming the host on an axis it passes.
        let mut r = skeleton(true, false);
        r.rows[0].grounds = vec![Ground::HostFitness];
        let errs = r
            .validate()
            .expect_err("a fitness claim on a fit host must be refused");
        assert!(errs.iter().any(|e| e.contains("host_fitness")), "{errs:?}");

        // `HostFitness` again on a non-timing axis: the fixture splits timing from ratio,
        // so a mutant reading `fair_for_timing` is caught.
        let mut r = skeleton(false, false);
        assert!(
            !r.fitness.fair_for_timing && r.fitness.fair_for_ratios,
            "the fixture must split the two axes or this arm tests nothing"
        );
        r.rows[0].sensitivity = Sensitivity::Ratio;
        r.rows[0].grounds = vec![Ground::HostFitness];
        let errs = r
            .validate()
            .expect_err("a fitness claim on an axis the host passes must be refused");
        assert!(errs.iter().any(|e| e.contains("host_fitness")), "{errs:?}");

        // `HostCores` is a conjunction of three; first, the host has the cores.
        let mut r = skeleton(false, false);
        r.rows[0].needs_n_cores = true;
        r.rows[0].grounds = vec![Ground::HostCores];
        r.fitness.enough_cores = true;
        let errs = r
            .validate()
            .expect_err("a core-count claim on a wide host must be refused");
        assert!(errs.iter().any(|e| e.contains("host_cores")), "{errs:?}");

        // Second: a `Memory` row is exempt from the budget (`tft_16_workers_rss`).
        let mut r = skeleton(false, false);
        r.fitness.enough_cores = false;
        r.rows[0].needs_n_cores = true;
        r.rows[0].sensitivity = Sensitivity::Memory;
        r.rows[0].grounds = vec![Ground::HostCores];
        let errs = r
            .validate()
            .expect_err("a memory row may not blame the core budget");
        assert!(errs.iter().any(|e| e.contains("host_cores")), "{errs:?}");

        // Third: a row that runs no consumers may not blame the core budget; deleting
        // `&& row.needs_n_cores` from `Ground::holds` was otherwise green.
        let mut r = skeleton(false, false);
        r.fitness.enough_cores = false;
        r.rows[0].needs_n_cores = false;
        r.rows[0].sensitivity = Sensitivity::AbsoluteTiming;
        r.rows[0].grounds = vec![Ground::HostCores];
        let errs = r
            .validate()
            .expect_err("a row that runs no consumers may not blame the core budget");
        assert!(errs.iter().any(|e| e.contains("host_cores")), "{errs:?}");

        // Non-degenerate: the budget actually reaching the row validates.
        let mut ok = skeleton(false, false);
        ok.fitness.enough_cores = false;
        ok.rows[0].needs_n_cores = true;
        ok.rows[0].grounds = vec![Ground::HostCores];
        assert_eq!(ok.validate(), Ok(()));
    }

    /// An `unavailable` row rests on a stated ground, and a row with numbers keeps
    /// none (a leftover ground is an excuse nothing re-checks).
    ///
    /// Mutants: delete either check.
    #[test]
    fn a_gap_rests_on_a_ground_and_a_claim_carries_none() {
        let mut r = skeleton(false, false);
        r.rows[0].grounds.clear();
        let errs = r
            .validate()
            .expect_err("a reason resting on nothing must be refused");
        assert!(
            errs.iter().any(|e| e.contains("no stated ground")),
            "{errs:?}"
        );

        let mut r = skeleton(true, false);
        r.rows[0].status = Status::Measured;
        r.rows[0].reason = String::new();
        r.rows[0].tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        // Deliberately not `measured_as`, which clears the grounds.
        let errs = r
            .validate()
            .expect_err("a promoted row must drop its ground");
        assert!(
            errs.iter().any(|e| e.contains("still carries the ground")),
            "{errs:?}"
        );
    }

    /// §9.3 bullet 3's "THP setting" is two knobs, and the one that governs the arena
    /// is `shmem_enabled`. Values are compared against the sysfs files, so this passes
    /// on any configuration and fails if the two keys share a file.
    ///
    /// Mutant: point the `transparent_hugepage_shmem` push at `.../enabled`.
    #[test]
    fn both_transparent_hugepage_knobs_are_recorded() {
        let p = Provenance::collect();
        for (key, path) in [
            (
                "transparent_hugepage",
                "/sys/kernel/mm/transparent_hugepage/enabled",
            ),
            (
                "transparent_hugepage_shmem",
                "/sys/kernel/mm/transparent_hugepage/shmem_enabled",
            ),
        ] {
            let got = p.get(key).unwrap_or_else(|| panic!("no `{key}` fact"));
            let want = std::fs::read_to_string(path)
                .map_or_else(|_| "unknown".to_owned(), |s| s.trim().to_owned());
            assert_eq!(got, want, "`{key}` must be read from {path}");
        }
    }

    /// §9.2's required row set is one list, `REQUIRED_ROWS`. This counts rather than
    /// matches names (the table cells are prose titles), catching a row added to one
    /// list and not the other.
    ///
    /// Mutant: delete a row line from §9.2's table.
    #[test]
    fn the_required_row_set_is_the_size_of_phase5_section_9_2s_table() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/PHASE5.md"),
        )
        .expect("docs/PHASE5.md");
        let section = doc
            .split_once("### 9.2 Required rows")
            .expect("§9.2 must exist")
            .1;
        let table = section
            .split_once("| Measurement | Both stacks |")
            .expect("§9.2's table must keep its header")
            .1;
        let rows = table
            .lines()
            .map(str::trim)
            // Skip the emptied header remainder and the `|---|` separator; stop at the first non-row.
            .skip_while(|l| l.is_empty() || l.starts_with("|---"))
            .take_while(|l| l.starts_with('|'))
            .count();
        assert_eq!(
            rows,
            REQUIRED_ROWS.len(),
            "§9.2's table lists {rows} rows and `REQUIRED_ROWS` names {}: {:?}",
            REQUIRED_ROWS.len(),
            REQUIRED_ROWS
        );
    }

    /// `indicative` is the `TF_TREE_BENCH_FORCE=1` escape hatch only: invalid without
    /// the override, and invalid on a fit host.
    ///
    /// Mutant: delete the `!self.fitness.forced` check.
    #[test]
    fn indicative_requires_the_force_override_and_an_unfit_host() {
        let mut r = skeleton(false, false);
        r.rows[0].status = Status::Indicative;
        r.rows[0].tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        let errs = r.validate().expect_err("indicative without force");
        assert!(
            errs.iter().any(|e| e.contains("TF_TREE_BENCH_FORCE")),
            "{errs:?}"
        );

        let mut r = skeleton(true, true);
        r.rows[0].status = Status::Indicative;
        let errs = r.validate().expect_err("indicative on a fit host");
        assert!(
            errs.iter().any(|e| e.contains("passed the fitness probe")),
            "{errs:?}"
        );

        // Unfit + forced is the one allowed combination.
        let mut r = skeleton(false, true);
        r.rows[0].measured_as(Status::Indicative, "forced on an unfit host".to_owned());
        r.rows[0].tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        assert_eq!(r.validate(), Ok(()));
    }

    /// The four §9.3 "where we are worse" topics are required, and each must state the cost.
    ///
    /// Mutants: delete the `REQUIRED_WORSE` loop, or the empty-`statement` check.
    #[test]
    fn the_where_we_are_worse_entries_are_required_and_must_state_the_cost() {
        let dropped = REQUIRED_WORSE[0];
        let mut r = skeleton(false, false);
        r.worse.retain(|w| w.id != dropped);
        let errs = r.validate().expect_err("a dropped `worse` topic must fail");
        assert!(errs.iter().any(|e| e.contains(dropped)), "{errs:?}");

        // Present but empty is the likelier regression.
        let mut r = skeleton(false, false);
        r.worse[1].statement = "   ".to_owned();
        let errs = r
            .validate()
            .expect_err("a `worse` entry that says nothing must fail");
        assert!(
            errs.iter().any(|e| e.contains("states nothing")),
            "{errs:?}"
        );
        assert!(
            errs.iter().any(|e| e.contains(REQUIRED_WORSE[1])),
            "the violation must name the offending entry: {errs:?}"
        );
    }

    /// A `where_we_are_worse` entry whose numbers are all informational is refused
    /// (the row rule, applied to entries; `arena_memory_floor` is why). Scoped to a host
    /// whose memory axis passed, since [`worse_entries`] withholds Pss metrics otherwise.
    ///
    /// Mutant: delete the `fair_for_memory` conjunct and the direction arm.
    #[test]
    fn a_worse_entry_whose_numbers_are_all_informational_cannot_be_gated_and_fails() {
        let mut r = skeleton(true, false);
        r.worse[0].metrics = vec![
            Metric::new("idle_arena_bytes", 2_405_696.0, "B"),
            Metric::new("idle_arena_resident_bytes", 24_576.0, "B"),
        ];
        r.worse[0].metrics_absent_because = None;
        let errs = r
            .validate()
            .expect_err("an entry whose numbers cannot regress must fail");
        assert!(
            errs.iter().any(|e| {
                e.contains(REQUIRED_WORSE[0]) && e.contains("every one of them is informational")
            }),
            "the violation must name the entry: {errs:?}"
        );

        // One direction is enough.
        let mut ok = skeleton(true, false);
        ok.worse[0].metrics = vec![
            Metric::new("idle_arena_bytes", 2_405_696.0, "B"),
            Metric::new("idle_arena_resident_bytes", 24_576.0, "B")
                .lower_is_better(RESIDENCY_SLACK),
        ];
        ok.worse[0].metrics_absent_because = None;
        assert!(
            ok.validate().is_ok(),
            "one directional metric satisfies it: {:?}",
            ok.validate()
        );

        // A tolerance that makes the gate always or never fire is not a gate.
        let mut bad_tol = skeleton(true, false);
        bad_tol.worse[0].metrics =
            vec![Metric::new("idle_arena_resident_bytes", 24_576.0, "B").lower_is_better(-1.0)];
        bad_tol.worse[0].metrics_absent_because = None;
        let errs = bad_tol
            .validate()
            .expect_err("a negative tolerance must fail");
        assert!(errs.iter().any(|e| e.contains("either always")), "{errs:?}");

        // On a host that cannot weigh anything the rule stands down.
        let mut unfit = skeleton(true, false);
        unfit.fitness.fair_for_memory = false;
        unfit.fitness.memory_reasons = vec!["/proc/self/smaps_rollup is unreadable".to_owned()];
        unfit.worse[0].metrics = vec![Metric::new("idle_arena_bytes", 2_405_696.0, "B")];
        unfit.worse[0].metrics_absent_because = None;
        assert!(
            unfit.validate().is_ok(),
            "a host that cannot measure Pss must still get an artifact: {:?}",
            unfit.validate()
        );
    }

    /// The shipped `arena_memory_floor` entry carries a direction
    /// (`docs/decisions/0021` step 4).
    ///
    /// Uses `Fitness::assess`, not `probe`: a test binary always has
    /// `debug_assertions`, so `probe` would fail the memory axis and skip every
    /// assertion.
    ///
    /// Mutant: drop `.lower_is_better(RESIDENCY_SLACK)` from `worse_entries`.
    #[test]
    fn the_arena_memory_floor_entry_gates_its_residency_figure() {
        let opts = Options::default();
        // Release-run host facts, handed in rather than probed.
        let fitness = Fitness::assess(
            opts.consumers,
            32,
            Some(32),
            0.0,
            Some(vec!["performance".to_owned(); 32]),
            false,
            true,
        );
        // Non-degenerate: the axis this test needs must pass.
        assert!(
            fitness.fair_for_memory,
            "the axis this test needs must pass: {:?}",
            fitness.memory_reasons
        );
        let entries = worse_entries(&opts, &fitness);
        let floor = entries
            .iter()
            .find(|w| w.id == "arena_memory_floor")
            .expect("§9.3 requires the entry");
        let resident = floor
            .metrics
            .iter()
            .find(|m| m.key == "idle_arena_resident_bytes")
            .expect("a host whose memory axis passed must publish the figure");
        assert_eq!(
            resident.drift,
            Drift::LowerIsBetter,
            "the residency figure must be gated, not merely printed"
        );
        assert!(
            resident.tolerance.is_finite() && resident.tolerance > 0.0,
            "tolerance {} would make the gate always or never fire",
            resident.tolerance
        );
        let fraction = floor
            .metrics
            .iter()
            .find(|m| m.key == "idle_arena_resident_fraction")
            .expect("the fraction is published");
        assert_eq!(
            fraction.drift,
            Drift::Informational,
            "gating the quotient as well would be a second spelling of the same claim"
        );
    }

    /// A §9.3 entry with no numbers must say why, and one with numbers must not claim
    /// it has none.
    ///
    /// Mutants: delete the `(true, None | Some(""))` arm, or the `(false, Some(_))` arm.
    #[test]
    fn a_worse_entry_with_no_numbers_must_say_why() {
        let mut r = skeleton(false, false);
        r.worse[0].metrics_absent_because = None;
        let errs = r
            .validate()
            .expect_err("an unexplained empty metric list must fail");
        assert!(
            errs.iter()
                .any(|e| e.contains(REQUIRED_WORSE[0]) && e.contains("metrics_absent_because")),
            "the violation must name the entry and the missing field: {errs:?}"
        );

        // Whitespace is not an explanation.
        let mut r = skeleton(false, false);
        r.worse[0].metrics_absent_because = Some("  ".to_owned());
        assert!(r.validate().is_err(), "a blank reason must not satisfy it");

        // Numbers *and* a reason they are absent: one is stale.
        let mut r = skeleton(false, false);
        r.worse[0].metrics = vec![Metric::new("bytes", 1.0, "B")];
        let errs = r
            .validate()
            .expect_err("metrics beside a reason they are absent must fail");
        assert!(
            errs.iter().any(|e| e.contains("One of the two is wrong")),
            "{errs:?}"
        );

        // The real report satisfies the rule.
        let opts = Options::default();
        for w in worse_entries(&opts, &Fitness::probe(opts.consumers)) {
            assert_eq!(
                w.metrics.is_empty(),
                w.metrics_absent_because.is_some(),
                "`{}` must carry numbers or a reason it has none, never both and never \
                 neither",
                w.id
            );
        }
    }

    /// The remaining row rules, each isolated: a `measured` row carries numbers, an
    /// `indicative` row says why, an `unavailable` row carries none, and a required row
    /// is not counted twice. Each block picks the fitness that silences the other rules.
    ///
    /// Mutants: delete the corresponding check in `validate`.
    #[test]
    fn a_row_must_carry_exactly_the_evidence_its_status_claims() {
        // `measured` with nothing to show; a fit host silences the timing rule.
        let mut r = skeleton(true, false);
        r.rows[0].status = Status::Measured;
        r.rows[0].reason = String::new();
        let errs = r.validate().expect_err("measured with no numbers");
        assert!(
            errs.iter()
                .any(|e| e.contains("`measured` with no numbers")),
            "{errs:?}"
        );

        // `indicative` with no reason; unfit + forced silences the other two.
        let mut r = skeleton(false, true);
        r.rows[0].status = Status::Indicative;
        r.rows[0].reason = "  ".to_owned();
        r.rows[0].tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        let errs = r.validate().expect_err("indicative with no reason");
        assert!(
            errs.iter()
                .any(|e| e.contains("`indicative` with no reason")),
            "{errs:?}"
        );

        // `unavailable` carrying a number, in the `tf2` column so the `||`'s right operand is tested.
        let mut r = skeleton(false, false);
        r.rows[0].tf2 = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        let errs = r.validate().expect_err("unavailable carrying numbers");
        assert!(
            errs.iter().any(|e| e.contains("but carries numbers")),
            "{errs:?}"
        );

        // Duplicated required row: only the count arm catches it.
        let mut r = skeleton(false, false);
        let dup = r.rows[3].clone();
        r.rows.push(dup);
        let errs = r.validate().expect_err("a duplicated row must fail");
        assert!(
            errs.iter().any(|e| e.contains("appears 2 times")),
            "{errs:?}"
        );
    }

    /// §9.3 puts "where we are worse" in the same table as the results. Asserted
    /// structurally: the header and every topic fall inside the results `<table>`.
    ///
    /// Mutant: insert `s.push_str("</table>\n")` before the worse-entry block in `to_html`.
    #[test]
    fn worse_entries_render_inside_the_results_table() {
        let mut r = skeleton(false, false);
        r.worse[0].topic = "arena memory floor";
        let html = r.to_html();
        let start = html.find("<h2>Results</h2>").expect("results heading");
        let open = start + html[start..].find("<table>").expect("results <table>");
        let close = start + html[start..].find("</table>").expect("results </table>");
        assert!(open < close, "results table not found");
        let marker = html.find("Where tf_tree is worse").expect("worse header");
        assert!(
            open < marker && marker < close,
            "the `worse` section is outside the results table"
        );
        let topic = html.find("arena memory floor").expect("worse topic");
        assert!(
            open < topic && topic < close,
            "a `worse` topic is outside the results table"
        );
    }

    /// The JSON survives hostile reason characters and never emits `NaN`.
    ///
    /// Mutants: drop the `'"'` arm from `jstr`; make `jnum` print `{v}` unconditionally.
    #[test]
    fn json_escapes_hostile_reasons_and_never_emits_nan() {
        let mut r = skeleton(false, false);
        r.rows[0].reason = "a \"quoted\" reason\nwith a \\ and a \ttab".to_owned();
        r.rows[0].status = Status::Unavailable;
        r.worse[0].metrics = vec![Metric::new("ratio", f64::NAN, "x")];
        let json = r.to_json();
        assert!(json.contains("\\\"quoted\\\""), "{json}");
        assert!(json.contains("\\n"), "{json}");
        assert!(json.contains("\\\\"), "{json}");
        assert!(!json.contains("NaN"), "{json}");
        assert!(json.contains("\"value\": null"), "{json}");
        // Braces balance and the schema key is first; the version is spelled out so a bump costs a deliberate edit.
        assert!(json.starts_with("{\n  \"schema\": \"tf_tree.bench-report/2\""));
        let opens = json.matches('{').count();
        let closes = json.matches('}').count();
        assert_eq!(opens, closes, "unbalanced JSON braces");
    }

    /// The shipped rows' reasons, checked against the property rather than the wording.
    ///
    /// Every unavailable row rests on at least one [`Ground`] and every ground it names
    /// still holds. The test also asserts the shipped set has a **decidable** ground,
    /// else moving every row to `MeasuredElsewhere` would leave the rule green and
    /// checking nothing. The three-phrase scan is a subordinate check.
    ///
    /// Mutants: give `tft_16_workers_rss` `Ground::HostCores`; drop `.on(...)` from `lookup_latency`.
    #[test]
    fn no_unavailable_reason_rests_on_a_claim_that_has_gone_stale() {
        let opts = Options {
            lookup_samples: 1,
            differential_queries: 64,
            warmup: Duration::from_millis(1),
            ..Options::default()
        };
        let report = assemble(&opts).expect("assemble");

        // Non-degenerate: some rows must be unavailable.
        let unavailable: Vec<&Row> = report
            .rows
            .iter()
            .filter(|r| r.status == Status::Unavailable)
            .collect();
        assert!(
            unavailable.len() >= 2,
            "this host reports {} unavailable rows; the two .tft rows are unconditionally \
             unavailable, so fewer than two means the fixture stopped exercising the rule",
            unavailable.len()
        );

        let mut decidable = 0usize;
        for r in &unavailable {
            assert!(
                !r.reason.is_empty(),
                "row `{}` is unavailable with no reason at all (PHASE5 §9.3)",
                r.id
            );
            assert!(
                !r.grounds.is_empty(),
                "row `{}` is unavailable and rests on no stated ground, so nothing \
                 re-derives its reason",
                r.id
            );
            for g in &r.grounds {
                match g.holds(report.build, &report.fitness, r) {
                    Some(true) => decidable += 1,
                    Some(false) => panic!(
                        "row `{}` rests on `{}`, which no longer holds — decided by {}. \
                         Reason was: {}",
                        r.id,
                        g.as_str(),
                        g.decided_by(),
                        r.reason
                    ),
                    // The undecidable three must not satisfy `decidable > 0`.
                    None => {}
                }
            }

            // The old spelling scan, demoted.
            for phrase in ["is not implemented", "are not implemented", "unimplemented"] {
                assert!(
                    !r.reason.contains(phrase),
                    "row `{}` explains itself with `{phrase}`, which is a claim about the \
                     roadmap rather than about this host or this build, and is the exact \
                     statement that had gone stale. Reason was: {}",
                    r.id,
                    r.reason
                );
            }
        }

        assert!(
            decidable > 0,
            "no unavailable row in the shipped report rests on a ground `validate` can \
             re-derive, so the honesty rule is green and checking nothing"
        );
    }

    /// The timestamp routine has no external oracle, so it is pinned against known instants.
    ///
    /// Mutant: change `719_468` to `719_469`.
    #[test]
    fn iso8601_matches_known_instants() {
        let at = |s: u64| iso8601_utc(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(1_000_000_000), "2001-09-09T01:46:40Z");
        // 2024-02-29: a leap day.
        assert_eq!(at(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(at(1_735_689_599), "2024-12-31T23:59:59Z");
    }

    /// Every command the artifact tells a stranger to run must exist: the "Reproducing
    /// this" line, and the `reproduce:` field and reason of every row. Checked against
    /// the real `justfile`, `xtask` dispatch and target files, on the shipped rows
    /// from `assemble`.
    ///
    /// Mutants: put `cargo xtask bench-report` back in `to_html`; rename the
    /// `scaling_curve` recipe to `just tf2-scaling-curve`.
    #[test]
    fn every_command_the_report_names_is_a_command_that_exists() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let justfile = std::fs::read_to_string(root.join("justfile")).expect("justfile");
        let xtask = std::fs::read_to_string(root.join("xtask/src/main.rs")).expect("xtask main");

        // A `justfile` recipe is a column-0 line whose first token up to a space or colon is the name.
        let recipe_exists = |name: &str| {
            justfile.lines().any(|l| {
                !l.starts_with(char::is_whitespace)
                    && l.split([' ', ':']).next().is_some_and(|r| r == name)
            })
        };

        let mut checked = 0usize;
        let mut check = |text: &str, whence: &str| {
            // Strip HTML tags so `<code>just bench-report</code>` tokenises.
            let plain: String = {
                let mut out = String::with_capacity(text.len());
                let mut in_tag = false;
                for c in text.chars() {
                    match c {
                        '<' => in_tag = true,
                        '>' => {
                            in_tag = false;
                            out.push(' ');
                        }
                        _ if !in_tag => out.push(c),
                        _ => {}
                    }
                }
                out
            };
            let word = |t: &str| {
                t.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
                    .to_owned()
            };
            // The leading token needs its own trim: a reason spells a command in backticks
            // and `word` would also strip `--bench`'s dashes.
            let cmd = |t: &str| {
                t.trim_matches(|c: char| {
                    matches!(
                        c,
                        '`' | '*' | '(' | ')' | ',' | '.' | ';' | ':' | '"' | '\''
                    )
                })
                .to_owned()
            };
            let tok: Vec<String> = plain.split_whitespace().map(String::from).collect();
            for (i, t) in tok.iter().enumerate() {
                // Bind the trimmed token once; branching on the raw one mis-resolved backticked `--bench`.
                let key = cmd(t);
                match key.as_str() {
                    "just" => {
                        let name = word(tok.get(i + 1).map_or("", String::as_str));
                        assert!(
                            recipe_exists(&name),
                            "{whence} says `just {name}`, which is not a justfile recipe"
                        );
                        checked += 1;
                    }
                    "cargo" if tok.get(i + 1).map(|t| cmd(t)).as_deref() == Some("xtask") => {
                        let name = word(tok.get(i + 2).map_or("", String::as_str));
                        assert!(
                            xtask.contains(&format!("Some(\"{name}\")")),
                            "{whence} says `cargo xtask {name}`, which xtask does not dispatch"
                        );
                        checked += 1;
                    }
                    // `--bench X` / `--test X` name files cargo must find.
                    "--bench" | "--test" => {
                        let dir = if key == "--bench" { "benches" } else { "tests" };
                        let name = word(tok.get(i + 1).map_or("", String::as_str));
                        let path = root.join("crates/tf_tree_bench").join(dir);
                        assert!(
                            path.join(format!("{name}.rs")).exists(),
                            "{whence} says `{key} {name}`, but {} has no {name}.rs",
                            path.display()
                        );
                        checked += 1;
                    }
                    _ => {}
                }
            }
        };

        let opts = Options {
            lookup_samples: 1,
            differential_queries: 64,
            warmup: Duration::from_millis(1),
            ..Options::default()
        };
        let r = assemble(&opts).expect("assemble");
        for row in &r.rows {
            check(row.reproduce, row.id);
            // The reason too, not only `reproduce:`: recipes named in a reason (e.g.
            // `just bench-report` in `frozen_row_reason`) rot the same way.
            check(&row.reason, row.id);
        }
        let html = r.to_html();
        let block = html
            .split_once("<h2>Reproducing this</h2>")
            .expect("the report must tell a reader how to reproduce it")
            .1;
        check(block, "the `Reproducing this` block");

        // Guards the parser: if `check` matched nothing every assertion is vacuous. The
        // floor sits above the yield from `reproduce` and the HTML block alone and below
        // the yield with the reason scan, so deleting either fails; re-derive it by
        // substituting an unreachable floor and reading the panic.
        assert!(
            checked >= 20,
            "only {checked} commands were checked — the scanner matched nothing"
        );
    }

    /// Real `/proc/cpuinfo` fragments: x86-64 (two SMT siblings per core, four cores
    /// over two sockets; `core id` repeats per socket, so keying on it alone answers 2,
    /// not 4) and aarch64, which has no `physical id` or `core id`.
    /// answer 2 instead of 4.
    const X86_CPUINFO: &str = "\
processor\t: 0
physical id\t: 0
core id\t\t: 0
processor\t: 1
physical id\t: 0
core id\t\t: 1
processor\t: 2
physical id\t: 0
core id\t\t: 0
processor\t: 3
physical id\t: 0
core id\t\t: 1
processor\t: 4
physical id\t: 1
core id\t\t: 0
processor\t: 5
physical id\t: 1
core id\t\t: 1
";
    const AARCH64_CPUINFO: &str = "\
processor\t: 0
BogoMIPS\t: 50.00
Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32
CPU implementer\t: 0x41
CPU part\t: 0xd0c
processor\t: 1
BogoMIPS\t: 50.00
CPU implementer\t: 0x41
CPU part\t: 0xd0c
";

    /// aarch64 and many containers publish no core ids: the parse must say so rather
    /// than answer with the logical CPU count.
    ///
    /// Mutant: fall back to `available_parallelism()` instead of `None`.
    #[test]
    fn a_host_that_publishes_no_core_ids_is_unknown_not_guessed() {
        assert_eq!(physical_cores_from_cpuinfo(X86_CPUINFO), Some(4));
        assert_eq!(physical_cores_from_cpuinfo(AARCH64_CPUINFO), None);
        assert_eq!(physical_cores_from_cpuinfo(""), None);
    }

    /// Verdicts degrade honestly when the physical core count is unknown.
    /// `debug_build: false` so the host facts decide `fair_for_timing`.
    ///
    /// Mutant: drop both `unknown_cores` branches in `assess`.
    #[test]
    fn an_unknown_physical_core_count_fails_both_verdicts_and_says_why() {
        // Control: a known-good host, so the assertions below are not passing because `assess` refuses everything.
        let ok = Fitness::assess(
            4,
            8,
            Some(8),
            0.01,
            Some(vec!["performance".to_owned(); 8]),
            false,
            true,
        );
        assert!(ok.fair_for_timing, "{:?}", ok.reasons);
        assert!(ok.enough_cores, "{:?}", ok.core_reason);
        assert!(ok.physical_cores_known);

        // Same host, no core ids.
        let blind = Fitness::assess(
            4,
            8,
            None,
            0.01,
            Some(vec!["performance".to_owned(); 8]),
            false,
            true,
        );
        assert!(!blind.physical_cores_known);
        assert!(
            !blind.fair_for_timing,
            "an unmeasured host must not pass the clock verdict"
        );
        assert!(
            blind
                .reasons
                .iter()
                .any(|r| r.contains("physical core count is unknown")),
            "{:?}",
            blind.reasons
        );
        assert!(
            !blind.enough_cores,
            "a core budget checked against SMT siblings is not a budget check"
        );

        // The SMT reason goes quiet under a silent fallback, so pin it separately.
        let smt = Fitness::assess(
            2,
            8,
            Some(4),
            0.01,
            Some(vec!["performance".to_owned(); 8]),
            false,
            true,
        );
        assert!(
            smt.reasons.iter().any(|r| r.contains("SMT is on")),
            "{:?}",
            smt.reasons
        );
    }

    /// `--consumers` is operator input: `consumers + 1` wraps to 0 at `usize::MAX` in
    /// release and would print the core budget as PASS.
    ///
    /// Mutant: restore `let needed = consumers + 1;`.
    #[test]
    fn an_absurd_consumer_count_still_refuses_the_core_budget() {
        let f = Fitness::assess(
            usize::MAX,
            8,
            Some(8),
            0.01,
            Some(vec!["performance".to_owned(); 8]),
            false,
            true,
        );
        assert!(
            !f.enough_cores,
            "8 physical cores cannot host usize::MAX consumers"
        );
        assert!(
            f.core_reason
                .as_deref()
                .is_some_and(|r| r.contains("physical cores for")),
            "{:?}",
            f.core_reason
        );
    }

    /// `measure_lookup_latency` is the only real measurement here and `assemble`
    /// reaches it only on a fit host, which running the suite prevents; so it is called
    /// directly at a cheap sample count.
    ///
    /// Mutants: bound the loop at `samples.min(100)`; swap the `p50_ns` and `p999_ns` rows.
    #[test]
    fn the_lookup_measurement_reports_every_sample_and_ordered_percentiles() {
        const SAMPLES: usize = 4_096;
        let m = measure_lookup_latency(SAMPLES, Duration::from_millis(5))
            .expect("the fixture must measure");
        let get = |k: &str| {
            m.iter()
                .find(|x| x.key == k)
                .unwrap_or_else(|| panic!("metric `{k}` is missing from {m:?}"))
                .value
        };

        // Every sample must reach the histogram.
        assert_eq!(get("samples"), SAMPLES as f64);

        let (p50, p99, p999) = (get("p50_ns"), get("p99_ns"), get("p999_ns"));
        assert!(p50 <= p99 && p99 <= p999, "p50={p50} p99={p99} p999={p999}");
        // A p50 of 0 ns would be a constant, not a measurement.
        assert!(p50 > 0.0, "p50 of {p50} ns is not a measurement");
        // A millisecond median means a wrong unit or fixture; loose enough for a contended runner.
        assert!(p50 < 1_000_000.0, "p50 of {p50} ns is not a depth-3 lookup");
        assert!(get("clock_overhead_p50_ns") >= 0.0);
        assert!(m.iter().all(|x| x.value.is_finite()), "{m:?}");
    }

    /// [`LOOKUP_NOTE`] states what `docs/PHASE1.md` §11.3 requires, about the plan
    /// `measure_lookup_latency` compiles. The step count is read from the compiled plan,
    /// so changing the fixture or the note's number fails it.
    ///
    /// Mutants: write "**2 dynamic steps**" in the note (assertion 1); set
    /// [`LOOKUP_STAMP_NS`] back to `crate::fixture::NOW_NS` (assertion 2, not the loop);
    /// move `fixture::QUERY_NS` to `NOW_NS - 1_000_000`, on the 1 kHz grid (assertion 3).
    #[test]
    fn the_lookup_row_note_states_what_phase1_requires() {
        // 1. The dynamic-step count, from the plan.
        let tree = crate::fixture::build_tree_with(InterpPolicy::LerpSlerp).expect("fixture");
        let target = tree.frame("imu_link").expect("target frame");
        let source = tree.frame("map").expect("source frame");
        let plan = tree.plan(target, source).expect("plan");
        let steps = crate::workload::dyn_steps(&plan);
        let stated = format!("**{steps} dynamic steps**");
        assert!(
            LOOKUP_NOTE.contains(&stated),
            "PHASE1 §11.3 requires this row to state its dynamic-step count, and \
             the plan `map <- imu_link` compiles to {steps} dynamic steps, so the \
             note must contain {stated:?}. It reads: {LOOKUP_NOTE}"
        );

        // 2. The note names a stamp, and it must be the one the loop queries.
        assert!(
            LOOKUP_NOTE.contains("fixture::QUERY_NS"),
            "the note must name the stamp it was taken at: {LOOKUP_NOTE}"
        );
        assert_eq!(
            LOOKUP_STAMP_NS,
            crate::fixture::QUERY_NS,
            "the note names fixture::QUERY_NS and `measure_lookup_latency` \
             queries something else"
        );

        // 3. "off-grid" must be true of the stamp; this guards the note's claim, `fixture` the constant.
        assert!(
            LOOKUP_NOTE.contains("off-grid"),
            "the note must state the stamp regime: {LOOKUP_NOTE}"
        );
        assert!(!crate::fixture::DYNAMIC_EDGES.is_empty());
        for &(parent, child, rate_hz) in crate::fixture::DYNAMIC_EDGES {
            let period_ns = (1e9 / rate_hz) as i64;
            assert_ne!(
                LOOKUP_STAMP_NS % period_ns,
                0,
                "{parent}->{child}: this row's stamp lands on the {rate_hz} Hz \
                 grid, so that edge takes the exact-hit branch and the note's \
                 \"off-grid … so the interpolator runs\" is false"
            );
        }
    }

    /// The all-clear host: no obstacle at all. `assemble` probes and this host has four
    /// cores, so `core_reason` was always `Some`; on a host with none, `host_grounds`
    /// came out empty and `validate` rejected the whole report. The fix is the seeded
    /// `Ground::MeasuredElsewhere` in `assemble_on`.
    ///
    /// Mutant: replace `n_way_grounds` with `host_grounds.clone()`.
    #[test]
    fn a_host_with_no_obstacle_still_grounds_every_n_way_row() {
        const N: usize = 4;
        let fitness = Fitness::assess(
            N,
            32,
            Some(32),
            0.0,
            Some(vec!["performance".to_owned(); 32]),
            false,
            true,
        );
        // Non-degenerate: the all-clear host really is one.
        assert!(fitness.fair_for_timing, "{:?}", fitness.reasons);
        assert!(fitness.fair_for_ratios && fitness.fair_for_memory);
        assert!(fitness.enough_cores && fitness.core_reason.is_none());

        let opts = Options {
            lookup_samples: 2_000,
            differential_queries: 512,
            warmup: Duration::from_millis(10),
            consumers: N,
            ..Options::default()
        };
        // `Build::current()`, not a fabricated `Build`: the `#[cfg]`-selected arms cannot
        // be talked out of their build. The ROS flag is read from the environment, so it is handed in.
        let r = assemble_on(&opts, fitness, Build::current(), true).expect("assemble");

        // The ground each row must carry on any host; `publish_to_visible` may not say `MeasuredElsewhere`.
        for (id, standing) in [
            ("cpu_per_consumer", Ground::MeasuredElsewhere),
            ("scaling_curve", Ground::MeasuredElsewhere),
            ("publish_to_visible", Ground::NoInstrument),
        ] {
            let row = r.rows.iter().find(|row| row.id == id).expect(id);
            assert_eq!(row.status, Status::Unavailable, "row `{id}`");
            assert!(
                row.grounds.contains(&standing),
                "row `{id}` is unavailable on an all-clear host carrying grounds {:?}",
                row.grounds
            );
            // The prose half: the reason used to read "no obstacle was found".
            assert!(
                !row.reason.contains("no obstacle"),
                "row `{id}` explains its absence with: {}",
                row.reason
            );
            assert!(
                row.reason.contains("single process"),
                "row `{id}` does not name the standing gap: {}",
                row.reason
            );
        }

        // The row with no instrument may not borrow the others' ground.
        let ptv = r
            .rows
            .iter()
            .find(|row| row.id == "publish_to_visible")
            .expect("publish_to_visible");
        assert!(
            !ptv.grounds.contains(&Ground::MeasuredElsewhere),
            "`publish_to_visible` claims its number is measured elsewhere: {:?}",
            ptv.grounds
        );

        // A fit host produces a report this tool will write.
        assert_eq!(r.validate(), Ok(()));
    }

    /// End-to-end: the report assembled on *this* host passes its own §9.3 validation
    /// (the skeleton tests use hand-built reports; only this notices `assemble` disagreeing).
    ///
    /// Mutant: give `lookup_latency` `Status::Measured` before the `timing_status` match.
    #[test]
    fn the_assembled_report_passes_its_own_validation() {
        let opts = Options {
            // Small: `assemble`'s structure is under test, not the sample size.
            lookup_samples: 2_000,
            differential_queries: 512,
            warmup: Duration::from_millis(10),
            ..Options::default()
        };
        let r = assemble(&opts).expect("assemble");
        assert_eq!(r.validate(), Ok(()));

        // The differential row is a claim everywhere.
        let diff = r
            .rows
            .iter()
            .find(|row| row.id == "differential_agreement")
            .expect("differential row");
        assert_eq!(diff.status, Status::Measured);
        assert!(!diff.timing_sensitive());
        let compared = diff
            .tf_tree
            .iter()
            .find(|m| m.key == "compared")
            .expect("compared metric");
        // Pinning `compared` makes the row non-degenerate.
        assert!(
            compared.value > 100.0,
            "only {} queries scored",
            compared.value
        );

        // §9.3's "say why" means the actual why: this row is single-threaded, in-process.
        let lookup = r
            .rows
            .iter()
            .find(|row| row.id == "lookup_latency")
            .expect("lookup row");
        if lookup.status == Status::Unavailable {
            assert!(
                lookup.reason.contains("failed the fitness probe"),
                "{}",
                lookup.reason
            );
            assert!(!lookup.reason.contains("consumers plus a publisher"));
            assert!(!lookup.reason.contains("no ROS 2 in this build"));
        }
    }

    /// A pair that cannot resolve §9.2's 5% is `unavailable` even on a fit host: a band
    /// straddling 1.05 gets no verdict and no numbers.
    ///
    /// Mutant: delete the `Verdict::Unresolved` early return in `embedding_row`.
    #[test]
    fn a_pair_that_cannot_resolve_five_percent_gets_no_verdict() {
        let fit = skeleton(true, false).fitness;
        assert_eq!(
            fit.timing_status(),
            Status::Measured,
            "fixture must be fair"
        );

        let dir = std::env::temp_dir().join(format!("tf-tree-embed-row-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let write = |name: &str, profile: &str, lo: f64, hi: f64| {
            let run = crate::embed::Run {
                profile_dir: profile.to_owned(),
                source_id: "0123456789abcdef".to_owned(),
                out_of_crate_ns: 210.0,
                in_crate_ns: 200.0,
                boundary_ratio: 1.05,
                ratio_lo: lo,
                ratio_hi: hi,
                out_of_crate_spread: 0.09,
                in_crate_spread: 0.09,
                rounds: crate::embed::ROUNDS,
                lookups_per_round: 409_600,
            };
            std::fs::write(dir.join(format!("{name}.json")), run.to_json()).expect("write");
        };
        // Rounds landed on both sides of 1.05.
        write("embedder", crate::embed::EMBEDDER_PROFILE, 1.01, 1.09);
        write("release", crate::embed::REFERENCE_PROFILE, 1.01, 1.09);

        let opts = Options {
            embed_cost: Some(dir.clone()),
            ..Options::default()
        };
        let row = embedding_row(&opts, &fit).expect("row");
        assert_eq!(row.status, Status::Unavailable, "reason: {}", row.reason);
        assert!(
            row.tf_tree.is_empty(),
            "an unresolved row must carry no numbers"
        );
        assert!(
            row.reason.contains("cannot answer"),
            "the reason must say the band could not resolve it: {}",
            row.reason
        );

        // The same pair, measured tightly enough, is a claim.
        write("embedder", crate::embed::EMBEDDER_PROFILE, 1.049, 1.05);
        let row = embedding_row(&opts, &fit).expect("row");
        assert_eq!(row.status, Status::Measured, "reason: {}", row.reason);
        assert!(!row.tf_tree.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The row's note states the two profiles' settings, read back here from the
    /// workspace manifest.
    ///
    /// Mutant: set `[profile.embedder]`'s `codegen-units` to `8`.
    #[test]
    fn the_row_note_states_the_settings_the_manifest_declares() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest =
            std::fs::read_to_string(root.join("Cargo.toml")).expect("workspace manifest");
        let (lto, cgu) =
            crate::embed::profile_settings_from_manifest(&manifest, crate::embed::EMBEDDER_PROFILE)
                .expect("[profile.embedder]");
        let stated = format!("lto = {lto}, codegen-units = {cgu}");
        assert!(
            EMBEDDING_NOTE.contains(&stated),
            "the row note does not state `{stated}`, which is what \
             [profile.embedder] now declares"
        );

        // The control profile whose `lto` is why this binary cannot measure the row.
        let (rel_lto, _) = crate::embed::profile_settings_from_manifest(
            &manifest,
            crate::embed::REFERENCE_PROFILE,
        )
        .expect("[profile.release]");
        let stated_rel = format!("lto = {rel_lto}");
        assert!(
            EMBEDDING_NOTE.contains(&stated_rel),
            "the row note does not state `{stated_rel}`, which is what \
             [profile.release] now declares"
        );
    }

    /// `build_profile` names the directory cargo built into, not a guess from `cfg!(debug_assertions)`.
    ///
    /// Mutant: hardcode `"release"`. Reinstating the old `cfg!` guess passes under
    /// `nextest` (both say `debug`); only building the tests at a third profile
    /// (`--cargo-profile embedder -E 'test(build_profile)'`) catches it.
    #[test]
    fn the_build_profile_fact_is_the_directory_cargo_built_into() {
        let p = Provenance::collect();
        assert_eq!(
            p.get("build_profile"),
            Some(crate::embed::PROFILE_DIR),
            "the provenance profile must be the one `build.rs` measured"
        );
    }

    /// The profile's meaning travels beside its name: `build_lto` is emitted from the
    /// manifest for the profile `build_profile` names.
    ///
    /// Mutant: make `build_lto()` query `REFERENCE_PROFILE` instead of `PROFILE_DIR`.
    #[test]
    fn the_build_lto_fact_is_the_one_the_manifest_declares_for_that_profile() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest =
            std::fs::read_to_string(root.join("Cargo.toml")).expect("workspace manifest");
        let p = Provenance::collect();
        let dir = p.get("build_profile").expect("build_profile");
        assert_eq!(
            p.get("build_lto"),
            Some(crate::embed::lto_for_profile_dir(&manifest, dir).as_str())
        );
    }
}
