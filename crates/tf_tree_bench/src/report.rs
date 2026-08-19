//! The `docs/PHASE5.md` §9 benchmark artifact: one reproducible report.
//!
//! §9.1 asks for a *product, not a script* — a single command that emits
//! `results.json` (stable schema, CI-diffable), `index.html`, and the exact
//! environment description needed to reproduce it. §9.2 lists the rows it must
//! carry. §9.3, which is normative, governs everything here:
//!
//! > If a row cannot be measured fairly, omit it and say why. An honest gap is
//! > worth more than a favourable number nobody trusts.
//!
//! # Why this module is mostly *refusal* machinery
//!
//! Most of §9.2's rows are comparisons against a running `tf2`, on a host with
//! at least as many spare cores as consumers. The measurement code for them
//! already exists — `just mp-bench`, `just mp-bench-tf2`, `just tf2-scaling`,
//! `just footprint`, `just shm-scaling`, `crates/tf_tree_c/examples/abi_cost.rs`
//! — and this module deliberately does not reimplement any of it. What did not
//! exist is the thing §9.3 actually asks for: a report that **cannot** print a
//! number it has no right to.
//!
//! So the honesty is structural rather than editorial:
//!
//! * [`Fitness::probe`] measures the host and decides whether a timing number
//!   taken here would describe this engine or somebody else's scheduler. It is
//!   the *tool* that decides, from measured facts, not a hardcoded verdict — on
//!   a machine that qualifies, the same binary emits the number.
//! * [`Report::validate`] refuses to emit a report whose rows overclaim: a
//!   timing row cannot be [`Status::Measured`] on a host that failed the
//!   fitness probe, an unavailable row must carry a reason *and* the command
//!   that would produce it elsewhere, §9.3's four "where we are worse"
//!   topics must all be present, and each of those must carry either a number
//!   or a stated reason it has none ([`Worse::metrics_absent_because`] — an
//!   honesty section that cannot regress is the failure it is written against). The binary treats a validation failure as a
//!   hard error, so the failure mode is "no report" rather than "a flattering
//!   report".
//! * [`Status::Indicative`] exists because `TF_TREE_BENCH_FORCE=1` already
//!   exists (`crate::mp::require_quiet_machine`). Someone who overrides the
//!   refusal gets numbers that are labelled, in the JSON and in the HTML, as
//!   *not a claim*, together with the reasons the host failed.
//!
//! `String` appears freely in these types. That is not a hot path and not an
//! error type in the sense `CLAUDE.md` forbids: the reasons embed measured host
//! facts, and a report whose reasons are `&'static str` could not name the core
//! count it actually found.
//!
//! # Schema stability
//!
//! `results.json` is emitted by hand (`to_json`) rather than by a serialiser,
//! for one reason worth the code: the schema is a compatibility surface — §12
//! gate 7 diffs it across machines — and hand-writing it makes a field rename a
//! deliberate edit in one place instead of a side effect of a `#[derive]`.
//! `SCHEMA` is the version; bump it when a consumer would break.

use std::fmt::Write as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use tf_tree::{InterpPolicy, Stamp};

/// `results.json` schema identifier. Bump on any consumer-visible change.
///
/// `/2` added `drift` and `tolerance` to every metric. `/1` emitted a bare
/// `{"value", "unit"}`, which meant a consumer — including
/// [`crate::baseline`], the regression gate §10 asks for — could not tell
/// whether `compared: 47922` growing was an improvement, a regression, or
/// nothing at all. A regression gate over untyped numbers is a coin flip with
/// extra steps, so the direction is now part of the artifact rather than
/// knowledge held by whoever wrote the checker.
pub const SCHEMA: &str = "tf_tree.bench-report/2";

/// The command that regenerates the whole report directory.
///
/// Named once, so the HTML's "Reproducing this" line and the test that checks it
/// against the real `justfile` cannot disagree. It is **not**
/// `cargo xtask bench-report`: `xtask` dispatches `loom | miri | bench-gate |
/// headers` and nothing else, so that spelling exits non-zero. In an artifact
/// whose whole thesis is that a benchmark nobody can reproduce persuades nobody,
/// the reproduce line is the last place a wrong command can be afforded.
pub const REPRODUCE_RECIPE: &str = "just bench-report";

/// The row ids `docs/PHASE5.md` §9.2 requires the report to carry.
///
/// A row may be [`Status::Unavailable`], but it may not be *missing*: a report
/// that silently dropped the rows it could not measure would read as a clean
/// sweep of the ones it could.
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

/// Relative slack the regression gate allows on the tf2 ratio row, as a
/// fraction of the committed baseline.
///
/// Wider than it looks necessary, and deliberately: the measured within-run band
/// is ~3%, but the *between-build* movement of a ratio also carries whatever the
/// container's toolchain does to either arm, and the row exists to catch an
/// engine regression rather than to police a codegen difference. 15% below the
/// baseline ratio is a real regression at this magnitude — 2.46x would have to
/// fall past 2.09x, which is nearly to the floor itself.
#[cfg(feature = "tf2")]
const RATIO_SLACK: f64 = 0.15;

/// Relative slack the regression gate allows on the differential row's
/// `max_deviation`, as a fraction of the committed baseline: `9.0` means the
/// gate fires above `baseline * (1 + 9)`, i.e. **10x** the baseline.
///
/// **10x, which reads loose and is not.** The measured deviation on this host is
/// ~2.5e-16 rad/m — a handful of f64 ULPs — against the row's own pass tolerance
/// of 1e-12. A tight relative bound on a quantity that close to machine epsilon
/// gates the *compiler*: a rustc upgrade that reassociates one FMA moves it by a
/// factor of two while the engine is unchanged, and a gate that cries wolf on
/// toolchain bumps is a gate that gets its baseline regenerated without anyone
/// reading the diff.
///
/// What this bound is for is the failure that matters: a real disagreement — a
/// dropped normalization, a wrong interpolation branch, a quaternion sign flip —
/// lands at 1e-3 or worse, thirteen orders above the ceiling this sets. It also
/// still leaves ~2.5e-15, three orders *below* the pass tolerance, so the gate
/// fires long before the differential itself would.
pub const DEVIATION_SLACK: f64 = 9.0;

/// Relative slack the regression gate allows on a latency percentile.
///
/// 25%. These rows are only ever [`Status::Measured`] on a host that passed
/// [`Fitness::probe`] — quiet, no SMT, a readable governor — and the gate only
/// compares a baseline taken on that same host to a run on it. Even there a
/// p99.9 moves several percent run to run from page placement and interrupt
/// timing alone, so a 10% bound would flap. 25% is above that noise and well
/// under the size of any regression worth a bisect: the changes this is written
/// against — an extra atomic in the read path, a lost inline, a bounds check
/// back in the bracket search — cost tens of percent or more.
pub const LATENCY_SLACK: f64 = 0.25;

/// The "where `tf_tree` is worse" topics `docs/PHASE5.md` §9.3 names, verbatim.
pub const REQUIRED_WORSE: &[&str] = &[
    "arena_memory_floor",
    "attach_latency",
    "format_bump_cost",
    "bridge_supervision",
];

/// Which way a metric is allowed to move before it is a regression.
///
/// This exists for [`crate::baseline`]. Without it the regression gate would
/// have to infer intent from key names — `p99_ns` down, `samples` neither,
/// `throughput` up — and a checker that guesses is a checker that will one day
/// pass a doubled latency because somebody named a field `ops_ns`.
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
    /// A metric with the given key, value and unit, **informational**.
    ///
    /// Informational is the default because most report numbers are context,
    /// and because a wrong direction is worse than none: a metric silently
    /// typed `LowerIsBetter` when it is really a count would fail the gate on
    /// every run that scored more queries. The omission is not silent —
    /// [`Report::validate`] refuses a row that claims to be `measured` while
    /// carrying nothing directional, so a new claim cannot arrive ungated.
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

/// What kind of host fitness a row's numbers actually depend on.
///
/// **One boolean could not answer this, and pretending it could is what left
/// rows unavailable for reasons that were not about them.** An absolute
/// duration, a paired ratio and a resident-memory figure fail for different
/// facts about a machine, so they are asked different questions:
///
/// - a **frequency governor** moves an absolute latency and cancels out of a
///   ratio measured by interleaving both engines inside one round;
/// - **SMT** makes a per-thread duration depend on the sibling, and again
///   cancels when the two arms are interleaved on the same thread;
/// - a **busy machine** does *not* cancel out of a cross-engine ratio, and this
///   is the exception worth knowing: cancellation needs the disturbance to land
///   on both arms alike, and these arms are asymmetric by construction —
///   `tf2::BufferCore` locks on every lookup and `tf_tree` does not, so load adds
///   lock-holder preemption and convoying to one arm only. It inflates the
///   quotient in our favour rather than adding noise to it;
/// - **PSS does not involve a clock at all** — it is proportional set size read
///   out of `/proc`, and neither the governor nor a noisy neighbour changes how
///   many pages a process has resident. What it does need is to be *readable*:
///   `smaps_rollup` is absent on non-Linux and on some hardened containers, and
///   a silent zero there would be a false PASS.
///
/// What every one of them *does* depend on is being a release build, because a
/// debug build is a different program rather than a slower one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    /// The same inputs give the same answer on any host — a differential
    /// deviation, or arithmetic on the arena layout. Nothing to check.
    HostIndependent,
    /// An absolute duration. Needs a trustworthy clock *and* a quiet machine:
    /// every check in [`Fitness::reasons`] applies.
    AbsoluteTiming,
    /// A ratio between two engines measured by interleaving them within each
    /// round and taking the median of per-round quotients. Common-mode drift —
    /// governor, SMT — lands on both arms and divides out, which is why
    /// `just cpp-bench`'s gate went from flapping to a stable 1.006× when it
    /// started interleaving. A debug build and a busy machine still invalidate
    /// it; see the type's own docs for why load is not common-mode here.
    ///
    /// **No row constructs this yet.** It is the mechanism the `PHASE5.md` §9.2
    /// tf2-comparison rows are intended to move onto, so that a 4-core host can
    /// gate a *ratio* against tf2 even where it cannot gate either side's
    /// absolute latency. `embedding_cross_crate` is the nearest existing
    /// candidate and deliberately is **not** one: §9.2 gates its two absolute
    /// durations as well as their quotient, so it needs the stricter axis.
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
            reproduce,
            tf_tree: Vec::new(),
            tf2: Vec::new(),
        }
    }

    /// Mark the row as running `consumers` processes or threads at once.
    #[must_use]
    pub fn n_way(mut self) -> Row {
        self.needs_n_cores = true;
        self
    }

    /// Whether this row reports an absolute duration.
    ///
    /// Kept as the JSON field of the same name so `tf_tree.bench-report/2` does
    /// not change shape: it means exactly what it always meant, and the two
    /// non-timing sensitivities were previously spelled `false` here alongside
    /// [`Sensitivity::HostIndependent`].
    #[must_use]
    pub fn timing_sensitive(&self) -> bool {
        matches!(self.sensitivity, Sensitivity::AbsoluteTiming)
    }

    /// The status this row should carry on `fitness`, from its sensitivity.
    ///
    /// The core budget is applied on top by the caller when [`Row::needs_n_cores`]
    /// is set; it is a separate question from whether the number is trustworthy.
    #[must_use]
    pub fn status_on(&self, fitness: &Fitness) -> Status {
        let (fair, _, _) = fitness.axis(self.sensitivity);
        Fitness::status_from(fair, fitness.forced)
    }
}

/// One §9.3 "where `tf_tree` is worse" entry.
///
/// §9.3 puts these "in the same table and not in a footnote", so
/// [`Report::to_html`] renders them inside the results table.
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
    /// Why [`Self::metrics`] is empty — required whenever it is.
    ///
    /// **An honesty section that cannot regress is the problem this field
    /// exists to close.** Two of the four §9.3 entries carried
    /// `metrics: Vec::new()` for their whole life, and an empty vector is
    /// indistinguishable from an oversight: nobody reading the report can tell
    /// "this cost has no number *because*..." from "somebody forgot". Both
    /// readings are bad, and only one of them is true of any given entry.
    ///
    /// [`Report::validate`] enforces the pair in both directions — empty
    /// metrics need a reason, and a reason beside metrics is a contradiction —
    /// so this is the same structural honesty [`Row::unavailable`] already has,
    /// applied to the section whose job is to be quotable against us.
    ///
    /// Two shapes of reason are legitimate, and the entries here use one each:
    /// the cost is measured *elsewhere*, by a recipe this binary cannot run
    /// (`bridge_supervision`); or the cost is genuinely not denominated in
    /// nanoseconds or bytes at all (`format_bump_cost`). "Nobody has got round
    /// to it" is not one of them — that is what `attach_latency` used to be,
    /// and the answer was to go and measure it.
    pub metrics_absent_because: Option<String>,
}

/// Whether this host can produce a timing number that means anything.
///
/// **Two independent verdicts, deliberately not merged.** Whether a clock
/// reading is trustworthy (`fair_for_timing`) and whether the machine has room
/// for N consumers plus a publisher (`enough_cores`) are different questions,
/// and folding them into one boolean makes every stated reason wrong for half
/// the rows: a single-threaded in-process lookup does not want seventeen cores,
/// and a memory row does not care about the frequency governor. §9.3's "say
/// why" is only worth anything if the *why* is the actual one.
#[derive(Debug, Clone)]
pub struct Fitness {
    /// True when nothing about this host makes a clock reading untrustworthy —
    /// release build, quiet machine, no SMT, `performance` governor. Says
    /// nothing about whether the host is big enough for the comparison.
    pub fair_for_timing: bool,
    /// True when this host can produce a trustworthy *ratio* between two
    /// engines measured by interleaving them within a round.
    ///
    /// Strictly weaker than [`Fitness::fair_for_timing`], and deliberately so:
    /// the governor, SMT and the machine's load all land on both arms of an
    /// interleaved pair and divide out, so none of them invalidates a quotient
    /// the way each invalidates an absolute duration.
    pub fair_for_ratios: bool,
    /// True when this host can produce a trustworthy *memory* figure.
    ///
    /// Also strictly weaker than [`Fitness::fair_for_timing`]. PSS is read out
    /// of `/proc`, involves no clock, and does not move because a neighbour is
    /// busy or the governor is unreadable.
    pub fair_for_memory: bool,
    /// True when the host has at least `consumers + 1` physical cores. Says
    /// nothing about whether a clock reading here would be trustworthy.
    pub enough_cores: bool,
    /// Whether `TF_TREE_BENCH_FORCE=1` was set.
    pub forced: bool,
    /// One string per failed *timing* check, each naming the measured fact.
    ///
    /// This is the widest of the three lists; [`Fitness::ratio_reasons`] and
    /// [`Fitness::memory_reasons`] are subsets of it.
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
    /// Physical cores, from `/proc/cpuinfo` core ids — or the logical CPU count
    /// when this host publishes no core ids. Read `physical_cores_known` before
    /// quoting this at anyone.
    pub physical_cores: usize,
    /// Whether `physical_cores` is a measurement or the logical-CPU fallback.
    pub physical_cores_known: bool,
    /// Logical CPUs.
    pub logical_cpus: usize,
}

impl Fitness {
    /// Probe the host for `consumers` concurrent consumers plus one publisher.
    ///
    /// Every check is a measurement of *this* machine. The thresholds are
    /// deliberately strict — §10 says under-promising is fine — and the
    /// consequence of failing one is that the affected rows come out
    /// [`Status::Unavailable`], not that anything is estimated.
    #[must_use]
    pub fn probe(consumers: usize) -> Fitness {
        // Every input this verdict rests on is read here and nowhere else, so
        // that `assess` — which holds all of the judgement — can be handed a
        // host it would take special hardware to stand in front of.
        Fitness::assess(
            consumers,
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            physical_cores(),
            crate::mp::busy_fraction(Duration::from_millis(300)),
            governors(),
            // A debug build is not a slower release build; it is a different
            // program. Checking it here means the CLI path (`cargo run` defaults
            // to debug) cannot quietly publish debug latencies.
            cfg!(debug_assertions),
            // Whether a Pss figure can be obtained at all. `self_pss_kib`
            // returns 0 when `smaps_rollup` is unreadable, and a silent 0 is a
            // false PASS of exactly the kind the physical-core fallback below
            // refuses to make: it would leave `fair_for_memory` true and let a
            // memory row publish zeros as a claim.
            crate::mp::self_pss_kib() > 0,
        )
    }

    /// The judgement half of [`Fitness::probe`], over measurements already taken.
    ///
    /// Split out because the interesting failures are hosts this one is not:
    /// a machine that publishes no physical core count (every aarch64 host, and
    /// many containers), and an absurd `--consumers`. Neither can be produced by
    /// running the probe here, so neither would ever be tested through it.
    ///
    /// `detected_physical` is [`None`] when the host published no core ids, and
    /// that is deliberately not the same value as `Some(logical)`.
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
        // Each failing check is collected into the bucket naming *which kinds of
        // claim it invalidates*, and the three verdicts are unions of buckets.
        // Writing it this way is what stops the axes drifting apart: a new check
        // has to state its reach to be added at all.
        let mut reasons = Vec::new();
        // Invalidates everything. A debug build is a different program: its
        // latencies, its quotients and its resident footprint all describe
        // something nobody ships.
        let mut universal = Vec::new();
        // Invalidates a duration *and* a quotient, but not a page count.
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

        // Falling back to `logical` *silently* is the one thing this must not
        // do. It makes `logical > physical` vacuously false, so the SMT reason
        // never fires, and it checks the core budget against sibling threads —
        // two PASSes about a host nothing was learned from. A refusal machine
        // whose failure mode is a false PASS has the defect it cannot afford,
        // so the fallback is stated and it fails both verdicts.
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

        // `saturating_add`, not `+`: `consumers` comes from `--consumers`, and
        // `usize::MAX + 1` wraps to 0 in a release build, making `physical < 0`
        // false and printing the core budget as PASS. `just bench-report` builds
        // `--release`, so the wrap is the reachable half, not the debug panic.
        let needed = consumers.saturating_add(1);
        // Not folded into `reasons`: this one governs the N-way rows only.
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

        // Load is the one timing check that does **not** divide out of a
        // cross-engine quotient, and it fails in the direction that flatters us.
        // Interleaving cancels a disturbance only when it lands on both arms the
        // same way, and these two arms are asymmetric by construction:
        // `tf2::BufferCore` takes a mutex on every lookup and `tf_tree`'s read
        // path takes none. Under load the tf2 arm additionally suffers
        // preemption while holding that lock, and the convoy behind it, which
        // the tf_tree arm has no equivalent of — so a busy host does not add
        // noise to the ratio, it *inflates* it. That is precisely the thumb on
        // the scale §9.3 exists to catch, so `busy` reaches the ratio axis.
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

        // The unions. `reasons` is every check that reaches a duration, which is
        // all three buckets except the memory-only one.
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

    /// The status a single-threaded, in-process timing row should carry.
    ///
    /// The core budget is deliberately not consulted: such a row uses one core
    /// and one process, so a 4-core host is no obstacle to it.
    #[must_use]
    pub fn timing_status(&self) -> Status {
        Fitness::status_from(self.fair_for_timing, self.forced)
    }

    /// The one place a [`Sensitivity`] is mapped to the verdict it rests on.
    ///
    /// Returns `(is_fair, how the row is described in a refusal, why it failed)`.
    /// This is the only `match` on [`Sensitivity`] that decides fitness, so
    /// adding a variant is one non-exhaustive-match error rather than three
    /// places to remember. Both arms of `Report::validate` and [`Row::status_on`]
    /// call it.
    ///
    /// **`status_on` and [`Fitness::memory_status`] currently have no caller.**
    /// `ratio_row` reaches for [`Fitness::ratio_status`] directly and the two
    /// `Memory` rows are unavailable for build reasons before fitness is
    /// consulted, so neither path runs yet. They are kept rather than deleted
    /// because they are the shape the memory rows need the moment those
    /// resolve — but that means the single-match property above is *enforced*
    /// only for the axes `validate` actually exercises.
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
    ///
    /// §9.3 asks for the DDS vendor, RMW implementation, QoS and executor
    /// configuration. When no ROS 2 is in the configuration those are recorded
    /// as `none (…)` rather than omitted: a reader must be able to tell "there
    /// was no middleware in this measurement" from "we forgot to write it down".
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
        // **The profile directory, measured, not `cfg!(debug_assertions)`.**
        //
        // This field used to be a two-valued guess: debug assertions on meant
        // "debug" and off meant "release". Under `--profile embedder` — the
        // profile every boundary measurement in this repository is taken at,
        // because it is the one whose `lto = false` leaves the boundary in the
        // binary — debug assertions are also off, so the guess printed
        // `release`. Two runs answering *different questions* therefore carried
        // *identical* provenance, and `baseline::PORTABLE_FACTS` and
        // `runstore::BUILD_CRITICAL_FACTS` both compare this key, so both would
        // have compared them and said nothing.
        //
        // `build.rs` reads the directory cargo actually built into out of
        // `OUT_DIR`; see its comment for why that is a fact rather than a label.
        push("build_profile", crate::embed::PROFILE_DIR.to_owned());
        // The half that says what the profile *means*. `build_profile` is the
        // join key — it is what a comparison matches on — and `build_lto` is
        // the reason the join key matters: thin LTO inlines across a crate
        // boundary, so a boundary priced under it is a boundary that was not
        // there. Recorded beside the profile so a reader of `results.json` does
        // not have to know this workspace's `[profile.*]` sections by heart.
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
        // Spelled `unknown` rather than backfilled from `available_parallelism`:
        // this is the provenance block, and a number here is read as a measured
        // fact about the host. See `physical_cores`.
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
        push(
            "transparent_hugepage",
            read_trim("/sys/kernel/mm/transparent_hugepage/enabled").unwrap_or_else(unknown),
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
    /// Host fitness verdict, and why.
    pub fitness: Fitness,
    /// Seconds of warm-up discarded before any timing row was recorded (§9.3
    /// requires this to be stated, not merely done).
    pub warmup_discarded_s: f64,
    /// The §9.2 rows.
    pub rows: Vec<Row>,
    /// The §9.3 "where we are worse" entries.
    pub worse: Vec<Worse>,
}

impl Report {
    /// Enforce §9.3 against the assembled report.
    ///
    /// # Errors
    ///
    /// One string per violation. The caller is expected to fail rather than to
    /// emit a report that broke a rule.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut bad = Vec::new();

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
            // Applies to `measured` and `indicative` alike: both print numbers,
            // so both can carry a regression past the gate. `unavailable` rows
            // carry no numbers at all (checked below), so there is nothing to
            // type.
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
            match r.status {
                Status::Measured => {
                    if r.tf_tree.is_empty() && r.tf2.is_empty() {
                        bad.push(format!("row `{}` is `measured` with no numbers", r.id));
                    }
                    // The rule the whole module exists for. Each sensitivity is
                    // checked against the axis it actually rests on, so a row
                    // can no longer be refused for a fact that does not reach
                    // it — nor claim `measured` on a host that fails the one
                    // that does.
                    let (fair, axis, why) = self.fitness.axis(r.sensitivity);
                    if !fair {
                        bad.push(format!(
                            "row `{}` is {axis} and claims `measured`, but the host \
                             failed the fitness probe: {why}",
                            r.id,
                        ));
                    }
                    // The second half of the same rule. An N-way row on a host
                    // with fewer cores than consumers measures the scheduler,
                    // and that is true even where the clock is perfect.
                    //
                    // **Except for a memory row, and that exception is the
                    // point.** "Above the core count the rows measure the
                    // scheduler" is a statement about throughput and latency.
                    // Sixteen workers mapping one `.tft` on four cores share
                    // exactly the pages they would share on sixteen: Pss is
                    // decided by the page tables, not by who is running. §12
                    // gate 4 — total Pss within 1.2x of one worker — is the
                    // wedge's central claim and it was unmeasurable here only
                    // because it was being asked a question about cores that it
                    // does not depend on.
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
                    // Per-axis for the same reason `measured` is: a memory row
                    // labelled `indicative` on a host whose only failing checks
                    // are about the clock is hiding a number that was never in
                    // doubt.
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
                    if r.reproduce.trim().is_empty() {
                        bad.push(format!(
                            "row `{}` is `unavailable` and names no command that would \
                             produce it elsewhere",
                            r.id
                        ));
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
                    "`worse` entry `{}` carries no metrics and no                      `metrics_absent_because`. §9.3's section is the one a reader is                      entitled to quote against us, and an entry with an empty metric                      list reads as an oversight whether or not it is one. Either give                      it a number, or say — in the entry — why the cost has none",
                    w.id
                )),
                (false, Some(_)) => bad.push(format!(
                    "`worse` entry `{}` carries {} metric(s) *and* a reason they are                      absent. One of the two is wrong, and a reader has no way to tell                      which",
                    w.id,
                    w.metrics.len()
                )),
                _ => {}
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
        // The other two axes are published too, or the split is invisible in the
        // artifact and a reader cannot tell why a memory row was measured on a
        // host whose `fair_for_timing` is false.
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

    /// `index.html` — self-contained, no external assets, no script.
    ///
    /// The §9.3 "where we are worse" entries are rendered **inside the results
    /// table**, because §9.3 says "in the same table and not in a footnote" and
    /// a separate section at the bottom of the page is a footnote with better
    /// typography.
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
                // The reason is rendered *with* the statement, not below the
                // table: an explanation of why a cost has no number is only
                // worth anything next to the place the number would have been.
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
    // §9.1 also spells `--duration`, the steady-state window per point. There is
    // deliberately no field for it: every row it would govern is an N-way
    // comparison row, all of which are UNAVAILABLE here, and the one row this
    // tool measures itself is bounded by `lookup_samples`, not by wall clock. A
    // stored-and-never-read knob is the same quiet dishonesty the module exists
    // to prevent, so the binary rejects the flag instead. It returns, with
    // something to govern, when the N-way rows do.
    /// Warm-up discarded before any timing row is recorded (§9.3).
    pub warmup: Duration,
    /// Lookup samples for the latency row.
    pub lookup_samples: usize,
    /// Random queries for the differential row.
    pub differential_queries: usize,
    /// Directory holding the two `embed_cost` runs (§9.2's last row).
    ///
    /// [`None`] is the ordinary case and not an omission: this binary is built
    /// with one profile, and the row is a comparison *between* two, so it cannot
    /// be measured from inside a single build of this tool. `just embed-cost`
    /// produces the pair; without it the row is [`Status::Unavailable`] with
    /// that as the reason.
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
/// # The reason is derived from a `cfg`, not written as prose, and that is the
/// point
///
/// Both rows previously carried a hand-written reason saying `docs/PHASE5.md`
/// §2 and §3 "are not implemented". **Both are, and §0.0's status table — which
/// those very strings cited as the source of truth — says so.** So the tool was
/// printing a false statement, under a section (§9.3) whose whole subject is
/// that an unmeasurable row must say *why*. A reason nobody can trust is worse
/// than a missing row, which is exactly the argument §9.3 makes about numbers.
///
/// The fix is to stop asserting anything about the roadmap. The frozen backend
/// is `#[cfg(all(feature = "shm", target_os = "linux"))]`, and `just
/// bench-report` builds without `--features shm`, so on the shipped recipe
/// `Tree::open_frozen` is *not compiled into this binary* — that is the real
/// blocker, it is checkable by the compiler, and it cannot go stale the way a
/// sentence about a phase can.
///
/// With `shm` on, the remaining blocker is data rather than code: this harness
/// builds synthetic fixtures, and both rows are about a *representative* index
/// (§12 gate 2 names 233 MB). Timing `open_frozen` on a fixture and reporting it
/// against a gate written about a 233 MB file would be the thumb on the scale
/// §9.3 opens by naming.
fn frozen_row_reason(attempt: &str) -> String {
    if cfg!(all(feature = "shm", target_os = "linux")) {
        format!(
            "{attempt} needs a representative .tft, and this harness builds only synthetic \
             fixtures. `docs/PHASE5.md` §12 gate 2 is written about a 233 MB index; a number \
             taken from a fixture a thousand times smaller would answer a different question \
             while appearing to answer that one"
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

/// Build the whole §9 artifact for this host.
///
/// Every row is either measured here or [`Status::Unavailable`] with the reason
/// and the command that measures it elsewhere. Nothing is estimated, and the
/// caller is expected to run [`Report::validate`] before writing anything.
///
/// # Errors
///
/// Only a *measurement* failure propagates — a missing fixture frame, say. An
/// unmeasurable row is not an error; it is a row.
pub fn assemble(opts: &Options) -> Result<Report> {
    let fitness = Fitness::probe(opts.consumers);
    let n = opts.consumers;

    let no_ros = std::env::var("ROS_DISTRO").is_err() && !cfg!(feature = "tf2");
    let ros_reason = if no_ros {
        "there is no ROS 2 in this build or environment, so the tf2 column cannot be \
         measured at all"
    } else {
        ""
    };
    // The reason an *N-way, cross-engine* row is missing here. Built from the
    // two independent verdicts, and only from the ones that actually failed —
    // a reason listing an obstacle the host does not have reads as padding and
    // teaches a reader to skip the reasons entirely.
    let host_reason = {
        let mut parts: Vec<&str> = Vec::new();
        if !ros_reason.is_empty() {
            parts.push(ros_reason);
        }
        let core_line;
        if let Some(c) = fitness.core_reason.as_deref() {
            core_line = format!("the host has {c}");
            parts.push(&core_line);
        }
        let timing_line;
        if !fitness.fair_for_timing {
            timing_line = fitness.reason_line();
            parts.push(&timing_line);
        }
        if parts.is_empty() {
            format!("no obstacle was found for a {n}-consumer comparison on this host")
        } else {
            parts.join("; ")
        }
    };

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
        .n_way(),
    );

    rows.push(
        Row::unavailable(
            "total_rss_n_consumers",
            "Total RSS across N consumers (MB)",
            "Both stacks, summed Pss from /proc/*/smaps_rollup. Memory is exact even on a \
         loaded machine, so this row's gap is the missing tf2 column, not the host.",
            Sensitivity::Memory,
            "the tf_tree column is measurable here, but the tf2 column needs a ROS 2 install \
         this report cannot reach in-process; running both halves from one tool would \
         mean linking tf2 into it. `just mp-bench-tf2` runs both in the container and \
         prints Pss for each. A one-sided memory row is exactly the thumb on the scale \
         §9.3 warns about, so it is a gap rather than a half-filled row"
                .to_owned(),
            "just mp-bench-tf2",
        )
        .n_way(),
    );

    // The one timing row this tool measures itself. It is deliberately the
    // narrowest one: a single-threaded hot-path lookup needs no second engine
    // and no second process, so the only thing standing between it and a number
    // is the host — which is exactly what the fitness probe decides.
    //
    // Its refusal reason is therefore `fitness.reason_line()` and **not**
    // `host_reason`: this row does not want 17 cores and does not want a ROS 2
    // install, so quoting either at a reader would be a false statement about
    // why the number is missing. §9.3's "say why" means the actual why.
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
    );
    match fitness.timing_status() {
        Status::Unavailable => {}
        status => {
            lookup.tf_tree = measure_lookup_latency(opts.lookup_samples, opts.warmup)?;
            lookup.status = status;
            lookup.reason = if status == Status::Indicative {
                format!(
                    "INDICATIVE, not a claim: TF_TREE_BENCH_FORCE=1 overrode the fitness \
                     refusal. {}",
                    fitness.reason_line()
                )
            } else {
                String::new()
            };
        }
    }
    rows.push(lookup);

    rows.push(
        Row::unavailable(
            "publish_to_visible",
            "Publish -> visible-to-consumer (p50, p99.9)",
            "Both stacks, publisher process to consumer process.",
            Sensitivity::AbsoluteTiming,
            format!(
                "{host_reason}. This row also needs the `shm` feature and a second process \
             per consumer, and its tf2 counterpart needs a DDS round trip that no \
             configuration here provides"
            ),
            "just mp-bench (tf_tree, service latency) / just mp-bench-tf2",
        )
        .n_way(),
    );

    rows.push(
        Row::unavailable(
            "scaling_curve",
            "Scaling curve, N = 1..16 (throughput, CPU)",
            "Both stacks. The claim under test is that reads scale with threads.",
            Sensitivity::AbsoluteTiming,
            // The 5.35-5.62x figure is attributed to the host `docs/PHASE5.md` §0.0
            // recorded it on, not to whatever host is running this binary — quoting
            // somebody else's number as if it came from here is the exact move §9.3
            // exists to stop.
            format!(
                "{host_reason}. That a short host produces a bent curve rather than a slow \
             one is not a guess: `docs/PHASE1.md` §11.3's read-scaling gate (>= 6x from \
             1 to 8 threads) is recorded in `docs/PHASE5.md` §0.0 as FAILING at \
             5.35-5.62x on the 4-physical-core development host, which is what an \
             oversubscribed 8-thread row looks like"
            ),
            "just tf2-scaling / just shm-scaling, on >= 16 physical cores",
        )
        .n_way(),
    );

    rows.push(
        Row::unavailable(
            "tft_16_workers_rss",
            "Frozen .tft: 16 dataloader workers, total RSS vs 16 bag parses (MB)",
            "The wedge's central claim (§12 gate 4: total Pss within 1.2x of one worker).",
            Sensitivity::Memory,
            frozen_row_reason("mapping one .tft from sixteen worker processes"),
            "just bench-report-shm on >= 16 physical cores, with a .tft built by \
             `tf_tree freeze --from-bag` from a representative recording",
        )
        .n_way(),
    );

    rows.push(Row::unavailable(
        "tft_open_vs_bag_parse",
        ".tft open time vs bag parse time (ms)",
        "§12 gate 2 wants open under 10 ms for a 233 MB index.",
        Sensitivity::AbsoluteTiming,
        frozen_row_reason("timing `Tree::open_frozen` against a 233 MB index"),
        "just bench-report-shm against a .tft built by `tf_tree freeze --from-bag` from a \
         recording large enough to produce §12 gate 2's 233 MB index",
    ));

    // Correctness, and the one row that is hardware-independent by construction:
    // a disagreement between two engines on the same inputs is the same number
    // on a busy laptop and on pinned hardware.
    let diff =
        crate::differential::run_naive_rust(opts.differential_queries, 0x5EED_1234_ABCD_0001)?;
    // A differential that scored nothing has a `max_error` of 0.0 and looks
    // perfect; `passed()` is what distinguishes the two, and a report whose
    // correctness row is a failure dressed as a number is worse than no report.
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
        reproduce: "cargo test -p tf_tree_bench --release --test differential",
        tf_tree: vec![
            // The one number in this report that is a claim on any host, so it
            // is also the one the regression gate can actually hold.
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

    // Built before the struct takes ownership of `fitness`: the resident-memory
    // entry is gated on the memory axis, so it has to see the verdict.
    let worse = worse_entries(opts, &fitness);
    Ok(Report {
        provenance: Provenance::collect(),
        fitness,
        warmup_discarded_s: opts.warmup.as_secs_f64(),
        rows,
        worse,
    })
}

/// §9.2's cross-crate row: the facade called from a separate crate against the
/// identical body called from inside `tf_tree_core`.
///
/// The measurement itself is [`crate::embed`]; this is only the row. The split
/// is forced rather than stylistic, and for two reasons. The row's in-crate
/// column is `tf_tree_core::bench_probe`, compiled only under a default-off
/// feature this tool does not enable; and §9.2 requires the row be reported at
/// an **embedder's** profile, while this tool is built with `[profile.release]`
/// — `lto = "thin"`, which erases the very boundary being measured. `just
/// embed-cost` builds and runs it under both profiles and writes the pair.
///
/// **The exploratory profile comparison is deliberately not a row here.** It is
/// two processes seconds apart, carries the full between-run noise of the host,
/// and `docs/PHASE1.md` §11.2's exploratory measurements are the shape for that:
/// `just embed-cost` prints it and writes it to `target/embed-cost/`, and
/// nothing gates it.
/// What the two columns of the tf2 ratio row mean, and what they do not.
///
/// (The stray first paragraph this doc comment used to open with belonged to
/// [`EMBEDDING_NOTE`] and described a different constant.)
const RATIO_NOTE: &str = "Both engines in one process, `LerpSlerp` on both sides (tf2's \
    policy), depth 3 after constant folding, 256 off-grid stamps. `speedup_vs_tf2` is the \
    MEDIAN PER-ROUND quotient, not the quotient of the two medians: the arms are timed back \
    to back inside every round and the leading arm alternates, so drift common to both \
    cancels and no arm always gets the colder cache. That pairing is what makes this \
    resolvable where an absolute is not. The two engines are checked to agree on every \
    stamp before either is timed. **The tf2 column goes through `tf_tree_tf2_sys` and \
    therefore FLATTERS tf_tree** by the residual FFI boundary, 45.3 ns / 10% at this depth \
    (498.2 ns through the binding against 452.9 ns native — this figure used to read \
    `~21 ns / 8%` here, which `docs/benchmarks/tf2.md` withdrew for having no derivation, \
    and the correction had reached `ratio.rs` but not this string); \
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

/// The depth-3 tf2 ratio, and the first row on the `Ratio` axis.
///
/// **This is the row a 4-core host can actually gate.** Every other tf2
/// comparison in §9.2 reports an absolute duration, and this host cannot produce
/// one: the fitness probe fails on SMT, an unreadable governor and four physical
/// cores, so all of them come out `unavailable` and the project's central
/// performance claim is gated by nothing. A quotient of two arms measured inside
/// one round is a different statistic — measured here, the within-run band is
/// ~3% wide on the same host whose absolute latencies are unusable.
///
/// It is `unavailable` without the `tf2` feature, which needs a ROS 2 install,
/// so `just bench-report` on a bare host still reports the gap rather than the
/// number. `just tf2-check`'s container is where it resolves.
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
        );
        let run = match crate::ratio::measure() {
            Ok(r) => r,
            Err(e) => {
                row.reason = format!("the paired measurement could not be taken: {e}");
                return row;
            }
        };
        // A band that straddles the floor has not answered, and saying so beats
        // resolving it by taking the median — `embed.rs`'s rule, and the reason
        // a gate whose noise exceeds its threshold reports rather than passes.
        // **Both non-`Above` verdicts stop here, and the `Below` arm is the one
        // that matters.** An earlier revision special-cased only `Unresolved`,
        // so a band lying entirely *under* the floor — `tf_tree` slower than the
        // multiple this row exists to gate — fell through and was published as a
        // clean `measured` row, with `floor` sitting next to a failing
        // `speedup_vs_tf2` and nothing saying so. The less severe outcome was
        // loud and the severe one was silent. Only the baseline's 15% slack
        // would have caught it, and `just tf2-bench-baseline-update` bypasses
        // that by construction.
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
        row.status = fitness.ratio_status();
        row.reason = match row.status {
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

/// Module-level rather than local to [`embedding_row`] so that
/// `the_row_note_states_the_settings_the_manifest_declares` can read the two
/// settings back out of the workspace manifest with
/// [`crate::embed::profile_settings_from_manifest`] and assert this prose still
/// matches them. `just embed-cost`'s own output deliberately does not repeat
/// them; a second copy is a second thing that can go stale.
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

/// `pub(crate)` for one reason: `crate::baseline`'s
/// `a_baseline_that_measured_the_embedding_row_fails_a_check_that_did_not`
/// builds the *real* `None`-arm row rather than a fixture that resembles it,
/// which is the point of that test.
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
        ));
    };

    // A pair that will not load is a measurement failure, not a row: the caller
    // asked for this row by naming a directory. Loading the *pair* rather than
    // the one half this row needs is what runs `Pair::load`'s two provenance
    // checks — same source, two different profiles.
    let pair = crate::embed::Pair::load(dir)
        .with_context(|| format!("loading the embed_cost pair from {}", dir.display()))?;
    let run = &pair.embedder;

    // **The spread gates the verdict.** A band that straddles §9.2's threshold
    // cannot answer it, and on this host that is the ordinary case rather than
    // an exception — so it is reported as unavailable with the band, never
    // rounded into a pass or a fail. This check is deliberately independent of
    // the fitness probe: a quiet host can still produce a run too noisy to
    // resolve 5%.
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
        ));
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
    );
    match fitness.timing_status() {
        Status::Unavailable => {}
        status => {
            row.tf_tree = run.metrics();
            row.status = status;
            row.reason = if status == Status::Indicative {
                format!(
                    "INDICATIVE, not a claim: TF_TREE_BENCH_FORCE=1 overrode the fitness \
                     refusal. {} Measured here: {}",
                    fitness.reason_line(),
                    run.verdict_line()
                )
            } else {
                String::new()
            };
        }
    }
    Ok(row)
}

/// Pss actually held by an idle arena of §9.3's stated geometry, and the arena's
/// own view of what it reserved.
///
/// Returns `(resident_bytes, reserved_bytes)`, or [`None`] where the figure
/// cannot be trusted — no `smaps_rollup` (not Linux, or a kernel built without
/// it), or a delta that came out non-positive because whole-process Pss moved
/// under the measurement.
///
/// **This is a delta of a whole-process counter, and that is its limitation.**
/// Pss is reported in whole KiB and the reads themselves allocate, so the result
/// is quantised to pages and carries a page or two of slack. That is far below
/// the difference it exists to settle — megabytes of reservation against
/// whatever an untouched arena really holds — but it is not a byte-exact
/// instrument, and a future reader should not treat it as one. The exact tool
/// would be `mincore(2)` over the arena's own range; it needs a raw pointer and
/// an `unsafe` block at a boundary `CLAUDE.md`'s budget does not currently name,
/// so it is a decision record rather than a patch.
///
/// The tree is built and then held across the second read: dropping it first
/// would measure the allocator returning memory, which is a different question.
fn measure_idle_arena_resident() -> Option<(f64, f64)> {
    use tf_tree::{Capacity, EdgeCfg, TreeBuilder};

    // 64 frames and 32 dynamic edges at 1024 slots each — the geometry
    // `from_totals` is asked about above, built for real rather than computed.
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
    // Headroom to the stated totals: the declared frames and edges above are
    // fewer than the geometry names, and the reservation is what is under test.
    let b = b
        .frame_headroom(FRAMES - DYNAMIC_EDGES - 1)
        .edge_headroom(EDGE_SLOTS - DYNAMIC_EDGES);

    // Warm the reader once so its own buffer is already allocated and does not
    // land inside the delta.
    let _ = crate::mp::self_pss_kib();
    let before = crate::mp::self_pss_kib();
    let tree = b.build().ok()?;
    let after = crate::mp::self_pss_kib();

    let reserved = tree.arena_size_bytes() as f64;
    // Hold it across the read above — see the doc comment.
    std::hint::black_box(&tree);
    drop(tree);

    if before == 0 || after <= before {
        return None;
    }
    Some(((after - before) as f64 * 1024.0, reserved))
}

/// §9.3's "report where `tf_tree` is worse, in the same table and not in a
/// footnote" — the four costs it names, with a number wherever the cost has one.
fn worse_entries(opts: &Options, fitness: &Fitness) -> Vec<Worse> {
    // A deployment-shaped arena: 64 frames, 64 edge slots, 32 dynamic edges at
    // 1024 samples each. `from_totals` reproduces the same region geometry from
    // the totals, which is all a size statement needs.
    const FRAMES: u32 = 64;
    const EDGES: u32 = 64;
    const SLOTS: u32 = 32 * 1024;
    let floor_bytes = tf_tree_arena::ArenaLayout::from_totals(FRAMES, EDGES, SLOTS)
        .map(|l| l.total_size() as f64);

    // The *resident* half of the same claim. `total_size()` is the mapping — the
    // last region's `offset + size` — and a mapping need not be a footprint,
    // because pages become resident when they are touched. Saying "an idle tree
    // costs its full size from the first second" without ever weighing one is
    // the kind of unfalsifiable statement §9.3 exists to stop, and this is the
    // row where `tf_tree` looks worst, so it is the last one that should rest on
    // arithmetic.
    //
    // **The measurement confirms the claim rather than softening it**, which was
    // not the expected result: an idle arena comes out ~100% resident, because
    // `alloc_zeroed` at 64-byte alignment zero-fills by hand instead of reaching
    // `calloc`. The number is kept — and the cause named in the statement —
    // precisely because a row that turned out worse than hoped is the one a
    // reader has most reason to want measured.
    let resident = measure_idle_arena_resident();

    // The measured half is stated only when it exists. A statement that asserts
    // a measurement's *result* while the measurement returned `None` — no
    // `smaps_rollup`, or a delta that came out non-positive — would be a claim
    // with nothing behind it, in the one section whose whole job is not to make
    // those.
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
    };
    // The arithmetic and the measurement are independent facts, so they are
    // emitted independently: a `from_totals` failure must not discard a Pss
    // figure that was obtained, and vice versa.
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
    // The memory axis reaches here too. `Worse` entries carry no `Sensitivity`
    // and `Report::validate` does not inspect them, so without this guard a
    // debug build would publish a resident-page figure that the same report's
    // `fair_for_memory: false` declares untrustworthy.
    let resident = if fitness.fair_for_memory {
        resident
    } else {
        None
    };
    if let Some((resident_bytes, arena_bytes)) = resident {
        floor.metrics.push(Metric::new(
            "idle_arena_resident_bytes",
            resident_bytes,
            "B",
        ));
        // Both sides of this quotient describe the arena the measurement
        // actually built. Dividing the measured residency by `from_totals`'s
        // arithmetic would mix two different arenas — they differ by a few KiB
        // of region rounding — and print a ratio whose numerator and
        // denominator never met.
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

    // Both sources of this row's numbers can fail — `from_totals` on an
    // arithmetic overflow, Pss on a host without `smaps_rollup` or on a build
    // the memory axis calls unfair — and if both do, the entry has to say so
    // rather than present an empty list. Set here rather than up with the
    // statement because only this point knows whether anything was pushed.
    if floor.metrics.is_empty() {
        floor.metrics_absent_because = Some(
            "neither half landed on this run: `ArenaLayout::from_totals` did not return a              layout for the stated geometry, and the Pss measurement was unavailable or              was withheld because this host failed the memory axis of the fitness probe.              The reservation arithmetic is host-independent, so this state is a bug or a              hostile /proc, not a property of the machine — `just bench-report` on any              Linux host that passes `Fitness::probe` fills both in."
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

/// The `lookup_latency` row's note — the sentence a reader of `results.json`
/// actually sees, which is the only place this row explains what it measured.
///
/// **Two of its clauses are required rather than chosen, and both were missing.**
///
/// 1. `docs/PHASE1.md` §11.3 is NORMATIVE that "every reported latency row must
///    state its dynamic-step count, not just its nominal depth", because the two
///    readings of "depth 3" differ by ~2.8× and a static-heavy path can pass a
///    gate without exercising the sampler at all. The note said "depth 3" in its
///    title and nothing about steps.
/// 2. The **stamp regime** decides whether the interpolator runs at all. This
///    row queried `fixture::NOW_NS` — a knot on all four dynamic grids — until
///    `docs/decisions/0013`, so every edge took `SampleRing::sample`'s exact-hit
///    branch and what shipped as a lookup latency was `bracket` plus a seqlock
///    read. A reader comparing a figure from before that change with one from
///    after is comparing two different measurements, and only this string can
///    tell them so.
///
/// Both are checked against the code that produces the number, not merely
/// written here, by `tests::the_lookup_row_note_states_what_phase1_requires`
/// below (a `#[cfg(test)]` item, so this is a name and not a link).
/// The stamp half of that check needs the measured stamp to be reachable from a
/// test without running the measurement, which is what [`LOOKUP_STAMP_NS`] is
/// for.
const LOOKUP_NOTE: &str = "tf_tree column: `map <- imu_link`, LerpSlerp, in-process, one \
     thread. **3 dynamic steps** after constant folding (1 kHz, 200 Hz, 50 Hz), which is what \
     docs/PHASE1.md §11.3's NORMATIVE reading of \"depth 3\" means. The query stamp is \
     fixture::QUERY_NS = NOW_NS - 500 us, which is off-grid on every one of those three rates, \
     so the interpolator runs on every step; it was on-grid until \
     docs/decisions/0013, and a p50 published before that change is not comparable with one \
     published after. Percentiles include two Instant::now() calls, whose own cost is reported \
     alongside as clock_overhead_p50_ns. The tf2 column is a separate, cross-engine comparison \
     and is not attempted here.";

/// The stamp [`measure_lookup_latency`] queries, named so a test can reach it.
///
/// It is one `const` rather than an expression inlined at the call site for the
/// same reason [`crate::embed`] reads `[profile.embedder]` back out of the
/// manifest: [`LOOKUP_NOTE`] makes two claims about this stamp — that it is
/// `fixture::QUERY_NS`, and that it is off every dynamic grid — and a claim
/// nothing evaluates is how `docs/decisions/0013` stayed true for as long as it
/// did.
const LOOKUP_STAMP_NS: i64 = crate::fixture::QUERY_NS;

/// Measure depth-3 hot-path lookup latency on this process.
///
/// The percentiles include two `Instant::now()` calls per lookup, so the clock's
/// own cost is measured in the same loop shape and reported as
/// `clock_overhead_p50_ns` rather than quoted from memory. Subtracting it is
/// left to the reader: the overhead distribution is not the same shape as the
/// measurement's, so a subtracted percentile would be a fabrication.
///
/// **The query stamp is [`crate::fixture::QUERY_NS`], which interpolates.** It
/// was [`crate::fixture::NOW_NS`], a knot on every dynamic grid, so this row —
/// the one the report publishes as a lookup latency — timed `bracket` plus the
/// seqlock read with `I::eval` never running (`docs/decisions/0013`). Reading
/// any figure published before that change against one published after it is a
/// comparison between two different measurements.
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
    // `LookupError` is `Copy` and deliberately not `std::error::Error`
    // (`CLAUDE.md`: errors are `Copy` and carry no `String`), so `?` cannot
    // convert it into `anyhow::Error` on its own.
    let plan = tree
        .plan(target, source)
        .map_err(|e| anyhow!("compiling the map <- imu_link plan: {e:?}"))?;
    let guard = tree.guard();
    let stamp: Stamp = Stamp::from_nanos(LOOKUP_STAMP_NS);

    // §9.3: warm, then discard, and state how long. Time-based rather than
    // iteration-based so the stated number is the one the report prints.
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

    // The clock's own cost, in the same loop shape, on this host.
    let mut clock = crate::mp::Histogram::new();
    for _ in 0..samples.min(50_000) {
        let t0 = Instant::now();
        clock.record(elapsed_ns(t0));
    }

    // Keep the loop from being optimised into nothing without pulling in
    // criterion's `black_box`: a NaN sink would mean the samples were discarded.
    if sink.is_nan() {
        bail!("lookup sink went NaN — the measured loop did not run as written");
    }

    // `LATENCY_SLACK` is per-percentile and generous because these can only be
    // `Measured` on a host that passed the fitness probe, and even a quiet,
    // non-SMT, fixed-frequency machine moves a p99.9 by more than a few percent
    // between runs. `samples` and `clock_overhead_p50_ns` stay informational:
    // the first is a run parameter, and the second describes the host's clock
    // rather than the engine — gating it would fail this repository's own
    // artifact on a kernel that made `clock_gettime` slower.
    Ok(vec![
        Metric::new("p50_ns", hist.quantile(0.50) as f64, "ns").lower_is_better(LATENCY_SLACK),
        Metric::new("p99_ns", hist.quantile(0.99) as f64, "ns").lower_is_better(LATENCY_SLACK),
        Metric::new("p999_ns", hist.quantile(0.999) as f64, "ns").lower_is_better(LATENCY_SLACK),
        Metric::new("samples", hist.count() as f64, "lookups"),
        Metric::new("clock_overhead_p50_ns", clock.quantile(0.50) as f64, "ns"),
    ])
}

/// Nanoseconds since `t0`, in 64-bit arithmetic.
///
/// `Duration::as_nanos` is `u128`, so the obvious spelling costs a 128-bit
/// multiply-add and a 128-bit compare *per sample*. That sits after the clock
/// read, so it never biased the recorded latency — it only widened the gap
/// between samples. `u64` nanoseconds saturate after 584 years of uptime.
#[inline]
fn elapsed_ns(t0: Instant) -> u64 {
    let d = t0.elapsed();
    d.as_secs()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::from(d.subsec_nanos()))
}

/// Lift a `Copy`, non-`std::error::Error` [`tf_tree::LookupError`] into `anyhow`.
///
/// Out of line so the hot loops keep a single non-inlined error path rather than
/// a format call per iteration.
#[cold]
fn eval_failed(e: tf_tree::LookupError) -> anyhow::Error {
    anyhow!("plan evaluation failed: {e:?}")
}

/// Physical core count from `/proc/cpuinfo` `physical id` / `core id` pairs,
/// or [`None`] when this host publishes none.
///
/// `available_parallelism` counts SMT siblings, which is the wrong denominator
/// for "can this host run N consumers without oversubscribing" — so this returns
/// [`None`] rather than quietly substituting it. **aarch64 `/proc/cpuinfo`
/// carries no `physical id` or `core id` lines at all**, and neither do many
/// container configurations, which makes [`None`] the ordinary answer on a
/// target this project supports rather than a corner case.
/// [`Fitness::assess`] is what decides what to do about it.
#[must_use]
pub fn physical_cores() -> Option<usize> {
    physical_cores_from_cpuinfo(&std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default())
}

/// The parse, over text rather than over `/proc`, so it can be tested against a
/// host this one is not.
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

/// The `lto` setting of the profile this binary was built into.
///
/// Read out of the workspace manifest at run time rather than baked at build
/// time, because the parser lives in [`crate::embed`] and a build script cannot
/// call it — and a second copy of a TOML reader is a second thing that can
/// disagree with the first. `CARGO_MANIFEST_DIR` is a *compile-time* constant,
/// so this resolves against the source tree this binary was built from, not
/// against wherever it happens to be run.
///
/// The failure arm names the path it could not read. A benchmark binary copied
/// out of its checkout is the case that reaches it, and it must be
/// distinguishable from "the profile declares no LTO", which
/// [`crate::embed::lto_for_profile_dir`] spells differently again.
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

/// UTC timestamp as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Hand-rolled because the workspace has no date crate and the benchmark report
/// is not a reason to add one. Civil-from-days is Howard Hinnant's algorithm,
/// with the era shifted so it is correct before 1970 as well — not that it will
/// be asked, but a timestamp routine that is only right on the happy path is
/// exactly the kind of thing that makes a provenance header untrustworthy.
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

/// JSON number, or `null` for a non-finite value.
///
/// `NaN` and `Infinity` are not JSON. Emitting them would produce a file that
/// every consumer rejects, which is a worse failure than a `null` a reader can
/// see.
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

/// Format a value for a human column: scientific where a fixed-point rendering
/// would print `0.000`, plain otherwise.
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
    // A failed assertion in a unit test is the intended failure mode, and these
    // helpers make the failure name the field it came from. `panic!` is in the
    // list because `expect` takes a `&str`: naming *which* metric went missing
    // needs a formatted message, and "a metric is missing" without the key is a
    // failure a reader has to reproduce before they can act on it.
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    /// A report skeleton with every required row and worse-entry present, all
    /// unavailable. Tests mutate one thing from here, so a failure names one
    /// cause.
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
            })
            .collect();
        Report {
            provenance: Provenance { facts: Vec::new() },
            fitness: Fitness {
                fair_for_timing: fair,
                // The skeleton's failing check is a *timing* one, so the ratio
                // and memory axes stay fair even when `fair` is false. That is
                // the split under test: a host can be unfit to time and still
                // fit to weigh.
                fair_for_ratios: true,
                fair_for_memory: true,
                // The skeleton's rows are not marked `n_way`, so the core budget
                // is out of the picture and each test isolates one rule.
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

    /// The skeleton itself must validate, or every negative test below passes
    /// for the wrong reason.
    ///
    /// Mutant: drop `"a stated reason"` to `""` in `skeleton` — this test fails.
    #[test]
    fn a_fully_unavailable_report_is_valid() {
        assert_eq!(skeleton(false, false).validate(), Ok(()));
    }

    /// **A row that prints numbers must print at least one the regression gate
    /// can hold.**
    ///
    /// [`crate::baseline`] compares only metrics with a direction, so a row
    /// whose numbers are all [`Drift::Informational`] passes `just bench-check`
    /// no matter what it says. That is the way a regression gate rots: not by
    /// being deleted, but by a new claim arriving next to it, ungated, and
    /// nobody noticing that the green tick covers less than it used to. The rule
    /// lives in `validate` rather than in the gate because the gate would have
    /// to *guess* that a row it skipped was meant to be checked.
    ///
    /// It binds `indicative` as well as `measured`, and the reason is *not* the
    /// one an earlier revision of this comment gave. `bench-check` skips every
    /// row whose **baseline** status is not `measured` (`baseline::compare`
    /// short-circuits on it), so an indicative row is compared neither with a
    /// direction nor without one, and "the gate would compare nothing in it" is
    /// false for that half. The rule binds it anyway because a row's status is a
    /// property of the *host*: the same row is indicative here and measured on
    /// the machine the baseline is cut from, and a direction it never acquired
    /// while it was cheap to add is one nothing gates once it matters.
    ///
    /// Mutant (applied, confirmed fatal): restrict the new arm to
    /// `r.status == Status::Measured` — the `indicative` half of this test then
    /// returns `Ok(())` and the `expect_err` panics.
    #[test]
    fn a_row_that_prints_numbers_must_print_one_the_gate_can_hold() {
        for (fair, forced, status) in [
            (true, false, Status::Measured),
            (false, true, Status::Indicative),
        ] {
            let mut r = skeleton(fair, forced);
            let row = &mut r.rows[0];
            row.status = status;
            row.reason = if status == Status::Indicative {
                "forced on an unfit host".to_owned()
            } else {
                String::new()
            };
            row.tf_tree = vec![Metric::new("samples", 1024.0, "lookups")];
            let errs = r
                .validate()
                .expect_err("a row of pure context claimed to be a result");
            assert!(
                errs.iter()
                    .any(|e| e.contains("every one of them is informational")),
                "{status:?}: {errs:?}"
            );

            // The same row with one directional metric is fine: the rule is
            // about being gateable, not about the metric count.
            r.rows[0]
                .tf_tree
                .push(Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK));
            assert_eq!(r.validate(), Ok(()), "{status:?}");
        }
    }

    /// A directional metric with a negative or non-finite tolerance is refused.
    ///
    /// `slack = |baseline| * tolerance` in the gate, so a negative tolerance
    /// makes the bound tighter than exact equality and fires on every run, and a
    /// NaN one makes every comparison `false` and fires on none. Both are worse
    /// than an ungated metric, because both look like a working gate.
    ///
    /// Mutant (applied, confirmed fatal): drop the `m.tolerance >= 0.0`
    /// conjunct — the `-0.5` case then validates and the loop's `expect_err`
    /// panics.
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

    /// §9.3's central rule: a timing row may not claim `measured` on a host that
    /// failed the fitness probe. This is the check the whole module exists for.
    ///
    /// Mutant: delete the `r.timing_sensitive && !self.fitness.fair_for_timing`
    /// arm in `validate` — this test fails (validation returns `Ok`).
    #[test]
    fn a_timing_row_cannot_claim_measured_on_an_unfit_host() {
        let mut r = skeleton(false, false);
        let row = &mut r.rows[0];
        row.status = Status::Measured;
        row.reason = String::new();
        row.tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        let errs = r.validate().expect_err("unfit host must reject the claim");
        assert!(
            errs.iter().any(|e| e.contains("failed the fitness probe")),
            "{errs:?}"
        );

        // The same row on a host that passed is fine — the rule is about the
        // host, not about the row being timing sensitive.
        let mut ok = skeleton(true, false);
        let row = &mut ok.rows[0];
        row.status = Status::Measured;
        row.reason = String::new();
        row.tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        assert_eq!(ok.validate(), Ok(()));
    }

    /// The other half of the same rule, and the reason `Fitness` carries two
    /// verdicts: an N-way row on a host with fewer cores than consumers measures
    /// the scheduler even when the clock is beyond reproach. The fixture sets
    /// `fair_for_timing: true` and [`Sensitivity::HostIndependent`] precisely so
    /// that only the core budget can produce the failure — otherwise the test
    /// would pass off the back of the timing rule and prove nothing.
    ///
    /// Mutant: delete the `r.needs_n_cores && !self.fitness.enough_cores` arm in
    /// `validate` — this test fails.
    #[test]
    fn an_n_way_row_cannot_claim_measured_without_the_cores() {
        let mut r = skeleton(true, false);
        r.fitness.enough_cores = false;
        r.fitness.core_reason =
            Some("4 physical cores for 16 consumers plus a publisher (17 needed)".to_owned());
        let row = &mut r.rows[0];
        row.needs_n_cores = true;
        row.sensitivity = Sensitivity::HostIndependent;
        row.status = Status::Measured;
        row.reason = String::new();
        row.tf_tree = vec![Metric::new("cpu_pct", 3.0, "%").lower_is_better(0.20)];
        let errs = r
            .validate()
            .expect_err("short core budget must reject the claim");
        assert!(
            errs.iter().any(|e| e.contains("runs 16 consumers")),
            "{errs:?}"
        );
        assert!(errs.iter().any(|e| e.contains("17 needed")), "{errs:?}");

        // The converse: the identical row on a host that has the cores is fine.
        r.fitness.enough_cores = true;
        r.fitness.core_reason = None;
        assert_eq!(r.validate(), Ok(()));
    }

    /// A memory row is not a timing row, and refusing it for a timing reason is
    /// how §12 gate 4 came to be unmeasurable on a host that could always have
    /// weighed it. The skeleton fails every *timing* check; a `Memory` row on it
    /// must still be allowed to claim `measured`.
    ///
    /// Mutant: in `validate`'s `Status::Measured` arm, change the
    /// `Sensitivity::Memory` branch to read `self.fitness.fair_for_timing`
    /// instead of `self.fitness.fair_for_memory` — this test fails.
    #[test]
    fn a_memory_row_is_measurable_on_a_host_that_only_fails_the_timing_checks() {
        let mut r = skeleton(false, false);
        assert!(!r.fitness.fair_for_timing, "fixture must be unfit to time");
        assert!(r.fitness.fair_for_memory, "fixture must be fit to weigh");

        let row = &mut r.rows[0];
        row.sensitivity = Sensitivity::Memory;
        row.status = Status::Measured;
        row.reason = String::new();
        row.tf_tree = vec![Metric::new("pss_kib", 4096.0, "KiB").lower_is_better(0.20)];
        assert_eq!(r.validate(), Ok(()));

        // And the converse, so this is not vacuous: the same row on a host whose
        // *memory* axis failed — a debug build — is refused.
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

    /// The core budget is a statement about the scheduler, and Pss is not
    /// scheduled. Sixteen workers mapping one `.tft` share the same pages on
    /// four cores as on sixteen, so `needs_n_cores` must not reach a memory row.
    ///
    /// The fixture sets `fair_for_timing: true` so that only the core-budget
    /// rule could produce a failure here, exactly as
    /// `an_n_way_row_cannot_claim_measured_without_the_cores` does.
    ///
    /// Mutant: drop the `&& r.sensitivity != Sensitivity::Memory` conjunct from
    /// `core_budget_applies` in `validate` — this test fails.
    #[test]
    fn a_memory_row_does_not_need_the_core_budget() {
        let mut r = skeleton(true, false);
        r.fitness.enough_cores = false;
        r.fitness.core_reason =
            Some("4 physical cores for 16 consumers plus a publisher (17 needed)".to_owned());

        let row = &mut r.rows[0];
        row.needs_n_cores = true;
        row.sensitivity = Sensitivity::Memory;
        row.status = Status::Measured;
        row.reason = String::new();
        row.tf_tree = vec![Metric::new("total_pss_kib", 65536.0, "KiB").lower_is_better(0.20)];
        assert_eq!(r.validate(), Ok(()));

        // Not vacuous: the identical row that reports a *duration* instead is
        // still refused by the same short core budget.
        r.rows[0].sensitivity = Sensitivity::AbsoluteTiming;
        let errs = r
            .validate()
            .expect_err("a timing row must still want the cores");
        assert!(
            errs.iter().any(|e| e.contains("runs 16 consumers")),
            "{errs:?}"
        );
    }

    /// The classification itself: which measured facts about a host invalidate
    /// which kind of claim.
    ///
    /// SMT and an unreadable governor are common-mode: they land on both arms of
    /// an interleaved pair and divide out, and neither moves a page count. A
    /// **busy machine does not**, and that is the interesting row — the two arms
    /// are asymmetric (only tf2 takes a lock), so load inflates the quotient
    /// instead of cancelling. A debug build reaches everything, and an
    /// unreadable `smaps_rollup` reaches memory alone.
    ///
    /// Mutant: set `fair_for_ratios: reasons.is_empty()` in `assess` (i.e. fold
    /// the axes back into one boolean) — this test fails on the first block.
    #[test]
    fn each_host_check_reaches_only_the_axes_it_bears_on() {
        // SMT on (8 logical over 4 physical) and an unreadable governor, on a
        // quiet machine: both are common-mode between two interleaved arms.
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

        // Load is the timing check that *does* reach a cross-engine ratio, and
        // it fails in the flattering direction, so it must not be waved through.
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

        // An unreadable smaps_rollup reaches the memory axis and nothing else.
        // Without this, `self_pss_kib`'s silent 0 would let a memory row publish
        // zeros as a measurement.
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

        // A debug build is a different program, so it reaches all three.
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

        // And a host that passes everything passes all three.
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

    /// A required row may be unavailable, but it may not be dropped, and an
    /// unavailable row must say why *and* name the command that would produce
    /// it elsewhere.
    ///
    /// Mutants, each of which makes this test fail: remove the `REQUIRED_ROWS`
    /// loop from `validate`; remove the empty-`reason` check; remove the empty-
    /// `reproduce` check.
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

    /// `indicative` is the `TF_TREE_BENCH_FORCE=1` escape hatch and nothing
    /// else: it is invalid without the override, and invalid on a fit host
    /// (where it would hide a number that *is* a claim).
    ///
    /// Mutant: delete the `!self.fitness.forced` check — the first half fails.
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

        // Unfit + forced is the one combination that is allowed.
        let mut r = skeleton(false, true);
        r.rows[0].status = Status::Indicative;
        r.rows[0].tf_tree = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        assert_eq!(r.validate(), Ok(()));
    }

    /// The four §9.3 "where we are worse" topics are as required as the rows,
    /// and each must actually state the cost. A report that quietly shed them
    /// reads as a clean sweep, which is the flattering-report failure mode
    /// `validate` exists to make impossible.
    ///
    /// Mutants, each applied and confirmed to make this test fail: delete the
    /// `REQUIRED_WORSE` presence loop from `validate` (the first half then
    /// validates `Ok`); delete the empty-`statement` check (the second half
    /// does).
    #[test]
    fn the_where_we_are_worse_entries_are_required_and_must_state_the_cost() {
        let dropped = REQUIRED_WORSE[0];
        let mut r = skeleton(false, false);
        r.worse.retain(|w| w.id != dropped);
        let errs = r.validate().expect_err("a dropped `worse` topic must fail");
        assert!(errs.iter().any(|e| e.contains(dropped)), "{errs:?}");

        // A *present but empty* entry is the more likely regression: the id is
        // still there, so a presence-only check would pass it.
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

    /// A §9.3 entry with no numbers must say why it has none, and one with
    /// numbers must not claim it has none.
    ///
    /// **This is the rule that closes the hole two of the four entries sat in.**
    /// `bridge_supervision` and `format_bump_cost` carried `metrics: Vec::new()`
    /// from the day they were written, and nothing could tell that state apart
    /// from an oversight — which is exactly the shape of failure this whole
    /// module is built against, except pointed at the section that is supposed
    /// to be quotable against us. An entry that cannot regress is not an honest
    /// entry; an entry that explains why it cannot is.
    ///
    /// Mutants, each applied and confirmed to make this test fail: delete the
    /// `(true, None | Some(""))` arm from `validate` (the first half validates
    /// `Ok`); delete the `(false, Some(_))` arm (the second half does).
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

        // Whitespace is not an explanation, for the same reason `"   "` is not
        // a statement two tests above.
        let mut r = skeleton(false, false);
        r.worse[0].metrics_absent_because = Some("  ".to_owned());
        assert!(r.validate().is_err(), "a blank reason must not satisfy it");

        // And the contradiction: numbers *and* a reason they are absent. One of
        // the two is stale, and the report cannot say which.
        let mut r = skeleton(false, false);
        r.worse[0].metrics = vec![Metric::new("bytes", 1.0, "B")];
        let errs = r
            .validate()
            .expect_err("metrics beside a reason they are absent must fail");
        assert!(
            errs.iter().any(|e| e.contains("One of the two is wrong")),
            "{errs:?}"
        );

        // The real report satisfies the rule on this host — which is the point
        // of the field, not an incidental check: `worse_entries` is where the
        // four entries are actually written.
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

    /// The remaining §9.3 row rules, each isolated so a failure names one cause:
    /// a `measured` row must carry numbers, an `indicative` row must say why, an
    /// `unavailable` row must carry none, and a required row may not be counted
    /// twice (which would let a second, flattering copy sit beside the first).
    ///
    /// Each block picks the `skeleton` fitness that makes the *other* rules
    /// inapplicable — otherwise the assertion would pass off the back of a rule
    /// it is not testing.
    ///
    /// Mutants, each applied and confirmed to make this test fail: delete the
    /// `r.tf_tree.is_empty() && r.tf2.is_empty()` check; delete the
    /// `indicative`/`r.reason.trim().is_empty()` check; delete the
    /// `!r.tf_tree.is_empty() || !r.tf2.is_empty()` check under `Unavailable`;
    /// replace `validate`'s duplicate-count `n =>` arm with `_ => {}`.
    #[test]
    fn a_row_must_carry_exactly_the_evidence_its_status_claims() {
        // `measured` with nothing to show. Fit host, so the timing rule is
        // silent and only the missing-numbers rule can fire.
        let mut r = skeleton(true, false);
        r.rows[0].status = Status::Measured;
        r.rows[0].reason = String::new();
        let errs = r.validate().expect_err("measured with no numbers");
        assert!(
            errs.iter()
                .any(|e| e.contains("`measured` with no numbers")),
            "{errs:?}"
        );

        // `indicative` with no reason. Unfit + forced is the one combination the
        // other two indicative rules allow, so the reason rule is alone.
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

        // `unavailable` and yet carrying a number. The number would render in
        // the table beside the refusal, which is a claim wearing a disclaimer.
        // Asserted on the `tf2` column specifically: the `tf_tree` column alone
        // would leave the `||`'s right operand untested.
        let mut r = skeleton(false, false);
        r.rows[0].tf2 = vec![Metric::new("p50_ns", 42.0, "ns").lower_is_better(LATENCY_SLACK)];
        let errs = r.validate().expect_err("unavailable carrying numbers");
        assert!(
            errs.iter().any(|e| e.contains("but carries numbers")),
            "{errs:?}"
        );

        // A duplicated required row: present twice, so the presence check is
        // satisfied and only the count arm can catch it.
        let mut r = skeleton(false, false);
        let dup = r.rows[3].clone();
        r.rows.push(dup);
        let errs = r.validate().expect_err("a duplicated row must fail");
        assert!(
            errs.iter().any(|e| e.contains("appears 2 times")),
            "{errs:?}"
        );
    }

    /// §9.3 puts the "where we are worse" entries in the same table as the
    /// results, not in a footnote. Asserted structurally: the section header and
    /// every topic must fall between the results table's `<table>` and its
    /// `</table>`.
    ///
    /// Mutant (verified): close the results table early by inserting
    /// `s.push_str("</table>\n")` immediately before the worse-entry block in
    /// `to_html` — the topics then land outside the table and this test fails.
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

    /// The JSON must survive a reason containing the characters that break
    /// hand-written serialisers, and must never emit `NaN`.
    ///
    /// Mutants, each verified to fail this test: drop the `'"'` arm from `jstr`
    /// (the quote is then emitted raw and closes the string early); make `jnum`
    /// print `{v}` unconditionally (`NaN` then reaches the file, and `NaN` is
    /// not JSON — no parser accepts it).
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
        // Cheap structural check: braces balance and the schema key is first.
        // The version is spelled out rather than read from `SCHEMA` on purpose —
        // it is a compatibility surface, so bumping it should cost an edit here
        // that somebody reads, not pass silently because both sides moved.
        assert!(json.starts_with("{\n  \"schema\": \"tf_tree.bench-report/2\""));
        let opens = json.matches('{').count();
        let closes = json.matches('}').count();
        assert_eq!(opens, closes, "unbalanced JSON braces");
    }

    /// **No row may explain itself by claiming a phase is unimplemented.**
    ///
    /// Two rows did, and both statements had become false: `tft_16_workers_rss`
    /// and `tft_open_vs_bag_parse` said `docs/PHASE5.md` §2 and §3 "are not
    /// implemented", citing §0.0's status table as the source of truth — while
    /// §0.0 recorded §2 as **Done** and §3 as partly done for MCAP. §9.3 is
    /// NORMATIVE that an unmeasurable row must say *why*, and a report that
    /// misstates the reason costs exactly the credibility §9.3 opens by naming.
    ///
    /// The rule this pins is the general form: **an unavailable row's reason is
    /// about this host or this build, never about the roadmap.** A `cfg` or a
    /// core count is checkable and cannot rot; a sentence about what phase has
    /// landed is neither, and §0.0 is the one place that tracks it.
    ///
    /// Mutant: restore either row's old reason (`"`docs/PHASE5.md` §2 (the
    /// frozen .tft arena) is not implemented ..."`). Applied: the assertion
    /// fails naming that row and quoting the phrase.
    #[test]
    fn no_unavailable_reason_claims_a_phase_is_unimplemented() {
        let opts = Options {
            lookup_samples: 1,
            differential_queries: 64,
            warmup: Duration::from_millis(1),
            ..Options::default()
        };
        let report = assemble(&opts).expect("assemble");

        // Non-degenerate: if nothing were unavailable this would pass vacuously,
        // and on a host that can measure everything it still must not.
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

        for r in &unavailable {
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
            assert!(
                !r.reason.is_empty(),
                "row `{}` is unavailable with no reason at all (PHASE5 §9.3)",
                r.id
            );
        }
    }

    /// The timestamp routine is the one piece of the provenance header with no
    /// external oracle, so it is pinned against known instants.
    ///
    /// Mutant: change `719_468` to `719_469` in `iso8601_utc` — every date
    /// shifts by a day and this test fails.
    #[test]
    fn iso8601_matches_known_instants() {
        let at = |s: u64| iso8601_utc(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(1_000_000_000), "2001-09-09T01:46:40Z");
        // 2024-02-29: a leap day in a century-divisible-by-400 era.
        assert_eq!(at(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(at(1_735_689_599), "2024-12-31T23:59:59Z");
    }

    /// Every command the artifact tells a stranger to run must be a command that
    /// exists — the "Reproducing this" line at the top, and the `reproduce:`
    /// field of every unavailable row. §9.3's "no private benchmark" is worth
    /// nothing if the published incantation exits non-zero, and it is worse than
    /// nothing: it teaches the reader that the rest of the page is decorative.
    ///
    /// Checked against the real `justfile`, the real `xtask` dispatch and the
    /// real target files, so this fails on a renamed recipe as well as on an
    /// invented one. `assemble` is called rather than `skeleton` because the
    /// commands under test are the ones in the shipped rows.
    ///
    /// Mutant (applied, confirmed fatal): put `cargo xtask bench-report` back in
    /// `to_html`'s "Reproducing this" block — `xtask` dispatches no such task and
    /// this fails naming it.
    ///
    /// Mutant (applied, confirmed fatal): rename the `scaling_curve` row's
    /// reproduce recipe to `just tf2-scaling-curve` — no such recipe, and this
    /// fails.
    #[test]
    fn every_command_the_report_names_is_a_command_that_exists() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let justfile = std::fs::read_to_string(root.join("justfile")).expect("justfile");
        let xtask = std::fs::read_to_string(root.join("xtask/src/main.rs")).expect("xtask main");

        // `just <name>` where `<name>` is a recipe: a `justfile` recipe is a
        // line at column 0 whose first token, up to a space or a colon, is the
        // name. Comments start with `#`, so they cannot match a bare name.
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
            let tok: Vec<String> = plain.split_whitespace().map(String::from).collect();
            for (i, t) in tok.iter().enumerate() {
                match t.as_str() {
                    "just" => {
                        let name = word(tok.get(i + 1).map_or("", String::as_str));
                        assert!(
                            recipe_exists(&name),
                            "{whence} says `just {name}`, which is not a justfile recipe"
                        );
                        checked += 1;
                    }
                    "cargo" if tok.get(i + 1).map(String::as_str) == Some("xtask") => {
                        let name = word(tok.get(i + 2).map_or("", String::as_str));
                        assert!(
                            xtask.contains(&format!("Some(\"{name}\")")),
                            "{whence} says `cargo xtask {name}`, which xtask does not dispatch"
                        );
                        checked += 1;
                    }
                    // `--bench X` / `--test X` name files cargo must be able to
                    // find; a renamed harness is the same class of rot.
                    "--bench" | "--test" => {
                        let dir = if t == "--bench" { "benches" } else { "tests" };
                        let name = word(tok.get(i + 1).map_or("", String::as_str));
                        let path = root.join("crates/tf_tree_bench").join(dir);
                        assert!(
                            path.join(format!("{name}.rs")).exists(),
                            "{whence} says `{t} {name}`, but {} has no {name}.rs",
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
        }
        let html = r.to_html();
        let block = html
            .split_once("<h2>Reproducing this</h2>")
            .expect("the report must tell a reader how to reproduce it")
            .1;
        check(block, "the `Reproducing this` block");

        // Guards the parser itself: if `check` silently matched nothing, every
        // assertion above would be vacuous and this test would pass on a report
        // naming only fictional commands.
        assert!(
            checked >= 8,
            "only {checked} commands were checked — the scanner matched nothing"
        );
    }

    /// A real x86-64 `/proc/cpuinfo` fragment (two SMT siblings per core, four
    /// cores across two sockets) and a real aarch64 one, which carries no
    /// `physical id` or `core id` lines at all.
    ///
    /// Non-degenerate on purpose: the x86 text repeats `core id: 0` on socket 0
    /// and again on socket 1, so a parse that keyed on `core id` alone would
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

    /// `/proc/cpuinfo` publishes no core ids on aarch64 — the target
    /// `CLAUDE.md` requires CI to cover — nor in many containers. The parse must
    /// say so rather than answer with the logical CPU count, which is the wrong
    /// denominator by this function's own documentation.
    ///
    /// Mutant (applied, confirmed fatal): make the parse fall back to
    /// `available_parallelism()` instead of returning `None` — the aarch64 half
    /// then yields `Some(n)` and this fails.
    #[test]
    fn a_host_that_publishes_no_core_ids_is_unknown_not_guessed() {
        assert_eq!(physical_cores_from_cpuinfo(X86_CPUINFO), Some(4));
        assert_eq!(physical_cores_from_cpuinfo(AARCH64_CPUINFO), None);
        assert_eq!(physical_cores_from_cpuinfo(""), None);
    }

    /// The verdicts must degrade honestly when the physical core count is
    /// unknown. A silent fallback to logical CPUs makes `logical > physical`
    /// vacuously false — so the SMT reason never fires — and checks the core
    /// budget against sibling threads, and the visible result is
    /// `clock fitness: PASS` / `core budget: PASS` on a host nothing was learned
    /// from. A false PASS is the one failure a refusal machine cannot afford.
    ///
    /// `debug_build: false` throughout, so `fair_for_timing` is decided by the
    /// host facts rather than by the fact that tests run in a debug build.
    ///
    /// Mutant (applied, confirmed fatal): in `assess`, drop both `unknown_cores`
    /// branches and keep only `let physical = detected_physical
    /// .unwrap_or(logical);` — the unknown host then reports
    /// `fair_for_timing == true` and `core_reason == None`, and this fails.
    #[test]
    fn an_unknown_physical_core_count_fails_both_verdicts_and_says_why() {
        // A known-good host: quiet, `performance`, no SMT, cores to spare.
        // This is the control — without it the assertions below could be passing
        // because `assess` refuses everything.
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

        // The same host, except that it published no core ids.
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

        // The SMT reason is the one that goes quiet under a silent fallback, so
        // it is pinned separately: a genuine SMT host must still name it.
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

    /// `--consumers` is operator input. `consumers + 1` wraps to 0 at
    /// `usize::MAX` in a release build, making `physical < needed` false, so the
    /// core budget prints PASS and the N-way rows become claimable — the refusal
    /// inverted by an argument. `just bench-report` builds `--release`, so the
    /// wrap is the reachable half; a debug build panics instead, and neither is
    /// acceptable.
    ///
    /// Mutant (applied, confirmed fatal): restore `let needed = consumers + 1;`
    /// — this test panics with `attempt to add with overflow` under `cargo
    /// nextest` (debug), and reports `enough_cores == true` under `--release`.
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

    /// `measure_lookup_latency` is the only real measurement in this module, and
    /// `assemble` reaches it only when the clock-fitness probe passes — which
    /// running the test suite actively prevents, since the suite is what makes
    /// the machine busy. Left to `assemble`, the one code path whose numbers ever
    /// get published as a claim would be the one path no test executes. So it is
    /// called here directly, at a sample count that costs milliseconds.
    ///
    /// Mutant (applied, confirmed fatal): change the measured loop's bound to
    /// `for _ in 0..samples.min(100)` — the `samples` metric then reports 100 and
    /// the first assertion fails.
    ///
    /// Mutant (applied, confirmed fatal): swap the `p50_ns` and `p999_ns` rows of
    /// the returned vector — the ordering assertion fails.
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

        // Every sample must reach the histogram: a loop that silently recorded
        // fewer would still produce plausible percentiles.
        assert_eq!(get("samples"), SAMPLES as f64);

        let (p50, p99, p999) = (get("p50_ns"), get("p99_ns"), get("p999_ns"));
        assert!(p50 <= p99 && p99 <= p999, "p50={p50} p99={p99} p999={p999}");
        // A p50 of 0 ns would mean the histogram recorded a constant, not a
        // measurement — the degenerate fixture this repo has shipped before.
        assert!(p50 > 0.0, "p50 of {p50} ns is not a measurement");
        // A depth-3 in-process lookup taking a millisecond at the *median* means
        // the unit is wrong or the fixture is not the fixture. Loose enough to
        // survive a contended test runner, tight enough to catch a unit error.
        assert!(p50 < 1_000_000.0, "p50 of {p50} ns is not a depth-3 lookup");
        assert!(get("clock_overhead_p50_ns") >= 0.0);
        assert!(m.iter().all(|x| x.value.is_finite()), "{m:?}");
    }

    /// [`LOOKUP_NOTE`] states the two things `docs/PHASE1.md` §11.3 requires,
    /// and states them about the plan `measure_lookup_latency` actually compiles.
    ///
    /// This is a gate on a *string*, so it is written to fail for the reasons
    /// that matter rather than on any edit: the step count is read out of the
    /// compiled plan and formatted, so changing the fixture's `base_link ->
    /// imu_link` chain fails this test without anyone touching the note, and
    /// changing the note's number fails it without anyone touching the fixture.
    ///
    /// Three mutants were applied and each was observed fatal; all three
    /// outputs are pasted in this branch's report.
    ///
    /// 1. In [`LOOKUP_NOTE`], write "**2 dynamic steps**" — assertion 1:
    ///    *"the plan `map <- imu_link` compiles to 3 dynamic steps, so the note
    ///    must contain `"**3 dynamic steps**"`"*.
    /// 2. Set [`LOOKUP_STAMP_NS`] back to `crate::fixture::NOW_NS`, which is
    ///    what `measure_lookup_latency` queried before `docs/decisions/0013` —
    ///    assertion 2, *"the note names fixture::QUERY_NS and
    ///    `measure_lookup_latency` queries something else, left: 9900000000,
    ///    right: 9899500000"*. Note that it is assertion 2 and **not** the
    ///    off-grid loop that catches this one: the loop runs on the stamp this
    ///    row reads, and the name check runs first.
    /// 3. Move `fixture::QUERY_NS` to `NOW_NS - 1_000_000`, which is off three
    ///    of the four grids and *on* the 1 kHz one — assertion 3, naming the
    ///    edge: *"base_link->imu_link: this row's stamp lands on the 1000 Hz
    ///    grid"*. This is the mutant that shows the loop is live rather than
    ///    shadowed by the equality above it.
    #[test]
    fn the_lookup_row_note_states_what_phase1_requires() {
        // 1. The dynamic-step count, taken from the plan rather than from prose.
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

        // 2. The note names a stamp, and it must be the one the measured loop
        //    queries. Two claims, so two assertions.
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

        // 3. …and "off-grid" must be true of the stamp this row reads. That
        //    duplicates `fixture`'s own test on purpose: `fixture` guards the
        //    constant, this guards the *claim the note publishes about it*, and
        //    the loop runs over every dynamic edge in the fixture — a superset
        //    of the three on this path, so it cannot pass by missing one.
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

    /// End-to-end: the report this tool actually assembles on *this* host must
    /// pass its own §9.3 validation, and the emitted JSON must survive a
    /// round trip through a parser strict about escapes and non-finite numbers.
    ///
    /// The skeleton tests above check `validate` against hand-built reports; if
    /// `assemble` disagrees with them, only this test notices.
    ///
    /// Mutant (verified): in `assemble`, give the `lookup_latency` row
    /// `Status::Measured` and a `p50_ns` metric before the `timing_status`
    /// match — this test fails on any host that does not pass the clock-fitness
    /// probe, which includes the one this was developed on.
    #[test]
    fn the_assembled_report_passes_its_own_validation() {
        let opts = Options {
            // Small enough to run inside a unit test; `assemble`'s structure is
            // what is under test, not the size of the sample.
            lookup_samples: 2_000,
            differential_queries: 512,
            warmup: Duration::from_millis(10),
            ..Options::default()
        };
        let r = assemble(&opts).expect("assemble");
        assert_eq!(r.validate(), Ok(()));

        // The differential row is the one row that is a claim everywhere: it is
        // a disagreement between engines on fixed inputs, not a timing number.
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
        // A differential that scored nothing would report max_deviation 0.0 and
        // look perfect; pinning `compared` is what makes the row non-degenerate.
        assert!(
            compared.value > 100.0,
            "only {} queries scored",
            compared.value
        );

        // §9.3's "say why" means the *actual* why. This row is single-threaded
        // and in-process, so a reason quoting the 16-consumer core budget or a
        // missing ROS 2 install would be a false statement.
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

    /// A pair that cannot resolve §9.2's 5% is reported `unavailable`, on a
    /// host that passes the fitness probe.
    ///
    /// **The spread is not advisory here.** A gate whose noise floor exceeds
    /// its threshold is not a gate, so a run whose per-round band straddles
    /// 1.05 gets no verdict at all — not a pass, not a fail, and no numbers a
    /// baseline could later gate against.
    ///
    /// Mutant: delete the `Verdict::Unresolved` early return in
    /// `embedding_row`. The straddling pair is then reported `measured` with
    /// numbers, and the first two assertions fail.
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

    /// The row's note names the two profiles' settings, and this is what keeps
    /// that statement true rather than merely written down.
    ///
    /// The settings are stated in exactly one place in this repository —
    /// [`EMBEDDING_NOTE`] — and read back here out of the workspace manifest by
    /// the same function the `embed` module's own profile test uses. `just
    /// embed-cost`'s printed output deliberately no longer repeats them.
    ///
    /// Mutant (applied, observed): set `[profile.embedder]`'s `codegen-units`
    /// to `8` in the workspace manifest. Output pasted in the branch's fix
    /// report; the assertion below fires with
    /// `the row note does not state "lto = false, codegen-units = 8"`.
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

        // The note's other half: the control profile whose `lto` is the reason
        // the row cannot be measured by this binary's own build.
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

    /// `build_profile` names the directory cargo built into, not a two-valued
    /// guess from `cfg!(debug_assertions)`.
    ///
    /// The guess is what let a `--profile embedder` run — the profile every
    /// boundary measurement here is taken at, because `lto = false` is the only
    /// setting that leaves the boundary in the binary — call itself `release`
    /// and compare cleanly against a thin-LTO baseline.
    ///
    /// Mutant (applied, observed): replace the `push("build_profile", …)` value
    /// with the literal `"release".to_owned()`. This test fails under `cargo
    /// nextest` with `left: "release", right: "debug"`.
    ///
    /// **What this test does not catch**, stated because a note nobody checked
    /// is how this repository got six wrong attributions: reinstating the old
    /// `cfg!(debug_assertions)` spelling passes here, because under `cargo
    /// nextest` both spellings say `debug`. It fails only when the tests
    /// themselves are built at a third profile
    /// (`cargo nextest run -p tf_tree_bench --cargo-profile embedder -E
    /// 'test(build_profile)'` — run, and observed to fail with
    /// `left: "release", right: "embedder"`). The mutation that fires here is
    /// the hardcode; the mutation that fires there is the guess.
    #[test]
    fn the_build_profile_fact_is_the_directory_cargo_built_into() {
        let p = Provenance::collect();
        assert_eq!(
            p.get("build_profile"),
            Some(crate::embed::PROFILE_DIR),
            "the provenance profile must be the one `build.rs` measured"
        );
    }

    /// And the profile's *meaning* travels beside its name.
    ///
    /// A reader of `results.json` should not have to know this workspace's
    /// `[profile.*]` sections by heart to know whether the crate boundary was
    /// inlined away, so `build_lto` is emitted from the manifest for whichever
    /// profile `build_profile` names.
    ///
    /// Mutant (applied, observed): make `build_lto()` ask
    /// `lto_for_profile_dir` about `crate::embed::REFERENCE_PROFILE` instead of
    /// `PROFILE_DIR`. This test fails under `cargo nextest` with
    /// `left: Some("\"thin\"")`, right the `dev` profile's default `false (…)`.
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
