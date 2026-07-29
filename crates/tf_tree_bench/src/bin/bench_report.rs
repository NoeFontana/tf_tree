//! `docs/PHASE5.md` §9's benchmark artifact: emit `results.json` + `index.html`.
//!
//! Usage:
//!
//! ```text
//! just bench-report                              # -> report/
//! just bench-report --out dir/ --consumers 8
//! ```
//!
//! There is no `cargo xtask bench-report`: `xtask` dispatches `loom | miri |
//! bench-gate | headers` only.
//!
//! §9.1 spells the entry point `tf_tree bench compare --bag run.mcap ...`. That
//! spelling is **not wired up.** The reason is no longer the one an earlier
//! revision of this comment gave — it said §3 (bag ingestion) was unimplemented,
//! and §3 has since landed for MCAP. Two reasons remain, and both are about this
//! crate rather than about the roadmap:
//!
//! * `tf_tree_bench` is `publish = false` and carries `criterion`, `proptest`
//!   and an optional ROS-adjacent dependency. Making the shipped `tf_tree`
//!   binary depend on it to gain a subcommand would drag a benchmark harness
//!   into every install.
//! * Every row `--bag` would feed is UNAVAILABLE here for a reason the report
//!   states, so the subcommand would accept a recording and use none of it.
//!
//! Wiring it is therefore a crate-boundary question, which `CLAUDE.md` routes to
//! a decision record rather than to a PR.
//!
//! The interesting behaviour is in `tf_tree_bench::report`, in particular
//! `Report::validate`: this binary **exits non-zero without writing anything**
//! if the assembled report breaks one of §9.3's rules. A tool that cannot emit
//! an over-claiming report is a stronger guarantee than a reviewer who promises
//! not to write one.
//!
//! Run it inside `./docker/tf2/run.sh` (or on any host with ROS 2) with
//! `--features tf2` and the tf2 correctness column fills in; the comparison rows
//! additionally need a host with more physical cores than consumers.
// This binary's output *is* its result.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tf_tree_bench::baseline;
use tf_tree_bench::report::{self, Options, Status};

fn main() -> Result<()> {
    let mut out = PathBuf::from("report");
    let mut check: Option<PathBuf> = None;
    let mut opts = Options::default();

    // Hand-rolled argument parsing: this is a benchmark binary in a
    // `publish = false` crate, and `clap` is not one of its dependencies. The
    // CLI (`tf_tree bench compare`) is where the parsed surface lives.
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |name: &str| -> Result<String> {
            args.next().with_context(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--out" => out = PathBuf::from(value("--out")?),
            "--consumers" => {
                opts.consumers = value("--consumers")?.parse()?;
                if opts.consumers == 0 {
                    bail!(
                        "--consumers 0 describes no comparison: the N-way rows would run \
                         a publisher and nobody, and the `bridge_supervision` cost would \
                         be stated about 0 consumers."
                    );
                }
            }
            // Rejected rather than accepted-and-ignored. §9.1 spells `--duration`
            // as the steady-state window per point, and every point it would
            // govern is an N-way comparison row that is UNAVAILABLE here (§0.0:
            // no ROS 2, and the core budget). The one row this tool measures
            // itself is bounded by lookup samples, not by wall clock. Taking the
            // flag and producing a byte-identical report is exactly the quiet
            // dishonesty §9.3 is written against.
            "--duration" => bail!(
                "--duration is `docs/PHASE5.md` §9.1's steady-state window for the N-way \
                 comparison rows, every one of which this host reports as UNAVAILABLE, so \
                 the flag would govern nothing here. Accepting it and emitting an \
                 identical report would be a lie of omission. Use --warmup (which is \
                 recorded as warmup_discarded_s) to change the discarded window."
            ),
            "--warmup" => opts.warmup = parse_duration(&value("--warmup")?)?,
            // Still rejected, but **not** for the reason an earlier revision
            // gave. That one said §3 "is not implemented"; §3 landed (MCAP), and
            // `tf_tree ingest --bag` / `tf_tree freeze --from-bag` are how a
            // recording is read today. What is missing is the wiring *here*:
            // this harness never opens a `.tft`, so a recording handed to it
            // would be parsed and then have nothing to feed.
            "--bag" => bail!(
                "--bag is `docs/PHASE5.md` §9.1's spelling for feeding this harness a \
                 recording, and it is not wired up: the two bag-dependent rows \
                 (`tft_16_workers_rss`, `tft_open_vs_bag_parse`) are UNAVAILABLE for a \
                 reason the report states, so a recording would be read and then unused. \
                 §3 itself *is* implemented for MCAP — use `tf_tree ingest --bag` to read \
                 a recording, or `tf_tree freeze --from-bag` to keep the result."
            ),
            // `docs/PHASE5.md` §10's "benchmark artifact as a regression gate".
            // The comparison runs *after* the report is written, so a failing
            // gate still leaves the artifact on disk to look at — a gate that
            // deletes the evidence it failed on is unusable.
            "--check-baseline" => check = Some(PathBuf::from(value("--check-baseline")?)),
            "-h" | "--help" => {
                println!(
                    "usage: bench_report [--out DIR] [--consumers N] [--warmup 2s] \
                     [--check-baseline results.json]"
                );
                println!(
                    "  --duration and --bag are `docs/PHASE5.md` §9.1 spellings that would \
                     govern nothing on this host; both are rejected with the reason rather \
                     than accepted and ignored. Neither refusal is a claim about what phase \
                     has landed — §0.0's status table owns that, and a copy of it here has \
                     already gone stale once."
                );
                return Ok(());
            }
            other => bail!("unknown argument `{other}`"),
        }
    }

    let report = report::assemble(&opts)?;
    if let Err(violations) = report.validate() {
        eprintln!("bench_report: the assembled report violates PHASE5 §9.3:");
        for v in &violations {
            eprintln!("  - {v}");
        }
        bail!("refusing to write a report that over-claims");
    }

    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let json = out.join("results.json");
    let html = out.join("index.html");
    std::fs::write(&json, report.to_json())?;
    std::fs::write(&html, report.to_html())?;

    println!("tf_tree benchmark report — docs/PHASE5.md §9\n");
    for f in &report.provenance.facts {
        println!("  {:<22} {}", f.key, f.value);
    }
    println!(
        "  {:<22} {}",
        "warmup_discarded_s", report.warmup_discarded_s
    );
    println!();
    // The two verdicts are printed apart, because they refuse different rows:
    // the clock verdict governs any timing number, the core budget governs only
    // the rows that run `--consumers` processes at once.
    if report.fitness.fair_for_timing {
        println!("clock fitness: PASS — timing rows on this host are claims.");
    } else {
        println!(
            "clock fitness: FAIL — timing rows are {}:",
            if report.fitness.forced {
                "INDICATIVE (TF_TREE_BENCH_FORCE=1), not claims"
            } else {
                "UNAVAILABLE"
            }
        );
        for r in &report.fitness.reasons {
            println!("  - {r}");
        }
    }
    match report.fitness.core_reason.as_deref() {
        None => println!(
            "core budget:  PASS — {} physical cores for {} consumers plus a publisher.\n",
            report.fitness.physical_cores, report.fitness.consumers
        ),
        Some(why) => println!("core budget:  FAIL — {why}\n"),
    }
    for row in &report.rows {
        println!("[{:^12}] {}", row.status.as_str().to_uppercase(), row.title);
        if row.status == Status::Unavailable {
            println!("               why: {}", row.reason);
            println!("               get it with: {}", row.reproduce);
        } else {
            for m in row.tf_tree.iter().chain(&row.tf2) {
                println!("               {} = {} {}", m.key, m.value, m.unit);
            }
        }
    }
    println!("\nwhere tf_tree is worse (PHASE5 §9.3):");
    for w in &report.worse {
        println!("  - {}", w.topic);
    }
    println!("\nwrote {} and {}", json.display(), html.display());

    if let Some(path) = check {
        let cmp = baseline::check_file(&path, &report)?;
        println!("\nregression gate against {} (PHASE5 §10):", path.display());
        for n in &cmp.notes {
            println!("  note: {n}");
        }
        if cmp.passed() {
            // The count is printed on the passing path deliberately. "0
            // failures" is also what a gate that compared nothing prints, and a
            // regression gate that has quietly stopped comparing is the exact
            // failure this whole file is written against.
            println!(
                "  PASS — {} directional metric{} held.",
                cmp.checked,
                if cmp.checked == 1 { "" } else { "s" }
            );
            if cmp.checked == 0 {
                eprintln!(
                    "  ...but zero metrics were compared. The baseline carries no \
                     directional metric this build also emits, so the gate is green \
                     without having checked anything. Regenerate it with \
                     `just bench-baseline-update`."
                );
                bail!("the regression gate compared nothing");
            }
        } else {
            eprintln!("regression gate FAILED against {}:", path.display());
            for f in &cmp.failures {
                eprintln!("  - {f}");
            }
            eprintln!(
                "\nIf the change is intended, regenerate the baseline with \
                 `just bench-baseline-update` and put the diff in the commit that \
                 causes it."
            );
            bail!(
                "{} regression(s) against the committed baseline",
                cmp.failures.len()
            );
        }
    }
    Ok(())
}

/// `120s`, `2s`, `500ms`, or a bare number of seconds.
fn parse_duration(s: &str) -> Result<Duration> {
    if let Some(ms) = s.strip_suffix("ms") {
        return Ok(Duration::from_millis(ms.parse()?));
    }
    let secs = s.strip_suffix('s').unwrap_or(s);
    Ok(Duration::from_secs_f64(secs.parse()?))
}
