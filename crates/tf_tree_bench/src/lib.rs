#![forbid(unsafe_code)]
//! Shared fixtures and harnesses for the `tf_tree` benchmark suite.
//!
//! This crate holds the mobile-robot [`fixture`] tree (`docs/PHASE1.md` §11.1
//! *Fixture*) that the criterion benches (`benches/*.rs`), the CLI demo
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

// **No `///` summary on any `mod` declaration below, and that is a rule rather
// than a style.** An outer doc comment on a `mod` declaration is concatenated
// with the module file's own `//!` docs and the whole block is then resolved in
// *this* module's scope, so every intra-doc link in the module file — items it
// defines itself — becomes an unresolved-link warning pointing at no file at
// all (rustdoc cannot compute a span for the concatenated block, which is why
// these were the hardest of the workspace's rustdoc warnings to locate).
//
// Measured: `embed` carried this comment alone, and the five `mod` lines that
// still had a summary — `baseline`, `mp`, `report`, `runstore`, `workload` —
// were emitting **22 of `cargo doc --no-deps --workspace`'s 80 warnings**
// between them. Removing the five summaries took `-p tf_tree_bench`'s lib doc
// from 22 warnings to 0. Nothing is lost from the module index: every one of
// these files opens with a `//!` summary of its own, which is what the index
// shows.
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod backing;
pub mod baseline;
pub mod differential;
pub mod embed;
pub mod fixture;
pub mod mp;
pub mod replay;
pub mod report;
pub mod runstore;
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod shm_util;
pub mod workload;

#[cfg(feature = "tf2")]
pub mod ratio;
#[cfg(feature = "tf2")]
pub mod replay_tf2;
#[cfg(feature = "tf2")]
pub mod tf2;
