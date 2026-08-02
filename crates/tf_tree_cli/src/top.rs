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
//!   read-only participant records nothing at all. That is asserted, not
//!   claimed: [`tests::capturing_the_arena_moves_no_counter`] reads a populated
//!   arena repeatedly and requires every counter to stand still.
//!
//! # Its ages are against `doctor`'s reference clock
//!
//! [`Capture::decide_clock`] delegates to [`crate::checks::Clock::decide`]
//! rather than reducing the arena's stamps itself. The reduction that looks
//! obvious — the newest stamp on any edge — is the one this repository already
//! removed from `doctor`, because it lets a single broken publisher define
//! "now" and invert every staleness reading in the view.
//!
//! # Frame names are somebody else's bytes
//!
//! Everything this view did not author goes through [`sanitize`] before it is
//! written into an ANSI frame. See that function for what is at stake.
//!
//! # Rates here are observed, never a deviation from a nominal
//!
//! An edge *may* now carry a declared `EdgeRecord::nominal_rate_mhz` (that is
//! what `TFT007` compares against), but this view does not show a deviation and
//! deliberately so: it redraws every few hundred milliseconds, and a column
//! that flickers between "on rate" and "8% slow" as the window slides teaches
//! an operator to ignore it. Judging an observed rate against a declared one is
//! `doctor`'s job, once, with a stated tolerance. So every rate here is derived
//! from evidence and labelled as such:
//!
//! * `rate(Hz)` comes from the **median inter-arrival** of the stamps the ring
//!   still retains. It is in the publisher's stamp domain.
//! * `Δ/s` comes from the **head advance between two ticks** divided by this
//!   observer's own elapsed wall time. It shares no epoch with the stamps and
//!   is the honest answer to "is this edge moving *now*".
//!
//! They disagree when a publisher back-dates or replays, and seeing them
//! disagree is the diagnosis.

use std::collections::{BTreeMap, VecDeque};
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::Result;

use tf_tree::{EdgeId, EdgeKind, Tree};

use crate::catalogue::{Severity, Tft};
// `OCCUPANCY_LIMIT` is imported rather than restated as a local `0.80`. A second
// copy disagreed with `TFT015` at exactly 80.0 %: this pane compared with `>=`
// and `checks::tft015` compares with `>`, so a table sitting on the line was
// coloured yellow beside the words "TFT015 warns above 80%" while `doctor`
// reported nothing at all. One constant, one comparator.
use crate::checks::{Clock, OCCUPANCY_LIMIT};
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
    /// The **first retained stamp in push order** — the oldest one, for a
    /// publisher whose stamps advance.
    ///
    /// Deliberately not `min(stamps)`: paired with `newest_stamp` it is the
    /// window's two ends *as the ring holds them*, and a `min` would quietly
    /// repair a publisher that stamps out of order into a plausible-looking
    /// window. [`IntervalStats::non_monotonic`] is where that shows up instead.
    pub oldest_stamp: Option<i64>,
    /// Newest stamp the ring holds — the last in push order.
    pub newest_stamp: Option<i64>,
    /// How many stamps the ring currently retains.
    ///
    /// Carried rather than reconstructed from `intervals.len()`: the two differ
    /// by one only while there *are* intervals, so `len + 1` reads `0` for a
    /// ring holding exactly one sample — a publisher that has just started,
    /// which is precisely when somebody is watching this pane.
    pub retained: usize,
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
    /// The reference clock every age in this view is measured against, or
    /// `None` when no ring holds a stamp.
    ///
    /// Decided by [`Clock::decide`] — see [`Capture::decide_clock`].
    pub clock: Option<Clock>,
}

impl Capture {
    /// Read the whole arena once.
    ///
    /// Read-only throughout: [`Tree::arena_view`] on a read-only attachment is
    /// a `PROT_READ` mapping, and every load here is `Relaxed`/`Acquire` on a
    /// value somebody else owns.
    ///
    /// # It is a smear, not an instant
    ///
    /// Publishers keep publishing while this walks the tables, exactly as
    /// `tf_tree freeze --from-live` warns about its copy. So an edge's `head`
    /// (read by [`Snapshot::capture`]) can be a few samples behind the stamps
    /// read from its ring a moment later. That is why `head` is only ever used
    /// for *differences between ticks* and never to index the stamp array: a
    /// tick-to-tick delta of a monotone counter is right whichever side of the
    /// skew each read landed on, and at a 1 Hz redraw the skew is invisible
    /// beside the interval it is divided by.
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
                // `head - capacity` is the slot being overwritten right now, not
                // a retained sample — `retained()` is what excludes it.
                let retained = ring.retained().min(head);
                retained_count = usize::try_from(retained).unwrap_or(usize::MAX);
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

    /// The reference instant every age in this view is measured against.
    ///
    /// # Why this is [`Clock::decide`] and not `newest_stamp.max()`
    ///
    /// The obvious reduction — the arena's newest stamp on any edge — hands the
    /// definition of "now" to the single worst publisher. One
    /// nanoseconds-into-a-seconds-field overshoot (`now * 2`, the classic) is
    /// the arena's maximum by decades, so every *healthy* edge's `age(ms)`
    /// column reads ~54 years and the *broken* one reads `0.0`: the diagnostic
    /// inverted, blaming the innocent majority and exonerating the fault. Every
    /// participant's `attached(s)` degrades to `epoch?` at the same time.
    ///
    /// `checks::Clock::decide` is the estimator this repository already fixed
    /// that bug with (`checks::a_single_units_error_cannot_capture_the_reference_clock`).
    /// It votes: if a majority of edges agree with the host's wall clock the
    /// arena is Unix-stamped and the wall clock is the reference; otherwise the
    /// stamps are in some other domain and the **median** newest stamp is — a
    /// centre one edge cannot drag. `top` gets a second property for free: with
    /// a wall-clock reference an arena where *everything* froze five minutes ago
    /// reads five minutes on every row, where `max()` always painted the newest
    /// edge as `0.0` however long ago it stopped.
    ///
    /// `None` only when no ring holds a stamp at all, which is a real state —
    /// an arena whose publishers have not started.
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
    // `(n - 1) * 99 / 100` rather than `n * 99 / 100`. Neither can leave the
    // slice (`n * 99 / 100 <= n - 1` for every n >= 1), so this is not a bounds
    // argument — it is a rank argument. `n * 99 / 100` lands on `n - 1`, the
    // *maximum*, for every n that is a multiple of 100: at n=100 it picks
    // `sorted[99]`, the largest of 100, which is p100. `p99` and `max` are
    // printed side by side in the detail pane, and a p99 that is defined to
    // equal the max makes the pair useless exactly on the round sample counts a
    // 100- or 1000-slot ring produces. Nearest-rank on `n - 1` gives
    // `sorted[98]` — the 99th of 100.
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
    // The edges are computed in `i128` for the same reason the index above is:
    // `span` can be up to `i64::MAX` (one wall-clock stamp landing in a
    // boot-relative ring gives intervals of +-1.75e18, which `checks.rs` already
    // catalogues as a real pathology), and `span * i` overflows `i64` for any
    // span past `i64::MAX / buckets` — about 29 years of nanoseconds. In a debug
    // build that is a panic *inside the redraw loop*, on exactly the dataset
    // this pane exists to diagnose.
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
/// An exact id first, then the first label containing `needle`. Id first
/// because `1` is a legal substring of `edge#11` and an operator who typed an
/// id means the id.
#[must_use]
pub fn select_edge<'a>(edges: &'a [EdgeSample], needle: &str) -> Option<&'a EdgeSample> {
    select_edge_index(edges, needle).map(|i| &edges[i])
}

/// [`select_edge`] as a position, so a caller that also holds the parallel
/// `Vec<EdgeRow>` can reach the row without a second search.
#[must_use]
pub fn select_edge_index(edges: &[EdgeSample], needle: &str) -> Option<usize> {
    if let Ok(id) = needle.parse::<u32>() {
        if let Some(i) = edges.iter().position(|e| e.id == id) {
            return Some(i);
        }
    }
    edges.iter().position(|e| e.label.contains(needle))
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
///
/// **Positional, not self-contained:** `rows[i]` describes `capture.edges[i]`.
/// It used to carry a `sample: EdgeSample` clone of that same element, which
/// deep-copied the label and the whole interval vector once per tick, for no
/// reader — `render` only ever read `row.sample`, which *is* `edges[i]`. The
/// cost is `8 * sum(retained)` bytes per redraw and is linear in both edge count
/// and ring capacity: the reference fixture retains 12 600 samples across its
/// four dynamic edges, so it was copying ~100 KB a frame to hand the renderer
/// what it was already holding. The renderer zips the two vectors instead.
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
    /// A copy of the sampler's deque, and deliberately the *whole* one rather
    /// than the `feed_lines` the renderer will show: bounded at
    /// [`FEED_CAPACITY`] by construction, so unlike the per-edge interval
    /// vectors it does not grow with the arena, and a tick that carried only
    /// the visible tail could not answer "did this fire twice over fourteen
    /// ticks", which is what the hysteresis tests ask it.
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
                // Saturating: `n` and `s` can be in different time domains (a
                // wall-clock stamp in a boot-relative arena, or the reverse),
                // and `i64::MIN - anything` is a panic in the redraw loop. The
                // detail pane two panes away already saturates the same
                // quantity; this is the same subtraction and gets the same
                // treatment.
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

    // Which clock the ages are against is not a footnote: `doctor` prints the
    // same `Clock::label` for the same reason, and an operator comparing the two
    // tools has to be able to see that they agreed on the reference.
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
                // `Clock::label` already says whether the epochs agree, so the
                // sentence does not restate it — and for a `Wall` clock they
                // *do* agree, which the old unconditional disclaimer denied.
                "  {}ages are against the {} ({} ns){}",
                p.dim,
                clock.label(),
                clock.nanos(),
                p.reset
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
    // `rows[i]` describes `edges[i]` — see [`EdgeRow`] for why the row does not
    // carry its own copy.
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
    // Sanitized, not truncated: the detail pane is about one edge and its full
    // name is the point, but it is still a name somebody else's robot chose.
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
    // `tick.rows[i].stats` is `interval_stats(&e.intervals)`, already computed
    // for this exact edge in `observe`. Recomputing it here sorted the interval
    // vector a second time every frame for no new information.
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

/// Replace every control character with `?`.
///
/// **Not decorative.** The two strings this view interpolates that it did not
/// author are frame names — arbitrary UTF-8, `intern_core` validates only the
/// hash — and the lock file's `comm`, which is `from_utf8_lossy` of bytes
/// *another process wrote*. Both land in a full-screen ANSI frame, so a frame
/// named `"\x1b[2Jowned"` clears and repaints the operator's terminal on every
/// redraw, and `--color never > bug_report.txt` stops producing escape-free
/// text — which is the one thing that flag promises. `catalogue::json_escape`
/// exists for the same exposure on the JSON path; this is the ANSI path's half.
///
/// It is also an alignment fix: `{:<30}` counts `char`s, and a control character
/// occupies zero columns, so one of them shifts every column to its right.
///
/// C1 (`0x80..=0x9F`) is included because `ESC` is not the only introducer — a
/// terminal in 8-bit mode reads `U+009B` as CSI directly.
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
/// Counts `char`s, not bytes: a frame name is UTF-8 and slicing it by byte
/// index would panic on a multi-byte boundary — in a redraw loop, i.e. on
/// whatever screen happened to be showing when the name appeared.
///
/// Sanitizing *before* truncating rather than after: truncation must not be able
/// to cut a multi-character escape in half and leave a fragment whose meaning
/// depends on what the next column happens to contain.
fn truncate(s: &str, n: usize) -> String {
    let s = sanitize(s);
    if s.chars().count() <= n {
        return s;
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

    /// A fixed "now" for the clock vote, so no test depends on when it runs.
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
        // The fixture's stamps are boot-relative, so the clock vote falls
        // through to the median and the header says which clock that is.
        assert!(out.contains("do not share an epoch"), "{out}");
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

    /// **One publisher with a units error must not define "now" for the whole
    /// view.**
    ///
    /// The reference clock decides the `age(ms)` column and every participant's
    /// `attached(s)`. Reducing the arena's newest stamps with `max()` hands it
    /// to the single largest one, so a nanoseconds-into-a-seconds-field
    /// overshoot makes the five *healthy* edges read ~54 years stale and the
    /// *broken* one read `0.0` — the diagnostic inverted. `checks.rs` fixed this
    /// once already (`a_single_units_error_cannot_capture_the_reference_clock`,
    /// commit `90561fc`); `top` must not be a third copy of the rejected
    /// estimator.
    ///
    /// The fixture is `checks.rs`'s: five distinct Unix stamps within a second
    /// of `UNIX_NOW`, plus one at `UNIX_NOW * 2`, which is the arena maximum by
    /// decades. Non-degenerate on the axis that matters — the good stamps
    /// differ from each other, so a median is not trivially the max.
    ///
    /// **Mutant:** `Capture::decide_clock` → `Some(Clock::NewestStamp(stamps
    /// .into_iter().max().unwrap()))`. Applied: the rogue defines now, the five
    /// healthy ages become 1.7e18 ns and the `< 1s` assertion fails with
    /// `age 1700000000000000000`.
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
        // ...and the rogue is the one that stands out, as a stamp in the future.
        assert!(
            t.rows[5].age_ns.unwrap() < -1_000_000_000_000_000,
            "the rogue edge must be the outlier, not the reference: {:?}",
            t.rows[5].age_ns
        );
    }

    /// A boot-relative arena falls back to the **median** newest stamp, and the
    /// header names which clock it chose.
    ///
    /// **Mutant:** in `render`, print the literal `"the arena's newest stamp"`
    /// instead of `clock.label()`. Applied: the `median arena stamp` assertion
    /// fails — and with it the property that an operator can tell `top` and
    /// `doctor` agreed on a reference.
    #[test]
    fn a_boot_relative_arena_names_the_median_stamp_as_its_clock() {
        // Seconds-since-boot stamps: nowhere near the Unix epoch, so no edge
        // agrees with the wall clock and the vote falls through. Three distinct
        // newest stamps, so the median is neither the min nor the max.
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

    /// A stamp near `i64::MIN` must not panic the age column.
    ///
    /// Reachable when one publisher writes a wall-clock stamp into a
    /// boot-relative arena or the reverse — the domain mix `checks.rs` already
    /// catalogues — and `now - stamp` then leaves `i64`.
    ///
    /// **Mutant:** `age_ns: Some(n - s)` instead of `n.saturating_sub(s)`.
    /// Applied: "attempt to subtract with overflow" at the `observe` call.
    #[test]
    fn an_extreme_stamp_saturates_rather_than_panicking() {
        let mut extreme = edge(2, &[i64::MIN, i64::MIN]);
        extreme.label = "sunk->child (edge#2)".to_owned();
        let c = capture(vec![edge(1, &[UNIX_NOW - 10_000_000, UNIX_NOW]), extreme]);
        // Non-vacuity: the reference really is the wall clock, so the
        // subtraction really does span the whole i64 range.
        assert_eq!(c.clock, Some(Clock::Wall(UNIX_NOW)));
        let mut s = Sampler::new();
        let t = s.observe(c, Duration::from_secs(1));
        assert_eq!(t.rows[1].age_ns, Some(i64::MAX));
    }

    /// Bucket **edges**, not just the bucket index, must survive a span wider
    /// than `i64::MAX / buckets`.
    ///
    /// A single wall-clock stamp landing in a boot-relative ring — a node
    /// toggling `use_sim_time`, or one zero stamp in a Unix-domain ring — gives
    /// intervals of +-1.75e18 and a span of ~3.5e18. That is the exact
    /// pathology `tf_tree top --edge <it>` exists to draw.
    ///
    /// **Mutant:** compute the edges in `i64` again (`lo_ns: min + span * i as
    /// i64 / n`). Applied: "attempt to multiply with overflow" at the bucket
    /// construction — a panic inside the redraw loop.
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
        // Monotone, non-degenerate edges: the axis is still readable, not
        // wrapped through negative.
        assert_eq!(h[0].lo_ns, -1_500_000_000_000_000_000);
        for w in h.windows(2) {
            assert!(w[1].lo_ns > w[0].lo_ns, "axis is not monotone: {h:?}");
        }
    }

    /// `p99` is the 99th of 100, not the maximum.
    ///
    /// The detail pane prints `p99` and `max` side by side; a `p99` defined to
    /// equal `max` makes that pair carry one number instead of two, on exactly
    /// the round sample counts a 100- or 1000-slot ring produces.
    ///
    /// **Mutant:** index with `sorted[n * 99 / 100]`. Applied: `p99` becomes
    /// 500 ms, equal to `max`, and both assertions fail.
    #[test]
    fn p99_is_not_the_maximum_on_a_round_sample_count() {
        let mut intervals = vec![10_000_000i64; 99];
        intervals.push(500_000_000);
        let st = interval_stats(&intervals).unwrap();
        assert_eq!(st.n, 100);
        assert_eq!(st.max_ns, 500_000_000);
        assert_eq!(st.p99_ns, 10_000_000, "p99 must not be the max");
    }

    /// A ring holding exactly one sample says so.
    ///
    /// Reconstructing the retained count from `intervals.len()` is right for
    /// `n >= 2` and reads `0` at `n == 1` — while the line below it prints a
    /// non-empty retained window. Two adjacent lines contradicting each other,
    /// on a publisher that has just started, which is when somebody is watching.
    ///
    /// **Mutant:** print `e.intervals.len() + usize::from(!e.intervals
    /// .is_empty())` again. Applied: "retained 0 samples" and the assertion
    /// fails.
    #[test]
    fn a_ring_holding_one_sample_reports_one_retained() {
        let one = edge(1, &[12_345]);
        assert_eq!(one.retained, 1);
        assert!(one.intervals.is_empty());
        let mut s = Sampler::new();
        let t = s.observe(capture(vec![one]), Duration::from_secs(1));
        let out = render(&t, &plain_opts(Some("1")));
        assert!(out.contains("retained 1 samples"), "{out}");
        // Non-vacuity: the window line it must agree with is present.
        assert!(out.contains("retained window 12345 .. 12345"), "{out}");
    }

    /// **A frame name is somebody else's UTF-8 and must not reach the terminal
    /// as an escape sequence.**
    ///
    /// Frame names are validated only by their hash, and the lock file's `comm`
    /// is `from_utf8_lossy` of bytes another process wrote. Both are
    /// interpolated into a full-screen ANSI frame, so `"\x1b[2J..."` repaints
    /// the operator's terminal every redraw — and `--color never > report.txt`
    /// stops producing the escape-free text that flag promises.
    /// `catalogue::json_escape` guards the JSON path against the same input.
    ///
    /// **Mutant:** drop the `sanitize` call from `truncate` (and from
    /// `render_detail`'s label). Applied: the frame contains `\x1b[2J` and the
    /// no-escapes assertion fails.
    #[test]
    fn a_hostile_frame_name_cannot_reach_the_terminal() {
        let mut e = edge(1, &[0, 10_000_000]);
        e.label = "\u{1b}[2J\u{1b}[31mPWNED\u{7}\u{9b}5m".to_owned();
        let mut c = capture(vec![e]);
        c.merge_lock_rows(&[(3, 9, "ro", "\u{1b}[5mblink".to_owned(), true)]);
        let mut s = Sampler::new();
        let t = s.observe(c, Duration::from_secs(1));
        // The detail pane prints the label untruncated, so it is covered too.
        let out = render(&t, &plain_opts(Some("1")));
        assert!(!out.contains('\u{1b}'), "escape reached the frame: {out:?}");
        assert!(!out.contains('\u{7}'), "bell reached the frame: {out:?}");
        assert!(!out.contains('\u{9b}'), "8-bit CSI reached the frame");
        // Non-vacuity: the rows really are there, sanitized rather than dropped.
        assert!(out.contains("PWNED"), "{out}");
        assert!(out.contains("blink"), "{out}");
    }

    /// `top`'s occupancy colour fires on exactly the rule `TFT015` fires on.
    ///
    /// A local `0.80` with a `>=` comparator coloured a table sitting on the
    /// line yellow, beside the words "TFT015 warns above 80%", while `doctor`
    /// reported nothing about it.
    ///
    /// **Mutant:** compare with `frac >= OCCUPANCY_LIMIT`. Applied: the 80/100
    /// row is coloured and the first assertion fails.
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

    /// **`top` reads the arena and performs no lookup.**
    ///
    /// The banner claims it "performs no lookups, so it adds nothing to
    /// `lookups_ok` and cannot invent an extrapolation failure", and
    /// `docs/PHASE5.md` §7's amendment repeats it — but until this test the
    /// claim was prose only: a real `tree.lookup(..)` added to
    /// `Capture::from_tree` left every existing test passing. It matters because
    /// `top` is the longest-lived process an operator points at a robot, and an
    /// observer that moves the counters it is displaying makes `doctor`'s
    /// extrapolation-rate findings unreadable.
    ///
    /// Counter activity in total rather than `lookups_ok` alone: a lookup that
    /// *fails* moves an error counter instead, and inventing a failure is the
    /// worse half of the claim.
    ///
    /// **An in-process writable tree, deliberately, and not the live `--attach`
    /// path.** On a read-only attachment `Guard::drop` returns early on
    /// `!view.is_writable()`, so no counter can move whatever the code does —
    /// the property is structurally guaranteed there and a live assertion
    /// would be vacuous. This is the one configuration in which `from_tree`
    /// *could* move a counter, which makes it the only one where the property
    /// has any content. It is also a reachable configuration: `run` takes a
    /// `&Tree`, and a library caller can hand it a writable one.
    ///
    /// **Mutant:** add a `tree.lookup("map", "odom", newest)` to
    /// `Capture::from_tree`, evaluated once per capture. Applied: "reading the
    /// arena moved a counter it is meant to be observing, left: 1, right: 0".
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

        // Non-vacuity: the counters this asserts are still are ones that move.
        // Without this the test would pass just as well against an engine built
        // with no counters at all.
        //
        // `map <- odom` and not a longer chain on purpose: `Guard::drop` credits
        // `lookups_ok` to an edge only when the whole batch went through exactly
        // one, so a two-edge lookup would move nothing and the non-vacuity
        // check would be the vacuous one.
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
