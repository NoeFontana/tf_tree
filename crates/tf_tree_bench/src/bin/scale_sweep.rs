//! Where does tf_tree bend, and where does it break? Four questions:
//!
//! 1. Does lookup cost depend on tree size or only depth? Sweeps width to 12 000
//!    frames with the dynamic-step count fixed (`docs/PHASE1.md` §11.3).
//! 2. What do plan compilation and `Tree::build` cost?
//! 3. Does the arena cost what `ArenaLayout` says? Checked against a measured
//!    RSS delta (`report::worse_entries`).
//! 4. Where are the walls (`BuildError::TooManyFrames`, `TooManyEdges`,
//!    `LayoutError::ArenaTooLarge`)?
//!
//! Wall-clock rows are indicative on any host; ratios, counts and byte/slot
//! figures are exact.
//!
//! Usage: `just scale-sweep`, or `scale_sweep --json out.json`.
// This binary's output IS its result.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Plan, Stamp, TreeBuilder};
use tf_tree_bench::mp::{Histogram, ProcStats};
use tf_tree_bench::report::Metric;
use tf_tree_bench::runstore::{Run, RunRow};
use tf_tree_bench::workload::{self, Backing, Built};

/// Lookups per timed sample set: loop dominates the clock pair, row under 1 s.
const LOOKUPS: usize = 200_000;
/// Distinct stamps swept, so the bracket search does real work.
const STAMP_STEPS: usize = 1024;
/// A/B differ slack on wall-clock rows: wide, rows are indicative.
const TIME_SLACK: f64 = 0.35;

fn main() {
    let mut json: Option<PathBuf> = None;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--json" => {
                json = Some(PathBuf::from(argv.get(i + 1).expect("--json needs a path")));
                i += 2;
            }
            "-h" | "--help" => {
                println!("usage: scale_sweep [--json PATH]");
                return;
            }
            other => {
                eprintln!("scale_sweep: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }

    let mut run = Run::begin(1);

    println!("tf_tree scale sweep");
    println!("===================");
    println!(
        "Wall-clock rows are INDICATIVE on this host: {}",
        if run.fitness.fair_for_timing {
            "the fitness probe passed".to_owned()
        } else {
            run.fitness.reasons.join("; ")
        }
    );
    println!("Byte, slot and step counts are arithmetic and are exact everywhere.");
    println!();

    catalogue_sweep(&mut run);
    println!();
    ring_sweep(&mut run);
    println!();
    publish_sweep(&mut run);
    println!();
    limits();

    if let Some(path) = &json {
        match run.write(path) {
            Ok(()) => println!("\nwrote {}", path.display()),
            Err(e) => {
                eprintln!("scale_sweep: {e:#}");
                std::process::exit(1);
            }
        }
    }
}

fn catalogue_sweep(run: &mut Run) {
    println!("Per-workload: lookup cost, what the tree itself costs, and memory");
    println!(
        "{:<14} {:>7} {:>7} {:>6} | {:>8} {:>9} {:>10} | {:>9} | {:>9} {:>9} {:>8}",
        "workload",
        "frames",
        "edges",
        "steps",
        "at p50",
        "at p99.9",
        "common p50",
        "compile",
        "build ms",
        "arena MiB",
        "Pss/arena"
    );
    println!(
        "{:<14} {:>7} {:>7} {:>6} | {:>29} | {:>9} |",
        "", "", "", "", "nanoseconds", "ns"
    );

    for w in workload::CATALOGUE {
        let Ok(estimate) = w.estimate() else {
            println!("{:<14} (estimate refused this workload)", w.name);
            continue;
        };

        let rss_before = ProcStats::read().pss_kib;
        let t0 = Instant::now();
        let built = match w.build(InterpPolicy::LerpSlerp, Backing::Heap) {
            Ok(b) => b,
            Err(e) => {
                println!("{:<14} build failed: {e:#}", w.name);
                continue;
            }
        };
        let build_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let rss_after = ProcStats::read().pss_kib;

        let plans = built.plans().expect("compile query pairs");
        let steps = built.shape.dyn_steps.unwrap_or(0);

        let at = time_at(&built, &plans);
        let common = time_latest_common(&built, &plans);
        let compile_ns = time_compile(&built);

        let rss_delta_kib = rss_after.saturating_sub(rss_before);
        let rss_ratio = if built.shape.arena_bytes == 0 {
            f64::NAN
        } else {
            (rss_delta_kib as f64 * 1024.0) / built.shape.arena_bytes as f64
        };

        println!(
            "{:<14} {:>7} {:>7} {:>6} | {:>8.0} {:>9.0} {:>10.0} | {:>9.0} | {:>9.2} \
             {:>9.1} {:>8.2}",
            w.name,
            estimate.frames,
            estimate.edges,
            steps,
            at.0,
            at.1,
            common,
            compile_ns,
            build_ms,
            estimate.arena_bytes as f64 / (1024.0 * 1024.0),
            rss_ratio,
        );

        run.push(
            RunRow::new("scale_sweep", w.name, "tf_tree", "catalogue")
                .with_shape(built.shape)
                .metric(Metric::new("at_p50_ns", at.0, "ns").lower_is_better(TIME_SLACK))
                .metric(Metric::new("at_p99_9_ns", at.1, "ns").lower_is_better(TIME_SLACK))
                .metric(
                    Metric::new("latest_common_p50_ns", common, "ns").lower_is_better(TIME_SLACK),
                )
                .metric(
                    Metric::new("plan_compile_ns", compile_ns, "ns").lower_is_better(TIME_SLACK),
                )
                .metric(Metric::new("build_ms", build_ms, "ms").lower_is_better(TIME_SLACK))
                .metric(
                    Metric::new("arena_bytes", built.shape.arena_bytes as f64, "B")
                        .lower_is_better(0.01),
                )
                // The id says `rss` but the instrument is Pss; ids are join keys
                // (`runstore::Run::key`), so it is frozen.
                .metric(Metric::new("rss_over_arena", rss_ratio, "x"))
                .metric(Metric::new("dyn_steps", steps as f64, "steps")),
        );
    }

    println!();
    println!("`steps` is the post-folding dynamic-step count of the deepest query — the number");
    println!("`docs/PHASE1.md` §11.3 says a latency row is uninterpretable without. Compare rows");
    println!("with equal `steps` to read the width axis; compare `humanoid` against `av` to read");
    println!("depth. `common p50` is `latest_common`, whose cost should grow with the tree where");
    println!("`at` should not.");
}

/// `(p50, p99.9)` of `Plan::at`, in nanoseconds.
fn time_at(built: &Built, plans: &[Plan]) -> (f64, f64) {
    let stamps = sweep(built);
    let guard = built.tree.guard();
    let mut hist = Histogram::new();
    let mut acc = 0.0f64;

    for p in plans {
        let warm: Stamp = Stamp::from_nanos(stamps[0]);
        let _ = p.at(&guard, warm);
    }

    for k in 0..LOOKUPS {
        let stamp: Stamp = Stamp::from_nanos(stamps[k % stamps.len()]);
        let plan = &plans[k % plans.len()];
        let t0 = Instant::now();
        if let Ok(p) = plan.at(&guard, stamp) {
            acc += p.t.x;
        }
        hist.record(t0.elapsed().as_nanos() as u64);
    }
    black_box(acc);
    (hist.quantile(0.50) as f64, hist.quantile(0.999) as f64)
}

/// p50 of `Plan::latest_common`, whose cost should grow with the tree.
fn time_latest_common(built: &Built, plans: &[Plan]) -> f64 {
    let guard = built.tree.guard();
    let mut hist = Histogram::new();
    let mut acc = 0.0f64;
    for k in 0..LOOKUPS / 10 {
        let plan = &plans[k % plans.len()];
        let t0 = Instant::now();
        if let Ok(p) = plan.latest_common(&guard) {
            acc += p.t.x;
        }
        hist.record(t0.elapsed().as_nanos() as u64);
    }
    black_box(acc);
    hist.quantile(0.50) as f64
}

/// Mean nanoseconds to compile one plan: the row that should scale with the tree.
fn time_compile(built: &Built) -> f64 {
    let (target, source) = &built.pairs[0];
    let t = built.tree.frame(target).expect("target frame");
    let s = built.tree.frame(source).expect("source frame");

    const ITERS: usize = 2_000;
    let start = Instant::now();
    for _ in 0..ITERS {
        black_box(built.tree.plan(t, s).expect("compile"));
    }
    start.elapsed().as_nanos() as f64 / ITERS as f64
}

fn sweep(built: &Built) -> Vec<i64> {
    let (lo, hi) = built.window;
    (0..STAMP_STEPS)
        .map(|k| lo + (hi - lo) * k as i64 / STAMP_STEPS as i64)
        .collect()
}

/// Ring sizes to sweep, in slots (powers of two: `Capacity` rounds up).
const RING_SLOTS: &[u32] = &[8, 64, 1024, 16_384, 262_144, 1_048_576];

/// How lookup cost moves with the depth of one ring: logarithmic in theory.
fn ring_sweep(run: &mut Run) {
    println!("Ring depth: one dynamic edge, query stamps swept across the whole ring");
    println!(
        "{:>10} {:>10} {:>10} | {:>9} {:>10} | {:>10}",
        "slots", "retained", "MiB", "at p50", "at p99.9", "ns/doubling"
    );

    let mut prev: Option<(u32, f64)> = None;
    for &slots in RING_SLOTS {
        let tree = TreeBuilder::new()
            .default_interp(InterpPolicy::LerpSlerp)
            .dynamic_edge("a", "b", EdgeCfg::new(Capacity::slots(slots)))
            .build()
            .expect("build one-edge tree");

        let a = tree.frame("a").expect("a");
        let b = tree.frame("b").expect("b");
        let w = tree.claim(b, a).expect("claim");

        let retained = slots - 1;
        let step_ns = 1_000_000i64;
        for k in 0..u64::from(retained) {
            w.push(
                k as i64 * step_ns,
                &tf_tree_bench::fixture::dynamic_pose(1.0, k as i64),
            )
            .expect("push");
        }
        drop(w);

        let t = tree.frame("b").expect("b");
        let s = tree.frame("a").expect("a");
        let plan = tree.plan(t, s).expect("plan");
        let guard = tree.guard();

        let lo = 0i64;
        let hi = i64::from(retained - 1) * step_ns;
        let stamps: Vec<i64> = (0..STAMP_STEPS)
            .map(|k| lo + (hi - lo) * k as i64 / STAMP_STEPS as i64)
            .collect();

        let mut hist = Histogram::new();
        let mut acc = 0.0f64;
        let warm: Stamp = Stamp::from_nanos(stamps[0]);
        let _ = plan.at(&guard, warm);
        for k in 0..LOOKUPS {
            let stamp: Stamp = Stamp::from_nanos(stamps[k % stamps.len()]);
            let t0 = Instant::now();
            if let Ok(p) = plan.at(&guard, stamp) {
                acc += p.t.x;
            }
            hist.record(t0.elapsed().as_nanos() as u64);
        }
        black_box(acc);

        let p50 = hist.quantile(0.50) as f64;
        let per_doubling = prev.map_or(f64::NAN, |(pslots, pp50)| {
            let doublings = (f64::from(slots) / f64::from(pslots)).log2();
            (p50 - pp50) / doublings
        });
        prev = Some((slots, p50));

        // 8 bytes of stamp + 64 of pose per slot.
        let mib = f64::from(slots) * 72.0 / (1024.0 * 1024.0);
        println!(
            "{slots:>10} {retained:>10} {mib:>10.1} | {p50:>9.0} {:>10.0} | {:>10.1}",
            hist.quantile(0.999) as f64,
            per_doubling,
        );

        run.push(
            RunRow::new("scale_sweep", "ring", "tf_tree", format!("slots={slots}"))
                .metric(Metric::new("at_p50_ns", p50, "ns").lower_is_better(TIME_SLACK))
                .metric(
                    Metric::new("at_p99_9_ns", hist.quantile(0.999) as f64, "ns")
                        .lower_is_better(TIME_SLACK),
                )
                .metric(Metric::new("ns_per_doubling", per_doubling, "ns")),
        );
    }
    println!();
    println!("A flat `ns/doubling` is the binary search behaving; a rising one is the cache");
    println!("giving out, which is a property of the machine as much as of the engine.");
}

/// Concurrent writer-edge counts to sweep.
const WRITER_EDGES: &[usize] = &[1, 16, 64, 256];

/// Publishing cost should not depend on the edge count (`EdgeCounters` is padded
/// to 128 bytes); this row catches lost padding.
fn publish_sweep(run: &mut Run) {
    println!("Publish: one thread, round-robin over N edges");
    println!("{:>10} | {:>12} {:>12}", "edges", "ns/push", "Mpush/s");

    for &n in WRITER_EDGES {
        let mut b = TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
        for i in 0..n {
            b = b.dynamic_edge(
                "root",
                &format!("e{i}"),
                EdgeCfg::new(Capacity::slots(1024)),
            );
        }
        let tree = b.build().expect("build publish tree");
        let root = tree.frame("root").expect("root");
        let writers: Vec<_> = (0..n)
            .map(|i| {
                let c = tree.frame(&format!("e{i}")).expect("edge frame");
                tree.claim(c, root).expect("claim")
            })
            .collect();

        let pose = tf_tree_bench::fixture::dynamic_pose(1.0, 0);
        const PUSHES: usize = 500_000;
        // Warm every ring so first-touch faults are not charged.
        for (i, w) in writers.iter().enumerate() {
            w.push(i as i64, &pose).expect("warm push");
        }

        let start = Instant::now();
        for k in 0..PUSHES {
            let w = &writers[k % n];
            let _ = w.push(k as i64 + 1_000_000, &pose);
        }
        let elapsed = start.elapsed();
        let ns_per_push = elapsed.as_nanos() as f64 / PUSHES as f64;

        println!(
            "{n:>10} | {ns_per_push:>12.2} {:>12.2}",
            PUSHES as f64 / elapsed.as_secs_f64() / 1e6
        );

        run.push(
            RunRow::new("scale_sweep", "publish", "tf_tree", format!("edges={n}"))
                .metric(Metric::new("ns_per_push", ns_per_push, "ns").lower_is_better(TIME_SLACK)),
        );
    }
    println!();
    println!("A flat column is per-edge isolation holding. A rising one means writes to");
    println!("different edges are sharing a cache line, which is what EdgeCounters' 128-byte");
    println!("padding exists to prevent.");
}

/// Print the limits by asking for something too big and reporting the refusal;
/// nothing allocates the arena asked about.
fn limits() {
    println!("Limits, as reported by the engine itself");

    let mut lo = 1u32;
    let mut hi = u32::MAX / 64;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if tf_tree_arena::ArenaLayout::from_totals(64, 64, mid).is_ok() {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    println!(
        "  sample slots in one arena:   {lo} (~{:.2} GiB) — LayoutError::ArenaTooLarge \
         beyond it,\n\
         {:31}because every region offset in the header is a u32",
        f64::from(lo) * 72.0 / (1024.0 * 1024.0 * 1024.0),
        "",
    );

    // Two depth constants since `0034`: print both.
    println!(
        "  path edges walked:           {} — LookupError::TreeTooDeep beyond it.\n\
         {:31}This is what a tf2 tree's own depth is measured against.\n\
           compiled plan steps:         {} — LookupError::TreeTooDeep beyond it,\n\
         {:31}counted *after* adjacent static links fold into one step, so a\n\
         {:31}rigid chain costs one step however long it is.",
        tf_tree::MAX_PATH_EDGES,
        "",
        tf_tree::MAX_DEPTH,
        "",
        "",
    );

    println!(
        "  frames / edges:              u32 each (BuildError::TooManyFrames / TooManyEdges),\n\
         {:31}but the slot ceiling above is reached first on any populated tree",
        "",
    );
}
