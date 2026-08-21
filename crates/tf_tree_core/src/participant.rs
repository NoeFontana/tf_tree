//! The participant table — who is attached, and are they still alive.
//!
//! `docs/PHASE2.md` §1 A6 and §5. A participant is a process that has mapped the
//! arena. Its **slot index** is the identity everything else names: a claim
//! records `participant_slot + 1`, and the topology lock records the same, so
//! both publish an owner and its full identity in a *single* store.
//!
//! That indirection is the whole point (A3). Phase 1's claim record wrote
//! `state` and then `owner_pid` as two stores; a writer `SIGKILL`ed between them
//! left `state = HELD, owner_pid = 0` — held by nobody, reclaimable by nobody, a
//! permanently leaked edge. Pointing at a participant record that was **fully
//! written at attach time, long before any claim** collapses that to one store.
//!
//! `unsafe`-free: the record slice is handed in by [`crate::arena_view`].
//!
//! # Identity is PID + start time, never a bare PID
//!
//! PIDs wrap. A reaper that trusted a bare PID would eventually conclude that a
//! long-dead participant is alive because an unrelated process now holds its
//! number — and then refuse to reclaim its resources forever. The process start
//! time (`/proc/<pid>/stat` field 22) pins the identity: the pair is unique for
//! as long as the machine is up, and [`crate::arena_view::ArenaView`]'s header
//! carries the boot id that scopes it (§5.1).

use crate::sync::{AtomicI64, AtomicU32, AtomicU64, Ordering};

/// Slot is unused.
pub const FREE: u32 = 0;
/// Slot is being filled in by a registrant that has not published yet.
pub const RESERVED: u32 = 1;
/// Slot is fully written and its participant is attached.
///
/// Stored in the low 2 bits of `state`; the high 30 carry the incarnation, so a
/// release can check "LIVE and still mine" with one compare-exchange. See
/// [`ParticipantTable::release`] for why two words were not enough.
pub const LIVE: u32 = 2;

/// The `state` word for a live slot at `incarnation`.
///
/// Only the low 30 bits of the incarnation survive. The authoritative counter
/// stays the full `AtomicU64` in the record; this is a *guard*, and 2^30
/// re-registrations of one slot would have to occur between a process's last
/// instruction and its `release` for the truncation to alias.
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
    /// Process start time in clock ticks since boot — what makes `pid`
    /// reuse-proof.
    pub start_time: AtomicU64,
    /// Bumped every time this slot is reused, so a claim naming a slot can be
    /// told apart from one naming the *same* slot a generation earlier.
    pub incarnation: AtomicU64,
    /// When the participant attached (arena-local nanoseconds; diagnostics).
    pub attached_at_nanos: AtomicI64,
    /// Advisory liveness hint. **Never a reaping trigger on its own**
    /// (`docs/PHASE2.md` §6.4): a participant that is merely idle, or stopped by
    /// a debugger, is not dead.
    pub heartbeat: AtomicU64,
    _pad: [u8; 88],
}

#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<ParticipantRecord>() == 128);
    assert!(core::mem::align_of::<ParticipantRecord>() == 64);
};

/// Zeroed, i.e. [`FREE`] — the state a fresh arena's participant region is in.
///
/// Test-only because the real records live in mapped arena bytes, which are
/// zero by construction; nothing in the engine ever builds one on the heap.
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
    /// Distinct from [`ParticipantError::TableFull`] because the caller asked
    /// for *this* slot and cannot simply take another: the slot came from an
    /// owner's `HelloResponse` (`docs/PHASE2.md` §3.7) and is also the lock-file
    /// byte the client is about to take, so silently landing elsewhere would
    /// break the very correspondence `register_at` exists to establish.
    SlotTaken {
        /// The slot that was already occupied.
        slot: u32,
    },
    /// [`ParticipantTable::register_at`] was given a slot beyond the table.
    ///
    /// Reachable only from a malformed or hostile `HelloResponse`, which is
    /// exactly why it is an error rather than a panic — the owner is a peer
    /// process, and a peer's bug must not take this process down.
    SlotOutOfRange {
        /// The slot that was asked for.
        slot: u32,
        /// The table's capacity.
        capacity: u32,
    },
}

/// Take one slot and publish an identity into it, or fail if it is not free.
///
/// The single implementation of the publication protocol both
/// [`ParticipantTable::register`] and [`ParticipantTable::register_at`] use.
/// Returns the new incarnation on success, `None` if the slot was not [`FREE`].
///
/// # Crash consistency
///
/// The CAS is what makes this exclusive: the winner owns the slot from that
/// instruction, and the identity stores that follow are invisible to anyone,
/// because no reader trusts a non-[`LIVE`] slot. The final store is `Release`,
/// so a peer that observes `LIVE` observes every field written above it.
///
/// A process killed between the CAS and the store leaves the slot [`RESERVED`],
/// which is **distinguishable garbage** rather than a plausible-looking record:
/// no live participant is `RESERVED` for more than a few instructions, so a
/// reaper can reclaim one on sight without having to judge whether it is
/// looking at a valid record.
#[inline]
fn fill_slot(rec: &ParticipantRecord, pid: u32, start_time: u64, now_nanos: i64) -> Option<u64> {
    rec.state
        .compare_exchange(FREE, RESERVED, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    // Exclusively ours: no other registrant can be here, and no reader trusts a
    // non-LIVE slot.
    rec.pid.store(pid, Ordering::Relaxed);
    rec.start_time.store(start_time, Ordering::Relaxed);
    rec.attached_at_nanos.store(now_nanos, Ordering::Relaxed);
    rec.heartbeat.store(0, Ordering::Relaxed);
    let incarnation = rec.incarnation.fetch_add(1, Ordering::AcqRel) + 1;
    // Release publishes every store above to anyone who sees LIVE, and folds the
    // incarnation in so a release can prove the slot is still the same occupancy
    // it registered.
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
    /// # Crash consistency
    ///
    /// Publication is two-phase, and the intermediate state is **distinguishable
    /// garbage** rather than a plausible-looking record:
    ///
    /// 1. CAS [`FREE`] -> [`RESERVED`], which wins the slot exclusively.
    /// 2. Write the identity fields, which nobody may read yet.
    /// 3. Release-store [`LIVE`], which publishes them.
    ///
    /// A process killed between 1 and 3 leaves a [`RESERVED`] slot. No live
    /// participant is ever `RESERVED` for more than a few instructions, so a
    /// reaper can reclaim one on sight without having to decide whether it is
    /// looking at a valid record — which is exactly the judgement A3's broken
    /// claim record forced and could not make.
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
    /// [`ParticipantTable::register`] takes whichever slot it wins; this takes
    /// the one it is told to and fails if it cannot.
    ///
    /// # Why the caller does not get to choose
    ///
    /// `docs/PHASE2.md` §3.7's `HelloResponse.participant_slot` "matches the
    /// lock-file byte the client must take". That correspondence is the whole
    /// point: the arena record and the `F_OFD_SETLK` byte have to be the *same
    /// integer*, because §5.1's liveness predicate asks the kernel about the
    /// byte and then reads the record it indexes. If the two were allocated
    /// independently — as they are today, by `register` here and by
    /// `LockFile::take_any_participant` there — a process would hold byte 3
    /// while occupying record 7, and every liveness answer would be about
    /// somebody else.
    ///
    /// So a *joiner* uses this, with the slot the owner assigned. A creator or
    /// a process taking ownership has no owner to ask and uses `register`.
    ///
    /// Crash consistency is identical to `register` — the same
    /// `FREE -> RESERVED -> fields -> Release(LIVE)` publication, sharing one
    /// implementation deliberately, because two copies of a crash-consistency
    /// protocol is two chances to amend only one of them.
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
            // Saturate rather than `as u32`. In an arena the length is bounded
            // by the header's `max_participants`, which is a u32 — but
            // `ParticipantTable::new` accepts any slice, so that is a property
            // of the caller and not of this type. A silent truncation here
            // would report a *smaller* capacity than the table has and make the
            // error read as though the slot were out of range when it was not.
            capacity: u32::try_from(self.slots.len()).unwrap_or(u32::MAX),
        })?;
        fill_slot(rec, pid, start_time, now_nanos).ok_or(ParticipantError::SlotTaken { slot })
    }

    /// Release a slot on clean detach.
    ///
    /// Idempotent at the memory level. The identity fields are deliberately left
    /// behind: a reaper inspecting a freed slot gets a truthful record of who was
    /// last there, and the next registrant overwrites them under [`RESERVED`]
    /// before anyone can read them.
    pub fn release(&self, slot: u32, incarnation: u64) {
        let Some(rec) = self.get(slot) else { return };
        // **One CAS on one word.** An earlier version loaded `incarnation`,
        // compared it, and then CAS'd `state` — two words, not atomic. Between
        // them a reaper could free the slot and another process `register` into
        // it, and the `LIVE -> FREE` CAS would then free the *new* occupant's
        // slot: exactly the bug the guard was added to close. Two live processes
        // would then share a slot index, and the `slot + 1` owner encoding that
        // both claims (A3) and the topology lock (A2) rest on stops being
        // unique.
        //
        // `state` therefore carries the incarnation in its high bits, so
        // "still LIVE *and* still mine" is a single comparison.
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
    /// One `compare_exchange(observed, FREE)`, so **any** change to the word
    /// between the caller's observation and this CAS aborts the reclamation.
    /// It differs from [`ParticipantTable::release`] in one way that matters: it
    /// needs no incarnation, because the observed word carries it —
    /// [`live_word`] packs the incarnation into the high 30 bits. `release`
    /// stays exactly as it is for the clean-detach path; this is the path for a
    /// slot whose process never ran `Drop` (`docs/decisions/0028`).
    ///
    /// **The liveness verdict is not taken here.** `docs/PHASE2.md` §5.1 is
    /// normative that "whether it is live is a kernel fact": the caller decides
    /// from the participant's OFD lock byte and this is only the guarded store
    /// that acts on the decision. Nothing in this function reads `heartbeat`,
    /// and `state` selects *which* slots are candidates rather than answering
    /// whether a process is alive.
    ///
    /// Returns whether the CAS succeeded — i.e. whether the word was still
    /// `observed` and is now [`FREE`]. An `observed` of [`FREE`] is vacuously
    /// such a case and collects nothing; callers pass a non-`FREE` word because
    /// a `FREE` slot has nothing to collect. `false` for a slot beyond the
    /// table.
    ///
    /// # `RESERVED` is accepted, and *only* under two preconditions
    ///
    /// `observed` may be [`RESERVED`] as well as `live_word(inc)`. Unlike
    /// `live_word`, **`RESERVED` is one bare constant carrying no
    /// incarnation**: two different occupancies of a slot are byte-identical
    /// words, so against it the CAS guard degenerates to an ABA and cannot tell
    /// a killed registrant from a running one. What makes accepting it safe is
    /// therefore not this function. It is two properties of the code around it,
    /// and **if either stops holding, this must be narrowed back to
    /// `live_word(inc)`**:
    ///
    /// 1. **Every process that writes a record holds the matching lock byte
    ///    across the whole of `fill_slot`.** `Tree::attach_shared` and
    ///    `Tree::attach_shared_at` refuse `ReadWrite`, so a writer joins through
    ///    the rendezvous, which takes the byte before the arena record is
    ///    written (`0028` step 0b).
    /// 2. **The lock byte and the arena record index are the same integer**,
    ///    asserted where the two are paired rather than assumed (`0028` step
    ///    0c). Without it a reclaimer asks the kernel about one participant and
    ///    frees another's record.
    ///
    /// With both, the byte — not the word — is the occupancy authority, and a
    /// stale verdict that frees a `RESERVED` word frees one whose byte a live
    /// joiner holds. That joiner publishes over it and is *correct* to, because
    /// it really does own the slot: the outcome is a spurious free, not a second
    /// occupant. Without either, it **is** a second occupant, sharing the
    /// `slot + 1` owner encoding that claims (A3) and the topology lock (A2)
    /// rest on. `0028` open question 6 works that interleaving through; an
    /// earlier revision of that record shipped the opposite claim, which is why
    /// the precondition is stated here and not left as a rule.
    ///
    /// # Ordering — a caller obligation, not an implementation detail
    ///
    /// A caller must **observe the word before it probes the byte**. The
    /// `Acquire` load that produces a `live_word` synchronises-with
    /// `fill_slot`'s publishing `Release` store, so a byte probe sequenced after
    /// it must see the byte held. Reversed — byte first, or one up-front holder
    /// mask such as `LockFile::held_participants()` returns — a reclaimer probes
    /// a byte before its joiner takes it, then observes the record that joiner
    /// has since published, and erases it.
    /// `loom_tests::reclaim_races_register` is that property, and it ships with
    /// two **runnable** failing controls: reversing the two reads erases a
    /// published record, and so does keeping the order while weakening the
    /// observation to `Relaxed`. The obligation is therefore the `Acquire`, not
    /// the source order. That model says nothing about the CAS below it — see
    /// the next section, and its own doc comment, which measures how little.
    ///
    /// # Ordering — this CAS's own strength, which no test here distinguishes
    ///
    /// Written down because it is **unpinned**, rather than left for the next
    /// reader to discover and quietly "simplify": weakening this
    /// `compare_exchange` to `Relaxed`/`Relaxed` passes the whole `tf_tree_core`
    /// suite and all of `cargo xtask loom`, controls included — measured on
    /// 2026-08-21 (71 unit tests, 20 loom models, all green), not assumed. The loom model above is about the *caller's* read order and
    /// never reaches this CAS on a contended slot; the unit tests that do reach
    /// it are single-threaded, where every ordering is equivalent. `AcqRel` is
    /// here on a protocol argument, and `docs/PHASE1.md` §10.2 is why that
    /// argument has to be stated rather than implied:
    ///
    /// - **The `Release` half orders this reclaimer's decision *inputs* before
    ///   the store that acts on them.** The verdict is formed from the state
    ///   word and the OFD byte, and for `RESERVED` — which carries no
    ///   incarnation, so the guard below degenerates to an ABA — the byte is the
    ///   whole of it. A `Relaxed` store may be reordered before a preceding
    ///   load: the slot could become `FREE` to other threads before the probe
    ///   has read the byte, which is a reclaimer acting on a verdict it has not
    ///   finished forming. Nothing in this workspace can measure that, because
    ///   the probe is `F_OFD_GETLK` — a syscall, and a syscall is a barrier on
    ///   every architecture this builds for. The ordering is stated for the
    ///   model, and the model is where the byte-as-authority argument lives.
    /// - **The `Acquire` half publishes the collected occupancy to the
    ///   collector.** A successful CAS *reads* the word `fill_slot` released, so
    ///   `Acquire` makes it synchronise-with that publication and the `pid` and
    ///   `start_time` written `Relaxed` under `RESERVED` are visible to whoever
    ///   just reclaimed the slot — which is what `0028` piece 4 and `TFT014`
    ///   report out of a sweep.
    /// - **`Acquire` on failure leaves a losing caller ordered after the
    ///   occupancy that defeated it.** A `false` return is a load of the word
    ///   that beat this verdict; the same synchronises-with edge means the
    ///   identity a caller re-reads *after* the failure belongs to the new
    ///   occupant. Without it a sweep that retries can re-read the dead
    ///   process's identity, re-form the same verdict, and name a participant
    ///   that has already been replaced.
    /// - **It is the same store [`ParticipantTable::release`] makes**, from the
    ///   clean-detach path, at the same strength. The two differ in their guard,
    ///   not in what they publish, and a slot freed by a reaper being weaker
    ///   than one freed by its owner would need a reason nobody has.
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
    /// The Acquire load pairs with `register`'s Release store, so a caller that
    /// sees `LIVE` sees fully-written identity fields.
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
