//! Freezing a tree to a `.tft`, and opening one — `docs/PHASE5.md` §2.
//!
//! # What this module is *not*
//!
//! It is not a read path. §2.1 is NORMATIVE that a frozen arena is read by the
//! identical `Plan::at` code as a live one, so [`Tree::open_frozen`] hands back
//! an ordinary [`Tree`] and every lookup below it is the code that was already
//! there. What is here is the container: the manifest, and the two directions
//! across the filesystem.
//!
//! # The manifest is cold, and nothing reads it to make a decision
//!
//! Everything the read path needs is in the arena image. The manifest exists so
//! a human, or a tool that has never heard of `tf_tree`, can answer "what is in
//! this file and where did it come from" without mapping 233 MB — which is why
//! §2.3 chose CBOR over a packed struct. Losing it would cost provenance and
//! nothing else, and [`FrozenArena::open`](tf_tree_arena::FrozenArena::open)
//! deliberately does not look at it.

use std::path::Path;

use tf_tree_arena::{Arena, FrozenArena, FrozenError, FrozenHeader};

use crate::cbor::Writer;
use crate::tree::Tree;

/// Why a `.tft` could not be opened or written.
///
/// `Copy` and `String`-free like every other error here (`docs/PROJECT.md` §5).
/// The `std::io::Error` from opening the path is reduced to its errno for that
/// reason: an errno is what an operator acts on, and it is what survives being
/// `Copy`.
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
    #[error("{0:?}")]
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
    /// The returned tree is permanently read-only: every mutating entry point
    /// refuses, because the mapping is `PROT_READ` and a store through it would
    /// be a `SIGSEGV` rather than an error (§2.4 — `AttachMode` is implicitly and
    /// permanently `ReadOnly`).
    ///
    /// # Errors
    ///
    /// [`FrozenFileError::Path`] if the file cannot be opened;
    /// [`FrozenFileError::Frozen`] if it is not a `.tft`, is truncated, or was
    /// written by a build with a different `FORMAT_VERSION` or `layout_hash`. The
    /// last case is a hard error by §2.4 and the remedy is to re-freeze — a
    /// `.tft` is a cache, not an archive, and `tf_tree doctor --explain-version`
    /// prints the same reasoning.
    /// # Why there is no `populate_hot` here
    ///
    /// A shared-memory attach prefaults its used extents, because a control loop
    /// must not take a page-fault storm on its first iteration. A `.tft` is the
    /// opposite case and §2.2 says so: a dataloader worker seeks to the
    /// timestamps its batch needs and never touches the rest, and the win is
    /// precisely that untouched pages cost nothing across sixteen workers.
    /// Prefaulting a 233 MB index to serve a query that reads four pages of it
    /// would throw that away.
    pub fn open_frozen(path: &Path) -> Result<Tree, FrozenFileError> {
        let file = std::fs::File::open(path).map_err(|e| path_err(&e))?;
        let arena = FrozenArena::open(file.into())?;
        Ok(Tree::from_frozen(arena))
    }

    /// Write this tree's arena to `path` as a `.tft` (§2.3).
    ///
    /// This is what backs `tf_tree freeze --from-live`. **§5.6's counter capture
    /// is structural here rather than a step**: the whole arena is copied, so the
    /// `EdgeCounters` and `ParticipantCounters` regions land at their own offsets
    /// in the image and are read back through the identical
    /// `ArenaView::edge_counters` accessor. There is no code path that can
    /// forget them, which is a stronger guarantee than remembering to copy them.
    ///
    /// The file is created with `O_TRUNC`, so an existing `.tft` at `path` is
    /// replaced. It is **not** written atomically — a crash mid-freeze leaves a
    /// short file, which [`Tree::open_frozen`] refuses on the `file_size` check
    /// rather than mapping past the end. Callers that need atomicity should
    /// freeze to a temporary path and rename.
    ///
    /// `source_digest` is BLAKE3 of the recording this tree was built from, or
    /// all-zero when there is none — which is the `--from-live` case, since a
    /// live arena has no recording to name.
    ///
    /// # Snapshot consistency
    ///
    /// Freezing a *live* arena copies bytes while publishers are storing into
    /// them. See
    /// [`write_frozen`](tf_tree_arena::write_frozen) for why that is a smear
    /// rather than corruption, and what the per-slot seqlock does about it.
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
        let file = std::fs::File::create(path).map_err(|e| path_err(&e))?;
        let arena: &dyn Arena = self.backing();
        let header = tf_tree_arena::write_frozen(
            std::os::fd::AsFd::as_fd(&file),
            arena,
            &manifest,
            source_digest,
            created_unix_ns,
            tool_version(),
        )?;
        Ok(header)
    }

    /// Build the CBOR manifest for this tree (§2.3).
    ///
    /// # Amendment to §2.3 — the per-edge span is one-sided, and says so
    ///
    /// §2.3 asks for a "per-edge time span". The *newest* stamp is a public
    /// accessor; the oldest **retained** one is computed here from the ring's
    /// head and its retained window, and is emitted as `oldest_ns`. That is the
    /// oldest sample still in the file, which is not the same thing as the oldest
    /// sample the source recording contained — a ring that lapped during ingest
    /// has already dropped the earlier ones. §3's counting pass knows the real
    /// span and can widen this key when it lands; until then the key means what
    /// its name says and no more, because a `span` that silently meant
    /// "whatever survived" would be worse than a narrower one.
    fn manifest(&self, source: Option<&str>, created_unix_ns: i64) -> Vec<u8> {
        let view = self.arena_view();
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
            // `null`, not `""`: an empty path and "frozen from a live arena, so
            // there is no path" are different facts and a reader has to be able
            // to tell them apart.
            None => w.null(),
        }

        // `frame_count` is the number of *interned* frames and ids run
        // `1..=frame_count` — index 0 is `FrameId`'s reserved root sentinel and
        // is not counted. (`edge_count` below is the opposite: it includes its
        // sentinel, so real edge ids are `1..edge_count`. The two fields
        // genuinely disagree; `tf_tree doctor` iterates them the same way.)
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

        // `edge_count` is stored as (declared edges + 1 sentinel), so real ids
        // are `1..edge_count` and the array is indexed by `EdgeId - 1`.
        w.text("edges");
        w.array(edges.saturating_sub(1) as usize);
        for i in 1..edges {
            let id = tf_tree_core::EdgeId(i);
            w.map(7);
            w.text("parent");
            w.u64(u64::from(view.edge(id).map_or(0, |e| e.parent)));
            w.text("child");
            w.u64(u64::from(view.edge(id).map_or(0, |e| e.child)));
            w.text("kind");
            w.u64(u64::from(view.edge(id).map_or(0, |e| e.kind)));
            w.text("capacity");
            w.u64(u64::from(view.edge(id).map_or(0, |e| e.capacity)));
            let head = view
                .edge(id)
                .map_or(0, |e| e.head.load(std::sync::atomic::Ordering::Acquire));
            w.text("samples");
            w.u64(head);
            let span = view.ring(id).and_then(|ring| {
                let newest = ring.newest_stamp()?;
                // The readable window is `[head - retained, head - 1]`
                // (`SampleRing::retained`), so this is the oldest logical index a
                // reader may still touch — clamped at 0 for a ring that has not
                // lapped yet.
                let oldest_idx = head.saturating_sub(ring.retained());
                let oldest = ring.stamps[(oldest_idx & ring.mask) as usize]
                    .load(std::sync::atomic::Ordering::Relaxed);
                Some((oldest, newest))
            });
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
