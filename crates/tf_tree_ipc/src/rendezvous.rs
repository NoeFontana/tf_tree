//! Identity, defaults, and the two paths every participant must agree on.
//!
//! `docs/PHASE2.md` §3.2:
//!
//! ```text
//! domain: $TF_TREE_DOMAIN, else $ROS_DOMAIN_ID, else 0
//! name:   $TF_TREE_NAME,   else "default"
//! ```
//!
//! Falling back to `$ROS_DOMAIN_ID` is the part worth understanding. A ROS 2
//! system has already configured its isolation; inheriting it means `tf_tree`
//! partitions exactly the way the rest of the stack does with no additional
//! setup, so two robots on one bench — or a simulator alongside hardware — stay
//! separated because they were already separated. Nobody has to know this
//! library has a notion of domain at all.

use std::path::{Path, PathBuf};

use crate::error::{EnvVar, IpcError, NameProblem};
use crate::runtime_dir::{current_uid, EnvLookup, RuntimeDir, SystemEnv};

/// Longest arena name, in bytes.
///
/// It has to fit a filename with room for `.lock`/`.sock` suffixes, and short
/// names are the ones people actually type into a launch file.
pub const MAX_NAME_LEN: usize = 64;

/// The default arena name when `$TF_TREE_NAME` is unset.
pub const DEFAULT_NAME: &str = "default";

/// A validated arena name: one path component, UTF-8, non-empty, at most
/// [`MAX_NAME_LEN`] bytes.
///
/// Stored inline rather than as a `String` so it stays `Copy` and can sit inside
/// error values, matching the workspace rule that errors never allocate.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ArenaName {
    bytes: [u8; MAX_NAME_LEN],
    len: u8,
}

impl ArenaName {
    /// Validate and store `name`.
    ///
    /// # Errors
    ///
    /// [`IpcError::NameInvalid`] if it is empty, too long, or not a single path
    /// component. `var` only decides how the error names itself.
    pub fn new(name: &str, var: EnvVar) -> Result<ArenaName, IpcError> {
        let problem = if name.is_empty() {
            Some(NameProblem::Empty)
        } else if name.len() > MAX_NAME_LEN {
            Some(NameProblem::TooLong)
        } else if name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\0')
            || name.contains('\\')
        {
            // A name is a single path component. `$TF_TREE_NAME=../other` would
            // otherwise let two processes that "agree" on the name resolve to
            // different directories, which is the exact failure §3.1 is built to
            // prevent.
            Some(NameProblem::NotOneComponent)
        } else {
            None
        };
        if let Some(problem) = problem {
            return Err(IpcError::NameInvalid { var, problem });
        }
        let mut bytes = [0u8; MAX_NAME_LEN];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        // `len` fits a u8 because MAX_NAME_LEN is 64.
        Ok(ArenaName {
            bytes,
            len: name.len() as u8,
        })
    }

    /// The name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The bytes came from a `&str` in `new`, and nothing else writes them.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or(DEFAULT_NAME)
    }
}

impl core::fmt::Debug for ArenaName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl core::fmt::Display for ArenaName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a group of processes meets: a runtime directory, a domain, and a name.
///
/// Holding these three together as one value is deliberate — they are jointly
/// the sharing boundary (§3.1), and code that passes them around separately
/// eventually passes two of the three.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendezvous {
    runtime_dir: RuntimeDir,
    domain: u32,
    name: ArenaName,
    dir: PathBuf,
    lock_path: PathBuf,
    sock_path: PathBuf,
}

impl Rendezvous {
    /// Resolve everything from the process environment.
    ///
    /// # Errors
    ///
    /// Anything [`RuntimeDir::resolve`], [`domain_from_env`] or
    /// [`name_from_env`] can produce.
    pub fn from_env() -> Result<Rendezvous, IpcError> {
        Rendezvous::from_env_with(&SystemEnv, current_uid())
    }

    /// Resolve from an injected environment, for tests.
    ///
    /// # Errors
    ///
    /// As [`Rendezvous::from_env`].
    pub fn from_env_with(env: &dyn EnvLookup, uid: u32) -> Result<Rendezvous, IpcError> {
        let dir = RuntimeDir::resolve_with(env, uid)?;
        let domain = domain_from_env(env)?;
        let name = name_from_env(env)?;
        Ok(Rendezvous::new(dir, domain, name))
    }

    /// Assemble the paths for an already-resolved directory, domain and name.
    #[must_use]
    pub fn new(runtime_dir: RuntimeDir, domain: u32, name: ArenaName) -> Rendezvous {
        let dir = runtime_dir.path().join(domain.to_string());
        let lock_path = dir.join(format!("{name}.lock"));
        let sock_path = dir.join(format!("{name}.sock"));
        Rendezvous {
            runtime_dir,
            domain,
            name,
            dir,
            lock_path,
            sock_path,
        }
    }

    /// `<runtime_dir>/<domain>` — created by [`Rendezvous::ensure_dir`].
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `<runtime_dir>/<domain>/<name>.lock`.
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// `<runtime_dir>/<domain>/<name>.sock`.
    ///
    /// [`crate::OwnerServer::bind_at`] binds it and [`crate::attach`] connects
    /// to it, but the path is part of the rendezvous identity, so it is derived
    /// here rather than in the code that binds it.
    #[must_use]
    pub fn sock_path(&self) -> &Path {
        &self.sock_path
    }

    /// The resolved runtime directory.
    #[must_use]
    pub fn runtime_dir(&self) -> &RuntimeDir {
        &self.runtime_dir
    }

    /// The domain.
    #[must_use]
    pub fn domain(&self) -> u32 {
        self.domain
    }

    /// The arena name.
    #[must_use]
    pub fn name(&self) -> ArenaName {
        self.name
    }

    /// Create `<runtime_dir>/<domain>` if it does not exist, mode `0700`.
    ///
    /// # Errors
    ///
    /// [`IpcError::RuntimeDirUnusable`] if it cannot be created.
    pub fn ensure_dir(&self) -> Result<(), IpcError> {
        if self.dir.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| IpcError::RuntimeDirUnusable {
            source: self.runtime_dir.source(),
            raw_os_error: IpcError::os(&e),
        })?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            IpcError::RuntimeDirUnusable {
                source: self.runtime_dir.source(),
                raw_os_error: IpcError::os(&e),
            }
        })
    }
}

/// `$TF_TREE_DOMAIN`, else `$ROS_DOMAIN_ID`, else `0`.
///
/// # Errors
///
/// [`IpcError::DomainNotAnInteger`] if either variable is set to something that
/// is not a `u32`. Deliberately fatal: a typo that fell back to domain 0 would
/// silently place the process on the wrong arena, and a process on the wrong
/// arena reports nothing at all — it just never sees the transforms it expects.
pub fn domain_from_env(env: &dyn EnvLookup) -> Result<u32, IpcError> {
    for var in [EnvVar::Domain, EnvVar::RosDomainId] {
        let Some(raw) = env.var(var.as_str()) else {
            continue;
        };
        let Some(s) = raw.to_str() else {
            return Err(IpcError::DomainNotAnInteger { var });
        };
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        return s.parse().map_err(|_| IpcError::DomainNotAnInteger { var });
    }
    Ok(0)
}

/// `$TF_TREE_NAME`, else `"default"`.
///
/// # Errors
///
/// [`IpcError::NameInvalid`] if the variable is set to something unusable as a
/// path component.
pub fn name_from_env(env: &dyn EnvLookup) -> Result<ArenaName, IpcError> {
    match env.var(EnvVar::Name.as_str()) {
        Some(raw) if !raw.is_empty() => {
            let s = raw.to_str().ok_or(IpcError::NameInvalid {
                var: EnvVar::Name,
                problem: NameProblem::NotUtf8,
            })?;
            ArenaName::new(s, EnvVar::Name)
        }
        _ => ArenaName::new(DEFAULT_NAME, EnvVar::Name),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    struct FakeEnv(HashMap<String, OsString>);

    impl FakeEnv {
        fn new(pairs: &[(&str, &str)]) -> FakeEnv {
            FakeEnv(
                pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), OsString::from(*v)))
                    .collect(),
            )
        }
    }

    impl EnvLookup for FakeEnv {
        fn var(&self, key: &str) -> Option<OsString> {
            self.0.get(key).cloned()
        }
    }

    #[test]
    fn domain_defaults_to_zero() {
        assert_eq!(domain_from_env(&FakeEnv::new(&[])).unwrap(), 0);
    }

    #[test]
    fn ros_domain_id_is_inherited() {
        let env = FakeEnv::new(&[("ROS_DOMAIN_ID", "7")]);
        assert_eq!(domain_from_env(&env).unwrap(), 7);
    }

    #[test]
    fn tf_tree_domain_wins_over_ros_domain_id() {
        let env = FakeEnv::new(&[("ROS_DOMAIN_ID", "7"), ("TF_TREE_DOMAIN", "9")]);
        assert_eq!(domain_from_env(&env).unwrap(), 9);
    }

    #[test]
    fn an_unparsable_domain_is_fatal_not_zero() {
        // Silently using domain 0 here would be the worst possible behaviour:
        // the process starts, joins the wrong arena, and reports nothing.
        for (var, key) in [
            (EnvVar::Domain, "TF_TREE_DOMAIN"),
            (EnvVar::RosDomainId, "ROS_DOMAIN_ID"),
        ] {
            for bad in ["seven", "-1", "3.5", "0x7"] {
                let env = FakeEnv::new(&[(key, bad)]);
                assert_eq!(
                    domain_from_env(&env).unwrap_err(),
                    IpcError::DomainNotAnInteger { var },
                    "{key}={bad} should have been rejected"
                );
            }
        }
    }

    #[test]
    fn an_empty_domain_variable_falls_through() {
        let env = FakeEnv::new(&[("TF_TREE_DOMAIN", ""), ("ROS_DOMAIN_ID", "4")]);
        assert_eq!(domain_from_env(&env).unwrap(), 4);
    }

    #[test]
    fn name_defaults_to_default() {
        let n = name_from_env(&FakeEnv::new(&[])).unwrap();
        assert_eq!(n.as_str(), "default");
    }

    #[test]
    fn name_comes_from_the_environment() {
        let env = FakeEnv::new(&[("TF_TREE_NAME", "robot")]);
        assert_eq!(name_from_env(&env).unwrap().as_str(), "robot");
    }

    #[test]
    fn names_must_be_one_path_component() {
        for bad in ["../other", "a/b", ".", "..", "a\\b"] {
            let env = FakeEnv::new(&[("TF_TREE_NAME", bad)]);
            assert_eq!(
                name_from_env(&env).unwrap_err(),
                IpcError::NameInvalid {
                    var: EnvVar::Name,
                    problem: NameProblem::NotOneComponent
                },
                "{bad:?} should have been rejected"
            );
        }
        let long = "x".repeat(MAX_NAME_LEN + 1);
        let env = FakeEnv::new(&[("TF_TREE_NAME", long.as_str())]);
        assert!(matches!(
            name_from_env(&env).unwrap_err(),
            IpcError::NameInvalid {
                problem: NameProblem::TooLong,
                ..
            }
        ));
        assert!(ArenaName::new(&"x".repeat(MAX_NAME_LEN), EnvVar::Name).is_ok());
    }

    #[test]
    fn paths_are_domain_then_name() {
        let dir = std::env::temp_dir().join(format!("tf_tree_ipc_rv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let env = FakeEnv::new(&[
            ("TF_TREE_RUNTIME_DIR", dir.to_str().unwrap()),
            ("TF_TREE_DOMAIN", "7"),
            ("TF_TREE_NAME", "robot"),
        ]);
        let rv = Rendezvous::from_env_with(&env, current_uid()).unwrap();
        assert_eq!(rv.dir(), dir.join("7"));
        assert_eq!(rv.lock_path(), dir.join("7").join("robot.lock"));
        assert_eq!(rv.sock_path(), dir.join("7").join("robot.sock"));
        rv.ensure_dir().unwrap();
        assert!(rv.dir().is_dir());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn different_domains_never_collide() {
        let dir = std::env::temp_dir().join(format!("tf_tree_ipc_iso-{}", std::process::id()));
        let mk = |domain: &str| {
            let env = FakeEnv::new(&[
                ("TF_TREE_RUNTIME_DIR", dir.to_str().unwrap()),
                ("TF_TREE_DOMAIN", domain),
            ]);
            Rendezvous::from_env_with(&env, current_uid()).unwrap()
        };
        let a = mk("0");
        let b = mk("1");
        assert_ne!(a.lock_path(), b.lock_path());
        assert_ne!(a.sock_path(), b.sock_path());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
