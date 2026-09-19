# 0027: the 48-byte frame-name store

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none yet

## Context

`FrameRecord::name` is `[u8; 48]`. `FrameRecord::for_name` writes
`let n = src.len().min(48)` (`crates/tf_tree_core/src/frame.rs`) with no refusal,
flag or signal: a 56-byte name is interned as successfully as a 6-byte one.

`blake3_64` is taken over the **full** name, and `FrameRecord::name_matches`
compares the truncated bytes only as a hash-collision tiebreak. So *identity* is
exact and *display* is not, and every published name surface — `Tree::frames`,
`Tree::edges`, the `.tft` manifest, `doctor`, `top` — reads the truncated copy.

[`0026`](./0026-the-corpus-shape-of-a-frozen-index.md) found this and left it to
this record: the defect (`edges()` returning a self-loop) is reachable by any
user with a long frame name, corpus or not.

## What is broken, and what is not

Reproduced against the built wheel (`arena_format_version() == 3`,
`arena_layout_hash() == 0x3d104195`).

### Not broken: resolution

**A `lookup` on a full over-length name does not resolve to the truncated
frame.** With `V` 51 bytes and `T = V[:48]` declared as a **separate** frame under
`root` with different translations, `lookup('root', V)` and `lookup('root', T)`
each return their own transform, because the hash is over the full name. A caller
holding the real names gets right answers.

### Broken 1 — `frames()` output is not usable as `frames()` input

`frames()` emits three entries, two distinct, and the two 48-byte entries are one
string: byte-identical names for two different frames. The string a user copies
out for `V` **is** `T`'s name and answers as `T`, with no error; the full name `V`
is not in `frames()` at all. That output-then-reuse loop is what a corpus index,
`doctor` and any plotting script do; a silent wrong transform is the failure class
D11 and D15 exist to eliminate.

`len(name) == 48` is **not** a sound tell: `T` is a genuine 48-byte name that
resolves correctly.

### Broken 2 — `edges()` reports a graph that is not a tree

Two episodes under a 56-byte corpus key, each a `map → odom → base_link` chain
(six frames, four edges), give `frames()` 6 entries / 1 distinct and `edges()` 4
entries / 1 distinct: **four self-loops**, while the engine's lookups stay correct.
A consumer reconstructing topology from `edges()` builds a cycle (D2: a tree, not
a pose graph). It needs **one** over-long frame name and no corpus.

### Two corollaries

**The full name is nowhere in the frozen file** (0 occurrences of `V`; 4 of `T`).
`Frozen::manifest` (`crates/tf_tree/src/frozen.rs`) reads `FrameRecord::name` out of
the arena, which holds no other copy, so **the freeze path cannot emit the full
name**, and "put full names in the manifest" is not implementable against the
current store.

**Truncation can split a UTF-8 codepoint**: 47 ASCII bytes plus a 3-byte
codepoint keeps one of the three, so `frames()` shows U+FFFD and the stored bytes
are not guaranteed valid UTF-8.

### What a wider store costs

`FrameRecord` is `#[repr(C, align(64))]`, exactly 64 bytes (asserted at compile
time): `name_hash` 8, `name` 48, `name_len`/`flags`/`frame_kind` 1 each,
`_pad: [u8; 5]`. The arena layout is pinned by
`tf_tree_arena`'s `layout_hash_is_deterministic_and_stable`,
`large_uniform_fixture` (1000 frames: `frame_table().size` 64 000 of
`total_size()` 295 393 600) and `small_mixed_capacity_fixture` (8 frames: 512).

The frame table is 0.0217 % of that arena; a 256-byte name field (record 320 B)
adds +0.087 % and, on the `just gate4` fixture (1 537 frames), +0.11 %. **Bytes are
not the cost; a format break is:**

- **A stride change invalidates everything.** `layout_hash()` hashes the
  frame-table stride as the literal `64` (`tf_tree_arena/src/layout.rs`);
  changing it changes `0x3D10_4195`, refusing every existing `.tft` and running
  participant. `PHASE5.md` §1 spent one break to avoid taking three, and
  arena fields are not to be added opportunistically.
- **A change that fits inside the stride is worse.** `name: [u8; 53]` keeps 64
  bytes and is **invisible to every check the project has** (`layout_hash` hashes
  region strides, not field offsets), so two builds disagreeing on where
  `name_len` lives would attach and misread names. And 53 does not reach the
  56-byte key that exposed the defect.

## Decision

**`intern` refuses a name longer than 48 bytes. Nothing is truncated, and the
store does not move.**

1. **`FrameError` gains `NameTooLong { len: u32 }`.** `ArenaView::intern` returns
   it before hashing. It carries the length, not the name (the error type is
   `Copy`; R5, D11). `FrameError` is `#[non_exhaustive]`.
2. **`FrameRecord::for_name` keeps `min(48)` and gains a debug assertion**, not a
   second refusal: it has one caller, inside `intern`'s `write_record` closure,
   which has already refused.
3. **The two places names arrive from outside report the refusal; they do not
   abort.** MCAP ingest (`tf_tree_ingest`) and the ROS bridge (`tf_tree_bridge`)
   must not stop reading because one publisher has a long name. §3.2's anomaly
   table gains a row: *frame name over 48 bytes → drop the edge, count, report
   loudly, name the frame*.
4. **`doctor` gains `TFT020`, a warning.** The refusal is not retroactive: earlier
   arenas and `.tft` files still hold truncated names. The check flags a stored
   name length of 48. It **cannot** be an error, because the tell has a measured
   false positive (`T` above). Message: "may have been truncated; regenerate this
   file with a build that refuses".
5. **The 48-byte bound is documented as a public constraint** in `API.md` §2 and
   `tf_tree.build`'s docstring, with the measured margin: **5 bytes** (longest
   shipped name 43), at which **no namespacing prefix the ROS ecosystem ships
   fits**. It must say so plainly, not only "48 minus your longest name".

## Rationale

**Every alternative leaves a name in `frames()` that is not the caller's name.**
A refusal is the only option that makes "what `frames()` returned" and "what you
can pass back" the same set. It fixes Broken 2 outright and turns Broken 1 from a
silent wrong transform into a declaration-time error (a weaker claim, stated as
such).

The refusal is structurally cheap: `intern` is already fallible, `FrameError` is
`Copy` and `#[non_exhaustive]`, and `TreeBuilder::build_with`
(`crates/tf_tree/src/tree.rs`) already maps it to `BuildError::Frame` at three
`intern` sites (the frame loop and the edge loop's `parent` and `child`). Nothing
on a hot path.

### Alternatives considered

- **Keep 48 bytes and make truncation loud** with a `TRUNCATED` bit in the unused
  `FrameRecord::flags`. `frames() -> list[str]` has nowhere to carry a per-entry
  flag, and `frames_truncated()` beside it is a forbidden second spelling (§6). It
  leaves the silent wrong answer for callers who do not run `doctor`. Worth
  having only if `TFT020` fires too often.
- **Widen the store.** Negligible bytes, expensive format; and any finite bound has
  the same failure shape at the bound. It converts a certainty into a rarity.
- **Reclaim the pad bytes (48 → 53).** Invisible to `layout_hash`; does not reach
  56.
- **Hash-suffix the truncation** (`name[..40]` plus 8 hex of `blake3_64`). Names
  stay distinct and a suffixed string fed back fails loudly, with no layout
  change. Rejected: it manufactures a name that is not the caller's, and
  `name_matches` would have to reproduce the suffixing. **The shelf fallback if
  question 1 had found real users broken.**
- **Put full names in the manifest.** Measured dead (above); redundant after a
  wider store.

## Consequences

- **It is a breaking change**: a program that builds a tree with a 60-byte name
  works today and stops. The headroom is **5 bytes, not the 28 the arithmetic
  `48 − len("camera_optical_frame")` suggested** (question 1): the break is
  theoretical for one unprefixed robot and one namespace character away for a fleet.
- **Existing `.tft` files are not fixed and cannot be.** `TFT020` reports;
  regeneration from the source recording is the only repair.
- **`0026`'s Decision item 4 gets firmer footing.** Its index writer must read names
  from its own source, not `frames()`/`edges()`, for files written before this.
- **The C ABI has a new failure to express.** `tft_bridge_*` needs a way to say
  "this name was refused", and [`0020`](./0020-the-consumer-side-of-the-arena-refusal.md)
  records that widening an existing function's return set is the one addition the
  minor-bump precedent does not cover. See question 3.
- **The arena layout, `FORMAT_VERSION` and `layout_hash` are untouched**
  (`0x3D10_4195` stands; no `.tft` is invalidated).
- `just` recipes and CI are unchanged; new tests attach to existing suites.

## Implementation plan

1. `FrameError::NameTooLong { len: u32 }` and the refusal in `ArenaView::intern`,
   before `blake3_64`. Verified by tests that intern a 49-byte name (asserting the
   variant) and a 48-byte name (asserting `Ok`); both sides of the boundary.
   Mutant: `> 49` must fail the first and pass the second.
2. Python exception mapping in `crates/tf_tree_py/src/errors.rs` beside the
   `FrameError` arms; `API.md` §2 and docstring text from Decision item 5.
   Verified by a Python test and `just py-test`.
3. §3.2's anomaly row and the ingest/bridge reporting path. Verified by an ingest
   fixture with one over-long `frame_id`, asserting the recording still ingests,
   the edge is absent, and the report names the frame.
4. `TFT020` with its fixture, per §11 (one test per check ID), **and** a second
   asserting a genuine 48-byte name does *not* raise the check to error level.
5. `docs/PHASE5.md` §6 catalogue entry and the `doctor --json` schema addition,
   verified by the existing schema test.

## Open questions

1. **RESOLVED 2026-08-22 by measurement: nothing reachable exceeds 48. The margin
   is 5 bytes, not 28.** Every set was interned through `TreeBuilder::frame` and
   read back through `Tree::frames()`, with a self-test control proving the probe
   sees the defect (49 and 56-byte names lost).

   | Source | Kind | Distinct | Max B | >48 |
   |---|---|---:|---:|---:|
   | `testdata/tfstream/indoor_atelier.tfstream` | captured | 10 | 16 | 0 |
   | Zenodo 19894190 outdoor run, Header `frame_id`s | captured | 3 | 14 | 0 |
   | HF `xrkong/nuway_rosbag`, Nav2 shuttle, `/tf` | captured | 2 | 9 | 0 |
   | Zenodo 13749419, RoboCup@Home 2024, PAL TIAGo, `/tf` + `/tf_static` | captured | 65 | **42** | 0 |
   | HF `UniflexAI/rosbag2_d435i_g1_indoor`, Unitree G1, **sampled** | captured | — | 27 | 0 |
   | **26** published robot-description packages, literal `<link name>` | published | **887** | **43** | 0 |
   | bench, ingest and `loop` fixtures | synthetic | ≤ 24 | ≤ 20 | 0 |

   The longest recorded name is `head_front_camera_link_color_optical_frame`
   (42 B); the longest shipped is `narrow_stereo_l_stereo_camera_optical_frame`
   (43 B, xacro-expanded PR2). The hash-suffix alternative stays on the shelf.

   **(a) The budget is 5 bytes, and no prefix fits.** `realsense2_camera`'s own test
   uses `robot1/` (7 B) and `spot_ros2` prefixes the unit's own name (unbounded).
   Composed by hand over real names: `tiago/` (6 B) refuses 2 of the 887 links
   (49 B), and `robot1/` refuses 2 TIAGo frames.
   **Decision item 5 must state 5, measured, rather than 28.**

   **(b) A URDF is a lower bound on a recording.** The TIAGo's `/tf` carries a
   42-byte name absent from its own URDF (longest link 40): a driver built it by
   appending `_color_optical_frame` to a link name. Concatenation drift is the
   mechanism most likely to push a deployment past 48, and 307 further link names
   are xacro templates whose prefix is supplied at build time.

   **(c) `TFT020`'s false positive is reachable:** the TIAGo's two longest frames
   under `tiago/` land at **exactly 48 bytes**, genuine and correct.

   **Also found:** 10 literal identifiers over 48 bytes (up to 55) in the same
   packages are all `<joint name>`s, not tf frames. If tf_tree ever interns
   anything joint-shaped (an edge label, a `parent_to_child` key), the store fails
   on a shipping robot.

   **Not established:** five recordings is not a survey, one was sampled, and no
   multi-robot or `tf_prefix`-namespaced `/tf` bag was found — the shape that
   would cross 48. The description corpus is 26 of 34 repositories attempted (eight
   downloads failed). Probes were standalone crates against `crates/tf_tree`, not
   gated under `just`.
2. **Should the bound be exposed as a constant?** `tf_tree_core::MAX_FRAME_NAME`
   would let a caller check before declaring, but pins 48 as public API and makes a
   widening a second break. Not answered.
3. **How does the C ABI report it?** `0020` is the precedent and is itself `draft`;
   the two want deciding together.
4. **Refusal at `intern` or at every public entry point?** Item 1 puts it at the
   single choke point. `tf_tree.build` interns in a loop, so a caller with one bad
   name among fifty learns of one per attempt; a builder-level pre-pass reporting
   all at once is a different change, not scoped here.

## What would make this ready

Question 1 is answered. Questions 2 and 4 are ergonomics and can be answered by
choosing. Question 3 needs `0020` to move; if it has not when steps 1–5 are
wanted, steps 1, 2, 4 and 5 can land and leave the C ABI reporting
`TFT_ERR_INTERNAL` as today, as a decision rather than an omission. This record
does not authorise any step.

## Reproduction

Against any interpreter with the `transform_tree` wheel installed; no fixture or
`shm` feature needed. Broken 1 and the refutation:

```python
import tf_tree

V = "v" * 51
T = V[:48]
tree = tf_tree.build([("root", V), ("root", T)], capacity=8)
q = lambda tx: [1.0, 0.0, 0.0, 0.0, tx, 0.0, 0.0]
tf_tree.push(tree, V, "root", 1000, q(-104.0))
tf_tree.push(tree, T, "root", 1000, q(-99.0))

fr = tree.frames()
print(len(fr), len(set(fr)))                 # 3 entries, 2 distinct
print(tree.lookup("root", V, 1000)[0][3])    # -104.0
print(tree.lookup("root", T, 1000)[0][3])    # -99.0
print(V in fr, T in fr)                      # False True
```

Broken 2 builds two chains under a 56-byte prefix and counts self-loops in
`tree.edges()`; the corollaries `tree.freeze(path)` the same tree and count `V`
and `T` in the file bytes, and freeze a 50-byte name (47 ASCII + `中`) to see
U+FFFD.
