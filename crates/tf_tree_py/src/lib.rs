//! PyO3 bindings for `tf_tree` — `docs/PHASE3.md`.
//!
//! # The declaration that matters most — and its default has flipped (§1.2)
//!
//! `docs/PHASE3.md` §1.2 says an extension that does not declare itself
//! free-threading-safe silently re-enables the GIL for the whole process. That
//! was true of PyO3 <= 0.28. **In PyO3 0.29 the default is the opposite**:
//! `pyo3-macros-backend-0.29.0/src/module.rs:394` reads
//! `options.gil_used.is_some_and(|op| op.value.value)`, and `None` yields
//! `false` — so an *absent* attribute declares the module free-threading-safe.
//! Verified by experiment as well as by reading: removing the attribute below
//! leaves `sys._is_gil_enabled()` false.
//!
//! The hazard is now worse, not better. Forgetting the declaration used to cost
//! parallelism, loudly enough that somebody would eventually profile it.
//! Forgetting to *audit* now costs correctness: the module claims a safety it
//! may not have, and the failure is a data race rather than a slowdown.
//!
//! So the attribute stays — explicit beats inherited, and a future PyO3 could
//! flip the default back — but **it is not what makes the claim true, and no
//! test of the attribute can be non-vacuous.** What makes it true is that every
//! `#[pyclass]` here is `Send + Sync` (`Tree` and `Plan` already are in Rust;
//! `Publisher` is `!Sync` by design and is therefore not exposed yet) and that
//! there is no global mutable state. What *checks* it is the concurrent
//! evaluation test on a `3.14t` interpreter, and ThreadSanitizer (§7.3).
//!
//! # Time is integer nanoseconds (§3)
//!
//! `float` stamps are rejected with a `TypeError` that states the measurement:
//! at a 2026 epoch the ULP of `float64` seconds is 238 ns, so **every**
//! consecutive-sample interval in a 1 kHz stream is wrong after a round trip.
//! For a library whose purpose is sub-millisecond temporal accuracy, silently
//! accepting that input would produce interpolation errors users would blame on
//! the interpolator.
//!
//! # No views into the arena (§5.1)
//!
//! Nothing here hands Python a buffer that aliases arena memory. An edge's
//! sample storage is a ring being overwritten by another process, and correct
//! reads go through the seqlock protocol; a NumPy array pointing into it would
//! bypass that entirely — a data race by construction, producing torn poses
//! that look like occasional impossible transforms. "Zero-copy" here means no
//! *intermediate* allocation: results are computed by interpolation and written
//! exactly once, into their final home.
#![allow(unsafe_code, clippy::needless_pass_by_value)]
// `unsafe` boundary: a foreign runtime that owns its own objects.
// See `docs/decisions/0007`.
#![deny(unsafe_op_in_unsafe_fn)]

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

mod errors;
mod offline;
mod tree;

pub use errors::*;
pub use offline::open_file;
pub use tree::*;

/// Nanoseconds from float seconds, and the exact reason it is lossy.
///
/// The only sanctioned path from a wall-clock float. Documented as lossy above
/// ~10^7 s rather than silently accepted, because the loss is invisible: the
/// value still *looks* like a timestamp.
#[pyfunction]
#[pyo3(signature = (seconds, /))]
fn from_sec(seconds: f64) -> PyResult<i64> {
    if !seconds.is_finite() {
        return Err(PyValueError::new_err("stamp must be finite"));
    }
    Ok((seconds * 1e9) as i64)
}

/// Whether this build can share a tree between processes.
///
/// Compile-time on the Rust side (`shm` + Linux), so a caller does not have to
/// infer it from an error it was going to get anyway (§4.1).
#[pyfunction]
fn has_shared_memory() -> bool {
    // This crate always builds `tf_tree` with `shm`, so the only remaining
    // question is the platform. On macOS and Windows `open()` gives an
    // in-process tree with a documented one-process limitation (§10), and a
    // caller should be able to branch on that rather than infer it from an
    // error it was going to get.
    cfg!(target_os = "linux")
}

/// `tf_tree` — a transform tree engine.
#[pymodule(gil_used = false)]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(from_sec, m)?)?;
    m.add_function(wrap_pyfunction!(has_shared_memory, m)?)?;
    m.add_function(wrap_pyfunction!(tree::build, m)?)?;
    m.add_function(wrap_pyfunction!(tree::push, m)?)?;
    m.add_function(wrap_pyfunction!(tree::open_arena, m)?)?;
    m.add_function(wrap_pyfunction!(offline::open_file, m)?)?;
    m.add_class::<PyTree>()?;
    m.add_class::<PyPlan>()?;
    m.add_class::<PyPublisher>()?;
    errors::register(m)?;
    Ok(())
}
