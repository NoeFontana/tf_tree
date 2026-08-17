//! What the declared capacities cost, **in bytes**.
//!
//! # The gap this exists to make visible
//!
//! [`tf_tree::Capacity`] is denominated in *slots*; tf2 evicts by *time*. A
//! publisher that writes `Capacity::history(1000.0, 10.0)` is asking for ten
//! seconds of a 1 kHz stream — 10 000 slots — and gets **16 384**, because
//! `mask == capacity - 1` is the ring's hot index and the mask is only a mask
//! when the capacity is a power of two. That rounding cannot be removed. What
//! it *can* be is visible: at 10 Hz the same declaration retains 27 minutes of
//! history, and neither the slot count nor the retained window is a number an
//! operator should have to derive from `crates/tf_tree_arena/src/layout.rs`.
//!
//! **This is operational hygiene, not a measured loss.** Since
//! [`0021`](../../../docs/decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md) the
//! heap arena reaches `calloc`, so over-declared slots are demand-faulted pages
//! that are never touched and never become resident. What an over-declaration
//! still costs is *reservation*: address space, the `.tft` file on disk, the
//! bytes copied by a segment transfer, and the headroom a machine under strict
//! overcommit has to have. Those are real and they are what the numbers below
//! describe — a resident-memory claim would need a Pss measurement, which this
//! module does not make and does not imply.
//!
//! # The sizing formula
//!
//! ```text
//! total = 16 704 B fixed
//!       +    320 B per edge slot
//!       +  144-176 B per frame slot
//!       +     72 B per sample slot
//! ```
//!
//! Every term is a sum of `compute()`'s regions in
//! `crates/tf_tree_arena/src/layout.rs`, and
//! `tests::the_formula_is_the_layouts_own_arithmetic` derives all four from
//! `ArenaLayout` rather than trusting this comment:
//!
//! | term | regions |
//! |---|---|
//! | fixed | header (320 B) + participant table (64 x 128 B) + participant counters (64 x 128 B) |
//! | per edge | claim record 64 + edge record 128 + edge counters 128 |
//! | per frame | frame record 64 + 4 topology blocks x 12 + intern slots |
//! | per slot | stamp 8 (`i64`) + pose 64 (`PoseSlot`, one cache line) |
//!
//! The per-frame term is a **range** because the intern table is
//! `next_pow2(2 * max_frames)` slots of 16 B: exactly 32 B/frame when the frame
//! count is a power of two, and up to 64 B/frame just above one. It is the only
//! term that is not exact, which is why it is written as a range everywhere it
//! is printed instead of being averaged into a single misleading number.
//!
//! All four are *rate* terms, and on a small tree there is one more thing in the
//! total: every region is `align64`-padded, so the frame table, the intern table
//! and each of the four topology blocks round up to 64 B. That is under 384 B in
//! total, fixed rather than per frame — invisible at 64 frames, and the reason a
//! 1-frame arena measures 384 B/frame against a stated 144. The formula is for
//! sizing a deployment, not for auditing a two-frame unit test, and
//! `tests::the_per_frame_term_stays_inside_its_stated_range` pins exactly that
//! claim rather than the stronger one it would be nice to make.
//!
//! # What is derivable about the rounding, and what is not
//!
//! The pre-rounding request is **not stored**: `EdgeConfig` carries the
//! post-`next_pow2` capacity and nothing remembers what was asked for. So the
//! honest statement is a bracket rather than a figure. A ring of capacity `C`
//! (a power of two, `C >= 2`) was declared with some `n` in `[C/2 + 1, C]`, so
//! the rounding wasted `C - n`, which is at most `C/2 - 1` slots. That upper
//! bound is what [`Rings::rounding_slack_slots`] reports, and it is reported as
//! "at most", because a publisher that asked for exactly 16 384 wasted nothing
//! and this module cannot tell the two apart.

use core::fmt::Write as _;

/// Bytes reserved per sample slot: 8 B of stamp arena + 64 B of pose arena.
pub const SLOT_BYTES: u64 = 72;

/// Bytes reserved per edge slot: 64 B claim + 128 B edge record + 128 B counters.
pub const EDGE_BYTES: u64 = 320;

/// Bytes reserved per frame slot when the frame capacity is a power of two.
///
/// 64 B frame record + 4 x 12 B topology + 32 B of intern table.
pub const FRAME_BYTES_MIN: u64 = 144;

/// Upper bound on bytes reserved per frame slot.
///
/// The intern table is `next_pow2(2 * max_frames)` x 16 B, which is worst just
/// above a power of two: 64 B/frame instead of 32.
pub const FRAME_BYTES_MAX: u64 = 176;

/// Bytes that do not scale with any capacity: header + participant table +
/// participant counters.
pub const FIXED_BYTES: u64 = 16_704;

/// The formula, in one line, for the surfaces that print a sizing number.
///
/// Printed next to the numbers rather than left in this module's rustdoc,
/// because the operator reading `doctor` output at 3 a.m. is not reading
/// rustdoc, and a byte count with no unit price is a number they cannot act on.
pub const FORMULA: &str =
    "arena = 16704 B fixed + 320 B/edge + 144-176 B/frame + 72 B/slot (docs/RUNBOOK.md)";

/// What every dynamic ring in a tree reserves, and how much of it holds data.
///
/// Built from the two fields both inspection surfaces already carry — the ring
/// capacity and the ring occupancy — so `doctor` and `top` cannot disagree
/// about the arithmetic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rings {
    /// Dynamic edges with a ring (capacity != 0).
    pub edges: usize,
    /// Slots reserved across those rings.
    pub reserved_slots: u64,
    /// Slots currently holding a sample (`min(head, capacity)`, summed).
    pub used_slots: u64,
    /// Upper bound on the slots that exist only because of `next_pow2`.
    ///
    /// `sum(C/2 - 1)`. An **upper** bound, not a figure: see the module docs —
    /// the declared count is not stored, so a ring that asked for exactly its
    /// capacity is indistinguishable here from one that asked for half plus one.
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
    /// One line, because both callers print it into a header an operator scans
    /// rather than reads. It states the rounding bound as "at most" for the
    /// reason [`Self::rounding_slack_slots`] gives.
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

/// Human byte count: B under 1 KiB, then KiB, then MiB.
///
/// Deliberately not a `humansize`-style dependency for one call site, and
/// deliberately binary units — every number it formats came from a layout whose
/// regions are 64-byte aligned and page-backed, so a decimal "MB" would be the
/// wrong unit as well as an extra crate.
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
    // The crate denies these; a test that cannot `expect` a layout it just
    // constructed would have to carry error handling that says nothing.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tf_tree_arena::ArenaLayout;

    fn total(frames: u32, edges: u32, slots: u32) -> u64 {
        ArenaLayout::from_totals(frames, edges, slots)
            .expect("layout")
            .total_size() as u64
    }

    /// The four constants are the layout's own arithmetic, read out of
    /// `ArenaLayout` by differencing rather than transcribed from it.
    ///
    /// This is the check that stops the four numbers this module prints from
    /// drifting when `docs/PHASE5.md` §1 next changes a region — the header grew
    /// 256 -> 320 B and gained two counter regions in v3, and a hand-copied
    /// constant would have survived that silently.
    ///
    /// Mutant (applied, confirmed fatal): `SLOT_BYTES = 64` — the stamp arena is
    /// the 8 that is easiest to forget — fails the first assertion with
    /// `73728 != 65536`.
    #[test]
    fn the_formula_is_the_layouts_own_arithmetic() {
        // Per slot: 1024 more slots, everything else fixed.
        assert_eq!(
            total(64, 64, 1024 + 1024) - total(64, 64, 1024),
            SLOT_BYTES * 1024
        );
        // Per edge: 64 more edge slots. A multiple of 64 keeps every region's
        // `align64` a no-op, so the difference is the record sizes and nothing
        // else.
        assert_eq!(total(64, 128, 1024) - total(64, 64, 1024), EDGE_BYTES * 64);
        // Per frame, at powers of two on both sides — where the intern table is
        // exactly 2x the frame count and the term is exact.
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

    /// The per-frame range holds across frame counts that are *not* powers of
    /// two, which is the only reason it is written as a range.
    ///
    /// The worst *rate* is just above a power of two, where `next_pow2(2f)`
    /// nearly doubles: 65 frames intern into 256 slots, 64 B/frame of table
    /// against 32. The `PADDING` term is the module docs' claim about `align64`
    /// — under 384 B, fixed — and asserting the bound *with* it rather than
    /// widening `FRAME_BYTES_MAX` to swallow it keeps the printed range honest:
    /// 176 is what a frame costs, not a number chosen so a 1-frame arena fits.
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
        // And the rate is tight rather than merely safe: some frame count
        // reaches the top of the range, or the upper bound is a number that
        // describes no layout. 65 is the shape that does it.
        let per_frame = |f: u32| (total(f, 0, 0) - FIXED_BYTES) as f64 / f64::from(f);
        assert!(
            per_frame(65) > FRAME_BYTES_MAX as f64 - 8.0,
            "65 frames cost {:.1} B/frame; the stated {FRAME_BYTES_MAX} is not the worst case \
             it claims to be",
            per_frame(65)
        );
        // ...and the bottom of the range is exact at a power of two.
        assert_eq!(
            total(1024, 0, 0) - FIXED_BYTES,
            FRAME_BYTES_MIN * 1024,
            "a power-of-two frame count must cost exactly the low end of the range"
        );
    }

    /// A `Capacity::history(1000.0, 10.0)` ring is the module docs' example, and
    /// the bracket it reports is the honest one: 10 000 declared, 16 384
    /// reserved, and this module can only say the rounding is *at most* 8191
    /// slots because the 10 000 was never stored.
    #[test]
    fn the_rounding_bound_brackets_the_declaration_it_cannot_see() {
        let r = Rings::from_edges([(16_384u32, 10_000u64)]);
        assert_eq!(r.reserved_slots, 16_384);
        assert_eq!(r.used_slots, 10_000);
        assert_eq!(r.rounding_slack_slots, 8_191);
        // The real waste, which this module deliberately does not claim to know.
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

    /// Occupancy is clamped to the capacity, because `head` is monotone and
    /// exceeds it on every ring that has wrapped — the overwhelming majority of
    /// live rings. An unclamped sum would report a tree using 40x the bytes it
    /// reserved.
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
