//! Which interpolation region a real `/tf` stream actually lands in — step 0a of
//! [`0060`](../../../docs/decisions/0060-the-batch-fold-that-reads-before-it-interpolates.md).
//!
//! ```sh
//! cargo run --release -p tf_tree_bench --example bracket_mix
//! cargo run --release -p tf_tree_bench --example bracket_mix -- <stream.tfstream> [sweep_hz]
//! ```
//!
//! # Why this exists
//!
//! `0060` measured a two-phase batch fold (Decision A) and an SoA interpolation
//! kernel on top of it (Decision B) against the synthetic fixture, and found
//! −43% to −58% on the flagship `at_many` row. Both numbers were taken on data
//! that keeps nearly every bracket in `slerp`'s **series region** — the regime
//! the kernel is for. B's kernel does not apply anywhere else: a bracket outside
//! the series region is recomputed by a scalar fix-up, and `0060` §5 measured
//! the prototype **losing** 80% on an all-fallback cell.
//!
//! So the headline is a function of the data, and nobody had measured what the
//! data is. This example is that measurement, and it deliberately contains **no
//! engine code and no prototype**: it classifies the brackets an `at_many` sweep
//! would read, and says nothing about how fast anything runs.
//!
//! # The five classes
//!
//! They are the arms of [`tf_tree::slerp`] and of `dualquat`'s `screw_parts` /
//! `ScrewParts::pow`, which is what the kernel's safe-region predicate tests:
//!
//! | class | `LerpSlerp` | `ScLerp` |
//! |---|---|---|
//! | **exact hit** | the stamp is a knot: one slot read, no `eval` at all | same |
//! | **stationary** | `h == 0`: the two rotations are bit-identical, `slerp` returns `qa` | `sin²(θ/2) < 1e-290`: `screw_parts` takes its degenerate return |
//! | **LERP fallback** | `θ² < 1e-12`: near-parallel, LERP and renormalise | — (`ScLerp` has no such arm) |
//! | **series** | `1e-12 ≤ θ² ≤ 0.0225`: the polynomial weights — **the kernel's regime** | `1e-290 ≤ sin²(θ/2) ≤ 0.02233…`: same |
//! | **large arc** | `θ² > 0.0225`: `acos` / `sin`, the exact form | `sin²(θ/2) > 0.02233…`: the exact form |
//!
//! A bracket is in the series region when consecutive samples are *close* — so
//! the fraction is set by publish rate against how fast the body turns, and a
//! low-rate edge on a moving body is pushed out of it.
//!
//! # Three sweeps, because the answer depends on what you ask for
//!
//! - **`rate`** (the headline): a fixed-rate grid at 100 Hz, offset 1 ns off the
//!   window origin so nothing lands on a knot. This is a consumer batching
//!   lookups at its own sensor rate, and it weights each bracket by its
//!   **duration** — a long gap in the recording counts once per query inside it.
//! - **`interval`**: one query at the midpoint of every sample interval. Each
//!   bracket is counted exactly once, so this is the recording's intrinsic
//!   per-bracket distribution with no duration weighting.
//! - **`ongrid`**: a query at every recorded stamp, which is 100% exact hits by
//!   construction. It is the exact-hit ceiling for a consumer whose clock is the
//!   publisher's.
//!
//! # Four controls, so that every column can be non-zero
//!
//! `0060`'s plan asked for two, *"without both, a classifier reading everything
//! as series passes"*. There are four because two of them found something:
//!
//! | control | `LerpSlerp` | `ScLerp` |
//! |---|---|---|
//! | the synthetic [`fixture`], 50–1000 Hz smooth screw | 100% series | 100% series |
//! | one pose repeated, the recorded wheel edges' quaternion | 100% stationary | 100% stationary |
//! | one pose repeated, four non-zero components | 100% stationary | **100% series** |
//! | ~1e-7 rad of jitter a sample | **100% LERP fallback** | 100% series |
//!
//! The third exists because the stationary control **as specified fails**: a
//! motionless `ScLerp` edge takes its degenerate arm only when the quaternion's
//! zero pattern makes `conj(q) ⊗ q` cancel exactly, which the recording's wheel
//! frames happen to do and a general rotation does not. [`control_stationary`]
//! has the arithmetic. The fourth exists because without it the LERP-fallback
//! column reads 0.0% in every row this example prints, and a column that can
//! only ever be zero is decoration rather than a measurement.
//!
//! # The bracket search is checked against the engine, per stamp
//!
//! This example mirrors `SampleRing::sample_from` under `ExtrapPolicy::Error`
//! rather than calling it — the bracket is not a public return. A mirror that
//! drifted would classify pairs the engine never reads, so every swept stamp is
//! also put through `Plan::at` on the same one-edge plan and the two are
//! required to agree **bit-identically**: a decline against an `Err`, an exact
//! hit against the recorded pose, and a bracket against `I::eval(a, b, s)`. The
//! count of checked stamps is printed, so a check that stopped running is
//! visible rather than silent.
#![allow(clippy::print_stdout)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};

use tf_tree::{
    Interp, InterpPolicy, Iso3, LerpSlerp, Quat, ScLerp, Stamp, SystemDomain, Tree, Vec3,
};
use tf_tree_bench::{fixture, replay::TfStream};

/// The chunk a batch fold would classify and bail out on, as prototyped.
const CHUNK: usize = 64;

/// `SLERP_LERP_FALLBACK`, `tf_tree_math::interp`'s private constant.
///
/// Pinned there by `const _: () = assert!(SLERP_LERP_FALLBACK == 1e-6);`, so a
/// change to it breaks that crate's build rather than silently moving this
/// classifier's boundary.
const SLERP_LERP_FALLBACK: f64 = 1e-6;

/// `THETA_SLERP_SMALL`, pinned the same way (`assert!(THETA_SLERP_SMALL == 0.15)`).
const THETA_SLERP_SMALL: f64 = 0.15;

/// `SIN_HALF_THETA_SMALL_SQ`, `tf_tree_math::dualquat`'s private constant.
///
/// Pinned there against `sin(THETA_SLERP_SMALL)²` by a unit test, so it moves
/// only when `THETA_SLERP_SMALL` does.
const SIN_HALF_THETA_SMALL_SQ: f64 = 0.022_331_755_437_196_99;

/// `SCREW_DEGENERATE_SQ`, `tf_tree_math::dualquat`'s degenerate-screw floor.
const SCREW_DEGENERATE_SQ: f64 = 1e-290;

/// How close to a class boundary a bracket has to be before the route this
/// example takes to `θ²` could change its bucket. See [`Counts::near_boundary`].
const BOUNDARY_BAND: f64 = 1e-9;

/// Which arm of the interpolant a bracket lands in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Class {
    /// The kernel's regime: the polynomial weights.
    Series,
    /// `h == 0` under `LerpSlerp`, a degenerate screw under `ScLerp`.
    Stationary,
    /// Near-parallel: LERP and renormalise. `LerpSlerp` only.
    LerpFallback,
    /// Past the small-angle threshold: the exact `acos` / `sin` form.
    LargeArc,
    /// The stamp is a knot; no interpolation happens.
    ExactHit,
}

impl Class {
    const ALL: [Class; 5] = [
        Class::Series,
        Class::Stationary,
        Class::LerpFallback,
        Class::LargeArc,
        Class::ExactHit,
    ];

    fn label(self) -> &'static str {
        match self {
            Class::Series => "series",
            Class::Stationary => "stationary",
            Class::LerpFallback => "lerp_fb",
            Class::LargeArc => "large_arc",
            Class::ExactHit => "exact_hit",
        }
    }
}

/// `θ²` for a quaternion pair, from the chord rather than from `acos`.
///
/// `tf_tree_math` reaches the same number through an eight-term series
/// (`theta_sq_from_chord`) that exists to avoid forming `1 − dot`. This example
/// is handed `h` directly and so has no cancellation to avoid: `h/2 = sin²(θ/2)`
/// exactly, and `asin` near zero is well conditioned. Taking the independent
/// route is deliberate — a classifier that re-derived the series would inherit
/// whatever the series gets wrong, and [`Counts::near_boundary`] counts the
/// brackets where the two routes could possibly disagree about a bucket.
fn theta_sq_from_chord(h: f64) -> f64 {
    let theta = 2.0 * (h * 0.5).sqrt().min(1.0).asin();
    theta * theta
}

/// How far a bracket sits from the nearest class boundary, in relative terms.
///
/// `f64::INFINITY` when the bracket is not near one.
fn boundary_distance(x: f64, edges: &[f64]) -> f64 {
    edges
        .iter()
        .map(|e| ((x - e) / e).abs())
        .fold(f64::INFINITY, f64::min)
}

/// Classify one bracket, returning the class and its distance from the nearest
/// boundary.
fn classify(policy: InterpPolicy, a: &Iso3, b: &Iso3) -> (Class, f64) {
    match policy {
        InterpPolicy::LerpSlerp => {
            let qa = a.q;
            let qb = if qa.dot(b.q) < 0.0 { b.q.neg() } else { b.q };
            let h = 0.5 * qa.sub(qb).norm_squared();
            if h <= 0.0 {
                return (Class::Stationary, f64::INFINITY);
            }
            let theta_sq = theta_sq_from_chord(h);
            let lo = SLERP_LERP_FALLBACK * SLERP_LERP_FALLBACK;
            let hi = THETA_SLERP_SMALL * THETA_SLERP_SMALL;
            let near = boundary_distance(theta_sq, &[lo, hi]);
            if theta_sq < lo {
                (Class::LerpFallback, near)
            } else if theta_sq > hi {
                (Class::LargeArc, near)
            } else {
                (Class::Series, near)
            }
        }
        InterpPolicy::ScLerp => {
            // `inv_mul`'s rotation part, which is what the kernel's safe-region
            // predicate computes.
            let rel = a.q.conjugate() * b.q;
            let rel = if rel.w < 0.0 { rel.neg() } else { rel };
            let sh2 = rel.vector().norm_squared();
            let near = boundary_distance(sh2, &[SCREW_DEGENERATE_SQ, SIN_HALF_THETA_SMALL_SQ]);
            if sh2 < SCREW_DEGENERATE_SQ {
                (Class::Stationary, near)
            } else if sh2 > SIN_HALF_THETA_SMALL_SQ {
                (Class::LargeArc, near)
            } else {
                (Class::Series, near)
            }
        }
    }
}

/// What a read of one edge at one stamp resolves to.
enum Read {
    /// Outside `[oldest, newest]`: `ExtrapPolicy::Error` declines.
    Declined,
    /// The stamp is a knot; the sample is returned unchanged.
    Hit(Iso3),
    /// `t_i < t < t_j`, with `s` the fraction.
    Bracket(Iso3, Iso3, f64),
}

/// `SampleRing::sample_from` under `ExtrapPolicy::Error`, mirrored over the
/// recorded sample list.
///
/// Every caller checks the answer against `Plan::at` on the same edge, so a
/// drift between this and the engine fails rather than skews the table.
fn read(samples: &[(i64, Iso3)], t: i64) -> Read {
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        return Read::Declined;
    };
    let (t_old, t_new) = (first.0, last.0);
    if t < t_old || t > t_new {
        return Read::Declined;
    }
    if t == t_new {
        return Read::Hit(last.1);
    }
    // `bracket_from`: the last index whose stamp is `<= t`. The preconditions
    // `stamp[lo] <= t < stamp[hi]` hold from the two tests above.
    let (mut lo, mut hi) = (0usize, samples.len() - 1);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if samples[mid].0 <= t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    if samples[lo].0 == t {
        return Read::Hit(samples[lo].1);
    }
    let (t_i, t_j) = (samples[lo].0, samples[lo + 1].0);
    // `span_ns`, spelled the way `sample.rs` spells it: a wrapping `u64`
    // difference, so the two cannot diverge on a stamp pair that straddles
    // `i64::MAX`.
    let span = |from: i64, to: i64| (to as u64).wrapping_sub(from as u64) as f64;
    let s = span(t_i, t) / span(t_i, t_j);
    Read::Bracket(samples[lo].1, samples[lo + 1].1, s)
}

/// A class tally, plus the two things that say whether it can be believed.
#[derive(Default, Clone)]
struct Counts {
    per_class: BTreeMap<Class, usize>,
    /// Stamps the sweep asked for that the edge declined. Not a class.
    declined: usize,
    /// Brackets within [`BOUNDARY_BAND`] of a class boundary — the only ones
    /// whose bucket could depend on which route this example takes to `θ²`. A
    /// non-zero count here means the table has a footnote to write.
    near_boundary: usize,
    /// Stamps put through `Plan::at` and required to agree bit-for-bit.
    checked: usize,
}

impl Counts {
    fn total(&self) -> usize {
        self.per_class.values().sum()
    }

    fn frac(&self, c: Class) -> f64 {
        let n = self.total();
        if n == 0 {
            return 0.0;
        }
        *self.per_class.get(&c).unwrap_or(&0) as f64 / n as f64
    }

    fn row(&self) -> String {
        let cells: Vec<String> = Class::ALL
            .iter()
            .map(|c| format!("{:>7.1}%", 100.0 * self.frac(*c)))
            .collect();
        format!(
            "{:>6} {} {:>5} {:>4} {:>7}",
            self.total(),
            cells.join(" "),
            self.declined,
            self.near_boundary,
            self.checked
        )
    }
}

fn header(what: &str) -> String {
    let cells: Vec<String> = Class::ALL
        .iter()
        .map(|c| format!("{:>8}", c.label()))
        .collect();
    format!(
        "  {what:<34} {:<10} {:>6} {} {:>5} {:>4} {:>7}",
        "policy",
        "n",
        cells.join(" "),
        "decl",
        "near",
        "checked"
    )
}

/// The per-chunk view a bail-out would see: for every 64-stamp chunk, the
/// fraction of its elements the kernel's classifier admits.
struct ChunkStats {
    fracs: Vec<f64>,
}

impl ChunkStats {
    fn summarise(&self) -> String {
        if self.fracs.is_empty() {
            return "no chunks".to_owned();
        }
        let mut v = self.fracs.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        let q = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
        let all_in = v.iter().filter(|f| **f >= 0.999).count();
        let all_out = v.iter().filter(|f| **f <= 0.001).count();
        format!(
            "chunks={:<5} series_frac p10={:.2} p50={:.2} p90={:.2}  100%={} 0%={}",
            v.len(),
            q(0.10),
            q(0.50),
            q(0.90),
            all_in,
            all_out
        )
    }
}

/// Which stamps to ask for.
#[derive(Clone, Copy)]
enum Sweep {
    /// A fixed-rate grid, offset 1 ns so nothing lands on a knot.
    Rate(f64),
    /// One query at the midpoint of every sample interval.
    Interval,
    /// A query at every recorded stamp.
    OnGrid,
}

impl Sweep {
    fn label(self) -> String {
        match self {
            Sweep::Rate(hz) => format!("rate {hz:.0} Hz"),
            Sweep::Interval => "interval midpoints".to_owned(),
            Sweep::OnGrid => "on-grid (every knot)".to_owned(),
        }
    }

    fn stamps(self, samples: &[(i64, Iso3)]) -> Vec<i64> {
        let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
            return Vec::new();
        };
        let (t_old, t_new) = (first.0, last.0);
        match self {
            Sweep::Rate(hz) => {
                #[allow(clippy::cast_possible_truncation)]
                let step = (1e9 / hz) as i64;
                let mut v = Vec::new();
                let mut t = t_old + 1;
                while t < t_new {
                    v.push(t);
                    t += step.max(1);
                }
                v
            }
            Sweep::Interval => samples
                .windows(2)
                .map(|w| w[0].0 + (w[1].0 - w[0].0) / 2)
                .collect(),
            Sweep::OnGrid => samples.iter().map(|s| s.0).collect(),
        }
    }
}

/// Sweep one edge and tally it, checking every stamp against the engine.
fn sweep_edge(
    tree: &Tree,
    parent: &str,
    child: &str,
    samples: &[(i64, Iso3)],
    policy: InterpPolicy,
    sweep: Sweep,
) -> Result<(Counts, ChunkStats)> {
    let p = tree
        .frame(parent)
        .map_err(|e| anyhow!("frame {parent}: {e}"))?;
    let c = tree
        .frame(child)
        .map_err(|e| anyhow!("frame {child}: {e}"))?;
    // `plan(target, source)` is `lookup(target, source)`, so for an edge that
    // stores `T_parent_child` the plan that returns the sample unchanged is
    // `plan(parent, child)`. The per-stamp bit-identity check below is what says
    // so: the wrong direction fails on the first exact hit.
    let plan = tree
        .plan(p, c)
        .map_err(|e| anyhow!("plan {parent}->{child}: {e}"))?;
    let guard = tree.guard();

    let mut counts = Counts::default();
    let mut chunk_admitted = 0usize;
    let mut chunk_len = 0usize;
    let mut fracs = Vec::new();

    for t in sweep.stamps(samples) {
        let got = plan.at(&guard, Stamp::<SystemDomain>::from_nanos(t));
        let class = match read(samples, t) {
            Read::Declined => {
                if got.is_ok() {
                    bail!("mirror declined {t} on {parent}->{child} but the engine answered");
                }
                counts.declined += 1;
                counts.checked += 1;
                continue;
            }
            Read::Hit(p) => {
                let got = got.map_err(|e| anyhow!("engine declined a hit at {t}: {e}"))?;
                if got.to_bits() != p.to_bits() {
                    bail!("hit at {t} on {parent}->{child}: engine {got:?} != sample {p:?}");
                }
                counts.checked += 1;
                Class::ExactHit
            }
            Read::Bracket(a, b, s) => {
                let got = got.map_err(|e| anyhow!("engine declined a bracket at {t}: {e}"))?;
                let want = match policy {
                    InterpPolicy::LerpSlerp => LerpSlerp::eval(&a, &b, s),
                    InterpPolicy::ScLerp => ScLerp::eval(&a, &b, s),
                };
                if got.to_bits() != want.to_bits() {
                    bail!("bracket at {t} on {parent}->{child}: engine != eval(a, b, {s})");
                }
                counts.checked += 1;
                let (class, near) = classify(policy, &a, &b);
                if near < BOUNDARY_BAND {
                    counts.near_boundary += 1;
                }
                class
            }
        };
        *counts.per_class.entry(class).or_insert(0) += 1;

        chunk_admitted += usize::from(class == Class::Series);
        chunk_len += 1;
        if chunk_len == CHUNK {
            fracs.push(chunk_admitted as f64 / chunk_len as f64);
            chunk_admitted = 0;
            chunk_len = 0;
        }
    }
    if chunk_len > 0 {
        fracs.push(chunk_admitted as f64 / chunk_len as f64);
    }
    Ok((counts, ChunkStats { fracs }))
}

/// The publish rate at which each recorded interval would fall inside the series
/// region, and what that says about a rate this recording does not carry.
///
/// **Both policies share one bound, and it is a statement about angle, not about
/// either kernel.** `LerpSlerp` is in its series arm when `θ² ≤
/// THETA_SLERP_SMALL²`, where `θ` is the *quaternion* angle between the two
/// samples. `ScLerp` is in its series arm when `sin²(θ) ≤
/// SIN_HALF_THETA_SMALL_SQ`, and that constant is defined as `sin(0.15)²` — so
/// both reduce to `θ ≤ 0.15 rad`, a body rotation of `0.30 rad` (17.2°) between
/// consecutive samples.
///
/// So for an interval of length `Δt` whose endpoints are `θ` apart, the same
/// motion published at `f` Hz would put `θ/(f·Δt)` between samples, and the
/// interval is in the series region for every `f ≥ θ/(0.15·Δt)`. That number is
/// this edge's *series rate* for that interval.
///
/// **This is an extrapolation and its model is ScLerp's own**: it assumes the
/// body turns at a constant rate across the interval, which is exactly what the
/// interpolant assumes when it answers a query inside it. It is worth taking
/// because the alternative — reporting one recording's series fraction and
/// stopping — says nothing about a corpus with different rates, and publish rate
/// is the single variable the fraction is most sensitive to.
struct SeriesRate {
    /// Per interval, the publish rate above which it is series, in Hz. Only
    /// intervals that rotate at all: an interval whose endpoints are the same
    /// rotation is never series, at any rate.
    hz: Vec<f64>,
    /// Intervals whose endpoints are bit-identical rotations (`θ == 0`).
    motionless: usize,
    /// Intervals that are *large arc at the rate they were actually published
    /// at*, with the longest such interval's duration. On a recording whose
    /// publisher never stops these are the fast-motion intervals; on one whose
    /// publisher does, they are the gaps.
    large_arc: usize,
    longest_large_arc_s: f64,
    /// The recording's own median interval, in Hz.
    actual_hz: f64,
}

impl SeriesRate {
    fn measure(samples: &[(i64, Iso3)]) -> SeriesRate {
        let mut out = SeriesRate {
            hz: Vec::new(),
            motionless: 0,
            large_arc: 0,
            longest_large_arc_s: 0.0,
            actual_hz: 0.0,
        };
        let mut dts = Vec::with_capacity(samples.len().saturating_sub(1));
        for w in samples.windows(2) {
            let dt = (w[1].0 - w[0].0) as f64 * 1e-9;
            if dt <= 0.0 {
                continue;
            }
            dts.push(dt);
            let qa = w[0].1.q;
            let qb = if qa.dot(w[1].1.q) < 0.0 {
                w[1].1.q.neg()
            } else {
                w[1].1.q
            };
            let h = 0.5 * qa.sub(qb).norm_squared();
            if h <= 0.0 {
                out.motionless += 1;
                continue;
            }
            let theta = 2.0 * (h * 0.5).sqrt().min(1.0).asin();
            if theta > THETA_SLERP_SMALL {
                out.large_arc += 1;
                out.longest_large_arc_s = out.longest_large_arc_s.max(dt);
            }
            out.hz.push(theta / (THETA_SLERP_SMALL * dt));
        }
        dts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        out.actual_hz = if dts.is_empty() {
            0.0
        } else {
            1.0 / dts[dts.len() / 2]
        };
        out
    }

    fn summarise(&self) -> String {
        let head = format!(
            "published at {:.1} Hz (median interval); motionless={} large_arc={}",
            self.actual_hz, self.motionless, self.large_arc
        );
        if self.hz.is_empty() {
            return format!("{head}; no rotating interval");
        }
        let mut v = self.hz.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        let q = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
        let gap = if self.large_arc > 0 {
            format!(" longest_large_arc={:.2} s", self.longest_large_arc_s)
        } else {
            String::new()
        };
        format!(
            "{head}{gap}\n  {:<34} series above p50={:.3} p90={:.3} p99={:.3} max={:.3} Hz",
            "",
            q(0.50),
            q(0.90),
            q(0.99),
            v[v.len() - 1]
        )
    }
}

/// Per-edge sample lists, in the stream's edge order.
fn samples_by_edge(stream: &TfStream) -> Vec<Vec<(i64, Iso3)>> {
    let mut out = vec![Vec::new(); stream.dynamic_edges.len()];
    for s in &stream.samples {
        out[s.edge].push((s.stamp_ns, s.pose));
    }
    out
}

fn report_stream(name: &str, stream: &TfStream, sweeps: &[Sweep]) -> Result<()> {
    let by_edge = samples_by_edge(stream);
    for &policy in &[InterpPolicy::LerpSlerp, InterpPolicy::ScLerp] {
        let tree = stream.build_tree(policy)?;
        for &sweep in sweeps {
            println!("\n{name} — {} — {policy:?}", sweep.label());
            println!("{}", header("edge"));
            let mut total = Counts::default();
            for (i, (p, c)) in stream.dynamic_edges.iter().enumerate() {
                if by_edge[i].len() < 2 {
                    continue;
                }
                let (counts, chunks) = sweep_edge(&tree, p, c, &by_edge[i], policy, sweep)?;
                println!("  {:<34} {:<10} {}", format!("{p}->{c}"), "", counts.row());
                println!("  {:<34} {}", "", chunks.summarise());
                println!(
                    "  {:<34} {}",
                    "",
                    SeriesRate::measure(&by_edge[i]).summarise()
                );
                for cl in Class::ALL {
                    *total.per_class.entry(cl).or_insert(0) +=
                        counts.per_class.get(&cl).copied().unwrap_or(0);
                }
                total.declined += counts.declined;
                total.near_boundary += counts.near_boundary;
                total.checked += counts.checked;
            }
            println!("  {:<34} {:<10} {}", "ALL EDGES", "", total.row());
        }
    }
    Ok(())
}

/// Control 1: the synthetic fixture, whose 50–1000 Hz edges must read ~100%
/// series.
fn control_fixture(sweeps: &[Sweep]) -> Result<()> {
    let stream = fixture_stream()?;
    report_stream("CONTROL fixture (expect ~100% series)", &stream, sweeps)
}

/// The fixture's dynamic history, as a [`TfStream`] so it goes through exactly
/// the same path as the recording.
fn fixture_stream() -> Result<TfStream> {
    let mut stream = TfStream::default();
    for (i, (p, c, rate)) in fixture::DYNAMIC_EDGES.iter().enumerate() {
        stream
            .dynamic_edges
            .push(((*p).to_owned(), (*c).to_owned()));
        #[allow(clippy::cast_possible_truncation)]
        let step = (1e9 / rate) as i64;
        let n = (fixture::NOW_NS / step).min(4096);
        for k in 0..n {
            let t = fixture::NOW_NS - (n - 1 - k) * step;
            stream.samples.push(tf_tree_bench::replay::Sample {
                edge: i,
                stamp_ns: t,
                pose: fixture::dynamic_pose(i as f64, t),
            });
        }
    }
    stream.samples.sort_by_key(|s| s.stamp_ns);
    Ok(stream)
}

/// Control 2: one dynamic edge pushed the **same pose** every time, in two
/// quaternion shapes — and the two do not agree, which is the point.
///
/// `LerpSlerp` reads a repeated pose as `h == 0` whatever the quaternion is:
/// `qa.sub(qb)` is exactly zero when the two are the same bits. So this control
/// pins the all-fallback regime `0060` §5 measured the prototype losing 80% on,
/// and a classifier that read everything as `series` fails it.
///
/// `ScLerp` does not, and **only one of the two shapes below reads as
/// degenerate.** Its predicate is `sin²(θ/2)` of `inv_mul`'s rotation part, i.e.
/// of `conj(q) ⊗ q`, whose vector components are differences that cancel
/// exactly only when the operands line up:
///
/// - `axis`, the recorded wheel edges' shape (`w = z = 0`), gives every vector
///   component as a difference of *identical* products, so `sin²(θ/2)` is
///   exactly `0` and the bracket is degenerate;
/// - `generic`, all four components non-zero, computes `y` as
///   `(w·y + x·z) − w·y − z·x`, where the first sum has already rounded. The
///   residue is ~1e-18, so `sin²(θ/2) ≈ 5e-36` — which is **4.7e254 times**
///   `SCREW_DEGENERATE_SQ` (1e-290). A motionless edge lands in `ScLerp`'s
///   **series region**, at an angle of ~4e-18 rad that is pure rounding.
///
/// That is not a defect: `dualquat`'s threshold was deliberately lowered by
/// ~280 orders of magnitude because the regrouped algebra stays conditioned
/// there, and `screw_pow_is_accurate_down_to_the_degenerate_threshold` sweeps θ
/// to 1e-160 against the reference. It is a fact about the *mix*, and it is why
/// this control has two arms: "the robot is not moving" and "the interpolant
/// takes its degenerate arm" are the same statement under `LerpSlerp` and are
/// not under `ScLerp`.
fn control_stationary(sweeps: &[Sweep]) -> Result<()> {
    // The recorded wheel edges' quaternion, w-first: a pure axis rotation with
    // two zero components. These are the shortest literals that round-trip to
    // the same `f64` as the recording's own 17-digit text.
    let axis = Quat::new(0.0, 0.707_388_269_167_199_8, 0.706_825_181_105_366, 0.0);
    let repeated = |q: Quat| {
        let mut stream = TfStream::default();
        stream
            .dynamic_edges
            .push(("map".to_owned(), "base_link".to_owned()));
        let pose = Iso3::new(q, Vec3::new(0.25, -0.5, 0.125));
        for k in 0..2048i64 {
            stream.samples.push(tf_tree_bench::replay::Sample {
                edge: 0,
                stamp_ns: k * 10_000_000,
                pose,
            });
        }
        stream
    };
    report_stream(
        "CONTROL repeated pose, axis quaternion (expect 100% stationary, both policies)",
        &repeated(axis),
        sweeps,
    )?;
    report_stream(
        "CONTROL repeated pose, generic quaternion (LerpSlerp stationary; ScLerp series)",
        &repeated(fixture::dynamic_pose(0.0, 1_234_567_890).q),
        sweeps,
    )
}

/// Control 3: an edge that jitters by ~1e-7 rad a sample, which is the only
/// thing that reaches `LerpSlerp`'s **LERP fallback** arm.
///
/// Without it the `lerp_fb` column would read 0.0% in every row above — in the
/// recording, in both other controls and in the fixture — and a column that can
/// only ever be zero is decoration rather than a measurement. Under `ScLerp`
/// the same data is series: 1e-7 rad is `sin²(θ/2) ≈ 2.5e-15`, far above
/// `SCREW_DEGENERATE_SQ`.
fn control_jitter(sweeps: &[Sweep]) -> Result<()> {
    let mut stream = TfStream::default();
    stream
        .dynamic_edges
        .push(("map".to_owned(), "base_link".to_owned()));
    for k in 0..2048i64 {
        // A yaw of a few times 1e-8 rad, alternating, so consecutive samples
        // differ by ~1e-7 rad: below `SLERP_LERP_FALLBACK` (1e-6) and above the
        // `h == 0` that a repeated pose gives.
        let yaw = 5e-8 * f64::from(i32::try_from(k % 3).unwrap_or(0));
        stream.samples.push(tf_tree_bench::replay::Sample {
            edge: 0,
            stamp_ns: k * 10_000_000,
            pose: Iso3::new(
                tf_tree::exp_so3(Vec3::new(0.0, 0.0, yaw)),
                Vec3::new(0.25, -0.5, 0.125),
            ),
        });
    }
    report_stream(
        "CONTROL jitter ~1e-7 rad (LerpSlerp lerp_fb; ScLerp series)",
        &stream,
        sweeps,
    )
}

fn main() -> Result<()> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "testdata/tfstream/indoor_atelier.tfstream".to_owned())
        .into();
    let hz: f64 = match std::env::args().nth(2) {
        Some(a) => a.parse().map_err(|e| anyhow!("bad rate {a:?}: {e}"))?,
        None => 100.0,
    };

    let sweeps = [Sweep::Rate(hz), Sweep::Interval, Sweep::OnGrid];

    println!("# 0060 step 0a — bracket class mix");
    println!("#");
    println!("# chunk = {CHUNK} stamps; `near` counts brackets within {BOUNDARY_BAND:e}");
    println!("# relative of a class boundary; `checked` counts stamps verified");
    println!("# bit-identically against `Plan::at`.");

    let stream = TfStream::load(&path)?;
    println!("\n## {}", path.display());
    for p in &stream.provenance {
        println!("#   {p}");
    }
    report_stream(&format!("{}", path.display()), &stream, &sweeps)?;

    println!("\n## controls");
    control_fixture(&sweeps)?;
    control_stationary(&sweeps)?;
    control_jitter(&sweeps)?;
    Ok(())
}
