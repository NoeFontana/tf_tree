//! `docs/PHASE5.md` §9's benchmark artifact: emit `results.json` + `index.html`.
//!
//! Usage:
//!
//! ```text
//! just bench-report                              # -> report/
//! just bench-report --out dir/ --consumers 8
//! ```
//!
//! §9.1 spells the entry point `tf_tree bench compare --bag run.mcap ...`. That
//! spelling is **not wired up**, and deliberately: its `--bag` argument is §3
//! (bag ingestion), which `docs/PHASE5.md` §0.0 records as not implemented, so a
//! CLI subcommand accepting it would advertise a capability that does not exist.
//! When §3 lands, the CLI grows the subcommand and calls this same code.
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
use tf_tree_bench::report::{self, Options, Status};

fn main() -> Result<()> {
    let mut out = PathBuf::from("report");
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
            "--consumers" => opts.consumers = value("--consumers")?.parse()?,
            "--duration" => opts.duration = parse_duration(&value("--duration")?)?,
            "--warmup" => opts.warmup = parse_duration(&value("--warmup")?)?,
            "--bag" => bail!(
                "--bag is `docs/PHASE5.md` §3 (bag ingestion), which is not implemented \
                 (see §0.0's status table). Run without --bag: the report is emitted \
                 with the bag-dependent rows marked UNAVAILABLE and the reason stated."
            ),
            "-h" | "--help" => {
                println!(
                    "usage: bench_report [--out DIR] [--consumers N] [--duration 120s] \
                     [--warmup 2s]"
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
