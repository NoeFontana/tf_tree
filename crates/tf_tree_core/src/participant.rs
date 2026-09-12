//! The participant table — who is attached, and are they still alive.
//!
//! `docs/PHASE2.md` §1 A6 and §5. A participant is a process that has mapped
//! the arena; its **slot index** is the identity everything else names, so a
//! claim (`participant_slot + 1`) and the topology lock each publish an owner
//! and its full identity in a *single* store. That indirection is A3: Phase 1
//! wrote `state` then `owner_pid` separately, and a `SIGKILL` between them left
//! `HELD` owned by pid 0 — an edge held by nobody, reclaimable by nobody.
//!
//! Identity is PID + start time (`/proc/<pid>/stat` field 22), never a bare
//! PID: PIDs wrap, and a reused number reads as alive forever.
//! [`crate::arena_view::ArenaView`]'s header carries the boot id that scopes
//! the pair (§5.1). `unsafe`-free: the record slice comes from
//! [`crate::arena_view`].

use crate::crash::crash_point;
use crate::sync::{AtomicI64, AtomicU32, AtomicU64, Ordering};

/// Slot is unused.
pub const FREE: u32 = 0;
/// Slot is being filled in by a registrant that has not published yet.
pub const RESERVED: u32 = 1;
/// Slot is fully written and its participant is attached.
///
/// Low 2 bits of `state`; the high 30 carry the incarnation, so
/// [`ParticipantTable::release`] can check "LIVE and still mine" in one CAS.
pub const LIVE: u32 = 2;

/// The `state` word for a live slot at `incarnation`.
///
/// A *guard* only — the record's `AtomicU64` stays authoritative. Aliasing
/// needs 2^30 re-registrations of one slot between a process's last
/// instruction and its `release`.
#[inline]
#[must_use]
pub fn live_word(incarnation: u64) -> u32 {
    ((incarnation as u32) << 2) | LIVE
}

/// The lifecycle state encoded in a `state` word.
#[inline]
#[must_use]
pub fn state_of(word: u32) -> u32 {
    word & 0b11
}

/// One participant's record. 128 bytes, matching the arena's participant stride.
#[repr(C, align(64))]
pub struct ParticipantRecord {
    /// [`FREE`] / [`RESERVED`] / [`LIVE`].
    pub state: AtomicU32,
    /// Operating-system process id.
    pub pid: AtomicU32,
    /// Start time in clock ticks since boot — what makes `pid` reuse-proof.
    pub start_time: AtomicU64,
    /// Bumped on reuse, so a claim naming this slot is distinguishable from one
    /// naming the same slot a generation earlier.
    pub incarnation: AtomicU64,
    /// When the participant attached (arena-local nanoseconds; diagnostics).
    pub attached_at_nanos: AtomicI64,
    /// Advisory hint. **Never a reaping trigger on its own**
    /// (`docs/PHASE2.md` §6.4): idle, or stopped by a debugger, is not dead.
    pub heartbeat: AtomicU64,
    _pad: [u8; 88],
}

// **`size_of` is not a layout.** `size_of`, `align_of` and `layout_hash`'s
// strides are all invariant under a field *reorder*, which changes what every
// byte means while they stay green: two builds then agree on `FORMAT_VERSION`
// and `layout_hash` and read each other's records wrong. These are wire records
// — a `memfd` another process maps, and what `write_frozen` memcpys into a
// `.tft` a later build opens. Appending a field is fine; moving one is a format
// break and now says so at compile time. `docs/decisions/0032` has the same gap
// still open for the region table and `layout_hash`'s stride array.
#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<ParticipantRecord>() == 128);
    assert!(core::mem::align_of::<ParticipantRecord>() == 64);
    assert!(core::mem::offset_of!(ParticipantRecord, state) == 0);
    assert!(core::mem::offset_of!(ParticipantRecord, pid) == 4);
    assert!(core::mem::offset_of!(ParticipantRecord, start_time) == 8);
    assert!(core::mem::offset_of!(ParticipantRecord, incarnation) == 16);
    assert!(core::mem::offset_of!(ParticipantRecord, attached_at_nanos) == 24);
    assert!(core::mem::offset_of!(ParticipantRecord, heartbeat) == 32);
};

/// Zeroed, i.e. [`FREE`] — the state a fresh arena's participant region is in.
///
/// Test-only: real records live in mapped arena bytes, zero by construction.
#[cfg(test)]
impl Default for ParticipantRecord {
    fn default() -> ParticipantRecord {
        ParticipantRecord {
            state: AtomicU32::new(FREE),
            pid: AtomicU32::new(0),
            start_time: AtomicU64::new(0),
            incarnation: AtomicU64::new(0),
            attached_at_nanos: AtomicI64::new(0),
            heartbeat: AtomicU64::new(0),
            _pad: [0; 88],
        }
    }
}

/// Why a process could not join the arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParticipantError {
    /// Every slot is occupied. Capacity is fixed at construction (invariant 3).
    TableFull,
    /// [`ParticipantTable::register_at`] was told to take a slot that is not
    /// [`FREE`].
    ///
    /// Distinct from [`ParticipantError::TableFull`]: the slot came from an
    /// owner's `HelloResponse` (`docs/PHASE2.md` §3.7) and is the lock-file byte
    /// the client is about to take, so landing elsewhere breaks that pairing.
    SlotTaken {
        /// The slot that was already occupied.
        slot: u32,
    },
    /// [`ParticipantTable::register_at`] was given a slot beyond the table.
    ///
    /// Only from a malformed or hostile `HelloResponse` — an error, not a panic,
    /// because a peer process's bug must not take this one down.
    SlotOutOfRange {
        /// The slot that was asked for.
        slot: u32,
        /// The table's capacity.
        capacity: u32,
    },
}

/// Take one slot and publish an identity into it, or fail if it is not free.
///
/// The single implementation behind [`ParticipantTable::register`] and
/// [`ParticipantTable::register_at`]. Returns the new incarnation, or `None` if
/// the slot was not [`FREE`]. The CAS wins the slot exclusively, the stores that
/// follow are invisible because no reader trusts a non-[`LIVE`] slot, and the
/// `Release` store publishes them together — so a kill in between leaves
/// [`RESERVED`], distinguishable garbage a reaper can reclaim on sight.
#[inline]
fn fill_slot(rec: &ParticipantRecord, pid: u32, start_time: u64, now_nanos: i64) -> Option<u64> {
    rec.state
        .compare_exchange(FREE, RESERVED, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    // `docs/PHASE2.md` §11.3 `attach.after_slot_assigned_before_publish`. The
    // window is ~12 ns (measured in `0028` open question 4), so only fault
    // injection can kill a process inside it — which is why §11.2's two
    // `..._collects_a_record_left_reserved_by_a_killed_registrant` tests *stage*
    // the word and cover the recovery only. Here the record is really produced.
    crash_point!("attach.after_slot_assigned_before_publish");
    // Ours exclusively, and no reader trusts a non-LIVE slot.
    rec.pid.store(pid, Ordering::Relaxed);
    rec.start_time.store(start_time, Ordering::Relaxed);
    rec.attached_at_nanos.store(now_nanos, Ordering::Relaxed);
    rec.heartbeat.store(0, Ordering::Relaxed);
    let incarnation = rec.incarnation.fetch_add(1, Ordering::AcqRel) + 1;
    // Release publishes the stores above to anyone who sees LIVE; the folded-in
    // incarnation lets a later release prove the slot is still its occupancy.
    rec.state.store(live_word(incarnation), Ordering::Release);
    Some(incarnation)
}

/// A borrowed view over the participant table.
pub struct ParticipantTable<'a> {
    slots: &'a [ParticipantRecord],
}

impl<'a> ParticipantTable<'a> {
    /// Wrap the arena's participant records.
    #[must_use]
    pub fn new(slots: &'a [ParticipantRecord]) -> ParticipantTable<'a> {
        ParticipantTable { slots }
    }

    /// Number of slots.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// The record at `slot`, or `None` if out of range.
    #[must_use]
    pub fn get(&self, slot: u32) -> Option<&'a ParticipantRecord> {
        self.slots.get(slot as usize)
    }

    /// Register this process, returning its slot index and incarnation.
    ///
    /// `fill_slot`'s two-phase publication: a kill mid-way leaves a [`RESERVED`]
    /// slot — distinguishable garbage, not the plausible-looking record A3's
    /// broken claim forced a reaper to judge.
    ///
    /// # Errors
    ///
    /// [`ParticipantError::TableFull`] if no slot is free.
    pub fn register(
        &self,
        pid: u32,
        start_time: u64,
        now_nanos: i64,
    ) -> Result<(u32, u64), ParticipantError> {
        for (i, rec) in self.slots.iter().enumerate() {
            if let Some(incarnation) = fill_slot(rec, pid, start_time, now_nanos) {
                return Ok((i as u32, incarnation));
            }
        }
        Err(ParticipantError::TableFull)
    }

    /// Register this process into **one named slot**, returning its incarnation.
    ///
    /// A *joiner* uses this with the slot its owner assigned (`docs/PHASE2.md`
    /// §3.7's `HelloResponse.participant_slot`); a creator or a process taking
    /// ownership has no owner to ask and uses [`ParticipantTable::register`],
    /// which takes whichever slot it wins.
    ///
    /// The caller does not choose because the arena record and the
    /// `F_OFD_SETLK` byte must be the *same integer*: §5.1's liveness predicate
    /// asks the kernel about the byte and then reads the record it indexes, so
    /// allocating them independently leaves a process holding byte 3 while
    /// occupying record 7. Nothing scans for a byte any more — `0035` put the
    /// creator on `try_take_participant(0)` and #201 deleted the scanning
    /// takeover arm (`0037`), leaving `LockFile::take_any_participant` with no
    /// production caller. Crash consistency is `register`'s, from the one shared
    /// implementation.
    ///
    /// # Errors
    ///
    /// [`ParticipantError::SlotOutOfRange`] if `slot >= capacity()`;
    /// [`ParticipantError::SlotTaken`] if the slot is not [`FREE`].
    pub fn register_at(
        &self,
        slot: u32,
        pid: u32,
        start_time: u64,
        now_nanos: i64,
    ) -> Result<u64, ParticipantError> {
        let rec = self.get(slot).ok_or(ParticipantError::SlotOutOfRange {
            slot,
            // Saturate, not `as u32`: `ParticipantTable::new` takes any slice,
            // so the header's u32 `max_participants` bound is the caller's
            // property, not this type's. A truncation would report a smaller
            // capacity than the table has and make an in-range slot read as out
            // of range.
            capacity: u32::try_from(self.slots.len()).unwrap_or(u32::MAX),
        })?;
        fill_slot(rec, pid, start_time, now_nanos).ok_or(ParticipantError::SlotTaken { slot })
    }

    /// Release a slot on clean detach.
    ///
    /// Idempotent at the memory level. The identity fields are left behind on
    /// purpose: a reaper inspecting a freed slot sees who was last there, and
    /// the next registrant overwrites them under [`RESERVED`] first.
    pub fn release(&self, slot: u32, incarnation: u64) {
        let Some(rec) = self.get(slot) else { return };
        // **One CAS on one word.** Loading `incarnation`, comparing it, then
        // CAS'ing `state` is two words and not atomic: in between, a reaper can
        // free the slot and another process `register` into it, so the
        // `LIVE -> FREE` CAS frees the *new* occupant's. Two live processes
        // would then share a slot index and break the unique `slot + 1` owner
        // encoding that claims (A3) and the topology lock (A2) rest on.
        let _ = rec.state.compare_exchange(
            live_word(incarnation),
            FREE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Free a slot whose participant is gone, guarded by the state word the
    /// caller observed.
    ///
    /// One `compare_exchange(observed, FREE)`: any change to the word since the
    /// caller's observation aborts the reclamation, and no incarnation is needed
    /// because [`live_word`] packs one into `observed`. This is the path for a
    /// process that never ran `Drop` (`docs/decisions/0028`);
    /// [`ParticipantTable::release`] stays the clean-detach one. Returns whether
    /// the CAS succeeded — `false` for a slot beyond the table, and vacuously
    /// `true` for an `observed` of [`FREE`], so callers pass a non-`FREE` word.
    /// **The liveness verdict is not taken here**: `docs/PHASE2.md` §5.1 is
    /// normative that liveness is a kernel fact about the OFD lock byte, so
    /// `state` only selects candidates and `heartbeat` is never read.
    ///
    /// # `RESERVED` is accepted, and *only* under two preconditions
    ///
    /// It is one bare constant carrying no incarnation, so against it the guard
    /// degenerates to an ABA and cannot tell a killed registrant from a running
    /// one. Two properties of the surrounding code make it safe, and **if either
    /// stops holding, narrow this back to `live_word(inc)`**:
    ///
    /// 1. **Every process that writes a record holds the matching lock byte
    ///    across the whole of `fill_slot`** — `Tree::attach_shared` and
    ///    `Tree::attach_shared_at` refuse `ReadWrite`, so a writer joins through
    ///    the rendezvous, which takes the byte first (`0028` step 0b).
    /// 2. **The lock byte and the record index are the same integer**, asserted
    ///    where they are paired (`0028` step 0c); otherwise a reclaimer asks the
    ///    kernel about one participant and frees another's record.
    ///
    /// With both, the byte is the occupancy authority, so a stale verdict is a
    /// spurious free of a slot its live joiner really owns — not a second
    /// occupant sharing the `slot + 1` encoding that claims (A3) and the
    /// topology lock (A2) rest on. `0028` open question 6 works the interleaving
    /// through; an earlier revision of that record shipped the opposite claim.
    ///
    /// # Ordering
    ///
    /// A caller must **observe the word before it probes the byte**: the
    /// `Acquire` load of a `live_word` synchronises-with `fill_slot`'s
    /// publishing `Release`, so a probe sequenced after it sees the byte held.
    /// Reversed — byte first, or one up-front holder mask such as
    /// `LockFile::held_participants()` — a reclaimer probes before the joiner
    /// takes the byte and erases the record it then publishes.
    /// `loom_tests::reclaim_races_register` pins that with two runnable failing
    /// controls (reversed reads; `Relaxed` observation), so the obligation is
    /// the `Acquire`, not the source order.
    ///
    /// This CAS's own strength is **unpinned**: `Relaxed`/`Relaxed` passed the
    /// whole `tf_tree_core` suite and all of `cargo xtask loom` with controls on
    /// 2026-08-21 (71 unit tests, 20 loom models, all green), because that model
    /// is about the caller's read order and the unit tests reaching this CAS are
    /// single-threaded. Do not weaken it — `docs/PHASE1.md` §10.2 wants the
    /// protocol argument stated. `Release` orders the verdict's inputs before
    /// the store acting on them (for `RESERVED` the byte *is* the verdict, and a
    /// `Relaxed` store could sink before the probe's load; unmeasurable here,
    /// since `F_OFD_GETLK` is a syscall barrier on every target, so it is stated
    /// for the model). `Acquire` makes the CAS read the word `fill_slot`
    /// released, so `pid`/`start_time`, written `Relaxed` under `RESERVED`, are
    /// visible to the reclaimer (`0028` piece 4, `TFT014`), and on failure it
    /// orders a losing caller after the occupancy that beat it rather than
    /// re-reading a dead identity and naming a participant already replaced. It
    /// is the same store [`ParticipantTable::release`] makes, at the same
    /// strength: they differ in their guard, not in what they publish.
    pub fn reclaim(&self, slot: u32, observed: u32) -> bool {
        let Some(rec) = self.get(slot) else {
            return false;
        };
        rec.state
            .compare_exchange(observed, FREE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Read a slot's `(pid, start_time, incarnation)` if it is [`LIVE`].
    ///
    /// The Acquire load pairs with `fill_slot`'s Release store: seeing `LIVE`
    /// means the identity fields are fully written.
    #[must_use]
    pub fn identity(&self, slot: u32) -> Option<(u32, u64, u64)> {
        let rec = self.get(slot)?;
        if state_of(rec.state.load(Ordering::Acquire)) != LIVE {
            return None;
        }
        Some((
            rec.pid.load(Ordering::Relaxed),
            rec.start_time.load(Ordering::Relaxed),
            rec.incarnation.load(Ordering::Relaxed),
        ))
    }
}
