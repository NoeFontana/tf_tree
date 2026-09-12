//! Edge records, the claim table, and the exclusive-writer `Publisher` handle.
//!
//! `unsafe`-free: raw arena access lives in [`crate::arena_view`]. The claim
//! protocol (`docs/PHASE1.md` §5.4; `docs/PROJECT.md` §5 D7) is one
//! `compare_exchange`; a second claim on a live edge errors, never wins.

use core::marker::PhantomData;

use tf_tree_math::Iso3;

use crate::buffer::SampleRing;
use crate::crash::crash_point;
use crate::error::{ClaimError, EdgeId, PushError};
use crate::sync::{AtomicI64, AtomicU64, Ordering};

/// Discriminant stored in [`EdgeRecord::kind`].
///
/// Not `#[non_exhaustive]`: see [`crate::plan::InterpPolicy`]. An arena field, so
/// facade-visible only as `tf_tree::unstable::EdgeKind` (`docs/API.md` §2.6).
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
    /// Decode the [`EdgeRecord::kind`] discriminant; undefined values map to
    /// [`EdgeKind::Tombstone`]. A zeroed slot reads `Dynamic`, and is only read
    /// for a slot that was actually declared.
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
/// Layout: `#[repr(C, align(64))]`, **exactly 128 bytes** for the frozen arena
/// edge stride. `docs/PHASE1.md` §5.3's nominal fields exceed that once `head` is
/// 8-aligned, so order and semantics are kept and the trailing pad trimmed.
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
    /// `0` = "not declared", **not** "0 Hz": §6's `TFT007` compares an observed
    /// rate against this, so reading the sentinel as a rate makes every undeclared
    /// edge deviate by infinity. Set from `tf_tree::EdgeCfg::nominal_rate_hz`; a
    /// ring sized by slot count leaves 0. Milli-hertz because rates span 0.1 Hz (a
    /// map) to 1 kHz (an IMU).
    pub nominal_rate_mhz: u32,
    /// Padding before `head`'s 8-byte alignment, named so a struct literal
    /// initialises it: `write_frozen` memcpys this record to disk.
    _pad1: [u8; 4],
    /// Monotone total samples published (invariant 5).
    pub head: AtomicU64,
    /// Inline pose for static edges (`f64` bit patterns; see [`Iso3::to_bits`]).
    pub static_pose: [u64; 7],
    /// The participant slot that **declared** this edge (§1.2). Unlike the claim
    /// it never moves, so a diagnostic can tell "declared by a node now gone" from
    /// "unclaimed". `u32::MAX` = unknown: the builder has no identity to write.
    pub declared_by_slot: u32,
    _pad2: [u8; 28],
}

// **`size_of` is not a layout.** Wire records — a peer maps them from the `memfd`,
// `write_frozen` memcpys them into a `.tft` a later build opens — so offsets are
// format, yet `size_of`, `align_of` and `layout_hash`'s strides all survive a
// field *reorder*: two builds would agree on `FORMAT_VERSION` and read each
// other's records wrong. Appending is fine; moving now fails to compile.
// `docs/decisions/0032` covers the equally hand-kept region table.
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
/// Layout: `#[repr(C, align(64))]`, exactly 64 bytes. `docs/PHASE1.md` §5.4 has
/// `owner_pid`/`owner_boot_id` as plain integers; atomics of identical layout
/// here, so the failing claimer's diagnostic read is UB-free.
#[cfg(not(loom))]
#[repr(C, align(64))]
pub struct ClaimRecord {
    /// `0` = free, else `(epoch << 16) | (participant_slot + 1)` from `pack_owner`
    /// (`docs/PHASE2.md` §1 A3's `slot + 1` shorthand names only the low half).
    /// **One word carries state and full identity** (A3), indirecting into a
    /// participant record written at attach. Phase 1 wrote `owner_pid` *after* the
    /// CAS: a writer killed in between left `state = HELD, owner_pid = 0`, held
    /// and reclaimable by nobody, leaking the edge for the arena's life.
    pub owner: AtomicU64,
    /// Bumped on every successful claim **and every reap** — the zombie fence
    /// (A4). A `Publisher` re-checks the epoch it claimed at on every push, so a
    /// process stopped, judged dead, reaped and resumed cannot write to an edge
    /// somebody else owns.
    pub epoch: AtomicU64,
    /// Advisory liveness hint, bumped by the writer on every push. **Never a
    /// reaping trigger on its own** (`docs/PHASE2.md` §6.4).
    pub heartbeat: AtomicU64,
    /// The publisher's **clock offset**, in nanoseconds: host wall clock minus
    /// the header stamp, read at the same push. Diagnostics only, **never a
    /// reaping trigger** (`docs/PHASE2.md` §6.4); `docs/PHASE5.md` §6's `TFT004`
    /// compares it across publishers to find a drifted clock. `no_std`, so this
    /// crate writes it nowhere (D14).
    ///
    /// The *writer* subtracts because only it holds both sides at one instant: a
    /// wall-clock read costs ~8× a push, so `EdgeWriter` samples one per second,
    /// and a reader's `receipt - newest_stamp` on a 10 Hz publisher with an exact
    /// clock ranges +3 µs to -900 ms by arrival alone — a ±1 s noise floor under
    /// a signal `TFT004` must resolve at tens of ms, and it does not cancel
    /// across a fleet (`docs/decisions/0036` retired `last_push_nanos`).
    ///
    /// `0` = **no sample yet**, cleared by a fresh claim, which inherits the edge
    /// and not the writer; a true zero therefore reads as unset — one in ~10^9,
    /// overwritten next push, cheaper than a sentinel a zeroed arena cannot
    /// express. **Both sides must share an epoch and this cannot check that**:
    /// where stamps are not Unix time it is an epoch difference, and a check
    /// must skip as `TFT005` does.
    pub clock_offset_nanos: AtomicI64,
    _pad: [u8; 32],
}

// `EdgeRecord`'s argument, same two routes. This block used to cover two of the
// four fields: `heartbeat` and `clock_offset_nanos` are the same width class, so
// swapping them changes neither size, alignment, nor `layout_hash` — measured, the
// swap builds warning-free with the suite green including the `.tft` fixture test,
// and two builds then read a monotone push count as a clock offset.
// Narrow radius (both diagnostics-only; `owner`/`epoch` were already pinned), but
// the pin is four lines that can only fail once the layout has moved.
#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<ClaimRecord>() == 64);
    assert!(core::mem::align_of::<ClaimRecord>() == 64);
    assert!(core::mem::offset_of!(ClaimRecord, owner) == 0);
    assert!(core::mem::offset_of!(ClaimRecord, epoch) == 8);
    assert!(core::mem::offset_of!(ClaimRecord, heartbeat) == 16);
    assert!(core::mem::offset_of!(ClaimRecord, clock_offset_nanos) == 24);
};

/// Under `loom`, a plain heap struct of loom atomics (not `repr(C)`) holding only
/// the fields the claim protocol touches; `claim`/`release` are identical.
#[cfg(loom)]
pub struct ClaimRecord {
    /// `0` = free, else `(epoch << 16) | (slot + 1)`; `pack_owner` is shared.
    pub owner: AtomicU64,
    /// Claim epoch; bumped on claim and on reap.
    pub epoch: AtomicU64,
    /// Writer heartbeat.
    pub heartbeat: AtomicU64,
    /// Clock offset at the last sampled push; see the production field.
    pub clock_offset_nanos: AtomicI64,
}

impl ClaimRecord {
    /// A fresh, unclaimed record. Builds heap claim slots for the loom tests; the
    /// production arena views zeroed bytes instead.
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
/// **One `compare_exchange` publishes both the held state and the owner's
/// identity** (`docs/PHASE2.md` §1, A3), so a claimer killed at any instruction
/// leaves the edge free or owned by a participant a reaper can resolve and probe.
/// The caller re-checks the returned epoch on every push (A4). Exactly one of a
/// set of racing claimers succeeds (loom-tested).
///
/// # Errors
///
/// [`ClaimError::EdgeAlreadyClaimed`] if held; `owner_slot` is a participant slot
/// the facade resolves to a PID.
pub fn claim(rec: &ClaimRecord, participant_slot: u32) -> Result<(u64, u64), ClaimError> {
    // Win the record exclusively first. `CLAIMING` is distinguishable garbage, not
    // a plausible owner: a claimer killed before step 3 leaves a word no
    // participant could hold, so a reaper clears it on sight (A6's `RESERVED`).
    rec.owner
        .compare_exchange(0, CLAIMING, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|held| ClaimError::EdgeAlreadyClaimed {
            owner_slot: slot_of(held),
        })?;
    let epoch = rec.epoch.fetch_add(1, Ordering::AcqRel) + 1;
    let word = pack_owner(epoch, participant_slot);
    rec.owner.store(word, Ordering::Release);

    // §11.3 `claim.after_cas`: "claim held by a dead participant -> reapable via
    // slot indirection (A3)". After the owner *word*, not the CAS: A3's single
    // publication of state-plus-identity finishes here, and the earlier `CLAIMING`
    // window is a different state with no §11.3 row. No `Publisher` exists yet, so
    // no `Drop` runs — the leak A3 makes repairable, and why §11.3 bans `panic!`.
    crash_point!("claim.after_cas");

    Ok((epoch, word))
}

/// Owner word for a mid-claim record: no epoch, no valid slot.
const CLAIMING: u64 = u64::MAX;

/// `(epoch, slot + 1)` packed into the owner word.
///
/// The epoch must be in it: a bare `slot + 1` is constant per participant, so P
/// claims E, is reaped, resumes, re-claims as `ClaimRevoked` documents — identical
/// word — then drops the old `Publisher`, whose release frees the *new* claim
/// mid-publish, letting a third process in. A4's failure, through `Drop`.
#[inline]
#[must_use]
fn pack_owner(epoch: u64, participant_slot: u32) -> u64 {
    (epoch << 16) | (u64::from(participant_slot) + 1)
}

/// The participant slot an owner word names; `u32::MAX` if none (free or
/// mid-claim).
///
/// Public because a reaper cannot do without it: the word is
/// `(epoch << 16) | (slot + 1)` (A3; #20's "one acquisition, not just one slot"),
/// so comparing a whole word against `slot + 1` matches only at epoch 0, which
/// `claim` never produces — such a reaper revokes its own live claims, the bug in
/// `docs/decisions/0005` §6's pseudocode that
/// `a_reaper_does_not_reap_its_own_live_claim` found.
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
/// Public because [`slot_of`] maps both "free" and "mid-claim" to `u32::MAX`: a
/// handoff's few instructions of `CLAIMING` then look exactly like an owner slot
/// that has genuinely died. [`reap`] needs no predicate because `probe_claim` is
/// an independent liveness source; a caller without one must not treat `CLAIMING`
/// as evidence of anything.
#[inline]
#[must_use]
pub fn is_claiming(word: u64) -> bool {
    word == CLAIMING
}

/// Release a held claim. Idempotent at the memory level but should be called
/// exactly once, by the owner, via `Publisher::drop`.
pub fn release(rec: &ClaimRecord, owner: u64) {
    // **A CAS, not a store.** After P1 is reaped and P2 claims E, P1's stale
    // `Publisher::drop` would store 0 and free P2's *live* claim, letting a third
    // process in alongside it — A4's failure by the back door. Our own word
    // carries the *epoch*, so it is unique per acquisition; `slot + 1` alone
    // would match the re-claim `ClaimRevoked` tells a revoked writer to make.
    let _ = rec
        .owner
        .compare_exchange(owner, 0, Ordering::AcqRel, Ordering::Acquire);
}

/// Forcibly reclaim an edge whose owner is dead.
///
/// **The epoch is bumped *before* the owner word is cleared** (`docs/PHASE2.md`
/// §6.3), closing the zombie window from both ends: a resuming writer sees a
/// changed epoch and refuses to push (A4), and cannot re-acquire it because the
/// next claimer bumps again. Idempotent, so racing reapers are harmless.
pub fn reap(rec: &ClaimRecord) {
    rec.epoch.fetch_add(1, Ordering::AcqRel);
    rec.owner.store(0, Ordering::Release);
}

/// Exclusive writer handle for one edge.
///
/// `Send + !Sync` (D7): moveable between threads, never shared, so "single writer
/// per edge" is a type-level property; `PhantomData<Cell<()>>` withholds `Sync`.
/// `Drop` releases the claim.
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
/// The code is pinned because a bare `compile_fail` also passes when the type is
/// renamed or un-exported. Mutant: `E0599` fails `cargo +nightly test --doc` yet
/// reports `ok` on stable, so `just test-doc-error-codes` (CI's `miri` job) gates
/// this line, not the stable `just test-doc`.
pub struct Publisher<'a> {
    ring: SampleRing<'a>,
    claim: &'a ClaimRecord,
    epoch: u64,
    /// The owner word written at claim time ([`pack_owner`]). Retained so `Drop`
    /// releases by compare-exchange, not store, and cannot free a claim that has
    /// passed to somebody else — see [`release`].
    owner: u64,
    /// Set by [`Publisher::abandon`]; makes `Drop` touch no arena memory.
    abandoned: bool,
    // `Cell<()>` is `Send + !Sync` — the auto-trait profile to project onto
    // `Publisher` regardless of what its other fields allow.
    _not_sync: PhantomData<core::cell::Cell<()>>,
}

impl<'a> Publisher<'a> {
    /// Wrap a freshly-won claim and its sample ring into a writer handle.
    ///
    /// `epoch` is [`claim`]'s return, retained so a reap/reclaim is detectable.
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
    /// Releasing writes to the arena, wrong after a `fork()`: the shared mapping
    /// is `MADV_DONTFORK`, so the child's `claim` points into a hole and `Drop`
    /// faults. A `no_std` crate cannot detect that, only be told. The claim stays
    /// held — right for the still-live forking process, elsewhere a leak until a
    /// reaper collects, so **do not use this to avoid a release**.
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
        // A4: the zombie-writer check, one Relaxed load on a cacheline this writer
        // already owns (~1 ns) and not optional — SIGSTOP, a GC pause or a slow
        // page fault gets a live writer judged dead and reaped, and on resume it
        // would tear samples against the new owner of a single-writer ring. `reap`
        // bumps the epoch before freeing the claim, closing the window both ends.
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
