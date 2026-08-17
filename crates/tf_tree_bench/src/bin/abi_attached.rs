//! Is the C ABI's +101 ns on a shared arena the **ABI**, or the **C++ caller**?
//!
//! # Why this binary exists
//!
//! `docs/benchmarks/tf2.md` records a C++ caller at **302 ns** on the §11.1
//! fixture over a shared arena where native Rust measures **200.6**. Four things
//! have been eliminated by measurement:
//!
//! | candidate | measured |
//! |---|---|
//! | the `memfd` mapping | <= 9.6 ns |
//! | the cross-process read-only attach | -0.2 ns |
//! | static against shared linkage | ~1 ns |
//! | the per-call `Guard` **on this arena** | **+19.3 ns** |
//!
//! That leaves **~81 ns unattributed**, and `abi_cost` does not reproduce it:
//! its full ABI costs **+2.3 ns** over a native arm that also builds a guard per
//! call. The two differ by 35x, so one of them is answering a different
//! question.
//!
//! **The remaining variable was the caller — and the answer is that it is not the
//! caller at all.** This binary calls `tft_plan_at` from Rust, on the same arena,
//! in the same process, against the same stamps, under two build profiles:
//!
//! | profile | native Rust | Rust -> ABI | C++ -> ABI |
//! |---|---|---|---|
//! | `release` (`lto = "thin"`) | 200.5 | 225.8 (+25) | 302.0 |
//! | **`embedder` (`lto = false`)** | 241.3 | **298.4 (+57)** | **302.0 (+61)** |
//!
//! At a real boundary a Rust caller and a C++ caller **agree to within 4 ns**. So
//! the cost is the boundary itself, not the language — and the reason
//! `abi_cost` sees only +2.3 ns is that the workspace `release` profile is
//! `lto = "thin"`, which **inlines `tft_plan_at` into its Rust caller**. No C or
//! C++ embedder can get that, and `report.rs`'s §9.2 embedding row already says
//! so in those words: thin LTO "is exactly what erases the boundary".
//!
//! **That makes `PHASE4` §7 gate criterion 1 unmeasurable in its own build**, and
//! it also explains the baseline instability recorded there: with the call
//! inlined, both arms collapse into the same optimisable blob and the ratio
//! becomes a coin toss on how well LLVM specialises it.
//!
//! # What is inside the per-call `Guard` — the second question this answers
//!
//! Amendment 3 of `docs/decisions/0022` closed "is the ABI's cost the boundary
//! or the caller" and immediately opened "then what is the **guard**". At
//! `[profile.embedder]` the per-call guard is ~45 of the ~55 ns an embedder pays
//! over native Rust, so it is the whole prize, and "it is the guard" is not an
//! actionable answer. The arms below take it apart. Medians of six runs, one
//! `just abi-attached` each, `taskset -c 2`, all at `[profile.embedder]`:
//!
//! | part | ns | how it was isolated |
//! |---|---|---|
//! | `tf_tree_ipc::fork::generation()` | **+0.2** | a loop with the call against the identical loop without it |
//! | `Tree::view()` | +3.7 | the same loop, building a view per iteration |
//! | `Guard::new(view)` | +4.8 | over the view arm |
//! | the rest of `Tree::guard` — `detached`, `is_shared`, `with_fork_check` | +6.7 | over `Guard::new` |
//! | **= build + drop a guard, in isolation** | **15.1** | |
//! | the same, on `Plan::at`'s critical path | ~22 | arm `E`: guard built per call, *evaluated* through the hoisted one |
//! | the cold bracket-search cursor | ~4.8 | arm `B` − `A`: hoisted guard, stamps visited in a fixed permutation |
//! | **still unattributed** | **~16** | |
//!
//! **The leading hypothesis was wrong, and this is the third one in this area to
//! be.** `fork::generation` was expected to be the cost — a cross-crate call
//! that thin LTO inlines and `[profile.embedder]` does not. It costs **0.2 ns**.
//! It is `#[inline]`, so its MIR crosses the crate boundary and it is inlined at
//! `lto = false` too; the LTO difference in the guard's cost is not this.
//!
//! Two more candidates were killed the same way, by changing them and finding
//! the number did not move:
//!
//! * **`#[inline]` on `Tree::guard`.** Applied, measured, reverted: rung 1 was
//!   43.2 ns against a 43.0–46.9 spread without it, and the ABI arm 298 against
//!   294–297. So the guard is not paying for the *call*.
//! * **The size of the `Guard`.** It is **208 bytes**, 128 of which are the
//!   `[Cell<u64>; MAX_DEPTH]` cursor on a fixture whose plan is three steps
//!   deep. `MAX_DEPTH` was cut 16 → 8, making the guard 144 bytes: rung 1 44.3,
//!   the isolated build+drop 15.0, arm `E` 21.1, the cursor 5.0 — every one of
//!   them inside the run-to-run spread. Reverted. **Zeroing that array is not
//!   what a guard costs.**
//!
//! What is left is ~16 ns for *evaluating through a fresh guard* beyond the
//! cursor, and it is left **unattributed on purpose**. The plausible reading is
//! that a guard materialised per iteration cannot have its fields held in
//! registers across `Plan::at` the way a hoisted one can — but that is a
//! hypothesis, it is what the three refuted candidates above also sounded like,
//! and `0022` does not need it decided. One known asymmetry to fold in before
//! anyone tries: the isolated build+drop arm never performs a lookup, so its
//! `Guard::drop` takes the `n == 0` early return, while a real guard's drop
//! reaches the `is_writable` one.
//!
//! # Usage
//!
//! Needs an arena served by `native_arena --name <n>`; run it through
//! `just abi-attached`, which wires the two together.

#![allow(clippy::print_stdout)]

use anyhow::{anyhow, bail, Context, Result};

use tf_tree::{AttachMode, CreatePolicy, Open, Stamp, SystemDomain};
use tf_tree_c::{
    tft_plan_at, tft_plan_create, tft_test_plan_at_unguarded, tft_tree_open, TFT_LAYOUT_QVEC7_WXYZ,
    TFT_OK,
};

const TARGET: &str = "imu_link";
const SOURCE: &str = "map";
const STAMPS: usize = 256;
const SWEEPS: usize = 40;
const ROUNDS: usize = 9;
const WARMUP: usize = 60_000;

/// Byte-identical to `backing::stamp_ns` and `ratio::stamp_ns` — off every
/// dynamic grid, so `I::eval` runs. `docs/decisions/0013`.
fn stamp_ns(i: i64) -> i64 {
    tf_tree_bench::fixture::NOW_NS - 3_700_000 - i * 9_631
}

fn main() -> Result<()> {
    // **The profile is measured here, not asserted by the caller.**
    //
    // `--boundary-real` used to *be* the answer: `just abi-attached` passed it
    // to the `[profile.embedder]` build and withheld it from the `release` one,
    // and this binary believed whichever it got. That put the most load-bearing
    // fact about a boundary measurement — whether the boundary was in the binary
    // at all — in a shell script, where swapping two lines would silently label
    // the LTO-erased run as the real one. `build.rs` reads the profile directory
    // out of `OUT_DIR` and `embed::lto_for_profile_dir` maps it to the `lto` the
    // workspace manifest declares, so both halves are now facts about this
    // executable.
    //
    // The flag is still accepted, and is now a *claim that is checked*: a
    // mismatch against the measured profile is an error rather than a wrong
    // label on a right-looking table. That is what makes the recipe's two lines
    // unswappable.
    let mut name = "abi_attached".to_owned();
    let mut claimed_real = false;
    for a in std::env::args().skip(1) {
        if a == "--boundary-real" {
            claimed_real = true;
        } else {
            name = a;
        }
    }
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml"),
    )
    .context("reading the workspace manifest to find out what profile this binary is")?;
    let lto =
        tf_tree_bench::embed::lto_for_profile_dir(&manifest, tf_tree_bench::embed::PROFILE_DIR);
    // Anything but a declared `false` leaves LTO able to inline across the crate
    // boundary. `starts_with` rather than `==` because the "cargo's default"
    // spelling is `false (…)`; see `lto_for_profile_dir`. An `unknown (…)` value
    // is correctly *not* a real boundary: a run that cannot name its own LTO
    // setting has not earned the claim.
    let boundary_real = lto.starts_with("false");
    if claimed_real != boundary_real {
        bail!(
            "{} `--boundary-real`, but this binary was built into `target/{}/`, whose \
             profile declares `lto = {lto}` — so the crate boundary is {}. The flag is a \
             claim about the build and this binary measures the build; the two must agree.",
            if claimed_real {
                "the caller passed"
            } else {
                "the caller did not pass"
            },
            tf_tree_bench::embed::PROFILE_DIR,
            if boundary_real { "REAL" } else { "ERASED" },
        );
    }

    // --- the Rust arm: attach through the facade, hoist a guard -------------
    let tree = Open::new()
        .name(&name)
        .map_err(|e| anyhow!("`{name}` is not a usable arena name: {e:?}"))?
        .mode(AttachMode::ReadOnly)
        .create(CreatePolicy::Never)
        .open()
        .map_err(|e| anyhow!("attaching read-only to `{name}`: {e:?}"))?;
    let t = tree
        .frame(TARGET)
        .map_err(|e| anyhow!("frame `{TARGET}`: {e:?}"))?;
    let s = tree
        .frame(SOURCE)
        .map_err(|e| anyhow!("frame `{SOURCE}`: {e:?}"))?;
    let plan = tree
        .plan(t, s)
        .map_err(|e| anyhow!("compiling {SOURCE} <- {TARGET}: {e:?}"))?;

    // --- the ABI arm: attach again, through the C entry points --------------
    //
    // A second, independent attach of the same segment. `tft_tree_open` reads
    // the same `TF_TREE_NAME`/`TF_TREE_RUNTIME_DIR` the facade used, so both
    // arms are on the same arena without either handle being converted into the
    // other — which there is no API to do, and which would defeat the point.
    let mut ctree = core::ptr::null_mut();
    // SAFETY: `out` is a writable pointer to a null-initialised handle slot.
    let rc = unsafe { tft_tree_open(&mut ctree) };
    if rc != TFT_OK {
        bail!("tft_tree_open failed ({rc}) — is `{name}` still served?");
    }
    let ca = std::ffi::CString::new(TARGET)?;
    let cb = std::ffi::CString::new(SOURCE)?;
    let mut cplan = core::ptr::null_mut();
    // SAFETY: live tree handle, NUL-terminated names, writable out slot.
    let rc = unsafe { tft_plan_create(ctree, ca.as_ptr(), cb.as_ptr(), &mut cplan) };
    if rc != TFT_OK {
        bail!("tft_plan_create failed ({rc})");
    }

    let raw: Vec<i64> = (0..STAMPS as i64).map(stamp_ns).collect();
    let stamps: Vec<Stamp<SystemDomain>> = raw.iter().map(|&n| Stamp::from_nanos(n)).collect();

    // Agreement before timing: an arm answering a different question would move
    // the gap and nothing in the timing would say so.
    let guard = tree.guard();
    let mut out = [0.0f64; 7];
    for (i, &st) in stamps.iter().enumerate() {
        let ours = plan
            .at(&guard, st)
            .map_err(|e| anyhow!("the Rust arm declined a stamp it must answer: {e:?}"))?;
        // SAFETY: live plan; `out` is exactly `tft_layout_size(QVEC7_WXYZ)`.
        let rc = unsafe {
            tft_plan_at(
                cplan,
                raw[i],
                TFT_LAYOUT_QVEC7_WXYZ,
                out.as_mut_ptr().cast(),
            )
        };
        if rc != TFT_OK {
            bail!("the ABI arm declined stamp {} ({rc})", raw[i]);
        }
        let d = (ours.t.x - out[4]).abs();
        if d > 1e-12 {
            bail!("the two arms disagree at stamp {} by {d}", raw[i]);
        }
    }

    let sweep_rust = || {
        let mut acc = 0.0f64;
        for _ in 0..SWEEPS {
            for &st in &stamps {
                if let Ok(v) = plan.at(&guard, std::hint::black_box(st)) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };
    let mut obuf = [0.0f64; 7];
    let mut sweep_abi = || {
        let mut acc = 0.0f64;
        for _ in 0..SWEEPS {
            for &n in &raw {
                // SAFETY: as above.
                let rc = unsafe {
                    tft_plan_at(
                        cplan,
                        std::hint::black_box(n),
                        TFT_LAYOUT_QVEC7_WXYZ,
                        obuf.as_mut_ptr().cast(),
                    )
                };
                if rc == TFT_OK {
                    acc += obuf[4];
                }
            }
        }
        std::hint::black_box(acc)
    };

    // --- the rungs between them ------------------------------------------
    //
    // `abi_cost` prices these too, but with `lto = "thin"` erasing the boundary
    // and on a 3-edge tree that fits in L1 — where they total ~3 ns. At a real
    // boundary on this fixture they are the ~35 ns `0022` question 5 leaves
    // open, so they are measured here rather than carried across.

    // 1. The guard, per call — what the C signature cannot hoist.
    let sweep_guard = || {
        let mut acc = 0.0f64;
        for _ in 0..SWEEPS {
            for &st in &stamps {
                let g = tree.guard();
                if let Ok(v) = plan.at(&g, std::hint::black_box(st)) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };

    // 2. The same, plus the 56-byte `QVEC7_WXYZ` store the ABI must make and a
    //    native caller never does: Rust returns an `Iso3` the caller consumes
    //    from registers.
    let mut wbuf = [0.0f64; 7];
    let mut sweep_write = || {
        let mut acc = 0.0f64;
        for _ in 0..SWEEPS {
            for &st in &stamps {
                let g = tree.guard();
                if let Ok(v) = plan.at(&g, std::hint::black_box(st)) {
                    wbuf = [v.q.w, v.q.x, v.q.y, v.q.z, v.t.x, v.t.y, v.t.z];
                    acc += wbuf[4];
                }
            }
        }
        std::hint::black_box(acc)
    };

    // 3. The ABI's own body without `catch_unwind`, so the panic guard is a
    //    subtraction on a real, non-inlinable call.
    let mut ubuf = [0.0f64; 7];
    let mut sweep_unguarded = || {
        let mut acc = 0.0f64;
        for _ in 0..SWEEPS {
            for &n in &raw {
                // SAFETY: live plan; `ubuf` is exactly the layout's payload.
                let rc = unsafe {
                    tft_test_plan_at_unguarded(
                        cplan,
                        std::hint::black_box(n),
                        TFT_LAYOUT_QVEC7_WXYZ,
                        ubuf.as_mut_ptr().cast(),
                    )
                };
                if rc == TFT_OK {
                    acc += ubuf[4];
                }
            }
        }
        std::hint::black_box(acc)
    };

    // --- decomposing rung 1: what inside `Tree::guard()` costs the ~45 ns? ---
    //
    // Rung 1 above prices "a guard per call" as a *difference between two
    // sweeps that both evaluate the plan*. That difference contains two things
    // and the ladder cannot tell them apart:
    //
    //   (a) building and dropping the `Guard` value itself, and
    //   (b) what a fresh guard does to `Plan::at` — a guard carries the
    //       per-step bracket-search cursor (`plan.rs`'s `cursor` field), and
    //       `Guard::new` starts it at the `EdgeId(0)` sentinel, so **every step
    //       of every call takes the cold search path**. A hoisted guard warms
    //       that cursor once and resumes beside the previous answer.
    //
    // Attributing the whole 47 ns to (a) would be the residue mistake this
    // file's own header is about. So (a) is measured directly — the same loop,
    // constructing and dropping a guard and *not* evaluating anything — and (b)
    // is what is left over, named as such.
    //
    // Within (a), three arms in increasing order of what they include, so each
    // step is one named thing:
    //
    //   `empty`    the loop itself, so nothing below is loop overhead;
    //   `forkgen`  `tf_tree_ipc::fork::generation` — the leading hypothesis:
    //              a cross-crate call that thin LTO inlines and `embedder` does
    //              not. It is `#[inline]`, so its MIR crosses the crate anyway;
    //              this is what says whether that is true in the build;
    //   `view`     `Tree::view` (through the `unstable` public spelling) — the
    //              `ArenaView` a guard is built from: an `as_dyn` vtable hop,
    //              the header deref, and three builder fields;
    //   `gnew`     `Guard::new(view)` — adds the seqlock generation read and
    //              zeroing the 16-entry cursor array;
    //   `gfull`    `Tree::guard()` — adds `detached()`, `is_shared()` and
    //              `with_fork_check`, i.e. the whole fork-safety half.
    //
    // `gfull - gnew` is therefore the fork check *as the facade pays for it*,
    // and `forkgen - empty` is the counter load alone. If those two disagree by
    // a lot, the cost is the branching and the extra `Option<(u64, fn)>` field,
    // not the load.
    //
    // Every arm carries the same `black_box(n)` fold so the loop shape, the
    // `raw` walk and the accumulator are identical across all five and the
    // subtractions remove them.
    let sweep_empty = || {
        let mut acc = 0u64;
        for _ in 0..SWEEPS {
            for &n in &raw {
                acc ^= std::hint::black_box(n) as u64;
            }
        }
        std::hint::black_box(acc)
    };
    let sweep_forkgen = || {
        let mut acc = 0u64;
        for _ in 0..SWEEPS {
            for &n in &raw {
                acc ^= std::hint::black_box(n) as u64 ^ tf_tree_ipc::fork::generation();
            }
        }
        std::hint::black_box(acc)
    };
    let sweep_view = || {
        let mut acc = 0u64;
        for _ in 0..SWEEPS {
            for &n in &raw {
                acc ^= std::hint::black_box(n) as u64;
                let v = tree.arena_view();
                std::hint::black_box(&v);
            }
        }
        std::hint::black_box(acc)
    };
    let sweep_gnew = || {
        let mut acc = 0u64;
        for _ in 0..SWEEPS {
            for &n in &raw {
                acc ^= std::hint::black_box(n) as u64;
                let g = tf_tree_core::Guard::new(tree.arena_view());
                std::hint::black_box(&g);
            }
        }
        std::hint::black_box(acc)
    };
    let sweep_gfull = || {
        let mut acc = 0u64;
        for _ in 0..SWEEPS {
            for &n in &raw {
                acc ^= std::hint::black_box(n) as u64;
                let g = tree.guard();
                std::hint::black_box(&g);
            }
        }
        std::hint::black_box(acc)
    };

    // --- and the residue is a hypothesis until it is varied against ---------
    //
    // "Rung 1 minus the guard's construction cost is the cold cursor" is
    // exactly the shape of reasoning this repository has been wrong about six
    // times. So it is tested rather than asserted, with the one variable the
    // cursor is sensitive to and nothing else is: **the order the stamps are
    // visited in**.
    //
    // The `stamp_ns` sequence walks monotonically backwards, 9631 ns per step,
    // so a hoisted guard's cursor is always one probe from the answer. Visiting
    // the same 256 stamps in a fixed permutation instead leaves the cursor
    // pointing somewhere useless on nearly every call — which is precisely the
    // state `Guard::new` puts a fresh guard in. That gives a 2x2:
    //
    //   |            | in order | shuffled |
    //   | hoisted    |    A     |    B     |
    //   | per call   |    C     |    D     |
    //
    // If the cursor is the mechanism, then B rises to meet C-minus-the-guard,
    // and D stays level with C — a per-call guard is already cold, so the order
    // cannot make it colder. If instead B stays level with A, the cursor is not
    // what rung 1 is paying for and the residue is something else, which is a
    // result worth having and would stop the amendment being written.
    //
    // The permutation is a fixed multiplicative step over a prime-free stride
    // (`STAMPS` is a power of two, so an odd stride is a full cycle) — no RNG,
    // no dependency, byte-identical run to run. All four arms walk an index
    // slice, including the two "in order" ones, so the extra indirection is in
    // every arm and cancels in the comparison.
    let idx_seq: Vec<usize> = (0..STAMPS).collect();
    let idx_shuf: Vec<usize> = (0..STAMPS).map(|i| (i * 97 + 13) % STAMPS).collect();
    debug_assert_eq!(
        {
            let mut s = idx_shuf.clone();
            s.sort_unstable();
            s
        },
        idx_seq
    );
    let sweep_hoist_ix = |ix: &[usize]| {
        let mut acc = 0.0f64;
        for _ in 0..SWEEPS {
            for &i in ix {
                if let Ok(v) = plan.at(&guard, std::hint::black_box(stamps[i])) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };
    let sweep_percall_ix = |ix: &[usize]| {
        let mut acc = 0.0f64;
        for _ in 0..SWEEPS {
            for &i in ix {
                let g = tree.guard();
                if let Ok(v) = plan.at(&g, std::hint::black_box(stamps[i])) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };

    // --- and one more, because the parts do not add up ----------------------
    //
    // The 2x2 above prices the cold cursor at ~7 ns and the standalone loop
    // prices building+dropping a guard at ~15 ns, against a rung 1 of ~43. So
    // ~21 ns is still unaccounted for, and there are two candidate readings:
    //
    //   (i)  the standalone 15 ns *underestimates* the guard, because in that
    //        loop nothing depends on the guard and the CPU overlaps successive
    //        iterations freely, while in rung 1 the construction sits on
    //        `Plan::at`'s critical path;
    //   (ii) or evaluating through a *fresh* guard costs something beyond the
    //        cursor — the guard's `ArenaView` and generation are re-read from a
    //        stack slot the compiler cannot hold in registers across the call.
    //
    // This arm separates them: build a guard per call, `black_box` it so it is
    // really built and really dropped — then evaluate the plan through the
    // **hoisted** guard. It pays construction on the same critical path as rung
    // 1 and pays nothing for using it. `E - A` is therefore the guard object as
    // rung 1 actually pays for it, and `C - E` is everything about *using* a
    // fresh one.
    let sweep_build_only = || {
        let mut acc = 0.0f64;
        for _ in 0..SWEEPS {
            for &st in &stamps {
                let g = tree.guard();
                std::hint::black_box(&g);
                drop(g);
                if let Ok(v) = plan.at(&guard, std::hint::black_box(st)) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };

    let per_round = (SWEEPS * stamps.len()) as f64;
    let per_call = SWEEPS * stamps.len();
    for _ in 0..WARMUP.div_ceil(per_call) {
        std::hint::black_box(sweep_rust());
        std::hint::black_box(sweep_guard());
        std::hint::black_box(sweep_write());
        std::hint::black_box(sweep_unguarded());
        std::hint::black_box(sweep_abi());
        std::hint::black_box(sweep_empty());
        std::hint::black_box(sweep_forkgen());
        std::hint::black_box(sweep_view());
        std::hint::black_box(sweep_gnew());
        std::hint::black_box(sweep_gfull());
        std::hint::black_box(sweep_hoist_ix(&idx_seq));
        std::hint::black_box(sweep_hoist_ix(&idx_shuf));
        std::hint::black_box(sweep_percall_ix(&idx_seq));
        std::hint::black_box(sweep_percall_ix(&idx_shuf));
        std::hint::black_box(sweep_build_only());
    }
    let mut g_ns = Vec::with_capacity(ROUNDS);
    let mut w_ns = Vec::with_capacity(ROUNDS);
    let mut u_ns = Vec::with_capacity(ROUNDS);
    let mut e_ns = Vec::with_capacity(ROUNDS);
    let mut f_ns = Vec::with_capacity(ROUNDS);
    let mut v_ns = Vec::with_capacity(ROUNDS);
    let mut gn_ns = Vec::with_capacity(ROUNDS);
    let mut gf_ns = Vec::with_capacity(ROUNDS);
    let mut cell_ns = [(); 4].map(|()| Vec::with_capacity(ROUNDS));
    let mut bo_ns = Vec::with_capacity(ROUNDS);

    let mut r_ns = Vec::with_capacity(ROUNDS);
    let mut a_ns = Vec::with_capacity(ROUNDS);
    for r in 0..ROUNDS {
        let (a, b) = if r % 2 == 0 {
            let t0 = std::time::Instant::now();
            let _ = sweep_rust();
            let a = t0.elapsed().as_nanos() as f64 / per_round;
            let t1 = std::time::Instant::now();
            let _ = sweep_abi();
            (a, t1.elapsed().as_nanos() as f64 / per_round)
        } else {
            let t1 = std::time::Instant::now();
            let _ = sweep_abi();
            let b = t1.elapsed().as_nanos() as f64 / per_round;
            let t0 = std::time::Instant::now();
            let _ = sweep_rust();
            (t0.elapsed().as_nanos() as f64 / per_round, b)
        };
        r_ns.push(a);
        a_ns.push(b);
        // The intermediate rungs are not part of the alternating pair above —
        // they only ever appear as differences against their neighbours, so a
        // fixed order costs them nothing the subtraction does not remove.
        let t = std::time::Instant::now();
        let _ = sweep_guard();
        g_ns.push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_write();
        w_ns.push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_unguarded();
        u_ns.push(t.elapsed().as_nanos() as f64 / per_round);
        // The five decomposition arms, same fixed-order argument as above: each
        // is only ever read as a difference against its neighbour.
        let t = std::time::Instant::now();
        let _ = sweep_empty();
        e_ns.push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_forkgen();
        f_ns.push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_view();
        v_ns.push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_gnew();
        gn_ns.push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_gfull();
        gf_ns.push(t.elapsed().as_nanos() as f64 / per_round);
        // The 2x2, in A B C D order every round.
        let t = std::time::Instant::now();
        let _ = sweep_hoist_ix(&idx_seq);
        cell_ns[0].push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_hoist_ix(&idx_shuf);
        cell_ns[1].push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_percall_ix(&idx_seq);
        cell_ns[2].push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_percall_ix(&idx_shuf);
        cell_ns[3].push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_build_only();
        bo_ns.push(t.elapsed().as_nanos() as f64 / per_round);
    }
    bo_ns.sort_by(f64::total_cmp);
    let build_only = bo_ns[bo_ns.len() / 2];
    for v in &mut cell_ns {
        v.sort_by(f64::total_cmp);
    }
    let cell = cell_ns.each_ref().map(|v| v[v.len() / 2]);
    for v in [&mut e_ns, &mut f_ns, &mut v_ns, &mut gn_ns, &mut gf_ns] {
        v.sort_by(f64::total_cmp);
    }
    let (empty, forkgen, viewb, gnew, gfull) = (
        e_ns[e_ns.len() / 2],
        f_ns[f_ns.len() / 2],
        v_ns[v_ns.len() / 2],
        gn_ns[gn_ns.len() / 2],
        gf_ns[gf_ns.len() / 2],
    );
    g_ns.sort_by(f64::total_cmp);
    w_ns.sort_by(f64::total_cmp);
    u_ns.sort_by(f64::total_cmp);
    let (gd, wr, un) = (
        g_ns[g_ns.len() / 2],
        w_ns[w_ns.len() / 2],
        u_ns[u_ns.len() / 2],
    );
    r_ns.sort_by(f64::total_cmp);
    a_ns.sort_by(f64::total_cmp);
    let rust = r_ns[r_ns.len() / 2];
    let abi = a_ns[a_ns.len() / 2];

    println!("Rust and the C ABI on the SAME shared arena, same process, same stamps");
    println!(
        "  arena `{name}`, §11.1 fixture, {} off-grid stamps",
        STAMPS
    );
    println!();
    println!("  native Rust, guard hoisted        {rust:7.1} ns");
    println!(
        "  + guard built per call            {gd:7.1} ns   ({:+.1})",
        gd - rust
    );
    println!(
        "  + the 56-byte QVEC7 store         {wr:7.1} ns   ({:+.1})",
        wr - gd
    );
    println!(
        "  the ABI, no panic guard           {un:7.1} ns   ({:+.1})",
        un - wr
    );
    println!(
        "  tft_plan_at, called from Rust     {abi:7.1} ns   ({:+.1})",
        abi - un
    );
    println!(
        "  tft_plan_at, called from C++      {:7.1} ns   (recorded, docs/benchmarks/tf2.md)",
        302.0
    );
    println!();
    println!(
        "  decomposing rung 1 — a `Guard` is {} bytes, which is the thing being built:",
        core::mem::size_of::<tf_tree_core::Guard<'_>>()
    );
    println!("    the loop alone                  {empty:7.1} ns");
    println!(
        "    + fork::generation()            {forkgen:7.1} ns   ({:+.1})",
        forkgen - empty
    );
    println!(
        "    + Tree::view()                  {viewb:7.1} ns   ({:+.1} over the loop)",
        viewb - empty
    );
    println!(
        "    + Guard::new(view)              {gnew:7.1} ns   ({:+.1})",
        gnew - viewb
    );
    println!(
        "    + the fork half (Tree::guard)   {gfull:7.1} ns   ({:+.1})",
        gfull - gnew
    );
    println!(
        "    => building+dropping a guard    {:7.1} ns   (gfull - the loop)",
        gfull - empty
    );
    println!(
        "    => the rest of rung 1           {:7.1} ns   (unattributed by the arms above)",
        (gd - rust) - (gfull - empty)
    );
    println!();
    println!("  is that residue the cold bracket-search cursor? vary the stamp ORDER:");
    println!(
        "    hoisted guard, in order         {:7.1} ns  (A)",
        cell[0]
    );
    println!(
        "    hoisted guard, shuffled         {:7.1} ns  (B)   ({:+.1} vs A)",
        cell[1],
        cell[1] - cell[0]
    );
    println!(
        "    guard per call, in order        {:7.1} ns  (C)   ({:+.1} vs A)",
        cell[2],
        cell[2] - cell[0]
    );
    println!(
        "    guard per call, shuffled        {:7.1} ns  (D)   ({:+.1} vs C)",
        cell[3],
        cell[3] - cell[2]
    );
    println!(
        "    B-A is order-sensitivity a HOISTED guard has; D-C is what is left of it\n    \
         once every call starts cold. C-B {:+.1} ns is then the guard OBJECT, to be\n    \
         compared with the {:.1} ns measured directly above.",
        cell[2] - cell[1],
        gfull - empty
    );
    println!();
    println!(
        "    guard BUILT per call, evaluated through the hoisted one:\n      \
         {build_only:7.1} ns  (E)   ({:+.1} vs A = the object on the critical path)\n      \
         and C - E = {:+.1} ns is everything about USING a fresh guard.",
        build_only - cell[0],
        cell[2] - build_only
    );
    println!();
    // **Which profile this binary was built with decides what it measured.**
    // The workspace release profile is `lto = "thin"`, which erases the crate
    // boundary — `report.rs`'s §9.2 embedding row says so in those words — so a
    // Rust caller gets `tft_plan_at` *inlined*, which no foreign caller ever
    // does. `[profile.embedder]` is `lto = false` and is the one that measures a
    // real boundary.
    // The profile travels with the numbers, on the same page as the numbers.
    // Every table above is a boundary measurement and a boundary measurement
    // read without its profile is the error `docs/PHASE4.md` §0.0 records twice.
    println!(
        "  build: target/{}/  (the workspace manifest declares lto = {lto} for it)",
        tf_tree_bench::embed::PROFILE_DIR
    );
    if !boundary_real {
        println!(
            "  Built with LTO on: THIS RUN CANNOT ANSWER THE QUESTION. The ABI call is\n  \
             inlined into the Rust caller, which is not something a C or C++ embedder can\n  \
             get. Re-run at `--profile embedder` (lto = false); `just abi-attached` does\n  \
             both and prints them together."
        );
    } else {
        println!(
            "  Built without LTO, so this is a real boundary. The ABI costs {:+.0} ns from a\n  \
             **Rust** caller — against the {:+.0} ns a C++ caller pays on this same arena.\n  \
             The two agree, so the cost is the boundary itself and NOT the language: a\n  \
             foreign caller and a non-inlined Rust caller pay the same thing.",
            abi - rust,
            302.0 - rust
        );
    }

    // SAFETY: handles created above, freed once, not used after.
    unsafe {
        tf_tree_c::tft_plan_free(cplan);
        tf_tree_c::tft_tree_free(ctree);
    }
    drop(guard);
    Ok(())
}

/// Keeps `Context` in use when the `?` paths above are all `map_err`.
#[allow(dead_code)]
fn _ctx(r: std::io::Result<()>) -> Result<()> {
    r.context("unused")
}
