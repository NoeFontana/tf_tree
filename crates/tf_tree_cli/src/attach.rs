//! Attaching the CLI to a live arena (`docs/decisions/0005` step 11): read-only
//! by default (D18), and never creating unless asked.

use anyhow::{Context, Result};
use clap::Args;

use tf_tree::{AttachMode, CreatePolicy, Tree};

/// Flags shared by every subcommand that can operate on a live arena.
#[derive(Args, Clone, Debug)]
pub struct AttachArgs {
    /// Attach to a running arena instead of building the in-process fixture.
    #[arg(long, global = true)]
    pub attach: bool,
    /// Rendezvous domain. Defaults to `$TF_TREE_DOMAIN`, then `0`.
    #[arg(long, global = true)]
    pub domain: Option<u32>,
    /// Arena name. Defaults to `$TF_TREE_NAME`, then `default`.
    #[arg(long, global = true)]
    pub name: Option<String>,
    /// Map read-write. Off by default (D18).
    #[arg(long, global = true)]
    pub rw: bool,
    /// Create the arena if it is absent. Off by default; requires `--rw`
    /// (`docs/decisions/0019` plan step 1).
    #[arg(long, global = true, requires = "rw")]
    pub create: bool,
    /// Seconds to wait for a contended rendezvous to settle.
    #[arg(long, global = true, default_value_t = 5)]
    pub timeout: u64,
}

impl AttachArgs {
    /// Join the live arena these flags name.
    ///
    /// # Errors
    ///
    /// Any rendezvous or attach failure, naming the resolved domain and name.
    pub fn open(&self) -> Result<Tree> {
        let mut open = tf_tree::Open::new()
            .mode(if self.rw {
                AttachMode::ReadWrite
            } else {
                AttachMode::ReadOnly
            })
            .create(if self.create {
                CreatePolicy::IfAbsent
            } else {
                CreatePolicy::Never
            })
            .timeout(core::time::Duration::from_secs(self.timeout));
        if let Some(d) = self.domain {
            open = open.domain(d);
        }
        if let Some(n) = &self.name {
            open = open
                .name(n)
                .with_context(|| format!("{n:?} is not a legal arena name"))?;
        }
        open.open().with_context(|| {
            format!(
                "no arena at domain {} name {}",
                self.domain
                    .map_or_else(|| "$TF_TREE_DOMAIN".to_owned(), |d| d.to_string()),
                self.name
                    .clone()
                    .unwrap_or_else(|| "$TF_TREE_NAME".to_owned()),
            )
        })
    }

    /// The rendezvous these flags name, for commands that read the lock file
    /// without the arena (§3.3).
    ///
    /// # Errors
    ///
    /// If the runtime directory cannot be resolved, or the name is not a legal
    /// arena name.
    pub fn rendezvous(&self) -> Result<tf_tree_ipc::Rendezvous> {
        let rv = tf_tree_ipc::Rendezvous::from_env()
            .context("resolving the runtime directory (see $TF_TREE_RUNTIME_DIR)")?;
        let dir = rv.runtime_dir().clone();
        let domain = self.domain.unwrap_or_else(|| rv.domain());
        let name = match &self.name {
            None => rv.name(),
            Some(n) => tf_tree_ipc::ArenaName::new(n, tf_tree_ipc::EnvVar::Name)
                .with_context(|| format!("{n:?} is not a legal arena name"))?,
        };
        Ok(tf_tree_ipc::Rendezvous::new(dir, domain, name))
    }
}
