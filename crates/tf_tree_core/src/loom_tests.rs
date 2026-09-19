//! `loom` model-checked concurrency tests (run under `--cfg loom`).
//!
//! The gate for `docs/PHASE1.md` §10.2. Each test drives the shared algorithm code
//! over heap instances built from `crate::sync` atomics; capacities stay small to
//! bound the state space, but not so small that the code under test is unreachable.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use loom::sync::Arc;
use loom::thread;

use tf_tree_math::{exp_se3, Iso3, LerpSlerp};

use crate::buffer::{PoseSlot, SampleRing};
use crate::edge::{claim, ClaimRecord};
use crate::error::{EdgeId, LookupError};
use crate::frame::{intern_core, InternTable, CLAIM_UNRECORDED};
use crate::participant::{state_of, ParticipantRecord, ParticipantTable, FREE, LIVE, RESERVED};
use crate::sample::{Bracket, ExtrapPolicy};
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
        }
    }

    fn ring(&self) -> SampleRing<'_> {
        SampleRing {
            head: &self.head,
            heartbeat: &self.heartbeat,
            stamps: &self.stamps,
            poses: &self.poses,
            edge: EdgeId(0),
        }
    }
}

/// Run a `loom` model under a preemption bound the environment can raise but
/// **cannot lower**: unbounded search is weaker (it missed a broken liveness
/// predicate in `two_mutators_race_...` that bound 3 finds). `xtask loom` sets 3.
fn model(f: impl Fn() + Sync + Send + 'static) {
    /// Reaches the topology lock's steal path, which needs `MODEL_SPIN_LIMIT`
    /// exhausted while another thread holds the word. Lowering it removes that path.
    const PREEMPTION_FLOOR: usize = 3;

    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(
        builder
            .preemption_bound
            .unwrap_or(PREEMPTION_FLOOR)
            .max(PREEMPTION_FLOOR),
    );
    builder.check(f);
}

#[test]
fn writer_three_pushes_reader_never_torn() {
    model(|| {
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
            // Slot 1 is written once (pose(2)): a read is the zero pose or pose(2).
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
/// while a reader samples. The reader returns the one interpolation the published
/// history permits, or a documented error, never a pose from two eras.
///
/// Capacity 4 and five pushes: capacity 2 leaves a one-sample window that cannot
/// reach the bracket search. The only bracket containing `t = 25` is `(20, 30)`.
#[test]
fn writer_wraps_reader_gets_valid_or_recycled() {
    model(|| {
        let hr = Arc::new(HeapRing::new(4));
        let expect = <LerpSlerp as tf_tree_math::Interp>::eval(&pose(2), &pose(3), 0.5).to_bits();

        let w = Arc::clone(&hr);
        let writer = thread::spawn(move || {
            let ring = w.ring();
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

/// The same lap through [`SampleRing::sample_from`] with a **caller-held cursor
/// pointing at the slot the lap destroys**, the shape `Plan::at` and every monotone
/// batch use.
///
/// `10, 20, 30` are published first; the writer lands `40, 50`. The only legal `Ok`
/// for `t = 25` is `(20, 30)`. The cursor is seeded `0`: seeded at `1` or `2` the
/// trailing `head - i > retained` check in [`SampleRing::read_from`] can be deleted
/// unnoticed (`docs/decisions/0060` step 2).
///
/// The `Hold` and exact-newest arms are [`sample_from_hold_revalidates_across_a_lap`]
/// and [`sample_from_exact_newest_revalidates_across_a_lap`].
#[test]
fn sample_from_with_a_stale_cursor_across_a_lap() {
    model(|| {
        let hr = Arc::new(HeapRing::new(4));
        {
            let ring = hr.ring();
            for i in 1..=3u64 {
                ring.push(i as i64 * 10, &pose(i)).unwrap();
            }
        }
        let expect = <LerpSlerp as tf_tree_math::Interp>::eval(&pose(2), &pose(3), 0.5).to_bits();

        let w = Arc::clone(&hr);
        let writer = thread::spawn(move || {
            let ring = w.ring();
            ring.push(40, &pose(4)).unwrap();
            ring.push(50, &pose(5)).unwrap();
        });

        let r = Arc::clone(&hr);
        let reader = thread::spawn(move || {
            let ring = r.ring();
            // Caller-held, as `Plan::fold_at_cursors` holds one per step.
            let mut cursor = 0u64;
            match ring.sample_from::<LerpSlerp>(25, ExtrapPolicy::Error, &mut cursor) {
                Ok(iso) => assert_eq!(
                    iso.to_bits(),
                    expect,
                    "sample_from followed a stale cursor into a recycled slot"
                ),
                Err(
                    LookupError::Extrapolation { .. }
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

/// `sample_from`'s **`Hold` arm** revalidates the newest slot against a lap.
///
/// Capacity 2: the newest slot is overwritten only after `capacity` pushes. `10` is
/// published first; the writer lands `20` and `30`. Only at `head == 1` is `15` past
/// the newest stamp, so the only legal `Ok` is `pose(1)`. One arm per model keeps
/// each at ~30 s.
#[test]
fn sample_from_hold_revalidates_across_a_lap() {
    model(|| {
        let hr = Arc::new(HeapRing::new(2));
        hr.ring().push(10, &pose(1)).unwrap();
        let held = pose(1).to_bits();

        let w = Arc::clone(&hr);
        let writer = thread::spawn(move || {
            let ring = w.ring();
            ring.push(20, &pose(2)).unwrap();
            ring.push(30, &pose(3)).unwrap();
        });

        let r = Arc::clone(&hr);
        let reader = thread::spawn(move || {
            let ring = r.ring();
            let mut cursor = 0u64;
            match ring.sample_from::<LerpSlerp>(15, ExtrapPolicy::Hold, &mut cursor) {
                Ok(iso) => assert_eq!(
                    iso.to_bits(),
                    held,
                    "sample_from's Hold arm returned a lapped slot"
                ),
                Err(
                    LookupError::Extrapolation { .. }
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

/// `sample_from`'s **exact-newest arm** (`t == t_new`) revalidates the newest slot
/// against a lap. Same ring and writer as [`sample_from_hold_revalidates_across_a_lap`];
/// `sample_from(10, Error)` may return only `pose(1)`.
#[test]
fn sample_from_exact_newest_revalidates_across_a_lap() {
    model(|| {
        let hr = Arc::new(HeapRing::new(2));
        hr.ring().push(10, &pose(1)).unwrap();
        let held = pose(1).to_bits();

        let w = Arc::clone(&hr);
        let writer = thread::spawn(move || {
            let ring = w.ring();
            ring.push(20, &pose(2)).unwrap();
            ring.push(30, &pose(3)).unwrap();
        });

        let r = Arc::clone(&hr);
        let reader = thread::spawn(move || {
            let ring = r.ring();
            let mut cursor = 0u64;
            match ring.sample_from::<LerpSlerp>(10, ExtrapPolicy::Error, &mut cursor) {
                Ok(iso) => assert_eq!(
                    iso.to_bits(),
                    held,
                    "sample_from's exact-newest arm returned a lapped slot"
                ),
                Err(
                    LookupError::Extrapolation { .. }
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

/// [`SampleRing::read_from`] validates the bracket **before it hands it back**:
/// `Plan::fold_batch` carries it across further reads, so an
/// `Ok(Bracket::Between { .. })` must carry `(20, 30)` bit for bit. Ring, writer,
/// cursor and query are those of [`sample_from_with_a_stale_cursor_across_a_lap`].
#[test]
fn read_from_validates_the_bracket_it_hands_back() {
    model(|| {
        let hr = Arc::new(HeapRing::new(4));
        {
            let ring = hr.ring();
            for i in 1..=3u64 {
                ring.push(i as i64 * 10, &pose(i)).unwrap();
            }
        }
        let (lo, hi) = (pose(2).to_bits(), pose(3).to_bits());

        let w = Arc::clone(&hr);
        let writer = thread::spawn(move || {
            let ring = w.ring();
            ring.push(40, &pose(4)).unwrap();
            ring.push(50, &pose(5)).unwrap();
        });

        let r = Arc::clone(&hr);
        let reader = thread::spawn(move || {
            let ring = r.ring();
            // Held across the chunk, as `Plan::fold_batch` does.
            let mut cursor = 0u64;
            match ring.read_from::<Bracket>(25, ExtrapPolicy::Error, &mut cursor) {
                // The only bracket for 25 is (20, 30) at s = 0.5.
                Ok(Bracket::Between { a, b, s }) => {
                    assert_eq!(
                        (a.to_bits(), b.to_bits(), s),
                        (lo, hi, 0.5),
                        "read_from handed back a bracket built from a recycled slot"
                    );
                }
                Ok(Bracket::Exact(_)) => {
                    panic!("25 is not a published stamp and the policy is Error")
                }
                Err(
                    LookupError::Extrapolation { .. }
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

/// Loom test 3 (`docs/PHASE1.md` §10.2's *mutation test*): observing `head == h`
/// makes the stamps of all `h` published samples visible.
///
/// No `sample`-shaped fixture catches a weakened `head.store(h + 1, Release)`, so
/// the reader is transcribed as [`SampleRing::sample`] does it (`head` `Acquire`,
/// stamps `Relaxed`) against the real `push`.
#[test]
fn head_publishes_every_stamp_below_it() {
    model(|| {
        let hr = Arc::new(HeapRing::new(4));

        let w = Arc::clone(&hr);
        let writer = thread::spawn(move || {
            let ring = w.ring();
            // No lap: a stale read is the zero value.
            for i in 1..=4u64 {
                ring.push(i as i64 * 10, &pose(i)).unwrap();
            }
        });

        let r = Arc::clone(&hr);
        let reader = thread::spawn(move || {
            let h = r.head.load(Ordering::Acquire);
            for i in 0..h {
                let got = r.stamps[(i & 3) as usize].load(Ordering::Relaxed);
                assert_eq!(
                    got,
                    (i as i64 + 1) * 10,
                    "head published {h} samples but stamp {i} is not visible"
                );
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    });
}

/// The interning table's parallel arrays plus its id allocator, on the heap from
/// loom atomics. Zero-initialized like the production arena.
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

/// Loom test 3: two threads racing `intern` on the same name get the same `FrameId`.
#[test]
fn intern_race_same_id() {
    model(|| {
        // 4 hash slots (mask 3), capacity 3.
        let t = Arc::new(HeapInternTable::new(4));
        let hash: u64 = 0xdead_beef_0000_0001;

        // Two live participants (`me` is 1 and 2); neither may be taken over.
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
/// One thread performs the stores a killed interner completed (open-coded, since
/// `intern_core` cannot be abandoned part-way) and vanishes. The other must still
/// terminate with the name interned.
#[test]
fn intern_takes_over_from_a_claimant_that_died_before_publishing() {
    /// Doomed interner's slot, as stored in `claiming` (slot + 1).
    const DEAD: u32 = 1;
    /// The survivor's own `claiming` value.
    const ME: u32 = 2;

    model(|| {
        let t = Arc::new(HeapInternTable::new(4));
        let hash: u64 = 0xdead_beef_0000_0001;
        let slot = (hash & 3) as usize;

        let d = Arc::clone(&t);
        let dying = thread::spawn(move || {
            if d.hashes[slot]
                .compare_exchange(0, hash, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Record the claim, then die: nothing is published into `ids[slot]`.
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
            // Participant `DEAD` is gone, everyone else runs (`docs/PHASE2.md` §5.1).
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
        // The dead claimant never reached `frame_count`.
        assert_eq!(t.count.load(Ordering::Relaxed), 1, "no id was leaked");
    });
}

/// Loom test 4: two threads racing `claim` on the same edge — exactly one wins.
#[test]
fn claim_race_exactly_one_wins() {
    model(|| {
        let rec = Arc::new(ClaimRecord::new());

        let a = Arc::clone(&rec);
        let t1 = thread::spawn(move || claim(&a, 1).is_ok());
        let b = Arc::clone(&rec);
        let t2 = thread::spawn(move || claim(&b, 2).is_ok());

        let ok1 = t1.join().unwrap();
        let ok2 = t2.join().unwrap();
        assert!(ok1 ^ ok2, "expected exactly one claim to succeed");
        // The winner incremented the epoch once.
        assert_eq!(rec.epoch.load(Ordering::Relaxed), 1);
    });
}

/// A model of the topology protocol as amended by `docs/PHASE2.md` §1 (A1's packed
/// word, A2's mutation lock), mirroring `topology::{TopologyView, TopoLockView}`
/// step for step; keep the two in step. It is a reimplementation because loom's
/// atomics cannot inhabit a `#[repr(C)]` arena header.
///
/// # The control run
///
/// With `is_alive`'s refusal removed, `two_mutators_race_...` fails while
/// [`a_dead_lock_holder_is_stolen_from_and_leaves_no_trace`] still passes, so each
/// test exercises the gate from a different side. The control fires only under
/// [`model`]'s preemption bound.
///
/// `MODEL_BLOCKS` matches [`tf_tree_arena::TOPO_BLOCKS`], which is load-bearing:
/// with two blocks loom produced a reader observing `(P_OLD, D_A)`.
struct TopoModel {
    /// A2: `0` = free, else `participant_slot + 1`.
    lock: AtomicU64,
    /// A2: diagnostics only; written after the CAS that publishes ownership.
    acquired_at: AtomicI64,
    /// A1: `pack(generation, active)`. **There is no odd state.**
    word: AtomicU64,
    parent: [AtomicU32; MODEL_BLOCKS],
    depth: [AtomicU32; MODEL_BLOCKS],
    /// Test-only witness: threads believing they are in the critical section.
    in_section: AtomicU32,
}

/// Mirrors `tf_tree_arena::TOPO_BLOCKS`; see [`TopoModel`].
const MODEL_BLOCKS: usize = 4;
/// Small enough that loom reaches the steal path.
const MODEL_SPIN_LIMIT: u32 = 3;

const P_OLD: u32 = 5;
const D_OLD: u32 = 1;

/// `pack`/`unpack` from `tf_tree_arena`, re-stated to avoid a `not(loom)` module.
fn pack(generation: u64, active: usize) -> u64 {
    (generation << 8) | active as u64
}

fn unpack(word: u64) -> (u64, usize) {
    (word >> 8, (word & 0xff) as usize % MODEL_BLOCKS)
}

/// A held lock. Releases with a CAS, so a participant stolen from cannot free the thief's lock.
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

    /// A2's acquire: bounded spin, then steal if the holder is dead. `is_alive` is injected; the lock never decides liveness.
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

    /// A1's writer under A2's lock: mutate the *inactive* block, then publish with a single store.
    fn mutate(&self, guard: &ModelGuard<'_>, parent: u32, depth: u32) {
        let _ = guard; // the type is the proof; this silences "unused".

        let concurrent = self.in_section.fetch_add(1, Ordering::AcqRel);
        assert_eq!(concurrent, 0, "two mutators inside the critical section");

        let (g, active) = unpack(self.word.load(Ordering::Relaxed));
        let next = (active + 1) % MODEL_BLOCKS;
        // Re-copy the active block wholesale: stealing needs no rollback.
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

    /// A1's reader (plan compilation). Wait-free: no writer state needs waiting out.
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

/// Loom test 5: a topology mutation concurrent with a read; the reader sees the old or new pair, never a mix.
#[test]
fn topology_read_sees_old_or_new_never_mixed() {
    const P_NEW: u32 = 9;
    const D_NEW: u32 = 3;

    model(|| {
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
/// lock while a reader compiles a plan. Asserted: one mutator at a time; a winner
/// publishes once and a loser not at all; the reader never sees a mix.
#[test]
fn two_mutators_race_the_lock_and_a_reader_sees_no_mix() {
    const P_A: u32 = 11;
    const D_A: u32 = 2;
    const P_B: u32 = 22;
    const D_B: u32 = 4;

    model(|| {
        let topo = Arc::new(TopoModel::new());

        let spawn_mutator = |topo: Arc<TopoModel>, slot: u32, parent: u32, depth: u32| {
            thread::spawn(move || match topo.acquire(slot, |_| true) {
                Some(g) => {
                    topo.mutate(&g, parent, depth);
                    true
                }
                // Contended, every participant alive: nothing is stolen.
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
        // The lock is free again however the race went.
        assert_eq!(topo.lock.load(Ordering::Relaxed), 0, "the lock leaked");
    });
}

/// Loom test 7 — the `topo.holding_lock` crash point (`docs/PHASE2.md` §11.3).
///
/// One participant takes the lock, scribbles on the inactive block, and dies. A
/// second must steal the lock and complete with no trace of the first (A2), and a
/// concurrent reader must never see the scribble. The death is modelled inline,
/// before the rescuer exists.
#[test]
fn a_dead_lock_holder_is_stolen_from_and_leaves_no_trace() {
    const P_GARBAGE: u32 = 0xDEAD;
    const D_GARBAGE: u32 = 0xBEEF;
    const P_NEW: u32 = 7;
    const D_NEW: u32 = 2;
    /// The dead participant's slot; only this one is reported dead.
    const DEAD_SLOT: u32 = 0;

    model(|| {
        let topo = Arc::new(TopoModel::new());

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

        assert!(
            (parent, depth) == (P_OLD, D_OLD) || (parent, depth) == (P_NEW, D_NEW),
            "a reader observed an abandoned mutation: ({parent}, {depth})"
        );
        let (generation, active) = unpack(topo.word.load(Ordering::Relaxed));
        assert_eq!(generation, 1, "exactly one mutation should have published");
        assert_eq!(topo.parent[active].load(Ordering::Relaxed), P_NEW);
        assert_eq!(topo.depth[active].load(Ordering::Relaxed), D_NEW);
    });
}

/// A late `release` racing a reap + re-`register` must not free the new occupant
/// (the incarnation packed into `state` makes the guard one CAS).
#[test]
fn a_late_release_racing_a_slot_handover_frees_nobody() {
    const P_LATE: u32 = 111;
    const P_NEW: u32 = 222;

    model(|| {
        let table = Arc::new(alloc::vec![ParticipantRecord::default()]);
        let (slot, inc) = ParticipantTable::new(&table)
            .register(P_LATE, 1, 0)
            .unwrap();
        assert_eq!((slot, inc), (0, 1));

        let a = Arc::clone(&table);
        let late = thread::spawn(move || ParticipantTable::new(&a).release(0, 1));

        // B: a reap is a release by somebody else, then the new registration.
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

/// Two joiners told to take the *same* slot: exactly one may get it
/// (`docs/PHASE2.md` §3.7, `docs/decisions/0005`). Loom-only: the window is inside
/// `register_at`, which a sequential test cannot enter.
#[test]
fn two_joiners_handed_the_same_slot_cannot_both_take_it() {
    const P_A: u32 = 101;
    const P_B: u32 = 202;

    model(|| {
        // Four slots: `register_at` takes the named slot or nothing.
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
        // The record belongs to the winner entire.
        let (winner_pid, winner_inc) = if let Ok(i) = ra {
            (P_A, i)
        } else {
            (P_B, rb.unwrap())
        };
        assert_eq!((pid, inc), (winner_pid, winner_inc));

        for other in [0, 1, 3] {
            assert_eq!(t.identity(other), None, "slot {other} should be untouched");
        }
    });
}

/// A reclaimer sweeping the table while a joiner registers: pins **the caller's**
/// read order (`docs/decisions/0028` open question 6). The state word is observed
/// before the lock byte is probed, and no record a joiner published is erased.
///
/// - **Ordering.** An `Acquire` load returning a `live_word` synchronises-with
///   `fill_slot`'s `Release`; a byte-first sweep has no such edge.
/// - **No erasure.** A joiner that returned `Ok` is never left with a `FREE` record.
///
/// It does not pin `reclaim`'s CAS guard
/// ([`crate::tests::reclaim_fails_when_the_observed_word_has_changed`] does) or its
/// strength (stated on `reclaim`).
///
/// # Controls
///
/// Both are runnable `#[should_panic]` tests, so an unfalsifiable model fails:
/// [`control_reclaim_races_register_probes_the_byte_first`] and
/// [`control_reclaim_races_register_observes_relaxed`].
///
/// # Modelling notes
///
/// Slot 0's corpse is reclaimed uncontended, avoiding loom 0.7.2's
/// `match_rmw_to_stores` over-approximation. The byte is an exclusive per-slot lock
/// word, probed `Relaxed`.
#[test]
fn reclaim_races_register() {
    reclaim_races_register_model(Sweep::WordThenByte);
}

/// The failing control: the byte is probed **before** the word is observed.
#[test]
#[should_panic(expected = "no erasure")]
fn control_reclaim_races_register_probes_the_byte_first() {
    reclaim_races_register_model(Sweep::ByteThenWord);
}

/// The second failing control: the right order, the word observed `Relaxed`.
#[test]
#[should_panic(expected = "no erasure")]
fn control_reclaim_races_register_observes_relaxed() {
    reclaim_races_register_model(Sweep::WordRelaxedThenByte);
}

/// How one sweep reads a slot: the protocol, and the two controls that break it.
#[derive(Clone, Copy)]
enum Sweep {
    /// The word (`Acquire`) first, then the byte.
    WordThenByte,
    /// The byte first. Control.
    ByteThenWord,
    /// The right order, `Relaxed` observation. Control.
    WordRelaxedThenByte,
}

/// The model behind [`reclaim_races_register`] and its controls.
fn reclaim_races_register_model(shape: Sweep) {
    /// Killed between `fill_slot`'s CAS and its publish: slot 0 `RESERVED`, byte released.
    const P_CORPSE: u32 = 0xDEAD;
    /// The joiner the owner granted slot 1.
    const P_JOINER: u32 = 7;
    const T_JOINER: u64 = 2;
    /// Lock-byte states; the byte index equals the record index (`docs/decisions/0005`, 0028 step 0c).
    const BYTE_FREE: u32 = 0;
    const BYTE_HELD: u32 = 1;

    /// One reclamation decision. Returns whether the CAS fired, and whether the sweeper
    /// read a byte free under a published `live_word` (the ordering violation itself).
    fn sweep(
        table: &[ParticipantRecord],
        bytes: &[AtomicU32],
        slot: usize,
        shape: Sweep,
    ) -> (bool, bool) {
        let (observed, held) = match shape {
            Sweep::WordThenByte => {
                let observed = table[slot].state.load(Ordering::Acquire);
                (observed, bytes[slot].load(Ordering::Relaxed) != BYTE_FREE)
            }
            Sweep::ByteThenWord => {
                let held = bytes[slot].load(Ordering::Relaxed) != BYTE_FREE;
                (table[slot].state.load(Ordering::Acquire), held)
            }
            Sweep::WordRelaxedThenByte => {
                let observed = table[slot].state.load(Ordering::Relaxed);
                (observed, bytes[slot].load(Ordering::Relaxed) != BYTE_FREE)
            }
        };
        if observed == FREE || held {
            return (false, false);
        }
        let fired = ParticipantTable::new(table).reclaim(slot as u32, observed);
        (fired, state_of(observed) == LIVE)
    }

    model(move || {
        let table = Arc::new(alloc::vec![
            ParticipantRecord::default(),
            ParticipantRecord::default()
        ]);
        let bytes = Arc::new(alloc::vec![
            AtomicU32::new(BYTE_FREE),
            AtomicU32::new(BYTE_FREE)
        ]);

        // Slot 0's corpse: `register` never collects it.
        table[0].pid.store(P_CORPSE, Ordering::Relaxed);
        table[0].state.store(RESERVED, Ordering::Release);

        // A: a joiner granted slot 1 takes its byte before writing (0028 step 0b).
        let ta = Arc::clone(&table);
        let ba = Arc::clone(&bytes);
        let joiner = thread::spawn(move || {
            assert!(
                ba[1]
                    .compare_exchange(BYTE_FREE, BYTE_HELD, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok(),
                "nobody else takes byte 1 in this model"
            );
            ParticipantTable::new(&ta).register_at(1, P_JOINER, T_JOINER, 0)
        });

        // B: a peer reaping the whole table; it must not touch the live joiner.
        let tb = Arc::clone(&table);
        let bb = Arc::clone(&bytes);
        let sweeper = thread::spawn(move || (sweep(&tb, &bb, 0, shape), sweep(&tb, &bb, 1, shape)));

        let registered = joiner.join().unwrap();
        let ((corpse_fired, corpse_bad_order), (_, joiner_bad_order)) = sweeper.join().unwrap();

        let t = ParticipantTable::new(&table);
        let inc = registered.expect("slot 1 was granted to this joiner alone");
        assert_eq!(
            t.identity(1),
            Some((P_JOINER, T_JOINER, inc)),
            "no erasure: a joiner that returned Ok was left without its record"
        );
        assert!(
            !(corpse_bad_order || joiner_bad_order),
            "ordering: a byte read free under a published live_word, so the word \
             was not observed first"
        );
        // Harness liveness: fails a model in which nothing under test runs.
        assert!(
            corpse_fired,
            "the widened CAS never fired: nothing was tested"
        );
        assert_eq!(t.get(0).unwrap().state.load(Ordering::Acquire), FREE);
    });
}
