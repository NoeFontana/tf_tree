//! `docs/PHASE5.md` §9.2's embedding row: the facade from a separate crate.
//!
//! Two modes, because the row is two builds of this program:
//!
//! ```text
//! embed_cost --json target/embed-cost/embedder.json   # measure this build
//! embed_cost --compare target/embed-cost              # print the row
//! ```
//!
//! `just embed-cost` runs all three steps; run that rather than this. The
//! measurement is only worth as much as the pinning and the profile flags the
//! recipe supplies, and both are easy to leave off by hand.
//!
//! The interesting design is in [`tf_tree_bench::embed`] — in particular why the
//! reference column is a second *build* rather than a probe compiled inside the
//! engine, and why there is exactly one loop shape.
// This binary's output *is* its result.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use tf_tree_bench::embed::{self, Pair};

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
    // row is a statement about generated code. Refusing is cheaper than
    // explaining the number later.
    if cfg!(debug_assertions) {
        bail!(
            "this is a debug build: debug_assertions are on, so the timing describes a \
             program nobody ships. Run `just embed-cost`, which builds it twice with the \
             two profiles the row compares."
        );
    }

    let run = embed::measure()?;
    println!(
        "profile dir {:<10} {:>8.1} ns/lookup   spread {:.2}%   ({} rounds x {} lookups)",
        run.profile_dir,
        run.ns_per_lookup,
        run.spread * 100.0,
        run.rounds,
        run.lookups_per_round
    );
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

fn report(pair: &Pair) {
    println!("docs/PHASE5.md §9.2 — facade Plan::at from a separate crate, depth 3\n");
    println!("{:>34} {:>12} {:>10}", "profile", "ns/lookup", "spread");
    println!(
        "{:>34} {:>12.1} {:>9.2}%",
        "embedder (lto=false, cgu=16)",
        pair.embedder.ns_per_lookup,
        pair.embedder.spread * 100.0
    );
    println!(
        "{:>34} {:>12.1} {:>9.2}%",
        "reference (lto=thin, cgu=1)",
        pair.reference.ns_per_lookup,
        pair.reference.spread * 100.0
    );
    println!("\n  {}", pair.verdict());
    println!(
        "\n  The two spreads bound what this ratio is worth: a 5% criterion over halves \
         that each move by more\n  than a percent or two between rounds is arithmetic, \
         not a measurement."
    );
}
