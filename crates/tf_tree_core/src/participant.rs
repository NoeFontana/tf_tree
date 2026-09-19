//! The participant table (`docs/PHASE2.md` §1 A6, §5): who is attached, and are they
//! still alive. A slot index is the identity a claim (`slot + 1`) and the topology
//! lock record. `unsafe`-free.
//!
//! # Identity is PID + start time, never a bare PID
//!
//! PIDs wrap; the start time (`/proc/<pid>/stat` field 22) pins identity while the
//! machine is up, and the header's boot id scopes it (§5.1).

use crate::crash::crash_point;
use crate::sync::{AtomicI64, AtomicU32, AtomicU64, Ordering};

/// Slot is unused.
pub const FREE: u32 = 0;
/// Slot is being filled in by a registrant that has not published yet.
pub const RESERVED: u32 = 1;
/// Slot is fully written and its participant is attached.
///
/// Stored in the low 2 bits of `state`; the high 30 carry the incarnation, so
/// `release` checks "LIVE and still mine" with one compare-exchange.
pub const LIVE: u32 = 2;

/// The `state` word for a live slot at `incarnation`.
///
/// Only the low 30 bits of the incarnation survive; it is a guard.
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

/// One participant's record, 128 bytes.
#[repr(C, align(64))]
pub struct ParticipantRecord {
    /// [`FREE`] / [`RESERVED`] / [`LIVE`].
    pub state: AtomicU32,
    /// Operating-system process id.
    pub pid: AtomicU32,
    /// Process start time in clock ticks since boot; makes `pid` reuse-proof.
    pub start_time: AtomicU64,
    /// Bumped on every reuse, so a claim can tell this occupancy from an earlier one.
    pub incarnation: AtomicU64,
    /// When the participant attached (arena-local nanoseconds; diagnostics).
    pub attached_at_nanos: AtomicI64,
    /// Advisory liveness hint. **Never a reaping trigger on its own**
    /// (`docs/PHASE2.md` §6.4).
    pub heartbeat: AtomicU64,
    _pad: [u8; 88],
}

// These offsets are format: appending is fine, moving is a break (`docs/decisions/0032`).
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

/// Zeroed, i.e. [`FREE`]. Test-only.
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
    /// [`ParticipantTable::register_at`] was told to take a slot that is not [`FREE`]
    /// (`docs/PHASE2.md` §3.7); distinct from `TableFull` because the slot must match
    /// the lock-file byte.
    SlotTaken {
        /// The slot that was already occupied.
        slot: u32,
    },
    /// [`ParticipantTable::register_at`] was given a slot beyond the table (a
    /// malformed `HelloResponse`; an error, not a panic, because the owner is a peer).
    SlotOutOfRange {
        /// The slot that was asked for.
        slot: u32,
        /// The table's capacity.
        capacity: u32,
    },
}

// `Display` and `Error` follow `docs/decisions/0059`.

/// **The text is a diagnostic and not a compatibility promise**
/// (`docs/API.md` R5); match on the discriminant.
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

/// Lets a `ParticipantError` leave a function through `?` into `Box<dyn Error>`.
impl core::error::Error for ParticipantError {}

/// Take one slot and publish an identity into it (the protocol shared by `register`
/// and `register_at`); returns the new incarnation, or `None` if not [`FREE`].
#[inline]
fn fill_slot(rec: &ParticipantRecord, pid: u32, start_time: u64, now_nanos: i64) -> Option<u64> {
    rec.state
        .compare_exchange(FREE, RESERVED, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    crash_point!("attach.after_slot_assigned_before_publish");
    rec.pid.store(pid, Ordering::Relaxed);
    rec.start_time.store(start_time, Ordering::Relaxed);
    rec.attached_at_nanos.store(now_nanos, Ordering::Relaxed);
    rec.heartbeat.store(0, Ordering::Relaxed);
    let incarnation = rec.incarnation.fetch_add(1, Ordering::AcqRel) + 1;
    // Release publishes the stores above; the incarnation lets `release` prove ownership.
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
    /// CAS [`FREE`] -> [`RESERVED`], write the identity, release-store [`LIVE`]. A process
    /// killed between the two leaves a [`RESERVED`] slot, which a reaper reclaims on sight.
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

    /// Register this process into **one named slot**, returning its incarnation. The
    /// slot comes from the owner's `HelloResponse` (`docs/PHASE2.md` §3.7) and must equal
    /// the lock-file byte the client takes (§5.1).
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
            capacity: u32::try_from(self.slots.len()).unwrap_or(u32::MAX),
        })?;
        fill_slot(rec, pid, start_time, now_nanos).ok_or(ParticipantError::SlotTaken { slot })
    }

    /// Release a slot on clean detach. Idempotent; identity fields are left for a reaper.
    pub fn release(&self, slot: u32, incarnation: u64) {
        let Some(rec) = self.get(slot) else { return };
        let _ = rec.state.compare_exchange(
            live_word(incarnation),
            FREE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Free a slot whose participant is gone, guarded by the state word the caller
    /// observed: one `compare_exchange(observed, FREE)`. The liveness verdict is the
    /// caller's (`docs/PHASE2.md` §5.1, `docs/decisions/0028`). Returns whether the slot
    /// was freed; callers pass a non-`FREE` word.
    ///
    /// # `RESERVED` is accepted, only under two preconditions
    ///
    /// [`RESERVED`] carries no incarnation, so against it the guard is an ABA. Narrow to
    /// `live_word(inc)` if either stops holding: every record writer holds the matching
    /// lock byte across `fill_slot` (`0028` step 0b), and the byte and record index are
    /// the same integer (step 0c).
    ///
    /// # Ordering — a caller obligation
    ///
    /// Observe the word **before** probing the byte: the `Acquire` load of a `live_word`
    /// synchronises-with `fill_slot`'s `Release`. `loom_tests::reclaim_races_register`
    /// pins this.
    ///
    /// # Ordering — this CAS, unpinned
    ///
    /// `Relaxed`/`Relaxed` would pass every test and `cargo xtask loom`; `AcqRel` rests
    /// on `docs/PHASE1.md` §10.2 and `0028` piece 4.
    pub fn reclaim(&self, slot: u32, observed: u32) -> bool {
        let Some(rec) = self.get(slot) else {
            return false;
        };
        rec.state
            .compare_exchange(observed, FREE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Read a slot's `(pid, start_time, incarnation)` if it is [`LIVE`].
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

    /// `docs/decisions/0059` step 1(b): every variant renders by decision 2's rules.
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
