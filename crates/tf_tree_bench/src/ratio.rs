//! The depth-3 lookup ratio against `tf2::BufferCore`, measured **paired**.
//!
//! # Why a ratio, and why this is the row a 4-core host can gate
//!
//! `docs/PHASE5.md` §9.2's comparison rows all report absolute durations, and
//! this development host cannot produce one honestly: `Fitness::probe` fails on
//! SMT, an unreadable governor, and four physical cores. Every one of those rows
//! is therefore `unavailable`, and the tf2 comparison — the project's central
//! performance claim — is gated by nothing at all.
//!
//! A **quotient of two engines measured inside one round** is a different
//! statistic. The governor and the SMT sibling move both arms together and
//! divide out, which is why `just cpp-bench`'s §7 gate 2 went from flapping
//! across 0.948/1.001/1.002 to a stable 1.006× when it started interleaving
//! rather than comparing medians of two separately-timed loops. `report`'s
//! [`Sensitivity::Ratio`](crate::report::Sensitivity::Ratio) exists for exactly
//! this, and this module is its first row.
//!
//! **Load is the exception and is not divided out**, because the two arms are
//! asymmetric: `tf2::BufferCore` takes a mutex on every lookup and `tf_tree`'s
//! read path takes none, so a busy machine adds lock-holder preemption to one
//! arm only and *inflates* the quotient in our favour. `Fitness::fair_for_ratios`
//! carries that; this module does not have to.
//!
//! # What this number is not
//!
//! **The tf2 column goes through `tf_tree_tf2_sys`, so it flatters `tf_tree`.**
//! `docs/benchmarks/tf2.md` found four measurement biases and priced them; three
//! are removed, and the one that cannot be is the residual FFI boundary —
//! cross-TU, no inlining, one extra copy — worth **45.3 ns (10%)** to tf2 at this
//! depth: 498.2 ns through the binding against 452.9 ns native, a subtraction
//! between two rows of that document's bracket table. (This line used to say
//! "~21 ns (8%)", which contradicted [`FLOOR`]'s own doc comment two screens
//! down; `tf2.md` withdrew the 21 because nothing derived it.)
//! An in-process Rust harness cannot delete it: the only thing that does
//! is `docker/tf2/native_scaling.cpp`, which is the same load with the binding
//! removed outright, and that is where the honest **2.7×** headline comes from
//! rather than from here.
//!
//! So the gate floor below is set well under the measured value on purpose. This
//! row exists to catch a *regression in the engine*, not to publish the headline
//! ratio, and a floor set just beneath a binding-inflated number would be a gate
//! on the binding.
//!
//! It is also single-threaded, one process, warm cursor, repeated stamps — the
//! best case for both engines. The contended rows, where tf_tree's advantage is
//! 27× and tf2 is *anti-scaling*, are `contended_scaling` and `tf2_scaling`, and
//! they are exploratory by design.
//!
//! # Which consumer build this row speaks for — **this workspace's, not yours**
//!
//! `just tf2-bench-check` builds this row with `cargo run --release`, so every
//! number below is taken under the *workspace* `[profile.release]`, which sets
//! `lto = "thin", codegen-units = 1`. **A consumer of `tf_tree` does not get
//! that build.** Cargo applies the **top-level** package's profile to its entire
//! dependency graph, so somebody who runs `cargo add tf_tree` and builds
//! `--release` compiles the engine under *cargo's* release defaults — and cargo's
//! release defaults do not enable LTO. `[profile.embedder]` in the workspace
//! manifest is those defaults written out field by field, and it exists because
//! `docs/API.md` §2.3 item 3 already found this distinction to be worth tens of
//! nanoseconds on the very path this row times: thin LTO inlines `Plan::at`
//! across the `tf_tree` crate boundary into the caller, and `lto = false` does
//! not.
//!
//! So the same paired harness was built twice and run twice, in `docker/tf2`,
//! `taskset -c 2`, one session (2026-08-15) — `just tf2-ratio-profiles`:
//!
//! | build | `lto` | tf_tree | tf2 (via binding) | **paired ratio** | band |
//! |---|---|---|---|---|---|
//! | workspace `[profile.release]` — what this row gates | `"thin"` | 201.6 ns | 504.4 ns | **2.490×** | 2.452–2.547 |
//! | `[profile.embedder]` — cargo's release defaults, i.e. a consumer | `false` | 244.2 ns | 506.1 ns | **2.075×** | 2.063–2.080 |
//!
//! **The tf2 column is the control and it holds: 504.4 → 506.1 ns, +0.34%.**
//! That arm goes through `tf_tree_tf2_sys`, an `extern "C"` call into a C++ shim
//! that no Rust LTO setting can inline into, so it *should* be invariant across
//! the two builds — and it is, to well inside this host's run-to-run spread.
//! The tf_tree column moves +21.1% and is the whole of the difference. Without
//! that control the two runs would be two numbers from two processes and the
//! comparison would not be evidence, which is the failure this file's history is
//! made of.
//!
//! A second control, on the axis that should *not* matter: `[profile.profiling]`
//! inherits `[profile.release]` and differs from it only in debuginfo. It
//! measures 200.4 ns / 2.468× — the LTO arm, not the embedder one. So the number
//! tracks `lto` and not "a profile other than release".
//!
//! **The consequence for [`FLOOR`] is stated there, and it is not comfortable.**

use anyhow::{anyhow, bail, Result};

use tf_tree::{InterpPolicy, Stamp};

use crate::tf2::Tf2Fixture;

/// The floor this row gates: `tf_tree` must be at least this many times faster
/// than `tf2` on a depth-3 hot lookup, **built the way this workspace builds
/// it** — `[profile.release]`, `lto = "thin"`. See the module docs for why that
/// qualifier is not a technicality.
///
/// At that build the row measures 2.49× against a floor of 2.0, so a ~24%
/// margin. The margin has to clear the *bias*, not the noise: the tf2 arm here
/// pays the Rust binding, worth ~10%, so the unbiased figure for this fixture is
/// [`UNBIASED_ESTIMATE`] and the floor sits under it. A floor above it could be
/// passed by the binding alone.
///
/// # The floor is **not** defensible for a consumer's default `--release`
///
/// Say it plainly, because the arithmetic is short and the conclusion is
/// unwelcome. At `[profile.embedder]` — cargo's release defaults, what somebody
/// who `cargo add`s this crate compiles — the tf_tree arm is 244.2 ns, not
/// 201.6. Against the *native C++* tf2 figure, which is the only tf2 number with
/// no binding in it, that is [`UNBIASED_ESTIMATE_DEFAULT_RELEASE`] ≈ **1.80×**,
/// and 1.80 < 2.0. **The relationship this file's compile-time check exists to
/// enforce — floor under the unbiased estimate — is false at a consumer's
/// default build.** The row still *passes* there, at a paired 2.075×, and that
/// is the point: it passes on 0.27× of binding bias it is not entitled to.
///
/// Three things follow, and only the first is done here.
///
/// 1. **The floor is not lowered.** Lowering 2.0 to clear 1.80 would weaken the
///    gate for the build it is actually measured in, to describe a build it is
///    never measured in, and it would be a change made to make a number look
///    right — the sample-selection move this repository has caught before. The
///    constant stays, and its scope is now written down instead of assumed.
/// 2. **The claim is scoped, not withdrawn.** `just tf2-bench-check` builds
///    `--release` in this workspace; the gate therefore *is* a statement about
///    this workspace's build, and always was. That is honest as a **regression
///    detector**, which is all the module docs ever claimed for it, and it is
///    *not* a consumer-facing guarantee. The headline a consumer should be
///    quoted remains `tf2.md`'s ~2.7× recorded-stream row, whose provenance is
///    its own.
/// 3. **Gating the consumer build instead would change what this floor means**,
///    and by `CLAUDE.md`'s rule that is a decision record, not an edit here. The
///    honest shape of it — a second gated row at `[profile.embedder]` with its
///    own floor under 1.80, since `runstore::BUILD_CRITICAL_FACTS` already
///    refuses to compare two profiles against one baseline — is drafted nowhere
///    yet. `just tf2-ratio-profiles` is the measurement that record would rest
///    on.
pub const FLOOR: f64 = 2.0;

/// The same fixture with **no binding on either arm**, at this workspace's
/// `[profile.release]`: `tf_tree` native Rust (201.5 ns) against `tf2` native
/// C++ (452.9 ns), from `docker/tf2/native_ratio.sh`.
///
/// **This used to be 2.7 and that was the wrong number.** 2.7× is `tf2.md`'s
/// *recorded-stream* row — a different fixture and a different loop shape — and
/// using it here made the check below assert a relationship between two
/// measurements that were never taken together. The figure for this fixture is
/// 2.25×, and it is unpaired (the two halves come from different processes), so
/// it is a point estimate rather than a gate. That is exactly what it is used
/// for: bounding [`FLOOR`], not being one.
///
/// Re-measured 2026-08-15 in the same session as the profile table above: tf2
/// native C++ 439.2 ns against 201.6 ns, i.e. **2.18×**. The constant is left at
/// 2.25 rather than chased to the newer run — both are unpaired point estimates
/// of the same quantity on a host with a few percent of day-to-day spread, and
/// re-cutting a bound to the latest sample is how a bound stops being one. What
/// would falsify it is a run materially under 2.0, which neither is.
const UNBIASED_ESTIMATE: f64 = 2.25;

/// The same quantity at **cargo's release defaults** — `[profile.embedder]`,
/// what a consumer compiles: 439.2 ns native C++ tf2 against 244.2 ns tf_tree,
/// both measured 2026-08-15, `docker/tf2`, `taskset -c 2`.
///
/// Against `UNBIASED_ESTIMATE`'s older tf2 half (452.9 ns) it is 1.86× instead;
/// the range 1.80–1.86 is this host's spread on an unpaired quotient and nothing
/// in it reaches 2.0. That is the whole point of the constant, so it is recorded
/// at the pessimistic end.
///
/// It bounds nothing — no gate reads it — and that is deliberate: it exists so
/// that [`FLOOR`]'s prose is checkable arithmetic rather than a claim, via
/// `the_floor_is_bounded_at_one_profile_and_not_the_other` below.
///
/// `pub` where [`UNBIASED_ESTIMATE`] is private, and the asymmetry is the point
/// rather than an oversight: the private one is machinery, consumed by the
/// compile-time assertion below and by nothing else, while this one is the
/// caveat on a number people quote. A reader who reaches [`FLOOR`] in rendered
/// documentation should be able to follow the link that says the floor does not
/// hold for their build. (It is also the only thing keeping `dead_code` quiet
/// outside `cfg(test)`, which is a symptom of the same fact: a constant nothing
/// executes is documentation, so it should be documentation people can see.)
pub const UNBIASED_ESTIMATE_DEFAULT_RELEASE: f64 = 1.80;

/// [`FLOOR`] must stay under the unbiased estimate, or this row could be passed
/// by the binding's bias rather than by the engine.
///
/// A compile-time check and not a test, because it is a relationship between two
/// constants: there is nothing to run.
///
/// **It is a statement about this workspace's `lto = "thin"` build only.** The
/// same relationship at a consumer's default `--release` is false, which is a
/// fact about the gate rather than about these two constants, so it is pinned by
/// a test below rather than by a second `assert!` that would refuse to compile.
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

/// Stamps off every dynamic grid, so `I::eval` actually runs.
///
/// The same construction `embed.rs` uses and for the same reason: `NOW_NS` is an
/// exact multiple of all four dynamic periods, so a sweep anchored on it would
/// take `SampleRing::sample`'s exact-hit branch and measure `bracket` plus a
/// seqlock read. That is the defect `docs/decisions/0013` is about, and it must
/// not be reintroduced in a second harness.
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
    /// The band straddles the floor, so this run cannot answer.
    ///
    /// Reported rather than resolved by taking the median, for `embed.rs`'s
    /// reason: a gate whose noise floor exceeds its own threshold has not
    /// measured anything, and saying so is the only honest output.
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
    /// Median per-round `tf2_ns / tf_tree_ns`.
    ///
    /// **Paired, and deliberately not the quotient of the two medians below.**
    /// The arms are timed back to back inside one round, so drift common to both
    /// cancels out of each round's ratio in a way it cannot cancel out of a
    /// quotient of two separately-timed loops.
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

/// Measure the pair. See the module docs for what the number is and is not.
///
/// # Errors
///
/// If the fixture cannot be built, the pair cannot be planned, tf2 cannot be
/// loaded, or the two engines disagree on an answer before either is timed.
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
    // `LerpSlerp` on both sides: it is tf2's interpolation policy, and comparing
    // our default `ScLerp` against it would be measuring a deliberate difference
    // in output rather than a difference in speed. `PROJECT.md` §5 D5 keeps
    // `LerpSlerp` for precisely this.
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
    // Converted once, outside every timed loop: `Stamp::from_nanos` is a
    // constructor, not part of a lookup, and timing it on our side only would be
    // a fifth measurement bias on top of the four `tf2.md` already prices.
    let ours_stamps: Vec<Stamp> = stamps.iter().map(|&s| Stamp::from_nanos(s)).collect();

    // **The two engines must agree on the answers before either is timed.**
    // `embed.rs` makes the same check for the same reason: an arm that is fast
    // because it is answering a different question would move the ratio and
    // nothing in the timing would say so. Here it also catches a tf2 horizon
    // miss, which would otherwise time a `None` return against a real lookup.
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
                // Both arms carry one `if let` for their own result type, so the
                // branch is symmetric and cancels out of the quotient. It cannot
                // be an `expect`: this crate denies it, and a panic in a timed
                // loop is not a measurement anyway. Agreement was established
                // above, so neither arm takes the else.
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

    // Warm both arms: tf2's buffer walks the topology per call and populates its
    // own caches, and ours faults in the rings.
    // `sweeps * per_sweep`, not `per_sweep`: one call to `sweep_ours` is
    // `sweeps` passes over the table, so dividing by the table alone overshot
    // the documented warmup by 40x and made `measure_with`'s loop counts unable
    // to bound the cost — which is the whole reason that escape hatch exists.
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
        // Alternate which arm goes first. A fixed order gives one arm the colder
        // cache in every round, which is a bias the pairing would otherwise
        // preserve rather than cancel.
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

    /// The verdict is read off the band, not off the median — the property that
    /// makes an `unresolved` outcome possible at all.
    ///
    /// Mutant: make `verdict` compare `self.ratio` against `FLOOR` instead of
    /// `ratio_lo`/`ratio_hi` — the straddling case below then reports `Above`
    /// and this test fails.
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

    /// [`FLOOR`]'s scope, as arithmetic rather than as prose.
    ///
    /// The compile-time `assert!` above pins one half — the floor sits under the
    /// unbiased estimate for this workspace's `lto = "thin"` build, so the
    /// binding's bias cannot pass it. This pins the *other* half, the one that
    /// is unwelcome and therefore the one most likely to be quietly forgotten:
    /// at cargo's release defaults the same relationship is **false**, and the
    /// floor is above the unbiased figure rather than under it.
    ///
    /// A test and not a second `const _: () = assert!(...)`, because the second
    /// relationship is the one that does **not** hold: spelling it as a
    /// compile-time assertion would mean either refusing to build or asserting
    /// its negation, and asserting a negation reads like a requirement that the
    /// consumer build be slow. It is not a requirement, it is a measurement, and
    /// the day it stops being true — an engine change that closes the 21% the
    /// crate boundary costs at `lto = false` — this test failing is exactly the
    /// notification wanted, because at that point [`FLOOR`] could honestly be
    /// promoted to a claim about a consumer's build.
    ///
    /// Deliberately no build facts in here (`cfg!`, `PROFILE_DIR`, the manifest):
    /// this is a relationship between three constants and it must read the same
    /// in every configuration, unlike `runstore`'s `differing_value`, whose
    /// subject genuinely is the build.
    ///
    /// Mutant (applied, observed): set `UNBIASED_ESTIMATE_DEFAULT_RELEASE` to
    /// `2.30` — the second assertion fails with the message below, and the
    /// compile-time one keeps holding, which is the point of separating them.
    ///
    /// `assertions_on_constants` is expected rather than worked around. The lint
    /// exists to catch a tautology nobody will ever revisit, and this is the
    /// opposite: two of the three constants are measurements, they are expected
    /// to move, and asserting on them *is* the mechanism by which a future edit
    /// to one is forced to re-read the other two. Rewriting the comparison
    /// through a helper to dodge the lint would leave the same assertion with
    /// its intent hidden.
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
