//! Multi-buffered topology blocks and the single store that publishes mutations.
//!
//! `unsafe`-free: the atomic slices are handed in by [`crate::arena_view`].
//! [`TOPO_BLOCKS`] blocks are kept; a mutation is applied to an *inactive* one,
//! its depths are recomputed, and the active index is advanced — so a reader
//! sees the old topology or the new one, never a mix (`docs/PHASE1.md` §5.2;
//! `docs/PROJECT.md` §5 D4). **`ArcSwap` is forbidden here**: `Arc` refcounts do
//! not cross a process boundary.
//!
//! # There is no odd state (`docs/PHASE2.md` §1, A1)
//!
//! Phase 1 used a seqlock: bump the generation to odd, copy, flip, bump to even.
//! In one process that was fine, because a writer that died mid-mutation took
//! every reader with it. Across processes it is not: a writer `SIGKILL`ed after
//! the first bump leaves the generation **permanently odd**, and every reader in
//! every other process then spins forever in plan compilation. That wedges the
//! whole arena with no recovery.
//!
//! The odd state was never necessary. The writer only ever mutates a block *no
//! reader is looking at*, and the active block is never modified in place — so
//! publication is a **single store** of `pack(generation + 1, next)` and there is
//! no window to make atomic. A writer killed at any instruction now leaves the
//! arena indistinguishable from one where the mutation never started, which is
//! also what makes the A2 lock stealable with no rollback.
//!
//! # Arena-layout resolution
//!
//! Each block reserves **12 bytes per frame** (`align64(max_frames * 12)`):
//! `parent: u32` + `edge_of_child: u32` + `depth: u16` + 2 bytes of padding. The `edge_of_child[c]`
//! side array (frame → the edge whose child is that frame) lives in the block, as
//! `docs/PHASE1.md` §5.2 intends, so plan compilation is an O(1) array walk and
//! the `(parent, depth, edge_of_child)` triple is double-buffered together under
//! this seqlock — a reader always sees a consistent snapshot. (This resolves the
//! inconsistency between "edge_of_child lives in the topology block" (§5.2) and
//! the nominal 6-byte stride in the §4.3 layout table.)
//!
//! The two `u32` arrays are placed first (`parent`, then `edge_of_child`) and the
//! `u16` `depth` last, so both `u32` arrays stay 4-byte aligned for any
//! `max_frames`.
//!
//! This module is `#[cfg(not(loom))]`: `depth` is an `AtomicU16` which the loom
//! build does not model. The topology loom test reimplements the identical
//! generation/active protocol with wider atoms.

use tf_tree_arena::{pack_topo, unpack_topo, TOPO_BLOCKS};

use crate::error::{FrameId, TopologyError};
use crate::sync::{fence, AtomicU16, AtomicU32, AtomicU64, Ordering};

/// How many times a reader re-reads a topology block before giving up.
///
/// A reader is only disturbed if the writer advances the active index all the
/// way around while the reader is between its first and last field load — with
/// [`TOPO_BLOCKS`] = 4 that takes four mutations inside a three-load window.
/// Topology mutations happen a few hundred times per process lifetime, so this
/// bound is a safety net, not a tuning knob.
const TOPO_RETRY_LIMIT: u32 = 64;

/// One topology block: parallel `parent`/`edge_of_child`/`depth` arrays, each
/// `max_frames` long and indexed by frame id. Index `0` is the reserved root slot.
pub struct Block<'a> {
    /// `parent[c] == 0` means root or unattached.
    pub parent: &'a [AtomicU32],
    /// `edge_of_child[c]` is the id of the edge whose child frame is `c` (`0` if
    /// none). Lets plan compilation find a frame's edge without searching.
    pub edge_of_child: &'a [AtomicU32],
    /// Depth from the frame's root (root frames have depth `0`).
    pub depth: &'a [AtomicU16],
}

/// A view over the header's packed topology word and all of its blocks.
pub struct TopologyView<'a> {
    /// Packed `(generation << 8) | active`, published by a single store.
    topo: &'a AtomicU64,
    blocks: [Block<'a>; TOPO_BLOCKS],
    max_frames: u32,
}

impl<'a> TopologyView<'a> {
    /// Assemble a view from the header's topology word and the blocks.
    #[must_use]
    pub fn new(
        topo: &'a AtomicU64,
        blocks: [Block<'a>; TOPO_BLOCKS],
        max_frames: u32,
    ) -> TopologyView<'a> {
        TopologyView {
            topo,
            blocks,
            max_frames,
        }
    }

    /// The current topology generation.
    ///
    /// Every published generation is stable — A1 removed the odd state — so
    /// there is nothing to spin on and no parity to check.
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        unpack_topo(self.topo.load(Ordering::Acquire)).0
    }

    /// The current generation.
    ///
    /// Retained as a distinct name because callers that pin a snapshot read
    /// better for it, but since A1 there is no unstable generation to wait for
    /// and this is exactly [`Self::generation`].
    #[inline]
    #[must_use]
    pub fn stable_generation(&self) -> u64 {
        self.generation()
    }

    /// Attach `child` under `parent` via edge `edge`: set `parent[child]` and
    /// `edge_of_child[child]`, recompute depths, and publish atomically. Pass
    /// `edge == 0` when only the parent link matters (e.g. reparenting a frame
    /// with no distinct edge record).
    ///
    /// Callers serialize mutations with a builder-side mutex (this is the single
    /// writer of the seqlock). Aborts without flipping if the mutation would
    /// create a cycle.
    ///
    /// # Errors
    ///
    /// * [`TopologyError::WouldCreateCycle`] — attaching `child` under `parent`
    ///   introduces a cycle.
    /// * [`TopologyError::UnknownFrame`] — `child` or `parent` is out of range.
    pub fn set_parent(&self, child: FrameId, parent: u32, edge: u32) -> Result<(), TopologyError> {
        let mf = self.max_frames;
        let c = child.get();
        if c >= mf {
            return Err(TopologyError::UnknownFrame { frame: c });
        }
        if parent >= mf {
            return Err(TopologyError::UnknownFrame { frame: parent });
        }

        // The caller holds the mutation lock (A2 in a shared arena, the facade's
        // mutex in a private one), so this load races nothing.
        let word = self.topo.load(Ordering::Relaxed);
        let (g, active) = unpack_topo(word);
        let active = active as usize % TOPO_BLOCKS;
        let next = (active + 1) % TOPO_BLOCKS;
        let src = &self.blocks[active];
        let dst = &self.blocks[next];

        // Copy the active block into the inactive one, then apply the mutation.
        // parent and edge_of_child move together so the snapshot stays consistent.
        for f in 0..mf as usize {
            dst.parent[f].store(src.parent[f].load(Ordering::Relaxed), Ordering::Relaxed);
            dst.edge_of_child[f].store(
                src.edge_of_child[f].load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
        }
        dst.parent[c as usize].store(parent, Ordering::Relaxed);
        dst.edge_of_child[c as usize].store(edge, Ordering::Relaxed);

        if creates_cycle(dst.parent, c, mf) {
            // Abort costs nothing to undo: the active block was never touched, so
            // the published topology is byte-identical to what it was on entry
            // and the topology word was never stored. The scratch block is left
            // dirty, which is harmless — the next mutation copies the active
            // block over it wholesale before touching it. This is the same
            // property that makes a *crashed* writer harmless, and it is why the
            // A2 lock can be stolen with no rollback.
            return Err(TopologyError::WouldCreateCycle { child });
        }

        recompute_depths(dst, mf);

        // Publish. The Release fence orders every block store above before the
        // single publishing store, which is the only thing a reader observes.
        fence(Ordering::Release);
        self.topo
            .store(pack_topo(g + 1, next as u8), Ordering::Release);
        Ok(())
    }

    /// Read `child`'s `(parent, depth, edge_of_child)` plus the generation it was
    /// read at.
    ///
    /// Wait-free: it never spins on a writer, because there is no state a writer
    /// can leave that a reader must wait out. The loop re-reads only if the
    /// *whole* topology word changed between the first and last field load,
    /// which needs [`TOPO_BLOCKS`] mutations inside a three-load window.
    ///
    /// `None` means `child` is out of range for this arena (`FrameId` only
    /// guarantees non-zero, not in-bounds), or — after [`TOPO_RETRY_LIMIT`]
    /// attempts — that the topology is churning hard enough that no consistent
    /// snapshot could be taken. Callers turn either into an error; neither is a
    /// state a caller can usefully distinguish, and both mean "do not use this
    /// frame".
    #[must_use]
    pub fn read_frame(&self, child: FrameId) -> Option<(u32, u16, u32, u64)> {
        if child.get() >= self.max_frames {
            return None;
        }
        let c = child.get() as usize;
        for _ in 0..TOPO_RETRY_LIMIT {
            let w1 = self.topo.load(Ordering::Acquire);
            let (g1, active) = unpack_topo(w1);
            // Bounds-check the index out of the word before using it: a torn or
            // scribbled word must not index past the block array.
            let blk = active as usize % TOPO_BLOCKS;
            let parent = self.blocks[blk].parent[c].load(Ordering::Relaxed);
            let depth = self.blocks[blk].depth[c].load(Ordering::Relaxed);
            let edge = self.blocks[blk].edge_of_child[c].load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.topo.load(Ordering::Relaxed) == w1 {
                return Some((parent, depth, edge, g1));
            }
        }
        None
    }
}

/// Walk from `child`'s (already-updated) parent to a root, budget `max_frames`.
/// Returns `true` if the walk revisits `child` or overruns the budget.
fn creates_cycle(parent: &[AtomicU32], child: u32, max_frames: u32) -> bool {
    let mut cur = parent[child as usize].load(Ordering::Relaxed);
    for _ in 0..max_frames {
        if cur == 0 {
            return false; // reached a root
        }
        if cur == child {
            return true; // closed a loop back to child
        }
        cur = parent[cur as usize].load(Ordering::Relaxed);
    }
    true // budget exhausted without hitting a root: treat as a cycle
}

/// Recompute every frame's depth from the block's `parent` array. Roots (parent
/// `0`) have depth `0`. O(max_frames * depth); mutation rates are low.
fn recompute_depths(block: &Block<'_>, max_frames: u32) {
    for f in 1..max_frames as usize {
        let mut d: u16 = 0;
        let mut cur = block.parent[f].load(Ordering::Relaxed);
        let mut steps = 0u32;
        while cur != 0 && steps < max_frames {
            d = d.saturating_add(1);
            cur = block.parent[cur as usize].load(Ordering::Relaxed);
            steps += 1;
        }
        block.depth[f].store(d, Ordering::Relaxed);
    }
}
