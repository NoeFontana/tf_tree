//! Frame records and the lock-free interning table.
//!
//! `unsafe`-free: [`intern_core`] operates on caller-supplied atomic arrays, shared
//! by the arena view and the loom test; raw [`FrameRecord`] access lives in
//! [`crate::arena_view`].
//!
//! Publish-then-spin (`docs/PHASE1.md` §5.1): a writer CASes a hash slot, writes
//! the record, then publishes the id; a concurrent interner of the same name
//! spins on the id. "Unpublished" must be the zeroed-arena state
//! ([`ID_UNPUBLISHED`]), and every slot claimer must leave a terminal state, the
//! real id or [`ID_FAILED`], or later interners of that name hang.
//!
//! # A8 — a dead claimant must not wedge the table
//!
//! `docs/PHASE2.md` §1 A8; §11.3 crash point `intern.after_hash_cas_before_id_store`.
//! A `SIGKILL`ed claimant publishes nothing, so a third parallel array,
//! `claiming`, holds the *participant slot + 1* of the in-flight interner
//! ([`CLAIM_UNRECORDED`] = nobody). A waiter that has spun [`INTERN_SPIN_LIMIT`]
//! times resolves the claimant and, if gone, takes the entry over.
//!
//! ## Liveness is injected
//!
//! `claimant_alive` is a caller-supplied predicate (this crate is `no_std`;
//! `docs/PHASE2.md` §5.1 makes the OFD lock file authoritative, §6.2 the
//! participant record the fallback). [`crate::arena_view::ArenaView`] defaults it
//! to "assume alive", the fail-safe direction: a false "dead" steals from a live
//! process, a false "alive" only delays recovery.
//!
//! ## Why the claim is CASed, not stored
//!
//! The hash CAS grants the slot, so nothing may precede it, leaving a
//! two-instruction window with `hashes[i]` set and `claiming[i] == 0`. The winner
//! therefore CASes `CLAIM_UNRECORDED -> me`, and a waiter that finds
//! `CLAIM_UNRECORDED` after the spin limit may take over too. That is
//! **leak-free even when wrong**: the claim CAS precedes id allocation, so a
//! loser has not touched `frame_count` and adopts the rescuer's id.
//!
//! ## Residual gap
//!
//! `claiming` has no incarnation: if the claimant dies and a *live* process
//! recycles its participant slot first, recovery is delayed until that occupant
//! exits, never lost. Closing it needs a wider word carrying
//! `ParticipantRecord::incarnation`, a layout change beyond A8.

use crate::crash::crash_point;
use crate::error::FrameError;
use crate::sync::{spin, AtomicU32, AtomicU64, Ordering};

/// Sentinel stored in the `ids` array before a winning interner publishes the
/// real id.
///
/// It **must** be `0`, the zeroed-arena value; frame ids are 1-based, so a
/// published id is never `0`.
pub const ID_UNPUBLISHED: u32 = 0;

/// Published into the `ids` array when the winning interner could *not* complete
/// (the frame table filled after it claimed the hash slot); waiters return
/// [`FrameError::CapacityExceeded`]. Never a real id: `max_frames` is capped far
/// below `u32::MAX`.
pub const ID_FAILED: u32 = u32::MAX;

/// The 64-bit frame-name hash: the first eight bytes of `blake3(name)`, read as a
/// little-endian `u64`.
///
#[must_use]
pub fn blake3_64(name: &str) -> u64 {
    let digest = blake3::hash(name.as_bytes());
    let bytes = digest.as_bytes();
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(prefix)
}

/// Per-frame record. `FrameId` indexes the frame table.
///
/// `#[repr(C, align(64))]`, exactly 64 bytes, plain integers: the record write is
/// ordered by the `ids` publish store (Release) and a reader's Acquire load.
#[cfg(not(loom))]
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct FrameRecord {
    /// [`blake3_64`] of the full name.
    pub name_hash: u64,
    /// UTF-8 name, NUL-padded, truncated to 48 bytes for storage/display.
    pub name: [u8; 48],
    /// Stored name length (`min(len, 48)`).
    pub name_len: u8,
    /// Frame flags (reserved).
    pub flags: u8,
    /// What kind of thing this frame denotes (`docs/PHASE5.md` §1.2).
    ///
    /// `0` = unspecified (what this build writes); 1 = link, 2 = sensor, 3 = map,
    /// 4 = virtual.
    pub frame_kind: u8,
    _pad: [u8; 5],
}

// `size_of` is not a layout: a field reorder passes every size check and
// `layout_hash` yet changes what shared-segment and `.tft` bytes mean. These
// pins fix the offsets; appending is fine, moving one is a format break.
// See `docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md`.
#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<FrameRecord>() == 64);
    assert!(core::mem::align_of::<FrameRecord>() == 64);
    assert!(core::mem::offset_of!(FrameRecord, name_hash) == 0);
    assert!(core::mem::offset_of!(FrameRecord, name) == 8);
    assert!(core::mem::offset_of!(FrameRecord, name_len) == 56);
    assert!(core::mem::offset_of!(FrameRecord, flags) == 57);
    assert!(core::mem::offset_of!(FrameRecord, frame_kind) == 58);
};

#[cfg(not(loom))]
impl FrameRecord {
    /// Build a record for `name`, truncating the stored copy to 48 bytes.
    #[must_use]
    pub fn for_name(name: &str, hash: u64) -> FrameRecord {
        let src = name.as_bytes();
        let n = src.len().min(48);
        let mut buf = [0u8; 48];
        buf[..n].copy_from_slice(&src[..n]);
        FrameRecord {
            name_hash: hash,
            name: buf,
            name_len: n as u8,
            flags: 0,
            frame_kind: 0,
            _pad: [0; 5],
        }
    }

    /// Whether this record's stored (truncated) name matches `name`.
    ///
    /// Distinguishes a re-intern from a 64-bit hash collision by comparing the
    /// truncated stored bytes.
    #[must_use]
    pub fn name_matches(&self, name: &str) -> bool {
        let src = name.as_bytes();
        let n = src.len().min(48);
        self.name_len as usize == n && self.name[..n] == src[..n]
    }
}

/// Value of a `claiming` entry that names nobody.
///
/// `0`: the zeroed-arena value; participant slots are recorded as `slot + 1`
/// (`docs/PHASE2.md` §1 A3/A6).
pub const CLAIM_UNRECORDED: u32 = 0;

/// A claimant that is working but cannot name itself.
///
/// An [`crate::arena_view::ArenaView`] built without `as_participant` has no slot
/// to publish. `CLAIM_ANONYMOUS` means *somebody is working and nobody can judge
/// them*: neither a reader nor a rescuer may act on it (else a reader answers
/// "no such frame" for a live name, or a rescuer allocates a second id). Only
/// [`CLAIM_UNRECORDED`] after the spin budget means the window was abandoned.
pub const CLAIM_ANONYMOUS: u32 = u32::MAX;

/// How many times a waiter spins on an unpublished id before it stops trusting
/// the claimant and checks whether it is still alive (`docs/PHASE2.md` §1 A8).
///
/// A liveness-poll interval, not a timeout: a claimant reported alive is waited
/// on again. Tiny under `loom` to bound the interleavings.
#[cfg(not(loom))]
pub const INTERN_SPIN_LIMIT: u32 = 10_000;

/// Spin rounds a *reader* waits on an unrecorded claimant before concluding the
/// name is not there.
///
/// A reader cannot tell a healthy winner mid-CAS from a dead one; both read
/// `CLAIM_UNRECORDED`. It waits several rounds, still bounded (A8).
pub const READER_UNRECORDED_ROUNDS: u32 = 4;
/// See the `not(loom)` variant.
#[cfg(loom)]
pub const INTERN_SPIN_LIMIT: u32 = 2;

/// The three parallel interning arrays plus the id allocator
/// (`docs/PHASE1.md` §5.1). All slices have length `next_pow2(2 * max_frames)`.
pub struct InternTable<'a> {
    /// Frame-name hashes; `0` = empty slot.
    pub hashes: &'a [AtomicU64],
    /// Published frame ids; [`ID_UNPUBLISHED`] / [`ID_FAILED`] are the sentinels.
    pub ids: &'a [AtomicU32],
    /// **A8**: participant slot + 1 of the in-flight interner of each slot,
    /// [`CLAIM_UNRECORDED`] if none.
    pub claiming: &'a [AtomicU32],
    /// Frames interned so far; the id allocator.
    pub frame_count: &'a AtomicU32,
    /// Maximum interned frames (`max_frames - 1`; slot 0 is the root sentinel).
    pub capacity: u32,
}

/// What a bounded wait on an unpublished slot concluded.
enum Wait {
    /// The slot reached a terminal id (a real id, or [`ID_FAILED`]).
    Published(u32),
    /// The claimant is gone; this caller owns the entry and must publish a terminal id.
    TakenOver,
    /// The claimant is gone and this caller (a reader) may not take over.
    Abandoned,
    /// An anonymous claimant holds the entry and cannot be judged; report.
    Contended,
}

/// Who is waiting, and therefore what they are allowed to do about a claimant
/// that never published.
#[derive(Clone, Copy)]
enum Role {
    /// A writer, by participant slot + 1 ([`CLAIM_UNRECORDED`] if unregistered).
    Interner(u32),
    /// A lookup. Must not write to the arena, so it can only report the absence.
    Reader,
}

impl InternTable<'_> {
    /// Publish `value` into slot `i` unless a terminal id is already there.
    ///
    /// Returns whichever value is now visible. The CAS (not A8's plain store)
    /// keeps a takeover racing a resurrected claimant safe: one id per hash.
    fn publish(&self, i: usize, value: u32) -> u32 {
        match self.ids[i].compare_exchange(
            ID_UNPUBLISHED,
            value,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => value,
            Err(observed) => observed,
        }
    }

    /// Allocate an id, write its record, and publish it; run by every slot owner.
    fn finish(
        &self,
        i: usize,
        hash: u64,
        name_matches: &impl Fn(u32) -> bool,
        write_record: &impl Fn(u32),
    ) -> Result<u32, FrameError> {
        let n = self.frame_count.fetch_add(1, Ordering::AcqRel);
        if n >= self.capacity {
            // Lost the capacity race: return the id and publish ID_FAILED.
            self.frame_count.fetch_sub(1, Ordering::AcqRel);
            return match self.publish(i, ID_FAILED) {
                ID_FAILED => Err(FrameError::CapacityExceeded),
                other => resolve(other, hash, name_matches),
            };
        }
        let id = n + 1;
        write_record(id);
        let winner = self.publish(i, id);
        if winner == id {
            return Ok(id);
        }
        // A rescuer published first. Ours is abandoned and `frame_count` over-counts
        // by one, deliberately: `fetch_sub` could hand a live id back.
        resolve(winner, hash, name_matches)
    }

    /// Wait for slot `i` to publish, giving up on the claimant if it dies.
    ///
    /// Spins at most [`INTERN_SPIN_LIMIT`] times between liveness checks (module docs).
    fn wait_for_publish(
        &self,
        i: usize,
        role: Role,
        claimant_alive: &impl Fn(u32) -> bool,
    ) -> Wait {
        let mut spins: u32 = 0;
        let mut unrecorded_rounds: u32 = 0;
        loop {
            let id = self.ids[i].load(Ordering::Acquire);
            if id != ID_UNPUBLISHED {
                return Wait::Published(id);
            }
            spins += 1;
            if spins >= INTERN_SPIN_LIMIT {
                spins = 0;
                let owner = self.claiming[i].load(Ordering::Acquire);
                match role {
                    Role::Reader => {
                        if owner == CLAIM_ANONYMOUS {
                            // Not evidence of absence; wait, bounded (A8).
                            unrecorded_rounds += 1;
                            if unrecorded_rounds >= READER_UNRECORDED_ROUNDS {
                                return Wait::Contended;
                            }
                            continue;
                        }
                        if owner != CLAIM_UNRECORDED && !claimant_alive(owner) {
                            // Proven dead: nobody is going to publish this.
                            return Wait::Abandoned;
                        }
                        if owner == CLAIM_UNRECORDED {
                            // Not proof: also a healthy winner's window between
                            // its two CASes. Buy patience (several spin rounds).
                            unrecorded_rounds += 1;
                            if unrecorded_rounds >= READER_UNRECORDED_ROUNDS {
                                return Wait::Abandoned;
                            }
                        }
                    }
                    Role::Interner(me) => {
                        if owner == CLAIM_ANONYMOUS {
                            // Taking over an anonymous claimant would allocate a
                            // second id for one name; wait, bounded, then report.
                            unrecorded_rounds += 1;
                            if unrecorded_rounds >= READER_UNRECORDED_ROUNDS {
                                return Wait::Contended;
                            }
                            continue;
                        }
                        let recoverable = if owner == CLAIM_UNRECORDED {
                            // Killed between the two CASes, or anonymous. Only a
                            // registered participant may take over: an anonymous
                            // rescuer's CAS 0 -> 0 "succeeds" against a healthy
                            // claimant and leaks an id.
                            me != CLAIM_UNRECORDED
                        } else {
                            !claimant_alive(owner)
                        };
                        if recoverable
                            && self.claiming[i]
                                .compare_exchange(owner, me, Ordering::AcqRel, Ordering::Acquire)
                                .is_ok()
                        {
                            // It may have published before our CAS; its id stands.
                            let id = self.ids[i].load(Ordering::Acquire);
                            if id != ID_UNPUBLISHED {
                                return Wait::Published(id);
                            }
                            return Wait::TakenOver;
                        }
                        // Claimant alive, or another rescuer won: keep waiting.
                    }
                }
            }
            spin();
        }
    }
}

/// Interpret a published id for the caller that was waiting on it.
fn resolve(id: u32, hash: u64, name_matches: &impl Fn(u32) -> bool) -> Result<u32, FrameError> {
    if id == ID_FAILED {
        // Capacity is fixed, so this name will never be interned.
        return Err(FrameError::CapacityExceeded);
    }
    if name_matches(id) {
        Ok(id)
    } else {
        Err(FrameError::FrameHashCollision { hash })
    }
}

/// The lock-free interning core.
///
/// Open addressing with linear probing. `name_matches` detects a collision on a
/// hash hit; `write_record` populates the record before publish (at most once
/// per call). `me` is the participant slot **+ 1**, or [`CLAIM_UNRECORDED`] if
/// unregistered. `claimant_alive` is A8's predicate and must fail *safe*
/// (return `true` when it cannot tell).
///
/// # Errors
///
/// * [`FrameError::FrameHashCollision`] — a different name occupies this hash.
/// * [`FrameError::CapacityExceeded`] — the table is full, or the slot holds
///   [`ID_FAILED`].
///
/// # Panics
///
/// Never for a correctly sized table (power-of-two length `>= 2 * max_frames`,
/// equal across all three arrays).
pub fn intern_core(
    table: &InternTable<'_>,
    hash: u64,
    me: u32,
    claimant_alive: impl Fn(u32) -> bool,
    name_matches: impl Fn(u32) -> bool,
    write_record: impl Fn(u32),
) -> Result<u32, FrameError> {
    debug_assert_eq!(table.hashes.len(), table.ids.len());
    debug_assert_eq!(table.hashes.len(), table.claiming.len());
    let mask = (table.hashes.len() - 1) as u64;
    let mut i = (hash & mask) as usize;
    // Bounds the probe on misuse; `2 * len` covers one wasted iteration per lost CAS.
    for _ in 0..(2 * table.hashes.len()) {
        let cur = table.hashes[i].load(Ordering::Acquire);
        if cur == hash {
            return match table.wait_for_publish(i, Role::Interner(me), &claimant_alive) {
                Wait::Published(id) => resolve(id, hash, &name_matches),
                Wait::TakenOver => table.finish(i, hash, &name_matches, &write_record),
                // `Role::Interner` never abandons: it either publishes or waits.
                Wait::Abandoned => Err(FrameError::CapacityExceeded),
                Wait::Contended => Err(FrameError::InternContended),
            };
        }
        if cur == 0 {
            if table.frame_count.load(Ordering::Relaxed) >= table.capacity {
                return Err(FrameError::CapacityExceeded);
            }
            match table.hashes[i].compare_exchange(0, hash, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    // A8: record the claimant *before* allocating an id. An anonymous
                    // caller records `CLAIM_ANONYMOUS`, else it would look crashed.
                    let mark = if me == CLAIM_UNRECORDED {
                        CLAIM_ANONYMOUS
                    } else {
                        me
                    };
                    if table.claiming[i]
                        .compare_exchange(
                            CLAIM_UNRECORDED,
                            mark,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        // A rescuer took the entry over; it publishes, we wait.
                        continue;
                    }

                    // §11.3 `intern.after_hash_cas_before_id_store`: hash claimed,
                    // `claiming` names us (after both CASes, as no 128-bit CAS
                    // exists), `ids[i]` unpublished, `frame_count` untouched. The
                    // narrower window before recording is covered by
                    // `intern_recovers_when_the_claimant_died_before_recording_itself`.
                    crash_point!("intern.after_hash_cas_before_id_store");

                    return table.finish(i, hash, &name_matches, &write_record);
                }
                // Lost the race for this slot: re-read it.
                Err(_) => continue,
            }
        }
        i = (i + 1) & (mask as usize);
    }
    Err(FrameError::CapacityExceeded)
}

/// Look up an already-interned hash **without** inserting: the read-only half of
/// [`intern_core`], sharing its bounded wait.
///
/// Returns `Ok(None)` when the name was never interned, its slot holds
/// [`ID_FAILED`], or (A8) its claimant is provably gone with nothing published;
/// so a read-only participant cannot wedge on a dead writer.
///
/// # Errors
///
/// [`FrameError::FrameHashCollision`] if a different name occupies this hash.
/// [`FrameError::InternContended`] if the slot's claimant is anonymous and still
/// mid-publish when the reader's wait gives up (`Wait::Contended`).
pub fn find_core(
    table: &InternTable<'_>,
    hash: u64,
    claimant_alive: impl Fn(u32) -> bool,
    name_matches: impl Fn(u32) -> bool,
) -> Result<Option<u32>, FrameError> {
    debug_assert_eq!(table.hashes.len(), table.ids.len());
    debug_assert_eq!(table.hashes.len(), table.claiming.len());
    let mask = (table.hashes.len() - 1) as u64;
    let mut i = (hash & mask) as usize;
    for _ in 0..table.hashes.len() {
        let cur = table.hashes[i].load(Ordering::Acquire);
        if cur == 0 {
            return Ok(None); // reached an empty slot: name was never interned
        }
        if cur == hash {
            let id = match table.wait_for_publish(i, Role::Reader, &claimant_alive) {
                Wait::Published(id) => id,
                Wait::Abandoned => return Ok(None),
                // `Role::Reader` never takes over.
                Wait::TakenOver => return Ok(None),
                Wait::Contended => return Err(FrameError::InternContended),
            };
            if id == ID_FAILED {
                return Ok(None);
            }
            return resolve(id, hash, &name_matches).map(Some);
        }
        i = (i + 1) & (mask as usize);
    }
    Ok(None)
}
