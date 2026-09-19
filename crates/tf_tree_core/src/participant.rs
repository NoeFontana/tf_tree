//! The participant table — who is attached, and are they still alive.
//!
//! `docs/PHASE2.md` §1 A6 and §5. A participant's **slot index** is the identity
//! a claim (`slot + 1`) and the topology lock both record, so an owner and its
//! full identity publish in a single store (A3): the record was written at
//! attach time, long before any claim.
//!
//! `unsafe`-free: the record slice is handed in by [`crate::arena_view`].
//!
//! # Identity is PID + start time, never a bare PID
//!
//! PIDs wrap. The start time (`/proc/<pid>/stat` field 22) pins the identity for
//! as long as the machine is up; the header's boot id scopes it (§5.1).

use crate::crash::crash_point;
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

// `size_of` does not pin a layout: a field reorder passes it while changing what
// every wire byte means. These offsets are part of the format; appending is fine,
// moving is a break (`docs/decisions/0032`).
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

// `Display` and `Error` follow `docs/decisions/0059`; the match is exhaustive.

/// **The text is a diagnostic and not a compatibility promise**
/// (`docs/API.md` R5): it may change in any release, and the discriminant is
/// what a caller matches on. [`core::error::Error::source`] returns `None`,
/// and that is not promised either.
impl core::fmt::Display for ParticipantError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            ParticipantError::TableFull => {
                write!(f, "the arena's participant table is full (TableFull)")
            }
            ParticipantError::SlotTaken { slot } => {
                write!(f, "participant slot {slot} is already taken (SlotTaken)")
            }
            ParticipantError::SlotOutOfRange { slot, capacity } => write!(
                f,
                "participant slot {slot} is past the table's {capacity} slots (SlotOutOfRange)"
            ),
        }
    }
}

/// Lets a `ParticipantError` leave a function through `?` into
/// `Box<dyn Error>` or `anyhow::Error`. [`source`](core::error::Error::source)
/// is the default `None`, which is not a compatibility promise.
impl core::error::Error for ParticipantError {}

/// Take one slot and publish an identity into it, or fail if it is not free.
///
/// The single implementation of the publication protocol both
/// [`ParticipantTable::register`] and [`ParticipantTable::register_at`] use.
/// Returns the new incarnation on success, `None` if the slot was not [`FREE`].
#[inline]
fn fill_slot(rec: &ParticipantRecord, pid: u32, start_time: u64, now_nanos: i64) -> Option<u64> {
    rec.state
        .compare_exchange(FREE, RESERVED, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    // `docs/PHASE2.md` §11.3. Killing a process in this ~12 ns window is possible
    // only under fault injection; §11.2's `..._left_reserved_by_a_killed_registrant`
    // tests stage the word instead.
    crash_point!("attach.after_slot_assigned_before_publish");
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
    /// The slot comes from the owner's `HelloResponse` (`docs/PHASE2.md` §3.7) and
    /// must equal the lock-file byte the client takes, because §5.1's liveness
    /// predicate probes the byte and reads the record it indexes. A joiner uses
    /// this; a creator or a process taking ownership uses `register`. Both share
    /// `fill_slot`'s publication protocol.
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
            // Saturate: `new` accepts any slice, and truncation would under-report.
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
        // One CAS on one word: a load-then-CAS on two words lets a reaper free the
        // slot and another process re-register before the CAS frees the new occupant.
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
    /// observation aborts. Needs no incarnation, because [`live_word`] packs one
    /// in. The liveness verdict is not taken here (`docs/PHASE2.md` §5.1): the
    /// caller decides from the OFD lock byte, and nothing here reads `heartbeat`
    /// (`docs/decisions/0028`).
    ///
    /// Returns whether the word was still `observed` and is now [`FREE`]; `false`
    /// for a slot beyond the table. Callers pass a non-`FREE` word.
    ///
    /// # `RESERVED` is accepted, only under two preconditions
    ///
    /// [`RESERVED`] carries no incarnation, so against it the guard is an ABA.
    /// Narrow this back to `live_word(inc)` if either stops holding:
    ///
    /// 1. Every record writer holds the matching lock byte across `fill_slot`
    ///    (`0028` step 0b).
    /// 2. The lock byte and the record index are the same integer (`0028` step
    ///    0c).
    ///
    /// The byte, not the word, is then the occupancy authority (`0028` open
    /// question 6).
    ///
    /// # Ordering — a caller obligation
    ///
    /// Observe the word **before** probing the byte: the `Acquire` load of a
    /// `live_word` synchronises-with `fill_slot`'s `Release`, so a later byte
    /// probe sees the byte held. `loom_tests::reclaim_races_register` pins this
    /// with two failing controls (reversed reads; `Relaxed` observation).
    ///
    /// # Ordering — this CAS, unpinned
    ///
    /// Weakening it to `Relaxed`/`Relaxed` passes every test and `cargo xtask
    /// loom`; `AcqRel` rests on the protocol argument (`docs/PHASE1.md` §10.2):
    ///
    /// - `Release` orders the reclaimer's decision inputs before the store.
    /// - `Acquire` publishes the reclaimed occupancy's `pid`/`start_time` to the
    ///   collector (`0028` piece 4, `TFT014`).
    /// - `Acquire` on failure orders a loser after the occupancy that beat it.
    /// - It is the same store [`ParticipantTable::release`] makes.
    ///   not in what they publish.
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::ParticipantError;
    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;

    /// `docs/decisions/0059` step 1(b): every variant renders by decision 2's
    /// rules, with every carried integer at `u32::MAX`. Structure only (R5).
    ///
    /// **Mutant (M2):** `TableFull`'s arm → `Ok(())` fails this test and
    /// `a_participant_error_can_leave_a_function_as_box_dyn_error`.
    /// `a_participant_error_can_leave_a_function_as_box_dyn_error`.
    #[test]
    fn every_participant_error_variant_renders_by_0059s_rules() {
        fn index(e: &ParticipantError) -> usize {
            match e {
                ParticipantError::TableFull => 0,
                ParticipantError::SlotTaken { .. } => 1,
                ParticipantError::SlotOutOfRange { .. } => 2,
            }
        }
        let max = || u32::MAX.to_string();
        let all: Vec<(ParticipantError, Vec<String>)> = vec![
            (ParticipantError::TableFull, vec![]),
            (ParticipantError::SlotTaken { slot: u32::MAX }, vec![max()]),
            (
                ParticipantError::SlotOutOfRange {
                    slot: u32::MAX,
                    capacity: u32::MAX,
                },
                vec![max(), max()],
            ),
        ];
        let mut hit: Vec<usize> = all.iter().map(|(e, _)| index(e)).collect();
        hit.sort_unstable();
        hit.dedup();
        assert_eq!(hit, vec![0, 1, 2], "one value of every variant");

        for (e, numbers) in &all {
            let debug = format!("{e:?}");
            let shown = format!("{e}");
            let name = &debug[..debug.find(" {").unwrap_or(debug.len())];
            assert!(!shown.is_empty(), "{debug} renders as nothing");
            assert_ne!(shown, debug, "{debug} renders as its Debug");
            assert!(
                !shown.contains('{') && !shown.contains('}'),
                "{debug} renders with a brace: {shown:?}"
            );
            assert!(shown.is_ascii(), "{debug} renders non-ASCII: {shown:?}");
            assert!(
                shown.len() <= 120,
                "{debug} renders past 120 bytes: {shown:?}"
            );
            assert!(
                shown.ends_with(&format!("({name})")),
                "{debug} does not end with its search key ({name}): {shown:?}"
            );
            assert!(
                shown.matches(max().as_str()).count() >= numbers.len(),
                "{debug} drops a carried number: {shown:?}"
            );
        }
    }

    /// A `ParticipantError` leaves a function through `?` into `Box<dyn Error>`.
    #[test]
    fn a_participant_error_can_leave_a_function_as_box_dyn_error() {
        use alloc::boxed::Box;

        fn join() -> Result<(), Box<dyn core::error::Error>> {
            Err(ParticipantError::TableFull)?;
            Ok(())
        }
        assert!(join().unwrap_err().to_string().ends_with("(TableFull)"));
    }
}
