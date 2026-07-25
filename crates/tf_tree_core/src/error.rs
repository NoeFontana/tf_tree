//! Identity types and the `Copy`, allocation-free error enums.
//!
//! Every error is `Copy` and `no_std`: it carries integer IDs, never a
//! `String`. Names are resolved for humans by a `Display` wrapper (Phase 1
//! step 7) that consults the arena; the error itself stays allocation-free so
//! it can be returned from the wait-free read path. **Every variant that can
//! name an edge does name one** (decision D11).

use core::num::NonZeroU32;

/// Stable identity of a frame.
///
/// A `NonZeroU32` so `Option<FrameId>` is four bytes and index `0` is reserved
/// as the root / "no parent" sentinel. Identity is append-only (invariant 1 /
/// D10): a `FrameId` is never reused, so a stale reference is always in bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameId(NonZeroU32);

impl FrameId {
    /// Construct a `FrameId` from a raw index, returning `None` for the reserved
    /// root sentinel `0`.
    #[inline]
    #[must_use]
    pub const fn new(index: u32) -> Option<FrameId> {
        match NonZeroU32::new(index) {
            Some(nz) => Some(FrameId(nz)),
            None => None,
        }
    }

    /// The raw `u32` index into the frame table.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Stable identity of an edge (index into the edge table).
///
/// Like [`FrameId`], edge identity is append-only; removal is tombstoning, never
/// recycling (invariant 1 / D10). Edge index `0` is a valid edge slot (edges are
/// not sentinel-indexed the way frames are).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeId(pub u32);

impl EdgeId {
    /// The raw `u32` index into the edge table.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A lookup or sample failure.
///
/// `Copy`, allocation-free, `no_std`. Returned by the sample/read path and by
/// plan evaluation (the latter lands in a later PR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LookupError {
    /// A frame name that was never interned into this tree.
    UnknownFrame {
        /// The 64-bit BLAKE3 prefix hash of the requested name.
        hash: u64,
    },
    /// `target` and `source` are in different connected components; the walk to
    /// the common ancestor hit a root at `cut_at`.
    Disconnected {
        /// The target frame of the failed lookup.
        target: FrameId,
        /// The source frame of the failed lookup.
        source: FrameId,
        /// The frame at which the ancestor walk ran out of parents.
        cut_at: FrameId,
    },
    /// The combined path depth exceeded [`crate::MAX_DEPTH`].
    TreeTooDeep {
        /// The depth that overflowed the fixed step array.
        depth: u16,
    },
    /// The edge has no published samples yet.
    NoData {
        /// The edge that is empty.
        edge: EdgeId,
    },
    /// The requested stamp lies outside the retained history of `edge`.
    Extrapolation {
        /// The edge whose history does not cover the request.
        edge: EdgeId,
        /// The requested stamp.
        requested: i64,
        /// The oldest retained stamp on the edge.
        oldest: i64,
        /// The newest published stamp on the edge.
        newest: i64,
    },
    /// The ring lapped the reader mid-read: the bracketing samples were
    /// overwritten before the read completed. The caller decides whether a retry
    /// makes sense.
    SlotRecycled {
        /// The edge whose ring lapped the reader.
        edge: EdgeId,
    },
    /// A single slot's seqlock stayed odd (a write in progress) for
    /// [`crate::buffer::SEQ_RETRY_LIMIT`] consecutive attempts.
    SlotContended {
        /// The edge whose slot stayed contended.
        edge: EdgeId,
    },
    /// The plan was compiled against a topology generation that has since
    /// changed. This is actionable ("re-plan"), not a failure to hide with a
    /// retry loop.
    TopologyChanged {
        /// The topology generation the plan was compiled against.
        plan: u64,
        /// The current topology generation.
        current: u64,
    },
    /// A cross-domain lookup: the plan's time domain does not match the query.
    TimeDomainMismatch {
        /// The domain the plan expects.
        expected: u8,
        /// The domain actually supplied.
        got: u8,
    },
    /// The path crosses dynamic edges in **different** time domains, so no single
    /// query stamp can address all of them. Rejected at compile time rather than
    /// silently sampling one edge's clock with another's stamp (D9).
    MixedTimeDomains {
        /// The edge whose domain differs from the rest of the path.
        edge: EdgeId,
        /// The domain established by the path's earlier dynamic edges.
        expected: u8,
        /// The domain `edge` declares.
        got: u8,
    },
    /// An edge id that names no usable edge record in this arena: out of range
    /// for the edge table, or naming a slot with no sample ring.
    UnknownEdge {
        /// The offending edge id.
        edge: EdgeId,
    },
    /// A frame id out of range for this arena's frame table. [`FrameId`] only
    /// guarantees non-zero, not that the frame exists here.
    FrameOutOfRange {
        /// The offending frame id.
        frame: FrameId,
    },
    /// The topology says this frame has a parent, but records no edge for the
    /// link (`edge_of_child == 0`, the "no edge" sentinel). The path cannot be
    /// evaluated; edge slot `0` is a real record and must not be sampled in its
    /// place.
    MissingEdge {
        /// The child frame whose parent link carries no edge.
        child: FrameId,
    },
}

/// A failed `push` onto an edge's sample ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushError {
    /// The pushed stamp is strictly older than the edge's newest stamp. Stamps
    /// are non-decreasing per edge (invariant 6); equal stamps are accepted and
    /// the newer value wins.
    NonMonotonicStamp {
        /// The edge's current newest stamp.
        last: i64,
        /// The (rejected) stamp that was pushed.
        got: i64,
    },
}

/// A failed attempt to claim exclusive write access to an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClaimError {
    /// The edge is already claimed by a live writer (invariant 4 / D7).
    EdgeAlreadyClaimed {
        /// The PID recorded by the current owner (best-effort diagnostic).
        owner_pid: u32,
    },
}

/// A failed frame interning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameError {
    /// Two distinct names collided on the same 64-bit hash. Detected rather than
    /// silently corrupting (probability ~3e-12 at 1e4 frames, but real).
    FrameHashCollision {
        /// The colliding 64-bit hash.
        hash: u64,
    },
    /// The frame table is full (`max_frames` reached). Capacity is fixed at
    /// construction (invariant 3); there is no growth.
    CapacityExceeded,
    /// The arena is mapped read-only, so a name that is not already interned
    /// cannot be added.
    ///
    /// Interning publishes into the arena's hash table with a
    /// `compare_exchange`, which a `PROT_READ` mapping cannot service — the
    /// process would take `SIGSEGV` rather than an error. A read-only
    /// participant can *resolve* any name the creator declared; it cannot
    /// introduce new ones.
    ReadOnly,
}

/// A failed topology mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TopologyError {
    /// Attaching `child` under the requested parent would create a cycle (the
    /// ancestor walk exceeded its `max_frames` step budget).
    WouldCreateCycle {
        /// The child frame whose attachment was rejected.
        child: FrameId,
    },
    /// The edge table is full (`max_edges` reached).
    CapacityExceeded,
    /// A frame index referenced by the mutation is out of range for this arena.
    UnknownFrame {
        /// The offending raw frame index.
        frame: u32,
    },
}
