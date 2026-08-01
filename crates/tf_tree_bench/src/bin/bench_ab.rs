//! `bench_ab a.json b.json` — did that change help?
//!
//! Reads two run files written by any harness in this suite
//! ([`tf_tree_bench::runstore`]) and prints a verdict per metric: `better`,
//! `worse`, `noise`, `info` or `unmeasured`. Exits non-zero if anything
//! regressed past the tolerance the **baseline** recorded, so it drops into a
//! bisect script without further wrapping.
//!
//! # What it will not do
//!
//! Guess. The direction a metric is allowed to move and the slack below which a
//! move is not news both travel in the file, next to the number; nothing here
//! infers either from a key name. That is the whole reason
//! `docs/PHASE5.md` §10's `results.json` went to schema `/2`, and the same
//! reasoning applies one level down.
//!
//! It also will not quietly drop a row. A workload that stopped running appears
//! under "Rows only in a" rather than as an absence, because an absence reads
//! as "unchanged".
//!
//! Usage:
//!
//! ```text
//! just bench-ab target/bench-runs/before.json target/bench-runs/after.json
//! ```
// This binary's output IS its result.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;
use std::process::ExitCode;

use tf_tree_bench::runstore::{self, Run};

fn usage() -> String {
    "usage: bench_ab <baseline.json> <new.json>\n\
     \n\
     Both files are written by a harness's `--json <path>` flag:\n\
     \n\
       just bench-run robot            # writes target/bench-runs/<sha>.json\n\
       ... change the core ...\n\
       just bench-run robot\n\
       just bench-ab <first> <second>\n"
        .to_owned()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 || args.iter().any(|a| a == "-h" || a == "--help") {
        eprint!("{}", usage());
        return ExitCode::from(2);
    }

    let (pa, pb) = (PathBuf::from(&args[0]), PathBuf::from(&args[1]));
    let (a, b) = match (Run::load(&pa), Run::load(&pb)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("bench_ab: {e:#}");
            return ExitCode::from(2);
        }
    };

    println!(
        "a: {}  ({} rows, {})",
        pa.display(),
        a.rows.len(),
        describe(&a)
    );
    println!(
        "b: {}  ({} rows, {})",
        pb.display(),
        b.rows.len(),
        describe(&b)
    );
    println!();

    let d = runstore::diff(&a, &b);
    print!("{}", runstore::render(&d));

    if d.deltas.is_empty() {
        // Not a pass. Two files with no metric in common is the shape a typo in
        // a harness name produces, and reporting it as "no regressions" is the
        // failure mode this whole tool exists to avoid.
        eprintln!(
            "\nbench_ab: the two runs share no comparable metric. Check that both \
             were produced by the same harness and workload."
        );
        return ExitCode::from(2);
    }

    if d.regressed() {
        println!("\nREGRESSION: at least one metric moved the wrong way past its tolerance.");
        ExitCode::FAILURE
    } else {
        println!("\nNo regression.");
        ExitCode::SUCCESS
    }
}

/// The commit and build a run came from — the two facts a reader needs to know
/// *what* is being compared, as opposed to how it did.
fn describe(run: &Run) -> String {
    let commit = run.fact("git_commit").unwrap_or("unknown commit");
    let short: String = commit.chars().take(12).collect();
    let dirty = if run.fact("git_dirty") == Some("true") {
        "-dirty"
    } else {
        ""
    };
    let profile = run.fact("build_profile").unwrap_or("unknown profile");
    format!("{short}{dirty}, {profile}")
}
