#![forbid(unsafe_code)]
//! Shared fixtures and harnesses for the `tf_tree` benchmark suite: the
//! mobile-robot [`fixture`] tree (`docs/PHASE1.md` §11.1), the tf2
//! [`differential`] harness, and the gates built on them.
//!
//! The criterion benches and [`differential::run_naive_rust`] run anywhere. The
//! official go/no-go numbers need core-pinned hardware and a ROS 2 install
//! (`cargo xtask bench-gate`). `tests/zero_alloc.rs` is decisive and runs here.

// No `///` summary on a `mod` line: it is concatenated with the module's own
// `//!` block and resolved in *this* scope, so every intra-doc link in the
// module file becomes an unresolved-link warning with no span.
#[cfg(all(feature = "shm", target_os = "linux"))]
pub mod backing;
pub mod baseline;
pub mod differential;
pub mod embed;
pub mod fixture;
pub mod gate;
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
