//! The depth-3 lookup ratio against `tf2::BufferCore`, measured **paired**.
//!
//! `docs/PHASE5.md` §9.2's absolute-duration rows are `unavailable` on a host
//! `Fitness::probe` rejects. A quotient of two engines timed inside one round is
//! a different statistic: governor and SMT effects move both arms and divide out
//! ([`Sensitivity::Ratio`](crate::report::Sensitivity::Ratio)). Load is not
//! divided out (`tf2` takes a mutex, `tf_tree` none, so a busy host inflates the
//! quotient); `Fitness::fair_for_ratios` carries that.
//!
//! # What this number is not
//!
//! The tf2 column goes through `tf_tree_tf2_sys`, so it flatters `tf_tree`: the
//! residual FFI boundary is worth 45.3 ns (10%) to tf2 (`docs/benchmarks/tf2.md`).
//! The honest headline (2.7×) comes from `docker/tf2/native_scaling.cpp`. This
//! row is a regression detector, so [`FLOOR`] sits well under the measured
//! value. It is also single-threaded and warm, the best case for both engines;
//! the contended rows are `contended_scaling` and `tf2_scaling`.
//!
//! # Which consumer build this row speaks for: this workspace's, not yours
//!
//! The row is built under the workspace `[profile.release]` (`lto = "thin"`). A
//! consumer gets cargo's release defaults (`[profile.embedder]`, no LTO), where
//! `Plan::at` is not inlined across the crate boundary (`docs/API.md` §2.3
//! item 3). Measured in `docker/tf2`, `taskset -c 2`, 2026-08-15
//! (`just tf2-ratio-profiles`):
//!
//! | build | `lto` | tf_tree | tf2 (via binding) | **paired ratio** | band |
//! |---|---|---|---|---|---|
//! | workspace `[profile.release]` — what this row gates | `"thin"` | 201.6 ns | 504.4 ns | **2.490×** | 2.452–2.547 |
//! | `[profile.embedder]` — cargo's release defaults, i.e. a consumer | `false` | 244.2 ns | 506.1 ns | **2.075×** | 2.063–2.080 |
//!
//! The tf2 column is the control (+0.34%, an `extern "C"` arm no Rust LTO can
//! inline); the tf_tree column moves +21.1%. `[profile.profiling]` (debuginfo
//! only) measures 200.4 ns / 2.468×, so the number tracks `lto`.
//!
//! The consequence for [`FLOOR`] is stated there.

use anyhow::{anyhow, bail, Result};

use tf_tree::{InterpPolicy, Stamp};

use crate::tf2::Tf2Fixture;

/// The floor this row gates: `tf_tree` at least this many times faster than
/// `tf2` on a depth-3 hot lookup, **built as this workspace builds it**
/// (`lto = "thin"`). The row measures 2.49×; the floor sits under
/// [`UNBIASED_ESTIMATE`] so the binding's ~10% bias alone cannot pass it.
///
/// # Not defensible for a consumer's default `--release`
///
/// At `[profile.embedder]` the tf_tree arm is 244.2 ns, or
/// [`UNBIASED_ESTIMATE_DEFAULT_RELEASE`] ≈ **1.80×** against native C++ tf2,
/// which is under 2.0. The floor is not lowered (that would weaken the gate for
/// the build it measures); the claim is scoped to a regression detector for this
/// workspace's build. The consumer headline remains `tf2.md`'s ~2.7×. Gating the
/// consumer build is a decision record, not an edit here.
pub const FLOOR: f64 = 2.0;

/// The same fixture with no binding on either arm at `[profile.release]`:
/// `tf_tree` native Rust (201.5 ns) against `tf2` native C++ (452.9 ns), from
/// `docker/tf2/native_ratio.sh`. Unpaired, so a point estimate that bounds
/// [`FLOOR`], not a gate. A 2026-08-15 re-measure gave 2.18×; the constant is
/// not chased to it.
const UNBIASED_ESTIMATE: f64 = 2.25;

/// The same quantity at cargo's release defaults (`[profile.embedder]`, what a
/// consumer compiles): 439.2 ns native C++ tf2 against 244.2 ns tf_tree,
/// 2026-08-15. Recorded at the pessimistic end of a 1.80–1.86 spread; nothing
/// in it reaches 2.0.
///
/// No gate reads it; it makes [`FLOOR`]'s prose checkable via
/// `the_floor_is_bounded_at_one_profile_and_not_the_other`. `pub` so the caveat
/// is reachable from [`FLOOR`] in rendered docs.
///
/// # Why there is no second gated row at this profile
///
/// `docs/decisions/0025`: across three repeats the consumer row's band straddles
/// [`FLOOR`] in two (medians 2.088 / 2.083 / 2.047; bands 2.080–2.137,
/// 1.975–2.563, 1.923–2.340), and [`Run::verdict`] compares the band, so it
/// answers `Unresolved`. A threshold cannot be derived from a band that contains
/// it. Revisit when a host resolves this row across repeats.
pub const UNBIASED_ESTIMATE_DEFAULT_RELEASE: f64 = 1.80;

/// [`FLOOR`] must stay under the unbiased estimate, or the binding's bias could
/// pass this row. A compile-time check; it holds for this workspace's
/// `lto = "thin"` build only (see the test below).
const _: () = assert!(FLOOR < UNBIASED_ESTIMATE);

/// Rounds of the interleaved pair. Odd, so the median is an observation.
pub const ROUNDS: usize = 9;

/// Sweeps of the stamp table per arm per round.
const SWEEPS: usize = 40;

/// Stamps swept, all off every dynamic grid — `0013`'s subject.
const STAMPS: usize = 256;

/// Lookups per arm before any round is timed.
const WARMUP: usize = 20_000;

/// The pair this fixture is measured on: three dynamic steps after folding,
/// which is what `PHASE1.md` §11.3 means by "depth-3" — NORMATIVE there.
const TARGET: &str = "imu_link";
const SOURCE: &str = "map";

/// Stamps off every dynamic grid, so `I::eval` actually runs
/// (`docs/decisions/0013`).
const fn stamp_ns(i: i64) -> i64 {
    crate::fixture::NOW_NS - 3_700_000 - i * 9_631
}

/// Where the observed band sits relative to [`FLOOR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The whole observed band is at or above the floor.
    Above,
    /// The whole observed band is below it.
    Below,
    /// The band straddles the floor, so this run cannot answer; reported rather
    /// than resolved by the median.
    Unresolved,
}

impl Verdict {
    /// Stable spelling for the report.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Above => "above",
            Verdict::Below => "below",
            Verdict::Unresolved => "unresolved",
        }
    }
}

/// One interleaved run: both arms, one process, `ROUNDS` rounds.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Median per-round `tf2_ns / tf_tree_ns`: paired, not the quotient of the
    /// two medians below.
    pub ratio: f64,
    /// Smallest per-round ratio observed.
    pub ratio_lo: f64,
    /// Largest per-round ratio observed.
    pub ratio_hi: f64,
    /// Median `tf_tree` nanoseconds per lookup. Reported, never gated: it is an
    /// absolute duration, and this host cannot claim one.
    pub tf_tree_ns: f64,
    /// Median `tf2` nanoseconds per lookup, through `tf_tree_tf2_sys`.
    pub tf2_ns: f64,
    /// Rounds timed.
    pub rounds: usize,
    /// Lookups per arm per round.
    pub lookups_per_round: u64,
    /// Queries on which the two engines were checked to agree before timing.
    pub agreed: usize,
}

impl Run {
    /// `(ratio_hi - ratio_lo) / ratio_lo`: what this run can resolve.
    #[must_use]
    pub fn spread(&self) -> f64 {
        (self.ratio_hi - self.ratio_lo) / self.ratio_lo
    }

    /// [`FLOOR`] against the **observed band**, not against a point.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        if self.ratio_lo >= FLOOR {
            Verdict::Above
        } else if self.ratio_hi < FLOOR {
            Verdict::Below
        } else {
            Verdict::Unresolved
        }
    }

    /// One line stating the verdict and the band it rests on.
    #[must_use]
    pub fn verdict_line(&self) -> String {
        format!(
            "{} the {FLOOR:.1}x floor: median {:.3}x over {} rounds, band {:.3}-{:.3}x \
             ({:.1}% wide); tf_tree {:.1} ns against tf2 {:.1} ns, {} queries agreed first",
            match self.verdict() {
                Verdict::Above => "ABOVE",
                Verdict::Below => "BELOW",
                Verdict::Unresolved => "UNRESOLVED against",
            },
            self.ratio,
            self.rounds,
            self.ratio_lo,
            self.ratio_hi,
            self.spread() * 100.0,
            self.tf_tree_ns,
            self.tf2_ns,
            self.agreed,
        )
    }
}

/// Measure the pair.
///
/// # Errors
///
/// If the fixture cannot be built, the pair cannot be planned, tf2 cannot be
/// loaded, or the two engines disagree on an answer before either is timed.
pub fn measure() -> Result<Run> {
    measure_with(ROUNDS, SWEEPS, WARMUP)
}

/// [`measure`] with the loop counts as parameters, so a unit test can run it
/// cheaply.
///
/// # Errors
///
/// As [`measure`].
pub fn measure_with(rounds: usize, sweeps: usize, warmup: usize) -> Result<Run> {
    // `LerpSlerp` on both sides: it is tf2's policy (`PROJECT.md` §5 D5).
    let tree = crate::fixture::build_tree_with(InterpPolicy::LerpSlerp)?;
    let (_writers, _pushed) = crate::fixture::spin_up(&tree)?;
    let target = tree
        .frame(TARGET)
        .map_err(|e| anyhow!("fixture frame `{TARGET}` is missing: {e:?}"))?;
    let source = tree
        .frame(SOURCE)
        .map_err(|e| anyhow!("fixture frame `{SOURCE}` is missing: {e:?}"))?;
    let plan = tree
        .plan(target, source)
        .map_err(|e| anyhow!("compiling the {SOURCE} <- {TARGET} plan: {e:?}"))?;
    let guard = tree.guard();

    let tf2 = Tf2Fixture::load()?;

    let stamps: Vec<i64> = (0..STAMPS as i64).map(stamp_ns).collect();
    // Converted once, outside every timed loop.
    let ours_stamps: Vec<Stamp> = stamps.iter().map(|&s| Stamp::from_nanos(s)).collect();

    // The engines must agree before either is timed: an arm answering a
    // different question (or a tf2 horizon miss) would move the ratio silently.
    let mut agreed = 0usize;
    for (i, &s) in stamps.iter().enumerate() {
        let ours = plan
            .at(&guard, ours_stamps[i])
            .map_err(|e| anyhow!("tf_tree declined the stamp {s} it must answer: {e:?}"))?;
        let Some(theirs) = tf2.lookup(TARGET, SOURCE, s) else {
            bail!(
                "tf2 declined the stamp {s}, so the two arms would not be timed on the same \
                 query set. The sweep is inside the fixture's common window by construction, \
                 so this is a fixture or cache-horizon problem, not an extrapolation."
            );
        };
        let d = crate::differential::pose_error(&ours, &theirs);
        if d > 1e-9 {
            bail!(
                "the two engines disagree at stamp {s} by {d} (rad or m) before timing; a ratio \
                 between arms answering different questions is not a measurement"
            );
        }
        agreed += 1;
    }

    let sweep_ours = || {
        let mut acc = 0.0f64;
        for _ in 0..sweeps {
            for &s in &ours_stamps {
                // Symmetric with the other arm's `if let`, so it cancels out of
                // the quotient; agreement was established above.
                if let Ok(v) = plan.at(&guard, std::hint::black_box(s)) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };
    let sweep_theirs = || {
        let mut acc = 0.0f64;
        for _ in 0..sweeps {
            for &s in &stamps {
                if let Some(v) = tf2.lookup(TARGET, SOURCE, std::hint::black_box(s)) {
                    acc += v.t.x;
                }
            }
        }
        std::hint::black_box(acc)
    };

    // Warm both arms; `sweeps * per_sweep`, since one `sweep_ours` call is
    // `sweeps` passes over the table.
    let per_sweep = stamps.len();
    let per_call = sweeps.saturating_mul(per_sweep).max(1);
    for _ in 0..warmup.div_ceil(per_call) {
        std::hint::black_box(sweep_ours());
        std::hint::black_box(sweep_theirs());
    }

    let per_round = (sweeps * per_sweep) as u64;
    let mut ratios = Vec::with_capacity(rounds);
    let mut ours_ns = Vec::with_capacity(rounds);
    let mut theirs_ns = Vec::with_capacity(rounds);
    for r in 0..rounds {
        // Alternate which arm goes first, or one arm gets the colder cache.
        let (a, b) = if r % 2 == 0 {
            let t0 = std::time::Instant::now();
            let _ = sweep_ours();
            let a = t0.elapsed().as_nanos() as f64 / per_round as f64;
            let t1 = std::time::Instant::now();
            let _ = sweep_theirs();
            let b = t1.elapsed().as_nanos() as f64 / per_round as f64;
            (a, b)
        } else {
            let t1 = std::time::Instant::now();
            let _ = sweep_theirs();
            let b = t1.elapsed().as_nanos() as f64 / per_round as f64;
            let t0 = std::time::Instant::now();
            let _ = sweep_ours();
            let a = t0.elapsed().as_nanos() as f64 / per_round as f64;
            (a, b)
        };
        if a <= 0.0 {
            bail!("a timed round measured {a} ns per lookup, which is not a duration");
        }
        ratios.push(b / a);
        ours_ns.push(a);
        theirs_ns.push(b);
    }

    Ok(Run {
        ratio: median(&mut ratios.clone()),
        ratio_lo: ratios.iter().copied().fold(f64::INFINITY, f64::min),
        ratio_hi: ratios.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        tf_tree_ns: median(&mut ours_ns),
        tf2_ns: median(&mut theirs_ns),
        rounds,
        lookups_per_round: per_round,
        agreed,
    })
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

    /// The verdict is read off the band, not the median.
    ///
    /// Mutant: compare `self.ratio` against `FLOOR` in `verdict`; the
    /// straddling case then reports `Above`.
    #[test]
    fn a_band_straddling_the_floor_is_unresolved_not_a_pass() {
        let base = Run {
            ratio: 2.5,
            ratio_lo: 1.5,
            ratio_hi: 3.5,
            tf_tree_ns: 100.0,
            tf2_ns: 250.0,
            rounds: 9,
            lookups_per_round: 1024,
            agreed: 256,
        };
        assert_eq!(base.verdict(), Verdict::Unresolved);

        let clear = Run {
            ratio_lo: 2.4,
            ratio_hi: 2.6,
            ..base
        };
        assert_eq!(clear.verdict(), Verdict::Above);

        let bad = Run {
            ratio: 1.2,
            ratio_lo: 1.1,
            ratio_hi: 1.3,
            ..base
        };
        assert_eq!(bad.verdict(), Verdict::Below);
    }

    /// [`FLOOR`]'s scope as arithmetic: at cargo's release defaults the
    /// relationship the compile-time `assert!` pins is **false**. A test, not a
    /// second `const` assertion, because it is a measurement expected to move;
    /// when an engine change closes the crate-boundary cost, this failing is the
    /// prompt to promote [`FLOOR`] to a consumer-build claim. No build facts in
    /// here.
    ///
    /// Mutant (observed): `UNBIASED_ESTIMATE_DEFAULT_RELEASE = 2.30` fails the
    /// second assertion while the compile-time one keeps holding.
    ///
    /// `assertions_on_constants` is expected: two of the three constants are
    /// measurements, and asserting on them forces a re-read when one moves.
    #[expect(
        clippy::assertions_on_constants,
        reason = "the constants are measurements that are expected to move; pinning their \
                  relationship is the purpose of the test"
    )]
    #[test]
    fn the_floor_is_bounded_at_one_profile_and_not_the_other() {
        assert!(
            FLOOR < UNBIASED_ESTIMATE,
            "at this workspace's [profile.release] the floor {FLOOR} must sit under the \
             unbiased estimate {UNBIASED_ESTIMATE}, or the binding's bias could pass this row"
        );
        assert!(
            FLOOR > UNBIASED_ESTIMATE_DEFAULT_RELEASE,
            "the floor {FLOOR} is no longer above {UNBIASED_ESTIMATE_DEFAULT_RELEASE}, the \
             unbiased estimate at cargo's release defaults. If that is because the measurement \
             moved, FLOOR has become defensible for a consumer's build and its doc comment's \
             point 3 — a decision record to widen what this gate claims — is now worth writing. \
             Do not simply delete this assertion: it is the record that the gate was scoped."
        );
    }
}
