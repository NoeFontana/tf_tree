//! Which interpolation region a real `/tf` stream actually lands in — step 0a of
//! [`0060`](../../../docs/decisions/0060-the-batch-fold-that-reads-before-it-interpolates.md).
//!
//! ```sh
//! cargo run --release -p tf_tree_bench --example bracket_mix
//! cargo run --release -p tf_tree_bench --example bracket_mix -- <stream> [sweep_hz]
//! ```
//!
//! `0060`'s SoA kernel wins only in `slerp`'s series region, so its headline depends on the data; this classifies
//! the brackets an `at_many` sweep would read and measures no speed.
//!
//! # The five classes
//!
//! | class | `LerpSlerp` | `ScLerp` |
//! |---|---|---|
//! | **exact hit** | the stamp is a knot: one slot read, no `eval` at all | same |
//! | **stationary** | `h == 0`: the two rotations are bit-identical, `slerp` returns `qa` | `sin²(θ/2) < 1e-290`: `screw_parts` takes its degenerate return |
//! | **LERP fallback** | `θ² < 1e-12`: near-parallel, LERP and renormalise | — (`ScLerp` has no such arm) |
//! | **series** | `1e-12 ≤ θ² ≤ 0.0225`: the polynomial weights — **the kernel's regime** | `1e-290 ≤ sin²(θ/2) ≤ 0.02233…`: same |
//! | **large arc** | `θ² > 0.0225`: `acos` / `sin`, the exact form | `sin²(θ/2) > 0.02233…`: the exact form |
//!
//! # Three sweeps
//!
//! `rate` (the headline): a 100 Hz grid offset 1 ns off the window origin, each bracket weighted by duration;
//! `interval`: one query at each interval's midpoint; `ongrid`: a query at every recorded stamp (all exact hits).
//!
//! # Four controls, so that every column can be non-zero
//!
//! | control | `LerpSlerp` | `ScLerp` |
//! |---|---|---|
//! | the synthetic [`fixture`], 50–1000 Hz smooth screw | 100% series | 100% series |
//! | one pose repeated, the recorded wheel edges' quaternion | 100% stationary | 100% stationary |
//! | one pose repeated, four non-zero components | 100% stationary | **100% series** |
//! | ~1e-7 rad of jitter a sample | **100% LERP fallback** | 100% series |
#![allow(clippy::print_stdout)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};

use tf_tree::{
    Interp, InterpPolicy, Iso3, LerpSlerp, Quat, ScLerp, Stamp, SystemDomain, Tree, Vec3,
};
use tf_tree_bench::{fixture, replay::TfStream};

const CHUNK: usize = 64;

/// `tf_tree_math::interp`'s private `SLERP_LERP_FALLBACK`.
const SLERP_LERP_FALLBACK: f64 = 1e-6;

const THETA_SLERP_SMALL: f64 = 0.15;

/// `tf_tree_math::dualquat`'s private `SIN_HALF_THETA_SMALL_SQ`.
const SIN_HALF_THETA_SMALL_SQ: f64 = 0.022_331_755_437_196_99;

const SCREW_DEGENERATE_SQ: f64 = 1e-290;

const BOUNDARY_BAND: f64 = 1e-9;

/// Which arm of the interpolant a bracket lands in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Class {
    Series,
    Stationary,
    LerpFallback,
    LargeArc,
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

/// `θ²` from the chord rather than `acos`, so the classifier does not inherit `tf_tree_math`'s series errors.
fn theta_sq_from_chord(h: f64) -> f64 {
    let theta = 2.0 * (h * 0.5).sqrt().min(1.0).asin();
    theta * theta
}

/// How far a bracket sits from the nearest class boundary, relative; `f64::INFINITY` if not near one.
fn boundary_distance(x: f64, edges: &[f64]) -> f64 {
    edges
        .iter()
        .map(|e| ((x - e) / e).abs())
        .fold(f64::INFINITY, f64::min)
}

/// Classify one bracket: its class and its distance from the nearest boundary.
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
    Declined,
    Hit(Iso3),
    Bracket(Iso3, Iso3, f64),
}

/// `SampleRing::sample_from` under `ExtrapPolicy::Error`, mirrored over the recorded samples; checked against `Plan::at`.
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
    let span = |from: i64, to: i64| (to as u64).wrapping_sub(from as u64) as f64;
    let s = span(t_i, t) / span(t_i, t_j);
    Read::Bracket(samples[lo].1, samples[lo + 1].1, s)
}

/// A class tally, plus the two things that say whether it can be believed.
#[derive(Default, Clone)]
struct Counts {
    per_class: BTreeMap<Class, usize>,
    declined: usize,
    near_boundary: usize,
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

/// The per-chunk view a bail-out would see: the fraction of each 64-stamp chunk the classifier admits.
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

#[derive(Clone, Copy)]
enum Sweep {
    Rate(f64),
    Interval,
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
                // `s` can round **up** to `1.0` (not to `0.0`); the kernel picks an endpoint for both, as for a knot.
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

/// The publish rate at which each recorded interval would fall in the series region: `f ≥ θ/(0.15·Δt)`.
///
/// An extrapolation assuming constant turn rate across the interval.
struct SeriesRate {
    hz: Vec<f64>,
    motionless: usize,
    /// Intervals that are *large arc at the rate they were published at*, with the longest one's duration.
    large_arc: usize,
    longest_large_arc_s: f64,
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

/// Control 1: the synthetic fixture, whose 50–1000 Hz edges must read ~100% series.
fn control_fixture(sweeps: &[Sweep]) -> Result<()> {
    report_stream(
        "CONTROL fixture (expect ~100% series)",
        &fixture_stream(),
        sweeps,
    )
}

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

/// Control 2: one dynamic edge pushed the same pose every time, in two quaternion shapes.
///
/// `LerpSlerp` reads it as `h == 0` whatever the quaternion (`0060` §5's all-fallback regime). `ScLerp` is
/// degenerate only when `conj(q) ⊗ q` cancels exactly:
///
/// - `axis`, the recorded wheel edges' shape (`w = z = 0`): degenerate;
/// - `generic`, all four components non-zero: rounding keeps it in the **series region**
///   (`screw_pow_is_accurate_down_to_the_degenerate_threshold`).
fn control_stationary(sweeps: &[Sweep]) -> Result<()> {
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
