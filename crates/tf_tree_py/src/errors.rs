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
create_exception!(
    _core,
    DerivativesUnavailableError,
    TfTreeError,
    "This edge's interpolator has no exact derivative; layout='quat_twist' \
     cannot be served over it."
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
    m.add(
        "DerivativesUnavailableError",
        py.get_type::<DerivativesUnavailableError>(),
    )?;
    Ok(())
}

/// The error every entry point raises on a tree inherited across a `fork()`.
///
/// # Why a message rather than a class of its own
///
/// `TfTreeError` is the base, and this is deliberately not a new leaf: a
/// detached tree is not a condition a program branches on — it is not
/// retryable, not repairable, and the only response is to open a new tree in the
/// child. `docs/PHASE3.md` §4.4's hierarchy exists so a caller can *program*
/// against a distinction (which edge extrapolated, which frame is missing), and
/// there is nothing to program against here.
///
/// The message says what to do because the caller almost never typed `fork` —
/// `multiprocessing` did, and its default start method on Linux is what put
/// them here.
pub(crate) fn detached_err() -> PyErr {
    TfTreeError::new_err(
        "this tree was inherited across a fork(); the child's mapping is gone \
         and the handle cannot be repaired. Open a new tree in the child \
         (tf_tree.open(...)), or use multiprocessing's 'spawn' or 'forkserver' \
         start method",
    )
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
        // **A type, not a message, because a caller branches on it.** `LerpSlerp`
        // is `tf2`'s interpolator and has no exact body twist, so
        // `layout="quat_twist"` over an edge that declares it is refused rather
        // than finite-differenced (`docs/PHASE5.md` §4.4 item 1). The response —
        // re-declare the edge `ScLerp`, or ask for a pose layout — is a decision
        // a program makes, and `docs/API.md` R5 is that the exception *type* is
        // what such a decision may be written against. It is the only refusal
        // `layout="quat_twist"` adds over the pose layouts.
        LookupError::DerivativesUnavailable { edge, interp } => {
            DerivativesUnavailableError::new_err(format!(
                "edge {edge:?} declares interpolation policy {interp}, which has no \
                 exact derivative; use layout='quat' or declare the edge ScLerp"
            ))
        }
        // Routed through the shared spelling so a fork victim gets the same
        // sentence whether it arrived through `lookup` or through `frames`.
        LookupError::ChildDetached => detached_err(),
        other => TfTreeError::new_err(format!("{other:?}")),
    }
}
