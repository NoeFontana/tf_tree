//! Where the C ABI's measured +52% over native Rust actually goes.
//!
//! `docs/benchmarks/tf2.md` reports a C++ caller against `libtf_tree_c.so` at
//! **306.7 ns** where native Rust measures ~**201.5 ns**, on a run that changed
//! two variables together: the call crosses a shared-library boundary, *and* the
//! arena is a `MAP_SHARED` `memfd` rather than a private heap allocation.
//!
//! **Neither is the cause.** [`tf_tree_bench::backing`] measures the mapping
//! (<= 9.6 ns, paired) and — with `--attach` — the cross-process read-only
//! attach (-0.7 ns). The link mode was measured separately by building
//! `tests/cpp/bench.cpp` against the `.a` and the `.so`: 245.4 against 244.4 ns.
//! What is left is the C ABI's own per-call work, ~99.5 ns of it, which
//! `tft_plan_at_many` partly amortizes (302.0 -> 261.0 ns).
//!
//! **The residue row below is a subtraction, not a measurement**, and that is
//! exactly how this binary first led to a wrong conclusion: an earlier version
//! measured the mapping, found it free, and attributed everything else to the
//! linker without measuring the linker. The row is labelled in the output for
//! that reason.
//!
//! ```text
//! just abi-split                       # the paired heap-vs-memfd rungs
//! arena_backing --attach <name>        # plus the cross-process rung
//! ```

#![allow(clippy::print_stdout)]

use anyhow::Result;

/// The C++ arm from `docker/tf2/native_ratio.sh`, for the subtraction.
///
/// A literal, and that is a real weakness rather than a shortcut: it is a
/// figure from a different run on a different day, so the residue below is a
/// decomposition of *recorded* numbers rather than a measurement. Treating that
/// residue as if it had been measured is precisely how this file's first version
/// concluded "it is the linker", which is wrong by a factor of ~100. The rungs
/// this binary measures itself are paired; the output says which is which.
const CPP_ABI_NS: f64 = 306.7;

fn main() -> Result<()> {
    // `--attach NAME` is the cross-process rung of the ladder, and it needs an
    // arena somebody else is already serving (`native_arena --name NAME`), so it
    // cannot be folded into the default run.
    let mut attach: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--attach" => {
                attach = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--attach wants an arena name"))?,
                );
            }
            other => anyhow::bail!("unknown argument `{other}`"),
        }
    }

    let run = tf_tree_bench::backing::measure()?;
    println!("{}", run.verdict_line());

    // **The bound, not the point estimate, is what the split rests on.** The
    // sign of a ~2 ns effect measured on this host resolves in some runs and not
    // in others; the band's upper edge barely moves. Since the residue being
    // attributed is ~100 ns, "the mapping accounts for at most this" settles the
    // question either way, and it settles it without needing a quiet host.
    let bound = run.backing_ns_bound();
    let total = CPP_ABI_NS - run.heap_ns;
    let boundary_min = total - bound;
    println!();
    println!("splitting the C ABI's +{total:.1} ns over native Rust:");
    match run.backing_ns() {
        Some(b) => println!(
            "  arena backing (heap -> memfd)   {b:+7.1} ns   measured, paired over {} rounds; \
             at most {bound:.1} ns across the band",
            run.rounds
        ),
        None => println!(
            "  arena backing (heap -> memfd)   <={bound:6.1} ns   sign unresolved over {} rounds \
             (the band contains 1.0), so this is the bound and not a point estimate",
            run.rounds
        ),
    }
    println!(
        "  everything else                 >={boundary_min:6.1} ns   the residue, against \
         {CPP_ABI_NS:.1} ns recorded in docs/benchmarks/tf2.md"
    );
    println!();
    println!(
        "  {:.0}% of the gap is NOT the arena backing. It is also not the link mode (the .a\n  \
         and the .so measure 245.4 against 244.4 ns) and not the cross-process attach (-0.7\n  \
         ns, see --attach). It is the C ABI's per-call work: tft_plan_at builds a Guard on\n  \
         every call where the Rust arm hoists one, and tft_plan_at_many recovers 41 ns of it.\n  \
         The backing row is measured and paired; this residue row is a subtraction.",
        100.0 * boundary_min / total
    );

    // **Residency, since `0021`.** The heap arena is now demand-faulted, so
    // declared-but-unpublished slots cost it nothing; the shared one pre-faults
    // every declared slot in `populate_hot`. This prices that difference, which
    // is the whole remaining resident gap on an over-declared arena and which
    // nothing measured before — `scale_sweep`'s `rss_over_arena` is heap-only.
    let (heap_kib, shm_kib, arena_bytes) = tf_tree_bench::backing::residency_both()?;
    let declared_kib = arena_bytes as f64 / 1024.0;
    println!();
    println!("resident Pss for the same declared arena ({declared_kib:.0} KiB reserved):");
    println!(
        "  heap arena     {heap_kib:6} KiB   {:.0}% of what it declares",
        heap_kib as f64 * 1024.0 / arena_bytes as f64 * 100.0
    );
    println!(
        "  memfd arena    {shm_kib:6} KiB   {:.0}% — every edge here is claimed, so every ring is warm",
        shm_kib as f64 * 1024.0 / arena_bytes as f64 * 100.0
    );
    println!(
        "  the shared path holds {:+} KiB more for the same data. That is PHASE2 §7.1's\n  \
         latency guarantee priced: no page fault inside a lookup, paid for in residency.\n  \
         **Read the 100% as a property of THIS fixture, not of the arena.** Since\n  \
         `docs/decisions/0024` rings are populated per-edge, at claim and at plan — not\n  \
         wholesale at attach — and `spin_up` claims every edge the fixture declares, so\n  \
         there is nothing here left cold. A process that uses a subset of a shared arena\n  \
         is charged for that subset: 19.5% of a 64-edge arena when it takes up four\n  \
         (`crates/tf_tree_bench/tests/population.rs`).",
        shm_kib as i64 - heap_kib as i64
    );

    // What a per-call guard costs on each backing, with no C ABI anywhere in
    // the loop. This is what `tft_plan_at` is forced into by its signature, and
    // it is also what refuted `0022`'s first question 4: the heap and shared
    // rows are within noise of each other, so the `is_shared()` fork check is
    // not the differentiator it was assumed to be.
    let (heap_g, shm_g) = tf_tree_bench::backing::guard_cost_both(run.rounds, 40, 60_000)?;
    println!();
    println!("Tree::guard() per lookup vs hoisted (safe Rust, no C ABI):");
    println!(
        "  heap arena     hoisted {:6.1} ns   per-call {:6.1} ns   guard costs {:+6.1} ns",
        heap_g.hoisted_ns,
        heap_g.per_call_ns,
        heap_g.guard_ns()
    );
    println!(
        "  memfd arena    hoisted {:6.1} ns   per-call {:6.1} ns   guard costs {:+6.1} ns",
        shm_g.hoisted_ns,
        shm_g.per_call_ns,
        shm_g.guard_ns()
    );
    println!(
        "  the is_shared() fork check costs {:+.1} ns per lookup — noise, so it is NOT what\n  \
         makes a shared arena expensive. Build with --no-default-features to drop `counters`:\n  \
         that halves the guard (+16.8 / +18.9 ns), and is 0022's question 1.",
        shm_g.guard_ns() - heap_g.guard_ns()
    );

    // `docs/decisions/0023` open question 3's falsifier: the same guard cost on
    // the three-edge fixture `abi_cost.rs` gates R3 on, against §11.1's, in one
    // binary with the two interleaved. The claim under test is that the toy
    // fixture understates R3 because the cursor's cost is a stamp-array
    // working-set effect, and the evidence for it was 16 vs 34.4 ns from two
    // different binaries.
    let (small_g, big_g) = tf_tree_bench::backing::guard_cost_fixture_pair(run.rounds, 40, 60_000)?;
    println!();
    println!("0023 q3 — per-call guard by FIXTURE, paired and interleaved, both heap:");
    println!("  three-edge, 256 slots (abi_cost.rs's tree)   guard costs {small_g:+6.1} ns");
    println!("  the §11.1 fixture                            guard costs {big_g:+6.1} ns");
    println!(
        "  paired difference {:+.1} ns. 0023 q3 predicts ~+18 ns from the stamp array\n  \
         crossing L1d (2 KiB searched against 128 KiB); the unpaired figures it\n  \
         argued from were 16 and 34.4 ns. A difference near zero REFUTES the\n  \
         working-set reading and withdraws q3's recommendation to move R3.",
        big_g - small_g
    );

    if let Some(name) = attach {
        // Same rounds/sweeps as the paired run, so the two `Rust native` rungs
        // are the same loop and differ only in which process built the arena.
        let (ns, per_call_ns) = tf_tree_bench::backing::measure_attached(
            &name,
            tf_tree_bench::backing::ROUNDS,
            40,
            60_000,
        )?;
        println!();
        println!("cross-process rung, attached read-only to `{name}`:");
        println!(
            "  H  Rust native, heap,     in-process     {:7.1} ns",
            run.heap_ns
        );
        println!(
            "  S  Rust native, memfd RW, in-process     {:7.1} ns",
            run.shm_ns
        );
        println!("  A  Rust native, memfd RO, cross-process  {ns:7.1} ns");
        println!(
            "  attaching costs {:+.1} ns against the in-process shared arena — and this is \
             native Rust, so no C ABI is involved in it.",
            ns - run.shm_ns
        );
        println!();
        println!(
            "  A  with the guard acquired PER LOOKUP     {per_call_ns:7.1} ns  ({:+.1} ns)",
            per_call_ns - ns
        );
        println!(
            "  that is the shape tft_plan_at is forced into, on the exact arena a C++\n  \
             caller measures at 302.0 ns. Native Rust here is {ns:.1}; the guard accounts\n  \
             for {:.0}% of the {:.0} ns between them.",
            (per_call_ns - ns) / (302.0 - ns) * 100.0,
            302.0 - ns
        );
    }
    Ok(())
}
