//! What the declared capacities cost, **in bytes**.
//!
//! [`tf_tree::Capacity`] is denominated in slots and rounded up to a power of
//! two (`mask == capacity - 1`), so `Capacity::history(1000.0, 10.0)` asks for
//! 10 000 slots and gets 16 384. This module makes that visible.
//!
//! Operational hygiene, not a measured loss: since
//! [`0021`](../../../docs/decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md)
//! over-declared slots are never resident. What they cost is reservation
//! (address space, `.tft` size, segment transfer, strict overcommit); no Pss
//! claim is made.
//!
//! ```text
//! total = 16 704 B fixed
//!       +    320 B per edge slot
//!       +  144-176 B per frame slot
//!       +     72 B per sample slot
//! ```
//!
//! `tests::the_formula_is_the_layouts_own_arithmetic` derives all four from
//! `ArenaLayout` in `crates/tf_tree_arena/src/layout.rs`:
//!
//! | term | regions |
//! |---|---|
//! | fixed | header (320 B) + participant table (64 x 128 B) + participant counters (64 x 128 B) |
//! | per edge | claim record 64 + edge record 128 + edge counters 128 |
//! | per frame | frame record 64 + 4 topology blocks x 12 + intern slots |
//! | per slot | stamp 8 (`i64`) + pose 64 (`PoseSlot`, one cache line) |
//!
//! The per-frame term is a range: the intern table is `next_pow2(2 * max_frames)`
//! slots of 16 B, so 32 B/frame at a power of two and up to 64 just above one.
//! `align64` padding adds under 384 B fixed, which is why a 1-frame arena reads
//! 384 B/frame; `tests::the_per_frame_term_stays_inside_its_stated_range` pins it.
//!
//! The pre-rounding request is not stored, so the rounding is a bracket: a ring
//! of capacity `C >= 2` wasted at most `C/2 - 1` slots, reported as "at most" by
//! [`Rings::rounding_slack_slots`].

use core::fmt::Write as _;

/// Bytes reserved per sample slot: 8 B of stamp arena + 64 B of pose arena.
pub const SLOT_BYTES: u64 = 72;

/// Bytes reserved per edge slot: 64 B claim + 128 B edge record + 128 B counters.
pub const EDGE_BYTES: u64 = 320;

/// Bytes per frame slot at a power-of-two capacity (64 record + 4 x 12 topology + 32 intern).
pub const FRAME_BYTES_MIN: u64 = 144;

/// Upper bound on bytes per frame slot (intern table at 64 B/frame just above a power of two).
pub const FRAME_BYTES_MAX: u64 = 176;

/// Bytes that scale with no capacity: header + participant table + counters.
pub const FIXED_BYTES: u64 = 16_704;

/// The formula in one line, printed next to every sizing number.
pub const FORMULA: &str =
    "arena = 16704 B fixed + 320 B/edge + 144-176 B/frame + 72 B/slot (docs/RUNBOOK.md)";

/// What every dynamic ring in a tree reserves, and how much of it holds data.
///
/// Built from ring capacity and occupancy so `doctor` and `top` share the arithmetic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rings {
    /// Dynamic edges with a ring (capacity != 0).
    pub edges: usize,
    /// Slots reserved across those rings.
    pub reserved_slots: u64,
    /// Slots currently holding a sample (`min(head, capacity)`, summed).
    pub used_slots: u64,
    /// Upper bound on the slots that exist only because of `next_pow2`: `sum(C/2 - 1)`.
    ///
    /// A bound, not a figure: the declared count is not stored.
    pub rounding_slack_slots: u64,
}

impl Rings {
    /// Sum `(capacity, occupancy)` pairs. Static edges (`capacity == 0`) are
    /// skipped: they reserve no ring and would dilute every percentage here.
    pub fn from_edges(edges: impl IntoIterator<Item = (u32, u64)>) -> Rings {
        let mut r = Rings::default();
        for (capacity, occupancy) in edges {
            if capacity == 0 {
                continue;
            }
            r.edges += 1;
            r.reserved_slots += u64::from(capacity);
            r.used_slots += occupancy.min(u64::from(capacity));
            // `C/2 - 1` for C >= 2; a capacity of 1 rounds from nothing.
            r.rounding_slack_slots += u64::from(capacity / 2).saturating_sub(1);
        }
        r
    }

    /// Bytes reserved by the rings.
    #[must_use]
    pub fn reserved_bytes(&self) -> u64 {
        self.reserved_slots * SLOT_BYTES
    }

    /// Bytes of ring that currently hold a sample.
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.used_slots * SLOT_BYTES
    }

    /// Bytes reserved and not holding a sample.
    #[must_use]
    pub fn unused_bytes(&self) -> u64 {
        self.reserved_bytes() - self.used_bytes()
    }

    /// Upper bound on the bytes that exist only because of `next_pow2`.
    #[must_use]
    pub fn rounding_slack_bytes(&self) -> u64 {
        self.rounding_slack_slots * SLOT_BYTES
    }

    /// The operator's line: declared against used, in slots and in bytes.
    ///
    /// The rounding bound reads "at most" (see [`Self::rounding_slack_slots`]).
    #[must_use]
    pub fn line(&self) -> String {
        if self.edges == 0 {
            return "rings: none declared (no dynamic edge in this tree reserves one)".to_owned();
        }
        let mut s = String::new();
        let pct = if self.reserved_slots == 0 {
            0.0
        } else {
            self.used_slots as f64 / self.reserved_slots as f64 * 100.0
        };
        let _ = write!(
            s,
            "rings: {} slots declared = {} over {} edge(s); {} used = {} ({pct:.0}%); \
             at most {} slots = {} is next_pow2 rounding",
            self.reserved_slots,
            bytes(self.reserved_bytes()),
            self.edges,
            self.used_slots,
            bytes(self.used_bytes()),
            self.rounding_slack_slots,
            bytes(self.rounding_slack_bytes()),
        );
        s
    }
}

/// Human byte count in binary units: B, then KiB, then MiB.
#[must_use]
pub fn bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{:.2} MiB", n as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    // The crate denies these; tests `expect` layouts they just built.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tf_tree_arena::ArenaLayout;

    fn total(frames: u32, edges: u32, slots: u32) -> u64 {
        ArenaLayout::from_totals(frames, edges, slots)
            .expect("layout")
            .total_size() as u64
    }

    /// The four constants are `ArenaLayout`'s own arithmetic, read by differencing.
    #[test]
    fn the_formula_is_the_layouts_own_arithmetic() {
        // Per slot: 1024 more slots, everything else fixed.
        assert_eq!(
            total(64, 64, 1024 + 1024) - total(64, 64, 1024),
            SLOT_BYTES * 1024
        );
        // Per edge: a multiple of 64 keeps every `align64` a no-op.
        assert_eq!(total(64, 128, 1024) - total(64, 64, 1024), EDGE_BYTES * 64);
        // Per frame, at powers of two, where the term is exact.
        assert_eq!(
            total(128, 64, 1024) - total(64, 64, 1024),
            FRAME_BYTES_MIN * 64
        );
        // Fixed: what is left when the three scaling terms are removed.
        assert_eq!(
            total(64, 64, 32 * 1024)
                - FRAME_BYTES_MIN * 64
                - EDGE_BYTES * 64
                - SLOT_BYTES * 32 * 1024,
            FIXED_BYTES
        );
    }

    /// The per-frame range holds at non-power-of-two frame counts; `PADDING` is
    /// the fixed `align64` term, kept out of `FRAME_BYTES_MAX`.
    #[test]
    fn the_per_frame_term_stays_inside_its_stated_range() {
        const PADDING: u64 = 384;
        for frames in 1u32..=1024 {
            let cost = total(frames, 0, 0) - FIXED_BYTES;
            assert!(
                cost <= FRAME_BYTES_MAX * u64::from(frames) + PADDING,
                "{frames} frames cost {cost} B, above {FRAME_BYTES_MAX} B/frame + {PADDING} B \
                 of region padding"
            );
        }
        // The upper bound is tight: 65 frames reach the top of the range.
        let per_frame = |f: u32| (total(f, 0, 0) - FIXED_BYTES) as f64 / f64::from(f);
        assert!(
            per_frame(65) > FRAME_BYTES_MAX as f64 - 8.0,
            "65 frames cost {:.1} B/frame; the stated {FRAME_BYTES_MAX} is not the worst case \
             it claims to be",
            per_frame(65)
        );
        // The bottom is exact at a power of two.
        assert_eq!(
            total(1024, 0, 0) - FIXED_BYTES,
            FRAME_BYTES_MIN * 1024,
            "a power-of-two frame count must cost exactly the low end of the range"
        );
    }

    /// `Capacity::history(1000.0, 10.0)`: 10 000 declared, 16 384 reserved, rounding at most 8191.
    #[test]
    fn the_rounding_bound_brackets_the_declaration_it_cannot_see() {
        let r = Rings::from_edges([(16_384u32, 10_000u64)]);
        assert_eq!(r.reserved_slots, 16_384);
        assert_eq!(r.used_slots, 10_000);
        assert_eq!(r.rounding_slack_slots, 8_191);
        let actual_rounding = 16_384 - 10_000;
        assert!(
            actual_rounding <= r.rounding_slack_slots,
            "the reported bound must contain the true rounding"
        );
        assert_eq!(r.reserved_bytes(), 16_384 * 72);
        let line = r.line();
        assert!(line.contains("at most"), "{line}");
        assert!(line.contains("16384 slots declared"), "{line}");
        assert!(
            line.contains("MiB"),
            "1 179 648 B should read in MiB: {line}"
        );
    }

    /// Occupancy is clamped to capacity: `head` is monotone and exceeds it on a wrapped ring.
    #[test]
    fn a_wrapped_ring_does_not_report_more_used_than_it_reserves() {
        let r = Rings::from_edges([(1024u32, 4_000_000u64)]);
        assert_eq!(r.used_slots, 1024);
        assert_eq!(r.unused_bytes(), 0);
    }

    /// Static edges reserve no ring, so they are not in the denominator.
    #[test]
    fn static_edges_are_not_rings() {
        let r = Rings::from_edges([(0u32, 0u64), (256, 128)]);
        assert_eq!(r.edges, 1);
        assert_eq!(r.reserved_slots, 256);
    }
}
