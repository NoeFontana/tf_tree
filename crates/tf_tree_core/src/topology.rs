//! Multi-buffered topology blocks and the single store that publishes mutations.
//!
//! `unsafe`-free: the atomic slices come from [`crate::arena_view`]. A mutation
//! is applied to an *inactive* one of the [`TOPO_BLOCKS`] blocks, depths are
//! recomputed, then the active index advances — so a reader sees the old
//! topology or the new one, never a mix (`docs/PHASE1.md` §5.2). **`ArcSwap` is
//! forbidden here** (`docs/PROJECT.md` §5 D4): `Arc` refcounts do not cross a
//! process boundary. `#[cfg(not(loom))]` because `depth` is an `AtomicU16`,
//! which loom does not model; the topology loom test reimplements this protocol
//! with wider atoms.
//!
//! **No odd state** (`docs/PHASE2.md` §1, A1): a seqlock's odd generation is
//! permanent if its writer is `SIGKILL`ed mid-bump, wedging every reader in
//! every process. Publication is one store of `pack(generation + 1, next)`, so
//! a killed writer leaves the arena as if it never started — which is what
//! makes the A2 lock stealable with no rollback.
//!
//! **Layout** (§5.2 against §4.3's nominal 6-byte stride): **12 bytes per
//! frame**, `align64(max_frames * 12)` — `parent: u32`, `edge_of_child: u32`,
//! `depth: u16`, 2 bytes padding, the `u32`s first so both stay 4-byte aligned
//! for any `max_frames`. `edge_of_child` lives here so plan compilation is an
//! O(1) walk over a double-buffered triple.
//!
//! **The mutation lock is in the arena** (§1, A2; [`TopoLockView`]) because a
//! Rust `Mutex` serializes nothing against a peer that mapped the same segment.
//! It is *reapable*: bounded spin, then a liveness question before stealing.

use tf_tree_arena::{pack_topo, unpack_topo, TOPO_BLOCKS};

use crate::crash::crash_point;
use crate::error::{FrameId, TopologyError};
use crate::sync::{fence, spin, AtomicI64, AtomicU16, AtomicU32, AtomicU64, Ordering};

/// How many times a reader re-reads a topology block before giving up.
///
/// A safety net, not a tuning knob: only a writer advancing the active index
/// all the way around ([`TOPO_BLOCKS`] = 4 mutations) inside a three-load
/// window disturbs a reader, and mutations run a few hundred times per process.
const TOPO_RETRY_LIMIT: u32 = 64;

/// How many times an acquirer re-tries the lock CAS before it stops waiting and
/// asks whether the holder is still alive.
///
/// Bounded because an unbounded wait is the A1/A8 defect in a costume: one
/// `SIGKILL`ed holder would wedge every mutator in every process
/// (`docs/PHASE2.md` §1, A2). A patience knob, not a timeout — the liveness
/// check after the spin is the arbiter, so this need only outlast a live
/// holder's `O(max_frames)` block copy.
pub const TOPO_LOCK_SPIN_LIMIT: u32 = 1024;

/// Why the topology mutation lock could not be taken.
///
/// `Copy`, naming the participant rather than allocating a message
/// (`docs/PROJECT.md` §5).
/// Deliberately **not** `#[non_exhaustive]`, alone here: its only consumer,
/// `impl From<TopoLockError> for tf_tree::ReparentError`, would have no
/// `owner_slot` for a catch-all arm, and users see `ReparentError`, which is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopoLockError {
    /// The lock is held by a participant the liveness predicate says is still
    /// running, or a third participant won the steal. Neither is a fault.
    Contended {
        /// Participant slot of the holder observed when the attempt gave up.
        owner_slot: u32,
    },
}

/// The in-arena topology mutation lock (`docs/PHASE2.md` §1, A2).
///
/// A borrowed view over `tf_tree_arena::TopoLock`'s two fields rather than the
/// struct, so the protocol can run over plain heap atomics in a test.
///
/// Liveness is injected, never decided here: [`Self::acquire`] takes
/// `is_alive: &dyn Fn(u32) -> bool` over a *participant slot* because §5.1/§6.1
/// make the **OFD lock file** the authoritative kernel fact while this crate is
/// `no_std` with no syscall layer in its budget (§2); the interim
/// `/proc/<pid>/stat`-field-22 predicate (§6.2) lives in the `tf_tree` facade.
/// It must **fail safe** (`true` when unsure): a false negative steals from a
/// live mutator, the one way two writers can race the block copy; a false
/// positive only costs a retry.
pub struct TopoLockView<'a> {
    owner: &'a AtomicU64,
    acquired_at_nanos: &'a AtomicI64,
}

/// Proof that this participant holds the topology mutation lock.
///
/// Released on drop only if this participant is *still* the holder: a steal is
/// legal ([`TopoLockView::acquire`]), and clearing somebody else's lock would
/// hand a third mutator a concurrent block copy.
#[derive(Debug)]
pub struct TopoGuard<'a> {
    owner: &'a AtomicU64,
    /// The owner word this guard installed: `participant_slot + 1`.
    want: u64,
}

// The release CAS below compares only the owner word (`participant_slot + 1`,
// constant per participant, not per acquisition). Enough because a slot never
// holds two guards at once: processes have distinct slots and `Tree::reparent`
// takes a process-local mutex first. Remove that mutex and this CAS needs a
// per-acquisition token, or a stale guard could free a live holder's lock.
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
        // CAS, not a plain store: if this participant was judged dead and its
        // lock stolen, a store would clear the *stealer*'s lock and let the
        // next acquirer join it mid-copy. Release publishes the block stores
        // and the topology word to that acquirer; the failure path sees none.
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
    /// `is_alive` is a parameter because liveness needs a syscall this crate's
    /// budget cannot buy (`docs/PROJECT.md` §5 D14), and what fills it varies
    /// by tree (`docs/decisions/0029`): under `tf_tree::Tree::reparent`'s
    /// exclusive kernel lock it only separates a dead holder from a mutator
    /// with no lock file; with no lock file it is the whole answer. The steal
    /// is sound either way — it acts on `false`, never manufactures it.
    ///
    /// `now_nanos` is stamped in for `doctor`'s staleness report *after* the
    /// ownership CAS, which is what makes the claim atomic (A3: a second store
    /// is a second crash window); it never triggers reaping on its own (§6.4).
    ///
    /// **Stealing needs no rollback**, the payoff for A1: a killed holder can
    /// only have dirtied an *inactive* block, since the active one is never
    /// mutated in place and the topology word is a single store that either
    /// happened or did not. [`TopologyView::set_parent`] re-derives `next` and
    /// overwrites that scratch block **wholesale**; crash point
    /// `topo.holding_lock` (§11.3).
    ///
    /// # Errors
    ///
    /// [`TopoLockError::Contended`] if the holder is alive, or a third
    /// participant won the steal. Both mean "try again" and leave no state.
    pub fn acquire(
        &self,
        participant_slot: u32,
        now_nanos: i64,
        is_alive: &dyn Fn(u32) -> bool,
    ) -> Result<TopoGuard<'a>, TopoLockError> {
        let want = u64::from(participant_slot) + 1;

        // 1. Ordinary path. AcqRel pairs with the previous holder's Release in
        //    `TopoGuard::drop`, so we see every block store it published —
        //    which is what lets `set_parent` read the topology word Relaxed.
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

        // 2. Patience exhausted: a slow holder, or a dead one that never
        //    releases.
        let held = self.owner.load(Ordering::Acquire);
        if held == 0 {
            // Freed since the last spin: ordinary path, so no liveness
            // question arises.
            return self.finish(want, now_nanos);
        }

        let owner_slot = slot_of(held);
        if owner_slot == participant_slot {
            // Our own slot: as alive as we are, and stealing would put two
            // threads of this process in the same critical section.
            return Err(TopoLockError::Contended { owner_slot });
        }
        if is_alive(owner_slot) {
            return Err(TopoLockError::Contended { owner_slot });
        }

        // 3. Steal; nothing to repair (see above). CAS on the *observed* word,
        //    not a blind store: two rescuers may arrive together, or the holder
        //    may have proved us wrong by releasing. The loser just retries.
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
                // `slot_of(0)` saturates to 0, so a lock *freed* between the
                // load and this CAS would blame slot 0 — `doctor`'s old bug.
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
    /// Always stable (A1 removed the odd state): no parity check, no spin.
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        unpack_topo(self.topo.load(Ordering::Acquire)).0
    }

    /// The current generation.
    ///
    /// Exactly [`Self::generation`] since A1 left no unstable generation to
    /// wait for; kept as a name because pinning callers read better for it.
    #[inline]
    #[must_use]
    pub fn stable_generation(&self) -> u64 {
        self.generation()
    }

    /// Attach `child` under `parent` via edge `edge`, recompute depths, and
    /// publish atomically. Pass `edge == 0` when only the parent link matters.
    ///
    /// **The caller must hold the mutation lock**; nothing here enforces it.
    /// Callers hold [`TopoLockView::acquire`]'s guard, or at construction hold
    /// the arena exclusively (`ArenaBuilder` takes `&mut`). Being *stolen from*
    /// mid-call is survivable: every step targets an inactive block or is the
    /// single publishing store. Aborts without flipping on a cycle.
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

        // The caller holds the A2 lock, whose acquiring CAS is AcqRel and pairs
        // with the previous holder's Release, so this load races nothing.
        let word = self.topo.load(Ordering::Relaxed);
        let (g, active) = unpack_topo(word);
        let active = active as usize % TOPO_BLOCKS;
        let next = (active + 1) % TOPO_BLOCKS;
        let src = &self.blocks[active];
        let dst = &self.blocks[next];

        // Copy the active block into the inactive one, then apply the mutation;
        // parent and edge_of_child move together so the snapshot stays
        // consistent. **This loop is the recovery story for a stolen lock**: it
        // writes every index unconditionally, as `recompute_depths` does for
        // `depth`, and `next` derives from the *current* active index, which a
        // dead holder never advanced — so the stealer lands on that holder's
        // scratch block and erases it. Index 0 is the reserved root slot, which
        // `FrameId` being non-zero makes unaddressable.
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
            // Nothing to undo: the active block was never touched and the word
            // never stored, so the published topology is byte-identical to
            // entry. The dirty scratch block is harmless — the next mutation
            // overwrites it wholesale, as after a *crashed* writer.
            return Err(TopologyError::WouldCreateCycle { child });
        }

        recompute_depths(dst, mf);

        // §11.3 `topo.after_copy_before_publish`: "inactive block dirty, word
        // unchanged -> **no observable effect** (A1)". Before the publishing
        // store rather than before the fence, matching A1's writer pseudo-code,
        // since the fence orders the block stores against that store alone.
        crash_point!("topo.after_copy_before_publish");

        // Publish: the Release fence orders every block store against this one.
        fence(Ordering::Release);
        self.topo
            .store(pack_topo(g + 1, next as u8), Ordering::Release);
        Ok(())
    }

    /// Read `child`'s `(parent, depth, edge_of_child)` plus the generation it was
    /// read at.
    ///
    /// Wait-free: no writer state must be waited out, and it re-reads only if
    /// the *whole* topology word changed between the first and last field load
    /// — [`TOPO_BLOCKS`] mutations inside a three-load window. `None` means out
    /// of range (`FrameId` guarantees non-zero, not in-bounds) or, after
    /// `TOPO_RETRY_LIMIT` attempts, no consistent snapshot; both mean "do not
    /// use this frame".
    #[must_use]
    pub fn read_frame(&self, child: FrameId) -> Option<(u32, u16, u32, u64)> {
        if child.get() >= self.max_frames {
            return None;
        }
        let c = child.get() as usize;
        for _ in 0..TOPO_RETRY_LIMIT {
            let w1 = self.topo.load(Ordering::Acquire);
            let (g1, active) = unpack_topo(w1);
            // Bound the index out of the word: a torn one must not index past
            // the block array.
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

    /// Every participant is alive — the **fail-safe** answer (§6.2).
    fn all_alive(_slot: u32) -> bool {
        true
    }

    /// Nobody is alive — stands in for "the holder's lock byte is free" (§5.1).
    fn all_dead(_slot: u32) -> bool {
        false
    }

    /// The production field types on the heap: real [`TopoLockView`], no arena.
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

    /// [`TOPO_BLOCKS`] heap blocks plus the word: [`TopologyView`], no arena.
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

    /// The `topo.holding_lock` crash point (`docs/PHASE2.md` §11.3) end to end:
    /// a second mutator steals from a holder that died mid-mutation and must
    /// leave **no trace** of it — A2's "recovery is a no-op", tested.
    #[test]
    fn a_dead_holder_is_stolen_from_and_leaves_no_trace() {
        const MF: u32 = 8;
        let topo = HeapTopo::new(MF);
        let lock = HeapLock::new();
        let tv = topo.view();
        let lv = lock.view();

        // A published baseline.
        {
            let _g = lv.acquire(0, 0, &all_alive).unwrap();
            tv.set_parent(fid(1), 2, 11).unwrap();
        }
        assert_eq!(tv.generation(), 1);

        // Participant 1 dirties the *inactive* block like a half-finished copy
        // and dies; `forget` is the crash — no release, no unwinding.
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

        let stolen = lv.acquire(2, 500, &all_dead).unwrap();
        assert_eq!(stolen.participant_slot(), 2);
        assert_eq!(lock.acquired_at_nanos.load(Ordering::Relaxed), 500);

        // No rollback: `set_parent` re-copies the active block over scratch.
        tv.set_parent(fid(3), 1, 33).unwrap();
        drop(stolen);
        assert_eq!(lv.holder(), None);

        assert_eq!(tv.generation(), 2);
        assert_eq!(tv.read_frame(fid(3)).unwrap(), (1, 2, 33, 2));
        assert_eq!(tv.read_frame(fid(1)).unwrap(), (2, 1, 11, 2));
        assert_eq!(
            tv.read_frame(fid(5)).unwrap(),
            (0, 0, 0, 2),
            "a stolen lock left the dead holder's garbage behind"
        );
    }

    /// A participant stolen from must not free the thief's lock on release.
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

    /// Two threads with *distinct* slots on the real lock and `set_parent`,
    /// nothing else serializing them: every published generation must be
    /// accounted for and the final topology exactly what both writers left.
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
        // All `2 * ROUNDS` published once; a loss means a shared scratch block.
        assert_eq!(tv.generation(), u64::from(2 * ROUNDS));
        assert_eq!(tv.read_frame(fid(1)).unwrap().0, 0);
        assert_eq!(tv.read_frame(fid(1)).unwrap().2, 10);
        assert_eq!(tv.read_frame(fid(2)).unwrap().2, 20);
    }
}
