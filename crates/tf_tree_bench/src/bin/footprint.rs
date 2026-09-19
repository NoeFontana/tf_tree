//! Memory footprint and computation-per-lookup, tf_tree vs `tf2::BufferCore`.
//!
//! `docs/PHASE1.md` §11 pins latency; this covers memory and work per lookup.
//!
//! One engine per process: building both would let the first's freed chunks
//! satisfy the second's requests. `just footprint` runs the modes separately
//! (the tf2 modes need the container).
//!
//! Memory is `mallinfo2` (`uordblks + hblkhd`), not RSS: C++ `operator new` and
//! Rust both bottom out in `malloc`, and tf_tree's arena is a single mmapped
//! allocation invisible to `uordblks` alone.
//!
//! Computation is cachegrind's exact `Ir`: `--mode lookup-* 0` performs setup
//! only, so subtracting it from an `N`-lookup run leaves the lookups alone.
// Output is the result: `just footprint` pipes it into `docs/benchmarks/tf2.md`'s table.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]
// `docs/decisions/0007` rule 1, kind 2 (the OS), per `0048`. A bin is a separate
// crate root, so the library's `forbid(unsafe_code)` does not govern it.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

// SAFETY (module invariant): the single `unsafe` block calls glibc's `mallinfo2`,
// declared in this file's `extern "C"` block. It takes no arguments, reads only
// allocator bookkeeping, and returns a POD struct mirroring the documented
// ten-`size_t` layout.

use std::hint::black_box;

use tf_tree::{InterpPolicy, Stamp};
use tf_tree_bench::fixture;

/// glibc's `struct mallinfo2` — ten `size_t` fields, declared here (stable ABI).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MallInfo2 {
    arena: usize,
    ordblks: usize,
    smblks: usize,
    hblks: usize,
    /// Bytes in `mmap`ed regions, including tf_tree's whole arena.
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
    // SAFETY: `mallinfo2` takes no arguments, reads only allocator bookkeeping and
    // returns a POD struct by value mirroring the documented layout.
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
        // The tf2-comparable policy (tf2 has no screw-geodesic interpolation).
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

/// Heap held by a fully populated tf_tree. Reports per declared slot (marginal ring
/// capacity) and per stored sample (larger: `Capacity::history` rounds rings up to a
/// power of two).
fn mem_tf_tree() {
    // `mallinfo2` compares the engines; Pss is what an operator sees (0021).
    let before = heap_in_use();
    let pss_before = tf_tree_bench::mp::self_pss_kib();
    let tree = {
        let (tree, samples) = fixture::populated_tree().expect("build fixture");
        // The harness's recorded push stream is not engine memory.
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
    black_box(&tree);
}

/// `N` publishes onto one dynamic edge: the write-path allocation measure.
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

/// Stamp for lookup `i`: a 100 ms window ending at `NOW` (`docs/PHASE1.md` §11.2), in
/// 1 µs steps so queries do not stay inside a few ring slots. Shared by both engines.
fn window_stamp(i: usize) -> i64 {
    fixture::NOW_NS - (i as i64 % 100_000) * 1_000
}

/// `N` plan evaluations at the deepest dynamic chain (`imu_link <- map`). Only
/// `LerpSlerp` is tf2-comparable. Untimed: it runs under cachegrind.
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

    /// `N` `setTransform` calls onto one edge, mirroring `push_tf_tree`, via prebuilt
    /// `std::string` handles so tf2 is not charged for allocations a C++ caller avoids.
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

    /// `N` `lookupTransform` calls over the same chain and window, via `lookup_by_name`
    /// (the `&str` overload allocates two C++ strings per call).
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
