//! Child process for the multi-process integration test and benchmark.
//!
//! Attaches to a shared arena over an **inherited file descriptor** (standard
//! input; see `shm_util::spawn_attached`) and runs the Phase 1 reader unmodified,
//! which is `docs/PHASE2.md` §4's claim. Argument 1 selects the mode:
//!
//! * `verify <target> <source> <ns> <count>`: one line per lookup, `ok <bits...>`
//!   or `err`, for bit-for-bit comparison by the parent.
//! * `bench <target> <source> <count>`: time `count` lookups and print
//!   `ns_per_lookup <f64>` plus the process's own RSS.
//!
//! Output is line-oriented on stdout (parsed by the parent); diagnostics go to stderr.
// stdout IS the protocol; the parent parses it line by line.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::hint::black_box;
use std::os::fd::AsFd;
use std::time::Instant;

use tf_tree::{AttachMode, Stamp, Tree};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args[1].as_str();

    // The parent installed the segment as stdin; duplicate it to an `OwnedFd`.
    let fd = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .expect("duplicate the segment from stdin");

    // ReadOnly on purpose: PROT_READ makes corrupting the arena structurally
    // impossible (`docs/PHASE2.md` §0).
    let tree = Tree::attach_shared(fd, AttachMode::ReadOnly).expect("attach shared arena");
    assert!(tree.is_shared(), "attached tree reports itself heap-backed");

    match mode {
        "verify" => verify(
            &tree,
            &args[2],
            &args[3],
            args[4].parse().unwrap(),
            args[5].parse().unwrap(),
        ),
        "bench" => bench(&tree, &args[2], &args[3], args[4].parse().unwrap()),
        other => {
            eprintln!("shm_child: unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}

/// Emit one line per lookup so the parent can compare bit patterns.
fn verify(tree: &Tree, target: &str, source: &str, base_ns: i64, count: usize) {
    let t = tree.frame(target).expect("target frame");
    let s = tree.frame(source).expect("source frame");
    let plan = tree.plan(t, s).expect("plan");
    let guard = tree.guard();

    for i in 0..count {
        let ns = base_ns - (i as i64) * 1_000_000;
        let stamp: Stamp = Stamp::from_nanos(ns);
        match plan.at(&guard, stamp) {
            Ok(p) => {
                let b = p.to_bits();
                println!(
                    "ok {} {} {} {} {} {} {}",
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6]
                );
            }
            Err(_) => println!("err"),
        }
    }
}

/// Time `count` lookups against the shared arena.
fn bench(tree: &Tree, target: &str, source: &str, count: usize) {
    let t = tree.frame(target).expect("target frame");
    let s = tree.frame(source).expect("source frame");
    let plan = tree.plan(t, s).expect("plan");
    let guard = tree.guard();

    // A 100 ms window, as `footprint::window_stamp`; `(k % 100_000) * 1_000` over
    // `k < 4096` spans only 4 ms (`cost_model`'s header warns of it).
    const WINDOW_NS: i64 = 100_000_000;
    let stamps: Vec<i64> = (0..4096)
        .map(|k: i64| tf_tree_bench::fixture::NOW_NS - k * (WINDOW_NS / 4096))
        .collect();

    let run = |n: usize| {
        let mut acc = 0.0f64;
        for i in 0..n {
            let stamp: Stamp = Stamp::from_nanos(stamps[i % stamps.len()]);
            if let Ok(p) = plan.at(&guard, stamp) {
                acc += p.t.x;
            }
        }
        acc
    };
    black_box(run(10_000));

    let t0 = Instant::now();
    black_box(run(count));
    let ns = t0.elapsed().as_nanos() as f64 / count as f64;

    println!("ns_per_lookup {ns:.2}");
    println!("rss_kib {}", rss_kib());
}

/// Resident set size in KiB from `/proc/self/statm`; a `MAP_SHARED` arena is
/// resident once however many map it, so the parent reports the system-wide figure too.
fn rss_kib() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = s
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    pages * 4
}
