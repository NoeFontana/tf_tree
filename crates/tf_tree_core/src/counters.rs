//! Consumer-side diagnostic counters — `docs/PHASE5.md` §5.
//!
//! # Publish-side counters need no storage, and adding them would be a
//! regression
//!
//! §1.3 is emphatic and it is right: a relaxed `fetch_add` on the push path
//! costs ~5-10 ns against a ~50 ns push, to store something the arena already
//! holds.
//!
//! * **push count** is `EdgeRecord::head`, already a monotone counter of every
//!   sample ever published.
//! * **rate, jitter and gaps** are derivable from the stamp array, which is
//!   already contiguous and cache-friendly.
//! * **last publish time** is the newest stamp.
//!
//! So the entire publish-side diagnostic surface is computed by a *reader*
//! walking data that exists, and the hottest write in the system is untouched.
//! Only consumer-side *failures* need new storage, and those increment on error
//! paths where cost is irrelevant.
//!
//! # What is here, and why the denominator is the interesting one
//!
//! Everything below except `lookups_ok` increments on an error path. Those are
//! free by construction. `lookups_ok` is the denominator — "3 extrapolation
//! errors" means nothing without it — and it is the one that would be on the
//! hot path if it were an atomic per lookup.
//!
//! §5.4's answer is that it does not have to be: a `Guard` is already
//! per-thread, already scoped, and already spans a batch, so the count
//! accumulates in a plain `Cell` and flushes once on `Drop`. A guard spanning
//! 1000 lookups pays one relaxed atomic per 1000 per thread.
//!
//! # Always on
//!
//! §5.3 is NORMATIVE: no environment variable, no runtime flag. A robot runs
//! for weeks and the failure you care about happened once, unattended; if
//! enabling the counter needs a restart, the incident is already gone. The only
//! switch is the compile-time `counters` feature (§5.5), and even that leaves
//! the arena **regions in place** so that disabling it does not fork the layout
//! hash (D34).

use crate::sync::{AtomicI64, AtomicU32, AtomicU64};

/// Per-edge consumer-side counters. One cache line, exclusively.
///
/// `align(64)` and padded to 128 so two edges never share a line. Counters are
/// written by whichever consumer failed, and consumers are on different cores;
/// false-sharing the failure counters of two unrelated edges would turn a
/// diagnostic into a source of the contention it is meant to diagnose.
#[repr(C, align(64))]
#[derive(Debug)]
pub struct EdgeCounters {
    /// Successful lookups that traversed this edge. **The denominator.**
    ///
    /// Flushed from a `Guard` on drop (§5.4), not incremented per lookup.
    pub lookups_ok: AtomicU64,
    /// Requests older than the retained window.
    pub err_extrap_before: AtomicU64,
    /// Requests newer than the newest sample — the common one, and the one that
    /// usually means a publisher stopped rather than that a consumer is early.
    pub err_extrap_after: AtomicU64,
    /// Requests against an edge with no samples at all.
    pub err_no_data: AtomicU64,
    /// The ring lapped a reader mid-read.
    pub err_slot_recycled: AtomicU64,
    /// A slot stayed mid-write past the retry limit.
    pub err_slot_contended: AtomicU64,
    /// When the most recent failure happened, in arena-domain nanoseconds.
    ///
    /// `0` means "never failed". Enough to answer "was this a burst an hour ago
    /// or is it happening now", which is the first question an operator asks and
    /// the one a bare count cannot answer.
    pub last_err_nanos: AtomicI64,
    /// The largest gap, in nanoseconds, between a requested stamp and the
    /// nearest end of the retained window.
    ///
    /// A high-water mark, not a total: "we were 4 seconds past the end once" is
    /// actionable and "we were past the end 900 times" is not.
    pub worst_extrap_gap_ns: AtomicI64,
    _pad: [u8; 64],
}

/// The same counters, per **participant slot** rather than per edge.
///
/// This is what makes the diagnostic actionable: an edge counter says failures
/// exist, and a participant counter says *which consumer* is failing. On a robot
/// with twelve nodes reading one tree, that is the difference between a
/// diagnosis and a hunt.
#[repr(C, align(64))]
#[derive(Debug)]
pub struct ParticipantCounters {
    /// Successful lookups by this participant, across all edges.
    pub lookups_ok: AtomicU64,
    /// Failures by cause, mirroring [`EdgeCounters`].
    pub err_extrap_before: AtomicU64,
    /// See [`EdgeCounters::err_extrap_after`].
    pub err_extrap_after: AtomicU64,
    /// See [`EdgeCounters::err_no_data`].
    pub err_no_data: AtomicU64,
    /// See [`EdgeCounters::err_slot_recycled`].
    pub err_slot_recycled: AtomicU64,
    /// See [`EdgeCounters::err_slot_contended`].
    pub err_slot_contended: AtomicU64,
    /// See [`EdgeCounters::last_err_nanos`].
    pub last_err_nanos: AtomicI64,
    /// The edge this participant most recently failed on, or `u32::MAX`.
    ///
    /// Not in [`EdgeCounters`], where it would be a tautology. Here it closes
    /// the loop: the participant table names the process, and this names what it
    /// was reaching for.
    pub last_err_edge: AtomicU32,
    _pad: [u8; 60],
}

// `Default` by hand rather than derived: `[u8; 64]` has no `Default` impl (the
// standard library stops at 32), and a manual one also lets the "never failed"
// sentinels be stated rather than assumed to be zero.
impl Default for EdgeCounters {
    fn default() -> EdgeCounters {
        EdgeCounters {
            lookups_ok: AtomicU64::new(0),
            err_extrap_before: AtomicU64::new(0),
            err_extrap_after: AtomicU64::new(0),
            err_no_data: AtomicU64::new(0),
            err_slot_recycled: AtomicU64::new(0),
            err_slot_contended: AtomicU64::new(0),
            last_err_nanos: AtomicI64::new(0),
            worst_extrap_gap_ns: AtomicI64::new(0),
            _pad: [0; 64],
        }
    }
}

impl Default for ParticipantCounters {
    fn default() -> ParticipantCounters {
        ParticipantCounters {
            lookups_ok: AtomicU64::new(0),
            err_extrap_before: AtomicU64::new(0),
            err_extrap_after: AtomicU64::new(0),
            err_no_data: AtomicU64::new(0),
            err_slot_recycled: AtomicU64::new(0),
            err_slot_contended: AtomicU64::new(0),
            last_err_nanos: AtomicI64::new(0),
            // `u32::MAX` = "no edge", not edge 0 — which is a real edge id in
            // the raw table even though no builder hands one out.
            last_err_edge: AtomicU32::new(u32::MAX),
            _pad: [0; 60],
        }
    }
}

#[cfg(not(loom))]
const _: () = {
    // The layout is a cross-process contract and is folded into the arena's
    // layout hash (`tf_tree_arena::layout::layout_hash`, whose stride table
    // carries 128 for each of the two counter regions). A change here without
    // a change there would let two incompatible builds attach to each other.
    assert!(core::mem::size_of::<EdgeCounters>() == 128);
    assert!(core::mem::align_of::<EdgeCounters>() == 64);
    assert!(core::mem::size_of::<ParticipantCounters>() == 128);
    assert!(core::mem::align_of::<ParticipantCounters>() == 64);
};

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use core::mem::offset_of;

    /// **The counters must be one cache line each, exclusively.**
    ///
    /// Not decoration: they are written by whichever consumer failed, and
    /// consumers run on different cores. Two edges sharing a line would make
    /// one node's extrapolation errors invalidate another node's counter line,
    /// which is a diagnostic causing the contention it exists to report.
    ///
    /// Mutant: drop `_pad` ⇒ `size_of` falls to 64 and the const assertion
    /// above fires, which is the same guard from the other side.
    #[test]
    fn counters_occupy_exactly_two_cache_lines_each() {
        assert_eq!(core::mem::size_of::<EdgeCounters>(), 128);
        assert_eq!(core::mem::size_of::<ParticipantCounters>(), 128);
        // Every counter must be inside the first line, so a failure path
        // touches one line rather than two.
        assert!(offset_of!(EdgeCounters, worst_extrap_gap_ns) < 64);
        assert!(offset_of!(ParticipantCounters, last_err_edge) < 64);
    }

    /// The two structs must agree field for field where they overlap, or
    /// `doctor` cannot present them in one table — and the moment it cannot,
    /// somebody writes a second formatter and the two drift.
    #[test]
    fn the_two_counter_layouts_agree_on_their_shared_prefix() {
        assert_eq!(
            offset_of!(EdgeCounters, lookups_ok),
            offset_of!(ParticipantCounters, lookups_ok)
        );
        assert_eq!(
            offset_of!(EdgeCounters, err_extrap_before),
            offset_of!(ParticipantCounters, err_extrap_before)
        );
        assert_eq!(
            offset_of!(EdgeCounters, err_slot_contended),
            offset_of!(ParticipantCounters, err_slot_contended)
        );
        assert_eq!(
            offset_of!(EdgeCounters, last_err_nanos),
            offset_of!(ParticipantCounters, last_err_nanos)
        );
    }
}
