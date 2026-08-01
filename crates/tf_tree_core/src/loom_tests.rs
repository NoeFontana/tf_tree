//! `loom` model-checked concurrency tests (run under `--cfg loom`).
//!
//! These are the hard gate for step 5 of `docs/PHASE1.md`'s implementation
//! order, and for its §10.2 *Concurrency (loom)*: the publish/read/claim/
//! intern protocols must be sound under every interleaving loom explores, not
//! merely on x86. Buffers are capacity 4 and push counts <= 5 to keep the state
//! space tractable — small, but never so small that the code under test becomes
//! unreachable, which is a failure mode these models have already had once (see
//! [`writer_wraps_reader_gets_valid_or_recycled`]). Each test drives the
//! *shared* algorithm code (the same functions the production arena view calls)
//! over heap-allocated instances built from `crate::sync` (loom) atomics.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use loom::sync::Arc;
use loom::thread;

use tf_tree_math::{exp_se3, Iso3, LerpSlerp};

use crate::buffer::{PoseSlot, SampleRing};
use crate::edge::{claim, ClaimRecord};
use crate::error::{EdgeId, LookupError};
use crate::frame::{intern_core, InternTable, CLAIM_UNRECORDED};
use crate::participant::{ParticipantRecord, ParticipantTable};
use crate::sample::ExtrapPolicy;
use crate::sync::{AtomicI64, AtomicU32, AtomicU64, Ordering};

fn pose(seed: u64) -> Iso3 {
    let f = seed as f64;
    exp_se3([0.01 * f, 0.02 * f, -0.03 * f, 0.1 * f, 0.2 * f, -0.15 * f])
}

/// Heap sample ring built from loom atomics (mirrors the arena `SampleRing`).
struct HeapRing {
    head: AtomicU64,
    heartbeat: AtomicU64,
    stamps: alloc::vec::Vec<AtomicI64>,
    poses: alloc::vec::Vec<PoseSlot>,
    mask: u64,
}

impl HeapRing {
    fn new(capacity: usize) -> HeapRing {
        let mut stamps = alloc::vec::Vec::with_capacity(capacity);
        let mut poses = alloc::vec::Vec::with_capacity(capacity);
        for _ in 0..capacity {
            stamps.push(AtomicI64::new(0));
            poses.push(PoseSlot::new());
        }
        HeapRing {
            head: AtomicU64::new(0),
            heartbeat: AtomicU64::new(0),
            stamps,
            poses,
            mask: (capacity as u64) - 1,
        }
    }

    fn ring(&self) -> SampleRing<'_> {
        SampleRing {
            head: &self.head,
            heartbeat: &self.heartbeat,
            stamps: &self.stamps,
            poses: &self.poses,
            mask: self.mask,
            edge: EdgeId(0),
        }
    }
}

/// Loom test 1: one writer pushing 3 samples, one concurrent reader. The reader
/// observes a fully-consistent slot, never a torn one.
#[test]
fn writer_three_pushes_reader_never_torn() {
    loom::model(|| {
        let hr = Arc::new(HeapRing::new(4));
        let zero = Iso3::from_bits(&[0u64; 7]).to_bits();
        let p1 = pose(2).to_bits();

        let w = Arc::clone(&hr);
        let writer = thread::spawn(move || {
            let ring = w.ring();
            ring.push(10, &pose(1)).unwrap();
            ring.push(20, &pose(2)).unwrap();
            ring.push(30, &pose(3)).unwrap();
        });

        let r = Arc::clone(&hr);
        let reader = thread::spawn(move || {
            // Physical slot 1 is written exactly once (the second push, pose(2)),
            // so a consistent read is either the initial zero pose or pose(2) —
            // never a mix of the two.
            let ring = r.ring();
            match ring.read_slot(1) {
                Ok(iso) => {
                    let bits = iso.to_bits();
                    assert!(bits == zero || bits == p1, "torn read: {bits:?}");
                }
                Err(LookupError::SlotContended { .. }) => {}
                Err(other) => panic!("unexpected read error: {other:?}"),
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    });
}

/// Loom test 2 (`docs/PHASE1.md` §10.2, bullet 2): the writer **laps** the ring
/// while a reader samples. The reader returns the one interpolation the
/// published history permits, or a documented error — never a pose assembled
/// from two eras.
///
/// # Why capacity 4 and five pushes, and not capacity 2 and three
///
/// [`SampleRing::retained`] is `capacity - 1`, so a capacity-2 ring has a
/// readable window of exactly *one* sample. `sample()` then finds `t_old ==
/// t_new` and can only ever answer `NoData` or `Extrapolation`: the bracket
/// search, the interpolation and the trailing `head - i > retained`
/// revalidation — the entire subject of this test — are unreachable. That was
/// this test's shape until it was measured: a `panic!` planted in the `Ok` arm
/// never fired, across every interleaving loom explores. Five pushes into four
/// slots is the smallest configuration in which the ring genuinely laps a
/// reader *and* the reader has something to interpolate.
///
/// The assertion is bit equality against the single legal answer, not
/// finiteness. Finiteness was the old check and it proves nothing here: a pose
/// interpolated between two samples from different eras is perfectly finite.
/// Whether the writer has landed three, four or five pushes when the reader
/// looks, the only bracket containing `t = 25` is `(20, 30)` at `s = 0.5`, so
/// any other value the reader could return is a splice.
#[test]
fn writer_wraps_reader_gets_valid_or_recycled() {
    loom::model(|| {
        let hr = Arc::new(HeapRing::new(4));
        // The only legal `Ok`: stamps 20 and 30 bracket t = 25 at s = 0.5.
        let expect = <LerpSlerp as tf_tree_math::Interp>::eval(&pose(2), &pose(3), 0.5).to_bits();

        let w = Arc::clone(&hr);
        let writer = thread::spawn(move || {
            let ring = w.ring();
            // Stamps 10..50 into four slots: the fifth push laps slot 0.
            for i in 1..=5u64 {
                ring.push(i as i64 * 10, &pose(i)).unwrap();
            }
        });

        let r = Arc::clone(&hr);
        let reader = thread::spawn(move || {
            let ring = r.ring();
            match ring.sample::<LerpSlerp>(25, ExtrapPolicy::Error) {
                Ok(iso) => assert_eq!(
                    iso.to_bits(),
                    expect,
                    "reader composed a sample from two eras"
                ),
                Err(
                    LookupError::NoData { .. }
                    | LookupError::Extrapolation { .. }
                    | LookupError::SlotRecycled { .. }
                    | LookupError::SlotContended { .. },
                ) => {}
                Err(other) => panic!("undocumented error: {other:?}"),
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    });
}

/// The interning table's three parallel arrays plus its id allocator, on the heap
/// and built from loom atomics — the same shape `ArenaView` hands `intern_core`.
///
/// Every array is **zero-initialized, exactly like the production arena**
/// (`alloc_zeroed`). Seeding `ids` with a different "unpublished" sentinel is what
/// once let this model check pass while the real publish-then-spin handshake was
/// inert: nothing in the arena ever writes a non-zero unpublished marker.
struct HeapInternTable {
    hashes: alloc::vec::Vec<AtomicU64>,
    ids: alloc::vec::Vec<AtomicU32>,
    claiming: alloc::vec::Vec<AtomicU32>,
    count: AtomicU32,
}

impl HeapInternTable {
    /// `slots` must be a power of two (the mask is `slots - 1`).
    fn new(slots: usize) -> HeapInternTable {
        let mut hashes = alloc::vec::Vec::with_capacity(slots);
        let mut ids = alloc::vec::Vec::with_capacity(slots);
        let mut claiming = alloc::vec::Vec::with_capacity(slots);
        for _ in 0..slots {
            hashes.push(AtomicU64::new(0));
            ids.push(AtomicU32::new(crate::frame::ID_UNPUBLISHED));
            claiming.push(AtomicU32::new(CLAIM_UNRECORDED));
        }
        HeapInternTable {
            hashes,
            ids,
            claiming,
            count: AtomicU32::new(0),
        }
    }

    fn table(&self, capacity: u32) -> InternTable<'_> {
        InternTable {
            hashes: &self.hashes,
            ids: &self.ids,
            claiming: &self.claiming,
            frame_count: &self.count,
            capacity,
        }
    }
}

/// Loom test 3: two threads racing `intern` on the same name get the same
/// `FrameId`.
#[test]
fn intern_race_same_id() {
    loom::model(|| {
        // Interning table: 4 hash slots (mask 3), capacity 3 usable frames.
        let t = Arc::new(HeapInternTable::new(4));
        let hash: u64 = 0xdead_beef_0000_0001;

        // Two *live* registered participants (slots 0 and 1, so `me` is 1 and 2).
        // Neither may be taken over: `claimant_alive` always agrees they are
        // running, which is what the fail-safe default does in production.
        let spawn_one = |t: Arc<HeapInternTable>, me: u32| {
            thread::spawn(move || {
                intern_core(&t.table(3), hash, me, |_| true, |_| true, |_| {}).unwrap()
            })
        };

        let t1 = spawn_one(Arc::clone(&t), 1);
        let t2 = spawn_one(Arc::clone(&t), 2);
        let id1 = t1.join().unwrap();
        let id2 = t2.join().unwrap();

        assert_eq!(id1, id2, "concurrent intern of same name diverged");
        assert_eq!(id1, 1);
        assert_eq!(
            t.count.load(Ordering::Relaxed),
            1,
            "one distinct name -> one id"
        );
    });
}

/// Loom test 6 — **amendment A8** (`docs/PHASE2.md` §1 A8, §11.3 crash point
/// `intern.after_hash_cas_before_id_store`).
///
/// One thread plays the process that wins the hash slot and is `SIGKILL`ed before
/// publishing the id: it performs exactly the stores a killed interner would have
/// completed and then vanishes. The other thread must still terminate with the
/// name interned. Before A8 it spun forever, in every interleaving where the
/// "dead" thread got there first.
///
/// The dying thread's writes are open-coded rather than done through
/// `intern_core` because there is no way to abandon that function part-way; the
/// two CASes below are precisely its prefix up to the crash point.
#[test]
fn intern_takes_over_from_a_claimant_that_died_before_publishing() {
    /// Participant slot of the doomed interner, as stored in `claiming` (slot + 1).
    const DEAD: u32 = 1;
    /// The survivor's own `claiming` value.
    const ME: u32 = 2;

    loom::model(|| {
        let t = Arc::new(HeapInternTable::new(4));
        let hash: u64 = 0xdead_beef_0000_0001;
        let slot = (hash & 3) as usize;

        let d = Arc::clone(&t);
        let dying = thread::spawn(move || {
            if d.hashes[slot]
                .compare_exchange(0, hash, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Record the claim, then die: no id is allocated, no record is
                // written, nothing is ever published into `ids[slot]`.
                let _ = d.claiming[slot].compare_exchange(
                    CLAIM_UNRECORDED,
                    DEAD,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
        });

        let s = Arc::clone(&t);
        let survivor = thread::spawn(move || {
            // Liveness predicate: participant `DEAD` is gone, everyone else runs.
            // In production this is the injected OFD-lock/`/proc` predicate
            // (`docs/PHASE2.md` §5.1, §6.2).
            intern_core(
                &s.table(3),
                hash,
                ME,
                |owner| owner != DEAD,
                |_| true,
                |_| {},
            )
        });

        dying.join().unwrap();
        let id = survivor
            .join()
            .unwrap()
            .expect("A8: intern must recover from a dead claimant, not fail");

        assert_eq!(id, 1, "the rescued entry gets the first frame id");
        assert_eq!(
            t.ids[slot].load(Ordering::Relaxed),
            1,
            "the rescuer must publish a terminal id into the wedged slot"
        );
        assert_eq!(
            t.claiming[slot].load(Ordering::Relaxed),
            ME,
            "the rescuer must record itself as the entry's claimant"
        );
        // Whoever wins, exactly one id is allocated: the dead claimant never got
        // as far as `frame_count`, and the takeover happens before the rescuer
        // touches it.
        assert_eq!(t.count.load(Ordering::Relaxed), 1, "no id was leaked");
    });
}

/// Loom test 4: two threads racing `claim` on the same edge — exactly one wins.
#[test]
fn claim_race_exactly_one_wins() {
    loom::model(|| {
        let rec = Arc::new(ClaimRecord::new());

        let a = Arc::clone(&rec);
        let t1 = thread::spawn(move || claim(&a, 1).is_ok());
        let b = Arc::clone(&rec);
        let t2 = thread::spawn(move || claim(&b, 2).is_ok());

        let ok1 = t1.join().unwrap();
        let ok2 = t2.join().unwrap();
        assert!(ok1 ^ ok2, "expected exactly one claim to succeed");
        // The winner incremented the epoch exactly once.
        assert_eq!(rec.epoch.load(Ordering::Relaxed), 1);
    });
}

/// A model of the topology protocol as amended by `docs/PHASE2.md` §1 — A1's
/// packed word and A2's in-arena mutation lock — mirroring
/// `topology::{TopologyView, TopoLockView}` step for step.
///
/// It is a reimplementation rather than a call into the real code because
/// `crate::topology` is `#[cfg(not(loom))]`: its `depth` array is an
/// `AtomicU16`, which loom does not provide, and the lock word lives in a
/// `#[repr(C)]` arena header, which loom atomics cannot inhabit. Everything that
/// matters is preserved — the same orderings, the same single publishing store,
/// the same bounded spin and liveness-gated steal. Keep the two in step; the
/// real code is the one that ships.
///
/// `MODEL_BLOCKS` matches production's [`tf_tree_arena::TOPO_BLOCKS`] and that is
/// **load-bearing, not decoration**. A first draft of this model used two blocks
/// on the theory that the count is a tuning knob, and loom immediately produced
/// a reader that observed `(P_OLD, D_A)` — a genuine mix of two generations. The
/// mechanism is worth writing down, because it is the whole reason A1 says four:
///
/// The reader's re-check (`word.load(Relaxed) == w1`) detects a mutation only if
/// the word *changed*. Cache coherence guarantees the second load sees `w1` or
/// something newer, but a `Relaxed` load is free to keep returning `w1` after
/// other threads have moved on. So the re-check is not a proof that nothing
/// happened — it is a proof that nothing has become *visible here* yet. What
/// actually protects the reader is that a mutator never writes the block the
/// reader is walking, and with `N` blocks that needs `N` publications inside one
/// read. With two blocks and two mutators, `N` is reachable and the reader tore.
/// With four it is not, which is precisely A1's "four flips" argument.
struct TopoModel {
    /// A2: `0` = free, else `participant_slot + 1`.
    lock: AtomicU64,
    /// A2: diagnostics only; written after the CAS that publishes ownership.
    acquired_at: AtomicI64,
    /// A1: `pack(generation, active)`. **There is no odd state.**
    word: AtomicU64,
    parent: [AtomicU32; MODEL_BLOCKS],
    depth: [AtomicU32; MODEL_BLOCKS],
    /// Test-only witness: how many threads believe they are in the critical
    /// section. Must never exceed one.
    in_section: AtomicU32,
}

/// Mirrors `tf_tree_arena::TOPO_BLOCKS`; see [`TopoModel`] for why it must.
const MODEL_BLOCKS: usize = 4;
/// Small enough that loom can reach the steal path; the production constant is
/// `topology::TOPO_LOCK_SPIN_LIMIT`.
const MODEL_SPIN_LIMIT: u32 = 3;

const P_OLD: u32 = 5;
const D_OLD: u32 = 1;

/// `pack`/`unpack` from `tf_tree_arena` — re-stated so the model does not depend
/// on a `not(loom)` module. Bits 63..8 generation, bits 7..0 active index.
fn pack(generation: u64, active: usize) -> u64 {
    (generation << 8) | active as u64
}

fn unpack(word: u64) -> (u64, usize) {
    (word >> 8, (word & 0xff) as usize % MODEL_BLOCKS)
}

/// A held lock. Releases with a CAS, not a store, so a participant that was
/// stolen from cannot free the thief's lock.
struct ModelGuard<'a> {
    model: &'a TopoModel,
    want: u64,
}

impl Drop for ModelGuard<'_> {
    fn drop(&mut self) {
        let _ =
            self.model
                .lock
                .compare_exchange(self.want, 0, Ordering::Release, Ordering::Relaxed);
    }
}

impl TopoModel {
    fn new() -> TopoModel {
        TopoModel {
            lock: AtomicU64::new(0),
            acquired_at: AtomicI64::new(0),
            word: AtomicU64::new(pack(0, 0)),
            parent: core::array::from_fn(|i| AtomicU32::new(if i == 0 { P_OLD } else { 0 })),
            depth: core::array::from_fn(|i| AtomicU32::new(if i == 0 { D_OLD } else { 0 })),
            in_section: AtomicU32::new(0),
        }
    }

    /// A2's acquire: bounded spin, then resolve the holder and steal if it is
    /// dead. `is_alive` is injected exactly as in the real code — the lock never
    /// decides liveness itself.
    fn acquire(&self, slot: u32, is_alive: impl Fn(u32) -> bool) -> Option<ModelGuard<'_>> {
        let want = u64::from(slot) + 1;
        for _ in 0..MODEL_SPIN_LIMIT {
            if self
                .lock
                .compare_exchange(0, want, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.acquired_at.store(1, Ordering::Relaxed);
                return Some(ModelGuard { model: self, want });
            }
            crate::sync::spin();
        }
        let held = self.lock.load(Ordering::Acquire);
        if held == 0 || held == want {
            return None;
        }
        if is_alive((held - 1) as u32) {
            return None;
        }
        self.lock
            .compare_exchange(held, want, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ModelGuard { model: self, want })
    }

    /// A1's writer, held under A2's lock: mutate the *inactive* block, then
    /// publish with a **single store**. No odd state, so nothing a crash can
    /// leave half-done is observable.
    fn mutate(&self, guard: &ModelGuard<'_>, parent: u32, depth: u32) {
        let _ = guard; // the type is the proof; this silences "unused".

        // Exactly one mutator may be here. If A2's lock is broken this fires.
        let concurrent = self.in_section.fetch_add(1, Ordering::AcqRel);
        assert_eq!(concurrent, 0, "two mutators inside the critical section");

        let (g, active) = unpack(self.word.load(Ordering::Relaxed));
        let next = (active + 1) % MODEL_BLOCKS;
        // Re-copy the active block wholesale — this is what makes stealing need
        // no rollback: whatever a dead holder left in `next` is overwritten.
        self.parent[next].store(
            self.parent[active].load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.depth[next].store(
            self.depth[active].load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.parent[next].store(parent, Ordering::Relaxed);
        self.depth[next].store(depth, Ordering::Relaxed);

        crate::sync::fence(Ordering::Release);
        self.word.store(pack(g + 1, next), Ordering::Release);

        self.in_section.fetch_sub(1, Ordering::AcqRel);
    }

    /// A1's reader — what plan compilation does. Wait-free: it never spins on a
    /// writer, because there is no state a writer can leave that a reader must
    /// wait out.
    fn read(&self) -> (u32, u32, u64) {
        loop {
            let w1 = self.word.load(Ordering::Acquire);
            let (g, blk) = unpack(w1);
            let parent = self.parent[blk].load(Ordering::Relaxed);
            let depth = self.depth[blk].load(Ordering::Relaxed);
            crate::sync::fence(Ordering::Acquire);
            if self.word.load(Ordering::Relaxed) == w1 {
                return (parent, depth, g);
            }
            crate::sync::spin();
        }
    }
}

/// Loom test 5: a topology mutation concurrent with a topology read. The reader
/// sees the old pair or the new pair, never a mix.
#[test]
fn topology_read_sees_old_or_new_never_mixed() {
    const P_NEW: u32 = 9;
    const D_NEW: u32 = 3;

    loom::model(|| {
        let topo = Arc::new(TopoModel::new());

        let w = Arc::clone(&topo);
        let writer = thread::spawn(move || {
            let g = w.acquire(0, |_| true).unwrap();
            w.mutate(&g, P_NEW, D_NEW);
        });

        let r = Arc::clone(&topo);
        let reader = thread::spawn(move || r.read());

        writer.join().unwrap();
        let (parent, depth, _g) = reader.join().unwrap();
        assert!(
            (parent, depth) == (P_OLD, D_OLD) || (parent, depth) == (P_NEW, D_NEW),
            "mixed topology read: ({parent}, {depth})"
        );
    });
}

/// Loom test 6 (`docs/PHASE2.md` §1, A2): two participants racing the mutation
/// lock, with a third thread compiling a plan against the topology throughout.
///
/// Three properties, all of which fail without A2:
///
/// * **Exactly one mutator at a time** — asserted from inside the critical
///   section by `in_section`, so a broken lock is caught where it happens rather
///   than inferred from the wreckage.
/// * **Every published generation is accounted for.** A mutation that succeeded
///   published exactly once; one that lost the lock published nothing. Two
///   mutators sharing a scratch block would lose one.
/// * **The reader sees one generation or the other, never a mix.** The
///   `(parent, depth)` pair is written as a unit under the lock, so any pairing
///   the reader observes must be one that some single mutator wrote.
#[test]
fn two_mutators_race_the_lock_and_a_reader_sees_no_mix() {
    const P_A: u32 = 11;
    const D_A: u32 = 2;
    const P_B: u32 = 22;
    const D_B: u32 = 4;

    loom::model(|| {
        let topo = Arc::new(TopoModel::new());

        let spawn_mutator = |topo: Arc<TopoModel>, slot: u32, parent: u32, depth: u32| {
            thread::spawn(move || match topo.acquire(slot, |_| true) {
                Some(g) => {
                    topo.mutate(&g, parent, depth);
                    true
                }
                // Contended. Every participant is alive here, so nothing may be
                // stolen and the loser simply does not publish.
                None => false,
            })
        };

        let m1 = spawn_mutator(Arc::clone(&topo), 0, P_A, D_A);
        let m2 = spawn_mutator(Arc::clone(&topo), 1, P_B, D_B);

        let r = Arc::clone(&topo);
        let reader = thread::spawn(move || r.read());

        let ok1 = m1.join().unwrap();
        let ok2 = m2.join().unwrap();
        let (parent, depth, _g) = reader.join().unwrap();

        let published = u64::from(ok1) + u64::from(ok2);
        let (generation, _) = unpack(topo.word.load(Ordering::Relaxed));
        assert_eq!(
            generation, published,
            "{published} mutations succeeded but the generation is {generation}"
        );
        assert!(
            (parent, depth) == (P_OLD, D_OLD)
                || (parent, depth) == (P_A, D_A)
                || (parent, depth) == (P_B, D_B),
            "reader saw a topology nobody published: ({parent}, {depth})"
        );
        // The lock is free again however the race went: every winner released,
        // and no loser ever held it.
        assert_eq!(topo.lock.load(Ordering::Relaxed), 0, "the lock leaked");
    });
}

/// Loom test 7 — the `topo.holding_lock` crash point (`docs/PHASE2.md` §11.3).
///
/// One participant takes the lock, scribbles on the inactive block the way a
/// half-finished copy would, and dies without releasing or publishing. A second
/// participant must steal the lock and complete, and the result must carry **no
/// trace** of the first — which is A2's claim that recovery is a no-op, because
/// A1 left the dead holder nothing observable to undo.
///
/// A reader runs throughout, and must never see the scribble: it was written to
/// a block the topology word never pointed at.
///
/// # Why the death is not a thread
///
/// The first draft ran the dying participant as a loom thread, and loom
/// immediately scheduled its scribble *after* the rescuer had published —
/// leaving `0xDEAD` in the block that was by then active. That is a real hazard,
/// but it is **not this one**: it is the false-negative case, where liveness
/// wrongly declares a live-but-stalled participant dead and it later resumes.
/// `docs/PHASE2.md` §6.2 addresses that by making the predicate fail safe, and
/// §6.1 removes it entirely once claims are kernel locks.
///
/// A participant that actually died executes no further instruction, ever. So
/// the death is modelled inline, before the rescuer exists: every store the
/// corpse will ever make has already happened.
#[test]
fn a_dead_lock_holder_is_stolen_from_and_leaves_no_trace() {
    const P_GARBAGE: u32 = 0xDEAD;
    const D_GARBAGE: u32 = 0xBEEF;
    const P_NEW: u32 = 7;
    const D_NEW: u32 = 2;
    /// The dead participant's slot. `is_alive` reports only this one dead, so
    /// the test cannot pass by stealing indiscriminately.
    const DEAD_SLOT: u32 = 0;

    loom::model(|| {
        let topo = Arc::new(TopoModel::new());

        // Participant 0 dies holding the lock, mid-copy: it took the lock and
        // dirtied the scratch block, and it will never release or publish.
        {
            let g = topo.acquire(DEAD_SLOT, |_| true).unwrap();
            let (_, active) = unpack(topo.word.load(Ordering::Relaxed));
            let scratch = (active + 1) % MODEL_BLOCKS;
            topo.parent[scratch].store(P_GARBAGE, Ordering::Relaxed);
            topo.depth[scratch].store(D_GARBAGE, Ordering::Relaxed);
            core::mem::forget(g); // the crash: no release, no `Drop`
        }
        assert_eq!(
            topo.lock.load(Ordering::Relaxed),
            u64::from(DEAD_SLOT) + 1,
            "the corpse should still hold the lock"
        );

        // Participant 1 finds the lock held by a corpse and takes it over.
        let thief = Arc::clone(&topo);
        let rescuer = thread::spawn(move || loop {
            if let Some(g) = thief.acquire(1, |slot| slot != DEAD_SLOT) {
                thief.mutate(&g, P_NEW, D_NEW);
                return;
            }
            crate::sync::spin();
        });

        let r = Arc::clone(&topo);
        let reader = thread::spawn(move || r.read());

        rescuer.join().unwrap();
        let (parent, depth, _g) = reader.join().unwrap();

        // The scribble was never published, whenever the reader looked.
        assert!(
            (parent, depth) == (P_OLD, D_OLD) || (parent, depth) == (P_NEW, D_NEW),
            "a reader observed an abandoned mutation: ({parent}, {depth})"
        );
        // The stealer's mutation is the only one that landed, and it landed
        // whole — no rollback, no repair, nothing inherited.
        let (generation, active) = unpack(topo.word.load(Ordering::Relaxed));
        assert_eq!(generation, 1, "exactly one mutation should have published");
        assert_eq!(topo.parent[active].load(Ordering::Relaxed), P_NEW);
        assert_eq!(topo.depth[active].load(Ordering::Relaxed), D_NEW);
    });
}

/// A late `release` racing a reap + re-`register` must not free the new occupant.
///
/// The single-threaded version of this (`tests.rs`) cannot fail on the code this
/// guards against, and saying so matters: the old guard *did* reject a stale
/// incarnation — but as a load of `incarnation` followed by a CAS on `state`,
/// two words apart. The bug lives entirely in the window between them, so only
/// an interleaving exhibits it. Here, thread A is the departing participant's
/// late `release(slot, 1)`; thread B reaps the same slot and hands it to a new
/// process. Loom explores the schedule where A reads "still incarnation 1",
/// B completes the whole handover, and A's CAS then lands on the new occupant.
///
/// Packing the incarnation into `state` makes that schedule harmless: there is
/// one word, so "still LIVE and still mine" is decided by the CAS itself.
///
/// The consequence of getting it wrong is not a lost slot — it is two live
/// processes sharing a slot index, after which the `slot + 1` owner encoding
/// used by claims (A3) and by the topology lock (A2) no longer names one process.
#[test]
fn a_late_release_racing_a_slot_handover_frees_nobody() {
    const P_LATE: u32 = 111;
    const P_NEW: u32 = 222;

    loom::model(|| {
        let table = Arc::new(alloc::vec![ParticipantRecord::default()]);
        let (slot, inc) = ParticipantTable::new(&table)
            .register(P_LATE, 1, 0)
            .unwrap();
        assert_eq!((slot, inc), (0, 1));

        // A: the departing process finally gets around to detaching.
        let a = Arc::clone(&table);
        let late = thread::spawn(move || ParticipantTable::new(&a).release(0, 1));

        // B: a reaper decides that participant is gone, and a new process takes
        // the freed slot. Modelled as the same release (a reap *is* a release
        // performed by somebody else) followed by a registration.
        let b = Arc::clone(&table);
        let handover = thread::spawn(move || {
            let t = ParticipantTable::new(&b);
            t.release(0, 1);
            t.register(P_NEW, 2, 0).ok()
        });

        late.join().unwrap();
        let registered = handover.join().unwrap();

        let t = ParticipantTable::new(&table);
        if let Some((new_slot, new_inc)) = registered {
            assert_eq!(new_slot, 0, "only one slot exists");
            assert_eq!(
                t.identity(new_slot),
                Some((P_NEW, 2, new_inc)),
                "the late release freed a slot that had already been handed over"
            );
        }
    });
}

/// Two joiners told to take the *same* slot: exactly one may get it.
///
/// `docs/PHASE2.md` §3.7 has the owner hand each client a `participant_slot`,
/// and `docs/decisions/0005` makes that integer double as the lock-file byte.
/// Two clients can be handed the same slot — by an owner bug, by a takeover
/// mid-handshake, or by a stale `HelloResponse` replayed after a reap — and the
/// arena must be the thing that says no.
///
/// This has to be a loom test rather than a sequential one. The window is
/// between `register_at`'s CAS and its release-store: a sequential test calls
/// them in order and can never place a second thread inside. Restore the
/// pre-CAS shape (load `state`, compare to `FREE`, store `RESERVED`) and loom
/// finds the interleaving where both threads observe `FREE` and both proceed to
/// publish — two live processes sharing one slot index, which is exactly what
/// the `slot + 1` owner encoding behind A3 claims cannot happen.
#[test]
fn two_joiners_handed_the_same_slot_cannot_both_take_it() {
    const P_A: u32 = 101;
    const P_B: u32 = 202;

    loom::model(|| {
        // Four slots, so a losing thread has somewhere it *could* have gone —
        // the assertion is that it does not go there, because `register_at`
        // takes the named slot or nothing.
        let table = Arc::new(alloc::vec![
            ParticipantRecord::default(),
            ParticipantRecord::default(),
            ParticipantRecord::default(),
            ParticipantRecord::default(),
        ]);

        let a = Arc::clone(&table);
        let ta = thread::spawn(move || ParticipantTable::new(&a).register_at(2, P_A, 1, 0));
        let b = Arc::clone(&table);
        let tb = thread::spawn(move || ParticipantTable::new(&b).register_at(2, P_B, 2, 0));

        let ra = ta.join().unwrap();
        let rb = tb.join().unwrap();

        assert!(
            ra.is_ok() ^ rb.is_ok(),
            "exactly one joiner may take slot 2: {ra:?} / {rb:?}"
        );

        let t = ParticipantTable::new(&table);
        let (pid, _, inc) = t.identity(2).expect("the winner published a LIVE record");
        // The record must belong to the winner *entire* — not a mix of A's pid
        // and B's incarnation, which is what a torn publication would leave.
        let (winner_pid, winner_inc) = if let Ok(i) = ra {
            (P_A, i)
        } else {
            (P_B, rb.unwrap())
        };
        assert_eq!((pid, inc), (winner_pid, winner_inc));

        // And the loser did not silently land somewhere else.
        for other in [0, 1, 3] {
            assert_eq!(t.identity(other), None, "slot {other} should be untouched");
        }
    });
}
