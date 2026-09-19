# 0027: the 48-byte frame-name store

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none yet

## Context

`FrameRecord::name` is `[u8; 48]`. `FrameRecord::for_name` writes
`src.len().min(48)` (`crates/tf_tree_core/src/frame.rs`) with no refusal: a
56-byte name is interned as successfully as a 6-byte one. `blake3_64` is taken
over the full name, so identity is exact and display is not; every published
name surface (`Tree::frames`, `Tree::edges`, the `.tft` manifest, `doctor`,
`top`) reads the truncated copy. [`0026`](./0026-the-corpus-shape-of-a-frozen-index.md)
left this to this record.

## What is broken

- **Not broken: resolution.** `lookup` on the full over-length name resolves to
  its own frame, not to the truncated one.
- **Broken 1: `frames()` output is not `frames()` input.** Two distinct frames
  whose names share a 48-byte prefix appear as byte-identical entries; the string
  copied out for the long one answers as the short one, with no error. `len == 48`
  is not a sound tell: a genuine 48-byte name resolves correctly.
- **Broken 2: `edges()` reports a graph that is not a tree.** Long names yield
  self-loops in `edges()` while lookups stay correct (D2). One over-long name
  suffices.
- **The full name is nowhere in the frozen file.** `Frozen::manifest`
  (`crates/tf_tree/src/frozen.rs`) reads `FrameRecord::name`, the only copy, so
  the freeze path cannot emit it.
- **Truncation can split a UTF-8 codepoint**, so stored bytes need not be valid
  UTF-8.

A wider store is cheap in bytes and expensive as a format break.
`FrameRecord` is `#[repr(C, align(64))]`, 64 bytes; `layout_hash()` hashes the
frame-table stride as the literal `64` (`tf_tree_arena/src/layout.rs`), so any
stride change invalidates every `.tft` and running participant. A change inside
the stride (`name: [u8; 53]`) is invisible to every check (`layout_hash` hashes
region strides, not field offsets) and does not reach the 56-byte case.

## Decision

**`intern` refuses a name longer than 48 bytes. Nothing is truncated, and the
store does not move.**

1. **`FrameError` gains `NameTooLong { len: u32 }`.** `ArenaView::intern` returns
   it before hashing. It carries the length, not the name (`Copy`; R5, D11).
   `FrameError` is `#[non_exhaustive]`.
2. **`FrameRecord::for_name` keeps `min(48)` and gains a debug assertion**: its
   one caller is inside `intern`, which has already refused.
3. **The two places names arrive from outside report the refusal; they do not
   abort.** MCAP ingest (`tf_tree_ingest`) and the ROS bridge (`tf_tree_bridge`)
   keep reading. §3.2's anomaly table gains a row: *frame name over 48 bytes ->
   drop the edge, count, report loudly, name the frame*.
4. **`doctor` gains `TFT020`, a warning.** The refusal is not retroactive; the
   check flags a stored name length of 48. It cannot be an error: a genuine
   48-byte name is a reachable false positive. Message: "may have been truncated;
   regenerate this file with a build that refuses".
5. **The 48-byte bound is documented as a public constraint** in `API.md` §2 and
   `tf_tree.build`'s docstring, with the measured margin of **5 bytes** (longest
   shipped name 43), at which no namespacing prefix the ROS ecosystem ships fits.

## Consequences

- Breaking: a program that builds a tree with a 60-byte name stops working.
- Existing `.tft` files are not fixed; `TFT020` reports and regeneration from the
  source recording is the only repair.
- `0026`'s Decision item 4: its index writer reads names from its own source, not
  `frames()`/`edges()`, for files written before this.
- The C ABI needs a way to say "this name was refused";
  [`0020`](./0020-the-consumer-side-of-the-arena-refusal.md) records that widening
  an existing function's return set is not covered by the minor-bump precedent
  (question 3).
- Arena layout, `FORMAT_VERSION` and `layout_hash` (`0x3D10_4195`) are untouched.

## Implementation plan

1. `FrameError::NameTooLong { len: u32 }` and the refusal in `ArenaView::intern`,
   before `blake3_64`. Tests intern a 49-byte name (asserting the variant) and a
   48-byte name (asserting `Ok`). Mutant: `> 49` must fail the first and pass the
   second.
2. Python exception mapping in `crates/tf_tree_py/src/errors.rs` beside the
   `FrameError` arms; `API.md` §2 and docstring text from Decision item 5.
   Verified by a Python test and `just py-test`.
3. §3.2's anomaly row and the ingest/bridge reporting path, verified by an ingest
   fixture with one over-long `frame_id`: the recording ingests, the edge is
   absent, the report names the frame.
4. `TFT020` with its fixture, per §11, and a second test asserting a genuine
   48-byte name does not raise the check to error level.
5. `docs/PHASE5.md` §6 catalogue entry and the `doctor --json` schema addition,
   verified by the existing schema test.

## Open questions

1. **RESOLVED by measurement: nothing reachable exceeds 48; the margin is 5
   bytes.** Five captured recordings and 26 published robot-description packages
   (887 distinct links) have a longest name of 43 bytes; no prefix a fleet would
   use fits (`tiago/` refuses two links). Not established: no multi-robot or
   `tf_prefix`-namespaced `/tf` bag was found.
2. **Should the bound be exposed as a constant?** `tf_tree_core::MAX_FRAME_NAME`
   would pin 48 as public API and make a widening a second break. Not answered.
3. **How does the C ABI report it?** `0020` is the precedent and is itself
   `draft`; the two want deciding together. If it has not moved when steps 1-5 are
   wanted, steps 1, 2, 4 and 5 can land and leave the C ABI reporting
   `TFT_ERR_INTERNAL`, as a decision rather than an omission.
4. **Refusal at `intern` or at every public entry point?** Item 1 puts it at the
   single choke point; a builder-level pre-pass reporting all bad names at once is
   a different change, not scoped here.

This record does not authorise any step.
