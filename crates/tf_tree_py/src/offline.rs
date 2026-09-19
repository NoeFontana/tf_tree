//! The offline API — `docs/PHASE5.md` §4.
//!
//! §4.1 (NORMATIVE): a `.tft` opens into the **same** [`PyTree`](crate::PyTree) a live arena
//! does. This module adds only [`open_file`], [`freeze_impl`] and the queries §4.2/§4.4
//! cannot phrase online: [`span_impl`] and the *names* half of `edges`.
//!
//! Not shipped: `resample`, per-edge statistics, `gaps()` (§4.2, §4.4), `manifest`.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pyo3::prelude::*;

use tf_tree::unstable::ArenaView;
use tf_tree::{EdgeId, FrameId, LookupError, Plan, Step, Tree};

use crate::errors::{
    detached_err, lookup_err_untagged, no_data_err, resolve_frame, resolved_edge, TfTreeError,
};
use crate::tree::PyTree;

/// Open a frozen `.tft` and read it through the ordinary `Tree` (§4.1).
///
/// Opening is an `mmap`; open it inside the worker, not the parent (§2.2). A `Tree` cannot
/// be pickled: hold a `None` until the first `__getitem__` (§4.3). Under `fork` an inherited
/// mapping keeps working.
///
/// `path` is any `os.PathLike`, including non-UTF-8 paths.
#[pyfunction]
#[pyo3(signature = (path, /))]
pub fn open_file(path: PathBuf) -> PyResult<PyTree> {
    Ok(PyTree::wrap(std::sync::Arc::new(open_frozen(&path)?)))
}

/// The interval over which a plan is answerable, or `None` when it is unbounded.
///
/// # Errors
///
/// Everything `Plan::span` reports, mapped by [`lookup_err_untagged`], except
/// `LookupError::NoData`, re-raised naming the silent edge on the path.
pub(crate) fn span_impl(
    py: Python<'_>,
    tree: &Tree,
    target: &str,
    source: &str,
) -> PyResult<Option<(i64, i64)>> {
    // `resolve_frame`, not the interning `Tree::frame`: `span` must not add a frame.
    let t = resolve_frame(py, tree, target)?;
    let s = resolve_frame(py, tree, source)?;
    let plan = tree
        .plan(t, s)
        .map_err(|e| lookup_err_untagged(py, tree, e))?;
    plan.span(&tree.guard()).map_err(|e| match e {
        LookupError::NoData { edge } => {
            let view = tree.arena_view();
            let (label, named) = resolved_edge(tree, &view, edge);
            no_data_err(
                py,
                named,
                format!(
                    "{label} on the path from {source:?} to {target:?} has no \
                     samples, so the path is not answerable at any stamp"
                ),
            )
        }
        other => lookup_err_untagged(py, tree, other),
    })
}

/// `(parent, child)` frame names for an edge, or `None` if either is missing.
///
/// Takes the view so per-edge callers build it once.
pub(crate) fn named_edge_in(view: &ArenaView<'_>, edge: EdgeId) -> Option<(String, String)> {
    // One observation of the record: two reads could name a parent and child of different edges.
    let rec = view.edge(edge)?;
    let name = |raw: u32| named_frame_in(view, FrameId::new(raw)?);
    Some((name(rec.parent)?, name(rec.child)?))
}

/// One frame's stored name, or `None` if this arena has no usable record there.
///
/// Rejects the root sentinel, headroom slots past `frame_count`, and a slot counted by a
/// concurrent interner before its record exists (`name_hash == 0`). `Relaxed`: `frame_count`
/// is bumped before the record is written, so the `name_hash` check is the guard.
pub(crate) fn named_frame_in(view: &ArenaView<'_>, frame: FrameId) -> Option<String> {
    if frame.get() > view.header().frame_count.load(Ordering::Relaxed) {
        return None;
    }
    let rec = view.frame_record(frame)?;
    if rec.name_hash == 0 {
        return None;
    }
    Some(stored_name(&rec.name, rec.name_len))
}

/// A frame record's stored — possibly truncated — name. Lossy UTF-8: a cut can land
/// mid-codepoint, and one bad byte must not fail a whole listing.
fn stored_name(bytes: &[u8], len: u8) -> String {
    let n = (len as usize).min(bytes.len());
    String::from_utf8_lossy(&bytes[..n]).into_owned()
}

/// The frame names on this tree, in `FrameId` order, behind `Tree.frames`.
///
/// A snapshot; names are not promised unique (a rescued interner's abandoned record lands at
/// a second id), so `len()` is an upper bound.
///
/// # Errors
///
/// [`detached_err`] on a tree inherited across a `fork()`.
pub(crate) fn frames_impl(tree: &Tree) -> PyResult<Vec<String>> {
    // A fork-detached tree's poison arena reads 0 frames (`docs/PHASE5.md` §4.3).
    if tree.detached() {
        return Err(detached_err());
    }
    let view = tree.arena_view();
    // Ids are `1..=frame_count`; `Relaxed` as in `named_frame_in`.
    let count = view.header().frame_count.load(Ordering::Relaxed);
    let mut out = Vec::with_capacity(count as usize);
    for raw in 1..=count {
        let Some(id) = FrameId::new(raw) else {
            continue;
        };
        let Some(name) = named_frame_in(&view, id) else {
            continue;
        };
        out.push(name);
    }
    Ok(out)
}

/// The edges on this tree as `(parent, child)` name pairs, behind `Tree.edges`.
///
/// The graph, **not** a round trip: an edge's kind is not reported, so a rebuilt tree turns
/// every static edge dynamic. The pair is the *declared* endpoints (`EdgeRecord::parent`).
/// Names only (§4.2).
///
/// # Errors
///
/// [`detached_err`] on a tree inherited across a `fork()`.
pub(crate) fn edges_impl(tree: &Tree) -> PyResult<Vec<(String, String)>> {
    // A detached tree's poison arena has zero edges: refuse rather than return `[]`.
    if tree.detached() {
        return Err(detached_err());
    }
    let view = tree.arena_view();
    // Real ids are `1..edge_count` (the count includes the sentinel).
    let count = view.header().edge_count.load(Ordering::Relaxed);
    let mut out = Vec::with_capacity(count.saturating_sub(1) as usize);
    for raw in 1..count {
        // `None` (a zeroed record) keeps sentinel and headroom slots out; pinned in `test_api.py`.
        if let Some(pair) = named_edge_in(&view, EdgeId(raw)) {
            out.push(pair);
        }
    }
    Ok(out)
}

/// The **dynamic** edges a compiled plan samples, behind `Plan.edges`.
///
/// Static edges fold into one `Step::Static`; only `Step::Dyn` steps are listed, in fold order.
///
/// # Errors
///
/// [`detached_err`] on a tree inherited across a `fork()`.
pub(crate) fn plan_edges_impl(tree: &Tree, plan: &Plan) -> PyResult<Vec<(String, String)>> {
    if tree.detached() {
        return Err(detached_err());
    }
    let view = tree.arena_view();
    let mut out = Vec::with_capacity(plan.len());
    for step in plan.steps() {
        let Step::Dyn { edge, .. } = step else {
            continue;
        };
        if let Some(pair) = named_edge_in(&view, *edge) {
            out.push(pair);
        }
    }
    Ok(out)
}

/// Write this tree's arena to `path` as a `.tft` (§2.3), behind `Tree.freeze`.
///
/// `source_digest` is BLAKE3 of the source recording, all-zero when there is none (`0046`).
/// The GIL is released for the copy.
#[cfg(target_os = "linux")]
pub(crate) fn freeze_impl(
    py: Python<'_>,
    tree: &Tree,
    path: &Path,
    source: Option<&str>,
    source_digest: [u8; 32],
) -> PyResult<()> {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0);
    // `freeze_to` reads without a `Guard` and would `SIGSEGV`; `docs/PHASE3.md` §8.1 requires
    // `ChildProcessDetachedError`.
    if tree.detached() {
        return Err(detached_err());
    }
    py.detach(|| tree.freeze_to(path, source, source_digest, created))
        .map(|_| ())
        .map_err(|e| frozen_err(path, e))
}

/// See [`freeze_impl`]. Linux-only.
#[cfg(not(target_os = "linux"))]
pub(crate) fn freeze_impl(
    _py: Python<'_>,
    _tree: &Tree,
    _path: &Path,
    _source: Option<&str>,
    _source_digest: [u8; 32],
) -> PyResult<()> {
    Err(not_on_this_platform())
}

#[cfg(target_os = "linux")]
fn open_frozen(path: &Path) -> PyResult<Tree> {
    Tree::open_frozen(path).map_err(|e| frozen_err(path, e))
}

/// See [`open_file`]. Linux-only.
#[cfg(not(target_os = "linux"))]
fn open_frozen(_path: &Path) -> PyResult<Tree> {
    Err(not_on_this_platform())
}

/// The method still exists and refuses, so a portable script gets no `AttributeError`.
#[cfg(not(target_os = "linux"))]
fn not_on_this_platform() -> PyErr {
    TfTreeError::new_err(
        "frozen .tft files need the mmap-backed arena, which is Linux-only in \
         this build",
    )
}

/// Map a `.tft` failure onto Python, keeping the path and the remedy.
///
/// Errno failures become `OSError` subclasses; container failures stay `TfTreeError` and
/// carry §2.4's remedy (re-freeze).
#[cfg(target_os = "linux")]
fn frozen_err(path: &Path, e: tf_tree::FrozenFileError) -> PyErr {
    use tf_tree::{FrozenError, FrozenFileError, ShmError};
    let shown = path.display();
    match e {
        FrozenFileError::Path { raw_os_error } if raw_os_error != 0 => {
            let io = std::io::Error::from_raw_os_error(raw_os_error);
            PyErr::new::<pyo3::exceptions::PyOSError, _>((
                raw_os_error,
                io.to_string(),
                // `OsString`, not `PathBuf`: keeps `e.filename` a `str`.
                path.as_os_str().to_owned(),
            ))
        }
        FrozenFileError::Path { .. } => {
            TfTreeError::new_err(format!("{shown}: could not be opened"))
        }
        FrozenFileError::Frozen(f) => {
            let detail = match f {
                FrozenError::BadMagic => {
                    "does not begin with the .tft magic, so it is not a frozen arena".to_owned()
                }
                FrozenError::LayoutMismatch { found, expected } => format!(
                    "was written with arena layout hash {found:#010x}; this build computes \
                     {expected:#010x}. Re-freeze the source recording — a .tft is a cache, \
                     not an archive (`tf_tree doctor --explain-version`)"
                ),
                FrozenError::VersionMismatch { found, expected } => format!(
                    "was written by arena FORMAT_VERSION {found}; this build speaks \
                     {expected}. Re-freeze the source recording — a .tft is a cache, not \
                     an archive (`tf_tree doctor --explain-version`)"
                ),
                // Exhaustive on purpose: a new variant is a compile error. Only the layout/version
                // arms mean "re-freeze"; damaged-file arms must not.
                FrozenError::Truncated => {
                    "ends before a structure its own header promises — the write \
                     was interrupted, or the file is still being written"
                        .to_owned()
                }
                FrozenError::HeaderInconsistent => {
                    "has a header whose offsets do not describe a consistent \
                     file: a region runs past the end, the arena is misaligned, \
                     or the manifest overlaps something. The file is corrupt"
                        .to_owned()
                }
                FrozenError::SizeMismatch { actual, expected } => format!(
                    "is {actual} bytes but its header says {expected}; the file is \
                     truncated or has been appended to"
                ),
                FrozenError::Io(errno) | FrozenError::Map(errno) => format!(
                    "could not be read or mapped: {}",
                    std::io::Error::from_raw_os_error(errno.raw_os_error())
                ),
                // Engine `Display` forwarded (`0059`); `LayoutMismatch` split out per §2.4.
                FrozenError::Arena(inner @ ShmError::LayoutMismatch { .. }) => format!(
                    "contains an arena image whose layout hash is not this \
                     build's. Re-freeze the source recording — a .tft is a cache, \
                     not an archive (`tf_tree doctor --explain-version`). The \
                     engine's reason: {inner}"
                ),
                FrozenError::Arena(inner) => format!(
                    "contains an arena image whose header did not validate; the \
                     file is corrupt, or was written by a build with a different \
                     arena layout. The engine's reason: {inner}"
                ),
            };
            TfTreeError::new_err(format!("{shown}: {detail}"))
        }
        // `#[non_exhaustive]`: a new variant reaches Python as a base `TfTreeError`.
        other => TfTreeError::new_err(format!("{shown}: {other:?}")),
    }
}
