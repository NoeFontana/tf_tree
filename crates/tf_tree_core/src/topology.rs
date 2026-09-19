//! Multi-buffered topology blocks and the single store that publishes mutations.
//!
//! `unsafe`-free: the atomic slices are handed in by [`crate::arena_view`].
//! [`TOPO_BLOCKS`] blocks are kept; a mutation is applied to an *inactive* one and
//! the active index is advanced, so a reader sees the old topology or the new one
//! (`docs/PHASE1.md` §5.2; `docs/PROJECT.md` §5 D4). **`ArcSwap` is forbidden
//! here**: `Arc` refcounts do not cross a process boundary.
//!
//! # There is no odd state (`docs/PHASE2.md` §1, A1)
//!
//! The active block is never modified in place, so publication is a **single
//! store** of `pack(generation + 1, next)`. A writer killed at any instruction
//! leaves the arena as if the mutation never started, which is what makes the A2
//! lock stealable with no rollback.
//!
//! # Layout
//!
//! 12 B per frame (`docs/PHASE1.md` §4.3): `parent` then `edge_of_child` (`u32`),
//! `depth` (`u16`) last, so both `u32` arrays stay 4-byte aligned.
//!
//! # The mutation lock lives in the arena (`docs/PHASE2.md` §1, A2)
//!
//! [`TopoLockView`] is its writer side. The lock word lives in the header
//! (`tf_tree_arena::TopoLock`), and it is *reapable*: the spin is bounded, and an
//! acquirer that runs out of patience asks whether the holder is alive before
//! stealing (see [`TopoLockView::acquire`]).
//!
//! `#[cfg(not(loom))]`: `depth` is an `AtomicU16`, which loom does not model; the
//! topology loom test reimplements the protocol with wider atoms.

use tf_tree_arena::{pack_topo, unpack_topo, TOPO_BLOCKS};

use crate::crash::crash_point;
use crate::error::{FrameId, TopologyError};
use crate::sync::{fence, spin, AtomicI64, AtomicU16, AtomicU32, AtomicU64, Ordering};

/// How many times a reader re-reads a topology block before giving up: a safety
/// net (a reader is disturbed only if [`TOPO_BLOCKS`] mutations land inside its
/// three-load window), not a tuning knob.
const TOPO_RETRY_LIMIT: u32 = 64;

/// How many times an acquirer re-tries the lock CAS before asking whether the
/// holder is alive (`docs/PHASE2.md` §1, A2). The wait must be bounded; the value
/// only has to outlast an ordinary `O(max_frames)` mutation by a live holder.
pub const TOPO_LOCK_SPIN_LIMIT: u32 = 1024;

/// Why the topology mutation lock could not be taken. `Copy`, names the
/// participant (`docs/PROJECT.md` §5).
///
/// Deliberately **not** `#[non_exhaustive]`: its only consumer is
/// `impl From<TopoLockError> for tf_tree::ReparentError`, which needs the payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopoLockError {
    /// Held by a live participant, or a third participant won the steal. Not a
    /// fault: the caller retries.
    Contended {
        /// Participant slot of the holder observed when the attempt gave up.
        owner_slot: u32,
    },
}

/// The in-arena topology mutation lock (`docs/PHASE2.md` §1, A2), a borrowed view
/// over `tf_tree_arena::TopoLock`'s two fields so tests can drive it over heap atomics.
///
/// # Liveness is injected
///
/// [`Self::acquire`] takes `is_alive: &dyn Fn(u32) -> bool` over a participant
/// slot. This `no_std` crate cannot answer it (`docs/PHASE2.md` §2, §5.1; the
/// interim predicate is in the `tf_tree` facade, §6.2). The predicate must **fail
/// safe**: when it cannot tell it returns `true`, because a false negative steals
/// from a live mutator and a false positive only costs a retry.
pub struct TopoLockView<'a> {
    owner: &'a AtomicU64,
    acquired_at_nanos: &'a AtomicI64,
}

/// Proof that this participant holds the topology mutation lock. Released on
/// drop, but only if still the holder (a stealer may have taken it).
#[derive(Debug)]
pub struct TopoGuard<'a> {
    owner: &'a AtomicU64,
    /// The owner word this guard installed: `participant_slot + 1`.
    want: u64,
}

// The release CAS compares only `participant_slot + 1`, which suffices because a
// slot never holds two guards at once: `Tree::reparent` takes a process-local
// mutex first. If that mutex goes, this needs a per-acquisition token.
impl TopoGuard<'_> {
    /// The participant slot this guard holds the lock on behalf of.
    #[inline]
    #[must_use]
    pub fn participant_slot(&self) -> u32 {
        slot_of(self.want)
    }
}

impl Drop for TopoGuard<'_> {
    fn drop(&mut self) {
        // CAS, not store: after a steal a plain store would clear the stealer's
        // lock. Release publishes the block stores; the failure path reads nothing.
        let _ = self
            .owner
            .compare_exchange(self.want, 0, Ordering::Release, Ordering::Relaxed);
    }
}

/// The participant slot encoded in an owner word (`slot + 1`, `0` = free).
#[inline]
fn slot_of(word: u64) -> u32 {
    word.saturating_sub(1) as u32
}

impl<'a> TopoLockView<'a> {
    /// Wrap the header's lock fields.
    #[must_use]
    pub fn new(owner: &'a AtomicU64, acquired_at_nanos: &'a AtomicI64) -> TopoLockView<'a> {
        TopoLockView {
            owner,
            acquired_at_nanos,
        }
    }

    /// The participant slot currently holding the lock, or `None` if it is free.
    ///
    /// Diagnostics only — it is stale the instant it returns.
    #[must_use]
    pub fn holder(&self) -> Option<u32> {
        match self.owner.load(Ordering::Acquire) {
            0 => None,
            w => Some(slot_of(w)),
        }
    }

    /// Take the lock on behalf of `participant_slot`, stealing it from a dead
    /// holder if the bounded spin runs out.
    ///
    /// `is_alive` is whatever the caller can prove: `tf_tree::Tree::reparent`
    /// takes a kernel lock first where a lock file exists, and on a tree without
    /// one the predicate is the whole answer (`docs/decisions/0029`). The steal
    /// acts on `false` and never manufactures it.
    ///
    /// `now_nanos` is stamped for `doctor` *after* the ownership CAS (a second
    /// store is a second crash window) and never triggers reaping (§6.4).
    ///
    /// # Stealing needs no rollback
    ///
    /// A holder killed inside the critical section has only scribbled on an
    /// **inactive** block. [`TopologyView::set_parent`] re-derives `next` from the
    /// active block and copies it over the scratch one wholesale, so recovery is a
    /// no-op (crash point `topo.holding_lock`, §11.3).
    ///
    /// # Errors
    ///
    /// [`TopoLockError::Contended`] if the holder is alive or a third participant
    /// won the steal; the caller retries.
    pub fn acquire(
        &self,
        participant_slot: u32,
        now_nanos: i64,
        is_alive: &dyn Fn(u32) -> bool,
    ) -> Result<TopoGuard<'a>, TopoLockError> {
        let want = u64::from(participant_slot) + 1;

        // 1. Bounded spin on the CAS; AcqRel pairs with `TopoGuard::drop`'s Release.
        for _ in 0..TOPO_LOCK_SPIN_LIMIT {
            if self
                .owner
                .compare_exchange(0, want, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.acquired_at_nanos.store(now_nanos, Ordering::Relaxed);
                return Ok(TopoGuard {
                    owner: self.owner,
                    want,
                });
            }
            spin();
        }

        // 2. Patience exhausted: the holder is slow or dead.
        let held = self.owner.load(Ordering::Acquire);
        if held == 0 {
            // Freed since the last spin: still the ordinary path.
            return self.finish(want, now_nanos);
        }

        let owner_slot = slot_of(held);
        if owner_slot == participant_slot {
            // Our own slot: another thread of this process, as alive as we are.
            return Err(TopoLockError::Contended { owner_slot });
        }
        if is_alive(owner_slot) {
            return Err(TopoLockError::Contended { owner_slot });
        }

        // 3. Steal, by CAS on the *observed* word: only one rescuer may win.
        match self
            .owner
            .compare_exchange(held, want, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                self.acquired_at_nanos.store(now_nanos, Ordering::Relaxed);
                Ok(TopoGuard {
                    owner: self.owner,
                    want,
                })
            }
            Err(_) => Err(TopoLockError::Contended { owner_slot }),
        }
    }

    /// One last ordinary attempt on a lock observed free.
    fn finish(&self, want: u64, now_nanos: i64) -> Result<TopoGuard<'a>, TopoLockError> {
        match self
            .owner
            .compare_exchange(0, want, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                self.acquired_at_nanos.store(now_nanos, Ordering::Relaxed);
                Ok(TopoGuard {
                    owner: self.owner,
                    want,
                })
            }
            Err(cur) => Err(TopoLockError::Contended {
                // `slot_of(0)` is 0, so name `u32::MAX` rather than slot 0 for a freed lock.
                owner_slot: if cur == 0 { u32::MAX } else { slot_of(cur) },
            }),
        }
    }
}

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

    /// The current topology generation. Every published generation is stable.
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        unpack_topo(self.topo.load(Ordering::Acquire)).0
    }

    /// Exactly [`Self::generation`].
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
    /// # The caller must hold the mutation lock
    ///
    /// Nothing here enforces it: callers hold [`TopoLockView::acquire`]'s guard, or
    /// hold the arena exclusively during construction. Being *stolen from* mid-call
    /// is survivable: every step targets an inactive block or is the publishing store.
    ///
    /// Aborts without flipping if the mutation would create a cycle.
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

        // The lock's AcqRel acquire pairs with the previous holder's Release.
        let word = self.topo.load(Ordering::Relaxed);
        let (g, active) = unpack_topo(word);
        let active = active as usize % TOPO_BLOCKS;
        let next = (active + 1) % TOPO_BLOCKS;
        let src = &self.blocks[active];
        let dst = &self.blocks[next];

        // Copy the active block over the scratch one unconditionally. This is the
        // recovery for a stolen lock: whatever a dead holder left in `dst` is
        // overwritten (`recompute_depths` does the same for `depth`; index 0 is
        // never addressable).
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
            // Nothing to undo: the active block was never touched; the dirty
            // scratch block is overwritten by the next mutation.
            return Err(TopologyError::WouldCreateCycle { child });
        }

        recompute_depths(dst, mf);

        // §11.3 `topo.after_copy_before_publish`: scratch dirty, word unchanged, no
        // observable effect (A1). Placed before the fence, which orders the block
        // stores against the publishing store only.
        crash_point!("topo.after_copy_before_publish");

        // Publish: the Release fence orders the block stores before the single store.
        fence(Ordering::Release);
        self.topo
            .store(pack_topo(g + 1, next as u8), Ordering::Release);
        Ok(())
    }

    /// Read `child`'s `(parent, depth, edge_of_child)` plus the generation it was
    /// read at.
    ///
    /// Wait-free: re-reads only if the whole topology word changed mid-read.
    ///
    /// `None` means `child` is out of range, or, after `TOPO_RETRY_LIMIT`
    /// attempts, that no consistent snapshot could be taken; both mean "do not use
    /// this frame".
    #[must_use]
    pub fn read_frame(&self, child: FrameId) -> Option<(u32, u16, u32, u64)> {
        if child.get() >= self.max_frames {
            return None;
        }
        let c = child.get() as usize;
        for _ in 0..TOPO_RETRY_LIMIT {
            let w1 = self.topo.load(Ordering::Acquire);
            let (g1, active) = unpack_topo(w1);
            // Bounds-check the index: a scribbled word must not index past the blocks.
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

#[cfg(all(test, not(loom)))]
mod lock_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use alloc::vec::Vec;

    /// The fail-safe predicate (`docs/PHASE2.md` §6.2).
    fn all_alive(_slot: u32) -> bool {
        true
    }

    /// Stands in for "the holder's lock byte is free" (§5.1).
    fn all_dead(_slot: u32) -> bool {
        false
    }

    /// Heap-backed lock words: the real [`TopoLockView`] protocol without an arena.
    struct HeapLock {
        owner: AtomicU64,
        acquired_at_nanos: AtomicI64,
    }

    impl HeapLock {
        fn new() -> HeapLock {
            HeapLock {
                owner: AtomicU64::new(0),
                acquired_at_nanos: AtomicI64::new(0),
            }
        }

        fn view(&self) -> TopoLockView<'_> {
            TopoLockView::new(&self.owner, &self.acquired_at_nanos)
        }
    }

    /// [`TOPO_BLOCKS`] heap blocks plus the packed word: the real [`TopologyView`] without an arena.
    struct HeapTopo {
        word: AtomicU64,
        parent: Vec<Vec<AtomicU32>>,
        edge_of_child: Vec<Vec<AtomicU32>>,
        depth: Vec<Vec<AtomicU16>>,
        max_frames: u32,
    }

    impl HeapTopo {
        fn new(max_frames: u32) -> HeapTopo {
            let mk32 = || (0..max_frames).map(|_| AtomicU32::new(0)).collect();
            let mk16 = || (0..max_frames).map(|_| AtomicU16::new(0)).collect();
            HeapTopo {
                word: AtomicU64::new(pack_topo(0, 0)),
                parent: (0..TOPO_BLOCKS).map(|_| mk32()).collect(),
                edge_of_child: (0..TOPO_BLOCKS).map(|_| mk32()).collect(),
                depth: (0..TOPO_BLOCKS).map(|_| mk16()).collect(),
                max_frames,
            }
        }

        fn view(&self) -> TopologyView<'_> {
            TopologyView::new(
                &self.word,
                core::array::from_fn(|i| Block {
                    parent: &self.parent[i],
                    edge_of_child: &self.edge_of_child[i],
                    depth: &self.depth[i],
                }),
                self.max_frames,
            )
        }

        /// The scratch block index the next mutation would use.
        fn scratch(&self) -> usize {
            let (_, active) = unpack_topo(self.word.load(Ordering::Relaxed));
            (active as usize % TOPO_BLOCKS + 1) % TOPO_BLOCKS
        }
    }

    fn fid(n: u32) -> FrameId {
        FrameId::new(n).unwrap()
    }

    /// The uncontended path; the guard frees the word.
    #[test]
    fn acquire_is_exclusive_and_the_guard_releases() {
        let lock = HeapLock::new();
        let v = lock.view();
        assert_eq!(v.holder(), None);

        let g = v.acquire(3, 100, &all_alive).unwrap();
        assert_eq!(g.participant_slot(), 3);
        assert_eq!(v.holder(), Some(3));
        assert_eq!(lock.acquired_at_nanos.load(Ordering::Relaxed), 100);

        // A live peer holds it; the error names the slot.
        assert_eq!(
            v.acquire(4, 200, &all_alive).unwrap_err(),
            TopoLockError::Contended { owner_slot: 3 }
        );

        drop(g);
        assert_eq!(v.holder(), None);
        let g2 = v.acquire(4, 300, &all_alive).unwrap();
        assert_eq!(g2.participant_slot(), 4);
    }

    /// A live holder is never stolen from.
    #[test]
    fn a_live_holder_is_not_stolen_from() {
        let lock = HeapLock::new();
        let v = lock.view();
        let held = v.acquire(0, 0, &all_alive).unwrap();

        assert_eq!(
            v.acquire(1, 0, &all_alive).unwrap_err(),
            TopoLockError::Contended { owner_slot: 0 }
        );
        assert_eq!(v.holder(), Some(0), "a live holder lost its lock");
        drop(held);
    }

    /// Our own slot is never stolen from, whatever the predicate says.
    #[test]
    fn a_participant_never_steals_from_its_own_slot() {
        let lock = HeapLock::new();
        let v = lock.view();
        let held = v.acquire(7, 0, &all_alive).unwrap();

        assert_eq!(
            v.acquire(7, 0, &all_dead).unwrap_err(),
            TopoLockError::Contended { owner_slot: 7 }
        );
        assert_eq!(v.holder(), Some(7));
        drop(held);
    }

    /// The `topo.holding_lock` crash point (`docs/PHASE2.md` §11.3): a holder
    /// dies mid-mutation; a stealer completes its own and leaves no trace of it.
    #[test]
    fn a_dead_holder_is_stolen_from_and_leaves_no_trace() {
        const MF: u32 = 8;
        let topo = HeapTopo::new(MF);
        let lock = HeapLock::new();
        let tv = topo.view();
        let lv = lock.view();

        // A published baseline: frame 1 under frame 2.
        {
            let _g = lv.acquire(0, 0, &all_alive).unwrap();
            tv.set_parent(fid(1), 2, 11).unwrap();
        }
        assert_eq!(tv.generation(), 1);

        // Participant 1 dirties the inactive block and dies (`forget`: no `Drop`).
        {
            let g = lv.acquire(1, 0, &all_alive).unwrap();
            let scratch = topo.scratch();
            topo.parent[scratch][3].store(0xDEAD, Ordering::Relaxed);
            topo.edge_of_child[scratch][3].store(0xBEEF, Ordering::Relaxed);
            topo.depth[scratch][3].store(99, Ordering::Relaxed);
            topo.parent[scratch][5].store(0xDEAD, Ordering::Relaxed);
            core::mem::forget(g);
        }
        assert_eq!(lv.holder(), Some(1), "the dead holder still holds the lock");
        assert_eq!(
            tv.generation(),
            1,
            "a mutation that never published must not bump the generation"
        );

        // Participant 2 finds the holder dead and steals.
        let stolen = lv.acquire(2, 500, &all_dead).unwrap();
        assert_eq!(stolen.participant_slot(), 2);
        assert_eq!(lock.acquired_at_nanos.load(Ordering::Relaxed), 500);

        // No rollback: `set_parent` re-copies the active block wholesale.
        tv.set_parent(fid(3), 1, 33).unwrap();
        drop(stolen);
        assert_eq!(lv.holder(), None);

        assert_eq!(tv.generation(), 2);
        // The stealer's own mutation.
        assert_eq!(tv.read_frame(fid(3)).unwrap(), (1, 2, 33, 2));
        // The baseline survived it.
        assert_eq!(tv.read_frame(fid(1)).unwrap(), (2, 1, 11, 2));
        // And every byte of the dead holder's scribble is gone.
        assert_eq!(
            tv.read_frame(fid(5)).unwrap(),
            (0, 0, 0, 2),
            "a stolen lock left the dead holder's garbage behind"
        );
    }

    /// A stolen-from participant must not free the thief's lock on release.
    #[test]
    fn releasing_after_being_stolen_from_is_a_no_op() {
        let lock = HeapLock::new();
        let v = lock.view();
        let victim = v.acquire(1, 0, &all_alive).unwrap();
        let thief = v.acquire(2, 0, &all_dead).unwrap();
        assert_eq!(v.holder(), Some(2));

        drop(victim);
        assert_eq!(
            v.holder(),
            Some(2),
            "a stale release freed a lock somebody else holds"
        );
        drop(thief);
        assert_eq!(v.holder(), None);
    }

    /// Two threads with distinct slots hammer the real lock and `set_parent`;
    /// every published generation must be accounted for.
    #[test]
    fn concurrent_mutators_are_serialized_by_the_arena_lock() {
        const MF: u32 = 8;
        const ROUNDS: u32 = 40;
        let topo = HeapTopo::new(MF);
        let lock = HeapLock::new();

        std::thread::scope(|s| {
            for (slot, child) in [(0u32, 1u32), (1, 2)] {
                let topo = &topo;
                let lock = &lock;
                s.spawn(move || {
                    let tv = topo.view();
                    let lv = lock.view();
                    for r in 0..ROUNDS {
                        // Retry on contention.
                        loop {
                            match lv.acquire(slot, i64::from(r), &all_alive) {
                                Ok(_g) => {
                                    tv.set_parent(fid(child), 0, child * 10).unwrap();
                                    break;
                                }
                                Err(TopoLockError::Contended { .. }) => core::hint::spin_loop(),
                            }
                        }
                    }
                });
            }
        });

        let tv = topo.view();
        // A lost mutation would mean two writers shared a scratch block.
        assert_eq!(tv.generation(), u64::from(2 * ROUNDS));
        assert_eq!(tv.read_frame(fid(1)).unwrap().0, 0);
        assert_eq!(tv.read_frame(fid(1)).unwrap().2, 10);
        assert_eq!(tv.read_frame(fid(2)).unwrap().2, 20);
    }
}
