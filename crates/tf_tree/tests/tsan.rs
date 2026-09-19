//! ThreadSanitizer over the concurrent read path (`docs/PHASE3.md` §7.3).
//!
//! Complements `just loom`: real threads and generated code, so it catches a race
//! in the facade (an unsynchronised field, a `&mut` aliasing a shared read) that
//! a model cannot. It is also what backs `tf_tree_py`'s `gil_used = false`.
#![cfg(all(feature = "shm", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Stamp, SystemDomain, TreeBuilder};

/// Eight readers and a live writer on one tree; a quiescent tree exercises no
/// seqlock retry.
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
