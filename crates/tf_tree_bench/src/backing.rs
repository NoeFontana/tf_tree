//! What the arena's *backing* costs a hot lookup: heap against shared `memfd`.
//!
//! # The question this exists to answer
//!
//! `docs/benchmarks/tf2.md`'s bracket ended on an owed measurement. A C++ caller
//! against `libtf_tree_c.so` measures **306.7 ns** on the depth-3 fixture where
//! native Rust measures **201.5 ns** — **+52%** — and
//! [`PHASE4.md`](../../../docs/PHASE4.md) §7 gate 1 records the same ABI at
//! **1.020×**. Both are real. The 52% run changed *two* things at once against
//! its Rust comparand:
//!
//! 1. the call crosses a shared-library boundary the linker cannot see across;
//! 2. the arena is a `MAP_SHARED` `memfd` rather than a private heap allocation.
//!
//! **The answer turned out to be neither of them** — see the ladder below. This
//! module measures rung 2 (and, with [`measure_attached`], the cross-process
//! rung), which is what made the elimination possible; the culprit is the C
//! ABI's per-call `Guard`.
//!
//! `tf2.md` argued from a neighbouring row that it is not the mapping, and
//! called that *an argument rather than a measurement*. It is worth being
//! specific about why, because there are two priors and **both have a defect
//! that points the same way**:
//!
//! - **`mp_bench` 213 ns against `cost_model` 217 ns** (`tf2.md`, "there is no
//!   penalty for the arena being shared"). Two different harnesses, in two
//!   different processes, compared as medians. That is the unpaired comparison
//!   this host cannot resolve: its run-to-run spread is ~4%, and the effect
//!   under test is smaller than that.
//! - **`examples/heap_vs_shared` 51.1 ns against 51.3 ns.** Paired in one
//!   process, but it queries the single stamp `1_500_000_000` against samples
//!   laid down at `1_000_000 + i * 1_000_000` — an **exact grid hit** at
//!   `i = 1499`. That is precisely `docs/decisions/0013`'s defect: the lookup
//!   takes `SampleRing::sample`'s exact-hit branch, `I::eval` never runs, and
//!   what is being compared is `bracket` plus a seqlock read. If the mapping
//!   costs anything, it costs it on the loads the interpolation issues, and
//!   that measurement never issues them. It is also a different topology from
//!   the fixture the 52% was measured on.
//!
//! Neither is wrong about its own subject; neither settles this one. This
//! module runs the §11.1 fixture, off-grid, paired, on both backings.
//!
//! # The ladder, and the wrong turn taken on the way up it
//!
//! ```text
//!   H  native Rust API, heap arena, in-process         200.7 ns
//!   S  native Rust API, memfd arena, in-process RW     203.2 ns  <- measure()
//!   A  native Rust API, memfd arena, RO cross-process  202.5 ns  <- measure_attached()
//!   C  C ABI (tft_plan_at), same arena as A            302.0 ns
//!   C' C ABI (tft_plan_at_many), same arena as A       261.0 ns
//! ```
//!
//! **The first version of this module had only `H` and `S`, and that was enough
//! to reach a wrong answer.** It measured the mapping, found it ~free, and
//! attributed the entire residue to the shared-library boundary — a subtraction
//! presented as a finding. Building `tests/cpp/bench.cpp` against
//! `libtf_tree_c.a` and `libtf_tree_c.so` prices that boundary at **245.4
//! against 244.4 ns**, or 0.4%. The residue was never the linker.
//!
//! With `A` in place the ladder is unambiguous: the mapping costs <= 9.6 ns, the
//! cross-process read-only attach costs -0.7 ns, the link mode ~1 ns, and the
//! remaining **+99.5 ns (+49%) is the C ABI's own per-call work** —
//! `tft_plan_at` constructs a `Guard` on every call (`tf_tree_c/src/lib.rs:684`)
//! where the Rust arm hoists one out of the loop. `C'` is the evidence rather
//! than the inference: paying that guard once per batch instead of once per
//! element recovers 41 of the 99.5 ns.
//!
//! **This does not resolve `PHASE4.md` §7 gate 1, it sharpens it.** That gate
//! measures 1.020x on a *heap* tree in-process, and `Tree::guard` takes a
//! different path on a shared arena (`tf_tree/src/tree.rs:1984` — a
//! fork-generation check a heap arena skips). The gate therefore prices a
//! configuration no `shm` consumer uses. Fixing that is a decision record.
//!
//! # Method
//!
//! Identical to `crate::ratio`, and for the identical reason: this host cannot
//! produce an honest absolute duration (SMT, unreadable governor, four cores),
//! but it can produce a **quotient of two arms timed inside one round**. Both
//! arms run the same `Plan::at` over the same off-grid stamps; the leading arm
//! alternates so neither gets the cold cache every time; the reported figure is
//! the median of per-round quotients and the band is printed with it.
//!
//! **Load is common-mode here in a way it is not in `ratio.rs`.** That module
//! has to disclaim it, because `tf2::BufferCore` locks per lookup and our read
//! path does not, so contention moves one arm only. Here both arms are the same
//! engine on the same read path and differ solely in which pages the addresses
//! land on, so a busy host slows both together. This row is still declared
//! [`Sensitivity::Ratio`](crate::report::Sensitivity::Ratio) rather than
//! host-independent — it is a timing quotient — but it is the better-behaved
//! kind.

use anyhow::{anyhow, bail, Result};

use tf_tree::{InterpPolicy, Stamp, Tree};

/// Rounds of the interleaved pair. Odd, so the median is an observation.
pub const ROUNDS: usize = 9;

/// Sweeps of the stamp table per arm per round.
const SWEEPS: usize = 40;

/// Stamps swept, all off every dynamic grid — `0013`'s subject.
const STAMPS: usize = 256;

/// Lookups per arm before any round is timed.
///
/// Larger than `crate::ratio`'s, and the reason is the thing under test: the
/// `memfd` arm's pages are faulted in on first touch, and a minor fault charged
/// to an early timed round would be reported as backing cost when it is really
/// warmup. `build_shared` calls `populate_hot()`, which pre-faults the hot
/// region, so this is belt and braces — but the arm that could be flattered by
/// under-warming is the one whose cost is being measured, so it gets the belt.
const WARMUP: usize = 60_000;

/// The pair this fixture is measured on: three dynamic steps after folding,
/// matching `crate::ratio` exactly so the two runs' `tf_tree` arms are
/// comparable. **If one moves, both move.**
const TARGET: &str = "imu_link";
const SOURCE: &str = "map";

/// The `memfd` arena's rendezvous name.
///
/// Fixed rather than randomised: this crate is `#![forbid(unsafe_code)]` so
/// there is no `getpid` to hand, and two concurrent runs of a benchmark on one
/// host is not a configuration worth defending against — `build_shared` fails
/// loudly on a name collision, which is the right outcome.
const ARENA: &str = "tf_tree_bench_backing";

/// Stamps off every dynamic grid, so `I::eval` actually runs.
///
/// **Byte-identical to `ratio::stamp_ns`'s construction, deliberately.**
/// `NOW_NS` is an exact multiple of all four dynamic periods, so a sweep
/// anchored on it takes `SampleRing::sample`'s exact-hit branch and measures
/// `bracket` plus a seqlock read — the defect `docs/decisions/0013` is about.
/// Reintroducing it here would silently make *both* arms measure the cheap path,
/// and since the quotient would still look plausible nothing would say so.
const fn stamp_ns(i: i64) -> i64 {
    crate::fixture::NOW_NS - 3_700_000 - i * 9_631
}

/// One interleaved run: both backings, one process, `ROUNDS` rounds.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Median per-round `memfd_ns / heap_ns`.
    ///
    /// Above 1.0 means the shared mapping costs a lookup something. Paired, and
    /// deliberately not the quotient of the two medians below.
    pub ratio: f64,
    /// Smallest per-round ratio observed.
    pub ratio_lo: f64,
    /// Largest per-round ratio observed.
    pub ratio_hi: f64,
    /// Median heap-arena nanoseconds per lookup. Reported, never gated.
    pub heap_ns: f64,
    /// Median `memfd`-arena nanoseconds per lookup. Reported, never gated.
    pub shm_ns: f64,
    /// Rounds timed.
    pub rounds: usize,
    /// Lookups per arm per round.
    pub lookups_per_round: u64,
    /// Queries on which the two arenas were checked to agree before timing.
    pub agreed: usize,
}

impl Run {
    /// `(ratio_hi - ratio_lo) / ratio_lo`: what this run can resolve.
    #[must_use]
    pub fn spread(&self) -> f64 {
        (self.ratio_hi - self.ratio_lo) / self.ratio_lo
    }

    /// Whether the observed band excludes 1.0 — i.e. whether this run can say
    /// the backing costs anything at all.
    ///
    /// Returns `None` when the band straddles 1.0, which is the honest answer
    /// when it does: the same reason `ratio::Verdict::Unresolved`
    /// exists. A point estimate read off a band that contains the null is not a
    /// finding.
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

    /// The nanoseconds the backing costs a single lookup, or `None` when this
    /// run could not resolve the sign.
    ///
    /// Reported as a difference of the two medians rather than scaled from
    /// [`Run::ratio`], because the *subtraction* is what
    /// `docs/benchmarks/tf2.md` needs: the C++ arm's 306.7 ns is an absolute
    /// figure and the split has to be stated in the same units.
    #[must_use]
    pub fn backing_ns(&self) -> Option<f64> {
        self.resolved().map(|_| self.shm_ns - self.heap_ns)
    }

    /// The **most** the backing could be costing a lookup, consistent with the
    /// observed band.
    ///
    /// This is the number that actually answers `tf2.md`'s question, and it is
    /// available whether or not [`Run::resolved`] could fix the sign — which
    /// matters, because on this host the sign frequently cannot be fixed while
    /// the bound stays tight. Runs measured at 1.4 ns and 2.2 ns with bands
    /// reaching 1.0227x and 1.0476x: the point estimate flickers in and out of
    /// significance, the bound does not move much, and the bound is what the
    /// decomposition needs. "At most this" is a real finding when the residue is
    /// two orders of magnitude larger.
    ///
    /// Taken from `ratio_hi` against the heap median, and floored at zero: a
    /// band lying entirely under 1.0 means the shared mapping was never observed
    /// to cost anything, and a negative "upper bound on a cost" would read as a
    /// guaranteed saving, which is a stronger claim than a bound can make.
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

/// Measure the pair. See the module docs for what the number decomposes.
///
/// # Errors
///
/// If either arena cannot be built or populated, the pair cannot be planned, or
/// the two arenas disagree on an answer before either is timed.
pub fn measure() -> Result<Run> {
    measure_with(ROUNDS, SWEEPS, WARMUP)
}

/// [`measure`], with the loop counts as parameters, so a unit test can run the
/// same code without spending minutes in a debug build.
///
/// # Errors
///
/// As [`measure`].
pub fn measure_with(rounds: usize, sweeps: usize, warmup: usize) -> Result<Run> {
    if rounds == 0 || sweeps == 0 {
        bail!("rounds and sweeps must both be non-zero; got {rounds} and {sweeps}");
    }

    // `LerpSlerp` on both arms, matching `ratio.rs`. Not because tf2 is here —
    // it is not — but because the `H` arm has to be the *same* measurement
    // `ratio.rs` reports as 201.5 ns, or the subtraction against the C++ arm is
    // over two different loops and the decomposition does not hold.
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

    // **The two arenas must agree before either is timed**, for `ratio.rs`'s
    // reason and then a stronger one: this is the *same engine on both sides*,
    // fed the same deterministic fixture, so the two answers are not merely
    // close — they are the same arithmetic on the same inputs. Any difference at
    // all means the two arenas were not populated identically, which would make
    // the quotient a comparison of two different query sets.
    //
    // Hence a tolerance of 1e-15 rather than `ratio.rs`'s 1e-9: that module is
    // comparing two independent implementations and has to allow for them, and
    // this one does not.
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
                // Symmetric `if let` on both arms so the branch cancels out of
                // the quotient, and no `expect`: this crate denies it, and a
                // panic in a timed loop is not a measurement. Agreement was
                // established above, so neither arm takes the else.
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
        // Alternate the leading arm. A fixed order gives one arm the colder
        // cache in every round — and here that bias would land squarely on the
        // quantity being measured, since a fraction of a nanosecond is the whole
        // effect size.
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

/// Time the same sweep against an arena **another process is serving**,
/// attached read-only through the rendezvous.
///
/// # Why this arm exists
///
/// It is the one that decides whether the C ABI is implicated at all. The
/// ladder up to the C++ figure has four rungs, and until this function existed
/// two of them moved together:
///
/// ```text
///   H  Rust native, heap,          in-process     201.2 ns
///   S  Rust native, memfd RW,      in-process     203.4 ns
///   A  Rust native, memfd RO,      cross-process  <- this function
///   C  C ABI,       memfd RO,      cross-process  348.1 ns
/// ```
///
/// If `A` lands near `C`, the cost is the cross-process read-only attach and
/// the C ABI is innocent — which would make `PHASE4.md` §7 gate 1's 1.020× the
/// honest figure for the ABI after all, and move the whole question to the
/// attach path. If `A` lands near `S`, the ABI is doing something on attached
/// trees that it does not do on heap ones.
///
/// Unpaired, unavoidably: the two comparands are in different processes, so
/// there is no interleaving available. It is reported against `S` from the same
/// invocation to keep the host constant, and the difference being measured
/// (~145 ns) is far outside this host's ~4% run-to-run spread, which is what
/// makes an unpaired reading usable here where it would not be for the ~2 ns
/// backing effect.
///
/// # Errors
///
/// If no arena of that name is being served, the pair cannot be planned, or a
/// timed round measures a non-duration.
pub fn measure_attached(name: &str, rounds: usize, sweeps: usize, warmup: usize) -> Result<f64> {
    if rounds == 0 || sweeps == 0 {
        bail!("rounds and sweeps must both be non-zero; got {rounds} and {sweeps}");
    }
    // `CreatePolicy::Never`: this arm is only meaningful against an arena
    // somebody else built. Creating one here would silently measure an
    // in-process arena and report it as a cross-process number.
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

    let per_sweep = stamps.len();
    let per_call = sweeps.saturating_mul(per_sweep).max(1);
    for _ in 0..warmup.div_ceil(per_call) {
        std::hint::black_box(sweep());
    }

    let per_round = (sweeps * per_sweep) as f64;
    let mut ns = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t0 = std::time::Instant::now();
        let _ = sweep();
        let d = t0.elapsed().as_nanos() as f64 / per_round;
        if d <= 0.0 {
            bail!("a timed round measured {d} ns per lookup, which is not a duration");
        }
        ns.push(d);
    }
    Ok(median(&mut ns))
}

/// The fixture topology on a `MAP_SHARED` `memfd`.
///
/// `build_shared` and not the rendezvous [`tf_tree::Open`]: the rendezvous adds
/// a lock file, a socket and an `SCM_RIGHTS` handshake, none of which is in the
/// read path being timed. What is under test is which pages the loads land on,
/// and `build_shared` is the shortest route to the same mapping. (It is also
/// what `Open` itself calls once it has won the race — `open.rs` line 550.)
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

    /// The sign is read off the **band**, not off the median — the property that
    /// makes an unresolved outcome possible at all, and the one that stops a
    /// 0.3% median difference on a noisy host from being published as a finding.
    ///
    /// Mutant: make `resolved` compare `self.ratio` against 1.0 instead of
    /// `ratio_lo`/`ratio_hi`. The straddling case then reports `Some(true)` and
    /// this test fails on the first assertion.
    #[test]
    fn a_band_containing_one_cannot_say_the_backing_costs_anything() {
        assert_eq!(run(0.97, 1.06, 200.0, 201.0).resolved(), None);
        assert_eq!(run(1.02, 1.09, 200.0, 210.0).resolved(), Some(true));
        assert_eq!(run(0.90, 0.98, 200.0, 190.0).resolved(), Some(false));
    }

    /// An unresolved run reports no nanosecond figure at all.
    ///
    /// This is the guard that matters for `docs/benchmarks/tf2.md`: the whole
    /// point of the split is to subtract this number from the C++ arm's 306.7
    /// ns, and subtracting a difference of medians whose sign is not established
    /// would launder noise into an attribution.
    ///
    /// Mutant: make `backing_ns` return `Some(self.shm_ns - self.heap_ns)`
    /// unconditionally — the first assertion then yields `Some(1.0)` and fails.
    #[test]
    fn an_unresolved_run_publishes_no_nanosecond_split() {
        assert_eq!(run(0.97, 1.06, 200.0, 201.0).backing_ns(), None);
        assert_eq!(run(1.02, 1.09, 200.0, 210.0).backing_ns(), Some(10.0));
    }

    /// The **bound** survives an unresolved sign, which is the whole reason it
    /// exists: on this host the point estimate flickers in and out of
    /// significance run to run while the bound stays put, and the bound is what
    /// the decomposition subtracts against.
    ///
    /// Mutant: make `backing_ns_bound` return `0.0` when `resolved()` is `None`
    /// — i.e. gate it the way `backing_ns` is gated. This test then reads 0.0
    /// where it expects 12.0 and fails, and the binary would silently attribute
    /// 100% of the gap to the boundary on every unresolved run.
    #[test]
    fn the_upper_bound_survives_an_unresolved_sign() {
        // Band 0.97-1.06 on a 200 ns arm: at most 6% of 200 ns.
        let bound = run(0.97, 1.06, 200.0, 201.0).backing_ns_bound();
        assert!((bound - 12.0).abs() < 1e-9, "got {bound}");
    }

    /// A band lying entirely below 1.0 bounds the cost at zero rather than
    /// reporting a negative one.
    ///
    /// A negative upper bound on a cost reads as a *guaranteed saving*, which is
    /// a stronger claim than a band can support and would be published as one by
    /// the subtraction in `arena_backing`.
    ///
    /// Mutant: drop the `.max(0.0)` — this yields -4.0 and fails.
    #[test]
    fn a_band_entirely_below_one_bounds_the_cost_at_zero() {
        assert_eq!(run(0.90, 0.98, 200.0, 190.0).backing_ns_bound(), 0.0);
    }

    /// The verdict line names the direction, so a run where the shared mapping
    /// is *faster* cannot be read as though it were slower.
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
