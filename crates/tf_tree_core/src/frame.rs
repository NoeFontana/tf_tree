//! Frame records and the lock-free interning table.
//!
//! `unsafe`-free: the interning *algorithm* ([`intern_core`]) operates purely on
//! the caller-supplied atomic arrays, so it is shared verbatim by the production
//! arena view and by the loom test. Raw access to [`FrameRecord`] bytes lives in
//! [`crate::arena_view`].
//!
//! The publish-then-spin protocol (`docs/PHASE1.md` §5.1) exists because Phase 2 has
//! two processes interning concurrently: a writer claims a hash slot with a CAS,
//! writes the record, and only then publishes the id; a concurrent interner of
//! the same name observes the hash, spins until the id is published, and returns
//! it. It costs nothing in Phase 1 and cannot be retrofitted.
//!
//! The protocol only works if the "unpublished" state is the state a **zeroed
//! arena** is already in — see [`ID_UNPUBLISHED`]. Nothing pre-fills the id
//! array, so a non-zero sentinel makes the wait loop inert: it exits on the first
//! read and hands back a bogus id. Every interner that claims a slot must also
//! leave it in a terminal state, either the real id or [`ID_FAILED`]; returning
//! `Err` with the slot claimed and the id unpublished hangs every later interner
//! of that name.
//!
//! # A8 — a dead claimant must not wedge the table
//!
//! `docs/PHASE2.md` §1 A8, and its §11.3 crash point
//! `intern.after_hash_cas_before_id_store`. [`ID_FAILED`] covers the interner
//! that *fails* — it publishes a terminal marker on the way out. It cannot cover
//! the interner that is `SIGKILL`ed between winning the hash CAS and publishing
//! the id, because a killed process publishes nothing at all. Phase 1's wait was
//! unbounded, so that slot wedged **every future interner of that name, in every
//! process, forever**. In one process the defect is unobservable (the dead
//! process took its readers with it); across processes it is fatal.
//!
//! The fix is a third parallel array, `claiming`, holding the *participant slot +
//! 1* of whoever owns the slot's in-flight intern ([`CLAIM_UNRECORDED`] = nobody
//! recorded). A waiter that has spun [`INTERN_SPIN_LIMIT`] times stops trusting
//! the claimant, resolves it, and — if it is gone — takes the entry over.
//!
//! ## Liveness is injected, never inferred here
//!
//! `claimant_alive` is a **caller-supplied predicate**, deliberately: this crate
//! is `no_std` and has no business parsing `/proc`, and `docs/PHASE2.md` §5.1
//! makes the **OFD lock file** the authoritative liveness source, with the
//! participant record's `state`/`pid`/`start_time` (§6.2) as the fallback. That
//! source is being built separately; wiring it in must not require touching this
//! algorithm. Callers pass a closure; [`crate::arena_view::ArenaView`] defaults
//! it to *"assume alive"*, which is the fail-safe direction §6.2 mandates — a
//! false "dead" verdict steals an entry from a working process, a false "alive"
//! verdict merely delays recovery.
//!
//! ## Why the claim is CASed, not stored
//!
//! A8 says `claiming` is "written BEFORE the hash is published". It cannot
//! literally be: the hash CAS is what *grants* ownership of the slot, so nothing
//! may be written to `claiming[i]` before it. That leaves a two-instruction
//! window in which a crash leaves `hashes[i]` set and `claiming[i] == 0` — the
//! very hang A8 exists to remove, just narrower. So the winner *CASes*
//! `CLAIM_UNRECORDED -> me`, and a waiter that finds `CLAIM_UNRECORDED` after the
//! spin limit may take the slot over too. That takeover is **leak-free even when
//! it is wrong**, because the claim CAS happens strictly before any id is
//! allocated: a claimant that loses it has not yet touched `frame_count`, falls
//! back to waiting, and adopts the rescuer's id.
//!
//! ## Residual gap (documented, not fixed here)
//!
//! `claiming` is a `u32` slot index with no incarnation, exactly as A8 specifies.
//! If the claimant dies and its participant slot is recycled by a *live* process
//! before any waiter looks, the waiter judges the entry live and keeps waiting —
//! until that new occupant itself exits. Recovery is delayed, never lost, and
//! nothing is corrupted. Closing it needs a wider `claiming` word carrying
//! `ParticipantRecord::incarnation`, which is a layout change beyond A8.

use crate::error::FrameError;
use crate::sync::{spin, AtomicU32, AtomicU64, Ordering};

/// Sentinel stored in the `ids` array before a winning interner publishes the
/// real id.
///
/// It **must** be `0`: the arena is `alloc_zeroed` and nothing ever pre-fills
/// the id array, so any other sentinel would be unreachable and the
/// publish-then-spin wait loop below would exit immediately on an unpublished
/// slot with a bogus id. `0` is safe to use because frame ids are 1-based (slot
/// `0` of the frame table is the reserved root sentinel), so a published id is
/// never `0`.
pub const ID_UNPUBLISHED: u32 = 0;

/// Published into the `ids` array when the winning interner could *not* complete
/// (the frame table turned out to be full after it had already claimed the hash
/// slot). Waiters observe it and return [`FrameError::CapacityExceeded`] instead
/// of spinning forever on a slot that will never be filled.
///
/// `u32::MAX` is never a real frame id: ids are bounded by `max_frames`, which
/// the arena layout caps far below `u32::MAX`.
pub const ID_FAILED: u32 = u32::MAX;

/// The 64-bit frame-name hash: the first eight bytes of `blake3(name)`, read as a
/// little-endian `u64`.
///
/// This is the approved resolution of the conflict between `docs/PHASE1.md`
/// §5.1 (BLAKE3 name hashing) and its §0 dependency budget: `blake3` is an
/// accepted `no_std` dependency of `tf_tree_core`.
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
/// `#[repr(C, align(64))]`, exactly 64 bytes. All fields are plain integers (no
/// atomics), so the record write is ordered purely by the `ids` publish store:
/// the winner writes the record, then `ids[slot].store(id, Release)`; a reader
/// sees the record only after `ids[slot].load(Acquire) != ID_UNPUBLISHED`.
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
    /// `0` = unspecified, which is what this build writes; 1 = link, 2 = sensor,
    /// 3 = map, 4 = virtual. It exists so a renderer or `tf_tree top` can group
    /// a hundred-frame tree by something more useful than name prefixes, which
    /// is what everybody does instead and which breaks the first time somebody
    /// renames a link.
    pub frame_kind: u8,
    _pad: [u8; 5],
}

// **`size_of` is not a layout.** Every structural check this crate had before
// these pins — `size_of`, `align_of`, `layout_hash`'s region strides — is
// invariant under a *field reorder*, and a reorder changes what every byte on
// disk and in a shared segment **means** while all of them still pass. Two
// builds then attach to each other, agree on `FORMAT_VERSION` and
// `layout_hash`, and read each other's records wrong.
//
// These are wire records: they go into a shared `memfd` another process maps,
// and `write_frozen` memcpys them into a `.tft` a later build opens. Their
// field offsets are part of the format, and nothing asserted them.
//
// Appending a field is still fine — the pins below do not move. *Moving* one is
// a format break and now says so at compile time. See
// `docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md` for
// the neighbouring gap: the region table and `layout_hash`'s stride array are
// also two hand-kept facts with nothing between them.
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
    /// On a hash match this distinguishes a genuine re-intern of the same name
    /// from a 64-bit hash collision. Because the hash is over the *full* name,
    /// equal hashes imply equal full names except for a true hash collision, so
    /// comparing the truncated stored bytes is sufficient.
    #[must_use]
    pub fn name_matches(&self, name: &str) -> bool {
        let src = name.as_bytes();
        let n = src.len().min(48);
        self.name_len as usize == n && self.name[..n] == src[..n]
    }
}

/// Value of a `claiming` entry that names nobody.
///
/// `0` because the arena is `alloc_zeroed` and because participant slots are
/// recorded as `slot + 1` (`docs/PHASE2.md` §1 A3/A6 use the same +1 encoding for
/// claim and topology-lock owners, so slot `0` is a legal owner everywhere).
pub const CLAIM_UNRECORDED: u32 = 0;

/// A claimant that is working but cannot name itself.
///
/// An [`crate::arena_view::ArenaView`] built without `as_participant` has no
/// slot to publish, so under the original encoding it left `claiming` at
/// [`CLAIM_UNRECORDED`] **permanently** — indistinguishable from "the winner
/// died in the two-instruction window before it recorded itself". That
/// conflation caused two distinct bugs:
///
/// * a reader gave up and answered `Ok(None)` — *no such frame* — for a name a
///   live process was actively publishing, and
/// * an identified rescuer took the entry over from a healthy anonymous
///   claimant, allocating a second id for one name and leaking the loser's,
///   inflating `frame_count` for the life of the arena.
///
/// Writing this sentinel instead keeps the two cases apart: `CLAIM_ANONYMOUS`
/// means *somebody is working and nobody can judge them*, so neither a reader
/// nor a rescuer may act on it. Only [`CLAIM_UNRECORDED`] after the spin budget
/// means the window was genuinely abandoned.
pub const CLAIM_ANONYMOUS: u32 = u32::MAX;

/// How many times a waiter spins on an unpublished id before it stops trusting
/// the claimant and checks whether it is still alive (`docs/PHASE2.md` §1 A8).
///
/// The bound is a *liveness-poll interval*, not a timeout: a claimant that the
/// predicate reports alive is waited on again, without limit. Only a claimant
/// that is provably gone (or never recorded itself — see the module docs) loses
/// the entry, so the value trades a little recovery latency against the cost of
/// the check. Under `loom` it is tiny: every extra iteration multiplies the
/// interleavings the model checker must explore.
#[cfg(not(loom))]
pub const INTERN_SPIN_LIMIT: u32 = 10_000;

/// Spin rounds a *reader* waits on an unrecorded claimant before concluding the
/// name is not there.
///
/// A reader cannot tell "healthy winner mid-CAS" from "dead before it recorded
/// itself" — both read `CLAIM_UNRECORDED`. Giving up after one round made
/// `find_frame` report a live, in-flight name as absent, so it waits several.
/// Still bounded, which is the whole point of A8.
pub const READER_UNRECORDED_ROUNDS: u32 = 4;
/// See the `not(loom)` variant.
#[cfg(loom)]
pub const INTERN_SPIN_LIMIT: u32 = 2;

/// The three parallel interning arrays plus the frame allocator they draw ids
/// from — the whole mutable state of `docs/PHASE1.md` §5.1's interning table.
///
/// All three slices have the same length, `next_pow2(2 * max_frames)`, a power of
/// two so `mask == len - 1`.
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
    /// The claimant is gone and this caller now owns the entry: `claiming` names
    /// it, and it must publish a terminal id.
    TakenOver,
    /// The claimant is gone and this caller may not take over (it is read-only).
    /// Nothing was written.
    Abandoned,
    /// An anonymous claimant holds the entry and cannot be judged. Nothing was
    /// written; the caller must report rather than wait or steal.
    Contended,
}

/// Who is waiting, and therefore what they are allowed to do about a claimant
/// that never published.
#[derive(Clone, Copy)]
enum Role {
    /// A writer, identified by its participant slot + 1 ([`CLAIM_UNRECORDED`] if
    /// it is not a registered participant — single-process Phase 1 use).
    Interner(u32),
    /// A lookup. Must not write to the arena, so it can only report the absence.
    Reader,
}

impl InternTable<'_> {
    /// Publish `value` into slot `i` unless a terminal id is already there.
    ///
    /// Returns whichever value is now visible — `value` if we won, otherwise the
    /// one that beat us. The CAS (rather than A8's plain store) is what makes a
    /// takeover racing a resurrected claimant safe: exactly one id is ever
    /// visible for a hash, so no two callers can hand out different ids for the
    /// same name.
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
            // Lost the capacity race (only reachable when several threads intern
            // distinct names at exactly capacity). Give the id back so
            // `frame_count` stays exact, and publish ID_FAILED so waiters on this
            // slot terminate.
            self.frame_count.fetch_sub(1, Ordering::AcqRel);
            return match self.publish(i, ID_FAILED) {
                ID_FAILED => Err(FrameError::CapacityExceeded),
                other => resolve(other, hash, name_matches),
            };
        }
        let id = n + 1;
        write_record(id);
        // The record write above is ordered before this Release CAS; a waiter's
        // Acquire load of `ids[i]` therefore sees a fully-written record.
        let winner = self.publish(i, id);
        if winner == id {
            return Ok(id);
        }
        // Somebody that judged us dead rescued this slot and published first. Its
        // id is the one the name resolves to; ours is abandoned — the record stays
        // written
        // but unreferenced, and `frame_count` over-counts by one. That is the
        // deliberate trade: `fetch_sub`bing it here could hand a *live* id back to
        // the allocator and alias two frames onto one record.
        resolve(winner, hash, name_matches)
    }

    /// Wait for slot `i` to publish, giving up on the claimant if it dies.
    ///
    /// Spins at most [`INTERN_SPIN_LIMIT`] times between liveness checks; see the
    /// module docs for why a `CLAIM_UNRECORDED` claimant is also recoverable and
    /// why doing so cannot leak an id.
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
                // path, so an owner we read is a fully-published participant slot.
                let owner = self.claiming[i].load(Ordering::Acquire);
                match role {
                    Role::Reader => {
                        if owner == CLAIM_ANONYMOUS {
                            // Somebody is working and nobody can judge them, so
                            // this is not evidence of absence. Wait — but
                            // *bounded*, because an unbounded wait here is the
                            // hang A8 exists to prevent, and an anonymous
                            // claimant that dies can never be proven dead.
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
                            // **Not proof of anything.** `CLAIM_UNRECORDED` is
                            // also (a) the two-instruction window between a
                            // healthy winner's hash CAS and its claiming CAS,
                            // and (b) the permanent state of an anonymous
                            // interner. Abandoning on sight made `find_frame`
                            // answer `Ok(None)` — "no such frame" — for a name a
                            // live process was in the middle of publishing.
                            //
                            // A reader cannot resolve the ambiguity, so it buys
                            // patience instead: several full spin rounds before
                            // giving up. That keeps the bound A8 requires while
                            // making the false negative require a claimant
                            // descheduled across ~40 000 spins rather than one.
                            unrecorded_rounds += 1;
                            if unrecorded_rounds >= READER_UNRECORDED_ROUNDS {
                                return Wait::Abandoned;
                            }
                        }
                    }
                    Role::Interner(me) => {
                        if owner == CLAIM_ANONYMOUS {
                            // A live anonymous claimant. Taking this over would
                            // allocate a second id for one name and leak the
                            // loser's, permanently inflating `frame_count` —
                            // the module note about "has not yet touched
                            // frame_count" holds only for an *identified*
                            // claimant, which this is not. Wait, bounded, then
                            // report; never steal.
                            unrecorded_rounds += 1;
                            if unrecorded_rounds >= READER_UNRECORDED_ROUNDS {
                                return Wait::Contended;
                            }
                            continue;
                        }
                        let recoverable = if owner == CLAIM_UNRECORDED {
                            // Nobody recorded themselves: either the claimant was
                            // killed in the window between the two CASes, or it is
                            // anonymous. Only a registered participant may take
                            // over — an anonymous rescuer would CAS 0 -> 0, which
                            // "succeeds" against a perfectly healthy anonymous
                            // claimant and would leak an id every time a thread is
                            // preempted here.
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
                        // Either the claimant is alive, or another rescuer won the
                        // takeover. Both mean somebody is still on the hook for
                        // this slot: keep waiting.
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
        // The owner of this slot ran out of table; the name is not, and never
        // will be, interned (capacity is fixed for the arena's life).
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
/// Open addressing with linear probing over [`InternTable`]. `name_matches` is
/// consulted on a hash hit to detect a collision; `write_record` is called by the
/// slot's owner to populate the frame record before the id is published (at most
/// once per call, from whichever of the two paths — winner or rescuer — this call
/// takes).
///
/// `me` is this interner's participant slot **+ 1**, or [`CLAIM_UNRECORDED`] if
/// the caller is not a registered participant. `claimant_alive` is A8's injected
/// liveness predicate, called with another interner's `me`; see the module docs
/// for why it is a parameter and why it must fail *safe* (return `true` when it
/// cannot tell).
///
/// # Errors
///
/// * [`FrameError::FrameHashCollision`] — a different name already occupies this
///   hash.
/// * [`FrameError::CapacityExceeded`] — the frame table is full, or this name's
///   hash slot was poisoned with [`ID_FAILED`] by an interner that lost the
///   capacity race.
///
/// # Panics
///
/// Never panics for a correctly sized table (a power-of-two length `>= 2 *
/// max_frames`, equal across all three arrays); the mask indexing stays in bounds
/// by construction.
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
    // Guard against an infinite probe on a full table (only reachable on genuine
    // misuse; capacity is checked before we claim a slot). `2 * len` covers the
    // at-most-one wasted iteration a lost CAS costs before advancing.
    for _ in 0..(2 * table.hashes.len()) {
        let cur = table.hashes[i].load(Ordering::Acquire);
        if cur == hash {
            // Existing (or in-flight) entry for this hash: wait for publication,
            // rescuing the slot if its claimant died mid-intern (A8).
            return match table.wait_for_publish(i, Role::Interner(me), &claimant_alive) {
                Wait::Published(id) => resolve(id, hash, &name_matches),
                Wait::TakenOver => table.finish(i, hash, &name_matches, &write_record),
                // `Role::Interner` never abandons: it either publishes or waits.
                Wait::Abandoned => Err(FrameError::CapacityExceeded),
                // An anonymous claimant holds the entry and cannot be judged.
                // Reporting beats stealing (a second id for one name) and beats
                // waiting forever (the hang A8 exists to prevent).
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
                    // A8: record who is working on this slot *before* allocating
                    // an id, so a crash from here on is recoverable and so that
                    // losing this CAS costs nothing.
                    //
                    // An anonymous caller records `CLAIM_ANONYMOUS` rather than
                    // nothing. Leaving `CLAIM_UNRECORDED` behind made a healthy
                    // anonymous winner look exactly like a crashed one, so a
                    // reader would answer "no such frame" for a name being
                    // published right then, and a rescuer would take the entry
                    // over and allocate a second id for it.
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
                        // A rescuer decided we were gone and took the entry over
                        // before we recorded ourselves. It publishes; we wait.
                        continue;
                    }
                    return table.finish(i, hash, &name_matches, &write_record);
                }
                // Lost the race for this slot: re-read it (someone else's hash is
                // now here, or ours — the loop handles both).
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
/// Returns `Ok(None)` when the name was never interned, when its slot was
/// poisoned with [`ID_FAILED`], and — A8 — when its claimant is provably gone and
/// nothing was ever published. The last case is why a read-only participant
/// cannot wedge on a dead writer; it is truthful (no id exists for that name at
/// that instant) and self-correcting (the next interner rescues the slot).
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
                // Nothing was ever published for this name, and nobody live is
                // going to publish it.
                Wait::Abandoned => return Ok(None),
                // `Role::Reader` never takes over.
                Wait::TakenOver => return Ok(None),
                // An anonymous claimant is mid-publish. `Ok(None)` would be a
                // lie — the name exists — so say what is actually true.
                Wait::Contended => return Err(FrameError::InternContended),
            };
            if id == ID_FAILED {
                // An interner claimed this slot and then lost the capacity race:
                // the name was never actually interned.
                return Ok(None);
            }
            return resolve(id, hash, &name_matches).map(Some);
        }
        i = (i + 1) & (mask as usize);
    }
    Ok(None)
}
