#![forbid(unsafe_code)]
//! Shared fixtures and harnesses for the `tf_tree` benchmark suite.
//!
//! This crate holds the mobile-robot [`fixture`] tree (decision `0003`,
//! *Benchmarks*) that the criterion benches (`benches/*.rs`), the CLI demo
//! (`tf_tree_cli`), the tf2 [`differential`] harness, and the zero-allocation
//! gate all share, so they never drift apart.
//!
//! # What runs where
//!
//! * The criterion benches and the [`differential::run_naive_rust`] cross-check
//!   run on any machine (`cargo bench`, `cargo test`).
//! * The **official** go/no-go numbers (depth-3 p50, read-scaling, the
//!   `tf2::BufferCore` ratio) need dedicated, core-pinned hardware and a ROS 2
//!   install. They are *not* produced by simply building this crate; see
//!   the feature-gated `tf2` module and `cargo xtask bench-gate`.
//! * The zero-allocation gate (`tests/zero_alloc.rs`) is decisive and runs here.

pub mod differential;
pub mod fixture;

#[cfg(feature = "tf2")]
pub mod tf2;
