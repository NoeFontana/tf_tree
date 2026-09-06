# 0032: the region table was not part of the purchase

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`docs/PHASE5.md` §1.2 said, of the Phase 6 fields it reserves:

> Regions whose Phase 6 content does not exist yet are declared in the header
> with offset `0`, meaning absent. Phase 6 then fills them **without another
> layout change**, because the region table already accounts for them.

`CLAUDE.md` repeated the conclusion in its own voice — *"`FORMAT_VERSION = 3`
already happened. Phase 6's regions are reserved so the break happened once"* —
and it was the reason given, everywhere in this project, for refusing to spend a
byte now: the break is paid for, so wait for it.

**The second half of that sentence is false, and the first half is true.** What
`FORMAT_VERSION = 3` bought was the **header**. It did not buy the region table.

`CLAUDE.md` has since been corrected to this record's conclusion while this
record was still `draft`, and `PHASE5.md` §13's `docs/PHASE6.md` box had the
region half right while the same file's §0.0 row, §1.1, §1.2 and §13 tick-box
stated the opposite. A claim being both cited as settled and disputed inside one
file is what `ready` closes here.

## What was measured

**Read *and run*, on this branch.** The instruments are named below so that a
doubter re-runs them rather than trusting a reading; the earlier revision of this
section said *"No build; this is an inspection, and it is labelled as one"*, and
that is no longer the state.

**The header fields exist**, exactly as §1.2 promised
(`crates/tf_tree_arena/src/header.rs`, with `offset_of!` assertions beside them):

```rust
_reserved_covariance: [u8; 8],
pub spline_region_off: u32,
pub spline_degree: u8,
_pad_v3: [u8; 3],
```

**The region table does not.** `crates/tf_tree_arena/src/layout.rs` declares
`R_HEADER` … `R_PARTICIPANT_COUNTERS` and an `N_REGIONS` that counts exactly
those; none of them is a spline region, and `grep -n spline
crates/tf_tree_arena/src/layout.rs` exits 1 with no output. `compute`
allocates `[Region; N_REGIONS]` and loops `while i < N_REGIONS`.

So a Phase 6 spline region is **a twelfth region**, and adding one changes
`ArenaLayout::total_size()` for the same declared geometry.
`spline_region_off` being present in the header does not help: it is a place to
*write* an offset, not a reservation of the bytes the offset would point at.

### The staged twelfth region

A twelfth region was appended in a `git archive HEAD` copy outside the
repository — an `R_SPLINE` constant, `N_REGIONS` incremented, a per-edge entry in
`compute`'s `sizes` array, a `spline()` accessor, and an entry in the
`all_regions` test helper — in two variants, **stride forgotten** and **stride
updated**, against an unmodified copy as the positive control.

**Adding a region is more than the three edits this section used to claim.** The
measured set is: `N_REGIONS`, the region constant, the `sizes` entry, a public
accessor, the `all_regions` test helper, the stride array, and the two
`total_size` fixtures in `layout.rs`'s own tests. Two of those refuse to be
forgotten. `all_regions` returns `[Region; N_REGIONS]`, so omitting the new entry
is `error[E0308]: … expected an array with a size of 12, found one with a size
of 11` — **under `cargo test`, not `cargo build`**, because it is a
`#[cfg(test)]` helper; that is the same mechanism part 3 gives the stride array,
already present one line away from it. And the two `total_size` fixtures fail
loudly on the first run. **The stride array is the one that is silent**, which is
the conclusion the old count was reaching for.

**With the stride forgotten, the arena crate goes fully green.** Updating the
two `total_size` fixtures — which any implementer adding a region does, because
they fail on the first run — leaves `cargo nextest run -p tf_tree_arena
--features shm` green, `layout::tests::layout_hash_is_deterministic_and_stable`
included: the committed `0x3D10_4195` literal still holds, because the hash folds
the stride array and the stride array did not move.

**One test in the workspace default set goes red, and it is not about the
stride.** `cargo nextest run --workspace --no-tests=pass` reports exactly one
failure, `tf_tree_cli sizing::tests::the_formula_is_the_layouts_own_arithmetic`,
which differences `ArenaLayout::total_size` against `sizing`'s own per-edge term.
It fails because a per-edge region was *added*, in either variant, and an
implementer updates `EDGE_BYTES` beside it. It is in the same class as the two
`total_size` fixtures and says nothing about the stride.

### What notices the forgotten stride

`crates/tf_tree/tests/frozen.rs::the_committed_sensor_domain_fixture_reads_and_is_still_tag_one`.
`testdata/frozen/sensor_domain.tft` is a committed arena written by an
eleven-region build, so a twelfth-region build reading it exercises exactly the
foreign-header path `validate_arena_header` exists for.

| variant | what the committed fixture reports |
|---|---|
| unmodified (positive control) | opens; the whole `frozen` target passes |
| twelfth region, **stride updated** | `Frozen(LayoutMismatch { found, expected })` — a **true** statement about version skew, naming both hashes |
| twelfth region, **stride forgotten** | `Frozen(Arena(HeaderInconsistent))` — the corruption diagnosis `docs/RUNBOOK.md` tells the operator to act on by recreating the arena |

No hash value is quoted for the middle row on purpose: it is a function of
whatever stride the staged region happened to pick, so a number here would pin a
fiction. What the row asserts is the **variant**.

**The two arms are the whole argument in one test.** The same test goes red
either way; what changes is what an operator is told. That is D22 from the other
side, and it is why part 3 is worth one line even though the failure is not
silent.

**Where that test runs, checked rather than assumed.** `crates/tf_tree/Cargo.toml`
gives the `frozen` target `required-features = ["shm"]`, so `cargo nextest list
--workspace --no-tests=pass` does not list it and `just test` does not reach it;
`just shm-check` runs it. The test's own doc comment claimed it failed in `just
test`, and that sentence is corrected in the same change as this record.

**The bound on that claim, stated rather than implied.** What was run against the
staged variants is `cargo nextest run --workspace --no-tests=pass`, the arena
crate under `shm`, and — under `shm` — `tf_tree`'s `frozen`, `owned_writer` and
`lib` targets and `tf_tree_cli`'s `doctor_frozen` and `replay_bit_identity`. The
remaining `shm-check` targets were not run, so "this is what notices it" is a
statement about that set and not about the tree.

**One trap in reproducing any of this, met and measured.** A staged copy that
shares `CARGO_TARGET_DIR` with the repository can be linked against the *other*
tree's `tf_tree_arena`: the staged tree's own failure appeared in an unmodified
checkout, and touching the sources made it disappear. Give the staged copy its
own target directory, or force a rebuild of every path package in it, before
believing either colour.

**`SizeMismatch` cannot pre-empt any of it.** `validate_arena_header` refuses in
the order magic → version → layout hash → declared size → implied geometry, and
a writer's segment agrees with its own `arena_size` field, so the geometry
conjunct is reached in both arms.

## The part that is worse than a second break

`layout_hash` folds a stride array that, until this record landed, was
**separately hardcoded**:

```rust
let strides: [u32; 12] = [
    320, 64, FRAME_HASH_STRIDE as u32, 12, TOPO_BLOCKS as u32,
    64, 128, 128, 8, 64,
    128, // edge counters (v3)
    128, // participant counters (v3)
];
```

**Nothing coupled that array to `N_REGIONS`.** They were twelve numbers and
eleven numbers, maintained by hand, in the same file, with no assertion between
them — and the twelve is not a typo: `R_TOPO` folds two values, the per-frame
width and `TOPO_BLOCKS`, which is why the relation is `N_REGIONS + 1` and not
`N_REGIONS`.

**If the stride is updated, the outcome is correct**: `layout_hash` changes, and
a v3 consumer meets `ShmError::LayoutMismatch`, which is a true statement about
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

## Decision

**Taken.** Three parts.

1. **Retract `PHASE5.md` §1.2's second clause.** The header fields are reserved;
   the region table is not. Phase 6's spline region requires a twelfth region, a
   twelfth stride and therefore a **new `FORMAT_VERSION`** — the second break §1
   was written to avoid. **Only the second clause is retracted:** the header
   fields exist and their offsets are asserted, and a retraction reading "§1.2
   was wrong" invites someone to relitigate work that is sound. Saying so is most
   of the value of this record: the sentence was load-bearing for decisions taken
   elsewhere.

2. **Name that break as the project's one scheduled break, and open a ledger for
   it** — `docs/PROJECT.md` §5.1, beside the decision log D22 belongs to.
   A named break with a list turns "wait for the next one" from a refusal into a
   schedule. Its first entries, **none of them authorised by this record**, are
   Phase 6's spline region, `0031`'s question, and an `EdgeMeta` provenance byte
   no record has argued either way — that last is listed so a future request has
   somewhere to be argued, and this record is not that argument.

   **A fabricated citation is removed here rather than repaired.** *This part
   used to attribute to [`0009`](./0009-descoping-phase-6.md) the sentence*
   *"no bump is queued … so 'wait' means 'indefinitely', which is a decision not*
   *to build it."* `0009` does not contain it, and neither does anything else in
   the tree: `grep -rn 'bump is queued'` finds one hit, this file, and
   `git log -S` finds it entering in this record's own first commit. What `0009`
   *does* say is adjacent and weaker — that §1 spent the 2 → 3 break "partly to
   pre-reserve Phase 6's layout, so that *the break happens exactly once*", and
   that removing a reserved field after Phase 6 populates it is a
   `FORMAT_VERSION = 4`. That is the cost this ledger exists to schedule, and it
   carries the argument without a quotation nothing compares.

3. **Couple the stride array to the region count**, so the silent edit cannot be
   missed. The form is the array's own type — `let strides: [u32; N_REGIONS + 1]`
   — and both halves of that sentence are corrections to this record's own
   earlier text. *It proposed "a `const` assertion in `layout.rs` relating
   `strides.len()` to `N_REGIONS`"*: `strides` is a function-local `let` inside a
   `pub const fn`, so a module-level `const` assertion cannot see it; and the
   relation is `N_REGIONS + 1`, because `R_TOPO` contributes two stride entries.
   An implementer following the old text literally writes an assertion that is
   false on an unmodified tree, and the tempting repair — deleting a stride entry
   — silently changes `layout_hash` for every participant in the fleet.

   **What it does not catch, so that nobody over-reads one line.** It is a
   *cardinality* check: it cannot see a stride written at the wrong index or with
   the wrong value, and the `+ 1` is a constant argued for in prose — here and in
   the comment beside the array — and by nothing the compiler reads: a second
   two-stride region makes it `+ 2` with nothing to say so.
   `rustc`'s own diagnostic suggests `[u32; 12]` as the fix, which is the escape
   hatch spelled out for the person the check is aimed at. It is worth doing
   whether or not Phase 6 is ever built, and it is not a proof.

## Consequences

- `CLAUDE.md` no longer says the break happened once. It says the header fields
  are reserved and the region table is not — and that edit landed *before* this
  record moved out of `draft`, which is what made a `draft` record load-bearing
  for the project's agent contract. Its practical instruction — do not add arena
  fields opportunistically — is **unchanged and if anything strengthened**,
  because it now names the queue a byte joins rather than an indefinite wait.
- `PHASE5.md` §12's gates are untouched: nothing in them depends on the
  retracted clause. What moved is §13, where the `FORMAT_VERSION = 3` box and
  the `docs/PHASE6.md` box had said opposite things about the region table; the
  second of them also restated this record's status, and now points at the
  status line instead, which is the rule `decisions/README.md` states for
  itself.
- No existing `.tft` is affected, and part 3 does not move `layout_hash` — the
  array's contents are untouched, only its declared length is now an expression.
- The forgotten-stride failure keeps its diagnosis until someone writes a
  twelfth region: part 3 turns it into `error[E0308]` at that moment, and until
  then it changes nothing an operator can see.

## Rationale

The alternatives, and why they lost.

**Make the spline region fit inside the reserved header bytes.** It cannot: the
header's `_reserved` is bytes, and a spline region is per-edge storage whose size
scales with `max_edges`. The reservation is the wrong shape, not the wrong size.

**Leave §1.2 as it is and discover this when Phase 6 starts.** This is the status
quo and it is the option this record exists to refuse. The sentence was quoted as
settled in `CLAUDE.md` and used to decline byte requests; a false premise used to
make decisions is worse than an open question.

**Treat `HeaderInconsistent` as good enough.** It is a refusal, so nothing
corrupts, and the committed fixture test does go red. But `RUNBOOK.md` sends the
operator to recreate the arena as though their data were damaged, and D22's whole
argument is that a diagnostic which lies costs more than the check saves.

**Derive the stride array from the region constants, so there is one list rather
than two.** The better form, and rejected here as scope: the strides are record
*widths* and the region constants are *indices*, so deriving one from the other
means giving every region a declared width — a refactor of
`compute` that this record has no measurement behind. One line that turns
a silent edit into a compile error is what the evidence supports.

**Keep the ledger inside this record.** Rejected: a list that changes belongs
where changes are cheap, and a `ready` record is meant to be stable. The
counter-argument — [`0028`](./0028-the-slot-a-killed-participant-keeps.md) is
precedent for a record whose head table is edited until it freezes — loses
because this list will outlive the record that opened it.

## Implementation plan

1. **Retract the clause.** `docs/PHASE5.md` §1.2 (the sentence, and the
   `Disputed` block under it becomes the retraction), §0.0's §1 row and its
   title, §0's in-scope row, §1.1's *"Do it once, now, with room reserved"*, and
   §13's `FORMAT_VERSION = 3` box and its `docs/PHASE6.md` box;
   `docs/PROJECT.md` §4's Phase 5 paragraph, and this record's row in
   [`README.md`](./README.md). Verified by

   ```sh
   grep -rnE 'regions? (are |is )?reserved|reserved regions|room reserved|Phase 6 regions' \
     docs/ CLAUDE.md README.md CHANGELOG.md
   ```

   returning only corrected wording or a quoted retraction. **Two properties of
   that pattern are load-bearing.** Run it with `-E`: written with a basic-regex
   `|` it matches nothing and exits 1, which reads as a clean sweep. And it has
   to match the two words in **both orders** — a pattern that sees
   `regions reserved` and not `reserved regions` returns a clean sweep over the
   sites that phrase it the other way, which is where the stale claims sit.
2. **Open the ledger** — `docs/PROJECT.md` §5.1, with the three seed entries,
   each marked queued and none authorised. Verified by review. `0031` is `draft`
   and its row says so.
3. **Couple the stride array.** One line in
   `crates/tf_tree_arena/src/layout.rs` — `let strides: [u32; N_REGIONS + 1] = [`
   — with the comment naming `R_TOPO` as the `+ 1` and stating what a cardinality
   check cannot see. The stale `// The eight regions in header order.` beside
   `N_REGIONS` goes in the same diff, and becomes a reference to the constant
   rather than a new number. Verified **green** by `cargo nextest run
   -p tf_tree_arena --features shm` with
   `layout::tests::layout_hash_is_deterministic_and_stable` passing on the
   unchanged literal, and **red** by the staged twelfth region, which then fails
   `cargo build -p tf_tree_arena` with `error[E0308]: mismatched types …
   expected an array with a size of 13, found one with a size of 12`.
4. **Fix the evidence's own comment.** `crates/tf_tree/tests/frozen.rs`'s doc
   comment on the committed-fixture test says where it fails; it named `just
   test`, which cannot reach a `required-features = ["shm"]` target. Verified by
   `cargo nextest list --workspace --no-tests=pass` not listing it.
5. **Flip to `ready`**, with the `CHANGELOG.md` entry `CONTRIBUTING.md` requires.

Steps 1–4 travel together: a document describing a coupling the code does not
have is the failure mode this record exists to prevent. Step 3 is the one that
must land before any other item adds an arena region, because it is what makes
that item's mistake a compile error.

## Open questions

None.

1. ~~**Does the twelfth region actually produce `HeaderInconsistent`, or does
   something else refuse first?**~~ **Answered, and the answer needed no
   staging of the reader.** Both variants were built and read the committed
   `testdata/frozen/sensor_domain.tft`: stride forgotten gives
   `Frozen(Arena(HeaderInconsistent))`, stride updated gives
   `Frozen(LayoutMismatch { … })`, and an unmodified build opens it. Nothing
   refuses earlier — see *What was measured* for the check order and for why
   `SizeMismatch` cannot pre-empt it.

   **Two findings the question did not anticipate.** The first: with the two
   `total_size` fixtures updated, the forgotten-stride build passes the whole
   arena crate including the layout-hash snapshot, so the silence is real inside
   the crate that owns the layout. The second, which *lowers* part 3's urgency
   rather than raising it: the repository already contains the red arm, in
   `crates/tf_tree/tests/frozen.rs`, and it runs in `just shm-check`. Part 3 is
   still worth its one line — a compile error beats a golden-file failure that
   names a regenerator rather than a missing stride — but this record may not be
   cited for the claim that nothing catches it.

2. ~~**Is a spline region per-edge, and is it therefore certain to be a region at
   all?**~~ **Deferred, explicitly, and with the citation this record was
   missing.** [`0009`](./0009-descoping-phase-6.md)'s rationale says the opposite
   of §1.2's assumption: *"a spline evaluation needs a wider bracket read, not a
   new region shape. Its reserved header fields are therefore earned and stay."*
   So the break's *existence* is contingent on a Phase 6 nobody has designed.

   **The clause's falsity is not contingent on it.** Zero region slots are
   reserved, so §1.2's *"the region table already accounts for them"* is false
   whatever shape Phase 6 turns out to have — it would be false if Phase 6 were
   cancelled tomorrow. The deferral is about the size of the break, never about
   whether the sentence should stand. Settling it by designing a spline region
   would be closing an open question by writing code.

3. ~~**Should the ledger live in this record or in `PROJECT.md`?**~~
   **Decided: `PROJECT.md` §5.1**, beside the decision log, with this record
   holding only the argument for having one and the seed entries. See *Rationale*
   for the `0028` counter-argument and why it loses.

4. ~~**Does the C ABI or the Python binding surface `layout_hash` in a way that a
   second break would break twice?**~~ **Answered: no.**
   `grep -rnE 'layout_hash|format_version|LAYOUT_HASH|FORMAT_VERSION'
   crates/tf_tree_c/` returns nothing — the C ABI surfaces neither constant — and
   `crates/tf_tree_py` surfaces both as *build-computed accessors*
   (`arena_format_version`, `arena_layout_hash`), re-exported from
   `python/tf_tree/__init__.py` and typed in `_core.pyi`. Neither binding bakes
   in a literal, so a second break costs the bindings nothing beyond a rebuild.

   **`-E` is load-bearing in that command**, and it is why this answer is stated
   with its instrument: the same grep without it treats `|` as a literal, matches
   nothing, and exits 1 — which looks exactly like the answer it is being used to
   establish. What a break *does* touch is the literal `0x3D10_4195` where it is
   written out. `grep -rnE '0x3D10_?4195|3D104195|1024475541'` is the census;
   run it rather than reading a list here, which is a measurement with a date on
   it. What the census does not tell you is which hits **fail a gate**, and that
   is the part worth stating:

   * `crates/tf_tree_arena/src/layout.rs`'s snapshot assertion —
     `layout::tests::layout_hash_is_deterministic_and_stable`, red in
     `cargo nextest`.
   * `crates/tf_tree_bench/baseline/results.json` and `results-tf2.json` carry
     `layout_hash` in their `provenance` object, and it is in
     `baseline::PORTABLE_FACTS` — the facts that describe the *artifact* rather
     than the host — so `compare` pushes a hard failure naming both hashes and
     `just bench-check` goes red. A second break therefore costs a baseline
     regeneration, which is a full suite re-run and a diff of every number in
     the artifact.

   The remaining hits are a `#[cfg(test)]` fixture constant, `layout.rs`'s own
   hash-history comment, and prose. None of those refuses anything, and the
   bindings still bake in nothing.
