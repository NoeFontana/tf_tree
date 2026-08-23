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
/// recycling (invariant 1 / D10).
///
/// **`EdgeId` is a plain `u32` — index `0` is representable — but no builder
/// hands one out.** `TreeBuilder::build` reserves index `0` and stores
/// `declared + 1` in the header's `edge_count`, and `tf_tree doctor` iterates
/// `1..edge_count` to skip it. So the id space a consumer sees is `1 ..=
/// declared`, matching [`FrameId`]'s, and the difference is confined to the
/// header field.
///
/// An earlier version of this comment said edge 0 was an ordinary slot, which
/// contradicted the builder and cost `tf_tree_c::unstable` an off-by-one that
/// its own test caught. The type still permits `EdgeId(0)`; nothing produces
/// one.
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
/// plan compilation and evaluation ([`crate::plan`]).
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
    /// The path is too long for one of the two bounds it has to fit: more than
    /// [`crate::MAX_PATH_EDGES`] raw edges to walk, or more than
    /// [`crate::MAX_DEPTH`] steps once folded.
    ///
    /// One variant covers both because the C ABI's `tft_status` table is frozen
    /// and a second refusal would need a new code to describe a path nobody has
    /// (`0034`). `depth` is what tells them apart.
    TreeTooDeep {
        /// **The count that overran its bound**, and the two cases are disjoint
        /// by construction, so this one number says which bound refused:
        ///
        /// * `MAX_PATH_EDGES + 1` — the walk. It is the only value above
        ///   [`crate::MAX_PATH_EDGES`] this field takes, and it means "more than
        ///   the bound" rather than a measured length: the walk stops the moment
        ///   it runs out of buffer, so it never learns how much further the path
        ///   went.
        /// * `MAX_DEPTH + 1 ..= MAX_PATH_EDGES` — the folded step array, and
        ///   here the number is **exact**. `fold` keeps counting past the end of
        ///   the array precisely so it can report the real folded length.
        ///
        /// It was neither of those before `0034`: it was `nt + ns` at whichever
        /// per-side guard happened to fire, which is the bound for a one-sided
        /// chain, the truth for a balanced two-sided path, and neither for a
        /// lopsided one.
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
    /// A caller's output buffer is too small for the batch.
    ///
    /// Checked before any element is written, so the buffer is untouched
    /// (`docs/PHASE3.md` §5.3): a partially-written output is worse than none,
    /// because it looks like data.
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
    /// child has **no mapping** where the arena was, and every reference into it
    /// is dangling. This crate cannot detect that — it is `no_std` and knows
    /// nothing about processes — so the variant exists here only so that the
    /// `std` facade, which does detect it, has one error type to report through.
    /// Nothing in `tf_tree_core` ever constructs it.
    ///
    /// Not retryable and not a transient: the correct response is to open a new
    /// tree in the child, or to `exec`.
    ChildDetached,
    /// The topology says this frame has a parent, but records no edge for the
    /// link (`edge_of_child == 0`, the "no edge" sentinel). The path cannot be
    /// evaluated; edge slot `0` is a real record and must not be sampled in its
    /// place.
    MissingEdge {
        /// The child frame whose parent link carries no edge.
        child: FrameId,
    },
    /// A derivative was requested from an edge whose interpolation policy does
    /// not have one worth reporting — `docs/PHASE4.md` §2.4.
    ///
    /// This is a **refusal, not a limitation.** `LerpSlerp` does have a body
    /// twist, and computing it would be easy. It is withheld because it is an
    /// artifact of the interpolant rather than of the motion: LerpSlerp holds
    /// the *world-frame* linear velocity constant, so the *body-frame* velocity
    /// rotates through the segment. Measured on one segment, the body-frame `v`
    /// vector swings by 5.29 while its magnitude varies by 5e-10 — so a caller
    /// sanity-checking `‖v‖` sees nothing wrong. Handing that back as a velocity
    /// would be worse than refusing, and the compatibility interpolator exists
    /// to bit-match `tf2`, not to be differentiated.
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
    /// Distinct from [`LookupError::NoData`], which means the edge is empty: here
    /// the *pose* is perfectly well defined and only the derivative is not. Two
    /// causes, both transient and both resolved by publishing another sample:
    /// the ring retains exactly one sample, or the two samples bracketing `t`
    /// carry equal stamps (permitted by invariant 6) and so span zero time.
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
        /// The edge's current newest stamp.
        last: i64,
        /// The (rejected) stamp that was pushed.
        got: i64,
    },
    /// The claim this writer holds was revoked — the edge was reaped and is now
    /// free or owned by someone else.
    ///
    /// Returned instead of writing, because the alternative is two writers on a
    /// single-writer ring (`docs/PHASE2.md` §1, A4). A process that sees this was
    /// judged dead while it was stopped or stalled; the correct response is to
    /// stop publishing and re-claim if it still wants the edge.
    ClaimRevoked {
        /// The edge whose claim was revoked.
        edge: EdgeId,
    },
    /// This handle belongs to a process that no longer exists: it was created
    /// before a `fork()` and is being used in the child. See
    /// [`LookupError::ChildDetached`], which carries the full explanation.
    ///
    /// Never constructed by this crate; the `std` facade is what detects it.
    ChildDetached,
}

/// A failed attempt to claim exclusive write access to an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClaimError {
    /// The edge is already claimed by a live writer (invariant 4 / D7).
    EdgeAlreadyClaimed {
        /// The participant **slot** recorded by the current owner — not a PID.
        ///
        /// A3 made the claim word name a participant record rather than a
        /// process, so only a caller holding the arena can turn this into a pid.
        /// `Tree::claim` does; `doctor` prints both.
        owner_slot: u32,
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
    /// Another interner holds this name's slot and cannot be judged.
    ///
    /// Raised when the claimant is an *anonymous* view (one built without
    /// `ArenaView::as_participant`), which names no participant record, so no
    /// caller can decide whether it is alive. Taking the entry over would
    /// allocate a second id for one name; waiting forever is the hang A8 exists
    /// to prevent. Reporting it is the only remaining option, and it is
    /// actionable: identify the view.
    InternContended,
    /// This handle belongs to a process that no longer exists: it was created
    /// before a `fork()` and is being used in the child. See
    /// [`LookupError::ChildDetached`], which carries the full explanation.
    ///
    /// Never constructed by this crate; the `std` facade is what detects it.
    ChildDetached,
    /// **This name is not declared in this arena, and this participant cannot
    /// declare it.**
    ///
    /// Not a permissions complaint about a frame that exists — the name is
    /// *absent*, and the read-only mapping is only the reason nothing can be
    /// done about it here. Every name the creator declared resolves fine
    /// through a `PROT_READ` mapping, because resolving is a pure read; it is
    /// *interning a new one* that publishes into the hash table with a
    /// `compare_exchange`, which a read-only mapping answers with `SIGSEGV`
    /// rather than with an error.
    ///
    /// So it is the ordinary "unknown frame" answer on the default attach, and
    /// the remedies are the ones for an undeclared name: wait for the publisher
    /// that will intern it, or declare it where the arena is created.
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
