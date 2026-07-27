//! `tf_tree top` — `docs/PHASE5.md` §7's live view of a running arena.
//!
//! # Why there is no `ratatui` here
//!
//! §7 says "TUI first (`ratatui`)". This implements the pane layout §7 asks for
//! — topology with per-edge rate/staleness/occupancy and writer identity, a
//! participant list, a rolling diagnostics feed, and a per-edge detail view with
//! an inter-arrival histogram — with plain ANSI escapes and a redraw loop, and
//! **no new dependency**. `ratatui` brings `crossterm` and its transitive tail
//! into a workspace whose dependency budget is a stated hard rule
//! (`CLAUDE.md`); a redraw loop that only ever emits `ESC[H`, `ESC[K` and
//! `ESC[J` is thirty lines and cannot rot.
//!
//! **What that costs is real and is not hidden:** without a terminal in raw mode
//! there is no key handling, so the per-edge detail view is selected by
//! `--edge <id|name>` on the command line rather than by moving a cursor. Raw
//! mode means `termios`, which means `libc`, which is a dependency *and* an
//! `unsafe` boundary this crate does not have (`#![forbid(unsafe_code)]`). If
//! interactive selection is later judged worth that, it is a decision record,
//! not a drive-by `cargo add`.
//!
//! The alternate screen (`ESC[?1049h`) is deliberately not used either: leaving
//! it requires restoring on `SIGINT`, a signal handler is another `libc`
//! `unsafe`, and a `top` that leaves the operator's terminal wedged after
//! Ctrl-C is worse than one that leaves its last frame scrolled back.
//!
//! # It observes without perturbing
//!
//! * It attaches **read-only** (D18) and *refuses* `--rw` rather than silently
//!   ignoring it — a live view that can write to a robot's tree is a strictly
//!   worse tool than one that cannot.
//! * A read-only attachment writes **no arena participant record**
//!   (`Tree::participant_slot` returns `u32::MAX`), so `top` does not inflate
//!   the participant table `TFT015` measures. It does take a lock-file byte, so
//!   it *is* visible to `tf_tree participants` — the banner says both.
//! * It performs **no lookups**, so it adds nothing to `lookups_ok` and cannot
//!   invent an extrapolation failure. Even if it did, §5.6's amendment means a
//!   read-only participant records nothing at all.
//!
//! # Rates are observed, never a deviation from a nominal
//!
//! `EdgeRecord::nominal_rate_mhz` is always 0 — nothing in the system declares a
//! rate (which is exactly why `TFT007` cannot detect anything, per §0.0). So
//! every rate here is derived from evidence and labelled as such:
//!
//! * `rate(Hz)` comes from the **median inter-arrival** of the stamps the ring
//!   still retains. It is in the publisher's stamp domain.
//! * `Δ/s` comes from the **head advance between two ticks** divided by this
//!   observer's own elapsed wall time. It shares no epoch with the stamps and
//!   is the honest answer to "is this edge moving *now*".
//!
//! They disagree when a publisher back-dates or replays, and seeing them
//! disagree is the diagnosis. Neither is compared against a declared rate,
//! because there is no declared rate to compare against.

use std::collections::{BTreeMap, VecDeque};
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::Result;

use tf_tree::{EdgeId, EdgeKind, Tree};

use crate::catalogue::{Severity, Tft};
use crate::doctor::Snapshot;

// ---------------------------------------------------------------------------
// Capture: one read of the arena, as plain data
// ---------------------------------------------------------------------------

/// The counter values read from one [`tf_tree_core::counters::EdgeCounters`] or
/// [`tf_tree_core::counters::ParticipantCounters`].
///
/// A plain-data copy rather than a borrow of the arena: everything downstream
/// takes *differences between ticks*, and differencing two live atomics that
/// keep moving underneath the subtraction produces numbers that never
/// reconcile with the totals printed beside them.
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

    /// `self - prev`, per field, saturating.
    ///
    /// Saturating rather than wrapping because a counter can only *appear* to
    /// go backwards — an edge id reused after a reap, or a frozen arena opened
    /// after a live one — and a wrapped `u64` would render as `+18446744073709551615`
    /// in the feed and be read as a catastrophe.
    #[must_use]
    pub fn since(&self, prev: &CounterSample) -> CounterSample {
        CounterSample {
            lookups_ok: self.lookups_ok.saturating_sub(prev.lookups_ok),
            extrap_before: self.extrap_before.saturating_sub(prev.extrap_before),
            extrap_after: self.extrap_after.saturating_sub(prev.extrap_after),
            no_data: self.no_data.saturating_sub(prev.no_data),
            recycled: self.recycled.saturating_sub(prev.recycled),
            contended: self.contended.saturating_sub(prev.contended),
            // Not a difference: these two are a timestamp and a high-water mark,
            // and subtracting either would be meaningless.
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
    /// Oldest stamp the ring still retains.
    pub oldest_stamp: Option<i64>,
    /// Newest stamp the ring holds.
    pub newest_stamp: Option<i64>,
    /// Successive differences of the retained stamps, in push order.
    ///
    /// Kept rather than recomputed from a stamp vector because both the rate
    /// and the histogram want exactly this, and the raw stamps are only needed
    /// for the two extremes above.
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
    /// Whether the arena's participant table has a record for this slot.
    ///
    /// `false` for a read-only participant: it holds a lock byte but cannot
    /// write a record, because its mapping is `PROT_READ` (D18). That is the
    /// row shape `top` itself has.
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
    /// Whether this is somebody else's shared arena rather than a tree this
    /// process built. The perturbation disclosure is only meaningful for the
    /// former: an in-process fixture has no other observers to disturb.
    pub shared: bool,
    /// Whether the engine was built with §5's `counters` feature.
    pub counters_compiled_in: bool,
}

impl Capture {
    /// Read the whole arena once.
    ///
    /// Read-only throughout: [`Tree::arena_view`] on a read-only attachment is
    /// a `PROT_READ` mapping, and every load here is `Relaxed`/`Acquire` on a
    /// value somebody else owns.
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
            if let Some(ring) = view.ring(eid) {
                let head = ring.head.load(Ordering::Acquire);
                // `head - capacity` is the slot being overwritten right now, not
                // a retained sample — `retained()` is what excludes it.
                let retained = ring.retained().min(head);
                let mut prev: Option<i64> = None;
                for i in (head - retained)..head {
                    let s = ring.stamps[(i & ring.mask) as usize].load(Ordering::Relaxed);
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
        }
    }

    /// The arena's own idea of "now": the newest stamp on any edge.
    ///
    /// **Not the host clock.** `docs/PHASE5.md` §0.0 records that the arena's
    /// stamps need not share an epoch with the system clock — that is why
    /// `TFT005` skips on the reference fixture — so an age computed against
    /// `SystemTime::now()` would read as decades of staleness on a
    /// boot-relative arena. Every age in this view is relative to this, and the
    /// header says so.
    #[must_use]
    pub fn arena_now(&self) -> Option<i64> {
        self.edges.iter().filter_map(|e| e.newest_stamp).max()
    }

    /// Merge lock-file facts (mode, `comm`, held-ness) into the participant
    /// rows, adding rows for slots the arena table does not have.
    ///
    /// A read-only participant — `top` itself, and every consumer that attaches
    /// the way D18 wants — holds a lock byte and writes no arena record, so
    /// without this the participant pane would show only the writers.
    pub fn merge_lock_rows(&mut self, lock_rows: &[(u32, u32, &'static str, String, bool)]) {
        for (slot, pid, mode, comm, held) in lock_rows {
            match self.participants.iter_mut().find(|p| p.slot == *slot) {
                Some(existing) => {
                    existing.mode = Some(mode);
                    existing.comm.clone_from(comm);
                    // The kernel's answer wins over the arena record's: a record
                    // whose byte has been released is exactly what a leaked slot
                    // looks like, and reporting it as alive would hide it.
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

// ---------------------------------------------------------------------------
// Derived statistics
// ---------------------------------------------------------------------------

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
    /// The observed rate in Hz, from the median interval.
    ///
    /// The **median** and not the mean: one dropout of a second in a 100 Hz
    /// stream moves the mean by 10 % and the median not at all, and the number
    /// an operator wants from this column is "what is it normally doing".
    /// `None` when the median is not positive, which is the only honest answer
    /// for a stream whose stamps do not advance.
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
/// Sorts a copy: the caller's vector is in push order, which the histogram and
/// the non-monotonic count both depend on.
#[must_use]
pub fn interval_stats(intervals: &[i64]) -> Option<IntervalStats> {
    if intervals.is_empty() {
        return None;
    }
    let mut sorted = intervals.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    // `(n - 1) * 99 / 100` rather than `n * 99 / 100`: the latter indexes one
    // past the end when `n` is a multiple of 100 (n=100 -> index 99 is fine,
    // n=200 -> 198, but n*99/100 for n=100 is 99 and for n=1 is 0 — the failure
    // is at n=100k). Nearest-rank on `n - 1` cannot leave the slice.
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
/// Linear rather than logarithmic because the question this answers is "how
/// tight is the period", and a log axis compresses exactly the region that
/// matters. A stream with one 10-second dropout produces one lonely bar on the
/// right, which is the correct picture: the dropout is the finding.
///
/// Returns a single bucket when every interval is identical — a degenerate but
/// entirely normal case (a synthetic fixture, a perfectly regular publisher),
/// and the alternative of a zero-width span is a division by zero.
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
        // `.min(buckets - 1)` is load-bearing, not defensive: `v == max` gives
        // exactly `n`, one past the last bucket. Without the clamp the slowest
        // interval in every dataset — the dropout, the one being looked for —
        // panics the view.
        let idx = ((v.saturating_sub(min)) as i128 * n as i128 / span as i128) as usize;
        counts[idx.min(buckets - 1)] += 1;
    }
    (0..buckets)
        .map(|i| Bucket {
            lo_ns: min + span * i as i64 / n,
            hi_ns: min + span * (i as i64 + 1) / n,
            count: counts[i],
        })
        .collect()
}

/// Pick the edge a `--edge <needle>` detail view is about.
///
/// An exact id first, then the first label containing `needle`. Id first
/// because `1` is a legal substring of `edge#11` and an operator who typed an
/// id means the id.
#[must_use]
pub fn select_edge<'a>(edges: &'a [EdgeSample], needle: &str) -> Option<&'a EdgeSample> {
    if let Ok(id) = needle.parse::<u32>() {
        if let Some(e) = edges.iter().find(|e| e.id == id) {
            return Some(e);
        }
    }
    edges.iter().find(|e| e.label.contains(needle))
}

// ---------------------------------------------------------------------------
// The rolling feed
// ---------------------------------------------------------------------------

/// One line of the rolling diagnostics feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedEvent {
    /// The tick it was raised on.
    pub tick: u64,
    /// How serious it is, in the catalogue's vocabulary.
    pub severity: Severity,
    /// The catalogue id this corresponds to, where one does.
    ///
    /// `None` is deliberate and is not a gap: a claim changing hands is worth
    /// showing and is not a `TFT` finding, and inventing an id for it would put
    /// something in the feed that `doctor --json` consumers cannot look up.
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
/// Three rather than one: at a 1 s interval a 1 Hz publisher can straddle a
/// tick boundary and miss one, and a feed that cries dropout on every slow
/// publisher is a feed operators learn to ignore.
const SILENCE_TICKS: u64 = 3;

/// Turns a stream of [`Capture`]s into per-tick rows and feed events.
///
/// Holds the only mutable state in the view. Separated from both the capture
/// and the rendering so the interesting half — what counts as an event — is
/// testable by feeding it two hand-built captures, with no arena and no
/// terminal.
#[derive(Debug, Default)]
pub struct Sampler {
    tick: u64,
    prev: BTreeMap<u32, PrevEdge>,
    feed: VecDeque<FeedEvent>,
}

/// One edge's row in the rendered table.
#[derive(Clone, Debug)]
pub struct EdgeRow {
    /// The sample this row was computed from.
    pub sample: EdgeSample,
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
                age_ns: match (now, e.newest_stamp) {
                    (Some(n), Some(s)) => Some(n - s),
                    _ => None,
                },
                delta_errors: delta_counters.errors(),
                sample: e.clone(),
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
                // The ring lapped a reader mid-read, which is `TFT011`'s
                // question — the buffer is too small for what its consumers do
                // — observed directly rather than inferred from a gap.
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

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

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

    /// No colour at all — what a pipe, a log file and a test get.
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

/// The occupancy fraction above which `TFT015` fires.
const OCCUPANCY_WARN: f64 = 0.80;

/// Render one tick as a screenful of text.
///
/// Returns a `String` rather than writing: it makes the whole view a pure
/// function of the tick, which is what lets the tests assert on the pixels
/// without a terminal, and it means one `write_all` per frame instead of
/// dozens — a half-drawn frame under a slow pipe is the flicker `top`-like
/// tools are notorious for.
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

    // The perturbation disclosure. It is two sentences at the top of the screen
    // rather than a line in the manual because the question "is this tool making
    // the numbers I am reading" is asked while the tool is running.
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
                // Unreachable through the CLI, which refuses `--rw` — but a
                // library caller can hand `run` a writable tree, and then the
                // view *is* one of the participants it is describing.
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
        let colour = if frac >= OCCUPANCY_WARN { p.warn } else { "" };
        let _ = write!(
            occ,
            "  {colour}{what} {used}/{capacity} ({:.0}%){}",
            frac * 100.0,
            if colour.is_empty() { "" } else { p.reset }
        );
    }
    let _ = writeln!(
        s,
        "  occupancy:{occ}   {}TFT015 warns at 80%{}",
        p.dim, p.reset
    );

    match cap.arena_now() {
        None => {
            let _ = writeln!(
                s,
                "  {}no samples in any ring: ages and rates are unavailable{}",
                p.dim, p.reset
            );
        }
        Some(now) => {
            let _ = writeln!(
                s,
                "  {}ages are against the arena's newest stamp ({now} ns); its stamps need not share \
                 an epoch with this host's clock{}",
                p.dim, p.reset
            );
        }
    }
    s.push('\n');

    // `rate(Hz)` is stamp-derived and `d/s` is wall-derived; the header says
    // which is which, because a reader who assumes they are the same number
    // measured twice will misread every replay.
    let _ = writeln!(
        s,
        "  {:<30} {:<9} {:>9} {:>8} {:>11} {:>10} {:>11} {:>10} {:>7}",
        "edge", "kind", "rate(Hz)", "d/s", "occupancy", "age(ms)", "writer", "ok", "err"
    );
    for row in &tick.rows {
        let e = &row.sample;
        let kind = match e.kind {
            EdgeKind::Static => "static",
            EdgeKind::Dynamic => "dynamic",
            EdgeKind::Tombstone => "tombstone",
        };
        let rate = row
            .stats
            .and_then(|st| st.rate_hz())
            .map_or_else(String::new, |hz| format!("{hz:.1}"));
        // Blank rather than `0.0` for a static edge: its head cannot advance,
        // so a zero there is a fact about the edge kind, not about the
        // publisher, and a column of zeros trains the eye to skip the column.
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
        // Two conditions, one colour: an edge that just failed a lookup and a
        // dynamic edge nobody is writing are both "look here", and a second
        // colour would only compete with the feed for the operator's eye.
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
        // A participant's `attached_at_nanos` and the rings' stamps are two
        // different clocks — the first is the arena's own, the second is
        // whatever the publisher stamped with — and on a real robot they
        // routinely disagree. Showing `epoch?` is the honest rendering of that;
        // the negative age it replaces read as "attached 56 years from now".
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
        "  {}attached(s) is against the arena's newest stamp; `epoch?` means the record's clock \
         and the rings' stamps do not share one.\n  record=no is a read-only participant: it \
         cannot write an arena record, so it keeps no counters (PHASE5 §5.6).{}",
        p.dim, p.reset
    );

    s.push('\n');
    let _ = writeln!(
        s,
        "  {}feed{} {}(newest last; run `tf_tree doctor` for the full TFT001-TFT016 catalogue){}",
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
    let Some(e) = select_edge(&tick.capture.edges, needle) else {
        let _ = writeln!(s, "  {}no edge matches {needle:?}{}", p.warn, p.reset);
        return;
    };
    let _ = writeln!(s, "  {}edge detail{} — {}", p.bold, p.reset, e.label);
    let _ = writeln!(
        s,
        "  kind {:?}  capacity {}  head {}  retained {} samples",
        e.kind,
        e.capacity,
        e.head,
        e.intervals.len() + usize::from(!e.intervals.is_empty()),
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
    let Some(st) = interval_stats(&e.intervals) else {
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
        // 40 columns of bar, scaled to the tallest bucket. Scaled to the peak
        // rather than to the total because the shape is the point and a
        // total-scaled bar for a 1000-sample ring would be invisible.
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

/// A refresh interval, in the unit it was probably typed in.
///
/// `{:.1}s` alone renders `--interval 60` as `0.1s`, which is both wrong to
/// three decimal places and wrong about what the operator asked for.
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

/// Truncate to `n` characters, with an ellipsis when it bites.
///
/// Counts `char`s, not bytes: a frame name is UTF-8 and slicing it by byte
/// index would panic on a multi-byte boundary — in a redraw loop, i.e. on
/// whatever screen happened to be showing when the name appeared.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}

// ---------------------------------------------------------------------------
// The redraw loop
// ---------------------------------------------------------------------------

/// Terminal control for the redraw, or nothing at all when stdout is not a tty.
///
/// A `top` whose output is piped into a file must not fill it with escape
/// sequences, and one whose output is piped into a test must be comparable
/// against plain text.
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
            // Full clear once, then home-and-overwrite: clearing every frame is
            // what makes a redraw loop flicker.
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
/// `iterations == 0` means "until interrupted". The sleep is skipped after the
/// last frame, so a bounded run costs `n * interval - interval` rather than
/// `n * interval` and a test asking for two frames is not billed for a second
/// it does not use.
///
/// # Errors
///
/// Only stdout failures. A closed pipe (`head -n 20`) is not an error: it is
/// how the tool is used, and it exits quietly.
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
            intervals: stamps.windows(2).map(|w| w[1] - w[0]).collect(),
            counters: CounterSample::default(),
        }
    }

    fn capture(edges: Vec<EdgeSample>) -> Capture {
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
        }
    }

    /// A 100 Hz stream with one 500 ms dropout: the median is the period and the
    /// max is the dropout, which is the whole reason the rate is a median.
    ///
    /// **Mutant:** make `interval_stats` fill `median_ns` with the *mean*
    /// (`intervals.iter().sum::<i64>() / n as i64`), which is what
    /// [`IntervalStats::rate_hz`] then divides into. Applied: the mean of the
    /// 38 x 10 ms intervals plus one 510 ms is 22.8 ms, the rate reads 43.8 Hz,
    /// and the `(99.0..=101.0)` assertion fails.
    #[test]
    fn median_rate_survives_a_dropout() {
        let mut stamps: Vec<i64> = (0..40).map(|i| i * 10_000_000).collect();
        // The dropout, inserted in the middle so it is not an endpoint effect.
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
    /// **Mutant:** change `filter(|v| **v <= 0)` to `filter(|v| **v < 0)` in
    /// `interval_stats`. Applied: the repeated stamp's zero-length interval is
    /// no longer counted and `non_monotonic` reads 1 instead of 2.
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
    /// **Mutant:** drop the `.min(buckets - 1)` clamp in `histogram`. Applied:
    /// `index out of bounds: the len is 4 but the index is 4`, i.e. the view
    /// panics on every dataset that has a maximum, which is all of them.
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
    /// **Mutant:** delete the `span == 0` early return. Applied: the division
    /// `... / span as i128` panics with "attempt to divide by zero".
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
    /// **Mutant:** in `observe`, replace the `prev`-guarded delta with
    /// `e.counters` itself (i.e. treat the absolute value as the delta).
    /// Applied: the first tick emits `TFT010 +7 extrapolation` and the
    /// `feed.is_empty()` assertion fails.
    #[test]
    fn a_preexisting_counter_value_is_not_a_first_frame_event() {
        let mut e = edge(1, &[0, 10_000_000, 20_000_000]);
        e.counters.extrap_after = 7;
        let mut s = Sampler::new();
        let t1 = s.observe(capture(vec![e.clone()]), Duration::from_secs(1));
        assert!(t1.feed.is_empty(), "feed: {:?}", t1.feed);

        // Non-vacuity: the same sampler *does* fire once the counter moves.
        e.counters.extrap_after = 9;
        let t2 = s.observe(capture(vec![e]), Duration::from_secs(1));
        assert_eq!(t2.feed.len(), 1);
        assert_eq!(t2.feed[0].id, Some(Tft::Tft010));
        assert!(t2.feed[0].message.contains("+2"), "{:?}", t2.feed[0]);
    }

    /// An edge that stops advancing is reported once, not once per tick, and
    /// only after `SILENCE_TICKS`.
    ///
    /// **Mutant:** delete the `silence_reported = true` assignment. Applied: the
    /// event repeats on ticks 4, 5 and 6 and the `len() == 1` assertion fails
    /// with three events.
    #[test]
    fn silence_is_reported_once_and_only_after_the_grace_period() {
        let mut s = Sampler::new();
        let mut head = 3u64;
        let stamps = [0i64, 10_000_000, 20_000_000];
        // Two ticks that advance, so the edge has "ever advanced".
        for _ in 0..2 {
            let mut e = edge(1, &stamps);
            head += 1;
            e.head = head;
            let t = s.observe(capture(vec![e]), Duration::from_secs(1));
            assert!(t.feed.is_empty(), "advancing edges say nothing");
        }
        // Now it stops. Ticks 3 and 4 are inside the grace period.
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
    /// **Mutant:** delete the `if advanced { silence_reported = false; }` branch.
    /// Applied: the second silence never reports and the final assertion sees 1
    /// event instead of 2.
    #[test]
    fn silence_rearms_after_the_edge_resumes() {
        let stamps = [0i64, 10_000_000];
        let mut s = Sampler::new();
        let mut head = 2u64;
        let mut feed = Vec::new();
        for tick in 0..14 {
            let mut e = edge(1, &stamps);
            // Advance on the first two ticks and on tick 7, stall otherwise.
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
    /// **Mutant:** make `EdgeRow::age_ns` use `SystemTime::now()` nanos as the
    /// reference instead of `Capture::arena_now`. Applied: the age of the
    /// boot-relative edge becomes ~1.7e18 ns and the `< 1s` assertion fails.
    #[test]
    fn ages_are_measured_against_the_arena_clock() {
        // Boot-relative stamps: seconds since boot, nowhere near the Unix epoch.
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
    /// **Mutant:** swap the two arms of `select_edge` so the substring match is
    /// tried first. Applied: the needle "1" matches `edge#11`'s label first and
    /// the `id == 1` assertion fails with 11.
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
    /// **Mutant:** make `Palette::plain` return `Palette::colour`. Applied: the
    /// frame contains `\x1b[` and the no-escapes assertion fails.
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
        assert!(out.contains("need not share an epoch"));
        assert!(out.contains("edge detail"));
        assert!(out.contains("inter-arrival"));
        // 100 Hz, from a stamp-derived median of 10 ms.
        assert!(out.contains("100.0"), "{out}");
    }

    /// A read-only participant has no arena record, and the pane says so
    /// instead of dropping it.
    ///
    /// **Mutant:** in `merge_lock_rows`, skip slots with no arena record (i.e.
    /// drop the `None` arm's `push`). Applied: slot 7 vanishes from the frame
    /// and the `"    7"` assertion fails — which is precisely the bug of a
    /// participant list that shows only writers.
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

    /// The lock file's answer about liveness overrides the arena record's,
    /// because a record whose byte the kernel released is what a leak looks
    /// like.
    ///
    /// **Mutant:** delete the `existing.alive = *held;` line. Applied: the
    /// stale slot still reads `alive == true` and the assertion fails.
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
    /// **Mutant:** make `Screen::home` return the escape unconditionally.
    /// Applied: the first assertion sees `\x1b[2J\x1b[H` and fails.
    #[test]
    fn a_pipe_gets_no_cursor_control() {
        let mut piped = Screen::new(false);
        assert_eq!(piped.home(), "");
        assert_eq!(piped.tail(), "\n");
        let mut tty = Screen::new(true);
        assert_eq!(tty.home(), "\x1b[2J\x1b[H", "first frame clears");
        assert_eq!(tty.home(), "\x1b[H", "later frames only home");
    }

    /// A counter that appears to go backwards saturates to zero rather than
    /// wrapping into a headline-grabbing 1.8e19.
    ///
    /// **Mutant:** change `saturating_sub` to `wrapping_sub` in
    /// `CounterSample::since`. Applied: the delta reads `u64::MAX` and the
    /// `== 0` assertion fails.
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
    /// **Mutant:** implement `truncate` as `s[..n].to_owned()`. Applied:
    /// "byte index 4 is not a char boundary" — a panic in the redraw loop.
    #[test]
    fn truncation_is_char_wise() {
        assert_eq!(truncate("ééééé", 3), "éé…");
        assert_eq!(truncate("abc", 3), "abc");
    }
}
