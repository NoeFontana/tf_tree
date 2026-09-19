//! Where does *one dynamic step* actually go? — the residual, measured.
//!
//! `docs/design/fast-path.md` §11 closes with a per-step budget whose largest line is a residual:
//!
//! | Term | ns/step | How it is known |
//! |---|---|---|
//! | Interpolation | ~27 | measured (`interp_cost`) |
//! | Bracket search | ~20 | measured (the `cost_model` capacity sweep) |
//! | **Slot reads, composition, bounds checks** | **~25** | **a residual** |
//!
//! This harness measures each term directly and checks that they add up.
//!
//! # What is measured
//!
//! Per dynamic step `Plan::at` runs `view.sampler(edge)`, `ring.sample::<I>(t, policy)` and a compose:
//!
//! ```text
//! lookup(d) ≈ fixed + d × (sampler + sample + compose)
//! ```
//!
//! The **residual** `measured − predicted` is what this harness produces; one that grows with depth is a
//! per-step cost outside the three terms (e.g. `check_domain`, `first_dynamic_edge`).
//!
//! **Run pinned:** `taskset -c 2 cargo run --release -p tf_tree_bench --example step_cost`. Unpinned it
//! swings >30%. `--json <path>` writes a `runstore` run for `bench_ab before.json after.json`.
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

/// Ring capacity: `fast-path.md`'s reference point.
const CAP: u32 = 4096;
const FILL: usize = CAP as usize - 1;
const N: usize = 8192;
const ROUNDS: usize = 41;
const DEPTHS: &[usize] = &[1, 2, 3, 4, 6];

/// Median ns per iteration over [`ROUNDS`] rounds; the closure's `f64` is `black_box`ed.
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

/// A chain `f0 -> ... -> f{depth}` of dynamic edges at capacity `cap`; identical to `cost_model::chain`.
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

fn chain(depth: usize) -> (Tree, Vec<String>) {
    chain_cap(depth, CAP)
}

/// Stamps landing on sample stamps (the exact-hit path). Derive the between sequence from this one, or both
/// interpolate.
fn swept_exact_cap(cap: u32) -> Vec<i64> {
    let fill = i64::from(cap) - 1;
    (0..N as i64)
        .map(|k| (k % (fill - 2) + 1) * 1_000_000)
        .collect()
}

fn swept_exact() -> Vec<i64> {
    swept_exact_cap(CAP)
}

/// The same stamps offset half a period, so the search and the interpolation both run.
fn swept_between() -> Vec<i64> {
    swept_exact().iter().map(|t| t + 500_000).collect()
}

fn plan_edges(plan: &tf_tree_core::plan::Plan) -> Vec<EdgeId> {
    plan.steps()
        .iter()
        .filter_map(|s| match s {
            Step::Dyn { edge, .. } => Some(*edge),
            Step::Static(_) => None,
        })
        .collect()
}

/// Term 1 — `ArenaView::sampler`; every ring field is folded into the accumulator so it is kept.
fn t_sampler(view: &ArenaView<'_>, edges: &[EdgeId]) -> f64 {
    let iters = N * edges.len();
    median_ns(iters, || {
        let mut acc = 0u64;
        for _ in 0..N {
            for &e in edges {
                let (interp, ring) = view.sampler(black_box(e)).unwrap();
                acc ^= u64::from(interp)
                    ^ ring.mask()
                    ^ (ring.stamps.as_ptr() as u64)
                    ^ (ring.poses.as_ptr() as u64);
            }
        }
        acc as f64
    })
}

/// Term 2 — `SampleRing::read_slot` over the slots the swept search reaches.
fn t_read_slot(ring: &SampleRing<'_>, stamps: &[i64]) -> f64 {
    let idx: Vec<usize> = stamps
        .iter()
        .map(|t| ((t / 1_000_000) as u64 & ring.mask()) as usize)
        .collect();
    median_ns(idx.len(), || {
        let mut acc = 0.0;
        for &i in &idx {
            acc += ring.read_slot(black_box(i)).unwrap().t.x;
        }
        acc
    })
}

/// Term 3 — `Iso3 * Iso3` chained, since the fold's composition is a serial dependency chain.
fn t_compose(poses: &[Iso3]) -> f64 {
    median_ns(poses.len(), || {
        let mut acc = Iso3::IDENTITY;
        for p in poses {
            acc = acc * *black_box(p);
        }
        acc.t.x
    })
}

/// Terms 4 and 5 — `SampleRing::sample` on its exact-hit and interpolating paths; the difference is the
/// interpolation in context.
fn t_sample(ring: &SampleRing<'_>, stamps: &[i64]) -> f64 {
    t_sample_policy(ring, stamps, ExtrapPolicy::Error)
}

/// The ring preamble without a bracket search (newest stamp under `ExtrapPolicy::Hold`):
/// `sample_hold - read_slot` is the preamble, `sample_exact - sample_hold` the search.
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

/// A replica of `Guard::sample`: sampler, **dispatch on the interp discriminant**, then sample; the gap to
/// [`t_sample`] is the dispatch's price.
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

/// `d` independent samples of one ring: the ILP control.
fn t_ilp_control(view: &ArenaView<'_>, edge: EdgeId, stamps: &[i64], d: usize) -> f64 {
    let groups = stamps.len() / d;
    median_ns(groups * d, || {
        let mut acc = 0.0;
        for grp in stamps.chunks_exact(d) {
            for &t in grp {
                let (interp, ring) = view.sampler(black_box(edge)).unwrap();
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

/// `fold_at` replicated: walk the `[Step; MAX_DEPTH]` array, match, sample, `?`, compose. Landing on
/// `Plan::at`'s number means the residual is the step-array walk.
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

fn monotone_between() -> Vec<i64> {
    let span = FILL as i64 - 3;
    (0..N as i64)
        .map(|k| (1 + k * span / N as i64) * 1_000_000 + 500_000)
        .collect()
}

/// `sample_from` (the galloping cursor) against `sample`, on a monotone sweep; bounds the win of a cursor
/// in scalar `Plan::at` (`fold_at_cursors` already uses it).
fn t_sample_cursor(ring: &SampleRing<'_>, stamps: &[i64]) -> f64 {
    median_ns(stamps.len(), || {
        let mut acc = 0.0;
        let mut cursor = 0u64;
        for &t in stamps {
            if let Ok(p) =
                ring.sample_from::<LerpSlerp>(black_box(t), ExtrapPolicy::Error, &mut cursor)
            {
                acc += p.t.x;
            }
        }
        acc
    })
}

/// The fold over a compact one-`u32`-per-step encoding, bounding the win of shrinking `Step`. Otherwise
/// identical to [`t_fold_replica`]. Harness-only: it would change what `Plan` is.
fn t_fold_compact(view: &ArenaView<'_>, plan: &tf_tree_core::plan::Plan, stamps: &[i64]) -> f64 {
    const DYN: u32 = 1 << 31;
    const INV: u32 = 1 << 30;
    let mut ops = [0u32; 16];
    let mut statics: Vec<Iso3> = Vec::new();
    for (i, step) in plan.steps().iter().enumerate() {
        ops[i] = match step {
            Step::Dyn { edge, inverted } => DYN | if *inverted { INV } else { 0 } | edge.0,
            Step::Static(m) => {
                statics.push(*m);
                (statics.len() - 1) as u32
            }
        };
    }
    let n = plan.steps().len();

    median_ns(stamps.len(), || {
        let mut acc = 0.0;
        for &t in stamps {
            let mut iso = Iso3::IDENTITY;
            let mut ok = true;
            for &op in &ops[..n] {
                if op & DYN == 0 {
                    iso = iso * statics[op as usize];
                    continue;
                }
                let Some((interp, ring)) = view.sampler(EdgeId(op & !(DYN | INV))) else {
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
                        iso = if op & INV != 0 {
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
            if ok {
                acc += iso.t.x;
            }
        }
        acc
    })
}

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
    let past_end = vec![FILL as i64 * 1_000_000; N];
    let sample_hold = t_sample_hold(&ring1, &past_end);

    let poses: Vec<Iso3> = (0..N)
        .map(|k| ring1.read_slot(k & (ring1.mask() as usize)).unwrap())
        .collect();
    let compose = t_compose(&poses);

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

    //
    // `t_guard_sample` samples `edges.len()` independent edges per stamp; a cost falling with `d` means overlap.
    println!("\n## available ILP: d independent samples per stamp (capacity {CAP})");
    println!("{:>10} {:>16} {:>14}", "d (edges)", "ns/sample", "vs d=1");
    let mut ilp_base = f64::NAN;
    for &d in DEPTHS {
        let (tr, nm) = chain(d);
        let pl = tr
            .plan(tr.frame(&nm[d]).unwrap(), tr.frame(&nm[0]).unwrap())
            .unwrap();
        let g = tr.guard();
        let es = plan_edges(&pl);
        let ns = t_guard_sample(g.view(), &es, &between);
        if d == 1 {
            ilp_base = ns;
        }
        println!("{d:>10} {ns:>16.2} {:>13.2}x", ns / ilp_base);
    }
    println!("  flat  -> the OoO engine already overlaps them; Lever 2 has nothing to win");
    println!("  falls -> the serial fold is leaving that overlap on the table");

    // Control: one ring sampled `d` times, so the working set does not grow with `d`.
    println!("\n## the control: d independent samples of ONE ring (footprint fixed)");
    println!("{:>10} {:>16} {:>14}", "d (repeats)", "ns/sample", "vs d=1");
    let mut ctl_base = f64::NAN;
    for &d in DEPTHS {
        let ns = t_ilp_control(view1, edges1[0], &between, d);
        if d == 1 {
            ctl_base = ns;
        }
        println!("{d:>10} {ns:>16.2} {:>13.2}x", ns / ctl_base);
    }

    println!("\n## the galloping cursor vs a fresh search (depth 1, monotone sweep)");
    println!("{:>34} {:>12} {:>10}", "path", "ns/sample", "vs fresh");
    let mono = monotone_between();
    let fresh_mono = t_sample(&ring1, &mono);
    let cursor_mono = t_sample_cursor(&ring1, &mono);
    println!(
        "{:>34} {fresh_mono:>12.2} {:>9.2}x",
        "sample (fresh search)", 1.0
    );
    println!(
        "{:>34} {cursor_mono:>12.2} {:>9.2}x",
        "sample_from (cursor)",
        cursor_mono / fresh_mono
    );
    let (trb, nmb) = chain_cap(1, 16_384);
    let plb = trb
        .plan(trb.frame(&nmb[1]).unwrap(), trb.frame(&nmb[0]).unwrap())
        .unwrap();
    let gb = trb.guard();
    let (_, rb) = gb.view().sampler(plan_edges(&plb)[0]).unwrap();
    let span_b = 16_384i64 - 4;
    let mono_b: Vec<i64> = (0..N as i64)
        .map(|k| (1 + k * span_b / N as i64) * 1_000_000 + 500_000)
        .collect();
    let fresh_b = t_sample(&rb, &mono_b);
    let cursor_b = t_sample_cursor(&rb, &mono_b);
    println!("\n  at capacity 16384 (128 KiB of stamps — a 1 kHz edge, 10 s history):");
    println!(
        "{:>34} {fresh_b:>12.2} {:>9.2}x",
        "sample (fresh search)", 1.0
    );
    println!(
        "{:>34} {cursor_b:>12.2} {:>9.2}x",
        "sample_from (cursor)",
        cursor_b / fresh_b
    );

    // --- the search versus capacity ---
    //
    // Nearly flat to 1024, then steps where the ring stops fitting the cache; reported with no fit. `Hold`
    // reads one slot with no search, `exact` searches: flat `Hold` with climbing `exact` puts the cost in the probes.
    println!("\n## search cost vs ring capacity (depth 1, exact hits, whole window swept)");
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

    println!("\n## reconciliation: does the decomposition add up?");
    println!(
        "predicted(d) = fixed + d x (guard_sample + compose) = {fixed:.1} + d x {:.1}",
        guard_sample + compose
    );
    println!(
        "\n{:>7} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "depth", "measured", "predicted", "residual", "resid/step", "fold replica", "compact walk"
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

        let t = tree.frame(&names[d]).unwrap();
        let s = tree.frame(&names[0]).unwrap();
        let pl = tree.plan(t, s).unwrap();
        let g = tree.guard();
        let replica = t_fold_replica(g.view(), &pl, &between);
        let compact = t_fold_compact(g.view(), &pl, &between);

        println!(
            "{d:>7} {measured:>12.1} {predicted:>12.1} {residual:>12.1} {rps:>12.2} {replica:>12.1} {compact:>12.1}",
        );
        rows.push((d, measured, predicted, residual, replica, compact));
    }

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
        // Directional metrics only: `dispatch_ns` is a difference near 0, emitted as informational.
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

        for (d, measured, predicted, residual, replica, compact) in rows {
            run.push(
                RunRow::new("step_cost", "chain", "tf_tree", format!("depth={d}"))
                    .metric(Metric::new("lookup_ns", measured, "ns").lower_is_better(0.10))
                    .metric(Metric::new("fold_replica_ns", replica, "ns").lower_is_better(0.10))
                    .metric(Metric::new("fold_compact_ns", compact, "ns").lower_is_better(0.10))
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
