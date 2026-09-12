//! Frame records and the lock-free interning table.
//!
//! `unsafe`-free: the interning *algorithm* ([`intern_core`]) works purely on
//! caller-supplied atomic arrays, so the production arena view and the loom test
//! share it verbatim. Raw [`FrameRecord`] bytes live in [`crate::arena_view`].
//! Publish-then-spin (`docs/PHASE1.md` §5.1) exists for Phase 2's concurrent
//! interners — free in Phase 1, not retrofittable — and every interner that
//! claims a slot must leave a terminal id there, the real one or [`ID_FAILED`]:
//! an `Err` return with the slot claimed and the id unpublished hangs every
//! later interner of that name.
//!
//! # A8 — a dead claimant must not wedge the table
//!
//! `docs/PHASE2.md` §1 A8; crash point §11.3
//! `intern.after_hash_cas_before_id_store`. [`ID_FAILED`] cannot cover an
//! interner `SIGKILL`ed between the hash CAS and the id store, which publishes
//! nothing; Phase 1's unbounded wait then wedged that name in every process
//! forever. Hence `claiming`, a third array naming the in-flight interner's
//! *participant slot + 1* ([`CLAIM_UNRECORDED`] = nobody): past
//! [`INTERN_SPIN_LIMIT`] spins a waiter resolves the claimant and takes the
//! entry over if it is gone. A8 wants `claiming` written before the hash is
//! published, which is impossible — the hash CAS is what grants the slot — so it
//! is CASed second, and a waiter still seeing `CLAIM_UNRECORDED` may take over
//! too. That is leak-free even when wrong: the claim CAS precedes id allocation,
//! so the loser has not touched `frame_count` and adopts the rescuer's id.
//!
//! `claimant_alive` is injected, never inferred here: this crate is `no_std`,
//! and §5.1 makes the OFD lock file authoritative (§6.2's
//! `state`/`pid`/`start_time` the fallback). [`crate::arena_view::ArenaView`]
//! defaults it to "assume alive", §6.2's fail-safe direction — a false "dead"
//! steals an entry from a working process, a false "alive" only delays recovery.
//! Residual gap, exactly as A8 specifies: `claiming` carries no incarnation, so
//! a dead claimant whose slot is recycled by a live process reads as live until
//! that occupant exits (recovery delayed, never lost); closing it needs
//! `ParticipantRecord::incarnation` in a wider word, beyond A8's layout.

use crate::crash::crash_point;
use crate::error::FrameError;
use crate::sync::{spin, AtomicU32, AtomicU64, Ordering};

/// Sentinel stored in the `ids` array before a winning interner publishes the
/// real id.
///
/// Must be `0`: nothing pre-fills the `alloc_zeroed` array, so any other
/// sentinel would leave the wait loop inert, exiting on the first read with a
/// bogus id. Frame ids are 1-based (slot `0` is the root sentinel), so `0` is
/// never a published id.
pub const ID_UNPUBLISHED: u32 = 0;

/// Published into the `ids` array when the winning interner could *not* complete
/// (the frame table turned out full after it had claimed the hash slot), so
/// waiters return [`FrameError::CapacityExceeded`] instead of spinning forever.
///
/// `u32::MAX` is never a real frame id: ids are bounded by `max_frames`.
pub const ID_FAILED: u32 = u32::MAX;

/// The 64-bit frame-name hash: the first eight bytes of `blake3(name)`, read as a
/// little-endian `u64`.
///
/// The approved resolution of `docs/PHASE1.md` §5.1 (BLAKE3 hashing) against its
/// §0 dependency budget: `blake3` is an accepted `no_std` dependency here.
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
/// `#[repr(C, align(64))]`, 64 bytes, no atomic fields: the record write is
/// ordered purely by the Release publish into `ids`.
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
    /// `0` = unspecified (all this build writes); 1 = link, 2 = sensor, 3 = map,
    /// 4 = virtual. Lets a renderer or `tf_tree top` group by something sturdier
    /// than a name prefix.
    pub frame_kind: u8,
    _pad: [u8; 5],
}

// `size_of` is not a layout: `size_of`, `align_of` and `layout_hash`'s strides
// all survive a field *reorder*, which changes what every byte means while two
// builds still agree on `FORMAT_VERSION` and read each other's records wrong.
// These are wire records (shared `memfd`; `write_frozen`'s memcpy into a `.tft`),
// so offsets are part of the format: appending a field is fine, *moving* one is
// a format break and now fails to compile. Neighbouring hand-kept gap (region
// table, stride array):
// `docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md`.
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
    /// Distinguishes a re-intern from a 64-bit hash collision; the hash is over
    /// the *full* name, so the truncated bytes suffice.
    #[must_use]
    pub fn name_matches(&self, name: &str) -> bool {
        let src = name.as_bytes();
        let n = src.len().min(48);
        self.name_len as usize == n && self.name[..n] == src[..n]
    }
}

/// Value of a `claiming` entry that names nobody.
///
/// `0` because the arena is `alloc_zeroed` and participant slots are recorded as
/// `slot + 1` (`docs/PHASE2.md` §1 A3/A6 use the same +1 encoding, so slot `0` is
/// a legal owner everywhere).
pub const CLAIM_UNRECORDED: u32 = 0;

/// A claimant that is working but cannot name itself.
///
/// An [`crate::arena_view::ArenaView`] without `as_participant` has no slot to
/// publish; leaving [`CLAIM_UNRECORDED`] made it look like a winner that died
/// before recording itself, so readers answered `Ok(None)` for a name being
/// published right then, and rescuers stole the entry, allocating a second id
/// for one name and leaking the loser's. Neither may act on this sentinel; only
/// [`CLAIM_UNRECORDED`] past the spin budget means genuinely abandoned.
pub const CLAIM_ANONYMOUS: u32 = u32::MAX;

/// Spins on an unpublished id before a waiter checks the claimant's liveness
/// (`docs/PHASE2.md` §1 A8).
///
/// A poll interval, not a timeout: a claimant reported alive is waited on again
/// without limit. Tiny under `loom`, where each iteration multiplies the
/// interleavings.
#[cfg(not(loom))]
pub const INTERN_SPIN_LIMIT: u32 = 10_000;

/// Spin rounds a *reader* waits on an unrecorded claimant before concluding the
/// name is not there.
///
/// A reader cannot tell "healthy winner mid-CAS" from "dead before recording
/// itself" — both read `CLAIM_UNRECORDED` — and giving up after one round made
/// `find_frame` report a live, in-flight name as absent. Still bounded, per A8.
pub const READER_UNRECORDED_ROUNDS: u32 = 4;
/// See the `not(loom)` variant.
#[cfg(loom)]
pub const INTERN_SPIN_LIMIT: u32 = 2;

/// The three parallel interning arrays plus the frame allocator they draw ids
/// from — the whole mutable state of `docs/PHASE1.md` §5.1's interning table.
///
/// All three slices have length `next_pow2(2 * max_frames)`, so `mask == len-1`.
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
    /// The claimant is gone and this caller now owns the entry, so it must
    /// publish a terminal id.
    TakenOver,
    /// The claimant is gone but this caller is read-only. Nothing was written.
    Abandoned,
    /// An anonymous claimant holds the entry and cannot be judged. Nothing was
    /// written; the caller must report rather than wait or steal.
    Contended,
}

/// Who is waiting, and therefore what they may do about a claimant that never
/// published.
#[derive(Clone, Copy)]
enum Role {
    /// A writer, its participant slot + 1 ([`CLAIM_UNRECORDED`] when it is not a
    /// registered participant — single-process Phase 1 use).
    Interner(u32),
    /// A lookup. Must not write to the arena, so it can only report the absence.
    Reader,
}

impl InternTable<'_> {
    /// Publish `value` into slot `i` unless a terminal id is already there,
    /// returning whichever value is now visible.
    ///
    /// A CAS, not A8's plain store, so a takeover racing a resurrected claimant
    /// cannot hand out two ids for one name.
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

    /// Allocate an id, write its record, and publish it — the tail every owner of
    /// slot `i` runs, whether it won the hash CAS or rescued the slot.
    fn finish(
        &self,
        i: usize,
        hash: u64,
        name_matches: &impl Fn(u32) -> bool,
        write_record: &impl Fn(u32),
    ) -> Result<u32, FrameError> {
        let n = self.frame_count.fetch_add(1, Ordering::AcqRel);
        if n >= self.capacity {
            // Lost the capacity race (several threads interning distinct names at
            // exactly capacity). Give the id back so `frame_count` stays exact,
            // and publish ID_FAILED so waiters terminate.
            self.frame_count.fetch_sub(1, Ordering::AcqRel);
            return match self.publish(i, ID_FAILED) {
                ID_FAILED => Err(FrameError::CapacityExceeded),
                other => resolve(other, hash, name_matches),
            };
        }
        let id = n + 1;
        write_record(id);
        // Ordered before this Release CAS, so a waiter's Acquire load of `ids[i]`
        // sees a fully-written record.
        let winner = self.publish(i, id);
        if winner == id {
            return Ok(id);
        }
        // A rescuer that judged us dead published first: its id wins, ours is
        // abandoned and `frame_count` over-counts by one. Deliberate —
        // `fetch_sub`bing here could hand a *live* id back and alias two frames
        // onto one record.
        resolve(winner, hash, name_matches)
    }

    /// Wait for slot `i` to publish, giving up on the claimant if it dies.
    ///
    /// At most [`INTERN_SPIN_LIMIT`] spins between liveness checks; the module
    /// docs cover why a `CLAIM_UNRECORDED` claimant is recoverable without leak.
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
                // Acquire pairs with the claiming CAS below and in the winner
                // path, so an owner we read is a fully-published slot.
                let owner = self.claiming[i].load(Ordering::Acquire);
                match role {
                    Role::Reader => {
                        if owner == CLAIM_ANONYMOUS {
                            // Not evidence of absence, so wait — but bounded: an
                            // anonymous claimant that dies can never be proven
                            // dead, and an unbounded wait is A8's hang.
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
                            // **Not proof of anything**: also the two-instruction
                            // window between a healthy winner's two CASes, and an
                            // anonymous interner's permanent state. Abandoning on
                            // sight made `find_frame` answer `Ok(None)` for a name
                            // being published right then, so buy patience: the
                            // false negative now needs a claimant descheduled
                            // across ~40 000 spins rather than one.
                            unrecorded_rounds += 1;
                            if unrecorded_rounds >= READER_UNRECORDED_ROUNDS {
                                return Wait::Abandoned;
                            }
                        }
                    }
                    Role::Interner(me) => {
                        if owner == CLAIM_ANONYMOUS {
                            // Live anonymous claimant. Stealing it allocates a
                            // second id for one name and leaks the loser's: the
                            // module's "has not touched frame_count" holds only
                            // for an *identified* claimant. Report, never steal.
                            unrecorded_rounds += 1;
                            if unrecorded_rounds >= READER_UNRECORDED_ROUNDS {
                                return Wait::Contended;
                            }
                            continue;
                        }
                        let recoverable = if owner == CLAIM_UNRECORDED {
                            // Killed between the two CASes, or anonymous. Only a
                            // registered participant may take over: an anonymous
                            // rescuer would CAS 0 -> 0, "succeeding" against a
                            // healthy anonymous claimant and leaking an id on
                            // every preemption here.
                            me != CLAIM_UNRECORDED
                        } else {
                            !claimant_alive(owner)
                        };
                        if recoverable
                            && self.claiming[i]
                                .compare_exchange(owner, me, Ordering::AcqRel, Ordering::Acquire)
                                .is_ok()
                        {
                            // The claimant may have published between our load and
                            // our CAS; if so its id stands and we allocate nothing.
                            let id = self.ids[i].load(Ordering::Acquire);
                            if id != ID_UNPUBLISHED {
                                return Wait::Published(id);
                            }
                            return Wait::TakenOver;
                        }
                        // Claimant alive, or another rescuer won: somebody is
                        // still on the hook, so keep waiting.
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
        // This slot's owner ran out of table, and capacity is fixed for the
        // arena's life, so the name never will be interned.
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
/// Open addressing with linear probing over [`InternTable`]. `name_matches`
/// detects a hash collision; `write_record` is called at most once, by the slot's
/// owner (winner or rescuer), before the id is published. `me` is this interner's
/// participant slot **+ 1**, or [`CLAIM_UNRECORDED`] if it is not a registered
/// participant; `claimant_alive` is A8's injected liveness predicate, which must
/// fail *safe* (`true` when it cannot tell).
///
/// # Errors
///
/// * [`FrameError::FrameHashCollision`] — a different name occupies this hash.
/// * [`FrameError::CapacityExceeded`] — the frame table is full, or this slot was
///   poisoned with [`ID_FAILED`] by an interner that lost the capacity race.
///
/// # Panics
///
/// Never, for a correctly sized table (power-of-two length `>= 2 * max_frames`,
/// equal across all three arrays): mask indexing stays in bounds.
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
    // Guard against an infinite probe on a full table (only on misuse; capacity
    // is checked before we claim a slot). `2 * len` covers the one wasted
    // iteration a lost CAS costs before advancing.
    for _ in 0..(2 * table.hashes.len()) {
        let cur = table.hashes[i].load(Ordering::Acquire);
        if cur == hash {
            // Existing or in-flight entry: wait, rescuing the slot if its
            // claimant died mid-intern (A8).
            return match table.wait_for_publish(i, Role::Interner(me), &claimant_alive) {
                Wait::Published(id) => resolve(id, hash, &name_matches),
                Wait::TakenOver => table.finish(i, hash, &name_matches, &write_record),
                // `Role::Interner` never abandons: it publishes or waits.
                Wait::Abandoned => Err(FrameError::CapacityExceeded),
                // Unjudgeable anonymous claimant: reporting beats stealing (a
                // second id for one name) and beats A8's hang.
                Wait::Contended => Err(FrameError::InternContended),
            };
        }
        if cur == 0 {
            // Cheap pre-check: reject an obviously-full table before burning a
            // hash slot on a name that cannot be interned.
            if table.frame_count.load(Ordering::Relaxed) >= table.capacity {
                return Err(FrameError::CapacityExceeded);
            }
            match table.hashes[i].compare_exchange(0, hash, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    // A8: record the worker *before* allocating an id, so a crash
                    // from here on is recoverable. An anonymous caller records
                    // `CLAIM_ANONYMOUS`, never nothing — see that constant.
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
                        // A rescuer took the entry over before we recorded
                        // ourselves. It publishes; we wait.
                        continue;
                    }

                    // §11.3 `intern.after_hash_cas_before_id_store` sits after
                    // *both* words: A8 records `claiming` "alongside the hash",
                    // i.e. by the same CAS, and with no 128-bit CAS this splits it
                    // in two. State here: hash claimed, `claiming` naming us,
                    // `ids[i]` unpublished, `frame_count` untouched. The narrower
                    // window above (claimant not yet recorded) is a different
                    // state with a different rescuer branch, covered by
                    // `intern_recovers_when_the_claimant_died_before_recording_
                    // itself`; §11.3 has no row for it.
                    crash_point!("intern.after_hash_cas_before_id_store");

                    return table.finish(i, hash, &name_matches, &write_record);
                }
                // Lost the race: re-read the slot (someone else's hash, or ours).
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
/// `Ok(None)` when the name was never interned, when its slot holds
/// [`ID_FAILED`], and — A8 — when its claimant is provably gone with nothing
/// published, which is why a read-only participant cannot wedge on a dead writer:
/// truthful at that instant, self-corrected by the next interner's rescue.
///
/// # Errors
///
/// [`FrameError::FrameHashCollision`] if a different name occupies this hash.
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
                // Nothing published, and nobody live will publish it.
                Wait::Abandoned => return Ok(None),
                // `Role::Reader` never takes over.
                Wait::TakenOver => return Ok(None),
                // An anonymous claimant is mid-publish; `Ok(None)` would be a lie.
                Wait::Contended => return Err(FrameError::InternContended),
            };
            if id == ID_FAILED {
                // The slot's interner lost the capacity race: never interned.
                return Ok(None);
            }
            return resolve(id, hash, &name_matches).map(Some);
        }
        i = (i + 1) & (mask as usize);
    }
    Ok(None)
}
