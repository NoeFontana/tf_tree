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

/// `docs/PHASE5.md` §10's "benchmark artifact as a regression gate": compare a
/// fresh report against the committed baseline.
pub mod baseline;
pub mod differential;
// **Deliberately no `///` summary here**, unlike the modules around it. An outer
// doc comment on a `mod` declaration is concatenated with the module file's own
// `//!` docs and the whole block is then resolved in *this* module's scope, so
// every intra-doc link in `embed.rs` — `SOURCE_ID`, `Pair::load`, `Run::verdict`
// — becomes an unresolved-link warning. Measured: adding the summary back turns
// 8 working links in that file into 8 `cargo doc` warnings. `embed.rs` opens
// with a `//!` summary of its own, which is what the module index shows.
pub mod embed;
pub mod fixture;
/// Multi-process evaluation harness: open-loop latency, CPU and PSS accounting.
pub mod mp;
pub mod replay;
/// `docs/PHASE5.md` §9's benchmark artifact: the report and its honesty rules.
pub mod report;
/// The A/B run store: what every harness emits, and how two runs are compared.
pub mod runstore;
/// Spawning child processes attached to a shared arena (Phase 2). Linux-only.
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod shm_util;
/// The named workload catalogue: what load a harness is running, in one place.
pub mod workload;

#[cfg(feature = "tf2")]
pub mod replay_tf2;
#[cfg(feature = "tf2")]
pub mod tf2;
