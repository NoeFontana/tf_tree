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
//! **The guard is not specific to a shared arena, and assuming it was cost a
//! second wrong turn.** The obvious next move was to blame the fork check
//! `Tree::guard` adds only when `is_shared()` (`tf_tree/src/tree.rs:1984`) —
//! which would have meant `PHASE4.md` §7 gate 1's 1.020× was honest for a heap
//! tree and blind to shared ones. [`guard_cost_both`] priced that branch at
//! **+2.1 ns** (counters off) and **−8.4 ns** (counters on): noise. A per-call
//! guard costs ~17 ns on *both* backings, and Phase 5's diagnostic counters
//! roughly double it.
//!
//! **§7 gate 1 is simply failing.** `examples/abi_cost.rs` measures 1.34–1.46×
//! against its 1.05 gate on a heap tree and prints `FAIL` — it had just never
//! been run, appearing in no recipe and no workflow. `just abi-cost` now runs
//! it; `docs/PHASE4.md` §7 records the state and `0022` carries the question.
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
pub fn measure_attached(
    name: &str,
    rounds: usize,
    sweeps: usize,
    warmup: usize,
) -> Result<(f64, f64)> {
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
    // **The same sweep with the guard acquired per lookup** — the shape
    // `tft_plan_at` is forced into, on the *exact* arena the C++ probe measures
    // at 302 ns. This is the arm that decides whether the C ABI's +100 ns there
    // is the guard or something else, and it is measured here rather than
    // inferred from a different fixture: the guard costs +2.5 ns on
    // `abi_cost`'s 3-edge tree and +27-35 ns on this one, so a figure carried
    // across fixtures would answer the wrong question.
    //
    // Note this arena is attached **read-only**, and `Guard::drop` early-returns
    // when `!view.is_writable()` — so the counter flush that doubles the guard
    // on a writable arena is not paid here at all.
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
        // Interleaved and alternating, like every other pair in this module.
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

/// What acquiring a [`tf_tree::Guard`] **per lookup** costs, against hoisting
/// one out of the loop, on a tree the caller supplies.
///
/// # Why this is the measurement `0022` question 4 asks for
///
/// `0022` records the C ABI at +99.5 ns on a shared arena and attributes it to
/// `tft_plan_at` building a guard per call. That attribution rests on one piece
/// of evidence — `tft_plan_at_many`, which pays the guard once per batch,
/// recovering 41 ns — and on reading `tf_tree_c/src/lib.rs:684`. It is a good
/// inference. It is still an inference, and the last two attributions in this
/// area were both wrong.
///
/// This measures the guard directly, in **safe Rust with no C ABI anywhere near
/// it**, by running the identical `Plan::at` sweep twice: once with a hoisted
/// guard and once acquiring one inside the loop. Run it on a heap tree and on a
/// shared one and the difference between the two answers is the cost of the
/// `is_shared()` branch in `Tree::guard` (`tf_tree/src/tree.rs:1984`) — which is
/// precisely what question 4 proposes to move per-tree.
///
/// Returns `(hoisted_ns, per_call_ns)`. Paired and interleaved like everything
/// else here, so the difference resolves on this host.
///
/// # Errors
///
/// If the pair cannot be planned or a timed round measures a non-duration.
pub fn measure_guard_cost(
    tree: &Tree,
    rounds: usize,
    sweeps: usize,
    warmup: usize,
) -> Result<(f64, f64)> {
    measure_guard_cost_between(tree, TARGET, SOURCE, rounds, sweeps, warmup)
}

/// [`measure_guard_cost`] over a named frame pair, for a fixture that is not
/// §11.1's.
///
/// The pair is a parameter because [`guard_cost_fixture_pair`] measures a
/// three-edge tree that has no `imu_link` — asking for one does not fail
/// cleanly, it tries to *intern* the name and comes back `CapacityExceeded`,
/// which reads as a layout bug rather than a wrong frame.
///
/// # Errors
///
/// If the pair cannot be planned or a timed round measures a non-duration.
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

    // The hoisted arm acquires one guard for the whole sweep — what `ratio.rs`,
    // `measure_attached` and every in-process Rust consumer do.
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
    // The per-call arm acquires and drops one inside the loop — what
    // `tft_plan_at` is structurally forced to do, because the C signature has
    // nowhere to put a guard between calls. Everything else is identical, so
    // the difference is the guard and nothing else.
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
        // Alternate the leading arm, for the reason the rest of this module
        // does: a fixed order hands one arm the colder cache every round.
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

/// What each backing actually holds *resident* for the same declared arena.
///
/// # Why this exists
///
/// `0021` made the heap arena demand-faulted, so declared-but-unpublished slots
/// now cost it no resident memory. The shared path never had that defect —
/// a `memfd` is demand-faulted by construction — but it has a different one:
/// `MappedArena::populate_hot` deliberately pre-faults **every declared slot**
/// (`mapped.rs:394-396`, `stamp_slots * 8` and `pose_slots * 64`) so that an
/// attaching reader takes no page fault inside a lookup.
///
/// That is a real latency guarantee and `docs/PHASE2.md` §7.1 is NORMATIVE about
/// it. It is also, on an over-declared arena, the entire remaining resident
/// gap — and nothing had measured how large. `scale_sweep`, which reports
/// `rss_over_arena`, builds `Backing::Heap` only.
///
/// The §11.1 fixture declares 19 072 slots and publishes 12 600, a factor of
/// 1.51, so it is already the §9.3 case ("a robot that publishes far less than
/// it declared") at modest scale.
///
/// Returns `(heap_kib, shm_kib, arena_bytes)`.
///
/// # Method, and its one real limitation
///
/// Pss is a **whole-process level**, not a counter, so each figure is a delta
/// across building one arena and the two are taken in sequence in one process.
/// Each tree is dropped before the next is built. A delta still carries the
/// tree's own non-arena allocations — the same caveat `report.rs`'s
/// `measure_idle_arena_resident` states — so the order of magnitude is the
/// finding and the third digit is not.
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
        // Dropped *after* the reading, or the pages would be gone before it.
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

/// [`measure_guard_cost`] on a heap arena and on a shared one, in that order.
///
/// **This is the measurement that closed `0022`'s original question 4.** That
/// question asked whether `Tree::guard`'s `is_shared()` fork check could move
/// per-tree, on the theory that it was what made a shared arena expensive. The
/// shared row minus the heap row prices that branch at **+2.1 ns** with counters
/// off and **−8.4 ns** with them on — noise in both directions, so the premise
/// was false and no code was written against it.
///
/// What the rows *did* find is that a per-call guard costs ~17 ns, and that
/// Phase 5's diagnostic counters roughly double it (+35.4 / +27.0 ns with them
/// on). That is `0022`'s question 1 and the largest uncontested win on the
/// board.
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

/// A three-edge, 256-slot heap tree — a byte-for-byte match of the fixture
/// `crates/tf_tree_c/examples/abi_cost.rs` builds, and the one whose R3 row
/// `docs/decisions/0023` §7 gates.
///
/// It exists here so the toy fixture and the §11.1 one can be measured **in one
/// binary, one profile, interleaved**, which is the measurement `0023` open
/// question 3 names as the thing that would make its recommendation airtight.
/// Two arrays of 256 stamps are 2 KiB each and sit wholly in L1d on this host;
/// §11.1's 1 kHz edge searches 128 KiB, which is 4x it. That contrast is the
/// whole hypothesis.
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

/// **`docs/decisions/0023` open question 3's falsifier.** The per-call guard on
/// the three-edge fixture and on §11.1's, measured in one binary at one profile,
/// with the two fixtures interleaved round by round.
///
/// # What this is for
///
/// `0023` recommends moving §7's R3 criterion off the three-edge fixture and
/// onto §11.1's, on the argument that R3 prices the bracket cursor and the
/// cursor's cost is a **working-set** effect in the stamp array: 2 KiB sits on
/// the flat part of `docs/design/fast-path.md` §12's curve, 128 KiB sits past
/// the L1d cliff. The evidence was 16 ns against 34.4 ns — **from two different
/// binaries**, which is an inference and not a measurement, and this repository
/// has been wrong three times about exactly that kind of comparison.
///
/// So this pairs them. Both fixtures are heap-backed (the backing is worth
/// ~1.4 ns by `0022` amendment 5 and is not the variable), both are swept by the
/// identical code, and the fixture order alternates every round so neither
/// always meets the colder cache. If the paired difference does not reproduce
/// ~18 ns, question 3's recommendation is **withdrawn rather than argued**.
///
/// Returns the per-call guard cost on each, in ns/lookup: `(three_edge,
/// phase11_1)`. Deliberately two plain differences rather than two [`GuardCost`]
/// values — this measures the *difference* per round and takes the median of
/// those, so there is no single hoisted figure to put in the struct, and filling
/// one with a zero would be a number that reads as measured.
///
/// # Errors
///
/// If either fixture cannot be built or populated, or a sweep fails.
pub fn guard_cost_fixture_pair(rounds: usize, sweeps: usize, warmup: usize) -> Result<(f64, f64)> {
    let small = build_three_edge_tree()?;
    let big = crate::fixture::build_tree_with(InterpPolicy::LerpSlerp)?;
    let (_w, _s) = crate::fixture::spin_up(&big)?;

    // The three-edge tree's own chain, `map` -> `sensor`; it has no `imu_link`.
    let small_round = |t: &Tree, sweeps: usize, warmup: usize| {
        measure_guard_cost_between(t, "sensor", "map", 1, sweeps, warmup)
    };
    let mut small_acc = Vec::with_capacity(rounds);
    let mut big_acc = Vec::with_capacity(rounds);
    for r in 0..rounds {
        // One round each, alternating which fixture leads. `measure_guard_cost`
        // is already paired *within* a fixture (hoisted against per-call, with
        // its own alternation), so this adds the outer pairing and nothing else.
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
