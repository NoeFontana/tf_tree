//! The differential harness test (`docs/PHASE1.md` §10.5 *Differential against
//! tf2*).
//!
//! Two references, one query loop:
//!
//! * The **naive-Rust** differential runs anywhere: tf_tree's `LerpSlerp` lookups
//!   vs an independent Rust lookup pipeline over the same sample stream.
//! * The **tf2** differential (`--features tf2`) is the migration-credibility
//!   test — ROS 2's own `tf2::BufferCore`, driven with an identical tree and an
//!   identical sample stream. It needs a ROS 2 install; run it with
//!   `just tf2-differential`, which containerises the toolchain.
//!
//! Both are held to the same `1e-12` bound.
// Reporting the achieved agreement is the point of this test — the number is
// the deliverable, not a debugging leftover.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

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

/// The migration-credibility test: tf_tree vs ROS 2's `tf2::BufferCore` over an
/// identical tree and an identical sample stream, `LerpSlerp` on both sides.
///
/// A failure here is a release blocker (`docs/PHASE1.md` §10.5): it means code migrating
/// from tf2 to tf_tree would observe a different transform.
#[cfg(feature = "tf2")]
#[test]
fn tf2_buffer_core_agrees_within_1e_12() {
    let report = differential::run_tf2(100_000, 0xC0FF_EE12_3456_789A).expect("tf2 differential");
    assert_eq!(report.reference, Reference::Tf2);

    // Guard against a vacuous pass: if tf2 declined most queries (cache horizon,
    // unknown frames) the max error would be a meaningless 0.0.
    assert!(
        report.compared > report.queries / 2,
        "tf2 answered only {}/{} queries — the comparison proved little",
        report.compared,
        report.queries
    );
    assert!(
        report.passed(),
        "tf2 differential exceeded tolerance: {report}"
    );
    println!("tf2 differential: {report}");
}
