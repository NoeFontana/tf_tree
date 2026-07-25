//! `loom` model-checked concurrency tests (run under `--cfg loom`).
//!
//! These are the hard gate for decision `0003` step 5: the publish/read/claim/
//! intern protocols must be sound under every interleaving loom explores, not
//! merely on x86. Buffers are capacity 2–4 and push counts <= 3 to keep the
//! state space tractable. Each test drives the *shared* algorithm code (the same
//! functions the production arena view calls) over heap-allocated instances
//! built from `crate::sync` (loom) atomics.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use loom::sync::Arc;
use loom::thread;

use tf_tree_math::{exp_se3, Iso3, LerpSlerp};

use crate::buffer::{PoseSlot, SampleRing};
use crate::edge::{claim, ClaimRecord};
use crate::error::{EdgeId, LookupError};
use crate::frame::intern_core;
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

/// Loom test 2: the writer wraps the ring (capacity 2, three pushes) while a
/// reader samples. The reader returns a valid sample or a documented error,
/// never a torn or nonsensical one.
#[test]
fn writer_wraps_reader_gets_valid_or_recycled() {
    loom::model(|| {
        let hr = Arc::new(HeapRing::new(2));

        let w = Arc::clone(&hr);
        let writer = thread::spawn(move || {
            let ring = w.ring();
            ring.push(10, &pose(1)).unwrap();
            ring.push(20, &pose(2)).unwrap();
            ring.push(30, &pose(3)).unwrap(); // wraps: overwrites slot 0
        });

        let r = Arc::clone(&hr);
        let reader = thread::spawn(move || {
            let ring = r.ring();
            match ring.sample::<LerpSlerp>(15, ExtrapPolicy::Error) {
                Ok(iso) => {
                    // A consistent result: every component must be finite.
                    let b = iso.to_bits();
                    for w in b {
                        assert!(f64::from_bits(w).is_finite(), "non-finite component");
                    }
                }
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

/// Loom test 3: two threads racing `intern` on the same name get the same
/// `FrameId`.
#[test]
fn intern_race_same_id() {
    loom::model(|| {
        // Interning table: 4 hash slots (mask 3), capacity 3 usable frames.
        let hashes = Arc::new({
            let mut v = alloc::vec::Vec::new();
            for _ in 0..4 {
                v.push(AtomicU64::new(0));
            }
            v
        });
        // Zero-initialized, exactly like the production arena (`alloc_zeroed`).
        // Seeding this with a different sentinel is what let the model check pass
        // while the real publish-then-spin handshake was inert: nothing in the
        // arena ever writes a non-zero "unpublished" marker.
        let ids = Arc::new({
            let mut v = alloc::vec::Vec::new();
            for _ in 0..4 {
                v.push(AtomicU32::new(crate::frame::ID_UNPUBLISHED));
            }
            v
        });
        let count = Arc::new(AtomicU32::new(0));
        let hash: u64 = 0xdead_beef_0000_0001;

        let spawn_one = |hashes: Arc<_>, ids: Arc<_>, count: Arc<AtomicU32>| {
            thread::spawn(move || {
                let hashes: &alloc::vec::Vec<AtomicU64> = &hashes;
                let ids: &alloc::vec::Vec<AtomicU32> = &ids;
                intern_core(hashes, ids, &count, 3, hash, |_| true, |_| {}).unwrap()
            })
        };

        let t1 = spawn_one(Arc::clone(&hashes), Arc::clone(&ids), Arc::clone(&count));
        let t2 = spawn_one(Arc::clone(&hashes), Arc::clone(&ids), Arc::clone(&count));
        let id1 = t1.join().unwrap();
        let id2 = t2.join().unwrap();

        assert_eq!(id1, id2, "concurrent intern of same name diverged");
        assert_eq!(id1, 1);
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "one distinct name -> one id"
        );
    });
}

/// Loom test 4: two threads racing `claim` on the same edge — exactly one wins.
#[test]
fn claim_race_exactly_one_wins() {
    loom::model(|| {
        let rec = Arc::new(ClaimRecord::new());

        let a = Arc::clone(&rec);
        let t1 = thread::spawn(move || claim(&a, 1, 10).is_ok());
        let b = Arc::clone(&rec);
        let t2 = thread::spawn(move || claim(&b, 2, 20).is_ok());

        let ok1 = t1.join().unwrap();
        let ok2 = t2.join().unwrap();
        assert!(ok1 ^ ok2, "expected exactly one claim to succeed");
        // The winner incremented the epoch exactly once.
        assert_eq!(rec.claim_epoch.load(Ordering::Relaxed), 1);
    });
}

/// A two-block topology seqlock model, mirroring `topology::TopologyView` but
/// with wide atomics so loom can model it (the production `depth` is an
/// `AtomicU16`, which the loom build does not provide).
struct TopoModel {
    generation: AtomicU64,
    active: AtomicU32,
    parent: [AtomicU32; 2],
    depth: [AtomicU32; 2],
}

const P_OLD: u32 = 5;
const D_OLD: u32 = 1;
const P_NEW: u32 = 9;
const D_NEW: u32 = 3;

impl TopoModel {
    fn new() -> TopoModel {
        TopoModel {
            generation: AtomicU64::new(0),
            active: AtomicU32::new(0),
            parent: [AtomicU32::new(P_OLD), AtomicU32::new(0)],
            depth: [AtomicU32::new(D_OLD), AtomicU32::new(0)],
        }
    }

    /// Writer protocol, identical in structure to `TopologyView::set_parent`.
    fn mutate(&self) {
        let g = self.generation.load(Ordering::Relaxed);
        self.generation.store(g + 1, Ordering::Release); // odd: unstable
        let inactive = (1 - self.active.load(Ordering::Relaxed)) as usize;
        self.parent[inactive].store(P_NEW, Ordering::Relaxed);
        self.depth[inactive].store(D_NEW, Ordering::Relaxed);
        self.active.store(inactive as u32, Ordering::Release);
        self.generation.store(g + 2, Ordering::Release); // even: stable
    }

    /// Reader protocol, identical in structure to `TopologyView::read_frame`.
    fn read(&self) -> (u32, u32) {
        loop {
            let g1 = self.generation.load(Ordering::Acquire);
            if g1 & 1 != 0 {
                crate::sync::spin();
                continue;
            }
            let blk = self.active.load(Ordering::Acquire) as usize;
            let parent = self.parent[blk].load(Ordering::Relaxed);
            let depth = self.depth[blk].load(Ordering::Relaxed);
            crate::sync::fence(Ordering::Acquire);
            if self.generation.load(Ordering::Relaxed) == g1 {
                return (parent, depth);
            }
        }
    }
}

/// Loom test 5: a topology mutation concurrent with a topology read. The reader
/// sees the old pair or the new pair, never a mix.
#[test]
fn topology_read_sees_old_or_new_never_mixed() {
    loom::model(|| {
        let topo = Arc::new(TopoModel::new());

        let w = Arc::clone(&topo);
        let writer = thread::spawn(move || w.mutate());

        let r = Arc::clone(&topo);
        let reader = thread::spawn(move || r.read());

        writer.join().unwrap();
        let (parent, depth) = reader.join().unwrap();
        assert!(
            (parent, depth) == (P_OLD, D_OLD) || (parent, depth) == (P_NEW, D_NEW),
            "mixed topology read: ({parent}, {depth})"
        );
    });
}
