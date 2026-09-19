//! Arena layout math — region sizes, offsets, and the layout hash.
//!
//! An [`ArenaLayout`] describes where every region lives; every region is 64-byte
//! aligned, in header-field order.

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
    /// The computed arena exceeds `u32::MAX` bytes, the limit of the `u32` region offsets.
    ArenaTooLarge {
        /// The total size, in bytes, that overflowed the `u32` offset model.
        total_size: u64,
    },
}

// `Display` and `core::error::Error` follow `docs/decisions/0059`; the match is exhaustive on purpose.

/// **The text is a diagnostic, not a compatibility promise** (`docs/API.md` R5); `source()` is `None`
/// (`docs/decisions/0059` decisions 2 and 3).
impl core::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            LayoutError::CapacityNotPowerOfTwo { edge, capacity } => write!(
                f,
                "edge {edge} capacity {capacity} is neither 0 nor a power of two (CapacityNotPowerOfTwo)"
            ),
            LayoutError::EdgeCountMismatch { max_edges, got } => write!(
                f,
                "{got} edge capacities were given for {max_edges} edges (EdgeCountMismatch)"
            ),
            LayoutError::ArenaTooLarge { total_size } => write!(
                f,
                "the arena would be {total_size} bytes, past its u32 offsets (ArenaTooLarge)"
            ),
        }
    }
}

/// Lets a `LayoutError` convert through `?` into `Box<dyn Error>` or `anyhow::Error`.
impl core::error::Error for LayoutError {}

/// Fixed capacities and the derived region layout. Fields are private so the
/// power-of-two invariant on `edge_capacities` (load-bearing invariant 3) holds after construction.
#[derive(Clone, Debug)]
pub struct ArenaLayout {
    max_frames: u32,
    max_edges: u32,
    max_participants: u32,
    edge_capacities: Vec<u32>,
    computed: Computed,
}

// The regions in header order, `N_REGIONS` of them.
const R_HEADER: usize = 0;
const R_FRAME_TABLE: usize = 1;
const R_FRAME_HASH: usize = 2;
const R_TOPO: usize = 3;
const R_CLAIM: usize = 4;
const R_PARTICIPANT: usize = 5;
const R_EDGE: usize = 6;
const R_STAMP: usize = 7;
const R_POSE: usize = 8;
/// Per-edge diagnostic counters (`docs/PHASE5.md` §5.2). v3.
const R_EDGE_COUNTERS: usize = 9;
/// Per-participant diagnostic counters (§5.2). v3.
const R_PARTICIPANT_COUNTERS: usize = 10;
/// Number of regions in header order.
const N_REGIONS: usize = 11;

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
/// `claiming` (`docs/PHASE2.md` §1 A8, §11.3) lets a waiter find the crashed interner behind a claimed hash with no published id.
pub const FRAME_HASH_STRIDE: usize = 8 + 4 + 4;

#[derive(Clone, Copy, Debug)]
struct Computed {
    regions: [Region; N_REGIONS],
    topo_stride: usize,
    slots: usize,
}

/// Derive the region layout from the fixed capacities; called once, from [`ArenaLayout::new`].
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

    // 12 B per frame (`docs/PHASE1.md` §4.3), atomic (`docs/PHASE2.md` §1 A1).
    let topo_stride = align64(mf * 12);
    // Sizes in header order; each aligned so the running offset stays 64-aligned.
    let sizes = [
        320usize,                                       // header (v3: 256 -> 320)
        align64(mf * 64),                               // frame table (64 B / frame)
        align64(next_pow2(2 * mf) * FRAME_HASH_STRIDE), // frame hash (A8)
        TOPO_BLOCKS * topo_stride,                      // topology blocks (A1: four)
        align64(me * 64),                               // claim table (64 B / edge)
        align64(mp * 128),                              // participant table (128 B / slot)
        align64(me * 128),                              // edge table (128 B / edge)
        align64(slots * 8),                             // stamp arena (i64 / slot)
        align64(slots * 64),                            // pose arena (PoseSlot / slot)
        align64(me * 128),                              // edge counters (v3, §5.2)
        align64(mp * 128),                              // participant counters (v3)
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
    /// Validate that each per-edge capacity is `0` (static, no ring) or a power of two and
    /// that exactly `max_edges` capacities were supplied.
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
        // Reject rather than truncate: the last region's end bounds every offset.
        let last = computed.regions[N_REGIONS - 1];
        let total_size = last.offset + last.size;
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

    /// The smallest valid layout: one frame, no edges, no sample slots.
    ///
    /// Infallible, so callable from a `static` initializer. Its consumer is `tf_tree`'s fork
    /// poison arena, which answers every query "not here".
    #[must_use]
    pub fn minimal() -> ArenaLayout {
        let max_participants = DEFAULT_MAX_PARTICIPANTS;
        ArenaLayout {
            max_frames: 1,
            max_edges: 0,
            max_participants,
            edge_capacities: Vec::new(),
            computed: compute(1, 0, max_participants, &[]),
        }
    }

    /// The layout implied by totals alone, for validating a header: `compute` uses only the sum
    /// of the per-edge capacities, so `stamp_slots` reconstructs every region offset.
    ///
    /// Carries an empty `edge_capacities`; compare geometry only.
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
        let last = computed.regions[N_REGIONS - 1];
        let total_size = last.offset + last.size;
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

    /// The header region (offset 0, size 320 since FORMAT_VERSION 3).
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

    /// The topology region — all [`TOPO_BLOCKS`] blocks, contiguous.
    pub fn topo_blocks(&self) -> Region {
        self.computed.regions[R_TOPO]
    }

    /// Byte stride between consecutive topology blocks.
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

    /// Total stamp slots across all edges; fits `u32` ([`ArenaLayout::new`] rejects more).
    pub fn stamp_slots(&self) -> u32 {
        self.computed.slots as u32
    }

    /// Total pose slots across all edges (equal to [`Self::stamp_slots`]).
    pub fn pose_slots(&self) -> u32 {
        self.computed.slots as u32
    }

    /// The per-edge counter region (`docs/PHASE5.md` §5.2), present with or without `counters` (D34).
    pub fn edge_counters(&self) -> Region {
        self.computed.regions[R_EDGE_COUNTERS]
    }

    /// The per-participant counter region (§5.2). Same contract.
    pub fn participant_counters(&self) -> Region {
        self.computed.regions[R_PARTICIPANT_COUNTERS]
    }

    /// Total arena size in bytes, 64-byte aligned. Guaranteed `<= u32::MAX`.
    pub fn total_size(&self) -> usize {
        // The last region, not the pose arena: v3 appended two counter regions after it.
        let last = self.computed.regions[N_REGIONS - 1];
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

/// Compile-time layout hash of the arena-level structural constants: [`ArenaHeader`] size and
/// alignment plus each region's stride, folded by FNV-1a into [`ArenaHeader::layout_hash`].
/// Attach rejects a mismatch. `tf_tree_core` folds its own record sizes into the full hash.
pub const fn layout_hash() -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    h = fnv1a_u32(h, core::mem::size_of::<ArenaHeader>() as u32);
    h = fnv1a_u32(h, core::mem::align_of::<ArenaHeader>() as u32);
    // Strides in header order; `N_REGIONS + 1` because `R_TOPO` folds two values,
    // so a forgotten stride is a compile error (0032 part 3).
    let strides: [u32; N_REGIONS + 1] = [
        320,
        64,
        FRAME_HASH_STRIDE as u32,
        12,
        TOPO_BLOCKS as u32,
        64,
        128,
        128,
        8,
        64,
        128, // edge counters (v3)
        128, // participant counters (v3)
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

    /// [`ArenaLayout::minimal`] must equal `new(1, 0, [])`: it exists to be infallible, not different.
    #[test]
    fn minimal_matches_the_fallible_constructor() {
        let a = ArenaLayout::minimal();
        let b = ArenaLayout::new(1, 0, Vec::new()).expect("1 frame, 0 edges is valid");
        assert_eq!(a.total_size(), b.total_size());
        assert_eq!(a.max_frames, b.max_frames);
        assert_eq!(a.max_edges, b.max_edges);
        assert_eq!(a.max_participants, b.max_participants);
    }

    /// The poison arena's participant table must be indexable by any slot a real arena hands out.
    #[test]
    fn minimal_has_room_for_every_participant_slot() {
        assert_eq!(
            ArenaLayout::minimal().max_participants,
            DEFAULT_MAX_PARTICIPANTS
        );
    }

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
            l.edge_counters(),
            l.participant_counters(),
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
        // One edge with a 2^27-slot ring: pose arena alone is 8 GiB; must be a hard error.
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
                size: 320 // v3: was 256
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
        assert_eq!(l.edge_counters().size, 128_000); // v3: 1000 * 128
        assert_eq!(l.participant_counters().size, 8_192); // v3: 64 * 128

        // Pose arena is ~260 MB.
        assert!((260_000_000..=263_000_000).contains(&l.pose_arena().size));
        assert_eq!(l.total_size(), 295_393_600); // v3: +64 header, +2 counter regions

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
        assert_eq!(l.edge_counters().size, 512); // v3: 4 * 128
        assert_eq!(l.participant_counters().size, 8_192); // v3: 64 * 128
        assert_eq!(l.total_size(), 25_344); // v3: +64 header, +8_704 counters

        let regions = all_regions(&l);
        for w in regions.windows(2) {
            assert_eq!(w[0].offset % 64, 0);
            assert_eq!(w[0].size % 64, 0);
            assert_eq!(w[0].offset + w[0].size, w[1].offset);
        }
    }

    #[test]
    fn layout_hash_is_deterministic_and_stable() {
        // Snapshot: any change to the header layout or region strides changes this value.
        assert_eq!(layout_hash(), layout_hash());
        assert_ne!(layout_hash(), 0);
        assert_eq!(layout_hash(), 0x3D10_4195);
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

        // Fixed seed. Case count is cut under Miri, where 10_000 cases take tens of minutes.
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
                prop_assert_eq!(regions[0].size, 320); // v3 header
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

    /// `docs/decisions/0059` step 1(b): every variant renders by decision 2's rules, structurally.
    ///
    /// **Mutant (M3):** drop `{total_size}` from `ArenaTooLarge`'s arm; this test fails.
    #[test]
    fn every_layout_error_variant_renders_by_0059s_rules() {
        use crate::render_test::{assert_structure, variant_name};
        use alloc::format;
        use alloc::string::ToString;

        fn index(e: &LayoutError) -> usize {
            match e {
                LayoutError::CapacityNotPowerOfTwo { .. } => 0,
                LayoutError::EdgeCountMismatch { .. } => 1,
                LayoutError::ArenaTooLarge { .. } => 2,
            }
        }

        let all = vec![
            (
                LayoutError::CapacityNotPowerOfTwo {
                    edge: usize::MAX,
                    capacity: u32::MAX,
                },
                vec![usize::MAX.to_string(), u32::MAX.to_string()],
            ),
            (
                LayoutError::EdgeCountMismatch {
                    max_edges: u32::MAX,
                    got: usize::MAX,
                },
                vec![u32::MAX.to_string(), usize::MAX.to_string()],
            ),
            (
                LayoutError::ArenaTooLarge {
                    total_size: u64::MAX,
                },
                vec![u64::MAX.to_string()],
            ),
        ];
        let mut hit: Vec<usize> = all.iter().map(|(e, _)| index(e)).collect();
        hit.sort_unstable();
        hit.dedup();
        assert_eq!(hit, vec![0, 1, 2], "one value of every LayoutError variant");

        for (e, numbers) in &all {
            let debug = format!("{e:?}");
            assert_structure(&format!("{e}"), &debug, variant_name(&debug), numbers);
        }
    }
}
