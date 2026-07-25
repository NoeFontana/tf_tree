//! Double-buffered topology blocks and the seqlock that publishes mutations.
//!
//! `unsafe`-free: the atomic slices are handed in by [`crate::arena_view`]. Two
//! blocks are kept; a mutation is applied to the inactive block, its depths are
//! recomputed, and the active index is flipped — so a reader sees the old
//! topology or the new one, never a mix (decision `0003`, D4). **`ArcSwap` is
//! forbidden here**: `Arc` refcounts do not cross a process boundary.
//!
//! # Arena-layout resolution
//!
//! Each block reserves **10 bytes per frame** (`align64(max_frames * 10)`):
//! `parent: u32` + `edge_of_child: u32` + `depth: u16`. The `edge_of_child[c]`
//! side array (frame → the edge whose child is that frame) lives in the block, as
//! decision `0003` intends, so plan compilation is an O(1) array walk and the
//! `(parent, depth, edge_of_child)` triple is double-buffered together under this
//! seqlock — a reader always sees a consistent snapshot. (This resolves the
//! 0003 inconsistency between "edge_of_child lives in the topology block" and the
//! nominal 6-byte stride.)
//!
//! The two `u32` arrays are placed first (`parent`, then `edge_of_child`) and the
//! `u16` `depth` last, so both `u32` arrays stay 4-byte aligned for any
//! `max_frames`.
//!
//! This module is `#[cfg(not(loom))]`: `depth` is an `AtomicU16` which the loom
//! build does not model. The topology loom test reimplements the identical
//! generation/active protocol with wider atoms.

use crate::error::{FrameId, TopologyError};
use crate::sync::{fence, spin, AtomicU16, AtomicU32, AtomicU64, Ordering};

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

/// A view over the header's topology seqlock and both double-buffered blocks.
pub struct TopologyView<'a> {
    generation: &'a AtomicU64,
    active: &'a AtomicU32,
    blocks: [Block<'a>; 2],
    max_frames: u32,
}

impl<'a> TopologyView<'a> {
    /// Assemble a view from the header atomics and the two blocks.
    #[must_use]
    pub fn new(
        generation: &'a AtomicU64,
        active: &'a AtomicU32,
        blocks: [Block<'a>; 2],
        max_frames: u32,
    ) -> TopologyView<'a> {
        TopologyView {
            generation,
            active,
            blocks,
            max_frames,
        }
    }

    /// The raw topology generation counter. **Odd while a mutation is in
    /// flight** — callers that key a cache or pin a snapshot on it want
    /// [`Self::stable_generation`] instead; this accessor exists for the seqlock
    /// retry loops that check the parity themselves.
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// The current generation, spun until it is stable (even).
    ///
    /// Every value a plan is stamped with, and every value a reader pins for a
    /// batch, must come from here: an odd generation names a torn topology, and
    /// pinning one makes every subsequent `Plan::at` fail with
    /// [`crate::LookupError::TopologyChanged`] for no reason.
    #[inline]
    #[must_use]
    pub fn stable_generation(&self) -> u64 {
        loop {
            let g = self.generation.load(Ordering::Acquire);
            if g & 1 == 0 {
                return g;
            }
            spin();
        }
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

        let g = self.generation.load(Ordering::Relaxed);
        debug_assert!(g % 2 == 0, "topology writer saw an odd generation");
        self.generation.store(g + 1, Ordering::Release); // mark unstable

        let active = self.active.load(Ordering::Relaxed) as usize;
        let inactive = 1 - active;
        let src = &self.blocks[active];
        let dst = &self.blocks[inactive];

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
            // Abort: `active` is untouched, so the published topology is
            // byte-identical to what it was on entry. Restore the *original*
            // generation rather than advancing it — bumping to `g + 2` here would
            // invalidate every compiled `Plan` in the process (they would all
            // return `TopologyChanged`) for a mutation that never happened. The
            // inactive block is left dirty, which is harmless: the next mutation
            // copies the active block over it wholesale before touching it.
            self.generation.store(g, Ordering::Release);
            return Err(TopologyError::WouldCreateCycle { child });
        }

        recompute_depths(dst, mf);

        self.active.store(inactive as u32, Ordering::Release);
        self.generation.store(g + 2, Ordering::Release); // mark stable
        Ok(())
    }

    /// Read `child`'s `(parent, depth, edge_of_child)` under the seqlock. Retries
    /// while a write is in progress; returns the consistent triple plus the
    /// generation it was read at, or `None` if `child` is out of range for this
    /// arena (`FrameId` only guarantees non-zero, not in-bounds).
    #[must_use]
    pub fn read_frame(&self, child: FrameId) -> Option<(u32, u16, u32, u64)> {
        if child.get() >= self.max_frames {
            return None;
        }
        let c = child.get() as usize;
        loop {
            let g1 = self.generation.load(Ordering::Acquire);
            if g1 & 1 != 0 {
                spin();
                continue;
            }
            let blk = self.active.load(Ordering::Acquire) as usize;
            let parent = self.blocks[blk].parent[c].load(Ordering::Relaxed);
            let depth = self.blocks[blk].depth[c].load(Ordering::Relaxed);
            let edge = self.blocks[blk].edge_of_child[c].load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.generation.load(Ordering::Relaxed) == g1 {
                return Some((parent, depth, edge, g1));
            }
        }
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
