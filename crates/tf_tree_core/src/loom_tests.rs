//! `loom` model-checked concurrency tests (run under `--cfg loom`).
//!
//! The hard gate for step 5 of `docs/PHASE1.md` and its §10.2 *Concurrency
//! (loom)*. Capacity-4 buffers and <= 5 pushes keep the state space tractable,
//! but never so small that the code under test becomes unreachable — a failure
//! mode these models have already had (see
//! [`writer_wraps_reader_gets_valid_or_recycled`]). Each test drives the shared
//! algorithm code over heap instances built from `crate::sync` (loom) atomics.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use loom::sync::Arc;
use loom::thread;

use tf_tree_math::{exp_se3, Iso3, LerpSlerp};

use crate::buffer::{PoseSlot, SampleRing};
use crate::edge::{claim, ClaimRecord};
use crate::error::{EdgeId, LookupError};
use crate::frame::{intern_core, InternTable, CLAIM_UNRECORDED};
use crate::participant::{state_of, ParticipantRecord, ParticipantTable, FREE, LIVE, RESERVED};
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

/// Run a `loom` model under a preemption bound that the environment can raise
/// but **cannot lower**.
///
/// `loom::model` treats an unset `LOOM_MAX_PREEMPTIONS` as unbounded, which is
/// weaker, not stronger: on
/// `two_mutators_race_the_lock_and_a_reader_sees_no_mix` with its liveness
/// predicate deliberately broken, unbounded search missed the violation after
/// 8143 ms (as did bounds 0-2) while a bound of 3 caught it in 373 ms. The
/// bound was load-bearing and lived outside the code — `xtask loom` set 3, a
/// hand-run `cargo test` silently got the weaker, slower check.
fn model(f: impl Fn() + Sync + Send + 'static) {
    /// Enough to reach the topology lock's steal path, which needs the spin
    /// budget (`MODEL_SPIN_LIMIT`) exhausted while another thread holds the
    /// word. Raising it is safe; lowering it silently removes that path.
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
            // Slot 1 is written exactly once (the second push, pose(2)), so a
            // consistent read is the initial zero pose or pose(2), never a mix.
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
/// Capacity 4 and five pushes, not 2 and three: [`SampleRing::retained`] is
/// `capacity - 1`, so a capacity-2 ring has `t_old == t_new` and the bracket
/// search, interpolation and `head - i > retained` revalidation — this test's
/// whole subject — are unreachable; that was its shape until a `panic!` planted
/// in the `Ok` arm never fired. Five into four is the smallest ring that laps a
/// reader with something left to interpolate. The assertion is bit equality,
/// not finiteness: a spliced pose is finite, and whatever has landed the only
/// bracket around `t = 25` is `(20, 30)`.
#[test]
fn writer_wraps_reader_gets_valid_or_recycled() {
    model(|| {
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

/// Loom test 3 (`docs/PHASE1.md` §10.2's *mutation test*): the invariant
/// `head`'s `Release` store carries — that observing `head == h` makes the
/// stamps of all `h` published samples visible.
///
/// §10.2 requires each §6.2/§6.3 ordering weakened to `Relaxed` to break a
/// model, and a survivor to be investigated. Four of five die against the
/// models above; [`SampleRing::push`]'s `head.store(h + 1, Release)` survived,
/// because neither reads a *stamp* — and no `sample`-shaped fixture could:
/// `push`'s `fence(Release)` sits before that push's stamp store, so `head ==
/// h` guarantees only stamps `0 ..= h-2`, leaving `sample`'s `t_new`
/// (`stamps[h-1]`) unprotected, and a stale `t_new` — zero sentinel or lapped
/// older stamp — is always *below* the fresh one, so every `t` above it exits
/// through the tolerated `Extrapolation` arm, and reaching a bit-equality
/// assertion would need `t < 0`, pinning the sentinel rather than the ordering.
/// Hence the direct assertion in the reader's own shape: [`SampleRing::sample`]
/// loads `head` `Acquire` then the stamps `Relaxed`, and `stamp_at`'s doc rests
/// that `Relaxed` on this edge. **Mutation-verified**: `Relaxed` here fails it,
/// `Release` passes.
#[test]
fn head_publishes_every_stamp_below_it() {
    model(|| {
        let hr = Arc::new(HeapRing::new(4));

        let w = Arc::clone(&hr);
        let writer = thread::spawn(move || {
            let ring = w.ring();
            // Four pushes into four slots: nothing laps, so a stale read can
            // only be the zero-initialised value and the assertion is exact.
            for i in 1..=4u64 {
                ring.push(i as i64 * 10, &pose(i)).unwrap();
            }
        });

        let r = Arc::clone(&hr);
        let reader = thread::spawn(move || {
            // Exactly `sample`'s first two steps: head Acquire, stamps Relaxed.
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

/// The interning table's three parallel arrays plus its id allocator, on the
/// heap from loom atomics — the shape `ArenaView` hands `intern_core`.
///
/// Zero-initialized exactly like the production arena (`alloc_zeroed`): seeding
/// `ids` with a different "unpublished" sentinel once let this model pass while
/// the real publish-then-spin handshake was inert.
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
    model(|| {
        // Interning table: 4 hash slots (mask 3), capacity 3 usable frames.
        let t = Arc::new(HeapInternTable::new(4));
        let hash: u64 = 0xdead_beef_0000_0001;

        // Two *live* participants (slots 0 and 1, so `me` is 1 and 2), neither
        // stealable: `claimant_alive` always agrees, like production's default.
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
/// One thread wins the hash slot and is `SIGKILL`ed before publishing the id;
/// the other must still terminate with the name interned. Before A8 it spun
/// forever whenever the dead thread got there first. The dying thread is
/// open-coded because `intern_core` cannot be abandoned part-way; the two CASes
/// below are exactly its prefix up to the crash point.
#[test]
fn intern_takes_over_from_a_claimant_that_died_before_publishing() {
    /// Participant slot of the doomed interner, as stored in `claiming` (slot + 1).
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
                // Record the claim, then die: nothing reaches `ids[slot]`.
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
            // Liveness predicate: `DEAD` is gone, the rest run; production
            // injects the OFD-lock/`/proc` one (`docs/PHASE2.md` §5.1, §6.2).
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
        // Exactly one id however the race went: the dead claimant never reached
        // `frame_count`, and the takeover precedes the rescuer touching it.
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
        // The winner incremented the epoch exactly once.
        assert_eq!(rec.epoch.load(Ordering::Relaxed), 1);
    });
}

/// A model of the topology protocol as amended by `docs/PHASE2.md` §1 — A1's
/// packed word and A2's in-arena mutation lock — mirroring
/// `topology::{TopologyView, TopoLockView}` step for step.
///
/// A reimplementation because the lock word and topology blocks live in a
/// `#[repr(C)]` arena header that loom's atomics cannot inhabit (they carry
/// instrumentation state and are not constructible from zeroed bytes — the
/// constraint `buffer::PoseSlot` meets, and why `crate::topology` is
/// `#[cfg(not(loom))]`). Keep the two in step. Refuted: *"`depth` is an
/// `AtomicU16`, which loom does not provide"* — loom 0.7 exports it; the
/// `#[repr(C)]` reason is the one to answer before deleting this model.
///
/// The control that stops this being a theorem about itself: with `is_alive`
/// disabled, so every holder reads dead,
/// `two_mutators_race_the_lock_and_a_reader_sees_no_mix` fails with *"two
/// mutators inside the critical section"* while
/// [`a_dead_lock_holder_is_stolen_from_and_leaves_no_trace`] still passes — one
/// of the pair fails whichever way the predicate is broken. `MODEL_SPIN_LIMIT`
/// must stay small enough to reach the steal path, and the control fires only
/// under a preemption bound (8 s unbounded and missed; 0.37 s at 3): [`model`].
///
/// `MODEL_BLOCKS` matching [`tf_tree_arena::TOPO_BLOCKS`] is load-bearing: a
/// two-block draft let loom produce a reader observing `(P_OLD, D_A)`, a real
/// mix of generations. The `Relaxed` re-check proves only that nothing became
/// *visible here*, so what protects the reader is that a mutator never writes
/// the block being walked — `N` publications inside one read, reachable with
/// two blocks and not with four. A1's "four flips".
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

    /// A2's acquire: bounded spin, then steal if the holder is dead. `is_alive`
    /// is injected as in the real code — the lock never decides liveness.
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

    /// A1's writer, under A2's lock: mutate the *inactive* block, then publish
    /// with a **single store**: no odd state, so a crash leaves nothing seen.
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

    /// A1's reader — what plan compilation does. Wait-free: a writer leaves no
    /// state a reader must wait out.
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
/// lock while a third compiles a plan. Three properties, all failing without
/// A2: exactly one mutator at a time, asserted from inside the critical section
/// by `in_section`; every published generation accounted for, since two
/// mutators sharing a scratch block would lose one; and a reader that never
/// sees a mix, because `(parent, depth)` is written as a unit under the lock.
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
                // Contended. All alive here, so nothing is stealable.
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
        // Free however the race went: winners released, losers never held it.
        assert_eq!(topo.lock.load(Ordering::Relaxed), 0, "the lock leaked");
    });
}

/// Loom test 7 — the `topo.holding_lock` crash point (`docs/PHASE2.md` §11.3).
///
/// One participant takes the lock, scribbles on the inactive block as a
/// half-finished copy would, and dies without releasing or publishing. A second
/// must steal it and complete, leaving **no trace** — A2's claim that recovery
/// is a no-op, because A1 left the corpse nothing observable to undo. A reader
/// runs throughout and never sees the scribble: it went to a block the topology
/// word never pointed at. The death is staged inline, not as a thread, because
/// a corpse executes no further instruction; as a thread loom scheduled the
/// scribble *after* the rescuer published — a real hazard, but a different one
/// (liveness wrongly calling a stalled participant dead), which §6.2 fails safe
/// against and §6.1 removes once claims are kernel locks.
#[test]
fn a_dead_lock_holder_is_stolen_from_and_leaves_no_trace() {
    const P_GARBAGE: u32 = 0xDEAD;
    const D_GARBAGE: u32 = 0xBEEF;
    const P_NEW: u32 = 7;
    const D_NEW: u32 = 2;
    /// The dead participant's slot. `is_alive` reports only this one dead, so
    /// the test cannot pass by stealing indiscriminately.
    const DEAD_SLOT: u32 = 0;

    model(|| {
        let topo = Arc::new(TopoModel::new());

        // Participant 0 dies mid-copy: lock taken, scratch dirtied, no release.
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
        // The stealer's is the only mutation, and it landed whole: no rollback.
        let (generation, active) = unpack(topo.word.load(Ordering::Relaxed));
        assert_eq!(generation, 1, "exactly one mutation should have published");
        assert_eq!(topo.parent[active].load(Ordering::Relaxed), P_NEW);
        assert_eq!(topo.depth[active].load(Ordering::Relaxed), D_NEW);
    });
}

/// A late `release` racing a reap + re-`register` must not free the new tenant.
///
/// The sequential version in `tests.rs` cannot fail on the code this guards
/// against: the old guard loaded `incarnation` then CASed `state`, two words
/// apart, and the bug lives in the window between them. A is the departing
/// participant's late `release(slot, 1)`, B reaps the slot and hands it on, and
/// loom explores A reading "still incarnation 1", B completing the handover,
/// and A's CAS landing on the new occupant. Packing the incarnation into
/// `state` makes that harmless — one word, so the CAS decides "still LIVE and
/// still mine". The cost of getting it wrong is two live processes on one slot
/// index, after which the `slot + 1` owner encoding used by claims (A3) and the
/// topology lock (A2) no longer names one process.
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

        // A: the departing process finally gets around to detaching.
        let a = Arc::clone(&table);
        let late = thread::spawn(move || ParticipantTable::new(&a).release(0, 1));

        // B: a reaper decides it is gone and a new process takes the slot — the
        // same release (a reap *is* a release by somebody else), then register.
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
/// `docs/PHASE2.md` §3.7 has the owner hand each client a `participant_slot`
/// and `docs/decisions/0005` makes that integer double as the lock-file byte.
/// Two clients can be handed the same slot — owner bug, takeover mid-handshake,
/// a stale `HelloResponse` replayed after a reap — and the arena must say no.
/// Loom, not a sequential test: the window is between `register_at`'s CAS and
/// its release-store, which a sequential caller cannot enter. Restore the
/// pre-CAS shape (load `state`, compare to `FREE`, store `RESERVED`) and loom
/// finds both threads observing `FREE` and both publishing — two live processes
/// on one slot, which the `slot + 1` encoding behind A3 forbids.
#[test]
fn two_joiners_handed_the_same_slot_cannot_both_take_it() {
    const P_A: u32 = 101;
    const P_B: u32 = 202;

    model(|| {
        // Four slots, so a loser has somewhere it *could* have gone; it must
        // not, because `register_at` takes the named slot or nothing.
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
        // The winner *entire* — a torn publication would mix A's pid with B's.
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

/// A reclaimer sweeping the table while a joiner registers: **the caller's**
/// read order. The state word is observed before the lock byte is probed, and
/// no record a joiner published is erased.
///
/// Both asserted properties belong to the **caller** of
/// [`ParticipantTable::reclaim`], not to `reclaim`. *Ordering*: an `Acquire`
/// load returning a `live_word` synchronises-with `fill_slot`'s publishing
/// `Release` store, so the byte its holder took before that store must read
/// held to any probe after the load — a byte-first sweep, or one up-front
/// holder mask (the shape `LockFile::held_participants()` invites), has no such
/// edge. At bound 3 the sweeper sees slot 1 with a published `live_word` in 38
/// of 294 executions, byte held in every one. *No erasure*: a joiner that
/// returned `Ok` is never left `FREE`.
///
/// It does **not** pin `reclaim`'s own CAS guard — a claim made here until it
/// was measured: the sweeper reaches that CAS in zero of 294 executions at
/// bound 3 and zero of 781 at bound 5, so **a `reclaim` that ignored `observed`
/// entirely passes this model.** The guard is pinned by
/// [`crate::tests::reclaim_fails_when_the_observed_word_has_changed`], the
/// *strength* of its orderings by nothing in this workspace (a gap argued on
/// `reclaim` itself). "No two occupants" is not asserted either: two
/// independent models were vacuous for it (0028 question 6 — a byte-blind
/// reclaimer passed 1 140 088 executions while erasing 151 590 `LIVE` records),
/// because an exclusive byte entails it — the same non-C11 fact that makes
/// widening `reclaim` to `RESERVED` safe, a writer holding its byte across all
/// of `fill_slot` (0028 step 0b) with byte index and record index one integer
/// (step 0c).
///
/// 0028's plan requires the failing control to ship, so both are runnable
/// `#[test]`s ([`control_reclaim_races_register_probes_the_byte_first`],
/// [`control_reclaim_races_register_observes_relaxed`]): a model that quietly
/// stops being falsifiable fails the suite. Both witnesses are legal C11 —
/// nothing orders the byte's *initial* store after the joiner's acquisition.
///
/// Slot 0's corpse is reclaimed uncontended, keeping clear of loom 0.7.2's
/// `match_rmw_to_stores` over-approximation, under which a second writer to a
/// slot lets a `RESERVED -> FREE` CAS match a superseded store and report a
/// C11-illegal erasure: a pass here is sound, a failure needs its witness
/// hand-checked, and the same-slot race is not modelled at all. The byte is an
/// exclusive per-slot lock word, taken and never released (`Session` outlives
/// the `Tree`'s registration), probed `Relaxed` because `F_OFD_GETLK` is a
/// syscall and at least that strong. The corpse is staged inline, the idiom of
/// [`a_dead_lock_holder_is_stolen_from_and_leaves_no_trace`].
#[test]
fn reclaim_races_register() {
    reclaim_races_register_model(Sweep::WordThenByte);
}

/// The failing control: the byte is probed **before** the word is observed.
///
/// Loom reads byte 1 free before the joiner takes it, then CASes a live record
/// to `FREE`. `#[should_panic]` makes that erasure an assertion, not a claim.
#[test]
#[should_panic(expected = "no erasure")]
fn control_reclaim_races_register_probes_the_byte_first() {
    reclaim_races_register_model(Sweep::ByteThenWord);
}

/// The second failing control: the right read order, the word observed
/// `Relaxed`. It erases too — the property is carried by the synchronises-with
/// edge, not by source order, and this is how that is known.
#[test]
#[should_panic(expected = "no erasure")]
fn control_reclaim_races_register_observes_relaxed() {
    reclaim_races_register_model(Sweep::WordRelaxedThenByte);
}

/// How one sweep reads a slot: the protocol, and the two controls that break it.
#[derive(Clone, Copy)]
enum Sweep {
    /// The word (`Acquire`) first, then the byte — `reclaim`'s requirement.
    WordThenByte,
    /// The byte first. Control.
    ByteThenWord,
    /// The right order, `Relaxed` observation. Control.
    WordRelaxedThenByte,
}

/// The model behind [`reclaim_races_register`] and its two controls.
fn reclaim_races_register_model(shape: Sweep) {
    /// Killed between `fill_slot`'s CAS and its publishing store, leaving slot 0
    /// `RESERVED` with a byte the kernel released at its death.
    const P_CORPSE: u32 = 0xDEAD;
    /// The joiner the owner granted slot 1.
    const P_JOINER: u32 = 7;
    const T_JOINER: u64 = 2;
    /// Lock-byte states. `docs/decisions/0005` makes the byte index and the
    /// arena record index the same integer; 0028 step 0c asserts it.
    const BYTE_FREE: u32 = 0;
    const BYTE_HELD: u32 = 1;

    /// One reclamation decision, for one slot: whether the CAS fired, and
    /// whether a byte read free under a published `live_word` — the ordering
    /// violation, reported not acted on so the erasure assertion speaks first.
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

        // The corpse in slot 0. `register` only CASes from FREE, so before
        // `reclaim` existed such a slot was lost to everybody for ever.
        table[0].pid.store(P_CORPSE, Ordering::Relaxed);
        table[0].state.store(RESERVED, Ordering::Release);

        // A: a joiner granted slot 1. It takes its lock byte before it writes
        // anything to the arena — the invariant step 0b makes total.
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

        // B: a peer running `reap_participants` (0028 piece 4) over the table,
        // holding no byte. Slot 0 is a corpse; slot 1 must not be touched.
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
        // Harness liveness, not evidence about the guard: the corpse's CAS is
        // uncontended and fires even for a `reclaim` that ignored `observed`.
        // It is here so a model that runs nothing under test fails.
        assert!(
            corpse_fired,
            "the widened CAS never fired: nothing was tested"
        );
        assert_eq!(t.get(0).unwrap().state.load(Ordering::Acquire), FREE);
    });
}
