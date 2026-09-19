//! What one `tft_bridge_offer` costs (`docs/PHASE4.md` §7, bridge row). Not a gate.
//!
//! Measures a steady-state accepted `/tf` offer through §5.4-§5.8 and the arena
//! write, over `EDGES` edges (every §5 table is keyed on `(parent, child)`).
//! Allocation count is gated by `crates/tf_tree_bridge/tests/steady_state_alloc.rs`.
//!
//! Run pinned:
//! `taskset -c 2 cargo run --release -p tf_tree_c --features bridge --example bridge_cost`
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::expect_used)]
// 0007 rule 1, kind 5 (our own C ABI, called from Rust); 0048: an example is a
// separate crate root, so the posture is declared here.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use core::ptr;
use std::ffi::CString;
use std::hint::black_box;
use std::time::Instant;

use tf_tree_c::bridge::*;
use tf_tree_c::*;

/// §7's row is "1 kHz x 20 edges".
const EDGES: usize = 20;
/// Offers per round; a multiple of `EDGES`.
const N: usize = 20_000;
const ROUNDS: usize = 7;

/// A dynamic chain `link0 -> … -> link20` (a star would share one parent name).
fn topology() -> String {
    let mut s = String::new();
    for i in 0..EDGES {
        s.push_str(&format!(
            "[[edge]]\nparent = \"link{i}\"\nchild = \"link{}\"\nkind = \"dynamic\"\ncapacity = 256\n\n",
            i + 1
        ));
    }
    s
}

/// The minimum of `ROUNDS` rounds, in ns per offer (noise only adds time).
fn bench(mut run: impl FnMut() -> u64) -> f64 {
    for _ in 0..2 {
        black_box(run());
    }
    (0..ROUNDS)
        .map(|_| {
            let t0 = Instant::now();
            let accepted = black_box(run());
            assert_eq!(accepted, N as u64, "every offer must have been accepted");
            t0.elapsed().as_nanos() as f64 / N as f64
        })
        .fold(f64::INFINITY, f64::min)
}

fn main() {
    let toml = CString::new(topology()).unwrap();
    let opts = tft_bridge_options {
        struct_size: core::mem::size_of::<tft_bridge_options>() as u32,
        authority: TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
        on_clock_reset: TFT_BRIDGE_ON_CLOCK_RESET_HALT,
        domain: 0,
        tf_prefix: ptr::null(),
        // Private heap arena: no rendezvous in the number.
        arena_name: ptr::null(),
    };
    let mut b: *mut tft_bridge = ptr::null_mut();
    // SAFETY: NUL-terminated config, a live `opts`, `b` a live local.
    let rc = unsafe { tft_bridge_create(toml.as_ptr(), &opts, &mut b) };
    assert_eq!(rc, TFT_OK, "tft_bridge_create failed: {rc}");

    let gid = [0x5Au8; 16];
    let node = CString::new("/ekf").unwrap();
    assert_eq!(
        // SAFETY: live handle on this thread, 16 readable bytes, NUL-terminated.
        unsafe { tft_bridge_attribute(b, gid.as_ptr(), node.as_ptr()) },
        TFT_OK
    );

    // Built once, as `rclcpp` hands them over; per-offer would measure `CString::new`.
    let names: Vec<(CString, CString)> = (0..EDGES)
        .map(|i| {
            (
                CString::new(format!("link{i}")).unwrap(),
                CString::new(format!("link{}", i + 1)).unwrap(),
            )
        })
        .collect();

    // A 30-degree yaw, so pose validation has real components.
    let pose = [
        0.965_925_826_289_068_3,
        0.0,
        0.0,
        0.258_819_045_102_520_74,
        1.5,
        -2.25,
        0.75,
    ];

    let mut stamp: i64 = 1_000_000_000;
    // Receipt clock, read once per sweep like the ROS caller; `0` would skip the offset layer.
    let mut received = Instant::now();
    let epoch = received;
    let mut out = tft_bridge_outcome {
        struct_size: core::mem::size_of::<tft_bridge_outcome>() as u32,
        action: 0,
        reason: 0,
        status: 0,
        first_time: 0,
        by_nanos: 0,
        parent: ptr::null(),
        child: ptr::null(),
        owner: ptr::null(),
        intruder: ptr::null(),
        existing: [0.0; 7],
        offered: [0.0; 7],
        detail: ptr::null(),
        delta_nanos: 0,
        clock_evidence: TFT_BRIDGE_EVIDENCE_NONE,
        clock_evidence_detail: 0,
    };

    let ns = bench(|| {
        let mut accepted = 0u64;
        for k in 0..N {
            let (p, c) = &names[k % EDGES];
            // One sweep shares a stamp, as a batched `TFMessage` does.
            if k % EDGES == 0 {
                stamp += 1_000_000;
                received = Instant::now();
            }
            let s = tft_bridge_sample {
                struct_size: core::mem::size_of::<tft_bridge_sample>() as u32,
                frame_id: p.as_ptr(),
                child_frame_id: c.as_ptr(),
                stamp_nanos: black_box(stamp),
                pose,
                received_steady_nanos: i64::try_from(
                    received.saturating_duration_since(epoch).as_nanos(),
                )
                .unwrap_or(i64::MAX)
                .saturating_add(1),
            };
            out.struct_size = core::mem::size_of::<tft_bridge_outcome>() as u32;
            // SAFETY: live handle on its creating thread, a live sample whose
            // name pointers are NUL-terminated, 16 readable GID bytes, `out` a
            // live local with `struct_size` set.
            let rc =
                unsafe { tft_bridge_offer(b, TFT_BRIDGE_TOPIC_TF, &s, gid.as_ptr(), &mut out) };
            assert_eq!(rc, TFT_OK);
            if out.action == TFT_BRIDGE_APPLIED {
                accepted += 1;
            }
        }
        accepted
    });

    println!("tft_bridge_offer — {EDGES} dynamic edges, {N} accepted offers/round");
    println!("min of {ROUNDS} rounds: {ns:.1} ns per accepted transform");

    let mut stats = tft_bridge_stats::blank();
    // SAFETY: live handle on its creating thread; `stats` is a live local with
    // `struct_size` set.
    assert_eq!(unsafe { tft_bridge_get_stats(b, &mut stats) }, TFT_OK);
    println!(
        "ledger: {} offered, {} applied, 0 dropped: {}",
        stats.transforms,
        stats.applied,
        stats.transforms == stats.applied
    );

    // SAFETY: created above, freed exactly once, on the creating thread.
    unsafe { tft_bridge_free(b) };
}
