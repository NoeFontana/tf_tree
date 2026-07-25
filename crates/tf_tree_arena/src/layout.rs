//! Arena layout math — region sizes, offsets, and the layout hash.
//!
//! An [`ArenaLayout`] is the pure description of where every region lives inside
//! the flat arena allocation. It performs no allocation itself; [`crate::heap`]
//! (and, in Phase 2, a memory-mapped backend) consumes it to build a concrete
//! arena. Every region is 64-byte aligned and laid out in header-field order.

use alloc::vec::Vec;

use crate::header::{ArenaHeader, TOPO_BLOCKS};

/// Round `n` up to the next multiple of 64.
const fn align64(n: usize) -> usize {
    (n + 63) & !63
}

/// Smallest power of two `>= n` (with `next_pow2(0) == 1`).
const fn next_pow2(n: usize) -> usize {
    let mut p: usize = 1;
    while p < n {
        p <<= 1;
    }
    p
}

/// A contiguous region within the arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    /// Byte offset from the arena base. Always a multiple of 64.
    pub offset: usize,
    /// Region size in bytes. Always a multiple of 64.
    pub size: usize,
}

/// Error returned when an [`ArenaLayout`] cannot be constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayoutError {
    /// A per-edge ring capacity was neither `0` (static) nor a power of two.
    CapacityNotPowerOfTwo {
        /// Index of the offending edge in `edge_capacities`.
        edge: usize,
        /// The rejected capacity value.
        capacity: u32,
    },
    /// The number of supplied capacities did not equal `max_edges`.
    EdgeCountMismatch {
        /// Declared maximum edge count.
        max_edges: u32,
        /// Number of capacities actually supplied.
        got: usize,
    },
    /// The computed arena is too large to address with the `u32` region offsets
    /// stored in [`ArenaHeader`]. Every region offset must fit in `u32`, which
    /// caps `total_size` at `u32::MAX` (~4 GiB); larger configurations would
    /// silently truncate offsets and corrupt the arena.
    ArenaTooLarge {
        /// The total size, in bytes, that overflowed the `u32` offset model.
        total_size: u64,
    },
}

/// Description of an arena's fixed capacities and the derived region layout.
///
/// Fields are private so the power-of-two invariant on `edge_capacities`
/// (load-bearing invariant 3) cannot be violated after construction; use
/// [`ArenaLayout::new`] and the accessors. The region layout is computed once in
/// [`ArenaLayout::new`] and cached, so the accessors are pure reads.
#[derive(Clone, Debug)]
pub struct ArenaLayout {
    max_frames: u32,
    max_edges: u32,
    max_participants: u32,
    edge_capacities: Vec<u32>,
    computed: Computed,
}

// The eight regions in header order. These indices are used only internally.
const R_HEADER: usize = 0;
const R_FRAME_TABLE: usize = 1;
const R_FRAME_HASH: usize = 2;
const R_TOPO: usize = 3;
const R_CLAIM: usize = 4;
const R_PARTICIPANT: usize = 5;
const R_EDGE: usize = 6;
const R_STAMP: usize = 7;
const R_POSE: usize = 8;
/// Number of regions in header order.
const N_REGIONS: usize = 9;

/// Default participant-table capacity (`docs/PHASE2.md` §1 A6).
pub const DEFAULT_MAX_PARTICIPANTS: u32 = 64;

/// Bytes per interning slot in the frame-hash region.
///
/// Three parallel arrays, in this order, over `next_pow2(2 * max_frames)` slots:
///
/// | array | width | meaning |
/// |---|---|---|
/// | `hashes` | `AtomicU64`, 8 B | the 64-bit frame-name hash; `0` = empty |
/// | `ids` | `AtomicU32`, 4 B | published `FrameId`; `0` = not yet published |
/// | `claiming` | `AtomicU32`, 4 B | **A8**: participant slot + 1 of the interner that won the hash CAS; `0` = unrecorded |
///
/// `claiming` is what makes a crashed interner recoverable (`docs/PHASE2.md` §1
/// A8, §11.3 `intern.after_hash_cas_before_id_store`): without it, a waiter that
/// sees a claimed hash with an unpublished id has no way to find out *who* it is
/// waiting for, and so must wait forever.
///
/// The arrays are laid out contiguously rather than interleaved into a 16-byte
/// record so the hot path — probing `hashes` — keeps its dense cache lines; the
/// other two are touched once per intern.
pub const FRAME_HASH_STRIDE: usize = 8 + 4 + 4;

#[derive(Clone, Copy, Debug)]
struct Computed {
    regions: [Region; N_REGIONS],
    topo_stride: usize,
    slots: usize,
}

/// Derive the region layout from the fixed capacities. Pure arithmetic; called
/// exactly once, from [`ArenaLayout::new`].
fn compute(
    max_frames: u32,
    max_edges: u32,
    max_participants: u32,
    edge_capacities: &[u32],
) -> Computed {
    let mf = max_frames as usize;
    let me = max_edges as usize;
    let mp = max_participants as usize;
    let slots: usize = edge_capacities.iter().map(|&c| c as usize).sum();

    // 12 B / frame: parent AtomicU32 + edge_of_child AtomicU32 + depth AtomicU16
    // + 2 B pad. edge_of_child lives in the block (not a separate region) so plan
    // compilation is an O(1) array walk and the (parent, depth, edge_of_child)
    // triple is published together by the single topology store. (Resolves the
    // inconsistency between the 6-byte stride in `docs/PHASE1.md` §4.3 and
    // edge_of_child living in the topology block per §5.2.)
    //
    // The fields are **atomics** (`docs/PHASE2.md` §1 A1): a reader racing a
    // writer on the same block reads garbage and discards it, but reading a
    // non-atomic `u32` while another *process* writes it is a data race and
    // therefore UB even when the value is thrown away. Relaxed atomic loads
    // compile to the same instruction.
    let topo_stride = align64(mf * 12);
    // Sizes in header order; each aligned so the running offset stays 64-aligned.
    let sizes = [
        256usize,                                       // header
        align64(mf * 64),                               // frame table (64 B / frame)
        align64(next_pow2(2 * mf) * FRAME_HASH_STRIDE), // frame hash (A8)
        TOPO_BLOCKS * topo_stride,                      // topology blocks (A1: four)
        align64(me * 64),                               // claim table (64 B / edge)
        align64(mp * 128),                              // participant table (128 B / slot)
        align64(me * 128),                              // edge table (128 B / edge)
        align64(slots * 8),                             // stamp arena (i64 / slot)
        align64(slots * 64),                            // pose arena (PoseSlot / slot)
    ];

    let mut regions = [Region { offset: 0, size: 0 }; N_REGIONS];
    let mut off = 0usize;
    let mut i = 0;
    while i < N_REGIONS {
        regions[i] = Region {
            offset: off,
            size: sizes[i],
        };
        off += sizes[i];
        i += 1;
    }

    Computed {
        regions,
        topo_stride,
        slots,
    }
}

impl ArenaLayout {
    /// Construct a layout, validating that each per-edge capacity is `0`
    /// (static edge, no ring) or a power of two, and that exactly `max_edges`
    /// capacities were supplied.
    ///
    /// # Errors
    ///
    /// Returns [`LayoutError`] if a capacity is not `0`/power-of-two or if the
    /// capacity count does not match `max_edges`.
    pub fn new(
        max_frames: u32,
        max_edges: u32,
        edge_capacities: Vec<u32>,
    ) -> Result<ArenaLayout, LayoutError> {
        if edge_capacities.len() != max_edges as usize {
            return Err(LayoutError::EdgeCountMismatch {
                max_edges,
                got: edge_capacities.len(),
            });
        }
        for (edge, &capacity) in edge_capacities.iter().enumerate() {
            // 0 is allowed (static edge); otherwise a single power-of-two bit.
            if capacity != 0 && !capacity.is_power_of_two() {
                return Err(LayoutError::CapacityNotPowerOfTwo { edge, capacity });
            }
        }

        let max_participants = DEFAULT_MAX_PARTICIPANTS;
        let computed = compute(max_frames, max_edges, max_participants, &edge_capacities);
        // Every region offset and the slot counts are stored as `u32` in the
        // header. The pose arena is the last region, so its end is `total_size`;
        // if that fits `u32`, every offset (<= total_size) and every slot count
        // (slots * 64 <= total_size) fits too. Reject rather than truncate.
        let total_size = computed.regions[R_POSE].offset + computed.regions[R_POSE].size;
        if total_size > u32::MAX as usize {
            return Err(LayoutError::ArenaTooLarge {
                total_size: total_size as u64,
            });
        }

        Ok(ArenaLayout {
            max_frames,
            max_edges,
            max_participants,
            edge_capacities,
            computed,
        })
    }

    /// The layout implied by totals alone, for validating a header.
    ///
    /// `compute` uses only the **sum** of the per-edge capacities, never their
    /// split, so a segment's `stamp_slots` is enough to reconstruct every region
    /// offset without knowing how the slots were divided between edges. That is
    /// what lets [`crate::mapped::MappedArena::attach`] check a received header
    /// against the layout its own counts imply.
    ///
    /// The returned value carries an empty `edge_capacities` and must not be
    /// used to *build* an arena — only to compare region geometry.
    ///
    /// # Errors
    ///
    /// [`LayoutError::ArenaTooLarge`] if the implied size exceeds the `u32`
    /// offset model.
    pub fn from_totals(
        max_frames: u32,
        max_edges: u32,
        total_slots: u32,
    ) -> Result<ArenaLayout, LayoutError> {
        let max_participants = DEFAULT_MAX_PARTICIPANTS;
        let computed = compute(max_frames, max_edges, max_participants, &[total_slots]);
        let total_size = computed.regions[R_POSE].offset + computed.regions[R_POSE].size;
        if total_size > u32::MAX as usize {
            return Err(LayoutError::ArenaTooLarge {
                total_size: total_size as u64,
            });
        }
        Ok(ArenaLayout {
            max_frames,
            max_edges,
            max_participants,
            edge_capacities: Vec::new(),
            computed,
        })
    }

    /// Maximum number of frames.
    pub fn max_frames(&self) -> u32 {
        self.max_frames
    }

    /// Maximum number of edges.
    pub fn max_edges(&self) -> u32 {
        self.max_edges
    }

    /// Capacity of the participant table, in records.
    pub fn max_participants(&self) -> u32 {
        self.max_participants
    }

    /// The participant table region (`docs/PHASE2.md` §1 A6).
    pub fn participant_table(&self) -> Region {
        self.computed.regions[R_PARTICIPANT]
    }

    /// The validated per-edge ring capacities.
    pub fn edge_capacities(&self) -> &[u32] {
        &self.edge_capacities
    }

    /// The header region (offset 0, size 256).
    pub fn header_region(&self) -> Region {
        self.computed.regions[R_HEADER]
    }

    /// The frame table region.
    pub fn frame_table(&self) -> Region {
        self.computed.regions[R_FRAME_TABLE]
    }

    /// The frame interning hash region.
    pub fn frame_hash(&self) -> Region {
        self.computed.regions[R_FRAME_HASH]
    }

    /// The topology region (both blocks, contiguous).
    pub fn topo_blocks(&self) -> Region {
        self.computed.regions[R_TOPO]
    }

    /// Byte stride between the two topology blocks.
    pub fn topo_block_stride(&self) -> usize {
        self.computed.topo_stride
    }

    /// The claim table region.
    pub fn claim_table(&self) -> Region {
        self.computed.regions[R_CLAIM]
    }

    /// The edge table region.
    pub fn edge_table(&self) -> Region {
        self.computed.regions[R_EDGE]
    }

    /// The stamp arena region.
    pub fn stamp_arena(&self) -> Region {
        self.computed.regions[R_STAMP]
    }

    /// The pose arena region.
    pub fn pose_arena(&self) -> Region {
        self.computed.regions[R_POSE]
    }

    /// Total stamp slots across all edges (sum of ring capacities).
    ///
    /// Guaranteed to fit `u32`: [`ArenaLayout::new`] rejects any layout whose
    /// `total_size` (>= `slots * 64`) exceeds `u32::MAX`.
    pub fn stamp_slots(&self) -> u32 {
        self.computed.slots as u32
    }

    /// Total pose slots across all edges (equal to [`Self::stamp_slots`]).
    pub fn pose_slots(&self) -> u32 {
        self.computed.slots as u32
    }

    /// Total arena size in bytes, 64-byte aligned. Guaranteed `<= u32::MAX`.
    pub fn total_size(&self) -> usize {
        let last = self.computed.regions[R_POSE];
        last.offset + last.size
    }
}

/// FNV-1a fold of the four little-endian bytes of `v` into `h`.
const fn fnv1a_u32(mut h: u32, v: u32) -> u32 {
    let bytes = v.to_le_bytes();
    let mut i = 0;
    while i < 4 {
        h ^= bytes[i] as u32;
        h = h.wrapping_mul(0x0100_0193);
        i += 1;
    }
    h
}

/// Compile-time layout hash of the arena-level structural constants.
///
/// This folds the size and alignment of [`ArenaHeader`] together with the
/// per-region stride constants (bytes-per-element of each region) into a `u32`
/// via FNV-1a. It is written into [`ArenaHeader::layout_hash`] at construction;
/// Phase 2 checks it on attach and rejects a mismatch as a hard error.
///
/// PRE-RESOLVED (orchestrator): this arena-level hash intentionally covers only
/// the header plus region strides. `tf_tree_core` folds its own `#[repr(C)]`
/// record struct sizes into the *full* layout hash later; this function is the
/// arena's contribution, not the whole story.
pub const fn layout_hash() -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    h = fnv1a_u32(h, core::mem::size_of::<ArenaHeader>() as u32);
    h = fnv1a_u32(h, core::mem::align_of::<ArenaHeader>() as u32);
    // Region strides in header order: header size, frame/edge/claim/participant/
    // pose byte widths, frame-hash entry width ([`FRAME_HASH_STRIDE`], 16 since
    // A8 added the `claiming` array), topology per-frame width (12 = parent
    // AtomicU32 + edge_of_child AtomicU32 + depth AtomicU16 + pad), topology
    // block count, stamp width.
    let strides: [u32; 10] = [
        256,
        64,
        FRAME_HASH_STRIDE as u32,
        12,
        TOPO_BLOCKS as u32,
        64,
        128,
        128,
        8,
        64,
    ];
    let mut i = 0;
    while i < strides.len() {
        h = fnv1a_u32(h, strides[i]);
        i += 1;
    }
    h
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use alloc::vec;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

    fn all_regions(l: &ArenaLayout) -> [Region; N_REGIONS] {
        [
            l.header_region(),
            l.frame_table(),
            l.frame_hash(),
            l.topo_blocks(),
            l.claim_table(),
            l.participant_table(),
            l.edge_table(),
            l.stamp_arena(),
            l.pose_arena(),
        ]
    }

    #[test]
    fn rejects_non_power_of_two_capacity() {
        let err = ArenaLayout::new(4, 2, vec![4, 3]).unwrap_err();
        assert_eq!(
            err,
            LayoutError::CapacityNotPowerOfTwo {
                edge: 1,
                capacity: 3
            }
        );
    }

    #[test]
    fn rejects_capacity_count_mismatch() {
        let err = ArenaLayout::new(4, 2, vec![4]).unwrap_err();
        assert_eq!(
            err,
            LayoutError::EdgeCountMismatch {
                max_edges: 2,
                got: 1
            }
        );
    }

    #[test]
    fn rejects_arena_exceeding_u32_offsets() {
        // One edge with a 2^27-slot ring => pose arena alone is 8 GiB, past the
        // u32 offset model. Must be a hard error, not a silent truncation. (The
        // check is pure arithmetic; no 8 GiB allocation happens here.)
        let err = ArenaLayout::new(1, 1, vec![1 << 27]).unwrap_err();
        assert!(
            matches!(err, LayoutError::ArenaTooLarge { total_size } if total_size > u32::MAX as u64),
            "expected ArenaTooLarge with total_size > u32::MAX, got {err:?}"
        );
    }

    #[test]
    fn accepts_arena_just_under_the_u32_limit() {
        // Sanity floor: a multi-hundred-MB arena is fine; only >4 GiB is refused.
        let l = ArenaLayout::new(1000, 1000, vec![4096; 1000]).unwrap();
        assert!(l.total_size() <= u32::MAX as usize);
    }

    #[test]
    fn zero_capacity_is_static_and_allowed() {
        let l = ArenaLayout::new(4, 3, vec![0, 8, 0]).unwrap();
        assert_eq!(l.stamp_slots(), 8);
        assert_eq!(l.pose_slots(), 8);
    }

    #[test]
    fn large_uniform_fixture() {
        // 1000 frames, 1000 edges, 4096 samples per edge.
        let l = ArenaLayout::new(1000, 1000, vec![4096; 1000]).unwrap();

        assert_eq!(
            l.header_region(),
            Region {
                offset: 0,
                size: 256
            }
        );
        assert_eq!(l.frame_table().size, 64_000); // 1000 * 64
        assert_eq!(l.frame_hash().size, 32_768); // next_pow2(2000)=2048 * 16 (A8)
        assert_eq!(l.topo_block_stride(), 12_032); // align64(1000 * 12)
        assert_eq!(l.topo_blocks().size, 48_128); // TOPO_BLOCKS * 12032
        assert_eq!(l.claim_table().size, 64_000); // 1000 * 64
        assert_eq!(l.participant_table().size, 8_192); // 64 * 128
        assert_eq!(l.edge_table().size, 128_000); // 1000 * 128
        assert_eq!(l.stamp_arena().size, 32_768_000); // 4_096_000 * 8
        assert_eq!(l.pose_arena().size, 262_144_000); // 4_096_000 * 64

        // Pose arena is ~260 MB.
        assert!((260_000_000..=263_000_000).contains(&l.pose_arena().size));
        assert_eq!(l.total_size(), 295_257_344);

        // Every region offset is 64-byte aligned and regions are contiguous.
        let regions = all_regions(&l);
        assert_eq!(regions[0].offset, 0);
        for w in regions.windows(2) {
            assert_eq!(w[0].offset % 64, 0);
            assert_eq!(w[0].size % 64, 0);
            assert_eq!(w[0].offset + w[0].size, w[1].offset);
        }
        assert_eq!(l.total_size() % 64, 0);
    }

    #[test]
    fn small_mixed_capacity_fixture() {
        // 8 frames, 4 edges, capacities [16, 0, 4, 64] -> sum 84 slots.
        let l = ArenaLayout::new(8, 4, vec![16, 0, 4, 64]).unwrap();

        assert_eq!(l.frame_table().size, 512); // 8 * 64
        assert_eq!(l.frame_hash().size, 256); // next_pow2(16)=16 * 16 (A8)
        assert_eq!(l.topo_block_stride(), 128); // align64(8 * 12 = 96)
        assert_eq!(l.topo_blocks().size, 512); // TOPO_BLOCKS * 128
        assert_eq!(l.claim_table().size, 256); // 4 * 64
        assert_eq!(l.participant_table().size, 8_192); // 64 * 128
        assert_eq!(l.edge_table().size, 512); // 4 * 128
        assert_eq!(l.stamp_slots(), 84);
        assert_eq!(l.stamp_arena().size, 704); // align64(84 * 8 = 672)
        assert_eq!(l.pose_arena().size, 5_376); // 84 * 64 (already aligned)
        assert_eq!(l.total_size(), 16_576);

        let regions = all_regions(&l);
        for w in regions.windows(2) {
            assert_eq!(w[0].offset % 64, 0);
            assert_eq!(w[0].size % 64, 0);
            assert_eq!(w[0].offset + w[0].size, w[1].offset);
        }
    }

    #[test]
    fn layout_hash_is_deterministic_and_stable() {
        // Snapshot: any change to the header layout or region strides changes
        // this value, which is exactly what Phase 2's attach check relies on.
        //
        // Last changed by A8 (`claiming` widened the frame-hash stride from 12 to
        // 16 bytes): 0x1F32_7F69 -> 0x9075_90F5. `FORMAT_VERSION` stays 2 — it was
        // already bumped for A6's participant table and has not shipped.
        assert_eq!(layout_hash(), layout_hash());
        assert_ne!(layout_hash(), 0);
        assert_eq!(layout_hash(), 0x9075_90F5);
    }

    #[test]
    fn layout_invariants_hold_over_random_shapes() {
        let cap = prop_oneof![Just(0u32), (0u32..=16u32).prop_map(|k| 1u32 << k)];
        let strat = (0u32..=64u32, 0usize..=32usize).prop_flat_map(move |(mf, ne)| {
            (
                Just(mf),
                Just(ne as u32),
                proptest::collection::vec(cap.clone(), ne),
            )
        });

        // Fixed 32-byte seed: reproducible across runs and CI. The case count is
        // cut hard under Miri: 10_000 cases of the interpreter is tens of minutes
        // and makes `just miri` impractical to run at all. The layout math is pure
        // integer arithmetic, so a small sample exercises the same code paths for
        // UB purposes; the full sweep still runs on the normal test path.
        let mut runner = TestRunner::new_with_rng(
            Config {
                cases: if cfg!(miri) { 64 } else { 10_000 },
                failure_persistence: None,
                ..Config::default()
            },
            TestRng::from_seed(RngAlgorithm::ChaCha, &[0x42; 32]),
        );

        runner
            .run(&strat, |(mf, me, caps)| {
                let sum: u64 = caps.iter().map(|&c| c as u64).sum();
                let l = ArenaLayout::new(mf, me, caps).unwrap();

                let regions = all_regions(&l);
                prop_assert_eq!(regions[0].offset, 0);
                prop_assert_eq!(regions[0].size, 256);
                for w in regions.windows(2) {
                    prop_assert_eq!(w[0].offset % 64, 0);
                    prop_assert_eq!(w[0].size % 64, 0);
                    prop_assert_eq!(w[0].offset + w[0].size, w[1].offset);
                }
                let last = regions[N_REGIONS - 1];
                prop_assert_eq!(l.total_size(), last.offset + last.size);
                prop_assert_eq!(l.total_size() % 64, 0);
                prop_assert_eq!(u64::from(l.stamp_slots()), sum);
                prop_assert_eq!(u64::from(l.pose_slots()), sum);
                Ok(())
            })
            .unwrap();
    }
}
