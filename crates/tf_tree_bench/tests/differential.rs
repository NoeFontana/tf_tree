//! The differential harness test (decision `0003`, *Differential against tf2*).
//!
//! Runs the default naive-Rust reference differential: tf_tree's `LerpSlerp`
//! lookups vs an independent Rust lookup pipeline over the same sample stream,
//! asserting agreement within `1e-12` across a large random query set. This is
//! the runnable half of the spec's tf2 differential; the `tf2::BufferCore` half
//! needs a ROS 2 machine (`--features tf2`, see `src/tf2.rs`) and is not run here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use tf_tree_bench::differential::{self, Reference};

#[test]
fn naive_rust_reference_agrees_within_1e_12() {
    let report =
        differential::run_naive_rust(100_000, 0xC0FF_EE12_3456_789A).expect("differential");
    assert_eq!(report.reference, Reference::NaiveRust);
    assert_eq!(report.queries, 100_000);
    assert!(
        report.passed(),
        "naive-Rust differential exceeded tolerance: max_error={:e} > tol={:e}",
        report.max_error,
        report.tolerance
    );
    // The two pipelines share only input data and the LerpSlerp math, so we
    // expect agreement far tighter than the 1e-12 gate.
    assert!(
        report.max_error < 1e-9,
        "unexpectedly large disagreement: {:e}",
        report.max_error
    );
}
