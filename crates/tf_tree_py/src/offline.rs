//! The offline API — `docs/PHASE5.md` §4.
//!
//! # There is no offline API
//!
//! That is the point of §4.1, which is NORMATIVE: a `.tft` opens into the
//! **same** [`PyTree`](crate::PyTree) a live arena does, so `plan`, `at`,
//! `at_into`, `adaptive` and `latest` are the objects that were already there.
//! What this module adds is a way in ([`open_file`]), a way out
//! ([`freeze_impl`], behind `Tree.freeze`), and the one query in §4.2 that
//! cannot be phrased in terms of the online API ([`span_impl`]).
//!
//! # What of §4.2 is here, and what deliberately is not
//!
//! §4.2 lists five helpers. Only `span` is in the module, and the omissions are
//! decisions rather than a backlog:
//!
//! * `resample(t0, t1, hz)` is `plan.at(np.arange(t0, t1, 10**9 // hz))` — one
//!   line of NumPy over the vectorised call §4.1 insists is the same one. A
//!   binding for it would be a second spelling of an existing path, which is
//!   exactly what §4.1 forbids.
//! * `edges()`, `gaps()` and `manifest` each need engine surface that does not
//!   exist yet: per-edge rate and jitter are §3's counting pass (the ring knows
//!   what it *retained*, which is not what the source produced — see
//!   `tf_tree::Tree::manifest`'s amendment), and `manifest` needs a CBOR
//!   *reader* where the crate has only a writer. Shipping them off the retained
//!   window would answer a different question than their names promise, which
//!   is the failure §2.3's `samples`/`pushes_total` amendment already had to
//!   correct once.

use std::path::{Path, PathBuf};

use pyo3::prelude::*;

use tf_tree::{EdgeId, FrameId, LookupError, Tree};

use crate::errors::{lookup_err, NoDataError, TfTreeError};
use crate::tree::PyTree;

/// Open a frozen `.tft` and read it through the ordinary `Tree` (§4.1).
///
/// Opening is an `mmap`: microseconds, and no parse. Sixteen dataloader workers
/// that each open the same file share one set of clean page-cache pages, which
/// is the whole argument of §2.2 — so **open it inside the worker**, not once in
/// the parent.
///
/// A `Tree` cannot be pickled, and a `DataLoader` with `num_workers > 0` sends
/// the dataset object to its workers by pickle under `spawn` *and* under
/// `forkserver`, which is CPython 3.14's default start method on Linux. So the
/// §4.3 pattern holds a `None` until the first `__getitem__`. Under a plain
/// `fork` an inherited mapping does keep working — it is `MAP_PRIVATE |
/// PROT_READ` and deliberately not fork-poisoned, unlike a shared-memory attach
/// — which is why the rule is about picklability and not, as §4.3 says, about
/// the arena going away.
///
/// **This text is duplicated in `_core.pyi`, on purpose.** The stub is what an
/// IDE shows; this is what `help(tf_tree.open_file)` shows at a REPL, and the
/// reader in front of a dataloader that has just deadlocked is at the REPL.
///
/// `path` is any `os.PathLike`, not only a `str`: this and `Tree.freeze` are the
/// binding's first filesystem-path arguments, so there is no earlier spelling to
/// stay consistent with, and a dataloader is precisely where paths arrive as
/// `pathlib.Path`. PyO3's `PathBuf` extractor also accepts the non-UTF-8 paths a
/// `&str` parameter cannot represent at all.
#[pyfunction]
#[pyo3(signature = (path, /))]
pub fn open_file(path: PathBuf) -> PyResult<PyTree> {
    Ok(PyTree {
        inner: open_frozen(&path)?,
    })
}

/// The interval over which a plan is answerable, or `None` when it is unbounded.
///
/// The arithmetic is [`tf_tree::Plan::span`] and **this is a forwarder**, which
/// is the whole point: an earlier revision re-derived the window intersection
/// here, in the one crate the workspace's `just test`, `just miri` and
/// `just loom` never build. `tf_tree/src/frozen.rs` makes the same argument
/// about the same arithmetic — the definition of a ring's readable window has
/// already changed once, and a private copy of it does not move when the
/// definition does. Read that method for the three answers `span` distinguishes
/// and why an empty intersection is returned rather than raised.
///
/// Going through `Plan` also buys the two checks a hand-rolled view walk did not
/// have: a plan compiled against an older topology raises
/// `TopologyChangedError`, and a fork-poisoned guard raises rather than reading
/// an arena the child has been detached from.
///
/// # Errors
///
/// Everything `Plan::span` reports, mapped by [`lookup_err`] — except
/// `LookupError::NoData`, which is re-raised **naming the two frames** rather
/// than an `EdgeId`. §4.2's premise is that this query answers "why did my
/// lookup fail at t", and `EdgeId(2)` does not: the Python surface exposes no
/// way to turn an edge id back into the names the caller typed.
pub(crate) fn span_impl(tree: &Tree, target: &str, source: &str) -> PyResult<Option<(i64, i64)>> {
    let t = tree.frame(target).map_err(|_| {
        crate::errors::FrameNotDeclaredError::new_err(format!("no frame named {target:?}"))
    })?;
    let s = tree.frame(source).map_err(|_| {
        crate::errors::FrameNotDeclaredError::new_err(format!("no frame named {source:?}"))
    })?;
    let plan = tree.plan(t, s).map_err(lookup_err)?;
    plan.span(&tree.guard()).map_err(|e| match e {
        LookupError::NoData { edge } => match named_edge(tree, edge) {
            Some((parent, child)) => NoDataError::new_err(format!(
                "edge {parent:?} -> {child:?} on the path from {source:?} to \
                 {target:?} has no samples, so the path is not answerable at any \
                 stamp"
            )),
            None => lookup_err(e),
        },
        other => lookup_err(other),
    })
}

/// `(parent, child)` frame names for an edge, or `None` if either is missing.
///
/// Only ever called on an error path, so the two `String`s it allocates are not
/// a hot-path allocation — the rule they would otherwise violate.
fn named_edge(tree: &Tree, edge: EdgeId) -> Option<(String, String)> {
    let view = tree.arena_view();
    // One observation of the record: re-reading `view.edge(edge)` for the child
    // could name a parent and a child that never belonged to the same edge.
    let rec = view.edge(edge)?;
    let name = |raw: u32| -> Option<String> {
        let r = view.frame_record(FrameId::new(raw)?)?;
        let n = (r.name_len as usize).min(r.name.len());
        Some(String::from_utf8_lossy(&r.name[..n]).into_owned())
    };
    Some((name(rec.parent)?, name(rec.child)?))
}

/// Write this tree's arena to `path` as a `.tft` (§2.3), behind `Tree.freeze`.
///
/// This is the Python entry §3.3 asks for — the way in for a user whose poses
/// were never in a bag — and it is also what makes §4.1's claim testable from
/// Python at all: freeze a tree, reopen the file, and demand the *same* numbers
/// out of the *same* calls.
///
/// `source_digest` is all-zero. §2.3 defines it as BLAKE3 of the source
/// recording, and a tree assembled in Python has none; inventing a digest of the
/// arena bytes instead would put a value in a field that means something else,
/// which is worse than the documented "there was no recording" zero.
///
/// # The GIL is released for the copy
///
/// A freeze is one `write` of the *whole* arena — hundreds of milliseconds and
/// hundreds of megabytes for a tree big enough to be worth freezing, against
/// CPython's 5 ms switch interval. Holding the GIL across it stops every other
/// thread in the process dead, which is the rule `PyPlan::at_many_into` already
/// follows from 1 µs of estimated work upward
/// ([`GIL_RELEASE_THRESHOLD_NS`](crate::tree::GIL_RELEASE_THRESHOLD_NS)). There
/// is no size threshold here because there is no cheap case: the smallest useful
/// arena is still a file write.
///
/// Nothing inside the `detach` touches a Python object — `path` and `source` are
/// already owned Rust values, which is what makes the release sound.
#[cfg(target_os = "linux")]
pub(crate) fn freeze_impl(
    py: Python<'_>,
    tree: &Tree,
    path: &Path,
    source: Option<&str>,
) -> PyResult<()> {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0);
    py.detach(|| tree.freeze_to(path, source, [0u8; 32], created))
        .map(|_| ())
        .map_err(|e| frozen_err(path, e))
}

/// See [`freeze_impl`]. `.tft` support is Linux-only, like the mapping it is.
#[cfg(not(target_os = "linux"))]
pub(crate) fn freeze_impl(
    _py: Python<'_>,
    _tree: &Tree,
    _path: &Path,
    _source: Option<&str>,
) -> PyResult<()> {
    Err(not_on_this_platform())
}

#[cfg(target_os = "linux")]
fn open_frozen(path: &Path) -> PyResult<Tree> {
    Tree::open_frozen(path).map_err(|e| frozen_err(path, e))
}

/// See [`open_file`]. `.tft` support is Linux-only, like the mapping it is.
#[cfg(not(target_os = "linux"))]
fn open_frozen(_path: &Path) -> PyResult<Tree> {
    Err(not_on_this_platform())
}

/// The whole frozen path is `#[cfg(all(feature = "shm", target_os = "linux"))]`
/// in the facade, so on any other platform the *method* still exists and
/// refuses. A missing attribute would make a portable script fail with
/// `AttributeError` at a line that has nothing to do with the reason.
#[cfg(not(target_os = "linux"))]
fn not_on_this_platform() -> PyErr {
    TfTreeError::new_err(
        "frozen .tft files need the mmap-backed arena, which is Linux-only in \
         this build",
    )
}

/// Map a `.tft` failure onto Python, keeping the path and the remedy.
///
/// **The errno path becomes a real `OSError` subclass**, because that is what a
/// Python caller already handles: a missing index raises `FileNotFoundError`,
/// and a `try: ... except FileNotFoundError:` around the open works without
/// anyone learning our exception hierarchy. Passing `(errno, strerror,
/// filename)` is what makes CPython pick the subclass — a bare
/// `OSError(message)` would not.
///
/// The container failures stay `TfTreeError` and carry §2.4's remedy: a
/// `layout_hash` mismatch names **both** values and says to re-freeze, because
/// a `.tft` is a cache and not an archive.
#[cfg(target_os = "linux")]
fn frozen_err(path: &Path, e: tf_tree::FrozenFileError) -> PyErr {
    use tf_tree::{FrozenError, FrozenFileError};
    // `Path` has no `Display`; `display()` is lossy for a non-UTF-8 path, which
    // is right for a *message*. The `filename` attribute below keeps the real
    // bytes, because that is the one a caller may reopen with.
    let shown = path.display();
    match e {
        FrozenFileError::Path { raw_os_error } if raw_os_error != 0 => {
            let io = std::io::Error::from_raw_os_error(raw_os_error);
            PyErr::new::<pyo3::exceptions::PyOSError, _>((
                raw_os_error,
                io.to_string(),
                // `OsString`, deliberately, not `PathBuf`. PyO3 converts a
                // `PathBuf` into a `pathlib.PurePath`, which would make
                // `e.filename` a `PosixPath` even when the caller passed a
                // `str` — CPython's own `OSError.filename` is a `str` there.
                // `OsString` converts with `os.fsdecode` semantics, so it is
                // the string form *and* survives a non-UTF-8 path.
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
                other => format!("could not be opened as a .tft: {other:?}"),
            };
            TfTreeError::new_err(format!("{shown}: {detail}"))
        }
        // `FrozenFileError` is `#[non_exhaustive]`, so a variant added later
        // reaches Python as a base `TfTreeError` rather than failing to
        // compile here — deliberate: this crate is outside the workspace and a
        // compile error in it is found late.
        other => TfTreeError::new_err(format!("{shown}: {other:?}")),
    }
}
