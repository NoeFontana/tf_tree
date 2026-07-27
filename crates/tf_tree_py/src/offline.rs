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

use pyo3::prelude::*;

use tf_tree::{Step, Tree};

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
#[pyfunction]
#[pyo3(signature = (path, /))]
pub fn open_file(path: &str) -> PyResult<PyTree> {
    Ok(PyTree {
        inner: open_frozen(path)?,
    })
}

/// The interval over which a plan is answerable, or `None` when it is unbounded.
///
/// §4.2 calls this the single most useful offline query, because "why did my
/// lookup fail at t" is nearly always "one edge on the path had not started
/// yet". It is `LatestCommon` generalised to a range: the **intersection** of
/// every dynamic edge's retained window, so the lower end is a `max` and the
/// upper end a `min`.
///
/// Three answers, kept distinct on purpose:
///
/// * `Some((t0, t1))` with `t0 <= t1` — the plan answers there, and nowhere
///   else without extrapolating.
/// * `Some((t0, t1))` with `t0 > t1` — **an empty intersection is a real
///   answer**, not an error: two edges on the path have disjoint histories, and
///   the caller's `t0 <= t <= t1` is correctly false everywhere. Collapsing it
///   to `None` would make it indistinguishable from the unbounded case below.
/// * `None` — every step folded to a static transform, so the plan is
///   answerable at *any* stamp and there is no finite interval to report.
///
/// An edge with no samples at all raises `NoDataError` naming that edge, rather
/// than returning an empty interval: the distinction the caller acts on is
/// "nobody has published this yet" versus "the windows do not overlap".
///
/// # Why this is not frozen-specific
///
/// It reads retained windows out of the arena, which a live arena has too. On a
/// live tree the answer is a snapshot that ages the moment it is returned — but
/// so is `latest()`, and refusing to answer would be worse than answering the
/// question that was asked.
pub(crate) fn span_impl(tree: &Tree, target: &str, source: &str) -> PyResult<Option<(i64, i64)>> {
    let t = tree.frame(target).map_err(|_| {
        crate::errors::FrameNotDeclaredError::new_err(format!("no frame named {target:?}"))
    })?;
    let s = tree.frame(source).map_err(|_| {
        crate::errors::FrameNotDeclaredError::new_err(format!("no frame named {source:?}"))
    })?;
    let plan = tree.plan(t, s).map_err(lookup_err)?;
    let view = tree.arena_view();
    let mut span: Option<(i64, i64)> = None;
    for step in plan.steps() {
        let Step::Dyn { edge, .. } = step else {
            // A static step carries its transform in the plan and constrains
            // nothing in time.
            continue;
        };
        let ring = view.ring(*edge).ok_or_else(|| {
            TfTreeError::new_err(format!("edge {edge:?} on this path carries no sample ring"))
        })?;
        // Both ends from **one** ring handle. Re-reading the view per end would
        // let a concurrent push move the window between them and produce an
        // interval neither state ever had.
        let (Some(oldest), Some(newest)) = (ring.oldest_stamp(), ring.newest_stamp()) else {
            return Err(NoDataError::new_err(format!(
                "edge {edge:?} on this path has no samples, so the path is not \
                 answerable at any stamp"
            )));
        };
        span = Some(match span {
            None => (oldest, newest),
            Some((lo, hi)) => (lo.max(oldest), hi.min(newest)),
        });
    }
    Ok(span)
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
#[cfg(target_os = "linux")]
pub(crate) fn freeze_impl(tree: &Tree, path: &str, source: Option<&str>) -> PyResult<()> {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0);
    tree.freeze_to(std::path::Path::new(path), source, [0u8; 32], created)
        .map(|_| ())
        .map_err(|e| frozen_err(path, e))
}

/// See [`freeze_impl`]. `.tft` support is Linux-only, like the mapping it is.
#[cfg(not(target_os = "linux"))]
pub(crate) fn freeze_impl(_tree: &Tree, _path: &str, _source: Option<&str>) -> PyResult<()> {
    Err(not_on_this_platform())
}

#[cfg(target_os = "linux")]
fn open_frozen(path: &str) -> PyResult<Tree> {
    Tree::open_frozen(std::path::Path::new(path)).map_err(|e| frozen_err(path, e))
}

/// See [`open_file`]. `.tft` support is Linux-only, like the mapping it is.
#[cfg(not(target_os = "linux"))]
fn open_frozen(_path: &str) -> PyResult<Tree> {
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
fn frozen_err(path: &str, e: tf_tree::FrozenFileError) -> PyErr {
    use tf_tree::{FrozenError, FrozenFileError};
    match e {
        FrozenFileError::Path { raw_os_error } if raw_os_error != 0 => {
            let io = std::io::Error::from_raw_os_error(raw_os_error);
            PyErr::new::<pyo3::exceptions::PyOSError, _>((
                raw_os_error,
                io.to_string(),
                path.to_owned(),
            ))
        }
        FrozenFileError::Path { .. } => {
            TfTreeError::new_err(format!("{path}: could not be opened"))
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
            TfTreeError::new_err(format!("{path}: {detail}"))
        }
        // `FrozenFileError` is `#[non_exhaustive]`, so a variant added later
        // reaches Python as a base `TfTreeError` rather than failing to
        // compile here — deliberate: this crate is outside the workspace and a
        // compile error in it is found late.
        other => TfTreeError::new_err(format!("{path}: {other:?}")),
    }
}
