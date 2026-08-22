# 0032: the region table was not part of the purchase

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none. This record authorises no code.

## Context

`docs/PHASE5.md` §1.2 says, of the Phase 6 fields it reserves:

> Regions whose Phase 6 content does not exist yet are declared in the header
> with offset `0`, meaning absent. Phase 6 then fills them **without another
> layout change**, because the region table already accounts for them.

`CLAUDE.md` repeats the conclusion in its own voice — *"`FORMAT_VERSION = 3`
already happened. Phase 6's regions are reserved so the break happened once"* —
and it is the reason given, everywhere in this project, for refusing to spend a
byte now: the break is paid for, so wait for it.

**The second half of that sentence is false, and the first half is true.** What
`FORMAT_VERSION = 3` bought was the **header**. It did not buy the region table.

## What was measured

Read at `60f9541`. No build; this is an inspection, and it is labelled as one.

**The header fields exist**, exactly as §1.2 promised
(`crates/tf_tree_arena/src/header.rs`):

```rust
_reserved_covariance: [u8; 8],
pub spline_region_off: u32,
pub spline_degree: u8,
_pad_v3: [u8; 3],
```

**The region table does not.** `crates/tf_tree_arena/src/layout.rs`:

```rust
const R_HEADER: usize = 0;          const R_EDGE: usize = 6;
const R_FRAME_TABLE: usize = 1;     const R_STAMP: usize = 7;
const R_FRAME_HASH: usize = 2;      const R_POSE: usize = 8;
const R_TOPO: usize = 3;            const R_EDGE_COUNTERS: usize = 9;
const R_CLAIM: usize = 4;           const R_PARTICIPANT_COUNTERS: usize = 10;
const R_PARTICIPANT: usize = 5;
const N_REGIONS: usize = 11;
```

Eleven regions, none of them a spline region. `compute_regions` allocates
`[Region; N_REGIONS]` and loops `while i < N_REGIONS`. `grep -n spline
crates/tf_tree_arena/src/layout.rs` returns nothing.

So a Phase 6 spline region is **a twelfth region**, and adding one changes
`ArenaLayout::total_size()` for the same declared geometry. `spline_region_off`
being present in the header does not help: it is a place to *write* an offset,
not a reservation of the bytes the offset would point at.

## The part that is worse than a second break

`layout_hash` folds a **separately hardcoded** stride array
(`layout.rs`, in `layout_hash()`):

```rust
let strides: [u32; 12] = [
    320, 64, FRAME_HASH_STRIDE as u32, 12, TOPO_BLOCKS as u32,
    64, 128, 128, 8, 64,
    128, // edge counters (v3)
    128, // participant counters (v3)
];
```

**Nothing couples that array to `N_REGIONS`.** `grep -n 'strides.len()\|N_REGIONS'
layout.rs` shows the two facts never meet: `strides.len()` appears once, in
`layout_hash`'s own loop, and `N_REGIONS` never appears in that function. They
are twelve numbers and eleven numbers, maintained by hand, in the same file, with
no assertion between them.

So adding a region is **three independent edits** — `N_REGIONS`, the region
constant and its slot in `compute_regions`, and the stride array — and the third
is the one a reader would not know to make.

**If it is made, the outcome is correct**: `layout_hash` changes, and a v3
consumer meets `ShmError::LayoutMismatch`, which is a true statement about
version skew.

**If it is missed, the outcome is the failure D22 exists to forbid.**
`layout_hash` is unchanged, so nothing refuses on version; but
`check.rs`'s `validate_arena_header` recomputes the geometry and requires

```rust
let matches = implied.total_size() as u64 == h.arena_size && …
```

which now fails — so two builds of the *same* `FORMAT_VERSION` and the *same*
`layout_hash` refuse each other with `ShmError::HeaderInconsistent`, whose
`docs/RUNBOOK.md` entry tells an operator the header *"does not match the
geometry its own capacities imply"*. That is a corruption diagnosis delivered
for a version-skew fact. D22 is **"a disabled feature never forks the layout
hash"**; this is the same principle from the other side — an *enabled* region
forking the geometry without forking the hash.

**Not measured:** nobody has built the twelfth region and observed either
outcome. Both are read off `compute_regions`, `layout_hash` and
`validate_arena_header`. That is the first thing an implementer should do, and
it is cheap.

## Decision

**Proposed, not taken.** Three parts.

1. **Retract `PHASE5.md` §1.2's second clause.** The header fields are reserved;
   the region table is not. Phase 6's spline region requires a twelfth region, a
   twelfth stride and therefore a **new `FORMAT_VERSION`** — the second break §1
   was written to avoid. Saying so is most of the value of this record: the
   sentence is currently load-bearing for decisions taken elsewhere.

2. **Name that break as the project's one scheduled break, and open a ledger for
   it.** `0009` already made this argument in the opposite direction —
   *"no bump is queued … so 'wait' means 'indefinitely', which is a decision not
   to build it."* A named break with a list turns "wait for the next one" from a
   refusal into a schedule. First entries, none of them authorised by this
   record:
   - Phase 6's spline region, which is what forces the break.
   - `0031`'s question, if it is answered by giving a byte-less participant
     record something to be judged by.
   - The `EdgeMeta` provenance byte, **declined on its own merits** and queued
     only so that a future consumer has somewhere to argue.

3. **Couple the stride array to the region count**, so the third edit cannot be
   missed. The cheapest form is a `const` assertion in `layout.rs` relating
   `strides.len()` to `N_REGIONS`; the better form derives the array from the
   region constants so there is one list rather than two. **This is worth doing
   whether or not Phase 6 is ever built**, and it is the only part of this record
   that is a plain code change rather than a decision.

## Consequences

- `CLAUDE.md`'s *"the break happened once"* becomes *"the header break happened
  once; the region break is scheduled"*. Its practical instruction — do not add
  arena fields opportunistically — is **unchanged and if anything strengthened**,
  because there is now a queue to join rather than an indefinite wait.
- `PHASE5.md` §12's gates are untouched: nothing in them depends on the
  retracted clause.
- No existing `.tft` is affected. This record changes what the project *says*,
  not what it writes.

## Alternatives considered

**Make the spline region fit inside the reserved header bytes.** It cannot: the
header's `_reserved` is bytes, and a spline region is per-edge storage whose size
scales with `max_edges`. The reservation is the wrong shape, not the wrong size.

**Leave §1.2 as it is and discover this when Phase 6 starts.** This is the status
quo and it is the option this record exists to refuse. The sentence is quoted as
settled in `CLAUDE.md` and used to decline byte requests; a false premise used to
make decisions is worse than an open question.

**Treat `HeaderInconsistent` as good enough.** It is a refusal, so nothing
corrupts. But `RUNBOOK.md` sends the operator to recreate the arena as though
their data were damaged, and D22's whole argument is that a diagnostic which
lies costs more than the check saves.

## Open questions

1. **Does the twelfth region actually produce `HeaderInconsistent`, or does
   something else refuse first?** Read from `validate_arena_header`, not run. An
   implementer should stage both variants — stride updated and stride forgotten —
   and record what each reports. **This is the question that decides how urgent
   part 3 is.**
2. **Is a spline region per-edge, and is it therefore certain to be a region at
   all?** §1.2 assumes so. If Phase 6's splines turned out to be a fixed-size
   header extension, the break would be smaller than this record claims.
   Nobody has designed Phase 6, which is exactly why the reservation was made
   speculatively and exactly why it did not fit.
3. **Should the ledger live in this record or in `PROJECT.md`?** A list that
   changes belongs where changes are cheap; a decision record is meant to be
   stable once `ready`. `0028` is the precedent for a record whose head table is
   edited until it freezes, and the argument against is that this list will
   outlive the record.
4. **Does the C ABI or the Python binding surface `layout_hash` in a way that a
   second break would break twice?** Not checked.

## What would make this `ready`

- Question 1 answered by staging both variants and quoting what each reports.
- Question 2 answered, or explicitly deferred with the note that the break's
  *size* is unknown while its *existence* is not.
- Part 3 drafted as a patch, since it is the one piece that needs no decision and
  the record should not sit `draft` while a compile-time assertion waits on it.
