//! Validating an arena header that came from somewhere else.
//!
//! Two backends map bytes this process did not write: [`crate::mapped`] takes
//! them from a peer's `memfd`, and [`crate::frozen`] takes them from a file on
//! disk. Both must decide whether the header in front of them describes an arena
//! *this build* can read, and the answer has to be the same in both — a check
//! that exists on one path and not the other is a hole with a filename on it.
//!
//! So the checks live here, once, and both backends call
//! [`validate_arena_header`]. The alternative (each backend validating what its
//! author remembered) is exactly how [`ShmError::HeaderInconsistent`] came to be
//! missing the participant-table region for a release: `ArenaView::participants`
//! builds a slice straight off `participant_table_off`, and nothing checked it.
//!
//! [`ShmError`] lives here rather than in [`crate::mapped`] for the same reason:
//! it is the vocabulary of *validating foreign arena bytes*, which is now two
//! backends' shared concern and not the `memfd` one's private business.

use crate::header::{ArenaHeader, FORMAT_VERSION, TF_TREE_MAGIC};
use crate::layout::{layout_hash, ArenaLayout};

/// Everything that can go wrong obtaining or validating a shared segment.
///
/// `Copy` and `String`-free, like every other error in this workspace
/// (`docs/PROJECT.md` §5): an errno and what was being attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShmError {
    /// `memfd_create` failed.
    Create(rustix::io::Errno),
    /// `ftruncate` to the arena size failed.
    Truncate(rustix::io::Errno),
    /// `mmap` failed.
    Map(rustix::io::Errno),
    /// `F_ADD_SEALS` failed. The segment is not safe to share.
    Seal(rustix::io::Errno),
    /// `F_GET_SEALS` failed, so the seals could not be verified.
    SealQuery(rustix::io::Errno),
    /// The segment is missing `F_SEAL_SHRINK`/`F_SEAL_GROW`, so it could be
    /// truncated under a reader and fault it with `SIGBUS`.
    Unsealed,
    /// `fstat` on the segment failed.
    Stat(rustix::io::Errno),
    /// `getrandom` could not fill the arena's `instance_uuid`.
    ///
    /// Deliberately fatal rather than falling back to a counter or a timestamp:
    /// a *guessable* instance id still compares equal to itself, so the
    /// split-brain check it exists for would keep passing while no longer
    /// meaning anything.
    Random(rustix::io::Errno),
    /// The fd's size disagrees with the header's `arena_size`.
    SizeMismatch {
        /// Bytes the segment actually has.
        actual: u64,
        /// Bytes the header claims.
        expected: u64,
    },
    /// The first eight bytes are not `TF_TREE_MAGIC` — not a tf_tree arena.
    BadMagic,
    /// The segment was written by a different `FORMAT_VERSION`.
    VersionMismatch {
        /// Version found in the segment.
        found: u32,
        /// Version this build speaks.
        expected: u32,
    },
    /// The segment's record layout differs from this build's. Attaching anyway
    /// would reinterpret every offset.
    LayoutMismatch {
        /// Hash found in the segment.
        found: u32,
        /// Hash this build computes.
        expected: u32,
    },
    /// The segment is smaller than an `ArenaHeader`, so it cannot even be
    /// validated.
    TooSmall,
    /// Every participant slot is taken, so this process cannot join.
    ParticipantTableFull,
    /// [`crate::AttachMode::ReadWrite`] was asked for over a bare file
    /// descriptor, which takes no participant lock byte.
    ///
    /// **The fd-passing attach is for readers**
    /// (`docs/decisions/0028-the-slot-a-killed-participant-keeps.md`, open
    /// question 1). A process that publishes joins through the rendezvous —
    /// `tf_tree::Open` — which takes an OFD lock byte for its slot *before* the
    /// arena record is written, and that byte is what decides whether the slot
    /// may be reclaimed. A writer registered over a raw descriptor holds a live
    /// record with a permanently free byte, which is indistinguishable, by the
    /// byte alone, from a slot leaked by a killed process.
    ///
    /// Attach [`crate::AttachMode::ReadOnly`] over the descriptor, or build the
    /// writer on `tf_tree::Open`.
    ReadWriteNeedsRendezvous,
    /// The header's region offsets do not match the geometry its own capacities
    /// imply, so the regions cannot be trusted to lie within the segment.
    ///
    /// Distinct from [`ShmError::LayoutMismatch`], which compares against a
    /// *build* constant: this catches a header that is internally inconsistent,
    /// whether from a peer bug, a scribbled byte, or a build that shares this
    /// one's record sizes but not its capacities.
    HeaderInconsistent,
}

/// Decide whether `h` describes an arena of `size` bytes that this build can
/// read, without touching anything outside the header.
///
/// Called by every backend that maps foreign bytes, **after** the header is
/// mapped and **before** any region offset in it is used to form a slice. The
/// order of the checks is deliberate: identity (magic), then vocabulary
/// (version), then geometry (hash), then self-consistency — each one narrowing
/// what the next is allowed to assume.
///
/// # Errors
///
/// [`ShmError::BadMagic`], [`ShmError::VersionMismatch`],
/// [`ShmError::LayoutMismatch`], [`ShmError::SizeMismatch`] or
/// [`ShmError::HeaderInconsistent`], in that order of precedence.
pub(crate) fn validate_arena_header(h: &ArenaHeader, size: u64) -> Result<(), ShmError> {
    if h.magic != u64::from_le_bytes(TF_TREE_MAGIC) {
        return Err(ShmError::BadMagic);
    }
    if h.format_version != FORMAT_VERSION {
        return Err(ShmError::VersionMismatch {
            found: h.format_version,
            expected: FORMAT_VERSION,
        });
    }
    // The layout hash is what makes a mismatched build fail loudly instead of
    // reading every region at the wrong offset.
    if h.layout_hash != layout_hash() {
        return Err(ShmError::LayoutMismatch {
            found: h.layout_hash,
            expected: layout_hash(),
        });
    }
    if h.arena_size != size {
        return Err(ShmError::SizeMismatch {
            actual: size,
            expected: h.arena_size,
        });
    }

    // `layout_hash()` is a *build* constant — it pins the record sizes this
    // binary was compiled against, not this segment's capacities. Two builds
    // can agree on it and disagree about `max_frames`. Since `ArenaView` forms
    // slices straight off these offsets, an inconsistent header would produce
    // out-of-bounds reads rather than an error, so recompute the geometry the
    // header's own counts imply and require it to match.
    //
    // `from_totals` is exact here: the region layout depends only on the *sum*
    // of the per-edge capacities, which is `stamp_slots`.
    let implied = ArenaLayout::from_totals(h.max_frames, h.max_edges, h.stamp_slots)
        .map_err(|_| ShmError::HeaderInconsistent)?;
    let matches = implied.total_size() as u64 == h.arena_size
        && implied.frame_table().offset as u32 == h.frame_table_off
        && implied.frame_hash().offset as u32 == h.frame_hash_off
        && implied.topo_blocks().offset as u32 == h.topo_block_off
        && implied.topo_block_stride() as u32 == h.topo_block_stride
        && implied.claim_table().offset as u32 == h.claim_table_off
        // A6's region. Omitting these was a real hole: `ArenaView::participants`
        // builds a slice from `participant_table_off` and `max_participants`
        // and its SAFETY comment cites *this* check as what bounds them, so a
        // header carrying garbage there produced an out-of-bounds slice —
        // exactly the failure this validation exists to prevent.
        && implied.participant_table().offset as u32 == h.participant_table_off
        && implied.max_participants() == h.max_participants
        && implied.edge_table().offset as u32 == h.edge_table_off
        && implied.stamp_arena().offset as u32 == h.stamp_arena_off
        && implied.pose_arena().offset as u32 == h.pose_arena_off
        // v3: the counter regions are part of the geometry a foreign header
        // must agree about. Without these two, a header claiming a v3 layout
        // hash could still point them anywhere, and §5.2's readers would build
        // slices from the numbers — the same failure the participant-table
        // check above exists to prevent.
        && implied.edge_counters().offset as u32 == h.edge_counters_off
        && implied.participant_counters().offset as u32 == h.participant_counters_off
        && h.stamp_slots == h.pose_slots;
    if !matches {
        return Err(ShmError::HeaderInconsistent);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::heap::{Arena, HeapArena};
    use alloc::vec;

    fn arena() -> HeapArena {
        let layout = ArenaLayout::new(8, 4, vec![16, 0, 4, 64]).unwrap();
        HeapArena::new(&layout, 0, 0, [0; 16])
    }

    /// Every field the geometry block compares, scrambled one at a time.
    ///
    /// The block is the only thing bounding `participant_table_off` /
    /// `max_participants` before `ArenaView::participants` builds a slice from
    /// them, and this module's own header records that omitting it shipped an
    /// out-of-bounds slice for a release. Poking each field individually is what
    /// makes a *partial* deletion attributable: a single scrambled-header case
    /// would pass as long as any one comparison survived.
    ///
    /// Mutant: drop any one `&& implied.… == h.…` conjunct ⇒ the case naming that
    /// field reports `Ok(())` and fails.
    #[test]
    fn a_header_that_disagrees_with_its_own_counts_is_refused() {
        let good = arena();
        assert_eq!(
            validate_arena_header(good.header(), good.len() as u64),
            Ok(()),
            "the fixture must pass, or the failures below prove nothing"
        );

        type Poke = fn(&mut ArenaHeader);
        let pokes: [(&str, Poke); 13] = [
            ("frame_table_off", |h| h.frame_table_off += 64),
            ("frame_hash_off", |h| h.frame_hash_off += 64),
            ("topo_block_off", |h| h.topo_block_off += 64),
            ("topo_block_stride", |h| h.topo_block_stride += 64),
            ("claim_table_off", |h| h.claim_table_off += 64),
            ("participant_table_off", |h| h.participant_table_off += 64),
            ("max_participants", |h| h.max_participants += 1),
            ("edge_table_off", |h| h.edge_table_off += 64),
            ("stamp_arena_off", |h| h.stamp_arena_off += 64),
            ("pose_arena_off", |h| h.pose_arena_off += 64),
            ("edge_counters_off", |h| h.edge_counters_off += 64),
            ("participant_counters_off", |h| {
                h.participant_counters_off += 64
            }),
            ("pose_slots", |h| h.pose_slots += 1),
        ];

        for (field, poke) in pokes {
            let a = arena();
            let size = a.len() as u64;
            // SAFETY: this test uniquely owns `a`, whose base is a live,
            // 64-byte-aligned, initialized `ArenaHeader` written by
            // `HeapArena::new`. No other reference to it is live across this
            // call, so the `&mut` is unaliased for its whole (statement-long)
            // lifetime.
            unsafe { poke(&mut *a.base().cast::<ArenaHeader>()) };
            assert_eq!(
                validate_arena_header(a.header(), size),
                Err(ShmError::HeaderInconsistent),
                "{field} is not compared against the implied geometry"
            );
        }
    }

    /// Identity, then vocabulary, then geometry, then self-consistency.
    ///
    /// The order is documented on [`validate_arena_header`] as "each one
    /// narrowing what the next is allowed to assume", and it is observable only
    /// by breaking several fields at once and watching which error comes out
    /// first. Each step below repairs exactly the field the previous step's
    /// error named, so the sequence of errors *is* the order.
    ///
    /// Mutant: hoist the version check above the magic check ⇒ the first
    /// assertion sees `VersionMismatch` and fails.
    #[test]
    fn the_checks_run_in_the_documented_order() {
        let a = arena();
        let size = a.len() as u64;

        // SAFETY: as in the test above — sole owner, live initialized header,
        // no other reference live across the `&mut`'s use.
        let h = unsafe { &mut *a.base().cast::<ArenaHeader>() };
        h.magic ^= 1;
        h.format_version ^= 0x5555;
        h.layout_hash ^= 0x5555;
        assert_eq!(
            validate_arena_header(a.header(), size),
            Err(ShmError::BadMagic)
        );

        // SAFETY: as above.
        let h = unsafe { &mut *a.base().cast::<ArenaHeader>() };
        h.magic ^= 1;
        assert_eq!(
            validate_arena_header(a.header(), size),
            Err(ShmError::VersionMismatch {
                found: FORMAT_VERSION ^ 0x5555,
                expected: FORMAT_VERSION,
            })
        );

        // SAFETY: as above.
        let h = unsafe { &mut *a.base().cast::<ArenaHeader>() };
        h.format_version ^= 0x5555;
        assert_eq!(
            validate_arena_header(a.header(), size),
            Err(ShmError::LayoutMismatch {
                found: layout_hash() ^ 0x5555,
                expected: layout_hash(),
            })
        );

        // SAFETY: as above.
        let h = unsafe { &mut *a.base().cast::<ArenaHeader>() };
        h.layout_hash ^= 0x5555;
        assert_eq!(
            validate_arena_header(a.header(), size - 64),
            Err(ShmError::SizeMismatch {
                actual: size - 64,
                expected: size,
            })
        );
    }
}
