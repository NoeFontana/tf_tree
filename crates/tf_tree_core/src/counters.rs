//! Consumer-side diagnostic counters — `docs/PHASE5.md` §5.
//!
//! Publish-side counters need no storage: push count is `EdgeRecord::head`, rate
//! and gaps derive from the stamp array (§1.3). Only consumer-side failures are
//! stored; `lookups_ok` accumulates in a `Guard`'s `Cell` and flushes on `Drop` (§5.4).
//!
//! §5.3 is NORMATIVE: always on, no environment variable or runtime flag. The
//! compile-time `counters` feature (§5.5) leaves the arena regions in place (D34).

use crate::sync::{AtomicI64, AtomicU32, AtomicU64};

/// Per-edge consumer-side counters; `align(64)`, padded to 128 so two edges
/// never share a line.
#[repr(C, align(64))]
#[derive(Debug)]
pub struct EdgeCounters {
    /// Successful lookups that traversed this edge. **The denominator;** flushed on `Guard` drop (§5.4).
    pub lookups_ok: AtomicU64,
    /// Requests older than the retained window.
    pub err_extrap_before: AtomicU64,
    /// Requests newer than the newest sample; usually a stopped publisher.
    pub err_extrap_after: AtomicU64,
    /// Requests against an edge with no samples at all.
    pub err_no_data: AtomicU64,
    /// The ring lapped a reader mid-read.
    pub err_slot_recycled: AtomicU64,
    /// A slot stayed mid-write past the retry limit.
    pub err_slot_contended: AtomicU64,
    /// When the most recent failure happened, in arena-domain nanoseconds.
    /// When the most recent failure happened (arena-domain ns); `0` = never.
    pub last_err_nanos: AtomicI64,
    /// The largest gap (ns) between a requested stamp and the nearest end of
    /// the retained window; a high-water mark.
    pub worst_extrap_gap_ns: AtomicI64,
    _pad: [u8; 64],
}

/// The same counters per **participant slot**: which consumer is failing.
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
    pub last_err_edge: AtomicU32,
    _pad: [u8; 60],
}

// By hand: `[u8; 64]` has no `Default`.
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
            // `u32::MAX` = "no edge"; edge 0 is a real raw id.
            last_err_edge: AtomicU32::new(u32::MAX),
            _pad: [0; 60],
        }
    }
}

#[cfg(not(loom))]
const _: () = {
    // Folded into `tf_tree_arena::layout::layout_hash` (stride 128); change both together.
    assert!(core::mem::size_of::<EdgeCounters>() == 128);
    assert!(core::mem::align_of::<EdgeCounters>() == 64);
    assert!(core::mem::size_of::<ParticipantCounters>() == 128);
    assert!(core::mem::align_of::<ParticipantCounters>() == 64);
};

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use core::mem::offset_of;

    /// Each counter struct owns its cache lines exclusively.
    #[test]
    fn counters_occupy_exactly_two_cache_lines_each() {
        assert_eq!(core::mem::size_of::<EdgeCounters>(), 128);
        assert_eq!(core::mem::size_of::<ParticipantCounters>(), 128);
        // Every counter is in the first line.
        assert!(offset_of!(EdgeCounters, worst_extrap_gap_ns) < 64);
        assert!(offset_of!(ParticipantCounters, last_err_edge) < 64);
    }

    /// The two structs agree where they overlap, so `doctor` has one formatter.
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
