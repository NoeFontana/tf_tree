//! **`docs/PHASE2.md` §12's attach rows**, which have never existed.
//!
//! §12's table asks for two things this repository has never measured:
//!
//! | row | what §12 wants |
//! |---|---|
//! | attach time, cold and warm | p50 |
//! | first access after attach, per-edge population on vs off | p99.9, both |
//!
//! `crates/tf_tree_bench/benches/` has no attach or population benchmark at all,
//! and `report.rs`'s `attach_latency` — a required `where_we_are_worse` entry —
//! has carried `metrics: Vec::new()` since it was written. An honesty section
//! with no number in it cannot regress, which is the point of filling it.
//!
//! # What attaching actually is
//!
//! `Tree::attach_shared` maps the segment, validates the header, claims a
//! participant slot and calls `populate_hot()` — which since
//! `docs/decisions/0024` warms the tables and *not* the two ring arenas, so the
//! rings show up in the `plan compile` row instead. That last step is the one §7.1
//! is NORMATIVE about: it pre-faults every region a reader touches so that no
//! page fault lands *inside* a lookup. So attach is where tf_tree pays what tf2
//! does not — a tf2 consumer constructs a `BufferCore` in-process and is ready
//! immediately — and it is a real cost even though it is paid once.
//!
//! # The row this does NOT produce, and why
//!
//! **"population on vs off" needs a way to attach without populating, and there
//! isn't one.** `populate_hot()` is called unconditionally inside
//! `attach_shared_inner` (`tf_tree/src/tree.rs:2133`). Getting the "off" arm
//! would take one of:
//!
//! * `madvise(MADV_DONTNEED)` over the mapping between attach and first access,
//!   which would need the arena's base pointer and length — `ArenaView` exposes
//!   neither, and widening it for a benchmark is the wrong trade;
//! * `MappedArena::attach` directly, which does skip population — but yields an
//!   arena, not a `Tree`, and there is no public path from one to the other.
//!
//! Reporting the "on" arm alone and saying so is better than inventing an "off"
//! arm out of a different code path. The row stays owed; it is no longer owed
//! *and* unmeasurable, because the cost it would be compared against is here.
//!
//! **An earlier revision listed a third route and said the "off" arm would
//! arrive with `docs/decisions/0024`. It did not, and the prediction was
//! wrong in a way worth keeping.** `0024` moved ring population from attach to
//! the moment an edge is taken up, which does give the attach path a policy —
//! but it is not a *toggle*. There is still no `populate: false`, because there
//! is still no case that wants one: population is now scoped to what the process
//! actually reads, so the thing an "off" arm would have argued for is the
//! default. What `0024` did give this file is the decomposition, and it is
//! better than the row asked for: `attach` and `plan compile (first)` bracket
//! what population costs and say which half of the process pays it.
//!
//! # "Cold" is only as cold as this host allows
//!
//! `cold` is the first attach in this process to a segment it has never mapped:
//! fresh VMA, fresh page tables, allocator not yet warm. It is **not** a cold
//! page cache — the creator wrote the arena moments earlier and dropping caches
//! needs root. So `cold` here is an upper bound on the warm case and a lower
//! bound on a genuinely cold one, and the gap between the two columns is the
//! part that is this process's own state rather than the kernel's.

#![allow(clippy::print_stdout)]

use anyhow::{anyhow, Context, Result};

use tf_tree::{AttachMode, InterpPolicy, Stamp, Tree};

/// Attach/lookup cycles timed. Odd, so a median is an observation.
const CYCLES: usize = 201;

/// The page size the per-page arithmetic in `docs/PHASE2.md` §12.2 divides by.
/// A constant rather than `sysconf(_SC_PAGESIZE)`, which would buy an `unsafe`
/// block to print something the byte count beside it already carries: on a host
/// whose base page is not 4 KiB — a 64 KiB aarch64 kernel is the live example —
/// the page column is wrong and the byte column still is not, so a reader there
/// divides again.
const PAGE_BYTES: usize = 4096;

/// The pair every other harness in this crate measures, so the first-access
/// number is comparable with the steady-state one.
const TARGET: &str = "imu_link";
const SOURCE: &str = "map";

/// A stamp off every dynamic grid, so the first lookup actually interpolates —
/// `docs/decisions/0013`. An exact-hit stamp would measure `bracket` plus a
/// seqlock read and under-report the pages a real first access touches.
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

        // Plan compilation is separated from the lookup rather than folded into
        // it: it walks the topology blocks, which `populate_hot` warms, so a
        // combined figure would hide which of the two the population is for.
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
        // Checked, not assumed: a first access that returned an error would be
        // measuring a refusal rather than a lookup, and would be *faster*.
        got.map_err(|e| anyhow!("the first lookup after attach was refused: {e:?}"))?;

        attach_ns.push(a as f64);
        plan_ns.push(p as f64);
        first_at_ns.push(f as f64);

        // `guard` borrows `tree`, so it goes first. (`Plan` is `Copy` and owns
        // nothing, so there is nothing to drop.) Unmapping inside the loop is
        // not optional: 201 live mappings would accumulate and the later cycles
        // would be measuring a different process from the earlier ones.
        drop(guard);
        drop(tree);
    }

    // **A separate pass, not a fourth timer inside the loop above.** It was
    // written that way first and the loop stopped measuring what it had been
    // measuring: `first lookup after attach` went 130 ns p50 to 210 ns, five
    // runs to three, with *no engine change at all* — bisected by reverting
    // every engine file and re-running, at which point the row stayed at 210.
    // An extra compile per iteration is enough to leave the branch predictor and
    // caches in a different state for the next iteration's lookup, and moving it
    // after the timed region does not help because the damage lands on the
    // iteration that follows. So the loop above is byte-identical to what it was
    // before this row existed, and this pass pays for its own attaches.
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
        // Compiling the *same* path a second time in the same process. Since
        // population became per-edge this is the row that prices the risk the
        // change introduces: a topology change invalidates every cached plan, so
        // the next lookup recompiles, and recompiling now re-populates. If
        // `madvise(MADV_POPULATE_READ)` over resident pages were expensive, a
        // `reparent` would put that cost in front of the next lookup on every
        // reader in the system. The compile work itself is identical between the
        // two, so the difference is the population and nothing else.
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
    // Leak the writers so the claims stay live and the history stays published
    // for every attach below; the process is about to exit anyway.
    let (writers, samples) = tf_tree_bench::fixture::spin_up(&tree)?;
    core::mem::forget(writers);
    drop(samples);
    Ok(tree)
}

fn report(arena_bytes: usize, attach: &[f64], plan: &[f64], replan: &[f64], first: &[f64]) {
    println!("PHASE2 §12 — attach time, and first access after attach");
    println!("  §11.1 fixture on a memfd, {CYCLES} attach/lookup cycles, ReadOnly");
    // The arena's size is what turns these figures into a per-page cost, and
    // `docs/PHASE2.md` §12.2 quotes one. That row carried the byte count by
    // hand from the sitting that first filled it, with nothing re-deriving it;
    // printing it here makes the division reproducible from this recipe alone.
    // Pages round **up**, because population advises whole pages: 1 401 472 B
    // is 342 whole pages and a 640 B remainder, and the remainder is charged.
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

/// Nearest-rank percentile over a sorted slice.
fn pct(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let i = ((q * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted[i.min(sorted.len() - 1)]
}
