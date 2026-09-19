//! Validating an arena header that came from somewhere else.
//!
//! [`crate::mapped`] (a peer's `memfd`) and [`crate::frozen`] (a file) both
//! decide through [`validate_arena_header`], so the answer is the same on both
//! paths. [`ShmError`] lives here as the vocabulary of validating foreign bytes.

use crate::header::{ArenaHeader, FORMAT_VERSION, TF_TREE_MAGIC};
use crate::layout::{layout_hash, ArenaLayout};

/// Everything that can go wrong obtaining or validating a shared segment.
///
/// `Copy` and `String`-free (`docs/PROJECT.md` §5).
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
    /// The segment is missing `F_SEAL_SHRINK`/`F_SEAL_GROW` (`SIGBUS` risk).
    Unsealed,
    /// `fstat` on the segment failed.
    Stat(rustix::io::Errno),
    /// `getrandom` could not fill the arena's `instance_uuid`. Fatal rather than
    /// falling back to a guessable id, which would defeat the split-brain check.
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
    /// This process could not register in the arena's participant table.
    ///
    /// **Not evidence that the table is full**: the attach raises it for any
    /// refusal to register, usually the granted slot being taken or out of range.
    /// A full table is refused earlier, by the rendezvous.
    ParticipantTableFull,
    /// [`crate::AttachMode::ReadWrite`] was asked for over a bare file
    /// descriptor, which takes no participant lock byte.
    ///
    /// The fd-passing attach is for readers (`docs/decisions/0028`, open
    /// question 1); writers join through `tf_tree::Open`.
    ReadWriteNeedsRendezvous,
    /// The header's region offsets do not match the geometry its own capacities
    /// imply. Distinct from [`ShmError::LayoutMismatch`], which compares against
    /// a *build* constant.
    HeaderInconsistent,
}

// `Display` and `core::error::Error` follow `docs/decisions/0059` decision 2;
// the match is exhaustive, so a new variant fails to compile here.

/// **The text is a diagnostic, not a compatibility promise** (`docs/API.md`
/// R5); callers match on the discriminant. `source` returns `None`.
impl core::fmt::Display for ShmError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            ShmError::Create(e) => write!(
                f,
                "memfd_create failed with errno {} (Create)",
                e.raw_os_error()
            ),
            ShmError::Truncate(e) => write!(
                f,
                "sizing the segment failed with errno {} (Truncate)",
                e.raw_os_error()
            ),
            ShmError::Map(e) => write!(
                f,
                "mapping the segment failed with errno {} (Map)",
                e.raw_os_error()
            ),
            ShmError::Seal(e) => write!(
                f,
                "sealing the segment failed with errno {} (Seal)",
                e.raw_os_error()
            ),
            ShmError::SealQuery(e) => write!(
                f,
                "reading the segment's seals failed with errno {} (SealQuery)",
                e.raw_os_error()
            ),
            ShmError::Unsealed => write!(
                f,
                "the segment is not sealed against shrinking and growing (Unsealed)"
            ),
            ShmError::Stat(e) => write!(
                f,
                "fstat on the segment failed with errno {} (Stat)",
                e.raw_os_error()
            ),
            ShmError::Random(e) => write!(
                f,
                "getrandom for the arena instance id failed with errno {} (Random)",
                e.raw_os_error()
            ),
            ShmError::SizeMismatch { actual, expected } => write!(
                f,
                "arena is {actual} bytes but its header says {expected} (SizeMismatch)"
            ),
            ShmError::BadMagic => write!(
                f,
                "the arena header does not start with the tf_tree magic (BadMagic)"
            ),
            ShmError::VersionMismatch { found, expected } => write!(
                f,
                "arena format version {found} is not this build's {expected} (VersionMismatch)"
            ),
            ShmError::LayoutMismatch { found, expected } => write!(
                f,
                "arena layout hash 0x{found:08X} is not this build's 0x{expected:08X} (LayoutMismatch)"
            ),
            ShmError::TooSmall => write!(
                f,
                "the segment is smaller than an arena header (TooSmall)"
            ),
            ShmError::ParticipantTableFull => write!(
                f,
                "this process could not register in the arena's participant table (ParticipantTableFull)"
            ),
            ShmError::ReadWriteNeedsRendezvous => write!(
                f,
                "an attach over a bare descriptor cannot be read-write (ReadWriteNeedsRendezvous)"
            ),
            ShmError::HeaderInconsistent => write!(
                f,
                "the arena header's region offsets disagree with its capacities (HeaderInconsistent)"
            ),
        }
    }
}

/// Lets a `ShmError` leave a function through `?` into `Box<dyn Error>`.
/// `source` is `None`: what a variant carries is already in its `Display`.
impl core::error::Error for ShmError {}

/// Decide whether `h` describes an arena of `size` bytes that this build can
/// read, without touching anything outside the header.
///
/// Call **after** the header is mapped and **before** any region offset is used
/// to form a slice. Order: identity (magic), vocabulary (version), geometry
/// (hash), then self-consistency.
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
    // A mismatched build must fail loudly, not read regions at wrong offsets.
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

    // `layout_hash()` pins record sizes, not capacities, and `ArenaView` forms
    // slices off these offsets: recompute the geometry the counts imply.
    let implied = ArenaLayout::from_totals(h.max_frames, h.max_edges, h.stamp_slots)
        .map_err(|_| ShmError::HeaderInconsistent)?;
    let matches = implied.total_size() as u64 == h.arena_size
        && implied.frame_table().offset as u32 == h.frame_table_off
        && implied.frame_hash().offset as u32 == h.frame_hash_off
        && implied.topo_blocks().offset as u32 == h.topo_block_off
        && implied.topo_block_stride() as u32 == h.topo_block_stride
        && implied.claim_table().offset as u32 == h.claim_table_off
        // `ArenaView::participants` builds a slice from these and its SAFETY
        // comment cites this check.
        && implied.participant_table().offset as u32 == h.participant_table_off
        && implied.max_participants() == h.max_participants
        && implied.edge_table().offset as u32 == h.edge_table_off
        && implied.stamp_arena().offset as u32 == h.stamp_arena_off
        && implied.pose_arena().offset as u32 == h.pose_arena_off
        // v3: the counter regions are part of the geometry.
        && implied.edge_counters().offset as u32 == h.edge_counters_off
        && implied.participant_counters().offset as u32 == h.participant_counters_off
        && h.stamp_slots == h.pose_slots;
    if !matches {
        return Err(ShmError::HeaderInconsistent);
    }
    Ok(())
}

/// One value of every `ShmError` variant, integers at their maximum and each
/// `Errno` at 4095, paired with the numbers `docs/decisions/0059` decision 2(a)
/// requires in the text. Shared with `frozen.rs`'s rendering test. Guarded by
/// the exhaustive `shm_error_index` and
/// `every_shm_error_variant_renders_by_0059s_rules`.
#[cfg(test)]
pub(crate) fn every_shm_error(
) -> alloc::vec::Vec<(ShmError, alloc::vec::Vec<alloc::string::String>)> {
    use alloc::string::ToString;
    use alloc::vec;
    use rustix::io::Errno;

    let errno = Errno::from_raw_os_error(4095);
    let e = || vec!["errno 4095".to_string()];
    let u64s = || vec![u64::MAX.to_string(), u64::MAX.to_string()];
    let u32s = || vec![u32::MAX.to_string(), u32::MAX.to_string()];
    let hashes = || vec!["0xFFFFFFFF".to_string(), "0xFFFFFFFF".to_string()];
    vec![
        (ShmError::Create(errno), e()),
        (ShmError::Truncate(errno), e()),
        (ShmError::Map(errno), e()),
        (ShmError::Seal(errno), e()),
        (ShmError::SealQuery(errno), e()),
        (ShmError::Unsealed, vec![]),
        (ShmError::Stat(errno), e()),
        (ShmError::Random(errno), e()),
        (
            ShmError::SizeMismatch {
                actual: u64::MAX,
                expected: u64::MAX,
            },
            u64s(),
        ),
        (ShmError::BadMagic, vec![]),
        (
            ShmError::VersionMismatch {
                found: u32::MAX,
                expected: u32::MAX,
            },
            u32s(),
        ),
        (
            ShmError::LayoutMismatch {
                found: u32::MAX,
                expected: u32::MAX,
            },
            hashes(),
        ),
        (ShmError::TooSmall, vec![]),
        (ShmError::ParticipantTableFull, vec![]),
        (ShmError::ReadWriteNeedsRendezvous, vec![]),
        (ShmError::HeaderInconsistent, vec![]),
    ]
}

/// The guard on [`every_shm_error`]: exhaustive, with no catch-all.
#[cfg(test)]
pub(crate) fn shm_error_index(e: &ShmError) -> usize {
    match e {
        ShmError::Create(_) => 0,
        ShmError::Truncate(_) => 1,
        ShmError::Map(_) => 2,
        ShmError::Seal(_) => 3,
        ShmError::SealQuery(_) => 4,
        ShmError::Unsealed => 5,
        ShmError::Stat(_) => 6,
        ShmError::Random(_) => 7,
        ShmError::SizeMismatch { .. } => 8,
        ShmError::BadMagic => 9,
        ShmError::VersionMismatch { .. } => 10,
        ShmError::LayoutMismatch { .. } => 11,
        ShmError::TooSmall => 12,
        ShmError::ParticipantTableFull => 13,
        ShmError::ReadWriteNeedsRendezvous => 14,
        ShmError::HeaderInconsistent => 15,
    }
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

    /// Every field the geometry block compares, scrambled one at a time,
    /// including `arena_size`, which ties the derived geometry to the mapped
    /// length. `size` is read *after* the poke so `SizeMismatch` cannot
    /// pre-empt the geometry block (it is covered by
    /// `the_checks_run_in_the_documented_order`).
    ///
    /// Mutant: drop any one conjunct of the `let matches = …` chain ⇒ the case
    /// naming that field reports `Ok(())` and fails.
    #[test]
    fn a_header_that_disagrees_with_its_own_counts_is_refused() {
        let good = arena();
        assert_eq!(
            validate_arena_header(good.header(), good.len() as u64),
            Ok(()),
            "the fixture must pass, or the failures below prove nothing"
        );

        type Poke = fn(&mut ArenaHeader);
        let pokes: [(&str, Poke); 14] = [
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
            ("arena_size", |h| h.arena_size -= 4096),
        ];

        for (field, poke) in pokes {
            let a = arena();
            // SAFETY: this test uniquely owns `a`, whose base is a live,
            // 64-byte-aligned, initialized `ArenaHeader`; the `&mut` is unaliased.
            unsafe { poke(&mut *a.base().cast::<ArenaHeader>()) };
            // Read after the poke so the `arena_size` row reaches the geometry.
            let size = a.header().arena_size;
            assert_eq!(
                validate_arena_header(a.header(), size),
                Err(ShmError::HeaderInconsistent),
                "{field} is not compared against the implied geometry"
            );
        }
    }

    /// Identity, then vocabulary, then geometry, then self-consistency: each step
    /// repairs the field the previous error named, so the errors *are* the order.
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

    /// `docs/decisions/0059` step 1(b): every variant renders structurally, never
    /// as a pinned sentence (`docs/API.md` R5).
    ///
    /// **Mutant (M1):** `SizeMismatch`'s arm → `write!(f, "{self:?}")` ⇒ this
    /// and `frozen.rs`'s test fail.
    #[test]
    fn every_shm_error_variant_renders_by_0059s_rules() {
        use crate::render_test::{assert_structure, variant_name};
        use alloc::format;
        use alloc::vec::Vec;

        let all = every_shm_error();
        let mut hit: Vec<usize> = all.iter().map(|(e, _)| shm_error_index(e)).collect();
        hit.sort_unstable();
        hit.dedup();
        assert_eq!(
            hit,
            (0..16).collect::<Vec<_>>(),
            "every_shm_error must hold one value of every variant"
        );

        for (e, numbers) in &all {
            let debug = format!("{e:?}");
            assert_structure(&format!("{e}"), &debug, variant_name(&debug), numbers);
        }
    }

    /// Decision 2(f): on `tf_tree`'s joiner path this variant erases a taken or
    /// out-of-range slot, so its text may not say the table is full.
    #[test]
    fn participant_table_full_does_not_claim_a_full_table() {
        use alloc::string::ToString;

        let shown = ShmError::ParticipantTableFull.to_string();
        let prose = shown
            .strip_suffix("(ParticipantTableFull)")
            .expect("the search key is last");
        assert!(
            !prose.to_ascii_lowercase().contains("full"),
            "{shown:?} claims a full table"
        );
    }

    /// A `ShmError` leaves a function through `?` into `Box<dyn Error>`.
    /// **Mutant (M4):** delete `impl core::error::Error for ShmError` ⇒ the tests
    /// do not compile.
    #[test]
    fn a_shm_error_can_leave_a_function_as_box_dyn_error() {
        use alloc::boxed::Box;
        use alloc::string::ToString;

        fn attach() -> Result<(), Box<dyn core::error::Error>> {
            Err(ShmError::Unsealed)?;
            Ok(())
        }
        let e = attach().unwrap_err();
        assert!(e.to_string().ends_with("(Unsealed)"));
        assert!(e.source().is_none());
    }
}
