//! ThreadSanitizer over the concurrent read path (`docs/PHASE3.md` §7.3).
//!
//! # Why this exists, given loom already runs
//!
//! `just loom` model-checks `tf_tree_core`'s protocols exhaustively, but over a
//! *model*: `loom::sync` atomics, two or three threads, and a bounded
//! interleaving budget. It is the right tool for "is this ordering correct" and
//! it cannot see anything outside the types it substitutes.
//!
//! TSan is the complement — real threads, real atomics, the real allocator, and
//! the actual generated code. It catches what a model cannot: a race introduced
//! by the *facade* rather than the protocol, an unsynchronised field on `Tree`,
//! a `&mut` aliasing a shared read.
//!
//! # It is what makes the free-threading claim honest
//!
//! `tf_tree_py` declares `gil_used = false`. PyO3 0.29 **defaults that flag to
//! false**, so its presence proves nothing and no test of the attribute can be
//! non-vacuous (`docs/PHASE3.md` §1.2, corrected). What actually supports the
//! declaration is that every `#[pyclass]` is `Send + Sync`, and that the Rust
//! underneath is race-free with many threads reading while one writes — which
//! is exactly this file. The Python layer only calls through.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Stamp, SystemDomain, TreeBuilder};

/// Eight readers and a live writer on one tree.
///
/// The writer matters. A quiescent tree exercises no seqlock retry, invalidates
/// no cache line a reader holds, and would let a completely unsynchronised
/// implementation pass — which is the same reason `mp_bench` insists on a
/// publisher running throughout.
#[test]
fn concurrent_readers_and_a_writer_are_race_free() {
    let tree = Arc::new(
        TreeBuilder::new()
            .default_interp(InterpPolicy::LerpSlerp)
            .dynamic_edge("map", "base", EdgeCfg::new(Capacity::slots(256)))
            .dynamic_edge("base", "cam", EdgeCfg::new(Capacity::slots(256)))
            .build()
            .expect("build"),
    );

    // Seed both edges so the readers have something bracketed to interpolate.
    let child = tree.frame("base").unwrap();
    let parent = tree.frame("map").unwrap();
    let cam = tree.frame("cam").unwrap();
    {
        let p = tree.claim(child, parent).expect("claim");
        let q = tree.claim(cam, child).expect("claim cam");
        for k in 0..64i64 {
            let iso = tf_tree::exp_se3([0.0, 0.0, k as f64 * 0.01, k as f64, 0.0, 0.0]);
            p.push(k * 1_000, &iso).expect("push");
            q.push(k * 1_000, &iso).expect("push cam");
        }
    }

    let stop = Arc::new(AtomicBool::new(false));

    // One writer, appending while the readers run.
    let writer = {
        let tree = Arc::clone(&tree);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let p = tree.claim(child, parent).expect("claim");
            let mut k = 64i64;
            while !stop.load(Ordering::Relaxed) {
                let iso = tf_tree::exp_se3([0.0, 0.0, k as f64 * 0.01, k as f64, 0.0, 0.0]);
                let _ = p.push(k * 1_000, &iso);
                k += 1;
            }
        })
    };

    let readers: Vec<_> = (0..8)
        .map(|_| {
            let tree = Arc::clone(&tree);
            std::thread::spawn(move || {
                let plan = tree.plan(parent, cam).expect("plan");
                for _ in 0..2_000 {
                    let g = tree.guard();
                    // Inside the seeded window, so this is the interpolating
                    // path rather than an immediate extrapolation error.
                    let _ = plan.at(&g, Stamp::<SystemDomain>::from_nanos(32_000));
                    let mut out = [0.0f64; 8 * 16];
                    let stamps: Vec<i64> = (0..8).map(|i| 30_000 + i * 100).collect();
                    let _ = plan.at_many_into::<SystemDomain>(
                        &g,
                        &stamps,
                        tf_tree::Layout::Mat4,
                        &mut out,
                    );
                }
            })
        })
        .collect();

    for r in readers {
        r.join().expect("reader panicked");
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().expect("writer panicked");
}
