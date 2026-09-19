//! `docs/PHASE5.md` §9's benchmark artifact: emit `results.json` + `index.html`.
//!
//! Usage:
//!
//! ```text
//! just bench-report                              # -> report/
//! just bench-report --out dir/ --consumers 8
//! ```
//!
//! `tf_tree bench compare --bag` (§9.1) is not wired: `tf_tree_bench` is
//! `publish = false` and the shipped binary must not depend on it, which is a
//! crate-boundary question for a decision record. `xtask` has no bench-report.
//!
//! `Report::validate` makes this binary exit non-zero without writing anything
//! if the report breaks one of §9.3's rules. `--features tf2` (under
//! `./docker/tf2/run.sh`) fills the tf2 correctness column.
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

    // Hand-rolled parsing: `clap` is not a dependency of this crate.
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
            // Rejected, not ignored: every row `--duration` would govern is UNAVAILABLE here.
            "--duration" => bail!(
                "--duration is `docs/PHASE5.md` §9.1's steady-state window for the N-way \
                 comparison rows, every one of which this host reports as UNAVAILABLE, so \
                 the flag would govern nothing here. Accepting it and emitting an \
                 identical report would be a lie of omission. Use --warmup (which is \
                 recorded as warmup_discarded_s) to change the discarded window."
            ),
            "--warmup" => opts.warmup = parse_duration(&value("--warmup")?)?,
            // §9.2's embedding row compares two builds, so it arrives from `just embed-cost`.
            "--embed-cost" => opts.embed_cost = Some(PathBuf::from(value("--embed-cost")?)),
            // Rejected: this harness never opens a `.tft`, so a recording would go unused.
            "--bag" => bail!(
                "--bag is `docs/PHASE5.md` §9.1's spelling for feeding this harness a \
                 recording, and it is not wired up: the two bag-dependent rows \
                 (`tft_16_workers_rss`, `tft_open_vs_bag_parse`) are UNAVAILABLE for a \
                 reason the report states, so a recording would be read and then unused. \
                 §3 itself *is* implemented for MCAP — use `tf_tree ingest --bag` to read \
                 a recording, or `tf_tree freeze --from-bag` to keep the result."
            ),
            // The comparison runs after the report is written so a failing gate leaves the artifact.
            "--check-baseline" => check = Some(PathBuf::from(value("--check-baseline")?)),
            "-h" | "--help" => {
                println!(
                    "usage: bench_report [--out DIR] [--consumers N] [--warmup 2s] \
                     [--embed-cost DIR] [--check-baseline results.json]"
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
    // The clock verdict governs any timing number; the core budget only the N-consumer rows.
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
        // Checked first so a gate that compared nothing never prints as a PASS.
        if cmp.compared_nothing() {
            eprintln!(
                "regression gate compared NOTHING against {}: the baseline carries no \
                 directional metric this build also emits, so a green result here would \
                 mean only that the comparison ran. Regenerate it with \
                 `just bench-baseline-update`.",
                path.display()
            );
            bail!("the regression gate compared nothing");
        }
        if cmp.passed() {
            println!(
                "  PASS — {} directional metric{} held.",
                cmp.checked,
                if cmp.checked == 1 { "" } else { "s" }
            );
        } else {
            eprintln!("regression gate FAILED against {}:", path.display());
            for f in &cmp.failures {
                eprintln!("  - {f}");
            }
            // Two recipes, one per build; a baseline from the other fails every row.
            let recipe = if path.to_string_lossy().contains("results-tf2") {
                "just tf2-bench-baseline-update"
            } else {
                "just bench-baseline-update"
            };
            eprintln!(
                "\nIf the change is intended, regenerate the baseline with \
                 `{recipe}` and put the diff in the commit that causes it."
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
