//! Runtime-directory resolution — the sharing boundary.
//!
//! `docs/PHASE2.md` §3.1: **two processes share an arena if and only if they
//! resolve to the same runtime directory, domain and name.** Every ambiguity
//! fails loudly rather than landing in a different directory, which would leave
//! nodes talking to half a transform tree with nothing reporting an error. The
//! boundary is a directory so it is inspectable with `ls` and shared between
//! containers by a volume mount.

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

/// Environment lookup, injected so the resolution *rules* are unit-testable
/// without mutating process-global state.
pub trait EnvLookup {
    /// The value of `key`, or `None` if unset.
    ///
    /// Returns `OsString` because `$TF_TREE_RUNTIME_DIR` is a path and need not
    /// be UTF-8.
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

    /// Which §3.1 rule produced it; worth printing at startup.
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
    /// **A set variable is a hit even if it does not work**: falling through
    /// would put this process on a different sharing boundary. Only
    /// `/run/tf_tree` may be skipped, being a probe rather than an instruction.
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
        // A non-root process cannot create `/run/tf_tree`; that is normal.
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
    // `/tmp/tf_tree-<uid>` can be pre-created by anyone; this is the only place
    // §3.10's same-user boundary is checkable.
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

/// NORMATIVE (`docs/PHASE2.md` §3.1): refuse NFS and CIFS, where locks are
/// leases that can be lost or outlive the holder, so §3.3's "released by the
/// kernel at exit" and §3.4's split-brain check would degrade to a timing
/// heuristic.
fn reject_network_filesystem(path: &Path, source: RuntimeDirSource) -> Result<(), IpcError> {
    let st = rustix::fs::statfs(path).map_err(|e| IpcError::StatFsFailed {
        source,
        raw_os_error: e.raw_os_error(),
    })?;
    // `f_type` is i32 on 32-bit targets, where `CIFS_MAGIC_NUMBER`'s top bit
    // would sign-extend and never match; masking to 32 bits is right for both.
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

/// Create `path` (and any missing parents) with mode `0700`, set explicitly
/// because `mkdir`'s mode is masked by the umask.
fn ensure_dir(path: &Path) -> std::io::Result<()> {
    // `symlink_metadata`, and a symlink is refused: another user could pre-create
    // `/tmp/tf_tree-<uid>` as a symlink to a directory we own and redirect the
    // whole rendezvous.
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

    /// An environment built from a literal list.
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
        // Must behave as if the variable were absent.
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
        // Falling through would change the sharing boundary.
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
        // No variables and `/run/tf_tree` not creatable: /tmp (as root, /run).
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
        // Ask for uid+1's directory: it is created owned by the real uid, the
        // "belongs to somebody else" shape.
        let uid = current_uid();
        let env = FakeEnv::new(&[]);
        if uid == 0 {
            // As root the resolver never reaches /tmp.
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

    /// **A symlinked runtime directory is refused, never followed.** Changing
    /// `symlink_metadata` back to `metadata` left every other test green.
    #[test]
    fn a_symlinked_runtime_directory_is_refused_rather_than_followed() {
        let target = scratch("symlink-target");
        let link = std::env::temp_dir().join(format!(
            "tf_tree_ipc_test-{}-{}-symlink-link",
            std::process::id(),
            current_uid()
        ));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let env = FakeEnv::new(&[("TF_TREE_RUNTIME_DIR", link.to_str().unwrap())]);
        let err = RuntimeDir::resolve_with(&env, current_uid()).unwrap_err();
        assert!(
            matches!(
                err,
                IpcError::RuntimeDirUnusable {
                    source: RuntimeDirSource::Env,
                    ..
                }
            ),
            "a symlink must be refused, not followed: {err}"
        );

        // ...and a real directory resolves, so the refusal is the symlink's.
        std::fs::remove_file(&link).unwrap();
        std::fs::create_dir_all(&link).unwrap();
        let rd = RuntimeDir::resolve_with(&env, current_uid()).unwrap();
        assert_eq!(rd.path(), link);
        assert_eq!(rd.source(), RuntimeDirSource::Env);

        std::fs::remove_dir_all(&link).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn a_local_filesystem_passes_the_normative_check() {
        let dir = scratch("statfs");
        reject_network_filesystem(&dir, RuntimeDirSource::Env).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_network_magics_are_the_ones_the_spec_names() {
        // Pinned against `docs/PHASE2.md` §3.1.
        assert_eq!(NFS_SUPER_MAGIC, 0x6969);
        assert_eq!(CIFS_MAGIC_NUMBER, 0xFF53_4D42);
    }
}
