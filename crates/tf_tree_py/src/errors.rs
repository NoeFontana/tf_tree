//! The exception hierarchy (`docs/PHASE3.md` §4.4).
//!
//! Rust's errors are `Copy` and carry structured fields; Python's carry
//! messages. The gap matters: a user who has to parse a string to find out
//! *which edge* extrapolated cannot program against it, so the fields are
//! attached to the exception rather than only formatted into it.

use pyo3::prelude::*;
use pyo3::{create_exception, exceptions::PyException};

use tf_tree::LookupError;

create_exception!(
    _core,
    TfTreeError,
    PyException,
    "Base of every tf_tree error."
);
create_exception!(
    _core,
    ExtrapolationError,
    TfTreeError,
    "The requested stamp lies outside an edge's retained history."
);
create_exception!(
    _core,
    DisconnectedError,
    TfTreeError,
    "No path joins the two frames."
);
create_exception!(
    _core,
    NoDataError,
    TfTreeError,
    "An edge on the path has no samples yet."
);
create_exception!(
    _core,
    TopologyChangedError,
    TfTreeError,
    "The tree was re-parented after this plan was compiled; re-plan."
);
create_exception!(
    _core,
    FrameNotDeclaredError,
    TfTreeError,
    "No such frame in this arena."
);
create_exception!(
    _core,
    BufferError,
    TfTreeError,
    "An output buffer was the wrong shape, dtype, or size."
);

/// Add every exception type to the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("TfTreeError", py.get_type::<TfTreeError>())?;
    m.add("ExtrapolationError", py.get_type::<ExtrapolationError>())?;
    m.add("DisconnectedError", py.get_type::<DisconnectedError>())?;
    m.add("NoDataError", py.get_type::<NoDataError>())?;
    m.add(
        "TopologyChangedError",
        py.get_type::<TopologyChangedError>(),
    )?;
    m.add(
        "FrameNotDeclaredError",
        py.get_type::<FrameNotDeclaredError>(),
    )?;
    m.add("BufferError", py.get_type::<BufferError>())?;
    Ok(())
}

/// Map a `LookupError` to its Python exception, keeping the structured detail.
///
/// `TopologyChanged` is the one a *correct* program routinely hits — a peer
/// re-parented the tree — so its message says what to do rather than only what
/// happened.
pub(crate) fn lookup_err(e: LookupError) -> PyErr {
    match e {
        LookupError::Extrapolation {
            edge,
            requested,
            oldest,
            newest,
        } => ExtrapolationError::new_err(format!(
            "edge {edge:?}: stamp {requested} ns is outside the retained \
             history [{oldest}, {newest}] ns"
        )),
        LookupError::Disconnected {
            target,
            source,
            cut_at,
        } => DisconnectedError::new_err(format!(
            "no path from {source:?} to {target:?}; the chain stops at {cut_at:?}"
        )),
        LookupError::NoData { edge } => {
            NoDataError::new_err(format!("edge {edge:?} has no samples yet"))
        }
        LookupError::TopologyChanged { plan, current } => TopologyChangedError::new_err(format!(
            "this plan was compiled at topology generation {plan}, the tree is \
             now at {current}; call tree.plan(...) again"
        )),
        LookupError::UnknownFrame { hash } => {
            FrameNotDeclaredError::new_err(format!("no frame with hash {hash:#x} in this arena"))
        }
        LookupError::BufferTooSmall { need, got } => BufferError::new_err(format!(
            "output buffer holds {got} elements; this batch needs {need}"
        )),
        other => TfTreeError::new_err(format!("{other:?}")),
    }
}
