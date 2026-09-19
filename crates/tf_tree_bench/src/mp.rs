//! Shared machinery for the multi-process evaluation (`mp_bench`): what each of
//! N consumers at its own rate experiences, and what it costs, rather than
//! `shm_scaling`'s saturated roofline.
//!
//! * **Open loop.** [`RateLoop`] fixes the schedule in advance and measures
//!   against intended start times, so a stall shows as latency instead of
//!   fewer samples (coordinated omission).
//! * **A writer is running**, so the seqlock retry path and `tf2::BufferCore`'s
//!   mutex are exercised.
//! * **Per-consumer distributions** ([`Histogram`]): `docs/PHASE1.md` §11.2
//!   wants p99.9, not the mean.
//! * **CPU per consumer** ([`ProcStats`]): `docs/PHASE2.md` §12.4's claim that
//!   cost is O(1) in the number of consumers.
//! * **PSS, not summed RSS**: RSS counts a shared page once per mapper.

use std::time::{Duration, Instant};

/// Sub-buckets per power of two. 128 gives ~0.8% worst-case quantisation error,
/// which is far below the run-to-run spread of anything measured here.
const SUB_BITS: u32 = 7;
const SUB: u64 = 1 << SUB_BITS;
/// Values below `SUB` get their own bucket, so sub-128 ns resolution is exact.
const BUCKETS: usize = (64 - SUB_BITS as usize) * SUB as usize + SUB as usize;

/// A log-linear latency histogram, in nanoseconds: one array, constant relative
/// error, ~2 ns to record (it runs inside the measured loop). Hand-rolled to
/// avoid a dependency.
#[derive(Clone)]
pub struct Histogram {
    counts: Vec<u32>,
    total: u64,
    max: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    /// An empty histogram.
    #[must_use]
    pub fn new() -> Histogram {
        Histogram {
            counts: vec![0; BUCKETS],
            total: 0,
            max: 0,
        }
    }

    #[inline]
    fn bucket(v: u64) -> usize {
        if v < SUB {
            return v as usize;
        }
        let msb = 63 - v.leading_zeros();
        let shift = msb - SUB_BITS;
        let sub = (v >> shift) & (SUB - 1);
        (shift as usize + 1) * SUB as usize + sub as usize
    }

    /// Lowest value that lands in `bucket` — the value a quantile reports.
    fn bucket_floor(bucket: usize) -> u64 {
        if (bucket as u64) < SUB {
            return bucket as u64;
        }
        let major = bucket / SUB as usize - 1;
        let sub = (bucket % SUB as usize) as u64;
        ((SUB | sub) << major) & !((1u64 << major) - 1)
    }

    /// Record one observation, in nanoseconds.
    #[inline]
    pub fn record(&mut self, ns: u64) {
        self.counts[Self::bucket(ns)] += 1;
        self.total += 1;
        if ns > self.max {
            self.max = ns;
        }
    }

    /// Fold another histogram into this one.
    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += *b;
        }
        self.total += other.total;
        self.max = self.max.max(other.max);
    }

    /// Number of recorded observations.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.total
    }

    /// Largest observation seen (exact, not bucketed).
    #[must_use]
    pub fn max(&self) -> u64 {
        self.max
    }

    /// The value at quantile `q` (0.0..=1.0), in nanoseconds; the bucket floor,
    /// so never rounded upward.
    #[must_use]
    pub fn quantile(&self, q: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let target = (q * self.total as f64).ceil() as u64;
        let target = target.clamp(1, self.total);
        let mut seen = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            seen += u64::from(c);
            if seen >= target {
                return Self::bucket_floor(i);
            }
        }
        self.max
    }

    /// Fraction of observations strictly below `ns`, in `0.0..=1.0`; the inverse
    /// of [`Histogram::quantile`], for bimodal distributions quantiles cannot
    /// describe. A bucket counts whole when its floor is below `ns`, so it can
    /// overstate by under 0.8%. `0.0` for an empty histogram.
    #[must_use]
    pub fn fraction_below(&self, ns: u64) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        let mut seen = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            if Self::bucket_floor(i) >= ns {
                break;
            }
            seen += u64::from(c);
        }
        seen as f64 / self.total as f64
    }

    /// Encode as a compact `bucket:count` line for a child to print.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut s = format!("hist {} {}", self.total, self.max);
        for (i, &c) in self.counts.iter().enumerate() {
            if c != 0 {
                s.push_str(&format!(" {i}:{c}"));
            }
        }
        s
    }

    /// Inverse of [`Histogram::encode`].
    ///
    /// # Errors
    ///
    /// If the line is not a histogram or a field fails to parse.
    pub fn decode(line: &str) -> Result<Histogram, &'static str> {
        let mut it = line.split_whitespace();
        if it.next() != Some("hist") {
            return Err("not a histogram line");
        }
        let mut h = Histogram::new();
        h.total = it.next().ok_or("no total")?.parse().map_err(|_| "total")?;
        h.max = it.next().ok_or("no max")?.parse().map_err(|_| "max")?;
        for field in it {
            let (b, c) = field.split_once(':').ok_or("bad bucket field")?;
            let b: usize = b.parse().map_err(|_| "bucket")?;
            let c: u32 = c.parse().map_err(|_| "count")?;
            *h.counts.get_mut(b).ok_or("bucket out of range")? = c;
        }
        Ok(h)
    }
}

/// A fixed-rate loop that measures against the **intended** schedule
/// (coordinated-omission fix): `next_due()` is computed from the start time and
/// the period, and a loop that has fallen behind returns an already-past
/// deadline without sleeping, so backlog shows up as latency.
pub struct RateLoop {
    start: Instant,
    period: Duration,
    tick: u64,
}

impl RateLoop {
    /// A loop ticking at `hz`, starting now.
    #[must_use]
    pub fn new(hz: f64) -> RateLoop {
        RateLoop {
            start: Instant::now(),
            period: Duration::from_secs_f64(1.0 / hz),
            tick: 0,
        }
    }

    /// Sleep until the next tick is due and return the instant it was *due*;
    /// latency is `Instant::now() - due` after the work completes.
    pub fn next_due(&mut self) -> Instant {
        let due = self.start + self.period * u32::try_from(self.tick).unwrap_or(u32::MAX);
        self.tick += 1;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        due
    }
}

/// Per-process resource counters, read from `/proc/self`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcStats {
    /// User + system CPU time consumed, in nanoseconds.
    pub cpu_ns: u64,
    /// Proportional set size, in KiB: private pages plus each shared page
    /// divided by the number of processes mapping it.
    pub pss_kib: u64,
}

impl ProcStats {
    /// Read this process's counters.
    #[must_use]
    pub fn read() -> ProcStats {
        ProcStats {
            cpu_ns: self_cpu_ns(),
            pss_kib: self_pss_kib(),
        }
    }

    /// Counters accumulated between two reads.
    #[must_use]
    pub fn since(&self, earlier: ProcStats) -> ProcStats {
        ProcStats {
            cpu_ns: self.cpu_ns.saturating_sub(earlier.cpu_ns),
            // PSS is a level, not a counter: report the later reading.
            pss_kib: self.pss_kib,
        }
    }
}

/// CPU time of this process, in nanoseconds.
///
/// Read from `schedstat` (nanoseconds), not `stat` (10 ms USER_HZ ticks): a
/// consumer uses ~4 ms of CPU per window, under one tick, so `stat` would read
/// `0.0`. Summed over `task/*` so threads are counted whole.
///
/// `task/*` lists live threads only, so a joined thread's CPU leaves the sum
/// (`docs/benchmarks/tf2.md`). The two instruments are therefore cross-checked:
/// when `stat` exceeds the task sum by more than two ticks, the coarse `stat`
/// reading is returned. `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` needs
/// `unsafe`, which this crate forbids. Tested by
/// `a_joined_threads_cpu_is_not_lost` and the sub-tick test below.
fn self_cpu_ns() -> u64 {
    // Always read: it is the completeness reference for the cross-check below.
    let stat_ns = stat_cpu_ns();

    if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
        let mut ns = 0u64;
        let mut any = false;
        for t in tasks.flatten() {
            // A thread can exit between readdir and open; skip it rather than
            // abandoning the sum, which would silently under-report.
            if let Some(v) = schedstat_ns(&t.path().join("schedstat")) {
                ns += v;
                any = true;
            }
        }
        // Two ticks of slack: `stat` rounds up to whole ticks.
        if any && stat_ns <= ns.saturating_add(2 * TICK_NS) {
            return ns;
        }
        if any {
            // The task sum lost a thread. `stat` still has it.
            return stat_ns;
        }
    }
    // CONFIG_SCHEDSTATS=n. Fall back to 10 ms ticks, which is worse but is not
    // nothing, rather than reporting zero and looking like an answer.
    stat_ns
}

/// One USER_HZ clock tick in nanoseconds (100 Hz; `sysconf` needs `unsafe`).
const TICK_NS: u64 = 10_000_000;

/// Field 1 of a `schedstat` file: time on cpu, in nanoseconds.
fn schedstat_ns(path: &std::path::Path) -> Option<u64> {
    let s = std::fs::read_to_string(path).ok()?;
    s.split_whitespace().next()?.parse().ok()
}

/// User + system CPU time in nanoseconds, quantized to 10 ms. Fallback only.
/// Parsed after the last `)` because `comm` may contain parentheses
/// (`docs/PHASE2.md` §5.1).
fn stat_cpu_ns() -> u64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0;
    };
    let Some(after) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
        return 0;
    };
    let f: Vec<&str> = after.split_whitespace().collect();
    // After `)`: index 0 = state (field 3), so field 14 is index 11.
    let utime: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
    let stime: u64 = f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    (utime + stime) * 10_000_000
}

/// Proportional set size of this process, in KiB. `pub` because
/// `bin/frozen_workers.rs` reports PHASE5 §12 gate 4 as Pss summed over workers.
pub fn self_pss_kib() -> u64 {
    let Ok(rollup) = std::fs::read_to_string("/proc/self/smaps_rollup") else {
        return 0;
    };
    for line in rollup.lines() {
        if let Some(rest) = line.strip_prefix("Pss:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

// ---- machine-quiet accounting ------------------------------------------
// A row taken while something else was running is a different experiment.

/// System-wide busy fraction from `/proc/stat` over `window`: 0.0 is quiet,
/// 1.0 is every core saturated.
#[must_use]
pub fn busy_fraction(window: Duration) -> f64 {
    let Some((idle0, total0)) = cpu_jiffies() else {
        return 0.0;
    };
    std::thread::sleep(window);
    let Some((idle1, total1)) = cpu_jiffies() else {
        return 0.0;
    };
    let d_total = total1.saturating_sub(total0);
    if d_total == 0 {
        return 0.0;
    }
    let d_idle = idle1.saturating_sub(idle0);
    1.0 - (d_idle as f64 / d_total as f64)
}

/// `(idle, total)` jiffies from the aggregate `cpu` line of `/proc/stat`.
fn cpu_jiffies() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let mut it = line.split_whitespace();
    if it.next()? != "cpu" {
        return None;
    }
    let v: Vec<u64> = it.filter_map(|f| f.parse().ok()).collect();
    // user nice system idle iowait irq softirq steal ...
    let idle = v.get(3).copied()? + v.get(4).copied().unwrap_or(0);
    Some((idle, v.iter().sum()))
}

/// Busy fraction above which a measurement is not worth taking; the harness's
/// own load is a fraction of one core.
pub const QUIET_ENOUGH: f64 = 0.10;

/// Refuse to measure on a busy machine, naming what is running. Overridable
/// with `TF_TREE_BENCH_FORCE=1`.
///
/// # Errors
///
/// A human-readable refusal when the machine is too busy to measure.
pub fn require_quiet_machine() -> Result<f64, String> {
    let busy = busy_fraction(Duration::from_millis(300));
    if busy <= QUIET_ENOUGH || std::env::var_os("TF_TREE_BENCH_FORCE").is_some() {
        return Ok(busy);
    }
    Err(format!(
        "machine is {:.0}% busy before the run even starts (threshold {:.0}%).\n\
         Latency here is largely a measurement of the scheduler, so a number taken\n\
         now would describe the other workload, not this one.\n\
         Top consumers:\n{}\n\
         Wait for the machine to go quiet, or set TF_TREE_BENCH_FORCE=1 if you are\n\
         certain the load is irrelevant.",
        busy * 100.0,
        QUIET_ENOUGH * 100.0,
        top_consumers()
    ))
}

/// The three busiest processes, for the refusal message.
fn top_consumers() -> String {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return "  (unavailable)".into();
    };
    let mut rows: Vec<(u64, String)> = dir
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            name.parse::<u32>().ok()?;
            let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
            let close = stat.rfind(')')?;
            let comm = stat.get(stat.find('(')? + 1..close)?.to_owned();
            let f: Vec<&str> = stat.get(close + 2..)?.split_whitespace().collect();
            let cpu: u64 =
                f.get(11)?.parse().ok().unwrap_or(0) + f.get(12)?.parse().ok().unwrap_or(0);
            Some((cpu, comm))
        })
        .collect();
    rows.sort_unstable_by_key(|(cpu, _)| std::cmp::Reverse(*cpu));
    rows.truncate(3);
    rows.iter()
        .map(|(cpu, comm)| format!("  {comm} ({} s CPU)", cpu / 100))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn histogram_quantiles_are_within_the_bucketing_error() {
        let mut h = Histogram::new();
        for v in 1..=10_000u64 {
            h.record(v);
        }
        assert_eq!(h.count(), 10_000);
        for (q, want) in [(0.5, 5000.0), (0.99, 9900.0), (0.999, 9990.0)] {
            let got = h.quantile(q) as f64;
            let err = (got - want).abs() / want;
            assert!(err < 0.01, "q{q}: got {got}, want ~{want} ({err:.4} rel)");
        }
    }

    #[test]
    fn histogram_never_reports_above_the_truth() {
        // Reporting the bucket *ceiling* would flatter nothing but would still
        // be a number nobody measured; floors keep every reported tail honest.
        let mut h = Histogram::new();
        h.record(1_000_000);
        assert!(h.quantile(0.5) <= 1_000_000);
        assert_eq!(h.max(), 1_000_000);
    }

    /// `fraction_below` sees bimodality (30% of samples at composed speed, 70% a
    /// decade slower).
    ///
    /// Mutant: `>=` → `>` in `fraction_below`; the exact-boundary case then
    /// reports 0.4 where 0.3 is true.
    #[test]
    fn fraction_below_reports_the_fast_mode_a_quantile_hides() {
        let mut h = Histogram::new();
        for _ in 0..300 {
            h.record(800);
        }
        for _ in 0..700 {
            h.record(9_000);
        }
        // The p50 lands in the slow mode and is silent about the other 30%.
        assert!(h.quantile(0.50) > 5_000, "p50 {}", h.quantile(0.50));
        assert!(
            (h.fraction_below(1_000) - 0.30).abs() < 0.001,
            "fraction below 1 us: {}",
            h.fraction_below(1_000)
        );
        // Strictly below: 1000 is a bucket floor here, so the bucket holding
        // the 1000s must not be counted.
        let mut exact = Histogram::new();
        for _ in 0..300 {
            exact.record(800);
        }
        for _ in 0..100 {
            exact.record(1_000);
        }
        for _ in 0..600 {
            exact.record(9_000);
        }
        assert!(
            (exact.fraction_below(1_000) - 0.30).abs() < 0.001,
            "1000 ns is not below 1000 ns: {}",
            exact.fraction_below(1_000)
        );
        assert_eq!(Histogram::new().fraction_below(1_000), 0.0);
    }

    #[test]
    fn histogram_round_trips_through_its_wire_form() {
        let mut h = Histogram::new();
        for v in [1u64, 7, 999, 123_456, 9_999_999] {
            h.record(v);
        }
        let back = Histogram::decode(&h.encode()).unwrap();
        assert_eq!(back.count(), h.count());
        assert_eq!(back.max(), h.max());
        for q in [0.5, 0.9, 0.99, 1.0] {
            assert_eq!(back.quantile(q), h.quantile(q));
        }
    }

    #[test]
    fn merging_is_the_same_as_recording_into_one() {
        let (mut a, mut b, mut both) = (Histogram::new(), Histogram::new(), Histogram::new());
        for v in 1..=500u64 {
            a.record(v);
            both.record(v);
        }
        for v in 501..=1000u64 {
            b.record(v);
            both.record(v);
        }
        a.merge(&b);
        assert_eq!(a.count(), both.count());
        for q in [0.5, 0.99, 0.999] {
            assert_eq!(a.quantile(q), both.quantile(q));
        }
    }

    /// The property the whole harness rests on: a slow tick must show up as
    /// latency, not vanish into a reduced sample count.
    #[test]
    fn the_rate_loop_charges_overrun_to_latency() {
        let mut r = RateLoop::new(1000.0); // 1 ms period
        let first = r.next_due();
        // Simulate a consumer that overruns its budget by ~5 periods.
        std::thread::sleep(Duration::from_millis(5));
        let second = r.next_due();
        assert!(
            second.duration_since(first) < Duration::from_millis(2),
            "the schedule slipped with the consumer — this is coordinated omission"
        );
        let lateness = Instant::now().duration_since(second);
        assert!(
            lateness >= Duration::from_millis(3),
            "an overrun did not register as lateness: {lateness:?}"
        );
    }

    #[test]
    fn proc_stats_are_readable_and_monotone() {
        let a = ProcStats::read();
        let mut x = 0u64;
        for i in 0..3_000_000u64 {
            x = x.wrapping_add(i);
        }
        std::hint::black_box(x);
        let b = ProcStats::read();
        assert!(b.cpu_ns >= a.cpu_ns, "cpu time went backwards");
        assert!(
            b.pss_kib > 0,
            "PSS unreadable — /proc/self/smaps_rollup absent?"
        );
    }

    /// The counter must resolve less than one 10 ms clock tick (a counter that
    /// is always zero satisfies monotonicity; see `self_cpu_ns`).
    #[test]
    fn cpu_time_resolves_below_one_clock_tick() {
        if !std::path::Path::new("/proc/self/schedstat").exists() {
            // CONFIG_SCHEDSTATS=n: the 10 ms fallback is all there is, and
            // asserting sub-tick resolution against it would be a false alarm.
            return;
        }
        let spin = Duration::from_millis(3);
        let a = ProcStats::read();
        let start = Instant::now();
        let mut x = 0u64;
        while start.elapsed() < spin {
            x = x.wrapping_add(std::hint::black_box(1));
        }
        std::hint::black_box(x);
        let d = ProcStats::read().since(a);
        assert!(
            d.cpu_ns >= 1_000_000,
            "3 ms of spinning read as {} ns of CPU — the counter is quantized \
             coarser than the thing it measures",
            d.cpu_ns
        );
        assert!(
            d.cpu_ns < 500_000_000,
            "3 ms of spinning read as {} ns of CPU — implausible, check the units",
            d.cpu_ns
        );
    }

    /// A thread's CPU must not vanish from the reading when the thread is
    /// joined (`self_cpu_ns` cross-check).
    ///
    /// Mutant: delete the `if any { return stat_ns; }` arm; the test then
    /// reports ~0 s for ~0.4 s burned.
    #[test]
    fn cpu_time_survives_a_thread_exiting() {
        if !std::path::Path::new("/proc/self/schedstat").exists() {
            return;
        }
        // 200 ms wall per thread, asserted at 100 ms CPU total: a quota'd host
        // does not give two threads 400 ms, and the mutant reports ~0.34 ms.
        let burn = Duration::from_millis(200);
        let before = ProcStats::read();
        let hs: Vec<_> = (0..2)
            .map(|_| {
                std::thread::spawn(move || {
                    let start = Instant::now();
                    let mut x = 0u64;
                    while start.elapsed() < burn {
                        x = x.wrapping_add(std::hint::black_box(1));
                    }
                    std::hint::black_box(x);
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let d = ProcStats::read().since(before);
        assert!(
            d.cpu_ns >= 100_000_000,
            "two threads burned ~400 ms of CPU and then exited; the reading is \
             {} ns, so their time left the counter with them",
            d.cpu_ns
        );
    }
}
