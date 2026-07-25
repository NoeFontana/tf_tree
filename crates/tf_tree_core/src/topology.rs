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
//! # The mutation lock lives in the arena (`docs/PHASE2.md` §1, A2)
//!
//! [`TopoLockView`] is the writer side of that. Phase 1 serialized mutations
//! with a Rust `Mutex`, which is per-process and therefore serializes **nothing**
//! against a peer that mapped the same segment. The lock word lives in the
//! header (`tf_tree_arena::TopoLock`), so every participant contends on the same
//! bytes, and it is *reapable*: the spin is bounded, and an acquirer that runs
//! out of patience asks whether the holder is still alive before stealing it.
//!
//! **Stealing needs no rollback**, and that is the entire payoff for A1. See
//! [`TopoLockView::acquire`].
//!
//! This module is `#[cfg(not(loom))]`: `depth` is an `AtomicU16` which the loom
//! build does not model. The topology loom test reimplements the identical
//! generation/active/lock protocol with wider atoms.

use tf_tree_arena::{pack_topo, unpack_topo, TOPO_BLOCKS};

use crate::error::{FrameId, TopologyError};
use crate::sync::{fence, spin, AtomicI64, AtomicU16, AtomicU32, AtomicU64, Ordering};

/// How many times a reader re-reads a topology block before giving up.
///
/// A reader is only disturbed if the writer advances the active index all the
/// way around while the reader is between its first and last field load — with
/// [`TOPO_BLOCKS`] = 4 that takes four mutations inside a three-load window.
/// Topology mutations happen a few hundred times per process lifetime, so this
/// bound is a safety net, not a tuning knob.
const TOPO_RETRY_LIMIT: u32 = 64;

/// How many times an acquirer re-tries the lock CAS before it stops waiting and
/// asks whether the holder is still alive.
///
/// The bound is the point (`docs/PHASE2.md` §1, A2). An unbounded wait is the
/// A1/A8 defect in a different costume: a participant `SIGKILL`ed while holding
/// the lock would wedge every other mutator in every other process forever.
/// The value only has to be long enough that a *live* holder finishing an
/// ordinary mutation is not mistaken for a dead one — a mutation is an
/// `O(max_frames)` block copy, and the liveness check that follows the spin is
/// itself the real arbiter, so this is a patience knob, not a timeout.
pub const TOPO_LOCK_SPIN_LIMIT: u32 = 1024;

/// Why the topology mutation lock could not be taken.
///
/// `Copy`, and it names the offending participant rather than allocating a
/// message (`docs/PROJECT.md` §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopoLockError {
    /// The lock is held by a participant that the liveness predicate says is
    /// still running, or a third participant won the steal. Neither is a fault:
    /// the caller retries.
    Contended {
        /// Participant slot of the holder observed when the attempt gave up.
        owner_slot: u32,
    },
}

/// The in-arena topology mutation lock (`docs/PHASE2.md` §1, A2).
///
/// A borrowed view over `tf_tree_arena::TopoLock`'s two fields rather than over
/// the struct itself, so the protocol can be driven over plain heap atomics in a
/// test without constructing an arena.
///
/// # Liveness is injected, never decided here
///
/// [`Self::acquire`] takes `is_alive: &dyn Fn(u32) -> bool` — given a
/// *participant slot*, is that participant still running? This module
/// deliberately does not know the answer and must not learn it:
///
/// * `docs/PHASE2.md` §5.1 makes the **OFD lock file** the authoritative
///   liveness source, and §6.1 makes it a kernel fact rather than a heuristic.
///   That machinery is not in this crate — `tf_tree_core` is `no_std` and its
///   dependency budget forbids the syscall layer that would answer the question
///   (§2: "`tf_tree_core` gaining a dependency in this phase is a design
///   failure").
/// * The interim predicate, `/proc/<pid>/stat` field 22 against the participant
///   record's `start_time` (§6.2), lives in the `tf_tree` facade. Swapping it
///   for the lock file is a change to *that* function and to nothing here.
///
/// The predicate must **fail safe**: when it cannot tell, it returns `true`.
/// A false negative steals the lock from a live mutator, which is the one way
/// two writers can race the block copy; a false positive only makes this
/// acquirer retry. The asymmetry is enormous (§6.2).
pub struct TopoLockView<'a> {
    owner: &'a AtomicU64,
    acquired_at_nanos: &'a AtomicI64,
}

/// Proof that this participant holds the topology mutation lock.
///
/// Released on drop, but only if this participant is *still* the holder: a
/// stealer may have taken it in the meantime (which is legal — see
/// [`TopoLockView::acquire`]), and clearing somebody else's lock would hand a
/// third mutator a concurrent block copy.
#[derive(Debug)]
pub struct TopoGuard<'a> {
    owner: &'a AtomicU64,
    /// The owner word this guard installed: `participant_slot + 1`.
    want: u64,
}

// The release CAS below compares only the owner word, which is
// `participant_slot + 1` — constant per participant, not per acquisition. That
// is enough here because a slot can never hold two guards at once: distinct
// processes have distinct slots, and `Tree::reparent` takes a process-local
// mutex before the arena lock, so two threads of one process cannot both be in
// the critical section. If that mutex is ever removed, this CAS needs a
// per-acquisition token (e.g. a generation packed into the owner word) or a
// stale guard could free a live holder's lock.
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
        // A plain store would be wrong. If this participant was judged dead and
        // its lock stolen — because it was `SIGSTOP`ped, or the predicate was
        // conservative — a store here clears a lock the *stealer* holds, and the
        // next acquirer joins it mid-copy. The CAS makes a stale release a
        // no-op.
        //
        // Release publishes the block stores and the topology word to whoever
        // acquires next; the failure path observes nothing, so Relaxed.
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
    /// `now_nanos` is stamped into the lock for `doctor`'s staleness report. It
    /// is written *after* the CAS that publishes ownership, because the CAS is
    /// what makes the claim atomic (A3's lesson: a second store is a second
    /// crash window), and it is never a reaping trigger on its own (§6.4).
    ///
    /// # Stealing needs no rollback — the payoff for A1
    ///
    /// A holder killed at any instruction inside the critical section has done
    /// exactly one kind of damage: it scribbled on an **inactive** topology
    /// block. It cannot have done anything else, because A1 removed the odd
    /// generation and the active block is never mutated in place, so the only
    /// state a reader can observe is the packed topology word — and that word is
    /// written by a *single store* which either happened or did not.
    ///
    /// So the stealer has nothing to undo. [`TopologyView::set_parent`] re-reads
    /// the word, re-derives `next` from the currently-active block, and copies
    /// that block over the scratch one **wholesale** before applying its own
    /// mutation. Whatever the dead holder left there is overwritten, not merged.
    /// Recovery is a no-op; that is why this lock is safe to steal at all, and
    /// it is the crash point `topo.holding_lock` in §11.3.
    ///
    /// # Errors
    ///
    /// [`TopoLockError::Contended`] if the holder is alive, or if a third
    /// participant won the steal. Both mean "try again", and neither leaves any
    /// state behind.
    pub fn acquire(
        &self,
        participant_slot: u32,
        now_nanos: i64,
        is_alive: &dyn Fn(u32) -> bool,
    ) -> Result<TopoGuard<'a>, TopoLockError> {
        let want = u64::from(participant_slot) + 1;

        // 1. The ordinary path: a bounded spin on the uncontended CAS.
        //    AcqRel on success pairs with the previous holder's Release in
        //    `TopoGuard::drop`, so this participant sees every block store and
        //    the topology word it published. That pairing is what lets
        //    `set_parent` read the topology word Relaxed.
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

        // 2. Patience exhausted. Either the holder is genuinely slow, or it is
        //    dead and nothing will ever release this word.
        let held = self.owner.load(Ordering::Acquire);
        if held == 0 {
            // Freed between the last spin and now: this is still the ordinary
            // path, not a steal, so no liveness question arises.
            return self.finish(want, now_nanos);
        }

        let owner_slot = slot_of(held);
        if owner_slot == participant_slot {
            // Our own slot. Another thread of this process holds it; it is by
            // definition as alive as we are, and stealing from ourselves would
            // put two threads in the same critical section. The facade also
            // serializes its own threads, so this is belt and braces.
            return Err(TopoLockError::Contended { owner_slot });
        }
        if is_alive(owner_slot) {
            return Err(TopoLockError::Contended { owner_slot });
        }

        // 3. Steal. See the section above: there is nothing to repair, because
        //    the dead holder can only have dirtied an inactive block, and the
        //    next `set_parent` re-copies the active block over it.
        //
        //    CAS on the *observed* word rather than a blind store: two rescuers
        //    can reach this point together and only one may win, or the holder
        //    may have proved us wrong by releasing it. Either way the loser
        //    retries and neither corrupts anything.
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
                // `slot_of(0)` is 0 because of the `saturating_sub`, so a lock
                // that was *freed* between the load and this CAS would report
                // "held by slot 0" and name whichever process holds that slot —
                // the same plausible-looking wrong answer `doctor` was fixed for.
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
    /// # The caller must hold the mutation lock
    ///
    /// This is the single writer of the topology protocol, and nothing in this
    /// function enforces that. Callers hold [`TopoLockView::acquire`]'s guard for
    /// its duration — or, during construction, hold the arena exclusively
    /// (`ArenaBuilder` takes `&mut`, so no other participant can exist yet).
    ///
    /// It is written so that being *stolen from* mid-call is survivable: every
    /// step below either targets an inactive block or is the single publishing
    /// store. A stealer re-enters here and re-copies the active block wholesale,
    /// so it inherits no state from the holder it replaced.
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

        // The caller holds the in-arena mutation lock (A2), whose acquiring CAS
        // is AcqRel and pairs with the previous holder's Release — so this load
        // races nothing and needs no acquire of its own.
        let word = self.topo.load(Ordering::Relaxed);
        let (g, active) = unpack_topo(word);
        let active = active as usize % TOPO_BLOCKS;
        let next = (active + 1) % TOPO_BLOCKS;
        let src = &self.blocks[active];
        let dst = &self.blocks[next];

        // Copy the active block into the inactive one, then apply the mutation.
        // parent and edge_of_child move together so the snapshot stays consistent.
        //
        // **This loop is the whole recovery story for a stolen lock.** It writes
        // every index unconditionally, so whatever a holder that died here left
        // in `dst` is overwritten rather than inherited — and `recompute_depths`
        // below does the same for `depth` over every frame a `FrameId` can name
        // (index 0 is the reserved root slot, which nothing ever writes and no
        // reader can address: `FrameId` is non-zero). `next` is derived from the *current*
        // active index, which the dead holder never advanced (that store is the
        // last thing it would have done), so the stealer lands on the same
        // scratch block and erases it. No rollback, no repair, no bookkeeping.
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

#[cfg(all(test, not(loom)))]
mod lock_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use alloc::vec::Vec;

    /// Every participant is alive. This is the **fail-safe** answer, and what
    /// the facade's predicate returns whenever it cannot tell
    /// (`docs/PHASE2.md` §6.2).
    fn all_alive(_slot: u32) -> bool {
        true
    }

    /// Nobody is alive — the injected predicate standing in for "the holder's
    /// lock byte is free", which §5.1 makes the authoritative answer.
    fn all_dead(_slot: u32) -> bool {
        false
    }

    /// A heap-backed lock word pair, so the real [`TopoLockView`] protocol runs
    /// without an arena. The production fields are these exact types.
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

    /// [`TOPO_BLOCKS`] heap topology blocks plus the packed word, so the real
    /// [`TopologyView`] protocol runs without an arena.
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

        /// The block index a mutation would use as scratch right now.
        fn scratch(&self) -> usize {
            let (_, active) = unpack_topo(self.word.load(Ordering::Relaxed));
            (active as usize % TOPO_BLOCKS + 1) % TOPO_BLOCKS
        }
    }

    fn fid(n: u32) -> FrameId {
        FrameId::new(n).unwrap()
    }

    /// The uncontended path, and that the guard actually frees the word.
    #[test]
    fn acquire_is_exclusive_and_the_guard_releases() {
        let lock = HeapLock::new();
        let v = lock.view();
        assert_eq!(v.holder(), None);

        let g = v.acquire(3, 100, &all_alive).unwrap();
        assert_eq!(g.participant_slot(), 3);
        assert_eq!(v.holder(), Some(3));
        assert_eq!(lock.acquired_at_nanos.load(Ordering::Relaxed), 100);

        // A second participant finds it held by a live peer, and is told which
        // one by slot rather than by an allocated message.
        assert_eq!(
            v.acquire(4, 200, &all_alive).unwrap_err(),
            TopoLockError::Contended { owner_slot: 3 }
        );

        drop(g);
        assert_eq!(v.holder(), None);
        let g2 = v.acquire(4, 300, &all_alive).unwrap();
        assert_eq!(g2.participant_slot(), 4);
    }

    /// A live holder is never stolen from, however long the spin runs.
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

    /// A thread of *this* process holding the lock is never stolen from either,
    /// whatever the predicate says: the owner slot is ours, so the holder is
    /// exactly as alive as the caller asking the question.
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

    /// The `topo.holding_lock` crash point (`docs/PHASE2.md` §11.3), end to end
    /// over the real `set_parent`.
    ///
    /// A holder is killed mid-mutation: it has taken the lock and scribbled on
    /// the inactive block, and it will never release either. A second mutator
    /// must steal the lock, complete its own mutation, and leave **no trace** of
    /// the first — which is A2's claim that recovery is a no-op, tested rather
    /// than asserted.
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

        // Participant 1 takes the lock, dirties the *inactive* block the way a
        // half-finished copy would, and dies. `forget` is the crash: no release,
        // no unwinding, no `Drop`.
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

        // Participant 2 finds it held, learns the holder is dead, and steals.
        let stolen = lv.acquire(2, 500, &all_dead).unwrap();
        assert_eq!(stolen.participant_slot(), 2);
        assert_eq!(lock.acquired_at_nanos.load(Ordering::Relaxed), 500);

        // No rollback was performed, and none is needed: `set_parent` re-copies
        // the active block over the scratch one wholesale.
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

    /// A participant that was stolen from must not free the thief's lock when it
    /// finally gets around to releasing.
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

    /// Two threads with *distinct* participant slots hammering the real lock and
    /// the real `set_parent`. Nothing else serializes them, so every published
    /// generation must be accounted for and the final topology must be exactly
    /// what both writers left.
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
                        // Retry on contention, exactly as a caller must.
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
        // Every one of the `2 * ROUNDS` mutations published exactly once. A lost
        // one means two writers shared a scratch block; an extra one is
        // impossible.
        assert_eq!(tv.generation(), u64::from(2 * ROUNDS));
        assert_eq!(tv.read_frame(fid(1)).unwrap().0, 0);
        assert_eq!(tv.read_frame(fid(1)).unwrap().2, 10);
        assert_eq!(tv.read_frame(fid(2)).unwrap().2, 20);
    }
}
