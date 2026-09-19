//! Freezing a tree to a `.tft`, and opening one — `docs/PHASE5.md` §2.
//!
//! §2.1: a frozen arena is read by the same `Plan::at` code as a live one, so
//! [`Tree::open_frozen`] returns an ordinary [`Tree`]; the manifest is never read.

use std::path::Path;

use tf_tree_arena::{Arena, FrozenArena, FrozenError, FrozenHeader};

use crate::cbor::Writer;
use crate::tree::Tree;

/// Why a `.tft` could not be opened or written.
///
/// `Copy` and `String`-free (`docs/PROJECT.md` §5): I/O errors reduce to errno.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FrozenFileError {
    /// The path could not be opened or created.
    #[error("could not open the .tft path (errno {raw_os_error})")]
    Path {
        /// `errno`, or `0` if the platform did not supply one.
        raw_os_error: i32,
    },
    /// The file was opened, but is not a `.tft` this build can read — or could
    /// not be written.
    #[error("{0}")]
    Frozen(FrozenError),
}

impl From<FrozenError> for FrozenFileError {
    fn from(e: FrozenError) -> FrozenFileError {
        FrozenFileError::Frozen(e)
    }
}

fn path_err(e: &std::io::Error) -> FrozenFileError {
    FrozenFileError::Path {
        raw_os_error: e.raw_os_error().unwrap_or(0),
    }
}

/// Dot-prefixed sibling temporary (`rename` is atomic within a filesystem), with pid and counter.
fn temp_sibling(path: &Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("out.tft"));
    let mut name = std::ffi::OsString::from(".");
    name.push(stem);
    name.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    dir.join(name)
}

/// This crate's version, NUL-padded into the header's `tool_version`.
fn tool_version() -> [u8; 32] {
    let mut out = [0u8; 32];
    let src = env!("CARGO_PKG_VERSION").as_bytes();
    let n = src.len().min(32);
    out[..n].copy_from_slice(&src[..n]);
    out
}

impl Tree {
    /// Open a `.tft` and read it through the ordinary [`Tree`] API (§2.1).
    ///
    /// The returned tree is read-only (`PROT_READ`, §2.4).
    ///
    /// # Errors
    ///
    /// [`FrozenFileError::Path`] if the file cannot be opened;
    /// [`FrozenFileError::Frozen`] if it is not a `.tft`, is truncated, or has a
    /// different `FORMAT_VERSION` or `layout_hash` (a hard error, §2.4: re-freeze).
    pub fn open_frozen(path: &Path) -> Result<Tree, FrozenFileError> {
        let file = std::fs::File::open(path).map_err(|e| path_err(&e))?;
        let arena = FrozenArena::open(file.into())?;
        Ok(Tree::from_frozen(arena))
    }

    /// Write this tree's arena to `path` as a `.tft` (§2.3).
    ///
    /// Backs `tf_tree freeze --from-live`. The write goes to a sibling temporary
    /// `rename`d over `path` once the header lands, so `path` is always the
    /// previous or the new `.tft`.
    ///
    /// `source_digest` is BLAKE3 of the source recording, or all-zero.
    ///
    /// # Snapshot consistency
    ///
    /// A live freeze copies bytes while publishers store; see
    /// [`write_frozen`](tf_tree_arena::write_frozen).
    ///
    /// # Errors
    ///
    /// [`FrozenFileError::Path`] if `path` cannot be created;
    /// [`FrozenFileError::Frozen`] for a failing write.
    pub fn freeze_to(
        &self,
        path: &Path,
        source: Option<&str>,
        source_digest: [u8; 32],
        created_unix_ns: i64,
    ) -> Result<FrozenHeader, FrozenFileError> {
        let manifest = self.manifest(source, created_unix_ns);
        let tmp = temp_sibling(path);
        let file = std::fs::File::create(&tmp).map_err(|e| path_err(&e))?;
        let arena: &dyn Arena = self.backing();
        let written = (|| -> Result<FrozenHeader, FrozenFileError> {
            let header = tf_tree_arena::write_frozen(
                std::os::fd::AsFd::as_fd(&file),
                arena,
                &manifest,
                source_digest,
                created_unix_ns,
                tool_version(),
            )?;
            std::fs::rename(&tmp, path).map_err(|e| path_err(&e))?;
            Ok(header)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    }

    /// Build the CBOR manifest for this tree (§2.3).
    ///
    /// `oldest_ns` is the oldest *retained* sample; `samples` is what the file
    /// holds, `pushes_total` what the source produced.
    fn manifest(&self, source: Option<&str>, created_unix_ns: i64) -> Vec<u8> {
        let view = self.view();
        let header = view.header();
        let frames = header
            .frame_count
            .load(std::sync::atomic::Ordering::Acquire);
        let edges = header.edge_count.load(std::sync::atomic::Ordering::Acquire);

        let mut w = Writer::new();
        w.map(7);
        w.text("tf_tree");
        w.text(env!("CARGO_PKG_VERSION"));
        w.text("format_version");
        w.u64(u64::from(crate::arena_format_version()));
        w.text("layout_hash");
        w.u64(u64::from(crate::arena_layout_hash()));
        w.text("created_unix_ns");
        w.i64(created_unix_ns);
        w.text("source");
        match source {
            Some(s) => w.text(s),
            None => w.null(),
        }

        // Ids `1..=frame_count`; index 0 is the root sentinel.
        w.text("frames");
        w.array(frames as usize);
        for i in 1..=frames {
            let name = tf_tree_core::FrameId::new(i)
                .and_then(|id| view.frame_record(id))
                .map(|r| {
                    let n = (r.name_len as usize).min(r.name.len());
                    String::from_utf8_lossy(&r.name[..n]).into_owned()
                })
                .unwrap_or_default();
            w.text(&name);
        }

        // `edge_count` includes its sentinel.
        w.text("edges");
        w.array(edges.saturating_sub(1) as usize);
        for i in 1..edges {
            let id = tf_tree_core::EdgeId(i);
            // One observation: per-key re-reads could interleave with a writer.
            let e = view.edge(id);
            w.map(8);
            w.text("parent");
            w.u64(u64::from(e.map_or(0, |e| e.parent)));
            w.text("child");
            w.u64(u64::from(e.map_or(0, |e| e.child)));
            w.text("kind");
            w.u64(u64::from(e.map_or(0, |e| e.kind)));
            w.text("capacity");
            w.u64(u64::from(e.map_or(0, |e| e.capacity)));
            let ring = view.ring(id);
            w.text("samples");
            w.u64(ring.as_ref().map_or(0, |r| r.stored()));
            w.text("pushes_total");
            w.u64(e.map_or(0, |e| e.head.load(std::sync::atomic::Ordering::Acquire)));
            let span = ring
                .as_ref()
                .and_then(|r| Some((r.oldest_stamp()?, r.newest_stamp()?)));
            w.text("oldest_ns");
            match span {
                Some((oldest, _)) => w.i64(oldest),
                None => w.null(),
            }
            w.text("newest_ns");
            match span {
                Some((_, newest)) => w.i64(newest),
                None => w.null(),
            }
        }
        w.finish()
    }
}
