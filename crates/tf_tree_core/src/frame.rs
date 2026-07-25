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
    _pad: [u8; 6],
}

#[cfg(not(loom))]
const _: () = {
    assert!(core::mem::size_of::<FrameRecord>() == 64);
    assert!(core::mem::align_of::<FrameRecord>() == 64);
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
            _pad: [0; 6],
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

/// The lock-free interning core.
///
/// Open addressing with linear probing over `hashes`/`ids` (both `len ==
/// next_pow2(2 * max_frames)`, a power of two so `mask == len - 1`). `name_matches`
/// is consulted on a hash hit to detect a collision; `write_record` is called by
/// the unique winner to populate the frame record before the id is published.
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
/// Never panics for a correctly sized table (`hashes.len()` a power of two `>=
/// 2 * max_frames`, `hashes.len() == ids.len()`); the mask indexing stays in
/// bounds by construction.
pub fn intern_core(
    hashes: &[AtomicU64],
    ids: &[AtomicU32],
    frame_count: &AtomicU32,
    capacity: u32,
    hash: u64,
    name_matches: impl Fn(u32) -> bool,
    write_record: impl FnOnce(u32),
) -> Result<u32, FrameError> {
    let mask = (hashes.len() - 1) as u64;
    let mut i = (hash & mask) as usize;
    // Guard against an infinite probe on a full table (only reachable on genuine
    // misuse; capacity is checked before we claim a slot). `2 * len` covers the
    // at-most-one wasted iteration a lost CAS costs before advancing.
    for _ in 0..(2 * hashes.len()) {
        let cur = hashes[i].load(Ordering::Acquire);
        if cur == hash {
            // Existing (or in-flight) entry for this hash: wait for publication.
            let id = loop {
                let id = ids[i].load(Ordering::Acquire);
                if id != ID_UNPUBLISHED {
                    break id;
                }
                spin();
            };
            if id == ID_FAILED {
                // The winner of this slot ran out of table; the name is not, and
                // never will be, interned (capacity is fixed for the arena's life).
                return Err(FrameError::CapacityExceeded);
            }
            return if name_matches(id) {
                Ok(id)
            } else {
                Err(FrameError::FrameHashCollision { hash })
            };
        }
        if cur == 0 {
            // Cheap pre-check: reject an obviously-full table before burning a
            // hash slot on a name that cannot be interned.
            if frame_count.load(Ordering::Relaxed) >= capacity {
                return Err(FrameError::CapacityExceeded);
            }
            match hashes[i].compare_exchange(0, hash, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    let n = frame_count.fetch_add(1, Ordering::AcqRel);
                    if n >= capacity {
                        // Lost the capacity race (only reachable when several
                        // threads intern distinct names at exactly capacity).
                        // Give the id back so `frame_count` stays exact, and
                        // publish ID_FAILED so waiters on this slot terminate.
                        frame_count.fetch_sub(1, Ordering::AcqRel);
                        ids[i].store(ID_FAILED, Ordering::Release);
                        return Err(FrameError::CapacityExceeded);
                    }
                    let id = n + 1;
                    write_record(id);
                    ids[i].store(id, Ordering::Release);
                    return Ok(id);
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
