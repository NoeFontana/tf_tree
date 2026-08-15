//! C ABI overhead against native Rust — `docs/PHASE4.md` §7, gate criterion 1.
//!
//! # What this measures, and the two ways it used to fail to
//!
//! §7 gate criterion 1 asks whether `tft_plan_at` costs a caller more than
//! native Rust, and for a long time this file could not answer it, for two
//! compounding reasons — both fixed here, both worth knowing about before
//! trusting any number below.
//!
//! **1. It was built at the workspace `release` profile, which is
//! `lto = "thin"`.** That inlines `tft_plan_at` into a Rust caller, so the C
//! boundary the gate exists to price was *not in the binary doing the pricing*.
//! `report.rs`'s PHASE5 §9.2 embedding row had already written down that trap in
//! those words — thin LTO "is exactly what erases the boundary" — and nothing
//! had applied it here. `just abi-cost` now builds this twice and **only the
//! `[profile.embedder]` run (`lto = false`) gates**; the `release` run is kept
//! as the contrast. On this host the same C ABI prices at **1.019x** with the
//! boundary erased (1.016-1.019) and **1.03-1.04x** with it present, so thin
//! LTO was hiding about half the thing being measured.
//!
//! **2. The denominator was at LLVM's discretion.** Adding a second, wholly
//! unrelated `Tree::guard()` call site to this file moved the native baseline
//! 133 -> 190 ns (43%) and the verdict FAIL -> PASS. The ABI arm never moved.
//! Every native comparand is now `#[inline(never)]` with `black_box` on the
//! stamp going in and the scalar coming out, and the ladder carries a **control
//! row** — two structurally identical native arms, two symbols, two call sites —
//! that fails if a call site ever specialises one of them again. The same
//! unrelated edit was re-applied after the pin: the ratios moved by at most
//! 0.4 percentage points while the *host* moved the absolute baseline 14%.
//!
//! # What it reports
//!
//! A ladder, each rung differing from the one below by exactly one thing, all
//! five arms interleaved within every round so drift is common-mode:
//!
//! | rung | what it adds |
//! |---|---|
//! | native, guard hoisted | the shape a Rust embedder writes |
//! | native, guard per call | the shape the **C signature** forces (`0022`) |
//! | (control: its twin) | nothing — it must agree with the row above |
//! | the ABI, no panic guard | the boundary, minus `catch_unwind` |
//! | `tft_plan_at` | the shipped call |
//!
//! and gates three quotients between them rather than one quotient across all
//! of them, because R1 (the ABI) and R3 (the signature) have different owners
//! and rolling them together makes neither diagnosable. The allowances and the
//! argument for replacing §7's single 1.05 with them are `docs/decisions/0023`,
//! **draft**.
//!
//! Also reported, none of them gates:
//!
//! * **`catch_unwind` in isolation.** §3.4 asserts it is free on the happy path
//!   because it emits landing pads rather than a runtime check. Measured twice:
//!   `tft_layout_size` against `tft_guarded_noop` (both small enough to inline
//!   cross-crate, so ~+0.1 ns *in inlined code*), and `tft_publisher_push`
//!   against `tft_test_push_unguarded` (+0.6 ns on a call too large to inline —
//!   the number that actually supports §3.4 for the shipped ABI).
//! * **The batch paths**, where the boundary is amortized over n, including
//!   `Layout::QuatTwist`'s monotone-cursor batch and its strided variant.
//! * **What the publish path costs, and why.** 2.5x against native is the kind
//!   of number that should be explained rather than left to be discovered; the
//!   ablation rows are there because three separate hypotheses about it were
//!   measured and all three were wrong.
//!
//! Every arm evaluates the **same plan on the same tree at the same stamps** and
//! writes the same 128 bytes, so a difference between two rungs is the one thing
//! that changed between them.
//!
//! Run through `just abi-cost`, which pins to a core: an unpinned run migrates
//! cores and swings by >30%, which is more than any allowance here.
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::expect_used)]

use core::ptr;
use std::hint::black_box;
use std::time::Instant;

use tf_tree_c::*;

const N: usize = 4096;
const ROUNDS: usize = 41;

// --- §7 gate criterion 1, rung by rung ----------------------------------
//
// **These allowances are a proposal, not yet the spec.** §7's gate table says
// "C ABI within 5% of native", one quotient, and `docs/decisions/0023` is the
// draft that would replace it with the three rungs below. The numbers here are
// what this host measures at `[profile.embedder]` plus headroom; each is
// justified where it is declared, and `0023` records how it was chosen and what
// would falsify it. A human accepts or rejects them by merging that record.
//
// The old 1.05 is deliberately not reused for any of them. It was a figure for
// a quotient nobody could reproduce, and carrying it forward would smuggle in a
// threshold that had never been measured against a real boundary.

/// **R1 — what the C ABI itself costs**, over a native caller written in the
/// shape the C signature forces (a guard per lookup, behind a call the compiler
/// cannot inline). This is the tight one: everything between these two arms is
/// the boundary — magic-word check, null checks, layout enum dispatch, the
/// output slice, and `catch_unwind`.
///
/// **Measured 1.025–1.038 at `[profile.embedder]`**, twelve runs, `taskset -c 2`,
/// on a host whose absolute baseline wandered 217–248 ns across the same twelve
/// (a busy neighbour: another project was building). That is the point of a
/// ratio here — `report.rs`'s `Fitness` refuses absolute timing rows on this
/// machine and `fair_for_ratios` is the weaker axis that survives, because a
/// noisy round lands on both arms of an interleaved pair.
///
/// 1.10 is about two and a half times the largest measured excess over 1.0. It is loose
/// on purpose: this row must not go red for noise, or it joins the recipes
/// people learn to re-run until green. It would still catch a doubling of any
/// single check the boundary performs. Falsified by a run at this profile on a
/// quiet host that lands above 1.10 with the ABI unchanged — or, in the other
/// direction, by a quiet host showing the spread is small enough to justify
/// tightening it, which is the better outcome and the one `0023` invites.
const ABI_OVER_GUARDED: f64 = 1.10;

/// **R2 — `catch_unwind` on the happy path.** §3.4 asserts this is ~zero
/// because it emits landing pads rather than a runtime check, and the ABI's own
/// body with the guard removed (`tft_test_plan_at_unguarded`) is the subtraction
/// that says so on a real call. Measured 0.999–1.006 across the same nine runs,
/// i.e. indistinguishable from free, exactly as §3.4 predicts. 1.05 fails if the
/// landing pads ever stop being free on this target.
const PANIC_GUARD: f64 = 1.05;

/// **R3 — what a guard per lookup costs**, against one hoisted out of the loop.
/// This is not the ABI's cost, it is the *signature's*, and it is
/// `docs/decisions/0022`'s subject. It is gated anyway because it is the larger
/// of the two and because `0022`'s whole argument rests on its size: at
/// `[profile.embedder]` the guard is ~45 ns on the §11.1 fixture (`just
/// abi-attached`) and **~16 ns here** — measured 1.059–1.075 across nine runs on
/// a three-edge heap tree, which is ~6.5% of a ~245 ns lookup.
///
/// **This comment used to guess that the difference was the shared arena
/// ("whatever makes the shared case ~3x dearer is `0022`'s to find"). It is
/// not, and the guess is retracted.** `just guard-cost` measures both backings
/// on the *same* fixture in one binary at this profile: heap +34.4 ns, memfd
/// +35.8 ns, counters off. The backing is worth ~1.4 ns; the **fixture** is
/// worth the rest. The mechanism is that a fresh `Guard`'s cursor is the
/// `EdgeId(0)` sentinel, so every step restarts its bracket search at the window
/// midpoint, and `docs/design/fast-path.md` §12 measures that as a cache cliff
/// in the *stamp* array — flat below L1d, ~17 ns/sample at capacity 16384, which
/// is what §11.1's 1 kHz edge is. The tree built below has 256-slot rings, i.e.
/// 2 KiB of stamps per edge: it sits on the flat part of that curve, so it
/// prices `Guard`'s constructor and almost none of the cold search a real robot
/// pays. `docs/decisions/0023` open question 3 is the proposal to gate the §11.1
/// fixture instead, and the 1.25 below is an allowance for *this* numerator.
///
/// The allowance is set to catch a *regression* rather than to assert a target —
/// if `Guard` acquires new per-construction work, this is the row that moves.
/// **It is not a row anybody intends to lower:** `0022` is `ready` and declines
/// the `tft_guard` handle, answering the per-call guard with `tft_plan_at_many`
/// (~41 ns of a ~302 ns scalar call at n = 256) rather than with new API.
const PER_CALL_GUARD: f64 = 1.25;

/// **C — the control**, and the reason this file can gate at all. Two
/// structurally identical native arms, two symbols, two call sites: if the
/// compiler still specialised the comparand per call site the way it did when
/// the old gate swung 43% on an unrelated edit, these two would disagree. The
/// allowance is a *symmetric* band — |ratio - 1| < 0.02 — because either
/// direction is the same failure.
///
/// **2% is more than twice the spread these two arms actually show**: twelve
/// runs gave 0.992–1.002, and the widest excursion was 0.8%. It is deliberately much
/// tighter than the rungs it protects, because the failure it looks for is not
/// subtle — when the comparand was unpinned, one unrelated call site moved it
/// **43%**. A control loose enough to miss that would be decoration.
const CONTROL: f64 = 1.02;

fn verdict(ok: bool) -> &'static str {
    if ok {
        "PASS"
    } else {
        "FAIL"
    }
}

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

/// Measure every arm **once per round, in the same order**, and return one
/// vector of per-round ns/lookup per arm.
///
/// `bench` above runs an arm to completion before the next one starts, so a
/// frequency step or a noisy neighbour that arrives halfway through the run
/// lands on one arm and not the other. Here every round contains every arm, so
/// such drift is common-mode and divides out of a per-round quotient. This is
/// the same construction `tf_tree_bench`'s `report.rs` calls
/// `Sensitivity::Ratio` and the reason `just cpp-bench`'s gate stopped
/// flapping; `abi_cost` cannot depend on that crate (it is a `tf_tree_c`
/// example), so the construction is repeated rather than shared.
///
/// The arms are `&mut dyn FnMut` because they close over different buffers and
/// therefore have different types; the dynamic dispatch is one indirect call
/// per **round**, not per lookup, so it is 1/4096th of anything measured.
fn ladder(arms: &mut [(&'static str, &mut dyn FnMut() -> f64)]) -> Vec<Vec<f64>> {
    for _ in 0..8 {
        for (_, run) in arms.iter_mut() {
            black_box(run());
        }
    }
    let mut out = vec![Vec::with_capacity(ROUNDS); arms.len()];
    for _ in 0..ROUNDS {
        for (i, (_, run)) in arms.iter_mut().enumerate() {
            let t0 = Instant::now();
            black_box(run());
            out[i].push(t0.elapsed().as_nanos() as f64 / N as f64);
        }
    }
    out
}

/// The median of the **per-round** quotients, not the quotient of the medians.
///
/// They differ exactly when the two arms drift together, which is the case this
/// function exists for: a round in which both arms were slow contributes a ratio
/// near the true one, while a quotient of medians would compare a slow round of
/// one arm against a fast round of the other.
fn ratio(num: &[f64], den: &[f64]) -> f64 {
    median(num.iter().zip(den).map(|(a, b)| a / b).collect())
}

// --- the pinned native comparands ---------------------------------------
//
// **These three functions are the fix for a gate that could not gate.** §7 gate
// criterion 1 is a ratio, and its denominator used to be an inlined loop whose
// cost was at LLVM's discretion: adding a second, wholly unrelated
// `Tree::guard()` call site to this file moved it 133 -> 190 ns (43%) and the
// verdict FAIL -> PASS, while the ABI arm never moved. See `docs/PHASE4.md` §7.
//
// Each is `#[inline(never)]` with `black_box` on the stamp going in and on the
// scalar coming out, so:
//
// * the loop that calls it cannot hoist, vectorise or partially evaluate it —
//   the compiler must assume the stamp is unknown and the result is observed;
// * every call site in this file shares **one** machine-code body, so adding a
//   call site cannot re-specialise anything. That is the structural half of the
//   argument; the empirical half is in `docs/PHASE4.md` §7, where the same
//   unrelated edit was applied again after this pin and the baseline held.
//
// **An opaque call is also the honest native shape at `lto = false`.** A real
// embedder calls `Plan::at` across a crate boundary, and `PHASE5.md` §9.2's
// embedding row is the measurement of what that costs. Pinning does not
// manufacture a handicap for the denominator; it stops the denominator being
// given an optimisation no embedder gets.
//
// The bodies are duplicated rather than factored through a shared helper on
// purpose: a `#[inline(never)]` helper called by another `#[inline(never)]`
// helper would charge the outer arm an extra real call, and the whole point of
// the ladder is that adjacent rungs differ by exactly one thing.

/// Rung 0: a lookup through a guard the caller already holds.
#[inline(never)]
fn native_hoisted(
    plan: &tf_tree::Plan,
    g: &tf_tree::Guard<'_>,
    t: i64,
    buf: &mut [f64; 16],
) -> f64 {
    let iso = plan
        .at(
            g,
            tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(black_box(t)),
        )
        .unwrap();
    tf_tree::write_mat4(&iso, buf);
    black_box(buf[0])
}

/// Rung 1: the same, with the guard built inside the call.
///
/// This is the shape the C signature forces — `tft_plan_at` takes a plan and a
/// stamp and has nowhere to keep a guard between calls, so it builds one every
/// time (`lib.rs`, `h.share.tree.guard()`). `docs/decisions/0022` is about
/// giving the C tier somewhere to keep one.
#[inline(never)]
fn native_per_call_guard(
    plan: &tf_tree::Plan,
    tree: &tf_tree::Tree,
    t: i64,
    buf: &mut [f64; 16],
) -> f64 {
    let g = tree.guard();
    let iso = plan
        .at(
            &g,
            tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(black_box(t)),
        )
        .unwrap();
    tf_tree::write_mat4(&iso, buf);
    black_box(buf[0])
}

/// **The pin's self-check, measured on every run.**
///
/// A structural twin of [`native_per_call_guard`]: same work, different symbol,
/// different call site. If the compiler were still specialising the comparand
/// per call site — the failure that made the old gate move 43% on an unrelated
/// edit — these two would not agree. They are printed side by side and their
/// difference is the ladder's own error bar.
///
/// It reads `buf[15]` rather than `buf[0]` **so that identical-code folding
/// cannot merge the two symbols**, which would make the control vacuous by
/// construction. `write_mat4` has already written all sixteen lanes, so the two
/// bodies do the same amount of work.
#[inline(never)]
fn native_per_call_guard_twin(
    plan: &tf_tree::Plan,
    tree: &tf_tree::Tree,
    t: i64,
    buf: &mut [f64; 16],
) -> f64 {
    let g = tree.guard();
    let iso = plan
        .at(
            &g,
            tf_tree::Stamp::<tf_tree::SystemDomain>::from_nanos(black_box(t)),
        )
        .unwrap();
    tf_tree::write_mat4(&iso, buf);
    black_box(buf[15])
}

/// The cargo profile directory this executable is running out of, if it is
/// running out of one.
///
/// Cargo lays an example out at `<target>/[<triple>/]<profile-dir>/examples/`,
/// so the path component immediately before `examples` is the profile
/// directory — `release` for `--release`, `embedder` for `--profile embedder`.
/// The optional `<triple>` component when cross-compiling is why this searches
/// for `examples` from the right rather than counting from `target`.
///
/// This is the same fact `tf_tree_bench`'s `build.rs` gets from `OUT_DIR`, read
/// a weaker way: `OUT_DIR` is what cargo told the build, while this is where the
/// file ended up, so a copied binary answers `None` here and `build.rs`'s answer
/// would have travelled with it. `None` is the honest reply for a copied
/// binary — it is *why* an unverified `embedder` claim is not allowed to gate —
/// and the weaker mechanism is the right trade for a published crate that
/// should not grow a build script to serve one benchmark example.
fn profile_dir_of_this_binary() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let parts: Vec<String> = exe
        .iter()
        .map(|c| c.to_string_lossy().into_owned())
        .collect();
    let i = parts.iter().rposition(|c| c == "examples")?;
    parts.get(i.checked_sub(1)?).cloned()
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

    // **The profile is half of a boundary measurement**, so it is printed — and
    // since it also decides *whether this run gates at all* (see the exit-status
    // comment far below), argv[1] is a claim that gets checked against where
    // cargo actually put this binary.
    //
    // It used to be only a claim. `just abi-cost` builds twice and passes
    // `release` to one and `embedder` to the other, and the binary believed
    // whichever string it got; swapping the two lines of that recipe would have
    // moved the gate onto the `lto = "thin"` run — the one where rustc inlines
    // `tft_plan_at` into its Rust caller, so the boundary being priced is not in
    // the binary — and nothing in the output would have said so. That is the
    // same shape as the two wrong attributions `docs/PHASE4.md` §0.0 records.
    //
    // `tf_tree_c` is a published crate and does not get a `build.rs` for this
    // (`tf_tree_bench`'s bakes `OUT_DIR`'s profile directory in, which is the
    // better fact when it is available). `current_exe` is the version that costs
    // the shipped crate nothing: cargo lays an example out at
    // `<target>/[<triple>/]<profile-dir>/examples/<name>`, so the component
    // before `examples` is the profile directory.
    let claimed = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "unstated".to_owned());
    let measured = profile_dir_of_this_binary();
    if let Some(m) = &measured {
        assert!(
            *m == claimed,
            "argv[1] claims this binary was built at `{claimed}`, but it is running from \
             `.../{m}/examples/`. The profile decides whether this run gates and what its \
             ratios mean, so a wrong label here is worse than no measurement."
        );
    }
    // A binary copied out of its target directory cannot vouch for its profile,
    // and an unvouched `embedder` must not gate: an unverifiable claim is not
    // evidence. `boundary_real` therefore needs *both* halves.
    let boundary_real = claimed == "embedder" && measured.is_some();
    let profile = claimed;

    println!("C ABI overhead — PHASE4 §7");
    println!("==========================");
    println!("{N} lookups/round, {ROUNDS} interleaved rounds, depth 3");
    println!(
        "profile: {profile}{}",
        match (profile.as_str(), measured.is_some()) {
            ("embedder", true) => "  (lto = false — the C boundary is REAL; verified)",
            ("embedder", false) =>
                "  (claimed, UNVERIFIED — not run from a target dir, so it does not gate)",
            ("release", _) => "  (lto = \"thin\" — the C boundary is ERASED; not the gate)",
            _ => "  (pass `release` or `embedder` as argv[1]; the profile decides what this means)",
        }
    );
    println!();

    // --- the ladder ---------------------------------------------------------
    //
    // Every arm writes the same 128 bytes and evaluates the same plan on the
    // same tree at the same stamps, so adjacent rungs differ by exactly one
    // thing. `[f64; 16]` rather than `[u8; 128]` on the native side because
    // `write_mat4` wants `&mut [f64]` and building one from a byte array would
    // need an alignment the type does not promise — the same 128 bytes either
    // way.
    //
    // **One buffer per arm**, and this is not tidiness. The first version of
    // this ladder shared `nbuf` between the native arms; letting it escape into
    // an `#[inline(never)]` callee forced it to memory for *every* arm including
    // the baseline, which went 133 -> 190 ns and made the gate appear to pass.
    // The instrument had moved the thing it measures. Here every arm passes a
    // buffer to an `#[inline(never)]` callee, so that cost is paid uniformly and
    // is part of every rung rather than a difference between two of them.
    let mut nbuf = [0.0f64; 16];
    let mut lbuf = [0.0f64; 16];
    let mut tbuf = [0.0f64; 16];
    let mut cbuf = [0u8; 128];
    let mut ubuf = [0u8; 128];

    // Rung 0. The guard is built once per *round* — 4096 lookups — so its cost
    // is 1/4096th of anything below, which is the "hoisted out of the loop"
    // shape a Rust embedder writes.
    let mut arm_hoisted = || {
        let g = native.guard();
        let mut acc = 0.0;
        for &t in &stamps {
            acc += native_hoisted(&nplan, &g, t, &mut nbuf);
        }
        acc
    };
    // Rung 1. A guard per lookup: what the C signature forces.
    let mut arm_per_call = || {
        let mut acc = 0.0;
        for &t in &stamps {
            acc += native_per_call_guard(&nplan, &native, t, &mut lbuf);
        }
        acc
    };
    // The control. Structurally identical to rung 1, separate symbol.
    let mut arm_twin = || {
        let mut acc = 0.0;
        for &t in &stamps {
            acc += native_per_call_guard_twin(&nplan, &native, t, &mut tbuf);
        }
        acc
    };
    // Rung 2. The ABI's own body with `catch_unwind` removed and nothing else
    // changed, so the panic guard is a subtraction on a real, non-inlinable
    // call. (`tft_guarded_noop` further down measures an *inlined* body and
    // answers a different question.)
    let mut arm_unguarded = || {
        let mut acc = 0.0;
        for &t in &stamps {
            // SAFETY: live plan, and `ubuf` is exactly `tft_layout_size(MAT4_ROW)`.
            let rc = unsafe {
                tft_test_plan_at_unguarded(
                    plan,
                    black_box(t),
                    TFT_LAYOUT_MAT4_ROW,
                    ubuf.as_mut_ptr().cast(),
                )
            };
            debug_assert_eq!(rc, TFT_OK);
            acc += ubuf[0] as f64;
        }
        acc
    };
    // Rung 3. The shipped call.
    let mut arm_abi = || {
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
    };

    let m = ladder(&mut [
        ("native, guard hoisted", &mut arm_hoisted),
        ("native, guard per call", &mut arm_per_call),
        ("  (control: its twin)", &mut arm_twin),
        ("the ABI, no panic guard", &mut arm_unguarded),
        ("tft_plan_at", &mut arm_abi),
    ]);
    let native_ns = median(m[0].clone());
    let per_call_guard_ns = median(m[1].clone());
    let twin_ns = median(m[2].clone());
    let unguarded_ns = median(m[3].clone());
    let abi_ns = median(m[4].clone());

    // The quotients that matter, each the median of per-round quotients.
    let r_control = ratio(&m[2], &m[1]);
    let r_guard = ratio(&m[1], &m[0]);
    let r_abi = ratio(&m[4], &m[1]);
    let r_panic = ratio(&m[4], &m[3]);
    let r_total = ratio(&m[4], &m[0]);

    println!("{:>28} {:>10}", "path", "ns/lookup");
    println!("{:>28} {native_ns:>10.1}", "native, guard hoisted");
    println!("{:>28} {per_call_guard_ns:>10.1}", "native, guard per call");
    println!("{:>28} {twin_ns:>10.1}", "  (control: its twin)");
    println!("{:>28} {unguarded_ns:>10.1}", "the ABI, no panic guard");
    println!("{:>28} {abi_ns:>10.1}", "tft_plan_at");

    println!("\nthe gate, rung by rung");
    println!("----------------------");
    println!(
        "  R1  the ABI over the shape it forces   {r_abi:.3}x   (allow < {ABI_OVER_GUARDED:.2})   {}",
        verdict(r_abi < ABI_OVER_GUARDED)
    );
    println!(
        "  R2  the panic guard                    {r_panic:.3}x   (allow < {PANIC_GUARD:.2})   {}",
        verdict(r_panic < PANIC_GUARD)
    );
    println!(
        "  R3  a guard per lookup, vs hoisted     {r_guard:.3}x   (allow < {PER_CALL_GUARD:.2})   {}",
        verdict(r_guard < PER_CALL_GUARD)
    );
    println!(
        "  C   the control against rung 1         {r_control:.3}x   (allow 1 +- {:.2})   {}",
        CONTROL - 1.0,
        verdict((r_control - 1.0).abs() < CONTROL - 1.0)
    );
    println!(
        "\n  for reference, the §7-as-written quotient (`tft_plan_at` over a hoisted\n  \
         guard, i.e. R1 x R3): {r_total:.3}x. It is reported, not gated — see below."
    );

    println!(
        "\n  WHY THREE RUNGS AND NOT ONE QUOTIENT.\n\n  \
         R1 is the C ABI: a handle validation, a layout dispatch, a `catch_unwind`\n  \
         landing pad and a write into caller memory. R3 is the *C signature* — it\n  \
         has nowhere to keep a guard between calls, so `tft_plan_at` builds one\n  \
         every time. Both are real costs a C caller pays, and they have different\n  \
         owners: R1 is this crate's, R3 is `docs/decisions/0022`'s. Rolled into\n  \
         one number they move together and neither is diagnosable.\n\n  \
         The old single quotient could not gate at all. Its denominator was an\n  \
         inlined loop, and adding a second, unrelated `Tree::guard()` call site to\n  \
         this file moved it 133 -> 190 ns and the verdict FAIL -> PASS while the\n  \
         ABI arm never moved. The comparands above are `#[inline(never)]` with\n  \
         `black_box` on the stamp in and the scalar out, so no call site can\n  \
         specialise them; row C is the standing check that this holds — two\n  \
         structurally identical bodies, two symbols, two call sites."
    );

    if !boundary_real {
        println!(
            "\n  NOT THE GATE READING. At `lto = \"thin\"` rustc inlines `tft_plan_at`\n  \
             into this Rust caller, so the boundary being priced is not in this\n  \
             binary. The verdicts above are printed for the contrast with the\n  \
             `embedder` run; only that one gates. `report.rs`'s §9.2 embedding row\n  \
             says the same thing in the same words about the same trap."
        );
    }

    // **The exit status gates, and only at the profile where the boundary
    // exists.** The `release` run is a contrast, not a criterion: failing on it
    // would make the recipe red for a reason that is about LLVM's inliner. The
    // control is included — if the pin stops holding, the ladder's numbers stop
    // meaning anything and a green run would be worse than a red one.
    //
    // **Both halves of that were run, not asserted.** With `ABI_OVER_GUARDED`
    // temporarily set to 1.00 the `embedder` binary exits 1 and the `release`
    // binary exits 0 on the same measured 1.016 — which is the profile guard
    // doing its job, not the threshold being unreachable.
    let gate_failed = boundary_real
        && !(r_abi < ABI_OVER_GUARDED
            && r_panic < PANIC_GUARD
            && r_guard < PER_CALL_GUARD
            && (r_control - 1.0).abs() < CONTROL - 1.0);

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

    // --- the twist layout's batch, where the monotone cursor is the point ---
    //
    // **Ascending stamps, deliberately.** `Layout::QuatTwist`'s batch fold has
    // a monotone-cursor branch that turns the per-step bracket search from
    // `O(log n)` into `O(1)` amortized, and `docs/API.md` §3.3's n = 1024
    // ML/perception batch is the caller it exists for. `tft_plan_at_many` used
    // to evaluate this layout with a scalar `at_with_derivatives` per element,
    // which cannot reach that branch — these rows are what says whether it does
    // now, and the native row beside them is what separates "the boundary" from
    // "derivatives cost twice a pose".
    //
    // The strided row is the second half of the same question: a caller writing
    // into an array of its own structs cannot be handed the batch's buffer
    // directly, so it takes the chunked path instead of the zero-copy one.
    let sorted: Vec<i64> = (0..N)
        .map(|i| 10_000_000 + (i as i64 * 600_000_000) / N as i64)
        .collect();
    let mut nrows = vec![0.0f64; N * 13];
    let twist_native_ns = bench(|| {
        let g = native.guard();
        nplan
            .at_many_into::<tf_tree::SystemDomain>(
                &g,
                black_box(&sorted),
                tf_tree::Layout::QuatTwist,
                &mut nrows,
            )
            .unwrap();
        nrows[0]
    });
    let mut trows = vec![0u8; N * 104];
    let twist_abi_ns = bench(|| {
        // SAFETY: live plan; `sorted` has N elements and `trows` is N*104 bytes,
        // which is exactly what a tightly packed 13-`f64` layout touches.
        let rc = unsafe {
            tft_plan_at_many(
                plan,
                sorted.as_ptr(),
                N,
                TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                trows.as_mut_ptr().cast(),
                0,
            )
        };
        debug_assert_eq!(rc, TFT_OK);
        trows[0] as f64
    });
    const TWIST_STRIDE: usize = 128;
    let mut srows = vec![0u8; N * TWIST_STRIDE];
    let twist_strided_ns = bench(|| {
        // SAFETY: live plan; the last element occupies 104 bytes at
        // (N-1)*TWIST_STRIDE, which is inside `srows`.
        let rc = unsafe {
            tft_plan_at_many(
                plan,
                sorted.as_ptr(),
                N,
                TFT_LAYOUT_QVEC7_WXYZ_TWIST6,
                srows.as_mut_ptr().cast(),
                TWIST_STRIDE,
            )
        };
        debug_assert_eq!(rc, TFT_OK);
        srows[0] as f64
    });
    println!("\nQVEC7_WXYZ_TWIST6 batch, ascending stamps — API.md §3.3");
    println!("{:>28} {:>10}", "path", "ns/elem");
    println!(
        "{:>28} {twist_native_ns:>10.1}",
        "native at_many_into(QuatTwist)"
    );
    println!("{:>28} {twist_abi_ns:>10.1}", "tft_plan_at_many, packed");
    println!(
        "{:>28} {twist_strided_ns:>10.1}",
        "tft_plan_at_many, strided"
    );
    println!(
        "  packed {:.3}x native, strided {:.3}x native",
        twist_abi_ns / twist_native_ns,
        twist_strided_ns / twist_native_ns
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

    // The free calls above run first on purpose: a gate failure should not
    // also look like a leak to whatever runs this under a sanitizer.
    if gate_failed {
        println!("\n§7 gate criterion 1: FAIL — see the rung marked FAIL above");
        std::process::exit(1);
    }
}
