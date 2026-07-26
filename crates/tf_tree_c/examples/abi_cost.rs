//! C ABI overhead against native Rust — `docs/PHASE4.md` §7, rows 1 and 2.
//!
//! Two gate criteria live here:
//!
//! * **`tft_plan_at` within 5% of native for a depth-3 lookup.** The C ABI adds
//!   a handle validation, a layout dispatch, a `catch_unwind` landing pad and a
//!   write into caller memory. If any of those is not free, this is where it
//!   shows.
//! * **`catch_unwind` overhead on the happy path.** §3.4 asserts it is zero
//!   because it emits landing pads rather than a runtime check. Measured by
//!   calling the *same* trivial body through `tft_layout_size` (no guard) and
//!   through `tft_guarded_noop` (guard, and nothing else), so the difference is
//!   the guard and the `clear_error` it performs — nothing else. An earlier
//!   revision printed only the unguarded number and claimed it measured both;
//!   reported by review.
//!
//! Both sides evaluate the **same plan on the same tree at the same stamps**, so
//! the difference is the boundary and nothing else. The native side writes into a
//! buffer too, so the comparison is not "with a store" against "without one".
//!
//! # Is the measurement good enough for a 5% gate?
//!
//! Yes, and it was checked rather than assumed: four consecutive pinned runs on
//! an idle machine gave 1.021, 1.020, 1.018, 1.026 — a spread of **0.8%** against
//! a 5% allowance. Reported by review, which was right to ask.
//!
//! Run pinned; unpinned runs migrate cores and swing by >30%:
//! `taskset -c 2 cargo run --release -p tf_tree_c --features test-hooks --example abi_cost`
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::expect_used)]

use core::ptr;
use std::hint::black_box;
use std::time::Instant;

use tf_tree_c::*;

const N: usize = 4096;
const ROUNDS: usize = 41;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn bench(mut run: impl FnMut() -> f64) -> f64 {
    for _ in 0..8 {
        black_box(run());
    }
    median(
        (0..ROUNDS)
            .map(|_| {
                let t0 = Instant::now();
                black_box(run());
                t0.elapsed().as_nanos() as f64 / N as f64
            })
            .collect(),
    )
}

fn main() {
    // The C-side handles.
    let mut tree: *mut tft_tree = ptr::null_mut();
    // SAFETY: `tree` is a live local.
    assert_eq!(unsafe { tft_test_tree_create(&mut tree) }, TFT_OK);
    let a = std::ffi::CString::new("map").unwrap();
    let b = std::ffi::CString::new("sensor").unwrap();
    let mut plan: *mut tft_plan = ptr::null_mut();
    // SAFETY: live handle and NUL-terminated names.
    assert_eq!(
        unsafe { tft_plan_create(tree, a.as_ptr(), b.as_ptr(), &mut plan) },
        TFT_OK
    );

    // The identical tree and plan, natively. Built the same way so the two sides
    // walk the same topology and read the same rings.
    let cfg = tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(256));
    let mount = tf_tree::exp_se3([0.3, -0.7, 0.2, 0.11, -0.05, 0.37]);
    let native = tf_tree::TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .dynamic_edge("odom", "base", cfg)
        .static_edge("base", "sensor", &mount)
        .build()
        .unwrap();
    for (parent, child, k) in [("map", "odom", 1.0f64), ("odom", "base", 2.0)] {
        let p = native.frame(parent).unwrap();
        let c = native.frame(child).unwrap();
        let w = native.claim(c, p).unwrap();
        for i in 0..64i64 {
            let f = i as f64;
            w.push(
                i * 10_000_000,
                &tf_tree::exp_se3([
                    0.004 * k * f,
                    -0.003 * f,
                    0.002 * k * f,
                    0.05 * f,
                    -0.02 * k * f,
                    0.01 * f,
                ]),
            )
            .unwrap();
        }
        core::mem::forget(w);
    }
    let nsrc = native.frame("map").unwrap();
    let ndst = native.frame("sensor").unwrap();
    let nplan = native.plan(nsrc, ndst).unwrap();

    let stamps: Vec<i64> = (0..N)
        .map(|i| 10_000_000 + ((i * 7919) % 600_000_000) as i64)
        .collect();

    println!("C ABI overhead — PHASE4 §7");
    println!("==========================");
    println!("{N} lookups/round, median of {ROUNDS} rounds, depth 3\n");

    // --- native: fold, then write the same 128 bytes the C side writes ---
    // `[f64; 16]`, not `[u8; 128]`: `write_mat4` wants `&mut [f64]`, and building
    // one from a byte array would need an alignment this type does not promise.
    // Same 128 bytes either way, so the comparison is still like-for-like.
    let mut nbuf = [0.0f64; 16];
    let native_ns = bench(|| {
        let g = native.guard();
        let mut acc = 0.0;
        for &t in &stamps {
            let iso = nplan
                .at(
                    &g,
                    tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(black_box(t)),
                )
                .unwrap();
            tf_tree::write_mat4(&iso, &mut nbuf);
            acc += nbuf[0];
        }
        acc
    });

    // --- C ABI: the same, through the boundary ---
    let mut cbuf = [0u8; 128];
    let abi_ns = bench(|| {
        let mut acc = 0.0;
        for &t in &stamps {
            // SAFETY: live plan, and `cbuf` is exactly `tft_layout_size(MAT4_ROW)`.
            let rc = unsafe {
                tft_plan_at(
                    plan,
                    black_box(t),
                    TFT_LAYOUT_MAT4_ROW,
                    cbuf.as_mut_ptr().cast(),
                )
            };
            debug_assert_eq!(rc, TFT_OK);
            acc += cbuf[0] as f64;
        }
        acc
    });

    let ratio = abi_ns / native_ns;
    println!("{:>28} {:>10}", "path", "ns/lookup");
    println!("{:>28} {native_ns:>10.1}", "native Rust (+ mat4 write)");
    println!("{:>28} {abi_ns:>10.1}", "tft_plan_at");
    println!("\n  ratio {ratio:.3}x   (gate: < 1.05)");
    println!(
        "  {}",
        if ratio < 1.05 {
            "PASS"
        } else {
            "FAIL — the boundary is costing more than the gate allows"
        }
    );

    // --- batch, where the boundary is amortized over n ---
    let mut big = vec![0u8; N * 128];
    let batch_ns = bench(|| {
        // SAFETY: live plan; `stamps` has N elements and `big` is N*128 bytes.
        let rc = unsafe {
            tft_plan_at_many(
                plan,
                stamps.as_ptr(),
                N,
                TFT_LAYOUT_MAT4_ROW,
                big.as_mut_ptr().cast(),
                0,
            )
        };
        debug_assert_eq!(rc, TFT_OK);
        big[0] as f64
    });
    println!("\n{:>28} {batch_ns:>10.1}", "tft_plan_at_many (per elem)");
    println!(
        "  the boundary is paid once per call, so a batch amortizes it: {:.3}x native",
        batch_ns / native_ns
    );

    // --- catch_unwind, isolated ---
    //
    // Same trivial body, called through `guard` and directly. §3.4 claims this is
    // zero on the happy path; anything else means the landing pads are not free
    // on this target.
    let unguarded = bench(|| {
        let mut acc = 0.0;
        for _ in 0..N {
            acc += tft_layout_size(black_box(TFT_LAYOUT_MAT4_ROW)) as f64;
        }
        acc
    });
    let guarded = bench(|| {
        let mut acc = 0.0;
        for _ in 0..N {
            acc += f64::from(tft_guarded_noop(black_box(0)));
        }
        acc
    });
    println!("\ncatch_unwind, isolated");
    println!("{:>28} {unguarded:>10.2}", "unguarded (tft_layout_size)");
    println!("{:>28} {guarded:>10.2}", "guarded (tft_guarded_noop)");
    println!(
        "  delta {:+.2} ns/call — §3.4 predicts ~0 on the happy path",
        guarded - unguarded
    );

    // SAFETY: each handle freed exactly once.
    unsafe {
        tft_plan_free(plan);
        tft_tree_free(tree);
    }
}
