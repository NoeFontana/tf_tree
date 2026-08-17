//! The depth-3 tf2 ratio, at **whichever profile this binary was built into**.
//!
//! # Why this exists separately from `bench_report`
//!
//! [`tf_tree_bench::ratio`] is the row `just tf2-bench-check` gates, and
//! `just tf2-bench-report` builds it with `cargo run --release` — i.e. under
//! *this workspace's* `[profile.release]`, which is `lto = "thin"`. That is not
//! the build a consumer gets. `cargo` applies the **top-level** package's
//! profile to its whole dependency graph, so somebody who runs `cargo add
//! tf_tree` and builds `--release` gets cargo's release defaults, and cargo's
//! release defaults do **not** enable LTO. `[profile.embedder]` in the workspace
//! manifest is exactly those defaults written out field by field, for precisely
//! this reason.
//!
//! Under thin LTO the compiler inlines `Plan::at` across the `tf_tree` crate
//! boundary into the harness; under `lto = false` it does not, and the same arm
//! has measured 240-245 ns rather than ~201 ns all week (`just guard-cost`,
//! `just abi-attached`, `embed.rs`'s exploratory profile row). So the numerator
//! of the gated ratio depends on a profile nobody had written down next to it.
//!
//! Running `bench_report` twice would answer the question too, but it takes the
//! whole §9.2 suite with it and its baseline check is not cross-profile
//! comparable (see `runstore`'s [`BUILD_CRITICAL_FACTS`]). This binary is the
//! one row, built twice — `just tf2-ratio-profiles`.
//!
//! [`BUILD_CRITICAL_FACTS`]: tf_tree_bench::runstore::BUILD_CRITICAL_FACTS
//!
//! # What is and is not paired here
//!
//! **Paired:** the two engines within one process, alternating which goes first
//! each round, exactly as `ratio.rs` already does. That is what makes a quotient
//! resolvable on a host whose absolute latencies are not.
//!
//! **Not paired:** the two *profiles*. They are two builds, therefore two
//! processes, and their tf_tree columns carry this host's run-to-run spread.
//! What makes the cross-profile comparison readable anyway is the tf2 column:
//! that arm goes through `tf_tree_tf2_sys`, a C++ shim behind an `extern "C"`
//! call that no Rust LTO setting can inline into, so it is *expected* to be
//! roughly invariant across the two builds. It is printed rather than assumed —
//! if it moves materially, the two runs are not comparable and the operator can
//! see that from the output rather than from a footnote.

use anyhow::{Context, Result};

use tf_tree_bench::{embed, ratio};

fn main() -> Result<()> {
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("Cargo.toml"),
    )
    .context("reading the workspace manifest to find out what profile this binary is")?;
    let lto = embed::lto_for_profile_dir(&manifest, embed::PROFILE_DIR);

    let run = ratio::measure()?;

    // `print!` rather than the crate's usual `eprintln!` discipline: this binary
    // *is* the report. `print_stdout` is `warn` workspace-wide, so the allow is
    // local and deliberate, as in the other reporting bins.
    #[allow(clippy::print_stdout)]
    {
        println!(
            "profile: target/{}/  (the workspace manifest declares lto = {lto} for it)",
            embed::PROFILE_DIR
        );
        println!("{}", run.verdict_line());
        // The two columns again, unrounded and labelled, because the
        // cross-profile reading is a comparison of *columns* and not of the
        // quotient: the tf2 column is the control.
        println!(
            "  tf_tree {:.2} ns/lookup   tf2 {:.2} ns/lookup   ratio {:.4}x  \
             (band {:.4}-{:.4}, {} rounds x {} lookups/arm)",
            run.tf_tree_ns,
            run.tf2_ns,
            run.ratio,
            run.ratio_lo,
            run.ratio_hi,
            run.rounds,
            run.lookups_per_round,
        );
    }

    Ok(())
}
