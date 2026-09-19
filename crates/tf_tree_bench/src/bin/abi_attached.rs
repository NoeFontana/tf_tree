//! Is the C ABI's +101 ns on a shared arena the ABI or the C++ caller?
//! (`docs/benchmarks/tf2.md`: C++ 302 ns against native Rust 200.6.)
//!
//! This binary calls `tft_plan_at` from Rust on the same arena and stamps under two
//! profiles:
//!
//! | profile | native Rust | Rust -> ABI | C++ -> ABI |
//! |---|---|---|---|
//! | `release` (`lto = "thin"`) | 200.5 | 225.8 (+25) | 302.0 |
//! | **`embedder` (`lto = false`)** | 241.3 | **298.4 (+57)** | **302.0 (+61)** |
//!
//! A Rust and a C++ caller agree to within 4 ns at a real boundary: the cost is the
//! boundary, not the language. `abi_cost` sees only +2.3 ns because thin LTO inlines
//! `tft_plan_at` into its Rust caller, which makes `PHASE4` §7 gate criterion 1
//! unmeasurable in its own build (`report.rs`'s §9.2 embedding row says the same).
//!
//! The arms also decompose the per-call `Guard` (`0022` amendment 3) at
//! `[profile.embedder]`: `fork::generation()` +0.2 ns, `Tree::view()` +3.7,
//! `Guard::new` +4.8, the rest of `Tree::guard` +6.7 (15.1 isolated build+drop, ~22 on
//! `Plan::at`'s critical path, arm `E`), the cold cursor ~4.8 (arm `B` - `A`), and
//! ~16 ns left unattributed on purpose. `#[inline]` on `Tree::guard` and halving
//! `MAX_DEPTH` moved nothing.
//!
//! Needs an arena served by `native_arena --name <n>`; run it through
//! `just abi-attached`.

#![allow(clippy::print_stdout)]
// `docs/decisions/0007` rule 1, kind 5 (our own C ABI, called from Rust to measure it),
// per `0048`; a bin is a separate crate root.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

// SAFETY (module invariant): every `unsafe` block calls a `tft_*` entry point of
// `tf_tree_c` on a handle this process created and has not freed, from its creating
// thread (the documented affinity). The two arms attach independently.

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

/// Byte-identical to `backing::stamp_ns` and `ratio::stamp_ns`: off every dynamic grid (`0013`).
fn stamp_ns(i: i64) -> i64 {
    tf_tree_bench::fixture::NOW_NS - 3_700_000 - i * 9_631
}

fn main() -> Result<()> {
    // The profile is measured (`build.rs` + `embed::lto_for_profile_dir`), not asserted
    // by the caller; `--boundary-real` is a claim that is checked against it.
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
    // Anything but a declared `false` leaves LTO able to inline across the boundary.
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
    // A second attach of the same segment via `TF_TREE_NAME`/`TF_TREE_RUNTIME_DIR`.
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
    // The rungs `abi_cost` prices at ~3 ns under thin LTO; `0022` question 5 leaves ~35 ns open.

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

    // 2. The same, plus the 56-byte `QVEC7_WXYZ` store a native caller never makes.
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

    // Decomposing rung 1: (a) building and dropping a `Guard`, and (b) what a fresh
    // guard does to `Plan::at` (its cursor starts cold at every step). Arms: `empty`
    // (loop), `forkgen` (`fork::generation`), `view` (`Tree::view`), `gnew`
    // (`Guard::new`), `gfull` (`Tree::guard()`); each carries the same `black_box(n)` fold.
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

    // The cursor residue is tested by varying visit order (stamps walk monotonically
    // backwards; a fixed odd-stride permutation leaves the cursor cold). 2x2:
    // hoisted/per-call x in-order/shuffled = A B / C D. All arms walk an index slice
    // so the indirection cancels.
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

    // Arm `E`: build a guard per call, `black_box` it, evaluate through the hoisted one.
    // `E - A` is the guard object on the critical path; `C - E` is using a fresh one.
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
        // Intermediate rungs only appear as differences, so fixed order is harmless.
        let t = std::time::Instant::now();
        let _ = sweep_guard();
        g_ns.push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_write();
        w_ns.push(t.elapsed().as_nanos() as f64 / per_round);
        let t = std::time::Instant::now();
        let _ = sweep_unguarded();
        u_ns.push(t.elapsed().as_nanos() as f64 / per_round);
        // The five decomposition arms, same fixed-order argument as above.
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
    // The profile travels with the numbers (`docs/PHASE4.md` §0.0).
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
