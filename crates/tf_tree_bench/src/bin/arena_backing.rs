//! Splits the C ABI's +52% over native Rust (`docs/benchmarks/tf2.md`: 306.7 ns
//! against ~201.5 ns) into arena backing, link mode and per-call ABI work.
//!
//! [`tf_tree_bench::backing`] measures the mapping (<= 9.6 ns, paired) and, with
//! `--attach`, the cross-process attach. The residue row is a subtraction from a
//! recorded figure, not a measurement, and is labelled so in the output.
//!
//! ```text
//! just abi-split                       # the paired heap-vs-memfd rungs
//! arena_backing --attach <name>        # plus the cross-process rung
//! ```

#![allow(clippy::print_stdout)]

use anyhow::Result;

/// The C++ arm from `docker/tf2/native_ratio.sh`: a recorded figure, so the residue
/// derived from it is a subtraction, not a measurement.
const CPP_ABI_NS: f64 = 306.7;

fn main() -> Result<()> {
    // `--attach NAME` needs an arena already served by `native_arena --name NAME`.
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

    // The upper bound, not the point estimate, carries the split: the sign of a
    // ~2 ns effect is noisy, the band's upper edge is not.
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

    // Residency (0021): the heap arena is demand-faulted, the shared one pre-faults
    // every declared slot in `populate_hot`.
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

    // Per-call guard cost on each backing (0022 question 4).
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

    // 0023 open question 3's falsifier: guard cost on the three-edge fixture vs §11.1's.
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
