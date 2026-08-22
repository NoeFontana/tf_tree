# 0027: the 48-byte frame-name store

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none yet

## Context

`FrameRecord::name` is `[u8; 48]`. `FrameRecord::for_name` writes
`let n = src.len().min(48)` and returns a record
(`crates/tf_tree_core/src/frame.rs:151`). There is no refusal, no flag, and no
signal of any kind: a 56-byte name is interned exactly as successfully as a
6-byte one.

`blake3_64` is taken over the **full** name (`frame.rs:102`), and
`FrameRecord::name_matches` (`frame.rs:171`) compares the truncated bytes only
as a hash-collision tiebreak. So *identity* is exact and *display* is not, and
every published name surface — `Tree::frames`, `Tree::edges`, the `.tft`
manifest, `doctor`, `top` — reads the truncated copy.

[`0026`](./0026-the-corpus-shape-of-a-frozen-index.md) found this while scoping
a different question and explicitly left it: its *What this does not decide*
says "the 48-byte store … own record", and its *Consequences* says the
truncation is **avoided** by deciding one `.tft` per episode, not fixed — "one
of them (`edges()` returning a self-loop) is reachable by any user with a long
frame name, corpus or not." That is what forces this record: the defect does not
depend on the corpus layout, and nothing else is scheduled to touch it.

## What is broken, and what is not

Everything below was re-run for this record from scripts written for it, not
inherited from `0026`. `0026`'s own review pass refuted one of its first
draft's claims about this exact code, so inheriting the rest would have been
the wrong lesson to draw from that.

**Instrument check.** The wheel under measurement is `transform_tree` 0.0.2 in
`/tmp/sdvenv`; its extension module is dated after the last commit touching
`crates/tf_tree_py` or `crates/tf_tree_core` (`2fb8283`, 2026-08-16 21:18), and
`find crates/tf_tree_py/src crates/tf_tree_core/src -name '*.rs' -newer
…/_core.cpython-313-x86_64-linux-gnu.so` returns nothing, so it is not a stale
artifact. The working tree is clean. It reports:

```
$ /tmp/sdvenv/bin/python -c "import tf_tree; print(tf_tree.arena_format_version(), hex(tf_tree.arena_layout_hash()))"
3 0x3d104195
```

which is the repository's `FORMAT_VERSION` and `layout_hash`.

### Not broken: resolution. The refuted claim, stated plainly

**A `lookup` on a full over-length name does not resolve to the truncated
frame.** `0026`'s first revision implied it did; its review refuted that, and it
is refuted again here. This is written first because it is the claim a future
reader is most likely to re-raise.

`V` is 51 bytes; `T` is `V[:48]`, declared as a **separate** frame; both hang
off `root` with different translations:

```
$ /tmp/sdvenv/bin/python repro_a.py
frames() = 3 entries, 2 distinct
48-byte entries: 2, distinct among them: 1
lookup('root', V=51B) tx = -104.0   truth -104.0
lookup('root', T=48B) tx = -99.0   truth  -99.0
copied out of frames() -> lookup tx = -99.0
V in frames(): False  T in frames(): True
```

Rows 3 and 4 are the refutation: each name answers with its own transform. The
engine never confuses two frames sharing a 48-byte prefix, because the hash is
over the full name. **A caller holding the real names gets right answers, and no
part of this record says otherwise.**

### Broken 1 — `frames()` output is not usable as `frames()` input

Rows 1 and 2 set it up: three entries, two distinct, and the two 48-byte entries
are **one** distinct string — `frames()` emits byte-identical names for two
different frames. Row 5 is the defect itself, the same list fed back: the string
a user copies out for `V` **is** `T`'s name, and answers as `T`, with no error.
Row 6 is the sharper form of it — the full name `V` is not in `frames()` at all,
so there is no string in that list that means `V`.

The hazard is entirely in the output-then-reuse loop, and that loop is not
hypothetical: it is what a corpus index, `doctor`, and any plotting script do. A
silent wrong transform is the failure class D11 and D15 exist to eliminate.

`len(name) == 48` is **not** a sound tell for it, and the run above is its own
counter-example: `T` is a genuine 48-byte name, it trips the test, and it
resolves correctly. Any check built on the stored length has false positives by
construction.

### Broken 2 — `edges()` reports a graph that is not a tree

Two episodes under the natural 56-byte corpus key, each a `map → odom →
base_link` chain — six frames, four edges:

```
$ /tmp/sdvenv/bin/python repro_b.py
prefix bytes: [56, 56]
frames(): 6 entries, 1 distinct
edges():  4 entries, 1 distinct -> [('bridge_data_v2/toykitchen2/put_carrot_on_plate/t',
                                     'bridge_data_v2/toykitchen2/put_carrot_on_plate/t')]
SELF-LOOPS in edges(): 4
engine identity: traj0000 tx=2.0
engine identity: traj0001 tx=4.0
```

Four self-loops, one distinct. Any consumer that reconstructs topology from
`edges()` builds a cycle, in a project whose D2 is *a tree, not a pose graph*.
The last two rows are the same point as above: the stored topology is fine, the
published one is not.

This needs **one** over-long frame name and no corpus. A single robot with a
deeply namespaced sensor frame reaches it.

### Two corollaries

**The full name is nowhere in the frozen file.** Freezing the `repro_a` tree:

```
$ /tmp/sdvenv/bin/python repro_c.py
frozen bytes: 2116608
occurrences of the 51-byte name V: 0
occurrences of the 48-byte prefix T: 4
reopened frames(): 3 distinct 2
V in reopened.frames(): False
```

This is not an oversight in the writer, and it bounds one of the remedies below.
`Frozen::manifest` (`crates/tf_tree/src/frozen.rs:270`) builds its `frames`
array by reading `FrameRecord::name` out of the arena, and the arena holds no
other copy of the name — so **the freeze path physically cannot emit the full
name.** "Put full names in the manifest" is not a remedy that can be implemented
against the current store.

**Truncation can split a UTF-8 codepoint.** 47 ASCII bytes followed by a 3-byte
codepoint is 50 bytes; the store keeps one byte of the three:

```
U is 50 bytes, 48 chars
frames() -> 'uuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuu�'
U+FFFD present: True
```

`from_utf8_lossy` substitutes a replacement character rather than raising. Minor
against the two above, but it means the stored bytes are not guaranteed to be
valid UTF-8, which any future consumer reading the frame table directly has to
handle.

### What the store costs, and what a wider one would cost

`FrameRecord` is `#[repr(C, align(64))]` and exactly 64 bytes, asserted at
compile time (`frame.rs`). The fields account for 59 of them — `name_hash` 8,
`name` 48, `name_len`/`flags`/`frame_kind` 1 each — leaving `_pad: [u8; 5]`.

The frame table's stride is that size, and the arena layout is a pinned
fixture:

```
$ cargo nextest run -p tf_tree_arena -E 'test(large_uniform_fixture) or test(small_mixed_capacity_fixture) or test(layout_hash_is_deterministic_and_stable)'
    Starting 3 tests across 2 binaries (22 tests skipped)
        PASS [   0.005s] (1/3) tf_tree_arena layout::tests::layout_hash_is_deterministic_and_stable
        PASS [   0.005s] (2/3) tf_tree_arena layout::tests::large_uniform_fixture
        PASS [   0.005s] (3/3) tf_tree_arena layout::tests::small_mixed_capacity_fixture
     Summary [   0.005s] 3 tests run: 3 passed, 22 skipped
```

Those three pin, respectively, `layout_hash() == 0x3D10_4195`; a 1000-frame
arena whose `frame_table().size` is `64_000` against a `total_size()` of
`295_393_600`; and an 8-frame arena whose `frame_table().size` is `512`.

So the frame table is **0.0217 %** of a 1000-frame, 1000-edge, 4096-sample
arena. Arithmetic on the same stride for the `just gate4` fixture — 1 537 frames
in a 338 MiB file, per that recipe's own output, quoted in `PHASE5.md` §12's
amendment — is 98 368 B, **0.028 %**. A 256-byte name field would make the record
320 bytes (8 + 256 + 3, rounded up to the 64-byte alignment), which **adds**
256 000 B to the first arena and 393 472 B to the second — **+0.087 %** and
**+0.11 %** of their totals.

**Bytes are not what a wider store costs.** What it costs is a format break, for
two separate reasons:

- **A stride change is visible to the attach check and invalidates everything.**
  `layout_hash()` hashes the frame-table stride as the literal `64`
  (`crates/tf_tree_arena/src/layout.rs`, the `strides` array). Changing it
  changes `0x3D10_4195`, which per `check.rs` means every existing `.tft` and
  every running participant is refused. `PHASE5.md` §1 spent one deliberate
  fleet-wide break to avoid taking three, and CLAUDE.md's standing caution is
  not to add arena fields opportunistically now that it has landed.
- **A change that fits inside the current stride is worse, not better.** Moving
  the name/pad boundary — `name: [u8; 53]`, consuming all five pad bytes — keeps
  the record at 64 bytes and is therefore **invisible to every check the project
  has.** `layout_hash` hashes region strides, not field offsets, and
  `tf_tree_core` contributes no hash of its own (`grep -rn 'fnv1a\|layout_hash'
  crates/tf_tree_core/src/` finds only two comments). Two builds disagreeing
  about where `name_len` lives would attach to each other and misread names.
  `ArenaHeader::_reserved_covariance`'s doc comment already records this exact
  hazard class for the header. So the "free" widening is the one that needs a
  `FORMAT_VERSION` bump most.

And 53 bytes does not reach the case that exposed the defect — the natural
corpus key is 56 bytes before the frame name.

## Decision

**`intern` refuses a name longer than 48 bytes. Nothing is truncated, and the
store does not move.**

1. **`FrameError` gains `NameTooLong { len: u32 }`.** `ArenaView::intern`
   returns it before hashing. The variant carries the offending length and not
   the name, because the error type is `Copy` and prose lives in a separate
   layer (R5, D11); the caller passed the name in and still holds it, which is
   how `CapacityExceeded` already works. `FrameError` is `#[non_exhaustive]`, so
   the variant is source-compatible for every `match` that exists.
2. **`FrameRecord::for_name` keeps `min(48)` and gains a debug assertion**, not
   a second refusal. It is called from exactly one place
   (`arena_view.rs:341`, inside `intern`'s `write_record` closure) and that
   place has already refused; a second check on the write path would be a
   second spelling of the rule with no caller able to reach it.
3. **The two places names arrive from outside the program report the refusal;
   they do not abort.** MCAP ingest (`tf_tree_ingest`, which interns `frame_id`
   straight off the wire) and the ROS bridge (`tf_tree_bridge`, whose declared
   set comes from `/tf`) must not stop being able to read a recording or a live
   graph because one publisher has a long frame name. §3.2's anomaly table is
   the mechanism that already exists for precisely this and it gains a row —
   *frame name over 48 bytes → drop the edge, count, report loudly, name the
   frame* — sitting beside "zero stamps" and "edge kind changes mid-recording",
   which are the same shape of problem.
4. **`doctor` gains `TFT020`, and it is a warning.** The refusal is not
   retroactive: arenas and `.tft` files written before it still contain
   truncated names, and nothing in them can recover the original. The check
   flags a frame table containing a name of stored length 48. It **cannot** be
   an error, because the tell has a measured false positive — `T` above is a
   genuine 48-byte name that resolves correctly — and §6's catalogue is not the
   place for a check that fires on healthy trees. Its message says "may have
   been truncated; regenerate this file with a build that refuses".
5. **The 48-byte bound is documented as a public constraint**, in `API.md` §2's
   per-binding surface and in `tf_tree.build`'s docstring, with the budget
   arithmetic that makes it concrete rather than a bare number: 48 bytes minus
   the longest frame name in your topology is what a namespace prefix has left.
   **The worst case measured for open question 1 is 5 bytes** — a shipped,
   xacro-expanded PR2 description whose longest link is 43 — and the
   documentation should say so plainly, because at 5 bytes **no namespacing
   prefix the ROS ecosystem ships fits**, and a reader who is told only "48 minus
   your longest name" will not work that out for themselves.

## Rationale

**Every alternative leaves a name in `frames()` that is not the caller's name,
and that is the actual defect.** The engine's identity is already exact; what is
broken is that the published surfaces emit strings which look like names, cannot
be used as names, and fail silently when they are. A refusal is the only option
that makes "what `frames()` returned" and "what you can pass back" the same set.
It fixes Broken 2 outright — with no truncated names there are no colliding
endpoints, so `edges()` is a tree again — and converts Broken 1 from a silent
wrong transform into a declaration-time error, which is a *different* claim and
a weaker one, and is said here rather than smoothed over.

**The refusal costs almost nothing structurally.** `intern` is already fallible
and `FrameError` is already `Copy` and `#[non_exhaustive]`; `tf_tree`'s builder
already maps it to `BuildError::Frame` at two call sites (`tree.rs:614`, `:627`).
No new error type, no new signature, no lifetime, no allocation, nothing on a
hot path — interning happens at declaration.

### Alternatives considered

- **Keep 48 bytes and make truncation loud.** `FrameRecord::flags` is a reserved
  `u8` that nothing reads — `grep -rn '\.flags' crates/tf_tree_core/src
  crates/tf_tree/src crates/tf_tree_cli/src` finds no consumer — so a `TRUNCATED`
  bit costs no layout change and no field. It
  fails on the surface that matters: `frames() -> list[str]` has nowhere to put
  a per-entry flag, and adding `frames_truncated()` beside it is exactly the
  "second spelling of an existing path" §6 forbids. The flag would make `doctor`
  precise (no false positive) while leaving the silent wrong answer in place for
  every caller who does not run `doctor`. Rejected as a remedy; the flag bit is
  worth having *if* item 4's warning turns out to fire too often, and is noted
  here rather than adopted.
- **Widen the store.** Costed above: negligible in bytes (a 256-byte name field
  adds 0.11 % to a 338 MiB arena), expensive in format. Rejected on a stronger
  ground than cost, though: **any finite bound has the same failure shape at the
  bound.** Widening
  converts a certainty into a rarity and leaves the silent-wrong-answer path
  intact, which is the worst of both — a defect that now needs an unlucky
  topology to reproduce.
- **Reclaim the five pad bytes (48 → 53).** Rejected twice over: it is invisible
  to `layout_hash` and so is the most dangerous change in this list per byte
  gained, and 53 does not reach the 56-byte key that exposed the problem.
- **Hash-suffix the truncation** — store `name[..40]` plus 8 hex of `blake3_64`.
  Genuinely better than today: names stay distinct, so `frames()` has no
  duplicates and `edges()` is a tree, and a suffixed string fed back into
  `lookup` fails *loudly* with `FrameNotDeclaredError` instead of answering about
  another frame. It needs no layout change. Rejected because it manufactures a
  name that is not the caller's and prints it everywhere a real name is printed,
  and because it moves the collision problem into `name_matches` — the
  hash-collision tiebreak would have to reproduce the suffixing to stay sound,
  which is a correctness change to the intern path in exchange for a
  presentation improvement. Kept on the shelf as the fallback if open question 1
  finds that refusing breaks real users.
- **Put full names in the manifest.** Measured dead: the arena holds no copy of
  the full name, and `Frozen::manifest` reads the frame table, so there is
  nothing for the writer to emit. It becomes possible only *after* a wider
  store, at which point it is redundant.

## Consequences

- **It is a breaking change, and the size of the break is now measured.** A
  program that builds a tree with a 60-byte frame name works today and stops.
  ~~The arithmetic says the common case is comfortable — `48 -
  len("camera_optical_frame")` leaves 28 bytes for a prefix, and conventional ROS
  link names are well under that — but **no survey of real fleet frame names was
  done for this record**, and one topology is not a survey.~~ **That 28 was
  arithmetic on a name nobody had looked up, and the survey now exists** (open
  question 1): the longest name in a decoded recording is **42 bytes** (a PAL
  TIAGo's `head_front_camera_link_color_optical_frame`) and the longest in a
  shipped robot description is **43** (`narrow_stereo_l_stereo_camera_optical_frame`,
  an xacro-expanded PR2). **The headroom is 5 bytes, not 28**, and no namespacing
  prefix the ecosystem ships fits in it — `realsense2_camera`'s own test uses
  `robot1/` (7 B) and `spot_ros2` sets the prefix to the unit's own name, which
  is unbounded. So the break is theoretical for one unprefixed robot and one
  namespace character away for a fleet.
- **Existing `.tft` files are not fixed and cannot be.** The full names are gone
  from any arena already written. `TFT020` reports; regeneration from the source
  recording is the only repair.
- **`0026`'s Decision item 4 gets a firmer footing.** That record's external
  corpus index carries frame and edge names, and its own *Consequences* warns
  that whatever writes the index "must read names from its own source, not from
  `frames()`/`edges()`, until the 48-byte record is settled". Under this
  decision the published names become trustworthy for arenas written after it —
  the warning narrows to files written before.
- **The C ABI has a new failure to express.** The bridge declares edges from
  ROS-supplied names, so `tft_bridge_*` needs a way to say "this name was
  refused". [`0020`](./0020-the-consumer-side-of-the-arena-refusal.md) records
  that widening an existing function's return set is the one addition this ABI's
  minor-bump precedent does not cover, so this is not free and is not settled
  here. See open question 3.
- **The arena layout, `FORMAT_VERSION` and `layout_hash` are untouched.** No
  region moves, no stride changes, `0x3D10_4195` stands. No `.tft` is
  invalidated. That is the whole point of choosing the refusal over the
  widening.
- **`just` recipes and CI are unchanged.** New tests attach to existing suites.

## Implementation plan

1. `FrameError::NameTooLong { len: u32 }` and the refusal in
   `ArenaView::intern`, before `blake3_64`. — verified by a test that interns a
   49-byte name and asserts the variant, plus one that interns a 48-byte name
   and asserts `Ok`; the boundary is the whole claim, so both sides of it are
   asserted. Mutant: change the comparison to `> 49` and the second test must
   still pass while the first fails.
2. Surface it: `BuildError::Frame` already carries it; add the Python exception
   mapping in `crates/tf_tree_py/src/errors.rs` beside the existing `FrameError`
   arms, and the `API.md` §2 and docstring text from Decision item 5. — verified
   by a Python test asserting the exception type and by `just py-test`.
3. §3.2's anomaly row and the ingest/bridge reporting path. — verified by an
   ingest fixture containing one over-long `frame_id`, asserting the recording
   still ingests, the edge is absent, and the report names the frame. A test
   that only asserts the error is not enough: the claim is that ingestion
   *continues*.
4. `TFT020` with its fixture, per §11's "one test per check ID, each with a
   fixture that triggers exactly that check and no other". — verified by that
   fixture, **and** by a second asserting a tree whose longest name is exactly
   48 bytes and genuine does *not* raise the check to error level, which is the
   false positive this record measured.
5. `docs/PHASE5.md` §6 catalogue entry and the `doctor --json` schema addition.
   — verified by the schema test that already guards consumer compatibility.

## Open questions

1. **RESOLVED 2026-08-22 by measurement: no — nothing reachable exceeds 48. The
   margin is **5 bytes**, not the 28 this record's *Consequences* asserts.**
   ~~**Does any real frame name exceed 48 bytes?** The refusal's entire cost is
   here and nothing in this record measures it. What would answer it: the frame
   sets of the recordings in `testdata/`, of `docker/tf2`'s fixtures, and of any
   public `/tf` bag, reported as a length histogram. If the maximum anywhere is
   comfortably under 48 the break is theoretical; if a real recording has one,
   the hash-suffix alternative comes off the shelf.~~

   **Synthetic and captured are separated, because most of what this repository
   holds is synthetic.** `zstd_conformance.mcap`, `synthetic_empty.db3` and
   `/home/dev/src/loop`'s eight `/tf` scenes each say in their own
   `ATTRIBUTION.md` that nothing came off a robot, and `docker/tf2` contributes
   no frame names of its own — its three harnesses read `parent`/`child` out of
   a `.tfstream`, `native_scaling.cpp:96` defaulting to
   `testdata/tfstream/indoor_atelier.tfstream` and `native_ratio.cpp:191` /
   `native_footprint.cpp:148` to `target/native/fixture.tfstream`, which
   `crates/tf_tree_bench/src/bin/native_arena.rs` generates from the bench
   fixture. Both are sources this repository already had. Public recordings were
   therefore fetched.

   | Source | Kind | Distinct | Max B | >48 |
   |---|---|---:|---:|---:|
   | `testdata/tfstream/indoor_atelier.tfstream` | captured | 10 | 16 | 0 |
   | Zenodo 19894190, upstream `.db3` of the above, re-decoded from CDR | captured | 10 | 16 | 0 |
   | Zenodo 19894190 outdoor run, 100 498 msgs, Header `frame_id`s | captured | 3 | 14 | 0 |
   | HF `xrkong/nuway_rosbag`, Nav2 shuttle, 1.32 GB MCAP — `/tf` proper | captured | 2 | 9 | 0 |
   | — same bag, Header `frame_id`s across 110 channels | captured | 16 | 24 | 0 |
   | Zenodo 13749419, RoboCup@Home 2024, PAL TIAGo, `/tf` + `/tf_static` | captured | 65 | **42** | 0 |
   | — same bag, its own `/robot_description` URDF | captured | 53 | 40 | 0 |
   | HF `UniflexAI/rosbag2_d435i_g1_indoor`, Unitree G1, **sampled** 24 × 2 MB of 16.4 GB | captured | — | 27 | 0 |
   | **26** published robot-description packages, literal `<link name>` (+`.xml`/`.world`/SDF `<frame name>`) | published | **887** | **43** | 0 |
   | `crates/tf_tree_bench/src/fixture.rs` | synthetic | 24 | 20 | 0 |
   | `crates/tf_tree_ingest/src/fixture.rs` | synthetic | 7 | 9 | 0 |
   | `/home/dev/src/loop` `fixtures/mcap-tf`, 8 scenes | synthetic | 3 | 9 | 0 |

   Pooled over the four **fully decoded** recordings — 86 distinct names — the
   tail is `34:2  35:1  37:1  40:1  42:2` and the maximum is
   `head_front_camera_link_color_optical_frame`, **42 bytes**, off the TIAGo. The
   G1 bag is a fifth recording, sampled rather than decoded (its `/tf` carries
   177 messages, and a name appearing under ~1 per 680 MB could hide from the
   sampler); it is corroboration, not a decoded row. The 887-name description
   population's tail is `37:6  38:4  39:2  40:3  41:4  43:2` — nothing at 42, and
   nothing between 44 and 48 — with the maximum
   `narrow_stereo_l_stereo_camera_optical_frame` and its `_r_` sibling, **43
   bytes**, in `moveit_resources-ros2/pr2_description/urdf/robot.xml`: a shipped,
   xacro-expanded, unprefixed PR2 description. A `robot_state_publisher` fed that
   file publishes that frame.

   **This was run, not computed.** Every set was interned through
   `TreeBuilder::frame` and read back through `Tree::frames()`, in probes built
   against this commit, with a control that proves the probe can see the defect:

   ```
   self-test 48/49/56:  interned 3 distinct, frames() -> 3 entries / 3 distinct; NOT round-tripped: 2
      lost: "bbbb…b" (49 B)
      lost: "cccc…c" (56 B)
   indoor_atelier /tf:          10 distinct, max input bytes 16; inputs >48B: 0; NOT round-tripped: 0
   nuway shuttle:               16 distinct, max input bytes 24; inputs >48B: 0; NOT round-tripped: 0
   RoboCup TIAGo 65 frames:     65 distinct, max input bytes 42; inputs >48B: 0; NOT round-tripped: 0
   description link names:     887 distinct, max input bytes 43; inputs >48B: 0; NOT round-tripped: 0
   ```

   **So the refusal breaks no recording that could be obtained, and the
   hash-suffix alternative stays on the shelf. Three corrections follow.**

   **(a) The budget is 5 bytes, not 28 — and there is no prefix length that
   fits.** *Consequences* argues from `48 − len("camera_optical_frame")`. The
   longest name in a recording is 42 B, leaving 6; the longest in a shipped
   description is 43 B, leaving **5**. Both are smaller than every namespacing
   prefix the ecosystem ships: `realsense2_camera` declares `tf_prefix`, "prefix
   to be prepended to all frame IDs" (`rs_launch.py:95`), and its own live test
   sets it to `robot1/`, 7 bytes; `spot_ros2` sets
   `frame_prefix = self.name + "/"` (`spot_ros2.py:503`), so the prefix is the
   unit's own name and is unbounded. Composed by hand — a real prefix over a real
   name set, and labelled as such because no recording obtained here publishes a
   prefixed frame:

   ```
   TIAGo 65 real frames,   'tiago/'  (6 B) : max 48 B; refused 0
   TIAGo 65 real frames,   'robot1/' (7 B) : max 49 B; refused 2
      "robot1/head_front_camera_link_color_optical_frame" (49 B)
   887 description links,  'tiago/'  (6 B) : refused 2
      "tiago/narrow_stereo_l_stereo_camera_optical_frame" (49 B), and its _r_ sibling
   ```

   An earlier revision of this answer said a 6-byte prefix still fits; measured
   against the wider corpus it does not, and the honest statement is stronger
   than the one it replaces: **no namespace prefix in the ecosystem is
   accommodated by the current bound across shipped descriptions.** **Decision
   item 5 must state 5, measured, rather than 28, illustrated** — a docstring
   that understates the constraint by five-fold is worse than a bare number.

   **(b) A URDF is a lower bound on a recording, not an upper one.** The TIAGo
   bag publishes its own `/robot_description`, whose longest link is 40 bytes,
   while its `/tf` carries 42: the 42-byte string
   `head_front_camera_link_color_optical_frame` does not occur anywhere in the
   63 066-byte URDF — the RGB-D driver built it at run time by appending
   `_color_optical_frame` to the link `head_front_camera_link`. Note the doubled
   `_link_…_frame`: this is concatenation drift, not a name anyone chose, and it
   is the mechanism most likely to push a deployment past 48. Independently
   reproduced against the bag. Anyone who checks a URDF and concludes there is
   room has checked the wrong artefact — and the description population is itself
   a lower bound on what it produces, since 307 further link names in the same
   packages are xacro templates (`${namespace}camera_depth_optical_frame`,
   `${prefix}${fingerprefix}_inner_finger_pad`) whose prefix is supplied at build
   time.

   **(c) `TFT020`'s false positive is reachable, not merely constructible.**
   Item 4 keeps the check a warning because a genuine 48-byte name resolves
   correctly. That case is not hypothetical: the TIAGo's two longest frames under
   the 6-byte namespace `tiago/` land at **exactly 48 bytes**, genuine and
   correct.

   **One thing found that nobody asked for.** The same packages contain **10**
   literal identifiers over 48 bytes, up to 55
   (`depth_camera_front_camera_parent_to_depth_optical_frame`, ANYmal C's shipped
   `anymal.urdf`) — but every one is a `<joint name>`, and joints are not tf
   frames. Two of the ten are PR2's, formed by suffixing `_joint` onto the same
   43-byte link that sets the link maximum, which is (b)'s mechanism again.
   ANYmal C's longest *link* is 38, so the 17-byte gap between its joint and link
   naming is pure convention. If tf_tree ever interns anything joint-shaped — a
   diagnostic, an edge label, an `edges()` key built as `parent_to_child` — the
   48-byte store fails immediately on a robot that is already shipping.

   **What this does not establish.** Five recordings is not a survey, and one of
   the five was sampled rather than decoded. **No multi-robot or
   `tf_prefix`-namespaced `/tf` bag could be found** — Zenodo's API searched three
   ways, HuggingFace's dataset index for `rosbag`, `mcap` and `ros2_bag`; the
   other candidates with tf data (Tesla, R3LIVE/FAST-LIVO, Málaga) carry no `/tf`
   topic at all — and that is precisely the shape that would cross 48. **The
   description corpus is 26 repositories of 34 attempted**: eight downloads
   returned 9- or 14-byte error bodies and were not retried —
   `nasa/val_description`, `ros-industrial/motoman`, `ros-industrial/abb`,
   `nobleo/nav2_multirobot_bringup`, `ipa320/cob_common`,
   `Sanctuary-AI/phoenix_description`, `leo-rover/leo_common-ros2`,
   `ros-perception/velodyne_simulator`. Three are reachable on a different branch
   (`motoman@noetic-devel`, `abb@noetic-devel`, `cob_common@kinetic_dev`); five
   are gone from GitHub, including the NASA Valkyrie description and the one
   *multirobot* bringup in the list — the two most relevant entries to this
   question. Nothing here was built or gated under `just`; the probes are
   standalone crates path-depending on `crates/tf_tree`, on local `rustc 1.97.1`
   against CI's pinned 1.98.0. **The finding is that the break is theoretical for
   one unprefixed robot and one namespace character away for a fleet**, which is
   a weaker claim than "comfortable" and is stated here rather than smoothed
   over.
2. **Should the bound be exposed as a constant?** `tf_tree_core::MAX_FRAME_NAME`
   would let a caller check before declaring, which R6's read-only-by-default
   posture likes. It also pins 48 as public API, which makes a future widening a
   second break. Not answered here.
3. **How does the C ABI report it?** `0020` is the precedent and is itself a
   `draft` with three open questions. This record should not invent a status
   code while that one is unresolved; the two want deciding together.
4. **Does the refusal belong at `intern` or at every public entry point?**
   Item 1 puts it at the single choke point, which is correct for the engine.
   But `tf_tree.build` interns in a loop after validating its edge list, so a
   caller with one bad name among fifty learns about one of them per attempt. A
   builder-level pre-pass that reports every over-long name at once is friendlier
   and is a different change; not scoped here.

## What would make this ready

Open question 1 answered by a measurement, because it is the only one that can
change the decision rather than its edges — a length histogram over real
recordings either makes the break theoretical or sends this record to the
hash-suffix alternative. Questions 2 and 4 are ergonomics and can be answered by
choosing. Question 3 needs `0020` to move, and if it has not moved by the time
steps 1–5 are wanted, this record can land steps 1, 2, 4 and 5 and leave the C
ABI reporting `TFT_ERR_INTERNAL` as it does today — stated explicitly so that
partial landing is a decision rather than an omission.

This record does not authorise any step. It is `draft` and its author is not the
one who moves it.

## Reproduction

Three scripts, verbatim, against any interpreter with the `transform_tree` wheel
installed. They need no fixture, no corpus and no `shm` feature. The outputs
above are theirs.

`repro_a.py` — Broken 1, and the refutation of the stronger claim:

```python
import tf_tree

V = "v" * 51
T = V[:48]
assert len(V.encode()) == 51 and len(T.encode()) == 48 and T != V

tree = tf_tree.build([("root", V), ("root", T)], capacity=8)


def q(tx: float) -> list[float]:
    return [1.0, 0.0, 0.0, 0.0, tx, 0.0, 0.0]


tf_tree.push(tree, V, "root", 1000, q(-104.0))
tf_tree.push(tree, T, "root", 1000, q(-99.0))

fr = tree.frames()
print(f"frames() = {len(fr)} entries, {len(set(fr))} distinct")
forty_eight = [n for n in fr if len(n.encode()) == 48]
print(f"48-byte entries: {len(forty_eight)}, distinct among them: {len(set(forty_eight))}")

print(f"lookup('root', V=51B) tx = {tree.lookup('root', V, 1000)[0][3]}   truth -104.0")
print(f"lookup('root', T=48B) tx = {tree.lookup('root', T, 1000)[0][3]}   truth  -99.0")

# The hazard: enumerate, then query what you enumerated.
for name in fr:
    if len(name.encode()) == 48:
        print(f"copied out of frames() -> lookup tx = {tree.lookup('root', name, 1000)[0][3]}")
        break

print("V in frames():", V in fr, " T in frames():", T in fr)
```

`repro_b.py` — Broken 2:

```python
import tf_tree

PFX = [
    "bridge_data_v2/toykitchen2/put_carrot_on_plate/traj0000/",
    "bridge_data_v2/toykitchen2/put_carrot_on_plate/traj0001/",
]
print("prefix bytes:", [len(p.encode()) for p in PFX])

edges = []
for p in PFX:
    edges.append((p + "map", p + "odom"))
    edges.append((p + "odom", p + "base_link"))

tree = tf_tree.build(edges, capacity=8)
for i, p in enumerate(PFX):
    tf_tree.push(tree, p + "odom", p + "map", 1000, [1.0, 0, 0, 0, 2.0 * (i + 1), 0, 0])
    tf_tree.push(tree, p + "base_link", p + "odom", 1000, [1.0, 0, 0, 0, 0, 0, 0])

fr, eg = tree.frames(), tree.edges()
print(f"frames(): {len(fr)} entries, {len(set(fr))} distinct")
print(f"edges():  {len(eg)} entries, {len(set(eg))} distinct -> {sorted(set(eg))}")
print("SELF-LOOPS in edges():", sum(1 for a, b in eg if a == b))

for i, p in enumerate(PFX):
    print(f"engine identity: traj000{i} tx={tree.lookup(p + 'map', p + 'odom', 1000)[0][3]}")
```

`repro_c.py` — the two corollaries:

```python
import os
import tempfile

import tf_tree

V = "v" * 51
T = V[:48]

tree = tf_tree.build([("root", V), ("root", T)], capacity=8)
tf_tree.push(tree, V, "root", 1000, [1.0, 0, 0, 0, -104.0, 0, 0])
tf_tree.push(tree, T, "root", 1000, [1.0, 0, 0, 0, -99.0, 0, 0])

path = os.path.join(tempfile.mkdtemp(), "one.tft")
tree.freeze(path)
blob = open(path, "rb").read()
print("frozen bytes:", len(blob))
print("occurrences of the 51-byte name V:", blob.count(V.encode()))
print("occurrences of the 48-byte prefix T:", blob.count(T.encode()))

reopened = tf_tree.open_file(path)
print("reopened frames():", len(reopened.frames()), "distinct", len(set(reopened.frames())))
print("V in reopened.frames():", V in reopened.frames())

U = "u" * 47 + "中"  # 50 bytes, 48 chars: the store keeps 1 of the 3
print(f"\nU is {len(U.encode())} bytes, {len(U)} chars")
got = [n for n in tf_tree.build([("root", U)], capacity=8).frames() if n != "root"][0]
print("frames() ->", repr(got))
print("U+FFFD present:", "�" in got)
```
