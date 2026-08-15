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
//! # Usage
//!
//! Needs an arena served by `native_arena --name <n>`; run it through
//! `just abi-attached`, which wires the two together.

#![allow(clippy::print_stdout)]

use anyhow::{anyhow, bail, Context, Result};

use tf_tree::{AttachMode, CreatePolicy, Open, Stamp, SystemDomain};
use tf_tree_c::{tft_plan_at, tft_plan_create, tft_tree_open, TFT_LAYOUT_QVEC7_WXYZ, TFT_OK};

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
    // `--boundary-real` is passed by `just abi-attached` for the
    // `[profile.embedder]` build only. A flag rather than a `cfg!`: the profile's
    // `lto` setting is not visible to `cfg!`, and a custom `--cfg` would need
    // `check-cfg` plumbing kept in sync with the recipe for no gain. The recipe
    // is the single place that knows which build it just made.
    let mut name = "abi_attached".to_owned();
    let mut boundary_real = false;
    for a in std::env::args().skip(1) {
        if a == "--boundary-real" {
            boundary_real = true;
        } else {
            name = a;
        }
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

    let per_round = (SWEEPS * stamps.len()) as f64;
    let per_call = SWEEPS * stamps.len();
    for _ in 0..WARMUP.div_ceil(per_call) {
        std::hint::black_box(sweep_rust());
        std::hint::black_box(sweep_abi());
    }

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
    }
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
        "  tft_plan_at, called from Rust     {abi:7.1} ns   ({:+.1})",
        abi - rust
    );
    println!(
        "  tft_plan_at, called from C++      {:7.1} ns   (recorded, docs/benchmarks/tf2.md)",
        302.0
    );
    println!();
    // **Which profile this binary was built with decides what it measured.**
    // The workspace release profile is `lto = "thin"`, which erases the crate
    // boundary — `report.rs`'s §9.2 embedding row says so in those words — so a
    // Rust caller gets `tft_plan_at` *inlined*, which no foreign caller ever
    // does. `[profile.embedder]` is `lto = false` and is the one that measures a
    // real boundary.
    if !boundary_real {
        println!(
            "  Built with lto = \"thin\": THIS RUN CANNOT ANSWER THE QUESTION. The ABI call is\n  \
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
