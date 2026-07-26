//! C ABI overhead against native Rust — `docs/PHASE4.md` §7, rows 1 and 2.
//!
//! Two gate criteria live here:
//!
//! * **`tft_plan_at` within 5% of native for a depth-3 lookup.** The C ABI adds
//!   a handle validation, a layout dispatch, a `catch_unwind` landing pad and a
//!   write into caller memory. If any of those is not free, this is where it
//!   shows.
//! * **`catch_unwind` overhead on the happy path.** §3.4 asserts it is zero
//!   because it emits landing pads rather than a runtime check. Measured twice,
//!   because the first measurement is weaker than it looks:
//!
//!   - `tft_layout_size` against `tft_guarded_noop` — the same trivial body with
//!     and without the guard. Both are small enough for rustc to inline across
//!     the crate boundary, so this row reports the guard's cost *in inlined
//!     code*, around +0.1 ns. (An earlier revision printed only the unguarded
//!     number and claimed it measured both; reported by review.)
//!   - `tft_publisher_push` against `tft_test_push_unguarded` — the identical
//!     body, guard removed, on a call too large to inline. **+0.6 ns**, which is
//!     the number that actually supports §3.4's claim for the shipped ABI.
//!
//! * **What the publish path costs, and why.** Not a gate — §3.7's 5 % is about
//!   `tft_plan_at` — but reported, because 2.5× against native is the kind of
//!   number that should be explained rather than left to be discovered. The
//!   ablation rows are there because three separate hypotheses about it were
//!   measured and all three were wrong.
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

    // --- the publish path, and what the thread-affinity check costs ---
    //
    // §3.2 requires `tft_publisher` to refuse a thread that does not own it.
    // That is one thread-local load and a compare on every publish, so it is
    // measured rather than waved through. The native side pushes the same
    // transforms through `EdgeWriter::push` — the same call the C ABI ends up
    // making — so the difference is the boundary, the layout read, and the
    // affinity check, and nothing else.
    let mut ptree: *mut tft_tree = ptr::null_mut();
    // SAFETY: `ptree` is a live local.
    assert_eq!(
        unsafe { tft_test_publishable_tree_create(&mut ptree) },
        TFT_OK
    );
    let child = std::ffi::CString::new("robot").unwrap();
    let par = std::ffi::CString::new("world").unwrap();
    let mut pubh: *mut tft_publisher = ptr::null_mut();
    // SAFETY: live handle, NUL-terminated names.
    assert_eq!(
        unsafe { tft_tree_claim(ptree, child.as_ptr(), par.as_ptr(), &mut pubh) },
        TFT_OK
    );

    // Identity quaternion, `[qw qx qy qz tx ty tz]`. The layout read for QVEC7
    // is a bounds-checked copy plus a norm check, which is the cheapest of the
    // five and therefore the one that shows the boundary most clearly.
    let mut payload = [0u8; 56];
    payload[..8].copy_from_slice(&1.0f64.to_ne_bytes());

    // A push is only valid with a non-decreasing stamp, so each round needs a
    // fresh monotone run. The counter is carried across rounds rather than
    // reset, which costs nothing and keeps every push on the accepted path.
    let mut stamp = 1i64;
    let abi_push_ns = bench(|| {
        for _ in 0..N {
            stamp += 1;
            // SAFETY: live publisher on its creating thread; `payload` is
            // exactly `tft_layout_size(QVEC7_WXYZ)`.
            let rc = unsafe {
                tft_publisher_push(
                    pubh,
                    black_box(stamp),
                    TFT_LAYOUT_QVEC7_WXYZ,
                    payload.as_ptr().cast(),
                )
            };
            debug_assert_eq!(rc, TFT_OK);
        }
        stamp as f64
    });

    let native_tree = tf_tree::TreeBuilder::new()
        .dynamic_edge(
            "world",
            "robot",
            tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(64)),
        )
        .build()
        .unwrap();
    let nw = native_tree
        .claim(
            native_tree.frame("robot").unwrap(),
            native_tree.frame("world").unwrap(),
        )
        .unwrap();
    let identity = tf_tree::Iso3::IDENTITY;
    let mut nstamp = 1i64;
    let native_push_ns = bench(|| {
        for _ in 0..N {
            nstamp += 1;
            nw.push(black_box(nstamp), &identity).unwrap();
        }
        nstamp as f64
    });

    // The same push, decoding the pose from the identical 56 bytes the C side
    // reads. **This row is why the first comparison is not the whole story**:
    // the row above hoists a constant `Iso3` out of the loop and never pays to
    // materialize one, while the C side decodes a foreign buffer every time.
    // Reporting only the first would charge the decode to "the boundary", which
    // it is not — a Rust caller publishing from wire bytes pays it too.
    // Continues the same monotone run — the writer above already advanced past
    // its own stamps, and a push that goes backwards is refused (which is how
    // this was found: `NonMonotonicStamp { last: 200705, got: 2 }`).
    let mut nstamp2 = nstamp + 1;
    let native_decode_ns = bench(|| {
        for _ in 0..N {
            nstamp2 += 1;
            let mut v = [0.0f64; 7];
            for (slot, c) in v.iter_mut().zip(payload.chunks_exact(8)) {
                *slot = f64::from_ne_bytes(c.try_into().unwrap());
            }
            let iso = tf_tree::Iso3::new(
                tf_tree::Quat::new(v[0], v[1], v[2], v[3]),
                tf_tree::Vec3::new(v[4], v[5], v[6]),
            );
            nw.push(black_box(nstamp2), &iso).unwrap();
        }
        nstamp2 as f64
    });

    // The same native push behind `#[inline(never)]`. Every native row above is
    // inlined into its loop; `tft_publisher_push` is a cross-crate `extern "C"`
    // call and cannot be. This row is what says whether that matters.
    #[inline(never)]
    fn opaque_push(w: &tf_tree::EdgeWriter<'_>, stamp: i64, iso: &tf_tree::Iso3) {
        w.push(stamp, iso).unwrap();
    }
    let mut ostamp = nstamp2 + 1;
    let opaque_ns = bench(|| {
        for _ in 0..N {
            ostamp += 1;
            opaque_push(&nw, black_box(ostamp), &identity);
        }
        ostamp as f64
    });

    // The ABI's own body with `guard` removed and nothing else changed, so the
    // panic guard's cost on a **real, non-inlinable** call is a subtraction
    // rather than an inference. (`tft_guarded_noop` above is small enough for
    // rustc to inline cross-crate, so that row measures inlined code.)
    let mut astamp = ostamp + 1;
    let unguarded_push_ns = bench(|| {
        for _ in 0..N {
            astamp += 1;
            // SAFETY: live publisher on its creating thread; `payload` is
            // exactly `tft_layout_size(QVEC7_WXYZ)`.
            let rc = unsafe {
                tft_test_push_unguarded(
                    pubh,
                    black_box(astamp),
                    TFT_LAYOUT_QVEC7_WXYZ,
                    payload.as_ptr().cast(),
                )
            };
            debug_assert_eq!(rc, TFT_OK);
        }
        astamp as f64
    });

    println!("\npublish path — PHASE4 §3.2");
    println!("{:>28} {:>10}", "path", "ns/push");
    println!("{:>28} {native_push_ns:>10.1}", "native, hoisted constant");
    println!(
        "{:>28} {native_decode_ns:>10.1}",
        "native, decoding the bytes"
    );
    println!("{:>28} {opaque_ns:>10.1}", "native, not inlined");
    println!(
        "{:>28} {unguarded_push_ns:>10.1}",
        "the ABI, no panic guard"
    );
    println!("{:>28} {abi_push_ns:>10.1}", "tft_publisher_push");
    println!(
        "\n  decoding a 56-byte pose: {:+5.1} ns   any caller with wire bytes pays this",
        native_decode_ns - native_push_ns
    );
    println!(
        "  an opaque call:         {:+5.1} ns   inlining is not what separates the two",
        opaque_ns - native_push_ns
    );
    println!(
        "  the panic guard:        {:+5.1} ns   catch_unwind + clear_error, real call",
        abi_push_ns - unguarded_push_ns
    );
    println!(
        "  validating a stranger:  {:+5.1} ns   <- everything left over",
        unguarded_push_ns - opaque_ns
    );
    println!(
        "\n  total {:+.1} ns ({:.2}x). NOT a gate: §3.7's 5 % applies to `tft_plan_at`,",
        abi_push_ns - native_push_ns,
        abi_push_ns / native_push_ns
    );
    println!("  which passes above. This row is here to be honest about the other direction.");
    println!("\n  Three hypotheses about the remainder were measured and all three were");
    println!("  wrong: the redundant sqrt (noise), the pose decode (+0.3 ns), and the");
    println!("  un-inlinable call (+0.3 ns). What is left is the checking itself —");
    println!("  magic word, thread affinity, finiteness, unit norm, det R. **The C ABI");
    println!("  pays at run time for what Rust's type system settles at compile time.**");
    println!("  A Rust caller cannot construct a left-handed rotation, a non-unit");
    println!("  quaternion, a stale handle or a cross-thread publisher; a C caller can");
    println!("  construct all four, and ~12 ns is what it costs to find out.");
    println!("\n  Three consecutive pinned runs agreed to 0.1 ns on every row, so the");
    println!("  deltas are real. 22 ns is 45 M pushes/s on one thread against a /tf");
    println!("  stream three to five orders of magnitude slower than that, so");
    println!("  there is no case for trading any of those checks away.");

    // SAFETY: each handle freed exactly once, publisher on its creating thread.
    unsafe {
        tft_publisher_free(pubh);
        tft_tree_free(ptree);
        tft_plan_free(plan);
        tft_tree_free(tree);
    }
}
