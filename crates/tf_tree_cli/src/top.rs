//! `tf_tree top` — `docs/PHASE5.md` §7's live view of a running arena.
//!
//! No `ratatui`: plain ANSI escapes and a redraw loop (`ESC[H`, `ESC[K`,
//! `ESC[J`), no new dependency (`CLAUDE.md` budget). The cost: no raw mode, so
//! no key handling (raw mode means `libc` and an `unsafe` boundary this crate
//! forbids) and the detail view is chosen with `--edge <id|name>`; interactive
//! selection would be a decision record. The alternate screen is not used
//! either, since restoring it on `SIGINT` needs a signal handler.
//!
//! It observes without perturbing:
//!
//! * It attaches read-only (D18) and refuses `--rw`.
//! * A read-only attachment writes no participant record
//!   (`Tree::participant_slot` is `u32::MAX`), so `TFT015`'s table is not
//!   inflated; it does take a lock-file byte, visible to `tf_tree participants`.
//! * It performs no lookups; `tests::capturing_the_arena_moves_no_counter`
//!   requires every counter to stand still.
//!
//! Unbuilt: the clock-drift rule `TFT004` cannot have (`docs/PHASE5.md` §6). A
//! recorded `ClaimRecord::clock_offset_nanos`
//! ([`0036`](../../../docs/decisions/0036-the-receipt-time-the-format-already-reserved.md))
//! is clock error plus stamp-to-push latency, and one sample cannot separate
//! them; drift can, and needs a per-publisher series, which only this view
//! collects. Sampling is per edge, so de-duplicate against the value, not the
//! poll, and do not add a column that flickers.
//!
//! Everything this view did not author goes through `sanitize` before reaching
//! an ANSI frame.
//!
//! Rates are observed, never a deviation from `nominal_rate_mhz` (that is
//! `doctor`'s `TFT007`; a flickering column teaches operators to ignore it):
//!
//! * `rate(Hz)` is the median inter-arrival of the retained stamps, in the
//!   publisher's stamp domain.
//! * `Δ/s` is the head advance between ticks over this observer's elapsed wall
//!   time. They disagree when a publisher back-dates or replays.

use std::collections::{BTreeMap, VecDeque};
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::Result;

use tf_tree::unstable::EdgeKind;
use tf_tree::{EdgeId, Tree};

use crate::catalogue::{Severity, Tft};
// Import `OCCUPANCY_LIMIT` rather than restating `0.80`: one constant, one comparator (`>`).
use crate::checks::{Clock, OCCUPANCY_LIMIT};
use crate::doctor::Snapshot;

/// The counter values read from one [`tf_tree_core::counters::EdgeCounters`] or
/// [`tf_tree_core::counters::ParticipantCounters`].
///
/// A plain-data copy, not a borrow: differencing live atomics never reconciles
/// with the printed totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CounterSample {
    /// Successful lookups — the denominator (§5.4).
    pub lookups_ok: u64,
    /// Requests older than the retained window.
    pub extrap_before: u64,
    /// Requests newer than the newest sample.
    pub extrap_after: u64,
    /// Requests against an edge with no samples.
    pub no_data: u64,
    /// The ring lapped a reader mid-read.
    pub recycled: u64,
    /// A slot stayed mid-write past the retry limit.
    pub contended: u64,
    /// When the most recent failure happened, arena-domain nanoseconds.
    pub last_err_nanos: i64,
    /// High-water mark of distance past either end of the window.
    pub worst_extrap_gap_ns: i64,
}

impl CounterSample {
    /// Every failure counter summed.
    #[must_use]
    pub fn errors(&self) -> u64 {
        self.extrap_before
            .saturating_add(self.extrap_after)
            .saturating_add(self.no_data)
            .saturating_add(self.recycled)
            .saturating_add(self.contended)
    }

    /// `self - prev`, saturating (a counter can appear to go backwards after a reap).
    #[must_use]
    pub fn since(&self, prev: &CounterSample) -> CounterSample {
        CounterSample {
            lookups_ok: self.lookups_ok.saturating_sub(prev.lookups_ok),
            extrap_before: self.extrap_before.saturating_sub(prev.extrap_before),
            extrap_after: self.extrap_after.saturating_sub(prev.extrap_after),
            no_data: self.no_data.saturating_sub(prev.no_data),
            recycled: self.recycled.saturating_sub(prev.recycled),
            contended: self.contended.saturating_sub(prev.contended),
            // A timestamp and a high-water mark: not differences.
            last_err_nanos: self.last_err_nanos,
            worst_extrap_gap_ns: self.worst_extrap_gap_ns,
        }
    }
}

/// One edge as of one tick.
#[derive(Clone, Debug)]
pub struct EdgeSample {
    /// Edge id.
    pub id: u32,
    /// `parent->child (edge#n)`.
    pub label: String,
    /// Static / dynamic / tombstone.
    pub kind: EdgeKind,
    /// Ring capacity (`0` for a static edge).
    pub capacity: u32,
    /// Total samples ever published (monotone).
    pub head: u64,
    /// Whether a writer currently holds the claim.
    pub claimed: bool,
    /// The claim owner's pid (`0` when unclaimed or unresolvable).
    pub owner_pid: u32,
    /// The first retained stamp in push order, not `min(stamps)`: a `min` would
    /// repair an out-of-order publisher ([`IntervalStats::non_monotonic`] shows it).
    pub oldest_stamp: Option<i64>,
    /// Newest stamp the ring holds — the last in push order.
    pub newest_stamp: Option<i64>,
    /// How many stamps the ring retains; `intervals.len() + 1` reads `0` for a
    /// ring holding one sample.
    pub retained: usize,
    /// Successive differences of the retained stamps, in push order.
    pub intervals: Vec<i64>,
    /// This edge's counters.
    pub counters: CounterSample,
}

impl EdgeSample {
    /// Ring occupancy (`min(head, capacity)`).
    #[must_use]
    pub fn occupancy(&self) -> u64 {
        if self.capacity == 0 {
            0
        } else {
            self.head.min(u64::from(self.capacity))
        }
    }
}

/// One participant as of one tick.
#[derive(Clone, Debug)]
pub struct ParticipantSample {
    /// Slot index — the one integer that indexes both the arena table and the
    /// lock file (`docs/PHASE2.md` §3.7).
    pub slot: u32,
    /// Process id (`0` when only a lock byte is held and no record exists yet).
    pub pid: u32,
    /// `"ro"`/`"rw"` from the lock file, `None` when there is no lock file to
    /// read (an in-process arena, or a build without `shm`).
    pub mode: Option<&'static str>,
    /// `comm` from the lock-file identity record.
    pub comm: String,
    /// Whether the arena record says `LIVE` **and** the kernel still holds the
    /// byte.
    pub alive: bool,
    /// Whether the participant table has a record for this slot; `false` for a
    /// read-only participant (`PROT_READ`, D18), like `top` itself.
    pub in_arena: bool,
    /// Attach time in arena-domain nanoseconds (`0` when unknown).
    pub attached_at_nanos: i64,
    /// This participant's counters.
    pub counters: CounterSample,
    /// The edge it most recently failed on, or `u32::MAX`.
    pub last_err_edge: u32,
}

/// Everything one tick read out of the arena.
#[derive(Clone, Debug)]
pub struct Capture {
    /// Where this came from, for the banner ("live arena" / "in-process fixture").
    pub source: &'static str,
    /// Arena size in bytes.
    pub arena_bytes: u64,
    /// Table occupancies as `(what, used, capacity)`, from
    /// [`crate::checks::occupancy_of`].
    pub occupancy: Vec<(&'static str, u32, u32)>,
    /// Frame count.
    pub frames: usize,
    /// Every edge, id order.
    pub edges: Vec<EdgeSample>,
    /// Every participant with a record or a held lock byte, slot order.
    pub participants: Vec<ParticipantSample>,
    /// This observer's own arena slot, `None` for a read-only attachment (which
    /// writes no record) and for a non-shared tree.
    pub self_slot: Option<u32>,
    /// Whether this is somebody else's shared arena, the only case where the
    /// perturbation disclosure applies.
    pub shared: bool,
    /// Whether the engine was built with §5's `counters` feature.
    pub counters_compiled_in: bool,
    /// The reference clock every age is measured against, or `None` when no ring
    /// holds a stamp ([`Capture::decide_clock`]).
    pub clock: Option<Clock>,
}

impl Capture {
    /// Read the whole arena once, read-only. A smear, not an instant: `head` is
    /// used only for tick-to-tick differences, never to index the stamp array.
    #[must_use]
    pub fn from_tree(tree: &Tree, source: &'static str) -> Capture {
        use core::sync::atomic::Ordering;

        let snap = Snapshot::capture(tree);
        let view = tree.arena_view();

        let mut edges = Vec::with_capacity(snap.edges.len());
        for e in &snap.edges {
            let eid = EdgeId(e.id);
            let mut oldest = None;
            let mut intervals = Vec::new();
            let mut retained_count = 0usize;
            if let Some(ring) = view.ring(eid) {
                let head = ring.head.load(Ordering::Acquire);
                // `retained()` excludes the slot being overwritten.
                let retained = ring.retained().min(head);
                retained_count = usize::try_from(retained).unwrap_or(usize::MAX);
                let mut prev: Option<i64> = None;
                for i in (head - retained)..head {
                    let s = ring.stamps[(i & ring.mask()) as usize].load(Ordering::Relaxed);
                    if oldest.is_none() {
                        oldest = Some(s);
                    }
                    if let Some(p) = prev {
                        intervals.push(s.saturating_sub(p));
                    }
                    prev = Some(s);
                }
            }
            let counters = view
                .edge_counters(eid)
                .map_or_else(CounterSample::default, |c| CounterSample {
                    lookups_ok: c.lookups_ok.load(Ordering::Relaxed),
                    extrap_before: c.err_extrap_before.load(Ordering::Relaxed),
                    extrap_after: c.err_extrap_after.load(Ordering::Relaxed),
                    no_data: c.err_no_data.load(Ordering::Relaxed),
                    recycled: c.err_slot_recycled.load(Ordering::Relaxed),
                    contended: c.err_slot_contended.load(Ordering::Relaxed),
                    last_err_nanos: c.last_err_nanos.load(Ordering::Relaxed),
                    worst_extrap_gap_ns: c.worst_extrap_gap_ns.load(Ordering::Relaxed),
                });
            edges.push(EdgeSample {
                id: e.id,
                label: snap.edge_label(e),
                kind: e.kind,
                capacity: e.capacity,
                head: e.head,
                claimed: e.claimed,
                owner_pid: e.owner_pid,
                oldest_stamp: oldest,
                newest_stamp: e.newest_stamp,
                retained: retained_count,
                intervals,
                counters,
            });
        }

        let participants = view.participants();
        let mut rows = Vec::new();
        for slot in 0..participants.capacity() as u32 {
            let Some((pid, _start, _inc)) = participants.identity(slot) else {
                continue;
            };
            let attached_at_nanos = participants
                .get(slot)
                .map_or(0, |r| r.attached_at_nanos.load(Ordering::Relaxed));
            let (counters, last_err_edge) = view.participant_counters(slot).map_or_else(
                || (CounterSample::default(), u32::MAX),
                |c| {
                    (
                        CounterSample {
                            lookups_ok: c.lookups_ok.load(Ordering::Relaxed),
                            extrap_before: c.err_extrap_before.load(Ordering::Relaxed),
                            extrap_after: c.err_extrap_after.load(Ordering::Relaxed),
                            no_data: c.err_no_data.load(Ordering::Relaxed),
                            recycled: c.err_slot_recycled.load(Ordering::Relaxed),
                            contended: c.err_slot_contended.load(Ordering::Relaxed),
                            last_err_nanos: c.last_err_nanos.load(Ordering::Relaxed),
                            worst_extrap_gap_ns: 0,
                        },
                        c.last_err_edge.load(Ordering::Relaxed),
                    )
                },
            );
            rows.push(ParticipantSample {
                slot,
                pid,
                mode: None,
                comm: String::new(),
                alive: tree.participant_alive(slot),
                in_arena: true,
                attached_at_nanos,
                counters,
                last_err_edge,
            });
        }

        let self_slot = match tree.participant_slot() {
            u32::MAX => None,
            s => Some(s),
        };

        let clock = Capture::decide_clock(&edges, crate::unix_nanos_now());

        Capture {
            source,
            arena_bytes: tree.arena_size_bytes() as u64,
            occupancy: crate::checks::occupancy_of(tree),
            frames: snap.frames.len(),
            edges,
            participants: rows,
            self_slot,
            shared: tree.is_shared(),
            counters_compiled_in: tf_tree::counters_compiled_in(),
            clock,
        }
    }

    /// The reference instant for every age, by [`Clock::decide`] and not
    /// `newest_stamp.max()`, which one units-overshooting publisher would capture
    /// (`checks::a_single_units_error_cannot_capture_the_reference_clock`).
    /// `None` only when no ring holds a stamp.
    #[must_use]
    pub fn decide_clock(edges: &[EdgeSample], system_unix_nanos: i64) -> Option<Clock> {
        let stamps: Vec<i64> = edges.iter().filter_map(|e| e.newest_stamp).collect();
        if stamps.is_empty() {
            return None;
        }
        Some(Clock::decide(&stamps, system_unix_nanos))
    }

    /// The reference instant, or `None` when no ring holds a stamp.
    #[must_use]
    pub fn arena_now(&self) -> Option<i64> {
        self.clock.map(Clock::nanos)
    }

    /// Merge lock-file facts (mode, `comm`, held-ness) into the participant rows,
    /// adding rows for read-only participants (a lock byte, no arena record).
    pub fn merge_lock_rows(&mut self, lock_rows: &[(u32, u32, &'static str, String, bool)]) {
        for (slot, pid, mode, comm, held) in lock_rows {
            match self.participants.iter_mut().find(|p| p.slot == *slot) {
                Some(existing) => {
                    existing.mode = Some(mode);
                    existing.comm.clone_from(comm);
                    // The kernel's answer wins: a record with a released byte is a leaked slot.
                    existing.alive = *held;
                }
                None => self.participants.push(ParticipantSample {
                    slot: *slot,
                    pid: *pid,
                    mode: Some(mode),
                    comm: comm.clone(),
                    alive: *held,
                    in_arena: false,
                    attached_at_nanos: 0,
                    counters: CounterSample::default(),
                    last_err_edge: u32::MAX,
                }),
            }
        }
        self.participants.sort_by_key(|p| p.slot);
    }
}

/// Order statistics over a set of inter-arrival intervals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntervalStats {
    /// How many intervals (one fewer than the retained stamps).
    pub n: usize,
    /// Smallest interval.
    pub min_ns: i64,
    /// Median interval — what the rate is derived from.
    pub median_ns: i64,
    /// p99 interval, which is what `TFT008`'s jitter question asks about.
    pub p99_ns: i64,
    /// Largest interval, which is what `TFT009`'s dropout question asks about.
    pub max_ns: i64,
    /// How many intervals were not strictly positive — a stamp that did not
    /// advance, or went backwards, between two consecutive pushes.
    pub non_monotonic: usize,
}

impl IntervalStats {
    /// The observed rate in Hz from the median (not the mean, which one dropout
    /// moves); `None` when the median is not positive.
    #[must_use]
    pub fn rate_hz(&self) -> Option<f64> {
        if self.median_ns > 0 {
            Some(1e9 / self.median_ns as f64)
        } else {
            None
        }
    }
}

/// Order statistics over `intervals`, or `None` if there are none.
///
/// Sorts a copy; the histogram needs push order.
#[must_use]
pub fn interval_stats(intervals: &[i64]) -> Option<IntervalStats> {
    if intervals.is_empty() {
        return None;
    }
    let mut sorted = intervals.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    // `(n - 1) * 99 / 100`: `n * 99 / 100` is the maximum at every multiple of 100.
    let p99 = sorted[(n - 1) * 99 / 100];
    Some(IntervalStats {
        n,
        min_ns: sorted[0],
        median_ns: sorted[n / 2],
        p99_ns: p99,
        max_ns: sorted[n - 1],
        non_monotonic: intervals.iter().filter(|v| **v <= 0).count(),
    })
}

/// One bar of an inter-arrival histogram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bucket {
    /// Inclusive lower edge, nanoseconds.
    pub lo_ns: i64,
    /// Exclusive upper edge (inclusive for the last bucket), nanoseconds.
    pub hi_ns: i64,
    /// How many intervals fell in it.
    pub count: usize,
}

/// A linear histogram of `intervals` over `buckets` bins spanning min..=max.
///
/// Linear: a lone dropout is one lonely bar. One bucket when all intervals are equal.
#[must_use]
pub fn histogram(intervals: &[i64], buckets: usize) -> Vec<Bucket> {
    if intervals.is_empty() || buckets == 0 {
        return Vec::new();
    }
    let min = intervals.iter().copied().min().unwrap_or(0);
    let max = intervals.iter().copied().max().unwrap_or(0);
    let span = max.saturating_sub(min);
    if span == 0 {
        return vec![Bucket {
            lo_ns: min,
            hi_ns: max,
            count: intervals.len(),
        }];
    }
    let n = buckets as i64;
    let mut counts = vec![0usize; buckets];
    for v in intervals {
        // `.min(buckets - 1)` is load-bearing: `v == max` gives `n`, past the last bucket.
        let idx = ((v.saturating_sub(min)) as i128 * n as i128 / span as i128) as usize;
        counts[idx.min(buckets - 1)] += 1;
    }
    // `i128` edges: `span * i` overflows `i64` for a wall-clock stamp in a boot-relative ring.
    let span128 = i128::from(span);
    let clamp = |v: i128| i64::try_from(v).unwrap_or(i64::MAX);
    (0..buckets)
        .map(|i| Bucket {
            lo_ns: clamp(i128::from(min) + span128 * i as i128 / i128::from(n)),
            hi_ns: clamp(i128::from(min) + span128 * (i as i128 + 1) / i128::from(n)),
            count: counts[i],
        })
        .collect()
}

/// Pick the edge a `--edge <needle>` detail view is about.
///
/// An exact id first, then the first label containing `needle`.
#[must_use]
pub fn select_edge<'a>(edges: &'a [EdgeSample], needle: &str) -> Option<&'a EdgeSample> {
    select_edge_index(edges, needle).map(|i| &edges[i])
}

/// [`select_edge`] as a position, to index the parallel `Vec<EdgeRow>`.
#[must_use]
pub fn select_edge_index(edges: &[EdgeSample], needle: &str) -> Option<usize> {
    if let Ok(id) = needle.parse::<u32>() {
        if let Some(i) = edges.iter().position(|e| e.id == id) {
            return Some(i);
        }
    }
    edges.iter().position(|e| e.label.contains(needle))
}

/// One line of the rolling diagnostics feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedEvent {
    /// The tick it was raised on.
    pub tick: u64,
    /// How serious it is, in the catalogue's vocabulary.
    pub severity: Severity,
    /// The catalogue id this corresponds to, where one does.
    ///
    /// `None` for events that are not `TFT` findings (a claim changing hands).
    pub id: Option<Tft>,
    /// What it is about (an edge label, usually).
    pub subject: String,
    /// The finding.
    pub message: String,
}

/// Per-edge state carried between ticks.
#[derive(Clone, Debug)]
struct PrevEdge {
    head: u64,
    claimed: bool,
    counters: CounterSample,
    last_advance_tick: u64,
    ever_advanced: bool,
    silence_reported: bool,
}

/// How many ticks an edge that *was* publishing must stay still before the feed
/// calls it silent.
///
/// Three, not one: a slow publisher can straddle a tick boundary.
const SILENCE_TICKS: u64 = 3;

/// Turns a stream of [`Capture`]s into per-tick rows and feed events.
///
/// The view's only mutable state; testable with hand-built captures.
#[derive(Debug, Default)]
pub struct Sampler {
    tick: u64,
    prev: BTreeMap<u32, PrevEdge>,
    feed: VecDeque<FeedEvent>,
}

/// One edge's row in the rendered table.
///
/// Positional: `rows[i]` describes `capture.edges[i]`.
#[derive(Clone, Debug)]
pub struct EdgeRow {
    /// Order statistics over the retained intervals.
    pub stats: Option<IntervalStats>,
    /// Samples published since the previous tick.
    pub delta_head: u64,
    /// `delta_head` over the observer's own elapsed wall time — the rate that
    /// does not depend on the publisher's clock epoch.
    pub observed_hz: Option<f64>,
    /// Age of the newest stamp against [`Capture::arena_now`], nanoseconds.
    pub age_ns: Option<i64>,
    /// Failures since the previous tick.
    pub delta_errors: u64,
}

/// One rendered tick.
#[derive(Debug)]
pub struct Tick {
    /// Tick number, starting at 1.
    pub tick: u64,
    /// Wall time since the previous tick.
    pub elapsed: Duration,
    /// The capture this tick was built from.
    pub capture: Capture,
    /// One row per edge, id order.
    pub rows: Vec<EdgeRow>,
    /// The whole retained feed, oldest first.
    ///
    /// The whole retained feed (bounded by `FEED_CAPACITY`), not just the visible tail.
    pub feed: Vec<FeedEvent>,
}

/// How many feed events are retained.
const FEED_CAPACITY: usize = 256;

impl Sampler {
    /// A sampler with no history — the first [`Sampler::observe`] produces rows
    /// with no deltas and no events.
    #[must_use]
    pub fn new() -> Sampler {
        Sampler::default()
    }

    /// Fold one capture in, producing this tick's rows and appending to the feed.
    ///
    /// `elapsed` is the observer's wall time since the previous capture; the
    /// first tick should pass whatever it likes, because `delta_head` is zero
    /// there and no rate is derived from it.
    #[must_use]
    pub fn observe(&mut self, capture: Capture, elapsed: Duration) -> Tick {
        self.tick += 1;
        let tick = self.tick;
        let secs = elapsed.as_secs_f64();
        let mut rows = Vec::with_capacity(capture.edges.len());
        let now = capture.arena_now();

        for e in &capture.edges {
            let prev = self.prev.get(&e.id).cloned();
            let delta_head = prev.as_ref().map_or(0, |p| e.head.saturating_sub(p.head));
            let delta_counters = prev
                .as_ref()
                .map(|p| e.counters.since(&p.counters))
                .unwrap_or_default();

            if let Some(p) = &prev {
                self.emit_edge_events(tick, e, p, delta_counters);
            }

            let advanced = delta_head > 0;
            let last_advance_tick = match &prev {
                Some(p) if !advanced => p.last_advance_tick,
                _ => tick,
            };
            let ever_advanced = advanced || prev.as_ref().is_some_and(|p| p.ever_advanced);
            let mut silence_reported = prev.as_ref().is_some_and(|p| p.silence_reported);
            if advanced {
                silence_reported = false;
            } else if ever_advanced
                && !silence_reported
                && tick.saturating_sub(last_advance_tick) >= SILENCE_TICKS
            {
                self.push_event(FeedEvent {
                    tick,
                    severity: Severity::Warn,
                    id: Some(Tft::Tft009),
                    subject: e.label.clone(),
                    message: format!(
                        "no new samples for {} ticks (it was publishing earlier)",
                        tick - last_advance_tick
                    ),
                });
                silence_reported = true;
            }

            self.prev.insert(
                e.id,
                PrevEdge {
                    head: e.head,
                    claimed: e.claimed,
                    counters: e.counters,
                    last_advance_tick,
                    ever_advanced,
                    silence_reported,
                },
            );

            rows.push(EdgeRow {
                stats: interval_stats(&e.intervals),
                delta_head,
                observed_hz: if prev.is_some() && secs > 0.0 {
                    Some(delta_head as f64 / secs)
                } else {
                    None
                },
                // Saturating: the stamp and reference can be in different domains.
                age_ns: match (now, e.newest_stamp) {
                    (Some(n), Some(s)) => Some(n.saturating_sub(s)),
                    _ => None,
                },
                delta_errors: delta_counters.errors(),
            });
        }

        Tick {
            tick,
            elapsed,
            capture,
            rows,
            feed: self.feed.iter().cloned().collect(),
        }
    }

    /// Counter-delta and claim-change events for one edge.
    fn emit_edge_events(
        &mut self,
        tick: u64,
        e: &EdgeSample,
        prev: &PrevEdge,
        delta: CounterSample,
    ) {
        let extrap = delta.extrap_before + delta.extrap_after;
        if extrap > 0 {
            self.push_event(FeedEvent {
                tick,
                severity: Severity::Warn,
                id: Some(Tft::Tft010),
                subject: e.label.clone(),
                message: format!(
                    "+{extrap} extrapolation ({} before, {} after), worst gap {}",
                    delta.extrap_before,
                    delta.extrap_after,
                    fmt_ns(e.counters.worst_extrap_gap_ns)
                ),
            });
        }
        if delta.recycled > 0 {
            self.push_event(FeedEvent {
                tick,
                severity: Severity::Warn,
                // `TFT011`'s question, observed directly.
                id: Some(Tft::Tft011),
                subject: e.label.clone(),
                message: format!("+{} reader lapped by the writer", delta.recycled),
            });
        }
        if delta.no_data > 0 {
            self.push_event(FeedEvent {
                tick,
                severity: Severity::Warn,
                id: None,
                subject: e.label.clone(),
                message: format!("+{} lookups against an edge with no samples", delta.no_data),
            });
        }
        if delta.contended > 0 {
            self.push_event(FeedEvent {
                tick,
                severity: Severity::Warn,
                id: None,
                subject: e.label.clone(),
                message: format!("+{} slot contended past the retry limit", delta.contended),
            });
        }
        if e.claimed != prev.claimed {
            self.push_event(FeedEvent {
                tick,
                severity: Severity::Info,
                id: None,
                subject: e.label.clone(),
                message: if e.claimed {
                    format!("claimed by pid {}", e.owner_pid)
                } else {
                    "claim released".to_owned()
                },
            });
        }
    }

    fn push_event(&mut self, ev: FeedEvent) {
        if self.feed.len() == FEED_CAPACITY {
            self.feed.pop_front();
        }
        self.feed.push_back(ev);
    }
}

/// ANSI colour codes, or empty strings when colour is off.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Dim, for units and disclosures.
    pub dim: &'static str,
    /// Warning.
    pub warn: &'static str,
    /// Error.
    pub error: &'static str,
    /// Bold, for headings.
    pub bold: &'static str,
    /// Reset.
    pub reset: &'static str,
}

impl Palette {
    /// The colour palette.
    #[must_use]
    pub fn colour() -> Palette {
        Palette {
            dim: "\x1b[2m",
            warn: "\x1b[33m",
            error: "\x1b[31m",
            bold: "\x1b[1m",
            reset: "\x1b[0m",
        }
    }

    /// No colour: what a pipe, a log file and a test get.
    #[must_use]
    pub fn plain() -> Palette {
        Palette {
            dim: "",
            warn: "",
            error: "",
            bold: "",
            reset: "",
        }
    }

    fn of(self, sev: Severity) -> &'static str {
        match sev {
            Severity::Info => self.dim,
            Severity::Warn => self.warn,
            Severity::Error => self.error,
        }
    }
}

/// What the renderer needs beyond the tick itself.
#[derive(Clone, Debug)]
pub struct RenderOpts {
    /// Colour codes, or [`Palette::plain`].
    pub palette: Palette,
    /// The `--edge` needle, if a detail view was asked for.
    pub detail: Option<String>,
    /// How many feed lines to show.
    pub feed_lines: usize,
    /// The refresh interval, for the header.
    pub interval: Duration,
}

/// Render one tick as a screenful of text.
///
/// Returns a `String`, so the view is a pure function of the tick (testable
/// without a terminal) and one `write_all` per frame avoids a half-drawn flicker.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn render(tick: &Tick, opts: &RenderOpts) -> String {
    use core::fmt::Write as _;
    let p = opts.palette;
    let cap = &tick.capture;
    let mut s = String::with_capacity(4096);

    let _ = writeln!(
        s,
        "{}tf_tree top{} — {} — {}read-only observer{}",
        p.bold, p.reset, cap.source, p.dim, p.reset
    );
    let _ = writeln!(
        s,
        "  tick {}  every {}  {} frames  {} edges  arena {} KiB  counters {}",
        tick.tick,
        fmt_interval(opts.interval),
        cap.frames,
        cap.edges.len(),
        cap.arena_bytes / 1024,
        if cap.counters_compiled_in {
            "on"
        } else {
            "OFF (built without the `counters` feature)"
        },
    );

    // The perturbation disclosure, on screen because it is asked while running.
    let _ = writeln!(
        s,
        "  {}{}{}",
        p.dim,
        match (cap.shared, cap.self_slot) {
            (false, _) =>
                "this process built the tree it is showing, so there is nothing here to \
                           perturb",
            (true, None) =>
                "performs no lookups and records no counters; holds a lock-file byte \
                             but no arena participant record",
            (true, Some(slot)) => {
                // Unreachable via the CLI (`--rw` refused), but a library caller can.
                let _ = slot;
                "ATTACHED READ-WRITE: it holds an arena participant slot and is counted below"
            }
        },
        p.reset
    );

    let mut occ = String::new();
    for (what, used, capacity) in &cap.occupancy {
        let frac = if *capacity == 0 {
            0.0
        } else {
            f64::from(*used) / f64::from(*capacity)
        };
        let colour = if frac > OCCUPANCY_LIMIT { p.warn } else { "" };
        let _ = write!(
            occ,
            "  {colour}{what} {used}/{capacity} ({:.0}%){}",
            frac * 100.0,
            if colour.is_empty() { "" } else { p.reset }
        );
    }
    let _ = writeln!(
        s,
        "  occupancy:{occ}   {}TFT015 warns above {:.0}%{}",
        p.dim,
        OCCUPANCY_LIMIT * 100.0,
        p.reset
    );

    // The same two lines `doctor` prints, from the same code.
    let rings =
        crate::sizing::Rings::from_edges(cap.edges.iter().map(|e| (e.capacity, e.occupancy())));
    let _ = writeln!(s, "  {}", rings.line());
    let _ = writeln!(s, "  {}{}{}", p.dim, crate::sizing::FORMULA, p.reset);

    // `doctor` prints the same `Clock::label`, so the two tools' references compare.
    match cap.clock {
        None => {
            let _ = writeln!(
                s,
                "  {}no samples in any ring: ages and rates are unavailable{}",
                p.dim, p.reset
            );
        }
        Some(clock) => {
            let _ = writeln!(
                s,
                // `Clock::label` already says whether the epochs agree.
                "  {}ages are against the {} ({} ns){}",
                p.dim,
                clock.label(),
                clock.nanos(),
                p.reset
            );
        }
    }
    s.push('\n');

    // `rate(Hz)` is stamp-derived and `d/s` wall-derived.
    let _ = writeln!(
        s,
        "  {:<30} {:<9} {:>9} {:>8} {:>11} {:>10} {:>11} {:>10} {:>7}",
        "edge", "kind", "rate(Hz)", "d/s", "occupancy", "age(ms)", "writer", "ok", "err"
    );
    // `rows[i]` describes `edges[i]` (see [`EdgeRow`]).
    for (row, e) in tick.rows.iter().zip(&cap.edges) {
        let kind = match e.kind {
            EdgeKind::Static => "static",
            EdgeKind::Dynamic => "dynamic",
            EdgeKind::Tombstone => "tombstone",
        };
        let rate = row
            .stats
            .and_then(|st| st.rate_hz())
            .map_or_else(String::new, |hz| format!("{hz:.1}"));
        // Blank for a static edge: its head cannot advance.
        let dps = if e.capacity == 0 {
            String::new()
        } else {
            row.observed_hz
                .map_or_else(String::new, |hz| format!("{hz:.1}"))
        };
        let occupancy = if e.capacity == 0 {
            String::new()
        } else {
            format!("{}/{}", e.occupancy(), e.capacity)
        };
        let age = row
            .age_ns
            .map_or_else(String::new, |ns| format!("{:.1}", ns as f64 / 1e6));
        let writer = if e.claimed {
            format!("pid {}", e.owner_pid)
        } else if e.kind == EdgeKind::Dynamic {
            "UNCLAIMED".to_owned()
        } else {
            String::new()
        };
        // A recent failure and an unwritten dynamic edge are both "look here".
        let colour = if row.delta_errors > 0 || (e.kind == EdgeKind::Dynamic && !e.claimed) {
            p.warn
        } else {
            ""
        };
        let _ = writeln!(
            s,
            "  {colour}{:<30} {:<9} {:>9} {:>8} {:>11} {:>10} {:>11} {:>10} {:>7}{}",
            truncate(&e.label, 30),
            kind,
            rate,
            dps,
            occupancy,
            age,
            writer,
            e.counters.lookups_ok,
            e.counters.errors(),
            if colour.is_empty() { "" } else { p.reset },
        );
    }

    s.push('\n');
    let _ = writeln!(
        s,
        "  {}participants{}  {}(arena record + lock-file byte){}",
        p.bold, p.reset, p.dim, p.reset
    );
    let _ = writeln!(
        s,
        "  {:>4} {:>8} {:<5} {:<7} {:<7} {:>12} {:>10} {:>7}  comm",
        "slot", "pid", "mode", "state", "record", "attached(s)", "ok", "err"
    );
    if cap.participants.is_empty() {
        let _ = writeln!(s, "  {}(none){}", p.dim, p.reset);
    }
    let now = cap.arena_now();
    for pa in &cap.participants {
        // `attached_at_nanos` (arena clock) and ring stamps (publisher clock)
        // routinely disagree; `epoch?` replaces a negative age.
        let attached = match (now, pa.attached_at_nanos) {
            (Some(n), a) if a > 0 && n >= a => format!("{:.1}", (n - a) as f64 / 1e9),
            (_, a) if a > 0 => "epoch?".to_owned(),
            _ => String::new(),
        };
        let state = if pa.alive { "live" } else { "stale" };
        let colour = if pa.alive { "" } else { p.warn };
        let _ = writeln!(
            s,
            "  {colour}{:>4} {:>8} {:<5} {:<7} {:<7} {:>12} {:>10} {:>7}  {}{}",
            pa.slot,
            pa.pid,
            pa.mode.unwrap_or("?"),
            state,
            if pa.in_arena { "yes" } else { "no" },
            attached,
            pa.counters.lookups_ok,
            pa.counters.errors(),
            truncate(&pa.comm, 20),
            if colour.is_empty() { "" } else { p.reset },
        );
    }
    let _ = writeln!(
        s,
        "  {}attached(s) is against the reference clock named above; `epoch?` means the record's \
         clock and that reference do not share one.\n  record=no is a read-only participant: it \
         cannot write an arena record, so it keeps no counters (PHASE5 §5.6).{}",
        p.dim, p.reset
    );

    s.push('\n');
    let _ = writeln!(
        s,
        "  {}feed{} {}(newest last; run `tf_tree doctor` for the full TFT001-TFT019 catalogue){}",
        p.bold, p.reset, p.dim, p.reset
    );
    if tick.feed.is_empty() {
        let _ = writeln!(s, "  {}(nothing yet){}", p.dim, p.reset);
    }
    let start = tick.feed.len().saturating_sub(opts.feed_lines);
    for ev in &tick.feed[start..] {
        let _ = writeln!(
            s,
            "  {}t={:<4} {:<5} {:<7} {:<30} {}{}",
            p.of(ev.severity),
            ev.tick,
            ev.severity.label(),
            ev.id.map_or("-", Tft::id),
            truncate(&ev.subject, 30),
            ev.message,
            p.reset,
        );
    }

    if let Some(needle) = &opts.detail {
        s.push('\n');
        render_detail(&mut s, tick, needle, p);
    }
    s
}

/// The `--edge` pane: window, order statistics, counters and the inter-arrival
/// histogram.
fn render_detail(s: &mut String, tick: &Tick, needle: &str, p: Palette) {
    use core::fmt::Write as _;
    let Some(i) = select_edge_index(&tick.capture.edges, needle) else {
        let _ = writeln!(s, "  {}no edge matches {needle:?}{}", p.warn, p.reset);
        return;
    };
    let e = &tick.capture.edges[i];
    // Sanitized, not truncated: the full name is the point.
    let _ = writeln!(
        s,
        "  {}edge detail{} — {}",
        p.bold,
        p.reset,
        sanitize(&e.label)
    );
    let _ = writeln!(
        s,
        "  kind {:?}  capacity {}  head {}  retained {} samples",
        e.kind, e.capacity, e.head, e.retained,
    );
    // The header's arithmetic narrowed to this edge, by the same code.
    let _ = writeln!(
        s,
        "  {}",
        crate::sizing::Rings::from_edges([(e.capacity, e.occupancy())]).line()
    );
    let c = &e.counters;
    let _ = writeln!(
        s,
        "  counters: ok {}  extrap_before {}  extrap_after {}  no_data {}  recycled {}  contended {}",
        c.lookups_ok, c.extrap_before, c.extrap_after, c.no_data, c.recycled, c.contended
    );
    let _ = writeln!(
        s,
        "  worst extrapolation gap {}  last failure at {} ns (arena clock; 0 = never)",
        fmt_ns(c.worst_extrap_gap_ns),
        c.last_err_nanos
    );
    match (e.oldest_stamp, e.newest_stamp) {
        (Some(o), Some(n)) => {
            let _ = writeln!(
                s,
                "  retained window {} .. {} ({})",
                o,
                n,
                fmt_ns(n.saturating_sub(o))
            );
        }
        _ => {
            let _ = writeln!(s, "  retained window: empty");
        }
    }
    // Already computed in `observe`; do not re-sort per frame.
    let Some(st) = tick.rows[i].stats else {
        let _ = writeln!(
            s,
            "  {}fewer than two retained samples: no inter-arrival distribution{}",
            p.dim, p.reset
        );
        return;
    };
    let _ = writeln!(
        s,
        "  inter-arrival: n {}  min {}  median {}  p99 {}  max {}{}",
        st.n,
        fmt_ns(st.min_ns),
        fmt_ns(st.median_ns),
        fmt_ns(st.p99_ns),
        fmt_ns(st.max_ns),
        if st.non_monotonic > 0 {
            format!("  ({} non-monotonic)", st.non_monotonic)
        } else {
            String::new()
        }
    );
    let buckets = histogram(&e.intervals, 10);
    let peak = buckets.iter().map(|b| b.count).max().unwrap_or(1).max(1);
    for b in &buckets {
        // 40 columns, scaled to the peak: the shape is the point.
        let width = b.count * 40 / peak;
        let _ = writeln!(
            s,
            "  {:>10} .. {:>10}  {:>6}  {}",
            fmt_ns(b.lo_ns),
            fmt_ns(b.hi_ns),
            b.count,
            "#".repeat(width)
        );
    }
}

/// A refresh interval in the unit it was probably typed in (`{:.1}s` alone would
/// render `--interval 60` as `0.1s`).
#[must_use]
pub fn fmt_interval(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{} ms", d.as_millis())
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

/// Nanoseconds in whichever unit keeps three significant figures.
#[must_use]
pub fn fmt_ns(ns: i64) -> String {
    let a = ns.unsigned_abs();
    let sign = if ns < 0 { "-" } else { "" };
    if a < 1_000 {
        format!("{sign}{a} ns")
    } else if a < 1_000_000 {
        format!("{sign}{:.1} us", a as f64 / 1e3)
    } else if a < 1_000_000_000 {
        format!("{sign}{:.1} ms", a as f64 / 1e6)
    } else {
        format!("{sign}{:.2} s", a as f64 / 1e9)
    }
}

/// Replace every control character with `?`.
///
/// Frame names (arbitrary UTF-8) and the lock file's `comm` are bytes another
/// process wrote, and a frame named `"\x1b[2Jowned"` would repaint the operator's
/// terminal and break `--color never` output. `catalogue::json_escape` is the
/// JSON path's half. It also fixes alignment (`{:<30}` counts a control
/// character as zero columns). C1 (`0x80..=0x9F`) is included: an 8-bit terminal
/// reads `U+009B` as CSI.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || ('\u{80}'..='\u{9f}').contains(&c) {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// Sanitize, then truncate to `n` characters with an ellipsis when it bites.
///
/// Counts `char`s, not bytes (a byte slice would panic mid-codepoint), and
/// sanitizes first so truncation cannot cut an escape in half.
fn truncate(s: &str, n: usize) -> String {
    let s = sanitize(s);
    if s.chars().count() <= n {
        return s;
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}

/// Terminal control for the redraw, or nothing at all when stdout is not a tty.
///
/// Piped output must be plain text, free of escape sequences.
struct Screen {
    tty: bool,
    first: bool,
}

impl Screen {
    fn new(tty: bool) -> Screen {
        Screen { tty, first: true }
    }

    /// The prefix that puts the cursor back at the top-left.
    fn home(&mut self) -> &'static str {
        if !self.tty {
            return "";
        }
        if core::mem::take(&mut self.first) {
            // Clear once, then overwrite: clearing every frame flickers.
            "\x1b[2J\x1b[H"
        } else {
            "\x1b[H"
        }
    }

    /// Erase whatever the previous, longer frame left below this one.
    fn tail(&self) -> &'static str {
        if self.tty {
            "\x1b[J"
        } else {
            "\n"
        }
    }
}

/// Run the view.
///
/// `iterations == 0` means "until interrupted"; no sleep follows the last frame.
///
/// # Errors
///
/// Only stdout failures; a closed pipe (`head -n 20`) exits quietly.
pub fn run(
    tree: &Tree,
    source: &'static str,
    interval: Duration,
    iterations: u64,
    opts_detail: Option<String>,
    colour: Option<bool>,
    merge_lock: &dyn Fn(&mut Capture),
) -> Result<()> {
    let mut out = std::io::stdout();
    let tty = out.is_terminal();
    let mut screen = Screen::new(tty);
    let opts = RenderOpts {
        palette: if colour.unwrap_or(tty) {
            Palette::colour()
        } else {
            Palette::plain()
        },
        detail: opts_detail,
        feed_lines: 8,
        interval,
    };

    let mut sampler = Sampler::new();
    let mut last = Instant::now();
    let mut n = 0u64;
    loop {
        let mut capture = Capture::from_tree(tree, source);
        merge_lock(&mut capture);
        let now = Instant::now();
        let tick = sampler.observe(capture, now.duration_since(last));
        last = now;

        let frame = format!("{}{}{}", screen.home(), render(&tick, &opts), screen.tail());
        if let Err(e) = out.write_all(frame.as_bytes()).and_then(|()| out.flush()) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(e.into());
        }

        n += 1;
        if iterations != 0 && n >= iterations {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const UNIX_NOW: i64 = 1_700_000_000_000_000_000;

    fn edge(id: u32, stamps: &[i64]) -> EdgeSample {
        EdgeSample {
            id,
            label: format!("map->base (edge#{id})"),
            kind: EdgeKind::Dynamic,
            capacity: 64,
            head: stamps.len() as u64,
            claimed: true,
            owner_pid: 4242,
            oldest_stamp: stamps.first().copied(),
            newest_stamp: stamps.last().copied(),
            retained: stamps.len(),
            intervals: stamps.windows(2).map(|w| w[1] - w[0]).collect(),
            counters: CounterSample::default(),
        }
    }

    fn capture(edges: Vec<EdgeSample>) -> Capture {
        let clock = Capture::decide_clock(&edges, UNIX_NOW);
        Capture {
            source: "test",
            arena_bytes: 65_536,
            occupancy: vec![("frames", 3, 64), ("edges", 2, 64)],
            frames: 3,
            edges,
            participants: Vec::new(),
            self_slot: None,
            shared: true,
            counters_compiled_in: true,
            clock,
        }
    }

    /// A 100 Hz stream with one 500 ms dropout: the median is the period.
    ///
    /// Mutant: make `interval_stats` fill `median_ns` with the *mean*
    /// (`intervals.iter().sum::<i64>() / n as i64`), which is what
    /// [`IntervalStats::rate_hz`] then divides into.
    #[test]
    fn median_rate_survives_a_dropout() {
        let mut stamps: Vec<i64> = (0..40).map(|i| i * 10_000_000).collect();
        let tail: Vec<i64> = (0..20)
            .map(|i| 200_000_000 + 500_000_000 + i * 10_000_000)
            .collect();
        stamps.truncate(20);
        stamps.extend(tail);
        let st = interval_stats(&edge(1, &stamps).intervals).unwrap();
        let hz = st.rate_hz().unwrap();
        assert!((99.0..=101.0).contains(&hz), "rate {hz}");
        assert_eq!(
            st.max_ns, 510_000_000,
            "the dropout must survive as the max"
        );
        assert_eq!(st.min_ns, 10_000_000);
        assert_eq!(st.non_monotonic, 0);
    }

    /// A stamp that goes backwards is counted, not silently absorbed.
    ///
    /// Mutant: change `filter(|v| **v <= 0)` to `filter(|v| **v < 0)` in
    /// `interval_stats`.
    #[test]
    fn non_monotonic_intervals_are_counted() {
        let stamps = [0i64, 10, 10, 30, 20, 40];
        let st = interval_stats(&edge(1, &stamps).intervals).unwrap();
        assert_eq!(st.non_monotonic, 2, "one repeat and one backwards step");
        assert_eq!(st.min_ns, -10);
    }

    /// The slowest interval — the dropout, the thing being looked for — must
    /// land in the last bucket rather than one past it.
    ///
    /// Mutant: drop the `.min(buckets - 1)` clamp in `histogram`.
    #[test]
    fn histogram_puts_the_maximum_in_the_last_bucket() {
        let intervals = [10i64, 20, 30, 40, 100];
        let h = histogram(&intervals, 4);
        assert_eq!(h.len(), 4);
        assert_eq!(h.iter().map(|b| b.count).sum::<usize>(), intervals.len());
        assert_eq!(h[3].count, 1, "only the 100 belongs in the top bucket");
        assert_eq!(h[0].count, 3, "10, 20 and 30 are all in the bottom decile");
    }

    /// A perfectly regular publisher has a zero-width span; that is normal, not
    /// a division by zero.
    ///
    /// Mutant: delete the `span == 0` early return.
    #[test]
    fn histogram_handles_a_perfectly_regular_stream() {
        let h = histogram(&[10_000_000; 32], 10);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].count, 32);
        assert_eq!(h[0].lo_ns, 10_000_000);
    }

    /// Counter *deltas* drive the feed, so a counter that was already high when
    /// `top` attached does not produce a phantom burst on the first frame.
    ///
    /// Mutant: in `observe`, replace the `prev`-guarded delta with `e.counters`
    /// itself (i.e. treat the absolute value as the delta).
    #[test]
    fn a_preexisting_counter_value_is_not_a_first_frame_event() {
        let mut e = edge(1, &[0, 10_000_000, 20_000_000]);
        e.counters.extrap_after = 7;
        let mut s = Sampler::new();
        let t1 = s.observe(capture(vec![e.clone()]), Duration::from_secs(1));
        assert!(t1.feed.is_empty(), "feed: {:?}", t1.feed);

        e.counters.extrap_after = 9;
        let t2 = s.observe(capture(vec![e]), Duration::from_secs(1));
        assert_eq!(t2.feed.len(), 1);
        assert_eq!(t2.feed[0].id, Some(Tft::Tft010));
        assert!(t2.feed[0].message.contains("+2"), "{:?}", t2.feed[0]);
    }

    /// An edge that stops advancing is reported once, not once per tick, and
    /// only after `SILENCE_TICKS`.
    ///
    /// Mutant: delete the `silence_reported = true` assignment.
    #[test]
    fn silence_is_reported_once_and_only_after_the_grace_period() {
        let mut s = Sampler::new();
        let mut head = 3u64;
        let stamps = [0i64, 10_000_000, 20_000_000];
        for _ in 0..2 {
            let mut e = edge(1, &stamps);
            head += 1;
            e.head = head;
            let t = s.observe(capture(vec![e]), Duration::from_secs(1));
            assert!(t.feed.is_empty(), "advancing edges say nothing");
        }
        let mut fired = Vec::new();
        for _ in 0..5 {
            let mut e = edge(1, &stamps);
            e.head = head;
            let t = s.observe(capture(vec![e]), Duration::from_secs(1));
            fired = t.feed;
        }
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert_eq!(fired[0].id, Some(Tft::Tft009));
        assert_eq!(
            fired[0].tick, 5,
            "3 ticks of silence after the tick-2 advance"
        );
    }

    /// Silence is forgiven: an edge that resumes and stops again reports again.
    ///
    /// Mutant: delete the `if advanced { silence_reported = false; }` branch.
    #[test]
    fn silence_rearms_after_the_edge_resumes() {
        let stamps = [0i64, 10_000_000];
        let mut s = Sampler::new();
        let mut head = 2u64;
        let mut feed = Vec::new();
        for tick in 0..14 {
            let mut e = edge(1, &stamps);
            if tick < 2 || tick == 7 {
                head += 1;
            }
            e.head = head;
            feed = s.observe(capture(vec![e]), Duration::from_secs(1)).feed;
        }
        let silences: Vec<_> = feed.iter().filter(|e| e.id == Some(Tft::Tft009)).collect();
        assert_eq!(silences.len(), 2, "{feed:?}");
    }

    /// Ages are relative to the arena's newest stamp, not to the host clock —
    /// an arena whose stamps are boot-relative must not read as 56 years stale.
    ///
    /// Mutant: make `EdgeRow::age_ns` use `SystemTime::now()` nanos as the reference
    /// instead of `Capture::arena_now`.
    #[test]
    fn ages_are_measured_against_the_arena_clock() {
        let fresh = edge(1, &[1_000_000_000, 1_010_000_000, 1_020_000_000]);
        let mut stale = edge(2, &[500_000_000, 510_000_000]);
        stale.label = "base->cam (edge#2)".to_owned();
        let mut s = Sampler::new();
        let t = s.observe(capture(vec![fresh, stale]), Duration::from_secs(1));
        assert_eq!(t.rows[0].age_ns, Some(0), "the newest edge defines now");
        assert_eq!(t.rows[1].age_ns, Some(1_020_000_000 - 510_000_000));
    }

    /// `--edge 1` means edge 1, even though "1" is a substring of "edge#11".
    ///
    /// Mutant: swap the two arms of `select_edge` so the substring match is tried
    /// first.
    #[test]
    fn edge_selection_prefers_an_exact_id() {
        let edges = vec![edge(11, &[0, 1]), edge(1, &[0, 1])];
        assert_eq!(select_edge(&edges, "1").unwrap().id, 1);
        assert_eq!(select_edge(&edges, "edge#11").unwrap().id, 11);
        assert!(select_edge(&edges, "nope").is_none());
    }

    /// The rendered frame states that it observes without perturbing, names the
    /// clock its ages are against, and carries no escape sequence when colour
    /// is off.
    ///
    /// Mutant: make `Palette::plain` return `Palette::colour`.
    #[test]
    fn a_plain_frame_discloses_the_observer_and_has_no_escapes() {
        let mut s = Sampler::new();
        let t = s.observe(
            capture(vec![edge(1, &[0, 10_000_000, 20_000_000])]),
            Duration::from_secs(1),
        );
        let out = render(
            &t,
            &RenderOpts {
                palette: Palette::plain(),
                detail: Some("1".to_owned()),
                feed_lines: 8,
                interval: Duration::from_secs(1),
            },
        );
        assert!(!out.contains('\x1b'), "escape sequence in a plain frame");
        assert!(out.contains("read-only observer"));
        assert!(out.contains("performs no lookups and records no counters"));
        assert!(out.contains("do not share an epoch"), "{out}");
        assert!(out.contains("edge detail"));
        assert!(out.contains("inter-arrival"));
        assert!(out.contains("100.0"), "{out}");
    }

    /// A read-only participant has no arena record, and the pane says so
    /// instead of dropping it.
    ///
    /// Mutant: in `merge_lock_rows`, skip slots with no arena record (i.e. drop the
    /// `None` arm's `push`).
    #[test]
    fn lock_only_participants_appear_with_record_no() {
        let mut c = capture(vec![edge(1, &[0, 1])]);
        c.participants.push(ParticipantSample {
            slot: 2,
            pid: 100,
            mode: None,
            comm: String::new(),
            alive: true,
            in_arena: true,
            attached_at_nanos: 0,
            counters: CounterSample::default(),
            last_err_edge: u32::MAX,
        });
        c.merge_lock_rows(&[
            (2, 100, "rw", "publisher".to_owned(), true),
            (7, 200, "ro", "tf_tree".to_owned(), true),
        ]);
        assert_eq!(c.participants.len(), 2);
        let ro = c.participants.iter().find(|p| p.slot == 7).unwrap();
        assert!(!ro.in_arena);
        assert_eq!(ro.mode, Some("ro"));
        let mut s = Sampler::new();
        let t = s.observe(c, Duration::from_secs(1));
        let out = render(
            &t,
            &RenderOpts {
                palette: Palette::plain(),
                detail: None,
                feed_lines: 8,
                interval: Duration::from_secs(1),
            },
        );
        assert!(out.contains("publisher"), "{out}");
        assert!(out.contains("tf_tree"), "{out}");
        assert!(out.contains("record=no is a read-only participant"));
    }

    /// The lock file's liveness answer overrides the arena record's.
    ///
    /// Mutant: delete the `existing.alive = *held;` line.
    #[test]
    fn a_released_lock_byte_makes_an_arena_record_stale() {
        let mut c = capture(Vec::new());
        c.participants.push(ParticipantSample {
            slot: 1,
            pid: 55,
            mode: None,
            comm: String::new(),
            alive: true,
            in_arena: true,
            attached_at_nanos: 0,
            counters: CounterSample::default(),
            last_err_edge: u32::MAX,
        });
        c.merge_lock_rows(&[(1, 55, "rw", "gone".to_owned(), false)]);
        assert!(!c.participants[0].alive);
    }

    /// The non-tty path emits no cursor control at all.
    ///
    /// Mutant: make `Screen::home` return the escape unconditionally.
    #[test]
    fn a_pipe_gets_no_cursor_control() {
        let mut piped = Screen::new(false);
        assert_eq!(piped.home(), "");
        assert_eq!(piped.tail(), "\n");
        let mut tty = Screen::new(true);
        assert_eq!(tty.home(), "\x1b[2J\x1b[H", "first frame clears");
        assert_eq!(tty.home(), "\x1b[H", "later frames only home");
    }

    /// A counter that appears to go backwards saturates to zero.
    ///
    /// Mutant: change `saturating_sub` to `wrapping_sub` in `CounterSample::since`.
    #[test]
    fn counters_that_go_backwards_saturate() {
        let hi = CounterSample {
            extrap_after: 10,
            ..CounterSample::default()
        };
        let lo = CounterSample {
            extrap_after: 3,
            ..CounterSample::default()
        };
        assert_eq!(lo.since(&hi).extrap_after, 0);
        assert_eq!(hi.since(&lo).extrap_after, 7);
    }

    /// Multi-byte frame names must not be sliced across a UTF-8 boundary.
    ///
    /// Mutant: implement `truncate` as `s[..n].to_owned()`.
    #[test]
    fn truncation_is_char_wise() {
        assert_eq!(truncate("ééééé", 3), "éé…");
        assert_eq!(truncate("abc", 3), "abc");
    }

    /// One publisher with a units error must not define "now" for the whole view
    /// (`checks::a_single_units_error_cannot_capture_the_reference_clock`): five
    /// distinct Unix stamps plus one at `UNIX_NOW * 2`.
    ///
    /// Mutant: `decide_clock` returning `NewestStamp(max)` makes the healthy ages ~1.7e18 ns.
    #[test]
    fn one_broken_publisher_cannot_define_the_reference_clock() {
        let mut edges: Vec<EdgeSample> = (0..5)
            .map(|i| {
                let s = UNIX_NOW - i * 200_000_000;
                let mut e = edge(u32::try_from(i).unwrap() + 1, &[s - 10_000_000, s]);
                e.label = format!("healthy{i}->child (edge#{})", i + 1);
                e
            })
            .collect();
        let rogue = UNIX_NOW * 2;
        let mut bad = edge(6, &[rogue - 10_000_000, rogue]);
        bad.label = "rogue->child (edge#6)".to_owned();
        edges.push(bad);

        let c = capture(edges);
        assert_eq!(
            c.clock,
            Some(Clock::Wall(UNIX_NOW)),
            "5 of 6 edges agree with the wall clock"
        );

        let mut s = Sampler::new();
        let t = s.observe(c, Duration::from_secs(1));
        for (i, row) in t.rows.iter().take(5).enumerate() {
            let age = row.age_ns.unwrap();
            assert!(
                (0..1_000_000_000).contains(&age),
                "healthy edge {i} reads age {age}"
            );
        }
        assert!(
            t.rows[5].age_ns.unwrap() < -1_000_000_000_000_000,
            "the rogue edge must be the outlier, not the reference: {:?}",
            t.rows[5].age_ns
        );
    }

    /// A boot-relative arena falls back to the median newest stamp, and the
    /// header names the clock.
    ///
    /// Mutant: printing a literal instead of `clock.label()`.
    #[test]
    fn a_boot_relative_arena_names_the_median_stamp_as_its_clock() {
        // Seconds-since-boot stamps; three distinct newest, so the median is neither extreme.
        let edges = vec![
            edge(1, &[1_000_000_000, 1_100_000_000]),
            edge(2, &[1_000_000_000, 1_500_000_000]),
            edge(3, &[1_000_000_000, 9_000_000_000]),
        ];
        let c = capture(edges);
        assert_eq!(c.clock, Some(Clock::NewestStamp(1_500_000_000)));
        let mut s = Sampler::new();
        let t = s.observe(c, Duration::from_secs(1));
        let out = render(&t, &plain_opts(None));
        assert!(out.contains("median arena stamp"), "{out}");
        assert!(out.contains("1500000000 ns"), "{out}");
    }

    /// A stamp near `i64::MIN` must not panic the age column (a wall-clock stamp
    /// in a boot-relative arena or the reverse).
    ///
    /// Mutant: `Some(n - s)` instead of `n.saturating_sub(s)`.
    #[test]
    fn an_extreme_stamp_saturates_rather_than_panicking() {
        let mut extreme = edge(2, &[i64::MIN, i64::MIN]);
        extreme.label = "sunk->child (edge#2)".to_owned();
        let c = capture(vec![edge(1, &[UNIX_NOW - 10_000_000, UNIX_NOW]), extreme]);
        assert_eq!(c.clock, Some(Clock::Wall(UNIX_NOW)));
        let mut s = Sampler::new();
        let t = s.observe(c, Duration::from_secs(1));
        assert_eq!(t.rows[1].age_ns, Some(i64::MAX));
    }

    /// Bucket edges, not just the index, survive a span wider than `i64::MAX /
    /// buckets` (one wall-clock stamp in a boot-relative ring gives +-1.75e18).
    ///
    /// Mutant: computing the edges in `i64` overflows in the redraw loop.
    #[test]
    fn histogram_bucket_edges_survive_a_full_range_span() {
        let intervals = [
            10_000_000i64,
            1_500_000_000_000_000_000,
            -1_500_000_000_000_000_000,
            10_000_000,
        ];
        let h = histogram(&intervals, 10);
        assert_eq!(h.len(), 10);
        assert_eq!(h.iter().map(|b| b.count).sum::<usize>(), intervals.len());
        assert_eq!(h[0].lo_ns, -1_500_000_000_000_000_000);
        for w in h.windows(2) {
            assert!(w[1].lo_ns > w[0].lo_ns, "axis is not monotone: {h:?}");
        }
    }

    /// `p99` is the 99th of 100, not the maximum.
    ///
    /// Mutant: index with `sorted[n * 99 / 100]`.
    #[test]
    fn p99_is_not_the_maximum_on_a_round_sample_count() {
        let mut intervals = vec![10_000_000i64; 99];
        intervals.push(500_000_000);
        let st = interval_stats(&intervals).unwrap();
        assert_eq!(st.n, 100);
        assert_eq!(st.max_ns, 500_000_000);
        assert_eq!(st.p99_ns, 10_000_000, "p99 must not be the max");
    }

    /// A ring holding exactly one sample says so (`intervals.len()` reads `0`).
    ///
    /// Mutant: printing `intervals.len() + usize::from(!is_empty())` again.
    #[test]
    fn a_ring_holding_one_sample_reports_one_retained() {
        let one = edge(1, &[12_345]);
        assert_eq!(one.retained, 1);
        assert!(one.intervals.is_empty());
        let mut s = Sampler::new();
        let t = s.observe(capture(vec![one]), Duration::from_secs(1));
        let out = render(&t, &plain_opts(Some("1")));
        assert!(out.contains("retained 1 samples"), "{out}");
        assert!(out.contains("retained window 12345 .. 12345"), "{out}");
    }

    /// A frame name must not reach the terminal as an escape sequence.
    ///
    /// Mutant: dropping `sanitize` from `truncate` and `render_detail`.
    #[test]
    fn a_hostile_frame_name_cannot_reach_the_terminal() {
        let mut e = edge(1, &[0, 10_000_000]);
        e.label = "\u{1b}[2J\u{1b}[31mPWNED\u{7}\u{9b}5m".to_owned();
        let mut c = capture(vec![e]);
        c.merge_lock_rows(&[(3, 9, "ro", "\u{1b}[5mblink".to_owned(), true)]);
        let mut s = Sampler::new();
        let t = s.observe(c, Duration::from_secs(1));
        let out = render(&t, &plain_opts(Some("1")));
        assert!(!out.contains('\u{1b}'), "escape reached the frame: {out:?}");
        assert!(!out.contains('\u{7}'), "bell reached the frame: {out:?}");
        assert!(!out.contains('\u{9b}'), "8-bit CSI reached the frame");
        assert!(out.contains("PWNED"), "{out}");
        assert!(out.contains("blink"), "{out}");
    }

    /// `top`'s occupancy colour fires on exactly the rule `TFT015` fires on.
    ///
    /// Mutant: compare with `frac >= OCCUPANCY_LIMIT`.
    #[test]
    fn occupancy_colours_on_the_same_rule_tft015_fires_on() {
        let render_with = |used: u32| {
            let mut c = capture(vec![edge(1, &[0, 10_000_000])]);
            c.occupancy = vec![("frames", used, 100)];
            let mut s = Sampler::new();
            let t = s.observe(c, Duration::from_secs(1));
            let mut o = plain_opts(None);
            o.palette = Palette::colour();
            render(&t, &o)
                .lines()
                .find(|l| l.contains("occupancy:"))
                .expect("occupancy line")
                .to_owned()
        };
        assert!(
            !render_with(80).contains("\u{1b}[33m"),
            "exactly 80% is not above 80%, and TFT015 does not fire on it"
        );
        assert!(
            render_with(81).contains("\u{1b}[33m"),
            "81% is above the limit and must be coloured"
        );
    }

    fn plain_opts(detail: Option<&str>) -> RenderOpts {
        RenderOpts {
            palette: Palette::plain(),
            detail: detail.map(ToOwned::to_owned),
            feed_lines: 8,
            interval: Duration::from_secs(1),
        }
    }

    /// `top` reads the arena and performs no lookup (banner claim; `docs/PHASE5.md`
    /// §7's amendment): counter activity in total, since a failing lookup moves
    /// an error counter.
    ///
    /// An in-process writable tree on purpose: on a read-only attachment
    /// `Guard::drop` returns early, so the property holds structurally and a live
    /// assertion would be vacuous.
    ///
    /// Mutant: a `tree.lookup("map", "odom", newest)` in `Capture::from_tree`.
    #[cfg(feature = "counters")]
    #[test]
    fn capturing_the_arena_moves_no_counter() {
        fn activity(c: &Capture) -> u64 {
            c.edges
                .iter()
                .map(|e| e.counters.lookups_ok + e.counters.errors())
                .sum()
        }

        let tree = tf_tree_bench::fixture::build_tree().expect("build fixture");
        let (writers, _samples) = tf_tree_bench::fixture::spin_up(&tree).expect("populate history");

        let before = activity(&Capture::from_tree(&tree, "test"));
        for _ in 0..4 {
            let c = Capture::from_tree(&tree, "test");
            assert_eq!(
                activity(&c),
                before,
                "reading the arena moved a counter it is meant to be observing"
            );
        }

        // Non-vacuity: these counters do move. `map <- odom`, not a longer chain:
        // `Guard::drop` credits `lookups_ok` only for a single-edge batch.
        let one_edge = Capture::from_tree(&tree, "test")
            .edges
            .into_iter()
            .find(|e| e.label.starts_with("map->odom"))
            .expect("the fixture publishes map->odom");
        let stamp: tf_tree::Stamp =
            tf_tree::Stamp::from_nanos(one_edge.newest_stamp.expect("map->odom has stamps"));
        let _ = tree.lookup("map", "odom", stamp);
        assert!(
            activity(&Capture::from_tree(&tree, "test")) > before,
            "a real lookup did not move any counter, so the assertions above \
             were vacuous"
        );
        drop(writers);
    }
}
