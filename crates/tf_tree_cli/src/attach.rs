//! Attaching the CLI to a **live** arena — `docs/decisions/0005` step 11.
//!
//! # Why this is the milestone's real acceptance test
//!
//! Everything else in `0005` is exercised by tests that arrange their own
//! processes. This is the first consumer that behaves like a user: it resolves
//! the rendezvous from the environment, joins whatever is running, and prints
//! what it finds. If `open()` needs a flag nobody would guess, or fails in a way
//! nobody can act on, that shows up here first.
//!
//! # Read-only by default, and it is not a nicety
//!
//! `--rw` is opt-in and `--create` defaults to `never`. A diagnostic tool that
//! attaches read-write to a robot's tree can corrupt it with any bug it happens
//! to have; the MMU is what stops that, and only if the mapping is `PROT_READ`
//! (D18). Defaulting to *create* would be worse still — a `doctor` run against a
//! typo'd domain would silently bring an empty arena into existence and then
//! report it as healthy.

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
    /// Map read-write. **Off by default** — a diagnostic tool has no business
    /// being able to write to a robot's tree (D18).
    #[arg(long, global = true)]
    pub rw: bool,
    /// Create the arena if it is absent. Off by default: a `doctor` run against
    /// a mistyped domain must say "nothing there", not conjure an empty arena
    /// and pronounce it healthy.
    #[arg(long, global = true)]
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
    /// Any rendezvous or attach failure, with the resolved domain and name in
    /// the context — "no arena" is a question about *which* arena, and the
    /// answer is almost always that the domain or name differs from the
    /// publisher's.
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
    /// **without** the arena (§3.3).
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
