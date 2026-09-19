//! Identity types and the `Copy`, allocation-free error enums.
//!
//! Errors carry integer IDs, never a `String`, so they can be returned from the
//! wait-free read path. **Every variant that can name an edge does name one**
//! (D11).

use core::fmt;
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
/// recycling (invariant 1 / D10).
///
/// A plain `u32`; index `0` is representable but no builder hands it out:
/// `TreeBuilder::build` reserves it (`edge_count` is `declared + 1`), so consumers
/// see `1 ..= declared`, like [`FrameId`].
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
/// Returned by the sample/read path and by plan compilation and evaluation
/// ([`crate::plan`]). Implements `Display` and [`core::error::Error`], so it
/// propagates with `?` ([`0040`](https://github.com/NoeFontana/tf_tree/blob/main/docs/decisions/0040-the-error-that-cannot-be-returned.md)).
///
/// ```
/// use tf_tree_core::{EdgeId, LookupError};
///
/// fn newest_pose(fail: bool) -> Result<f64, Box<dyn std::error::Error>> {
///     if fail {
///         Err(LookupError::NoData { edge: EdgeId(3) })?;
///     }
///     Ok(1.0)
/// }
///
/// let e = newest_pose(true).unwrap_err();
/// // Identifiers, not names: this type has no arena to resolve against.
/// assert!(e.to_string().contains('3'));
/// ```
///
/// # Identifiers here, names from `Tree::describe`
///
/// Messages say `edge 3`, not `odom -> base_link`: naming needs the arena, which
/// the wait-free read path cannot carry (D11, `docs/API.md` R5). Use
/// `tf_tree::Tree::describe` where a tree is in hand. **The message text is a
/// diagnostic, not a compatibility promise**; the type and discriminant are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LookupError {
    /// A frame name that does not resolve to a frame of this tree.
    ///
    /// Usually a name never interned. `tf_tree::Tree::lookup` also reports a hash
    /// collision ([`FrameError::FrameHashCollision`]) and a mid-publish interner
    /// ([`FrameError::InternContended`]) here; see its `# Errors`.
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
    /// The path is too long for one of the two bounds it has to fit: more than
    /// [`crate::MAX_PATH_EDGES`] raw edges to walk, or more than
    /// [`crate::MAX_DEPTH`] steps once folded.
    ///
    /// `docs/PHASE1.md` §7.1 says why one variant covers both; `depth` tells them apart.
    TreeTooDeep {
        /// The count that overran its bound. `MAX_PATH_EDGES + 1` means the walk
        /// (a lower bound; it stops when the buffer is full). `MAX_DEPTH + 1 ..=
        /// MAX_PATH_EDGES` means the folded step array, and is **exact**.
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
    /// The ring lapped the reader mid-read; the caller decides whether to retry.
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
    /// The plan's topology generation has changed; re-plan.
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
    /// query stamp can address all of them (D9). Rejected at compile time.
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
    /// A caller's output buffer is too small for the batch.
    ///
    /// Checked before any element is written (`docs/PHASE3.md` §5.3).
    BufferTooSmall {
        /// Elements required.
        need: usize,
        /// Elements the buffer has.
        got: usize,
    },
    /// An `f32` layout was passed to the `f64` entry point, or the reverse.
    WrongElementType,
    /// This handle belongs to a process that no longer exists: it was created
    /// before a `fork()` and is being used in the child.
    ///
    /// A shared arena is mapped `MADV_DONTFORK` (`docs/PHASE2.md` §7.3), so the
    /// child has no mapping. Only the `std` facade detects this; this crate never
    /// constructs it. Not retryable: open a new tree in the child, or `exec`.
    ChildDetached,
    /// The topology says this frame has a parent, but records no edge for the
    /// link (`edge_of_child == 0`, the "no edge" sentinel); edge slot `0` must not
    /// be sampled in its place.
    MissingEdge {
        /// The child frame whose parent link carries no edge.
        child: FrameId,
    },
    /// A derivative was requested from an edge whose interpolation policy does
    /// not have one worth reporting — `docs/PHASE4.md` §2.4.
    ///
    /// A **refusal, not a limitation**: `LerpSlerp`'s body twist is an artifact of
    /// the interpolant (it holds the world-frame velocity constant, so the
    /// body-frame velocity rotates while its norm looks fine), and the
    /// compatibility interpolator exists to bit-match `tf2`.
    ///
    /// The fix is to declare the edge `ScLerp`, which is the default.
    DerivativesUnavailable {
        /// The edge whose policy has no reportable derivative.
        edge: EdgeId,
        /// The policy that edge declares, as its stored discriminant.
        interp: u8,
    },
    /// A derivative was requested at a stamp with no segment to differentiate.
    ///
    /// Unlike [`LookupError::NoData`], the pose is defined. Transient: the ring
    /// holds one sample, or the bracketing samples share a stamp (invariant 6).
    NoSegment {
        /// The edge with no differentiable segment at the requested stamp.
        edge: EdgeId,
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
        /// The edge whose newest stamp the push predates (D11: an error names
        /// the edge it is about).
        edge: EdgeId,
        /// The edge's current newest stamp.
        last: i64,
        /// The (rejected) stamp that was pushed.
        got: i64,
    },
    /// The claim was revoked (the edge was reaped) and the push refused
    /// (`docs/PHASE2.md` §1, A4). Stop publishing; re-claim if still wanted.
    ClaimRevoked {
        /// The edge whose claim was revoked.
        edge: EdgeId,
    },
    /// This handle belongs to a process that no longer exists: it was created
    /// before a `fork()` and is being used in the child. See
    /// [`LookupError::ChildDetached`].
    ChildDetached,
}

/// A failed attempt to claim exclusive write access to an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClaimError {
    /// The edge is already claimed by a live writer (invariant 4 / D7).
    EdgeAlreadyClaimed {
        /// The participant **slot** of the current owner, not a PID (A3);
        /// `Tree::claim` resolves it.
        owner_slot: u32,
    },
}

/// A failed frame interning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameError {
    /// Two distinct names collided on the same 64-bit hash (~3e-12 at 1e4 frames).
    FrameHashCollision {
        /// The colliding 64-bit hash.
        hash: u64,
    },
    /// The frame table is full (`max_frames` reached). Capacity is fixed at
    /// construction (invariant 3); there is no growth.
    CapacityExceeded,
    /// Another interner holds this name's slot and cannot be judged.
    ///
    /// The claimant is an *anonymous* view (no `ArenaView::as_participant`), so
    /// its liveness cannot be judged; taking over would mint a second id and
    /// waiting is the hang A8 prevents.
    InternContended,
    /// This handle belongs to a process that no longer exists: it was created
    /// before a `fork()` and is being used in the child. See
    /// [`LookupError::ChildDetached`].
    ChildDetached,
    /// **This name is not declared in this arena, and this participant cannot
    /// declare it.**
    ///
    /// Interning publishes into the hash table with a `compare_exchange`, which
    /// a `PROT_READ` mapping answers with `SIGSEGV`; resolving declared names
    /// works. Wait for the publisher that will intern it, or declare it where the
    /// arena is created.
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

// `Display` and `core::error::Error` (0040): identifiers, never names (`docs/API.md`
// R5, D11); `core::error::Error` keeps the crate `no_std` (MSRV 1.87 > 1.81).
// Matches are exhaustive on purpose, so a new variant fails to compile here.

impl fmt::Display for LookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            LookupError::UnknownFrame { hash } => {
                write!(f, "unknown frame (name hash {hash:#018x})")
            }
            LookupError::Disconnected {
                target,
                source,
                cut_at,
            } => write!(
                f,
                "frames {} and {} are not connected; the walk stops at frame {}",
                target.get(),
                source.get(),
                cut_at.get(),
            ),
            LookupError::TreeTooDeep { depth } => {
                write!(f, "path is {depth} edges deep, past this build's bound")
            }
            LookupError::NoData { edge } => {
                write!(f, "edge {} has no published samples", edge.0)
            }
            LookupError::Extrapolation {
                edge,
                requested,
                oldest,
                newest,
            } => write!(
                f,
                "edge {}: stamp {requested} ns is outside its window [{oldest}, {newest}] ns",
                edge.0,
            ),
            LookupError::SlotRecycled { edge } => write!(
                f,
                "edge {}: the ring lapped the reader mid-read (the window moved past the sample being read)",
                edge.0,
            ),
            LookupError::SlotContended { edge } => write!(
                f,
                "edge {}: a sample slot stayed mid-write past the retry limit",
                edge.0,
            ),
            LookupError::TopologyChanged { plan, current } => write!(
                f,
                "plan is stale: compiled at topology generation {plan}, current is {current} (re-plan)",
            ),
            LookupError::TimeDomainMismatch { expected, got } => write!(
                f,
                "time-domain mismatch: plan expects domain {expected}, query supplied {got}",
            ),
            LookupError::MixedTimeDomains {
                edge,
                expected,
                got,
            } => write!(
                f,
                "path crosses time domains: edge {} is in domain {got}, the rest of the path is in domain {expected}",
                edge.0,
            ),
            LookupError::UnknownEdge { edge } => {
                write!(f, "edge {} names no usable edge in this tree", edge.0)
            }
            LookupError::FrameOutOfRange { frame } => {
                write!(f, "frame id {} is out of range for this tree", frame.get())
            }
            LookupError::BufferTooSmall { need, got } => write!(
                f,
                "output buffer too small: need {need} elements, got {got}",
            ),
            LookupError::WrongElementType => write!(
                f,
                "wrong output element type for this layout (an f32 layout needs an f32 buffer, and the reverse)",
            ),
            LookupError::ChildDetached => write!(
                f,
                "this handle was inherited across a fork and is poisoned in the child",
            ),
            LookupError::MissingEdge { child } => write!(
                f,
                "frame {} has a parent but no edge records the link",
                child.get(),
            ),
            LookupError::DerivativesUnavailable { edge, interp } => write!(
                f,
                "edge {} interpolates under policy {interp}, which has no derivative to report",
                edge.0,
            ),
            LookupError::NoSegment { edge } => write!(
                f,
                "edge {} has no bracketing segment at that stamp, so there is no twist",
                edge.0,
            ),
        }
    }
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PushError::NonMonotonicStamp { edge, last, got } => write!(
                f,
                "edge {}: stamp {got} ns is not newer than the last published {last} ns",
                edge.0,
            ),
            PushError::ClaimRevoked { edge } => write!(
                f,
                "edge {}: this publisher's claim was revoked, so the push was refused",
                edge.0,
            ),
            PushError::ChildDetached => write!(
                f,
                "this publisher was inherited across a fork and is poisoned in the child",
            ),
        }
    }
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ClaimError::EdgeAlreadyClaimed { owner_slot } => write!(
                f,
                "the edge is already claimed by participant slot {owner_slot} (one writer per edge)",
            ),
        }
    }
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            FrameError::FrameHashCollision { hash } => write!(
                f,
                "frame name collides with a different interned name (hash {hash:#018x})",
            ),
            FrameError::CapacityExceeded => {
                write!(f, "the frame table is full; raise the frame headroom")
            }
            FrameError::InternContended => write!(
                f,
                "interning contended past its retry budget; another process may have died mid-intern",
            ),
            FrameError::ChildDetached => write!(
                f,
                "this handle was inherited across a fork and is poisoned in the child",
            ),
            FrameError::ReadOnly => write!(
                f,
                "this tree is a read-only attachment and cannot intern a new frame",
            ),
        }
    }
}

impl fmt::Display for TopologyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TopologyError::WouldCreateCycle { child } => write!(
                f,
                "attaching frame {} under that parent would create a cycle",
                child.get(),
            ),
            TopologyError::CapacityExceeded => {
                write!(f, "the edge table is full; raise the edge headroom")
            }
            TopologyError::UnknownFrame { frame } => {
                write!(f, "frame index {frame} is out of range for this arena",)
            }
        }
    }
}

impl core::error::Error for LookupError {}
impl core::error::Error for PushError {}
impl core::error::Error for ClaimError {}
impl core::error::Error for FrameError {}
impl core::error::Error for TopologyError {}
