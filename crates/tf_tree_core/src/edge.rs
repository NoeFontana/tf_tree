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
    /// Declared publication rate, in **milli-hertz** (`docs/PHASE5.md` §1.2).
    ///
    /// `0` means "not declared" — **not** "declared as 0 Hz". The distinction is
    /// load-bearing: `docs/PHASE5.md` §6's `TFT007` compares an observed rate
    /// against this one, and reading the sentinel as a rate makes every
    /// undeclared edge deviate from it by infinity.
    ///
    /// Written at declaration time from `tf_tree::EdgeCfg::nominal_rate_hz`,
    /// which a topology file's `rate_hz` reaches through
    /// `tf_tree_bridge::TopologyConfig::builder`. An edge whose ring was sized
    /// by an explicit slot count declares no rate and leaves this 0.
    ///
    /// Milli-hertz rather than hertz because the rates that matter span
    /// 0.1 Hz (a map update) to 1 kHz (an IMU), and an integer hertz cannot
    /// express the low end.
    pub nominal_rate_mhz: u32,
    /// Monotone total samples published (invariant 5).
    pub head: AtomicU64,
    /// Inline pose for static edges (`f64` bit patterns; see [`Iso3::to_bits`]).
    pub static_pose: [u64; 7],
    /// The participant slot that **declared** this edge (§1.2).
    ///
    /// Distinct from the *claim*, which lives in the claim table and moves as
    /// writers come and go. This one does not move, so a diagnostic can say
    /// "this edge was declared by the node that is now gone" — which is a
    /// different fault from "this edge is unclaimed".
    ///
    /// `u32::MAX` means unknown, which is what a v3 arena built by this version
    /// writes; the builder has no participant identity at declaration time.
    pub declared_by_slot: u32,
    _pad2: [u8; 28],
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
            nominal_rate_mhz: 0,
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
/// `#[repr(C, align(64))]`, exactly 64 bytes. `owner_pid`/`owner_boot_id` are
/// documented in `docs/PHASE1.md` §5.4 as plain integers; they are modeled here
/// as atomics of identical layout so the failing claimer's diagnostic read is
/// UB-free (the spec does not pin the memory ordering of their publication).
#[cfg(not(loom))]
#[repr(C, align(64))]
pub struct ClaimRecord {
    /// `0` = free, else `(epoch << 16) | (participant_slot + 1)` as built by
    /// `pack_owner` — the `participant_slot + 1` shorthand `docs/PHASE2.md` §1
    /// A3 uses names only the low half of the word.
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
    /// `0` = free, else `(epoch << 16) | (participant_slot + 1)`, exactly as in
    /// the production record; `pack_owner` is shared between the two.
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
/// The reported `owner_slot` is a participant slot, which the facade resolves to
/// a PID through the participant table.
pub fn claim(rec: &ClaimRecord, participant_slot: u32) -> Result<(u64, u64), ClaimError> {
    // Win the record exclusively first. `CLAIMING` is distinguishable garbage,
    // not a plausible owner: a claimer killed before step 3 leaves a word no
    // participant could legitimately hold, so a reaper clears it on sight —
    // the same shape as A6's `RESERVED` participant slot.
    rec.owner
        .compare_exchange(0, CLAIMING, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|held| ClaimError::EdgeAlreadyClaimed {
            owner_slot: slot_of(held),
        })?;
    let epoch = rec.epoch.fetch_add(1, Ordering::AcqRel) + 1;
    let word = pack_owner(epoch, participant_slot);
    rec.owner.store(word, Ordering::Release);
    Ok((epoch, word))
}

/// Owner word for a mid-claim record: no epoch, no valid slot.
const CLAIMING: u64 = u64::MAX;

/// `(epoch, slot + 1)` packed into the owner word.
///
/// **The epoch is in the word, and that is the whole point.** A bare
/// `slot + 1` is constant per participant, not per acquisition, so this
/// sequence frees a live claim:
///
/// 1. P (slot 7) claims E; it is `SIGSTOP`ped and reaped.
/// 2. P resumes, `push` returns `ClaimRevoked` — and P does exactly what that
///    error documents: it re-claims. Same slot, so the *same* owner word.
/// 3. P drops the old `Publisher`. A release comparing only `slot + 1` matches
///    the new claim and frees it, while the new one is still publishing.
///
/// A third process then claims E and two writers share a single-writer ring:
/// the failure A4 exists to prevent, reached through `Drop`. Folding the epoch
/// in makes every acquisition's word distinct, so a stale release cannot match.
#[inline]
#[must_use]
fn pack_owner(epoch: u64, participant_slot: u32) -> u64 {
    (epoch << 16) | (u64::from(participant_slot) + 1)
}

/// The participant slot named by an owner word, or `u32::MAX` if it names none
/// (free, or a claim still in flight).
///
/// **Public because a reaper cannot do without it.** The word is
/// `(epoch << 16) | (slot + 1)` (A3, and #20's "one acquisition, not just one
/// slot"), so comparing a whole owner word against `slot + 1` matches only at
/// epoch 0 — which `claim` never produces, since it starts at 1. A reaper that
/// made that comparison would fail to recognise its *own* claims and revoke
/// them; `docs/decisions/0005` §6's pseudocode had exactly that bug, and
/// `a_reaper_does_not_reap_its_own_live_claim` is what found it.
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
/// **Public because [`slot_of`] deliberately erases the difference and some
/// callers need it back.** `slot_of` maps both "free" and "mid-claim" to
/// `u32::MAX`, which is right for anything that only wants to resolve an owner.
/// It is wrong for anything that draws a *conclusion* from failing to resolve
/// one: a record holding `CLAIMING` for a few instructions during a normal
/// handoff is indistinguishable, through `slot_of` alone, from one whose owner
/// slot has genuinely gone dead.
///
/// [`reap`] gets away without this because it consults an independent liveness
/// source — a claimer caught in that window is protected by `probe_claim`
/// reporting the lock still held. A caller with no such second source (a
/// snapshot-based diagnostic, say) must not treat `CLAIMING` as evidence of
/// anything, and needs this predicate to tell the two cases apart.
#[inline]
#[must_use]
pub fn is_claiming(word: u64) -> bool {
    word == CLAIMING
}

/// Release a held claim. Idempotent at the memory level but should be called
/// exactly once, by the owner, via `Publisher::drop`.
pub fn release(rec: &ClaimRecord, owner: u64) {
    // **A CAS, not a store.** An unconditional store frees whatever claim is
    // there, including one that belongs to somebody else.
    //
    // The sequence that breaks: P1 claims E, is `SIGSTOP`ped, is reaped, and P2
    // claims E. P1 resumes — `push` correctly refuses with `ClaimRevoked` (A4),
    // but dropping its now-stale `Publisher` would store `owner = 0` and free
    // *P2's* live claim. A third process could then claim E while P2 still holds
    // a `Publisher`: two writers on a single-writer ring, which is the exact
    // failure A4 exists to prevent, arriving through the back door.
    //
    // Comparing against our own owner word makes a stale release a no-op — and
    // the word carries the *epoch*, so it is unique per acquisition. Comparing
    // only `slot + 1` would still match a re-claim by the same participant,
    // which is exactly what `ClaimRevoked` tells a revoked writer to do.
    let _ = rec
        .owner
        .compare_exchange(owner, 0, Ordering::AcqRel, Ordering::Acquire);
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
/// ```compile_fail,E0277
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<tf_tree_core::edge::Publisher<'static>>();
/// ```
///
/// The error code is pinned so the negative test cannot pass for the wrong
/// reason: a bare `compile_fail` also succeeds when the type is renamed or
/// un-exported, which is the failure mode this repository's `Mutant:` notes
/// exist to prevent. **rustdoc enforces the code on nightly only** — measured:
/// mutating it to `E0599` fails `cargo +nightly test --doc -p tf_tree_core`
/// with *"Some expected error codes were not found"*, and passes on stable.
pub struct Publisher<'a> {
    ring: SampleRing<'a>,
    claim: &'a ClaimRecord,
    epoch: u64,
    /// The owner word this writer wrote when it claimed —
    /// `(epoch << 16) | (participant_slot + 1)`, as built by [`pack_owner`].
    ///
    /// Retained so `Drop` can release with a compare-exchange instead of a
    /// store, and therefore cannot free a claim that has since passed to
    /// somebody else. **The epoch is part of the word on purpose**: a bare
    /// `slot + 1` is constant per participant, so the same participant
    /// re-claiming after a `ClaimRevoked` would produce an identical word and a
    /// stale release would free the new claim. See [`pack_owner`] and
    /// [`release`].
    owner: u64,
    /// Set by [`Publisher::abandon`]; makes `Drop` touch no arena memory.
    abandoned: bool,
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

    /// Give up this claim **without releasing it**, so that dropping this
    /// writer performs no arena access whatsoever.
    ///
    /// # Why a `no_std` engine crate has this
    ///
    /// Releasing is a `compare_exchange` on the claim record — a *write* into
    /// the arena — and there is one situation where that write is not merely
    /// unnecessary but wrong: the memory is no longer there, or is no longer
    /// ours. The `std` facade hits it after a `fork()`, where the shared mapping
    /// is `MADV_DONTFORK` and the child holds a `Publisher` whose `claim`
    /// reference points into a hole in its address space. Dropping it faults,
    /// and the child dies in a destructor it never asked to run.
    ///
    /// This crate cannot detect that condition — it is `no_std` and has no
    /// notion of a process. It only has to be *tellable*, which is what this is.
    ///
    /// The claim stays held in the arena. That is the correct outcome in the
    /// case this exists for: the claim belongs to the process that forked, which
    /// is still alive and still writing to it. In any other use it leaks the
    /// claim until a reaper collects it, so **do not reach for this as a way to
    /// avoid a release** — [`release`] is that.
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
        if self.abandoned {
            return;
        }
        release(self.claim, self.owner);
    }
}
