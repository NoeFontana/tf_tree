//! Reading a recording from Python — `docs/PHASE5.md` §3 and §4,
//! [`0046`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md).
//!
//! [`ingest_bag`] returns an ordinary [`Tree`](crate::tree::PyTree) (§4.1); there
//! is no `freeze_bag` — use `ingest_bag(p).freeze(out)`.
//!
//! # The GIL
//!
//! Released around the whole ingest; nothing inside the `detach` touches a
//! Python object.

use std::path::PathBuf;

use pyo3::prelude::*;
use tf_tree_ingest::{Frames, IngestError, IngestOptions};

use crate::errors::TfTreeError;
use crate::tree::{PyTree, SourceInfo};

/// Build the library's options from the exposed keyword arguments.
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
/// Errno variants become `OSError`; the rest a [`TfTreeError`] (`docs/API.md` §1 R5).
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
/// `path` is any `os.PathLike` naming an MCAP recording. Returns a `Tree`
/// whose `Tree.source` records it; `Tree.source["digest"]` is its BLAKE3.
///
/// # Errors
///
/// `OSError` for an unreadable recording; `TfTreeError` for anything else
/// (not an MCAP, a clock reset, a changed edge kind, a record over
/// `max_record_bytes`).
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
