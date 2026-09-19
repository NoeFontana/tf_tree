//! Which interpolation region a real `/tf` stream actually lands in — step 0a of
//! [`0060`](../../../docs/decisions/0060-the-batch-fold-that-reads-before-it-interpolates.md).
//!
//! ```sh
//! cargo run --release -p tf_tree_bench --example bracket_mix
//! cargo run --release -p tf_tree_bench --example bracket_mix -- <stream> [sweep_hz]
//! ```
//!
//! # Why this exists
//!
//! `0060`'s SoA kernel is for `slerp`'s **series region** and loses elsewhere (`0060` §5: 80% on an
//! all-fallback cell), so its headline is a function of the data. This example classifies the
//! brackets an `at_many` sweep would read and says nothing about speed; it has no engine code.
//!
//! # The five classes
//!
//! They are the arms of [`tf_tree::slerp`] and of `dualquat`'s `screw_parts` / `ScrewParts::pow`:
//!
//! | class | `LerpSlerp` | `ScLerp` |
//! |---|---|---|
//! | **exact hit** | the stamp is a knot: one slot read, no `eval` at all | same |
//! | **stationary** | `h == 0`: the two rotations are bit-identical, `slerp` returns `qa` | `sin²(θ/2) < 1e-290`: `screw_parts` takes its degenerate return |
//! | **LERP fallback** | `θ² < 1e-12`: near-parallel, LERP and renormalise | — (`ScLerp` has no such arm) |
//! | **series** | `1e-12 ≤ θ² ≤ 0.0225`: the polynomial weights — **the kernel's regime** | `1e-290 ≤ sin²(θ/2) ≤ 0.02233…`: same |
//! | **large arc** | `θ² > 0.0225`: `acos` / `sin`, the exact form | `sin²(θ/2) > 0.02233…`: the exact form |
//!
//! The series fraction is set by publish rate against how fast the body turns.
//!
//! # Three sweeps
//!
//! - **`rate`** (the headline): a 100 Hz grid offset 1 ns off the window origin, so nothing lands on
//!   a knot; weights each bracket by its **duration**.
//! - **`interval`**: one query at the midpoint of every sample interval; each bracket counts once.
//! - **`ongrid`**: a query at every recorded stamp; 100% exact hits, the exact-hit ceiling.
//!
//! # Four controls, so that every column can be non-zero
//!
//! `0060` asked for two; two more found something, so every column can be non-zero:
//!
//! | control | `LerpSlerp` | `ScLerp` |
//! |---|---|---|
//! | the synthetic [`fixture`], 50–1000 Hz smooth screw | 100% series | 100% series |
//! | one pose repeated, the recorded wheel edges' quaternion | 100% stationary | 100% stationary |
//! | one pose repeated, four non-zero components | 100% stationary | **100% series** |
//! | ~1e-7 rad of jitter a sample | **100% LERP fallback** | 100% series |
//!
//! The third exists because the stationary control **as specified fails**: a motionless `ScLerp`
//! edge is degenerate only when the quaternion's zero pattern makes `conj(q) ⊗ q` cancel exactly
//! ([`control_stationary`]). The fourth keeps the LERP-fallback column from being always zero.
//!
//! # The bracket search is checked against the engine, per stamp
//!
//! This example mirrors `SampleRing::sample_from` under `ExtrapPolicy::Error` (the bracket is not a
//! public return), so every swept stamp is also put through `Plan::at` and the two must agree
//! **bit-identically**. The count of checked stamps is printed.
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

/// `SLERP_LERP_FALLBACK`, `tf_tree_math::interp`'s private constant, pinned there by a `const` assert.
const SLERP_LERP_FALLBACK: f64 = 1e-6;

/// `THETA_SLERP_SMALL`, pinned the same way (`assert!(THETA_SLERP_SMALL == 0.15)`).
const THETA_SLERP_SMALL: f64 = 0.15;

/// `SIN_HALF_THETA_SMALL_SQ`, `tf_tree_math::dualquat`'s private constant, pinned there by a unit test.
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

/// `θ²` for a quaternion pair, from the chord rather than from `acos`. Deliberately not
/// `tf_tree_math`'s eight-term series, so the classifier does not inherit that series' errors;
/// [`Counts::near_boundary`] counts the brackets where the two routes could disagree.
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

/// `SampleRing::sample_from` under `ExtrapPolicy::Error`, mirrored over the recorded sample list;
/// every caller checks it against `Plan::at`.
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
    // `span_ns` as `sample.rs` spells it: a wrapping `u64` difference.
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
    /// Brackets within [`BOUNDARY_BAND`] of a class boundary, whose bucket could depend on the route
    /// taken to `θ²`; a non-zero count means a footnote.
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
        "  {what:<34} {:>6} {} {:>5} {:>4} {:>7}",
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
            Sweep::Rate(hz) => format!("rate {hz} Hz"),
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
    // `plan(target, source)` is `lookup(target, source)`, so a `T_parent_child` edge returns the sample
    // unchanged from `plan(parent, child)`; the bit-identity check below fails on the wrong direction.
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
                // `s` is in `(0, 1)` mathematically but can round **up** to `1.0` for a multi-day span (2e16 ns
                // queried 1 ns short of its end); it cannot round to `0.0`. The kernel answers both by selecting
                // an endpoint, as for a knot, so they share a bucket.
                let got = got.map_err(|e| anyhow!("engine declined a bracket at {t}: {e}"))?;
                let want = match policy {
                    InterpPolicy::LerpSlerp => LerpSlerp::eval(&a, &b, s),
                    InterpPolicy::ScLerp => ScLerp::eval(&a, &b, s),
                };
                if got.to_bits() != want.to_bits() {
                    bail!("bracket at {t} on {parent}->{child}: engine != eval(a, b, {s})");
                }
                counts.checked += 1;
                if s == 0.0 || s == 1.0 {
                    Class::ExactHit
                } else {
                    let (class, near) = classify(policy, &a, &b);
                    if near < BOUNDARY_BAND {
                        counts.near_boundary += 1;
                    }
                    class
                }
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

/// The publish rate at which each recorded interval would fall inside the series region.
///
/// Both policies reduce to `θ ≤ 0.15 rad` (`LerpSlerp`: `θ² ≤ THETA_SLERP_SMALL²`; `ScLerp`:
/// `sin²(θ) ≤ SIN_HALF_THETA_SMALL_SQ = sin(0.15)²`), so an interval of length `Δt` with endpoints
/// `θ` apart is series for every `f ≥ θ/(0.15·Δt)`: its *series rate*.
///
/// **This is an extrapolation** assuming constant turn rate across the interval, as the interpolant
/// does; publish rate is the variable the fraction is most sensitive to.
struct SeriesRate {
    /// Per interval, the publish rate above which it is series, in Hz; only intervals that rotate.
    hz: Vec<f64>,
    /// Intervals whose endpoints are bit-identical rotations (`θ == 0`).
    motionless: usize,
    /// Intervals that are *large arc at the rate they were published at*, with the longest one's duration.
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
                println!("  {:<34} {}", format!("{p}->{c}"), counts.row());
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
            println!("  {:<34} {}", "ALL EDGES", total.row());
        }
    }
    Ok(())
}

/// Control 1: the synthetic fixture, whose 50–1000 Hz edges must read ~100%
/// series.
fn control_fixture(sweeps: &[Sweep]) -> Result<()> {
    report_stream(
        "CONTROL fixture (expect ~100% series)",
        &fixture_stream(),
        sweeps,
    )
}

/// The fixture's dynamic history, as a [`TfStream`] so it goes through exactly
/// the same path as the recording.
fn fixture_stream() -> TfStream {
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
    stream
}

/// Control 2: one dynamic edge pushed the **same pose** every time, in two quaternion shapes; the
/// two do not agree, which is the point.
///
/// `LerpSlerp` reads a repeated pose as `h == 0` whatever the quaternion; this pins the all-fallback
/// regime `0060` §5 measured the prototype losing 80% on.
///
/// `ScLerp` is degenerate only when `conj(q) ⊗ q`'s vector components cancel exactly:
///
/// - `axis`, the recorded wheel edges' shape (`w = z = 0`), cancels exactly: degenerate;
/// - `generic`, all four components non-zero, keeps rounding (`sin²(θ/2) ≈ 5e-36`, far above
///   `SCREW_DEGENERATE_SQ`), so a motionless edge lands in the **series region**.
///
/// That is not a defect (`screw_pow_is_accurate_down_to_the_degenerate_threshold`); "not moving" and
/// "takes the degenerate arm" differ under `ScLerp` and coincide under `LerpSlerp`.
fn control_stationary(sweeps: &[Sweep]) -> Result<()> {
    // The recorded wheel edges' quaternion, w-first: a pure axis rotation with two zero components.
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

/// Control 3: an edge that jitters by ~1e-7 rad a sample, the only thing that reaches `LerpSlerp`'s
/// **LERP fallback** arm (under `ScLerp` the same data is series).
fn control_jitter(sweeps: &[Sweep]) -> Result<()> {
    let mut stream = TfStream::default();
    stream
        .dynamic_edges
        .push(("map".to_owned(), "base_link".to_owned()));
    for k in 0..2048i64 {
        // A yaw of a few 1e-8 rad, alternating: below `SLERP_LERP_FALLBACK` (1e-6), above a repeated pose's `h == 0`.
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
