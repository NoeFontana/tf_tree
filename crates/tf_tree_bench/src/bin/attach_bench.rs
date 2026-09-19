//! `docs/PHASE2.md` §12's attach rows: attach time (cold and warm, p50) and first access after attach.
//!
//! `Tree::attach_shared` maps the segment, validates the header, claims a slot and calls `populate_hot()`,
//! which (`0024`) warms the tables, not the ring arenas; rings show in the `plan compile` row. No
//! "population on vs off" row: `populate_hot()` is unconditional. `cold` is the first attach in this
//! process, not a cold page cache, so it bounds warm from above.

#![allow(clippy::print_stdout)]

use anyhow::{anyhow, Context, Result};

use tf_tree::{AttachMode, InterpPolicy, Stamp, Tree};

const CYCLES: usize = 201;

/// The page size `docs/PHASE2.md` §12.2's arithmetic divides by; constant because `sysconf` needs `unsafe`.
const PAGE_BYTES: usize = 4096;

/// The pair every other harness measures, so first-access is comparable with steady state.
const TARGET: &str = "imu_link";
const SOURCE: &str = "map";

const STAMP_NS: i64 = tf_tree_bench::fixture::NOW_NS - 3_700_000;

fn main() -> Result<()> {
    let owner = build_owner()?;
    let fd = owner
        .shared_fd()
        .ok_or_else(|| anyhow!("the fixture arena is not shared — build it with `shm`"))?;

    let mut attach_ns = Vec::with_capacity(CYCLES);
    let mut plan_ns = Vec::with_capacity(CYCLES);
    let mut replan_ns = Vec::with_capacity(CYCLES);
    let mut first_at_ns = Vec::with_capacity(CYCLES);

    for _ in 0..CYCLES {
        let dup = fd
            .try_clone_to_owned()
            .context("duplicating the segment fd")?;

        let t0 = std::time::Instant::now();
        let tree = Tree::attach_shared(dup, AttachMode::ReadOnly)
            .map_err(|e| anyhow!("attaching: {e:?}"))?;
        let a = t0.elapsed().as_nanos();

        let t1 = std::time::Instant::now();
        let target = tree
            .frame(TARGET)
            .map_err(|e| anyhow!("frame `{TARGET}`: {e:?}"))?;
        let source = tree
            .frame(SOURCE)
            .map_err(|e| anyhow!("frame `{SOURCE}`: {e:?}"))?;
        let plan = tree
            .plan(target, source)
            .map_err(|e| anyhow!("compiling {SOURCE} <- {TARGET}: {e:?}"))?;
        let p = t1.elapsed().as_nanos();

        let guard = tree.guard();
        let stamp = Stamp::<tf_tree::SystemDomain>::from_nanos(STAMP_NS);
        let t2 = std::time::Instant::now();
        let got = plan.at(&guard, stamp);
        let f = t2.elapsed().as_nanos();
        got.map_err(|e| anyhow!("the first lookup after attach was refused: {e:?}"))?;

        attach_ns.push(a as f64);
        plan_ns.push(p as f64);
        first_at_ns.push(f as f64);

        drop(guard);
        drop(tree);
    }

    // A separate pass: a fourth timer in the loop above shifted `first lookup after attach` (130 to 210 ns).
    for _ in 0..CYCLES {
        let dup = fd
            .try_clone_to_owned()
            .context("duplicating the segment fd")?;
        let tree = Tree::attach_shared(dup, AttachMode::ReadOnly)
            .map_err(|e| anyhow!("attaching: {e:?}"))?;
        let target = tree
            .frame(TARGET)
            .map_err(|e| anyhow!("frame `{TARGET}`: {e:?}"))?;
        let source = tree
            .frame(SOURCE)
            .map_err(|e| anyhow!("frame `{SOURCE}`: {e:?}"))?;
        let _ = tree
            .plan(target, source)
            .map_err(|e| anyhow!("compiling {SOURCE} <- {TARGET}: {e:?}"))?;
        let t1b = std::time::Instant::now();
        let _ = tree
            .plan(target, source)
            .map_err(|e| anyhow!("recompiling {SOURCE} <- {TARGET}: {e:?}"))?;
        let pw = t1b.elapsed().as_nanos();

        replan_ns.push(pw as f64);
        drop(tree);
    }

    report(
        owner.arena_size_bytes(),
        &attach_ns,
        &plan_ns,
        &replan_ns,
        &first_at_ns,
    );
    Ok(())
}

/// The §11.1 fixture on a shared `memfd`, published and held open.
fn build_owner() -> Result<Tree> {
    let mut b = tf_tree::TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
    for e in tf_tree_bench::fixture::EDGES {
        b = match e.kind {
            tf_tree_bench::fixture::EdgeDefKind::Static { xi } => {
                b.static_edge(e.parent, e.child, &tf_tree_math::exp_se3(xi))
            }
            tf_tree_bench::fixture::EdgeDefKind::Dynamic { rate_hz } => b.dynamic_edge(
                e.parent,
                e.child,
                tf_tree::EdgeCfg::new(tf_tree::Capacity::history(
                    rate_hz,
                    tf_tree_bench::fixture::HISTORY_SECS,
                )),
            ),
        };
    }
    let tree = b
        .build_shared("tf_tree_attach_bench")
        .map_err(|e| anyhow!("building the shared fixture: {e:?}"))?;
    // Leak the writers so claims and history stay live for every attach.
    let (writers, samples) = tf_tree_bench::fixture::spin_up(&tree)?;
    core::mem::forget(writers);
    drop(samples);
    Ok(tree)
}

fn report(arena_bytes: usize, attach: &[f64], plan: &[f64], replan: &[f64], first: &[f64]) {
    println!("PHASE2 §12 — attach time, and first access after attach");
    println!("  §11.1 fixture on a memfd, {CYCLES} attach/lookup cycles, ReadOnly");
    println!(
        "  arena {arena_bytes} B = {} pages of {PAGE_BYTES} B",
        arena_bytes.div_ceil(PAGE_BYTES)
    );
    println!();
    println!(
        "  {:<28} {:>10} {:>10} {:>10} {:>10}",
        "", "cold", "p50", "p99", "p99.9"
    );
    row("attach (map+validate+populate)", attach);
    row("plan compile (first, populates)", plan);
    row("plan compile (repeat, warm)", replan);
    row("first lookup after attach", first);

    println!();
    println!(
        "  cold is the first cycle: fresh VMA and page tables, allocator not warm.\n  \
         It is NOT a cold page cache — the creator wrote this arena moments ago and\n  \
         dropping caches needs root — so it bounds the warm case from above and a\n  \
         genuinely cold attach from below."
    );
    println!();
    println!(
        "  The `population on vs off` half of §12's row is NOT here: `populate_hot()`\n  \
         is unconditional inside `attach_shared_inner`, and inventing an `off` arm out\n  \
         of a different code path would be worse than saying so. It arrives with\n  \
         `0022`'s B2-prime, which is the change that gives the attach path a policy."
    );
}

fn row(label: &str, v: &[f64]) {
    let cold = v.first().copied().unwrap_or(f64::NAN);
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    println!(
        "  {label:<28} {:>9.0}n {:>9.0}n {:>9.0}n {:>9.0}n",
        cold,
        pct(&s, 0.50),
        pct(&s, 0.99),
        pct(&s, 0.999)
    );
}

fn pct(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let i = ((q * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted[i.min(sorted.len() - 1)]
}
