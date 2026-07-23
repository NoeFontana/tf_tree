//! Integration test: verifies the public `add` API of `rust-python-template-core`.

use rust_python_template_core::add;

#[test]
fn integration_add() {
    assert_eq!(add(10, 20), 30);
}

#[test]
fn integration_add_identity() {
    assert_eq!(add(0, 42), 42);
    assert_eq!(add(42, 0), 42);
}
