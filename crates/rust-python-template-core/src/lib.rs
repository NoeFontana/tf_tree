#![forbid(unsafe_code)]
//! Pure-Rust core for the rust-python-template project.
//!
//! This crate is the source of truth for the library's logic. It contains no
//! Python or FFI code and has no dependencies on libpython. The Python bindings
//! live in the `rust-python-template-ffi` crate.
//!
//! ## Unsafe policy
//!
//! `#![forbid(unsafe_code)]` is enforced at the crate root. No unsafe is
//! permitted here. If you need unsafe interop, do it in `-ffi`.

/// Returns the sum of two 64-bit signed integers.
///
/// # Examples
///
/// ```
/// use rust_python_template_core::add;
/// assert_eq!(add(2, 3), 5);
/// ```
pub fn add(a: i64, b: i64) -> i64 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_works() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn add_handles_negatives() {
        assert_eq!(add(-1, 1), 0);
        assert_eq!(add(-5, -7), -12);
    }
}
