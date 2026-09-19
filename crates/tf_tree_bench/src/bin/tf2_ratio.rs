//! The depth-3 tf2 ratio at whichever profile this binary was built into.
//!
//! [`tf_tree_bench::ratio`] is built by `just tf2-bench-report` under the
//! workspace's thin-LTO release, which is not what a consumer gets;
//! `[profile.embedder]` is cargo's defaults (no LTO). This binary is the one
//! row, built twice (`just tf2-ratio-profiles`).
//!
//! The two engines are paired within a process; the two profiles are not. The
//! tf2 column goes through an `extern "C"` shim no Rust LTO can inline, so it is
//! the control: if it moves materially the two runs are not comparable.

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

    // This binary is the report; the `print_stdout` allow is local.
    #[allow(clippy::print_stdout)]
    {
        println!(
            "profile: target/{}/  (the workspace manifest declares lto = {lto} for it)",
            embed::PROFILE_DIR
        );
        println!("{}", run.verdict_line());
        // Columns, not the quotient: the tf2 column is the control.
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
