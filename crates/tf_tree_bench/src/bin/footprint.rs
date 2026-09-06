//! Memory footprint and computation-per-lookup, tf_tree vs `tf2::BufferCore`.
//!
//! `docs/PHASE1.md` §11 pins down *latency*. This binary covers the other two
//! axes a migration decision actually turns on: how much memory each engine
//! costs to hold the same history, and how much work each performs per lookup.
//!
//! # One engine per process, on purpose
//!
//! Every mode measures exactly one engine and then exits. Building both in one
//! process would let the first engine's freed chunks satisfy the second's
//! requests, so whichever ran second would look cheaper by an amount nobody can
//! bound. `just footprint` runs the modes separately and assembles the table.
//!
//! # Memory: `mallinfo2`, not RSS
//!
//! RSS is page-granular, includes text and stacks, and moves with the kernel's
//! reclaim mood. `mallinfo2` reports glibc's own accounting, and because C++
//! `operator new` bottoms out in `malloc` it sees tf2's allocations and Rust's
//! on the same footing — which is the whole point, since the two engines are
//! written in different languages.
//!
//! In-use bytes are `uordblks + hblkhd`: allocations above glibc's mmap
//! threshold (128 KiB by default) do not appear in `uordblks`, and tf_tree's
//! arena is a single allocation far above it, so reporting `uordblks` alone
//! would show tf_tree using almost nothing. That would be flattering and wrong.
//!
//! # Computation: cachegrind, not perf counters
//!
//! Instruction counts here are meant to be *reproducible*, including on
//! machines where `perf_event_paranoid` forbids hardware counters (this one).
//! `cachegrind` simulates, so its `Ir` is exact and machine-independent.
//! `--mode lookup-* 0` performs setup and no lookups, so subtracting it from a
//! run of `N` lookups removes construction, teardown and process startup
//! exactly, leaving instructions attributable to the lookups alone.
//!
//! Run: `just footprint` (needs the container for the tf2 modes).
// `print_stdout`/`print_stderr`: this binary's entire output *is* its result —
// `just footprint` pipes it into the table in `docs/benchmarks/tf2.md`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]
// **`docs/decisions/0007` rule 1, kind 2 — the OS** (`docs/decisions/0048`: a
// kind is a property, not a crate name). Declared here rather than inherited:
// `crates/tf_tree_bench/src/lib.rs` is `#![forbid(unsafe_code)]` and a bin is a
// **separate crate root**, so that attribute governs none of this file — which
// is why this binary's `mallinfo2` call compiled under a plain `just build` for
// the whole life of the project.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

// SAFETY (module invariant): the single `unsafe` block below calls glibc's
// `mallinfo2`, declared in this file's own `extern "C"` block because libc does
// not expose it. It takes no arguments, reads only the allocator's own
// bookkeeping, and returns a POD struct by value whose `MallInfo2` mirror is the
// documented ten-`size_t` layout. The alternatives measure something else: a
// `GlobalAlloc` counter is itself an `unsafe impl`, and /proc RSS is resident
// pages rather than allocator bookkeeping.

use std::hint::black_box;

use tf_tree::{InterpPolicy, Stamp};
use tf_tree_bench::fixture;

/// glibc's `struct mallinfo2` — ten `size_t` fields.
///
/// Declared here rather than pulled from the `libc` crate because this is the
/// only foreign item the benchmark crate needs, and the layout is stable ABI.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MallInfo2 {
    arena: usize,
    ordblks: usize,
    smblks: usize,
    hblks: usize,
    /// Bytes in `mmap`ed regions — where any allocation over glibc's 128 KiB
    /// mmap threshold lands, including tf_tree's whole arena.
    hblkhd: usize,
    usmblks: usize,
    fsmblks: usize,
    /// Bytes in use from the normal heap.
    uordblks: usize,
    fordblks: usize,
    keepcost: usize,
}

extern "C" {
    fn mallinfo2() -> MallInfo2;
}

/// Bytes currently in use across both the sbrk heap and mmapped regions.
fn heap_in_use() -> usize {
    // SAFETY: `mallinfo2` takes no arguments, reads only glibc's allocator
    // bookkeeping, and returns a POD struct by value. `MallInfo2` mirrors the
    // documented ten-`size_t` layout. (Bench binary; the engine crates are
    // `#![forbid(unsafe_code)]`.)
    let mi = unsafe { mallinfo2() };
    mi.uordblks + mi.hblkhd
}

/// Samples the fixture holds after `spin_up`: one per dynamic edge per tick.
fn fixture_sample_count() -> usize {
    fixture::DYNAMIC_EDGES
        .iter()
        .map(|&(_, _, hz)| (fixture::HISTORY_SECS * hz) as usize)
        .sum()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("mem-tf_tree");
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100_000);

    match mode {
        "mem-tf_tree" => mem_tf_tree(),
        // The tf2-comparable policy. tf2 has no screw-geodesic interpolation, so
        // this is the row that goes head-to-head.
        "lookup-tf_tree" => lookup_tf_tree(n, InterpPolicy::LerpSlerp),
        "lookup-tf_tree-sclerp" => lookup_tf_tree(n, InterpPolicy::ScLerp),
        "push-tf_tree" => push_tf_tree(n),
        #[cfg(feature = "tf2")]
        "mem-tf2" => tf2_modes::mem(),
        #[cfg(feature = "tf2")]
        "lookup-tf2" => tf2_modes::lookup(n),
        #[cfg(feature = "tf2")]
        "push-tf2" => tf2_modes::push(n),
        #[cfg(not(feature = "tf2"))]
        "mem-tf2" | "lookup-tf2" | "push-tf2" => {
            eprintln!("footprint: {mode} needs --features tf2 (build in the container)");
            std::process::exit(2);
        }
        other => {
            eprintln!("footprint: unknown mode {other:?}");
            eprintln!("modes: mem-tf_tree | mem-tf2 | lookup-tf_tree N | lookup-tf2 N");
            std::process::exit(2);
        }
    }
}

/// Heap held by a fully populated tf_tree, plus the arena's own view of itself.
///
/// Two per-sample numbers are reported, and reporting only the flattering one
/// would misrepresent the design:
///
/// * **per declared slot** — the marginal cost of ring capacity.
/// * **per stored sample** — what this fixture's history actually costs. It is
///   larger, because `Capacity::history` rounds each ring up to a power of two
///   (a 1 kHz edge over 10 s needs 10 000 slots and gets 16 384). That rounding
///   is a real cost of fixed-capacity, never-reallocating rings, and it belongs
///   in the comparison rather than averaged out of it.
fn mem_tf_tree() {
    // Two instruments, matching `docker/tf2/native_footprint.cpp` field for
    // field. `mallinfo2` is what the engines can be compared on identically
    // (C++ `operator new` bottoms out in `malloc`); **Pss is what an operator
    // sees in `top`**, and `mallinfo2` cannot see it, because an allocator can
    // hold address space it has not faulted. That difference is the entire
    // subject of decision `0021`, so a table with only the first would hide it.
    let before = heap_in_use();
    let pss_before = tf_tree_bench::mp::self_pss_kib();
    let tree = {
        let (tree, samples) = fixture::populated_tree().expect("build fixture");
        // The harness's own recorded push stream is ~300 KiB of `PushSample` and
        // is not engine memory. Drop it before the snapshot or tf_tree is
        // charged 29% more than it uses.
        drop(samples);
        tree
    };
    let after = heap_in_use();
    let pss_after = tf_tree_bench::mp::self_pss_kib();

    let samples = fixture_sample_count();
    let arena = tree.arena_size_bytes();
    let slots = tree.arena_view().header().pose_slots as usize;
    println!("engine\ttf_tree");
    println!("heap_bytes\t{}", after - before);
    println!("pss_kib_delta\t{}", pss_after.saturating_sub(pss_before));
    println!("pss_kib_total\t{pss_after}");
    println!("arena_bytes\t{arena}");
    println!("declared_slots\t{slots}");
    println!("samples_stored\t{samples}");
    println!("bytes_per_slot\t{:.1}", arena as f64 / slots as f64);
    println!("bytes_per_sample\t{:.1}", arena as f64 / samples as f64);
    // Keep the tree alive across the measurement.
    black_box(&tree);
}

/// `N` publishes onto one dynamic edge — the *write*-path allocation measure.
///
/// This is where the two designs actually diverge on memory. Both engines turn
/// out to be allocation-free per *lookup*, so the read path is not where the
/// difference lives; the write path is, because tf2 allocates a node per stored
/// transform and tf_tree overwrites a preallocated ring slot.
fn push_tf_tree(n: usize) {
    let tree = fixture::build_tree_with(InterpPolicy::LerpSlerp).expect("build fixture");
    let (parent, child, rate_hz) = fixture::DYNAMIC_EDGES[2]; // the 1 kHz edge
    let p = tree.frame(parent).expect("parent");
    let c = tree.frame(child).expect("child");
    let w = tree.claim(c, p).expect("claim");

    let period_ns = (1e9 / rate_hz) as i64;
    for k in 0..n {
        let stamp = k as i64 * period_ns;
        w.push(stamp, &fixture::dynamic_pose(2.0, stamp))
            .expect("push");
    }
    println!("engine\ttf_tree");
    println!("pushes\t{n}");
}

/// Stamp for lookup `i`: walks a 100 ms window ending at `NOW`, matching
/// `docs/PHASE1.md` §11.2's query mix.
///
/// Shared by both engines so they answer the identical question. The step is
/// 1 µs, not 1 ns: a 1 ns step over 100 000 lookups spans only 100 µs, which
/// keeps every query inside a handful of ring slots and measures a cache- and
/// branch-predictor best case rather than the intended window.
fn window_stamp(i: usize) -> i64 {
    fixture::NOW_NS - (i as i64 % 100_000) * 1_000
}

/// `N` plan evaluations at the fixture's deepest dynamic chain
/// (`imu_link <- map`: three dynamic steps after folding).
///
/// `interp` is a parameter because the comparison against tf2 is only fair on
/// `LerpSlerp` — tf2 has no screw-geodesic policy, so charging tf_tree for
/// `ScLerp`'s extra work in a head-to-head row would understate it. Both are
/// reported; the tf2-comparable row is the `LerpSlerp` one.
///
/// Deliberately *not* timed: this mode exists to be run under cachegrind, and a
/// timing loop would add clock reads to the instruction count.
fn lookup_tf_tree(n: usize, interp: InterpPolicy) {
    let tree = fixture::build_tree_with(interp).expect("build fixture");
    {
        let (writers, samples) = fixture::spin_up(&tree).expect("spin up");
        drop(writers);
        drop(samples);
    }
    let target = tree.frame("imu_link").expect("imu_link");
    let source = tree.frame("map").expect("map");
    let plan = tree.plan(target, source).expect("plan");
    let guard = tree.guard();

    let mut acc = 0.0f64;
    for i in 0..n {
        let stamp: Stamp = Stamp::from_nanos(black_box(window_stamp(i)));
        if let Ok(p) = plan.at(&guard, stamp) {
            acc += p.t.x;
        }
    }
    black_box(acc);
    println!("engine\ttf_tree");
    println!("interp\t{interp:?}");
    println!("lookups\t{n}");
}

#[cfg(feature = "tf2")]
mod tf2_modes {
    use super::{black_box, fixture_sample_count, heap_in_use, window_stamp};
    use tf_tree_bench::tf2::Tf2Fixture;
    use tf_tree_tf2_sys::{FrameName, Tf2Buffer};

    /// Heap held by a `tf2::BufferCore` loaded with the identical stream.
    pub(super) fn mem() {
        let before = heap_in_use();
        let fixture = Tf2Fixture::load().expect("load tf2 fixture");
        let after = heap_in_use();

        let samples = fixture_sample_count();
        println!("engine\ttf2");
        println!("heap_bytes\t{}", after - before);
        println!("samples_stored\t{samples}");
        println!(
            "bytes_per_sample\t{:.1}",
            (after - before) as f64 / samples as f64
        );
        black_box(&fixture);
    }

    /// `N` `setTransform` calls onto one edge, mirroring `push_tf_tree`.
    ///
    /// Uses `set_transform_by_name` with prebuilt `std::string` handles for the
    /// same reason `lookup` does: charging tf2 for string allocations a C++
    /// caller never makes would inflate the very number being reported.
    pub(super) fn push(n: usize) {
        use tf_tree_bench::fixture;
        let buffer = Tf2Buffer::new(fixture::HISTORY_SECS * 3.0).expect("tf2 buffer");
        let (parent, child, rate_hz) = fixture::DYNAMIC_EDGES[2]; // the 1 kHz edge
        let p = FrameName::new(parent).expect("parent");
        let c = FrameName::new(child).expect("child");

        let period_ns = (1e9 / rate_hz) as i64;
        for k in 0..n {
            let stamp = k as i64 * period_ns;
            buffer
                .set_transform_by_name(&p, &c, stamp, &fixture::dynamic_pose(2.0, stamp), false)
                .expect("set_transform");
        }
        println!("engine\ttf2");
        println!("pushes\t{n}");
        black_box(&buffer);
    }

    /// `N` `lookupTransform` calls over the same chain and window.
    ///
    /// Uses `lookup_by_name`, which takes pre-built `std::string` handles: the
    /// `&str` overload allocates two C++ strings per call, and charging tf2 for
    /// an allocation a C++ caller never makes would inflate exactly the number
    /// this binary exists to report.
    pub(super) fn lookup(n: usize) {
        let fixture = Tf2Fixture::load().expect("load tf2 fixture");
        let target = FrameName::new("imu_link").expect("imu_link");
        let source = FrameName::new("map").expect("map");

        let mut acc = 0.0f64;
        for i in 0..n {
            let ns = window_stamp(i);
            if let Ok(p) = fixture
                .buffer()
                .lookup_by_name(&target, &source, black_box(ns))
            {
                acc += p.t.x;
            }
        }
        black_box(acc);
        println!("engine\ttf2");
        println!("lookups\t{n}");
    }
}
