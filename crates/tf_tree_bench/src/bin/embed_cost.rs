//! `docs/PHASE5.md` §9.2's embedding measurements.
//!
//! Two modes:
//!
//! ```text
//! embed_cost --json target/embed-cost/embedder.json   # measure this build
//! embed_cost --compare target/embed-cost              # print both measurements
//! ```
//!
//! `just embed-cost` runs all of it; run that rather than this. The measurement
//! is only worth as much as the pinning and the profile flags the recipe
//! supplies, and both are easy to leave off by hand.
//!
//! A single `--json` run already contains **§9.2's gated row**: the crate
//! boundary is measured inside one build, by timing two identical bodies that
//! differ only in which crate they were compiled in. `--compare` adds the
//! **exploratory** half, which needs the second build: what the embedder's own
//! `[profile.*]` costs. The design behind both is in
//! [`tf_tree_bench::embed`] — in particular why the in-crate column has to live
//! in `tf_tree_core` and why the profile comparison is not gated.
// This binary's output *is* its result.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use tf_tree_bench::embed::{self, Pair, Run};

fn main() -> Result<()> {
    let mut json: Option<PathBuf> = None;
    let mut compare: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |name: &str| -> Result<String> {
            args.next().with_context(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--json" => json = Some(PathBuf::from(value("--json")?)),
            "--compare" => compare = Some(PathBuf::from(value("--compare")?)),
            "-h" | "--help" => {
                println!("usage: embed_cost [--json OUT.json] [--compare DIR]");
                return Ok(());
            }
            other => bail!("unknown argument `{other}`"),
        }
    }

    if let Some(dir) = compare {
        let pair = Pair::load(&dir)?;
        report(&pair);
        return Ok(());
    }

    // A debug build of this probe measures a different program, and the whole
    // measurement is a statement about generated code. Refusing is cheaper than
    // explaining the number later.
    if cfg!(debug_assertions) {
        bail!(
            "this is a debug build: debug_assertions are on, so the timing describes a \
             program nobody ships. Run `just embed-cost`, which builds it with the two \
             profiles these measurements need."
        );
    }

    let run = embed::measure()?;
    print_run(&run);
    if let Some(path) = json {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, run.to_json())
            .with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// One build's own numbers: §9.2's gated row, for whichever profile this is.
fn print_run(run: &Run) {
    println!(
        "profile dir {:<9} source {}   ({} rounds x {} lookups per column)",
        run.profile_dir, run.source_id, run.rounds, run.lookups_per_round
    );
    println!(
        "  out-of-crate (tf_tree_bench) {:>8.1} ns   spread {:>5.2}%",
        run.out_of_crate_ns,
        run.out_of_crate_spread * 100.0
    );
    println!(
        "  in-crate     (tf_tree_core)  {:>8.1} ns   spread {:>5.2}%",
        run.in_crate_ns,
        run.in_crate_spread * 100.0
    );
    println!("  boundary: {}", run.verdict_line());
}

fn report(pair: &Pair) {
    println!("docs/PHASE5.md §9.2 — GATED: the crate boundary, one build per profile\n");
    print_run(&pair.embedder);
    println!("    ^ this is the gated row: §9.2 requires an embedder's default profile.\n");
    print_run(&pair.reference);
    println!(
        "    ^ the control, not the row. Under `lto = \"thin\"` the boundary is erased at\n      \
         link time, so a ratio near 1.00 here is the mechanism working, not a passing gate.\n"
    );

    // The two profiles' `lto` / `codegen-units` are deliberately **not** spelled
    // out here. They are stated in exactly one place — the report row's note in
    // `tf_tree_bench::report` — and a test reads them back out of the workspace
    // manifest and checks that note against them. A second copy printed here
    // would be a second thing to keep true, and the profile *directory* on each
    // line below is already provenance `build.rs` derived rather than a label.
    println!("EXPLORATORY — what the embedder's own [profile.*] costs, not gated\n");
    println!(
        "  out-of-crate at [profile.embedder] {:>8.1} ns",
        pair.embedder.out_of_crate_ns
    );
    println!(
        "  out-of-crate at [profile.release]  {:>8.1} ns",
        pair.reference.out_of_crate_ns
    );
    println!(
        "  ratio {:.3}x — `docs/API.md` §2.3 item 2's LTO guidance, priced.",
        pair.profile_ratio()
    );
    println!(
        "\n  This second comparison is two processes seconds apart and carries the host's\n  \
         full between-run noise, so it informs and never gates: `docs/PHASE1.md` §11.2's\n  \
         exploratory shape. Only the first block enters `results.json`."
    );
}
