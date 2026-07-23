//! PyO3 bindings for the rust-python-template project.
//!
//! This crate contains data conversion between Python and Rust only. All
//! business logic lives in `rust-python-template-core`. This is the only
//! crate that links libpython.
//!
//! ## Unsafe policy
//!
//! Audited unsafe is permitted in this crate when required for FFI interop
//! (for example, DLPack tensor exchange). The current implementation contains
//! no unsafe blocks. Any future unsafe block must carry a `// SAFETY:` comment
//! that explains the invariants it relies on.

use pyo3::prelude::*;
use rust_python_template_core as core;

/// Adds two integers. Thin wrapper over [`core::add`].
#[pyfunction]
fn add(a: i64, b: i64) -> i64 {
    core::add(a, b)
}

/// The Python extension module, exposed as `rust_python_template._core`.
#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(add, m)?)?;
    Ok(())
}
