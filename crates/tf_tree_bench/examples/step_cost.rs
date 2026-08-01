//! Where does *one dynamic step* actually go? — the residual, measured.
//!
//! `docs/design/fast-path.md` §11 closes with a per-step budget whose largest
//! single line is not a measurement:
//!
//! | Term | ns/step | How it is known |
//! |---|---|---|
//! | Interpolation | ~27 | measured (`interp_cost`) |
//! | Bracket search | ~20 | measured (the `cost_model` capacity sweep) |
//! | **Slot reads, composition, bounds checks** | **~25** | **a residual** |
//!
//! A third of the budget is attributed by subtraction. That is exactly the
//! failure §11 diagnoses in its own predecessor — *"the explanation for a cost
//! needs its own measurement, separate from the measurement of the cost"* — so
//! this harness measures each term directly and then **checks that they add up**.
//!
//! # What is measured, and how it maps onto `Plan::at`
//!
//! `Plan::at` is, per dynamic step (`plan.rs`, `Guard::sample`):
//!
//! ```text
//! view.sampler(edge)          -> resolve the edge record, the claim record and
//!                                the two region sub-ranges into a `SampleRing`
//! ring.sample::<I>(t, policy) -> bracket search + one or two `read_slot`s + interp
//! acc * p   /  acc.mul_inv(p) -> compose
//! ```
//!
//! plus, once per call: `check_generation`, `check_domain`, `first_dynamic_edge`
//! and `note`. So the prediction is
//!
//! ```text
//! lookup(d) ≈ fixed + d × (sampler + sample + compose)
//! ```
//!
//! and the **residual** `measured − predicted` is the number this harness exists
//! to produce. A residual near zero means the decomposition is complete. A
//! residual that *grows with depth* means there is a per-step cost outside the
//! three terms above — which is what `Plan::at`'s two O(depth) scans
//! (`check_domain` → `has_dynamic`, and `first_dynamic_edge`) would look like,
//! since the identity-plan floor cannot see them.
//!
//! **Run pinned, or do not run it at all:**
//! `taskset -c 2 cargo run --release -p tf_tree_bench --example step_cost`
//!
//! Unpinned this migrates cores and swings by >30%, the same caveat
//! `cost_model`'s header carries and for the same reason.
//!
//! `--json <path>` writes a `runstore` run so a lever can be evaluated with
//! `bench_ab before.json after.json` rather than by eye.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Iso3, Stamp, Tree, TreeBuilder};
use tf_tree_bench::fixture::dynamic_pose;
use tf_tree_bench::report::Metric;
use tf_tree_bench::runstore::{Run, RunRow};
use tf_tree_core::arena_view::ArenaView;
use tf_tree_core::buffer::SampleRing;
use tf_tree_core::plan::Step;
use tf_tree_core::sample::ExtrapPolicy;
use tf_tree_core::EdgeId;
use tf_tree_math::{LerpSlerp, ScLerp};

/// Ring capacity for every measurement here. 4096 is `fast-path.md`'s reference
/// point, so the numbers are comparable to its tables.
const CAP: u32 = 4096;
/// Samples pushed. `retained() == capacity - 1`, so this fills without lapping.
const FILL: usize = CAP as usize - 1;
/// Iterations per timed round.
const N: usize = 8192;
/// Timed rounds; the median is reported.
const ROUNDS: usize = 41;
/// Depths swept for the reconciliation. 1 isolates the fixed cost, 6 gives the
/// residual enough leverage to show a slope.
const DEPTHS: &[usize] = &[1, 2, 3, 4, 6];

/// Median nanoseconds per iteration over [`ROUNDS`] rounds of `iters` iterations.
///
/// The closure returns an `f64` that is `black_box`ed, so every loop below has
/// to actually produce its value. Timing whole rounds rather than individual
/// operations keeps the clock out of the measurement: at ~30 ns per
/// `Instant::now` pair and ~8192 iterations a round, the clock is under 0.004 ns
/// per iteration.
fn median_ns(iters: usize, mut f: impl FnMut() -> f64) -> f64 {
    for _ in 0..5 {
        black_box(f());
    }
    let mut per_round = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        let r = f();
        let dt = t0.elapsed().as_nanos() as f64;
        black_box(r);
        per_round.push(dt / iters as f64);
    }
    per_round.sort_by(f64::total_cmp);
    per_round[per_round.len() / 2]
}

/// A chain `f0 -> f1 -> ... -> f{depth}` of dynamic edges at ring capacity
/// `cap`, filled to `cap - 1` samples at 1 kHz (the most it retains without
/// lapping). Identical construction to `cost_model::chain`, so the two
/// harnesses measure the same tree.
fn chain_cap(depth: usize, cap: u32) -> (Tree, Vec<String>) {
    let names: Vec<String> = (0..=depth).map(|i| format!("f{i}")).collect();
    let mut b = TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
    for i in 0..depth {
        b = b.dynamic_edge(&names[i], &names[i + 1], EdgeCfg::new(Capacity::slots(cap)));
    }
    let tree = b.build().expect("build chain");

    let mut writers = Vec::new();
    for i in 0..depth {
        let p = tree.frame(&names[i]).unwrap();
        let c = tree.frame(&names[i + 1]).unwrap();
        writers.push(tree.claim(c, p).unwrap());
    }
    for k in 0..cap as usize - 1 {
        let stamp = k as i64 * 1_000_000;
        for (i, w) in writers.iter().enumerate() {
            w.push(stamp, &dynamic_pose(i as f64, stamp)).unwrap();
        }
    }
    drop(writers);
    (tree, names)
}

/// [`chain_cap`] at the reference capacity.
fn chain(depth: usize) -> (Tree, Vec<String>) {
    chain_cap(depth, CAP)
}

/// Stamps landing **on** sample stamps, sweeping the filled window, so `sample`
/// takes its exact-hit path: bracket search plus one `read_slot`, no
/// interpolation.
///
/// These must be exact multiples of the 1 kHz period. An earlier revision built
/// them by subtracting 500 µs from [`swept_between`], which only lands on a
/// sample when the sweep's stride happens to be a whole millisecond — it is not
/// — so *both* sequences interpolated and the derived interpolation cost came
/// out **negative**. The reconciliation is what exposed it; the fix is to
/// generate the exact sequence first and derive the between sequence from it.
fn swept_exact_cap(cap: u32) -> Vec<i64> {
    let fill = i64::from(cap) - 1;
    (0..N as i64)
        .map(|k| (k % (fill - 2) + 1) * 1_000_000)
        .collect()
}

/// [`swept_exact_cap`] at the reference capacity.
fn swept_exact() -> Vec<i64> {
    swept_exact_cap(CAP)
}

/// The same stamps, offset half a period so the search runs *and* the
/// interpolation is performed — the case the fold pays.
fn swept_between() -> Vec<i64> {
    swept_exact().iter().map(|t| t + 500_000).collect()
}

/// Every dynamic edge the compiled plan traverses, in plan order.
fn plan_edges(plan: &tf_tree_core::plan::Plan) -> Vec<EdgeId> {
    plan.steps()
        .iter()
        .filter_map(|s| match s {
            Step::Dyn { edge, .. } => Some(*edge),
            Step::Static(_) => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The five primitives
// ---------------------------------------------------------------------------

/// Term 1 — `ArenaView::sampler`: bounds check, edge record, claim record, the
/// two region sub-range checks, and the six-field `SampleRing` construction.
///
/// Every field of the returned ring is folded into the accumulator, including
/// both slice base pointers, so the construction cannot be optimized down to
/// the discriminant.
fn t_sampler(view: &ArenaView<'_>, edges: &[EdgeId]) -> f64 {
    let iters = N * edges.len();
    median_ns(iters, || {
        let mut acc = 0u64;
        for _ in 0..N {
            for &e in edges {
                let (interp, ring) = view.sampler(black_box(e)).unwrap();
                acc ^= u64::from(interp)
                    ^ ring.mask
                    ^ (ring.stamps.as_ptr() as u64)
                    ^ (ring.poses.as_ptr() as u64);
            }
        }
        acc as f64
    })
}

/// Term 2 — `SampleRing::read_slot`: the seqlock read of one 64-byte slot.
///
/// Driven over the same physical slots the swept search reaches, so the cache
/// behaviour matches the fold's rather than being an artificial hot loop over
/// one line.
fn t_read_slot(ring: &SampleRing<'_>, stamps: &[i64]) -> f64 {
    let idx: Vec<usize> = stamps
        .iter()
        .map(|t| ((t / 1_000_000) as u64 & ring.mask) as usize)
        .collect();
    median_ns(idx.len(), || {
        let mut acc = 0.0;
        for &i in &idx {
            acc += ring.read_slot(black_box(i)).unwrap().t.x;
        }
        acc
    })
}

/// Term 3 — `Iso3 * Iso3`.
///
/// Chained (`acc = acc * b`) rather than independent, because the fold's
/// composition *is* a serial dependency chain and measuring throughput here
/// would understate what it costs there.
fn t_compose(poses: &[Iso3]) -> f64 {
    median_ns(poses.len(), || {
        let mut acc = Iso3::IDENTITY;
        for p in poses {
            acc = acc * *black_box(p);
        }
        acc.t.x
    })
}

/// Terms 4 and 5 — `SampleRing::sample`, on its exact-hit and interpolating
/// paths. The difference between them is the interpolation math *in context*,
/// which is the number `interp_cost` measures out of context.
fn t_sample(ring: &SampleRing<'_>, stamps: &[i64]) -> f64 {
    t_sample_policy(ring, stamps, ExtrapPolicy::Error)
}

/// The ring **preamble**, isolated — with no bracket search at all.
///
/// A stamp newer than the newest sample under [`ExtrapPolicy::Hold`] runs the
/// entire entry sequence — the `head` Acquire load, `retained()`, the two
/// `stamp_at` window loads and the range branches — and then reads the newest
/// slot directly. It never reaches `bracket`. So `sample_hold - read_slot` is
/// the preamble, measured rather than fitted, and `sample_exact - sample_hold`
/// is the search on its own.
///
/// This replaced a least-squares fit over the capacity sweep, which put the
/// preamble at **−22 ns**. The sweep is not log-linear — see the table it
/// prints — so a line through it has no intercept worth reading.
fn t_sample_hold(ring: &SampleRing<'_>, newer_than_window: &[i64]) -> f64 {
    t_sample_policy(ring, newer_than_window, ExtrapPolicy::Hold)
}

fn t_sample_policy(ring: &SampleRing<'_>, stamps: &[i64], policy: ExtrapPolicy) -> f64 {
    median_ns(stamps.len(), || {
        let mut acc = 0.0;
        for &t in stamps {
            if let Ok(p) = ring.sample::<LerpSlerp>(black_box(t), policy) {
                acc += p.t.x;
            }
        }
        acc
    })
}

/// What the fold actually calls per dynamic step — a byte-for-byte replica of
/// `Guard::sample`: resolve the sampler, **dispatch on the interp discriminant**,
/// then sample.
///
/// The dispatch is not bookkeeping. `cost_model`'s own header records that
/// `sample::<LerpSlerp>` and `sample::<ScLerp>` are monomorphized into the same
/// hot function behind this `match`, so writing it out here reproduces the
/// *code layout* the fold runs in — two monomorphizations reachable from one
/// call site — which calling `ring.sample::<LerpSlerp>` directly does not.
/// The gap between this and [`t_sample`] is the price of that context.
fn t_guard_sample(view: &ArenaView<'_>, edges: &[EdgeId], stamps: &[i64]) -> f64 {
    let iters = stamps.len() * edges.len();
    median_ns(iters, || {
        let mut acc = 0.0;
        for &t in stamps {
            for &e in edges {
                let (interp, ring) = view.sampler(black_box(e)).unwrap();
                let r = match InterpPolicy::from_u8(interp) {
                    InterpPolicy::LerpSlerp => {
                        ring.sample::<LerpSlerp>(black_box(t), ExtrapPolicy::Error)
                    }
                    InterpPolicy::ScLerp => {
                        ring.sample::<ScLerp>(black_box(t), ExtrapPolicy::Error)
                    }
                };
                if let Ok(p) = r {
                    acc += p.t.x;
                }
            }
        }
        acc
    })
}

/// `fold_at`, replicated in the harness: iterate the plan's `[Step; MAX_DEPTH]`
/// array, match the discriminant, sample, propagate with `?`, compose.
///
/// This exists to split the residual. [`t_guard_sample`] measures the sampling
/// work in a tight loop over one edge; the real fold does the same work while
/// walking a 1 KiB step array through a `match` and a `?` on every step. If this
/// replica lands on `Plan::at`'s measured number, the residual **is** that walk.
/// If it lands on the prediction instead, the residual is codegen context —
/// inlining and register pressure inside `tf_tree_core` — and no rearrangement
/// of the harness can find it.
fn t_fold_replica(view: &ArenaView<'_>, plan: &tf_tree_core::plan::Plan, stamps: &[i64]) -> f64 {
    median_ns(stamps.len(), || {
        let mut acc = 0.0;
        for &t in stamps {
            let mut iso = Iso3::IDENTITY;
            let mut ok = true;
            for step in plan.steps() {
                iso = match step {
                    Step::Static(m) => iso * *m,
                    Step::Dyn { edge, inverted } => {
                        let Some((interp, ring)) = view.sampler(*edge) else {
                            ok = false;
                            break;
                        };
                        let r = match InterpPolicy::from_u8(interp) {
                            InterpPolicy::LerpSlerp => {
                                ring.sample::<LerpSlerp>(black_box(t), ExtrapPolicy::Error)
                            }
                            InterpPolicy::ScLerp => {
                                ring.sample::<ScLerp>(black_box(t), ExtrapPolicy::Error)
                            }
                        };
                        match r {
                            Ok(p) => {
                                if *inverted {
                                    iso.mul_inv(&p)
                                } else {
                                    iso * p
                                }
                            }
                            Err(_) => {
                                ok = false;
                                break;
                            }
                        }
                    }
                };
            }
            if ok {
                acc += iso.t.x;
            }
        }
        acc
    })
}

/// The whole thing — `Plan::at`, which is what the terms above must sum to.
fn t_plan_at(tree: &Tree, target: &str, source: &str, stamps: &[i64]) -> f64 {
    let t = tree.frame(target).unwrap();
    let s = tree.frame(source).unwrap();
    let plan = tree.plan(t, s).unwrap();
    let guard = tree.guard();
    median_ns(stamps.len(), || {
        let mut acc = 0.0;
        for &ns in stamps {
            let stamp: Stamp = Stamp::from_nanos(black_box(ns));
            if let Ok(p) = plan.at(&guard, stamp) {
                acc += p.t.x;
            }
        }
        acc
    })
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut json: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        if a == "--json" {
            json = args.next().map(PathBuf::from);
        }
    }

    println!("tf_tree per-step cost attribution");
    println!("=================================");
    println!(
        "capacity {CAP}, {FILL} samples at 1 kHz, LerpSlerp, {N} iters/round, median of {ROUNDS}\n"
    );

    let between = swept_between();
    let exact = swept_exact();

    // One depth-1 chain supplies the isolated primitives: a single edge is all
    // any of them needs, and using the same tree for every term keeps them on
    // the same arena and the same cache footprint.
    let (tree1, names1) = chain(1);
    let f0 = tree1.frame(&names1[0]).unwrap();
    let f1 = tree1.frame(&names1[1]).unwrap();
    let plan1 = tree1.plan(f1, f0).unwrap();
    let edges1 = plan_edges(&plan1);
    let guard1 = tree1.guard();
    let view1 = guard1.view();
    let (_, ring1) = view1.sampler(edges1[0]).unwrap();

    let sampler = t_sampler(view1, &edges1);
    let read_slot = t_read_slot(&ring1, &between);
    let sample_exact = t_sample(&ring1, &exact);
    let sample_between = t_sample(&ring1, &between);
    let guard_sample = t_guard_sample(view1, &edges1, &between);
    // Past the newest sample, so `Hold` short-circuits before the search.
    let past_end = vec![FILL as i64 * 1_000_000; N];
    let sample_hold = t_sample_hold(&ring1, &past_end);

    // Poses for the composition term, read out of the ring so they are the same
    // values the fold composes.
    let poses: Vec<Iso3> = (0..N)
        .map(|k| ring1.read_slot(k & (ring1.mask as usize)).unwrap())
        .collect();
    let compose = t_compose(&poses);

    // The per-call floor: an identity plan (`lookup(x, x)`, zero steps) runs
    // `check_generation`, `check_domain`, `first_dynamic_edge` and `note` and
    // folds nothing. It therefore measures the *constant* part of the per-call
    // overhead and **not** the part that scales with plan length, which is the
    // whole point of the reconciliation below.
    let fixed = t_plan_at(&tree1, &names1[0], &names1[0], &between);

    println!("## primitives, measured directly");
    println!("{:>34} {:>12}", "term", "ns/op");
    println!("{:>34} {sampler:>12.2}", "ArenaView::sampler");
    println!("{:>34} {read_slot:>12.2}", "SampleRing::read_slot");
    println!("{:>34} {compose:>12.2}", "Iso3 * Iso3");
    println!("{:>34} {sample_hold:>12.2}", "sample (Hold, no search)");
    println!("{:>34} {sample_exact:>12.2}", "sample (exact hit)");
    println!("{:>34} {sample_between:>12.2}", "sample (interpolated)");
    println!("{:>34} {guard_sample:>12.2}", "sampler + dispatch + sample");
    println!("{:>34} {fixed:>12.2}", "Plan::at, identity plan");

    println!("\n## derived");
    let preamble = sample_hold - read_slot;
    let bracket = sample_exact - sample_hold;
    let interp = sample_between - sample_exact - read_slot;
    let dispatch = guard_sample - sampler - sample_between;
    println!(
        "{:>34} {preamble:>12.2}   sample(Hold) - read_slot",
        "ring preamble"
    );
    println!(
        "{:>34} {bracket:>12.2}   sample(exact) - sample(Hold)",
        "bracket search"
    );
    println!(
        "{:>34} {interp:>12.2}   sample(between) - sample(exact) - read_slot",
        "interpolation, in context"
    );
    println!(
        "{:>34} {dispatch:>12.2}   guard_sample - sampler - sample(between)",
        "interp-policy dispatch"
    );

    // --- the search versus capacity ----------------------------------------
    //
    // The textbook model says the search costs `log2(capacity)` dependent
    // probes, so this table should be a straight line in `log2`. **It is not**,
    // and that is the finding: the cost is nearly flat to capacity 1024 and then
    // steps hard. The break is where the ring stops fitting the cache, not where
    // the probe count changes — one more probe is ~2 ns and the step is ~20.
    //
    // Reported as measured, with no line fitted through it. An earlier revision
    // did fit one and read its intercept as the preamble; on data with a step in
    // it that produced a preamble of **−22 ns**, which is how the fit was caught.
    // The preamble now comes from `sample(Hold)` instead, which measures it.
    println!("\n## search cost vs ring capacity (depth 1, exact hits, whole window swept)");
    // The `Hold` column is the control that says **where** the cliff is. `Hold`
    // reads one pose slot and runs no search; `exact` runs the search and reads
    // one pose slot. If `Hold` stays flat while `exact` climbs, the cost is in
    // the stamp probes, and shrinking the *pose* array would buy nothing.
    println!(
        "{:>8} {:>7} {:>10} {:>10} {:>13} {:>12} {:>13}",
        "capacity", "log2", "stamps", "poses", "sample(exact)", "sample(Hold)", "marginal/log2"
    );
    let caps: &[u32] = &[64, 256, 1024, 4096, 16384];
    let mut prev: Option<(f64, f64)> = None;
    for &cap in caps {
        let (tr, nm) = chain_cap(1, cap);
        let pl = tr
            .plan(tr.frame(&nm[1]).unwrap(), tr.frame(&nm[0]).unwrap())
            .unwrap();
        let g = tr.guard();
        let (_, r) = g.view().sampler(plan_edges(&pl)[0]).unwrap();
        let ns = t_sample(&r, &swept_exact_cap(cap));
        let hold = t_sample_hold(&r, &vec![i64::from(cap) * 1_000_000; N]);
        let l2 = f64::from(cap).log2();
        let marginal = prev.map_or(f64::NAN, |(pl2, pns): (f64, f64)| (ns - pns) / (l2 - pl2));
        println!(
            "{cap:>8} {l2:>7.0} {:>9} K {:>9} K {ns:>13.2} {hold:>12.2} {marginal:>13.2}",
            u64::from(cap) * 8 / 1024,
            u64::from(cap) * 64 / 1024
        );
        prev = Some((l2, ns));
    }

    // --- the reconciliation ------------------------------------------------
    println!("\n## reconciliation: does the decomposition add up?");
    println!(
        "predicted(d) = fixed + d x (guard_sample + compose) = {fixed:.1} + d x {:.1}",
        guard_sample + compose
    );
    println!(
        "\n{:>7} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "depth", "measured", "predicted", "residual", "resid/step", "fold replica", "unexplained"
    );

    let per_step = guard_sample + compose;
    let mut rows = Vec::new();
    let mut resid_per_step = Vec::new();
    for &d in DEPTHS {
        let (tree, names) = chain(d);
        let measured = t_plan_at(&tree, &names[d], &names[0], &between);
        let predicted = fixed + d as f64 * per_step;
        let residual = measured - predicted;
        let rps = residual / d as f64;
        resid_per_step.push(rps);

        // The same plan, folded by the harness's own replica of `fold_at`.
        let t = tree.frame(&names[d]).unwrap();
        let s = tree.frame(&names[0]).unwrap();
        let pl = tree.plan(t, s).unwrap();
        let g = tree.guard();
        let replica = t_fold_replica(g.view(), &pl, &between);

        println!(
            "{d:>7} {measured:>12.1} {predicted:>12.1} {residual:>12.1} {rps:>12.2} {replica:>12.1} {:>12.1}",
            measured - replica
        );
        rows.push((d, measured, predicted, residual, replica));
    }

    // A residual that is roughly constant per step is a *per-step* cost the
    // three primitives do not contain. A residual that is constant per *lookup*
    // is a per-call cost the identity plan could not see.
    let lo = resid_per_step.iter().cloned().fold(f64::MAX, f64::min);
    let hi = resid_per_step.iter().cloned().fold(f64::MIN, f64::max);
    println!(
        "\nresidual per step across depths 1..6: {lo:.2} .. {hi:.2} ns  (spread {:.2})",
        hi - lo
    );
    println!("  `fold replica` walks the same [Step; MAX_DEPTH] array through the same match and");
    println!("  the same `?` as `fold_at`, calling the same primitives. Where it lands says what");
    println!("  the residual is:");
    println!("    replica ~= measured   -> the residual IS the step-array walk, and is attackable");
    println!("    replica ~= predicted  -> the residual is codegen context inside tf_tree_core");

    if let Some(path) = json {
        let mut run = Run::begin(1);
        let mut primitives = RunRow::new("step_cost", "chain", "tf_tree", "primitives");
        // **Directional metrics only.** `dispatch_ns` is deliberately not in
        // this list: it is a derived *difference* that measures ~0, and giving a
        // near-zero quantity a direction plus a 10% relative tolerance makes the
        // differ report a 0.5 ns wobble as a 46% regression — which it did, on
        // the first A/B run. It is emitted below as informational instead.
        for (k, v) in [
            ("sampler_ns", sampler),
            ("read_slot_ns", read_slot),
            ("compose_ns", compose),
            ("sample_exact_ns", sample_exact),
            ("sample_between_ns", sample_between),
            ("guard_sample_ns", guard_sample),
            ("fixed_per_call_ns", fixed),
            ("bracket_ns", bracket),
            ("interp_in_context_ns", interp),
            ("sample_hold_ns", sample_hold),
            ("ring_preamble_ns", preamble),
        ] {
            primitives = primitives.metric(Metric::new(k, v, "ns").lower_is_better(0.10));
        }
        primitives = primitives.metric(Metric::new("dispatch_ns", dispatch, "ns"));
        run.push(primitives);

        for (d, measured, predicted, residual, replica) in rows {
            run.push(
                RunRow::new("step_cost", "chain", "tf_tree", format!("depth={d}"))
                    .metric(Metric::new("lookup_ns", measured, "ns").lower_is_better(0.10))
                    .metric(Metric::new("fold_replica_ns", replica, "ns").lower_is_better(0.10))
                    .metric(Metric::new("predicted_ns", predicted, "ns"))
                    .metric(Metric::new("residual_ns", residual, "ns"))
                    .metric(Metric::new(
                        "residual_per_step_ns",
                        residual / d as f64,
                        "ns",
                    )),
            );
        }
        run.write(&path).expect("write run json");
        println!("\nwrote {}", path.display());
    }
}
