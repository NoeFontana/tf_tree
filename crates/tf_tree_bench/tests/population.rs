//! Page population — `docs/PHASE2.md` §7.1, `docs/decisions/0005` step 10.
//!
//! §7.1 is NORMATIVE: `mmap` **without** `MAP_POPULATE`, then populate at
//! declaration granularity. The reason is a page-fault storm — a minor fault
//! costs single-digit microseconds against a 150 ns p50 lookup budget, so the
//! *first* lookup after attach pays two orders of magnitude more than the steady
//! state and never appears in a steady-state benchmark.
//!
//! Residency is read from `/proc/self/statm` rather than `mincore`. `mincore`
//! needs the mapping's base pointer, which `tf_tree` does not expose and should
//! not start exposing for a test; RSS is coarser but answers the only question
//! being asked — whether tens of megabytes nobody declared are being charged to
//! this process.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Tree, TreeBuilder};

/// Resident set size in bytes, from `/proc/self/statm` field 2 (pages).
fn rss_bytes() -> usize {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: usize = s.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

/// An arena whose headroom dwarfs its declared content.
///
/// One declared dynamic edge; 200 000 slots of headroom, which is 200 000 x
/// (128 B edge record + 64 B claim record) ~= 36 MiB of tables nobody declared
/// and nothing ever touches, plus the frame table and frame hash on top.
fn over_provisioned(name: &str) -> Tree {
    TreeBuilder::new()
        .default_interp(InterpPolicy::LerpSlerp)
        .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(1024)))
        .edge_headroom(200_000)
        .frame_headroom(200_000)
        .build_shared(name)
        .expect("build_shared")
}

/// **The measurement `0005` step 10 has to be designed against**, checked in so
/// the numbers are reproducible rather than quoted.
///
/// Prints the arena's declared size against the RSS it actually charges. With
/// `MAP_POPULATE` those two numbers are the same; §7.1 exists to make the second
/// one much smaller.
#[test]
fn report_what_creating_an_over_provisioned_arena_charges() {
    let before = rss_bytes();
    let tree = over_provisioned("tf_tree_population_report");
    let after = rss_bytes();
    let size = tree.arena_size_bytes();

    let mib = |b: usize| b as f64 / (1 << 20) as f64;
    println!(
        "arena {:.1} MiB declared; RSS +{:.1} MiB on create ({:.0}% charged)",
        mib(size),
        mib(after.saturating_sub(before)),
        100.0 * (after.saturating_sub(before)) as f64 / size as f64
    );
    // Keep the tree alive across the measurement.
    assert!(tree.arena_size_bytes() > 0);
}
