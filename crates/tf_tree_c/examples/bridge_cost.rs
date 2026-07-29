//! What one `tft_bridge_offer` costs — `docs/PHASE4.md` §7, the bridge row.
//!
//! §5.9 says the bridge *"is the one component that still pays `tf2`'s
//! deserialization cost, so it should be measured and isolated, not spread
//! across a shared executor where it will be blamed for someone else's
//! latency"*. This is the measuring half. It is not a gate — §7's gate criterion
//! 3 compares the bridge against a `tf2` consumer and needs a fair comparison
//! machine, which this host is not (§0.0) — but the per-offer cost is a number
//! the C seam owns entirely, and a regression in it is this repository's fault
//! rather than the scheduler's.
//!
//! # What is measured
//!
//! A steady-state accepted `/tf` offer: a monotonic stamp on a declared dynamic
//! edge, with an attributed publisher, going all the way through §5.6 names →
//! §5.8 declared? → §5.7 kind → §5.4 authority → §5.5 clock **and the arena
//! write**. That is the path a 1 kHz `/tf` spends essentially all of its time
//! on, so it is the one worth a number.
//!
//! `EDGES` matters: every §5 table is keyed on `(parent, child)`, so a
//! single-edge measurement reports `BTreeMap` lookups that never compare
//! anything. Twenty is §7's row and roughly a small robot.
//!
//! The allocation count on the same path is
//! `crates/tf_tree_bridge/tests/steady_state_alloc.rs`, which is a *gate*.
//!
//! Run pinned; unpinned runs migrate cores and swing by more than anything here
//! is trying to resolve:
//! `taskset -c 2 cargo run --release -p tf_tree_c --features bridge --example bridge_cost`
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::expect_used)]

use core::ptr;
use std::ffi::CString;
use std::hint::black_box;
use std::time::Instant;

use tf_tree_c::bridge::*;
use tf_tree_c::*;

/// §7's row is "1 kHz x 20 edges".
const EDGES: usize = 20;
/// Offers per round. A multiple of `EDGES` so every round is a whole number of
/// sweeps and no edge is over-represented.
const N: usize = 20_000;
const ROUNDS: usize = 7;

/// A chain `link0 -> link1 -> … -> link20`, all dynamic.
///
/// A chain rather than a star because a star gives every edge the same parent
/// name, and the `BTreeMap` comparisons on the hot path would then all resolve
/// on the child alone — which is not the shape a real `/tf` has.
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

/// The minimum of `ROUNDS` rounds, in ns per offer.
///
/// **Minimum, not median**, unlike `abi_cost.rs`: that file compares two paths
/// and wants a central tendency for a ratio. This one reports an absolute cost,
/// where every source of noise on this host adds time and none removes it, so
/// the fastest round is the closest thing to the work itself.
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
    };
    let mut b: *mut tft_bridge = ptr::null_mut();
    // SAFETY: NUL-terminated config, a live `opts`, `b` a live local.
    let rc = unsafe { tft_bridge_create(toml.as_ptr(), &opts, &mut b) };
    assert_eq!(rc, TFT_OK, "tft_bridge_create failed: {rc}");

    let gid = [0x5Au8; 16];
    let node = CString::new("/ekf").unwrap();
    // SAFETY: live handle on this thread, 16 readable bytes, NUL-terminated.
    assert_eq!(
        unsafe { tft_bridge_attribute(b, gid.as_ptr(), node.as_ptr()) },
        TFT_OK
    );

    // The names are built once, as an `rclcpp` node's `TransformStamped`s hand
    // them over: a `const char *` into a message it already owns. Building them
    // per offer would measure `CString::new`.
    let names: Vec<(CString, CString)> = (0..EDGES)
        .map(|i| {
            (
                CString::new(format!("link{i}")).unwrap(),
                CString::new(format!("link{}", i + 1)).unwrap(),
            )
        })
        .collect();

    // A 30-degree yaw, so the pose validation has real components to check
    // rather than an identity's zeros.
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
    };

    let ns = bench(|| {
        let mut accepted = 0u64;
        for k in 0..N {
            let (p, c) = &names[k % EDGES];
            // One sweep of all `EDGES` shares a stamp, then time advances —
            // which is what a `/tf` publisher batching into one `TFMessage`
            // does, and it keeps the clock guard's compare on its real branch.
            if k % EDGES == 0 {
                stamp += 1_000_000;
            }
            let s = tft_bridge_sample {
                struct_size: core::mem::size_of::<tft_bridge_sample>() as u32,
                frame_id: p.as_ptr(),
                child_frame_id: c.as_ptr(),
                stamp_nanos: black_box(stamp),
                pose,
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

    let mut stats = tft_bridge_stats {
        struct_size: core::mem::size_of::<tft_bridge_stats>() as u32,
        // SAFETY: `tft_bridge_stats` is `#[repr(C)]` and made of integers, for
        // which all-zero is a valid value.
        ..unsafe { core::mem::zeroed() }
    };
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
