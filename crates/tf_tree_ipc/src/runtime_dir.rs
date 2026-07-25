//! Runtime-directory resolution — the sharing boundary.
//!
//! `docs/PHASE2.md` §3.1: **two processes share an arena if and only if they
//! resolve to the same runtime directory, domain and name.** That is the whole
//! mental model, and it is why this module refuses to guess. Every ambiguity is
//! resolved towards *failing loudly* rather than towards silently landing in a
//! different directory, because landing in a different directory is not a
//! degraded mode — it is two robots' worth of nodes each talking to half a
//! transform tree, with nothing reporting an error.
//!
//! The boundary is a directory on purpose. Sharing it between containers is a
//! volume mount (`-v /run/tf_tree:/run/tf_tree`), not sharing it is complete
//! isolation, and either way the answer is inspectable with `ls`. Abstract Unix
//! sockets would have tied it to the network namespace instead — invisible,
//! surprising, and usually the wrong boundary.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::{IpcError, RuntimeDirSource};

/// `NFS_SUPER_MAGIC`.
const NFS_SUPER_MAGIC: u64 = 0x6969;
/// `CIFS_MAGIC_NUMBER` (`"\xffSMB"`).
const CIFS_MAGIC_NUMBER: u64 = 0xFF53_4D42;
/// `SMB_SUPER_MAGIC`, the pre-CIFS name for the same family. Rejected for the
/// same reason.
const SMB_SUPER_MAGIC: u64 = 0x517B;
/// `SMB2_MAGIC_NUMBER`, used by the `smb3`/`cifs` module for SMB2+ mounts.
const SMB2_MAGIC_NUMBER: u64 = 0xFE53_4D42;

/// Environment lookup, injected so the resolution *rules* are unit-testable.
///
/// Reading the real environment is not testable in a way that survives parallel
/// tests: `std::env::set_var` mutates process-global state that every other
/// running test observes (and is `unsafe` since Rust 2024). The rules are the
/// interesting part, so they take an `EnvLookup` and the process environment is
/// one trivial implementation of it.
pub trait EnvLookup {
    /// The value of `key`, or `None` if unset.
    ///
    /// Returns `OsString` because `$TF_TREE_RUNTIME_DIR` is a *path* and paths
    /// need not be UTF-8. Domain and name are required to be UTF-8 separately,
    /// where that requirement is meaningful.
    fn var(&self, key: &str) -> Option<OsString>;
}

/// The process environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemEnv;

impl EnvLookup for SystemEnv {
    fn var(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }
}

/// A resolved runtime directory, and which rule produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeDir {
    path: PathBuf,
    source: RuntimeDirSource,
}

impl RuntimeDir {
    /// The directory itself.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which §3.1 rule produced it. Worth printing at startup: "which arena am I
    /// on" is the first question in every integration bug report.
    #[must_use]
    pub fn source(&self) -> RuntimeDirSource {
        self.source
    }

    /// Resolve from the process environment.
    ///
    /// # Errors
    ///
    /// See [`RuntimeDir::resolve_with`].
    pub fn resolve() -> Result<RuntimeDir, IpcError> {
        RuntimeDir::resolve_with(&SystemEnv, current_uid())
    }

    /// Resolve against an arbitrary environment and uid.
    ///
    /// The §3.1 order, first hit wins:
    ///
    /// 1. `$TF_TREE_RUNTIME_DIR`
    /// 2. `$XDG_RUNTIME_DIR/tf_tree`
    /// 3. `/run/tf_tree` if writable
    /// 4. `/tmp/tf_tree-<uid>`, created `0700`
    ///
    /// **A set variable is a hit even if it does not work.** If
    /// `$XDG_RUNTIME_DIR` is set but its `tf_tree` subdirectory cannot be
    /// created, this fails rather than falling through to `/run` — falling
    /// through would put this process on a *different sharing boundary* from
    /// every process for which the variable did work. `/run/tf_tree` is the one
    /// candidate that may be skipped, because it is a probe for a system
    /// deployment rather than an instruction from the operator.
    ///
    /// # Errors
    ///
    /// [`IpcError::RuntimeDirUnusable`] if the directory cannot be created,
    /// [`IpcError::RuntimeDirForeignOwner`] if the `/tmp` fallback belongs to
    /// another user, and [`IpcError::NetworkFilesystem`] /
    /// [`IpcError::StatFsFailed`] from the NORMATIVE §3.1 check.
    pub fn resolve_with(env: &dyn EnvLookup, uid: u32) -> Result<RuntimeDir, IpcError> {
        if let Some(dir) = non_empty(env.var("TF_TREE_RUNTIME_DIR")) {
            return finish(PathBuf::from(dir), RuntimeDirSource::Env, uid);
        }
        if let Some(xdg) = non_empty(env.var("XDG_RUNTIME_DIR")) {
            let path = PathBuf::from(xdg).join("tf_tree");
            return finish(path, RuntimeDirSource::XdgRuntimeDir, uid);
        }
        // Only here is a failure allowed to fall through: a non-root process
        // cannot create `/run/tf_tree`, and that is the normal case rather than
        // a misconfiguration.
        let run = PathBuf::from("/run/tf_tree");
        if ensure_dir(&run).is_ok() && is_writable(&run) {
            return finish(run, RuntimeDirSource::Run, uid);
        }
        finish(
            PathBuf::from(format!("/tmp/tf_tree-{uid}")),
            RuntimeDirSource::Tmp,
            uid,
        )
    }
}

/// Create the directory if needed, check ownership where it matters, then run
/// the NORMATIVE filesystem check.
fn finish(path: PathBuf, source: RuntimeDirSource, uid: u32) -> Result<RuntimeDir, IpcError> {
    ensure_dir(&path).map_err(|e| IpcError::RuntimeDirUnusable {
        source,
        raw_os_error: IpcError::os(&e),
    })?;

    let meta = std::fs::symlink_metadata(&path).map_err(|e| IpcError::RuntimeDirUnusable {
        source,
        raw_os_error: IpcError::os(&e),
    })?;
    if !meta.is_dir() {
        return Err(IpcError::RuntimeDirNotADirectory { source });
    }
    // `/tmp` is world-writable, so `/tmp/tf_tree-<uid>` can be pre-created by
    // anyone. Every other candidate is either named by the operator or inside a
    // per-user or root-owned directory. §3.10 scopes trust to same-user
    // processes; this is the only place that boundary is checkable at all.
    if source == RuntimeDirSource::Tmp {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != uid {
            return Err(IpcError::RuntimeDirForeignOwner {
                owner_uid: meta.uid(),
                our_uid: uid,
            });
        }
    }

    reject_network_filesystem(&path, source)?;
    Ok(RuntimeDir { path, source })
}

/// NORMATIVE (`docs/PHASE2.md` §3.1): refuse NFS and CIFS.
///
/// Not caution for its own sake. On NFS, locks are leases: they are recovered
/// after a server restart, they can be *lost* while the client believes it holds
/// them, and release on client death happens when the lease expires rather than
/// immediately. Every claim in §3.3's table — "holder dies without unlocking →
/// released by the kernel, immediately" — stops being true, and the split-brain
/// check in §3.4 silently degrades into a timing heuristic. Refusing to start is
/// vastly better than that.
fn reject_network_filesystem(path: &Path, source: RuntimeDirSource) -> Result<(), IpcError> {
    let st = rustix::fs::statfs(path).map_err(|e| IpcError::StatFsFailed {
        source,
        raw_os_error: e.raw_os_error(),
    })?;
    // `f_type` is signed on some architectures; the magics are compared as
    // unsigned, and 0xFF534D42 is where the difference would bite.
    // `f_type` is `__fsword_t`: i64 on 64-bit, **i32 on 32-bit** (armv7, i686).
    // `CIFS_MAGIC_NUMBER` (0xFF53_4D42) has its top bit set, so a plain
    // `as u64` sign-extends it to 0xFFFF_FFFF_FF53_4D42 there and the comparison
    // silently never matches — the NORMATIVE refusal would pass a CIFS mount
    // through on exactly the targets where it is least likely to be noticed.
    // Masking to 32 bits is correct for both widths: every magic here fits.
    #[allow(clippy::unnecessary_cast)]
    let magic = (st.f_type as i64 as u64) & 0xFFFF_FFFF;
    if matches!(
        magic,
        NFS_SUPER_MAGIC | CIFS_MAGIC_NUMBER | SMB_SUPER_MAGIC | SMB2_MAGIC_NUMBER
    ) {
        return Err(IpcError::NetworkFilesystem { source, magic });
    }
    Ok(())
}

/// Create `path` (and any missing parents) with mode `0700`.
///
/// The mode is set explicitly after creation rather than left to `mkdir`,
/// because `mkdir`'s mode argument is masked by the process umask and a service
/// started with `umask 000` would otherwise publish a world-writable rendezvous
/// directory.
fn ensure_dir(path: &Path) -> std::io::Result<()> {
    // `symlink_metadata`, not `is_dir()`, and a symlink is refused outright.
    //
    // `/tmp/tf_tree-<uid>` is the one candidate whose parent is world-writable,
    // so another user can pre-create the path. If they make it a *symlink* to a
    // directory this uid happens to own, a following stat sees a directory owned
    // by us, the ownership check passes, the 0700 chmod is skipped, and the
    // whole rendezvous — lock file, socket, identity records — lands somewhere
    // the operator never chose. Since this directory *is* the sharing boundary,
    // that is the one thing here worth being strict about.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "runtime directory is a symlink; refusing to follow it",
            ))
        }
        Ok(meta) if meta.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "runtime directory path exists and is not a directory",
            ))
        }
        Err(_) => {}
    }
    std::fs::create_dir_all(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

fn is_writable(path: &Path) -> bool {
    rustix::fs::access(
        path,
        rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK,
    )
    .is_ok()
}

/// An environment variable set to the empty string counts as unset.
///
/// `TF_TREE_NAME= prog` is how a shell says "no value"; treating it as a
/// zero-length name would fail later with a much worse message.
fn non_empty(v: Option<OsString>) -> Option<OsString> {
    v.filter(|s| !s.is_empty())
}

/// This process's real uid.
#[must_use]
pub fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::collections::HashMap;

    /// An environment built from a literal list, so a test states exactly what
    /// is set and nothing leaks in from the real one.
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

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tf_tree_ipc_test-{}-{}-{tag}",
            std::process::id(),
            current_uid()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn explicit_override_wins_over_everything() {
        let dir = scratch("override");
        let env = FakeEnv::new(&[
            ("TF_TREE_RUNTIME_DIR", dir.to_str().unwrap()),
            ("XDG_RUNTIME_DIR", "/nonexistent-xdg"),
        ]);
        let rd = RuntimeDir::resolve_with(&env, current_uid()).unwrap();
        assert_eq!(rd.path(), dir);
        assert_eq!(rd.source(), RuntimeDirSource::Env);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn xdg_gets_a_tf_tree_subdirectory() {
        let dir = scratch("xdg");
        let env = FakeEnv::new(&[("XDG_RUNTIME_DIR", dir.to_str().unwrap())]);
        let rd = RuntimeDir::resolve_with(&env, current_uid()).unwrap();
        assert_eq!(rd.path(), dir.join("tf_tree"));
        assert_eq!(rd.source(), RuntimeDirSource::XdgRuntimeDir);
        assert!(rd.path().is_dir());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        // `TF_TREE_RUNTIME_DIR= prog` must not resolve to the empty path; it
        // must behave as if the variable were absent.
        let dir = scratch("empty-var");
        let env = FakeEnv::new(&[
            ("TF_TREE_RUNTIME_DIR", ""),
            ("XDG_RUNTIME_DIR", dir.to_str().unwrap()),
        ]);
        let rd = RuntimeDir::resolve_with(&env, current_uid()).unwrap();
        assert_eq!(rd.source(), RuntimeDirSource::XdgRuntimeDir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_xdg_is_an_error_not_a_fallback() {
        // Falling through would put this process on a different sharing
        // boundary from every process where $XDG_RUNTIME_DIR did work.
        let env = FakeEnv::new(&[("XDG_RUNTIME_DIR", "/proc/self/mem-is-not-a-directory")]);
        let err = RuntimeDir::resolve_with(&env, current_uid()).unwrap_err();
        assert!(
            matches!(
                err,
                IpcError::RuntimeDirUnusable {
                    source: RuntimeDirSource::XdgRuntimeDir,
                    ..
                } | IpcError::RuntimeDirNotADirectory {
                    source: RuntimeDirSource::XdgRuntimeDir
                }
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn the_last_resort_is_tmp_per_uid() {
        // No variables at all, and `/run/tf_tree` is not creatable by a normal
        // user, so this lands in /tmp — unless the test runs as root, where
        // /run/tf_tree is the correct answer.
        let env = FakeEnv::new(&[]);
        let uid = current_uid();
        let rd = RuntimeDir::resolve_with(&env, uid).unwrap();
        if uid == 0 {
            assert_eq!(rd.source(), RuntimeDirSource::Run);
            assert_eq!(rd.path(), Path::new("/run/tf_tree"));
        } else {
            assert_eq!(rd.source(), RuntimeDirSource::Tmp);
            assert_eq!(rd.path(), PathBuf::from(format!("/tmp/tf_tree-{uid}")));
            assert!(rd.path().is_dir());
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::symlink_metadata(rd.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "the /tmp fallback must be 0700");
        }
    }

    #[test]
    fn a_tmp_directory_owned_by_someone_else_is_refused() {
        // A second user cannot be conjured in a unit test, so invert it: ask the
        // resolver for uid+1's directory. It creates `/tmp/tf_tree-<uid+1>`
        // owned by the *real* uid, which is precisely the "this directory
        // belongs to somebody else" shape the check exists to catch.
        let uid = current_uid();
        let env = FakeEnv::new(&[]);
        if uid == 0 {
            // As root the resolver stops at /run/tf_tree and never reaches the
            // /tmp fallback, so there is nothing to check.
            return;
        }
        let err = RuntimeDir::resolve_with(&env, uid + 1).unwrap_err();
        assert!(
            matches!(
                err,
                IpcError::RuntimeDirForeignOwner { our_uid, owner_uid }
                    if our_uid == uid + 1 && owner_uid == uid
            ),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir(PathBuf::from(format!("/tmp/tf_tree-{}", uid + 1)));
    }

    #[test]
    fn a_local_filesystem_passes_the_normative_check() {
        let dir = scratch("statfs");
        reject_network_filesystem(&dir, RuntimeDirSource::Env).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_network_magics_are_the_ones_the_spec_names() {
        // The values are load-bearing and impossible to sanity-check by reading
        // the call site, so pin them here against `docs/PHASE2.md` §3.1.
        assert_eq!(NFS_SUPER_MAGIC, 0x6969);
        assert_eq!(CIFS_MAGIC_NUMBER, 0xFF53_4D42);
    }
}
