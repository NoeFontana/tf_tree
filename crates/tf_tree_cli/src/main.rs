//! The `tf_tree` diagnostics binary. All logic lives in the `tf_tree_cli` lib
//! so the `tft` alias binary can share it.

fn main() -> anyhow::Result<()> {
    tf_tree_cli::run()
}
