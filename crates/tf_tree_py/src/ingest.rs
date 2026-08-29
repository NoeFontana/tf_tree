//! Reading a recording from Python — `docs/PHASE5.md` §3 and §4,
//! [`0046`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md).
//!
//! # Why this module exists, in the spec's own words
//!
//! §3's status row says `tf_tree_ingest` is a library crate rather than part of
//! `tf_tree_cli` **"because §4's offline Python API needs the same logic and
//! cannot depend on a binary crate"**. The crate boundary was drawn, and paid
//! for, for this module — and until `0046` this module did not exist. The wheel
//! exposed `open_file` (the way *in* to an index somebody else built) and
//! `Tree.freeze` (the way *out* of a tree assembled by hand), and no way to read
//! a recording at all.
//!
//! # One function, and deliberately not two
//!
//! [`ingest_bag`] returns an ordinary [`Tree`](crate::tree::PyTree) — the same
//! type `open_file` returns — so `plan`, `at`, `span`, `frames`, `edges` and
//! `freeze` all work on it unchanged. That is §4.1's "no parallel offline API"
//! holding structurally rather than by promise.
//!
//! **There is no `freeze_bag` beside it, and the first draft of this module had
//! one.** `tf_tree_ingest::tft::freeze_bag` exists, so binding it directly is
//! the obvious move, and this module's own header used to argue that
//! `ingest_bag(p).freeze(out)` "loses the recording's identity" and that a
//! direct binding was therefore a capability the composition could not express.
//! Reading the function refutes that: it is `digest_file` + `run` + `freeze_to`,
//! it streams the *digest* and not the tree, and `Tree::freeze_to` already takes
//! `source_digest` as a parameter. The gap was entirely in this crate's
//! `Tree.freeze`, which passed a hardcoded zero.
//!
//! So a top-level `freeze_bag` would have been exactly what `CLAUDE.md` forbids
//! — **a second spelling** of `ingest_bag(p).freeze(out)`, differing only in
//! whether the provenance field got filled in, and differing *silently*. The
//! tree carries where it came from instead ([`crate::tree::SourceInfo`]), and
//! `Tree.freeze` writes it. One path, correct by default.
//!
//! # The GIL
//!
//! Released around the whole ingest. This is not the 1 µs threshold
//! [`crate::tree::GIL_RELEASE_THRESHOLD_NS`] applies to a lookup: an ingest is
//! two or more passes over a file that is allowed to be larger than memory,
//! measured in seconds, and holding the GIL across it would stop every other
//! thread in the interpreter. Nothing inside the `detach` touches a Python
//! object — the path and the options are owned Rust values by then.

use std::path::PathBuf;

use pyo3::prelude::*;
use tf_tree_ingest::{Frames, IngestError, IngestOptions};

use crate::errors::TfTreeError;
use crate::tree::{PyTree, SourceInfo};

/// Build the library's options from the keyword arguments this API exposes.
///
/// **Five of `IngestOptions`' ten fields are keywords, and the split is not
/// arbitrary.** `tf_prefix`, the two topic lists and `max_memory_mb` are what a
/// user with a real recording reaches for. `on_clock_reset`, `on_bad_chunk` and
/// the chunk-bomb ceilings are either a single supported value or a guard whose
/// default exists to stop a hostile file, not a knob a dataloader user tunes;
/// exposing all ten would make the signature the struct's shape rather than the
/// task's.
///
/// `max_record_bytes` is the exception among the guards, and it is exposed for
/// the reason it was built:
/// [`0010`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0010-a-ceiling-on-one-record.md) added it so
/// that "the person who meets it can raise it without forking the crate", and
/// reachable only from Rust that argument does not hold for the audience §4 is
/// for.
fn options(
    static_topics: Option<Vec<String>>,
    tf_topics: Option<Vec<String>>,
    tf_prefix: Option<String>,
    max_memory_mb: Option<u64>,
    max_record_bytes: Option<u64>,
) -> IngestOptions {
    let mut opts = IngestOptions {
        tf_prefix,
        ..Default::default()
    };
    if let Some(t) = static_topics {
        opts.roles.static_topics = t;
    }
    if let Some(t) = tf_topics {
        opts.roles.dynamic_topics = t;
    }
    if let Some(mb) = max_memory_mb {
        opts.max_memory_bytes = mb.saturating_mul(1024 * 1024);
    }
    if let Some(b) = max_record_bytes {
        opts.max_record_bytes = b;
    }
    opts
}

/// Map an [`IngestError`] onto Python.
///
/// **The errno variants become real `OSError`s**, on the same argument
/// `offline::frozen_err` makes: a caller who wrote `except FileNotFoundError`
/// around the open should not have to learn this hierarchy to catch a missing
/// recording. Everything else is a [`TfTreeError`] carrying the library's own
/// rendered message.
///
/// `IngestError` is `Copy` and `String`-free (D11), and the rendering happens
/// here — at the boundary — rather than in the error, which is the separation
/// `docs/API.md` §1 R5 asks for.
fn ingest_err(err: IngestError, frames: &Frames) -> PyErr {
    match err {
        IngestError::Io { raw_os_error } | IngestError::Spill { raw_os_error }
            if raw_os_error != 0 =>
        {
            let io = std::io::Error::from_raw_os_error(raw_os_error);
            PyErr::new::<pyo3::exceptions::PyOSError, _>((raw_os_error, io.to_string()))
        }
        other => TfTreeError::new_err(tf_tree_ingest::describe(other, frames).to_string()),
    }
}

/// Read an MCAP recording into an in-memory tree.
///
/// `path` is any `os.PathLike` naming an MCAP recording. Returns an ordinary
/// `Tree`, carrying the recording it came from in `Tree.source` — which is what
/// lets `ingest_bag(p).freeze(out)` write a `.tft` traceable to `p` with
/// nothing extra to remember.
///
/// # The digest is taken on every call
///
/// `Tree.source["digest"]` is BLAKE3 of the recording's bytes, which costs one
/// extra sequential pass over the file. Measured on the development host,
/// hashing runs at gigabytes per second against an ingest that reads the same
/// file at least twice *and* decompresses it, so the digest is a minority of a
/// cost the caller has already chosen to pay — and making it lazy would move a
/// "the recording moved" failure to `freeze`, which is a stranger place to meet
/// it than the call that named the file.
///
/// # Errors
///
/// `OSError` for a recording that cannot be read; `TfTreeError` carrying the
/// library's rendered reason for anything else — a file that is not an MCAP, a
/// `.db3` rosbag2 bag (with the `ros2 bag convert` remedy), a clock reset, an
/// edge whose kind changed mid-recording, or a record over `max_record_bytes`.
#[pyfunction]
#[pyo3(signature = (
    path, /, *,
    static_topics = None, tf_topics = None, tf_prefix = None,
    max_memory_mb = None, max_record_bytes = None
))]
pub(crate) fn ingest_bag(
    py: Python<'_>,
    path: PathBuf,
    static_topics: Option<Vec<String>>,
    tf_topics: Option<Vec<String>>,
    tf_prefix: Option<String>,
    max_memory_mb: Option<u64>,
    max_record_bytes: Option<u64>,
) -> PyResult<PyTree> {
    let opts = options(
        static_topics,
        tf_topics,
        tf_prefix,
        max_memory_mb,
        max_record_bytes,
    );
    let mut frames = Frames::default();
    let (ingested, digest) = py
        .detach(|| {
            // The digest first: if the file cannot be read at all, that is the
            // error to report, and reporting it before the two ingest passes
            // costs nothing on the failing path.
            let digest = tf_tree_ingest::digest_file(&path)?;
            let ingested = tf_tree_ingest::run(&path, &opts, &mut frames)?;
            Ok::<_, IngestError>((ingested, digest))
        })
        .map_err(|e| ingest_err(e, &frames))?;

    let source = SourceInfo {
        path: path.display().to_string(),
        digest,
        transforms: ingested.survey.transforms_read,
        edges_without_samples: ingested.survey.edges_without_samples().len(),
        recording_ns: ingested.survey.span_ns(),
    };
    Ok(PyTree::from_recording(
        std::sync::Arc::new(ingested.tree),
        source,
    ))
}
