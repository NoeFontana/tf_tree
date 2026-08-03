//! The Rust half of the end-to-end DDS comparison: generate its inputs, then
//! aggregate its outputs.
//!
//! `ros/tf_tree_bench_ros` holds the nodes, because they need `rclcpp`. This
//! holds the two things that must **not** be hand-written:
//!
//! * `emit-config` — the publisher's plan, the bridge's topology TOML and the
//!   query set, all generated from one [`crate::workload`] entry. That is what
//!   makes "a `dds_bench` row and a `contended_scaling` row on the same workload
//!   name describe the same tree" structural rather than a promise somebody
//!   keeps by editing two files in step. It is also `docs/PHASE5.md` §9.3's
//!   "identical data" requirement, discharged by construction.
//! * `aggregate` — read every consumer's line protocol, merge the histograms
//!   with [`tf_tree_bench::mp::Histogram`] (the same code the Rust harnesses use, so no
//!   second quantile implementation exists to disagree with the first), and emit
//!   both the table and a [`tf_tree_bench::runstore`] run file.
//!
//! # The fourth arm, and how its extra process is paid for
//!
//! Until `docs/decisions/0015` landed, this file carried a `MISSING_ARM`
//! constant printed above the table on every run: there was no multi-process
//! tf_tree arm, because `tft_bridge_create` built a **heap** arena that no
//! second process could attach to. That is fixed rather than reworded — the arm
//! exists, and §9.3's *"if a row cannot be measured fairly, omit it and say
//! why"* no longer applies to it.
//!
//! What replaces the disclosure is an accounting rule, because the new arm runs
//! **N+1** processes to the tf2 arm's N and a table that let the extra one in
//! for free would be worse than the three-arm table it replaced. [`aggregate`]
//! groups processes by the arm label parsed out of the file name and sums
//! `cpu_ns` and `pss_kib` across every process in the group, dividing CPU by the
//! **summed** `consumers` count. The bridge process reports `consumers 0`, so
//! its whole cost lands in the arm it serves, amortized over exactly the
//! consumers it serves. Nothing about that is special-cased for this arm; it is
//! the shape the aggregator already had.
//!
//! `tests/dds_report_aggregate.rs` is what keeps the arm from silently
//! disappearing — and what keeps a "NOT MEASURED" sentence from silently coming
//! back.
//!
//! # What this tool refuses to print
//!
//! Three of the four ways this table could lie are structural rather than
//! numeric, and all three used to produce a *better-looking* row than the truth:
//!
//! * a `.out` truncated before its `cpu_ns` / `pss_kib` lines — [`parse_proc`]
//!   read the absent field as a zero and charged the arm nothing for the
//!   process;
//! * a `tf_tree.processes` arm with no `consumers 0` process in it, i.e. an arm
//!   whose bridge never ran, which is the whole cost the arm exists to account
//!   for ([`check_structure`]);
//! * a bridge that ran and received nothing, which serves fast lookups over an
//!   arena nobody wrote ([`check_structure`] again).
//!
//! The fourth is numeric and is the `<-- FAILING` flag, which now also fires on
//! an arm with *no* lookups at all: `fail_pct` is `NaN` there and `NaN > 5.0` is
//! false, so the flag whose comment says it exists to stop an empty row printing
//! the best latencies did not fire for the emptiest row there is.
//!
//! # Nothing here is remembered from another run
//!
//! The disclosure under the table about the `.processes` arms' bimodal `svc`
//! column is computed from the histograms of the run being reported. An earlier
//! revision printed five literal microsecond values from one past run as
//! "measured here" — on somebody else's host that is a false statement about
//! their machine, printed above their own table, and it replaced a disclosure
//! that had been permanently true. `docs/benchmarks/tf2.md` is where a worked
//! example with its control belongs; this tool states the shape of the run in
//! front of it.
// This binary's output IS its result.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use tf_tree_bench::mp::Histogram;
use tf_tree_bench::report::Metric;
use tf_tree_bench::runstore::{Run, RunRow};
use tf_tree_bench::workload::{self, EdgeDecl};

/// Query pairs handed to the C++ consumers.
///
/// Capped for the same reason `contended_scaling` caps them: a node resolving
/// more than a handful of chains per cycle is not the shape being modelled, and
/// `recorded` would otherwise hand 256 pairs to every arm.
const MAX_PAIRS: usize = 8;

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("emit-config") => emit_config(&argv[1..]),
        Some("aggregate") => aggregate(&argv[1..]),
        _ => {
            eprintln!(
                "usage:\n  \
                 dds_report emit-config --workload NAME --out DIR\n  \
                 dds_report aggregate --dir DIR [--json PATH]\n\n\
                 Driven by `just dds-bench`; see ros/dds_bench.sh."
            );
            std::process::exit(2);
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

// ---------------------------------------------------------------------------
// emit-config
// ---------------------------------------------------------------------------

fn emit_config(args: &[String]) -> Result<()> {
    let name = flag(args, "--workload").unwrap_or_else(|| "robot".to_owned());
    let out =
        PathBuf::from(flag(args, "--out").ok_or_else(|| anyhow!("emit-config needs --out DIR"))?);
    let w = workload::by_name(&name)?;
    let edges = w.edges()?;
    let pairs = w.query_pairs()?;
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;

    // --- the publisher's plan ------------------------------------------
    let mut plan = String::new();
    let _ = writeln!(
        plan,
        "# generated by `dds_report emit-config --workload {name}`"
    );
    for e in &edges {
        match e {
            EdgeDecl::Static {
                parent,
                child,
                pose,
            } => {
                let _ = writeln!(
                    plan,
                    "S {parent} {child} {} {} {} {} {} {} {}",
                    pose.q.w, pose.q.x, pose.q.y, pose.q.z, pose.t.x, pose.t.y, pose.t.z
                );
            }
            EdgeDecl::Dynamic {
                parent,
                child,
                rate_hz,
                ..
            } => {
                let _ = writeln!(plan, "D {parent} {child} {rate_hz}");
            }
        }
    }
    write(&out.join("plan.txt"), &plan)?;

    // --- the bridge's topology -----------------------------------------
    //
    // `interp = "lerpslerp"` because that is tf2's policy, and a comparison in
    // which the two engines interpolate differently measures the interpolation
    // rather than the engine. Every Rust harness in this suite builds with
    // `InterpPolicy::LerpSlerp` for the same reason.
    let mut toml = String::new();
    let _ = writeln!(
        toml,
        "# generated by `dds_report emit-config --workload {name}`\n\
         # Do not hand-edit: it is regenerated on every `just dds-bench`.\n\
         [topology]\n\
         interp = \"lerpslerp\""
    );
    for e in &edges {
        match e {
            EdgeDecl::Static {
                parent,
                child,
                pose,
            } => {
                let _ = writeln!(
                    toml,
                    "\n[[edge]]\nparent = \"{parent}\"\nchild = \"{child}\"\n\
                     kind = \"static\"\npose = [{}, {}, {}, {}, {}, {}, {}]",
                    pose.q.w, pose.q.x, pose.q.y, pose.q.z, pose.t.x, pose.t.y, pose.t.z
                );
            }
            EdgeDecl::Dynamic {
                parent,
                child,
                rate_hz,
                history_secs,
            } => {
                // `rate_hz` + `history_secs` rather than `capacity`, and the
                // choice is not cosmetic: `config.rs` records the declared rate
                // in `EdgeRecord::nominal_rate_mhz`, which is the only evidence
                // `tf_tree doctor`'s TFT007 has that an observed rate is *wrong*
                // rather than merely what it is. An edge sized by `capacity`
                // declares no rate and TFT007 says so for it.
                let _ = writeln!(
                    toml,
                    "\n[[edge]]\nparent = \"{parent}\"\nchild = \"{child}\"\n\
                     kind = \"dynamic\"\nrate_hz = {rate_hz}\nhistory_secs = {history_secs}"
                );
            }
        }
    }
    write(&out.join("topology.toml"), &toml)?;

    // --- the query set --------------------------------------------------
    let mut queries = String::new();
    let _ = writeln!(queries, "# target source");
    for (t, s) in pairs.iter().take(MAX_PAIRS) {
        let _ = writeln!(queries, "{t} {s}");
    }
    write(&out.join("queries.txt"), &queries)?;

    let shape = w.estimate()?;
    println!("workload {name}: {}", shape.describe());
    println!(
        "wrote {}/{{plan.txt,topology.toml,queries.txt}}",
        out.display()
    );
    Ok(())
}

fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

// ---------------------------------------------------------------------------
// aggregate
// ---------------------------------------------------------------------------

/// One consumer process's parsed output.
///
/// **`cpu_ns` and `pss_kib` are `Option`, and that is the whole point.** They
/// used to be `u64` filled by a parser that returns `0` for a line it never
/// saw, so a truncated `.out` — a process killed between its histograms and its
/// stats block, a full disk, a driver that stopped waiting — was
/// indistinguishable from a process that used no CPU and mapped no memory. The
/// resulting row is not merely wrong, it is wrong in the flattering direction
/// and it inverts the disclosure §9.3 requires: [`parse_proc`] measured a
/// bridge file truncated after its histograms at **0.146 %/consumer against a
/// true 0.847**, and a PSS that put `tf_tree.processes` on the winning side of
/// the memory comparison it actually loses. `procs`, `fail%` and the exit
/// status were all clean.
#[derive(Default)]
struct Proc {
    service: Histogram,
    cycle: Histogram,
    ok: u64,
    err: u64,
    cpu_ns: Option<u64>,
    pss_kib: Option<u64>,
    consumers: usize,
    warmup_s: f64,
    measured_s: f64,
    bridge_transforms: u64,
    bridge_dropped: u64,
}

/// Parse one process's `.out`, refusing one that is missing a cost field.
///
/// Takes the path rather than the text alone so the refusal names the file an
/// operator has to go and look at; the two stats lines are the last thing
/// `bench_consumer` prints, so their absence is nearly always truncation.
fn parse_proc(path: &Path, text: &str) -> Result<Proc> {
    let mut p = Proc {
        service: Histogram::new(),
        cycle: Histogram::new(),
        ..Proc::default()
    };
    for line in text.lines() {
        let mut it = line.split_whitespace();
        match it.next() {
            Some("consumer") => {
                // `consumer <i> service <hist…>` / `cycle` / `ok <n> err <n>`
                let _index = it.next();
                match it.next() {
                    Some("service") => {
                        let rest = line
                            .split_once(" service ")
                            .map(|x| x.1)
                            .unwrap_or_default();
                        p.service.merge(
                            &Histogram::decode(rest).map_err(|e| anyhow!("service hist: {e}"))?,
                        );
                    }
                    Some("cycle") => {
                        let rest = line.split_once(" cycle ").map(|x| x.1).unwrap_or_default();
                        p.cycle.merge(
                            &Histogram::decode(rest).map_err(|e| anyhow!("cycle hist: {e}"))?,
                        );
                    }
                    Some("ok") => {
                        p.ok += it.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                        let _ = it.next(); // "err"
                        p.err += it.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                    }
                    _ => {}
                }
            }
            Some("cpu_ns") => p.cpu_ns = it.next().and_then(|v| v.parse().ok()),
            Some("pss_kib") => p.pss_kib = it.next().and_then(|v| v.parse().ok()),
            Some("consumers") => p.consumers = num(it.next()) as usize,
            Some("warmup_s") => p.warmup_s = fnum(it.next()),
            Some("measured_s") => p.measured_s = fnum(it.next()),
            Some("bridge_transforms") => p.bridge_transforms = num(it.next()),
            Some("bridge_dropped") => p.bridge_dropped = num(it.next()),
            _ => {}
        }
    }
    for (field, present) in [
        ("cpu_ns", p.cpu_ns.is_some()),
        ("pss_kib", p.pss_kib.is_some()),
    ] {
        if !present {
            bail!(
                "{} has no `{field}` line. That file is truncated — `bench_consumer` prints \
                 `cpu_ns` and `pss_kib` last, after every histogram — and a missing cost \
                 field is NOT a zero one. Aggregating it would charge this arm nothing for \
                 the process, which is the one direction a benchmark must never round. Its \
                 `.err` file says why it stopped.",
                path.display()
            );
        }
    }
    Ok(p)
}

fn num(s: Option<&str>) -> u64 {
    s.and_then(|v| v.parse().ok()).unwrap_or(0)
}
fn fnum(s: Option<&str>) -> f64 {
    s.and_then(|v| v.parse().ok()).unwrap_or(0.0)
}

/// One arm: every consumer process that ran under the same label.
struct Arm {
    label: String,
    engine: String,
    procs: Vec<Proc>,
}

impl Arm {
    /// The `tf_tree.processes` shape: one bridge process plus N attached
    /// consumers.
    ///
    /// Matched on the label rather than on the contents because it is what the
    /// contents are checked *against* — `check_structure` asks whether an arm
    /// that is supposed to have a bridge has one, and an arm identified by
    /// having a bridge could not be asked. `tf2.processes` ends in the same
    /// word and has no bridge by construction, so the engine is part of the
    /// test.
    fn is_bridge_and_attach(&self) -> bool {
        self.engine == "tf_tree" && self.label.ends_with(".processes")
    }
}

/// The two invariants the fairness argument rests on, checked rather than
/// assumed.
///
/// Both are about the same thing: an arm whose whole claim is "one process pays
/// the deserialization for all of them" is only comparable if that process ran
/// and did the work. Neither is visible in any column — an arm missing its
/// bridge prints a *better* row, and an arm whose bridge ingested nothing
/// prints the best latencies in the table over an arena nobody wrote.
fn check_structure(arms: &BTreeMap<String, Arm>) -> Result<()> {
    for arm in arms.values() {
        if !arm.is_bridge_and_attach() {
            continue;
        }
        if !arm.procs.iter().any(|p| p.consumers == 0) {
            bail!(
                "arm `{}` has {} process(es) and not one of them reports `consumers 0`, so \
                 no bridge ran in it. Its consumers attached to an arena somebody else \
                 filled, or to none — either way the row would show N processes doing \
                 tf_tree's work with tf2's ingest cost charged to nobody, which is the \
                 single most flattering row this table can print.",
                arm.label,
                arm.procs.len()
            );
        }
        let transforms: u64 = arm.procs.iter().map(|p| p.bridge_transforms).sum();
        if transforms == 0 {
            bail!(
                "arm `{}` ran a bridge that received 0 transforms. Every lookup it served \
                 came from an arena nothing was writing, so its `svc` column is the cost of \
                 reading stale memory and its `fail%` is whatever the static edges happen to \
                 answer. `bridge_transforms` has been printed since this arm existed and \
                 gated nothing until now; the bridge's `.err` file says whether it saw `/tf` \
                 at all.",
                arm.label
            );
        }
    }
    Ok(())
}

fn aggregate(args: &[String]) -> Result<()> {
    let dir =
        PathBuf::from(flag(args, "--dir").ok_or_else(|| anyhow!("aggregate needs --dir DIR"))?);
    let json = flag(args, "--json").map(PathBuf::from);
    let workload = flag(args, "--workload").unwrap_or_else(|| "robot".to_owned());

    // Files are named `<arm>.<index>.out`, written by the shell driver — the
    // label is everything before the last `.` of the stem, so an arm label may
    // itself contain dots (`tf_tree.processes` does). Grouping on the name
    // rather than on the contents keeps the driver and this in step through one
    // convention instead of a second protocol, and it is what puts a bridge
    // process and the consumers it serves in the same row.
    let mut arms: BTreeMap<String, Arm> = BTreeMap::new();
    let entries = std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
    for e in entries {
        let path = e?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("out") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|x| x.to_str())
            .ok_or_else(|| anyhow!("bad file name {}", path.display()))?;
        let label = stem.rsplitn(2, '.').last().unwrap_or(stem).to_owned();
        let engine = if label.starts_with("tf2") {
            "tf2"
        } else {
            "tf_tree"
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        arms.entry(label.clone())
            .or_insert_with(|| Arm {
                label,
                engine: engine.to_owned(),
                procs: Vec::new(),
            })
            .procs
            .push(parse_proc(&path, &text)?);
    }

    if arms.is_empty() {
        bail!(
            "no `*.out` files in {} — every consumer failed, or the driver wrote elsewhere",
            dir.display()
        );
    }

    // **Before a single row is printed.** A structural fault found halfway
    // through the loop below would leave an operator reading three good rows
    // and an error, which is exactly the shape somebody quotes the good rows
    // from.
    check_structure(&arms)?;

    println!("tf_tree vs tf2, end to end over a real DDS  [workload: {workload}]");
    println!("=====================================================================");
    println!(
        "{:<26} {:>6} {:>6} | {:>9} {:>9} {:>10} | {:>10} | {:>9} {:>9} {:>7}",
        "arm",
        "procs",
        "cons",
        "svc p50",
        "svc p99",
        "svc p99.9",
        "cyc p99.9",
        "CPU %/con",
        "PSS MiB",
        "fail%"
    );
    println!(
        "{:<26} {:>13} | {:>31} | {:>10} |",
        "", "", "microseconds", "us"
    );

    let mut run = Run::begin(1);

    /// The reference point the wake-from-idle disclosure below is stated
    /// against: a fixed round number, not a remembered measurement. Every
    /// composed arm this suite has produced sits under it and every wake-from-
    /// idle sample sits well over it, so "fraction under 1 µs" separates the
    /// two modes without needing to know either one's value in advance.
    const FAST_MODE_NS: u64 = 1_000;
    // `(label, svc p50 ns, fraction of svc samples under FAST_MODE_NS)`, for
    // the arms that run one consumer per process. Collected while the rows are
    // folded so the disclosure after the table is this run's own arithmetic.
    let mut idle_shape: Vec<(String, u64, f64)> = Vec::new();

    for arm in arms.values() {
        let mut service = Histogram::new();
        let mut cycle = Histogram::new();
        let (mut ok, mut err, mut cpu_ns, mut pss_kib) = (0u64, 0u64, 0u64, 0u64);
        let mut consumers = 0usize;
        let mut measured_s = 0.0f64;
        let mut warmup_s = 0.0f64;
        let (mut bt, mut bd) = (0u64, 0u64);
        for p in &arm.procs {
            // `parse_proc` refuses a file missing either, so this cannot fall
            // back to a zero that would charge the arm nothing.
            let (Some(p_cpu_ns), Some(p_pss_kib)) = (p.cpu_ns, p.pss_kib) else {
                unreachable!("parse_proc returns no Proc with an absent cost field")
            };
            service.merge(&p.service);
            cycle.merge(&p.cycle);
            ok += p.ok;
            err += p.err;
            cpu_ns += p_cpu_ns;
            // PSS is a level per process; summing across processes is correct
            // precisely because PSS already divides each shared page by its
            // mapper count. That is the whole reason `mp.rs` uses it and not
            // RSS: summed RSS counts one shared arena once per consumer.
            //
            // It is also why a per-consumer PSS figure does NOT compare across
            // arms with different process counts — see the footer.
            pss_kib += p_pss_kib;
            consumers += p.consumers;
            measured_s = measured_s.max(p.measured_s);
            warmup_s = warmup_s.max(p.warmup_s);
            bt += p.bridge_transforms;
            bd += p.bridge_dropped;
        }

        let total = ok + err;
        let fail_pct = if total == 0 {
            f64::NAN
        } else {
            err as f64 / total as f64 * 100.0
        };
        let cpu_pct_per_consumer = if consumers == 0 || measured_s == 0.0 {
            f64::NAN
        } else {
            (cpu_ns as f64 / consumers as f64) / (measured_s * 1e9) * 100.0
        };
        let us = |v: u64| v as f64 / 1000.0;

        // A row where nearly every lookup failed is not a fast row, it is an
        // empty one — and without this flag it would print the best latencies
        // in the table.
        //
        // **`total == 0` is the case the comment above described and the test
        // did not cover.** With no lookups at all `fail_pct` is `NaN`, every
        // comparison against `NaN` is false, and the flag whose entire purpose
        // is "an empty row must not print the best latencies" did not fire for
        // the emptiest row possible. An arm whose consumers all timed out on
        // `--attach-timeout` reaches exactly that state.
        let flag = if total == 0 || fail_pct > 5.0 {
            " <-- FAILING"
        } else {
            ""
        };

        if arm.label.ends_with(".processes") {
            idle_shape.push((
                arm.label.clone(),
                service.quantile(0.50),
                service.fraction_below(FAST_MODE_NS),
            ));
        }

        println!(
            "{:<26} {:>6} {:>6} | {:>9.2} {:>9.2} {:>10.2} | {:>10.1} | {:>9.3} {:>9.2} \
             {:>7.2}{flag}",
            arm.label,
            arm.procs.len(),
            consumers,
            us(service.quantile(0.50)),
            us(service.quantile(0.99)),
            us(service.quantile(0.999)),
            us(cycle.quantile(0.999)),
            cpu_pct_per_consumer,
            pss_kib as f64 / 1024.0,
            fail_pct,
        );

        run.push(
            RunRow::new("dds_bench", &workload, &arm.engine, &arm.label)
                .metric(
                    Metric::new("service_p50_ns", service.quantile(0.50) as f64, "ns")
                        .lower_is_better(0.40),
                )
                .metric(
                    Metric::new("service_p99_ns", service.quantile(0.99) as f64, "ns")
                        .lower_is_better(0.40),
                )
                .metric(
                    Metric::new("service_p99_9_ns", service.quantile(0.999) as f64, "ns")
                        .lower_is_better(0.40),
                )
                .metric(Metric::new(
                    "cycle_p99_9_ns",
                    cycle.quantile(0.999) as f64,
                    "ns",
                ))
                .metric(Metric::new("total_pss_kib", pss_kib as f64, "KiB").lower_is_better(0.25))
                .metric(
                    Metric::new("cpu_pct_per_consumer", cpu_pct_per_consumer, "%")
                        .lower_is_better(0.40),
                )
                .metric(Metric::new("processes", arm.procs.len() as f64, "procs"))
                .metric(Metric::new("consumers", consumers as f64, "threads"))
                .metric(Metric::new("lookups_ok", ok as f64, "lookups"))
                .metric(Metric::new("lookups_failed", err as f64, "lookups"))
                .metric(Metric::new("warmup_discarded_s", warmup_s, "s"))
                .metric(Metric::new("measured_s", measured_s, "s"))
                .metric(Metric::new("bridge_transforms", bt as f64, "transforms"))
                .metric(Metric::new("bridge_dropped", bd as f64, "transforms")),
        );
    }

    println!();
    println!("`svc` is the engine call itself; `cyc p99.9` is measured from each tick's INTENDED");
    println!("time and on an idle machine is mostly scheduler wakeup. CPU is per consumer, so a");
    println!("column that stays flat as consumers rise is the O(1)-in-consumers claim holding.");
    println!("PSS sums across processes and counts each shared page once — summed RSS would");
    println!("count one shared arena n times over. The COLUMN is comparable across arms; a PSS");
    println!("PER CONSUMER derived from it is NOT, because PSS divides each shared page by its");
    println!("mapper count and these arms have different process counts (4 against 5), so the");
    println!("same rclcpp text is charged at S/4 to one and S/5 to the other before any");
    println!("architectural difference. Extrapolate from the totals, not from a quotient.");
    println!();
    println!("Every arm queries the same pairs at the same rate with the same 100 ms lag, from");
    println!("one publisher, with PHASE4 §5.2's QoS. The warm-up window is discarded and");
    println!("reported. The four arms are the same executable with a different --mode.");
    println!();
    println!("ARM ORDER IS A CONFOUND AND IT IS NOT RANDOMISED. ros/dds_bench.sh runs them in a");
    println!("fixed sequence — tf2.processes, tf2.composed, tf_tree.composed, tf_tree.processes");
    println!("— after a 3 s discovery settle, so the last arm meets the warmest cache, the");
    println!("settled DDS discovery and whatever thermal state the first three left. Nothing");
    println!("here corrects for it; it is disclosed so a reader can weigh it.");
    println!();
    println!("READ `tf_tree.processes` WITH THESE THREE FACTS (PHASE5 §9.3):");
    println!("  * Its `procs` count is N+1: one bridge process plus N attached consumers. The");
    println!("    bridge reports `consumers 0`, so its CPU and PSS are summed INTO this row and");
    println!("    divided by the consumers it serves — never left beside the table. It is also");
    println!("    a real operational cost tf2 does not have: one more process to supervise,");
    println!("    plus a memfd, a rendezvous entry and a participant slot.");
    println!("  * Its consumers do no deserialization and hold no per-node cache, and that is");
    println!("    the architecture under test rather than a shortcut in the harness: the");
    println!("    bridge in the same row pays that cost once for all of them. Query count,");
    println!("    rate, lag, warm-up and measured window are identical to every other arm.");
    println!("  * Each of its consumers still constructs and spins a full rclcpp node with a");
    println!("    DDS participant, exactly like a `tf2.processes` consumer, and subscribes to");
    println!("    nothing. Dropping the participant would move ~14 MiB per process out of the");
    println!("    row and measure `no rclcpp` rather than `no /tf`.");
    println!();
    println!("AND ONE ABOUT THE `svc` COLUMN OF *BOTH* `.processes` ARMS, COMPUTED FROM THIS");
    println!("RUN AND NOT REMEMBERED FROM ANOTHER. Their query threads wake at --hz on a host");
    println!("whose cores are free to idle between ticks, so the p50 carries a wake-from-idle");
    println!("that neither composed arm pays. The distribution is bimodal, so the p50 alone");
    println!("does not describe it — this run's own split:");
    for (label, p50_ns, fast) in &idle_shape {
        println!(
            "  {label:<20} svc p50 {:>7.2} us, {:>5.1}% of its samples under {:.2} us",
            *p50_ns as f64 / 1000.0,
            fast * 100.0,
            FAST_MODE_NS as f64 / 1000.0,
        );
    }
    println!("A large fast fraction under a p50 far above that threshold is two modes, not a");
    println!("slow engine, and it hits the consumer that does LEAST work hardest — an idle");
    println!("core is what a cheap consumer earns. It moves from run to run. Pin the cores");
    println!("(docs/benchmarks/tf2.md's runbook) before quoting either p50; that document");
    println!("carries the worked example, including the control that isolates the host's idle");
    println!("behaviour from the attach path. The CPU and PSS columns are what this arm exists");
    println!("to measure, and this tool sees one run and can say nothing about their spread.");

    if let Some(path) = &json {
        run.write(path)?;
        println!("\nwrote {}", path.display());
    }
    Ok(())
}
