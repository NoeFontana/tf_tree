//! Edge records, the claim table, and the exclusive-writer `Publisher` handle.
//!
//! `unsafe`-free: raw arena access to these records lives in
//! [`crate::arena_view`]. The claim protocol (`docs/PHASE1.md` §5.4;
//! `docs/PROJECT.md` §5 D7) is a single
//! `compare_exchange`; a second claim on a live edge is an error, never a silent
//! success.

use core::marker::PhantomData;

use tf_tree_math::Iso3;

use crate::buffer::SampleRing;
use crate::error::{ClaimError, EdgeId, PushError};
use crate::sync::{AtomicI64, AtomicU64, Ordering};

/// Discriminant stored in [`EdgeRecord::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EdgeKind {
    /// A dynamic edge backed by a sample ring.
    Dynamic = 0,
    /// A static edge whose pose lives inline in [`EdgeRecord::static_pose`].
    Static = 1,
    /// A tombstoned edge (removed; identity never recycled — invariant 1 / D10).
    Tombstone = 2,
}

impl EdgeKind {
    /// Decode the [`EdgeRecord::kind`] discriminant. Any value other than the
    /// three defined discriminants maps to [`EdgeKind::Tombstone`] (a zeroed edge
    /// slot has `kind == 0` = [`EdgeKind::Dynamic`], which is only ever read for a
    /// slot that was actually declared).
    #[inline]
    #[must_use]
    pub const fn from_u8(v: u8) -> EdgeKind {
        match v {
            0 => EdgeKind::Dynamic,
            1 => EdgeKind::Static,
            _ => EdgeKind::Tombstone,
        }
    }
}

/// Per-edge control record. `EdgeId` indexes the edge table.
///
/// # Layout
///
/// `#[repr(C, align(64))]`, **exactly 128 bytes** to match the frozen arena edge
/// stride (`max_edges * 128`). The nominal field list in `docs/PHASE1.md` §5.3
/// sums to more than 128 bytes once the `head` atomic is 8-aligned; this record
/// keeps the same field order and semantics and trims the trailing pad (`_pad2`)
/// so the whole thing lands on the 128-byte stride.
#[cfg(not(loom))]
#[repr(C, align(64))]
pub struct EdgeRecord {
    /// Parent frame index.
    pub parent: u32,
    /// Child frame index (the edge stores `T_parent_child`).
    pub child: u32,
    /// [`EdgeKind`] discriminant.
    pub kind: u8,
    /// Interpolation-policy discriminant.
    pub interp: u8,
    /// Time-domain id (D9).
    pub domain: u8,
    _pad0: u8,
    /// Ring capacity (power of two; `0` for static).
    pub capacity: u32,
    /// Element index of this edge's stamps within the stamp arena.
    pub stamp_off: u32,
    /// Element index of this edge's poses within the pose arena.
    pub pose_off: u32,
    _pad1: u32,
    /// Monotone total samples published (invariant 5).
    pub head: AtomicU64,
    /// Inline pose for static edges (`f64` bit patterns; see [`Iso3::to_bits`]).
    pub static_pose: [u64; 7],
    _pad2: [u8; 32],
}

#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<EdgeRecord>() == 128);
    assert!(core::mem::align_of::<EdgeRecord>() == 64);
};

#[cfg(not(loom))]
impl EdgeRecord {
    /// A fresh dynamic edge record with an empty ring. `stamp_off`/`pose_off` are
    /// element indices into the stamp/pose arenas; `capacity` is a power of two.
    #[must_use]
    pub fn dynamic(
        parent: u32,
        child: u32,
        capacity: u32,
        stamp_off: u32,
        pose_off: u32,
        interp: u8,
        domain: u8,
    ) -> EdgeRecord {
        EdgeRecord {
            parent,
            child,
            kind: EdgeKind::Dynamic as u8,
            interp,
            domain,
            _pad0: 0,
            capacity,
            stamp_off,
            pose_off,
            _pad1: 0,
            head: AtomicU64::new(0),
            static_pose: [0; 7],
            _pad2: [0; 32],
        }
    }

    /// A fresh static edge record carrying an inline pose (`f64` bit patterns).
    #[must_use]
    pub fn static_edge(parent: u32, child: u32, pose: [u64; 7], domain: u8) -> EdgeRecord {
        EdgeRecord {
            parent,
            child,
            kind: EdgeKind::Static as u8,
            interp: 0,
            domain,
            _pad0: 0,
            capacity: 0,
            stamp_off: 0,
            pose_off: 0,
            _pad1: 0,
            head: AtomicU64::new(0),
            static_pose: pose,
            _pad2: [0; 32],
        }
    }
}

/// Per-edge claim record — the exclusive-writer lock (invariant 4 / D7).
///
/// # Layout
///
/// `#[repr(C, align(64))]`, exactly 64 bytes. `owner_pid`/`owner_boot_id` are
/// documented in `docs/PHASE1.md` §5.4 as plain integers; they are modeled here
/// as atomics of identical layout so the failing claimer's diagnostic read is
/// UB-free (the spec does not pin the memory ordering of their publication).
#[cfg(not(loom))]
#[repr(C, align(64))]
pub struct ClaimRecord {
    /// `0` = free, else `participant_slot + 1`.
    ///
    /// **One word carries both the state and the full identity** (`docs/PHASE2.md`
    /// §1, A3), because the identity is an *indirection* into a participant
    /// record that was completely written at attach time, long before any claim.
    ///
    /// Phase 1 stored `state` and `owner_pid` separately and wrote the PID
    /// *after* winning the CAS. A writer killed in between left `state = HELD,
    /// owner_pid = 0` — held by nobody, reclaimable by nobody, and
    /// indistinguishable from a valid claim, so the edge leaked for the life of
    /// the arena. In one process that was unobservable; across processes it is a
    /// permanent resource leak.
    pub owner: AtomicU64,
    /// Bumped on every successful claim **and every reap**.
    ///
    /// This is what fences a zombie writer (A4): a `Publisher` records the epoch
    /// it claimed at and re-checks it on every push, so a process that was
    /// stopped, judged dead, reaped, and then resumed cannot write to an edge
    /// somebody else now owns.
    pub epoch: AtomicU64,
    /// Advisory liveness hint, bumped by the writer on every push. **Never a
    /// reaping trigger on its own** (`docs/PHASE2.md` §6.4).
    pub heartbeat: AtomicU64,
    /// Arena-local nanoseconds of the last push; diagnostics only.
    pub last_push_nanos: AtomicI64,
    _pad: [u8; 32],
}

#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<ClaimRecord>() == 64);
    assert!(core::mem::align_of::<ClaimRecord>() == 64);
};

/// Under `loom`, `ClaimRecord` is a plain heap struct of loom atomics (loom
/// atomics are not `repr(C)`), holding only the fields the claim protocol
/// touches. The `claim`/`release` algorithm is identical to the production one.
#[cfg(loom)]
pub struct ClaimRecord {
    /// `0` = free, else `participant_slot + 1`.
    pub owner: AtomicU64,
    /// Claim epoch; bumped on claim and on reap.
    pub epoch: AtomicU64,
    /// Writer heartbeat.
    pub heartbeat: AtomicU64,
    /// Arena-local nanoseconds of the last push.
    pub last_push_nanos: AtomicI64,
}

impl ClaimRecord {
    /// A fresh, unclaimed record. Used to build heap claim slots for the loom
    /// tests; the production arena views zeroed bytes instead of constructing.
    #[must_use]
    pub fn new() -> ClaimRecord {
        #[cfg(not(loom))]
        {
            ClaimRecord {
                owner: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
                heartbeat: AtomicU64::new(0),
                last_push_nanos: AtomicI64::new(0),
                _pad: [0; 32],
            }
        }
        #[cfg(loom)]
        {
            ClaimRecord {
                owner: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
                heartbeat: AtomicU64::new(0),
                last_push_nanos: AtomicI64::new(0),
            }
        }
    }
}

impl Default for ClaimRecord {
    fn default() -> Self {
        ClaimRecord::new()
    }
}

/// Attempt to claim exclusive write access to an edge, on behalf of
/// `participant_slot`.
///
/// **One `compare_exchange` publishes both the held state and the owner's
/// identity** (`docs/PHASE2.md` §1, A3). There is no window in which the edge is
/// held by an unidentified owner, so a claimer killed at any instruction leaves
/// the edge either free or owned by a participant record that a reaper can
/// resolve and check for liveness.
///
/// On success the epoch is incremented and returned; the caller stores it and
/// re-checks it on every push (A4).
///
/// Exactly one of any set of racing claimers succeeds (loom-tested).
///
/// # Errors
///
/// [`ClaimError::EdgeAlreadyClaimed`] if the edge is already held. The reported
/// `owner_pid` is the raw owner word — the participant slot plus one — which the
/// facade resolves to a PID through the participant table.
pub fn claim(rec: &ClaimRecord, participant_slot: u32) -> Result<u64, ClaimError> {
    let want = u64::from(participant_slot) + 1;
    match rec
        .owner
        .compare_exchange(0, want, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => Ok(rec.epoch.fetch_add(1, Ordering::AcqRel) + 1),
        Err(held) => Err(ClaimError::EdgeAlreadyClaimed {
            owner_pid: (held.saturating_sub(1)) as u32,
        }),
    }
}

/// Release a held claim. Idempotent at the memory level but should be called
/// exactly once, by the owner, via `Publisher::drop`.
pub fn release(rec: &ClaimRecord) {
    rec.owner.store(0, Ordering::Release);
}

/// Forcibly reclaim an edge whose owner is dead.
///
/// **The epoch is bumped *before* the owner word is cleared** (`docs/PHASE2.md`
/// §6.3), which closes the zombie window from both ends: a stopped writer that
/// resumes after this sees a changed epoch and refuses to push (A4), and it
/// cannot re-acquire the same epoch because the next claimer bumps it again.
///
/// Cooperative and idempotent: reaping an already-free edge is a no-op beyond
/// the epoch bump, so two reapers racing is harmless.
pub fn reap(rec: &ClaimRecord) {
    rec.epoch.fetch_add(1, Ordering::AcqRel);
    rec.owner.store(0, Ordering::Release);
}

/// Exclusive writer handle for one edge.
///
/// `Send + !Sync`: a writer may be moved between threads but never shared, so
/// "single writer per edge" is a type-level property, not a convention (D7). The
/// `!Sync` is enforced by the `PhantomData<Cell<()>>` marker (a `Cell` is `Send`
/// but not `Sync`). `Drop` releases the claim.
///
/// `Publisher` is `Send`:
/// ```
/// fn assert_send<T: Send>() {}
/// assert_send::<tf_tree_core::edge::Publisher<'static>>();
/// ```
///
/// but deliberately **not** `Sync` (this must fail to compile):
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<tf_tree_core::edge::Publisher<'static>>();
/// ```
pub struct Publisher<'a> {
    ring: SampleRing<'a>,
    claim: &'a ClaimRecord,
    epoch: u64,
    // `Cell<()>` is `Send + !Sync`, which is exactly the auto-trait profile we
    // want to project onto `Publisher` regardless of what its other fields allow.
    _not_sync: PhantomData<core::cell::Cell<()>>,
}

impl<'a> Publisher<'a> {
    /// Wrap a freshly-won claim and its sample ring into a writer handle.
    ///
    /// `epoch` is the value returned by [`claim`]; it is retained so a Phase 2
    /// reaper/reclaim can be detected.
    #[must_use]
    pub fn new(ring: SampleRing<'a>, claim: &'a ClaimRecord, epoch: u64) -> Publisher<'a> {
        Publisher {
            ring,
            claim,
            epoch,
            _not_sync: PhantomData,
        }
    }

    /// The claim epoch observed when this writer was created.
    #[inline]
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The edge this writer owns.
    #[inline]
    #[must_use]
    pub fn edge(&self) -> EdgeId {
        self.ring.edge
    }

    /// Publish one sample. Wait-free and allocation-free (invariant 8).
    ///
    /// # Errors
    ///
    /// [`PushError::NonMonotonicStamp`] if the stamp regresses (invariant 6).
    pub fn push(&self, stamp: i64, iso: &Iso3) -> Result<(), PushError> {
        // A4: the zombie-writer check. One Relaxed load, on a cacheline this
        // writer already owns and touches, so it costs about a nanosecond — and
        // it is not optional.
        //
        // A process stopped by SIGSTOP, a GC pause, or a page fault against a
        // slow device can be judged dead, have its claim reaped, and then
        // *resume*. Without this it would carry on publishing into an edge
        // another process now owns: two writers on a single-writer ring, tearing
        // each other's samples silently. That is precisely the failure the claim
        // model exists to prevent, so the model has to survive its own owner
        // being wrong about who is alive.
        //
        // `reap` bumps the epoch *before* freeing the claim, so the window is
        // closed from both ends.
        if self.claim.epoch.load(Ordering::Relaxed) != self.epoch {
            return Err(PushError::ClaimRevoked {
                edge: self.ring.edge,
            });
        }
        self.ring.push(stamp, iso)
    }
}

impl Drop for Publisher<'_> {
    fn drop(&mut self) {
        release(self.claim);
    }
}
