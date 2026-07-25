//! Does an interpolation-seeded bracket search actually work on real data?
//!
//! [`docs/design/fast-path.md`](../../../docs/design/fast-path.md) §5 proposes
//! replacing the bracket search's `log2(n)` dependent probes with a single
//! interpolated guess plus a fixed, small number of branchless corrections:
//!
//! ```text
//! guess = lo + (t − t_lo)·(hi − lo)/(t_hi − t_lo)
//! ```
//!
//! That is exact for perfectly isochronous stamps and degrades with jitter, so
//! §10 makes it conditional: *"Falsified if real recorded streams are jittery
//! enough that the seeded guess misses often — measure the correction-step
//! distribution on `indoor_atelier.tfstream` before committing."*
//!
//! This is that measurement, and it is deliberately a **pure analysis of stamp
//! sequences**: the guess quality depends only on the stamps and the query, not
//! on any engine internals, so nothing here has to touch the ring or be kept in
//! sync with it.
//!
//! Two seeds are compared, because the cheap one is not obviously the right one:
//!
//! * **global** — interpolate across the whole retained window `[lo, hi]`. One
//!   division, no state. Wrong whenever the *rate* varies across the window.
//! * **local** — interpolate using the mean period of the newest few samples.
//!   Robust to slow rate drift, useless under burstiness.
//!
//! Reported as a distribution of `|guess − true|` in index units, because the
//! mean is the wrong statistic: a seed that is perfect 99% of the time and 400
//! off in the tail is worse than one that is always within 3, and only the
//! quantiles show that.
//!
//! Run: `cargo run --release -p tf_tree_bench --example search_seed`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::path::Path;

use tf_tree_bench::fixture;
use tf_tree_bench::replay::TfStream;

/// Queries drawn per edge.
const QUERIES: usize = 20_000;

/// The last index `i` with `stamps[i] <= t`, by binary search — the answer the
/// seeded search has to reproduce.
fn true_index(stamps: &[i64], t: i64) -> usize {
    let (mut lo, mut hi) = (0usize, stamps.len() - 1);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if stamps[mid] <= t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Linear interpolation across the whole window.
fn seed_global(stamps: &[i64], t: i64) -> usize {
    let (lo, hi) = (0usize, stamps.len() - 1);
    let span = stamps[hi] - stamps[lo];
    if span <= 0 {
        return lo;
    }
    let frac = (t - stamps[lo]) as f64 / span as f64;
    ((frac * (hi - lo) as f64) as usize).min(hi)
}

/// Seed from the mean period of the newest `k` samples, extrapolated backwards
/// from the newest.
fn seed_local(stamps: &[i64], t: i64, k: usize) -> usize {
    let hi = stamps.len() - 1;
    let k = k.min(hi);
    if k == 0 {
        return hi;
    }
    let period = (stamps[hi] - stamps[hi - k]) as f64 / k as f64;
    if period <= 0.0 {
        return hi;
    }
    // `ceil`, not truncation: we want the last index whose stamp is <= t (the
    // *lower* bracket), and truncating toward zero lands one past it.
    let back = ((stamps[hi] - t) as f64 / period).ceil() as i64;
    (hi as i64 - back).clamp(0, hi as i64) as usize
}

/// Quantiles of a distribution of absolute index errors.
struct Dist {
    sorted: Vec<u64>,
}

impl Dist {
    fn new(mut v: Vec<u64>) -> Dist {
        v.sort_unstable();
        Dist { sorted: v }
    }
    fn q(&self, p: f64) -> u64 {
        if self.sorted.is_empty() {
            return 0;
        }
        let i = ((self.sorted.len() - 1) as f64 * p) as usize;
        self.sorted[i]
    }
    /// Fraction of queries within `d` index units — i.e. solvable by `d`
    /// correction steps.
    fn within(&self, d: u64) -> f64 {
        let c = self.sorted.partition_point(|&x| x <= d);
        c as f64 / self.sorted.len() as f64
    }
}

/// Deterministic stamps spanning `[stamps[0], stamps[last]]`.
fn queries(stamps: &[i64]) -> Vec<i64> {
    let (lo, hi) = (stamps[0], stamps[stamps.len() - 1]);
    (0..QUERIES as i64)
        .map(|k| lo + (hi - lo) * k / QUERIES as i64)
        .collect()
}

fn report(label: &str, stamps: &[i64]) {
    if stamps.len() < 8 {
        println!("{label:>34}  (only {} samples, skipped)", stamps.len());
        return;
    }
    let qs = queries(stamps);
    let mut g = Vec::with_capacity(qs.len());
    let mut l = Vec::with_capacity(qs.len());
    for &t in &qs {
        let truth = true_index(stamps, t) as i64;
        g.push((seed_global(stamps, t) as i64 - truth).unsigned_abs());
        l.push((seed_local(stamps, t, 8) as i64 - truth).unsigned_abs());
    }
    let (g, l) = (Dist::new(g), Dist::new(l));

    // Jitter: coefficient of variation of the inter-sample period.
    let d: Vec<f64> = stamps.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
    let mean = d.iter().sum::<f64>() / d.len() as f64;
    let var = d.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / d.len() as f64;
    let cv = if mean > 0.0 { var.sqrt() / mean } else { 0.0 };

    println!(
        "{label:>34} {:>7} {:>7.2} | {:>5} {:>5} {:>6} {:>7.1}% | {:>5} {:>5} {:>6} {:>7.1}%",
        stamps.len(),
        cv,
        g.q(0.50),
        g.q(0.99),
        g.q(1.0),
        g.within(2) * 100.0,
        l.q(0.50),
        l.q(0.99),
        l.q(1.0),
        l.within(2) * 100.0,
    );
}

fn main() {
    println!("interpolation-seeded bracket search: |guess - true| in index units");
    println!("==================================================================");
    println!("Falsification test for docs/design/fast-path.md §5 (see §10).\n");
    println!(
        "{:>34} {:>7} {:>7} | {:>5} {:>5} {:>6} {:>8} | {:>5} {:>5} {:>6} {:>8}",
        "edge", "samples", "jitter", "p50", "p99", "max", "<=2", "p50", "p99", "max", "<=2"
    );
    println!(
        "{:>34} {:>7} {:>7} | {:^25} | {:^25}",
        "", "", "CV", "global seed", "local seed (k=8)"
    );

    // --- synthetic fixture: perfectly isochronous, the best case -------------
    println!("\n-- synthetic fixture (isochronous by construction) --");
    for &(parent, child, hz) in fixture::DYNAMIC_EDGES {
        let period = (1e9 / hz) as i64;
        let count = (fixture::HISTORY_SECS * hz) as usize;
        let stamps: Vec<i64> = (0..count as i64).map(|k| k * period).collect();
        report(&format!("{parent}->{child} @{hz:.0}Hz"), &stamps);
    }

    // --- the real recording: the case that decides it ------------------------
    println!("\n-- recorded /tf stream (indoor_atelier.tfstream) --");
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/tfstream/indoor_atelier.tfstream");
    let stream = TfStream::load(&path).expect("load recorded stream");
    for (e, (parent, child)) in stream.dynamic_edges.iter().enumerate() {
        let stamps: Vec<i64> = stream
            .samples
            .iter()
            .filter(|s| s.edge == e)
            .map(|s| s.stamp_ns)
            .collect();
        report(&format!("{parent}->{child}"), &stamps);
    }
}
