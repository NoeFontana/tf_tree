# 0032: the region table was not part of the purchase

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`docs/PHASE5.md` §1.2 said Phase 6's regions are declared in the header with
offset `0`, so Phase 6 fills them "**without another layout change**, because the
region table already accounts for them". **The second half is false, and the first
half is true.** `FORMAT_VERSION = 3` bought the **header**, not the region table.

## What was measured

**The header fields exist** (`crates/tf_tree_arena/src/header.rs`):
`_reserved_covariance`, `spline_region_off: u32`, `spline_degree: u8`. **The region
table does not:** `crates/tf_tree_arena/src/layout.rs` declares `R_HEADER` ...
`R_PARTICIPANT_COUNTERS` and an `N_REGIONS` counting exactly those. A spline region
is **a twelfth region** and changes `ArenaLayout::total_size()`;
`spline_region_off` is a place to *write* an offset, not a reservation.

**A forgotten stride is silent to the compiler and to the layout-hash test.**
`layout_hash` folds a stride array in `layout.rs` that was hardcoded separately
from `N_REGIONS` (twelve numbers, eleven regions: `R_TOPO` folds two, so the
relation is `N_REGIONS + 1`). Adding a twelfth region with the stride forgotten
leaves `layout_hash` unchanged, but `validate_arena_header` requires
`implied.total_size() as u64 == h.arena_size`, so two builds of the same
`FORMAT_VERSION` and `layout_hash` refuse each other with
`ShmError::HeaderInconsistent`, whose `docs/RUNBOOK.md` entry is a corruption
diagnosis for a version-skew fact (D22 from the other side).
`crates/tf_tree/tests/frozen.rs::the_committed_sensor_domain_fixture_reads_and_is_still_tag_one`
goes red either way (`required-features = ["shm"]`, so `just shm-check` runs it and
`just test` does not); with the stride updated it reports a true
`Frozen(LayoutMismatch)`.

## Decision

**Taken.** Three parts.

1. **Retract `PHASE5.md` §1.2's second clause.** The header fields are reserved;
   the region table is not. Phase 6's spline region requires a twelfth region, a
   twelfth stride and a **new `FORMAT_VERSION`**. The header fields are sound.

2. **Name that break as the project's one scheduled break, and open a ledger for
   it** in `docs/PROJECT.md` §5.1, beside the decision log D22 belongs to. Its
   entries, **none authorised by this record**, are Phase 6's spline region and an
   `EdgeMeta` provenance byte no record has argued either way.
   [`0009`](./0009-descoping-phase-6.md): removing a reserved field after Phase 6
   populates it is `FORMAT_VERSION = 4`; the ledger schedules that cost.

3. **Couple the stride array to the region count:**
   `let strides: [u32; N_REGIONS + 1]`; the `+ 1` is `R_TOPO`'s second stride.
   **Cardinality only:** it cannot see a wrong index or value, and a second
   two-stride region makes it `+ 2`. Worth doing regardless; not a proof.

## Consequences

- `CLAUDE.md` says the header fields are reserved and the region table is not.
- Part 3 does not move `layout_hash`; no existing `.tft` is affected.

## Implementation plan

1. **Retract the clause.** `docs/PHASE5.md` §1.2, §0.0's §1 row and title, §0's
   in-scope row, §1.1's "Do it once, now, with room reserved", §13's boxes;
   `docs/PROJECT.md` §4's Phase 5 paragraph; this record's row in
   [`README.md`](./README.md). Verified by

   ```sh
   grep -rnE 'regions? (are |is )?reserved|reserved regions|room reserved|Phase 6 regions' \
     docs/ CLAUDE.md README.md CHANGELOG.md
   ```

   returning only corrected wording or a quoted retraction.
2. **Open the ledger** (`docs/PROJECT.md` §5.1). **Done.**
3. **Couple the stride array** in `crates/tf_tree_arena/src/layout.rs`:
   `let strides: [u32; N_REGIONS + 1] = [`, the comment naming `R_TOPO` as the
   `+ 1` and what a cardinality check cannot see. Verified **green** by
   `cargo nextest run -p tf_tree_arena --features shm` on the unchanged literal,
   and **red** by a staged twelfth region failing with `error[E0308]`.
4. **Fix the evidence's own comment:** `crates/tf_tree/tests/frozen.rs`'s doc on
   the fixture test named `just test`, which cannot reach a
   `required-features = ["shm"]` target.
5. **Flip to `ready`**, with the `CHANGELOG.md` entry `CONTRIBUTING.md` requires.

Steps 1-4 travel together; step 3 must land before any other item adds an arena
region.

## Open questions

1. **Does the twelfth region produce `HeaderInconsistent`?** Yes; nothing refuses
   earlier. The red arm already exists in `crates/tf_tree/tests/frozen.rs`.
2. **Is a spline region certain to be a region?** Deferred: `0009` says it "needs
   a wider bracket read, not a new region shape", so the break is contingent; the
   clause's falsity is not.
3. **Should the ledger live here or in `PROJECT.md`?** `PROJECT.md` §5.1.
4. **Does the C ABI or Python surface `layout_hash`?** No. A break moves the
   literal `0x3D10_4195`, failing `layout::tests::layout_hash_is_deterministic_and_stable`
   and `just bench-check` (`provenance.layout_hash` in
   `crates/tf_tree_bench/baseline/` is in `baseline::PORTABLE_FACTS`) until the
   baseline is regenerated.
