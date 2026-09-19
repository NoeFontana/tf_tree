# 0032: the region table was not part of the purchase

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`docs/PHASE5.md` §1.2 said Phase 6's regions are declared in the header with
offset `0`, so Phase 6 fills them "**without another layout change**, because the
region table already accounts for them", and `CLAUDE.md` repeated it as the reason
to refuse spending a byte now. **The second half is false, and the first half is
true.** `FORMAT_VERSION = 3` bought the **header**, not the region table.

## What was measured

**The header fields exist** (`crates/tf_tree_arena/src/header.rs`, `offset_of!`
asserted): `_reserved_covariance`, `spline_region_off: u32`, `spline_degree: u8`.
**The region table does not:** `crates/tf_tree_arena/src/layout.rs` declares
`R_HEADER` ... `R_PARTICIPANT_COUNTERS` and an `N_REGIONS` counting exactly those.
A spline region is **a twelfth region** and changes `ArenaLayout::total_size()`
for the same geometry; `spline_region_off` is a place to *write* an offset, not a
reservation of the bytes it points at.

**Staged twelfth region** (a `git archive HEAD` copy; stride forgotten vs updated,
against an unmodified control). Adding a region means `N_REGIONS`, the constant,
the `sizes` entry, an accessor, the `all_regions` test helper, the stride array
and the two `total_size` fixtures. Only the stride array is silent: with it
forgotten and the fixtures updated, `cargo nextest run -p tf_tree_arena --features
shm` is green, `layout::tests::layout_hash_is_deterministic_and_stable` included
(the `0x3D10_4195` literal holds, since the hash folds the stride array, which did
not move).

**What notices the forgotten stride:**
`crates/tf_tree/tests/frozen.rs::the_committed_sensor_domain_fixture_reads_and_is_still_tag_one`
reads `testdata/frozen/sensor_domain.tft`, written by an eleven-region build:

| variant | what the committed fixture reports |
|---|---|
| unmodified (control) | opens; the whole `frozen` target passes |
| twelfth region, **stride updated** | `Frozen(LayoutMismatch { found, expected })`, a **true** statement about version skew |
| twelfth region, **stride forgotten** | `Frozen(Arena(HeaderInconsistent))`, the corruption diagnosis `docs/RUNBOOK.md` acts on by recreating the arena |

The same test goes red either way; only the operator's diagnosis changes (D22
from the other side). It has `required-features = ["shm"]`, so `just shm-check`
runs it and `just test` does not. `SizeMismatch` cannot pre-empt:
`validate_arena_header` refuses magic -> version -> layout hash -> declared size
-> implied geometry.

## The part that is worse than a second break

`layout_hash` folds a stride array that was **separately hardcoded**:

```rust
let strides: [u32; 12] = [
    320, 64, FRAME_HASH_STRIDE as u32, 12, TOPO_BLOCKS as u32,
    64, 128, 128, 8, 64,
    128, // edge counters (v3)
    128, // participant counters (v3)
];
```

**Nothing coupled it to `N_REGIONS`** (twelve numbers, eleven regions: `R_TOPO`
folds two, so the relation is `N_REGIONS + 1`). Updated, `layout_hash` changes and
a v3 consumer meets a true `ShmError::LayoutMismatch`. Missed, `layout_hash` is
unchanged but `validate_arena_header` requires `implied.total_size() as u64 ==
h.arena_size && ...`, so two builds of the same `FORMAT_VERSION` and `layout_hash`
refuse each other with `ShmError::HeaderInconsistent`, whose `docs/RUNBOOK.md`
entry says the header "does not match the geometry its own capacities imply": a
corruption diagnosis for a version-skew fact. D22 is "a disabled feature never
forks the layout hash"; this is an *enabled* region forking the geometry without
forking the hash.

## Decision

**Taken.** Three parts.

1. **Retract `PHASE5.md` §1.2's second clause.** The header fields are reserved;
   the region table is not. Phase 6's spline region requires a twelfth region, a
   twelfth stride and a **new `FORMAT_VERSION`**, the second break §1 was written
   to avoid. Only the second clause is retracted; the header fields are sound.

2. **Name that break as the project's one scheduled break, and open a ledger for
   it** in `docs/PROJECT.md` §5.1, beside the decision log D22 belongs to, so
   "wait for the next one" becomes a schedule. Its entries, **none authorised by
   this record**, are Phase 6's spline region and an `EdgeMeta` provenance byte no
   record has argued either way (`0031`'s seed was removed 2026-09-18).
   [`0009`](./0009-descoping-phase-6.md): removing a reserved field after Phase 6
   populates it is `FORMAT_VERSION = 4`; the ledger schedules that cost.

3. **Couple the stride array to the region count:**
   `let strides: [u32; N_REGIONS + 1]`. A module-level `const` assertion cannot see
   a function-local `let` inside a `pub const fn`; the `+ 1` is `R_TOPO`'s second
   stride. Deleting a stride entry would silently change `layout_hash` for the
   fleet. **Cardinality only:** it cannot see a wrong index or value, the `+ 1` is
   argued in prose and by nothing the compiler reads (a second two-stride region
   makes it `+ 2`), and `rustc` suggests `[u32; 12]` as the fix. Worth doing
   regardless; not a proof.

## Consequences

- `CLAUDE.md` says the header fields are reserved and the region table is not; its
  instruction not to add arena fields opportunistically is unchanged and now names
  the queue a byte joins.
- `PHASE5.md` §12's gates are untouched; §13's contradicting boxes now agree.
- No existing `.tft` is affected; part 3 does not move `layout_hash`. Until a
  twelfth region is written, the forgotten-stride failure changes nothing an
  operator sees.

## Rationale

- **Fit the spline region in the reserved header bytes:** cannot; they are bytes,
  a spline region is per-edge storage scaling with `max_edges`.
- **Leave §1.2 until Phase 6 starts:** a false premise used to decline byte
  requests is worse than an open question.
- **Accept `HeaderInconsistent`:** it refuses safely but sends the operator to
  recreate an arena as if their data were damaged; a diagnostic that lies costs
  more than the check saves (D22).
- **Derive strides from the region constants:** better, rejected as scope: it
  needs a declared width per region, a refactor of `compute` with no measurement
  behind it.
- **Keep the ledger in this record:** a list that changes belongs where changes
  are cheap, and it outlives the record that opened it.

## Implementation plan

1. **Retract the clause.** `docs/PHASE5.md` §1.2, §0.0's §1 row and title, §0's
   in-scope row, §1.1's "Do it once, now, with room reserved", §13's boxes;
   `docs/PROJECT.md` §4's Phase 5 paragraph; this record's row in
   [`README.md`](./README.md). Verified by

   ```sh
   grep -rnE 'regions? (are |is )?reserved|reserved regions|room reserved|Phase 6 regions' \
     docs/ CLAUDE.md README.md CHANGELOG.md
   ```

   returning only corrected wording or a quoted retraction. Use `-E` (a
   basic-regex `|` reads as a clean sweep) and match both word orders.
2. **Open the ledger** (`docs/PROJECT.md` §5.1). **Done;** `0031`'s seed is gone
   because that record was answered out of contract, so re-running this step must
   not re-add it.
3. **Couple the stride array** in `crates/tf_tree_arena/src/layout.rs`:
   `let strides: [u32; N_REGIONS + 1] = [`, the comment naming `R_TOPO` as the
   `+ 1` and what a cardinality check cannot see; the stale `// The eight regions
   in header order.` becomes a reference to the constant. Verified **green** by
   `cargo nextest run -p tf_tree_arena --features shm` with the layout-hash test
   passing on the unchanged literal, and **red** by the staged twelfth region
   failing `cargo build -p tf_tree_arena` with `error[E0308]`.
4. **Fix the evidence's own comment:** `crates/tf_tree/tests/frozen.rs`'s doc on
   the fixture test named `just test`, which cannot reach a
   `required-features = ["shm"]` target.
5. **Flip to `ready`**, with the `CHANGELOG.md` entry `CONTRIBUTING.md` requires.

Steps 1-4 travel together; step 3 must land before any other item adds an arena
region.

## Open questions

None open.

1. ~~**Does the twelfth region produce `HeaderInconsistent`?**~~ **Answered yes**
   (table above); nothing refuses earlier. The red arm already exists in
   `crates/tf_tree/tests/frozen.rs` under `just shm-check`, so this record may not
   be cited for "nothing catches it".
2. ~~**Is a spline region certain to be a region?**~~ **Deferred.**
   [`0009`](./0009-descoping-phase-6.md) says spline evaluation "needs a wider
   bracket read, not a new region shape", so the break's *existence* is
   contingent; the clause's falsity is not (zero region slots are reserved).
3. ~~**Should the ledger live here or in `PROJECT.md`?**~~ **Decided:**
   `PROJECT.md` §5.1; this record holds the argument for a ledger and the seeds.
4. ~~**Does the C ABI or Python surface `layout_hash` so that a second break
   breaks twice?**~~ **No.** `grep -rnE 'layout_hash|format_version|LAYOUT_HASH|FORMAT_VERSION'
   crates/tf_tree_c/` returns nothing, and `crates/tf_tree_py` exposes both as
   build-computed accessors (`arena_format_version`, `arena_layout_hash`). What a
   break touches is the literal `0x3D10_4195` (census: `grep -rnE
   '0x3D10_?4195|3D104195|1024475541'`); the hits that fail a gate are
   `layout::tests::layout_hash_is_deterministic_and_stable` and
   `crates/tf_tree_bench/baseline/results.json` / `results-tf2.json`, whose
   `provenance.layout_hash` is in `baseline::PORTABLE_FACTS` so `just bench-check`
   goes red until the baseline is regenerated (a full suite re-run).
