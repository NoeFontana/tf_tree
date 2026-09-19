//! What the arena's backing costs a hot lookup: heap against shared `memfd`.
//!
//! Runs the §11.1 fixture, off-grid (`0013`), paired, on both backings. Neither backing causes
//! `docs/benchmarks/tf2.md`'s C++ gap; the C ABI's per-call `Guard` does (`PHASE4.md` §7 gate 1, `0022`).
//!
//! # Method
//!
//! As `crate::ratio`, declared [`Sensitivity::Ratio`](crate::report::Sensitivity::Ratio).

use anyhow::{anyhow, bail, Result};

use tf_tree::{InterpPolicy, Stamp, Tree};

/// Rounds of the interleaved pair (odd).
pub const ROUNDS: usize = 9;

/// Sweeps of the stamp table per arm per round.
const SWEEPS: usize = 40;

/// Stamps swept, all off every dynamic grid (`0013`).
const STAMPS: usize = 256;

/// Lookups per arm before timing, so first-touch faults are not charged to `memfd`.
const WARMUP: usize = 60_000;

/// The pair measured: three dynamic steps after folding, matching `crate::ratio`.
const TARGET: &str = "imu_link";
const SOURCE: &str = "map";

/// The `memfd` arena's rendezvous name.
const ARENA: &str = "tf_tree_bench_backing";

/// Stamps off every dynamic grid so `I::eval` runs (`0013`); identical to `ratio::stamp_ns`.
const fn stamp_ns(i: i64) -> i64 {
    crate::fixture::NOW_NS - 3_700_000 - i * 9_631
}

/// One interleaved run of both backings.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Median per-round `memfd_ns / heap_ns`.
    pub ratio: f64,
    /// Smallest per-round ratio.
    pub ratio_lo: f64,
    /// Largest per-round ratio.
    pub ratio_hi: f64,
    /// Median heap ns per lookup.
    pub heap_ns: f64,
    /// Median `memfd` ns per lookup.
    pub shm_ns: f64,
    /// Rounds timed.
    pub rounds: usize,
    /// Lookups per arm per round.
    pub lookups_per_round: u64,
    /// Queries checked to agree before timing.
    pub agreed: usize,
}

impl Run {
    /// `(ratio_hi - ratio_lo) / ratio_lo`: what this run resolves.
    #[must_use]
    pub fn spread(&self) -> f64 {
        (self.ratio_hi - self.ratio_lo) / self.ratio_lo
    }

    /// Whether the observed band excludes 1.0; `None` when it straddles 1.0.
    #[must_use]
    pub fn resolved(&self) -> Option<bool> {
        if self.ratio_lo > 1.0 {
            Some(true)
        } else if self.ratio_hi < 1.0 {
            Some(false)
        } else {
            None
        }
    }

    /// The nanoseconds the backing costs a lookup, or `None` when the sign is unresolved.
    #[must_use]
    pub fn backing_ns(&self) -> Option<f64> {
        self.resolved().map(|_| self.shm_ns - self.heap_ns)
    }

    /// The most the backing could cost a lookup: `ratio_hi` against the heap median, floored at zero.
    #[must_use]
    pub fn backing_ns_bound(&self) -> f64 {
        (self.heap_ns * (self.ratio_hi - 1.0)).max(0.0)
    }

    /// One line stating what the backing costs and the band it rests on.
    #[must_use]
    pub fn verdict_line(&self) -> String {
        let head = match self.resolved() {
            Some(true) => format!(
                "the shared mapping COSTS {:.1} ns/lookup",
                self.shm_ns - self.heap_ns
            ),
            Some(false) => format!(
                "the shared mapping SAVES {:.1} ns/lookup",
                self.heap_ns - self.shm_ns
            ),
            None => "UNRESOLVED — the band contains 1.0, so this run cannot say \
                     whether the backing costs anything"
                .to_owned(),
        };
        format!(
            "{head}: median {:.4}x over {} rounds, band {:.4}-{:.4}x ({:.1}% wide); \
             heap {:.1} ns against memfd {:.1} ns, {} queries agreed first",
            self.ratio,
            self.rounds,
            self.ratio_lo,
            self.ratio_hi,
            self.spread() * 100.0,
            self.heap_ns,
            self.shm_ns,
            self.agreed,
        )
    }
}

/// Measure the pair.
/// # Errors
///
/// If either arena cannot be built, or the arenas disagree before timing.
pub fn measure() -> Result<Run> {
    measure_with(ROUNDS, SWEEPS, WARMUP)
}

/// [`measure`] with loop counts as parameters, so a unit test can run in debug.
///
/// # Errors
///
/// As [`measure`].
pub fn measure_with(rounds: usize, sweeps: usize, warmup: usize) -> Result<Run> {
    if rounds == 0 || sweeps == 0 {
        bail!("rounds and sweeps must both be non-zero; got {rounds} and {sweeps}");
    }

    // `LerpSlerp` on both arms so `H` is the same measurement `ratio.rs` reports.
    let heap = crate::fixture::build_tree_with(InterpPolicy::LerpSlerp)?;
    let shm = build_shared_fixture()?;

    let (_hw, _hp) = crate::fixture::spin_up(&heap)?;
    let (_sw, _sp) = crate::fixture::spin_up(&shm)?;

    let heap_plan = plan_for(&heap)?;
    let shm_plan = plan_for(&shm)?;
    let heap_guard = heap.guard();
    let shm_guard = shm.guard();

    let stamps: Vec<Stamp> = (0..STAMPS as i64)
        .map(|i| Stamp::from_nanos(stamp_ns(i)))
        .collect();

    let mut agreed = 0usize;
    for &s in &stamps {
        let h = heap_plan
            .at(&heap_guard, s)
            .map_err(|e| anyhow!("the heap arena declined a stamp it must answer: {e:?}"))?;
        let m = shm_plan
            .at(&shm_guard, s)
            .map_err(|e| anyhow!("the memfd arena declined a stamp it must answer: {e:?}"))?;
        let d = crate::differential::pose_error(&h, &m);
        if d > 1e-15 {
            bail!(
                "the heap and memfd arenas disagree by {d} (rad or m) before timing. Both are the \
                 same engine on the same fixture, so this is a population difference between the \
                 two arenas, not an interpolation difference — the quotient would be comparing \
                 two different query sets"
            );
        }
        agreed += 1;
    }

    let sweep_heap = || {
        let mut acc = 0.0f64;
        for _ in 0..sweeps {
            for &s in &stamps {
                if let Ok(v) = heap_plan.at(&heap_guard, std::hint::black_box(s)) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };
    let sweep_shm = || {
        let mut acc = 0.0f64;
        for _ in 0..sweeps {
            for &s in &stamps {
                if let Ok(v) = shm_plan.at(&shm_guard, std::hint::black_box(s)) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };

    let per_sweep = stamps.len();
    let per_call = sweeps.saturating_mul(per_sweep).max(1);
    for _ in 0..warmup.div_ceil(per_call) {
        std::hint::black_box(sweep_heap());
        std::hint::black_box(sweep_shm());
    }

    let per_round = (sweeps * per_sweep) as u64;
    let mut ratios = Vec::with_capacity(rounds);
    let mut heap_ns = Vec::with_capacity(rounds);
    let mut shm_ns = Vec::with_capacity(rounds);
    for r in 0..rounds {
        let (h, m) = if r % 2 == 0 {
            let t0 = std::time::Instant::now();
            let _ = sweep_heap();
            let h = t0.elapsed().as_nanos() as f64 / per_round as f64;
            let t1 = std::time::Instant::now();
            let _ = sweep_shm();
            let m = t1.elapsed().as_nanos() as f64 / per_round as f64;
            (h, m)
        } else {
            let t1 = std::time::Instant::now();
            let _ = sweep_shm();
            let m = t1.elapsed().as_nanos() as f64 / per_round as f64;
            let t0 = std::time::Instant::now();
            let _ = sweep_heap();
            let h = t0.elapsed().as_nanos() as f64 / per_round as f64;
            (h, m)
        };
        if h <= 0.0 || m <= 0.0 {
            bail!("a timed round measured {h} / {m} ns per lookup, which is not a duration");
        }
        ratios.push(m / h);
        heap_ns.push(h);
        shm_ns.push(m);
    }

    Ok(Run {
        ratio: median(&mut ratios.clone()),
        ratio_lo: ratios.iter().copied().fold(f64::INFINITY, f64::min),
        ratio_hi: ratios.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        heap_ns: median(&mut heap_ns),
        shm_ns: median(&mut shm_ns),
        rounds,
        lookups_per_round: per_round,
        agreed,
    })
}

/// Time the same sweep against an arena another process serves, attached read-only (rung `A`); unpaired.
/// # Errors
///
/// If no arena of that name is served, or the pair cannot be planned.
pub fn measure_attached(
    name: &str,
    rounds: usize,
    sweeps: usize,
    warmup: usize,
) -> Result<(f64, f64)> {
    if rounds == 0 || sweeps == 0 {
        bail!("rounds and sweeps must both be non-zero; got {rounds} and {sweeps}");
    }
    // `CreatePolicy::Never`: creating one here would measure an in-process arena.
    let tree = tf_tree::Open::new()
        .name(name)
        .map_err(|e| anyhow!("`{name}` is not a usable arena name: {e:?}"))?
        .mode(tf_tree::AttachMode::ReadOnly)
        .create(tf_tree::CreatePolicy::Never)
        .open()
        .map_err(|e| anyhow!("attaching read-only to a served arena named `{name}`: {e:?}"))?;

    let plan = plan_for(&tree)?;
    let guard = tree.guard();
    let stamps: Vec<Stamp> = (0..STAMPS as i64)
        .map(|i| Stamp::from_nanos(stamp_ns(i)))
        .collect();

    let sweep = || {
        let mut acc = 0.0f64;
        for _ in 0..sweeps {
            for &s in &stamps {
                if let Ok(v) = plan.at(&guard, std::hint::black_box(s)) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };
    // A guard per lookup, as `tft_plan_at` is forced to; read-only, so `Guard::drop` skips the flush.
    let sweep_per_call = || {
        let mut acc = 0.0f64;
        for _ in 0..sweeps {
            for &s in &stamps {
                let g = tree.guard();
                if let Ok(v) = plan.at(&g, std::hint::black_box(s)) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };

    let per_sweep = stamps.len();
    let per_call = sweeps.saturating_mul(per_sweep).max(1);
    for _ in 0..warmup.div_ceil(per_call) {
        std::hint::black_box(sweep());
        std::hint::black_box(sweep_per_call());
    }

    let per_round = (sweeps * per_sweep) as f64;
    let mut ns = Vec::with_capacity(rounds);
    let mut pc = Vec::with_capacity(rounds);
    for r in 0..rounds {
        let (a, b) = if r % 2 == 0 {
            let t0 = std::time::Instant::now();
            let _ = sweep();
            let a = t0.elapsed().as_nanos() as f64 / per_round;
            let t1 = std::time::Instant::now();
            let _ = sweep_per_call();
            (a, t1.elapsed().as_nanos() as f64 / per_round)
        } else {
            let t1 = std::time::Instant::now();
            let _ = sweep_per_call();
            let b = t1.elapsed().as_nanos() as f64 / per_round;
            let t0 = std::time::Instant::now();
            let _ = sweep();
            (t0.elapsed().as_nanos() as f64 / per_round, b)
        };
        if a <= 0.0 || b <= 0.0 {
            bail!("a timed round measured {a} / {b} ns per lookup, which is not a duration");
        }
        ns.push(a);
        pc.push(b);
    }
    Ok((median(&mut ns), median(&mut pc)))
}

/// What a per-call [`tf_tree::Guard`] costs against a hoisted one (`0022` question 4): the same `Plan::at`
/// sweep twice. Returns `(hoisted_ns, per_call_ns)`.
///
/// # Errors
///
/// If the pair cannot be planned or a round is not a duration.
pub fn measure_guard_cost(
    tree: &Tree,
    rounds: usize,
    sweeps: usize,
    warmup: usize,
) -> Result<(f64, f64)> {
    measure_guard_cost_between(tree, TARGET, SOURCE, rounds, sweeps, warmup)
}

/// [`measure_guard_cost`] over a named frame pair, for a fixture without `imu_link`.
///
/// # Errors
///
/// As [`measure_guard_cost`].
pub fn measure_guard_cost_between(
    tree: &Tree,
    target: &str,
    source: &str,
    rounds: usize,
    sweeps: usize,
    warmup: usize,
) -> Result<(f64, f64)> {
    if rounds == 0 || sweeps == 0 {
        bail!("rounds and sweeps must both be non-zero; got {rounds} and {sweeps}");
    }
    let t = tree
        .frame(target)
        .map_err(|e| anyhow!("fixture frame `{target}` is missing: {e:?}"))?;
    let sf = tree
        .frame(source)
        .map_err(|e| anyhow!("fixture frame `{source}` is missing: {e:?}"))?;
    let plan = tree
        .plan(t, sf)
        .map_err(|e| anyhow!("compiling the {source} <- {target} plan: {e:?}"))?;
    let stamps: Vec<Stamp> = (0..STAMPS as i64)
        .map(|i| Stamp::from_nanos(stamp_ns(i)))
        .collect();

    let hoisted = || {
        let g = tree.guard();
        let mut acc = 0.0f64;
        for _ in 0..sweeps {
            for &s in &stamps {
                if let Ok(v) = plan.at(&g, std::hint::black_box(s)) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };
    let per_call = || {
        let mut acc = 0.0f64;
        for _ in 0..sweeps {
            for &s in &stamps {
                let g = tree.guard();
                if let Ok(v) = plan.at(&g, std::hint::black_box(s)) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };

    let per_sweep = stamps.len();
    let per_call_n = sweeps.saturating_mul(per_sweep).max(1);
    for _ in 0..warmup.div_ceil(per_call_n) {
        std::hint::black_box(hoisted());
        std::hint::black_box(per_call());
    }

    let per_round = (sweeps * per_sweep) as f64;
    let mut h_ns = Vec::with_capacity(rounds);
    let mut p_ns = Vec::with_capacity(rounds);
    for r in 0..rounds {
        let (h, p) = if r % 2 == 0 {
            let t0 = std::time::Instant::now();
            let _ = hoisted();
            let h = t0.elapsed().as_nanos() as f64 / per_round;
            let t1 = std::time::Instant::now();
            let _ = per_call();
            (h, t1.elapsed().as_nanos() as f64 / per_round)
        } else {
            let t1 = std::time::Instant::now();
            let _ = per_call();
            let p = t1.elapsed().as_nanos() as f64 / per_round;
            let t0 = std::time::Instant::now();
            let _ = hoisted();
            (t0.elapsed().as_nanos() as f64 / per_round, p)
        };
        if h <= 0.0 || p <= 0.0 {
            bail!("a timed round measured {h} / {p} ns per lookup, which is not a duration");
        }
        h_ns.push(h);
        p_ns.push(p);
    }
    Ok((median(&mut h_ns), median(&mut p_ns)))
}

/// One guard-cost row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuardCost {
    /// ns/lookup with one guard hoisted out of the sweep.
    pub hoisted_ns: f64,
    /// ns/lookup acquiring a guard inside the loop.
    pub per_call_ns: f64,
}

impl GuardCost {
    /// What one `Tree::guard()` costs a lookup, in nanoseconds.
    #[must_use]
    pub fn guard_ns(&self) -> f64 {
        self.per_call_ns - self.hoisted_ns
    }
}

/// What each backing holds resident for the same declared arena (Pss delta; `docs/PHASE2.md` §7.1).
///
/// Returns `(heap_kib, shm_kib, arena_bytes)`.
///
/// # Errors
///
/// If either fixture cannot be built or populated.
pub fn residency_both() -> Result<(u64, u64, usize)> {
    let heap_kib = {
        let before = crate::mp::self_pss_kib();
        let tree = crate::fixture::build_tree_with(InterpPolicy::LerpSlerp)?;
        let (w, p) = crate::fixture::spin_up(&tree)?;
        let after = crate::mp::self_pss_kib();
        let bytes = tree.arena_size_bytes();
        drop(w);
        drop(p);
        drop(tree);
        (after.saturating_sub(before), bytes)
    };
    let shm_kib = {
        let before = crate::mp::self_pss_kib();
        let tree = build_shared_fixture()?;
        let (w, p) = crate::fixture::spin_up(&tree)?;
        let after = crate::mp::self_pss_kib();
        drop(w);
        drop(p);
        drop(tree);
        after.saturating_sub(before)
    };
    Ok((heap_kib.0, shm_kib, heap_kib.1))
}

/// [`measure_guard_cost`] on a heap arena then a shared one (prices the `is_shared()` check, `0022` q4).
///
/// # Errors
///
/// If either fixture cannot be built or populated, or a sweep fails.
pub fn guard_cost_both(
    rounds: usize,
    sweeps: usize,
    warmup: usize,
) -> Result<(GuardCost, GuardCost)> {
    let heap = crate::fixture::build_tree_with(InterpPolicy::LerpSlerp)?;
    let shm = build_shared_fixture()?;
    let (_hw, _hp) = crate::fixture::spin_up(&heap)?;
    let (_sw, _sp) = crate::fixture::spin_up(&shm)?;

    let (hh, hp) = measure_guard_cost(&heap, rounds, sweeps, warmup)?;
    let (sh, sp) = measure_guard_cost(&shm, rounds, sweeps, warmup)?;
    Ok((
        GuardCost {
            hoisted_ns: hh,
            per_call_ns: hp,
        },
        GuardCost {
            hoisted_ns: sh,
            per_call_ns: sp,
        },
    ))
}

/// A three-edge, 256-slot heap tree matching `crates/tf_tree_c/examples/abi_cost.rs` (`0023` §7), for
/// [`guard_cost_fixture_pair`].
fn build_three_edge_tree() -> Result<Tree> {
    let cfg = tf_tree::EdgeCfg::new(tf_tree::Capacity::slots(256));
    let mount = tf_tree_math::exp_se3([0.3, -0.7, 0.2, 0.11, -0.05, 0.37]);
    let tree = tf_tree::TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .dynamic_edge("odom", "base", cfg)
        .static_edge("base", "sensor", &mount)
        .build()
        .map_err(|e| anyhow!("building the three-edge tree: {e:?}"))?;
    for (parent, child, k) in [("map", "odom", 1.0f64), ("odom", "base", 2.0)] {
        let p = tree
            .frame(parent)
            .map_err(|e| anyhow!("frame `{parent}`: {e:?}"))?;
        let c = tree
            .frame(child)
            .map_err(|e| anyhow!("frame `{child}`: {e:?}"))?;
        let w = tree
            .claim(c, p)
            .map_err(|e| anyhow!("claiming {child}: {e:?}"))?;
        for i in 0..64i64 {
            let f = i as f64;
            w.push(
                i * 10_000_000,
                &tf_tree_math::exp_se3([
                    0.004 * k * f,
                    -0.003 * f,
                    0.002 * k * f,
                    0.05 * f,
                    -0.02 * k * f,
                    0.01 * f,
                ]),
            )
            .map_err(|e| anyhow!("publishing to {child}: {e:?}"))?;
        }
        core::mem::forget(w);
    }
    Ok(tree)
}

/// `docs/decisions/0023` open question 3's falsifier: per-call guard cost on the three-edge fixture and on
/// §11.1's. Returns `(three_edge, phase11_1)` ns/lookup.
///
/// # Errors
///
/// If either fixture cannot be built or populated, or a sweep fails.
pub fn guard_cost_fixture_pair(rounds: usize, sweeps: usize, warmup: usize) -> Result<(f64, f64)> {
    let small = build_three_edge_tree()?;
    let big = crate::fixture::build_tree_with(InterpPolicy::LerpSlerp)?;
    let (_w, _s) = crate::fixture::spin_up(&big)?;

    let small_round = |t: &Tree, sweeps: usize, warmup: usize| {
        measure_guard_cost_between(t, "sensor", "map", 1, sweeps, warmup)
    };
    let mut small_acc = Vec::with_capacity(rounds);
    let mut big_acc = Vec::with_capacity(rounds);
    for r in 0..rounds {
        let (a, b) = if r % 2 == 0 {
            let a = small_round(&small, sweeps, if r == 0 { warmup } else { 0 })?;
            let b = measure_guard_cost(&big, 1, sweeps, if r == 0 { warmup } else { 0 })?;
            (a, b)
        } else {
            let b = measure_guard_cost(&big, 1, sweeps, 0)?;
            let a = small_round(&small, sweeps, 0)?;
            (a, b)
        };
        small_acc.push(a.1 - a.0);
        big_acc.push(b.1 - b.0);
    }
    Ok((median(&mut small_acc), median(&mut big_acc)))
}

/// The fixture topology on a `MAP_SHARED` `memfd` via `build_shared`.
fn build_shared_fixture() -> Result<Tree> {
    let mut b = tf_tree::TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
    for e in crate::fixture::EDGES {
        b = match e.kind {
            crate::fixture::EdgeDefKind::Static { xi } => {
                b.static_edge(e.parent, e.child, &tf_tree_math::exp_se3(xi))
            }
            crate::fixture::EdgeDefKind::Dynamic { rate_hz } => b.dynamic_edge(
                e.parent,
                e.child,
                tf_tree::EdgeCfg::new(tf_tree::Capacity::history(
                    rate_hz,
                    crate::fixture::HISTORY_SECS,
                )),
            ),
        };
    }
    b.build_shared(ARENA)
        .map_err(|e| anyhow!("building the shared-memfd fixture arena `{ARENA}`: {e:?}"))
}

/// Compile the [`TARGET`] <- [`SOURCE`] plan against one arena.
fn plan_for(tree: &Tree) -> Result<tf_tree::Plan> {
    let target = tree
        .frame(TARGET)
        .map_err(|e| anyhow!("fixture frame `{TARGET}` is missing: {e:?}"))?;
    let source = tree
        .frame(SOURCE)
        .map_err(|e| anyhow!("fixture frame `{SOURCE}` is missing: {e:?}"))?;
    tree.plan(target, source)
        .map_err(|e| anyhow!("compiling the {SOURCE} <- {TARGET} plan: {e:?}"))
}

/// Median of a scratch slice. Sorts in place; the caller owns the copy.
fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return f64::NAN;
    }
    v[v.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lo: f64, hi: f64, heap: f64, shm: f64) -> Run {
        Run {
            ratio: (lo + hi) / 2.0,
            ratio_lo: lo,
            ratio_hi: hi,
            heap_ns: heap,
            shm_ns: shm,
            rounds: 9,
            lookups_per_round: 10_240,
            agreed: 256,
        }
    }

    /// The sign is read off the band, not the median.
    #[test]
    fn a_band_containing_one_cannot_say_the_backing_costs_anything() {
        assert_eq!(run(0.97, 1.06, 200.0, 201.0).resolved(), None);
        assert_eq!(run(1.02, 1.09, 200.0, 210.0).resolved(), Some(true));
        assert_eq!(run(0.90, 0.98, 200.0, 190.0).resolved(), Some(false));
    }

    /// An unresolved run reports no nanosecond figure.
    #[test]
    fn an_unresolved_run_publishes_no_nanosecond_split() {
        assert_eq!(run(0.97, 1.06, 200.0, 201.0).backing_ns(), None);
        assert_eq!(run(1.02, 1.09, 200.0, 210.0).backing_ns(), Some(10.0));
    }

    /// The bound survives an unresolved sign.
    #[test]
    fn the_upper_bound_survives_an_unresolved_sign() {
        let bound = run(0.97, 1.06, 200.0, 201.0).backing_ns_bound();
        assert!((bound - 12.0).abs() < 1e-9, "got {bound}");
    }

    /// A band entirely below 1.0 bounds the cost at zero.
    #[test]
    fn a_band_entirely_below_one_bounds_the_cost_at_zero() {
        assert_eq!(run(0.90, 0.98, 200.0, 190.0).backing_ns_bound(), 0.0);
    }

    /// The verdict line names the direction.
    #[test]
    fn the_verdict_line_states_its_direction() {
        assert!(run(1.02, 1.09, 200.0, 210.0)
            .verdict_line()
            .contains("COSTS 10.0 ns"));
        assert!(run(0.90, 0.98, 200.0, 190.0)
            .verdict_line()
            .contains("SAVES 10.0 ns"));
        assert!(run(0.97, 1.06, 200.0, 201.0)
            .verdict_line()
            .contains("UNRESOLVED"));
    }

    /// Zero rounds or zero sweeps is a caller bug, not a run that returns NaN.
    #[test]
    fn degenerate_loop_counts_are_refused() {
        assert!(measure_with(0, 1, 1).is_err());
        assert!(measure_with(1, 0, 1).is_err());
    }
}
