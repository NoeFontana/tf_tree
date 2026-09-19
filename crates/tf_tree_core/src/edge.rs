//! Edge records, the claim table, and the exclusive-writer `Publisher` handle.
//!
//! `unsafe`-free: raw arena access to these records lives in
//! [`crate::arena_view`]. The claim protocol is `docs/PHASE1.md` §5.4 /
//! `docs/PROJECT.md` §5 D7.

use core::marker::PhantomData;

use tf_tree_math::Iso3;

use crate::buffer::SampleRing;
use crate::crash::crash_point;
use crate::error::{ClaimError, EdgeId, PushError};
use crate::sync::{AtomicI64, AtomicU64, Ordering};

/// Discriminant stored in [`EdgeRecord::kind`].
///
/// Not `#[non_exhaustive]` (see [`crate::plan::InterpPolicy`]); [`EdgeKind::from_u8`]
/// absorbs unknown discriminants. Facade path: `tf_tree::unstable::EdgeKind`
/// (`docs/API.md` §2.6).
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
    /// Decode [`EdgeRecord::kind`]; any undefined value maps to [`EdgeKind::Tombstone`].
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
/// `#[repr(C, align(64))]`, **exactly 128 bytes** (the arena edge stride);
/// `docs/PHASE1.md` §5.3's field list trimmed at `_pad2` to fit.
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
    /// Declared publication rate, in **milli-hertz** (`docs/PHASE5.md` §1.2).
    ///
    /// `0` means "not declared", **not** 0 Hz (`docs/PHASE5.md` §6 `TFT007`).
    /// Set from `tf_tree::EdgeCfg::nominal_rate_hz`; an edge sized by an explicit
    /// slot count leaves it 0. Milli-hertz so 0.1 Hz is expressible.
    pub nominal_rate_mhz: u32,
    /// Named so it is initialised: `write_frozen` memcpys this record.
    _pad1: [u8; 4],
    /// Monotone total samples published (invariant 5).
    pub head: AtomicU64,
    /// Inline pose for static edges (`f64` bit patterns; see [`Iso3::to_bits`]).
    pub static_pose: [u64; 7],
    /// The participant slot that **declared** this edge (§1.2).
    ///
    /// Distinct from the *claim*, which moves with writers. `u32::MAX` means
    /// unknown (what the builder writes).
    pub declared_by_slot: u32,
    _pad2: [u8; 28],
}

// `size_of` is not a layout: a field reorder passes every size check and
// `layout_hash` yet changes what shared-segment and `.tft` bytes mean. These
// pins fix the offsets; appending a field is fine, moving one is a format break.
// Neighbouring gap: `docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md`.
#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<EdgeRecord>() == 128);
    assert!(core::mem::align_of::<EdgeRecord>() == 64);
    assert!(core::mem::offset_of!(EdgeRecord, parent) == 0);
    assert!(core::mem::offset_of!(EdgeRecord, child) == 4);
    assert!(core::mem::offset_of!(EdgeRecord, kind) == 8);
    assert!(core::mem::offset_of!(EdgeRecord, interp) == 9);
    assert!(core::mem::offset_of!(EdgeRecord, domain) == 10);
    assert!(core::mem::offset_of!(EdgeRecord, capacity) == 12);
    assert!(core::mem::offset_of!(EdgeRecord, stamp_off) == 16);
    assert!(core::mem::offset_of!(EdgeRecord, pose_off) == 20);
    assert!(core::mem::offset_of!(EdgeRecord, nominal_rate_mhz) == 24);
    assert!(core::mem::offset_of!(EdgeRecord, head) == 32);
    assert!(core::mem::offset_of!(EdgeRecord, static_pose) == 40);
    assert!(core::mem::offset_of!(EdgeRecord, declared_by_slot) == 96);
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
            nominal_rate_mhz: 0,
            _pad1: [0; 4],
            head: AtomicU64::new(0),
            static_pose: [0; 7],
            declared_by_slot: u32::MAX,
            _pad2: [0; 28],
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
            nominal_rate_mhz: 0,
            _pad1: [0; 4],
            head: AtomicU64::new(0),
            static_pose: pose,
            declared_by_slot: u32::MAX,
            _pad2: [0; 28],
        }
    }
}

/// Per-edge claim record — the exclusive-writer lock (invariant 4 / D7).
///
/// # Layout
///
/// `#[repr(C, align(64))]`, exactly 64 bytes. `docs/PHASE1.md` §5.4's plain
/// integers are atomics of identical layout so a failing claimer's diagnostic
/// read is UB-free.
#[cfg(not(loom))]
#[repr(C, align(64))]
pub struct ClaimRecord {
    /// `0` = free, else `(epoch << 16) | (participant_slot + 1)` as built by
    /// `pack_owner`. One word carries both state and identity (`docs/PHASE2.md`
    /// §1, A3): the identity indexes a participant record written at attach time,
    /// so no crash window leaves a held claim with no resolvable owner.
    pub owner: AtomicU64,
    /// Bumped on every successful claim **and every reap**.
    ///
    /// Fences a zombie writer (A4): a `Publisher` re-checks its claim epoch on
    /// every push.
    pub epoch: AtomicU64,
    /// Advisory liveness hint, bumped by the writer on every push. **Never a
    /// reaping trigger on its own** (`docs/PHASE2.md` §6.4).
    pub heartbeat: AtomicU64,
    /// The publisher's **clock offset**, in nanoseconds: host wall clock minus
    /// the header stamp, both read at the same push. Diagnostics only, and
    /// **never a reaping trigger** (`docs/PHASE2.md` §6.4).
    ///
    /// `docs/PHASE5.md` §6's `TFT004` compares it across publishers. The writer
    /// stores the difference, not a receipt time, because only the writer holds
    /// both sides at one instant (sampled writes make a reader-side receipt
    /// unpairable with the newest stamp; `docs/decisions/0036`).
    ///
    /// # Reading it
    ///
    /// `0` means **no sample yet**; a fresh claim clears it. **Both sides must
    /// share an epoch**, which this field cannot check (`TFT005`). This `no_std`
    /// crate never writes it (D14).
    pub clock_offset_nanos: AtomicI64,
    _pad: [u8; 32],
}

// As above. `heartbeat`/`clock_offset_nanos` are the same width, so a swap
// changes no size and no `layout_hash`; both are diagnostics-only (`docs/PHASE2.md` §6.4).
#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<ClaimRecord>() == 64);
    assert!(core::mem::align_of::<ClaimRecord>() == 64);
    assert!(core::mem::offset_of!(ClaimRecord, owner) == 0);
    assert!(core::mem::offset_of!(ClaimRecord, epoch) == 8);
    assert!(core::mem::offset_of!(ClaimRecord, heartbeat) == 16);
    assert!(core::mem::offset_of!(ClaimRecord, clock_offset_nanos) == 24);
};

/// Under `loom`, a heap struct of loom atomics with only the fields the claim
/// protocol touches.
#[cfg(loom)]
pub struct ClaimRecord {
    /// As in the production record.
    pub owner: AtomicU64,
    /// Claim epoch; bumped on claim and on reap.
    pub epoch: AtomicU64,
    /// Writer heartbeat.
    pub heartbeat: AtomicU64,
    /// Clock offset at the last sampled push.
    pub clock_offset_nanos: AtomicI64,
}

impl ClaimRecord {
    /// A fresh, unclaimed record, for heap claim slots in tests.
    #[must_use]
    pub fn new() -> ClaimRecord {
        #[cfg(not(loom))]
        {
            ClaimRecord {
                owner: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
                heartbeat: AtomicU64::new(0),
                clock_offset_nanos: AtomicI64::new(0),
                _pad: [0; 32],
            }
        }
        #[cfg(loom)]
        {
            ClaimRecord {
                owner: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
                heartbeat: AtomicU64::new(0),
                clock_offset_nanos: AtomicI64::new(0),
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
/// On success returns `(epoch, owner_word)`; the caller re-checks the epoch on
/// every push (A4). Exactly one of any set of racing claimers succeeds
/// (loom-tested); a claimer killed at any instruction leaves the edge free or
/// owned by a resolvable participant (`docs/PHASE2.md` §1, A3).
///
/// # Errors
///
/// [`ClaimError::EdgeAlreadyClaimed`] if held; `owner_slot` is a participant slot.
pub fn claim(rec: &ClaimRecord, participant_slot: u32) -> Result<(u64, u64), ClaimError> {
    // `CLAIMING` is a word no participant can hold, so a reaper clears a claimer
    // killed before the store below (cf. A6's `RESERVED`).
    rec.owner
        .compare_exchange(0, CLAIMING, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|held| ClaimError::EdgeAlreadyClaimed {
            owner_slot: slot_of(held),
        })?;
    let epoch = rec.epoch.fetch_add(1, Ordering::AcqRel) + 1;
    let word = pack_owner(epoch, participant_slot);
    rec.owner.store(word, Ordering::Release);

    // §11.3 `claim.after_cas`: after the owner word is installed, so the state is
    // a claim reapable via slot indirection (A3); no `Publisher` exists to `Drop`.
    crash_point!("claim.after_cas");

    Ok((epoch, word))
}

/// Owner word for a mid-claim record: no epoch, no valid slot.
const CLAIMING: u64 = u64::MAX;

/// `(epoch, slot + 1)` packed into the owner word.
///
/// The epoch is in the word so every acquisition's word is distinct: a stale
/// `Publisher` dropped after a same-slot re-claim (`ClaimRevoked`, then re-claim)
/// must not match, and free, the new claim.
#[inline]
#[must_use]
fn pack_owner(epoch: u64, participant_slot: u32) -> u64 {
    (epoch << 16) | (u64::from(participant_slot) + 1)
}

/// The participant slot named by an owner word, or `u32::MAX` if it names none
/// (free, or a claim still in flight).
///
/// Public for reapers: comparing a whole word against `slot + 1` matches only
/// at epoch 0, which `claim` never produces
/// (`a_reaper_does_not_reap_its_own_live_claim`).
#[inline]
#[must_use]
pub fn slot_of(word: u64) -> u32 {
    if word == 0 || word == CLAIMING {
        return u32::MAX;
    }
    u32::try_from((word & 0xFFFF).saturating_sub(1)).unwrap_or(u32::MAX)
}

/// Whether an owner word is a claim still in flight rather than a held one.
///
/// [`slot_of`] maps both "free" and "mid-claim" to `u32::MAX`; a caller with no
/// independent liveness source (unlike [`reap`], protected by `probe_claim`)
/// needs this to avoid reading `CLAIMING` as a dead owner.
#[inline]
#[must_use]
pub fn is_claiming(word: u64) -> bool {
    word == CLAIMING
}

/// Release a held claim. Idempotent at the memory level but should be called
/// exactly once, by the owner, via `Publisher::drop`.
pub fn release(rec: &ClaimRecord, owner: u64) {
    // A CAS, not a store: a stale `Publisher` (reaped, slot re-claimed) must not
    // free the new owner's claim (A4). The word carries the epoch, so it is
    // unique per acquisition.
    let _ = rec
        .owner
        .compare_exchange(owner, 0, Ordering::AcqRel, Ordering::Acquire);
}

/// Forcibly reclaim an edge whose owner is dead.
///
/// The epoch is bumped *before* the owner word is cleared (`docs/PHASE2.md`
/// §6.3), so a resumed zombie sees the change (A4). Idempotent beyond the bump;
/// racing reapers are harmless.
pub fn reap(rec: &ClaimRecord) {
    rec.epoch.fetch_add(1, Ordering::AcqRel);
    rec.owner.store(0, Ordering::Release);
}

/// Exclusive writer handle for one edge.
///
/// `Send + !Sync` (via a `PhantomData<Cell<()>>` marker), so single-writer is a
/// type-level property (D7). `Drop` releases the claim.
///
/// `Publisher` is `Send`:
/// ```
/// fn assert_send<T: Send>() {}
/// assert_send::<tf_tree_core::edge::Publisher<'static>>();
/// ```
///
/// but deliberately **not** `Sync` (this must fail to compile):
/// ```compile_fail,E0277
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<tf_tree_core::edge::Publisher<'static>>();
/// ```
///
/// The error code is pinned so the test cannot pass for the wrong reason; rustdoc
/// enforces it on nightly only (`just test-doc-error-codes`).
pub struct Publisher<'a> {
    ring: SampleRing<'a>,
    claim: &'a ClaimRecord,
    epoch: u64,
    /// The owner word from the claim ([`pack_owner`]); `Drop` releases with a
    /// compare-exchange on it ([`release`]).
    owner: u64,
    /// Set by [`Publisher::abandon`]; makes `Drop` touch no arena memory.
    abandoned: bool,
    // Projects `Send + !Sync` onto `Publisher` regardless of what its other
    // fields allow.
    _not_sync: PhantomData<core::cell::Cell<()>>,
}

impl<'a> Publisher<'a> {
    /// Wrap a freshly-won claim and its sample ring into a writer handle.
    ///
    /// `epoch` and `owner` are the values [`claim`] returned.
    #[must_use]
    pub fn new(
        ring: SampleRing<'a>,
        claim: &'a ClaimRecord,
        epoch: u64,
        owner: u64,
    ) -> Publisher<'a> {
        Publisher {
            ring,
            claim,
            epoch,
            owner,
            abandoned: false,
            _not_sync: PhantomData,
        }
    }

    /// Give up this claim **without releasing it**: dropping this writer then
    /// touches no arena memory.
    ///
    /// For the `std` facade after `fork()`, where the mapping is `MADV_DONTFORK`
    /// and a release would fault. The claim stays held (correctly: the forking
    /// process still owns it); otherwise it leaks until reaped, so use
    /// [`release`] to release.
    #[inline]
    pub fn abandon(&mut self) {
        self.abandoned = true;
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
        // A4: zombie-writer check. A writer stopped, reaped and resumed must not
        // publish into an edge another process now owns.
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
        if self.abandoned {
            return;
        }
        release(self.claim, self.owner);
    }
}
