//! `tft` — the short alias for the `tf_tree` binary; both share one entry point.

fn main() -> anyhow::Result<()> {
    tf_tree_cli::run()
}
