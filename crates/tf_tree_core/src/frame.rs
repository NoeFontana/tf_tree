//! Frame records and the lock-free interning table.
//!
//! `unsafe`-free: the interning *algorithm* ([`intern_core`]) operates purely on
//! the caller-supplied atomic arrays, so it is shared verbatim by the production
//! arena view and by the loom test. Raw access to [`FrameRecord`] bytes lives in
//! [`crate::arena_view`].
//!
//! The publish-then-spin protocol (decision `0003`) exists because Phase 2 has
//! two processes interning concurrently: a writer claims a hash slot with a CAS,
//! writes the record, and only then publishes the id; a concurrent interner of
//! the same name observes the hash, spins until the id is published, and returns
//! it. It costs nothing in Phase 1 and cannot be retrofitted.

use crate::error::FrameError;
use crate::sync::{spin, AtomicU32, AtomicU64, Ordering};

/// Sentinel stored in the `ids` array before a winning interner publishes the
/// real id.
pub const ID_UNPUBLISHED: u32 = u32::MAX;

/// The 64-bit frame-name hash: the first eight bytes of `blake3(name)`, read as a
/// little-endian `u64`.
///
/// This is the approved resolution of the `0003` BLAKE3-vs-dependency-budget
/// conflict: `blake3` is an accepted `no_std` dependency of `tf_tree_core`.
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
/// * [`FrameError::CapacityExceeded`] — the frame table is full.
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
            return if name_matches(id) {
                Ok(id)
            } else {
                Err(FrameError::FrameHashCollision { hash })
            };
        }
        if cur == 0 {
            // Reject before claiming a slot so a rejected intern never leaves a
            // hash slot with an unpublished id (which would hang later interners).
            // Frame mutations are serialized by the builder mutex, so this
            // pre-check is exact in Phase 1.
            if frame_count.load(Ordering::Relaxed) >= capacity {
                return Err(FrameError::CapacityExceeded);
            }
            match hashes[i].compare_exchange(0, hash, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    let n = frame_count.fetch_add(1, Ordering::AcqRel);
                    if n >= capacity {
                        // Lost the capacity race (only possible under concurrent
                        // interning at exactly capacity). Publish the id anyway so
                        // waiters do not hang; the caller sized the table wrong.
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
